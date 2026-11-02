//! Document scanning: photos of sheets of paper become a straight, clean PDF.
//! The person marks each sheet's four corners on the photo; the sheet is straightened
//! (perspective correction), cleaned up (shadows and the paper's tint removed) and each photo
//! becomes one page. Everything happens on the device.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use image::imageops::FilterType;
use image::{DynamicImage, GrayImage, ImageDecoder, RgbImage};
use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter};

use crate::batch::Progress;
use crate::images::Picture;
use crate::pdf::{err, pdfium, pdfium_lock, RelPoint};

/// Longest side kept from a photo, in pixels: about 220 dpi on an A4 sheet, plenty for
/// documents, and a 12 MP phone photo shrinks to a size that's quick to process.
const MAX_SOURCE: u32 = 2600;
/// Longest side of a finished page, in pixels (about 200 dpi on A4).
const MAX_PAGE: u32 = 2300;
const PAGE_QUALITY: u8 = 80;
/// Quality of the photos kept in memory while scanning (they are re-encoded once).
const SOURCE_QUALITY: u8 = 92;
/// Long side of the PDF page, in points (A4's); the short side follows the sheet's shape.
const PAGE_LONG_SIDE: f32 = 842.0;

/// How the finished page looks.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    /// White paper and dark ink, without shadows: the usual for text.
    Document,
    Gray,
    /// The photo's colors, only straightened.
    Color,
}

/// A photo added to the scan, as the window sees it.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScanPhoto {
    id: u32,
    width: u32,
    height: u32,
}

/// A page to save: which photo, where the sheet is in it and how it should look.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScanPage {
    id: u32,
    /// The sheet's corners on the photo (0 to 1): top-left, top-right, bottom-right, bottom-left.
    corners: [RelPoint; 4],
    mode: ScanMode,
    /// Clockwise, in degrees: 0, 90, 180 or 270.
    rotation: u16,
}

/// The photos are kept compressed (a decoded photo takes about 20 MB, too much for a long scan
/// on a phone); the last one used stays decoded, since it's edited several times in a row.
struct Scans {
    next: u32,
    photos: BTreeMap<u32, Vec<u8>>,
    decoded: Option<(u32, Arc<RgbImage>)>,
}

static SCANS: Mutex<Scans> = Mutex::new(Scans { next: 1, photos: BTreeMap::new(), decoded: None });

fn scans() -> Result<std::sync::MutexGuard<'static, Scans>, String> {
    SCANS.lock().map_err(err)
}

// ---------- Photos ----------

/// Decodes a photo upright (phones store the rotation separately, in EXIF) and not larger
/// than `MAX_SOURCE`.
fn load_photo(bytes: &[u8]) -> Result<RgbImage, String> {
    let unreadable = |e: image::ImageError| format!("no se pudo leer la imagen: {e}");
    let mut decoder = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(err)?
        .into_decoder()
        .map_err(unreadable)?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).map_err(unreadable)?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    if image.width().max(image.height()) > MAX_SOURCE {
        image = image.resize(MAX_SOURCE, MAX_SOURCE, FilterType::Triangle);
    }
    Ok(image.to_rgb8())
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(image)
        .map_err(err)?;
    Ok(out)
}

/// Adds a photo to the scan. `temporary` photos (the camera's) are deleted once read.
pub fn add(path: String, temporary: bool) -> Result<ScanPhoto, String> {
    let bytes = crate::files::read(&path)?;
    if temporary {
        let _ = std::fs::remove_file(&path);
    }
    let image = load_photo(&bytes)?;
    let (width, height) = image.dimensions();
    let jpeg = encode_jpeg(&DynamicImage::ImageRgb8(image), SOURCE_QUALITY)?;
    let mut scans = scans()?;
    let id = scans.next;
    scans.next += 1;
    scans.photos.insert(id, jpeg);
    Ok(ScanPhoto { id, width, height })
}

fn decoded(id: u32) -> Result<Arc<RgbImage>, String> {
    let jpeg = {
        let scans = scans()?;
        if let Some((cached, image)) = &scans.decoded {
            if *cached == id {
                return Ok(image.clone());
            }
        }
        scans.photos.get(&id).cloned().ok_or("la foto ya no está disponible")?
    };
    let image = Arc::new(
        image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .map_err(err)?
            .to_rgb8(),
    );
    scans()?.decoded = Some((id, image.clone()));
    Ok(image)
}

/// The photo as is, `width` pixels wide, to mark the corners on it.
pub fn photo_jpeg(id: u32, width: u32) -> Result<Vec<u8>, String> {
    let photo = decoded(id)?;
    let width = width.clamp(16, photo.width());
    let height = (photo.height() as u64 * width as u64 / photo.width() as u64).max(1) as u32;
    let small = image::imageops::resize(&*photo, width, height, FilterType::Triangle);
    encode_jpeg(&DynamicImage::ImageRgb8(small), 85)
}

/// Forgets every photo (the scan window closed).
pub fn clear() {
    if let Ok(mut scans) = SCANS.lock() {
        scans.photos.clear();
        scans.decoded = None;
    }
}

// ---------- Straightening ----------

type Quad = [(f64, f64); 4];

/// Perspective transform that takes the corners `from` to the corners `to` (top-left,
/// top-right, bottom-right, bottom-left). `None` if the corners don't make a shape.
fn homography(from: &Quad, to: &Quad) -> Option<[f64; 8]> {
    // x = (a·u + b·v + c) / (g·u + h·v + 1), y = (d·u + e·v + f) / (g·u + h·v + 1)
    let mut m = [[0.0f64; 9]; 8];
    for (i, (&(u, v), &(x, y))) in from.iter().zip(to).enumerate() {
        m[2 * i] = [u, v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        m[2 * i + 1] = [0.0, 0.0, 0.0, u, v, 1.0, -u * y, -v * y, y];
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..8 {
        let pivot = (col..8).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-9 {
            return None;
        }
        m.swap(col, pivot);
        for row in 0..8 {
            if row != col {
                let factor = m[row][col] / m[col][col];
                for k in col..9 {
                    m[row][k] -= factor * m[col][k];
                }
            }
        }
    }
    let mut out = [0.0; 8];
    for (i, value) in out.iter_mut().enumerate() {
        *value = m[i][8] / m[i][i];
    }
    Some(out)
}

/// Where the transform `t` takes the point (u, v).
fn project(t: &[f64; 8], u: f64, v: f64) -> (f64, f64) {
    let w = t[6] * u + t[7] * v + 1.0;
    ((t[0] * u + t[1] * v + t[2]) / w, (t[3] * u + t[4] * v + t[5]) / w)
}

/// Size of the straightened sheet: its longest edges, scaled down to fit in `max_side`.
fn sheet_size(quad: &Quad, max_side: u32) -> Result<(u32, u32), String> {
    let dist = |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let width = dist(quad[0], quad[1]).max(dist(quad[3], quad[2]));
    let height = dist(quad[0], quad[3]).max(dist(quad[1], quad[2]));
    if width < 8.0 || height < 8.0 {
        return Err("las esquinas están demasiado juntas".into());
    }
    let scale = (max_side as f64 / width.max(height)).min(1.0);
    Ok(((width * scale).round().max(1.0) as u32, (height * scale).round().max(1.0) as u32))
}

/// Cuts the sheet out of the photo and straightens it into a `width` × `height` image.
fn warp(photo: &RgbImage, quad: &Quad, width: u32, height: u32) -> Result<RgbImage, String> {
    let (w, h) = (width as f64, height as f64);
    let t = homography(&[(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)], quad).ok_or("las esquinas no forman una hoja")?;
    let (pw, ph) = (photo.width() as usize, photo.height() as usize);
    let src = photo.as_raw();
    let mut out = RgbImage::new(width, height);
    let stride = width as usize * 3;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let rows_per_thread = (height as usize).div_ceil(threads);
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in out.chunks_mut(rows_per_thread * stride).enumerate() {
            scope.spawn(move || {
                for (r, row) in chunk.chunks_mut(stride).enumerate() {
                    let v = (chunk_index * rows_per_thread + r) as f64 + 0.5;
                    for (x, pixel) in row.chunks_mut(3).enumerate() {
                        let (sx, sy) = project(&t, x as f64 + 0.5, v);
                        let sx = (sx - 0.5).clamp(0.0, (pw - 1) as f64);
                        let sy = (sy - 0.5).clamp(0.0, (ph - 1) as f64);
                        // Bilinear: the four photo pixels around the point.
                        let (x0, y0) = (sx as usize, sy as usize);
                        let (x1, y1) = ((x0 + 1).min(pw - 1), (y0 + 1).min(ph - 1));
                        let (fx, fy) = (sx - x0 as f64, sy - y0 as f64);
                        for (c, value) in pixel.iter_mut().enumerate() {
                            let at = |x: usize, y: usize| src[(y * pw + x) * 3 + c] as f64;
                            let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
                            let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
                            *value = (top * (1.0 - fy) + bottom * fy).round() as u8;
                        }
                    }
                }
            });
        }
    });
    Ok(out)
}

// ---------- Cleaning up ----------

fn luma(image: &RgbImage) -> GrayImage {
    GrayImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b] = image.get_pixel(x, y).0;
        image::Luma([(0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32).round() as u8])
    })
}

/// Stretches the grays so the darkest and lightest 1% become black and white.
fn stretch(mut gray: GrayImage) -> GrayImage {
    let mut histogram = [0usize; 256];
    for p in gray.pixels() {
        histogram[p.0[0] as usize] += 1;
    }
    let total = gray.pixels().len();
    let level = |fraction: f64| {
        let mut seen = 0;
        for (value, count) in histogram.iter().enumerate() {
            seen += count;
            if seen as f64 >= total as f64 * fraction {
                return value as f32;
            }
        }
        255.0
    };
    let (low, high) = (level(0.01), level(0.99));
    if high - low < 16.0 {
        return gray;
    }
    for p in gray.pixels_mut() {
        p.0[0] = ((p.0[0] as f32 - low) / (high - low) * 255.0).clamp(0.0, 255.0).round() as u8;
    }
    gray
}

/// Separable filter over a small image: the max (`dilate`) or the mean of each pixel's
/// neighbors within `radius`, first along rows and then along columns.
fn filter(image: &GrayImage, radius: i64, dilate: bool) -> GrayImage {
    let (w, h) = (image.width() as i64, image.height() as i64);
    let pass = |src: &GrayImage, horizontal: bool| {
        GrayImage::from_fn(w as u32, h as u32, |x, y| {
            let (mut max, mut sum, mut count) = (0u8, 0u32, 0u32);
            for d in -radius..=radius {
                let (nx, ny) = if horizontal { (x as i64 + d, y as i64) } else { (x as i64, y as i64 + d) };
                if nx >= 0 && ny >= 0 && nx < w && ny < h {
                    let value = src.get_pixel(nx as u32, ny as u32).0[0];
                    max = max.max(value);
                    sum += value as u32;
                    count += 1;
                }
            }
            image::Luma([if dilate { max } else { (sum / count.max(1)) as u8 }])
        })
    };
    pass(&pass(image, true), false)
}

/// A shadow darkens paper to this fraction of its brightness at most; anything darker is ink,
/// a photo or a black box, and must stay dark.
const DEEPEST_SHADOW: f32 = 0.6;

/// How bright the paper is at each point, at low resolution: ink is wiped out by taking the
/// lightest value around each point, then smoothed so shadows fade gradually.
fn paper_brightness(gray: &GrayImage) -> GrayImage {
    const SIDE: u32 = 200;
    let scale = SIDE as f32 / gray.width().max(gray.height()) as f32;
    let small = image::imageops::resize(
        gray,
        ((gray.width() as f32 * scale).round() as u32).max(1),
        ((gray.height() as f32 * scale).round() as u32).max(1),
        FilterType::Triangle,
    );
    let mut paper = filter(&small, 3, true);
    // Dark areas wider than the filter (a photo, a black box) would pass for paper in shadow
    // and be washed out: no point is taken as darker than the deepest shadow.
    let mut levels: Vec<u8> = paper.pixels().map(|p| p.0[0]).collect();
    levels.sort_unstable();
    let lightest = levels[levels.len() * 9 / 10] as f32;
    let floor = (lightest * DEEPEST_SHADOW).round() as u8;
    for p in paper.pixels_mut() {
        p.0[0] = p.0[0].max(floor);
    }
    filter(&filter(&paper, 4, false), 4, false)
}

/// White paper and dark ink: each pixel is compared with the paper around it, so shadows,
/// uneven light and the paper's tint disappear while the writing stays.
fn clean_document(gray: &GrayImage) -> GrayImage {
    let paper = paper_brightness(gray);
    let (sx, sy) = (
        paper.width() as f32 / gray.width() as f32,
        paper.height() as f32 / gray.height() as f32,
    );
    let (pw, ph) = (paper.width() as f32, paper.height() as f32);
    GrayImage::from_fn(gray.width(), gray.height(), |x, y| {
        // Bilinear sample of the paper brightness under this pixel.
        let px = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, pw - 1.0);
        let py = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, ph - 1.0);
        let (x0, y0) = (px as u32, py as u32);
        let (x1, y1) = ((x0 + 1).min(paper.width() - 1), (y0 + 1).min(paper.height() - 1));
        let (fx, fy) = (px - x0 as f32, py - y0 as f32);
        let at = |x, y| paper.get_pixel(x, y).0[0] as f32;
        let background = (at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx) * (1.0 - fy)
            + (at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx) * fy;
        let ratio = gray.get_pixel(x, y).0[0] as f32 / background.max(1.0);
        // Paper (ratio near 1) turns white; ink keeps its darkness, a bit deepened.
        let level = ((ratio - 0.35) / (0.88 - 0.35)).clamp(0.0, 1.0).powf(1.4);
        image::Luma([(level * 255.0).round() as u8])
    })
}

fn enhance(image: RgbImage, mode: ScanMode) -> DynamicImage {
    match mode {
        ScanMode::Document => DynamicImage::ImageLuma8(clean_document(&luma(&image))),
        ScanMode::Gray => DynamicImage::ImageLuma8(stretch(luma(&image))),
        ScanMode::Color => DynamicImage::ImageRgb8(image),
    }
}

fn rotate(image: DynamicImage, rotation: u16) -> DynamicImage {
    match rotation % 360 {
        90 => image.rotate90(),
        180 => image.rotate180(),
        270 => image.rotate270(),
        _ => image,
    }
}

/// The page as it will look: straightened, cleaned up and rotated, at most `max_side` pixels.
fn finished(page: &ScanPage, max_side: u32) -> Result<DynamicImage, String> {
    let photo = decoded(page.id)?;
    let (w, h) = (photo.width() as f64, photo.height() as f64);
    let quad = page
        .corners
        .map(|c| ((c.x as f64).clamp(0.0, 1.0) * w, (c.y as f64).clamp(0.0, 1.0) * h));
    let (width, height) = sheet_size(&quad, max_side)?;
    let sheet = warp(&photo, &quad, width, height)?;
    Ok(rotate(enhance(sheet, page.mode), page.rotation))
}

/// Preview of a page as it will be saved, at most `width` pixels on its longest side.
pub fn preview_jpeg(page: &ScanPage, width: u32) -> Result<Vec<u8>, String> {
    encode_jpeg(&finished(page, width.clamp(16, MAX_PAGE))?, 85)
}

/// Saves the pages as a PDF, one per photo. `progress` is called after each page.
pub fn save(pages: Vec<ScanPage>, output: String, mut progress: impl FnMut(usize, usize)) -> Result<(), String> {
    if pages.is_empty() {
        return Err("no hay páginas para guardar".into());
    }
    let mut images = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let image = finished(page, MAX_PAGE)?;
        let (width, height) = (image.width(), image.height());
        images.push((encode_jpeg(&image, PAGE_QUALITY)?, width, height));
        progress(i + 1, pages.len());
    }
    let bytes = {
        let _guard = pdfium_lock();
        let mut doc = pdfium()?.create_new_pdf().map_err(err)?;
        for (jpeg, width, height) in images {
            let scale = PAGE_LONG_SIDE / width.max(height) as f32;
            let (page_w, page_h) = (width as f32 * scale, height as f32 * scale);
            let count = doc.pages().len();
            let mut page = doc
                .pages_mut()
                .create_page_at_index(PdfPagePaperSize::Custom(PdfPoints::new(page_w), PdfPoints::new(page_h)), count)
                .map_err(err)?;
            let mut object = Picture::from_bytes(jpeg)?.object(&doc)?;
            object.scale(page_w, page_h).map_err(err)?;
            page.objects_mut().add_image_object(object).map_err(err)?;
        }
        doc.save_to_bytes().map_err(err)?
    };
    crate::files::write(&output, &bytes)
}

// ---------- Tauri commands ----------
// Off the async runtime: decoding and straightening photos takes a moment.

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?
}

#[tauri::command]
pub async fn scan_add(path: String, temporary: bool) -> Result<ScanPhoto, String> {
    blocking(move || add(path, temporary)).await
}

#[tauri::command]
pub async fn scan_photo(id: u32, width: u32) -> Result<Response, String> {
    Ok(Response::new(blocking(move || photo_jpeg(id, width)).await?))
}

#[tauri::command]
pub async fn scan_preview(page: ScanPage, width: u32) -> Result<Response, String> {
    Ok(Response::new(blocking(move || preview_jpeg(&page, width)).await?))
}

#[tauri::command]
pub async fn scan_save(pages: Vec<ScanPage>, output: String, app: AppHandle) -> Result<(), String> {
    blocking(move || {
        save(pages, output, |done, total| {
            let _ = app.emit("scan-progress", Progress { done, total });
        })
    })
    .await
}

#[tauri::command]
pub async fn scan_clear() {
    clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use std::path::PathBuf;

    fn init() {
        // The helper initializes pdfium once for all tests.
        let _ = crate::pdf::tests::state_with_pages(1);
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pliego-scan-{}-{name}", std::process::id()))
    }

    const UNIT: Quad = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];

    /// A photo of a sheet on a dark table, seen at an angle: the sheet's corners are at
    /// `corners`, its paper is lit unevenly (darker to the right) and it has a black square
    /// in the middle.
    fn photo(corners: Quad) -> (PathBuf, ScanPhoto) {
        // From photo pixels to the sheet (0..1), to know what's under each pixel.
        let to_sheet = homography(&corners, &UNIT).unwrap();
        let image = RgbImage::from_fn(800, 600, |x, y| {
            let (u, v) = project(&to_sheet, x as f64 + 0.5, y as f64 + 0.5);
            if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
                return Rgb([40, 35, 30]);
            }
            if (0.4..0.6).contains(&u) && (0.4..0.6).contains(&v) {
                return Rgb([20, 20, 25]);
            }
            let light = (235.0 - 70.0 * u) as u8;
            Rgb([light, light, (light as f64 * 0.92) as u8])
        });
        let path = tmp(&format!("{}-{}.png", corners[0].0, corners[0].1));
        image.save(&path).unwrap();
        let added = add(path.to_string_lossy().into_owned(), false).unwrap();
        (path, added)
    }

    const SHEET: Quad = [(150.0, 80.0), (620.0, 120.0), (680.0, 540.0), (90.0, 500.0)];

    fn rel(corners: &Quad) -> [RelPoint; 4] {
        corners.map(|(x, y)| RelPoint { x: (x / 800.0) as f32, y: (y / 600.0) as f32 })
    }

    #[test]
    fn homography_maps_the_rectangle_onto_the_corners() {
        let rect = [(0.0, 0.0), (300.0, 0.0), (300.0, 400.0), (0.0, 400.0)];
        let t = homography(&rect, &SHEET).unwrap();
        for (&(u, v), &(x, y)) in rect.iter().zip(&SHEET) {
            let (px, py) = project(&t, u, v);
            assert!((px - x).abs() < 1e-6 && (py - y).abs() < 1e-6, "{px},{py} vs {x},{y}");
        }
        // Three corners on a line don't make a sheet.
        assert!(homography(&UNIT, &[(0.0, 0.0), (5.0, 5.0), (10.0, 10.0), (0.0, 0.0)]).is_none());
    }

    #[test]
    fn straightens_the_sheet_and_whitens_the_paper() {
        let (path, added) = photo(SHEET);
        let page = ScanPage { id: added.id, corners: rel(&SHEET), mode: ScanMode::Document, rotation: 0 };
        let image = finished(&page, 1000).unwrap().to_luma8();
        let (w, h) = image.dimensions();
        // The sheet's longest edges in the photo: about 591 wide and 424 tall.
        assert!((w as i32 - 591).abs() < 3 && (h as i32 - 424).abs() < 3, "{w}×{h}");
        let at = |fx: f64, fy: f64| image.get_pixel((fx * w as f64) as u32, (fy * h as f64) as u32).0[0];
        // The square ends up in the middle, and black.
        assert!(at(0.5, 0.5) < 60, "center {}", at(0.5, 0.5));
        // The paper is white on both sides, although the photo was darker on the right.
        assert!(at(0.15, 0.2) > 245 && at(0.85, 0.8) > 245, "paper {} {}", at(0.15, 0.2), at(0.85, 0.8));
        // The table around the sheet was cut out: the edges are paper too.
        assert!(at(0.02, 0.5) > 200 && at(0.98, 0.5) > 200, "edges {} {}", at(0.02, 0.5), at(0.98, 0.5));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rotation_and_modes_change_the_page() {
        let (path, added) = photo([(100.0, 100.0), (500.0, 100.0), (500.0, 400.0), (100.0, 400.0)]);
        let corners = rel(&[(100.0, 100.0), (500.0, 100.0), (500.0, 400.0), (100.0, 400.0)]);
        let page = ScanPage { id: added.id, corners, mode: ScanMode::Color, rotation: 0 };
        let straight = finished(&page, 1000).unwrap();
        assert_eq!((straight.width(), straight.height()), (400, 300));
        assert!(matches!(straight, DynamicImage::ImageRgb8(_)));
        let turned = finished(&ScanPage { rotation: 90, mode: ScanMode::Gray, ..page.clone() }, 1000).unwrap();
        assert_eq!((turned.width(), turned.height()), (300, 400));
        assert!(matches!(turned, DynamicImage::ImageLuma8(_)));
        // Limited to the size asked for.
        let small = finished(&page, 200).unwrap();
        assert_eq!(small.width().max(small.height()), 200);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saves_one_page_per_photo() {
        init();
        let (path_a, a) = photo(SHEET);
        let (path_b, b) = photo([(10.0, 10.0), (790.0, 10.0), (790.0, 590.0), (10.0, 590.0)]);
        let full = rel(&[(0.0, 0.0), (800.0, 0.0), (800.0, 600.0), (0.0, 600.0)]);
        let output = tmp("escaneo.pdf");
        let mut progress = Vec::new();
        save(
            vec![
                ScanPage { id: a.id, corners: rel(&SHEET), mode: ScanMode::Document, rotation: 0 },
                ScanPage { id: b.id, corners: full, mode: ScanMode::Color, rotation: 90 },
            ],
            output.to_string_lossy().into_owned(),
            |done, total| progress.push((done, total)),
        )
        .unwrap();
        assert_eq!(progress, vec![(1, 2), (2, 2)]);
        let _guard = pdfium_lock();
        let doc = pdfium().unwrap().load_pdf_from_file(&output, None).unwrap();
        assert_eq!(doc.pages().len(), 2);
        let second = doc.pages().get(1).unwrap();
        // 800×600 photo turned a quarter: a portrait page, A4 tall.
        assert!((second.height().value - PAGE_LONG_SIDE).abs() < 0.5 && second.width().value < second.height().value);
        drop(doc);
        for p in [path_a, path_b, output] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn camera_photos_are_deleted_once_read() {
        let path = tmp("camara.png");
        RgbImage::from_pixel(40, 30, Rgb([200, 200, 200])).save(&path).unwrap();
        let added = add(path.to_string_lossy().into_owned(), true).unwrap();
        assert_eq!((added.width, added.height), (40, 30));
        assert!(!path.exists());
        assert!(photo_jpeg(added.id, 20).is_ok());
    }
}
