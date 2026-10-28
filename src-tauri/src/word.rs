//! Export to Word (.docx): the PDF text in paragraphs, with approximate font size, bold
//! and centering, and a page break for each page. Images and tables aren't
//! rebuilt: it's for reusing or fixing the text, not for copying the layout.

use std::io::{Cursor, Write};
use std::path::Path;

use pdfium_render::prelude::*;
use tauri::State;

use crate::pdf::{err, get_page, AppState};

/// A line of the PDF.
#[derive(Debug)]
struct Line {
    text: String,
    size: f32,
    bold: bool,
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

#[derive(Debug, PartialEq)]
struct Paragraph {
    text: String,
    size: f32,
    bold: bool,
    centered: bool,
}

fn page_lines(page: &PdfPage) -> Result<Vec<Line>, String> {
    let text = page.text().map_err(err)?;
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut sizes: Vec<f32> = Vec::new();
    let mut bold_chars = 0;
    let mut rect: Option<(f32, f32, f32, f32)> = None;

    let mut flush = |current: &mut String, sizes: &mut Vec<f32>, bold_chars: &mut usize, rect: &mut Option<(f32, f32, f32, f32)>| {
        let trimmed = current.trim();
        if let (false, Some((left, bottom, right, top))) = (trimmed.is_empty(), *rect) {
            sizes.sort_by(f32::total_cmp);
            let size = sizes.get(sizes.len() / 2).copied().unwrap_or(12.0);
            let letters = trimmed.chars().filter(|c| !c.is_whitespace()).count().max(1);
            lines.push(Line {
                text: trimmed.to_string(),
                size,
                bold: *bold_chars * 2 > letters,
                top,
                bottom,
                left,
                right,
            });
        }
        current.clear();
        sizes.clear();
        *bold_chars = 0;
        *rect = None;
    };

    for ch in text.chars().iter() {
        let c = ch.unicode_char().unwrap_or(' ');
        if c == '\n' || c == '\r' {
            flush(&mut current, &mut sizes, &mut bold_chars, &mut rect);
            continue;
        }
        current.push(if c.is_whitespace() { ' ' } else { c });
        if c.is_whitespace() {
            continue;
        }
        if let Ok(b) = ch.loose_bounds() {
            let (l, bt, r, t) = (b.left().value, b.bottom().value, b.right().value, b.top().value);
            rect = Some(match rect {
                None => (l, bt, r, t),
                Some((l0, b0, r0, t0)) => (l0.min(l), b0.min(bt), r0.max(r), t0.max(t)),
            });
        }
        sizes.push(ch.scaled_font_size().value);
        let weight_bold = matches!(
            ch.font_weight(),
            Some(PdfFontWeight::Weight600 | PdfFontWeight::Weight700Bold | PdfFontWeight::Weight800 | PdfFontWeight::Weight900)
        );
        let name = ch.font_name().to_lowercase();
        if weight_bold || name.contains("bold") || name.contains("black") || name.contains("heavy") {
            bold_chars += 1;
        }
    }
    flush(&mut current, &mut sizes, &mut bold_chars, &mut rect);
    Ok(lines)
}

/// Joins consecutive lines with the same style into paragraphs.
fn paragraphs(lines: &[Line], page_width: f32) -> Vec<Paragraph> {
    let centered = |l: &Line| {
        let center = (l.left + l.right) / 2.0;
        (center - page_width / 2.0).abs() < page_width * 0.04 && l.right - l.left < page_width * 0.7
    };
    let mut out: Vec<Paragraph> = Vec::new();
    let mut previous: Option<&Line> = None;
    for line in lines {
        let joins = previous.is_some_and(|p| {
            let height = (p.top - p.bottom).max(line.top - line.bottom).max(1.0);
            let gap = p.bottom - line.top;
            (p.size - line.size).abs() < 1.0
                && p.bold == line.bold
                && centered(p) == centered(line)
                && gap < height * 0.6
                && gap > -height * 0.5
                // A short line ending in a period usually closes the paragraph.
                && !(p.text.ends_with(['.', ':', '!', '?']) && p.right - p.left < page_width * 0.6)
                // A line without trailing punctuation followed by one starting with an uppercase letter or
                // digit: it's a list or loose lines, not a paragraph.
                && !(p.text.ends_with(char::is_alphanumeric)
                    && line.text.starts_with(|c: char| c.is_uppercase() || c.is_numeric()))
        });
        match out.last_mut() {
            Some(paragraph) if joins => {
                // Word split with a hyphen at the end of the line.
                if paragraph.text.ends_with('-') && line.text.starts_with(char::is_lowercase) {
                    paragraph.text.pop();
                } else {
                    paragraph.text.push(' ');
                }
                paragraph.text.push_str(&line.text);
            }
            _ => out.push(Paragraph {
                text: line.text.clone(),
                size: line.size,
                bold: line.bold,
                centered: centered(line),
            }),
        }
        previous = Some(line);
    }
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Control characters: not valid in XML.
            c if (c as u32) < 0x20 && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn paragraph_xml(p: &Paragraph, page_break: bool) -> String {
    let half_points = (p.size * 2.0).round().clamp(8.0, 400.0) as u32;
    let mut xml = String::from("<w:p><w:pPr>");
    if page_break {
        xml.push_str("<w:pageBreakBefore/>");
    }
    if p.centered {
        xml.push_str(r#"<w:jc w:val="center"/>"#);
    }
    xml.push_str("</w:pPr><w:r><w:rPr>");
    if p.bold {
        xml.push_str("<w:b/>");
    }
    xml.push_str(&format!(r#"<w:sz w:val="{half_points}"/><w:szCs w:val="{half_points}"/>"#));
    xml.push_str(&format!(r#"</w:rPr><w:t xml:space="preserve">{}</w:t></w:r></w:p>"#, escape(&p.text)));
    xml
}

fn document_xml(pages: &[Vec<Paragraph>], page_size: (f32, f32)) -> String {
    let mut body = String::new();
    for (i, paragraphs) in pages.iter().enumerate() {
        if paragraphs.is_empty() {
            // Page without text (e.g. scanned): left empty to keep the page numbering.
            body.push_str(if i > 0 { "<w:p><w:pPr><w:pageBreakBefore/></w:pPr></w:p>" } else { "<w:p/>" });
        }
        for (j, p) in paragraphs.iter().enumerate() {
            body.push_str(&paragraph_xml(p, i > 0 && j == 0));
        }
    }
    let (w, h) = ((page_size.0 * 20.0) as u32, (page_size.1 * 20.0) as u32);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:sectPr><w:pgSz w:w="{w}" w:h="{h}"/><w:pgMar w:top="1134" w:right="1134" w:bottom="1134" w:left="1134" w:header="0" w:footer="0" w:gutter="0"/></w:sectPr></w:body></w:document>"#
    )
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

fn docx(document: &str) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", RELS),
        ("word/document.xml", document),
    ] {
        zip.start_file(name, options).map_err(err)?;
        zip.write_all(content.as_bytes()).map_err(err)?;
    }
    Ok(zip.finish().map_err(err)?.into_inner())
}

impl AppState {
    /// Saves the document's text as .docx. Returns how many pages had no text.
    pub fn export_word(&self, path: String) -> Result<usize, String> {
        let (pages, size) = self.with_doc(|open| {
            let mut pages = Vec::new();
            let mut size = (595.0, 842.0);
            for index in 0..open.doc.pages().len() {
                let page = get_page(open, index)?;
                if index == 0 {
                    size = (page.width().value, page.height().value);
                }
                pages.push(paragraphs(&page_lines(&page)?, page.width().value));
            }
            Ok((pages, size))
        })?;
        let empty = pages.iter().filter(|p| p.is_empty()).count();
        let bytes = docx(&document_xml(&pages, size))?;
        std::fs::write(Path::new(&path), bytes).map_err(|e| format!("no se pudo guardar {path}: {e}"))?;
        Ok(empty)
    }
}

#[tauri::command]
pub async fn export_word(path: String, state: State<'_, AppState>) -> Result<usize, String> {
    state.export_word(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::tests::state_with_lines;
    use std::io::Read;

    fn line(text: &str, top: f32, size: f32) -> Line {
        Line { text: text.into(), size, bold: false, top, bottom: top - size, left: 50.0, right: 500.0 }
    }

    #[test]
    fn lines_become_paragraphs() {
        let lines = [
            line("Primer renglón de un párrafo que se cor-", 700.0, 12.0),
            line("ta y sigue acá.", 686.0, 12.0),
            line("Otro párrafo después de un espacio.", 650.0, 12.0),
            line("Título", 600.0, 20.0),
        ];
        let ps = paragraphs(&lines, 600.0);
        assert_eq!(ps.len(), 3, "{ps:?}");
        // Loose lines (a list) aren't joined.
        let list = [line("Nombre: Ana", 700.0, 12.0), line("Fecha 2026", 686.0, 12.0), line("Total 10", 672.0, 12.0)];
        assert_eq!(paragraphs(&list, 600.0).len(), 3);
        assert_eq!(ps[0].text, "Primer renglón de un párrafo que se corta y sigue acá.");
        assert_eq!(ps[2].size, 20.0);
        assert_eq!(escape("a<b & \"c\""), "a&lt;b &amp; &quot;c&quot;");
    }

    #[test]
    fn exports_a_valid_docx() {
        let state = state_with_lines(&[&["Hola & chau", "Segunda línea"], &["Página dos"]]);
        let path = std::env::temp_dir().join(format!("pliego-word-{}.docx", std::process::id()));
        let empty = state.export_word(path.to_string_lossy().into()).unwrap();
        assert_eq!(empty, 0);

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut xml = String::new();
        archive.by_name("word/document.xml").unwrap().read_to_string(&mut xml).unwrap();
        assert!(xml.contains("Hola &amp; chau"), "{xml}");
        assert!(xml.contains("Página dos"));
        assert_eq!(xml.matches("<w:pageBreakBefore/>").count(), 1);
        assert!(archive.by_name("[Content_Types].xml").is_ok());
    }
}
