//! 8-bit coverage bitmaps and the operations libass performs on them
//! (libass/ass_bitmap.c, ass_blur.c, c/c_be_blur.c, c/c_blend_bitmaps.c).
//!
//! Rasterization is zeno's exact-area coverage (libass has its own
//! rasterizer of the same kind). `\be` is ported bit for bit; the gaussian
//! `\blur` is a true separable gaussian of the variance libass's cascade
//! filter approximates.

use super::outline::Outline;

/// A coverage bitmap at integer pixel position `(left, top)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub w: i32,
    pub h: i32,
    /// `w * h`, row-major.
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn new(left: i32, top: i32, w: i32, h: i32) -> Self {
        Self { left, top, w, h, data: vec![0; (w.max(0) * h.max(0)) as usize] }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    #[inline]
    pub fn at(&self, x: i32, y: i32) -> u8 {
        self.data[(y * self.w + x) as usize]
    }

}

fn render(cmds: &[zeno::Command], style: Option<zeno::Stroke<'_>>, transform: Option<zeno::Transform>) -> Option<Bitmap> {
    let mut mask = zeno::Mask::new(cmds);
    if let Some(s) = style {
        mask.style(s);
    }
    mask.transform(transform);
    let (data, place) = mask.render();
    if place.width == 0 || place.height == 0 {
        return None;
    }
    Some(Bitmap { left: place.left, top: place.top, w: place.width as i32, h: place.height as i32, data })
}

/// Fill an outline (26.6, screen coordinates), nonzero winding.
pub fn fill(outline: &Outline) -> Option<Bitmap> {
    if outline.is_empty() {
        return None;
    }
    render(&outline.to_commands(), None, None)
}


/// Pixelwise maximum of two bitmaps, on their union rectangle.
pub fn union(a: Option<Bitmap>, b: Option<Bitmap>) -> Option<Bitmap> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => {
            let l = a.left.min(b.left);
            let t = a.top.min(b.top);
            let r = (a.left + a.w).max(b.left + b.w);
            let bt = (a.top + a.h).max(b.top + b.h);
            let mut out = Bitmap::new(l, t, r - l, bt - t);
            for src in [&a, &b] {
                for y in 0..src.h {
                    let row = ((src.top - t + y) * out.w + (src.left - l)) as usize;
                    for x in 0..src.w as usize {
                        let v = src.data[(y * src.w) as usize + x];
                        let d = &mut out.data[row + x];
                        *d = (*d).max(v);
                    }
                }
            }
            Some(out)
        }
    }
}

/// libass `add_bitmaps` (saturating), `src` placed at its own position
/// inside `dst`'s rectangle (which must contain it).
pub fn add_into(dst: &mut Bitmap, src: &Bitmap, dx: i32, dy: i32) {
    for y in 0..src.h {
        let ty = src.top + dy + y - dst.top;
        if ty < 0 || ty >= dst.h {
            continue;
        }
        for x in 0..src.w {
            let tx = src.left + dx + x - dst.left;
            if tx < 0 || tx >= dst.w {
                continue;
            }
            let d = &mut dst.data[(ty * dst.w + tx) as usize];
            *d = (*d as u32 + src.data[(y * src.w + x) as usize] as u32).min(255) as u8;
        }
    }
}

/// libass `ass_be_padding`.
pub fn be_padding(be: i32) -> i32 {
    if be <= 3 {
        be
    } else if be <= 7 {
        4
    } else {
        5
    }
}

/// libass `be_blur_c`: one pass of the VSFilter [1 2 1]² / 16 kernel,
/// zero outside the bitmap, in place.
fn be_blur_pass(bm: &mut Bitmap) {
    let (w, h) = (bm.w as usize, bm.h as usize);
    if w < 2 || h < 2 {
        return;
    }
    let src = bm.data.clone();
    let at = |x: isize, y: isize| -> u32 {
        if x < 0 || y < 0 || x >= w as isize || y >= h as isize {
            0
        } else {
            src[y as usize * w + x as usize] as u32
        }
    };
    // Horizontal [1 2 1] then vertical [1 2 1]; integer sums, >> 4 at the end.
    let mut horiz = vec![0u32; w * h];
    for y in 0..h as isize {
        for x in 0..w as isize {
            horiz[y as usize * w + x as usize] = at(x - 1, y) + 2 * at(x, y) + at(x + 1, y);
        }
    }
    let hat = |x: usize, y: isize| -> u32 { if y < 0 || y >= h as isize { 0 } else { horiz[y as usize * w + x] } };
    for y in 0..h as isize {
        for x in 0..w {
            let v = hat(x, y - 1) + 2 * hat(x, y) + hat(x, y + 1);
            bm.data[y as usize * w + x] = (v >> 4) as u8;
        }
    }
}

/// libass `ass_synth_blur`: gaussian blur of variance `r2x`/`r2y` (px²),
/// then `be` passes of the box blur (with libass's 6-bit trick between
/// passes, so repeated passes round the same way).
pub fn synth_blur(bm: &mut Bitmap, be: i32, r2x: f64, r2y: f64) {
    if bm.is_empty() {
        return;
    }
    if r2x > 0.001 || r2y > 0.001 {
        gaussian_blur(bm, r2x, r2y);
    }
    if be <= 0 {
        return;
    }
    let mut be = be;
    be -= 1;
    if be > 0 {
        for v in bm.data.iter_mut() {
            *v = ((*v >> 1) + 1) >> 1;
        }
        while be > 0 {
            be_blur_pass(bm);
            be -= 1;
        }
        for v in bm.data.iter_mut() {
            let x = *v;
            *v = (x << 2).wrapping_sub((x > 32) as u8);
        }
    }
    be_blur_pass(bm);
}

fn gauss_kernel(r2: f64) -> Vec<f32> {
    if r2 <= 0.001 {
        return vec![1.0];
    }
    let sigma = r2.sqrt();
    let radius = (4.0 * sigma).ceil().max(1.0) as usize;
    let mut k: Vec<f64> = (0..=radius).map(|i| (-((i * i) as f64) / (2.0 * r2)).exp()).collect();
    let sum: f64 = k[0] + 2.0 * k[1..].iter().sum::<f64>();
    for v in &mut k {
        *v /= sum;
    }
    k.into_iter().map(|v| v as f32).collect()
}

/// Separable gaussian; the bitmap grows by the kernel radius on each side.
pub fn gaussian_blur(bm: &mut Bitmap, r2x: f64, r2y: f64) {
    let kx = gauss_kernel(r2x);
    let ky = gauss_kernel(r2y);
    let (rx, ry) = (kx.len() as i32 - 1, ky.len() as i32 - 1);
    let (w, h) = (bm.w + 2 * rx, bm.h + 2 * ry);
    // Horizontal pass into a float buffer of the grown width.
    let mut tmp = vec![0f32; (w * bm.h) as usize];
    for y in 0..bm.h {
        let row = &bm.data[(y * bm.w) as usize..((y + 1) * bm.w) as usize];
        let out = &mut tmp[(y * w) as usize..((y + 1) * w) as usize];
        for (x, &v) in row.iter().enumerate() {
            if v == 0 {
                continue;
            }
            let v = v as f32;
            let c = x as i32 + rx;
            for (i, &kv) in kx.iter().enumerate() {
                let i = i as i32;
                out[(c + i) as usize] += v * kv;
                if i > 0 {
                    out[(c - i) as usize] += v * kv;
                }
            }
        }
    }
    let mut out = vec![0f32; (w * h) as usize];
    for y in 0..bm.h {
        let c = y + ry;
        for x in 0..w {
            let v = tmp[(y * w + x) as usize];
            if v == 0.0 {
                continue;
            }
            for (i, &kv) in ky.iter().enumerate() {
                let i = i as i32;
                out[((c + i) * w + x) as usize] += v * kv;
                if i > 0 {
                    out[((c - i) * w + x) as usize] += v * kv;
                }
            }
        }
    }
    bm.left -= rx;
    bm.top -= ry;
    bm.w = w;
    bm.h = h;
    bm.data = out.into_iter().map(|v| (v + 0.5).clamp(0.0, 255.0) as u8).collect();
}

/// libass `ass_fix_outline`: take the glyph out of the border underneath it.
pub fn fix_outline(g: &Bitmap, o: &mut Bitmap) {
    if g.is_empty() || o.is_empty() {
        return;
    }
    let l = o.left.max(g.left);
    let t = o.top.max(g.top);
    let r = (o.left + o.w).min(g.left + g.w);
    let b = (o.top + o.h).min(g.top + g.h);
    for y in t..b {
        for x in l..r {
            let gv = g.data[((y - g.top) * g.w + (x - g.left)) as usize];
            let ov = &mut o.data[((y - o.top) * o.w + (x - o.left)) as usize];
            *ov = if *ov > gv { *ov - gv / 2 } else { 0 };
        }
    }
}

/// libass `ass_shift_bitmap`: shift by a fraction of a pixel (26.6, 0..63)
/// — coverage moves right/down into the next pixel. The bitmap gets one
/// extra column/row so nothing falls off.
pub fn shift(bm: &mut Bitmap, sx: i32, sy: i32) {
    if bm.is_empty() || (sx == 0 && sy == 0) {
        return;
    }
    let mut g = Bitmap::new(bm.left, bm.top, bm.w + 1, bm.h + 1);
    for y in 0..bm.h {
        let d = (y * g.w) as usize;
        g.data[d..d + bm.w as usize].copy_from_slice(&bm.data[(y * bm.w) as usize..((y + 1) * bm.w) as usize]);
    }
    let (w, h, s) = (g.w, g.h, g.w as usize);
    if sx != 0 {
        for y in 0..h as usize {
            for x in (1..w as usize).rev() {
                let b = ((g.data[x + y * s - 1] as i32 * sx) >> 6) as u8;
                g.data[x + y * s - 1] -= b;
                g.data[x + y * s] = g.data[x + y * s].saturating_add(b);
            }
        }
    }
    if sy != 0 {
        for x in 0..w as usize {
            for y in (1..h as usize).rev() {
                let b = ((g.data[x + y * s - s] as i32 * sy) >> 6) as u8;
                g.data[x + y * s - s] -= b;
                g.data[x + y * s] = g.data[x + y * s].saturating_add(b);
            }
        }
    }
    *bm = g;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderers::subtitle::ass::outline::parse_drawing;

    fn square(px: f64) -> Outline {
        parse_drawing(&format!("m 0 0 l {px} 0 {px} {px} 0 {px}")).unwrap().0
    }

    #[test]
    fn fill_covers_the_square_exactly() {
        let b = fill(&square(10.0)).unwrap();
        assert_eq!((b.left, b.top, b.w, b.h), (0, 0, 10, 10));
        assert!(b.data.iter().all(|&v| v == 255));
    }

    #[test]
    fn border_dilates_round_and_elliptic() {
        // The renderer's border: the outline's fill united with its stroke
        // (radii in 26.6, so 2 px = 128).
        let border = |o: &Outline, rx: f64, ry: f64| union(fill(o), fill(&crate::renderers::subtitle::ass::outline::stroke(o, rx * 64.0, ry * 64.0)));
        let b = border(&square(10.0), 2.0, 2.0).unwrap();
        assert_eq!((b.left, b.top, b.w, b.h), (-2, -2, 14, 14));
        // Corners are rounded: the extreme corner pixel is partial.
        assert!(b.at(0, 0) < 128);
        assert_eq!(b.at(1, 7), 255);
        let e = border(&square(10.0), 4.0, 1.0).unwrap();
        assert_eq!((e.left, e.top, e.w, e.h), (-4, -1, 18, 12));
    }

    #[test]
    fn be_blur_matches_the_vsfilter_kernel() {
        let mut b = Bitmap::new(0, 0, 3, 3);
        b.data[4] = 160;
        be_blur_pass(&mut b);
        assert_eq!(b.data, vec![10, 20, 10, 20, 40, 20, 10, 20, 10]);
    }

    #[test]
    fn gaussian_keeps_mass_and_grows() {
        let mut b = fill(&square(20.0)).unwrap();
        let before: u32 = b.data.iter().map(|&v| v as u32).sum();
        gaussian_blur(&mut b, 4.0, 4.0);
        assert!(b.w > 20 && b.left < 0);
        let after: u32 = b.data.iter().map(|&v| v as u32).sum();
        assert!((after as f64 / before as f64 - 1.0).abs() < 0.01);
    }

    #[test]
    fn shift_moves_coverage_by_a_fraction() {
        let mut b = Bitmap::new(0, 0, 1, 1);
        b.data[0] = 200;
        shift(&mut b, 32, 0);
        assert_eq!((b.w, b.h), (2, 2));
        assert_eq!((b.at(0, 0), b.at(1, 0)), (100, 100));
    }

    #[test]
    fn fix_outline_halves_under_the_glyph() {
        let g = Bitmap { left: 0, top: 0, w: 1, h: 1, data: vec![100] };
        let mut o = Bitmap { left: 0, top: 0, w: 2, h: 1, data: vec![200, 50] };
        fix_outline(&g, &mut o);
        assert_eq!(o.data, vec![150, 50]);
    }
}
