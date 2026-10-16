//! Golden tests: our frames against libass's.
//!
//! `player/tests/ass_golden/*.ass` are rendered at the times listed in
//! `frames.txt` and compared with `ref/<script>_<ms>.png`, which
//! `scripts/ass_golden.sh` renders with libass (ffmpeg's `subtitles`
//! filter) from the same fonts, over mid grey so black borders and shadows
//! count. Pixel-exact equality is not the goal — libass rasterizes with its
//! own antialiasing, strokes borders with its own offsetter and
//! approximates the gaussian with a cascade filter — so a frame passes when
//! the two agree except for edge noise. Both are box-blurred 3×3 first
//! (antialiasing noise averages out, a misplaced or missing shape does
//! not); then over the pixels that differ from the background in either:
//!
//! * the mean absolute difference,
//! * the share of pixels off by more than a quarter of the range (wrong
//!   colour, missing or extra shapes), and
//! * per 48 px tile with enough ink, how far the ink's centre of mass moved
//!   (misplaced text, wrong advances) and how much ink there is (wrong
//!   size, weight, border or blur).
//!
//! `ASS_GOLDEN_OUT=<dir>` writes ours / reference / amplified difference
//! for every frame (PPM) to look at.

use std::path::PathBuf;
use std::sync::Arc;

use super::{composite, AssRenderer, Frame};

const W: i32 = 960;
const H: i32 = 540;
const BG: u8 = 0x80;
const TILE: i32 = 48;

/// Mean |Δ| per channel (0..255) over covered pixels, after the blur.
const MAX_MEAN: f64 = 4.0;
/// Covered pixels with some channel off by more than 64, after the blur.
const MAX_BAD_SHARE: f64 = 0.005;
/// Centre-of-mass shift of the ink within a tile, px. (Advances agree to
/// ~1/200 px per glyph; over a 900 px line that adds up to ~0.5 px.)
const MAX_TILE_SHIFT: f64 = 0.6;
/// Ink of a tile relative to the reference's.
const MAX_TILE_INK: f64 = 0.08;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ass_golden")
}

fn decode_png(path: &std::path::Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut dec = png::Decoder::new(file);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let ch = info.color_type.samples();
    let mut rgb = Vec::with_capacity((info.width * info.height * 3) as usize);
    for px in buf[..info.buffer_size()].chunks(ch) {
        rgb.extend_from_slice(&px[..3]);
    }
    Some(rgb)
}

fn ours(script: &Arc<crate::parsers::ass::AssScript>, renderer: &mut AssRenderer, ms: i64) -> Vec<u8> {
    let active: Vec<usize> = (0..script.events.len()).collect();
    // ffmpeg's subtitles filter sets the storage size to the video's.
    let frame: Frame = super::frame_for(script, W as u32, H as u32, Some((W as u32, H as u32)), 1.0);
    let imgs = renderer.render(script, &active, ms, frame);
    let mut rgb = vec![BG; (W * H * 3) as usize];
    if let Some(c) = composite(&imgs, W, H) {
        for y in 0..c.height as i32 {
            for x in 0..c.width as i32 {
                let s = ((y * c.width as i32 + x) * 4) as usize;
                let d = (((c.top + y) * W + c.left + x) * 3) as usize;
                // premultiplied over the background
                let a = c.rgba[s + 3] as u32;
                for k in 0..3 {
                    rgb[d + k] = ((c.rgba[s + k] as u32 * 255 + BG as u32 * (255 - a) + 127) / 255).min(255) as u8;
                }
            }
        }
    }
    rgb
}

struct Stats {
    covered: usize,
    mean: f64,
    bad_share: f64,
    worst_shift: f64,
    worst_ink: f64,
}

fn blur3(rgb: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgb.len()];
    for y in 0..H {
        for x in 0..W {
            for c in 0..3 {
                let mut sum = 0u32;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (xx, yy) = ((x + dx).clamp(0, W - 1), (y + dy).clamp(0, H - 1));
                        sum += rgb[((yy * W + xx) * 3 + c) as usize] as u32;
                    }
                }
                out[((y * W + x) * 3 + c) as usize] = (sum / 9) as u8;
            }
        }
    }
    out
}

/// How far a pixel is from the background (the "ink").
fn ink(p: &[u8]) -> f64 {
    p.iter().map(|&c| (c as i32 - BG as i32).unsigned_abs() as f64).sum()
}

fn compare(a: &[u8], b: &[u8]) -> Stats {
    // Per tile: ink mass and centre of mass, unblurred.
    let (mut worst_shift, mut worst_ink) = (0f64, 0f64);
    for ty in (0..H).step_by(TILE as usize) {
        for tx in (0..W).step_by(TILE as usize) {
            let mut m = [[0f64; 3]; 2];
            for (k, img) in [a, b].iter().enumerate() {
                for y in ty..(ty + TILE).min(H) {
                    for x in tx..(tx + TILE).min(W) {
                        let i = ((y * W + x) * 3) as usize;
                        // Clear ink only: a large, nearly transparent area a
                        // blend rounding apart would otherwise drag the centre.
                        let v = ink(&img[i..i + 3]);
                        let v = if v >= 48.0 { v } else { 0.0 };
                        m[k][0] += v * x as f64;
                        m[k][1] += v * y as f64;
                        m[k][2] += v;
                    }
                }
            }
            // At least ~60 fully inked pixels in the reference.
            if m[1][2] < 60.0 * 3.0 * 128.0 {
                continue;
            }
            let ca = (m[0][0] / m[0][2].max(1.0), m[0][1] / m[0][2].max(1.0));
            let cb = (m[1][0] / m[1][2], m[1][1] / m[1][2]);
            let shift = ((ca.0 - cb.0).powi(2) + (ca.1 - cb.1).powi(2)).sqrt();
            let inkd = (m[0][2] / m[1][2] - 1.0).abs();
            if std::env::var("ASS_GOLDEN_VERBOSE").is_ok() && (shift > MAX_TILE_SHIFT || inkd > MAX_TILE_INK) {
                println!("  tile ({tx},{ty}): moved {shift:.2} px, ink {:+.1}%", inkd * 100.0);
            }
            worst_shift = worst_shift.max(shift);
            worst_ink = worst_ink.max(inkd);
        }
    }
    let (a, b) = (blur3(a), blur3(b));
    let (mut covered, mut sum, mut bad) = (0usize, 0u64, 0usize);
    for (pa, pb) in a.chunks(3).zip(b.chunks(3)) {
        if ink(pa) < 8.0 && ink(pb) < 8.0 {
            continue;
        }
        covered += 1;
        let d: Vec<u32> = pa.iter().zip(pb).map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs()).collect();
        sum += d.iter().map(|&v| v as u64).sum::<u64>();
        if d.iter().any(|&v| v > 64) {
            bad += 1;
        }
    }
    let n = covered.max(1) as f64;
    Stats { covered, mean: sum as f64 / (3.0 * n), bad_share: bad as f64 / n, worst_shift, worst_ink }
}

fn write_ppm(path: PathBuf, rgb: &[u8]) {
    let mut out = format!("P6\n{W} {H}\n255\n").into_bytes();
    out.extend_from_slice(rgb);
    let _ = std::fs::write(path, out);
}

#[test]
fn frames_match_libass() {
    let list = std::fs::read_to_string(dir().join("frames.txt")).expect("frames.txt");
    let out_dir = std::env::var("ASS_GOLDEN_OUT").ok().map(PathBuf::from);
    let mut failures = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut it = line.split_whitespace();
        let name = it.next().unwrap();
        let body = std::fs::read(dir().join(format!("{name}.ass"))).expect("script");
        let parsed = crate::parsers::sidecar::parse(&body, crate::parsers::sidecar::SubtitleFormat::Ass, None, 0).unwrap();
        let script = parsed.script.unwrap();
        for ms in it.map(|t| t.parse::<i64>().unwrap()) {
            // A fresh renderer per frame: libass renders each reference
            // frame without history, so collisions start from scratch too.
            let mut renderer = AssRenderer::new();
            let mine = ours(&script, &mut renderer, ms);
            let Some(reference) = decode_png(&dir().join(format!("ref/{name}_{ms}.png"))) else {
                failures.push(format!("{name}@{ms}: reference missing (scripts/ass_golden.sh)"));
                continue;
            };
            assert_eq!(reference.len(), mine.len(), "{name}@{ms}: reference size");
            let s = compare(&mine, &reference);
            println!(
                "{name}@{ms}: {} px covered, mean |d| {:.2}, {:.2}% off by >64, worst tile moved {:.2} px / ink {:.1}%",
                s.covered,
                s.mean,
                s.bad_share * 100.0,
                s.worst_shift,
                s.worst_ink * 100.0
            );
            if let Some(dir) = &out_dir {
                let _ = std::fs::create_dir_all(dir);
                write_ppm(dir.join(format!("{name}_{ms}_ours.ppm")), &mine);
                write_ppm(dir.join(format!("{name}_{ms}_ref.ppm")), &reference);
                let diff: Vec<u8> = mine.iter().zip(&reference).map(|(&a, &b)| ((a as i32 - b as i32).unsigned_abs() * 4).min(255) as u8).collect();
                write_ppm(dir.join(format!("{name}_{ms}_diff.ppm")), &diff);
            }
            if s.mean > MAX_MEAN || s.bad_share > MAX_BAD_SHARE || s.worst_shift > MAX_TILE_SHIFT || s.worst_ink > MAX_TILE_INK {
                failures.push(format!(
                    "{name}@{ms}: mean {:.2} (max {MAX_MEAN}), off {:.2}% (max {:.1}%), tile moved {:.2} px (max {MAX_TILE_SHIFT}), tile ink {:.1}% (max {:.0}%)",
                    s.mean,
                    s.bad_share * 100.0,
                    MAX_BAD_SHARE * 100.0,
                    s.worst_shift,
                    s.worst_ink * 100.0,
                    MAX_TILE_INK * 100.0
                ));
            }
        }
    }
    assert!(failures.is_empty(), "golden mismatches:\n{}", failures.join("\n"));
}
