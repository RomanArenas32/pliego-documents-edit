//! Real redaction: whatever is under the box is removed from the
//! file, not just covered. Letters are removed, image pixels are painted black,
//! drawings and annotations in that area are deleted, and a black box is drawn.

use pdfium_render::prelude::*;
use serde::Deserialize;
use tauri::State;

use crate::pdf::{err, get_page, to_rect, AppState, DocInfo, RelRect};

/// Areas to redact on a page.
#[derive(Deserialize, Debug)]
pub struct RedactArea {
    page: PdfPageIndex,
    rects: Vec<RelRect>,
}

fn intersects(a: &PdfRect, b: &PdfRect) -> bool {
    a.left().value < b.right().value
        && b.left().value < a.right().value
        && a.bottom().value < b.top().value
        && b.bottom().value < a.top().value
}

fn inside(inner: &PdfRect, outer: &PdfRect) -> bool {
    inner.left().value >= outer.left().value
        && inner.right().value <= outer.right().value
        && inner.bottom().value >= outer.bottom().value
        && inner.top().value <= outer.top().value
}

fn area(r: &PdfRect) -> f32 {
    r.width().value.max(0.0) * r.height().value.max(0.0)
}

/// A run of letters kept from a text touched by the area.
struct Run {
    text: String,
    origin: (f32, f32),
}

/// What to do with each object on the page.
enum Action {
    Keep,
    Remove,
    /// Partly touched text: replaced by the runs that fall outside the area.
    Split(Vec<Run>),
    /// Partly touched image: its pixels inside the area are painted black.
    BlackOut,
}

/// Common area of two rectangles.
fn overlap(a: &PdfRect, b: &PdfRect) -> f32 {
    let w = a.right().value.min(b.right().value) - a.left().value.max(b.left().value);
    let h = a.top().value.min(b.top().value) - a.bottom().value.max(b.bottom().value);
    w.max(0.0) * h.max(0.0)
}

/// A letter is removed if the area covers a noticeable part of its shape (touching its
/// box isn't enough: on tight lines it overlaps the boxes of the line above or below).
fn char_hidden(ch: &PdfPageTextChar, zones: &[PdfRect]) -> bool {
    let tight = ch.tight_bounds().ok().filter(|b| area(b) > 0.0);
    let Some(bounds) = tight.or_else(|| ch.loose_bounds().ok()) else { return false };
    let (cx, cy) = (
        (bounds.left().value + bounds.right().value) / 2.0,
        (bounds.bottom().value + bounds.top().value) / 2.0,
    );
    zones.iter().any(|z| {
        let center_inside =
            cx >= z.left().value && cx <= z.right().value && cy >= z.bottom().value && cy <= z.top().value;
        center_inside || overlap(&bounds, z) >= area(&bounds) * 0.3
    })
}

/// Letters of the object (in order) that fall outside every area, grouped into runs.
fn surviving_runs(text: &PdfPageText, object: &PdfPageTextObject, zones: &[PdfRect]) -> Option<Vec<Run>> {
    let chars = text.chars_for_object(object).ok()?;
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;
    for ch in chars.iter() {
        let c = ch.unicode_char().unwrap_or(' ');
        let hidden = char_hidden(&ch, zones);
        if hidden {
            runs.extend(current.take().filter(|r| !r.text.trim().is_empty()));
            continue;
        }
        match &mut current {
            Some(run) => run.text.push(c),
            None => {
                let (x, y) = ch.origin().ok()?;
                current = Some(Run { text: c.to_string(), origin: (x.value, y.value) });
            }
        }
    }
    runs.extend(current.filter(|r| !r.text.trim().is_empty()));
    Some(runs)
}

fn plan(text: &PdfPageText, object: &PdfPageObject, zones: &[PdfRect]) -> Action {
    let Ok(bounds) = object.bounds().map(|q| q.to_rect()) else { return Action::Keep };
    let touching: Vec<&PdfRect> = zones.iter().filter(|z| intersects(&bounds, z)).collect();
    if touching.is_empty() {
        return Action::Keep;
    }
    if touching.iter().any(|z| inside(&bounds, z)) {
        return Action::Remove;
    }
    match object.object_type() {
        PdfPageObjectType::Text => {
            let object = object.as_text_object().unwrap();
            match surviving_runs(text, object, zones) {
                Some(runs) if runs.is_empty() => Action::Remove,
                Some(runs) => Action::Split(runs),
                // If its letters can't be read, remove it entirely to be safe.
                None => Action::Remove,
            }
        }
        PdfPageObjectType::Image => Action::BlackOut,
        // Drawings and others: removed if they're about the size of the area (they may be letters
        // drawn as curves); large ones (backgrounds, table borders) are just covered.
        _ => {
            let zone_area: f32 = touching.iter().map(|z| area(z)).sum();
            if area(&bounds) <= zone_area * 4.0 {
                Action::Remove
            } else {
                Action::Keep
            }
        }
    }
}

/// Paints black the image pixels that fall inside any area.
fn black_out(image_object: &mut PdfPageImageObject, zones: &[PdfRect]) -> Result<(), String> {
    let matrix = image_object.matrix().map_err(err)?;
    let mut image = image_object.get_raw_image().map_err(err)?.to_rgba8();
    let (w, h) = image.dimensions();
    let (a, b, c, d, e, f) = (matrix.a(), matrix.b(), matrix.c(), matrix.d(), matrix.e(), matrix.f());
    let mut changed = false;
    for j in 0..h {
        for i in 0..w {
            // The image fills the unit square (row 0 at the top) transformed by the matrix.
            let u = (i as f32 + 0.5) / w as f32;
            let v = 1.0 - (j as f32 + 0.5) / h as f32;
            let x = a * u + c * v + e;
            let y = b * u + d * v + f;
            let hit = zones.iter().any(|z| {
                x >= z.left().value && x <= z.right().value && y >= z.bottom().value && y <= z.top().value
            });
            if hit {
                image.put_pixel(i, j, image::Rgba([0, 0, 0, 255]));
                changed = true;
            }
        }
    }
    if changed {
        image_object
            .set_image(&image::DynamicImage::ImageRgba8(image))
            .map_err(err)?;
    }
    Ok(())
}

fn redact_page(doc: &PdfDocument<'static>, page: &mut PdfPage<'static>, rects: &[RelRect]) -> Result<(), String> {
    let zones = rects
        .iter()
        .map(|r| to_rect(page, r.from, r.to))
        .collect::<Result<Vec<_>, _>>()?;
    if zones.is_empty() {
        return Ok(());
    }

    // Decide everything before touching the page (removing objects changes the indices).
    let actions: Vec<Action> = {
        let text = page.text().map_err(err)?;
        page.objects().iter().map(|o| plan(&text, &o, &zones)).collect()
    };
    for (index, action) in actions.into_iter().enumerate().rev() {
        let index = index as PdfPageObjectIndex;
        match action {
            Action::Keep => {}
            Action::Remove => {
                page.objects_mut().remove_object_at_index(index).map_err(err)?;
            }
            Action::BlackOut => {
                let failed = {
                    let mut object = page.objects().get(index).map_err(err)?;
                    object.as_image_object_mut().is_some_and(|image| black_out(image, &zones).is_err())
                };
                // If the image can't be read, remove it entirely.
                if failed {
                    page.objects_mut().remove_object_at_index(index).map_err(err)?;
                }
            }
            Action::Split(runs) => {
                let object = page.objects_mut().remove_object_at_index(index).map_err(err)?;
                let original = object.as_text_object().ok_or("el objeto no es texto")?;
                let matrix = original.matrix().map_err(err)?;
                let size = original.unscaled_font_size();
                let font = original.font();
                let fill = original.fill_color().unwrap_or(PdfColor::BLACK);
                let mode = original.render_mode();
                for run in runs {
                    let mut piece = PdfPageTextObject::new(doc, &run.text, &font, size).map_err(err)?;
                    piece.set_fill_color(fill).map_err(err)?;
                    piece.set_render_mode(mode).map_err(err)?;
                    // Same shape (scale, rotation) as the original, starting where the run started.
                    piece
                        .apply_matrix(PdfMatrix::new(
                            matrix.a(),
                            matrix.b(),
                            matrix.c(),
                            matrix.d(),
                            run.origin.0,
                            run.origin.1,
                        ))
                        .map_err(err)?;
                    page.objects_mut().add_text_object(piece).map_err(err)?;
                }
            }
        }
    }

    // Annotations (comments, links, stamps…) in the area.
    let annotations = page.annotations();
    let hit: Vec<PdfPageAnnotationIndex> = (0..annotations.len())
        .filter(|&i| {
            annotations
                .get(i)
                .and_then(|a| a.bounds())
                .is_ok_and(|b| zones.iter().any(|z| intersects(&b, z)))
        })
        .collect();
    for i in hit.into_iter().rev() {
        let annotations = page.annotations_mut();
        let annotation = annotations.get(i).map_err(err)?;
        annotations.delete_annotation(annotation).map_err(err)?;
    }

    for zone in zones {
        let rect = PdfPagePathObject::new_rect(doc, zone, None, None, Some(PdfColor::BLACK)).map_err(err)?;
        page.objects_mut().add_path_object(rect).map_err(err)?;
    }
    page.regenerate_content().map_err(err)?;
    Ok(())
}

impl AppState {
    pub fn redact(&self, areas: Vec<RedactArea>) -> Result<DocInfo, String> {
        if areas.iter().all(|a| a.rects.is_empty()) {
            return Err("no hay nada para ocultar".into());
        }
        self.edit(|open| {
            for area in &areas {
                let mut page = get_page(open, area.page)?;
                redact_page(&open.doc, &mut page, &area.rects)?;
            }
            Ok(())
        })
    }
}

#[tauri::command]
pub async fn redact(areas: Vec<RedactArea>, state: State<'_, AppState>) -> Result<DocInfo, String> {
    state.redact(areas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::ImageSource;
    use crate::pdf::tests::{state_with_lines, state_with_pages};
    use crate::pdf::{render, RelPoint};

    fn page_text(state: &AppState, page: PdfPageIndex) -> String {
        state.with_doc(|o| Ok(get_page(o, page)?.text().map_err(err)?.all())).unwrap()
    }

    /// What's left of the document after saving and reopening it.
    fn saved_text(state: &AppState) -> String {
        let bytes = state.with_doc(|o| o.doc.save_to_bytes().map_err(err)).unwrap();
        let _guard = crate::pdf::pdfium_lock();
        let doc = crate::pdf::load(bytes, None).unwrap();
        let text = doc.pages().iter().map(|p| p.text().unwrap().all()).collect::<Vec<_>>().join("\n");
        text
    }

    #[test]
    fn removes_only_the_hidden_words() {
        let state = state_with_lines(&[&["uno dos tres", "cuatro cinco"]]);
        let hit = &state.search_text("dos").unwrap()[0];
        state.redact(vec![RedactArea { page: 0, rects: hit.rects.clone() }]).unwrap();
        let text = page_text(&state, 0);
        assert!(!text.contains("dos"), "{text:?}");
        assert!(text.contains("uno") && text.contains("tres") && text.contains("cuatro cinco"), "{text:?}");
        let saved = saved_text(&state);
        assert!(!saved.contains("dos"), "{saved:?}");
        // "tres" stays where it was (doesn't shift left).
        let tres = &state.search_text("tres").unwrap()[0].rects[0];
        let hit_tres = state.text_in_rect(0, tres.from, tres.to).unwrap().unwrap();
        assert_eq!(hit_tres.text, "tres");
        assert!(tres.from.x > hit.rects[0].to.x - 0.005, "{:?} vs {:?}", tres, hit.rects[0]);
        // It can be undone.
        state.undo().unwrap();
        assert!(page_text(&state, 0).contains("dos"));
    }

    #[test]
    fn neighbouring_lines_are_not_touched() {
        // Very tight lines (as in long.pdf): the letter boxes overlap.
        let state = state_with_pages(1);
        state
            .with_doc(|open| {
                let font = open.doc.fonts_mut().helvetica();
                let mut page = get_page(open, 0)?;
                for (i, line) in ["arriba arriba", "medio secreto medio", "abajo abajo"].iter().enumerate() {
                    let mut t = PdfPageTextObject::new(&open.doc, line, font, PdfPoints::new(14.0)).map_err(err)?;
                    t.translate(PdfPoints::new(50.0), PdfPoints::new(500.0 - 13.0 * i as f32)).map_err(err)?;
                    page.objects_mut().add_text_object(t).map_err(err)?;
                }
                Ok(())
            })
            .unwrap();
        let hit = &state.search_text("secreto").unwrap()[0];
        state.redact(vec![RedactArea { page: 0, rects: hit.rects.clone() }]).unwrap();
        let text = saved_text(&state);
        assert!(!text.contains("secreto"), "{text:?}");
        assert!(text.contains("arriba arriba") && text.contains("abajo abajo"), "{text:?}");
    }

    #[test]
    fn whole_lines_and_several_pages() {
        let state = state_with_lines(&[&["secreto uno", "visible"], &["secreto dos"]]);
        let hits = state.search_text("secreto").unwrap();
        let areas = hits
            .iter()
            .map(|h| RedactArea { page: h.page, rects: h.rects.clone() })
            .collect();
        state.redact(areas).unwrap();
        let saved = saved_text(&state);
        assert!(!saved.contains("secreto"), "{saved:?}");
        assert!(saved.contains("uno") && saved.contains("dos") && saved.contains("visible"));
    }

    #[test]
    fn blacks_out_image_pixels_and_annotations() {
        let state = state_with_pages(1);
        let white = image::RgbImage::from_pixel(200, 200, image::Rgb([255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(white)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        state.add_image(0, ImageSource::Data { bytes: png }, RelPoint { x: 0.5, y: 0.5 }, 0.6).unwrap();
        state.add_note(0, RelPoint { x: 0.1, y: 0.1 }, "privado".into(), "#ffd400".into()).unwrap();

        // Area covering the left half of the image and the note.
        let zone = RelRect { from: RelPoint { x: 0.05, y: 0.05 }, to: RelPoint { x: 0.5, y: 0.95 } };
        state.redact(vec![RedactArea { page: 0, rects: vec![zone] }]).unwrap();
        assert!(state.annotation_at(0, RelPoint { x: 0.11, y: 0.11 }).unwrap().is_none());

        // Renders black on the left and white on the right of the image.
        let (left, right) = state
            .with_doc(|o| {
                let img = render(&get_page(o, 0)?, 400)?;
                let y = img.height() / 2;
                Ok((img.get_pixel(150, y).0[0], img.get_pixel(260, y).0[0]))
            })
            .unwrap();
        assert!(left < 30 && right > 220, "{left} {right}");
    }
}
