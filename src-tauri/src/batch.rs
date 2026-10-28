//! Batch generation: fills text fields of a template PDF with the data from each
//! row of a table (CSV or Excel), e.g. to issue certificates with different names.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, State};

use crate::pdf::{
    encode_png, err, load, objects_at, pdfium, pdfium_lock, render, replace_run, to_rect, Align, AppState,
    RelRect,
};

/// Set from the UI to stop a generation in progress.
static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Debug)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Text fragment of the template that is replaced with a table column, or a
/// box where a QR code with the column's value goes.
#[derive(Deserialize, Clone)]
pub struct Field {
    page: PdfPageIndex,
    /// Objects of the fragment, left to right (as returned by `text_object_at`).
    #[serde(default)]
    objects: Vec<PdfPageObjectIndex>,
    column: usize,
    #[serde(default = "default_align")]
    align: Align,
    /// Shrink the text if it doesn't fit the page width.
    #[serde(default)]
    shrink: bool,
    /// If set, the field is a QR code drawn in this box.
    #[serde(default)]
    qr: Option<RelRect>,
}

fn default_align() -> Align {
    Align::Left
}

/// Margin left on each side when shrinking a text.
const SHRINK_MARGIN: f32 = 28.0;

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Output {
    /// One PDF per row inside `dir`; `names[i]` is the file name for row `i`.
    Files { dir: String, names: Vec<String> },
    /// A single PDF with the pages of all rows.
    Single { path: String },
}

#[derive(Serialize, Clone)]
pub struct Progress {
    done: usize,
    total: usize,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    generated: usize,
    cancelled: bool,
    /// Rows (0-based) where some field had to use another font.
    font_replaced_rows: Vec<usize>,
    /// Files written.
    files: Vec<String>,
}

// ---------- Reading tables ----------

/// Reads a CSV or an Excel/LibreOffice spreadsheet. The first row holds the headers.
pub fn read_table(path: &Path) -> Result<Table, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let mut rows = match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => read_spreadsheet(path)?,
        _ => read_csv(&std::fs::read(path).map_err(err)?)?,
    };
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if rows.is_empty() {
        return Err("el archivo no tiene datos".into());
    }

    let headers: Vec<String> = rows
        .remove(0)
        .into_iter()
        .enumerate()
        .map(|(i, h)| match h.trim() {
            "" => format!("Columna {}", i + 1),
            h => h.to_string(),
        })
        .collect();
    for row in &mut rows {
        row.resize(headers.len(), String::new());
    }
    Ok(Table { headers, rows })
}

/// Text of a file honoring its BOM; without a BOM, UTF-8 if valid and otherwise Windows-1252,
/// which is how Excel saves CSV files in Spanish.
fn decode(bytes: &[u8]) -> String {
    let encoding = match encoding_rs::Encoding::for_bom(bytes) {
        Some((encoding, _)) => encoding,
        None if std::str::from_utf8(bytes).is_ok() => encoding_rs::UTF_8,
        None => encoding_rs::WINDOWS_1252,
    };
    encoding.decode(bytes).0.into_owned()
}

/// Most frequent separator in the first line (outside quotes): comma, semicolon or
/// tab. Spanish Excel uses semicolons.
fn sniff_delimiter(text: &str) -> u8 {
    let mut counts = [(b',', 0), (b';', 0), (b'\t', 0)];
    let mut in_quotes = false;
    for c in text.lines().next().unwrap_or_default().bytes() {
        if c == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes {
            for (delimiter, count) in &mut counts {
                if c == *delimiter {
                    *count += 1;
                }
            }
        }
    }
    counts.iter().max_by_key(|(_, n)| *n).filter(|(_, n)| *n > 0).map_or(b',', |(d, _)| *d)
}

fn read_csv(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    let text = decode(bytes);
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(sniff_delimiter(&text))
        .from_reader(text.as_bytes())
        .records()
        .map(|r| r.map(|record| record.iter().map(String::from).collect()).map_err(err))
        .collect()
}

fn read_spreadsheet(path: &Path) -> Result<Vec<Vec<String>>, String> {
    use calamine::{open_workbook_auto, Reader};

    let mut workbook = open_workbook_auto(path).map_err(err)?;
    let range = workbook
        .worksheet_range_at(0)
        .ok_or("el archivo no tiene hojas")?
        .map_err(err)?;
    Ok(range
        .rows()
        .map(|row| row.iter().map(cell_text).collect())
        .collect())
}

/// Cell text as a person would see it: no ".0" on integers, and dates as dd/mm/yyyy.
fn cell_text(cell: &calamine::Data) -> String {
    use calamine::Data;

    match cell {
        Data::Empty => String::new(),
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Data::DateTime(date) => match date.as_datetime() {
            Some(dt) if dt.format("%H:%M").to_string() == "00:00" => {
                dt.format("%d/%m/%Y").to_string()
            }
            Some(dt) => dt.format("%d/%m/%Y %H:%M").to_string(),
            None => cell.to_string(),
        },
        other => other.to_string(),
    }
}

// ---------- Generation ----------

/// Copy of the template with the values of one row. Also returns whether some field
/// had to use another font.
fn fill(
    template: &[u8],
    fields: &[Field],
    row: &[String],
    known: &mut HashMap<String, HashSet<char>>,
) -> Result<(PdfDocument<'static>, bool), String> {
    let mut doc = load(template.to_vec(), None)?;
    let mut font_replaced = false;

    let mut pages: Vec<PdfPageIndex> = fields.iter().map(|f| f.page).collect();
    pages.sort_unstable();
    pages.dedup();
    for index in pages {
        let mut page = doc.pages().get(index).map_err(err)?;
        let texts: Vec<&Field> = fields.iter().filter(|f| f.page == index && f.qr.is_none()).collect();
        // Resolve the objects of every field on the page first: replacing
        // one removes objects and would shift the indices of the others.
        let resolved = texts
            .iter()
            .map(|f| objects_at(&mut page, &f.objects))
            .collect::<Result<Vec<_>, _>>()?;
        for (field, objects) in texts.into_iter().zip(resolved) {
            let value = row.get(field.column).map_or("", |v| v.trim());
            let max_width = if field.shrink { available_width(&page, &objects, field.align)? } else { None };
            font_replaced |= replace_run(&mut doc, &mut page, objects, value, field.align, max_width, known)?;
        }
        for field in fields.iter().filter(|f| f.page == index) {
            if let Some(rect) = field.qr {
                let value = row.get(field.column).map_or("", |v| v.trim());
                draw_qr(&doc, &mut page, rect, value)?;
            }
        }
    }
    Ok((doc, font_replaced))
}

/// Width a text can take without leaving the page, depending on its alignment.
fn available_width(page: &PdfPage, objects: &[PdfPageObject], align: Align) -> Result<Option<f32>, String> {
    let (mut left, mut right) = (f32::MAX, f32::MIN);
    for o in objects {
        let r = o.bounds().map_err(err)?.to_rect();
        left = left.min(r.left().value);
        right = right.max(r.right().value);
    }
    if objects.is_empty() {
        return Ok(None);
    }
    let width = page.width().value;
    let available = match align {
        Align::Left => width - left - SHRINK_MARGIN,
        Align::Right => right - SHRINK_MARGIN,
        Align::Center => 2.0 * ((left + right) / 2.0).min(width - (left + right) / 2.0) - 2.0 * SHRINK_MARGIN,
    };
    // Never narrower than the template's original text.
    Ok(Some(available.max(right - left)))
}

/// Draws a QR code (vector, sharp at any size) centered in the box.
fn draw_qr(doc: &PdfDocument<'static>, page: &mut PdfPage<'static>, rect: RelRect, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    let code = qrcode::QrCode::new(value.as_bytes()).map_err(|e| format!("no se pudo crear el QR de \"{value}\": {e}"))?;
    let area = to_rect(page, rect.from, rect.to)?;
    let modules = code.width();
    // With a 2-module margin around it (readers need it to recognize the code).
    let side = area.width().value.min(area.height().value);
    let cell = side / (modules + 4) as f32;
    let x0 = area.left().value + (area.width().value - side) / 2.0 + 2.0 * cell;
    let top = area.top().value - (area.height().value - side) / 2.0 - 2.0 * cell;

    let background = PdfPagePathObject::new_rect(
        doc,
        PdfRect::new_from_values(top - (modules + 2) as f32 * cell, x0 - 2.0 * cell, top + 2.0 * cell, x0 + (modules + 2) as f32 * cell),
        None,
        None,
        Some(PdfColor::WHITE),
    )
    .map_err(err)?;
    page.objects_mut().add_path_object(background).map_err(err)?;

    let colors = code.to_colors();
    let mut path = PdfPagePathObject::new(doc, PdfPoints::new(x0), PdfPoints::new(top), None, None, Some(PdfColor::BLACK))
        .map_err(err)?;
    for y in 0..modules {
        let mut x = 0;
        while x < modules {
            if colors[y * modules + x] != qrcode::Color::Dark {
                x += 1;
                continue;
            }
            // One rectangle per run of consecutive dark modules.
            let start = x;
            while x < modules && colors[y * modules + x] == qrcode::Color::Dark {
                x += 1;
            }
            let (l, r) = (x0 + start as f32 * cell, x0 + x as f32 * cell);
            let (t, b) = (top - y as f32 * cell, top - (y + 1) as f32 * cell);
            path.move_to(PdfPoints::new(l), PdfPoints::new(t)).map_err(err)?;
            path.line_to(PdfPoints::new(r), PdfPoints::new(t)).map_err(err)?;
            path.line_to(PdfPoints::new(r), PdfPoints::new(b)).map_err(err)?;
            path.line_to(PdfPoints::new(l), PdfPoints::new(b)).map_err(err)?;
            path.close_path().map_err(err)?;
        }
    }
    page.objects_mut().add_path_object(path).map_err(err)?;
    page.regenerate_content().map_err(err)?;
    Ok(())
}

/// Safe file name that doesn't overwrite another file in the folder or in this batch.
fn unique_file_name(name: &str, row: usize, dir: &Path, used: &mut HashSet<String>) -> String {
    let clean: String = name
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    let clean = clean.trim();
    let stem = match clean.len().checked_sub(4) {
        Some(i) if clean.is_char_boundary(i) && clean[i..].eq_ignore_ascii_case(".pdf") => {
            clean[..i].trim()
        }
        _ => clean,
    };
    let stem = if stem.is_empty() {
        format!("fila-{}", row + 1)
    } else {
        stem.to_string()
    };

    let mut candidate = format!("{stem}.pdf");
    let mut n = 2;
    while dir.join(&candidate).exists() || !used.insert(candidate.to_lowercase()) {
        candidate = format!("{stem} ({n}).pdf");
        n += 1;
    }
    candidate
}

pub(crate) fn generate(
    template: &[u8],
    fields: &[Field],
    rows: &[Vec<String>],
    output: Output,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<BatchResult, String> {
    if fields.is_empty() {
        return Err("no hay campos marcados".into());
    }
    CANCEL.store(false, Ordering::SeqCst);
    let total = rows.len();
    let mut known = HashMap::new();
    let mut result = BatchResult {
        generated: 0,
        cancelled: false,
        font_replaced_rows: Vec::new(),
        files: Vec::new(),
    };

    match output {
        Output::Files { dir, names } => {
            let dir = PathBuf::from(dir);
            let mut used = HashSet::new();
            for (i, row) in rows.iter().enumerate() {
                if CANCEL.load(Ordering::SeqCst) {
                    result.cancelled = true;
                    break;
                }
                // Released between rows so the window can keep drawing.
                let _pdfium = pdfium_lock();
                let (doc, replaced) = fill(template, fields, row, &mut known)?;
                let name = names.get(i).map_or("", String::as_str);
                let path = dir.join(unique_file_name(name, i, &dir, &mut used));
                doc.save_to_file(&path).map_err(err)?;
                if replaced {
                    result.font_replaced_rows.push(i);
                }
                result.files.push(path.to_string_lossy().into_owned());
                result.generated += 1;
                on_progress(i + 1, total);
            }
        }
        Output::Single { path } => {
            let mut combined = Some(pdfium_lock_scope(|| pdfium()?.create_new_pdf().map_err(err))?);
            // `combined` is always dropped with the lock held, even if something fails.
            let run = || -> Result<(), String> {
                let combined = combined.as_mut().unwrap();
                for (i, row) in rows.iter().enumerate() {
                    if CANCEL.load(Ordering::SeqCst) {
                        // A half-written file would confuse more than help: it isn't saved.
                        result.cancelled = true;
                        return Ok(());
                    }
                    let _pdfium = pdfium_lock();
                    let (doc, replaced) = fill(template, fields, row, &mut known)?;
                    combined.pages_mut().append(&doc).map_err(err)?;
                    if replaced {
                        result.font_replaced_rows.push(i);
                    }
                    result.generated += 1;
                    on_progress(i + 1, total);
                }
                let _pdfium = pdfium_lock();
                combined.save_to_file(&path).map_err(err)?;
                result.files.push(path);
                Ok(())
            };
            let outcome = run();
            let _pdfium = pdfium_lock();
            drop(combined.take());
            outcome?;
        }
    }
    Ok(result)
}

fn pdfium_lock_scope<T>(f: impl FnOnce() -> T) -> T {
    let _pdfium = pdfium_lock();
    f()
}

pub(crate) fn template_bytes(state: &AppState) -> Result<Vec<u8>, String> {
    state.with_doc(|open| open.plain_bytes())
}

// ---------- Tauri commands ----------

#[tauri::command]
pub async fn read_table_file(path: String) -> Result<Table, String> {
    read_table(Path::new(&path))
}

/// Renders a template page with the values of one row, without modifying the document.
pub fn preview_png(
    state: &AppState,
    fields: &[Field],
    row: &[String],
    page: PdfPageIndex,
    width: u32,
) -> Result<Vec<u8>, String> {
    let template = template_bytes(state)?;
    let image = pdfium_lock_scope(|| {
        let (doc, _) = fill(&template, fields, row, &mut HashMap::new())?;
        render(&doc.pages().get(page).map_err(err)?, width)
    })?;
    encode_png(&image)
}

#[tauri::command]
pub async fn batch_preview(
    fields: Vec<Field>,
    row: Vec<String>,
    page: PdfPageIndex,
    width: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    Ok(Response::new(preview_png(&state, &fields, &row, page, width)?))
}

#[tauri::command]
pub async fn batch_generate(
    fields: Vec<Field>,
    rows: Vec<Vec<String>>,
    output: Output,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BatchResult, String> {
    let template = template_bytes(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        generate(&template, &fields, &rows, output, |done, total| {
            let _ = app.emit("batch-progress", Progress { done, total });
        })
    })
    .await
    .map_err(err)?
}

pub(crate) fn cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

#[tauri::command]
pub fn batch_cancel() {
    cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pliego-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn csv_from_spanish_excel() {
        // Spanish Excel: semicolons and Windows-1252 ("Pérez" with é = 0xE9).
        let bytes = b"Nombre;Curso\r\nAna P\xe9rez;\"Nutrici\xf3n; nivel 1\"\r\n;\r\nLuis;Qu\xedmica\r\n";
        let path = tmp_dir("csv").join("datos.csv");
        std::fs::write(&path, bytes).unwrap();
        let table = read_table(&path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Curso"]);
        assert_eq!(
            table.rows,
            [["Ana Pérez", "Nutrición; nivel 1"], ["Luis", "Química"]]
        );
    }

    #[test]
    fn csv_with_utf8_bom_and_commas() {
        let bytes = "\u{feff}Nombre,Fecha\nJosé Ñúñez,26/08/2026\n".as_bytes();
        let path = tmp_dir("bom").join("datos.csv");
        std::fs::write(&path, bytes).unwrap();
        let table = read_table(&path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Fecha"]);
        assert_eq!(table.rows, [["José Ñúñez", "26/08/2026"]]);
    }

    #[test]
    fn excel_numbers_and_dates() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/datos.xlsx"));
        let table = read_table(path).unwrap();
        assert_eq!(table.headers, ["Nombre", "Horas", "Fecha"]);
        assert_eq!(
            table.rows,
            [
                ["José Ñúñez", "2", "26/08/2026"],
                ["Ana", "1.5", "01/09/2026 14:30"]
            ]
        );
    }

    #[test]
    fn file_names_are_safe_and_unique() {
        let dir = tmp_dir("names");
        std::fs::write(dir.join("Ana.pdf"), b"").unwrap();
        let mut used = HashSet::new();
        assert_eq!(unique_file_name("Ana", 0, &dir, &mut used), "Ana (2).pdf");
        assert_eq!(unique_file_name("Ana.PDF", 1, &dir, &mut used), "Ana (3).pdf");
        assert_eq!(unique_file_name("a/b:c", 2, &dir, &mut used), "a-b-c.pdf");
        assert_eq!(unique_file_name("  ", 3, &dir, &mut used), "fila-4.pdf");
    }

    /// Template with a name centered at x = 300 and a date on the right.
    fn template() -> (Vec<u8>, Vec<Field>) {
        crate::pdf::init_pdfium(None).unwrap();
        let mut doc = pdfium().unwrap().create_new_pdf().unwrap();
        let size = PdfPagePaperSize::new_custom(PdfPoints::new(600.0), PdfPoints::new(400.0));
        let mut page = doc.pages_mut().create_page_at_end(size).unwrap();
        let font = doc.fonts_mut().times_bold();
        for (text, y) in [("Nombre Apellido", 200.0), ("01/01/2026", 100.0)] {
            let mut object = PdfPageTextObject::new(&doc, text, font, PdfPoints::new(24.0)).unwrap();
            let width = object.bounds().unwrap().to_rect().width().value;
            object
                .translate(PdfPoints::new(300.0 - width / 2.0), PdfPoints::new(y))
                .unwrap();
            page.objects_mut().add_text_object(object).unwrap();
        }
        let fields = vec![
            Field { page: 0, objects: vec![0], column: 0, align: Align::Center, shrink: false, qr: None },
            Field { page: 0, objects: vec![1], column: 1, align: Align::Right, shrink: false, qr: None },
        ];
        (doc.save_to_bytes().unwrap(), fields)
    }

    fn texts(doc: &PdfDocument) -> Vec<(String, f32, f32)> {
        let page = doc.pages().get(0).unwrap();
        let objects = page.objects();
        (0..objects.len())
            .filter_map(|i| {
                let o = objects.get(i).ok()?;
                let r = o.bounds().ok()?.to_rect();
                Some((o.as_text_object()?.text(), r.left().value, r.right().value))
            })
            .collect()
    }

    #[test]
    fn fill_keeps_alignment() {
        let (template, fields) = template();
        let original = texts(&load(template.clone(), None).unwrap());
        let row = vec!["Ana".to_string(), "26/08/2026".to_string()];
        let (doc, replaced) = fill(&template, &fields, &row, &mut HashMap::new()).unwrap();
        assert!(!replaced);
        let filled = texts(&doc);
        let find = |t: &str| filled.iter().find(|(text, ..)| text == t).cloned().unwrap();

        // Centered: the center doesn't move.
        let (_, l, r) = find("Ana");
        assert!(((l + r) / 2.0 - 300.0).abs() < 0.5, "centro {}", (l + r) / 2.0);
        // Right-aligned: the right edge doesn't move.
        let (_, _, r) = find("26/08/2026");
        assert!((r - original[1].2).abs() < 0.5, "derecha {r} vs {}", original[1].2);
    }

    #[test]
    fn long_values_shrink_to_fit_the_page() {
        let (template, mut fields) = template();
        fields[0].shrink = true;
        let long = "María de los Ángeles Fernández González de la Concepción".to_string();
        let row = vec![long.clone(), "1".to_string()];
        let (doc, _) = fill(&template, &fields, &row, &mut HashMap::new()).unwrap();
        let (_, l, r) = texts(&doc).into_iter().find(|(t, ..)| *t == long).unwrap();
        // Fits the page (600 wide) with margin and stays centered.
        assert!(l >= SHRINK_MARGIN - 0.5 && r <= 600.0 - SHRINK_MARGIN + 0.5, "{l} {r}");
        assert!(((l + r) / 2.0 - 300.0).abs() < 0.5);

        // Without the option, it runs off the page.
        fields[0].shrink = false;
        let (doc, _) = fill(&template, &fields, &row, &mut HashMap::new()).unwrap();
        let (_, l, r) = texts(&doc).into_iter().find(|(t, ..)| *t == long).unwrap();
        assert!(l < 0.0 && r > 600.0, "{l} {r}");
    }

    #[test]
    fn qr_codes_can_be_read() {
        let (template, mut fields) = template();
        let rect = RelRect {
            from: crate::pdf::RelPoint { x: 0.75, y: 0.05 },
            to: crate::pdf::RelPoint { x: 0.95, y: 0.35 },
        };
        fields.push(Field { page: 0, objects: vec![], column: 2, align: Align::Left, shrink: false, qr: Some(rect) });
        let url = "https://ejemplo.com/constancia?id=12345&nombre=Ana";
        let row = vec!["Ana".to_string(), "1".to_string(), url.to_string()];
        let (doc, _) = fill(&template, &fields, &row, &mut HashMap::new()).unwrap();
        let image = render(&doc.pages().get(0).unwrap(), 1200).unwrap();
        let gray = image::DynamicImage::ImageRgba8(image).to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(gray);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, content) = grids[0].decode().unwrap();
        assert_eq!(content, url);
        // Texts are still replaced correctly.
        assert!(texts(&doc).iter().any(|(t, ..)| t == "Ana"));
    }

    #[test]
    fn generate_files_and_single_pdf() {
        let (template, fields) = template();
        let rows: Vec<Vec<String>> = [["Ana", "1"], ["Ana", "2"], ["Luis", "3"]]
            .iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect();

        let dir = tmp_dir("generate");
        let names = rows.iter().map(|r| r[0].clone()).collect();
        let mut progress = Vec::new();
        let output = Output::Files { dir: dir.to_string_lossy().into(), names };
        let result = generate(&template, &fields, &rows, output, |d, t| progress.push((d, t))).unwrap();
        assert_eq!(result.generated, 3);
        assert_eq!(progress.last(), Some(&(3, 3)));
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["Ana (2).pdf", "Ana.pdf", "Luis.pdf"]);
        let luis = pdfium().unwrap().load_pdf_from_file(&dir.join("Luis.pdf"), None).unwrap();
        assert!(texts(&luis).iter().any(|(t, ..)| t == "Luis"));

        let single = dir.join("todas.pdf");
        let output = Output::Single { path: single.to_string_lossy().into() };
        generate(&template, &fields, &rows, output, |_, _| {}).unwrap();
        let combined = pdfium().unwrap().load_pdf_from_file(&single, None).unwrap();
        assert_eq!(combined.pages().len(), 3);
    }

    #[test]
    fn missing_letters_are_reported_per_row() {
        crate::pdf::init_pdfium(None).unwrap();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/subset-font.pdf");
        let template = std::fs::read(path).unwrap();
        let doc = load(template.clone(), None).unwrap();
        let page = doc.pages().get(0).unwrap();
        let objects = page.objects();
        let first_text = (0..objects.len())
            .find(|&i| objects.get(i).unwrap().as_text_object().is_some())
            .unwrap();
        let fields = vec![Field { page: 0, objects: vec![first_text], column: 0, align: Align::Left, shrink: false, qr: None }];
        let rows = vec![vec!["Hola".to_string()], vec!["Ñandú".to_string()]];
        let dir = tmp_dir("missing");
        let output = Output::Files { dir: dir.to_string_lossy().into(), names: vec![] };
        let result = generate(&template, &fields, &rows, output, |_, _| {}).unwrap();
        assert_eq!(result.font_replaced_rows, [1]);
    }
}
