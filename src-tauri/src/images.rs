//! Images and signatures added with the app. Each one is a stamp (annotation) holding a
//! single image object, just like the app's own texts: that way it can be moved, resized
//! and deleted without touching the page's original content.

use std::io::Cursor;

use image::DynamicImage;
use pdfium_render::prelude::*;
use serde::Deserialize;
use tauri::State;

use crate::pdf::{err, get_page, rotation_degrees, to_points, to_rect, AppState, DocInfo, RelPoint};

/// Where the image comes from: a chosen file or the bytes of a drawn signature (PNG).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ImageSource {
    File { path: String },
    Data { bytes: Vec<u8> },
}

/// Image ready to insert. JPEGs are inserted as they are (without recompressing).
pub(crate) enum Picture {
    Jpeg { bytes: Vec<u8>, width: u32, height: u32 },
    Decoded(DynamicImage),
}

impl Picture {
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            Picture::Jpeg { width, height, .. } => (*width, *height),
            Picture::Decoded(image) => (image.width(), image.height()),
        }
    }

    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(err)?;
        if reader.format() == Some(image::ImageFormat::Jpeg) {
            let (width, height) = reader.into_dimensions().map_err(err)?;
            // CMYK JPEGs or ones with odd orientation are decoded to avoid surprises.
            if is_plain_jpeg(&bytes) {
                return Ok(Picture::Jpeg { bytes, width, height });
            }
            let image = image::load_from_memory(&bytes).map_err(err)?;
            return Ok(Picture::Decoded(image));
        }
        let image = reader
            .decode()
            .map_err(|e| format!("no se pudo leer la imagen: {e}"))?;
        Ok(Picture::Decoded(image))
    }

    fn load(source: ImageSource) -> Result<Self, String> {
        let bytes = match source {
            ImageSource::File { path } => std::fs::read(&path).map_err(err)?,
            ImageSource::Data { bytes } => bytes,
        };
        if bytes.is_empty() {
            return Err("la imagen está vacía".into());
        }
        Self::from_bytes(bytes)
    }

    /// 1×1 point image object (scaled afterwards).
    pub(crate) fn object(&self, doc: &PdfDocument<'static>) -> Result<PdfPageImageObject<'static>, String> {
        match self {
            Picture::Jpeg { bytes, .. } => {
                PdfPageImageObject::new_from_jpeg_reader(doc, Cursor::new(bytes.clone())).map_err(err)
            }
            Picture::Decoded(image) => PdfPageImageObject::new(doc, image).map_err(err),
        }
    }
}

/// JPEG with 1 or 3 components (gray or RGB), which pdfium inserts without converting.
fn is_plain_jpeg(bytes: &[u8]) -> bool {
    // Looks for the SOF marker (FFC0..FFC2) and reads the number of components.
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            return false;
        }
        let marker = bytes[i + 1];
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC2) {
            return matches!(bytes[i + 9], 1 | 3);
        }
        i += 2 + len;
    }
    false
}

/// A stamp that only holds images: what `place` creates.
pub(crate) fn is_image_stamp(annotation: &PdfPageAnnotation) -> bool {
    if annotation.annotation_type() != PdfPageAnnotationType::Stamp {
        return false;
    }
    let objects = annotation.objects();
    !objects.is_empty()
        && (0..objects.len()).all(|i| objects.get(i).is_ok_and(|o| o.as_image_object().is_some()))
}

/// Recovers a stamp's image so it can be recreated somewhere else.
fn picture_of(doc: &PdfDocument, annotation: &PdfPageAnnotation) -> Result<Picture, String> {
    let object = annotation.objects().get(0).map_err(err)?;
    let image = object.as_image_object().ok_or("la anotación no es una imagen")?;
    let filters = image.filters();
    if filters.len() == 1 && filters.get(0).is_ok_and(|f| f.name() == "DCTDecode") {
        if let Ok(bytes) = image.get_raw_image_data() {
            if let Ok(picture @ Picture::Jpeg { .. }) = Picture::from_bytes(bytes) {
                return Ok(picture);
            }
        }
    }
    // Anything else is recovered with its transparency (the mask is applied when processing).
    Ok(Picture::Decoded(image.get_processed_image(doc).map_err(err)?))
}

/// Inserts the image filling `rect` (in page points), upright as seen on screen.
fn place(
    doc: &PdfDocument<'static>,
    page: &mut PdfPage<'static>,
    picture: &Picture,
    rect: PdfRect,
) -> Result<(), String> {
    let degrees = rotation_degrees(page);
    let (w, h) = (rect.width().value, rect.height().value);
    let mut object = picture.object(doc)?;
    // On rotated pages the image is rotated the other way to look upright: its sides swap.
    let sideways = degrees == 90.0 || degrees == 270.0;
    let (sx, sy) = if sideways { (h, w) } else { (w, h) };
    object.scale(sx, sy).map_err(err)?;
    if degrees != 0.0 {
        object.rotate_counter_clockwise_degrees(degrees).map_err(err)?;
    }
    let bounds = object.bounds().map_err(err)?.to_rect();
    object
        .translate(
            PdfPoints::new(rect.left().value - bounds.left().value),
            PdfPoints::new(rect.bottom().value - bounds.bottom().value),
        )
        .map_err(err)?;

    let mut annotation = page.annotations_mut().create_stamp_annotation().map_err(err)?;
    // Bounds are set before adding the object (otherwise pdfium doesn't draw it).
    annotation.set_bounds(rect).map_err(err)?;
    annotation.objects_mut().add_image_object(object).map_err(err)?;
    Ok(())
}

/// Size of the page in points as displayed (width, height), taking rotation into account.
fn display_size(page: &PdfPage) -> Result<(f32, f32), String> {
    let (x0, y0) = to_points(page, RelPoint { x: 0.0, y: 0.0 })?;
    let (x1, y1) = to_points(page, RelPoint { x: 1.0, y: 0.0 })?;
    let (x2, y2) = to_points(page, RelPoint { x: 0.0, y: 1.0 })?;
    Ok((
        (x1 - x0).hypot(y1 - y0),
        (x2 - x0).hypot(y2 - y0),
    ))
}

/// Relative rectangle centered on `center`, `width` wide (fraction of the page
/// width) and with the image's aspect ratio. Shrinks if it doesn't fit and stays inside.
fn fit(page: &PdfPage, picture: &Picture, center: RelPoint, width: f32) -> Result<PdfRect, String> {
    let (page_w, page_h) = display_size(page)?;
    let (iw, ih) = picture.size();
    let aspect = ih as f32 / iw.max(1) as f32;
    let mut w = width.clamp(0.02, 1.0);
    let mut h = w * page_w * aspect / page_h;
    if h > 0.95 {
        w *= 0.95 / h;
        h = 0.95;
    }
    let x = center.x.clamp(w / 2.0, 1.0 - w / 2.0);
    let y = center.y.clamp(h / 2.0, 1.0 - h / 2.0);
    to_rect(
        page,
        RelPoint { x: x - w / 2.0, y: y - h / 2.0 },
        RelPoint { x: x + w / 2.0, y: y + h / 2.0 },
    )
}

impl AppState {
    pub fn add_image(
        &self,
        index: PdfPageIndex,
        source: ImageSource,
        center: RelPoint,
        width: f32,
    ) -> Result<DocInfo, String> {
        let picture = Picture::load(source)?;
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let rect = fit(&page, &picture, center, width)?;
            place(&open.doc, &mut page, &picture, rect)
        })
    }

    /// Moves or resizes an image added with the app.
    pub fn set_image_rect(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
        from: RelPoint,
        to: RelPoint,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let mut page = get_page(open, index)?;
            let picture = {
                let old = page.annotations().get(annotation).map_err(err)?;
                if !is_image_stamp(&old) {
                    return Err("la anotación no es una imagen".into());
                }
                picture_of(&open.doc, &old)?
            };
            let annotations = page.annotations_mut();
            let old = annotations.get(annotation).map_err(err)?;
            annotations.delete_annotation(old).map_err(err)?;
            let rect = to_rect(&page, from, to)?;
            if rect.width().value < 2.0 || rect.height().value < 2.0 {
                return Err("la imagen quedaría demasiado chica".into());
            }
            place(&open.doc, &mut page, &picture, rect)
        })
    }
}

#[tauri::command]
pub async fn add_image(
    index: PdfPageIndex,
    source: ImageSource,
    center: RelPoint,
    width: f32,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.add_image(index, source, center, width)
}

#[tauri::command]
pub async fn set_image_rect(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    from: RelPoint,
    to: RelPoint,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.set_image_rect(index, annotation, from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::render;
    use crate::pdf::tests::state_with_pages;

    fn p(x: f32, y: f32) -> RelPoint {
        RelPoint { x, y }
    }

    /// 200×100 transparent PNG with a horizontal blue band in the middle.
    fn signature_png() -> Vec<u8> {
        let mut img = image::RgbaImage::new(200, 100);
        for y in 45..55 {
            for x in 0..200 {
                img.put_pixel(x, y, image::Rgba([20, 40, 200, 255]));
            }
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(64, 32, image::Rgb([200, 30, 30]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    fn pixel(state: &AppState, x: f32, y: f32) -> [u8; 4] {
        state
            .with_doc(|open| {
                let image = render(&get_page(open, 0)?, 400)?;
                let (w, h) = (image.width() as f32, image.height() as f32);
                Ok(image.get_pixel((x * w) as u32, (y * h) as u32).0)
            })
            .unwrap()
    }

    fn hit(state: &AppState, at: RelPoint) -> crate::pdf::AnnotationHit {
        state.annotation_at(0, at).unwrap().expect("no hay anotación")
    }

    #[test]
    fn signature_keeps_transparency_and_aspect() {
        let state = state_with_pages(1);
        let bytes = signature_png();
        state.add_image(0, ImageSource::Data { bytes }, p(0.5, 0.5), 0.5).unwrap();
        let found = hit(&state, p(0.5, 0.5));
        assert!(found.image);
        // 0.5 of the width (400pt) = 200pt wide → 100pt tall (out of 600pt).
        let r = found.rect;
        assert!(((r.to.x - r.from.x) - 0.5).abs() < 0.01, "{r:?}");
        assert!(((r.to.y - r.from.y) - 100.0 / 600.0).abs() < 0.01, "{r:?}");
        // The band looks blue and outside it the page shows (white).
        let [r_, _, b, _] = pixel(&state, 0.5, 0.5);
        assert!(b > 150 && r_ < 100);
        assert_eq!(pixel(&state, 0.5, 0.44), [255, 255, 255, 255]);
    }

    #[test]
    fn move_and_resize_image() {
        let state = state_with_pages(1);
        state.add_image(0, ImageSource::Data { bytes: jpeg() }, p(0.3, 0.3), 0.2).unwrap();
        let a = hit(&state, p(0.3, 0.3)).annotation;
        state.set_image_rect(0, a, p(0.5, 0.6), p(0.9, 0.8)).unwrap();
        assert!(state.annotation_at(0, p(0.3, 0.3)).unwrap().is_none());
        let moved = hit(&state, p(0.7, 0.7));
        assert!(moved.image);
        assert!((moved.rect.from.x - 0.5).abs() < 0.01 && (moved.rect.to.y - 0.8).abs() < 0.01);
        // Still the same JPEG (red), and the file didn't grow more than needed.
        let [r, g, _, _] = pixel(&state, 0.7, 0.7);
        assert!(r > 150 && g < 80, "{r} {g}");
        state.undo().unwrap();
        assert!(hit(&state, p(0.3, 0.3)).image);
    }

    #[test]
    fn image_on_rotated_page_stays_upright() {
        let state = state_with_pages(1);
        state.rotate_page(0, true).unwrap();
        state.add_image(0, ImageSource::Data { bytes: signature_png() }, p(0.5, 0.5), 0.4).unwrap();
        let r = hit(&state, p(0.5, 0.5)).rect;
        // Looks wider than tall, like the original image (2:1).
        let (w, h) = ((r.to.x - r.from.x) * 600.0, (r.to.y - r.from.y) * 400.0);
        assert!((w / h - 2.0).abs() < 0.1, "{w} x {h}");
        // The horizontal band stays horizontal: the center is blue and above/below white.
        assert!(pixel(&state, 0.5, 0.5)[2] > 150);
        assert_eq!(pixel(&state, 0.5, r.from.y + (r.to.y - r.from.y) * 0.2), [255, 255, 255, 255]);
    }

    #[test]
    fn invalid_image_is_rejected() {
        let state = state_with_pages(1);
        let result = state.add_image(0, ImageSource::Data { bytes: b"no es imagen".to_vec() }, p(0.5, 0.5), 0.3);
        assert!(result.is_err());
        assert!(state.annotation_at(0, p(0.5, 0.5)).unwrap().is_none());
    }
}
