//! Vector outlines: glyphs, `\p` drawings, `\clip` shapes, the opaque box.
//!
//! Coordinates are libass's: 26.6 fixed point (1/64 px) held in `f64`,
//! y pointing down. Glyph outlines come out of skrifa in font units with y
//! up and are converted the way libass's FreeType path converts them
//! (`ass_outline_convert` flips y), including FreeType's synthetic bold
//! (`FT_Outline_EmboldenXY`) and libass's synthetic italic shear
//! (`ass_glyph_italicize`) and underline / strike-out rectangles
//! (`ass_get_glyph_outline`). Drawings are parsed by a port of
//! libass/ass_drawing.c, VSFilter quirks included.

use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{GlyphId, MetadataProvider};

use super::fonts::Face;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P {
    pub x: f64,
    pub y: f64,
}

impl P {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(P),
    Line(P),
    Quad(P, P),
    Cubic(P, P, P),
    Close,
}

/// Axis-aligned bounds, 26.6.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outline {
    pub segs: Vec<Seg>,
}

impl Outline {
    pub fn is_empty(&self) -> bool {
        !self.segs.iter().any(|s| !matches!(s, Seg::Move(_) | Seg::Close))
    }

    fn points(&self) -> impl Iterator<Item = P> + '_ {
        self.segs.iter().flat_map(|s| {
            let pts: [Option<P>; 3] = match *s {
                Seg::Move(a) | Seg::Line(a) => [Some(a), None, None],
                Seg::Quad(a, b) => [Some(a), Some(b), None],
                Seg::Cubic(a, b, c) => [Some(a), Some(b), Some(c)],
                Seg::Close => [None, None, None],
            };
            pts.into_iter().flatten()
        })
    }

    /// Control box (libass `ass_outline_update_cbox`): all points,
    /// control points included.
    pub fn cbox(&self) -> Option<Rect> {
        let mut r: Option<Rect> = None;
        for p in self.points() {
            let rr = r.get_or_insert(Rect { x_min: p.x, y_min: p.y, x_max: p.x, y_max: p.y });
            rr.x_min = rr.x_min.min(p.x);
            rr.y_min = rr.y_min.min(p.y);
            rr.x_max = rr.x_max.max(p.x);
            rr.y_max = rr.y_max.max(p.y);
        }
        r
    }

    pub fn map(&self, f: impl Fn(P) -> P) -> Outline {
        Outline {
            segs: self
                .segs
                .iter()
                .map(|s| match *s {
                    Seg::Move(a) => Seg::Move(f(a)),
                    Seg::Line(a) => Seg::Line(f(a)),
                    Seg::Quad(a, b) => Seg::Quad(f(a), f(b)),
                    Seg::Cubic(a, b, c) => Seg::Cubic(f(a), f(b), f(c)),
                    Seg::Close => Seg::Close,
                })
                .collect(),
        }
    }


    /// Add a closed rectangle `x0..x1 × y0..y1`, wound clockwise on screen
    /// when `clockwise`.
    pub fn add_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, clockwise: bool) {
        let pts = if clockwise {
            [P::new(x0, y0), P::new(x1, y0), P::new(x1, y1), P::new(x0, y1)]
        } else {
            [P::new(x0, y0), P::new(x0, y1), P::new(x1, y1), P::new(x1, y0)]
        };
        self.segs.push(Seg::Move(pts[0]));
        for p in &pts[1..] {
            self.segs.push(Seg::Line(*p));
        }
        self.segs.push(Seg::Close);
    }

    /// Shoelace area over the on-curve and control points, positive when
    /// clockwise on screen (y down). Only its sign is used.
    pub fn signed_area(&self) -> f64 {
        let mut area = 0.0;
        let mut start = None;
        let mut prev = P::new(0.0, 0.0);
        let mut add = |a: P, b: P| area += a.x * b.y - b.x * a.y;
        for p in self.segs.iter() {
            match *p {
                Seg::Move(a) => {
                    if let Some(s) = start.replace(a) {
                        add(prev, s);
                    }
                    prev = a;
                }
                Seg::Line(a) => {
                    add(prev, a);
                    prev = a;
                }
                Seg::Quad(a, b) => {
                    add(prev, a);
                    add(a, b);
                    prev = b;
                }
                Seg::Cubic(a, b, c) => {
                    add(prev, a);
                    add(a, b);
                    add(b, c);
                    prev = c;
                }
                Seg::Close => {
                    if let Some(s) = start.take() {
                        add(prev, s);
                        prev = s;
                    }
                }
            }
        }
        if let Some(s) = start {
            add(prev, s);
        }
        area
    }

    /// zeno path commands in pixels (26.6 / 64).
    pub fn to_commands(&self) -> Vec<zeno::Command> {
        let v = |p: P| zeno::Vector::new((p.x / 64.0) as f32, (p.y / 64.0) as f32);
        let mut out = Vec::with_capacity(self.segs.len());
        let mut open = false;
        for s in &self.segs {
            match *s {
                Seg::Move(a) => {
                    if open {
                        out.push(zeno::Command::Close);
                    }
                    out.push(zeno::Command::MoveTo(v(a)));
                    open = true;
                }
                Seg::Line(a) => out.push(zeno::Command::LineTo(v(a))),
                Seg::Quad(a, b) => out.push(zeno::Command::QuadTo(v(a), v(b))),
                Seg::Cubic(a, b, c) => out.push(zeno::Command::CurveTo(v(a), v(b), v(c))),
                Seg::Close => {
                    out.push(zeno::Command::Close);
                    open = false;
                }
            }
        }
        if open {
            out.push(zeno::Command::Close);
        }
        out
    }
}

/// libass `ass_outline_transform_3d`: projective transform (the z divide
/// clamped at 0.1, as in libass).
pub fn transform_3d(o: &Outline, m: &[[f64; 3]; 3]) -> Outline {
    o.map(|p| {
        let v0 = m[0][0] * p.x + m[0][1] * p.y + m[0][2];
        let v1 = m[1][0] * p.x + m[1][1] * p.y + m[1][2];
        let v2 = m[2][0] * p.x + m[2][1] * p.y + m[2][2];
        let w = 1.0 / v2.max(0.1);
        P::new(v0 * w, v1 * w)
    })
}

/// Smallest x of the control points after the transform (libass
/// `ass_outline_update_min_transformed_x`, karaoke `\kf` origin).
pub fn min_transformed_x(o: &Outline, m: &[[f64; 3]; 3]) -> Option<f64> {
    o.points()
        .filter_map(|p| {
            let z = m[2][0] * p.x + m[2][1] * p.y + m[2][2];
            let x = (m[0][0] * p.x + m[0][1] * p.y + m[0][2]) / z.max(0.1);
            (!x.is_nan()).then_some(x)
        })
        .reduce(f64::min)
}

/// The stroke of `o` by an ellipse of radii `rx` × `ry` (26.6 units of
/// `o`), round joins and caps, as path geometry. Filled together with the
/// outline itself this is libass's border shape (its stroker emits the two
/// offset curves; the filled area is the same).
pub fn stroke(o: &Outline, rx: f64, ry: f64) -> Outline {
    if o.is_empty() || (rx <= 0.0 && ry <= 0.0) {
        return Outline::default();
    }
    // A circle of radius r in a space where the ellipse is one; a zero
    // radius on one axis becomes a very flat ellipse.
    let (r, sx, sy) = if rx >= ry { (rx, 1.0, (ry / rx).max(0.01)) } else { (ry, (rx / ry).max(0.01), 1.0) };
    let squeezed = o.map(|p| P::new(p.x / sx, p.y / sy));
    let mut st = zeno::Stroke::new((2.0 * r / 64.0) as f32);
    st.join(zeno::Join::Round).cap(zeno::Cap::Round);
    let transform = (sx != 1.0 || sy != 1.0).then(|| zeno::Transform::scale(sx as f32, sy as f32));
    let mut cmds: Vec<zeno::Command> = Vec::new();
    zeno::apply(&squeezed.to_commands(), st, transform, &mut cmds);
    let pt = |v: zeno::Vector| P::new(v.x as f64 * 64.0, v.y as f64 * 64.0);
    let mut out = Outline::default();
    for c in cmds {
        out.segs.push(match c {
            zeno::Command::MoveTo(a) => Seg::Move(pt(a)),
            zeno::Command::LineTo(a) => Seg::Line(pt(a)),
            zeno::Command::QuadTo(a, b) => Seg::Quad(pt(a), pt(b)),
            zeno::Command::CurveTo(a, b, c) => Seg::Cubic(pt(a), pt(b), pt(c)),
            zeno::Command::Close => Seg::Close,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

/// A glyph outline in font units, y up, one point list per contour in
/// drawing order (what the synthetic bold works on).
#[derive(Clone, Default)]
pub struct UnitGlyph {
    /// Each contour: segments starting with `Move`.
    pub contours: Vec<Vec<Seg>>,
}

struct Pen {
    contours: Vec<Vec<Seg>>,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.contours.push(vec![Seg::Move(P::new(x as f64, y as f64))]);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(c) = self.contours.last_mut() {
            c.push(Seg::Line(P::new(x as f64, y as f64)));
        }
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        if let Some(c) = self.contours.last_mut() {
            c.push(Seg::Quad(P::new(cx0 as f64, cy0 as f64), P::new(x as f64, y as f64)));
        }
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        if let Some(c) = self.contours.last_mut() {
            c.push(Seg::Cubic(
                P::new(cx0 as f64, cy0 as f64),
                P::new(cx1 as f64, cy1 as f64),
                P::new(x as f64, y as f64),
            ));
        }
    }
    fn close(&mut self) {}
}

/// Load a glyph's outline in font units. `None` for a glyph that can't be
/// drawn; an empty glyph (a space) is `Some` with no contours.
pub fn load_unit_glyph(face: &Face, glyph: u32) -> Option<UnitGlyph> {
    let font = face.font_ref()?;
    let outlines = font.outline_glyphs();
    let g = outlines.get(GlyphId::new(glyph))?;
    let mut pen = Pen { contours: Vec::new() };
    g.draw(DrawSettings::unhinted(Size::unscaled(), LocationRef::default()), &mut pen).ok()?;
    // skip degenerate contours (libass skips 2-point contours from broken fonts)
    pen.contours.retain(|c| c.len() > 1);
    Some(UnitGlyph { contours: pen.contours })
}

/// Advance width in font units.
pub fn unit_advance(face: &Face, glyph: u32) -> f64 {
    face.font.glyph_metrics().h_advance(GlyphId::new(glyph)) as f64
}

/// Advance height in font units (the em box height without `vmtx`).
pub fn unit_v_advance(face: &Face, glyph: u32) -> f64 {
    let v = face.font.glyph_metrics().v_advance(GlyphId::new(glyph)) as f64;
    if v > 0.0 {
        v
    } else {
        (face.ascender - face.descender) as f64
    }
}

/// What libass applies while loading a glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphStyle {
    /// Synthetic italic (the face isn't italic, the request is).
    pub italicize: bool,
    /// Synthetic bold (the face is ≥ 150 lighter than requested).
    pub embolden: bool,
    pub underline: bool,
    pub strike_out: bool,
    /// Vertical layout: turned 90°, advancing by the vertical advance.
    pub rotate: bool,
}

/// libass `ass_font_get_glyph` + `ass_get_glyph_outline`: the glyph at
/// `size` px (libass's REAL_DIM size), synthetic styles applied, y down,
/// 26.6. Returns the outline and the advance (26.6).
pub fn glyph_outline(face: &Face, unit: &UnitGlyph, glyph: u32, size: f64, style: GlyphStyle) -> (Outline, f64) {
    let scale = face.units_to_d6(size);
    // FreeType space: y up, 26.6.
    let mut contours: Vec<Vec<Seg>> = unit
        .contours
        .iter()
        .map(|c| c.iter().map(|s| map_seg(*s, |p| P::new(p.x * scale, p.y * scale))).collect())
        .collect();
    let advance = if style.rotate { unit_v_advance(face, glyph) } else { unit_advance(face, glyph) } * scale;

    if style.italicize {
        // GDI's slant for TrueType, 10° for PostScript outlines.
        let k = if face.is_postscript { 0x02d24 as f64 } else { 0x05700 as f64 } / 65536.0;
        for c in &mut contours {
            for s in c.iter_mut() {
                *s = map_seg(*s, |p| P::new(p.x + k * p.y, p.y));
            }
        }
    }
    if style.embolden {
        // FT_MulFix(units_per_EM, y_scale) / 64: the em in 26.6, over 64.
        let strength = face.units_per_em as f64 * scale / 64.0;
        embolden(&mut contours, strength, strength);
    }

    let mut out = Outline::default();
    for c in &contours {
        for s in c {
            out.segs.push(map_seg(*s, |p| P::new(p.x, -p.y)));
        }
        out.segs.push(Seg::Close);
    }
    if style.rotate {
        // libass ass_get_glyph_outline / ass_outline_rotate_90
        let desc = (face.typo_descender as f64 * scale).round();
        let dv = advance + desc;
        out = out.map(|p| P::new(dv + p.y, -desc - p.x));
    }

    if advance > 0.0 && (style.underline || style.strike_out) {
        // Wind the bars like the glyph so a nonzero fill adds them.
        let clockwise = out.signed_area() >= 0.0;
        let mut bar = |pos: i32, thick: i32| {
            let size = (thick as f64 * scale).round();
            let pos = -(pos as f64 * scale).round() - (size / 2.0).floor();
            out.add_rect(0.0, pos, advance, pos + size, clockwise);
        };
        if style.underline {
            if let Some((pos, thick)) = face.underline {
                bar(pos, thick);
            }
        }
        if style.strike_out {
            if let Some((pos, size)) = face.strikeout {
                bar(pos, size);
            }
        }
    }
    (out, advance)
}

fn map_seg(s: Seg, f: impl Fn(P) -> P) -> Seg {
    match s {
        Seg::Move(a) => Seg::Move(f(a)),
        Seg::Line(a) => Seg::Line(f(a)),
        Seg::Quad(a, b) => Seg::Quad(f(a), f(b)),
        Seg::Cubic(a, b, c) => Seg::Cubic(f(a), f(b), f(c)),
        Seg::Close => Seg::Close,
    }
}

/// FreeType `FT_Outline_EmboldenXY` on y-up contours: every point moves
/// half the strength outward along the corner bisector (shifts limited
/// where segments collapse), plus half the strength right and up.
fn embolden(contours: &mut [Vec<Seg>], xstrength: f64, ystrength: f64) {
    let xs = xstrength / 2.0;
    let ys = ystrength / 2.0;
    if xs == 0.0 && ys == 0.0 {
        return;
    }
    // Orientation over all contours (FT_Outline_Get_Orientation): positive
    // area = counter-clockwise = PostScript.
    let mut area = 0.0;
    for c in contours.iter() {
        let pts = contour_points(c);
        for i in 0..pts.len() {
            let a = pts[(i + pts.len() - 1) % pts.len()];
            let b = pts[i];
            area += (b.y - a.y) * (b.x + a.x);
        }
    }
    if area == 0.0 {
        return;
    }
    let truetype = area < 0.0;

    for c in contours.iter_mut() {
        let mut pts = contour_points(c);
        let n = pts.len();
        if n < 2 {
            continue;
        }
        let orig = pts.clone();
        let norm = |v: P| -> (P, f64) {
            let l = (v.x * v.x + v.y * v.y).sqrt();
            if l == 0.0 {
                (P::new(0.0, 0.0), 0.0)
            } else {
                (P::new(v.x / l, v.y / l), l)
            }
        };
        // Per-vertex shift from the incoming and outgoing directions; a
        // simplification of FreeType's loop that walks past coincident
        // points the same way (the shift of a run of equal points is the
        // shift of the corner they form).
        let mut shifts = vec![P::new(0.0, 0.0); n];
        for i in 0..n {
            let mut p = (i + n - 1) % n;
            let mut guard = 0;
            while orig[p] == orig[i] && guard < n {
                p = (p + n - 1) % n;
                guard += 1;
            }
            let mut q = (i + 1) % n;
            guard = 0;
            while orig[q] == orig[i] && guard < n {
                q = (q + 1) % n;
                guard += 1;
            }
            let (inv, l_in) = norm(P::new(orig[i].x - orig[p].x, orig[i].y - orig[p].y));
            let (out, l_out) = norm(P::new(orig[q].x - orig[i].x, orig[q].y - orig[i].y));
            if l_in == 0.0 || l_out == 0.0 {
                continue;
            }
            let d = inv.x * out.x + inv.y * out.y;
            // shift only if the turn is less than ~160 degrees
            if d > -0.9375 {
                let d = d + 1.0;
                let mut sx = inv.y + out.y;
                let mut sy = inv.x + out.x;
                if truetype {
                    sx = -sx;
                } else {
                    sy = -sy;
                }
                let mut qq = out.x * inv.y - out.y * inv.x;
                if truetype {
                    qq = -qq;
                }
                let l = l_in.min(l_out);
                sx = if xs * qq <= l * d { sx * xs / d } else { sx * l / qq };
                sy = if ys * qq <= l * d { sy * ys / d } else { sy * l / qq };
                shifts[i] = P::new(sx, sy);
            }
        }
        for i in 0..n {
            pts[i] = P::new(orig[i].x + xs + shifts[i].x, orig[i].y + ys + shifts[i].y);
        }
        set_contour_points(c, &pts);
    }
}

fn contour_points(c: &[Seg]) -> Vec<P> {
    let mut v = Vec::new();
    for s in c {
        match *s {
            Seg::Move(a) | Seg::Line(a) => v.push(a),
            Seg::Quad(a, b) => v.extend([a, b]),
            Seg::Cubic(a, b, cc) => v.extend([a, b, cc]),
            Seg::Close => {}
        }
    }
    // A closing point equal to the start is the same vertex.
    if v.len() > 1 && v.first() == v.last() {
        v.pop();
    }
    v
}

fn set_contour_points(c: &mut [Seg], pts: &[P]) {
    let mut i = 0;
    let n = pts.len();
    let mut next = || {
        let p = pts[i % n];
        i += 1;
        p
    };
    for s in c.iter_mut() {
        *s = match *s {
            Seg::Move(_) => Seg::Move(next()),
            Seg::Line(_) => Seg::Line(next()),
            Seg::Quad(_, _) => {
                let a = next();
                Seg::Quad(a, next())
            }
            Seg::Cubic(_, _, _) => {
                let a = next();
                let b = next();
                Seg::Cubic(a, b, next())
            }
            Seg::Close => Seg::Close,
        };
    }
}

// ---------------------------------------------------------------------------
// Drawings (libass/ass_drawing.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Move,
    MoveNc,
    Line,
    Cubic,
    BSpline,
    ExtendSpline,
}

/// libass `mystrtod` over drawing text: skips leading whitespace, reads a
/// float, reports how far it got.
fn read_num(s: &[u8], i: &mut usize) -> Option<f64> {
    let mut j = *i;
    while j < s.len() && matches!(s[j], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        j += 1;
    }
    let start = j;
    if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
        j += 1;
    }
    let ds = j;
    while j < s.len() && s[j].is_ascii_digit() {
        j += 1;
    }
    let mut digits = j - ds;
    if j < s.len() && s[j] == b'.' {
        let mut k = j + 1;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        digits += k - j - 1;
        if digits > 0 {
            j = k;
        }
    }
    if digits == 0 {
        return None;
    }
    if j < s.len() && (s[j] == b'e' || s[j] == b'E') {
        let mut k = j + 1;
        if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
            k += 1;
        }
        let es = k;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        if k > es {
            j = k;
        }
    }
    let v = std::str::from_utf8(&s[start..j]).ok()?.parse::<f64>().ok()?;
    *i = j;
    Some(v)
}

/// A point, rounded to 26.6 like `double_to_d6`.
fn read_point(s: &[u8], i: &mut usize) -> Option<(i64, i64)> {
    let save = *i;
    let x = read_num(s, i);
    let y = x.and_then(|_| read_num(s, i));
    match (x, y) {
        (Some(x), Some(y)) => Some(((x * 64.0).round() as i64, (y * 64.0).round() as i64)),
        _ => {
            // libass consumes a lone x; keep that.
            if x.is_none() {
                *i = save;
            }
            None
        }
    }
}

/// Parse a drawing (`\p`, `\clip(drawing)`) into an outline in 26.6 drawing
/// units, plus its control box. `None` when nothing drawable came out.
pub fn parse_drawing(text: &str) -> Option<(Outline, Rect)> {
    let tokens = tokenize_drawing(text.as_bytes())?;
    let mut out = Outline::default();
    let mut cbox: Option<Rect> = None;
    let mut upd = |p: (i64, i64)| {
        let (x, y) = (p.0 as f64, p.1 as f64);
        let r = cbox.get_or_insert(Rect { x_min: x, y_min: y, x_max: x, y_max: y });
        r.x_min = r.x_min.min(x);
        r.y_min = r.y_min.min(y);
        r.x_max = r.x_max.max(x);
        r.y_max = r.y_max.max(y);
    };
    let pt = |p: (i64, i64)| P::new(p.0 as f64, p.1 as f64);
    let mut started = false;
    let mut pen = (0i64, 0i64);
    let mut i = 0;
    while i < tokens.len() {
        let (ty, p) = tokens[i];
        match ty {
            Tok::MoveNc => {
                pen = p;
                upd(pen);
                i += 1;
            }
            Tok::Move => {
                pen = p;
                upd(pen);
                if started {
                    out.segs.push(Seg::Close);
                    started = false;
                }
                i += 1;
            }
            Tok::Line => {
                upd(p);
                if !started {
                    out.segs.push(Seg::Move(pt(pen)));
                }
                out.segs.push(Seg::Line(pt(p)));
                started = true;
                pen = p;
                i += 1;
            }
            Tok::Cubic | Tok::BSpline => {
                if i == 0 || i + 2 >= tokens.len() {
                    break;
                }
                let pts = [tokens[i - 1].1, tokens[i].1, tokens[i + 1].1, tokens[i + 2].1];
                add_curve(&mut out, &mut upd, pts, ty == Tok::BSpline, started);
                pen = pts[3];
                started = true;
                i += 3;
            }
            Tok::ExtendSpline => {
                if i < 3 {
                    break;
                }
                let pts = [tokens[i - 3].1, tokens[i - 2].1, tokens[i - 1].1, tokens[i].1];
                add_curve(&mut out, &mut upd, pts, true, started);
                pen = pts[3];
                started = true;
                i += 1;
            }
        }
    }
    if started {
        out.segs.push(Seg::Close);
    }
    let cbox = cbox?;
    Some((out, cbox))
}

fn add_curve(out: &mut Outline, upd: &mut impl FnMut((i64, i64)), mut p: [(i64, i64); 4], spline: bool, started: bool) {
    for q in p {
        upd(q);
    }
    if spline {
        // Uniform B-spline → Bézier, integer arithmetic as in libass
        // (C division truncates toward zero, `>>` floors).
        let d = |a: i64, b: i64| (b - a) / 3;
        let (x01, y01) = (d(p[0].0, p[1].0), d(p[0].1, p[1].1));
        let (x12, y12) = (d(p[1].0, p[2].0), d(p[1].1, p[2].1));
        let (x23, y23) = (d(p[2].0, p[3].0), d(p[2].1, p[3].1));
        p[0] = (p[1].0 + ((x12 - x01) >> 1), p[1].1 + ((y12 - y01) >> 1));
        p[3] = (p[2].0 + ((x23 - x12) >> 1), p[2].1 + ((y23 - y12) >> 1));
        p[1] = (p[1].0 + x12, p[1].1 + y12);
        p[2] = (p[2].0 - x12, p[2].1 - y12);
    }
    let pt = |q: (i64, i64)| P::new(q.0 as f64, q.1 as f64);
    if !started {
        out.segs.push(Seg::Move(pt(p[0])));
    }
    out.segs.push(Seg::Cubic(pt(p[1]), pt(p[2]), pt(p[3])));
}

/// libass `drawing_tokenize`, including its acceptance rules: commands
/// before the first `m` are ignored, an `n` before any `m` rejects the
/// drawing, `p` needs three earlier points, `c` closes a b-spline.
fn tokenize_drawing(s: &[u8]) -> Option<Vec<(Tok, (i64, i64))>> {
    let mut toks: Vec<(Tok, (i64, i64))> = Vec::new();
    let mut points = 0usize;
    let mut m_seen = false;
    let mut spline_start: Option<usize> = None;
    let mut i = 0;
    // Repeatedly read points as `ty`, `batch` at a time (partial batches
    // are consumed and dropped). Returns the count added.
    fn many(s: &[u8], i: &mut usize, toks: &mut Vec<(Tok, (i64, i64))>, ty: Tok, batch: usize) -> usize {
        let mut buf = Vec::with_capacity(3);
        let mut added = 0;
        while *i < s.len() {
            let Some(p) = read_point(s, i) else { break };
            buf.push(p);
            if buf.len() == batch {
                for q in buf.drain(..) {
                    toks.push((ty, q));
                    added += 1;
                }
            }
        }
        added
    }
    while i < s.len() {
        let cmd = s[i];
        i += 1;
        match cmd {
            b'm' => {
                m_seen = true;
                if toks.is_empty() {
                    let Some(p) = read_point(s, &mut i) else { continue };
                    toks.push((Tok::Move, p));
                    points = 1;
                }
                points += many(s, &mut i, &mut toks, Tok::Move, 1);
            }
            b'n' => {
                if toks.is_empty() {
                    let Some(p) = read_point(s, &mut i) else { continue };
                    if !m_seen {
                        return None;
                    }
                    toks.push((Tok::MoveNc, p));
                    points = 1;
                }
                points += many(s, &mut i, &mut toks, Tok::MoveNc, 1);
            }
            b'l' => {
                if toks.is_empty() {
                    continue;
                }
                points += many(s, &mut i, &mut toks, Tok::Line, 1);
            }
            b'b' => {
                if toks.is_empty() {
                    continue;
                }
                points += many(s, &mut i, &mut toks, Tok::Cubic, 3);
            }
            b's' | b'p' => {
                if toks.is_empty() {
                    continue;
                }
                if cmd == b's' {
                    let start = toks.len() - 1;
                    let mut three = Vec::with_capacity(3);
                    for _ in 0..3 {
                        match read_point(s, &mut i) {
                            Some(p) => three.push(p),
                            None => break,
                        }
                    }
                    if three.len() < 3 {
                        spline_start = None;
                        continue;
                    }
                    spline_start = Some(start);
                    for p in three {
                        toks.push((Tok::BSpline, p));
                    }
                    points += 3;
                }
                if points < 3 {
                    continue;
                }
                points += many(s, &mut i, &mut toks, Tok::ExtendSpline, 1);
            }
            b'c' => {
                let Some(start) = spline_start.take() else { continue };
                // Close the b-spline: its first three points again.
                for k in 0..3 {
                    if let Some(&(_, p)) = toks.get(start + k) {
                        toks.push((Tok::ExtendSpline, p));
                    }
                }
            }
            _ => {}
        }
    }
    (!toks.is_empty()).then_some(toks)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderers::subtitle::ass::fonts::{builtin_faces, FontDb};

    #[test]
    fn drawing_square_and_quirks() {
        let (o, cbox) = parse_drawing("m 0 0 l 100 0 100 100 0 100").unwrap();
        assert_eq!(cbox, Rect { x_min: 0.0, y_min: 0.0, x_max: 6400.0, y_max: 6400.0 });
        assert_eq!(o.segs.len(), 5); // move, 3 lines, close
        // Commands before the first m are ignored; n first rejects.
        assert!(parse_drawing("l 10 10 m 0 0 l 5 5").is_some());
        assert!(parse_drawing("n 0 0 l 10 10").is_none());
        assert!(parse_drawing("").is_none());
        // Bezier and spline make curves.
        let (o, _) = parse_drawing("m 0 0 b 10 0 20 10 20 20").unwrap();
        assert!(o.segs.iter().any(|s| matches!(s, Seg::Cubic(..))));
        let (o, _) = parse_drawing("m 0 0 s 10 0 20 10 20 20 c").unwrap();
        assert!(o.segs.iter().filter(|s| matches!(s, Seg::Cubic(..))).count() >= 2);
    }

    #[test]
    fn drawing_from_the_sample_logo_parses() {
        let (o, cbox) = parse_drawing("m 355.52 255.52 l 355.52 251.7 348.38 251.7 348.38 255.52 342.05 255.52").unwrap();
        assert!(!o.is_empty());
        assert!((cbox.x_min - 342.05 * 64.0).abs() < 1.0);
    }

    #[test]
    fn glyph_outline_is_y_down_and_sized_by_gdi_metrics() {
        let db = FontDb::new(builtin_faces());
        let face = db.face(0);
        let g = face.glyph('H' as u32).unwrap();
        let unit = load_unit_glyph(face, g).unwrap();
        let (o, adv) = glyph_outline(face, &unit, g, 100.0, GlyphStyle::default());
        let cbox = o.cbox().unwrap();
        // Cap height above the baseline → negative y.
        assert!(cbox.y_min < -40.0 * 64.0 && cbox.y_max <= 1.0, "{cbox:?}");
        assert!(adv > 30.0 * 64.0 && adv < 120.0 * 64.0);
        // Synthetic bold widens, italic leans right at the top.
        let (b, _) = glyph_outline(face, &unit, g, 100.0, GlyphStyle { embolden: true, ..Default::default() });
        let bb = b.cbox().unwrap();
        assert!(bb.x_max - bb.x_min > cbox.x_max - cbox.x_min);
        let (it, _) = glyph_outline(face, &unit, g, 100.0, GlyphStyle { italicize: true, ..Default::default() });
        assert!(it.cbox().unwrap().x_max > cbox.x_max);
        // Underline adds a bar under the baseline.
        let (u, _) = glyph_outline(face, &unit, g, 100.0, GlyphStyle { underline: true, ..Default::default() });
        assert!(u.cbox().unwrap().y_max > 0.0);
    }
}
