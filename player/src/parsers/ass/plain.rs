//! ASS events as plain cues: the text without its styling, placed by the
//! event's alignment / `\pos` translated to WebVTT-style [`CueLayout`].
//!
//! This is what a renderer without ASS support shows, and what ASS events
//! look like to everything that only reads [`VttCue::text`]. Dialogue
//! comes out right; typesetting (signs, positioned titles) lands in about
//! the right place without its look, and pure drawings (`\p1` logos)
//! have no text and show nothing.

use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use super::tags::{text_chars, Item, Tag};
use super::{numpad2align, AssCue, AssEvent, AssScript, HALIGN_LEFT, HALIGN_RIGHT, VALIGN_CENTER, VALIGN_TOP};
use crate::parsers::vtt::{Anchor, CueLayout, CueLine, TextAlign, VttCue};

/// One cue per event that is ever on screen (positive duration), sorted by
/// start time, each linked back to its event through [`VttCue::ass`].
/// Events whose text is empty (drawings, pure tag lines) are kept: an ASS
/// renderer draws them, the plain one skips empty text.
pub fn to_cues(script: &Arc<AssScript>) -> Vec<VttCue> {
    let mut cues: Vec<VttCue> = script
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.duration_ms > 0)
        .map(|(i, e)| {
            let plain = evaluate(script, e);
            VttCue {
                start_ms: e.start_ms,
                end_ms: e.end_ms(),
                layout: layout(script, e, &plain),
                text: plain.text,
                settings: String::new(),
                ass: Some(AssCue { script: Arc::clone(script), event: i }),
            }
        })
        .collect();
    // Stable: same-start events keep file order.
    cues.sort_by_key(|c| c.start_ms);
    cues
}

/// What the plain view needs from an event's tags.
struct Plain {
    text: String,
    /// libass alignment bits from `\an` / `\a` (first one wins).
    alignment: Option<i32>,
    /// `\pos`, or the start point of `\move` (first one wins).
    pos: Option<(f64, f64)>,
}

fn evaluate(script: &AssScript, event: &AssEvent) -> Plain {
    let style = &script.styles[event.style];
    let mut out = Plain { text: String::new(), alignment: None, pos: None };
    let mut wrap_style = script.wrap_style;
    let mut drawing = 0;
    for item in &event.items {
        match item {
            Item::Override(tags) => {
                for tag in tags {
                    match *tag {
                        Tag::AlignNumpad(v) if out.alignment.is_none() => {
                            out.alignment = Some(if (1..=9).contains(&v) { numpad2align(v) } else { style.alignment });
                        }
                        Tag::AlignLegacy(v) if out.alignment.is_none() => {
                            // VSFilter: the illegal \a4 / \a8 read as \a5.
                            out.alignment = Some(if (1..=11).contains(&v) {
                                if v & 3 == 0 { 5 } else { v }
                            } else {
                                style.alignment
                            });
                        }
                        Tag::Pos { x, y } if out.pos.is_none() => out.pos = Some((x, y)),
                        Tag::Move { x1, y1, .. } if out.pos.is_none() => out.pos = Some((x1, y1)),
                        Tag::Drawing(scale) => drawing = scale,
                        Tag::WrapStyle(v) => {
                            wrap_style = match v {
                                Some(v @ 0..=3) => v,
                                _ => script.wrap_style,
                            }
                        }
                        _ => {}
                    }
                }
            }
            Item::Text(raw) if drawing == 0 => out.text.extend(text_chars(raw, wrap_style)),
            Item::Text(_) => {}
        }
    }
    out.text = tidy(&out.text);
    out
}

/// Trim the spaces typesetters leave around line breaks, drop leading and
/// trailing blank lines (`\N` padding), and fold to NFC like the WebVTT
/// path does (the plain rasterizer has no shaping).
fn tidy(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_matches(' ')).collect();
    let first = lines.iter().position(|l| !l.is_empty());
    let last = lines.iter().rposition(|l| !l.is_empty());
    match (first, last) {
        (Some(a), Some(b)) => lines[a..=b].join("\n").nfc().collect(),
        _ => String::new(),
    }
}

/// The event's placement as cue settings, in fractions of the script's
/// PlayRes. Bottom-aligned events without `\pos` keep the overlay's default
/// bottom placement (`CueLine::Auto`) so plain ASS dialogue sits where
/// WebVTT/SRT dialogue does.
fn layout(script: &AssScript, event: &AssEvent, plain: &Plain) -> CueLayout {
    let style = &script.styles[event.style];
    let alignment = plain.alignment.unwrap_or(style.alignment);
    let (halign, valign) = (alignment & 3, alignment & 12);
    let prx = script.play_res_x as f32;
    let pry = script.play_res_y as f32;
    let (h_anchor, text_align) = match halign {
        HALIGN_LEFT => (Anchor::Start, TextAlign::Left),
        HALIGN_RIGHT => (Anchor::End, TextAlign::Right),
        _ => (Anchor::Middle, TextAlign::Center),
    };
    let v_anchor = match valign {
        VALIGN_TOP => Anchor::Start,
        VALIGN_CENTER => Anchor::Middle,
        _ => Anchor::End,
    };
    let mut layout = CueLayout { align: text_align, position_anchor: Some(h_anchor), ..CueLayout::DEFAULT };

    if let Some((x, y)) = plain.pos {
        layout.position = Some((x as f32 / prx).clamp(0.0, 1.0));
        layout.line = CueLine::Fraction((y as f32 / pry).clamp(0.0, 1.0));
        layout.line_anchor = v_anchor;
        return layout;
    }

    let margin = |event: i32, style: i32| if event != 0 { event } else { style } as f32;
    let ml = margin(event.margin_l, style.margin_l);
    let mr = margin(event.margin_r, style.margin_r);
    let mv = margin(event.margin_v, style.margin_v);
    layout.position = Some(
        match halign {
            HALIGN_LEFT => ml / prx,
            HALIGN_RIGHT => 1.0 - mr / prx,
            // Centred between the margins.
            _ => (ml + prx - mr) / 2.0 / prx,
        }
        .clamp(0.0, 1.0),
    );
    match valign {
        VALIGN_TOP => {
            layout.line = CueLine::Fraction((mv / pry).clamp(0.0, 1.0));
            layout.line_anchor = Anchor::Start;
        }
        VALIGN_CENTER => {
            layout.line = CueLine::Fraction(0.5);
            layout.line_anchor = Anchor::Middle;
        }
        _ => {}
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::ass::parse;

    fn script(events: &str) -> Arc<AssScript> {
        Arc::new(
            parse(&format!(
                "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\
                 [V4+ Styles]\n\
                 Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
                 Style: Default,Arial,26,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1.3,0,2,20,20,23,1\n\
                 Style: Top,Arial,26,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1.3,0,8,20,20,36,1\n\
                 [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{events}"
            ))
            .unwrap(),
        )
    }

    #[test]
    fn dialogue_becomes_plain_text_at_the_default_spot() {
        let s = script("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}Hello \\N there\\hfriend{\\i0}\n");
        let cues = to_cues(&s);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello\nthere\u{a0}friend");
        assert_eq!(cues[0].layout.line, CueLine::Auto);
        assert_eq!(cues[0].layout.position, Some(0.5));
        assert_eq!(cues[0].ass.as_ref().unwrap().event().text, "{\\i1}Hello \\N there\\hfriend{\\i0}");
    }

    #[test]
    fn positioned_and_top_events_map_to_fractions() {
        let s = script(
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\an8\\pos(320,36)}Title\n\
             Dialogue: 0,0:00:01.00,0:00:02.00,Top,,0,0,0,,Top line\n\
             Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\an4}Middle left\n",
        );
        let cues = to_cues(&s);
        let l = cues[0].layout;
        assert_eq!(l.position, Some(0.5));
        assert_eq!(l.line, CueLine::Fraction(0.1));
        assert_eq!(l.line_anchor, Anchor::Start);
        let l = cues[1].layout;
        assert_eq!(l.line, CueLine::Fraction(0.1));
        assert_eq!(l.line_anchor, Anchor::Start);
        let l = cues[2].layout;
        assert_eq!(l.position, Some(20.0 / 640.0));
        assert_eq!(l.position_anchor, Some(Anchor::Start));
        assert_eq!(l.align, TextAlign::Left);
        assert_eq!(l.line, CueLine::Fraction(0.5));
    }

    #[test]
    fn drawings_and_negative_events() {
        let s = script(
            "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\p1}m 0 0 l 10 0 10 10{\\p0}\n\
             Dialogue: 0,0:00:05.00,0:00:04.00,Default,,0,0,0,,never\n\
             Dialogue: 0,0:00:00.50,0:00:02.00,Default,,0,0,0,,{\\q2}a\\nb\n",
        );
        let cues = to_cues(&s);
        assert_eq!(cues.len(), 2);
        // Sorted by start: the \q2 event first.
        assert_eq!(cues[0].text, "a\nb");
        assert_eq!(cues[1].text, "");
        assert!(cues[1].ass.is_some());
    }
}
