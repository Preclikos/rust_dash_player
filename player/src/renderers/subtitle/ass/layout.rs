//! An event's text as positioned glyphs: libass ass_render.c from
//! `parse_events` to `calculate_rotation_params` — glyph state capture,
//! run splitting and shaping, outline metrics, line wrapping (`WrapStyle`
//! 0–3, VSFilter's smart rebalancing), whitespace trimming, line metrics,
//! karaoke timing, alignment, baseline shear and the event's screen
//! position (margins, `\pos`/`\move`, `Banner`/`Scroll`).
//!
//! Positions are libass's 26.6 integers so rounding happens where it
//! happens in libass.

use std::sync::Arc;

use crate::parsers::ass::{text_chars, Item, HALIGN_CENTER, HALIGN_LEFT, HALIGN_RIGHT, VALIGN_CENTER, VALIGN_SUB, VALIGN_TOP};

use super::context::{d6, from_d6, lrint, Effect, Scroll, State, EVENT_HSCROLL, EVENT_POSITIONED, EVENT_VSCROLL};
use super::outline::{Outline, Rect};
use super::shape::{shape_run, RunOptions};
use super::Caches;

/// One shaped glyph of a cluster (libass chains these through `next`).
#[derive(Clone, Debug, Default)]
pub struct Part {
    /// Font database face the glyph id belongs to.
    pub face: usize,
    pub glyph: u32,
    pub offset: (i32, i32),
    pub advance: (i32, i32),
    pub pos: (i32, i32),
    /// Outline in glyph space (26.6 at the glyph's font size, y down).
    pub outline: Option<Arc<Outline>>,
    pub cbox: Option<Rect>,
    /// `transform.scale` / `transform.offset`: glyph space → layout space.
    pub scale: (f64, f64),
    pub toffset: (f64, f64),
    pub bbox: (i32, i32, i32, i32),
    pub asc: i32,
    pub desc: i32,
    pub shift: (i32, i32),
}

#[derive(Clone, Debug)]
pub struct Drawing {
    pub text: String,
    pub scale: i32,
    pub pbo: i32,
}

/// libass `GlyphInfo`: one character with the render state it was set in.
#[derive(Clone, Debug)]
pub struct Glyph {
    pub symbol: u32,
    pub skip: bool,
    pub is_trimmed_whitespace: bool,
    pub font: Option<usize>,
    pub face_index: usize,
    pub glyph_index: u32,
    pub font_size: f64,
    pub drawing: Option<Drawing>,
    pub linebreak: u8,
    pub starts_new_run: bool,
    pub c: [u32; 4],
    pub effect_type: Effect,
    pub effect_timing: i32,
    pub effect_skip_timing: i32,
    pub reset_effect: bool,
    pub be: i32,
    pub blur: f64,
    pub shadow_x: f64,
    pub shadow_y: f64,
    pub frx: f64,
    pub fry: f64,
    pub frz: f64,
    pub fax: f64,
    pub fay: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub scale_fix: f64,
    pub border_style: i32,
    pub border_x: f64,
    pub border_y: f64,
    pub hspacing: f64,
    pub hspacing_scaled: i32,
    pub italic: i32,
    pub bold: i32,
    pub flags: u8,
    pub fade: i32,
    pub shape_run: usize,
    /// Bidi embedding level (odd = right to left).
    pub level: u8,
    /// Unicode script, common/inherited resolved from context.
    pub script: harfrust::Script,
    pub cluster_advance: (i32, i32),
    /// `parts[0]` is the cluster's own glyph; more when shaping produced
    /// several glyphs for one character.
    pub parts: Vec<Part>,
}

impl Glyph {
    pub fn root(&self) -> &Part {
        &self.parts[0]
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Line {
    pub asc: f64,
    pub desc: f64,
}

/// libass `TextInfo` plus the event's computed placement.
pub struct Text {
    pub glyphs: Vec<Glyph>,
    pub lines: Vec<Line>,
    pub height: f64,
    pub border_top: i32,
    pub border_bottom: i32,
    pub border_x: i32,
    pub bbox: (f64, f64, f64, f64),
    pub device_x: f64,
    pub device_y: f64,
    /// Visual order: `cmap[visual] = logical` glyph index.
    pub cmap: Vec<usize>,
}

/// VSFilter's font size scaling: libass `fix_glyph_scaling` without
/// hinting — outlines are loaded at 256 px and scaled from there.
const FT_SIZE: f64 = 256.0;

/// libass `is_harfbuzz_ignorable`.
fn is_ignorable(s: u32) -> bool {
    match s >> 8 {
        0x00 => s == 0x00AD,
        0x03 => s == 0x034F,
        0x06 => s == 0x061C,
        0x17 => (0x17B4..=0x17B5).contains(&s),
        0x18 => (0x180B..=0x180E).contains(&s),
        0x20 => (0x200B..=0x200F).contains(&s) || (0x202A..=0x202E).contains(&s) || (0x2060..=0x206F).contains(&s),
        0xFE => (0xFE00..=0xFE0F).contains(&s) || s == 0xFEFF,
        0xFF => (0xFFF0..=0xFFF8).contains(&s),
        0x1D1 => (0x1D173..=0x1D17A).contains(&s),
        _ => (0xE0000..=0xE0FFF).contains(&s),
    }
}

/// libass `parse_events`: walk the event's items, executing tags, and
/// capture a glyph per character (or drawing).
pub fn parse_events(st: &mut State<'_>, caches: &mut Caches) -> Vec<Glyph> {
    let mut glyphs = Vec::new();
    let event = st.event;
    for item in &event.items {
        match item {
            Item::Override(tags) => st.apply_tags(tags, 1.0, false, &caches.db, &mut caches.fonts),
            Item::Text(raw) => {
                if st.drawing_scale != 0 {
                    let g = capture(st, 0xFFFC, Some(Drawing { text: raw.clone(), scale: st.drawing_scale, pbo: st.pbo as i32 }));
                    glyphs.push(g);
                    continue;
                }
                let vertical = st.font.is_some_and(|f| caches.fonts.font(f).desc.vertical);
                let wrap = st.wrap_style;
                for ch in text_chars(raw, wrap) {
                    let mut g = capture(st, ch as u32, None);
                    if vertical && g.symbol >= super::context::VERTICAL_LOWER_BOUND {
                        g.flags |= super::context::DECO_ROTATE;
                    }
                    glyphs.push(g);
                }
            }
        }
    }
    glyphs
}

fn capture(st: &mut State<'_>, symbol: u32, drawing: Option<Drawing>) -> Glyph {
    let mut g = Glyph {
        symbol,
        skip: false,
        is_trimmed_whitespace: false,
        font: st.font,
        face_index: 0,
        glyph_index: 0,
        // VSFilter compatibility: font glyphs use PlayResY scaling in both dimensions
        font_size: (st.font_size * st.screen_scale_y).abs(),
        drawing,
        linebreak: 0,
        starts_new_run: false,
        c: st.c,
        effect_type: st.effect_type,
        effect_timing: st.effect_timing,
        effect_skip_timing: st.effect_skip_timing,
        reset_effect: st.reset_effect,
        be: st.be,
        blur: st.blur,
        shadow_x: st.shadow_x,
        shadow_y: st.shadow_y,
        frx: st.frx,
        fry: st.fry,
        frz: st.frz,
        fax: st.fax,
        fay: st.fay,
        scale_x: st.scale_x,
        scale_y: st.scale_y,
        scale_fix: 1.0,
        border_style: st.border_style,
        border_x: st.border_x,
        border_y: st.border_y,
        hspacing: st.hspacing,
        hspacing_scaled: 0,
        italic: st.italic,
        bold: st.bold,
        flags: st.flags,
        fade: st.fade,
        shape_run: 0,
        level: 0,
        script: harfrust::Script::COMMON,
        cluster_advance: (0, 0),
        parts: vec![Part::default()],
    };
    if g.drawing.is_none() {
        g.hspacing_scaled = d6(g.hspacing * st.screen_scale_x / st.frame.par * g.scale_x);
        // fix_glyph_scaling, hinting off.
        if g.font_size != 0.0 {
            let mul = g.font_size / FT_SIZE;
            g.scale_fix = 1.0 / mul;
            g.scale_x *= mul;
            g.scale_y *= mul;
            g.font_size = FT_SIZE;
        }
    }
    st.effect_type = Effect::None;
    st.effect_timing = 0;
    st.effect_skip_timing = 0;
    st.reset_effect = false;
    g
}

/// libass `ass_shaper_determine_script`: Common and Inherited characters
/// take the script of what precedes them (or, at the start, follows).
fn determine_script(glyphs: &mut [Glyph]) {
    use harfrust::Script;
    let neutral = |s: Script| s == Script::COMMON || s == Script::INHERITED;
    let mut last = Script::UNKNOWN;
    let mut backwards = false;
    for g in glyphs.iter_mut() {
        g.script = super::shape::char_script(g.symbol);
        if neutral(g.script) {
            if last != Script::UNKNOWN {
                g.script = last;
            } else {
                backwards = true;
            }
        } else {
            last = g.script;
        }
    }
    if backwards {
        let mut last = Script::UNKNOWN;
        for g in glyphs.iter_mut().rev() {
            if neutral(g.script) {
                if last != Script::UNKNOWN {
                    g.script = last;
                }
            } else {
                last = g.script;
            }
        }
    }
}

/// The script's `Language:` header as a static tag (harfrust wants
/// `'static` here; the header is two letters, from a short known list).
fn static_language(code: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        "ar", "bg", "cs", "da", "de", "el", "en", "es", "et", "fa", "fi", "fr", "he", "hi", "hr", "hu", "hy", "id", "it", "ja",
        "ka", "ko", "lt", "lv", "mn", "nl", "no", "pl", "pt", "ro", "ru", "sk", "sl", "sr", "sv", "th", "tr", "uk", "vi", "zh",
    ];
    let code = code.to_ascii_lowercase();
    KNOWN.iter().copied().find(|k| *k == code)
}

/// libass `split_style_runs`.
pub fn split_style_runs(glyphs: &mut [Glyph], caches: &Caches) {
    if glyphs.is_empty() {
        return;
    }
    let mut last_effect = glyphs[0].effect_type;
    glyphs[0].starts_new_run = true;
    for i in 1..glyphs.len() {
        let (a, b) = glyphs.split_at_mut(i);
        let last = &a[i - 1];
        let info = &mut b[0];
        let family = |g: &Glyph| g.font.map(|f| caches.fonts.font(f).desc.family.as_str());
        let vertical = |g: &Glyph| g.font.is_some_and(|f| caches.fonts.font(f).desc.vertical);
        info.starts_new_run = info.effect_timing != 0
            || (info.effect_type != Effect::None && info.effect_type != last_effect)
            || info.drawing.is_some()
            || last.drawing.is_some()
            || family(last) != family(info)
            || vertical(last) != vertical(info)
            || last.font_size != info.font_size
            || last.c != info.c
            || last.be != info.be
            || last.blur != info.blur
            || last.shadow_x != info.shadow_x
            || last.shadow_y != info.shadow_y
            || last.frx != info.frx
            || last.fry != info.fry
            || last.frz != info.frz
            || last.fax != info.fax
            || last.fay != info.fay
            || last.scale_x != info.scale_x
            || last.scale_y != info.scale_y
            || last.border_style != info.border_style
            || last.border_x != info.border_x
            || last.border_y != info.border_y
            || last.hspacing != info.hspacing
            || last.italic != info.italic
            || last.bold != info.bold
            || (last.flags ^ info.flags) & !super::context::DECO_ROTATE != 0;
        if info.effect_type != Effect::None {
            last_effect = info.effect_type;
        }
    }
}

/// libass `ass_shaper_find_runs` + `ass_shaper_shape` + `shape_harfbuzz`.
pub fn shape(glyphs: &mut [Glyph], st: &State<'_>, caches: &mut Caches) {
    let len = glyphs.len();
    determine_script(glyphs);
    for g in glyphs.iter_mut() {
        if is_ignorable(g.symbol) {
            g.skip = true;
        }
    }
    let mut run = 0;
    for i in 0..len {
        if glyphs[i].drawing.is_none() && !glyphs[i].skip {
            if let Some(f) = glyphs[i].font {
                let (fi, gi) = caches.fonts.font_mut(f).glyph(&caches.db, glyphs[i].symbol);
                glyphs[i].face_index = fi;
                glyphs[i].glyph_index = gi;
            }
        }
        if i > 0 {
            let (a, b) = glyphs.split_at_mut(i);
            let last = &a[i - 1];
            let info = &mut b[0];
            if last.font != info.font
                || (!info.skip && last.face_index != info.face_index)
                || last.script != info.script
                || info.starts_new_run
                || info.hspacing != 0.0
                || last.flags != info.flags
            {
                run += 1;
            } else if info.skip {
                info.face_index = last.face_index;
            }
        }
        glyphs[i].shape_run = run;
    }

    let levels = super::bidi::levels(glyphs);
    for (g, l) in glyphs.iter_mut().zip(levels) {
        g.level = l;
    }

    for g in glyphs.iter_mut() {
        g.skip = true;
    }
    let mut i = 0;
    while i < len {
        if glyphs[i].drawing.is_some() {
            glyphs[i].skip = false;
            i += 1;
            continue;
        }
        let offset = i;
        let run_id = glyphs[i].shape_run;
        let level = glyphs[i].level;
        while i + 1 < len && glyphs[i + 1].shape_run == run_id && glyphs[i + 1].level == level {
            i += 1;
        }
        let Some(font) = glyphs[offset].font else {
            i += 1;
            continue;
        };
        let face_id = caches.fonts.font(font).faces[glyphs[offset].face_index.min(caches.fonts.font(font).faces.len() - 1)];
        let face = caches.db.face(face_id).clone();
        let text: Vec<char> = glyphs[offset..=i].iter().map(|g| char::from_u32(g.symbol).unwrap_or('\u{FFFD}')).collect();
        let vertical = caches.fonts.font(font).desc.vertical;
        let script = glyphs[offset].script;
        let language = st.script.language.as_deref().and_then(static_language).or_else(|| super::shape::script_language(script));
        let opts = RunOptions {
            script: Some(script),
            language,
            vertical,
            rtl: level % 2 == 1,
            kerning: st.script.kerning,
            ligatures: glyphs[offset].hspacing == 0.0,
        };
        let shaped = shape_run(&face, glyphs[offset].font_size, &text, &opts);
        for s in shaped {
            let idx = offset + s.cluster;
            let info = &mut glyphs[idx];
            let part = if !info.skip {
                let copy = info.parts.last().cloned().unwrap_or_default();
                info.parts.push(copy);
                info.parts.last_mut().unwrap()
            } else {
                &mut info.parts[0]
            };
            part.face = face_id;
            part.glyph = s.glyph;
            // A turned glyph advances by its height (libass reports the
            // vertical advance to HarfBuzz for these).
            let s = if info.flags & super::context::DECO_ROTATE != 0 {
                let k = face.units_to_d6(info.font_size);
                super::shape::Shaped { x_advance: (super::outline::unit_v_advance(&face, s.glyph) * k).round() as i32, ..s }
            } else {
                s
            };
            part.offset = (lrint(s.x_offset as f64 * info.scale_x) as i32, lrint(-s.y_offset as f64 * info.scale_y) as i32);
            part.advance = (lrint(s.x_advance as f64 * info.scale_x) as i32, lrint(-s.y_advance as f64 * info.scale_y) as i32);
            let adv = part.advance;
            info.skip = false;
            info.cluster_advance.0 += adv.0;
            info.cluster_advance.1 += adv.1;
        }
        i += 1;
    }
    // Faces for unshaped glyphs (skipped ligature components keep their
    // own glyph so their metrics still count, as in libass).
    for g in glyphs.iter_mut() {
        if g.drawing.is_none() && g.skip {
            if let Some(f) = g.font {
                let font = caches.fonts.font(f);
                g.parts[0].face = font.faces[g.face_index.min(font.faces.len() - 1)];
                g.parts[0].glyph = g.glyph_index;
            }
        }
    }
}

/// libass `get_outline_glyph` for every part + `retrieve_glyphs`.
pub fn retrieve_glyphs(glyphs: &mut [Glyph], st: &State<'_>, caches: &mut Caches) {
    for i in 0..glyphs.len() {
        let mut parts = std::mem::take(&mut glyphs[i].parts);
        let g = &glyphs[i];
        for part in parts.iter_mut() {
            outline_glyph(g, part, st, caches);
        }
        if g.drawing.is_some() {
            let adv = parts[0].advance;
            glyphs[i].cluster_advance = adv;
        }
        glyphs[i].parts = parts;

        // Add additional space after italic to non-italic style changes
        if i > 0 && glyphs[i - 1].italic != 0 && glyphs[i].italic == 0 {
            let mut back = i - 1;
            while back > 0 && glyphs[back].root().bbox.2 - glyphs[back].root().bbox.0 == 0 && glyphs[back].italic != 0 {
                back -= 1;
            }
            let og = &mut glyphs[back];
            if og.parts[0].bbox.2 > og.cluster_advance.0 {
                og.cluster_advance.0 = og.parts[0].bbox.2;
            }
        }
        glyphs[i].cluster_advance.0 += glyphs[i].hspacing_scaled;
    }
}

fn outline_glyph(g: &Glyph, part: &mut Part, st: &State<'_>, caches: &mut Caches) {
    let (asc, desc, offset);
    if let Some(d) = &g.drawing {
        let Some((outline, cbox)) = caches.drawing(&d.text) else { return };
        let base = (1u32 << ((d.scale - 1) & 31)) as i32;
        let w = if base > 0 { 1.0 / base as f64 } else { 0.0 };
        part.scale = (g.scale_x * w * st.screen_scale_x / st.frame.par, g.scale_y * w * st.screen_scale_y);
        let dsc = 64 * d.pbo;
        let a = (cbox.y_max - cbox.y_min) - dsc as f64;
        offset = (0.0, -a * part.scale.1);
        part.toffset = offset;
        part.outline = Some(outline);
        part.cbox = Some(cbox);
        part.advance = (lrint((cbox.x_max - cbox.x_min) * part.scale.0) as i32, 0);
        asc = a;
        desc = dsc as f64;
    } else {
        let Some(font) = g.font else { return };
        let face_id = part.face;
        let desc_ = &caches.fonts.font(font).desc;
        let (want_bold, want_italic) = (desc_.bold, desc_.italic);
        let face = caches.db.face(face_id).clone();
        let style = super::outline::GlyphStyle {
            italicize: !face.italic && want_italic > 55,
            embolden: !face.bold && want_bold > face.weight + 150,
            underline: g.flags & super::context::DECO_UNDERLINE != 0,
            strike_out: g.flags & super::context::DECO_STRIKETHROUGH != 0,
            rotate: g.flags & super::context::DECO_ROTATE != 0,
        };
        let Some(sized) = caches.glyph(face_id, part.glyph, g.font_size, style) else { return };
        part.scale = (g.scale_x, g.scale_y);
        part.toffset = (0.0, 0.0);
        part.cbox = sized.cbox;
        part.outline = Some(sized.outline.clone());
        let k = face.units_to_d6(g.font_size);
        asc = (face.ascender as f64 * k).round();
        desc = (-face.descender as f64 * k).round();
        offset = (0.0, 0.0);
    }
    let (sx, sy) = part.scale;
    part.bbox = match part.cbox {
        Some(c) => (
            lrint(c.x_min * sx + offset.0) as i32,
            lrint(c.y_min * sy + offset.1) as i32,
            lrint(c.x_max * sx + offset.0) as i32,
            lrint(c.y_max * sy + offset.1) as i32,
        ),
        None => (0, 0, 0, 0),
    };
    part.asc = lrint(asc * sy) as i32;
    part.desc = lrint(desc * sy) as i32;
}

/// libass `preliminary_layout`.
pub fn preliminary_layout(glyphs: &mut [Glyph]) {
    let mut pen = (0, 0);
    for g in glyphs.iter_mut() {
        let mut cp = pen;
        for p in g.parts.iter_mut() {
            p.pos = cp;
            cp.0 += p.advance.0;
            cp.1 += p.advance.1;
        }
        pen.0 += g.cluster_advance.0;
        pen.1 += g.cluster_advance.1;
    }
}

fn sym(glyphs: &[Glyph], i: usize) -> u32 {
    glyphs[i].symbol
}

/// libass `wrap_lines_naive`. Returns the line count.
fn wrap_naive(glyphs: &mut [Glyph], max_w: f64, wrap_style: i32) -> usize {
    let mut n_lines = 1;
    let mut s1 = 0usize;
    let mut last_breakable: isize = -1;
    let len = glyphs.len();
    for i in 0..len {
        let mut break_at: isize = -1;
        let mut break_type = 0u8;
        let s_offset = from_d6(glyphs[s1].root().bbox.0 + glyphs[s1].root().pos.0);
        let cur = &glyphs[i];
        let l = from_d6(cur.root().bbox.2 + cur.root().pos.0) - s_offset;
        if cur.symbol == '\n' as u32 {
            break_type = 2;
            break_at = i as isize;
        } else if l >= max_w && cur.symbol != ' ' as u32 && wrap_style != 2 {
            break_type = 1;
            break_at = last_breakable;
        }
        if cur.symbol == ' ' as u32 {
            last_breakable = i as isize;
        }
        if break_at != -1 && (break_at + 1) < len as isize {
            let lead = (break_at + 1) as usize;
            glyphs[lead].linebreak = break_type;
            last_breakable = -1;
            s1 = lead;
            n_lines += 1;
        }
    }
    n_lines
}

/// libass `rewind_trailing_spaces`.
fn rewind_trailing_spaces(glyphs: &[Glyph], start1: usize, start2: usize) -> usize {
    let mut g = start2;
    loop {
        g -= 1;
        if !(g > start1 && sym(glyphs, g) == ' ' as u32) {
            break;
        }
    }
    g
}

/// libass `wrap_lines_rebalance`.
fn wrap_rebalance(glyphs: &mut [Glyph], wrap_style: i32) {
    if wrap_style == 1 {
        return;
    }
    let len = glyphs.len();
    let x = |g: &Glyph, end: bool| -> i32 {
        let r = g.root();
        if end {
            r.bbox.2 + r.pos.0
        } else {
            r.bbox.0 + r.pos.0
        }
    };
    loop {
        let mut exit = true;
        let mut s1: Option<usize>;
        let mut s2: Option<usize> = None;
        let mut s3 = 0usize;
        let mut i = 0;
        while i <= len {
            if i == len || glyphs[i].linebreak != 0 {
                s1 = s2;
                s2 = Some(s3);
                s3 = i;
                if let (Some(a), Some(b)) = (s1, s2) {
                    if glyphs[b].linebreak == 1 {
                        let mut w = rewind_trailing_spaces(glyphs, a, b);
                        let e1_old = w;
                        while w > a && sym(glyphs, w) != ' ' as u32 {
                            w -= 1;
                        }
                        let mut e1 = w;
                        while e1 > a && sym(glyphs, e1) == ' ' as u32 {
                            e1 -= 1;
                        }
                        if sym(glyphs, w) == ' ' as u32 {
                            w += 1;
                        }
                        if w != a {
                            let e2 = rewind_trailing_spaces(glyphs, b, s3);
                            let l1 = from_d6(x(&glyphs[e1_old], true) - x(&glyphs[a], false));
                            let l2 = from_d6(x(&glyphs[e2], true) - x(&glyphs[b], false));
                            let l1n = from_d6(x(&glyphs[e1], true) - x(&glyphs[a], false));
                            let l2n = from_d6(x(&glyphs[e2], true) - x(&glyphs[w], false));
                            if (l1n - l2n).abs() < (l1 - l2).abs() {
                                glyphs[w].linebreak = 1;
                                glyphs[b].linebreak = 0;
                                s2 = Some(w);
                                exit = false;
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        if exit {
            break;
        }
    }
}

/// libass `trim_whitespace`.
fn trim_whitespace(glyphs: &mut [Glyph]) {
    let len = glyphs.len();
    if len == 0 {
        return;
    }
    let ws = |g: &Glyph| (g.symbol == ' ' as u32 || g.symbol == '\n' as u32) && g.linebreak == 0;
    let mut i = len - 1;
    while i > 0 && ws(&glyphs[i]) {
        glyphs[i].skip = true;
        glyphs[i].is_trimmed_whitespace = true;
        i -= 1;
    }
    let mut i = 0;
    while i < len && ws(&glyphs[i]) {
        glyphs[i].skip = true;
        glyphs[i].is_trimmed_whitespace = true;
        i += 1;
    }
    if i < len {
        glyphs[i].starts_new_run = true;
    }
    let mut i = 0;
    while i < len {
        if glyphs[i].linebreak != 0 {
            if i > 0 {
                let mut j = i - 1;
                while j > 0 && ws(&glyphs[j]) {
                    glyphs[j].skip = true;
                    glyphs[j].is_trimmed_whitespace = true;
                    j -= 1;
                }
            }
            let mut cur = i;
            if glyphs[i].symbol == ' ' as u32 || glyphs[i].symbol == '\n' as u32 {
                glyphs[i].skip = true;
                glyphs[i].is_trimmed_whitespace = true;
                let mut j = i + 1;
                while j < len && ws(&glyphs[j]) {
                    glyphs[j].skip = true;
                    glyphs[j].is_trimmed_whitespace = true;
                    j += 1;
                }
                cur = j;
                i = j - 1;
            }
            if cur < len {
                glyphs[cur].starts_new_run = true;
            }
        }
        i += 1;
    }
}

/// libass `measure_text` (+ `measure_text_on_eol`).
fn measure_text(text: &mut Text, n_lines: usize, st: &State<'_>) {
    text.height = 0.0;
    text.border_x = 0;
    text.lines = vec![Line::default(); n_lines.max(1)];
    let mut cur_line = 0;
    let mut scale = 0.5 / 64.0;
    let (mut max_asc, mut max_desc) = (0i32, 0i32);
    let (mut max_bx, mut max_by) = (0f64, 0f64);
    let mut empty = true;
    let eol = |text: &mut Text, line: usize, scale: f64, asc: i32, desc: i32, bx: f64, by: f64| {
        if line < text.lines.len() {
            text.lines[line] = Line { asc: scale * asc as f64, desc: scale * desc as f64 };
        }
        text.height += scale * asc as f64 + scale * desc as f64;
        // For *VSFilter compatibility do biased rounding on max_border*
        text.border_bottom = (st.border_scale_y * by + 0.5) as i32;
        if line == 0 {
            text.border_top = text.border_bottom;
        }
        text.border_x = text.border_x.max((st.border_scale_x * bx + 0.5) as i32);
    };
    for i in 0..text.glyphs.len() {
        if text.glyphs[i].linebreak != 0 {
            eol(text, cur_line, scale, max_asc, max_desc, max_bx, max_by);
            empty = true;
            max_asc = 0;
            max_desc = 0;
            max_bx = 0.0;
            max_by = 0.0;
            scale = 0.5 / 64.0;
            cur_line += 1;
        }
        let cur = &text.glyphs[i];
        if empty && !cur.is_trimmed_whitespace {
            empty = false;
            max_asc = 0;
            max_desc = 0;
            max_bx = 0.0;
            max_by = 0.0;
        } else if !empty && cur.is_trimmed_whitespace {
            continue;
        }
        max_asc = max_asc.max(cur.root().asc);
        max_desc = max_desc.max(cur.root().desc);
        max_by = max_by.max(cur.border_y);
        max_bx = max_bx.max(cur.border_x);
        if cur.symbol != '\n' as u32 {
            scale = 1.0 / 64.0;
        }
    }
    eol(text, cur_line, scale, max_asc, max_desc, max_bx, max_by);
}

/// libass `ass_process_karaoke_effects`.
fn process_karaoke(glyphs: &mut [Glyph], st: &State<'_>) {
    let tm_current = st.time - st.event.start_ms;
    let mut timing: i32 = 0;
    let mut skip_timing: i32 = 0;
    let mut effect_type = Effect::None;
    let mut last_boundary: Option<usize> = None;
    let mut has_reset = false;
    let len = glyphs.len();
    for i in 0..=len {
        if i < len && !glyphs[i].starts_new_run {
            if glyphs[i].reset_effect {
                has_reset = true;
                skip_timing = 0;
            }
            skip_timing = (skip_timing as u32).wrapping_add(glyphs[i].effect_skip_timing as u32) as i32;
            continue;
        }
        let start = last_boundary;
        let end = i;
        last_boundary = Some(end);
        let Some(start) = start else { continue };
        if glyphs[start].effect_type != Effect::None {
            effect_type = glyphs[start].effect_type;
        }
        if effect_type == Effect::None {
            continue;
        }
        if glyphs[start].reset_effect {
            timing = 0;
        }
        let tm_start = timing as i64 + glyphs[start].effect_skip_timing as i64;
        let mut tm_end = tm_start + glyphs[start].effect_timing as i64;
        timing = (if has_reset { 0 } else { tm_end }) as i32 + skip_timing;
        skip_timing = 0;
        has_reset = false;
        if effect_type != Effect::KaraokeKf {
            tm_end = tm_start;
        }
        let x: i32 = if tm_current < tm_start {
            -100_000_000
        } else if tm_current >= tm_end {
            100_000_000
        } else {
            let (mut first, mut last) = (start, end - 1);
            while first < last && glyphs[first].skip {
                first += 1;
            }
            while first < last && glyphs[last].skip {
                last -= 1;
            }
            let x_start = glyphs[first].root().pos.0;
            let x_end = glyphs[last].root().pos.0 + glyphs[last].root().advance.0;
            let mut dt = (tm_current - tm_start) as f64 / (tm_end - tm_start) as f64;
            let frz = glyphs[start].frz % 360.0;
            if frz > 90.0 && frz < 270.0 {
                // Fill from right to left
                dt = 1.0 - dt;
                for g in &mut glyphs[start..end] {
                    g.c.swap(0, 1);
                }
            }
            x_start + lrint((x_end - x_start) as f64 * dt) as i32
        };
        for g in &mut glyphs[start..end] {
            g.effect_type = effect_type;
            g.effect_timing = x - g.parts[0].pos.0;
        }
    }
}

/// libass `reorder_text`: pen positions in visual order.
fn reorder_text(text: &mut Text) {
    let mut pen = (0, 0);
    let mut lineno = 1;
    for i in 0..text.glyphs.len() {
        if text.glyphs[i].linebreak != 0 {
            pen.0 = 0;
            pen.1 += d6(text.lines.get(lineno - 1).map_or(0.0, |l| l.desc));
            pen.1 += d6(text.lines.get(lineno).map_or(0.0, |l| l.asc));
            lineno += 1;
        }
        let g = &mut text.glyphs[text.cmap[i]];
        if g.skip {
            continue;
        }
        let mut cp = pen;
        pen.0 += g.cluster_advance.0;
        pen.1 += g.cluster_advance.1;
        for p in g.parts.iter_mut() {
            p.pos = (p.offset.0 + cp.0, p.offset.1 + cp.1);
            cp.0 += p.advance.0;
            cp.1 += p.advance.1;
        }
    }
}

/// libass `align_lines`.
fn align_lines(glyphs: &mut [Glyph], st: &State<'_>, max_text_width: f64) {
    let mut halign = st.alignment & 3;
    let mut justify = st.justify;
    if st.evt_type & EVENT_HSCROLL != 0 {
        justify = halign;
        halign = HALIGN_LEFT;
    }
    let counts = |g: &Glyph| !g.skip && g.symbol != '\n' as u32 && g.symbol != 0;
    let len = glyphs.len();
    let mut max_width: f64 = 0.0;
    let mut width = 0.0;
    for i in 0..=len {
        if i == len || glyphs[i].linebreak != 0 {
            max_width = max_width.max(width);
            width = 0.0;
        }
        if i < len && counts(&glyphs[i]) {
            width += from_d6(glyphs[i].cluster_advance.0);
        }
    }
    let mut last_break: isize = -1;
    width = 0.0;
    for i in 0..=len {
        if i == len || glyphs[i].linebreak != 0 {
            let shift = match halign {
                HALIGN_LEFT => match justify {
                    3 => max_width - width,
                    2 => (max_width - width) / 2.0,
                    _ => 0.0,
                },
                HALIGN_RIGHT => match justify {
                    1 => max_text_width - max_width,
                    2 => max_text_width - max_width + (max_width - width) / 2.0,
                    _ => max_text_width - width,
                },
                HALIGN_CENTER => match justify {
                    1 => (max_text_width - max_width) / 2.0,
                    3 => (max_text_width - max_width) / 2.0 + max_width - width,
                    _ => (max_text_width - width) / 2.0,
                },
                _ => 0.0,
            };
            let s = d6(shift);
            for g in &mut glyphs[(last_break + 1) as usize..i] {
                for p in g.parts.iter_mut() {
                    p.pos.0 += s;
                }
            }
            last_break = i as isize - 1;
            width = 0.0;
        }
        if i < len && counts(&glyphs[i]) {
            width += from_d6(glyphs[i].cluster_advance.0);
        }
    }
}

/// libass `compute_string_bbox`.
fn compute_string_bbox(text: &Text) -> (f64, f64, f64, f64) {
    if text.glyphs.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let mut x_min: f64 = 32000.0;
    let mut x_max: f64 = -32000.0;
    let y_min = -text.lines[0].asc;
    let y_max = y_min + text.height;
    for g in &text.glyphs {
        if g.skip {
            continue;
        }
        let s = from_d6(g.root().pos.0);
        let e = s + from_d6(g.cluster_advance.0);
        x_min = x_min.min(s);
        x_max = x_max.max(e);
    }
    (x_min, y_min, x_max, y_max)
}

/// libass `apply_baseline_shear`.
fn apply_baseline_shear(glyphs: &mut [Glyph], cmap: &[usize]) {
    let mut shear: i32 = 0;
    for i in 0..glyphs.len() {
        if glyphs[i].linebreak != 0 || glyphs[i].starts_new_run {
            shear = 0;
        }
        let g = &mut glyphs[cmap[i]];
        if g.scale_x == 0.0 || g.scale_y == 0.0 {
            g.skip = true;
        }
        if g.skip {
            continue;
        }
        let fay = g.fay / g.scale_x * g.scale_y;
        for p in g.parts.iter_mut() {
            p.pos.1 = (p.pos.1 as f64 + shear as f64 + fay * p.offset.0 as f64) as i32;
            shear = (shear as f64 + fay * p.advance.0 as f64) as i32;
        }
    }
}

/// libass `get_base_point`.
pub fn base_point(bbox: (f64, f64, f64, f64), alignment: i32) -> (f64, f64) {
    let bx = match alignment & 3 {
        HALIGN_LEFT => bbox.0,
        HALIGN_RIGHT => bbox.2,
        _ => (bbox.2 + bbox.0) / 2.0,
    };
    let by = match alignment & 12 {
        VALIGN_TOP => bbox.1,
        VALIGN_CENTER => (bbox.3 + bbox.1) / 2.0,
        _ => bbox.3,
    };
    (bx, by)
}

/// Script → frame coordinates (libass `x2scr_pos` / `y2scr_pos`; the
/// frame is the picture, no margins).
pub fn x2scr(st: &State<'_>, x: f64) -> f64 {
    x * st.frame.width as f64 / st.frame.par / st.script.play_res_x as f64
}

pub fn x2scr_scaled(st: &State<'_>, x: f64) -> f64 {
    x * st.frame.width as f64 / st.script.play_res_x as f64
}

pub fn y2scr(st: &State<'_>, y: f64) -> f64 {
    y * st.frame.height as f64 / st.script.play_res_y as f64
}

/// Everything from `wrap_lines_smart` to the event's device position.
pub fn layout(mut glyphs: Vec<Glyph>, st: &mut State<'_>) -> Text {
    let event = st.event;
    let style = &st.script.styles[event.style];
    let margin = |e: i32, s: i32| if e != 0 { e } else { s };
    let ml = margin(event.margin_l, style.margin_l);
    let mr = margin(event.margin_r, style.margin_r);
    let mv = margin(event.margin_v, style.margin_v);
    let valign = st.alignment & 12;

    let max_text_width = x2scr(st, (st.script.play_res_x - mr) as f64) - x2scr(st, ml as f64);

    let n_lines = wrap_naive(&mut glyphs, max_text_width, st.wrap_style);
    wrap_rebalance(&mut glyphs, st.wrap_style);
    trim_whitespace(&mut glyphs);
    let mut text = Text {
        glyphs,
        lines: Vec::new(),
        height: 0.0,
        border_top: 0,
        border_bottom: 0,
        border_x: 0,
        bbox: (0.0, 0.0, 0.0, 0.0),
        device_x: 0.0,
        device_y: 0.0,
        cmap: Vec::new(),
    };
    measure_text(&mut text, n_lines, st);

    process_karaoke(&mut text.glyphs, st);
    text.cmap = super::bidi::visual_order(&text.glyphs, &text.glyphs.iter().map(|g| g.level).collect::<Vec<_>>());
    reorder_text(&mut text);
    align_lines(&mut text.glyphs, st, max_text_width);
    let bbox = compute_string_bbox(&text);
    apply_baseline_shear(&mut text.glyphs, &text.cmap);

    let mut device_x = 0.0;
    let mut device_y = 0.0;
    if st.evt_type & EVENT_POSITIONED != 0 {
        let (bx, by) = base_point(bbox, st.alignment);
        device_x = x2scr(st, st.pos_x) - bx;
        device_y = y2scr(st, st.pos_y) - by;
    }
    if st.evt_type & EVENT_HSCROLL != 0 {
        match st.scroll_direction {
            Scroll::RightToLeft => device_x = x2scr(st, st.script.play_res_x as f64 - st.scroll_shift),
            Scroll::LeftToRight => device_x = x2scr(st, st.scroll_shift) - (bbox.2 - bbox.0),
            _ => {}
        }
    } else if st.evt_type & EVENT_POSITIONED == 0 {
        device_x = x2scr(st, ml as f64);
    }
    if st.evt_type & EVENT_VSCROLL != 0 {
        match st.scroll_direction {
            Scroll::TopToBottom => device_y = y2scr(st, st.scroll_y0 as f64 + st.scroll_shift) - bbox.3,
            Scroll::BottomToTop => device_y = y2scr(st, st.scroll_y1 as f64 - st.scroll_shift) - bbox.1,
            _ => {}
        }
    } else if st.evt_type & EVENT_POSITIONED == 0 {
        if valign == VALIGN_TOP {
            device_y = y2scr(st, mv as f64) + text.lines[0].asc;
        } else if valign == VALIGN_CENTER {
            let scr_y = y2scr(st, st.script.play_res_y as f64 / 2.0);
            device_y = scr_y - (bbox.3 + bbox.1) / 2.0;
        } else {
            let _ = VALIGN_SUB;
            let scr_bottom = y2scr(st, (st.script.play_res_y - mv) as f64);
            device_y = scr_bottom - text.height + text.lines[0].asc;
        }
    }

    // fix clip coordinates (no margins: always the frame-relative mapping)
    st.clip_x0 = x2scr_scaled(st, st.clip_x0 as f64).round() as i32;
    st.clip_x1 = x2scr_scaled(st, st.clip_x1 as f64).round() as i32;
    st.clip_y0 = y2scr(st, st.clip_y0 as f64).round() as i32;
    st.clip_y1 = y2scr(st, st.clip_y1 as f64).round() as i32;
    if st.explicit {
        st.clip_x0 = st.clip_x0.max(0);
        st.clip_y0 = st.clip_y0.max(0);
        st.clip_x1 = st.clip_x1.min(st.frame.width);
        st.clip_y1 = st.clip_y1.min(st.frame.height);
    }
    if st.evt_type & EVENT_VSCROLL != 0 {
        let y0 = y2scr(st, st.scroll_y0 as f64).round() as i32;
        let y1 = y2scr(st, st.scroll_y1 as f64).round() as i32;
        st.clip_y0 = st.clip_y0.max(y0);
        st.clip_y1 = st.clip_y1.min(y1);
    }

    // calculate_rotation_params
    let center = if st.have_origin {
        (x2scr(st, st.org_x), y2scr(st, st.org_y))
    } else {
        let (bx, by) = base_point(bbox, st.alignment);
        (device_x + bx, device_y + by)
    };
    for g in text.glyphs.iter_mut() {
        let (sx, sy) = (g.shadow_x, g.shadow_y);
        for p in g.parts.iter_mut() {
            p.shift.0 = p.pos.0 + d6(device_x - center.0 + sx * st.border_scale_x / st.frame.par);
            p.shift.1 = p.pos.1 + d6(device_y - center.1 + sy * st.border_scale_y);
        }
    }

    text.bbox = bbox;
    text.device_x = device_x;
    text.device_y = device_y;
    text
}
