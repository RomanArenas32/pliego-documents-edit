//! Tools over the whole document: extract and split pages, export them as
//! images, create a PDF from images, reduce the size and protect with a password.
//! None of them changes the open document: they all write new files.

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageDecoder};
use lopdf::{Object, Stream};
use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::images::Picture;
use crate::pdf::{err, get_page, pdfium, pdfium_lock, render, AppState};

/// A4 in points.
const A4: (f32, f32) = (595.0, 842.0);
const IMAGE_MARGIN: f32 = 24.0;

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
}

/// How much to reduce: stronger shrinks images more and lowers their quality.
#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum CompressLevel {
    Low,
    Medium,
    High,
}

impl CompressLevel {
    /// (maximum long side in pixels, JPEG quality)
    fn settings(self) -> (u32, u8) {
        match self {
            CompressLevel::Low => (2400, 85),
            CompressLevel::Medium => (1600, 72),
            CompressLevel::High => (1100, 55),
        }
    }
}

#[derive(Serialize, Debug)]
pub struct CompressResult {
    before: u64,
    after: u64,
    images: usize,
}

/// Base name of the open document (without extension), for naming the new files.
fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "documento".into())
}

/// "1-3,5" (1-based numbering) from 0-based indices, grouping consecutive ones.
fn page_ranges(pages: &[PdfPageIndex]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let start = pages[i];
        let mut end = start;
        while i + 1 < pages.len() && pages[i + 1] == end + 1 {
            i += 1;
            end = pages[i];
        }
        parts.push(if start == end { format!("{}", start + 1) } else { format!("{}-{}", start + 1, end + 1) });
        i += 1;
    }
    parts.join(",")
}

/// New document with the given pages (in that order), as bytes.
fn pages_to_bytes(source: &PdfDocument<'static>, pages: &[PdfPageIndex]) -> Result<Vec<u8>, String> {
    let mut doc = pdfium()?.create_new_pdf().map_err(err)?;
    for (at, &page) in pages.iter().enumerate() {
        doc.pages_mut().copy_page_from_document(source, page, at as PdfPageIndex).map_err(err)?;
    }
    doc.save_to_bytes().map_err(err)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("no se pudo guardar {}: {e}", path.display()))
}

impl AppState {
    /// Saves the chosen pages as a new PDF.
    pub fn extract_pages(&self, pages: Vec<PdfPageIndex>, path: String) -> Result<(), String> {
        if pages.is_empty() {
            return Err("no se eligió ninguna página".into());
        }
        let bytes = self.with_doc(|open| {
            let count = open.doc.pages().len();
            if let Some(&bad) = pages.iter().find(|&&p| p >= count) {
                return Err(format!("la página {} no existe", bad + 1));
            }
            pages_to_bytes(&open.doc, &pages)
        })?;
        write(Path::new(&path), &bytes)
    }

    /// Splits the document into parts of `every` pages inside `dir`. Returns the paths.
    pub fn split_pdf(&self, every: PdfPageIndex, dir: String) -> Result<Vec<String>, String> {
        let every = every.max(1);
        let parts = self.with_doc(|open| {
            let count = open.doc.pages().len();
            let name = stem(&open.path);
            let mut parts = Vec::new();
            for start in (0..count).step_by(every as usize) {
                let pages: Vec<_> = (start..(start + every).min(count)).collect();
                let file = format!("{name} (pág. {}).pdf", page_ranges(&pages));
                parts.push((file, pages_to_bytes(&open.doc, &pages)?));
            }
            Ok(parts)
        })?;
        let mut paths = Vec::new();
        for (file, bytes) in parts {
            let path = Path::new(&dir).join(file);
            write(&path, &bytes)?;
            paths.push(path.to_string_lossy().into_owned());
        }
        Ok(paths)
    }

    /// Saves each chosen page as an image in `dir`, at the given resolution.
    pub fn export_images(
        &self,
        pages: Vec<PdfPageIndex>,
        dir: String,
        format: ImageFormat,
        dpi: u32,
    ) -> Result<Vec<String>, String> {
        let dpi = dpi.clamp(36, 600);
        let mut paths = Vec::new();
        for page in pages {
            // One page at a time so all the images aren't in memory.
            let (name, image) = self.with_doc(|open| {
                let p = get_page(open, page)?;
                let width = (p.width().value / 72.0 * dpi as f32).round() as u32;
                Ok((stem(&open.path), render(&p, width.max(1))?))
            })?;
            let extension = if format == ImageFormat::Png { "png" } else { "jpg" };
            let path = Path::new(&dir).join(format!("{name} - página {}.{extension}", page + 1));
            let file = std::fs::File::create(&path).map_err(|e| format!("no se pudo guardar {}: {e}", path.display()))?;
            let mut out = std::io::BufWriter::new(file);
            match format {
                ImageFormat::Png => image.write_to(&mut out, image::ImageFormat::Png).map_err(err)?,
                ImageFormat::Jpeg => {
                    let rgb = DynamicImage::ImageRgba8(image).to_rgb8();
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
                        .encode_image(&rgb)
                        .map_err(err)?
                }
            }
            paths.push(path.to_string_lossy().into_owned());
        }
        Ok(paths)
    }

    /// Reduces the size of the document's images and saves a copy to `path`.
    pub fn compress_pdf(&self, path: String, level: CompressLevel) -> Result<CompressResult, String> {
        let bytes = self.with_doc(|open| open.plain_bytes())?;
        let before = bytes.len() as u64;
        let (out, images) = compress_bytes(&bytes, level)?;
        // If nothing is gained, the original is saved.
        let out = if out.len() as u64 >= before { bytes } else { out };
        write(Path::new(&path), &out)?;
        Ok(CompressResult { before, after: out.len() as u64, images })
    }

    /// Saves a copy that asks for a password to open (256-bit AES).
    pub fn protect_pdf(&self, path: String, password: String) -> Result<(), String> {
        if password.is_empty() {
            return Err("escribe una contraseña".into());
        }
        let bytes = self.with_doc(|open| open.plain_bytes())?;
        write(Path::new(&path), &encrypt_bytes(&bytes, &password)?)
    }
}

/// Creates a PDF with one image per page (A4, rotated to match the image) and saves it to `output`.
pub fn images_to_pdf(paths: Vec<String>, output: String) -> Result<(), String> {
    if paths.is_empty() {
        return Err("no se eligió ninguna imagen".into());
    }
    let _guard = pdfium_lock();
    let mut doc = pdfium()?.create_new_pdf().map_err(err)?;
    for path in &paths {
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let picture = upright(bytes).map_err(|e| format!("{}: {e}", file_name(path)))?;
        let (w, h) = picture.size();
        let (page_w, page_h) = if w > h { (A4.1, A4.0) } else { A4 };
        let scale = ((page_w - 2.0 * IMAGE_MARGIN) / w as f32).min((page_h - 2.0 * IMAGE_MARGIN) / h as f32);
        let (draw_w, draw_h) = (w as f32 * scale, h as f32 * scale);

        let count = doc.pages().len();
        let mut page = doc
            .pages_mut()
            .create_page_at_index(PdfPagePaperSize::Custom(PdfPoints::new(page_w), PdfPoints::new(page_h)), count)
            .map_err(err)?;
        let mut object = picture.object(&doc)?;
        object.scale(draw_w, draw_h).map_err(err)?;
        object
            .translate(PdfPoints::new((page_w - draw_w) / 2.0), PdfPoints::new((page_h - draw_h) / 2.0))
            .map_err(err)?;
        page.objects_mut().add_image_object(object).map_err(err)?;
    }
    let bytes = doc.save_to_bytes().map_err(err)?;
    write(Path::new(&output), &bytes)
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.into())
}

/// Upright image: phone photos store the rotation separately (EXIF) and it must be applied.
fn upright(bytes: Vec<u8>) -> Result<Picture, String> {
    let orientation = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_decoder().ok())
        .and_then(|mut d| d.orientation().ok());
    match orientation {
        Some(o) if o != image::metadata::Orientation::NoTransforms => {
            let mut image = image::load_from_memory(&bytes).map_err(err)?;
            image.apply_orientation(o);
            Ok(Picture::Decoded(image))
        }
        _ => Picture::from_bytes(bytes),
    }
}

// ---------- Reduce size ----------

/// Recompresses large images as JPEG (scaling them down if needed).
/// Returns the new bytes and how many images were changed.
fn compress_bytes(bytes: &[u8], level: CompressLevel) -> Result<(Vec<u8>, usize), String> {
    let (max_side, quality) = level.settings();
    let mut doc = lopdf::Document::load_mem(bytes).map_err(|e| format!("no se pudo leer el PDF: {e}"))?;
    let mut changed = 0;
    for object in doc.objects.values_mut() {
        if let Object::Stream(stream) = object {
            if let Some(jpeg) = recompress_image(stream, max_side, quality) {
                let (data, width, height, gray) = jpeg;
                // Only if it's actually smaller.
                if data.len() < stream.content.len() {
                    stream.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
                    stream.dict.remove(b"DecodeParms");
                    stream.dict.set("Width", width as i64);
                    stream.dict.set("Height", height as i64);
                    stream.dict.set("BitsPerComponent", 8);
                    if !uses_icc(stream) {
                        let space: &[u8] = if gray { b"DeviceGray" } else { b"DeviceRGB" };
                        stream.dict.set("ColorSpace", Object::Name(space.to_vec()));
                    }
                    stream.set_content(data);
                    changed += 1;
                }
            }
        }
    }
    doc.compress();
    let mut out = Vec::new();
    doc.save_modern(&mut out).map_err(|e| format!("no se pudo guardar: {e}"))?;
    Ok((out, changed))
}

fn name_of<'a>(stream: &'a Stream, key: &[u8]) -> Option<&'a [u8]> {
    stream.dict.get(key).ok().and_then(|o| o.as_name().ok())
}

fn uses_icc(stream: &Stream) -> bool {
    matches!(stream.dict.get(b"ColorSpace"), Ok(Object::Array(a)) if a.first().and_then(|o| o.as_name().ok()) == Some(b"ICCBased"))
}

/// Color components that can be converted (1 = gray, 3 = RGB); None for the rest.
fn components(stream: &Stream, doc_space: Option<&[u8]>) -> Option<u8> {
    match doc_space {
        Some(b"DeviceRGB") => Some(3),
        Some(b"DeviceGray") => Some(1),
        _ => match stream.dict.get(b"ColorSpace") {
            // [/ICCBased ref]: the number of components is in the profile; inferred from the data.
            Ok(Object::Array(_)) if uses_icc(stream) => Some(0),
            _ => None,
        },
    }
}

/// Recompressed image as (JPEG, width, height, is gray), or None if it's better left alone.
fn recompress_image(stream: &Stream, max_side: u32, quality: u8) -> Option<(Vec<u8>, u32, u32, bool)> {
    if name_of(stream, b"Subtype") != Some(b"Image") {
        return None;
    }
    let dict = &stream.dict;
    // Masks, special decodes and palettes are left as they are.
    if dict.has(b"ImageMask") || dict.has(b"Decode") || dict.has(b"Mask") && !matches!(dict.get(b"Mask"), Ok(Object::Reference(_))) {
        return None;
    }
    let width = dict.get(b"Width").ok()?.as_i64().ok()? as u32;
    let height = dict.get(b"Height").ok()?.as_i64().ok()? as u32;
    if width < 64 || height < 64 {
        return None;
    }
    let filters = stream.filters().ok()?;
    let image = match filters.as_slice() {
        [b"DCTDecode"] => {
            let image = image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg).ok()?;
            // CMYK and others: left untouched.
            if !matches!(image.color().channel_count(), 1 | 3) {
                return None;
            }
            image
        }
        [] | [b"FlateDecode"] => {
            if dict.get(b"BitsPerComponent").ok()?.as_i64().ok()? != 8 {
                return None;
            }
            let channels = components(stream, name_of(stream, b"ColorSpace"))?;
            let data = stream.decompressed_content().ok()?;
            let pixels = (width * height) as usize;
            let channels = if channels == 0 {
                (data.len() / pixels.max(1)) as u8
            } else {
                channels
            };
            match channels {
                3 => DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, data.get(..pixels * 3)?.to_vec())?),
                1 => DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, data.get(..pixels)?.to_vec())?),
                _ => return None,
            }
        }
        _ => return None,
    };
    let gray = image.color().channel_count() == 1;
    let image = if width.max(height) > max_side {
        image.resize(max_side, max_side, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    if gray {
        encoder.encode_image(&image.to_luma8()).ok()?;
    } else {
        encoder.encode_image(&image.to_rgb8()).ok()?;
    }
    Some((out, image.width(), image.height(), gray))
}

// ---------- Password ----------

fn encrypt_bytes(bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let mut doc = lopdf::Document::load_mem(bytes).map_err(|e| format!("no se pudo leer el PDF: {e}"))?;
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(err)?;
    // The owner password is another random one: the user's password opens it for normal use.
    let mut owner = [0u8; 24];
    getrandom::fill(&mut owner).map_err(err)?;
    let owner: String = owner.iter().map(|b| format!("{b:02x}")).collect();
    let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
        file_encryption_key: &key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: &owner,
        user_password: password,
        permissions: lopdf::Permissions::all(),
    })
    .map_err(|e| format!("no se pudo cifrar: {e}"))?;
    doc.encrypt(&state).map_err(|e| format!("no se pudo cifrar: {e}"))?;
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|e| format!("no se pudo guardar: {e}"))?;
    Ok(out)
}

// ---------- Commands ----------

#[tauri::command]
pub async fn extract_pages(pages: Vec<PdfPageIndex>, path: String, state: State<'_, AppState>) -> Result<(), String> {
    state.extract_pages(pages, path)
}

#[tauri::command]
pub async fn split_pdf(every: PdfPageIndex, dir: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    state.split_pdf(every, dir)
}

#[tauri::command]
pub async fn export_images(
    pages: Vec<PdfPageIndex>,
    dir: String,
    format: ImageFormat,
    dpi: u32,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    state.export_images(pages, dir, format, dpi)
}

#[tauri::command]
pub async fn compress_pdf(path: String, level: CompressLevel, state: State<'_, AppState>) -> Result<CompressResult, String> {
    state.compress_pdf(path, level)
}

#[tauri::command]
pub async fn protect_pdf(path: String, password: String, state: State<'_, AppState>) -> Result<(), String> {
    state.protect_pdf(path, password)
}

#[tauri::command(rename = "images_to_pdf")]
pub async fn images_to_pdf_command(paths: Vec<String>, output: String) -> Result<(), String> {
    images_to_pdf(paths, output)
}

/// Opens the system print dialog with what the window shows.
#[tauri::command]
pub async fn print_window(window: tauri::WebviewWindow) -> Result<(), String> {
    window.print().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::images::ImageSource;
    use crate::pdf::tests::{state_with_lines, state_with_pages};
    use crate::pdf::{load, RelPoint};

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pliego-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn page_count(path: &Path, password: Option<&str>) -> Result<PdfPageIndex, String> {
        let _guard = pdfium_lock();
        let doc = load(std::fs::read(path).map_err(err)?, password)?;
        Ok(doc.pages().len())
    }

    fn widths(path: &Path) -> Vec<f32> {
        let _guard = pdfium_lock();
        let doc = load(std::fs::read(path).unwrap(), None).unwrap();
        doc.pages().iter().map(|p| p.width().value).collect()
    }

    #[test]
    fn ranges_are_grouped() {
        assert_eq!(page_ranges(&[0, 1, 2, 4, 6, 7]), "1-3,5,7-8");
        assert_eq!(page_ranges(&[3]), "4");
    }

    #[test]
    fn extract_and_split() {
        let state = state_with_pages(5); // widths 400, 410, 420…
        let out = tmp("extract.pdf");
        state.extract_pages(vec![3, 0], out.to_string_lossy().into()).unwrap();
        assert_eq!(widths(&out), [430.0, 400.0]);
        assert!(state.extract_pages(vec![9], out.to_string_lossy().into()).is_err());

        let dir = tmp("split");
        std::fs::create_dir_all(&dir).unwrap();
        let parts = state.split_pdf(2, dir.to_string_lossy().into()).unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].ends_with("(pág. 1-2).pdf"), "{parts:?}");
        assert!(parts[2].ends_with("(pág. 5).pdf"), "{parts:?}");
        assert_eq!(widths(Path::new(&parts[1])), [420.0, 430.0]);
    }

    #[test]
    fn export_pages_as_images() {
        let state = state_with_lines(&[&["hola"], &["chau"]]);
        let dir = tmp("images");
        std::fs::create_dir_all(&dir).unwrap();
        let files = state.export_images(vec![0, 1], dir.to_string_lossy().into(), ImageFormat::Png, 144).unwrap();
        assert_eq!(files.len(), 2);
        let image = image::open(&files[0]).unwrap();
        let width = state.with_doc(|o| Ok(get_page(o, 0)?.width().value)).unwrap();
        assert_eq!(image.width(), (width * 2.0).round() as u32);
        let jpg = state.export_images(vec![1], dir.to_string_lossy().into(), ImageFormat::Jpeg, 72).unwrap();
        assert!(jpg[0].ends_with("página 2.jpg"));
        assert!(image::open(&jpg[0]).is_ok());
    }

    #[test]
    fn pdf_from_images() {
        let landscape = tmp("landscape.png");
        image::RgbImage::from_pixel(300, 200, image::Rgb([200, 30, 30])).save(&landscape).unwrap();
        let portrait = tmp("portrait.jpg");
        image::RgbImage::from_pixel(200, 300, image::Rgb([30, 30, 200])).save(&portrait).unwrap();
        let out = tmp("from-images.pdf");
        images_to_pdf(
            vec![landscape.to_string_lossy().into(), portrait.to_string_lossy().into()],
            out.to_string_lossy().into(),
        )
        .unwrap();
        assert_eq!(widths(&out), [842.0, 595.0]);
        // The image fills the page (it isn't blank).
        let _guard = pdfium_lock();
        let doc = load(std::fs::read(&out).unwrap(), None).unwrap();
        let image = render(&doc.pages().get(0).unwrap(), 200).unwrap();
        let center = image.get_pixel(100, image.height() / 2).0;
        assert!(center[0] > 150 && center[2] < 80, "{center:?}");
        drop(doc);
        drop(_guard);
        assert!(images_to_pdf(vec![], out.to_string_lossy().into()).is_err());
    }

    #[test]
    fn compress_shrinks_big_images() {
        let state = state_with_pages(1);
        // Large image with soft noise: big when not lossily compressed.
        let big = image::RgbImage::from_fn(2400, 1800, |x, y| {
            image::Rgb([(x / 10 % 256) as u8, (y / 7 % 256) as u8, ((x * y) % 251) as u8])
        });
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(big).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        state.add_image(0, ImageSource::Data { bytes: png }, RelPoint { x: 0.5, y: 0.5 }, 0.8).unwrap();

        let out = tmp("compressed.pdf");
        let result = state.compress_pdf(out.to_string_lossy().into(), CompressLevel::Medium).unwrap();
        assert_eq!(result.images, 1);
        assert!(result.after * 3 < result.before, "{result:?}");
        assert_eq!(page_count(&out, None).unwrap(), 1);
        // The image is still visible.
        let _guard = pdfium_lock();
        let doc = load(std::fs::read(&out).unwrap(), None).unwrap();
        let image = render(&doc.pages().get(0).unwrap(), 300).unwrap();
        let inked = image.pixels().filter(|p| p.0[..3].iter().any(|&c| c < 200)).count();
        assert!(inked > 10_000, "{inked}");
    }

    #[test]
    fn protect_requires_password() {
        let state = state_with_lines(&[&["secreto"]]);
        let out = tmp("protected.pdf");
        assert!(state.protect_pdf(out.to_string_lossy().into(), "".into()).is_err());
        state.protect_pdf(out.to_string_lossy().into(), "clave ñ".into()).unwrap();
        assert!(page_count(&out, None).is_err());
        assert!(page_count(&out, Some("otra")).is_err());
        assert_eq!(page_count(&out, Some("clave ñ")).unwrap(), 1);

        // A protected PDF can be opened with the password, edited and undone.
        let opened = state_with_pages(1);
        opened.open_pdf(out.to_string_lossy().into(), Some("clave ñ".into())).unwrap();
        opened.add_note(0, RelPoint { x: 0.5, y: 0.5 }, "nota".into(), "#ffd400".into()).unwrap();
        opened.undo().unwrap();
        opened.redo().unwrap();
        // Tools that read the PDF on their own also work.
        let again = tmp("protected-again.pdf");
        opened.protect_pdf(again.to_string_lossy().into(), "nueva".into()).unwrap();
        assert_eq!(page_count(&again, Some("nueva")).unwrap(), 1);
        // Saving keeps the original password.
        let saved = tmp("protected-saved.pdf");
        opened.save_pdf(Some(saved.to_string_lossy().into())).unwrap();
        assert!(page_count(&saved, None).is_err());
        assert_eq!(page_count(&saved, Some("clave ñ")).unwrap(), 1);
    }
}
