//! Font choice for new or edited text.
//!
//! The standard PDF fonts (Helvetica, Times, Courier) only have Latin letters. When
//! another font is needed (the original lacks some letter, or the text is in Greek,
//! Cyrillic, Chinese…) the fonts installed on the system are searched: first the same
//! one the PDF used, then any font that has all the letters.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pdfium_render::prelude::*;

#[derive(Debug)]
struct SystemFont {
    path: PathBuf,
    /// Normalized PostScript name (e.g. "arialboldmt").
    postscript: String,
    /// Normalized family (e.g. "arial").
    family: String,
    bold: bool,
    italic: bool,
    size: u64,
}

static INDEX: OnceLock<Vec<SystemFont>> = OnceLock::new();

/// Only lowercase letters and digits: "Arial-BoldMT" → "arialboldmt".
fn normalize(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
        dirs.extend(home.map(|h| h.join("Library/Fonts")));
    } else if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
        dirs.push(windir.join("Fonts"));
        dirs.extend(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft\\Windows\\Fonts")));
    } else {
        dirs.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
        dirs.extend(home.iter().flat_map(|h| [h.join(".local/share/fonts"), h.join(".fonts")]));
    }
    dirs
}

fn collect(dir: &Path, depth: u8, out: &mut Vec<SystemFont>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < 4 {
                collect(&path, depth + 1, out);
            }
            continue;
        }
        // Collections (.ttc) can't be embedded as they are.
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase);
        if !matches!(ext.as_deref(), Some("ttf" | "otf")) {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        let Ok(face) = ttf_parser::Face::parse(&data, 0) else { continue };
        // Pdfium embeds TrueType fonts ("glyf" outlines) well, but not CFF ones.
        if face.tables().glyf.is_none() {
            continue;
        }
        let name = |id: u16| {
            face.names()
                .into_iter()
                .filter(|n| n.name_id == id)
                .find_map(|n| {
                    // Old Mac fonts store their names in Mac Roman (almost always ASCII).
                    n.to_string().or_else(|| {
                        (n.platform_id == ttf_parser::PlatformId::Macintosh && n.name.is_ascii())
                            .then(|| String::from_utf8_lossy(n.name).into_owned())
                    })
                })
        };
        let Some(postscript) = name(ttf_parser::name_id::POST_SCRIPT_NAME) else { continue };
        let family = name(ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
            .or_else(|| name(ttf_parser::name_id::FAMILY))
            .unwrap_or_default();
        out.push(SystemFont {
            postscript: normalize(&postscript),
            family: normalize(&family),
            bold: face.is_bold(),
            italic: face.is_italic(),
            size: data.len() as u64,
            path,
        });
    }
}

fn index() -> &'static [SystemFont] {
    INDEX.get_or_init(|| {
        let mut fonts = Vec::new();
        for dir in font_dirs() {
            collect(&dir, 0, &mut fonts);
        }
        // Lightest first: they are embedded whole in the PDF.
        fonts.sort_by_key(|f| f.size);
        fonts
    })
}

/// Builds the font index in the background so the first edit doesn't wait.
pub fn warm_up() {
    std::thread::spawn(index);
}

/// Style inferred from a font name ("Calibri-BoldItalic", "TimesNewRomanPS-ItalicMT"…).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Style {
    bold: bool,
    italic: bool,
    serif: bool,
    mono: bool,
}

fn style_of(name: &str) -> Style {
    let n = normalize(name);
    Style {
        bold: ["bold", "black", "heavy", "semibold", "demi"].iter().any(|s| n.contains(s)),
        italic: n.contains("italic") || n.contains("oblique"),
        serif: ["times", "serif", "georgia", "garamond", "cambria", "roman", "book"].iter().any(|s| n.contains(s))
            && !n.contains("sans"),
        mono: ["courier", "mono", "monaco", "menlo", "consola", "code"].iter().any(|s| n.contains(s)),
    }
}

/// Name without the subset prefix ("ABCDEF+Arial-BoldMT" → "Arial-BoldMT").
fn base_name(name: &str) -> &str {
    match name.split_once('+') {
        Some((prefix, rest)) if prefix.len() == 6 && prefix.chars().all(|c| c.is_ascii_uppercase()) => rest,
        _ => name,
    }
}

/// Letters the standard PDF fonts can show (WinAnsi encoding).
fn standard_covers(c: char) -> bool {
    matches!(c as u32, 0x20..=0x7E | 0xA0..=0xFF)
        || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
}

fn standard_font(doc: &mut PdfDocument<'static>, style: Style) -> PdfFontToken {
    let fonts = doc.fonts_mut();
    match (style.mono, style.serif, style.bold, style.italic) {
        (true, _, false, false) => fonts.courier(),
        (true, _, true, false) => fonts.courier_bold(),
        (true, _, false, true) => fonts.courier_oblique(),
        (true, _, true, true) => fonts.courier_bold_oblique(),
        (_, true, false, false) => fonts.times_roman(),
        (_, true, true, false) => fonts.times_bold(),
        (_, true, false, true) => fonts.times_italic(),
        (_, true, true, true) => fonts.times_bold_italic(),
        (_, _, false, false) => fonts.helvetica(),
        (_, _, true, false) => fonts.helvetica_bold(),
        (_, _, false, true) => fonts.helvetica_oblique(),
        (_, _, true, true) => fonts.helvetica_bold_oblique(),
    }
}

/// Loads the font if it has every letter of the text.
fn load_if_covers(doc: &mut PdfDocument<'static>, font: &SystemFont, text: &str) -> Option<PdfFontToken> {
    let data = std::fs::read(&font.path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;
    if !text.chars().all(|c| c.is_whitespace() || face.glyph_index(c).is_some()) {
        return None;
    }
    doc.fonts_mut().load_true_type_from_bytes(&data, true).ok()
}

/// Font for writing `text` in place of `original` (or for new text if None).
/// Prefers, in order: the same font installed on the system, a standard PDF font with the
/// same style, and any system font that has all the letters.
pub(crate) fn font_for(doc: &mut PdfDocument<'static>, original: Option<&str>, text: &str) -> PdfFontToken {
    let name = original.map(base_name).unwrap_or("");
    let style = style_of(name);

    if !name.is_empty() {
        let postscript = normalize(name);
        // "Arial-BoldMT" → family "arial".
        let family = normalize(name.split(['-', ',']).next().unwrap_or(name));
        let same = index().iter().filter(|f| f.postscript == postscript);
        let family_match = index().iter().filter(|f| {
            !family.is_empty()
                && (f.family == family || f.postscript.starts_with(&family))
                && f.bold == style.bold
                && f.italic == style.italic
        });
        for font in same.chain(family_match) {
            if let Some(token) = load_if_covers(doc, font, text) {
                return token;
            }
        }
    }

    if text.chars().all(|c| c.is_whitespace() || standard_covers(c)) {
        return standard_font(doc, style);
    }

    // Non-Latin letters: first common fonts of the same kind, then any font
    // that has them; with the same style if possible.
    let common: &[&str] = if style.mono {
        &["couriernew", "dejavusansmono", "consolas", "liberationmono", "menlo"]
    } else if style.serif {
        &["timesnewroman", "georgia", "dejavuserif", "notoserif", "liberationserif"]
    } else {
        &["arial", "segoeui", "dejavusans", "notosans", "liberationsans", "verdana", "tahoma"]
    };
    let rank = |f: &SystemFont| {
        let family = common.iter().position(|c| f.family == *c).unwrap_or(common.len());
        let same_style = f.bold == style.bold && f.italic == style.italic;
        (!same_style, family)
    };
    let mut candidates: Vec<&SystemFont> = index().iter().collect();
    // Stable: within the same rank the lightest stay first.
    candidates.sort_by_key(|f| rank(f));
    for font in candidates {
        if let Some(token) = load_if_covers(doc, font, text) {
            return token;
        }
    }
    standard_font(doc, style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_styles() {
        assert_eq!(base_name("ABCDEF+Arial-BoldMT"), "Arial-BoldMT");
        assert_eq!(base_name("Arial+Algo"), "Arial+Algo");
        let s = style_of("TimesNewRomanPS-BoldItalicMT");
        assert!(s.bold && s.italic && s.serif && !s.mono);
        assert!(style_of("CourierNewPSMT").mono);
        assert!(style_of("ABCDEF+Monaco").mono);
        assert!(!style_of("OpenSans-Regular").serif);
        assert!(standard_covers('ñ') && standard_covers('€') && !standard_covers('Ж'));
    }
}
