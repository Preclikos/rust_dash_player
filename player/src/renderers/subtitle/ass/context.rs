//! Per-event render state and what override tags do to it.
//!
//! The state is libass's `RenderContext`; [`State::apply_tags`] is the
//! semantic half of `ass_parse_tags` (libass/ass_parse.c) — the same
//! interpolation under `\t` (`pwr`), the same "first `\pos` / `\an` /
//! `\fad` wins" rules, the same fallbacks to the current style.
//! [`State::new`] is `init_render_context` + `ass_reset_render_context`
//! (+ `ass_apply_transition_effects` for the legacy `Banner` / `Scroll`
//! effects).

use crate::parsers::ass::{numpad2align, AssEvent, AssScript, AssStyle, ClipShape, FontSize, KaraokeKind, Tag};

use super::fonts::{FontCache, FontDb, FontDesc};

pub const DECO_UNDERLINE: u8 = 1;
pub const DECO_STRIKETHROUGH: u8 = 2;
/// A vertical (`@Font`) glyph turned 90°.
pub const DECO_ROTATE: u8 = 4;
/// libass `VERTICAL_LOWER_BOUND`: below it, `@Font` text stays upright.
pub const VERTICAL_LOWER_BOUND: u32 = 0x02f1;

const BLUR_MAX_RADIUS: f64 = 100.0;
const MAX_BE: i32 = 127;

pub const EVENT_POSITIONED: u8 = 1;
pub const EVENT_HSCROLL: u8 = 2;
pub const EVENT_VSCROLL: u8 = 4;

const PARSED_A: u8 = 1;
const PARSED_FADE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Effect {
    #[default]
    None,
    Karaoke,
    KaraokeKf,
    KaraokeKo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    LeftToRight,
    RightToLeft,
    TopToBottom,
    BottomToTop,
}

/// Frame-wide settings (libass `ASS_Renderer` / `ASS_Settings`): the frame
/// is the video picture the script is laid out on.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub width: i32,
    pub height: i32,
    /// libass `par_scale_x`.
    pub par: f64,
    /// User text size multiplier (`ass_set_font_scale`), not applied to
    /// explicitly positioned events.
    pub font_size_coeff: f64,
    /// The video's own resolution (`ass_set_storage_size`): what blur,
    /// unscaled borders and 3D perspective are measured in when the script
    /// has no `LayoutResX/Y`. `None` = the script's PlayRes.
    pub storage: Option<(i32, i32)>,
}

/// libass `ass_layout_res`.
pub fn layout_res(script: &AssScript, frame: &Frame) -> (i32, i32) {
    if script.layout_res_x > 0 && script.layout_res_y > 0 {
        return (script.layout_res_x, script.layout_res_y);
    }
    match frame.storage {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => (script.play_res_x, script.play_res_y),
    }
}

/// libass `dtoi32`.
pub fn dtoi32(v: f64) -> i32 {
    if v.is_nan() || v <= i32::MIN as f64 || v >= i32::MAX as f64 + 1.0 {
        i32::MIN
    } else {
        v as i32
    }
}

/// libass `ass_lrint`: round half to even.
#[inline]
pub fn lrint(v: f64) -> i64 {
    v.round_ties_even() as i64
}

#[inline]
pub fn d6(v: f64) -> i32 {
    lrint(v * 64.0) as i32
}

#[inline]
pub fn from_d6(v: i32) -> f64 {
    v as f64 / 64.0
}

fn alpha_of(c: u32) -> u32 {
    c & 0xFF
}

fn calc_anim(new: f64, old: f64, pwr: f64) -> f64 {
    (1.0 - pwr) * old + new * pwr
}

/// libass `change_color`: RGB towards `new` by `pwr`, alpha kept.
fn change_color(var: &mut u32, new: u32, pwr: f64) {
    let mut out = *var & 0xFF;
    for shift in [24, 16, 8] {
        let o = ((*var >> shift) & 0xFF) as f64;
        let n = ((new >> shift) & 0xFF) as f64;
        let v = dtoi32(calc_anim(n, o, pwr)) as u32 & 0xFF;
        out |= v << shift;
    }
    *var = out;
}

/// libass `change_alpha`.
fn change_alpha(var: &mut u32, new: i32, pwr: f64) {
    let a = dtoi32(calc_anim(new as f64, alpha_of(*var) as f64, pwr)) as u8;
    *var = (*var & 0xFFFF_FF00) | a as u32;
}

/// libass `mult_alpha`.
fn mult_alpha(a: u32, b: u32) -> u32 {
    a - ((a as u64 * b as u64 + 0x7F) / 0xFF) as u32 + b
}

/// libass `ass_apply_fade` (positive fades only, VSFilter).
pub fn apply_fade(c: &mut u32, fade: i32) {
    if fade > 0 {
        let a = mult_alpha(alpha_of(*c), fade as u32);
        change_alpha(c, a as i32, 1.0);
    }
}

/// libass `interpolate_alpha` (`\fad` / `\fade`).
fn interpolate_alpha(now: i64, t1: i32, t2: i32, t3: i32, t4: i32, a1: i32, a2: i32, a3: i32) -> i32 {
    let now = now as i32;
    if now < t1 {
        a1
    } else if now < t2 {
        let cf = now.wrapping_sub(t1) as f64 / t2.wrapping_sub(t1) as f64;
        (a1 as f64 * (1.0 - cf) + a2 as f64 * cf) as i32
    } else if now < t3 {
        a2
    } else if now < t4 {
        let cf = now.wrapping_sub(t3) as f64 / t4.wrapping_sub(t3) as f64;
        (a2 as f64 * (1.0 - cf) + a3 as f64 * cf) as i32
    } else {
        a3
    }
}

/// libass `ass_event_has_hard_overrides`: `\pos`, `\move`, `\clip`,
/// `\iclip`, `\org`, `\pbo` or `\p…` anywhere inside an override block,
/// read off the raw text like libass does.
pub fn has_hard_overrides(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 2;
        } else if b[i] == b'{' {
            i += 1;
            while i < b.len() && b[i] != b'}' {
                if b[i] == b'\\' {
                    let rest = &b[i + 1..];
                    if ["pos", "move", "clip", "iclip", "org", "pbo", "p"].iter().any(|t| rest.starts_with(t.as_bytes())) {
                        return true;
                    }
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    false
}

pub struct State<'a> {
    pub script: &'a AssScript,
    pub event: &'a AssEvent,
    /// Current style: the event's, or the one a `\r` switched to.
    pub style: &'a AssStyle,
    /// Frame time, ms.
    pub time: i64,
    pub frame: Frame,

    pub font: Option<usize>,
    pub font_size: f64,
    parsed_tags: u8,
    pub flags: u8,
    pub alignment: i32,
    pub justify: i32,
    pub frx: f64,
    pub fry: f64,
    pub frz: f64,
    pub fax: f64,
    pub fay: f64,
    pub pos_x: f64,
    pub pos_y: f64,
    pub org_x: f64,
    pub org_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub hspacing: f64,
    pub border_x: f64,
    pub border_y: f64,
    pub evt_type: u8,
    pub border_style: i32,
    pub c: [u32; 4],
    pub clip_x0: i32,
    pub clip_y0: i32,
    pub clip_x1: i32,
    pub clip_y1: i32,
    pub have_origin: bool,
    /// `\iclip` (rectangle).
    pub clip_inverse: bool,
    pub detect_collisions: bool,
    pub be: i32,
    pub fade: i32,
    pub blur: f64,
    pub shadow_x: f64,
    pub shadow_y: f64,
    pub pbo: f64,
    /// Vector clip: drawing scale and commands, inverse flag.
    pub clip_drawing: Option<(i32, String)>,
    pub clip_drawing_inverse: bool,
    pub drawing_scale: i32,

    pub effect_type: Effect,
    pub effect_timing: i32,
    pub effect_skip_timing: i32,
    pub reset_effect: bool,

    pub scroll_direction: Scroll,
    pub scroll_shift: f64,
    pub scroll_y0: i32,
    pub scroll_y1: i32,

    pub family: String,
    pub bold: i32,
    pub italic: i32,
    pub wrap_style: i32,
    pub font_encoding: i32,

    pub apply_font_scale: bool,
    pub explicit: bool,

    pub screen_scale_x: f64,
    pub screen_scale_y: f64,
    pub border_scale_x: f64,
    pub border_scale_y: f64,
    pub blur_scale_x: f64,
    pub blur_scale_y: f64,
}

impl<'a> State<'a> {
    /// libass `init_render_context`.
    pub fn new(
        script: &'a AssScript,
        event: &'a AssEvent,
        time: i64,
        frame: Frame,
        db: &FontDb,
        fonts: &mut FontCache,
    ) -> Self {
        let style = &script.styles[event.style];
        let mut s = State {
            script,
            event,
            style,
            time,
            frame,
            font: None,
            font_size: 0.0,
            parsed_tags: 0,
            flags: 0,
            alignment: 0,
            justify: 0,
            frx: 0.0,
            fry: 0.0,
            frz: 0.0,
            fax: 0.0,
            fay: 0.0,
            pos_x: 0.0,
            pos_y: 0.0,
            org_x: 0.0,
            org_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            hspacing: 0.0,
            border_x: 0.0,
            border_y: 0.0,
            evt_type: 0,
            border_style: 1,
            c: [0; 4],
            clip_x0: 0,
            clip_y0: 0,
            clip_x1: script.play_res_x,
            clip_y1: script.play_res_y,
            have_origin: false,
            clip_inverse: false,
            detect_collisions: true,
            be: 0,
            fade: 0,
            blur: 0.0,
            shadow_x: 0.0,
            shadow_y: 0.0,
            pbo: 0.0,
            clip_drawing: None,
            clip_drawing_inverse: false,
            drawing_scale: 0,
            effect_type: Effect::None,
            effect_timing: 0,
            effect_skip_timing: 0,
            reset_effect: false,
            scroll_direction: Scroll::RightToLeft,
            scroll_shift: 0.0,
            scroll_y0: 0,
            scroll_y1: 0,
            family: String::new(),
            bold: 0,
            italic: 0,
            wrap_style: script.wrap_style,
            font_encoding: 0,
            apply_font_scale: true,
            explicit: false,
            screen_scale_x: 1.0,
            screen_scale_y: 1.0,
            border_scale_x: 1.0,
            border_scale_y: 1.0,
            blur_scale_x: 1.0,
            blur_scale_y: 1.0,
        };
        s.apply_transition_effects();
        s.explicit = s.evt_type != 0 || has_hard_overrides(&event.text);
        s.reset(None, db, fonts);
        s.alignment = s.style.alignment;
        s.justify = 0;
        s
    }

    /// libass `init_font_scale`.
    fn init_font_scale(&mut self) {
        let (w, h) = (self.frame.width as f64, self.frame.height as f64);
        self.screen_scale_x = w / self.script.play_res_x as f64;
        self.screen_scale_y = h / self.script.play_res_y as f64;
        let (lx, ly) = layout_res(self.script, &self.frame);
        self.blur_scale_x = w / lx as f64;
        self.blur_scale_y = h / ly as f64;
        if self.script.scaled_border_and_shadow {
            self.border_scale_x = self.screen_scale_x;
            self.border_scale_y = self.screen_scale_y;
        } else {
            self.border_scale_x = self.blur_scale_x;
            self.border_scale_y = self.blur_scale_y;
        }
        if self.apply_font_scale {
            let k = self.frame.font_size_coeff;
            self.screen_scale_x *= k;
            self.screen_scale_y *= k;
            self.border_scale_x *= k;
            self.border_scale_y *= k;
            self.blur_scale_x *= k;
            self.blur_scale_y *= k;
        }
    }

    /// libass `ass_reset_render_context`: back to `style` (a `\r` target)
    /// or the event's style.
    pub fn reset(&mut self, style: Option<usize>, db: &FontDb, fonts: &mut FontCache) {
        self.style = &self.script.styles[style.unwrap_or(self.event.style)];
        // No user style overrides: only the font scale is selective.
        self.apply_font_scale = !self.explicit;
        self.init_font_scale();
        let st = self.style;
        self.c = [st.primary_colour, st.secondary_colour, st.outline_colour, st.back_colour];
        self.flags = (if st.underline { DECO_UNDERLINE } else { 0 }) | (if st.strike_out { DECO_STRIKETHROUGH } else { 0 });
        self.font_size = st.font_size;
        self.family = st.font_name.clone();
        self.bold = st.bold as i32;
        self.italic = st.italic as i32;
        self.update_font(db, fonts);
        self.border_style = st.border_style;
        self.border_x = st.outline;
        self.border_y = st.outline;
        self.scale_x = st.scale_x;
        self.scale_y = st.scale_y;
        self.hspacing = st.spacing;
        self.be = 0;
        self.blur = 0.0;
        self.shadow_x = st.shadow;
        self.shadow_y = st.shadow;
        self.frx = 0.0;
        self.fry = 0.0;
        self.frz = st.angle;
        self.fax = 0.0;
        self.fay = 0.0;
        self.font_encoding = st.encoding;
    }

    pub fn update_font(&mut self, db: &FontDb, fonts: &mut FontCache) {
        self.font = fonts.get(db, FontDesc::new(&self.family, self.bold, self.italic));
    }


    /// libass `ass_apply_transition_effects` (`Banner;`, `Scroll up;`,
    /// `Scroll down;`).
    fn apply_transition_effects(&mut self) {
        let effect = self.event.effect.as_str();
        if effect.is_empty() {
            return;
        }
        let mut v = [0i32; 4];
        let mut cnt = 0;
        let mut rest = effect;
        while cnt < 4 {
            let Some(i) = rest.find(';') else { break };
            rest = &rest[i + 1..];
            v[cnt] = atoi(rest);
            cnt += 1;
        }
        let (lx, ly) = layout_res(self.script, &self.frame);
        let elapsed = (self.time - self.event.start_ms) as f64;
        if effect.starts_with("Banner;") {
            if cnt < 1 {
                return;
            }
            self.scroll_direction = if cnt >= 2 && v[1] != 0 { Scroll::LeftToRight } else { Scroll::RightToLeft };
            let scale_x = lx as f64 / self.script.play_res_x as f64;
            let delay = ((v[0] as f64 / scale_x).max(1.0) as i32) as f64 * scale_x;
            self.scroll_shift = elapsed / delay;
            self.evt_type |= EVENT_HSCROLL;
            self.detect_collisions = false;
            self.wrap_style = 2;
            return;
        }
        if effect.starts_with("Scroll up;") {
            self.scroll_direction = Scroll::BottomToTop;
        } else if effect.starts_with("Scroll down;") {
            self.scroll_direction = Scroll::TopToBottom;
        } else {
            return;
        }
        if cnt < 3 {
            return;
        }
        let scale_y = ly as f64 / self.script.play_res_y as f64;
        let delay = ((v[2] as f64 / scale_y).max(1.0) as i32) as f64 * scale_y;
        self.scroll_shift = elapsed / delay;
        let (y0, y1) = if v[0] < v[1] { (v[0], v[1]) } else { (v[1], v[0]) };
        self.scroll_y0 = y0;
        self.scroll_y1 = y1;
        self.evt_type |= EVENT_VSCROLL;
        self.detect_collisions = false;
    }

    fn elapsed(&self) -> i64 {
        self.time - self.event.start_ms
    }

    /// The semantic half of libass `ass_parse_tags`: apply one override
    /// block's tags at animation power `pwr`.
    pub fn apply_tags(&mut self, tags: &[Tag], mut pwr: f64, nested: bool, db: &FontDb, fonts: &mut FontCache) {
        let duration = self.event.duration_ms as i32;
        for tag in tags {
            match tag {
                Tag::BorderX(v) => {
                    self.border_x = match v {
                        Some(v) => (self.border_x * (1.0 - pwr) + v * pwr).max(0.0),
                        None => self.style.outline,
                    }
                }
                Tag::BorderY(v) => {
                    self.border_y = match v {
                        Some(v) => (self.border_y * (1.0 - pwr) + v * pwr).max(0.0),
                        None => self.style.outline,
                    }
                }
                Tag::ShadowX(v) => {
                    self.shadow_x = match v {
                        Some(v) => self.shadow_x * (1.0 - pwr) + v * pwr,
                        None => self.style.shadow,
                    }
                }
                Tag::ShadowY(v) => {
                    self.shadow_y = match v {
                        Some(v) => self.shadow_y * (1.0 - pwr) + v * pwr,
                        None => self.style.shadow,
                    }
                }
                Tag::ShearX(v) => self.fax = v.map_or(0.0, |v| v * pwr + self.fax * (1.0 - pwr)),
                Tag::ShearY(v) => self.fay = v.map_or(0.0, |v| v * pwr + self.fay * (1.0 - pwr)),
                Tag::Clip { inverse, shape } => match shape {
                    ClipShape::Rect { x0, y0, x1, y1 } => {
                        let anim = |old: i32, new: i32| (old as f64 * (1.0 - pwr) + new as f64 * pwr) as i32;
                        self.clip_x0 = anim(self.clip_x0, *x0);
                        self.clip_x1 = anim(self.clip_x1, *x1);
                        self.clip_y0 = anim(self.clip_y0, *y0);
                        self.clip_y1 = anim(self.clip_y1, *y1);
                        self.clip_inverse = *inverse;
                    }
                    ClipShape::Drawing { scale, commands } => {
                        if self.clip_drawing.is_none() {
                            self.clip_drawing = Some((*scale, commands.clone()));
                            self.clip_drawing_inverse = *inverse;
                        }
                    }
                },
                Tag::Blur(v) => {
                    self.blur = match v {
                        Some(v) => (self.blur * (1.0 - pwr) + v * pwr).clamp(0.0, BLUR_MAX_RADIUS),
                        None => 0.0,
                    }
                }
                Tag::ScaleX(v) => {
                    self.scale_x = match v {
                        Some(v) => (self.scale_x * (1.0 - pwr) + v * pwr).max(0.0),
                        None => self.style.scale_x,
                    }
                }
                Tag::ScaleY(v) => {
                    self.scale_y = match v {
                        Some(v) => (self.scale_y * (1.0 - pwr) + v * pwr).max(0.0),
                        None => self.style.scale_y,
                    }
                }
                Tag::ScaleReset => {
                    self.scale_x = self.style.scale_x;
                    self.scale_y = self.style.scale_y;
                }
                Tag::Spacing(v) => {
                    self.hspacing = match v {
                        Some(v) => self.hspacing * (1.0 - pwr) + v * pwr,
                        None => self.style.spacing,
                    }
                }
                Tag::FontSize(v) => {
                    let mut val = match v {
                        Some(FontSize::Relative(d)) => self.font_size * (1.0 + pwr * d / 10.0),
                        Some(FontSize::Absolute(a)) => self.font_size * (1.0 - pwr) + a * pwr,
                        None => 0.0,
                    };
                    if val <= 0.0 {
                        val = self.style.font_size;
                    }
                    self.font_size = val;
                }
                Tag::Border(v) => {
                    let (x, y) = match v {
                        Some(v) => (
                            (self.border_x * (1.0 - pwr) + v * pwr).max(0.0),
                            (self.border_y * (1.0 - pwr) + v * pwr).max(0.0),
                        ),
                        None => (self.style.outline, self.style.outline),
                    };
                    self.border_x = x;
                    self.border_y = y;
                }
                Tag::Move { x1, y1, x2, y2, t1, t2 } => {
                    let (mut t1, mut t2) = (*t1, *t2);
                    if t1 <= 0 && t2 <= 0 {
                        t1 = 0;
                        t2 = duration;
                    }
                    let delta = t2.wrapping_sub(t1);
                    let t = self.elapsed() as i32;
                    let k = if t <= t1 {
                        0.0
                    } else if t >= t2 {
                        1.0
                    } else {
                        t.wrapping_sub(t1) as f64 / delta as f64
                    };
                    if self.evt_type & EVENT_POSITIONED == 0 {
                        self.pos_x = k * (x2 - x1) + x1;
                        self.pos_y = k * (y2 - y1) + y1;
                        self.detect_collisions = false;
                        self.evt_type |= EVENT_POSITIONED;
                    }
                }
                Tag::RotateX(v) => self.frx = v.map_or(0.0, |v| v * pwr + self.frx * (1.0 - pwr)),
                Tag::RotateY(v) => self.fry = v.map_or(0.0, |v| v * pwr + self.fry * (1.0 - pwr)),
                Tag::RotateZ(v) => self.frz = v.map_or(self.style.angle, |v| v * pwr + self.frz * (1.0 - pwr)),
                Tag::FontName(name) => {
                    self.family = name.clone().unwrap_or_else(|| self.style.font_name.clone());
                    self.update_font(db, fonts);
                }
                Tag::Alpha(v) => match v {
                    Some(a) => {
                        for c in &mut self.c {
                            change_alpha(c, *a, pwr);
                        }
                    }
                    None => {
                        let st = self.style;
                        let src = [st.primary_colour, st.secondary_colour, st.outline_colour, st.back_colour];
                        for (c, s) in self.c.iter_mut().zip(src) {
                            change_alpha(c, alpha_of(s) as i32, 1.0);
                        }
                    }
                },
                Tag::AlignNumpad(v) => {
                    if self.parsed_tags & PARSED_A == 0 {
                        self.alignment = if (1..=9).contains(v) { numpad2align(*v) } else { self.style.alignment };
                        self.parsed_tags |= PARSED_A;
                    }
                }
                Tag::AlignLegacy(v) => {
                    if self.parsed_tags & PARSED_A == 0 {
                        // VSFilter: the illegal \a4 / \a8 read as \a5.
                        self.alignment = if (1..=11).contains(v) {
                            if v & 3 == 0 {
                                5
                            } else {
                                *v
                            }
                        } else {
                            self.style.alignment
                        };
                        self.parsed_tags |= PARSED_A;
                    }
                }
                Tag::Pos { x, y } => {
                    if self.evt_type & EVENT_POSITIONED == 0 {
                        self.evt_type |= EVENT_POSITIONED;
                        self.detect_collisions = false;
                        self.pos_x = *x;
                        self.pos_y = *y;
                    }
                }
                Tag::Fade { a1, a2, a3, t1, t2, t3, t4 } => {
                    let (mut t1, mut t3, mut t4) = (*t1, *t3, *t4);
                    if t1 == -1 && t4 == -1 {
                        t1 = 0;
                        t4 = duration;
                        t3 = t4.wrapping_sub(t3);
                    }
                    if self.parsed_tags & PARSED_FADE == 0 {
                        self.fade = interpolate_alpha(self.elapsed(), t1, *t2, t3, t4, *a1, *a2, *a3);
                        self.parsed_tags |= PARSED_FADE;
                    }
                }
                Tag::Origin { x, y } => {
                    if !self.have_origin {
                        self.org_x = *x;
                        self.org_y = *y;
                        self.have_origin = true;
                        self.detect_collisions = false;
                    }
                }
                Tag::Transform { t1, t2, accel, tags } => {
                    self.detect_collisions = false;
                    let t1 = *t1;
                    let t2 = if *t2 == 0 { duration } else { *t2 };
                    let delta = t2.wrapping_sub(t1);
                    let t = self.elapsed() as i32;
                    let k = if t < t1 {
                        0.0
                    } else if t >= t2 {
                        1.0
                    } else {
                        (t.wrapping_sub(t1) as f64 / delta as f64).powf(*accel)
                    };
                    if nested {
                        pwr = k;
                    }
                    if !tags.is_empty() {
                        self.apply_tags(tags, k, true, db, fonts);
                    }
                }
                Tag::Colour { index, value } => match value {
                    Some(v) => change_color(&mut self.c[*index], *v, pwr),
                    None => {
                        let st = self.style;
                        let src = [st.primary_colour, st.secondary_colour, st.outline_colour, st.back_colour];
                        change_color(&mut self.c[*index], src[*index], 1.0);
                    }
                },
                Tag::ColourAlpha { index, value } => match value {
                    Some(a) => change_alpha(&mut self.c[*index], *a, pwr),
                    None => {
                        let st = self.style;
                        let src = [st.primary_colour, st.secondary_colour, st.outline_colour, st.back_colour];
                        change_alpha(&mut self.c[*index], alpha_of(src[*index]) as i32, 1.0);
                    }
                },
                Tag::Reset(name) => {
                    let target = name.as_deref().and_then(|n| self.script.lookup_style_strict(n));
                    self.reset(target, db, fonts);
                }
                Tag::BlurEdges(v) => {
                    self.be = match v {
                        // VSFilter always adds +0.5, even if the value is negative
                        Some(v) => dtoi32(self.be as f64 * (1.0 - pwr) + v * pwr + 0.5).clamp(0, MAX_BE),
                        None => 0,
                    }
                }
                Tag::Bold(v) => {
                    self.bold = match v {
                        Some(v) if *v == 0 || *v == 1 || *v >= 100 => *v,
                        _ => self.style.bold as i32,
                    };
                    self.update_font(db, fonts);
                }
                Tag::Italic(v) => {
                    self.italic = match v {
                        Some(v) if *v == 0 || *v == 1 => *v,
                        _ => self.style.italic as i32,
                    };
                    self.update_font(db, fonts);
                }
                Tag::KaraokeStart(v) => {
                    self.effect_skip_timing = dtoi32(v.unwrap_or(0.0) * 10.0);
                    self.effect_timing = 0;
                    self.reset_effect = true;
                }
                Tag::Karaoke { kind, centis } => {
                    self.effect_type = match kind {
                        KaraokeKind::Fill => Effect::Karaoke,
                        KaraokeKind::Sweep => Effect::KaraokeKf,
                        KaraokeKind::Outline => Effect::KaraokeKo,
                    };
                    self.effect_skip_timing = (self.effect_skip_timing as u32).wrapping_add(self.effect_timing as u32) as i32;
                    self.effect_timing = dtoi32(centis.unwrap_or(100.0) * 10.0);
                }
                Tag::Shadow(v) => {
                    let (x, y) = match v {
                        // VSFilter: \shad clips at 0, \xshad / \yshad don't.
                        Some(v) => (
                            (self.shadow_x * (1.0 - pwr) + v * pwr).max(0.0),
                            (self.shadow_y * (1.0 - pwr) + v * pwr).max(0.0),
                        ),
                        None => (self.style.shadow, self.style.shadow),
                    };
                    self.shadow_x = x;
                    self.shadow_y = y;
                }
                Tag::StrikeOut(v) => {
                    let on = match v {
                        Some(v) if *v == 0 || *v == 1 => *v == 1,
                        _ => self.style.strike_out,
                    };
                    self.set_flag(DECO_STRIKETHROUGH, on);
                }
                Tag::Underline(v) => {
                    let on = match v {
                        Some(v) if *v == 0 || *v == 1 => *v == 1,
                        _ => self.style.underline,
                    };
                    self.set_flag(DECO_UNDERLINE, on);
                }
                Tag::DrawingBaselineOffset(v) => self.pbo = *v,
                Tag::Drawing(v) => self.drawing_scale = *v,
                Tag::WrapStyle(v) => {
                    self.wrap_style = match v {
                        Some(v) if (0..=3).contains(v) => *v,
                        _ => self.script.wrap_style,
                    }
                }
                Tag::Encoding(v) => self.font_encoding = v.unwrap_or(self.style.encoding),
            }
        }
    }

    fn set_flag(&mut self, flag: u8, on: bool) {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}

/// C `atoi`: optional whitespace and sign, then digits.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        v = (v * 10 + (b - b'0') as i64).min(i32::MAX as i64 + 1);
    }
    (if neg { -v } else { v }).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_and_alpha_animation() {
        let mut c = 0x0000_0000;
        change_color(&mut c, 0xFF80_4000, 0.5);
        assert_eq!(c, 0x7F40_2000);
        let mut c = 0xFFFF_FF00;
        change_alpha(&mut c, 0xFF, 0.5);
        assert_eq!(c & 0xFF, 0x7F);
    }

    #[test]
    fn fade_interpolation_and_application() {
        // \fad(200,300) on a 1000 ms event: t1=0 t2=200 t3=700 t4=1000.
        assert_eq!(interpolate_alpha(0, 0, 200, 700, 1000, 255, 0, 255), 255);
        assert_eq!(interpolate_alpha(100, 0, 200, 700, 1000, 255, 0, 255), 127);
        assert_eq!(interpolate_alpha(500, 0, 200, 700, 1000, 255, 0, 255), 0);
        assert_eq!(interpolate_alpha(1000, 0, 200, 700, 1000, 255, 0, 255), 255);
        let mut c = 0xFFFF_FF00;
        apply_fade(&mut c, 255);
        assert_eq!(c & 0xFF, 0xFF);
        let mut c = 0xFFFF_FF80;
        apply_fade(&mut c, 0);
        assert_eq!(c & 0xFF, 0x80);
    }

    #[test]
    fn hard_overrides_are_found_in_blocks_only() {
        assert!(has_hard_overrides(r"{\pos(1,2)}x"));
        assert!(has_hard_overrides(r"{\t(\p1)}x"));
        assert!(!has_hard_overrides(r"\pos(1,2) {\b1}x"));
        assert!(!has_hard_overrides(r"{\an8}x"));
    }
}
