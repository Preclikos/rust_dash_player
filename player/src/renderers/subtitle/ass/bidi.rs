//! Bidirectional text (libass ass_shaper.c with FriBidi): embedding levels
//! and visual order.
//!
//! libass computes levels with a left-to-right base direction (VSFilter
//! compatibility; `Encoding=-1` would auto-detect) separately for every
//! style run — a run ends at a paragraph separator, before a glyph that
//! starts a new style run, or after one with letter spacing — and reorders
//! each such run on its own within a line. RTL runs are shaped right to
//! left; HarfBuzz then also mirrors brackets.

use unicode_bidi::{bidi_class, BidiClass, BidiInfo, Level};

use super::layout::Glyph;

/// FriBidi `FRIBIDI_TYPE_BS`.
pub fn is_paragraph_separator(c: u32) -> bool {
    matches!(c, 0x0A | 0x0D | 0x1C..=0x1E | 0x85 | 0x2029)
}

fn may_be_rtl(c: u32) -> bool {
    let Some(ch) = char::from_u32(c) else { return false };
    matches!(
        bidi_class(ch),
        BidiClass::R | BidiClass::AL | BidiClass::AN | BidiClass::RLE | BidiClass::RLO | BidiClass::RLI | BidiClass::FSI
    )
}

/// Where libass closes a bidi segment after glyph `i`.
fn segment_ends(glyphs: &[Glyph], i: usize) -> bool {
    i + 1 == glyphs.len()
        || is_paragraph_separator(glyphs[i].symbol)
        || glyphs[i + 1].starts_new_run
        || glyphs[i].hspacing != 0.0
}

/// Embedding level of every glyph (all 0 for text with nothing
/// right-to-left in it).
pub fn levels(glyphs: &[Glyph]) -> Vec<u8> {
    let mut out = vec![0u8; glyphs.len()];
    if !glyphs.iter().any(|g| may_be_rtl(g.symbol)) {
        return out;
    }
    let mut start = 0;
    for i in 0..glyphs.len() {
        if !segment_ends(glyphs, i) {
            continue;
        }
        let seg = &glyphs[start..=i];
        if seg.iter().any(|g| may_be_rtl(g.symbol)) {
            // Object replacement for drawings; anything unrepresentable as
            // a neutral.
            let text: String = seg.iter().map(|g| char::from_u32(g.symbol).unwrap_or('\u{FFFC}')).collect();
            let info = BidiInfo::new(&text, Some(Level::ltr()));
            for (k, (byte, _)) in text.char_indices().enumerate() {
                out[start + k] = info.levels[byte].number();
            }
        }
        start = i + 1;
    }
    out
}

/// Visual order (`map[visual] = logical`) of every glyph: each reorder
/// segment — a line, cut further at style runs like the level
/// computation — reordered by its levels (UAX #9 rule L2).
pub fn visual_order(glyphs: &[Glyph], levels: &[u8]) -> Vec<usize> {
    let len = glyphs.len();
    let mut map: Vec<usize> = (0..len).collect();
    if levels.iter().all(|&l| l == 0) {
        return map;
    }
    let mut start = 0;
    for i in 0..len {
        let ends = i + 1 == len
            || glyphs[i + 1].linebreak != 0
            || is_paragraph_separator(glyphs[i].symbol)
            || glyphs[i + 1].starts_new_run
            || glyphs[i].hspacing != 0.0;
        if !ends {
            continue;
        }
        let seg: Vec<Level> = levels[start..=i].iter().map(|&l| Level::new(l).unwrap_or(Level::ltr())).collect();
        for (v, l) in BidiInfo::reorder_visual(&seg).into_iter().enumerate() {
            map[start + v] = start + l;
        }
        start = i + 1;
    }
    map
}
