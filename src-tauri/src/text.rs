//! Search and selection of the PDF text (for ⌘F search and copy).

use pdfium_render::prelude::*;
use serde::Serialize;
use tauri::State;

use crate::pdf::{err, get_page, rect_to_rel, to_rect, AppState, RelPoint, RelRect};

/// Maximum number of matches returned by a search.
const MAX_HITS: usize = 5_000;

#[derive(Serialize, Debug)]
pub struct SearchHit {
    pub(crate) page: PdfPageIndex,
    /// One rectangle per line the match spans.
    pub(crate) rects: Vec<RelRect>,
}

#[derive(Serialize, Debug)]
pub struct TextSelection {
    pub(crate) text: String,
    pub(crate) rects: Vec<RelRect>,
}

/// A character of the page with its box. Spaces and line breaks that pdfium adds
/// on its own have no box.
struct CharBox {
    c: char,
    rect: Option<PdfRect>,
}

fn page_chars(page: &PdfPage) -> Result<Vec<CharBox>, String> {
    let text = page.text().map_err(err)?;
    Ok(text
        .chars()
        .iter()
        .map(|ch| {
            let c = ch.unicode_char().unwrap_or(' ');
            let generated = ch.is_generated().unwrap_or(false);
            let rect = (!generated && !c.is_whitespace())
                .then(|| ch.loose_bounds().ok())
                .flatten()
                .filter(|r| r.width().value > 0.0 || r.height().value > 0.0);
            CharBox { c, rect }
        })
        .collect())
}

/// Lowercase and without accents, so "accion" finds "Acción".
fn fold(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        // Typographic quotes and dashes are searched as the plain ones.
        '‘' | '’' | '´' => '\'',
        '“' | '”' => '"',
        '–' | '—' | '\u{2010}' | '\u{2011}' => '-',
        '\u{fb01}' => 'f', // "fi" ligature: at least it doesn't split the word
        c if c.is_whitespace() => ' ',
        c => c,
    }
}

/// Merges the boxes of consecutive characters on the same line.
fn line_rects(page: &PdfPage, boxes: impl IntoIterator<Item = PdfRect>) -> Result<Vec<RelRect>, String> {
    let mut lines: Vec<PdfRect> = Vec::new();
    for r in boxes {
        if let Some(last) = lines.last_mut() {
            // Same line: they overlap vertically by more than half of the smaller height.
            let overlap = last.top().value.min(r.top().value) - last.bottom().value.max(r.bottom().value);
            let min_height = last.height().value.min(r.height().value);
            if overlap > min_height * 0.5 {
                *last = PdfRect::new_from_values(
                    last.bottom().value.min(r.bottom().value),
                    last.left().value.min(r.left().value),
                    last.top().value.max(r.top().value),
                    last.right().value.max(r.right().value),
                );
                continue;
            }
        }
        lines.push(r);
    }
    lines.into_iter().map(|r| rect_to_rel(page, r)).collect()
}

fn search_page(page: &PdfPage, needle: &[char], limit: usize) -> Result<Vec<Vec<RelRect>>, String> {
    let chars = page_chars(page)?;
    // Folded text without repeated spaces; `map[i]` is the original character at position i.
    let mut haystack: Vec<char> = Vec::with_capacity(chars.len());
    let mut map: Vec<usize> = Vec::with_capacity(chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let c = fold(ch.c);
        if c == ' ' && haystack.last().is_none_or(|&last| last == ' ') {
            continue;
        }
        haystack.push(c);
        map.push(i);
    }

    let mut hits = Vec::new();
    let mut start = 0;
    while hits.len() < limit && start + needle.len() <= haystack.len() {
        if haystack[start..start + needle.len()] == *needle {
            let first = map[start];
            let last = map[start + needle.len() - 1];
            let boxes = chars[first..=last].iter().filter_map(|c| c.rect);
            let rects = line_rects(page, boxes)?;
            if !rects.is_empty() {
                hits.push(rects);
            }
            start += needle.len();
        } else {
            start += 1;
        }
    }
    Ok(hits)
}

impl AppState {
    /// Searches the whole document ignoring case and accents.
    pub fn search_text(&self, query: &str) -> Result<Vec<SearchHit>, String> {
        let needle: Vec<char> = query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .map(fold)
            .collect();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        self.with_doc(|open| {
            let mut hits = Vec::new();
            for index in 0..open.doc.pages().len() {
                let page = get_page(open, index)?;
                let limit = MAX_HITS - hits.len();
                for rects in search_page(&page, &needle, limit)? {
                    hits.push(SearchHit { page: index, rects });
                }
                if hits.len() >= MAX_HITS {
                    break;
                }
            }
            Ok(hits)
        })
    }

    /// Text whose characters have their center inside the rectangle, in reading order.
    pub fn text_in_rect(
        &self,
        index: PdfPageIndex,
        from: RelPoint,
        to: RelPoint,
    ) -> Result<Option<TextSelection>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let area = to_rect(&page, from, to)?;
            let chars = page_chars(&page)?;
            let inside = |r: &PdfRect| {
                let x = (r.left().value + r.right().value) / 2.0;
                let y = (r.bottom().value + r.top().value) / 2.0;
                x >= area.left().value
                    && x <= area.right().value
                    && y >= area.bottom().value
                    && y <= area.top().value
            };
            let selected: Vec<bool> = chars.iter().map(|c| c.rect.as_ref().is_some_and(inside)).collect();
            let Some(first) = selected.iter().position(|&s| s) else {
                return Ok(None);
            };
            let last = selected.iter().rposition(|&s| s).unwrap();

            // Spaces and line breaks between chosen characters are copied too.
            let mut text = String::new();
            for (i, ch) in chars.iter().enumerate().take(last + 1).skip(first) {
                if selected[i] {
                    text.push(ch.c);
                } else if ch.rect.is_none() && ch.c.is_whitespace() {
                    let c = if ch.c == '\r' || ch.c == '\n' { '\n' } else { ' ' };
                    if !text.ends_with(['\n', ' ']) {
                        text.push(c);
                    } else if c == '\n' && text.ends_with(' ') {
                        text.pop();
                        text.push('\n');
                    }
                }
            }
            let boxes = chars
                .iter()
                .zip(&selected)
                .filter(|(_, s)| **s)
                .filter_map(|(c, _)| c.rect);
            Ok(Some(TextSelection {
                text: text.trim().to_string(),
                rects: line_rects(&page, boxes)?,
            }))
        })
    }
}

#[cfg(test)]
impl AppState {
    /// Rectangles of the selected lines (for testing other features).
    pub(crate) fn text_in_rect_for_tests(&self, index: PdfPageIndex, from: RelPoint, to: RelPoint) -> Vec<RelRect> {
        self.text_in_rect(index, from, to).unwrap().unwrap().rects
    }
}

#[tauri::command]
pub async fn search_text(query: String, state: State<'_, AppState>) -> Result<Vec<SearchHit>, String> {
    state.search_text(&query)
}

#[tauri::command]
pub async fn text_in_rect(
    index: PdfPageIndex,
    from: RelPoint,
    to: RelPoint,
    state: State<'_, AppState>,
) -> Result<Option<TextSelection>, String> {
    state.text_in_rect(index, from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::tests::state_with_lines;

    fn p(x: f32, y: f32) -> RelPoint {
        RelPoint { x, y }
    }

    #[test]
    fn search_ignores_case_and_accents() {
        let state = state_with_lines(&[&["Acción de prueba", "otra ACCION más"], &["sin nada", "accion final"]]);
        let hits = state.search_text("accion").unwrap();
        assert_eq!(hits.iter().map(|h| h.page).collect::<Vec<_>>(), [0, 0, 1]);
        assert!(hits.iter().all(|h| h.rects.len() == 1));
        // Several words, with extra spaces in the query.
        let hits = state.search_text("  de   prueba ").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(state.search_text("inexistente").unwrap().is_empty());
        assert!(state.search_text("   ").unwrap().is_empty());
    }

    #[test]
    fn search_hit_is_where_the_word_is() {
        let state = state_with_lines(&[&["uno dos tres"]]);
        let hit = &state.search_text("dos").unwrap()[0];
        let rect = &hit.rects[0];
        let selection = state.text_in_rect(0, rect.from, rect.to).unwrap().unwrap();
        assert_eq!(selection.text, "dos");
    }

    #[test]
    fn select_text_across_lines() {
        let state = state_with_lines(&[&["Primera línea", "Segunda línea", "Tercera"]]);
        let all = state.text_in_rect(0, p(0.0, 0.0), p(1.0, 1.0)).unwrap().unwrap();
        assert_eq!(all.text, "Primera línea\nSegunda línea\nTercera");
        assert_eq!(all.rects.len(), 3);
        assert!(state.text_in_rect(0, p(0.9, 0.9), p(1.0, 1.0)).unwrap().is_none());
    }
}
