//! OpenType shaping of one run (libass ass_shaper.c `shape_harfbuzz`).
//!
//! libass sets HarfBuzz's scale to the face's em in 26.6 at the run's size
//! and reads positions back in 26.6; `kern` is on only when the script says
//! `Kerning: yes`, `liga`/`clig` are off when letter spacing is non-zero.

use harfrust::{Buffer, Direction, Feature, Language, Script, ShapeOptions, ShaperFont, Tag};
use unicode_script::UnicodeScript;

use super::fonts::Face;

/// One output glyph, positions in 26.6 at the run's font size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shaped {
    /// Index of the cluster's first character within the run.
    pub cluster: usize,
    pub glyph: u32,
    pub x_advance: i32,
    pub y_advance: i32,
    pub x_offset: i32,
    pub y_offset: i32,
}

pub struct RunOptions {
    pub rtl: bool,
    /// The run's script (libass splits runs by script).
    pub script: Option<Script>,
    /// BCP 47 language the font's language system is chosen by.
    pub language: Option<&'static str>,
    /// `@Font`: vertical substitutions (`vert`, `vkna`).
    pub vertical: bool,
    pub kerning: bool,
    pub ligatures: bool,
}

/// Shape `text` set in `face` at `size` px (libass REAL_DIM size).
pub fn shape_run(face: &Face, size: f64, text: &[char], opts: &RunOptions) -> Vec<Shaped> {
    let em_d6 = (face.units_per_em as f64 * face.units_to_d6(size)).round() as i32;
    let shaper = ShaperFont::new(&face.font).with_scale(em_d6.max(1));
    let mut buf = Buffer::new();
    for (i, &c) in text.iter().enumerate() {
        buf.push(c as u32, i as u32);
    }
    buf.set_direction(if opts.rtl { Direction::RightToLeft } else { Direction::LeftToRight });
    if let Some(s) = opts.script.filter(|s| *s != Script::COMMON && *s != Script::INHERITED && *s != Script::UNKNOWN) {
        buf.set_script(Some(s));
    }
    buf.set_language(opts.language.and_then(Language::new));
    buf.guess_segment_properties();
    let features = [
        Feature::new(Tag::new(b"kern"), opts.kerning as u32, ..),
        Feature::new(Tag::new(b"liga"), opts.ligatures as u32, ..),
        Feature::new(Tag::new(b"clig"), opts.ligatures as u32, ..),
        Feature::new(Tag::new(b"vert"), opts.vertical as u32, ..),
        Feature::new(Tag::new(b"vkna"), opts.vertical as u32, ..),
    ];
    if harfrust::shape(&shaper, &mut buf, ShapeOptions::new().features(&features)).is_err() {
        // Malformed font tables: lay the characters out by nominal advance.
        return text
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let glyph = face.glyph(c as u32).unwrap_or(0);
                let adv = super::outline::unit_advance(face, glyph) * face.units_to_d6(size);
                Shaped { cluster: i, glyph, x_advance: adv.round() as i32, y_advance: 0, x_offset: 0, y_offset: 0 }
            })
            .collect();
    }
    buf.glyph_infos()
        .iter()
        .zip(buf.glyph_positions())
        .map(|(info, pos)| Shaped {
            cluster: info.cluster as usize,
            glyph: info.glyph_id,
            x_advance: pos.x_advance,
            y_advance: pos.y_advance,
            x_offset: pos.x_offset,
            y_offset: pos.y_offset,
        })
        .collect()
}

/// The Unicode script of a character, as a HarfBuzz script.
pub fn char_script(c: u32) -> Script {
    let Some(ch) = char::from_u32(c) else { return Script::UNKNOWN };
    let name = ch.script().short_name().as_bytes();
    if name.len() != 4 {
        return Script::UNKNOWN;
    }
    Script::from_iso15924_tag(Tag::new(&[name[0], name[1], name[2], name[3]])).unwrap_or(Script::UNKNOWN)
}

/// libass `script_to_language`: a representative language for a script
/// (Pango's table), so its OpenType language system gets used.
pub fn script_language(s: Script) -> Option<&'static str> {
    let t = s.tag().to_be_bytes();
    Some(match &t {
        b"Arab" => "ar",
        b"Armn" => "hy",
        b"Beng" => "bn",
        b"Cans" => "iu",
        b"Cher" => "chr",
        b"Copt" => "cop",
        b"Cyrl" => "ru",
        b"Deva" => "hi",
        b"Geor" => "ka",
        b"Grek" => "el",
        b"Gujr" => "gu",
        b"Guru" => "pa",
        b"Hang" => "ko",
        b"Hebr" => "he",
        b"Hira" => "ja",
        b"Knda" => "kn",
        b"Kana" => "ja",
        b"Laoo" => "lo",
        b"Latn" => "en",
        b"Mlym" => "ml",
        b"Mong" => "mn",
        b"Orya" => "or",
        b"Syrc" => "syr",
        b"Taml" => "ta",
        b"Telu" => "te",
        b"Thai" => "th",
        b"Tibt" => "bo",
        b"Ethi" => "am",
        b"Khmr" => "km",
        b"Mymr" => "my",
        b"Sinh" => "si",
        b"Thaa" => "dv",
        b"Buhd" => "bku",
        b"Hano" => "hnn",
        b"Tglg" => "tl",
        b"Tagb" => "tbw",
        b"Ugar" => "uga",
        b"Bugi" => "bug",
        b"Xpeo" => "peo",
        b"Sylo" => "syl",
        b"Nkoo" => "nko",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderers::subtitle::ass::fonts::{builtin_faces, FontDb};

    #[test]
    fn shapes_latin_with_advances_in_26_6() {
        let db = FontDb::new(builtin_faces());
        let face = db.face(0);
        let text: Vec<char> = "Hi".chars().collect();
        let out = shape_run(face, 64.0, &text, &RunOptions { rtl: false, script: None, language: None, vertical: false, kerning: false, ligatures: true });
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].cluster, 0);
        assert_eq!(out[1].cluster, 1);
        assert!(out[0].x_advance > 10 * 64 && out[0].x_advance < 64 * 64);
    }
}
