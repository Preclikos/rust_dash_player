//! Event text: override blocks (`{\pos(10,20)\c&H0000FF&}`) and the text
//! between them.
//!
//! Tokenizing is a port of libass `ass_parse_tags` (libass/ass_parse.c):
//! the same argument splitting (parenthesised arguments first, commas,
//! a backslash swallowing the rest up to the closing parenthesis — which
//! may be missing), the same tag names tried in the same order as
//! prefixes (`\fscx` before `\fsc` before `\fs`; `\fs20` is `\fs` with the
//! argument `20`), and the same argument conversions. What libass does
//! with a tag *while rendering* — animate it under `\t`, let the first
//! `\pos` win, fall back to the style when the argument is missing or out
//! of range — needs the render state and the event time, so it is not
//! decided here: a tag keeps exactly the information that decision needs
//! (`None` = "no argument", raw integers where libass validates later).

use super::num::{dtoi32, is_ass_space, strtod, strtoi32};

/// libass `MAX_VALID_NARGS`; one more argument is kept so excess ones can
/// be told apart from a valid count.
const MAX_VALID_NARGS: usize = 7;

/// A run of event text: an override block's tags, or the raw text up to
/// the next block.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Override(Vec<Tag>),
    /// Raw text with ASS escapes (`\N`, `\n`, `\h`, `\{`, `\}`) still in
    /// it — `\n` means a line break or a space depending on the wrap style
    /// in force at that point, which a `\q` earlier in the event can
    /// change. Read it through [`text_chars`]. In drawing mode (`\p1`…)
    /// the same text is vector drawing commands instead.
    Text(String),
}

/// `\fs` argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontSize {
    /// `\fs40`.
    Absolute(f64),
    /// `\fs+2` / `\fs-2`: the size grows by `n / 10` of itself (VSFilter).
    Relative(f64),
}

/// `\clip` / `\iclip` shape.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipShape {
    /// `\clip(x0,y0,x1,y1)`, script pixels.
    Rect { x0: i32, y0: i32, x1: i32, y1: i32 },
    /// `\clip([scale,]drawing)`, drawing commands as in `\p`.
    Drawing { scale: i32, commands: String },
}

/// Which karaoke tag a syllable carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KaraokeKind {
    /// `\k`: switch from secondary to primary colour at the syllable start.
    Fill,
    /// `\kf` / `\K`: sweep the fill left to right over the syllable.
    Sweep,
    /// `\ko`: like `\k`, but the outline only appears when it starts.
    Outline,
}

/// One override tag. Fields are the parsed arguments; `None` means the
/// tag had no argument, which for most tags resets the value to the
/// style's.
#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    /// `\bord`, `\xbord`, `\ybord`.
    Border(Option<f64>),
    BorderX(Option<f64>),
    BorderY(Option<f64>),
    /// `\shad`, `\xshad`, `\yshad`.
    Shadow(Option<f64>),
    ShadowX(Option<f64>),
    ShadowY(Option<f64>),
    /// `\fax`, `\fay` shear.
    ShearX(Option<f64>),
    ShearY(Option<f64>),
    /// `\clip` (`inverse == false`) / `\iclip`.
    Clip { inverse: bool, shape: ClipShape },
    /// `\blur` (gaussian), `\be` (box blur passes).
    Blur(Option<f64>),
    BlurEdges(Option<f64>),
    /// `\fscx` / `\fscy` as a fraction (`\fscx150` → 1.5); `\fsc` resets both.
    ScaleX(Option<f64>),
    ScaleY(Option<f64>),
    ScaleReset,
    /// `\fsp` letter spacing.
    Spacing(Option<f64>),
    /// `\fs`.
    FontSize(Option<FontSize>),
    /// `\fn`; `None` = back to the style's font (`\fn` / `\fn0`).
    FontName(Option<String>),
    /// `\frx`, `\fry`, `\frz` / `\fr` rotation in degrees.
    RotateX(Option<f64>),
    RotateY(Option<f64>),
    RotateZ(Option<f64>),
    /// `\an` (numpad) and `\a` (legacy SSA) alignment, unvalidated.
    AlignNumpad(i32),
    AlignLegacy(i32),
    /// `\pos(x,y)`.
    Pos { x: f64, y: f64 },
    /// `\move(x1,y1,x2,y2[,t1,t2])`; `t1 = t2 = 0` = the whole event.
    Move { x1: f64, y1: f64, x2: f64, y2: f64, t1: i32, t2: i32 },
    /// `\org(x,y)` rotation origin.
    Origin { x: f64, y: f64 },
    /// `\fade(a1,a2,a3,t1,t2,t3,t4)` as is; `\fad(in,out)` arrives as
    /// `a = (255, 0, 255)`, `t1 = t4 = -1`, `t2 = in`, `t3 = out` — the
    /// form libass resolves against the event duration.
    Fade { a1: i32, a2: i32, a3: i32, t1: i32, t2: i32, t3: i32, t4: i32 },
    /// `\t([t1,t2,][accel,]tags)`. `t2 == 0` = until the event ends.
    Transform { t1: i32, t2: i32, accel: f64, tags: Vec<Tag> },
    /// `\c`/`\1c`…`\4c`: libass RGBA (`0xRRGGBB00`; the tag never sets
    /// alpha), `index` 0 = primary … 3 = back.
    Colour { index: usize, value: Option<u32> },
    /// `\alpha` (all four) and `\1a`…`\4a`: 0 = opaque, 255 = invisible.
    Alpha(Option<i32>),
    ColourAlpha { index: usize, value: Option<i32> },
    /// `\r` / `\rStyle`.
    Reset(Option<String>),
    /// `\b` (0, 1 or a weight ≥ 100), `\i`, `\u`, `\s` — validated later.
    Bold(Option<i32>),
    Italic(Option<i32>),
    Underline(Option<i32>),
    StrikeOut(Option<i32>),
    /// `\k`, `\kf`/`\K`, `\ko`: duration in centiseconds (`None` = 100).
    Karaoke { kind: KaraokeKind, centis: Option<f64> },
    /// `\kt`: karaoke start time in centiseconds.
    KaraokeStart(Option<f64>),
    /// `\p` drawing mode scale (0 = text), `\pbo` baseline offset.
    Drawing(i32),
    DrawingBaselineOffset(f64),
    /// `\q` wrap style.
    WrapStyle(Option<i32>),
    /// `\fe` font encoding.
    Encoding(Option<i32>),
}

/// Split event text into override blocks and text runs.
///
/// A `{` opens a block only when a `}` follows somewhere after it (an
/// unmatched `{` is text); `\{` is an escaped brace, never a block. Text
/// inside a block that isn't a tag is a comment and disappears.
pub fn parse_event_text(text: &str) -> Vec<Item> {
    let b = text.as_bytes();
    let mut items = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                if let Some(close) = b[i + 1..].iter().position(|&c| c == b'}') {
                    let close = i + 1 + close;
                    if text_start < i {
                        items.push(Item::Text(text[text_start..i].to_string()));
                    }
                    items.push(Item::Override(parse_tags(&b[i + 1..close])));
                    i = close + 1;
                    text_start = i;
                    continue;
                }
                i += 1;
            }
            // An escape is one unit: `\{` must not open a block.
            b'\\' if matches!(b.get(i + 1), Some(b'N' | b'n' | b'h' | b'{' | b'}')) => i += 2,
            _ => i += 1,
        }
    }
    if text_start < b.len() {
        items.push(Item::Text(text[text_start..].to_string()));
    }
    items
}

/// The characters a text run stands for under `wrap_style` (libass
/// `ass_get_next_char`): `\N` is a line break, `\n` a line break only in
/// wrap style 2 and a space otherwise, `\h` a non-breaking space, `\{` /
/// `\}` literal braces, a tab a space. Any other backslash is itself.
pub fn text_chars(raw: &str, wrap_style: i32) -> impl Iterator<Item = char> + '_ {
    let mut chars = raw.chars().peekable();
    std::iter::from_fn(move || {
        let c = chars.next()?;
        Some(match c {
            '\t' => ' ',
            '\\' => match chars.peek() {
                Some('N') => {
                    chars.next();
                    '\n'
                }
                Some('n') => {
                    chars.next();
                    if wrap_style == 2 { '\n' } else { ' ' }
                }
                Some('h') => {
                    chars.next();
                    '\u{a0}'
                }
                Some(&b @ ('{' | '}')) => {
                    chars.next();
                    b
                }
                _ => '\\',
            },
            c => c,
        })
    })
}

/// Parse the inside of one override block (between `{` and `}`).
pub fn parse_tags(block: &[u8]) -> Vec<Tag> {
    let mut out = Vec::new();
    parse_into(block, &mut out);
    out
}

/// Tag names in the order libass tries them. `true` = a "simple" tag,
/// whose text after the name (`\fs20` → `20`) is appended as an argument;
/// the others only take parenthesised arguments.
const NAMES: &[(&str, bool)] = &[
    ("xbord", true),
    ("ybord", true),
    ("xshad", true),
    ("yshad", true),
    ("fax", true),
    ("fay", true),
    ("iclip", false),
    ("blur", true),
    ("fscx", true),
    ("fscy", true),
    ("fsc", true),
    ("fsp", true),
    ("fs", true),
    ("bord", true),
    ("move", false),
    ("frx", true),
    ("fry", true),
    ("frz", true),
    ("fr", true),
    ("fn", true),
    ("alpha", true),
    ("an", true),
    ("a", true),
    ("pos", false),
    ("fade", false),
    ("fad", false),
    ("org", false),
    ("t", false),
    ("clip", false),
    ("c", true),
    ("1c", true),
    ("2c", true),
    ("3c", true),
    ("4c", true),
    ("1a", true),
    ("2a", true),
    ("3a", true),
    ("4a", true),
    ("r", true),
    ("be", true),
    ("b", true),
    ("i", true),
    ("kt", true),
    ("kf", true),
    ("K", true),
    ("ko", true),
    ("k", true),
    ("shad", true),
    ("s", true),
    ("u", true),
    ("pbo", true),
    ("p", true),
    ("q", true),
    ("fe", true),
];

/// Argument byte ranges into the block, at most `MAX_VALID_NARGS + 1`.
struct Args<'a> {
    src: &'a [u8],
    ranges: Vec<(usize, usize)>,
}

impl<'a> Args<'a> {
    /// libass `push_arg`: right-trim spaces, drop empty arguments, keep
    /// one past the valid maximum.
    fn push(&mut self, start: usize, mut end: usize) {
        if self.ranges.len() <= MAX_VALID_NARGS {
            while end > start && is_ass_space(self.src[end - 1]) {
                end -= 1;
            }
            if end > start {
                self.ranges.push((start, end));
            }
        }
    }

    fn len(&self) -> usize {
        self.ranges.len()
    }

    fn get(&self, i: usize) -> &'a [u8] {
        self.ranges.get(i).map(|&(s, e)| &self.src[s..e]).unwrap_or(&[])
    }

    fn f(&self, i: usize) -> f64 {
        strtod(self.get(i))
    }

    fn i(&self, i: usize) -> i32 {
        strtoi32(self.get(i), 10)
    }

    /// First argument as a float, `None` when there are none.
    fn opt_f(&self) -> Option<f64> {
        (self.len() > 0).then(|| self.f(0))
    }

    fn opt_i(&self) -> Option<i32> {
        (self.len() > 0).then(|| self.i(0))
    }

    fn string(&self, i: usize) -> String {
        String::from_utf8_lossy(self.get(i)).into_owned()
    }
}

fn parse_into(b: &[u8], out: &mut Vec<Tag>) {
    let end = b.len();
    let mut p = 0;
    while p < end {
        while p < end && b[p] != b'\\' {
            p += 1;
        }
        if p >= end {
            break;
        }
        p += 1;
        while p < end && is_ass_space(b[p]) {
            p += 1;
        }
        let mut q = p;
        while q < end && b[q] != b'(' && b[q] != b'\\' {
            q += 1;
        }
        if q == p {
            continue;
        }
        let name_end = q;

        let mut args = Args { src: b, ranges: Vec::new() };
        let mut has_backslash_arg = false;
        // Parenthesised arguments are split for every tag, before any
        // trailing argument — VSFilter order.
        if q < end && b[q] == b'(' {
            q += 1;
            loop {
                while q < end && is_ass_space(b[q]) {
                    q += 1;
                }
                let mut r = q;
                while r < end && !matches!(b[r], b',' | b'\\' | b')') {
                    r += 1;
                }
                if r < end && b[r] == b',' {
                    args.push(q, r);
                    q = r + 1;
                } else {
                    // A backslash lumps everything up to the closing
                    // parenthesis (or the end) into one argument: the tag
                    // list of `\t`, or a stray backslash anywhere else.
                    if r < end && b[r] == b'\\' {
                        has_backslash_arg = true;
                        r = b[r..].iter().position(|&c| c == b')').map_or(end, |i| r + i);
                    }
                    args.push(q, r);
                    q = r;
                    if q != end {
                        q += 1;
                    }
                    break;
                }
            }
        }

        let name = &b[p..name_end];
        let Some(&(tag_name, simple)) = NAMES.iter().find(|(n, _)| name.starts_with(n.as_bytes())) else {
            p = q;
            continue;
        };
        if simple {
            args.push(p + tag_name.len(), name_end);
        }
        if let Some(tag) = build(tag_name, &args, has_backslash_arg) {
            out.push(tag);
        }
        p = q;
    }
}

fn build(name: &str, args: &Args<'_>, has_backslash_arg: bool) -> Option<Tag> {
    let n = args.len();
    Some(match name {
        "xbord" => Tag::BorderX(args.opt_f()),
        "ybord" => Tag::BorderY(args.opt_f()),
        "xshad" => Tag::ShadowX(args.opt_f()),
        "yshad" => Tag::ShadowY(args.opt_f()),
        "fax" => Tag::ShearX(args.opt_f()),
        "fay" => Tag::ShearY(args.opt_f()),
        "iclip" | "clip" => Tag::Clip { inverse: name == "iclip", shape: clip_shape(args)? },
        "blur" => Tag::Blur(args.opt_f()),
        "fscx" => Tag::ScaleX(args.opt_f().map(|v| v / 100.0)),
        "fscy" => Tag::ScaleY(args.opt_f().map(|v| v / 100.0)),
        "fsc" => Tag::ScaleReset,
        "fsp" => Tag::Spacing(args.opt_f()),
        "fs" => Tag::FontSize(args.opt_f().map(|v| {
            if matches!(args.get(0).first(), Some(b'+' | b'-')) {
                FontSize::Relative(v)
            } else {
                FontSize::Absolute(v)
            }
        })),
        "bord" => Tag::Border(args.opt_f()),
        "move" => {
            if n != 4 && n != 6 {
                return None;
            }
            let (mut t1, mut t2) = (0, 0);
            if n == 6 {
                t1 = args.i(4);
                t2 = args.i(5);
                if t1 > t2 {
                    std::mem::swap(&mut t1, &mut t2);
                }
            }
            Tag::Move { x1: args.f(0), y1: args.f(1), x2: args.f(2), y2: args.f(3), t1, t2 }
        }
        "frx" => Tag::RotateX(args.opt_f()),
        "fry" => Tag::RotateY(args.opt_f()),
        "frz" | "fr" => Tag::RotateZ(args.opt_f()),
        "fn" => {
            let arg = args.get(0);
            if n > 0 && arg != b"0" {
                let start = arg.iter().position(|&c| !is_ass_space(c)).unwrap_or(arg.len());
                Tag::FontName(Some(String::from_utf8_lossy(&arg[start..]).into_owned()))
            } else {
                Tag::FontName(None)
            }
        }
        "alpha" => Tag::Alpha((n > 0).then(|| parse_alpha_tag(args.get(0)))),
        "an" => Tag::AlignNumpad(args.i(0)),
        "a" => Tag::AlignLegacy(args.i(0)),
        "pos" => {
            if n != 2 {
                return None;
            }
            Tag::Pos { x: args.f(0), y: args.f(1) }
        }
        "fade" | "fad" => match n {
            2 => Tag::Fade { a1: 0xFF, a2: 0, a3: 0xFF, t1: -1, t2: args.i(0), t3: args.i(1), t4: -1 },
            7 => Tag::Fade {
                a1: args.i(0),
                a2: args.i(1),
                a3: args.i(2),
                t1: args.i(3),
                t2: args.i(4),
                t3: args.i(5),
                t4: args.i(6),
            },
            _ => return None,
        },
        "org" => {
            if n != 2 {
                return None;
            }
            Tag::Origin { x: args.f(0), y: args.f(1) }
        }
        "t" => {
            // The timing arguments read differently by count (VSFilter):
            // with accel the times are integers, without it floats
            // truncated. The tag list is the last argument.
            let cnt = n as i32 - 1;
            let (t1, t2, accel) = match cnt {
                3 => (args.i(0), args.i(1), args.f(2)),
                2 => (dtoi32(args.f(0)), dtoi32(args.f(1)), 1.0),
                1 => (0, 0, args.f(0)),
                _ => (0, 0, 1.0),
            };
            // A malformed `\t` still counts (it turns collision detection
            // off in libass), it just animates nothing.
            let mut tags = Vec::new();
            if (0..=3).contains(&cnt) && has_backslash_arg {
                parse_into(args.get(cnt as usize), &mut tags);
            }
            Tag::Transform { t1, t2, accel, tags }
        }
        "c" | "1c" | "2c" | "3c" | "4c" => Tag::Colour {
            index: colour_index(name),
            value: (n > 0).then(|| parse_color_tag(args.get(0))),
        },
        "1a" | "2a" | "3a" | "4a" => Tag::ColourAlpha {
            index: colour_index(name),
            value: (n > 0).then(|| parse_alpha_tag(args.get(0))),
        },
        "r" => Tag::Reset((n > 0).then(|| args.string(0))),
        "be" => Tag::BlurEdges(args.opt_f()),
        "b" => Tag::Bold(args.opt_i()),
        "i" => Tag::Italic(args.opt_i()),
        "kt" => Tag::KaraokeStart(args.opt_f()),
        "kf" | "K" => Tag::Karaoke { kind: KaraokeKind::Sweep, centis: args.opt_f() },
        "ko" => Tag::Karaoke { kind: KaraokeKind::Outline, centis: args.opt_f() },
        "k" => Tag::Karaoke { kind: KaraokeKind::Fill, centis: args.opt_f() },
        "shad" => Tag::Shadow(args.opt_f()),
        "s" => Tag::StrikeOut(args.opt_i()),
        "u" => Tag::Underline(args.opt_i()),
        "pbo" => Tag::DrawingBaselineOffset(args.f(0)),
        "p" => Tag::Drawing(args.i(0).max(0)),
        "q" => Tag::WrapStyle(args.opt_i()),
        "fe" => Tag::Encoding(args.opt_i()),
        _ => return None,
    })
}

fn colour_index(name: &str) -> usize {
    match name.as_bytes()[0] {
        b'2' => 1,
        b'3' => 2,
        b'4' => 3,
        _ => 0,
    }
}

fn clip_shape(args: &Args<'_>) -> Option<ClipShape> {
    match args.len() {
        4 => Some(ClipShape::Rect { x0: args.i(0), y0: args.i(1), x1: args.i(2), y1: args.i(3) }),
        1 => Some(ClipShape::Drawing { scale: 1, commands: args.string(0) }),
        2 => Some(ClipShape::Drawing { scale: args.i(0).max(1), commands: args.string(1) }),
        _ => None,
    }
}

/// Leading `&` / `H` (uppercase only, as in libass) are skipped.
fn skip_hex_prefix(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| c != b'&' && c != b'H').unwrap_or(s.len());
    &s[start..]
}

/// libass `parse_alpha_tag`: `&HFF&` → 255.
fn parse_alpha_tag(s: &[u8]) -> i32 {
    strtoi32(skip_hex_prefix(s), 16)
}

/// libass `parse_color_tag`: `&HBBGGRR&` → RGBA `0xRRGGBB00`.
fn parse_color_tag(s: &[u8]) -> u32 {
    (strtoi32(skip_hex_prefix(s), 16) as u32).swap_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(s: &str) -> Vec<Tag> {
        parse_tags(s.as_bytes())
    }

    #[test]
    fn prefixes_resolve_in_libass_order() {
        assert_eq!(
            tags(r"\fscx150\fscy\fsp2\fs20\fsc"),
            vec![
                Tag::ScaleX(Some(1.5)),
                Tag::ScaleY(None),
                Tag::Spacing(Some(2.0)),
                Tag::FontSize(Some(FontSize::Absolute(20.0))),
                Tag::ScaleReset,
            ]
        );
        assert_eq!(
            tags(r"\blur3\be1\bord2\b1"),
            vec![Tag::Blur(Some(3.0)), Tag::BlurEdges(Some(1.0)), Tag::Border(Some(2.0)), Tag::Bold(Some(1))]
        );
        assert_eq!(tags(r"\an8\a6\alpha&H80&"), vec![
            Tag::AlignNumpad(8),
            Tag::AlignLegacy(6),
            Tag::Alpha(Some(0x80)),
        ]);
        assert_eq!(tags(r"\iclip(1,2,3,4)\i1"), vec![
            Tag::Clip { inverse: true, shape: ClipShape::Rect { x0: 1, y0: 2, x1: 3, y1: 4 } },
            Tag::Italic(Some(1)),
        ]);
        assert_eq!(tags(r"\kf50\K20\ko10\k5\kt3"), vec![
            Tag::Karaoke { kind: KaraokeKind::Sweep, centis: Some(50.0) },
            Tag::Karaoke { kind: KaraokeKind::Sweep, centis: Some(20.0) },
            Tag::Karaoke { kind: KaraokeKind::Outline, centis: Some(10.0) },
            Tag::Karaoke { kind: KaraokeKind::Fill, centis: Some(5.0) },
            Tag::KaraokeStart(Some(3.0)),
        ]);
    }

    #[test]
    fn values_parse_leniently() {
        // From the sample fansub: `\be.25`, colour without `H`, no-arg resets.
        assert_eq!(
            tags(r"\fad(1575,0)\an5\c&H121519&\be.25\fs80\pos(320,38)"),
            vec![
                Tag::Fade { a1: 255, a2: 0, a3: 255, t1: -1, t2: 1575, t3: 0, t4: -1 },
                Tag::AlignNumpad(5),
                Tag::Colour { index: 0, value: Some(0x1915_1200) },
                Tag::BlurEdges(Some(0.25)),
                Tag::FontSize(Some(FontSize::Absolute(80.0))),
                Tag::Pos { x: 320.0, y: 38.0 },
            ]
        );
        assert_eq!(tags(r"\c&FFFFFF&\3c"), vec![
            Tag::Colour { index: 0, value: Some(0xFFFF_FF00) },
            Tag::Colour { index: 2, value: None },
        ]);
        assert_eq!(tags(r"\fs+2\fs-1.5"), vec![
            Tag::FontSize(Some(FontSize::Relative(2.0))),
            Tag::FontSize(Some(FontSize::Relative(-1.5))),
        ]);
    }

    #[test]
    fn transform_without_comma_before_the_tags() {
        // `\t(24,2274,1 \1a&H00&)` — the accel and the tag list share an
        // argument; libass reads it as two times (truncated floats) plus tags.
        assert_eq!(tags(r"\fs20\1a&HFF&\t(24,2274,1 \1a&H00&)"), vec![
            Tag::FontSize(Some(FontSize::Absolute(20.0))),
            Tag::ColourAlpha { index: 0, value: Some(255) },
            Tag::Transform {
                t1: 24,
                t2: 2274,
                accel: 1.0,
                tags: vec![Tag::ColourAlpha { index: 0, value: Some(0) }],
            },
        ]);
    }

    #[test]
    fn transform_argument_forms() {
        assert_eq!(tags(r"\t(\frz90)"), vec![Tag::Transform {
            t1: 0,
            t2: 0,
            accel: 1.0,
            tags: vec![Tag::RotateZ(Some(90.0))],
        }]);
        assert_eq!(tags(r"\t(0.5,\fscx200)"), vec![Tag::Transform {
            t1: 0,
            t2: 0,
            accel: 0.5,
            tags: vec![Tag::ScaleX(Some(2.0))],
        }]);
        assert_eq!(tags(r"\t(100,200,2,\bord4\shad2)\b1"), vec![
            Tag::Transform {
                t1: 100,
                t2: 200,
                accel: 2.0,
                tags: vec![Tag::Border(Some(4.0)), Tag::Shadow(Some(2.0))],
            },
            Tag::Bold(Some(1)),
        ]);
        // Missing closing parenthesis: the tags run to the end of the block.
        assert_eq!(tags(r"\t(0,500,\c&H0000FF&"), vec![Tag::Transform {
            t1: 0,
            t2: 500,
            accel: 1.0,
            tags: vec![Tag::Colour { index: 0, value: Some(0xFF00_0000) }],
        }]);
        // A clip inside \t: the argument ends at the first ')'.
        assert_eq!(tags(r"\t(0,100,\clip(0,0,10,10))"), vec![Tag::Transform {
            t1: 0,
            t2: 100,
            accel: 1.0,
            tags: vec![Tag::Clip { inverse: false, shape: ClipShape::Rect { x0: 0, y0: 0, x1: 10, y1: 10 } }],
        }]);
    }

    #[test]
    fn complex_tags_need_their_argument_count() {
        assert_eq!(tags(r"\pos(1)\move(1,2,3)\org(1,2,3)"), vec![]);
        assert_eq!(tags(r"\move(0,0,10,10,500,100)"), vec![Tag::Move {
            x1: 0.0,
            y1: 0.0,
            x2: 10.0,
            y2: 10.0,
            t1: 100,
            t2: 500,
        }]);
        assert_eq!(tags(r"\clip(2,m 0 0 l 10 0 10 10)"), vec![Tag::Clip {
            inverse: false,
            shape: ClipShape::Drawing { scale: 2, commands: "m 0 0 l 10 0 10 10".into() },
        }]);
    }

    #[test]
    fn font_names_and_resets() {
        assert_eq!(tags(r"\fnTimes New Roman\b1"), vec![
            Tag::FontName(Some("Times New Roman".into())),
            Tag::Bold(Some(1)),
        ]);
        assert_eq!(tags(r"\fn0\fn\rSign\r"), vec![
            Tag::FontName(None),
            Tag::FontName(None),
            Tag::Reset(Some("Sign".into())),
            Tag::Reset(None),
        ]);
    }

    #[test]
    fn unknown_tags_and_comments_are_skipped() {
        assert_eq!(tags(r"comment \xyz5 more\b1 \ "), vec![Tag::Bold(Some(1))]);
        assert_eq!(tags(r"\p2"), vec![Tag::Drawing(2)]);
        assert_eq!(tags(r"\p-1"), vec![Tag::Drawing(0)]);
    }

    #[test]
    fn event_text_splits_into_blocks_and_runs() {
        let items = parse_event_text(r"{\i1}Hello{comment} \{literal\} {open");
        assert_eq!(items, vec![
            Item::Override(vec![Tag::Italic(Some(1))]),
            Item::Text("Hello".into()),
            Item::Override(vec![]),
            Item::Text(r" \{literal\} {open".into()),
        ]);
    }

    #[test]
    fn escapes_follow_the_wrap_style() {
        let s: String = text_chars("a\\Nb\\nc\\hd\\{e\\}\\x\tf", 0).collect();
        assert_eq!(s, "a\nb c\u{a0}d{e}\\x f");
        let s: String = text_chars(r"b\nc", 2).collect();
        assert_eq!(s, "b\nc");
    }
}
