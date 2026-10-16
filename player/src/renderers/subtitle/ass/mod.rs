//! ASS/SSA renderer: a port of libass's rendering in pure Rust.
//!
//! [`AssRenderer::render`] is `ass_render_frame`: every event active at the
//! frame time is laid out and rasterized (`layout`, `combine`), events are
//! ordered by layer and file order, colliding unpositioned events are
//! stacked the way VSFilter does (an event keeps the place it got when it
//! first appeared), and the result is the image list libass would return.
//! [`composite`] flattens that into one premultiplied RGBA bitmap, which is
//! what the subtitle overlay uploads.
//!
//! Text shaping is harfrust, outlines come from skrifa, rasterization is
//! zeno. Fonts: the built-in faces, the script's `[Fonts]`, and whatever
//! the host adds ([`AssRenderer::add_font`]).
//!
//! libass is ISC-licensed: Copyright (C) 2006-2016 libass contributors.
//! Permission to use, copy, modify, and/or distribute this software for any
//! purpose with or without fee is hereby granted, provided that the above
//! copyright notice and this permission notice appear in all copies.

mod bidi;
mod bitmap;
mod combine;
mod context;
pub mod fonts;
mod layout;
mod outline;
mod shape;

use std::collections::HashMap;
use std::sync::Arc;

use crate::parsers::ass::{AssScript, VALIGN_SUB};

pub use combine::Image;
pub use context::Frame;
use fonts::{Face, FaceSource, FontBytes, FontCache, FontDb};
use outline::{GlyphStyle, Outline, Rect, UnitGlyph};

/// A glyph outline at a size with its synthetic styles.
#[derive(Clone)]
pub struct SizedGlyph {
    pub outline: Arc<Outline>,
    pub cbox: Option<Rect>,
}

/// Fonts and the caches outlines are looked up in.
pub struct Caches {
    pub db: FontDb,
    pub fonts: FontCache,
    units: HashMap<(usize, u32), Option<Arc<UnitGlyph>>>,
    sized: HashMap<(usize, u32, u64, u8), Option<SizedGlyph>>,
    drawings: HashMap<String, Option<(Arc<Outline>, Rect)>>,
}

impl Caches {
    fn new(db: FontDb) -> Self {
        Self { db, fonts: FontCache::default(), units: HashMap::new(), sized: HashMap::new(), drawings: HashMap::new() }
    }

    pub fn glyph(&mut self, face: usize, glyph: u32, size: f64, style: GlyphStyle) -> Option<SizedGlyph> {
        let bits = style.italicize as u8
            | (style.embolden as u8) << 1
            | (style.underline as u8) << 2
            | (style.strike_out as u8) << 3
            | (style.rotate as u8) << 4;
        let key = (face, glyph, size.to_bits(), bits);
        if let Some(v) = self.sized.get(&key) {
            return v.clone();
        }
        let f = self.db.face(face).clone();
        let unit = self
            .units
            .entry((face, glyph))
            .or_insert_with(|| outline::load_unit_glyph(&f, glyph).map(Arc::new))
            .clone();
        let v = unit.map(|u| {
            let (o, _advance) = outline::glyph_outline(&f, &u, glyph, size, style);
            SizedGlyph { cbox: o.cbox(), outline: Arc::new(o) }
        });
        if self.sized.len() > 20_000 {
            self.sized.clear();
        }
        self.sized.insert(key, v.clone());
        v
    }

    pub fn drawing(&mut self, text: &str) -> Option<(Arc<Outline>, Rect)> {
        if let Some(v) = self.drawings.get(text) {
            return v.clone();
        }
        let v = outline::parse_drawing(text).map(|(o, r)| (Arc::new(o), r));
        if self.drawings.len() > 2_000 {
            self.drawings.clear();
        }
        self.drawings.insert(text.to_string(), v.clone());
        v
    }
}

/// The frame for `script` on a `w`×`h` picture of a video `storage` pixels
/// big. Pixel aspect follows libass (`ass_start_frame`): with
/// `LayoutResX/Y` or a storage size, glyphs are corrected so a script laid
/// out for one aspect keeps its letter shapes on another.
pub fn frame_for(script: &AssScript, w: u32, h: u32, storage: Option<(u32, u32)>, font_size_coeff: f64) -> Frame {
    let storage = storage.filter(|s| s.0 > 0 && s.1 > 0).map(|s| (s.0 as i32, s.1 as i32));
    let mut frame = Frame { width: w as i32, height: h as i32, par: 1.0, font_size_coeff, storage };
    let lr = script.layout_res_x > 0 && script.layout_res_y > 0;
    if (lr || storage.is_some()) && w > 0 && h > 0 {
        let (lx, ly) = context::layout_res(script, &frame);
        frame.par = (w as f64 / h as f64) / (lx as f64 / ly as f64);
    }
    frame
}

/// One rendered event (libass `EventImages`).
struct EventImages {
    imgs: Vec<Image>,
    top: i32,
    height: i32,
    left: i32,
    width: i32,
    detect_collisions: bool,
    shift_direction: i32,
    layer: i32,
    read_order: usize,
    event: usize,
}

/// Where a collision-detected event was placed (libass `RenderPriv`).
#[derive(Clone, Copy, Default)]
struct Fixed {
    top: i32,
    height: i32,
    left: i32,
    width: i32,
}

pub struct AssRenderer {
    /// Built-in and host faces; the script's embedded fonts are added on
    /// top per script.
    base: Vec<Arc<Face>>,
    caches: Caches,
    /// Identity (address) of the script `caches.db` was built for.
    script: Option<usize>,
    fixed: HashMap<usize, Fixed>,
    fixed_frame: (i32, i32),
    /// Hashes of the font files added, so the same file added twice (a
    /// manifest font on every track switch) is parsed once.
    added: std::collections::HashSet<u64>,
}

impl Default for AssRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl AssRenderer {
    pub fn new() -> Self {
        let base: Vec<Arc<Face>> = fonts::builtin_faces().into_iter().map(Arc::new).collect();
        let mut db = FontDb::default();
        db.add_shared(&base);
        Self {
            base,
            caches: Caches::new(db),
            script: None,
            fixed: HashMap::new(),
            fixed_frame: (0, 0),
            added: std::collections::HashSet::new(),
        }
    }

    /// Add a font file (TTF/OTF/TTC) the host has: a fonts folder, a
    /// download named by the manifest, MKV attachments. Returns how many
    /// faces it held. Host fonts rank above the built-in fallback faces.
    pub fn add_font(&mut self, bytes: Vec<u8>) -> usize {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        if !self.added.insert(h.finish()) {
            return 0;
        }
        let faces: Vec<Arc<Face>> = fonts::faces_from_bytes(FontBytes::Shared(Arc::new(bytes)), FaceSource::Host)
            .into_iter()
            .map(Arc::new)
            .collect();
        let n = faces.len();
        for f in &faces {
            log::info!("[subs/ass] font added: {:?} ({})", f.families, f.name());
        }
        // Host faces go before the built-ins so a family that exists in
        // both resolves to the host's.
        let builtin: Vec<Arc<Face>> = self.base.drain(..).collect();
        self.base.extend(faces);
        self.base.extend(builtin);
        self.script = None;
        n
    }


    fn use_script(&mut self, script: &Arc<AssScript>) {
        let ptr = Arc::as_ptr(script) as usize;
        if self.script == Some(ptr) {
            return;
        }
        let mut db = FontDb::default();
        // Embedded fonts first: the script ships them for its own names.
        for f in &script.fonts {
            db.add(fonts::faces_from_bytes(FontBytes::Shared(Arc::new(f.data.clone())), FaceSource::Embedded));
        }
        db.add_shared(&self.base);
        self.caches = Caches::new(db);
        self.script = Some(ptr);
        self.fixed.clear();
    }

    /// Forget where colliding events were placed (after a seek, a size
    /// change): they get placed afresh.
    pub fn reset_layout(&mut self) {
        self.fixed.clear();
    }

    /// Render the events `active` (indices into `script.events`) at `time_ms`
    /// on a `frame` (libass `ass_render_frame`). Images come back in paint
    /// order, in frame pixels.
    pub fn render(&mut self, script: &Arc<AssScript>, active: &[usize], time_ms: i64, frame: Frame) -> Vec<Image> {
        self.use_script(script);
        if self.fixed_frame != (frame.width, frame.height) {
            self.fixed.clear();
            self.fixed_frame = (frame.width, frame.height);
        }
        let mut events: Vec<EventImages> = active
            .iter()
            .filter_map(|&i| {
                let e = script.events.get(i)?;
                (e.start_ms <= time_ms && time_ms < e.end_ms()).then_some(i)
            })
            .filter_map(|i| self.render_event(script, i, time_ms, frame))
            .collect();
        events.sort_by_key(|e| (e.layer, e.read_order));
        let mut start = 0;
        while start < events.len() {
            let layer = events[start].layer;
            let end = events[start..].iter().position(|e| e.layer != layer).map_or(events.len(), |n| start + n);
            self.fix_collisions(&mut events[start..end], frame);
            start = end;
        }
        let mut out = Vec::new();
        for e in events {
            out.extend(e.imgs.into_iter().filter(|i| i.color & 0xFF != 0xFF && i.w > 0 && i.h > 0));
        }
        out
    }

    /// libass `ass_render_event`.
    fn render_event(&mut self, script: &AssScript, index: usize, time: i64, frame: Frame) -> Option<EventImages> {
        let event = &script.events[index];
        let caches = &mut self.caches;
        let mut st = context::State::new(script, event, time, frame, &caches.db, &mut caches.fonts);
        let mut glyphs = layout::parse_events(&mut st, caches);
        if glyphs.is_empty() {
            return None;
        }
        layout::split_style_runs(&mut glyphs, caches);
        layout::shape(&mut glyphs, &st, caches);
        layout::retrieve_glyphs(&mut glyphs, &st, caches);
        layout::preliminary_layout(&mut glyphs);
        let valign = st.alignment & 12;
        let mut text = layout::layout(glyphs, &mut st);
        let mut imgs = combine::render(&mut text, &st, caches);

        let par = frame.par;
        let top = (text.device_y - text.lines[0].asc - text.border_top as f64) as i32;
        let height = (text.height + text.border_bottom as f64 + text.border_top as f64) as i32;
        let left = ((text.device_x + text.bbox.0) * par - text.border_x as f64 + 0.5) as i32;
        let width = ((text.bbox.2 - text.bbox.0) * par + 2.0 * text.border_x as f64 + 0.5) as i32;
        if st.border_style == 4 {
            if let Some(bg) = combine::background(&st, left, top, width, height) {
                imgs.insert(0, bg);
            }
        }
        Some(EventImages {
            imgs,
            top,
            height,
            left,
            width,
            detect_collisions: st.detect_collisions,
            shift_direction: if valign == VALIGN_SUB { -1 } else { 1 },
            layer: event.layer,
            read_order: event.read_order,
            event: index,
        })
    }

    /// libass `fix_collisions`.
    fn fix_collisions(&mut self, imgs: &mut [EventImages], frame: Frame) {
        let mut used: Vec<(i32, i32, i32, i32)> = Vec::new(); // y0, y1, x0, x1
        let overlap = |a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)| !(a.0 >= b.1 || b.0 >= a.1 || a.2 >= b.3 || b.2 >= a.3);
        // fill used[] with fixed events
        for ei in imgs.iter_mut() {
            if !ei.detect_collisions || ei.height == 0 || ei.width == 0 {
                continue;
            }
            let priv_ = self.fixed.entry(ei.event).or_default();
            if priv_.height > 0 {
                let s = (priv_.top, priv_.top + priv_.height, priv_.left, priv_.left + priv_.width);
                if priv_.height != ei.height {
                    *priv_ = Fixed::default();
                }
                if used.iter().any(|u| overlap(s, *u)) {
                    *priv_ = Fixed::default();
                }
                if priv_.height > 0 {
                    used.push((priv_.top, priv_.top + priv_.height, priv_.left, priv_.left + priv_.width));
                    let shift = priv_.top - ei.top;
                    shift_event(ei, shift, frame.height);
                }
            }
        }
        used.sort_by_key(|u| u.0);
        // try to fit other events in free spaces
        for ei in imgs.iter_mut() {
            if !ei.detect_collisions || ei.height == 0 || ei.width == 0 {
                continue;
            }
            let priv_ = self.fixed.entry(ei.event).or_default();
            if priv_.height == 0 {
                let s = (ei.top, ei.top + ei.height, ei.left, ei.left + ei.width);
                let shift = fit_rect(s, &mut used, ei.shift_direction);
                if shift != 0 {
                    shift_event(ei, shift, frame.height);
                }
                *priv_ = Fixed { top: ei.top, height: ei.height, left: ei.left, width: ei.width };
            }
        }
    }
}

/// libass `fit_rect`.
fn fit_rect(s: (i32, i32, i32, i32), fixed: &mut Vec<(i32, i32, i32, i32)>, dir: i32) -> i32 {
    let mut shift = 0;
    let hits = |f: &(i32, i32, i32, i32), shift: i32| !(s.1 + shift <= f.0 || s.0 + shift >= f.1 || s.3 <= f.2 || s.2 >= f.3);
    if dir == 1 {
        for f in fixed.iter() {
            if hits(f, shift) {
                shift = f.1 - s.0;
            }
        }
    } else {
        for f in fixed.iter().rev() {
            if hits(f, shift) {
                shift = f.0 - s.1;
            }
        }
    }
    fixed.push((s.0 + shift, s.1 + shift, s.2, s.3));
    fixed.sort_by_key(|u| u.0);
    shift
}

/// libass `shift_event`: move an event vertically, clipping its images to
/// the frame.
fn shift_event(ei: &mut EventImages, shift: i32, frame_h: i32) {
    for img in ei.imgs.iter_mut() {
        img.dst_y += shift;
        if img.dst_y < 0 {
            let clip = -img.dst_y;
            img.h -= clip;
            img.sy += clip;
            img.dst_y = 0;
        }
        if img.dst_y + img.h >= frame_h {
            img.h -= img.dst_y + img.h - frame_h;
        }
        if img.h <= 0 {
            img.h = 0;
            img.dst_y = 0;
        }
    }
    ei.top += shift;
}

/// A flattened frame: premultiplied RGBA covering `width`×`height` frame
/// pixels from `(left, top)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Composite {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Blend `images` in order (each: colour × coverage × (255 − alpha)) into
/// one premultiplied RGBA bitmap over their bounding box, clipped to the
/// frame. `None` when nothing is visible.
pub fn composite(images: &[Image], frame_w: i32, frame_h: i32) -> Option<Composite> {
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    for img in images {
        let (l, t) = (img.dst_x.max(0), img.dst_y.max(0));
        let (r, b) = ((img.dst_x + img.w).min(frame_w), (img.dst_y + img.h).min(frame_h));
        if r <= l || b <= t {
            continue;
        }
        bounds = Some(match bounds {
            None => (l, t, r, b),
            Some((a, bb, c, d)) => (a.min(l), bb.min(t), c.max(r), d.max(b)),
        });
    }
    let (l, t, r, b) = bounds?;
    let (w, h) = ((r - l) as u32, (b - t) as u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for img in images {
        let ca = 255 - (img.color & 0xFF);
        if ca == 0 {
            continue;
        }
        let (cr, cg, cb) = ((img.color >> 24) & 0xFF, (img.color >> 16) & 0xFF, (img.color >> 8) & 0xFF);
        for y in 0..img.h {
            let fy = img.dst_y + y;
            if fy < t || fy >= b {
                continue;
            }
            for x in 0..img.w {
                let fx = img.dst_x + x;
                if fx < l || fx >= r {
                    continue;
                }
                let m = img.at(x, y) as u32;
                if m == 0 {
                    continue;
                }
                // source alpha 0..=255*255, rounded to 0..=255
                let a = (m * ca + 127) / 255;
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                let i = (((fy - t) as u32 * w + (fx - l) as u32) * 4) as usize;
                let over = |d: u8, s: u32| ((s * a + d as u32 * inv + 127) / 255) as u8;
                rgba[i] = over(rgba[i], cr);
                rgba[i + 1] = over(rgba[i + 1], cg);
                rgba[i + 2] = over(rgba[i + 2], cb);
                rgba[i + 3] = ((a * 255 + rgba[i + 3] as u32 * inv + 127) / 255) as u8;
            }
        }
    }
    Some(Composite { left: l, top: t, width: w, height: h, rgba })
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
