//! Watermark removal. Watermarks are found by how they look and repeat, since pdfium-render
//! doesn't expose the "Watermark" marked-content tags:
//! - "Watermark" annotations (the kind some editors add on top of the page);
//! - text, images or groups drawn diagonally, semi-transparent or as large light-gray text;
//! - the same text, image or group repeated in the top or bottom margin of most pages
//!   (headers, footers, logos).
//! The person chooses which candidates to remove; repeated-only ones (headers, logos) start
//! unchecked. A watermark baked into a scanned image can't be removed this way.

use std::collections::{BTreeMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use pdfium_render::prelude::*;
use serde::Serialize;
use tauri::ipc::Response;
use tauri::State;

use crate::pdf::{encode_png, err, get_page, load, rect_to_rel, render, AppState, DocInfo, OpenDoc, RelRect};

/// Something that looks like a watermark: every object with the same `key` across the document.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct WatermarkCandidate {
    key: String,
    label: String,
    /// Pages where it appears (0-based).
    pages: Vec<PdfPageIndex>,
    /// Where it is on the first of those pages.
    rect: RelRect,
    /// Diagonal, semi-transparent or a watermark annotation: checked by default.
    likely: bool,
}

/// Identity of a page object for grouping, with a readable label and whether it looks like a
/// watermark on its own.
struct Signature {
    key: String,
    label: String,
    likely: bool,
}

const MAX_LABEL: usize = 40;

fn quoted(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > MAX_LABEL {
        format!("«{}…»", text.chars().take(MAX_LABEL).collect::<String>())
    } else {
        format!("«{text}»")
    }
}

/// Drawn at an angle that isn't a multiple of 90° (more than 3° off).
fn diagonal(object: &PdfPageObject) -> bool {
    object.matrix().is_ok_and(|m| {
        let angle = m.b().atan2(m.a()).to_degrees().rem_euclid(90.0);
        angle > 3.0 && angle < 87.0
    })
}

/// Large text in a light color, like a gray "BORRADOR" behind the content.
fn faint_text(text: &PdfPageTextObject) -> bool {
    text.scaled_font_size().value >= 24.0
        && text.fill_color().is_ok_and(|c| {
            let luminance = 0.299 * c.red() as f32 + 0.587 * c.green() as f32 + 0.114 * c.blue() as f32;
            luminance > 190.0
        })
}

/// Fraction of the page height that counts as top or bottom margin.
const MARGIN: f32 = 0.10;

/// Text of the objects inside a group, and how many there are.
fn group_contents(form: &PdfPageXObjectFormObject) -> (String, usize) {
    let mut text = String::new();
    let mut count = 0;
    for object in form.iter() {
        count += 1;
        if let Some(t) = object.as_text_object() {
            text.push_str(t.text().trim());
            text.push(' ');
        }
    }
    (text.trim().to_string(), count)
}

fn signature(page: &PdfPage, object: &PdfPageObject) -> Option<Signature> {
    let bounds = object.bounds().ok()?.to_rect();
    let page_area = page.width().value * page.height().value;
    let covered = bounds.width().value * bounds.height().value / page_area.max(1.0);
    let likely = diagonal(object) || object.has_transparency() || object.as_text_object().is_some_and(faint_text);
    // Something covering most of the page and not see-through is content (e.g. a scanned
    // page), never a watermark.
    if covered > 0.6 && !likely {
        return None;
    }
    let (kind, label) = match object.object_type() {
        PdfPageObjectType::Text => {
            let t = object.as_text_object()?;
            let text = t.text();
            if text.trim().is_empty() {
                return None;
            }
            (format!("t|{}|{:.0}", text.trim(), t.scaled_font_size().value), quoted(&text))
        }
        PdfPageObjectType::Image => {
            let image = object.as_image_object()?;
            // The content is hashed so that different images of the same size (e.g. scanned
            // pages) aren't taken for one repeated image.
            let mut hasher = DefaultHasher::new();
            image.get_raw_image_data().ok()?.hash(&mut hasher);
            let (w, h) = (image.width().ok()?, image.height().ok()?);
            (format!("i|{w}x{h}|{:x}", hasher.finish()), format!("Imagen de {w} × {h} px"))
        }
        PdfPageObjectType::XObjectForm => {
            let (text, count) = group_contents(object.as_x_object_form_object()?);
            let label = if text.is_empty() {
                format!("Grupo de {count} elementos")
            } else {
                quoted(&text)
            };
            (format!("f|{count}|{text}"), label)
        }
        // Lines and shapes repeat on every page (table borders, rules) and are rarely a watermark.
        _ => return None,
    };
    if likely {
        // Watermarks are grouped by content alone: they can shift a little between pages.
        return Some(Signature { key: kind, label, likely });
    }
    // Anything else only counts if it's in the top or bottom margin (where headers, footers
    // and logos go), so repeated phrases of the body aren't proposed.
    let cy = (bounds.bottom().value + bounds.top().value) / 2.0 / page.height().value;
    let zone = if cy > 1.0 - MARGIN {
        "top"
    } else if cy < MARGIN {
        "bottom"
    } else {
        return None;
    };
    // Left, center or right third, so the same text in two places of the margin stays apart.
    let third = ((bounds.left().value + bounds.right().value) / 2.0 / page.width().value * 3.0).floor();
    Some(Signature {
        key: format!("{kind}|{zone}{third}"),
        label,
        likely,
    })
}

const ANNOTATION_KEY: &str = "a|watermark";

fn is_watermark_annotation(annotation: &PdfPageAnnotation) -> bool {
    annotation.annotation_type() == PdfPageAnnotationType::Watermark
}

/// Watermark candidates of the whole document, the likeliest first.
fn find(open: &OpenDoc) -> Result<Vec<WatermarkCandidate>, String> {
    let total = open.doc.pages().len();
    // BTreeMap: same order every time for the same document.
    let mut found: BTreeMap<String, WatermarkCandidate> = BTreeMap::new();
    let mut add = |key: String, label: String, likely: bool, page: PdfPageIndex, rect: RelRect| {
        let entry = found.entry(key.clone()).or_insert(WatermarkCandidate {
            key,
            label,
            pages: Vec::new(),
            rect,
            likely: false,
        });
        entry.likely |= likely;
        if entry.pages.last() != Some(&page) {
            entry.pages.push(page);
        }
    };
    for index in 0..total {
        let page = get_page(open, index)?;
        for object in page.objects().iter() {
            if let Some(s) = signature(&page, &object) {
                let rect = rect_to_rel(&page, object.bounds().map_err(err)?.to_rect())?;
                add(s.key, s.label, s.likely, index, rect);
            }
        }
        for annotation in page.annotations().iter() {
            if is_watermark_annotation(&annotation) {
                let rect = rect_to_rel(&page, annotation.bounds().map_err(err)?)?;
                add(ANNOTATION_KEY.into(), "Marca de agua (anotación)".into(), true, index, rect);
            }
        }
    }
    let repeated = |c: &WatermarkCandidate| c.pages.len() >= 2 && c.pages.len() * 2 >= total as usize;
    let mut candidates: Vec<_> = found.into_values().filter(|c| c.likely || repeated(c)).collect();
    candidates.sort_by_key(|c| (!c.likely, std::cmp::Reverse(c.pages.len())));
    candidates.truncate(30);
    Ok(candidates)
}

/// Removes from the page every object and annotation whose key is in `keys`.
/// Returns how many were removed.
fn remove_from_page(page: &mut PdfPage<'static>, keys: &HashSet<String>) -> Result<usize, String> {
    // Decide first: removing objects changes the indices.
    let hits: Vec<PdfPageObjectIndex> = page
        .objects()
        .iter()
        .enumerate()
        .filter(|(_, o)| signature(page, o).is_some_and(|s| keys.contains(&s.key)))
        .map(|(i, _)| i as PdfPageObjectIndex)
        .collect();
    for &index in hits.iter().rev() {
        page.objects_mut().remove_object_at_index(index).map_err(err)?;
    }
    let mut removed = hits.len();
    if keys.contains(ANNOTATION_KEY) {
        let annotations: Vec<PdfPageAnnotationIndex> = page
            .annotations()
            .iter()
            .enumerate()
            .filter(|(_, a)| is_watermark_annotation(a))
            .map(|(i, _)| i as PdfPageAnnotationIndex)
            .collect();
        for &i in annotations.iter().rev() {
            let annotations = page.annotations_mut();
            let annotation = annotations.get(i).map_err(err)?;
            annotations.delete_annotation(annotation).map_err(err)?;
        }
        removed += annotations.len();
    }
    if !hits.is_empty() {
        page.regenerate_content().map_err(err)?;
    }
    Ok(removed)
}

impl AppState {
    pub fn find_watermarks(&self) -> Result<Vec<WatermarkCandidate>, String> {
        self.with_doc(|open| find(open))
    }

    pub fn remove_watermarks(&self, keys: Vec<String>) -> Result<DocInfo, String> {
        let keys: HashSet<String> = keys.into_iter().collect();
        if keys.is_empty() {
            return Err("elige al menos una marca de agua".into());
        }
        self.edit(|open| {
            let mut removed = 0;
            for index in 0..open.doc.pages().len() {
                removed += remove_from_page(&mut get_page(open, index)?, &keys)?;
            }
            if removed == 0 {
                return Err("no se encontró la marca de agua (¿cambió el documento?)".into());
            }
            Ok(())
        })
    }

    /// Renders a page as it would look without the chosen watermarks, on a copy of the document.
    pub fn preview_watermarks(&self, keys: Vec<String>, index: PdfPageIndex, width: u32) -> Result<Vec<u8>, String> {
        let keys: HashSet<String> = keys.into_iter().collect();
        let image = self.with_doc(|open| {
            let copy = load(open.doc.save_to_bytes().map_err(err)?, open.password.as_deref())?;
            let mut page = copy.pages().get(index).map_err(err)?;
            remove_from_page(&mut page, &keys)?;
            render(&page, width)
        })?;
        encode_png(&image)
    }
}

#[tauri::command]
pub async fn find_watermarks(state: State<'_, AppState>) -> Result<Vec<WatermarkCandidate>, String> {
    state.find_watermarks()
}

#[tauri::command]
pub async fn remove_watermarks(keys: Vec<String>, state: State<'_, AppState>) -> Result<DocInfo, String> {
    state.remove_watermarks(keys)
}

#[tauri::command]
pub async fn preview_watermarks(
    keys: Vec<String>,
    index: PdfPageIndex,
    width: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    Ok(Response::new(state.preview_watermarks(keys, index, width)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::tests::state_with_lines;

    /// Three pages of body text, each with a diagonal, semi-transparent "BORRADOR" in the middle
    /// and the same footer at the bottom.
    fn document() -> AppState {
        let state = state_with_lines(&[&["Primera página"], &["Segunda página"], &["Tercera página"]]);
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica_bold();
                for index in 0..3 {
                    let mut page = get_page(open, index)?;
                    let mut mark = PdfPageTextObject::new(&open.doc, "BORRADOR", font, PdfPoints::new(60.0)).map_err(err)?;
                    mark.set_fill_color(PdfColor::new(200, 0, 0, 90)).map_err(err)?;
                    mark.rotate_counter_clockwise_degrees(45.0).map_err(err)?;
                    mark.translate(PdfPoints::new(90.0), PdfPoints::new(180.0)).map_err(err)?;
                    page.objects_mut().add_text_object(mark).map_err(err)?;
                    let mut footer = PdfPageTextObject::new(&open.doc, "Empresa S.A.", font, PdfPoints::new(9.0)).map_err(err)?;
                    footer.translate(PdfPoints::new(50.0), PdfPoints::new(30.0)).map_err(err)?;
                    page.objects_mut().add_text_object(footer).map_err(err)?;
                    page.regenerate_content().map_err(err)?;
                }
                Ok(())
            })
            .unwrap();
        state
    }

    fn page_text(state: &AppState, index: PdfPageIndex) -> String {
        state.with_doc(|o| Ok(get_page(o, index)?.text().map_err(err)?.all())).unwrap()
    }

    #[test]
    fn finds_diagonal_and_repeated_objects() {
        let state = document();
        let found = state.find_watermarks().unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        // The watermark comes first and is checked by default; the footer only repeats.
        assert_eq!(found[0].label, "«BORRADOR»");
        assert!(found[0].likely);
        assert_eq!(found[0].pages, [0, 1, 2]);
        assert_eq!(found[1].label, "«Empresa S.A.»");
        assert!(!found[1].likely);
        // Body text that changes from page to page isn't a candidate.
        assert!(found.iter().all(|c| !c.label.contains("página")));
    }

    #[test]
    fn removes_only_the_chosen_watermark_and_can_be_undone() {
        let state = document();
        let key = state.find_watermarks().unwrap().remove(0).key;

        // The preview doesn't touch the document.
        let png = state.preview_watermarks(vec![key.clone()], 0, 300).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(page_text(&state, 0).contains("BORRADOR"));

        state.remove_watermarks(vec![key.clone()]).unwrap();
        for index in 0..3 {
            let text = page_text(&state, index);
            assert!(!text.contains("BORRADOR"), "{text}");
            assert!(text.contains("página") && text.contains("Empresa"), "{text}");
        }
        assert_eq!(state.find_watermarks().unwrap().len(), 1);
        // Nothing left to remove with the same key.
        assert!(state.remove_watermarks(vec![key]).is_err());

        state.undo().unwrap();
        assert!(page_text(&state, 1).contains("BORRADOR"));
    }

    #[test]
    fn repeated_body_text_is_not_proposed() {
        // The third line is in the body (outside the top margin) on every page.
        let state = state_with_lines(&[&["A1", "B1", "Ver anexo."], &["A2", "B2", "Ver anexo."], &["A3", "B3", "Ver anexo."]]);
        let found = state.find_watermarks().unwrap();
        assert!(found.iter().all(|c| !c.label.contains("anexo")), "{found:?}");
    }

    #[test]
    fn large_light_gray_text_is_a_watermark() {
        let state = state_with_lines(&[&["Contrato"]]);
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica_bold();
                let mut page = get_page(open, 0)?;
                let mut mark = PdfPageTextObject::new(&open.doc, "BORRADOR", font, PdfPoints::new(72.0)).map_err(err)?;
                mark.set_fill_color(PdfColor::new(220, 220, 220, 255)).map_err(err)?;
                mark.translate(PdfPoints::new(40.0), PdfPoints::new(280.0)).map_err(err)?;
                page.objects_mut().add_text_object(mark).map_err(err)?;
                page.regenerate_content().map_err(err)
            })
            .unwrap();
        let found = state.find_watermarks().unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].likely && found[0].label == "«BORRADOR»");
    }

    #[test]
    fn a_single_plain_page_has_no_candidates() {
        let state = state_with_lines(&[&["Hola", "Chau"]]);
        assert!(state.find_watermarks().unwrap().is_empty());
    }
}
