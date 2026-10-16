//! Number parsing the way libass reads ASS fields and tag arguments.
//!
//! ASS has no grammar for numbers beyond "whatever VSFilter's `scanf` /
//! `strtol` accepted", and real files lean on that leniency (`\be.25`,
//! `\fs+2`, `&H00FFFFFF`, `&HFF&`, trailing junk after a value). These are
//! ports of the libass helpers (`mystrtoi32`, `mystrtod`,
//! `mystrtou32_modulo`, `parse_int_header`, `parse_bool`), so a value
//! parses to exactly what libass makes of it: a prefix that is not a
//! number reads as 0, an over-long integer saturates (tags) or wraps
//! (header fields) the way the C code does.

/// `skip_spaces` in libass: space and tab only.
pub(super) fn is_ass_space(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// C `isspace`, what `strtoll`/`strtod` skip.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn digit_value(b: u8, base: u32) -> Option<u32> {
    let v = match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'z' => (b - b'a') as u32 + 10,
        b'A'..=b'Z' => (b - b'A') as u32 + 10,
        _ => return None,
    };
    (v < base).then_some(v)
}

/// C `strtoll(s, &end, base)` for base 10 or 16: the value (saturated at
/// the `i64` range) and how many bytes were consumed; `(0, 0)` when no
/// digits were found.
fn strtoll(s: &[u8], base: u32) -> (i64, usize) {
    let mut i = 0;
    while i < s.len() && is_c_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negative = s[i] == b'-';
        i += 1;
    }
    // "0x" only counts when a hex digit follows; otherwise strtoll parses
    // the "0" and stops at the "x".
    if base == 16
        && s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x') | Some(b'X'))
        && s.get(i + 2).and_then(|&b| digit_value(b, 16)).is_some()
    {
        i += 2;
    }
    let start = i;
    let mut value: i128 = 0;
    while let Some(d) = s.get(i).and_then(|&b| digit_value(b, base)) {
        value = (value * base as i128 + d as i128).min(i64::MAX as i128 + 1);
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    let value = if negative { -value } else { value };
    (value.clamp(i64::MIN as i128, i64::MAX as i128) as i64, i)
}

/// libass `mystrtoi32`: `strtoll` clamped to the `i32` range; 0 when the
/// text does not start with a number.
pub(super) fn strtoi32(s: &[u8], base: u32) -> i32 {
    strtoll(s, base).0.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// libass `ass_strtod` / `mystrtod`: a decimal floating-point prefix
/// (`[ws][sign]digits[.digits][e[sign]digits]`, either side of the point
/// may be empty but not both); 0 when there is none.
pub(super) fn strtod(s: &[u8]) -> f64 {
    let mut i = 0;
    while i < s.len() && is_c_space(s[i]) {
        i += 1;
    }
    let start = i;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - int_start;
    if i < s.len() && s[i] == b'.' {
        let frac_start = i + 1;
        let mut j = frac_start;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        digits += j - frac_start;
        if digits > 0 {
            i = j;
        }
    }
    if digits == 0 {
        return 0.0;
    }
    // Exponent only when at least one digit follows it.
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    std::str::from_utf8(&s[start..i])
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// libass `dtoi32`: truncate toward zero, `i32::MIN` for NaN / out of
/// range.
pub(super) fn dtoi32(v: f64) -> i32 {
    if v.is_nan() || v <= i32::MIN as f64 || v >= i32::MAX as f64 + 1.0 {
        i32::MIN
    } else {
        v as i32
    }
}

/// libass `mystrtou32_modulo`: like `strtoul` but reduced modulo 2³²
/// instead of saturating (VSFilter's `%d`/`%x` `scanf` behaviour).
fn strtou32_modulo(s: &[u8], base: u32) -> u32 {
    let mut i = 0;
    while i < s.len() && is_ass_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negative = s[i] == b'-';
        i += 1;
    }
    if base == 16 && s.len() >= i + 2 && s[i..i + 2].eq_ignore_ascii_case(b"0x") {
        i += 2;
    }
    let start = i;
    let mut value: u32 = 0;
    while let Some(d) = s.get(i).and_then(|&b| digit_value(b, base)) {
        value = value.wrapping_mul(base).wrapping_add(d);
        i += 1;
    }
    if i == start {
        return 0;
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// libass `parse_int_header`: decimal, or hex after `&H` / `0x`.
pub(super) fn parse_int_header(s: &str) -> i32 {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[..2].eq_ignore_ascii_case(b"&h") || b[..2].eq_ignore_ascii_case(b"0x")) {
        strtou32_modulo(&b[2..], 16) as i32
    } else {
        strtou32_modulo(b, 10) as i32
    }
}

/// libass `parse_color_header`: `&HAABBGGRR` (or decimal) → libass RGBA
/// (`0xRRGGBBAA`, alpha 0 = opaque).
pub(super) fn parse_color_header(s: &str) -> u32 {
    (parse_int_header(s) as u32).swap_bytes()
}

/// libass `parse_bool`: "yes…" or a positive integer.
pub(super) fn parse_bool(s: &str) -> bool {
    let t = s.trim_start_matches([' ', '\t']);
    let b = t.as_bytes();
    (b.len() >= 3 && b[..3].eq_ignore_ascii_case(b"yes")) || strtoll(b, 10).0 > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_read_a_prefix_and_clamp() {
        assert_eq!(strtoi32(b"12.7", 10), 12);
        assert_eq!(strtoi32(b"  -5px", 10), -5);
        assert_eq!(strtoi32(b"abc", 10), 0);
        assert_eq!(strtoi32(b"99999999999", 10), i32::MAX);
        assert_eq!(strtoi32(b"FF", 16), 255);
        assert_eq!(strtoi32(b"0x1F", 16), 31);
        // Colour tags may carry eight hex digits: strtoll then clamps.
        assert_eq!(strtoi32(b"FFFFFFFF", 16), i32::MAX);
    }

    #[test]
    fn floats_accept_the_forms_files_use() {
        assert_eq!(strtod(b".25"), 0.25);
        assert_eq!(strtod(b"1.5e2x"), 150.0);
        assert_eq!(strtod(b"3e"), 3.0);
        assert_eq!(strtod(b"-.5"), -0.5);
        assert_eq!(strtod(b"."), 0.0);
        assert_eq!(strtod(b""), 0.0);
        assert_eq!(strtod(b"+2"), 2.0);
    }

    #[test]
    fn header_integers_wrap_like_vsfilter() {
        assert_eq!(parse_int_header("&H00FFFFFF"), 0x00FF_FFFF);
        assert_eq!(parse_int_header("&HFF000000") as u32, 0xFF00_0000);
        assert_eq!(parse_int_header("-1"), -1);
        assert_eq!(parse_int_header("4294967297"), 1);
        assert_eq!(parse_color_header("&H00FFFFFF"), 0xFFFF_FF00);
        assert_eq!(parse_color_header("&H64000000"), 0x0000_0064);
    }

    #[test]
    fn booleans() {
        assert!(parse_bool(" yes"));
        assert!(parse_bool("Yes"));
        assert!(parse_bool("1"));
        assert!(!parse_bool("no"));
        assert!(!parse_bool("0"));
    }

    #[test]
    fn dtoi32_truncates_and_flags_out_of_range() {
        assert_eq!(dtoi32(24.9), 24);
        assert_eq!(dtoi32(-0.75), 0);
        assert_eq!(dtoi32(f64::NAN), i32::MIN);
        assert_eq!(dtoi32(3e10), i32::MIN);
    }
}
