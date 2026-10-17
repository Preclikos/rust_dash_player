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
use bitmap::Bitmap;
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
    /// Border outlines by (glyph outline, stroker radii); the value keeps
    /// the source outline alive so its address stays unique.
    strokes: HashMap<StrokeKey, (Arc<Outline>, Arc<Outline>)>,
    /// Rasterized glyphs (libass bitmap cache), see [`BitmapKey`].
    bitmaps: HashMap<BitmapKey, CachedBitmap>,
    bitmap_bytes: usize,
    bitmap_budget: usize,
}

/// A glyph bitmap under an affine transform: the outline (and, for a
/// border, its stroke) by address, the linear part of the matrix, and the
/// translation's fraction of a pixel in 1/1024 px. The bitmap is stored
/// at the integer pixel origin and moved to wherever the glyph lands, so a
/// glyph repeated across events, lines or `\move` frames rasterizes once
/// per subpixel phase.
pub type BitmapKey = (usize, usize, [u64; 4], i32, i32);

/// A glyph outline (by address) and the stroker radii' bits.
type StrokeKey = (usize, u64, u64);

/// A cached glyph bitmap, with the outline (and border stroke) its key
/// names by address, kept alive so the addresses stay unique.
type CachedBitmap = (Arc<Outline>, Option<Arc<Outline>>, Option<Arc<Bitmap>>);

/// Bitmap cache budget (all threads together); a cache starts over when
/// it is exceeded.
const BITMAP_CACHE_BYTES: usize = 48 << 20;

/// A frame with fewer events renders on one thread (spawning costs more
/// than it saves).
const PARALLEL_MIN_EVENTS: usize = 8;

/// Lower the calling thread's scheduling priority (nice 10, Android):
/// subtitle rendering must yield to decoding, presenting and audio — on a
/// weak 4-core TV stick a busy typeset frame otherwise costs video frames.
/// The overlay worker and every render lane call it.
pub fn background_priority() {
    #[cfg(target_os = "android")]
    // SAFETY: plain syscall; `who` 0 is the calling thread on Linux.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// Likewise for flattening a frame smaller than this.
const PARALLEL_MIN_PIXELS: usize = 256 * 1024;

/// Threads a busy frame renders on besides the overlay worker: all cores
/// but one (video, audio and the network keep theirs), at most 3.
fn extra_lanes() -> usize {
    #[cfg(target_arch = "wasm32")]
    return 0;
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(2).min(3)
}

impl Caches {
    fn new(db: FontDb, bitmap_budget: usize) -> Self {
        Self {
            db,
            fonts: FontCache::default(),
            units: HashMap::new(),
            sized: HashMap::new(),
            drawings: HashMap::new(),
            strokes: HashMap::new(),
            bitmaps: HashMap::new(),
            bitmap_bytes: 0,
            bitmap_budget,
        }
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

    /// `outline` stroked with radii `rx`/`ry` (glyph space, 26.6).
    pub fn stroke(&mut self, outline: &Arc<Outline>, rx: f64, ry: f64) -> Arc<Outline> {
        let key = (Arc::as_ptr(outline) as usize, rx.to_bits(), ry.to_bits());
        if let Some((_, s)) = self.strokes.get(&key) {
            return s.clone();
        }
        let s = Arc::new(outline::stroke(outline, rx, ry));
        if self.strokes.len() > 20_000 {
            self.strokes.clear();
        }
        self.strokes.insert(key, (outline.clone(), s.clone()));
        s
    }

    pub fn bitmap(&mut self, key: &BitmapKey) -> Option<Option<Arc<Bitmap>>> {
        self.bitmaps.get(key).map(|v| v.2.clone())
    }

    pub fn put_bitmap(&mut self, key: BitmapKey, outline: &Arc<Outline>, stroked: Option<&Arc<Outline>>, bm: Option<Arc<Bitmap>>) {
        let bytes = bm.as_ref().map_or(0, |b| b.data.len()) + 64;
        if self.bitmap_bytes + bytes > self.bitmap_budget {
            self.bitmaps.clear();
            self.bitmap_bytes = 0;
        }
        self.bitmap_bytes += bytes;
        self.bitmaps.insert(key, (outline.clone(), stroked.cloned(), bm));
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

thread_local! {
    /// `ASS_PROFILE=1`: per-event phase timings on stderr (benchmarks).
    static PROFILE: bool = std::env::var("ASS_PROFILE").is_ok();
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
    /// One cache set per extra thread a busy frame renders on (see
    /// `render`); built with `caches`.
    lanes: Vec<Caches>,
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
            caches: Caches::new(db, BITMAP_CACHE_BYTES),
            lanes: Vec::new(),
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
        // The bitmap budget is shared out between the threads.
        let n = 1 + extra_lanes();
        self.lanes = (1..n).map(|_| Caches::new(db.clone(), BITMAP_CACHE_BYTES / n)).collect();
        self.caches = Caches::new(db, BITMAP_CACHE_BYTES / n);
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
        let todo: Vec<usize> = active
            .iter()
            .copied()
            .filter(|&i| script.events.get(i).is_some_and(|e| e.start_ms <= time_ms && time_ms < e.end_ms()))
            .collect();
        let mut events = self.render_events(script, &todo, time_ms, frame);
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

    /// The events `todo` laid out and rasterized, in `todo` order. Events
    /// are independent until collisions are fixed, so a busy frame (a
    /// typeset sign is often hundreds of one-glyph events) is shared out
    /// between threads, each with its own caches; a quiet one stays on
    /// this thread.
    fn render_events(&mut self, script: &AssScript, todo: &[usize], time: i64, frame: Frame) -> Vec<EventImages> {
        if self.lanes.is_empty() || todo.len() < PARALLEL_MIN_EVENTS {
            return todo.iter().filter_map(|&i| render_event(&mut self.caches, script, i, time, frame)).collect();
        }
        #[cfg(target_arch = "wasm32")]
        unreachable!("no extra lanes on wasm");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let work = |caches: &mut Caches| {
                let mut out = Vec::new();
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&i) = todo.get(k) else { break };
                    if let Some(e) = render_event(caches, script, i, time, frame) {
                        out.push((k, e));
                    }
                }
                out
            };
            let mut done: Vec<(usize, EventImages)> = std::thread::scope(|s| {
                let helpers: Vec<_> = self
                    .lanes
                    .iter_mut()
                    .map(|c| {
                        s.spawn(|| {
                            background_priority();
                            work(c)
                        })
                    })
                    .collect();
                let mut all = work(&mut self.caches);
                for h in helpers {
                    all.extend(h.join().expect("ASS render lane panicked"));
                }
                all
            });
            done.sort_unstable_by_key(|(k, _)| *k);
            done.into_iter().map(|(_, e)| e).collect()
        }
    }
}

/// libass `ass_render_event`.
fn render_event(caches: &mut Caches, script: &AssScript, index: usize, time: i64, frame: Frame) -> Option<EventImages> {
    let event = &script.events[index];
    let prof = PROFILE.with(|p| *p);
    let t0 = prof.then(crate::rt::Instant::now);
    let mark = |label: &str, t: &mut Option<crate::rt::Instant>| {
        if let Some(t) = t {
            eprintln!("  [ass prof] {label}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
            *t = crate::rt::Instant::now();
        }
    };
    let mut t = t0;
    let mut st = context::State::new(script, event, time, frame, &caches.db, &mut caches.fonts);
    let mut glyphs = layout::parse_events(&mut st, caches);
    mark("parse", &mut t);
    if glyphs.is_empty() {
        return None;
    }
    layout::split_style_runs(&mut glyphs, caches);
    layout::shape(&mut glyphs, &st, caches);
    mark("shape", &mut t);
    layout::retrieve_glyphs(&mut glyphs, &st, caches);
    mark("outlines", &mut t);
    layout::preliminary_layout(&mut glyphs);
    let valign = st.alignment & 12;
    let mut text = layout::layout(glyphs, &mut st);
    mark("layout", &mut t);
    let mut imgs = combine::render(&mut text, &st, caches);
    mark("raster", &mut t);

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

impl AssRenderer {
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

/// `v / 255` rounded down, exact for `v` < 65536.
#[inline]
fn div255(v: u32) -> u32 {
    (v + 1 + (v >> 8)) >> 8
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
    // Rows are independent: a large frame is split into bands, one per
    // render lane.
    let lanes = 1 + extra_lanes();
    let band = if lanes > 1 && w as usize * h as usize >= PARALLEL_MIN_PIXELS { (h as usize).div_ceil(lanes) } else { h as usize };
    let blend_band = |top: i32, out: &mut [u8]| blend_rows(images, (l, r), top, top + (out.len() / (w as usize * 4)) as i32, w as usize, out);
    if band >= h as usize {
        blend_band(t, &mut rgba);
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::scope(|s| {
            for (i, chunk) in rgba.chunks_mut(band * w as usize * 4).enumerate() {
                s.spawn(move || {
                    background_priority();
                    blend_band(t + (i * band) as i32, chunk)
                });
            }
        });
    }
    Some(Composite { left: l, top: t, width: w, height: h, rgba })
}

/// Blend `images` into the frame rows `top..bottom` (frame coordinates),
/// columns `cols`; `out` holds those rows, `w` pixels each.
fn blend_rows(images: &[Image], cols: (i32, i32), top: i32, bottom: i32, w: usize, out: &mut [u8]) {
    let (l, r) = cols;
    for img in images {
        let ca = 255 - (img.color & 0xFF);
        if ca == 0 {
            continue;
        }
        let (cr, cg, cb) = ((img.color >> 24) & 0xFF, (img.color >> 16) & 0xFF, (img.color >> 8) & 0xFF);
        // the image's part inside the band, in image coordinates
        let (x0, x1) = ((l - img.dst_x).max(0), (r - img.dst_x).min(img.w));
        let (y0, y1) = ((top - img.dst_y).max(0), (bottom - img.dst_y).min(img.h));
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let bw = img.bitmap.w as usize;
        for y in y0..y1 {
            let src_row = (img.sy + y) as usize * bw + (img.sx + x0) as usize;
            let src = &img.bitmap.data[src_row..src_row + (x1 - x0) as usize];
            let dst_row = ((img.dst_y + y - top) as usize * w + (img.dst_x + x0 - l) as usize) * 4;
            let dst = &mut out[dst_row..dst_row + src.len() * 4];
            for (&m, d) in src.iter().zip(dst.as_chunks_mut::<4>().0) {
                if m == 0 {
                    continue;
                }
                // source alpha 0..=255*255, rounded to 0..=255
                let a = div255(m as u32 * ca + 127);
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                d[0] = div255(cr * a + d[0] as u32 * inv + 127) as u8;
                d[1] = div255(cg * a + d[1] as u32 * inv + 127) as u8;
                d[2] = div255(cb * a + d[2] as u32 * inv + 127) as u8;
                d[3] = div255(a * 255 + d[3] as u32 * inv + 127) as u8;
            }
        }
    }
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
