//! Advanced SubStation Alpha (`.ass`, v4.00+) and SubStation Alpha v4
//! (`.ssa`) scripts.
//!
//! ASS has no real specification: what a script means is what VSFilter
//! did with it, and libass is the maintained reference of that behaviour.
//! This parser is a port of the libass script reader (libass/ass.c) —
//! section handling, `Format:` lines, the built-in `Default` style, the
//! SSA conversions, the `ScaledBorderAndShadow` defaulting rules and the
//! `PlayResX`/`PlayResY` fallbacks all match it — so a script renders
//! from the same data libass would render it from. Override tags inside
//! event text are tokenized by [`tags`].
//!
//! libass is ISC-licensed: Copyright (C) 2006-2016 libass contributors.
//! Permission to use, copy, modify, and/or distribute this software for
//! any purpose with or without fee is hereby granted, provided that the
//! above copyright notice and this permission notice appear in all copies.
//!
//! One deliberate deviation: libass keeps the previous section's state
//! across a section header it doesn't know, so the lines of a
//! `[Graphics]` section that follows `[Fonts]` get appended to the last
//! font's data and corrupt it. Here an unknown header after `[Fonts]`
//! closes the font section.

mod fonts;
mod num;
pub mod plain;
pub mod tags;

pub use fonts::EmbeddedFont;
pub use tags::{text_chars, ClipShape, FontSize, Item, KaraokeKind, Tag};

use num::{parse_bool, parse_color_header, parse_int_header, strtod};

/// libass alignment bits (`ass_types.h`): vertical part …
pub const VALIGN_SUB: i32 = 0;
pub const VALIGN_TOP: i32 = 4;
pub const VALIGN_CENTER: i32 = 8;
/// … and horizontal part (`alignment & 3`).
pub const HALIGN_LEFT: i32 = 1;
pub const HALIGN_CENTER: i32 = 2;
pub const HALIGN_RIGHT: i32 = 3;

/// Numpad alignment (`\an`, ASS style `Alignment`) → libass alignment bits.
pub fn numpad2align(val: i32) -> i32 {
    let val = if val < -i32::MAX {
        2
    } else if val < 0 {
        -val
    } else {
        val
    };
    let mut res = ((val - 1) % 3) + 1;
    if val <= 3 {
        res |= VALIGN_SUB;
    } else if val <= 6 {
        res |= VALIGN_CENTER;
    } else {
        res |= VALIGN_TOP;
    }
    res
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptKind {
    /// SubStation Alpha v4 (`[V4 Styles]`, `ScriptType: v4.00`).
    Ssa,
    /// Advanced SubStation Alpha (`[V4+ Styles]`, `ScriptType: v4.00+`).
    Ass,
}

/// `YCbCr Matrix:` header — which matrix the script's colours were picked
/// under. VSFilter-compatible renderers use it to correct colours when the
/// video's matrix differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum YCbCrMatrix {
    /// No header.
    #[default]
    Default,
    /// `None`: colours are taken as RGB, no correction.
    None,
    Bt601Tv,
    Bt601Pc,
    Bt709Tv,
    Bt709Pc,
    Smpte240mTv,
    Smpte240mPc,
    FccTv,
    FccPc,
    /// A value libass doesn't recognise either.
    Unknown,
}

fn parse_ycbcr_matrix(s: &str) -> YCbCrMatrix {
    let s = s.trim_matches([' ', '\t']);
    if s.is_empty() {
        return YCbCrMatrix::Default;
    }
    match s.to_ascii_lowercase().as_str() {
        "none" => YCbCrMatrix::None,
        "tv.601" => YCbCrMatrix::Bt601Tv,
        "pc.601" => YCbCrMatrix::Bt601Pc,
        "tv.709" => YCbCrMatrix::Bt709Tv,
        "pc.709" => YCbCrMatrix::Bt709Pc,
        "tv.240m" => YCbCrMatrix::Smpte240mTv,
        "pc.240m" => YCbCrMatrix::Smpte240mPc,
        "tv.fcc" => YCbCrMatrix::FccTv,
        "pc.fcc" => YCbCrMatrix::FccPc,
        _ => YCbCrMatrix::Unknown,
    }
}

/// One `Style:` line. Colours are libass RGBA, `0xRRGGBBAA`, where the
/// alpha byte is ASS transparency: 0x00 opaque, 0xFF invisible.
#[derive(Clone, Debug, PartialEq)]
pub struct AssStyle {
    pub name: String,
    pub font_name: String,
    pub font_size: f64,
    pub primary_colour: u32,
    pub secondary_colour: u32,
    pub outline_colour: u32,
    pub back_colour: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike_out: bool,
    /// `ScaleX`/`ScaleY` as fractions (100 % → 1.0).
    pub scale_x: f64,
    pub scale_y: f64,
    pub spacing: f64,
    /// `Angle`, degrees (`\frz` default).
    pub angle: f64,
    /// 1 = outline + shadow, 3 = opaque box (4 is a libass extension).
    pub border_style: i32,
    pub outline: f64,
    pub shadow: f64,
    /// libass alignment bits (`VALIGN_* | HALIGN_*`), already converted
    /// from numpad (ASS) or legacy (SSA) numbering.
    pub alignment: i32,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    pub encoding: i32,
}

impl AssStyle {
    /// libass `set_default_style`: the style a script falls back to when it
    /// names one that doesn't exist and has no `Default` of its own.
    fn builtin_default() -> Self {
        Self {
            name: "Default".into(),
            font_name: "Arial".into(),
            font_size: 18.0,
            primary_colour: 0xffff_ff00,
            secondary_colour: 0x00ff_ff00,
            outline_colour: 0x0000_0000,
            back_colour: 0x0000_0080,
            // libass writes 200 here; it only ever reads it as a boolean
            // (`\b` with no argument), so 200 and `true` are the same.
            bold: true,
            italic: false,
            underline: false,
            strike_out: false,
            scale_x: 1.0,
            scale_y: 1.0,
            spacing: 0.0,
            angle: 0.0,
            border_style: 1,
            outline: 2.0,
            shadow: 3.0,
            alignment: 2,
            margin_l: 20,
            margin_r: 20,
            margin_v: 20,
            encoding: 0,
        }
    }

    /// What `process_style` starts each new style from (calloc + 100 %
    /// scale).
    fn blank() -> Self {
        Self {
            name: String::new(),
            font_name: String::new(),
            font_size: 0.0,
            primary_colour: 0,
            secondary_colour: 0,
            outline_colour: 0,
            back_colour: 0,
            bold: false,
            italic: false,
            underline: false,
            strike_out: false,
            scale_x: 100.0,
            scale_y: 100.0,
            spacing: 0.0,
            angle: 0.0,
            border_style: 0,
            outline: 0.0,
            shadow: 0.0,
            alignment: 0,
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
            encoding: 0,
        }
    }
}

/// One `Dialogue:` line.
#[derive(Clone, Debug, PartialEq)]
pub struct AssEvent {
    /// Position in the file. Renderers break ties between events of the
    /// same layer by it (and libass uses it to stack colliding events).
    pub read_order: usize,
    pub layer: i32,
    pub start_ms: i64,
    /// End minus start; zero or negative events are never shown.
    pub duration_ms: i64,
    /// Index into [`AssScript::styles`].
    pub style: usize,
    /// `Name` / `Actor`.
    pub name: String,
    /// Event margins; 0 means "use the style's".
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    /// `Effect` (`Banner;…`, `Scroll up;…` — legacy SSA effects).
    pub effect: String,
    /// The raw text, override blocks included.
    pub text: String,
    /// `text` tokenized.
    pub items: Vec<Item>,
}

impl AssEvent {
    pub fn end_ms(&self) -> i64 {
        self.start_ms + self.duration_ms
    }
}

/// A parsed script.
#[derive(Clone, Debug, PartialEq)]
pub struct AssScript {
    pub kind: ScriptKind,
    /// The script's coordinate space. Always positive: missing values are
    /// filled in the way libass does (384×288, or derived from the other).
    pub play_res_x: i32,
    pub play_res_y: i32,
    /// `LayoutResX/Y` (0 = absent): the resolution the script was laid out
    /// at, which corrects scaling on anamorphic video.
    pub layout_res_x: i32,
    pub layout_res_y: i32,
    pub timer: f64,
    pub wrap_style: i32,
    pub scaled_border_and_shadow: bool,
    pub kerning: bool,
    pub ycbcr_matrix: YCbCrMatrix,
    pub language: Option<String>,
    /// `styles[0]` is libass's built-in `Default`; the script's own follow
    /// in file order.
    pub styles: Vec<AssStyle>,
    /// The style events fall back to: the last one named exactly
    /// `Default`, else the built-in.
    pub default_style: usize,
    /// In file order.
    pub events: Vec<AssEvent>,
    pub fonts: Vec<EmbeddedFont>,
}

impl AssScript {
    /// libass `ass_lookup_style`: leading `*` ignored, `Default` matched
    /// case-insensitively, otherwise an exact match, the last definition
    /// winning; the default style when nothing matches.
    pub fn lookup_style(&self, name: &str) -> usize {
        let name = name.trim_start_matches('*');
        let name = if name.eq_ignore_ascii_case("Default") { "Default" } else { name };
        self.styles
            .iter()
            .rposition(|s| s.name == name)
            .unwrap_or(self.default_style)
    }

    /// The style `\r<name>` switches to: exact name, no fallback.
    pub fn lookup_style_strict(&self, name: &str) -> Option<usize> {
        self.styles.iter().rposition(|s| s.name == name)
    }
}

/// The link from a [`crate::parsers::vtt::VttCue`] back to the ASS event it
/// was made from, for renderers that draw the event itself rather than the
/// cue's plain text.
#[derive(Clone)]
pub struct AssCue {
    pub script: std::sync::Arc<AssScript>,
    /// Index into `script.events`.
    pub event: usize,
}

impl AssCue {
    pub fn event(&self) -> &AssEvent {
        &self.script.events[self.event]
    }
}

impl std::fmt::Debug for AssCue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The script is shared by every cue; printing it per cue would bury
        // the cue list.
        f.debug_struct("AssCue").field("event", &self.event).finish()
    }
}

const ASS_STYLE_FORMAT: &str = "Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
    OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, \
    ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, \
    Alignment, MarginL, MarginR, MarginV, Encoding";
const ASS_EVENT_FORMAT: &str = "Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";
const SSA_STYLE_FORMAT: &str = "Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
    TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, \
    Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding";
const SSA_EVENT_FORMAT: &str = "Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Info,
    Styles,
    Events,
    Fonts,
}

/// `[Script Info]` headers seen, for the `ScaledBorderAndShadow` rules
/// (libass `ScriptInfo` flags).
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct Seen {
    script_type: bool,
    play_res_x: bool,
    play_res_y: bool,
    scaled_border: bool,
    /// `; Script generated by FFmpeg/Lavc` comment.
    ffmpeg: bool,
    /// Any other header libass flags (LayoutRes, Timer, WrapStyle,
    /// Kerning, YCbCr Matrix, Language).
    other: bool,
}

struct Parser {
    kind: Option<ScriptKind>,
    section: Section,
    style_format: Option<String>,
    event_format: Option<String>,
    seen: Seen,
    script: AssScript,
    font_name: Option<String>,
    font_data: Vec<u8>,
}

/// Parse a decoded script. `None` when it is not an SSA/ASS script at all
/// (no `ScriptType` and no styles section — libass rejects those too).
/// Events come back in file order; most renderers want them sorted by
/// start time.
pub fn parse(body: &str) -> Option<AssScript> {
    let mut p = Parser {
        kind: None,
        section: Section::None,
        style_format: None,
        event_format: None,
        seen: Seen::default(),
        script: AssScript {
            kind: ScriptKind::Ass,
            play_res_x: 0,
            play_res_y: 0,
            layout_res_x: 0,
            layout_res_y: 0,
            timer: 0.0,
            wrap_style: 0,
            scaled_border_and_shadow: false,
            kerning: false,
            ycbcr_matrix: YCbCrMatrix::Default,
            language: None,
            styles: vec![AssStyle::builtin_default()],
            default_style: 0,
            events: Vec::new(),
            fonts: Vec::new(),
        },
        font_name: None,
        font_data: Vec::new(),
    };
    for line in body.split(['\r', '\n']) {
        let line = line.trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        p.line(line);
    }
    // No end-of-font marker in the format: the last font ends with the file.
    p.finish_font();

    let kind = p.kind?;
    let mut script = p.script;
    script.kind = kind;
    for (i, e) in script.events.iter_mut().enumerate() {
        e.read_order = i;
    }
    lazy_play_res(&mut script);
    Some(script)
}

/// libass `ass_lazy_track_init`: fill in a missing PlayRes.
fn lazy_play_res(s: &mut AssScript) {
    if s.play_res_x > 0 && s.play_res_y > 0 {
        return;
    }
    if s.play_res_x <= 0 && s.play_res_y <= 0 {
        s.play_res_x = 384;
        s.play_res_y = 288;
    } else if s.play_res_y <= 0 && s.play_res_x == 1280 {
        s.play_res_y = 1024;
    } else if s.play_res_y <= 0 {
        let x = (s.play_res_x - 1) as u32;
        s.play_res_y = (x - x / 4).max(1) as i32;
    } else if s.play_res_x <= 0 && s.play_res_y == 1024 {
        s.play_res_x = 1280;
    } else if s.play_res_x <= 0 {
        let y = s.play_res_y as u32;
        s.play_res_x = (y + y / 3).min(i32::MAX as u32) as i32;
    }
}

/// libass `next_token`: skip leading spaces, cut at the next comma
/// (optionally right-trimming); `None` once nothing is left.
fn next_token<'a>(s: &mut &'a str, rtrim: bool) -> Option<&'a str> {
    let t = s.trim_start_matches([' ', '\t']);
    if t.is_empty() {
        *s = t;
        return None;
    }
    let (tok, rest) = match t.find(',') {
        Some(i) => (&t[..i], &t[i + 1..]),
        None => (t, ""),
    };
    *s = rest;
    Some(if rtrim { tok.trim_end_matches([' ', '\t']) } else { tok })
}

/// libass `string2timecode`: `H:MM:SS.cc` — the fraction is always read
/// as centiseconds, whatever its digit count (`.5` = 50 ms, `.123` =
/// 1230 ms), exactly as VSFilter's `sscanf` did. Malformed → 0.
fn parse_timecode(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut fields = [0i64; 4];
    for (n, sep) in [Some(b':'), Some(b':'), Some(b'.'), None].into_iter().enumerate() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let neg = i < b.len() && b[i] == b'-';
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        let start = i;
        let mut v: i64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            v = v.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
            i += 1;
        }
        if i == start {
            return 0;
        }
        fields[n] = if neg { -v } else { v };
        if let Some(sep) = sep {
            if b.get(i) != Some(&sep) {
                return 0;
            }
            i += 1;
        }
    }
    let [h, m, sec, cs] = fields;
    ((h * 60 + m) * 60 + sec) * 1000 + cs * 10
}

/// libass `format_line_compare`: same field list, case-insensitive,
/// `Actor` = `Name`.
fn format_line_matches(a: &str, b: &str) -> bool {
    let fields = |s: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(t) = next_token(&mut rest, true) {
            out.push(if t == "Actor" { "name".to_string() } else { t.to_ascii_lowercase() });
        }
        out
    };
    fields(a) == fields(b)
}

impl Parser {
    fn kind(&self) -> Option<ScriptKind> {
        self.kind
    }

    fn line(&mut self, line: &str) {
        let line = line.trim_start_matches([' ', '\t']);
        let header = |h: &str| line.len() >= h.len() && line.as_bytes()[..h.len()].eq_ignore_ascii_case(h.as_bytes());
        if header("[Script Info]") {
            self.enter(Section::Info);
        } else if header("[V4 Styles]") {
            self.enter(Section::Styles);
            self.kind = Some(ScriptKind::Ssa);
        } else if header("[V4+ Styles]") {
            self.enter(Section::Styles);
            self.kind = Some(ScriptKind::Ass);
        } else if header("[Events]") {
            self.enter(Section::Events);
        } else if header("[Fonts]") {
            self.enter(Section::Fonts);
        } else if line.starts_with('[') && self.section == Section::Fonts {
            // See the module docs: don't let `[Graphics]` data corrupt the
            // last font.
            self.enter(Section::None);
        } else {
            match self.section {
                Section::Info => self.info_line(line),
                Section::Styles => self.styles_line(line),
                Section::Events => self.events_line(line),
                Section::Fonts => self.fonts_line(line),
                Section::None => {}
            }
        }
    }

    fn enter(&mut self, section: Section) {
        if self.section == Section::Fonts && section != Section::Fonts {
            self.finish_font();
        }
        self.section = section;
    }

    fn info_line(&mut self, line: &str) {
        let s = &mut self.script;
        let seen = &mut self.seen;
        if let Some(v) = line.strip_prefix("PlayResX:") {
            seen.play_res_x = true;
            s.play_res_x = parse_int_header(v);
        } else if let Some(v) = line.strip_prefix("PlayResY:") {
            seen.play_res_y = true;
            s.play_res_y = parse_int_header(v);
        } else if let Some(v) = line.strip_prefix("LayoutResX:") {
            seen.other = true;
            s.layout_res_x = parse_int_header(v);
        } else if let Some(v) = line.strip_prefix("LayoutResY:") {
            seen.other = true;
            s.layout_res_y = parse_int_header(v);
        } else if let Some(v) = line.strip_prefix("Timer:") {
            seen.other = true;
            s.timer = strtod(v.as_bytes());
        } else if let Some(v) = line.strip_prefix("WrapStyle:") {
            seen.other = true;
            s.wrap_style = parse_int_header(v);
        } else if let Some(v) = line.strip_prefix("ScaledBorderAndShadow:") {
            seen.scaled_border = true;
            s.scaled_border_and_shadow = parse_bool(v);
        } else if let Some(v) = line.strip_prefix("Kerning:") {
            seen.other = true;
            s.kerning = parse_bool(v);
        } else if let Some(v) = line.strip_prefix("YCbCr Matrix:") {
            seen.other = true;
            s.ycbcr_matrix = parse_ycbcr_matrix(v);
        } else if let Some(v) = line.strip_prefix("Language:") {
            seen.other = true;
            let v = v.trim_start();
            s.language = Some(v.chars().take(2).collect());
        } else if let Some(v) = line.strip_prefix("ScriptType:") {
            seen.script_type = true;
            self.script_type(v);
        } else if let Some(v) = line.strip_prefix("; Script generated by ") {
            if v.starts_with("FFmpeg/Lavc") {
                seen.ffmpeg = true;
            }
        }
    }

    /// libass `parse_script_type`: the value ends in `4.00` (SSA) or
    /// `4.00+` (ASS); a leading `v` is not checked.
    fn script_type(&mut self, v: &str) {
        let v = v.trim_end_matches([' ', '\t']);
        let (v, kind) = match v.strip_suffix('+') {
            Some(rest) => (rest, ScriptKind::Ass),
            None => (v, ScriptKind::Ssa),
        };
        if v.ends_with("4.00") {
            self.kind = Some(kind);
        }
    }

    /// A non-standard `Format:` line turns `ScaledBorderAndShadow` on unless
    /// the script set it: libass defaulted it to yes for years and scripts
    /// with custom format lines were written against that.
    fn custom_format_compat(&mut self, fmt: &str, standard: &str) {
        if !self.seen.scaled_border && !format_line_matches(fmt, standard) {
            self.script.scaled_border_and_shadow = true;
        }
    }

    fn styles_line(&mut self, line: &str) {
        if let Some(fmt) = line.strip_prefix("Format:") {
            let fmt = fmt.trim_start_matches([' ', '\t']).to_string();
            let standard = if self.kind() == Some(ScriptKind::Ssa) { SSA_STYLE_FORMAT } else { ASS_STYLE_FORMAT };
            self.custom_format_compat(&fmt, standard);
            self.style_format = Some(fmt);
        } else if let Some(rest) = line.strip_prefix("Style:") {
            self.style(rest.trim_start_matches([' ', '\t']));
        }
    }

    /// libass `process_style`.
    fn style(&mut self, line: &str) {
        let ssa = self.kind() == Some(ScriptKind::Ssa);
        let format = self
            .style_format
            .clone()
            .unwrap_or_else(|| if ssa { SSA_STYLE_FORMAT } else { ASS_STYLE_FORMAT }.to_string());
        let mut st = AssStyle::blank();
        let mut have_name = false;
        let mut have_font = false;
        let mut ssa_alpha = 0;
        let (mut bold, mut italic, mut underline, mut strike) = (0, 0, 0, 0);
        let mut fmt = format.as_str();
        let mut vals = line;
        loop {
            let Some(name) = next_token(&mut fmt, true) else { break };
            let Some(tok) = next_token(&mut vals, false) else { break };
            match name.to_ascii_lowercase().as_str() {
                "name" => {
                    st.name = tok.trim_start_matches('*').to_string();
                    have_name = true;
                }
                "fontname" => {
                    st.font_name = tok.to_string();
                    have_font = true;
                }
                "primarycolour" => st.primary_colour = parse_color_header(tok),
                "secondarycolour" => st.secondary_colour = parse_color_header(tok),
                "outlinecolour" => st.outline_colour = parse_color_header(tok),
                "backcolour" => {
                    st.back_colour = parse_color_header(tok);
                    // SSA draws outline and shadow in BackColour.
                    if ssa {
                        st.outline_colour = st.back_colour;
                    }
                }
                "alphalevel" => ssa_alpha = parse_int_header(tok),
                "fontsize" => st.font_size = strtod(tok.as_bytes()),
                "bold" => bold = parse_int_header(tok),
                "italic" => italic = parse_int_header(tok),
                "underline" => underline = parse_int_header(tok),
                "strikeout" => strike = parse_int_header(tok),
                "spacing" => st.spacing = strtod(tok.as_bytes()),
                "angle" => st.angle = strtod(tok.as_bytes()),
                "borderstyle" => st.border_style = parse_int_header(tok),
                "alignment" => {
                    let a = parse_int_header(tok);
                    st.alignment = if !ssa {
                        numpad2align(a)
                    } else if a == 8 {
                        // VSFilter compatibility.
                        3
                    } else if a == 4 {
                        11
                    } else {
                        a
                    };
                }
                "marginl" => st.margin_l = parse_int_header(tok),
                "marginr" => st.margin_r = parse_int_header(tok),
                "marginv" => st.margin_v = parse_int_header(tok),
                "encoding" => st.encoding = parse_int_header(tok),
                "scalex" => st.scale_x = strtod(tok.as_bytes()),
                "scaley" => st.scale_y = strtod(tok.as_bytes()),
                "outline" => st.outline = strtod(tok.as_bytes()),
                "shadow" => st.shadow = strtod(tok.as_bytes()),
                _ => {}
            }
        }
        if ssa {
            // VSFilter: SSA BackColour alpha is always 0x80.
            let front = ssa_alpha.clamp(0, 0xFF) as u32;
            st.primary_colour = (st.primary_colour & 0xFFFF_FF00) | front;
            st.secondary_colour = (st.secondary_colour & 0xFFFF_FF00) | front;
            st.outline_colour = (st.outline_colour & 0xFFFF_FF00) | front;
            st.back_colour = (st.back_colour & 0xFFFF_FF00) | 0x80;
        }
        st.scale_x = st.scale_x.max(0.0) / 100.0;
        st.scale_y = st.scale_y.max(0.0) / 100.0;
        st.spacing = st.spacing.max(0.0);
        st.outline = st.outline.max(0.0);
        st.shadow = st.shadow.max(0.0);
        st.bold = bold != 0;
        st.italic = italic != 0;
        st.underline = underline != 0;
        st.strike_out = strike != 0;
        if !have_name || st.name.is_empty() {
            st.name = "Default".into();
        }
        if !have_font {
            st.font_name = "Arial".into();
        }
        let is_default = st.name == "Default";
        self.script.styles.push(st);
        if is_default {
            self.script.default_style = self.script.styles.len() - 1;
        }
    }

    fn events_line(&mut self, line: &str) {
        if let Some(fmt) = line.strip_prefix("Format:") {
            let fmt = fmt.trim_start_matches([' ', '\t']).to_string();
            let standard = if self.kind() == Some(ScriptKind::Ssa) { SSA_EVENT_FORMAT } else { ASS_EVENT_FORMAT };
            self.custom_format_compat(&fmt, standard);
            self.event_format = Some(fmt);
            if self.legacy_ffmpeg_conversion() {
                self.script.scaled_border_and_shadow = true;
            }
        } else if let Some(rest) = line.strip_prefix("Dialogue:") {
            if self.event_format.is_none() {
                let ssa = self.kind() == Some(ScriptKind::Ssa);
                self.event_format = Some(if ssa { SSA_EVENT_FORMAT } else { ASS_EVENT_FORMAT }.to_string());
            }
            if let Some(event) = self.event(rest.trim_start_matches([' ', '\t'])) {
                self.script.events.push(event);
            }
        }
        // `Comment:` and anything else: ignored.
    }

    /// libass `detect_legacy_conv_subs`: SRT→ASS conversions by FFmpeg
    /// between 2014-10 and 2020-04 expected scaled borders without saying
    /// so. Recognised by their exact header set and single `Default` style.
    fn legacy_ffmpeg_conversion(&self) -> bool {
        let s = self.seen;
        s.script_type
            && s.play_res_x
            && s.play_res_y
            && s.ffmpeg
            && !s.scaled_border
            && !s.other
            && self.script.styles.len() == 2
            && self.script.styles[1].name.starts_with("Default")
    }

    /// libass `process_event_tail`.
    fn event(&self, line: &str) -> Option<AssEvent> {
        let format = self.event_format.as_deref().unwrap_or(ASS_EVENT_FORMAT);
        let mut e = AssEvent {
            read_order: 0,
            layer: 0,
            start_ms: 0,
            duration_ms: 0,
            style: 0,
            name: String::new(),
            margin_l: 0,
            margin_r: 0,
            margin_v: 0,
            effect: String::new(),
            text: String::new(),
            items: Vec::new(),
        };
        let mut end_ms = 0;
        let mut fmt = format;
        let mut vals = line;
        loop {
            let name = next_token(&mut fmt, true)?;
            if name.eq_ignore_ascii_case("Text") {
                // The rest of the line, commas and leading spaces included.
                e.text = vals.trim_end_matches(['\r', '\t', ' ']).to_string();
                e.items = tags::parse_event_text(&e.text);
                e.duration_ms = end_ms - e.start_ms;
                return Some(e);
            }
            let tok = next_token(&mut vals, false)?;
            match name.to_ascii_lowercase().as_str() {
                "layer" => e.layer = parse_int_header(tok),
                "style" => e.style = self.script.lookup_style(tok),
                "name" | "actor" => e.name = tok.to_string(),
                "effect" => e.effect = tok.to_string(),
                "marginl" => e.margin_l = parse_int_header(tok),
                "marginr" => e.margin_r = parse_int_header(tok),
                "marginv" => e.margin_v = parse_int_header(tok),
                "start" => e.start_ms = parse_timecode(tok),
                "end" => end_ms = parse_timecode(tok),
                _ => {}
            }
        }
    }

    fn fonts_line(&mut self, line: &str) {
        if let Some(name) = line.strip_prefix("fontname:") {
            self.finish_font();
            self.font_name = Some(name.trim_start_matches([' ', '\t']).to_string());
        } else if self.font_name.is_some() {
            self.font_data.extend_from_slice(line.as_bytes());
        }
    }

    fn finish_font(&mut self) {
        let Some(name) = self.font_name.take() else { return };
        let data = std::mem::take(&mut self.font_data);
        match fonts::decode(&data) {
            Some(data) => self.script.fonts.push(EmbeddedFont { name, data }),
            None => log::warn!("[subs] embedded font {name:?}: bad encoded data size"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = "\u{feff}[Script Info]\r\n\
        ScriptType: v4.00+\r\n\
        PlayResX: 1920\r\n\
        PlayResY: 1080\r\n\
        WrapStyle: 2\r\n\
        \r\n\
        [V4+ Styles]\r\n\
        Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\r\n\
        Style: Default,Open Sans Semibold,70,&H0000FFFF,&H000000FF,&H00000000,&H64000000,-1,0,0,0,100,140,0,0,1,3,1,2,120,120,42,1\r\n\
        Style: *Top,Arial,50,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,-1,0,0,100,100,0,0,1,2,0,8,10,10,20,1\r\n\
        \r\n\
        [Events]\r\n\
        Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\
        Dialogue: 0,0:00:01.50,0:00:04.00,Default,Gal,0,0,0,,Hello, world\\Nsecond line  \r\n\
        Comment: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,not shown\r\n\
        Dialogue: 1,0:02:44.14,0:02:49.02,top,,10,0,0,,{\\an8}Top\r\n\
        Dialogue: 0,1:00:00.00,0:59:59.00,Missing,,0,0,0,, negative\r\n";

    #[test]
    fn parses_header_styles_and_events() {
        let s = parse(MINI).unwrap();
        assert_eq!(s.kind, ScriptKind::Ass);
        assert_eq!((s.play_res_x, s.play_res_y, s.wrap_style), (1920, 1080, 2));
        assert!(!s.scaled_border_and_shadow);

        // styles[0] = built-in, then the script's own.
        assert_eq!(s.styles.len(), 3);
        assert_eq!(s.default_style, 1);
        let d = &s.styles[1];
        assert_eq!(d.font_name, "Open Sans Semibold");
        assert_eq!(d.font_size, 70.0);
        assert_eq!(d.primary_colour, 0xFFFF_0000); // &H0000FFFF = yellow
        assert_eq!(d.back_colour, 0x0000_0064);
        assert!(d.bold && !d.italic);
        assert_eq!((d.scale_x, d.scale_y), (1.0, 1.4));
        assert_eq!(d.alignment, HALIGN_CENTER | VALIGN_SUB);
        assert_eq!((d.margin_l, d.margin_r, d.margin_v), (120, 120, 42));
        let top = &s.styles[2];
        assert_eq!(top.name, "Top"); // leading '*' dropped
        assert_eq!(top.alignment, HALIGN_CENTER | VALIGN_TOP);

        assert_eq!(s.events.len(), 3);
        let e = &s.events[0];
        assert_eq!((e.start_ms, e.duration_ms), (1500, 2500));
        assert_eq!(e.name, "Gal");
        assert_eq!(e.text, "Hello, world\\Nsecond line");
        assert_eq!(e.style, 1);
        let e = &s.events[1];
        assert_eq!(e.layer, 1);
        assert_eq!((e.start_ms, e.end_ms()), (164_140, 169_020));
        // Style names are case-sensitive: "top" falls back to Default.
        assert_eq!(e.style, s.default_style);
        assert_eq!(e.margin_l, 10);
        assert_eq!(e.read_order, 1);
        let e = &s.events[2];
        assert!(e.duration_ms < 0);
        assert_eq!(e.text, " negative");
    }

    #[test]
    fn timecodes_read_the_fraction_as_centiseconds() {
        assert_eq!(parse_timecode("0:00:01.50"), 1500);
        assert_eq!(parse_timecode("0:00:01.5"), 1050);
        assert_eq!(parse_timecode("0:00:01.123"), 2230);
        assert_eq!(parse_timecode("1:02:03.04"), 3_723_040);
        assert_eq!(parse_timecode("1:02:03"), 0);
        assert_eq!(parse_timecode(" 0:00:02.00"), 2000);
    }

    #[test]
    fn rejects_text_that_is_not_a_script() {
        assert!(parse("1\n00:00:01,000 --> 00:00:02,000\nhi\n").is_none());
        assert!(parse("[Script Info]\nTitle: x\n").is_none());
    }

    #[test]
    fn play_res_falls_back_like_libass() {
        let s = parse("[Script Info]\nScriptType: v4.00+\n").unwrap();
        assert_eq!((s.play_res_x, s.play_res_y), (384, 288));
        let s = parse("[Script Info]\nScriptType: v4.00+\nPlayResX: 1280\n").unwrap();
        assert_eq!((s.play_res_x, s.play_res_y), (1280, 1024));
        let s = parse("[Script Info]\nScriptType: v4.00+\nPlayResX: 640\n").unwrap();
        assert_eq!((s.play_res_x, s.play_res_y), (640, 480));
        let s = parse("[Script Info]\nScriptType: v4.00+\nPlayResY: 1080\n").unwrap();
        assert_eq!((s.play_res_x, s.play_res_y), (1440, 1080));
    }

    #[test]
    fn ssa_v4_conversions() {
        let s = parse(
            "[Script Info]\nScriptType: v4.00\n\n[V4 Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\n\
             Style: Default,Arial,20,16777215,65535,65535,-2147483640,-1,0,1,3,0,6,30,30,30,0,0\n\
             Style: Mid,Arial,20,16777215,65535,65535,0,0,0,1,3,0,8,30,30,30,0,0\n\n\
             [Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
             Dialogue: Marked=0,0:00:01.00,0:00:02.00,Default,,0000,0000,0000,,Hi\n",
        )
        .unwrap();
        assert_eq!(s.kind, ScriptKind::Ssa);
        let d = &s.styles[1];
        assert_eq!(d.primary_colour, 0xFFFF_FF00);
        // Outline drawn in BackColour; back alpha forced to 0x80.
        assert_eq!(d.outline_colour & 0xFFFF_FF00, d.back_colour & 0xFFFF_FF00);
        assert_eq!(d.back_colour & 0xFF, 0x80);
        // SSA alignment is kept in legacy numbering; 6 = top centre.
        assert_eq!(d.alignment, 6);
        // VSFilter quirk: 8 reads as 3.
        assert_eq!(s.styles[2].alignment, 3);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].text, "Hi");
    }

    #[test]
    fn custom_format_lines_default_scaled_border_on() {
        let s = parse(
            "[Script Info]\nScriptType: v4.00+\n[V4+ Styles]\nFormat: Name, Fontsize\nStyle: Default,30\n",
        )
        .unwrap();
        assert!(s.scaled_border_and_shadow);
        assert_eq!(s.styles[1].font_name, "Arial");
        assert_eq!(s.styles[1].font_size, 30.0);
        let s = parse(
            "[Script Info]\nScriptType: v4.00+\nScaledBorderAndShadow: no\n[V4+ Styles]\nFormat: Name, Fontsize\n",
        )
        .unwrap();
        assert!(!s.scaled_border_and_shadow);
    }

    #[test]
    fn embedded_fonts_decode_and_graphics_does_not_leak_in() {
        let font = b"\x00\x01\x00\x00fake font bytes";
        let enc = fonts::encode(font);
        let (a, b) = enc.split_at(8);
        let script = format!(
            "[Script Info]\nScriptType: v4.00+\n[Fonts]\nfontname: fake_0.ttf\n{a}\n{b}\n\
             [Graphics]\nfilename: logo.png\n!!!!!!!!\n[Events]\n"
        );
        let s = parse(&script).unwrap();
        assert_eq!(s.fonts.len(), 1);
        assert_eq!(s.fonts[0].name, "fake_0.ttf");
        assert_eq!(s.fonts[0].data, font);
    }

    #[test]
    fn lookup_style_rules() {
        let s = parse(MINI).unwrap();
        assert_eq!(s.lookup_style("default"), 1);
        assert_eq!(s.lookup_style("**Top"), 2);
        assert_eq!(s.lookup_style("nope"), s.default_style);
        assert_eq!(s.lookup_style_strict("Top"), Some(2));
        assert_eq!(s.lookup_style_strict("top"), None);
    }

    fn sample(name: &str) -> String {
        let path = format!("{}/../docs/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// The two real fansub scripts in `docs/` (when present): every event
    /// parses, styles resolve, drawings and transforms tokenize.
    #[test]
    fn real_fansub_scripts() {
        for (file, res, events) in [
            ("Mushoku Tensei S3 - 01_Subtitles01.ENG.ass", (640, 360), 234),
            ("Mushoku Tensei S3 - 01_Subtitles02.CZE.ass", (1920, 1080), 236),
        ] {
            let body = sample(file);
            if body.is_empty() {
                continue;
            }
            let s = parse(&body).unwrap_or_else(|| panic!("{file} did not parse"));
            assert_eq!((s.play_res_x, s.play_res_y), res, "{file}");
            assert_eq!(s.events.len(), events, "{file}");
            assert!(s.scaled_border_and_shadow);
            assert_eq!(s.ycbcr_matrix, YCbCrMatrix::Bt601Tv);
            assert_eq!((s.layout_res_x, s.layout_res_y), (640, 360));
            // Every event's style exists in the file (none fall back).
            for e in &s.events {
                assert_ne!(e.style, 0, "{file}: {:?} fell back to the built-in style", e.text);
            }
            let has = |pred: &dyn Fn(&Tag) -> bool| {
                s.events.iter().any(|e| {
                    e.items.iter().any(|i| matches!(i, Item::Override(t) if t.iter().any(pred)))
                })
            };
            assert!(has(&|t| matches!(t, Tag::Pos { .. })), "{file}");
            assert!(has(&|t| matches!(t, Tag::Fade { .. })), "{file}");
        }
    }
}
