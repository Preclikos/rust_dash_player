//! The faces ASS text is set in, and libass's rules for picking one.
//!
//! A script names fonts by family (`Open Sans Semibold`, `Trebuchet MS`)
//! plus a weight and slant. libass resolves that against every face it
//! knows the way GDI did for VSFilter, and this is a port of those rules
//! (libass/ass_fontselect.c `find_font` / `font_attributes_similarity`,
//! ass_font.c `set_font_metrics` / `ass_face_get_weight` /
//! `ass_font_get_index`):
//!
//! * a face matches when one of its family names (name ID 1, Windows
//!   platform) equals the request case-insensitively — then the closest
//!   weight/slant within the family wins — or when its full name (ID 4) or
//!   PostScript name does, which wins outright;
//! * the face must have the requested glyph; a font keeps a list of faces
//!   and adds another one whenever a character is missing from all of them;
//! * vertical metrics are GDI's: `usWinAscent`/`usWinDescent`, so a font
//!   size is the distance between those two lines.
//!
//! Where libass then asks the system (fontconfig, DirectWrite) for a
//! fallback, there is no system here: the faces are the built-in ones, the
//! script's `[Fonts]` and whatever the host added. The fallback is the
//! built-in Roboto/DejaVu, and as a last resort any face that has the glyph.

use std::collections::HashMap;
use std::sync::Arc;

use skrifa::raw::{FileRef, FontRef, TableProvider};

/// Font file bytes: compiled in, or owned and shared.
#[derive(Clone)]
pub enum FontBytes {
    Static(&'static [u8]),
    Shared(Arc<Vec<u8>>),
}

impl FontBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            FontBytes::Static(b) => b,
            FontBytes::Shared(b) => b.as_slice(),
        }
    }

    fn blob(&self) -> harfrust::font::Blob {
        match self {
            FontBytes::Static(b) => harfrust::font::Blob::from(*b),
            FontBytes::Shared(b) => {
                let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = b.clone();
                harfrust::font::Blob::from(shared)
            }
        }
    }
}

/// Where a face came from (for logging and priority).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceSource {
    /// Compiled into the player.
    Builtin,
    /// The script's `[Fonts]` section.
    Embedded,
    /// Handed in by the host (a fonts directory, a manifest download).
    Host,
}

/// One face of a font file, with what selection and layout need.
pub struct Face {
    pub bytes: FontBytes,
    pub index: u32,
    pub source: FaceSource,
    pub families: Vec<String>,
    pub fullnames: Vec<String>,
    pub postscript_name: Option<String>,
    /// CFF outlines (libass `ass_face_is_postscript`): changes the faux
    /// italic slant and which name a full-name match goes by.
    pub is_postscript: bool,
    /// libass `ass_face_get_weight`.
    pub weight: i32,
    /// OS/2 `fsSelection` italic / bold bits (FreeType style flags without OS/2).
    pub italic: bool,
    pub bold: bool,
    pub units_per_em: u16,
    /// GDI vertical metrics in font units (libass `set_font_metrics`):
    /// ascender above the baseline positive, descender below negative.
    pub ascender: i32,
    pub descender: i32,
    /// `post` underline (position ≤ 0, thickness > 0), font units.
    pub underline: Option<(i32, i32)>,
    /// OS/2 strikeout (position ≥ 0, size > 0), font units.
    pub strikeout: Option<(i32, i32)>,
    /// OS/2 `sTypoDescender` (0 without OS/2), for rotated vertical glyphs.
    pub typo_descender: i32,
    /// The same face for shaping (and charmap / advance queries).
    pub font: harfrust::font::Font,
}

impl Face {
    /// Glyph for `ch`, `None` when the face lacks it.
    pub fn glyph(&self, ch: u32) -> Option<u32> {
        self.font
            .charmap()
            .map_unicode(ch)
            .map(|g| g.to_u32())
            .filter(|&g| g != 0)
    }

    /// libass provider `check_glyph`: code 0 asks for nothing in particular.
    fn has_glyph(&self, code: u32) -> bool {
        code == 0 || self.glyph(code).is_some()
    }

    pub fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(self.bytes.as_slice(), self.index).ok()
    }

    /// Font units → 26.6 pixels for a face set to `size` px the way libass
    /// does (`FT_SIZE_REQUEST_TYPE_REAL_DIM`: ascender − descender = size).
    pub fn units_to_d6(&self, size: f64) -> f64 {
        let h = (self.ascender - self.descender) as f64;
        if h > 0.0 {
            size * 64.0 / h
        } else {
            size * 64.0 / self.units_per_em.max(1) as f64
        }
    }

    /// Display name for logs.
    pub fn name(&self) -> &str {
        self.postscript_name
            .as_deref()
            .or(self.families.first().map(String::as_str))
            .unwrap_or("?")
    }
}

/// Read every face in a font file. Faces that don't parse are skipped.
pub fn faces_from_bytes(bytes: FontBytes, source: FaceSource) -> Vec<Face> {
    let count = match FileRef::new(bytes.as_slice()) {
        Ok(file) => file.fonts().count() as u32,
        Err(_) => return Vec::new(),
    };
    (0..count).filter_map(|i| face_info(bytes.clone(), i, source)).collect()
}

fn face_info(bytes: FontBytes, index: u32, source: FaceSource) -> Option<Face> {
    let font_ref = FontRef::from_index(bytes.as_slice(), index).ok()?;
    let head = font_ref.head().ok()?;
    let os2 = font_ref.os2().ok();

    let mut families = Vec::new();
    let mut fullnames = Vec::new();
    let mut postscript_name = None;
    if let Ok(name) = font_ref.name() {
        let data = name.string_data();
        let records = || {
            name.name_record().iter().filter_map(move |rec| {
                let s: String = rec.string(data).ok()?.chars().collect();
                (!s.is_empty()).then(|| (rec.platform_id(), rec.name_id().to_u16(), s))
            })
        };
        // libass reads family and full names from the Windows platform only.
        for (platform, id, s) in records() {
            if platform != 3 {
                continue;
            }
            match id {
                1 if families.len() < 10 && !families.contains(&s) => families.push(s),
                4 if fullnames.len() < 10 && !fullnames.contains(&s) => fullnames.push(s),
                6 if postscript_name.is_none() => postscript_name = Some(s),
                _ => {}
            }
        }
        if postscript_name.is_none() {
            postscript_name = records().find(|r| r.1 == 6).map(|r| r.2);
        }
        // No Windows family name: whatever family name the font has.
        if families.is_empty() {
            families.extend(records().find(|r| r.1 == 1).map(|r| r.2));
        }
    }
    if families.is_empty() {
        return None;
    }

    // Style flags: from OS/2 when present (GDI ignores what FreeType mixes
    // in), else from head.macStyle.
    let (italic, bold) = match &os2 {
        Some(os2) => {
            let sel = os2.fs_selection().bits();
            (sel & 1 != 0, sel & (1 << 5) != 0)
        }
        None => {
            let mac = head.mac_style().bits();
            (mac & 2 != 0, mac & 1 != 0)
        }
    };
    let weight = match os2.as_ref().map(|o| o.us_weight_class()).unwrap_or(0) {
        0 => 300 * bold as i32 + 400,
        1 => 100,
        2 => 200,
        3 => 300,
        4 => 350,
        5 => 400,
        6 => 600,
        7 => 700,
        8 => 800,
        9 => 900,
        w => w as i32,
    };

    // libass set_font_metrics: GDI's Win metrics, else what FreeType would
    // use (typo with USE_TYPO_METRICS, else hhea), else typo, else bbox.
    let hhea = font_ref.hhea().ok();
    let (mut asc, mut desc) = hhea
        .as_ref()
        .map(|h| (h.ascender().to_i16() as i32, h.descender().to_i16() as i32))
        .unwrap_or((0, 0));
    if let Some(os2) = &os2 {
        let use_typo = os2.fs_selection().bits() & (1 << 7) != 0;
        if use_typo {
            asc = os2.s_typo_ascender() as i32;
            desc = os2.s_typo_descender() as i32;
        }
        let win_asc = os2.us_win_ascent() as i16 as i32;
        let win_desc = os2.us_win_descent() as i16 as i32;
        if win_asc + win_desc != 0 {
            asc = win_asc;
            desc = -win_desc;
        }
    }
    if asc - desc == 0 {
        match &os2 {
            Some(os2) if os2.s_typo_ascender() as i32 - os2.s_typo_descender() as i32 != 0 => {
                asc = os2.s_typo_ascender() as i32;
                desc = os2.s_typo_descender() as i32;
            }
            _ => {
                asc = head.y_max() as i32;
                desc = head.y_min() as i32;
            }
        }
    }

    let underline = font_ref.post().ok().and_then(|p| {
        let pos = p.underline_position().to_i16() as i32;
        let thick = p.underline_thickness().to_i16() as i32;
        (pos <= 0 && thick > 0).then_some((pos, thick))
    });
    let strikeout = os2.as_ref().and_then(|o| {
        let pos = o.y_strikeout_position() as i32;
        let size = o.y_strikeout_size() as i32;
        (pos >= 0 && size > 0).then_some((pos, size))
    });
    let is_postscript = font_ref.cff().is_ok() || font_ref.cff2().is_ok();
    let typo_descender = os2.as_ref().map_or(0, |o| o.s_typo_descender() as i32);
    let units_per_em = head.units_per_em();

    let font = harfrust::font::Font::new(bytes.blob(), index)?;
    Some(Face {
        bytes,
        index,
        source,
        families,
        fullnames,
        postscript_name,
        is_postscript,
        weight,
        italic,
        bold,
        units_per_em,
        ascender: asc,
        descender: desc,
        underline,
        strikeout,
        typo_descender,
        font,
    })
}

/// Every face the renderer may use, in priority order of addition.
#[derive(Clone, Default)]
pub struct FontDb {
    faces: Vec<Arc<Face>>,
}

/// The built-in faces (see `rasterizer.rs` for why these two).
pub fn builtin_faces() -> Vec<Face> {
    let mut faces = faces_from_bytes(FontBytes::Static(super::super::rasterizer::DEFAULT_FONT), FaceSource::Builtin);
    // The embedded Roboto is Android's static build, named "RobotoStatic";
    // scripts ask for "Roboto".
    for f in &mut faces {
        if f.families.iter().any(|n| n == "RobotoStatic") && !f.families.iter().any(|n| n == "Roboto") {
            f.families.push("Roboto".into());
        }
    }
    faces.extend(faces_from_bytes(FontBytes::Static(super::super::rasterizer::FALLBACK_FONT), FaceSource::Builtin));
    faces
}

/// libass `font_attributes_similarity`: lower is better.
fn attributes_similarity(face: &Face, weight: i32, italic: bool) -> u32 {
    let mut score = 0;
    if italic && !face.italic {
        score += 1;
    } else if !italic && face.italic {
        score += 4;
    }
    let mut a_weight = face.weight;
    // Faux bold makes a non-bold face count as heavier.
    if weight > face.weight + 150 && !face.bold {
        a_weight += 120;
    }
    score + (73 * (a_weight - weight).unsigned_abs()) / 256
}

fn matches_family(face: &Face, family: &str) -> bool {
    face.families.iter().any(|f| f.eq_ignore_ascii_case(family) || eq_unicode_ci(f, family))
}

/// libass `matches_full_or_postscript_name`.
fn matches_full_or_postscript(face: &Face, name: &str) -> bool {
    let full = face.fullnames.iter().any(|f| eq_unicode_ci(f, name));
    let ps = face.postscript_name.as_deref().is_some_and(|p| eq_unicode_ci(p, name));
    if full == ps {
        return full;
    }
    if face.is_postscript {
        ps
    } else {
        full
    }
}

/// `ass_strcasecmp` folds ASCII only; families are compared the same way,
/// but a lowercase compare of other letters costs nothing and keeps
/// localized names usable.
fn eq_unicode_ci(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.chars().zip(b.chars()).all(|(x, y)| x == y || x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

impl FontDb {
    #[cfg(test)]
    pub fn new(faces: impl IntoIterator<Item = Face>) -> Self {
        Self { faces: faces.into_iter().map(Arc::new).collect() }
    }

    pub fn add(&mut self, faces: impl IntoIterator<Item = Face>) {
        self.faces.extend(faces.into_iter().map(Arc::new));
    }

    pub fn add_shared(&mut self, faces: &[Arc<Face>]) {
        self.faces.extend(faces.iter().cloned());
    }

    pub fn face(&self, id: usize) -> &Arc<Face> {
        &self.faces[id]
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// libass `find_font` for one name: family members by attribute
    /// similarity, full/PostScript name matches immediately. Returns the
    /// face and whether the name matched anything at all.
    fn find(&self, name: &str, weight: i32, italic: bool, code: u32) -> (Option<usize>, bool) {
        let mut best = None;
        let mut best_score = u32::MAX;
        let mut name_match = false;
        for (i, face) in self.faces.iter().enumerate() {
            let score = if matches_family(face, name) {
                name_match = true;
                attributes_similarity(face, weight, italic)
            } else if matches_full_or_postscript(face, name) {
                name_match = true;
                0
            } else {
                continue;
            };
            if score < best_score {
                if !face.has_glyph(code) {
                    continue;
                }
                best_score = score;
                best = Some(i);
            }
            if score == 0 {
                break;
            }
        }
        (best, name_match)
    }

    /// libass `ass_font_select`: the requested family, then the built-in
    /// families, then any face with the glyph.
    pub fn select(&self, family: &str, weight: i32, italic: bool, code: u32) -> Option<usize> {
        if !family.is_empty() {
            if let (Some(i), _) = self.find(family, weight, italic, code) {
                return Some(i);
            }
        }
        // Fallback families: the built-in faces in order.
        let mut tried: Vec<&str> = Vec::new();
        for face in self.faces.iter().filter(|f| f.source == FaceSource::Builtin) {
            let fam = face.families[0].as_str();
            if tried.contains(&fam) {
                continue;
            }
            tried.push(fam);
            if let (Some(i), _) = self.find(fam, weight, italic, code) {
                return Some(i);
            }
        }
        if code == 0 {
            return (!self.faces.is_empty()).then_some(0);
        }
        self.faces.iter().position(|f| f.has_glyph(code))
    }
}

/// What a script asks for: libass `ASS_FontDesc`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FontDesc {
    pub family: String,
    /// 400 normal, 700 bold, else an explicit weight.
    pub bold: i32,
    /// 0 or 100.
    pub italic: i32,
    /// `@Family`: vertical layout.
    pub vertical: bool,
}

impl FontDesc {
    /// libass `ass_update_font`: `\b`/`\i` values → weight / slant.
    pub fn new(family: &str, bold: i32, italic: i32) -> Self {
        let (family, vertical) = match family.strip_prefix('@') {
            Some(f) => (f, true),
            None => (family, false),
        };
        let bold = if bold == 1 || bold == -1 {
            700
        } else if bold <= 0 {
            400
        } else {
            bold
        };
        let italic = if italic == 1 {
            100
        } else if italic <= 0 {
            0
        } else {
            italic
        };
        Self { family: family.to_string(), bold, italic, vertical }
    }
}

/// libass `ASS_FONT_MAX_FACES`.
const MAX_FACES: usize = 10;

/// A requested font: the faces chosen for it so far (libass `ASS_Font`).
pub struct Font {
    pub desc: FontDesc,
    pub faces: Vec<usize>,
}

impl Font {
    fn new(db: &FontDb, desc: FontDesc) -> Option<Self> {
        let face = db.select(&desc.family, desc.bold, desc.italic > 0, 0)?;
        Some(Self { desc, faces: vec![face] })
    }

    /// libass `ass_font_get_index`: the font's face that has `symbol`
    /// (adding one if none does) and the glyph id there; glyph 0 when no
    /// face has it.
    pub fn glyph(&mut self, db: &FontDb, symbol: u32) -> (usize, u32) {
        if symbol < 0x20 || self.faces.is_empty() {
            return (0, 0);
        }
        for (i, &f) in self.faces.iter().enumerate() {
            if let Some(g) = db.face(f).glyph(symbol) {
                return (i, g);
            }
        }
        if self.faces.len() < MAX_FACES {
            if let Some(f) = db.select(&self.desc.family, self.desc.bold, self.desc.italic > 0, symbol) {
                let i = match self.faces.iter().position(|&x| x == f) {
                    Some(i) => i,
                    None => {
                        self.faces.push(f);
                        self.faces.len() - 1
                    }
                };
                return (i, db.face(f).glyph(symbol).unwrap_or(0));
            }
        }
        (0, 0)
    }
}

/// Fonts by description, built on first use (libass font cache).
#[derive(Default)]
pub struct FontCache {
    fonts: Vec<Font>,
    by_desc: HashMap<FontDesc, Option<usize>>,
}

impl FontCache {
    pub fn get(&mut self, db: &FontDb, desc: FontDesc) -> Option<usize> {
        if let Some(&id) = self.by_desc.get(&desc) {
            return id;
        }
        let id = Font::new(db, desc.clone()).map(|font| {
            log::debug!(
                "[subs/ass] font {:?} b{} i{} -> {}",
                font.desc.family,
                font.desc.bold,
                font.desc.italic,
                db.face(font.faces[0]).name()
            );
            self.fonts.push(font);
            self.fonts.len() - 1
        });
        self.by_desc.insert(desc, id);
        id
    }

    pub fn font(&self, id: usize) -> &Font {
        &self.fonts[id]
    }

    pub fn font_mut(&mut self, id: usize) -> &mut Font {
        &mut self.fonts[id]
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> FontDb {
        FontDb::new(builtin_faces())
    }

    #[test]
    fn builtin_faces_parse_with_gdi_metrics() {
        let db = db();
        assert_eq!(db.len(), 2);
        let roboto = db.face(0);
        assert!(roboto.families.iter().any(|f| f == "Roboto"), "{:?}", roboto.families);
        assert_eq!(roboto.weight, 400);
        assert!(!roboto.italic && !roboto.bold);
        assert!(roboto.ascender > 0 && roboto.descender < 0);
        assert!(roboto.underline.is_some());
        let dejavu = db.face(1);
        assert!(dejavu.families.iter().any(|f| f == "DejaVu Sans"), "{:?}", dejavu.families);
        assert!(dejavu.glyph(0x266A).is_some());
        assert!(roboto.glyph(0x266A).is_none());
    }

    #[test]
    fn selection_by_family_full_name_and_fallback() {
        let db = db();
        assert_eq!(db.select("roboto", 400, false, 0), Some(0));
        assert_eq!(db.select("DejaVu Sans", 700, false, 0), Some(1));
        // Unknown family: the first built-in.
        assert_eq!(db.select("Trebuchet MS", 400, false, 'a' as u32), Some(0));
        // A glyph only the fallback has.
        assert_eq!(db.select("Trebuchet MS", 400, false, 0x266A), Some(1));
    }

    #[test]
    fn a_font_adds_faces_for_missing_glyphs() {
        let db = db();
        let mut cache = FontCache::default();
        let id = cache.get(&db, FontDesc::new("Roboto", 0, 0)).unwrap();
        let font = cache.font_mut(id);
        let (face, glyph) = font.glyph(&db, 'A' as u32);
        assert_eq!(face, 0);
        assert_ne!(glyph, 0);
        let (face, glyph) = font.glyph(&db, 0x266A);
        assert_eq!(face, 1);
        assert_ne!(glyph, 0);
        assert_eq!(font.faces, vec![0, 1]);
    }

    #[test]
    fn desc_maps_bold_and_italic_like_libass() {
        assert_eq!(FontDesc::new("A", -1, 1).bold, 700);
        assert_eq!(FontDesc::new("A", 0, 0).bold, 400);
        assert_eq!(FontDesc::new("A", 600, 0).bold, 600);
        assert_eq!(FontDesc::new("A", 0, 1).italic, 100);
        let v = FontDesc::new("@Arial", 0, 0);
        assert!(v.vertical);
        assert_eq!(v.family, "Arial");
    }
}
