//! `[Fonts]` section: fonts embedded in the script, uuencoded.
//!
//! The encoding is SSA's own uuencode variant (libass `decode_font`):
//! every character carries six bits as `c - 33`, four characters make
//! three bytes, and a final group of two or three characters makes one or
//! two. Lines are concatenated without their line breaks.

/// A font file carried inside the script.
#[derive(Clone, PartialEq, Eq)]
pub struct EmbeddedFont {
    /// The `fontname:` the script gave it, usually the original file name
    /// with an `_0` style suffix (`arialbd_0.ttf`). Not the family name:
    /// font lookup goes by the names inside the font file.
    pub name: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for EmbeddedFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedFont")
            .field("name", &self.name)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Decode the concatenated data lines of one font. `None` for a length
/// that can't be valid (one character left over).
pub(super) fn decode(encoded: &[u8]) -> Option<Vec<u8>> {
    if encoded.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3 + 2);
    for group in encoded.chunks(4) {
        let mut value: u32 = 0;
        for (i, &c) in group.iter().enumerate() {
            value |= ((c.wrapping_sub(33) & 63) as u32) << (6 * (3 - i));
        }
        out.push((value >> 16) as u8);
        if group.len() >= 3 {
            out.push((value >> 8) as u8);
        }
        if group.len() >= 4 {
            out.push(value as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
pub(super) fn encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let mut value: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            value |= (b as u32) << (8 * (2 - i));
        }
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push((((value >> (18 - 6 * i)) & 63) as u8 + 33) as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_tail_length() {
        for len in 0..10 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(decode(encode(&data).as_bytes()).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn rejects_a_dangling_character() {
        assert!(decode(b"!!!!!").is_none());
    }
}
