//! Glyphs → bitmaps → images: libass ass_render.c
//! `render_and_combine_glyphs`, `get_bitmap_glyph`,
//! `calc_transform_matrix`, `ass_composite_construct`, `render_text`,
//! `render_glyph(_i)` and `blend_vector_clip`.
//!
//! Glyphs of one style run are rendered into one fill, one border and one
//! shadow bitmap; blur, `\be`, the border/fill interplay and the shadow
//! offset are applied per run exactly in libass's order. Each bitmap then
//! becomes images (clipped by `\clip`/`\iclip`, split at the `\kf` sweep)
//! in libass's paint order: all shadows, all borders, all fills.

use std::sync::Arc;

use super::bitmap::{self, Bitmap};
use super::context::{apply_fade, d6, from_d6, lrint, Effect, State};
use super::layout::{Glyph, Part, Text};
use super::outline::{self, Outline, P};
use super::Caches;

const F_BORDER_STYLE_3: u32 = 1;
const F_NONZERO_BORDER: u32 = 2;
const F_NONZERO_SHADOW: u32 = 4;
const F_FILL_IN_SHADOW: u32 = 8;
const F_FILL_IN_BORDER: u32 = 16;

const BLUR_PRECISION: f64 = 1.0 / 256.0;
const POSITION_PRECISION: f64 = 8.0;

/// One coloured mask on the frame: `w`×`h` pixels of `bitmap` starting at
/// `(sx, sy)`, drawn at `(dst_x, dst_y)` in `color` (libass RGBA, alpha =
/// transparency). libass `ASS_Image`.
#[derive(Clone, Debug)]
pub struct Image {
    pub bitmap: Arc<Bitmap>,
    pub sx: i32,
    pub sy: i32,
    pub w: i32,
    pub h: i32,
    pub dst_x: i32,
    pub dst_y: i32,
    pub color: u32,
}

impl Image {
    fn whole(bm: &Arc<Bitmap>, color: u32) -> Self {
        Image { bitmap: bm.clone(), sx: 0, sy: 0, w: bm.w, h: bm.h, dst_x: bm.left, dst_y: bm.top, color }
    }

    #[inline]
    pub fn at(&self, x: i32, y: i32) -> u8 {
        self.bitmap.data[((self.sy + y) * self.bitmap.w + self.sx + x) as usize]
    }
}

/// libass `CombinedBitmapInfo`.
struct Combined {
    flags: u32,
    be: i32,
    blur_x: i32,
    blur_y: i32,
    shadow: (i32, i32),
    c: [u32; 4],
    effect_type: Effect,
    effect_timing: i32,
    leftmost_x: f64,
    /// How far (px) blur, `\be` and the shadow spread a glyph's bitmaps;
    /// `None` = never cull (opaque boxes).
    spread: Option<(f64, f64)>,
    /// Subpixel residual of the run's first glyph (see `snap`).
    residual: Option<(f64, f64)>,
    glyphs: Vec<Bitmap>,
    borders: Vec<Bitmap>,
    bm: Option<Arc<Bitmap>>,
    bm_o: Option<Arc<Bitmap>>,
    bm_s: Option<Arc<Bitmap>>,
}

/// C `frexp` exponent.
fn frexp_exp(x: f64) -> i32 {
    if x == 0.0 || !x.is_finite() {
        return 0;
    }
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0 {
        // subnormal
        return frexp_exp(x * 2f64.powi(54)) - 54;
    }
    e - 1022
}

/// libass `quantize_blur`: quantized radius and shadow offset mask.
fn quantize_blur(radius: f64) -> (i32, i32) {
    let scale = 64.0 * BLUR_PRECISION / POSITION_PRECISION;
    let radius = radius * scale;
    let ord = frexp_exp((1.0 + radius) * (POSITION_PRECISION / 2.0));
    let mask = ((1u32 << ord.clamp(0, 31)) - 1) as i32;
    (lrint(radius.ln_1p() / BLUR_PRECISION) as i32, mask)
}

/// libass `restore_blur`: variance (px²) of a quantized radius.
fn restore_blur(q: i32) -> f64 {
    let scale = 64.0 * BLUR_PRECISION / POSITION_PRECISION;
    let sigma = (BLUR_PRECISION * q as f64).exp_m1() / scale;
    sigma * sigma
}

/// libass `calc_transform_matrix`: glyph layout space → frame (26.6),
/// rotation about the event's origin, shear, perspective.
fn transform_matrix(st: &State<'_>, g: &Glyph, p: &Part) -> [[f64; 3]; 3] {
    let deg = std::f64::consts::PI / 180.0;
    let (frx, fry, frz) = (g.frx * deg, g.fry * deg, g.frz * deg);
    let (sx, cx) = (-frx.sin(), frx.cos());
    let (sy, cy) = (fry.sin(), fry.cos());
    let (sz, cz) = (-frz.sin(), frz.cos());
    let fax = g.fax * g.scale_x / g.scale_y;
    let fay = g.fay * g.scale_y / g.scale_x;
    let x1 = [1.0, fax, p.shift.0 as f64 + p.asc as f64 * fax];
    let y1 = [fay, 1.0, p.shift.1 as f64];
    let mut x2 = [0.0; 3];
    let mut y2 = [0.0; 3];
    for i in 0..3 {
        x2[i] = x1[i] * cz - y1[i] * sz;
        y2[i] = x1[i] * sz + y1[i] * cz;
    }
    let mut y3 = [0.0; 3];
    let mut z3 = [0.0; 3];
    for i in 0..3 {
        y3[i] = y2[i] * cx;
        z3[i] = y2[i] * sx;
    }
    let mut x4 = [0.0; 3];
    let mut z4 = [0.0; 3];
    for i in 0..3 {
        x4[i] = x2[i] * cy - z3[i] * sy;
        z4[i] = x2[i] * sy + z3[i] * cy;
    }
    let dist = 20000.0 * st.blur_scale_y;
    z4[2] += dist;
    let par = st.frame.par;
    let scale_x = dist * par;
    let offs_x = p.pos.0 as f64 - p.shift.0 as f64 * par;
    let offs_y = p.pos.1 as f64 - p.shift.1 as f64;
    let mut m = [[0.0; 3]; 3];
    for i in 0..3 {
        m[0][i] = z4[i] * offs_x + x4[i] * scale_x;
        m[1][i] = z4[i] * offs_y + y3[i] * dist;
        m[2][i] = z4[i];
    }
    m
}

/// Post-multiply by a glyph-space scale + offset.
fn with_scale(m1: &[[f64; 3]; 3], scale: (f64, f64), offset: (f64, f64)) -> [[f64; 3]; 3] {
    let mut m = [[0.0; 3]; 3];
    for i in 0..3 {
        m[i][0] = m1[i][0] * scale.0;
        m[i][1] = m1[i][1] * scale.1;
        m[i][2] = m1[i][0] * offset.0 + m1[i][1] * offset.1 + m1[i][2];
    }
    m
}

/// Behind the viewer (libass `quantize_transform` rejects `m_zz ≤ 0` at
/// the outline's centre).
fn visible(m: &[[f64; 3]; 3], o: &Outline) -> bool {
    let c = o.cbox().unwrap_or(outline::Rect { x_min: 0.0, y_min: 0.0, x_max: 0.0, y_max: 0.0 });
    let (x0, y0) = ((c.x_min + c.x_max) / 2.0, (c.y_min + c.y_max) / 2.0);
    m[2][0] * x0 + m[2][1] * y0 + m[2][2] > 0.0
}

/// libass `quantize_transform`'s position rounding: the transformed centre
/// of the outline's control box goes on the 1/8 px grid after taking off the
/// run's first residual, so the glyphs of a run keep their spacing and the
/// run as a whole snaps. Returns the shift to apply (26.6); `first` records
/// the residual.
fn snap(m: &[[f64; 3]; 3], o: &Outline, residual: &mut Option<(f64, f64)>, first: bool) -> Option<(f64, f64)> {
    // An empty outline (a space) has a zero control box in libass.
    let c = o.cbox().unwrap_or(outline::Rect { x_min: 0.0, y_min: 0.0, x_max: 0.0, y_max: 0.0 });
    let (x0, y0) = ((c.x_min + c.x_max) / 2.0, (c.y_min + c.y_max) / 2.0);
    let z = m[2][0] * x0 + m[2][1] * y0 + m[2][2];
    if z <= 0.0 {
        return None;
    }
    let cx = (m[0][0] * x0 + m[0][1] * y0 + m[0][2]) / z;
    let cy = (m[1][0] * x0 + m[1][1] * y0 + m[1][2]) / z;
    // centre in 1/8 px (64 >> SUBPIXEL_ORDER)
    let (ex, ey) = (cx / 8.0, cy / 8.0);
    let delta = if first { (0.0, 0.0) } else { residual.unwrap_or((0.0, 0.0)) };
    let q = ((ex - delta.0).round_ties_even(), (ey - delta.1).round_ties_even());
    if first {
        *residual = Some((ex - q.0, ey - q.1));
    }
    Some((q.0 * 8.0 - cx, q.1 * 8.0 - cy))
}

/// A glyph whose bitmaps, grown by its border (`bord`, glyph space 26.6)
/// and `spread` (px), cannot reach the frame. libass renders such glyphs
/// too; skipping them changes nothing on screen and keeps an event that
/// runs far off the frame (a page of text, a long `\move`) from
/// rasterizing all of it. Affine transforms only.
fn off_frame(st: &State<'_>, m: &[[f64; 3]; 3], o: &Outline, shift: (f64, f64), bord: (f64, f64), spread: (f64, f64)) -> bool {
    if m[2][0] != 0.0 || m[2][1] != 0.0 || m[2][2] <= 0.0 {
        return false;
    }
    let Some(c) = o.cbox() else { return true };
    let w = 1.0 / m[2][2];
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in [(c.x_min, c.y_min), (c.x_max, c.y_min), (c.x_min, c.y_max), (c.x_max, c.y_max)] {
        let sx = (m[0][0] * x + m[0][1] * y + m[0][2]) * w + shift.0;
        let sy = (m[1][0] * x + m[1][1] * y + m[1][2]) * w + shift.1;
        x0 = x0.min(sx);
        x1 = x1.max(sx);
        y0 = y0.min(sy);
        y1 = y1.max(sy);
    }
    let bx = (m[0][0].abs() * bord.0 + m[0][1].abs() * bord.1) * w;
    let by = (m[1][0].abs() * bord.0 + m[1][1].abs() * bord.1) * w;
    let mx = bx / 64.0 + spread.0 + 2.0;
    let my = by / 64.0 + spread.1 + 2.0;
    x1 / 64.0 + mx < 0.0 || x0 / 64.0 - mx > st.frame.width as f64 || y1 / 64.0 + my < 0.0 || y0 / 64.0 - my > st.frame.height as f64
}

fn placed(o: &Outline, m: &[[f64; 3]; 3], shift: (f64, f64)) -> Outline {
    outline::transform_3d(o, m).map(|p| P::new(p.x + shift.0, p.y + shift.1))
}

/// The bitmap of `outline` (with `stroked`: the border, glyph ∪ stroke)
/// placed by `m` + `shift`, through the bitmap cache when the transform is
/// affine.
fn raster(
    caches: &mut Caches,
    outline: &Arc<Outline>,
    stroked: Option<&Arc<Outline>>,
    m: &[[f64; 3]; 3],
    shift: (f64, f64),
) -> Option<Bitmap> {
    let draw = |m: &[[f64; 3]; 3], shift: (f64, f64)| {
        let fill = bitmap::fill(&placed(outline, m, shift));
        match stroked {
            Some(s) => bitmap::union(fill, bitmap::fill(&placed(s, m, shift))),
            None => fill,
        }
    };
    if m[2][0] != 0.0 || m[2][1] != 0.0 || m[2][2] < 0.1 {
        return draw(m, shift);
    }
    let w = 1.0 / m[2][2];
    let lin = [m[0][0] * w, m[0][1] * w, m[1][0] * w, m[1][1] * w];
    let (tx, ty) = (m[0][2] * w + shift.0, m[1][2] * w + shift.1);
    let (ix, iy) = ((tx / 64.0).floor(), (ty / 64.0).floor());
    let (fx, fy) = (((tx - ix * 64.0) * 16.0).round() as i32, ((ty - iy * 64.0) * 16.0).round() as i32);
    let key = (
        Arc::as_ptr(outline) as usize,
        stroked.map_or(0, |s| Arc::as_ptr(s) as usize),
        lin.map(f64::to_bits),
        fx,
        fy,
    );
    let bm = match caches.bitmap(&key) {
        Some(bm) => bm,
        None => {
            let unit = [[lin[0], lin[1], fx as f64 / 16.0], [lin[2], lin[3], fy as f64 / 16.0], [0.0, 0.0, 1.0]];
            let bm = draw(&unit, (0.0, 0.0)).map(Arc::new);
            caches.put_bitmap(key, outline, stroked, bm.clone());
            bm
        }
    }?;
    let mut out = (*bm).clone();
    out.left += ix as i32;
    out.top += iy as i32;
    Some(out)
}

/// libass `get_bitmap_glyph`: the part's fill and border bitmaps.
#[allow(clippy::too_many_arguments)]
fn glyph_bitmaps(
    st: &State<'_>,
    caches: &mut Caches,
    g: &Glyph,
    p: &Part,
    flags: u32,
    spread: Option<(f64, f64)>,
    leftmost_x: &mut f64,
    residual: &mut Option<(f64, f64)>,
) -> (Option<Bitmap>, Option<Bitmap>) {
    let Some(outline) = &p.outline else { return (None, None) };
    if g.symbol == '\n' as u32 || g.symbol == 0 || g.skip {
        return (None, None);
    }
    let m1 = transform_matrix(st, g, p);
    let m2 = with_scale(&m1, p.scale, p.toffset);
    if g.effect_type == Effect::KaraokeKf {
        if let Some(x) = outline::min_transformed_x(outline, &m2) {
            *leftmost_x = leftmost_x.min(x);
        }
    }
    if !visible(&m2, outline) {
        return (None, None);
    }
    let first = residual.is_none();
    let Some(shift) = snap(&m2, outline, residual, first) else { return (None, None) };
    if let Some(spread) = spread {
        let bord = if flags & F_NONZERO_BORDER != 0 {
            (64.0 * st.border_scale_x * g.border_x / p.scale.0 / st.frame.par, 64.0 * st.border_scale_y * g.border_y / p.scale.1)
        } else {
            (0.0, 0.0)
        };
        if off_frame(st, &m2, outline, shift, (bord.0.abs(), bord.1.abs()), spread) {
            return (None, None);
        }
    }
    let bm = raster(caches, outline, None, &m2, shift);

    if flags & F_BORDER_STYLE_3 != 0 {
        if flags & (F_NONZERO_BORDER | F_NONZERO_SHADOW) == 0 {
            return (bm, None);
        }
        let par = st.frame.par;
        let mut bord = (64.0 * g.border_x * st.border_scale_x / par, 64.0 * g.border_y * st.border_scale_y);
        let mut width = g.hspacing_scaled as f64 + p.advance.0 as f64;
        let mut height = (p.asc + p.desc) as f64;
        // Emulate the WTFish behavior of VSFilter, i.e. double-scale the
        // sizes of the opaque box.
        let orig = (g.scale_x * g.scale_fix, g.scale_y * g.scale_fix);
        bord.0 *= orig.0;
        bord.1 *= orig.1;
        width *= orig.0;
        height *= orig.1;
        // to avoid gaps
        bord.0 = bord.0.max(64.0);
        bord.1 = bord.1.max(64.0);
        let scale = ((width + 2.0 * bord.0) / 64.0, (height + 2.0 * bord.1) / 64.0);
        let offset = (-bord.0, -bord.1 - p.asc as f64);
        let m = with_scale(&m1, scale, offset);
        let mut boxo = Outline::default();
        boxo.add_rect(0.0, 0.0, 64.0, 64.0, true);
        let shift = snap(&m, &boxo, residual, false).unwrap_or(shift);
        let bo = bitmap::fill(&placed(&boxo, &m, shift));
        return (bm, bo);
    }
    if flags & F_NONZERO_BORDER == 0 {
        return (bm, None);
    }
    let bord_x = 64.0 * st.border_scale_x * g.border_x / p.scale.0 / st.frame.par;
    let bord_y = 64.0 * st.border_scale_y * g.border_y / p.scale.1;
    // libass rounds the stroker radius away below its precision; the
    // border is then the glyph itself.
    let px_x = g.border_x * st.border_scale_x;
    let px_y = g.border_y * st.border_scale_y;
    if px_x < 1.0 / 128.0 && px_y < 1.0 / 128.0 {
        return (bm.clone(), bm);
    }
    let stroked = caches.stroke(outline, bord_x.max(0.0), bord_y.max(0.0));
    // libass snaps the border outline by its own control box.
    let bshift = snap(&m2, &stroked, residual, false).unwrap_or(shift);
    let bo = raster(caches, outline, Some(&stroked), &m2, bshift);
    (bm, bo)
}

/// Sum bitmaps into one covering all of them, plus `pad` on each side.
fn combine(bms: &[Bitmap], pad: i32) -> Option<Bitmap> {
    let first = bms.first()?;
    let (mut l, mut t, mut r, mut b) = (first.left, first.top, first.left + first.w, first.top + first.h);
    for bm in &bms[1..] {
        l = l.min(bm.left);
        t = t.min(bm.top);
        r = r.max(bm.left + bm.w);
        b = b.max(bm.top + bm.h);
    }
    if bms.len() == 1 && pad == 0 {
        return Some(first.clone());
    }
    let mut out = Bitmap::new(l - pad, t - pad, r - l + 2 * pad, b - t + 2 * pad);
    for bm in bms {
        bitmap::add_into(&mut out, bm, 0, 0);
    }
    Some(out)
}

/// libass `ass_composite_construct`.
fn composite(c: &mut Combined) {
    let pad = bitmap::be_padding(c.be);
    let mut bm = combine(&c.glyphs, pad);
    let mut bm_o = combine(&c.borders, pad);
    let flags = c.flags;
    let r2x = restore_blur(c.blur_x);
    let r2y = restore_blur(c.blur_y);
    if flags & F_NONZERO_BORDER == 0 || flags & F_BORDER_STYLE_3 != 0 {
        if let Some(b) = bm.as_mut() {
            bitmap::synth_blur(b, c.be, r2x, r2y);
        }
    }
    if let Some(b) = bm_o.as_mut() {
        bitmap::synth_blur(b, c.be, r2x, r2y);
    }
    if flags & F_FILL_IN_BORDER == 0 && flags & F_FILL_IN_SHADOW == 0 {
        if let (Some(g), Some(o)) = (bm.as_ref(), bm_o.as_mut()) {
            bitmap::fix_outline(g, o);
        }
    }
    let mut bm_s = None;
    if flags & F_NONZERO_SHADOW != 0 {
        if flags & F_NONZERO_BORDER != 0 {
            bm_s = bm_o.clone();
            if flags & F_FILL_IN_BORDER != 0 && flags & F_FILL_IN_SHADOW == 0 {
                if let (Some(g), Some(s)) = (bm.as_ref(), bm_s.as_mut()) {
                    bitmap::fix_outline(g, s);
                }
            }
        } else if flags & F_BORDER_STYLE_3 != 0 {
            bm_s = bm_o.take();
        } else {
            bm_s = bm.clone();
        }
        if let Some(s) = bm_s.as_mut() {
            // '>>' rounds toward negative infinity, '&' returns the remainder
            s.left += c.shadow.0 >> 6;
            s.top += c.shadow.1 >> 6;
            bitmap::shift(s, c.shadow.0 & 63, c.shadow.1 & 63);
        }
    }
    if flags & F_FILL_IN_SHADOW != 0 && flags & F_FILL_IN_BORDER == 0 {
        if let (Some(g), Some(o)) = (bm.as_ref(), bm_o.as_mut()) {
            bitmap::fix_outline(g, o);
        }
    }
    c.bm = bm.filter(|b| !b.is_empty()).map(Arc::new);
    c.bm_o = bm_o.filter(|b| !b.is_empty()).map(Arc::new);
    c.bm_s = bm_s.filter(|b| !b.is_empty()).map(Arc::new);
}

/// libass `render_and_combine_glyphs` + composite + `render_text`: the
/// event's images in paint order.
pub fn render(text: &mut Text, st: &State<'_>, caches: &mut Caches) -> Vec<Image> {
    let device_x = text.device_x * st.frame.par;
    let device_y = text.device_y;
    let mut runs: Vec<Combined> = Vec::new();
    let mut new_run = true;
    for gi in 0..text.glyphs.len() {
        if text.glyphs[gi].starts_new_run {
            new_run = true;
        }
        if text.glyphs[gi].skip {
            continue;
        }
        let nparts = text.glyphs[gi].parts.len();
        for pi in 0..nparts {
            let g = &text.glyphs[gi];
            let mut flags = 0;
            if g.border_style == 3 {
                flags |= F_BORDER_STYLE_3;
            }
            if g.border_x != 0.0 || g.border_y != 0.0 {
                flags |= F_NONZERO_BORDER;
            }
            if g.shadow_x != 0.0 || g.shadow_y != 0.0 {
                flags |= F_NONZERO_SHADOW;
            }
            if flags & F_NONZERO_SHADOW != 0
                && (g.effect_type == Effect::KaraokeKf
                    || g.effect_type == Effect::KaraokeKo
                    || (g.c[0] & 0xFF) != 0xFF
                    || g.border_style == 3)
            {
                flags |= F_FILL_IN_SHADOW;
            }
            if flags & F_NONZERO_BORDER == 0 && flags & F_FILL_IN_SHADOW == 0 {
                flags &= !F_NONZERO_SHADOW;
            }
            if (flags & F_NONZERO_BORDER != 0 && (g.c[0] & 0xFF) == 0 && (g.c[1] & 0xFF) == 0 && g.fade == 0)
                || g.border_style == 3
            {
                flags |= F_FILL_IN_BORDER;
            }

            if new_run {
                let mut c = g.c;
                for col in &mut c {
                    apply_fade(col, g.fade);
                }
                let radius_scale = 2.0 / 256f64.ln().sqrt();
                let (blur_x, mask_x) = quantize_blur(g.blur * st.blur_scale_x * radius_scale);
                let (blur_y, mask_y) = quantize_blur(g.blur * st.blur_scale_y * radius_scale);
                let shadow = if flags & F_NONZERO_SHADOW != 0 {
                    let x = d6(g.shadow_x * st.border_scale_x);
                    let y = d6(g.shadow_y * st.border_scale_y);
                    ((x + (mask_x >> 1)) & !mask_x, (y + (mask_y >> 1)) & !mask_y)
                } else {
                    (0, 0)
                };
                let spread = (g.border_style != 3).then(|| {
                    let extent = |q: i32| restore_blur(q).sqrt() * 4.0 + 1.0;
                    let be = g.be.max(0) as f64 + bitmap::be_padding(g.be) as f64;
                    (extent(blur_x) + be + (shadow.0.abs() as f64 / 64.0).ceil(), extent(blur_y) + be + (shadow.1.abs() as f64 / 64.0).ceil())
                });
                runs.push(Combined {
                    flags,
                    spread,
                    be: g.be,
                    blur_x,
                    blur_y,
                    shadow,
                    c,
                    effect_type: g.effect_type,
                    effect_timing: g.effect_timing,
                    leftmost_x: f64::MAX,
                    residual: None,
                    glyphs: Vec::new(),
                    borders: Vec::new(),
                    bm: None,
                    bm_o: None,
                    bm_s: None,
                });
                new_run = false;
            }
            {
                let p = &mut text.glyphs[gi].parts[pi];
                p.pos.0 = d6(device_x + from_d6(p.pos.0) * st.frame.par);
                p.pos.1 = d6(device_y) + p.pos.1;
            }
            let g = &text.glyphs[gi];
            let p = &g.parts[pi];
            let run = runs.last_mut().unwrap();
            let (bm, bo) = glyph_bitmaps(st, caches, g, p, run.flags, run.spread, &mut run.leftmost_x, &mut run.residual);
            if let Some(b) = bm {
                run.glyphs.push(b);
            }
            if let Some(b) = bo {
                run.borders.push(b);
            }
        }
    }

    for run in runs.iter_mut() {
        if run.glyphs.is_empty() && run.borders.is_empty() {
            continue;
        }
        if run.effect_type == Effect::KaraokeKf {
            let lm = if run.leftmost_x == f64::MAX { 0.0 } else { lrint(run.leftmost_x) as f64 };
            run.effect_timing = (from_d6(lm as i32) + from_d6(run.effect_timing) * st.frame.par).round() as i32;
        }
        composite(run);
    }

    // render_text: shadows, then borders, then fills.
    let mut imgs = Vec::new();
    for run in &runs {
        if st.border_style == 4 {
            break;
        }
        if let Some(b) = &run.bm_s {
            clip_image(st, b, run.c[3], 0, 1_000_000, &mut imgs);
        }
    }
    for run in &runs {
        if let Some(b) = &run.bm_o {
            if !(run.effect_type == Effect::KaraokeKo && run.effect_timing <= 0) {
                clip_image(st, b, run.c[2], 0, 1_000_000, &mut imgs);
            }
        }
    }
    for run in &runs {
        let Some(b) = &run.bm else { continue };
        match run.effect_type {
            Effect::Karaoke | Effect::KaraokeKo => {
                let col = if run.effect_timing > 0 { run.c[0] } else { run.c[1] };
                clip_image(st, b, col, 0, 1_000_000, &mut imgs);
            }
            Effect::KaraokeKf => clip_image(st, b, run.c[0], run.c[1], run.effect_timing, &mut imgs),
            Effect::None => clip_image(st, b, run.c[0], 0, 1_000_000, &mut imgs),
        }
    }
    blend_vector_clip(st, &mut imgs, caches);
    imgs
}

/// libass `render_glyph` / `render_glyph_i`: images for a bitmap inside
/// (or, `\iclip`, outside) the clip rectangle, split at `brk` (absolute x)
/// into `color` left of it and `color2` right of it.
fn clip_image(st: &State<'_>, bm: &Arc<Bitmap>, color: u32, color2: u32, brk: i32, out: &mut Vec<Image>) {
    let dst_x = bm.left;
    let dst_y = bm.top;
    let brk = brk.saturating_sub(dst_x);
    let mut rects: Vec<(i32, i32, i32, i32)> = Vec::with_capacity(4);
    if st.clip_inverse {
        let zx = 0;
        let zy = 0;
        let sx = super::layout::x2scr_scaled(st, st.script.play_res_x as f64) as i32;
        let sy = super::layout::y2scr(st, st.script.play_res_y as f64) as i32;
        let (x0, y0, x1, y1) = (0, 0, bm.w, bm.h);
        let cx0 = st.clip_x0 - dst_x;
        let cy0 = st.clip_y0 - dst_y;
        let cx1 = st.clip_x1 - dst_x;
        let cy1 = st.clip_y1 - dst_y;
        let cand = [
            (x0, y0, if cx0 > x1 { x1 } else { cx0 }, y1),
            (if cx0 < 0 { x0 } else { cx0 }, y0, if cx1 > x1 { x1 } else { cx1 }, if cy0 > y1 { y1 } else { cy0 }),
            (if cx0 < 0 { x0 } else { cx0 }, if cy1 < 0 { y0 } else { cy1 }, if cx1 > x1 { x1 } else { cx1 }, y1),
            (if cx1 < 0 { x0 } else { cx1 }, y0, x1, y1),
        ];
        for (rx0, ry0, rx1, ry1) in cand {
            if rx1 > rx0 && ry1 > ry0 {
                // clip each rectangle to screen coordinates
                let rx0 = if rx0 + dst_x < zx { zx - dst_x } else { rx0 };
                let ry0 = if ry0 + dst_y < zy { zy - dst_y } else { ry0 };
                let rx1 = if rx1 + dst_x > sx { sx - dst_x } else { rx1 };
                let ry1 = if ry1 + dst_y > sy { sy - dst_y } else { ry1 };
                rects.push((rx0, ry0, rx1, ry1));
            }
        }
    } else {
        let cx0 = st.clip_x0.clamp(0, st.frame.width);
        let cy0 = st.clip_y0.clamp(0, st.frame.height);
        let cx1 = st.clip_x1.clamp(0, st.frame.width);
        let cy1 = st.clip_y1.clamp(0, st.frame.height);
        let b_x0 = (cx0 - dst_x).max(0);
        let b_y0 = (cy0 - dst_y).max(0);
        let b_x1 = bm.w + (cx1 - dst_x - bm.w).min(0);
        let b_y1 = bm.h + (cy1 - dst_y - bm.h).min(0);
        rects.push((b_x0, b_y0, b_x1, b_y1));
    }
    for (x0, y0, x1, y1) in rects {
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let mut push = |a: i32, b: i32, c: u32| {
            if b > a {
                out.push(Image { bitmap: bm.clone(), sx: a, sy: y0, w: b - a, h: y1 - y0, dst_x: dst_x + a, dst_y: dst_y + y0, color: c });
            }
        };
        let lbrk = brk.clamp(x0, x1);
        if brk > x0 {
            push(x0, lbrk, color);
        }
        if brk < x1 {
            push(lbrk, x1, color2);
        }
    }
}

/// libass `blend_vector_clip`.
fn blend_vector_clip(st: &State<'_>, imgs: &mut Vec<Image>, caches: &mut Caches) {
    let Some((scale, commands)) = &st.clip_drawing else { return };
    let Some((outline, _)) = caches.drawing(commands) else {
        return;
    };
    let base = (1u32 << ((*scale - 1) & 31)) as i32;
    let w = if base > 0 { 1.0 / base as f64 } else { 0.0 };
    let (kx, ky) = (st.screen_scale_x * w, st.screen_scale_y * w);
    let clip = bitmap::fill(&outline.map(|p| P::new(p.x * kx, p.y * ky)));
    let inverse = st.clip_drawing_inverse;
    let Some(clip) = clip else {
        if !inverse {
            imgs.clear();
        }
        return;
    };
    let mut out = Vec::with_capacity(imgs.len());
    for img in imgs.drain(..) {
        let (ax, ay, aw, ah) = (img.dst_x, img.dst_y, img.w, img.h);
        let (bx, by, bw, bh) = (clip.left, clip.top, clip.w, clip.h);
        let left = ax.max(bx);
        let top = ay.max(by);
        let right = (ax + aw).min(bx + bw);
        let bottom = (ay + ah).min(by + bh);
        let (w, h) = (right - left, bottom - top);
        let disjoint = ax + aw < bx || ay + ah < by || ax > bx + bw || ay > by + bh || w <= 0 || h <= 0;
        if inverse {
            if disjoint {
                out.push(img);
                continue;
            }
            let mut nb = Bitmap::new(ax, ay, aw, ah);
            for y in 0..ah {
                for x in 0..aw {
                    nb.data[(y * aw + x) as usize] = img.at(x, y);
                }
            }
            for y in 0..h {
                for x in 0..w {
                    let i = ((top - ay + y) * aw + (left - ax + x)) as usize;
                    let m = clip.at(left - bx + x, top - by + y) as u32;
                    nb.data[i] = ((nb.data[i] as u32 * (255 - m) + 255) >> 8) as u8;
                }
            }
            out.push(Image { bitmap: Arc::new(nb), sx: 0, sy: 0, w: aw, h: ah, dst_x: ax, dst_y: ay, color: img.color });
        } else {
            if disjoint {
                continue;
            }
            let mut nb = Bitmap::new(left, top, w, h);
            for y in 0..h {
                for x in 0..w {
                    let a = img.at(left - ax + x, top - ay + y) as u32;
                    let m = clip.at(left - bx + x, top - by + y) as u32;
                    nb.data[(y * w + x) as usize] = ((a * m + 255) >> 8) as u8;
                }
            }
            out.push(Image { bitmap: Arc::new(nb), sx: 0, sy: 0, w, h, dst_x: left, dst_y: top, color: img.color });
        }
    }
    *imgs = out;
}

/// libass `add_background` (`BorderStyle=4`, a libass extension): one box
/// in the back colour behind the whole event.
pub fn background(st: &State<'_>, left: i32, top: i32, width: i32, height: i32) -> Option<Image> {
    let size_x = if st.shadow_x > 0.0 { (st.shadow_x * st.border_scale_x).round() as i32 } else { 0 };
    let size_y = if st.shadow_y > 0.0 { (st.shadow_y * st.border_scale_y).round() as i32 } else { 0 };
    let l = (left - size_x).clamp(0, st.frame.width);
    let t = (top - size_y).clamp(0, st.frame.height);
    let r = (left + width + size_x).clamp(0, st.frame.width);
    let b = (top + height + size_y).clamp(0, st.frame.height);
    let (w, h) = (r - l, b - t);
    if w < 1 || h < 1 {
        return None;
    }
    let mut bm = Bitmap::new(l, t, w, h);
    bm.data.fill(0xFF);
    let mut c = st.c[3];
    apply_fade(&mut c, st.fade);
    Some(Image::whole(&Arc::new(bm), c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_quantization_round_trips() {
        for r in [0.5, 1.0, 3.0, 10.0] {
            let (q, _) = quantize_blur(r);
            let back = restore_blur(q).sqrt();
            // Quantization is relative (1/256 in log space) plus an absolute
            // floor, so small radii come back within a couple of percent.
            assert!((back - r).abs() / r < 0.03, "{r} -> {back}");
        }
        // No blur: shadows quantized to 1/8 px.
        assert_eq!(quantize_blur(0.0), (0, 7));
    }

    #[test]
    fn frexp_matches_c() {
        assert_eq!(frexp_exp(4.0), 3);
        assert_eq!(frexp_exp(1.0), 1);
        assert_eq!(frexp_exp(0.75), 0);
    }
}
