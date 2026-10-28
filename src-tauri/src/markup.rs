//! Text markup (highlight, underline, strikeout, squiggly) and sticky notes.

use pdfium_render::prelude::*;
use serde::Deserialize;
use tauri::State;

use crate::pdf::{err, get_page, parse_color, to_points, to_rect, AppState, DocInfo, RelPoint, RelRect};

/// Side of a note's icon, in points.
const NOTE_SIZE: f32 = 20.0;

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum MarkupKind {
    Highlight,
    Underline,
    Strikeout,
    Squiggly,
}

/// Adds the lines as the annotation's quad points and sets its bounds and color.
macro_rules! fill_markup {
    ($annotation:expr, $quads:expr, $bounds:expr, $color:expr) => {{
        let mut annotation = $annotation.map_err(err)?;
        for (l, b, r, t) in $quads {
            // Pdfium expects the points in order: top-left, top-right, bottom-left, bottom-right.
            annotation
                .attachment_points_mut()
                .create_attachment_point_at_end(PdfQuadPoints::new_from_values(l, t, r, t, l, b, r, b))
                .map_err(err)?;
        }
        annotation.set_bounds($bounds).map_err(err)?;
        annotation.set_stroke_color($color).map_err(err)?;
    }};
}

impl AppState {
    /// A single annotation covering every line (so it's deleted or undone at once).
    pub fn add_markup(
        &self,
        index: PdfPageIndex,
        rects: Vec<RelRect>,
        kind: MarkupKind,
        color: String,
    ) -> Result<DocInfo, String> {
        if rects.is_empty() {
            return Err("no hay texto seleccionado".into());
        }
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let quads = rects
                .iter()
                .map(|r| {
                    let r = to_rect(&page, r.from, r.to)?;
                    Ok((r.left().value, r.bottom().value, r.right().value, r.top().value))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let bounds = quads.iter().fold(
                PdfRect::new_from_values(f32::MAX, f32::MAX, f32::MIN, f32::MIN),
                |acc, &(l, b, r, t)| {
                    PdfRect::new_from_values(
                        acc.bottom().value.min(b),
                        acc.left().value.min(l),
                        acc.top().value.max(t),
                        acc.right().value.max(r),
                    )
                },
            );
            let annotations = page.annotations_mut();
            match kind {
                MarkupKind::Highlight => {
                    fill_markup!(annotations.create_highlight_annotation(), quads, bounds, color)
                }
                MarkupKind::Underline => {
                    fill_markup!(annotations.create_underline_annotation(), quads, bounds, color)
                }
                MarkupKind::Strikeout => {
                    fill_markup!(annotations.create_strikeout_annotation(), quads, bounds, color)
                }
                MarkupKind::Squiggly => {
                    fill_markup!(annotations.create_squiggly_annotation(), quads, bounds, color)
                }
            }
            Ok(())
        })
    }

    /// Sticky note: an icon on the page with text that shows when clicked.
    pub fn add_note(&self, index: PdfPageIndex, at: RelPoint, text: String, color: String) -> Result<DocInfo, String> {
        self.edit(|open| {
            let color = parse_color(&color)?;
            let mut page = get_page(open, index)?;
            let (x, y) = to_points(&page, at)?;
            // The click marks the top-left corner, inside the page.
            let (w, h) = (page.width().value, page.height().value);
            let left = x.clamp(0.0, (w - NOTE_SIZE).max(0.0));
            let top = y.clamp(NOTE_SIZE.min(h), h);
            let mut note = page.annotations_mut().create_text_annotation(&text).map_err(err)?;
            note.set_bounds(PdfRect::new_from_values(top - NOTE_SIZE, left, top, left + NOTE_SIZE))
                .map_err(err)?;
            note.set_stroke_color(color).map_err(err)?;
            Ok(())
        })
    }

    pub fn set_note(&self, index: PdfPageIndex, annotation: PdfPageAnnotationIndex, text: String) -> Result<DocInfo, String> {
        self.edit(|open| {
            let page = get_page(open, index)?;
            let mut note = page.annotations().get(annotation).map_err(err)?;
            if note.annotation_type() != PdfPageAnnotationType::Text {
                return Err("no es una nota".into());
            }
            note.set_contents(&text).map_err(err)
        })
    }
}

/// Text of a sticky note (empty if it has none); None if the annotation isn't a note.
pub(crate) fn note_text(annotation: &PdfPageAnnotation) -> Option<String> {
    (annotation.annotation_type() == PdfPageAnnotationType::Text).then(|| annotation.contents().unwrap_or_default())
}

#[tauri::command]
pub async fn add_markup(
    index: PdfPageIndex,
    rects: Vec<RelRect>,
    kind: MarkupKind,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_markup(index, rects, kind, color)
}

#[tauri::command]
pub async fn add_note(
    index: PdfPageIndex,
    at: RelPoint,
    text: String,
    color: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_note(index, at, text, color)
}

#[tauri::command]
pub async fn set_note(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    text: String,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.set_note(index, annotation, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::tests::state_with_lines;

    fn p(x: f32, y: f32) -> RelPoint {
        RelPoint { x, y }
    }

    /// Non-white pixels of the page.
    fn inked(state: &AppState) -> usize {
        state
            .with_doc(|o| {
                let image = crate::pdf::render(&get_page(o, 0)?, 600)?;
                Ok(image.pixels().filter(|px| px.0[..3].iter().any(|&c| c < 200)).count())
            })
            .unwrap()
    }

    #[test]
    fn underline_and_strikeout_selected_text() {
        let state = state_with_lines(&[&["Primera línea", "Segunda línea"]]);
        let selection = state.text_in_rect_for_tests(0, p(0.0, 0.0), p(1.0, 1.0));
        let before = inked(&state);
        state.add_markup(0, selection.clone(), MarkupKind::Underline, "#0050ff".into()).unwrap();
        let underlined = inked(&state);
        assert!(underlined > before + 50, "subrayado: {before} → {underlined}");
        state.add_markup(0, selection, MarkupKind::Strikeout, "#e01e1e".into()).unwrap();
        assert!(inked(&state) > underlined + 50);
        // Two annotations (one per markup, not per line).
        let count = state.with_doc(|o| Ok(get_page(o, 0)?.annotations().len())).unwrap();
        assert_eq!(count, 2);
        assert!(state.add_markup(0, vec![], MarkupKind::Underline, "#000000".into()).is_err());
    }

    #[test]
    fn notes_are_created_edited_and_found() {
        let state = state_with_lines(&[&["texto"]]);
        let before = inked(&state);
        state.add_note(0, p(0.5, 0.5), "Revisar esto".into(), "#ffd400".into()).unwrap();
        assert!(inked(&state) > before, "la nota no se dibuja");
        let hit = state.annotation_at(0, p(0.51, 0.51)).unwrap().expect("nota");
        assert_eq!(hit.note.as_deref(), Some("Revisar esto"));
        state.set_note(0, hit.annotation, "Listo ✓".into()).unwrap();
        let hit = state.annotation_at(0, p(0.51, 0.51)).unwrap().unwrap();
        assert_eq!(hit.note.as_deref(), Some("Listo ✓"));
        // In a corner it stays inside the page.
        state.add_note(0, p(0.999, 0.999), "".into(), "#ffd400".into()).unwrap();
        assert!(state.annotation_at(0, p(0.98, 0.98)).unwrap().is_some());
    }
}
