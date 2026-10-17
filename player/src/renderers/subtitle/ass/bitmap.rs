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
                for (y, s) in src.data.chunks_exact(src.w as usize).enumerate() {
                    let row = ((src.top - t + y as i32) * out.w + (src.left - l)) as usize;
                    for (d, &v) in out.data[row..row + s.len()].iter_mut().zip(s) {
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
    let ox = src.left + dx - dst.left;
    let (x0, x1) = ((-ox).max(0), (dst.w - ox).min(src.w));
    if x1 <= x0 {
        return;
    }
    for y in 0..src.h {
        let ty = src.top + dy + y - dst.top;
        if ty < 0 || ty >= dst.h {
            continue;
        }
        let s = &src.data[(y * src.w + x0) as usize..(y * src.w + x1) as usize];
        let d0 = (ty * dst.w + ox + x0) as usize;
        for (d, &v) in dst.data[d0..d0 + s.len()].iter_mut().zip(s) {
            *d = d.saturating_add(v);
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
        blur(bm, r2x, r2y);
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

/// Gaussian taps 0..=radius in 1/65536, the full kernel summing to 65536.
fn gauss_kernel(r2: f64) -> Vec<u32> {
    if r2 <= 0.001 {
        return vec![65536];
    }
    let sigma = r2.sqrt();
    let radius = (4.0 * sigma).ceil().max(1.0) as usize;
    let k: Vec<f64> = (0..=radius).map(|i| (-((i * i) as f64) / (2.0 * r2)).exp()).collect();
    let sum: f64 = k[0] + 2.0 * k[1..].iter().sum::<f64>();
    let mut q: Vec<u32> = k.iter().map(|v| (v / sum * 65536.0).round() as u32).collect();
    // the centre tap takes the rounding so the kernel sums to exactly 1
    let side: u32 = q[1..].iter().sum();
    q[0] = 65536 - 2 * side;
    q
}

/// Separable gaussian; the bitmap grows by the kernel radius on each side.
/// Fixed point (integer math vectorizes on every target, 32-bit ARM
/// included): the horizontal pass keeps 8 fractional bits in `u16`, the
/// vertical one accumulates in `u32` and rounds once. Both run over whole
/// rows as plain slices.
pub fn gaussian_blur(bm: &mut Bitmap, r2x: f64, r2y: f64) {
    let kx = gauss_kernel(r2x);
    let ky = gauss_kernel(r2y);
    let (rx, ry) = (kx.len() - 1, ky.len() - 1);
    let (bw, bh) = (bm.w as usize, bm.h as usize);
    let (w, h) = (bw + 2 * rx, bh + 2 * ry);
    // Horizontal: out[x] = Σ k[|j|]·src[x − rx − j], the source row at 2·rx
    // in a zero-padded buffer so every tap is a slice. 255·65536 fits u32.
    let mut tmp = vec![0u16; w * bh];
    let mut padded = vec![0u32; bw + 4 * rx];
    let mut acc = vec![0u32; w];
    for (y, row) in bm.data.chunks_exact(bw).enumerate() {
        if row.iter().all(|&v| v == 0) {
            continue;
        }
        for (d, &v) in padded[2 * rx..2 * rx + bw].iter_mut().zip(row) {
            *d = v as u32;
        }
        for (o, &v) in acc.iter_mut().zip(&padded[rx..rx + w]) {
            *o = kx[0] * v;
        }
        for (i, &k) in kx.iter().enumerate().skip(1) {
            let (a, b) = (&padded[rx - i..rx - i + w], &padded[rx + i..rx + i + w]);
            for ((o, &a), &b) in acc.iter_mut().zip(a).zip(b) {
                *o += k * (a + b);
            }
        }
        for (t, &v) in tmp[y * w..(y + 1) * w].iter_mut().zip(&acc) {
            *t = ((v + 128) >> 8) as u16;
        }
    }
    // Vertical: out row y = Σ k[|j|]·tmp row (y − ry − j), gathered one
    // output row at a time so the accumulator stays in cache (65280·65536
    // still fits u32).
    let mut data = vec![0u8; w * h];
    vertical_gather(&tmp, w, bh, &ky, |y, acc| {
        for (d, &v) in data[y * w..(y + 1) * w].iter_mut().zip(acc) {
            *d = ((v as u64 + (1 << 23)) >> 24).min(255) as u8;
        }
    });
    bm.left -= rx as i32;
    bm.top -= ry as i32;
    bm.w = w as i32;
    bm.h = h as i32;
    bm.data = data;
}

/// The vertical pass of the separable gaussians: for each output row
/// (`rows + 2·r` of them) the kernel-weighted sum of the source rows around
/// it, symmetric taps paired, handed to `emit` as one row of `u32`.
fn vertical_gather(src: &[u16], w: usize, rows: usize, k: &[u32], mut emit: impl FnMut(usize, &[u32])) {
    let r = k.len() - 1;
    let zero = vec![0u16; w];
    let row = |i: isize| if i >= 0 && (i as usize) < rows { &src[i as usize * w..(i as usize + 1) * w] } else { &zero[..] };
    let mut acc = vec![0u32; w];
    for y in 0..rows + 2 * r {
        let c = y as isize - r as isize;
        for (a, &v) in acc.iter_mut().zip(row(c)) {
            *a = k[0] * v as u32;
        }
        for (j, &kj) in k.iter().enumerate().skip(1) {
            let (p, q) = (row(c - j as isize), row(c + j as isize));
            for ((a, &p), &q) in acc.iter_mut().zip(p).zip(q) {
                *a += kj * (p as u32 + q as u32);
            }
        }
        emit(y, &acc);
    }
}

/// Gaussian of variance `r2x`/`r2y` (px²), libass's way for large radii
/// (`ass_gaussian_blur`): halve the bitmap `L` times with the [1 5 10 10 5
/// 1]/32 filter, blur the small one with what is left of the variance,
/// double it back `L` times — the cost per pixel no longer grows with the
/// radius. Small radii go straight to [`gaussian_blur`]. The shrink and
/// expand filters are libass's ([1 5 10 10 5 1]/32 and (5 10 1)/16, 6-bit
/// fixed point); the variance they add themselves is measured once
/// (`pyramid_variance`) and taken off the small blur, so the total is
/// still `r2`.
pub fn blur(bm: &mut Bitmap, r2x: f64, r2y: f64) {
    let (lx, ly) = (pyramid_level(r2x), pyramid_level(r2y));
    if lx == 0 && ly == 0 {
        gaussian_blur(bm, r2x, r2y);
        return;
    }
    let mut img = Plane { w: bm.w as usize, h: bm.h as usize, data: bm.data.iter().map(|&v| (v as u16) << 6).collect() };
    for _ in 0..lx {
        img = img.shrink_h();
    }
    for _ in 0..ly {
        img = img.shrink_v();
    }
    let small = |r2: f64, l: usize| ((r2 - pyramid_variance(l)) / (1u32 << (2 * l)) as f64).max(0.0);
    let (gx, gy) = img.gaussian(small(r2x, lx), small(r2y, ly));
    for _ in 0..ly {
        img = img.expand_v();
    }
    for _ in 0..lx {
        img = img.expand_h();
    }
    // Where the result sits: each shrink+expand pair moves the image's
    // origin by 4 px at its own scale, the small blur by its radius.
    let shift = |l: usize, g: usize| (4 * ((1 << l) - 1) + (g << l)) as i32;
    bm.left -= shift(lx, gx);
    bm.top -= shift(ly, gy);
    bm.w = img.w as i32;
    bm.h = img.h as i32;
    bm.data = img.data.into_iter().map(|v| ((v as u32 + 32) >> 6).min(255) as u8).collect();
}

/// Smallest sigma (px, at the reduced scale) the blur after the halvings
/// may have: below it the pyramid's own kernel shape shows.
const PYRAMID_MIN_SIGMA: f64 = 1.0;

/// Pyramid levels for a blur of variance `r2`: as many halvings as leave
/// the small blur a sigma of at least [`PYRAMID_MIN_SIGMA`], at most 4.
fn pyramid_level(r2: f64) -> usize {
    let mut level = 0;
    while level < 4 {
        let next = level + 1;
        let rest = (r2 - pyramid_variance(next)) / (1u32 << (2 * next)) as f64;
        if rest < PYRAMID_MIN_SIGMA * PYRAMID_MIN_SIGMA {
            break;
        }
        level = next;
    }
    level
}

/// Variance (full-resolution px²) that `level` shrinks followed by as many
/// expands add on their own, measured on an impulse with the same filters
/// in floating point.
fn pyramid_variance(level: usize) -> f64 {
    static TABLE: std::sync::OnceLock<[f64; 5]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0.0; 5];
        for (l, v) in t.iter_mut().enumerate().skip(1) {
            let n = 64 << l;
            let mut x = vec![0.0f64; n];
            x[n / 2] = 1.0;
            for _ in 0..l {
                x = (0..(x.len() + 5) / 2)
                    .map(|i| {
                        let s = |j: isize| if j >= 0 && (j as usize) < x.len() { x[j as usize] } else { 0.0 };
                        let j = 2 * i as isize;
                        (s(j - 4) + 5.0 * s(j - 3) + 10.0 * s(j - 2) + 10.0 * s(j - 1) + 5.0 * s(j) + s(j + 1)) / 32.0
                    })
                    .collect();
            }
            for _ in 0..l {
                let s = |j: isize| if j >= 0 && (j as usize) < x.len() { x[j as usize] } else { 0.0 };
                let mut y = vec![0.0; 2 * x.len() + 4];
                for i in 0..x.len() + 2 {
                    let (p1, z0, n1) = (s(i as isize - 2), s(i as isize - 1), s(i as isize));
                    y[2 * i] = (5.0 * p1 + 10.0 * z0 + n1) / 16.0;
                    y[2 * i + 1] = (p1 + 10.0 * z0 + 5.0 * n1) / 16.0;
                }
                x = y;
            }
            let sum: f64 = x.iter().sum();
            let mean = x.iter().enumerate().map(|(i, v)| i as f64 * v).sum::<f64>() / sum;
            *v = x.iter().enumerate().map(|(i, v)| (i as f64 - mean).powi(2) * v).sum::<f64>() / sum;
        }
        t
    })[level]
}

/// A coverage plane in 6-bit fixed point (libass's blur precision).
struct Plane {
    w: usize,
    h: usize,
    data: Vec<u16>,
}

impl Plane {
    /// libass `shrink_func`'s filter: out[i] = (s[2i−4] + 5s[2i−3] +
    /// 10s[2i−2] + 10s[2i−1] + 5s[2i] + s[2i+1]) / 32, zero outside.
    #[inline]
    fn shrink(p1p: u32, p1n: u32, z0p: u32, z0n: u32, n1p: u32, n1n: u32) -> u16 {
        ((p1p + n1n + 5 * (p1n + n1p) + 10 * (z0p + z0n) + 16) >> 5) as u16
    }

    fn shrink_h(&self) -> Plane {
        let w = (self.w + 5) / 2;
        let mut data = vec![0u16; w * self.h];
        // taps reach 2(w − 1) + 5
        let mut row = vec![0u32; 2 * w + 4];
        for (src, dst) in self.data.chunks_exact(self.w).zip(data.chunks_exact_mut(w)) {
            // row[k] = src[k − 4], zero padded both sides
            for (r, &v) in row[4..4 + self.w].iter_mut().zip(src) {
                *r = v as u32;
            }
            for (i, d) in dst.iter_mut().enumerate() {
                let j = 2 * i;
                *d = Self::shrink(row[j], row[j + 1], row[j + 2], row[j + 3], row[j + 4], row[j + 5]);
            }
        }
        Plane { w, h: self.h, data }
    }

    fn shrink_v(&self) -> Plane {
        let h = (self.h + 5) / 2;
        let w = self.w;
        let mut data = vec![0u16; w * h];
        let zero = vec![0u16; w];
        let row = |k: isize| if k >= 0 && (k as usize) < self.h { &self.data[k as usize * w..(k as usize + 1) * w] } else { &zero[..] };
        for (i, dst) in data.chunks_exact_mut(w).enumerate() {
            let j = 2 * i as isize;
            let (a, b, c, d, e, f) = (row(j - 4), row(j - 3), row(j - 2), row(j - 1), row(j), row(j + 1));
            for x in 0..w {
                dst[x] = Self::shrink(a[x] as u32, b[x] as u32, c[x] as u32, d[x] as u32, e[x] as u32, f[x] as u32);
            }
        }
        Plane { w, h, data }
    }

    /// libass `expand_func`: two outputs around `z0`, from its neighbours.
    #[inline]
    fn expand(p1: u32, z0: u32, n1: u32) -> (u16, u16) {
        let r = (((p1 + n1) >> 1) + z0) >> 1;
        let rp = ((((r + p1) >> 1) + z0 + 1) >> 1) as u16;
        let rn = ((((r + n1) >> 1) + z0 + 1) >> 1) as u16;
        (rp, rn)
    }

    fn expand_h(&self) -> Plane {
        let w = 2 * self.w + 4;
        let mut data = vec![0u16; w * self.h];
        let mut row = vec![0u32; self.w + 4];
        for (src, dst) in self.data.chunks_exact(self.w).zip(data.chunks_exact_mut(w)) {
            // row[k] = src[k − 2]
            for (r, &v) in row[2..2 + self.w].iter_mut().zip(src) {
                *r = v as u32;
            }
            for i in 0..self.w + 2 {
                let (a, b) = Self::expand(row[i], row[i + 1], row[i + 2]);
                dst[2 * i] = a;
                dst[2 * i + 1] = b;
            }
        }
        Plane { w, h: self.h, data }
    }

    fn expand_v(&self) -> Plane {
        let h = 2 * self.h + 4;
        let w = self.w;
        let mut data = vec![0u16; w * h];
        let zero = vec![0u16; w];
        let row = |k: isize| if k >= 0 && (k as usize) < self.h { &self.data[k as usize * w..(k as usize + 1) * w] } else { &zero[..] };
        for i in 0..self.h + 2 {
            let (p1, z0, n1) = (row(i as isize - 2), row(i as isize - 1), row(i as isize));
            let (top, bottom) = data[2 * i * w..(2 * i + 2) * w].split_at_mut(w);
            for x in 0..w {
                let (a, b) = Self::expand(p1[x] as u32, z0[x] as u32, n1[x] as u32);
                top[x] = a;
                bottom[x] = b;
            }
        }
        Plane { w, h, data }
    }

    /// Separable gaussian (fixed point, like [`gaussian_blur`]); the plane
    /// grows by the kernel radius on each side, which is returned.
    fn gaussian(&mut self, r2x: f64, r2y: f64) -> (usize, usize) {
        let kx = gauss_kernel(r2x);
        let ky = gauss_kernel(r2y);
        let (rx, ry) = (kx.len() - 1, ky.len() - 1);
        let (bw, bh) = (self.w, self.h);
        let (w, h) = (bw + 2 * rx, bh + 2 * ry);
        let mut tmp = vec![0u16; w * bh];
        let mut padded = vec![0u32; bw + 4 * rx];
        let mut acc = vec![0u32; w];
        for (y, row) in self.data.chunks_exact(bw).enumerate() {
            for (d, &v) in padded[2 * rx..2 * rx + bw].iter_mut().zip(row) {
                *d = v as u32;
            }
            for (o, &v) in acc.iter_mut().zip(&padded[rx..rx + w]) {
                *o = kx[0] * v;
            }
            for (i, &k) in kx.iter().enumerate().skip(1) {
                let (a, b) = (&padded[rx - i..rx - i + w], &padded[rx + i..rx + i + w]);
                for ((o, &a), &b) in acc.iter_mut().zip(a).zip(b) {
                    *o += k * (a + b);
                }
            }
            for (t, &v) in tmp[y * w..(y + 1) * w].iter_mut().zip(&acc) {
                *t = ((v + (1 << 15)) >> 16) as u16;
            }
        }
        let mut data = vec![0u16; w * h];
        vertical_gather(&tmp, w, bh, &ky, |y, acc| {
            for (d, &v) in data[y * w..(y + 1) * w].iter_mut().zip(acc) {
                *d = ((v + (1 << 15)) >> 16) as u16;
            }
        });
        self.w = w;
        self.h = h;
        self.data = data;
        (rx, ry)
    }
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
    if r <= l {
        return;
    }
    let n = (r - l) as usize;
    for y in t..b {
        let gi = ((y - g.top) * g.w + (l - g.left)) as usize;
        let oi = ((y - o.top) * o.w + (l - o.left)) as usize;
        for (ov, &gv) in o.data[oi..oi + n].iter_mut().zip(&g.data[gi..gi + n]) {
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
        // Columns are independent: bottom-up row by row is the same as
        // column by column.
        for y in (1..h as usize).rev() {
            let (above, below) = g.data.split_at_mut(y * s);
            let above = &mut above[(y - 1) * s..];
            for (a, d) in above.iter_mut().zip(&mut below[..s]) {
                let b = ((*a as i32 * sy) >> 6) as u8;
                *a -= b;
                *d = d.saturating_add(b);
            }
        }
    }
    *bm = g;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderers::subtitle::ass::outline::parse_drawing;

    /// Centre of mass and variance per axis of a bitmap's ink, in frame px.
    fn moments(b: &Bitmap) -> ((f64, f64), (f64, f64)) {
        let (mut s, mut sx, mut sy, mut sxx, mut syy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for y in 0..b.h {
            for x in 0..b.w {
                let v = b.at(x, y) as f64;
                let (fx, fy) = ((b.left + x) as f64 + 0.5, (b.top + y) as f64 + 0.5);
                s += v;
                sx += v * fx;
                sy += v * fy;
                sxx += v * fx * fx;
                syy += v * fy * fy;
            }
        }
        let (mx, my) = (sx / s, sy / s);
        ((mx, my), (sxx / s - mx * mx, syy / s - my * my))
    }

    #[test]
    fn pyramid_blur_matches_the_gaussian() {
        // A 6x6 block at (100, 50); blurs from small (direct path) to
        // large (pyramid, up to 4 levels), round and elliptic.
        for (r2x, r2y) in [(4.0, 4.0), (25.0, 25.0), (100.0, 100.0), (400.0, 400.0), (1600.0, 900.0), (64.0, 900.0)] {
            let mut src = Bitmap::new(100, 50, 6, 6);
            src.data.fill(255);
            let mut exact = src.clone();
            gaussian_blur(&mut exact, r2x, r2y);
            let mut fast = src.clone();
            blur(&mut fast, r2x, r2y);
            let ((ex, ey), (evx, evy)) = moments(&exact);
            let ((fx, fy), (fvx, fvy)) = moments(&fast);
            assert!((ex - fx).abs() < 0.3 && (ey - fy).abs() < 0.3, "r2 {r2x}/{r2y}: centre moved ({ex},{ey}) -> ({fx},{fy})");
            assert!((fvx / evx - 1.0).abs() < 0.05 && (fvy / evy - 1.0).abs() < 0.05, "r2 {r2x}/{r2y}: variance {evx},{evy} -> {fvx},{fvy}");
            // and pixel by pixel, on the exact one's grid
            let mut worst = 0i32;
            for y in 0..exact.h {
                for x in 0..exact.w {
                    let (gx, gy) = (exact.left + x - fast.left, exact.top + y - fast.top);
                    let f = if (0..fast.w).contains(&gx) && (0..fast.h).contains(&gy) { fast.at(gx, gy) as i32 } else { 0 };
                    worst = worst.max((exact.at(x, y) as i32 - f).abs());
                }
            }
            let peak = exact.data.iter().copied().max().unwrap() as i32;
            assert!(worst * 10 <= peak.max(10), "r2 {r2x}/{r2y}: off by {worst} (peak {peak})");
        }
    }

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
