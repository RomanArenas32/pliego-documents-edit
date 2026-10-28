use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::ImageEncoder;
use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::State;

use crate::history::History;

/// Pdfium is initialized once and lives as long as the app, so open documents
/// can be `PdfDocument<'static>` and be stored in Tauri's state.
static PDFIUM: OnceLock<Pdfium> = OnceLock::new();

/// Maximum number of undo steps kept in memory.

/// Reference width (in pixels) for converting relative coordinates to PDF points.
pub(crate) const REF_WIDTH: Pixels = 10_000;

pub struct OpenDoc {
    pub(crate) doc: PdfDocument<'static>,
    pub(crate) path: PathBuf,
    history: History,
    /// Password the document was opened with (needed to reload the copies).
    pub(crate) password: Option<String>,
}

impl OpenDoc {
    /// Replaces the document with another version (undo, redo, failed edit…).
    pub(crate) fn reload(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.doc = load(bytes, self.password.as_deref())?;
        Ok(())
    }

    /// The document as a PDF without a password, for tools that read it on their own.
    pub(crate) fn plain_bytes(&self) -> Result<Vec<u8>, String> {
        let bytes = self.doc.save_to_bytes().map_err(err)?;
        match &self.password {
            Some(password) => crate::forms::decrypted(&bytes, password),
            None => Ok(bytes),
        }
    }
}

pub struct AppState {
    pub doc: Mutex<Option<OpenDoc>>,
}

#[derive(Serialize)]
pub struct PageSize {
    width: f32,
    height: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    path: String,
    pages: Vec<PageSize>,
    can_undo: bool,
    can_redo: bool,
}

/// Point relative to the page as displayed on screen:
/// (0, 0) is the top-left corner and (1, 1) the bottom-right.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RelPoint {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

/// A line of text; `x`/`y` mark the start of the baseline.
#[derive(Deserialize)]
pub struct TextLine {
    text: String,
    x: f32,
    y: f32,
}

/// Rectangle in view-relative coordinates.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RelRect {
    pub(crate) from: RelPoint,
    pub(crate) to: RelPoint,
}

/// Text added with the text tool, ready to be edited again.
#[derive(Serialize)]
pub struct TextInfo {
    lines: Vec<String>,
    size: f32,
    color: String,
    /// Start of the baseline of the first line.
    anchor: RelPoint,
}

#[derive(Serialize)]
pub struct AnnotationHit {
    pub(crate) annotation: PdfPageAnnotationIndex,
    pub(crate) rect: RelRect,
    /// Present only if the annotation is an editable text.
    pub(crate) text: Option<TextInfo>,
    /// It's an image or signature added with the app (can be moved and resized).
    pub(crate) image: bool,
    /// Text of the sticky note, if the annotation is a note.
    pub(crate) note: Option<String>,
}

/// Original text fragment of the page: a line (or part of one) with the same
/// font, size and color. Many PDFs store each letter as a separate object, so
/// a fragment can span several objects.
#[derive(Serialize)]
pub struct TextObjectHit {
    /// Object indices, left to right.
    objects: Vec<PdfPageObjectIndex>,
    text: String,
    rect: RelRect,
    /// Effective font size, in points.
    size: f32,
    color: String,
    font: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEditResult {
    doc: DocInfo,
    /// The original font didn't have every letter and another one was used.
    font_replaced: bool,
}

/// Looks for `libpdfium` in the bundle resources and, in development, in `src-tauri/pdfium`.
pub fn init_pdfium(resource_dir: Option<PathBuf>) -> Result<(), String> {
    if PDFIUM.get().is_some() {
        return Ok(());
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = resource_dir {
        candidates.push(dir.join("pdfium"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pdfium"));

    let mut last_err = String::from("no se encontró libpdfium");
    for dir in candidates {
        let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
        if !lib.exists() {
            continue;
        }
        match Pdfium::bind_to_library(&lib) {
            Ok(bindings) => {
                crate::forms::set_library_path(lib.clone());
                let _ = PDFIUM.set(Pdfium::new(bindings));
                return Ok(());
            }
            Err(e) => last_err = format!("{}: {e}", lib.display()),
        }
    }
    Err(last_err)
}

pub(crate) fn pdfium() -> Result<&'static Pdfium, String> {
    PDFIUM.get().ok_or_else(|| "pdfium no está inicializado".into())
}

/// pdfium crashes (the app closes) if two threads use it at once, even with
/// different documents. Every use of pdfium, including dropping a document, happens with
/// this lock held. Order: first this lock, then the open document's lock.
static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn pdfium_lock() -> MutexGuard<'static, ()> {
    // If another thread panicked while holding the lock, pdfium is still usable.
    PDFIUM_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(crate) fn load(bytes: Vec<u8>, password: Option<&str>) -> Result<PdfDocument<'static>, String> {
    pdfium()?
        .load_pdf_from_byte_vec(bytes, password)
        .map_err(|e| match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                "el PDF está protegido con contraseña".into()
            }
            e => err(e),
        })
}

pub(crate) fn doc_info(open: &OpenDoc) -> DocInfo {
    let pages = open
        .doc
        .pages()
        .iter()
        .map(|p| PageSize {
            width: p.width().value,
            height: p.height().value,
        })
        .collect();
    DocInfo {
        path: open.path.to_string_lossy().into_owned(),
        pages,
        can_undo: open.history.can_undo(),
        can_redo: open.history.can_redo(),
    }
}



pub(crate) fn get_page(open: &OpenDoc, index: PdfPageIndex) -> Result<PdfPage<'static>, String> {
    open.doc.pages().get(index).map_err(err)
}

pub(crate) fn render(page: &PdfPage, width: u32) -> Result<image::RgbaImage, String> {
    let config = PdfRenderConfig::new()
        .set_target_width(width.clamp(16, 8000) as Pixels)
        .render_form_data(true)
        .render_annotations(true);
    Ok(page
        .render_with_config(&config)
        .map_err(err)?
        .as_image()
        .map_err(err)?
        .to_rgba8())
}

pub(crate) fn encode_png(image: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new_with_quality(Cursor::new(&mut png), CompressionType::Fast, FilterType::Sub)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(err)?;
    Ok(png)
}

/// Converts a view-relative point to PDF points, honoring the page rotation.
pub(crate) fn to_points(page: &PdfPage, p: RelPoint) -> Result<(f32, f32), String> {
    let height = REF_WIDTH as f32 * page.height().value / page.width().value;
    let config = PdfRenderConfig::new().set_target_width(REF_WIDTH);
    let (x, y) = page
        .pixels_to_points(
            (p.x * REF_WIDTH as f32).round() as Pixels,
            (p.y * height).round() as Pixels,
            &config,
        )
        .map_err(err)?;
    Ok((x.value, y.value))
}

/// Rectangle in PDF points from two opposite corners in relative coordinates.
pub(crate) fn to_rect(page: &PdfPage, a: RelPoint, b: RelPoint) -> Result<PdfRect, String> {
    let (ax, ay) = to_points(page, a)?;
    let (bx, by) = to_points(page, b)?;
    Ok(PdfRect::new_from_values(
        ay.min(by),
        ax.min(bx),
        ay.max(by),
        ax.max(bx),
    ))
}

pub(crate) fn expand(rect: PdfRect, by: f32) -> PdfRect {
    PdfRect::new_from_values(
        rect.bottom().value - by,
        rect.left().value - by,
        rect.top().value + by,
        rect.right().value + by,
    )
}

/// Converts PDF points to view-relative coordinates (inverse of `to_points`).
pub(crate) fn to_rel(page: &PdfPage, x: f32, y: f32) -> Result<RelPoint, String> {
    let height = REF_WIDTH as f32 * page.height().value / page.width().value;
    let config = PdfRenderConfig::new().set_target_width(REF_WIDTH);
    let (px, py) = page
        .points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), &config)
        .map_err(err)?;
    Ok(RelPoint {
        x: px as f32 / REF_WIDTH as f32,
        y: py as f32 / height,
    })
}

pub(crate) fn rect_to_rel(page: &PdfPage, rect: PdfRect) -> Result<RelRect, String> {
    let a = to_rel(page, rect.left().value, rect.top().value)?;
    let b = to_rel(page, rect.right().value, rect.bottom().value)?;
    Ok(RelRect {
        from: RelPoint {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
        },
        to: RelPoint {
            x: a.x.max(b.x),
            y: a.y.max(b.y),
        },
    })
}

pub(crate) fn contains(rect: PdfRect, x: f32, y: f32, tolerance: f32) -> bool {
    x >= rect.left().value - tolerance
        && x <= rect.right().value + tolerance
        && y >= rect.bottom().value - tolerance
        && y <= rect.top().value + tolerance
}

/// Index of the topmost markup annotation at point (x, y).
fn hit_annotation(page: &PdfPage, x: f32, y: f32) -> Option<PdfPageAnnotationIndex> {
    use PdfPageAnnotationType::*;

    let annotations = page.annotations();
    // Iterate back to front: the last annotation is the one drawn on top.
    (0..annotations.len()).rev().find(|&i| {
        annotations.get(i).is_ok_and(|a| {
            matches!(
                a.annotation_type(),
                Ink | Highlight
                    | Square
                    | Circle
                    | Stamp
                    | FreeText
                    | Text
                    | Underline
                    | Strikeout
                    | Squiggly
                    | Line
                    | Polygon
                    | Polyline
                    | Caret
            ) && a.bounds().is_ok_and(|r| contains(r, x, y, 3.0))
        })
    })
}

/// Reads the text of an annotation created with `insert_text`: a stamp that only holds text.
fn text_info(page: &PdfPage, annotation: &PdfPageAnnotation) -> Option<TextInfo> {
    if annotation.annotation_type() != PdfPageAnnotationType::Stamp {
        return None;
    }
    let objects = annotation.objects();
    let all_text = (0..objects.len())
        .all(|i| objects.get(i).is_ok_and(|o| o.as_text_object().is_some()));
    let first = objects.get(0).ok()?;
    let first = first.as_text_object().filter(|_| all_text)?;
    let (x, y) = first.get_translation();
    // Pdfium can't extract the text of objects inside an annotation,
    // so the lines are read from the `contents` field that `insert_text` stores.
    let lines: Vec<String> = annotation.contents()?.split('\n').map(String::from).collect();
    if lines.len() != objects.len() {
        return None;
    }
    Some(TextInfo {
        lines,
        size: first.unscaled_font_size().value,
        color: color_hex(&first.fill_color().ok()?),
        anchor: to_rel(page, x.value, y.value).ok()?,
    })
}

/// Index of the topmost visible text object at point (x, y).
fn hit_text_object(page: &PdfPage, x: f32, y: f32) -> Option<PdfPageObjectIndex> {
    let objects = page.objects();
    (0..objects.len()).rev().find(|&i| {
        objects.get(i).is_ok_and(|o| {
            o.as_text_object().is_some_and(|t| t.is_visible())
                && o.bounds().is_ok_and(|q| contains(q.to_rect(), x, y, 1.0))
        })
    })
}

/// Data of a text object needed to group it with its neighbors.
struct Glyph {
    index: PdfPageObjectIndex,
    left: f32,
    right: f32,
    baseline: f32,
    size: f32,
    font: String,
    color: (u8, u8, u8),
}

fn glyph(index: PdfPageObjectIndex, object: &PdfPageObject) -> Option<Glyph> {
    let text = object.as_text_object().filter(|t| t.is_visible())?;
    let rect = object.bounds().ok()?.to_rect();
    let color = text.fill_color().unwrap_or(PdfColor::BLACK);
    Some(Glyph {
        index,
        left: rect.left().value,
        right: rect.right().value,
        baseline: text.get_vertical_translation().value,
        size: text.scaled_font_size().value,
        font: text.font().name(),
        color: (color.red(), color.green(), color.blue()),
    })
}

/// Text objects next to the `hit` object on the same line and with the same style,
/// sorted left to right.
fn text_run(page: &PdfPage, hit: PdfPageObjectIndex) -> Result<Vec<Glyph>, String> {
    let objects = page.objects();
    let target = glyph(hit, &objects.get(hit).map_err(err)?).ok_or("el objeto no es texto")?;
    let mut line: Vec<Glyph> = (0..objects.len())
        .filter_map(|i| glyph(i, &objects.get(i).ok()?))
        .filter(|g| {
            (g.baseline - target.baseline).abs() < 0.5
                && (g.size - target.size).abs() < 0.05
                && g.font == target.font
                && g.color == target.color
        })
        .collect();
    line.sort_by(|a, b| a.left.total_cmp(&b.left));

    // Two objects stay in the same fragment if the gap between them is under
    // ~1.5 spaces; a larger gap separates columns or different blocks.
    let max_gap = target.size * 0.6;
    let at = line.iter().position(|g| g.index == hit).unwrap_or(0);
    let mut start = at;
    while start > 0 && line[start].left - line[start - 1].right <= max_gap {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < line.len() && line[end + 1].left - line[end].right <= max_gap {
        end += 1;
    }
    Ok(line.drain(start..=end).collect())
}

/// Text of a fragment. Some PDFs don't store spaces as characters, so
/// one is added when there's a visible gap between two objects.
fn run_text(page: &PdfPage, run: &[Glyph]) -> Result<String, String> {
    let page_text = page.text().map_err(err)?;
    let objects = page.objects();
    let mut text = String::new();
    let mut prev_right: Option<f32> = None;
    for g in run {
        let object = objects.get(g.index).map_err(err)?;
        let piece = page_text.for_object(object.as_text_object().ok_or("el objeto no es texto")?);
        if let Some(prev) = prev_right {
            let gap = g.left - prev;
            let has_space = text.ends_with(char::is_whitespace) || piece.starts_with(char::is_whitespace);
            if gap > g.size * 0.28 && !has_space {
                text.push(' ');
            }
        }
        text.push_str(&piece);
        prev_right = Some(prev_right.map_or(g.right, |p| p.max(g.right)));
    }
    Ok(text.trim_end().to_string())
}

/// References to page objects; they stay valid even if other objects are removed.
pub(crate) fn objects_at(
    page: &mut PdfPage<'static>,
    indices: &[PdfPageObjectIndex],
) -> Result<Vec<PdfPageObject<'static>>, String> {
    indices
        .iter()
        .map(|&i| page.objects_mut().get(i).map_err(err))
        .collect()
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Replaces the text of a fragment (objects left to right): the whole text
/// goes into the first object and the rest are removed. If the embedded font lacks some
/// of the new letters, it's recreated with another font (see `fonts::font_for`). The result is aligned
/// to the width the original fragment took. An empty text deletes the fragment.
///
/// `objects` is obtained with `objects_at` before modifying the page, so that removing
/// objects of one field doesn't change the indices of another.
/// `known` keeps the known letters of each font between calls.
/// Returns `true` if the font had to be changed.
fn set_text_color(objects: &mut [PdfPageObject<'static>], color: PdfColor) -> Result<(), String> {
    for object in objects {
        if object.as_text_object().is_none() {
            return Err("el objeto no es texto".into());
        }
        // The stroke too: some texts are drawn with an outline (e.g. faux bold).
        object.set_fill_color(color).map_err(err)?;
        object.set_stroke_color(color).map_err(err)?;
    }
    Ok(())
}

pub(crate) fn replace_run(
    doc: &mut PdfDocument<'static>,
    page: &mut PdfPage<'static>,
    mut objects: Vec<PdfPageObject<'static>>,
    text: &str,
    align: Align,
    max_width: Option<f32>,
    known: &mut HashMap<String, HashSet<char>>,
) -> Result<bool, String> {
    if objects.is_empty() || objects.iter().any(|o| o.as_text_object().is_none()) {
        return Err("el objeto no es texto".into());
    }
    let (mut left, mut right) = (f32::MAX, f32::MIN);
    for o in &objects {
        let r = o.bounds().map_err(err)?.to_rect();
        left = left.min(r.left().value);
        right = right.max(r.right().value);
    }

    // Pdfium fails with empty texts, so emptying a fragment deletes it.
    if text.trim().is_empty() {
        for o in objects {
            page.objects_mut().remove_object(o).map_err(err)?;
        }
        return Ok(false);
    }

    let mut first = objects.remove(0);
    for o in objects {
        page.objects_mut().remove_object(o).map_err(err)?;
    }
    let (embedded, font_name, matrix, size, color) = {
        let original = first.as_text_object().ok_or("el objeto no es texto")?;
        let font = original.font();
        (
            font.is_embedded().unwrap_or(false),
            font.name(),
            original.matrix().map_err(err)?,
            original.unscaled_font_size(),
            original.fill_color().unwrap_or(PdfColor::BLACK),
        )
    };
    let missing = embedded && {
        let chars = known
            .entry(font_name.clone())
            .or_insert_with_key(|name| known_chars(doc, name));
        text.chars().any(|c| !c.is_whitespace() && !chars.contains(&c))
    };

    let mut result = if missing {
        page.objects_mut().remove_object(first).map_err(err)?;
        let font = crate::fonts::font_for(doc, Some(&font_name), text);
        let mut replacement = PdfPageTextObject::new(doc, text, font, size).map_err(err)?;
        replacement.set_fill_color(color).map_err(err)?;
        replacement.apply_matrix(matrix).map_err(err)?;
        page.objects_mut()
            .add_text_object(replacement)
            .map_err(err)?
    } else {
        first
            .as_text_object_mut()
            .ok_or("el objeto no es texto")?
            .set_text(text)
            .map_err(err)?;
        first
    };

    // If it doesn't fit the allowed width, it's shrunk (without moving it off its line).
    if let Some(max) = max_width {
        let r = result.bounds().map_err(err)?.to_rect();
        let width = r.width().value;
        if width > max && max > 0.0 {
            let f = max / width;
            let (x0, y0) = (r.left().value, (r.bottom().value + r.top().value) / 2.0);
            result
                .as_text_object_mut()
                .ok_or("el objeto no es texto")?
                .apply_matrix(PdfMatrix::new(f, 0.0, 0.0, f, x0 - f * x0, y0 - f * y0))
                .map_err(err)?;
        }
    }

    let r = result.bounds().map_err(err)?.to_rect();
    let dx = match align {
        Align::Left => left - r.left().value,
        Align::Center => (left + right - r.left().value - r.right().value) / 2.0,
        Align::Right => right - r.right().value,
    };
    if dx.abs() > 0.01 {
        result
            .as_text_object_mut()
            .ok_or("el objeto no es texto")?
            .translate(PdfPoints::new(dx), PdfPoints::new(0.0))
            .map_err(err)?;
    }
    page.regenerate_content().map_err(err)?;
    Ok(missing)
}

/// Letters an embedded font is known to have: the ones already used with it in the
/// document. Embedded fonts are usually subset to just those letters.
fn known_chars(doc: &PdfDocument, font_name: &str) -> HashSet<char> {
    let mut chars = HashSet::new();
    for page in doc.pages().iter() {
        let Ok(text) = page.text() else { continue };
        for c in text.chars().iter() {
            if c.font_name() == font_name {
                chars.extend(c.unicode_char());
            }
        }
    }
    chars
}

pub(crate) fn color_hex(color: &PdfColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red(), color.green(), color.blue())
}

/// Adds text as a stamp annotation, so it can be moved, edited or deleted
/// without touching the page's original content.
fn insert_text(
    doc: &mut PdfDocument<'static>,
    page: &mut PdfPage<'static>,
    lines: &[TextLine],
    size: f32,
    color: PdfColor,
) -> Result<(), String> {
    let all: String = lines.iter().map(|l| l.text.as_str()).collect();
    let font = crate::fonts::font_for(doc, None, &all);
    let degrees = rotation_degrees(page);

    let mut objects = Vec::new();
    let mut bounds: Option<PdfRect> = None;
    for line in lines {
        let (x, y) = to_points(page, RelPoint { x: line.x, y: line.y })?;
        // Empty lines are stored as a space to keep the line spacing.
        let text = if line.text.is_empty() { " " } else { &line.text };
        let mut object =
            PdfPageTextObject::new(doc, text, font, PdfPoints::new(size)).map_err(err)?;
        object.set_fill_color(color).map_err(err)?;
        // Rotated opposite to the page so the text reads upright.
        if degrees != 0.0 {
            object
                .rotate_counter_clockwise_degrees(degrees)
                .map_err(err)?;
        }
        object
            .translate(PdfPoints::new(x), PdfPoints::new(y))
            .map_err(err)?;

        let rect = object.bounds().map_err(err)?.to_rect();
        bounds = Some(match bounds {
            None => rect,
            Some(b) => PdfRect::new_from_values(
                b.bottom().value.min(rect.bottom().value),
                b.left().value.min(rect.left().value),
                b.top().value.max(rect.top().value),
                b.right().value.max(rect.right().value),
            ),
        });
        objects.push(object);
    }
    let bounds = bounds.ok_or("el texto está vacío")?;

    let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    let mut annotation = page
        .annotations_mut()
        .create_stamp_annotation()
        .map_err(err)?;
    annotation.set_bounds(expand(bounds, 2.0)).map_err(err)?;
    annotation.set_contents(&text.join("\n")).map_err(err)?;
    for object in objects {
        annotation
            .objects_mut()
            .add_text_object(object)
            .map_err(err)?;
    }
    Ok(())
}

/// Removes trailing empty lines; `None` if no text is left.
fn trim_lines(mut lines: Vec<TextLine>) -> Option<Vec<TextLine>> {
    while lines.last().is_some_and(|l| l.text.trim().is_empty()) {
        lines.pop();
    }
    (!lines.is_empty()).then_some(lines)
}

pub(crate) fn parse_color(hex: &str) -> Result<PdfColor, String> {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.len() == 6)
        .ok_or_else(|| format!("color inválido: #{hex}"))?;
    Ok(PdfColor::new(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
        255,
    ))
}

pub(crate) fn rotation_degrees(page: &PdfPage) -> f32 {
    match page.rotation() {
        Ok(PdfPageRenderRotation::Degrees90) => 90.0,
        Ok(PdfPageRenderRotation::Degrees180) => 180.0,
        Ok(PdfPageRenderRotation::Degrees270) => 270.0,
        _ => 0.0,
    }
}

impl AppState {
    pub(crate) fn with_doc<T>(
        &self,
        f: impl FnOnce(&mut OpenDoc) -> Result<T, String>,
    ) -> Result<T, String> {
        let _pdfium = pdfium_lock();
        let mut guard = self.doc.lock().map_err(err)?;
        let open = guard.as_mut().ok_or("no hay documento abierto")?;
        f(open)
    }

    /// Runs an edit, saving a copy first so it can be undone.
    /// If the edit fails, the document goes back to the previous state.
    pub(crate) fn edit(
        &self,
        f: impl FnOnce(&mut OpenDoc) -> Result<(), String>,
    ) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let snapshot = open.doc.save_to_bytes().map_err(err)?;
            if let Err(e) = f(open) {
                open.reload(snapshot)?;
                return Err(e);
            }
            open.history.record(snapshot);
            Ok(doc_info(open))
        })
    }

    // ---------- Document ----------

    pub fn open_pdf(&self, path: String, password: Option<String>) -> Result<DocInfo, String> {
        // Loaded into memory so the same file can be overwritten when saving.
        let bytes = std::fs::read(&path).map_err(err)?;
        let _pdfium = pdfium_lock();
        let open = OpenDoc {
            doc: load(bytes, password.as_deref())?,
            path: PathBuf::from(path),
            history: History::default(),
            password,
        };
        let info = doc_info(&open);
        *self.doc.lock().map_err(err)? = Some(open);
        Ok(info)
    }

    /// Saves the document. Without `path`, overwrites the original file.
    pub fn save_pdf(&self, path: Option<String>) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let target = path.map(PathBuf::from).unwrap_or_else(|| open.path.clone());
            let bytes = open.doc.save_to_bytes().map_err(err)?;
            std::fs::write(&target, bytes).map_err(err)?;
            open.path = target;
            Ok(doc_info(open))
        })
    }

    /// Renders a page to PNG at the given width in pixels.
    pub fn render_page(&self, index: PdfPageIndex, width: u32) -> Result<Vec<u8>, String> {
        let image = self.with_doc(|open| render(&get_page(open, index)?, width))?;
        // Encoding happens outside the lock so other commands aren't blocked.
        encode_png(&image)
    }

    pub fn undo(&self) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let current = open.doc.save_to_bytes().map_err(err)?;
            let previous = open.history.undo(current)?;
            open.reload(previous)?;
            Ok(doc_info(open))
        })
    }

    pub fn redo(&self) -> Result<DocInfo, String> {
        self.with_doc(|open| {
            let current = open.doc.save_to_bytes().map_err(err)?;
            let next = open.history.redo(current)?;
            open.reload(next)?;
            Ok(doc_info(open))
        })
    }

    // ---------- Pages ----------

    pub fn rotate_page(&self, index: PdfPageIndex, clockwise: bool) -> Result<DocInfo, String> {
        use PdfPageRenderRotation::*;

        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let next = match (page.rotation().map_err(err)?, clockwise) {
                (None, true) | (Degrees180, false) => Degrees90,
                (Degrees90, true) | (Degrees270, false) => Degrees180,
                (Degrees180, true) | (None, false) => Degrees270,
                (Degrees270, true) | (Degrees90, false) => None,
            };
            page.set_rotation(next);
            Ok(())
        })
    }

    pub fn delete_page(&self, index: PdfPageIndex) -> Result<DocInfo, String> {
        self.edit(|open| {
            if open.doc.pages().len() <= 1 {
                return Err("el documento debe tener al menos una página".into());
            }
            get_page(open, index)?.delete().map_err(err)
        })
    }

    /// Reorders the pages. `order[i]` is the current index of the page that moves to position `i`.
    pub fn reorder_pages(&self, order: Vec<PdfPageIndex>) -> Result<DocInfo, String> {
        self.edit(|open| {
            let count = open.doc.pages().len();
            let mut seen = vec![false; count as usize];
            let valid = order.len() == count as usize
                && order.iter().all(|&i| {
                    (0..count).contains(&i) && !std::mem::replace(&mut seen[i as usize], true)
                });
            if !valid {
                return Err("orden de páginas inválido".into());
            }

            // Pdfium can't move pages within a document, so all of them are copied
            // in the new order into a new document. A single import shares the
            // resources (fonts, images) between pages instead of duplicating them.
            let range = order
                .iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut doc = pdfium()?.create_new_pdf().map_err(err)?;
            doc.pages_mut()
                .copy_pages_from_document(&open.doc, &range, 0)
                .map_err(err)?;
            open.doc = doc;
            Ok(())
        })
    }

    /// Inserts all the pages of another PDF at position `at`.
    pub fn insert_pdf(&self, path: String, at: PdfPageIndex) -> Result<DocInfo, String> {
        let bytes = std::fs::read(&path).map_err(err)?;
        self.edit(|open| {
            let other = load(bytes, None)?;
            let count = other.pages().len();
            let at = at.clamp(0, open.doc.pages().len());
            open.doc
                .pages_mut()
                .copy_page_range_from_document(&other, 0..=count - 1, at)
                .map_err(err)
        })
    }

    /// Inserts a blank page the same size as page `like`.
    pub fn insert_blank_page(
        &self,
        at: PdfPageIndex,
        like: PdfPageIndex,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let model = get_page(open, like)?;
            let size = PdfPagePaperSize::new_custom(model.width(), model.height());
            let at = at.clamp(0, open.doc.pages().len());
            open.doc
                .pages_mut()
                .create_page_at_index(size, at)
                .map(|_| ())
                .map_err(err)
        })
    }

    // ---------- Annotations ----------

    /// Adds text as a stamp annotation, so it can be deleted or undone
    /// without touching the page's original content.
    pub fn add_text(
        &self,
        index: PdfPageIndex,
        lines: Vec<TextLine>,
        size: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let lines = trim_lines(lines).ok_or("el texto está vacío")?;
            let mut page = get_page(open, index)?;
            insert_text(&mut open.doc, &mut page, &lines, size, color)
        })
    }

    /// Replaces a text added with the app (to edit or move it).
    /// With no lines, deletes it.
    pub fn update_text(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
        lines: Vec<TextLine>,
        size: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            {
                let old = page.annotations().get(annotation).map_err(err)?;
                if text_info(&page, &old).is_none() {
                    return Err("la anotación no es un texto".into());
                }
            }
            let annotations = page.annotations_mut();
            let old = annotations.get(annotation).map_err(err)?;
            annotations.delete_annotation(old).map_err(err)?;
            match trim_lines(lines) {
                Some(lines) => insert_text(&mut open.doc, &mut page, &lines, size, color),
                None => Ok(()),
            }
        })
    }

    /// Markup annotation at the given point, if any.
    pub fn annotation_at(
        &self,
        index: PdfPageIndex,
        at: RelPoint,
    ) -> Result<Option<AnnotationHit>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let Some(i) = hit_annotation(&page, x, y) else {
                return Ok(None);
            };
            let annotation = page.annotations().get(i).map_err(err)?;
            Ok(Some(AnnotationHit {
                annotation: i,
                rect: rect_to_rel(&page, annotation.bounds().map_err(err)?)?,
                text: text_info(&page, &annotation),
                image: crate::images::is_image_stamp(&annotation),
                note: crate::markup::note_text(&annotation),
            }))
        })
    }

    pub fn delete_annotation(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let annotations = page.annotations_mut();
            let target = annotations.get(annotation).map_err(err)?;
            annotations.delete_annotation(target).map_err(err)
        })
    }

    /// Original text fragment of the page at the given point, if any.
    pub fn text_object_at(
        &self,
        index: PdfPageIndex,
        at: RelPoint,
    ) -> Result<Option<TextObjectHit>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let Some(hit) = hit_text_object(&page, x, y) else {
                return Ok(None);
            };
            let run = text_run(&page, hit)?;
            let objects = page.objects();
            let mut bounds: Option<PdfRect> = None;
            for g in &run {
                let r = objects.get(g.index).map_err(err)?.bounds().map_err(err)?.to_rect();
                bounds = Some(match bounds {
                    None => r,
                    Some(b) => PdfRect::new_from_values(
                        b.bottom().value.min(r.bottom().value),
                        b.left().value.min(r.left().value),
                        b.top().value.max(r.top().value),
                        b.right().value.max(r.right().value),
                    ),
                });
            }
            let first = &run[0];
            Ok(Some(TextObjectHit {
                text: run_text(&page, &run)?,
                rect: rect_to_rel(&page, bounds.ok_or("fragmento vacío")?)?,
                size: first.size,
                color: format!(
                    "#{:02x}{:02x}{:02x}",
                    first.color.0, first.color.1, first.color.2
                ),
                font: first.font.clone(),
                objects: run.iter().map(|g| g.index).collect(),
            }))
        })
    }

    /// Changes the color of an original text fragment.
    pub fn recolor_text_object(
        &self,
        index: PdfPageIndex,
        objects: Vec<PdfPageObjectIndex>,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            set_text_color(&mut objects_at(&mut page, &objects)?, color)?;
            page.regenerate_content().map_err(err)
        })
    }

    /// Moves an original text fragment by the offset between two points.
    pub fn move_text_object(
        &self,
        index: PdfPageIndex,
        objects: Vec<PdfPageObjectIndex>,
        from: RelPoint,
        to: RelPoint,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let (x0, y0) = to_points(&page, from)?;
            let (x1, y1) = to_points(&page, to)?;
            for &object in &objects {
                page.objects_mut()
                    .get(object)
                    .map_err(err)?
                    .as_text_object_mut()
                    .ok_or("el objeto no es texto")?
                    .translate(PdfPoints::new(x1 - x0), PdfPoints::new(y1 - y0))
                    .map_err(err)?;
            }
            page.regenerate_content().map_err(err)
        })
    }

    /// Changes the content of an original text fragment (see `replace_run`).
    pub fn edit_text_object(
        &self,
        index: PdfPageIndex,
        objects: Vec<PdfPageObjectIndex>,
        text: String,
        color: Option<String>,
    ) -> Result<TextEditResult, String> {
        let mut font_replaced = false;
        let doc = self.edit(|open| {
            let mut page = get_page(open, index)?;
            let mut known = HashMap::new();
            let mut objects = objects_at(&mut page, &objects)?;
            // replace_run keeps the objects' color, even when switching to Helvetica.
            if let Some(color) = color {
                set_text_color(&mut objects, parse_color(&color)?)?;
            }
            font_replaced = replace_run(
                &mut open.doc,
                &mut page,
                objects,
                &text,
                Align::Left,
                None,
                &mut known,
            )?;
            Ok(())
        })?;
        Ok(TextEditResult { doc, font_replaced })
    }

    pub fn add_highlight(
        &self,
        index: PdfPageIndex,
        from: RelPoint,
        to: RelPoint,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let r = to_rect(&page, from, to)?;
            let (l, b, rt, t) = (r.left().value, r.bottom().value, r.right().value, r.top().value);

            let mut annotation = page
                .annotations_mut()
                .create_highlight_annotation()
                .map_err(err)?;
            // Pdfium expects the points in order: top-left, top-right, bottom-left, bottom-right.
            // (`PdfQuadPoints::from_rect` uses another order and produces a "bow tie".)
            annotation
                .attachment_points_mut()
                .create_attachment_point_at_end(PdfQuadPoints::new_from_values(
                    l, t, rt, t, l, b, rt, b,
                ))
                .map_err(err)?;
            annotation.set_bounds(r).map_err(err)?;
            annotation.set_stroke_color(color).map_err(err)
        })
    }

    /// Unfilled rectangle.
    pub fn add_rect(
        &self,
        index: PdfPageIndex,
        from: RelPoint,
        to: RelPoint,
        width: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let rect = to_rect(&page, from, to)?;
            let path = PdfPagePathObject::new_rect(
                &open.doc,
                rect,
                Some(color),
                Some(PdfPoints::new(width)),
                None,
            )
            .map_err(err)?;
            add_drawing(&mut page, rect, width, color, path)
        })
    }

    /// Freehand stroke.
    pub fn add_ink(
        &self,
        index: PdfPageIndex,
        points: Vec<RelPoint>,
        width: f32,
        color: String,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let points = points
                .into_iter()
                .map(|p| to_points(&page, p))
                .collect::<Result<Vec<_>, _>>()?;
            let (&(x0, y0), rest) = points.split_first().ok_or("el trazo está vacío")?;

            let mut path = PdfPagePathObject::new(
                &open.doc,
                PdfPoints::new(x0),
                PdfPoints::new(y0),
                Some(color),
                Some(PdfPoints::new(width)),
                None,
            )
            .map_err(err)?;
            // A click without movement is drawn as a dot.
            let rest = if rest.is_empty() {
                &[(x0 + 0.01, y0)][..]
            } else {
                rest
            };
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (x0, y0, x0, y0);
            for &(x, y) in rest {
                path.line_to(PdfPoints::new(x), PdfPoints::new(y))
                    .map_err(err)?;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
            path.set_line_cap(PdfPageObjectLineCap::Round)
                .map_err(err)?;
            path.set_line_join(PdfPageObjectLineJoin::Round)
                .map_err(err)?;

            let bounds = PdfRect::new_from_values(min_y, min_x, max_y, max_x);
            add_drawing(&mut page, bounds, width, color, path)
        })
    }

    /// Deletes the topmost markup annotation at the given point.
    pub fn erase_at(&self, index: PdfPageIndex, at: RelPoint) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            let i = hit_annotation(&page, x, y).ok_or("no hay ninguna anotación en ese punto")?;
            let annotations = page.annotations_mut();
            let target = annotations.get(i).map_err(err)?;
            annotations.delete_annotation(target).map_err(err)
        })
    }
}

/// Adds a drawing as an "ink" annotation, so it can be deleted without touching the
/// page's original content.
fn add_drawing(
    page: &mut PdfPage<'static>,
    bounds: PdfRect,
    width: f32,
    color: PdfColor,
    path: PdfPagePathObject<'static>,
) -> Result<(), String> {
    let mut annotation = page
        .annotations_mut()
        .create_ink_annotation()
        .map_err(err)?;
    // Bounds must be set before adding the stroke or Pdfium doesn't draw it.
    annotation
        .set_bounds(expand(bounds, width))
        .map_err(err)?;
    annotation.set_stroke_color(color).map_err(err)?;
    annotation
        .objects_mut()
        .add_path_object(path)
        .map(|_| ())
        .map_err(err)
}

// ---------- Tauri commands ----------

#[tauri::command]
pub async fn open_pdf(
    path: String,
    password: Option<String>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.open_pdf(path, password)
}

#[tauri::command]
pub async fn save_pdf(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.save_pdf(path)
}

#[tauri::command]
pub async fn render_page(
    index: PdfPageIndex,
    width: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    Ok(Response::new(state.render_page(index, width)?))
}

#[tauri::command]
pub async fn undo(
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.undo()
}

#[tauri::command]
pub async fn redo(
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.redo()
}

#[tauri::command]
pub async fn rotate_page(
    index: PdfPageIndex,
    clockwise: bool,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.rotate_page(index, clockwise)
}

#[tauri::command]
pub async fn delete_page(
    index: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.delete_page(index)
}

#[tauri::command]
pub async fn reorder_pages(
    order: Vec<PdfPageIndex>,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.reorder_pages(order)
}

#[tauri::command]
pub async fn insert_pdf(
    path: String,
    at: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.insert_pdf(path, at)
}

#[tauri::command]
pub async fn insert_blank_page(
    at: PdfPageIndex,
    like: PdfPageIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.insert_blank_page(at, like)
}

#[tauri::command]
pub async fn add_text(
    index: PdfPageIndex,
    lines: Vec<TextLine>,
    size: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_text(index, lines, size, color)
}

#[tauri::command]
pub async fn add_highlight(
    index: PdfPageIndex,
    from: RelPoint,
    to: RelPoint,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_highlight(index, from, to, color)
}

#[tauri::command]
pub async fn add_rect(
    index: PdfPageIndex,
    from: RelPoint,
    to: RelPoint,
    width: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_rect(index, from, to, width, color)
}

#[tauri::command]
pub async fn add_ink(
    index: PdfPageIndex,
    points: Vec<RelPoint>,
    width: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_ink(index, points, width, color)
}

#[tauri::command]
pub async fn erase_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.erase_at(index, at)
}

#[tauri::command]
pub async fn update_text(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    lines: Vec<TextLine>,
    size: f32,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.update_text(index, annotation, lines, size, color)
}

#[tauri::command]
pub async fn annotation_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<Option<AnnotationHit>, String> {
    state.annotation_at(index, at)
}

#[tauri::command]
pub async fn delete_annotation(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.delete_annotation(index, annotation)
}

#[tauri::command]
pub async fn text_object_at(
    index: PdfPageIndex,
    at: RelPoint,
    state: State<'_, AppState>,
) -> Result<Option<TextObjectHit>, String> {
    state.text_object_at(index, at)
}

#[tauri::command]
pub async fn move_text_object(
    index: PdfPageIndex,
    objects: Vec<PdfPageObjectIndex>,
    from: RelPoint,
    to: RelPoint,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.move_text_object(index, objects, from, to)
}

#[tauri::command]
pub async fn recolor_text_object(
    index: PdfPageIndex,
    objects: Vec<PdfPageObjectIndex>,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.recolor_text_object(index, objects, color)
}

#[tauri::command]
pub async fn edit_text_object(
    index: PdfPageIndex,
    objects: Vec<PdfPageObjectIndex>,
    text: String,
    color: Option<String>,
    state: State<'_, AppState>,
) -> Result<TextEditResult, String> {
    state.edit_text_object(index, objects, text, color)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const RED: &str = "#e01e1e";

    /// Document with lines of text in Helvetica 14 (one below the other, from the top).
    pub(crate) fn state_with_lines(pages: &[&[&str]]) -> AppState {
        let state = state_with_pages(pages.len());
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica();
                for (index, lines) in pages.iter().enumerate() {
                    let mut page = get_page(open, index as PdfPageIndex)?;
                    for (i, line) in lines.iter().enumerate() {
                        let mut object = PdfPageTextObject::new(&open.doc, line, font, PdfPoints::new(14.0))
                            .map_err(err)?;
                        object
                            .translate(PdfPoints::new(50.0), PdfPoints::new(550.0 - 30.0 * i as f32))
                            .map_err(err)?;
                        page.objects_mut().add_text_object(object).map_err(err)?;
                    }
                }
                Ok(())
            })
            .unwrap();
        state
    }

    /// In-memory test document; each page has a different width to identify it.
    pub(crate) fn state_with_pages(count: usize) -> AppState {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| init_pdfium(None).unwrap());
        let mut doc = pdfium().unwrap().create_new_pdf().unwrap();
        for i in 0..count {
            let size = PdfPagePaperSize::new_custom(
                PdfPoints::new(400.0 + i as f32 * 10.0),
                PdfPoints::new(600.0),
            );
            doc.pages_mut().create_page_at_end(size).unwrap();
        }
        AppState {
            doc: Mutex::new(Some(OpenDoc {
                doc,
                path: PathBuf::new(),
                history: History::default(),
                password: None,
            })),
        }
    }

    fn widths(info: &DocInfo) -> Vec<f32> {
        info.pages.iter().map(|p| p.width.round()).collect()
    }

    fn info(state: &AppState) -> DocInfo {
        state.with_doc(|open| Ok(doc_info(open))).unwrap()
    }

    /// Non-white pixels of the rendered page.
    fn inked_pixels(state: &AppState, index: PdfPageIndex) -> usize {
        state
            .with_doc(|open| {
                let image = get_page(open, index)?
                    .render_with_config(&PdfRenderConfig::new().set_target_width(400))
                    .map_err(err)?
                    .as_image()
                    .map_err(err)?
                    .to_rgba8();
                Ok(image
                    .pixels()
                    .filter(|p| p[0] < 200 || p[1] < 200 || p[2] < 200)
                    .count())
            })
            .unwrap()
    }

    fn p(x: f32, y: f32) -> RelPoint {
        RelPoint { x, y }
    }

    #[test]
    fn rotation_updates_page_size() {
        let state = state_with_pages(1);
        let info = state.rotate_page(0, true).unwrap();
        assert!(info.pages[0].width > info.pages[0].height);
        let info = state.rotate_page(0, false).unwrap();
        assert!(info.pages[0].width < info.pages[0].height);
    }

    #[test]
    fn reorder_delete_and_history() {
        let state = state_with_pages(3);
        assert_eq!(widths(&info(&state)), [400.0, 410.0, 420.0]);

        let info1 = state.reorder_pages(vec![2, 0, 1]).unwrap();
        assert_eq!(widths(&info1), [420.0, 400.0, 410.0]);
        assert!(state.reorder_pages(vec![0, 0, 1]).is_err());
        assert!(state.reorder_pages(vec![0, 1]).is_err());

        let info2 = state.delete_page(0).unwrap();
        assert_eq!(widths(&info2), [400.0, 410.0]);

        let undone = state.undo().unwrap();
        assert_eq!(widths(&undone), [420.0, 400.0, 410.0]);
        let undone = state.undo().unwrap();
        assert_eq!(widths(&undone), [400.0, 410.0, 420.0]);
        assert!(!undone.can_undo && undone.can_redo);
        assert!(state.undo().is_err());

        let redone = state.redo().unwrap();
        assert_eq!(widths(&redone), [420.0, 400.0, 410.0]);
    }

    #[test]
    fn failed_edit_does_not_touch_history() {
        let state = state_with_pages(1);
        assert!(state.delete_page(0).is_err());
        assert!(!info(&state).can_undo);
    }

    #[test]
    fn insert_blank_page_copies_size() {
        let state = state_with_pages(2);
        let info = state.insert_blank_page(1, 1).unwrap();
        assert_eq!(widths(&info), [400.0, 410.0, 410.0]);
        assert_eq!(inked_pixels(&state, 1), 0);
    }

    #[test]
    fn insert_pdf_from_file() {
        let other = state_with_pages(2);
        let path = std::env::temp_dir().join("pliego-insert-test.pdf");
        other.save_pdf(Some(path.to_string_lossy().into())).unwrap();

        let state = state_with_pages(1);
        let info = state
            .insert_pdf(path.to_string_lossy().into(), 1)
            .unwrap();
        assert_eq!(widths(&info), [400.0, 400.0, 410.0]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn annotations_render_and_erase() {
        let state = state_with_pages(1);
        assert_eq!(inked_pixels(&state, 0), 0);

        state.add_rect(0, p(0.2, 0.2), p(0.6, 0.4), 3.0, RED.into()).unwrap();
        let after_rect = inked_pixels(&state, 0);
        assert!(after_rect > 100, "rectángulo: {after_rect}");

        state
            .add_ink(0, vec![p(0.1, 0.8), p(0.5, 0.9), p(0.9, 0.8)], 4.0, RED.into())
            .unwrap();
        let after_ink = inked_pixels(&state, 0);
        assert!(after_ink > after_rect + 100, "trazo: {after_ink}");

        state
            .add_highlight(0, p(0.1, 0.5), p(0.9, 0.55), "#ffd400".into())
            .unwrap();
        let after_highlight = inked_pixels(&state, 0);
        assert!(after_highlight > after_ink + 1000, "resaltado: {after_highlight}");

        let lines = vec![TextLine { text: "Ñandú, ¿qué tal?".into(), x: 0.1, y: 0.1 }];
        state.add_text(0, lines, 18.0, "#000000".into()).unwrap();
        let after_text = inked_pixels(&state, 0);
        assert!(after_text > after_highlight + 100, "texto: {after_text}");

        // The text is still there after saving and reopening.
        let reloaded = state
            .with_doc(|open| load(open.doc.save_to_bytes().map_err(err)?, None))
            .unwrap();
        assert_eq!(reloaded.pages().get(0).unwrap().annotations().len(), 4);

        // The eraser removes the annotation at the point and leaves the others.
        state.erase_at(0, p(0.5, 0.52)).unwrap();
        assert_eq!(inked_pixels(&state, 0), after_text - (after_highlight - after_ink));
        assert!(state.erase_at(0, p(0.95, 0.05)).is_err());
    }

    #[test]
    fn text_on_rotated_page_reads_upright() {
        let state = state_with_pages(1);
        state.rotate_page(0, true).unwrap();
        let lines = vec![TextLine { text: "Hola mundo".into(), x: 0.1, y: 0.2 }];
        state.add_text(0, lines, 24.0, "#000000".into()).unwrap();

        // In the view, the text must be wider than tall.
        let (w, h) = state
            .with_doc(|open| {
                let image = get_page(open, 0)?
                    .render_with_config(&PdfRenderConfig::new().set_target_width(600))
                    .map_err(err)?
                    .as_image()
                    .map_err(err)?
                    .to_rgba8();
                let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
                for (x, y, px) in image.enumerate_pixels() {
                    if px[0] < 128 {
                        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                    }
                }
                Ok((x1 - x0, y1 - y0))
            })
            .unwrap();
        assert!(w > h * 3, "texto girado: {w}x{h}");
    }

    fn line(text: &str, x: f32, y: f32) -> TextLine {
        TextLine {
            text: text.into(),
            x,
            y,
        }
    }

    fn close(a: RelPoint, b: RelPoint) -> bool {
        (a.x - b.x).abs() < 0.002 && (a.y - b.y).abs() < 0.002
    }

    #[test]
    fn own_text_can_be_read_back_moved_and_deleted() {
        for rotated in [false, true] {
            let state = state_with_pages(1);
            if rotated {
                state.rotate_page(0, true).unwrap();
            }
            let lines = vec![line("Hola", 0.2, 0.3), line("", 0.2, 0.35), line("mundo", 0.2, 0.4)];
            state.add_text(0, lines, 20.0, "#123456".into()).unwrap();

            let hit = state.annotation_at(0, p(0.21, 0.29)).unwrap().expect("sin anotación");
            let text = hit.text.expect("no se reconoció como texto");
            assert_eq!(text.lines, ["Hola", "", "mundo"]);
            assert_eq!(text.size, 20.0);
            assert_eq!(text.color, "#123456");
            assert!(close(text.anchor, p(0.2, 0.3)), "ancla {:?}", (text.anchor.x, text.anchor.y));

            // Moving = replacing at another position.
            let moved = vec![line("Hola", 0.5, 0.6), line("mundo", 0.5, 0.7)];
            state
                .update_text(0, hit.annotation, moved, 20.0, "#123456".into())
                .unwrap();
            assert!(state.annotation_at(0, p(0.21, 0.29)).unwrap().is_none());
            let hit = state.annotation_at(0, p(0.51, 0.59)).unwrap().unwrap();
            assert!(close(hit.text.unwrap().anchor, p(0.5, 0.6)));

            state.delete_annotation(0, hit.annotation).unwrap();
            assert!(state.annotation_at(0, p(0.51, 0.59)).unwrap().is_none());

            // Undoing the deletion brings it back.
            state.undo().unwrap();
            assert!(state.annotation_at(0, p(0.51, 0.59)).unwrap().is_some());
        }
    }

    #[test]
    fn drawings_are_not_editable_text() {
        let state = state_with_pages(1);
        state.add_rect(0, p(0.2, 0.2), p(0.4, 0.4), 2.0, RED.into()).unwrap();
        let hit = state.annotation_at(0, p(0.2, 0.3)).unwrap().unwrap();
        assert!(hit.text.is_none());
        assert!(state
            .update_text(0, hit.annotation, vec![line("x", 0.1, 0.1)], 12.0, RED.into())
            .is_err());
    }

    fn open_fixture() -> AppState {
        let state = state_with_pages(1);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/subset-font.pdf");
        state.open_pdf(path.into(), None).unwrap();
        state
    }

    /// Point near the start of the first text fragment containing `needle`.
    fn find_text(state: &AppState, needle: &str) -> RelPoint {
        state
            .with_doc(|open| {
                let page = get_page(open, 0)?;
                let objects = page.objects();
                for i in 0..objects.len() {
                    let object = objects.get(i).map_err(err)?;
                    if object.as_text_object().is_some_and(|t| t.text().contains(needle)) {
                        let r = rect_to_rel(&page, object.bounds().map_err(err)?.to_rect())?;
                        return Ok(p(r.from.x + 0.01, (r.from.y + r.to.y) / 2.0));
                    }
                }
                Err(format!("no se encontró {needle:?}"))
            })
            .unwrap()
    }

    fn page_text(state: &AppState) -> String {
        state
            .with_doc(|open| Ok(get_page(open, 0)?.text().map_err(err)?.all()))
            .unwrap()
    }

    #[test]
    fn edit_original_text_with_known_letters() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().expect("sin texto");
        assert!(hit.text.contains("Hola"), "{:?}", hit.text);
        assert!(hit.font.contains("Monaco"), "{}", hit.font);

        let before = inked_pixels(&state, 0);
        let new_text = hit.text.replace("Hola", "Hela");
        let result = state.edit_text_object(0, hit.objects, new_text, None).unwrap();
        assert!(!result.font_replaced);
        assert!(page_text(&state).contains("Hela"));
        assert_ne!(inked_pixels(&state, 0), before);

        // Still the same after saving and reopening.
        let path = std::env::temp_dir().join("pliego-edit-test.pdf");
        state.save_pdf(Some(path.to_string_lossy().into())).unwrap();
        state.open_pdf(path.to_string_lossy().into(), None).unwrap();
        assert!(page_text(&state).contains("Hela"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn edit_original_text_with_missing_letters_uses_another_font() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        // The subset font has neither "Ñ" nor "ú".
        let result = state
            .edit_text_object(0, hit.objects, "Ñandú".into(), None)
            .unwrap();
        assert!(result.font_replaced);
        assert!(page_text(&state).contains("Ñandú"));

        let hit = state.text_object_at(0, at).unwrap().expect("se perdió el fragmento");
        assert_eq!(hit.text, "Ñandú");
        // The same font installed on the system (Monaco on macOS) or, if missing, a
        // standard one with the same style (monospaced).
        assert!(hit.font.contains("Monaco") || hit.font.contains("Courier"), "{}", hit.font);
    }

    #[test]
    fn non_latin_text_uses_a_system_font() {
        // Own text in Cyrillic and Greek: Helvetica doesn't have those letters.
        let state = state_with_pages(1);
        let lines = vec![TextLine { text: "Привет мир αβγ".into(), x: 0.1, y: 0.2 }];
        state.add_text(0, lines, 14.0, "#000000".into()).unwrap();
        let hit = state.annotation_at(0, p(0.12, 0.21)).unwrap().expect("sin texto");
        assert_eq!(hit.text.unwrap().lines, ["Привет мир αβγ"]);
        // And it shows (no empty boxes: there's ink where the text goes).
        let inked = state
            .with_doc(|o| {
                let image = render(&get_page(o, 0)?, 400)?;
                Ok(image.pixels().filter(|p| p.0[0] < 128).count())
            })
            .unwrap();
        assert!(inked > 100, "{inked}");

        // Editing original text with letters its font doesn't have.
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        state.edit_text_object(0, hit.objects, "Ωμέγα".into(), None).unwrap();
        assert!(page_text(&state).contains("Ωμέγα"));
    }

    #[test]
    fn move_original_text() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        let to = p(at.x + 0.2, at.y + 0.3);
        state.move_text_object(0, hit.objects.clone(), at, to).unwrap();
        assert!(state.text_object_at(0, at).unwrap().is_none());
        let moved = state.text_object_at(0, to).unwrap().expect("no se movió");
        assert_eq!(moved.text, hit.text);
        assert!((moved.rect.from.x - hit.rect.from.x - 0.2).abs() < 0.002);
        assert!((moved.rect.from.y - hit.rect.from.y - 0.3).abs() < 0.002);
    }

    /// Page with a line written letter by letter, as many generators export it.
    fn state_with_letter_by_letter_text() -> AppState {
        let state = state_with_pages(1);
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica();
                let mut page = get_page(open, 0)?;
                let mut put = |text: &str, x: f32, y: f32, size: f32| -> Result<f32, String> {
                    let mut object = PdfPageTextObject::new(&open.doc, text, font, PdfPoints::new(size))
                        .map_err(err)?;
                    object.translate(PdfPoints::new(x), PdfPoints::new(y)).map_err(err)?;
                    let right = object.bounds().map_err(err)?.to_rect().right().value;
                    page.objects_mut().add_text_object(object).map_err(err)?;
                    Ok(right)
                };
                // "Hola mundo" letter by letter, without storing the space as a character.
                let mut x = 50.0;
                for c in "Hola".chars() {
                    x = put(&c.to_string(), x, 500.0, 12.0)? + 0.5;
                }
                x += 3.0;
                for c in "mundo".chars() {
                    x = put(&c.to_string(), x, 500.0, 12.0)? + 0.5;
                }
                // Another block far away on the same line, and a title with a different size.
                put("Lejos", 300.0, 500.0, 12.0)?;
                put("Titulo", 50.0, 550.0, 20.0)?;
                Ok(())
            })
            .unwrap();
        state
    }

    #[test]
    fn letter_by_letter_text_is_edited_as_one_fragment() {
        let state = state_with_letter_by_letter_text();
        let (x, y) = (60.0 / 400.0, 1.0 - 503.0 / 600.0);
        let hit = state.text_object_at(0, p(x, y)).unwrap().expect("sin texto");
        assert_eq!(hit.text, "Hola mundo");
        assert_eq!(hit.objects.len(), 9);

        state
            .edit_text_object(0, hit.objects, "Chau mundo".into(), None)
            .unwrap();
        let text = page_text(&state);
        assert!(text.contains("Chau mundo"), "{text:?}");
        assert!(text.contains("Lejos") && text.contains("Titulo"), "{text:?}");
        let hit = state.text_object_at(0, p(x, y)).unwrap().unwrap();
        assert_eq!((hit.text.as_str(), hit.objects.len()), ("Chau mundo", 1));
    }

    #[test]
    fn letter_by_letter_text_moves_together() {
        let state = state_with_letter_by_letter_text();
        let at = p(60.0 / 400.0, 1.0 - 503.0 / 600.0);
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        let to = p(at.x, at.y + 0.2);
        state.move_text_object(0, hit.objects, at, to).unwrap();
        let moved = state.text_object_at(0, to).unwrap().expect("no se movió");
        assert_eq!(moved.text, "Hola mundo");
        // "Lejos" stayed in place.
        let far = state.text_object_at(0, p(310.0 / 400.0, at.y)).unwrap().unwrap();
        assert_eq!(far.text, "Lejos");
    }

    #[test]
    fn recolor_original_text() {
        let state = state_with_letter_by_letter_text();
        let at = p(60.0 / 400.0, 1.0 - 503.0 / 600.0);
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        assert_eq!(hit.color, "#000000");
        state.recolor_text_object(0, hit.objects, "#e01e1e".into()).unwrap();
        // Still a single fragment, now red, and the rest didn't change.
        let red = state.text_object_at(0, at).unwrap().unwrap();
        assert_eq!((red.text.as_str(), red.color.as_str()), ("Hola mundo", "#e01e1e"));
        let far = state.text_object_at(0, p(310.0 / 400.0, at.y)).unwrap().unwrap();
        assert_eq!(far.color, "#000000");
        // Renders red.
        let image = state.with_doc(|open| render(&get_page(open, 0)?, 400)).unwrap();
        assert!(image.pixels().any(|px| px[0] > 180 && px[1] < 80 && px[2] < 80));
        state.undo().unwrap();
        assert_eq!(state.text_object_at(0, at).unwrap().unwrap().color, "#000000");
    }

    #[test]
    fn edit_original_text_with_new_color() {
        // With letters from the original font and letters that force Helvetica.
        for new_text in ["Hola", "Ñandú"] {
            let state = open_fixture();
            let at = find_text(&state, "Hola");
            let hit = state.text_object_at(0, at).unwrap().unwrap();
            state
                .edit_text_object(0, hit.objects, new_text.into(), Some("#1d4ed8".into()))
                .unwrap();
            let edited = state.text_object_at(0, at).unwrap().unwrap();
            assert!(edited.text.starts_with(new_text), "{:?}", edited.text);
            assert_eq!(edited.color, "#1d4ed8", "{new_text}");
        }
    }

    #[test]
    fn emptying_original_text_removes_it() {
        let state = open_fixture();
        let at = find_text(&state, "Hola");
        let hit = state.text_object_at(0, at).unwrap().unwrap();
        state.edit_text_object(0, hit.objects, "  ".into(), None).unwrap();
        assert!(!page_text(&state).contains("Hola"));
        state.undo().unwrap();
        assert!(page_text(&state).contains("Hola"));
    }

    #[test]
    fn color_parsing() {
        assert!(parse_color("#ff8800").is_ok());
        assert!(parse_color("ff8800").is_ok());
        assert!(parse_color("#f80").is_err());
        assert!(parse_color("#zzzzzz").is_err());
    }
}
