use std::sync::Arc;

use super::*;
use crate::parsers::ass::parse;

fn script(header_extra: &str, styles: &str, events: &str) -> Arc<AssScript> {
    Arc::new(
        parse(&format!(
            "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\nScaledBorderAndShadow: yes\n{header_extra}\n\
             [V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Default,Roboto,30,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,20,20,20,1\n{styles}\n\
             [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{events}"
        ))
        .unwrap(),
    )
}

const FRAME: Frame = Frame { width: 1280, height: 720, par: 1.0, font_size_coeff: 1.0, storage: None };

fn render(s: &Arc<AssScript>, t: i64) -> Vec<Image> {
    let mut r = AssRenderer::new();
    let active: Vec<usize> = (0..s.events.len()).collect();
    r.render(s, &active, t, FRAME)
}

fn bounds(imgs: &[Image]) -> (i32, i32, i32, i32) {
    let c = composite(imgs, FRAME.width, FRAME.height).expect("something visible");
    (c.left, c.top, c.left + c.width as i32, c.top + c.height as i32)
}

#[test]
fn default_dialogue_sits_bottom_centre_with_shadow_border_fill() {
    let s = script("", "", "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,Hello world\n");
    let imgs = render(&s, 1000);
    // shadow, border, fill — one run each.
    assert_eq!(imgs.len(), 3, "{imgs:?}");
    assert_eq!(imgs[0].color, 0x0000_0080);
    assert_eq!(imgs[1].color, 0x0000_0000);
    assert_eq!(imgs[2].color, 0xFFFF_FF00);
    let (l, t, r, b) = bounds(&imgs);
    let cx = (l + r) / 2;
    assert!((cx - 640).abs() < 8, "centred: {l}..{r}");
    // MarginV 20 at 360 → 40 px at 720; the text ends right above it
    // (plus border/shadow).
    assert!(b <= 720 - 40 + 8 && b > 600, "bottom {b}");
    assert!(t < b);
}

#[test]
fn pos_and_alignment_place_the_text() {
    let s = script("", "", "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\an7\\pos(100,50)\\bord0\\shad0}Top left\n");
    let imgs = render(&s, 1000);
    assert_eq!(imgs.len(), 1);
    let (l, t, _, _) = bounds(&imgs);
    // (100, 50) at 2× scale = (200, 100): the box's top-left corner.
    assert!((l - 200).abs() <= 3, "left {l}");
    assert!(t >= 100 && t < 120, "top {t}");
}

#[test]
fn nothing_outside_the_event_time() {
    let s = script("", "", "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\n");
    assert!(render(&s, 500).is_empty());
    assert!(!render(&s, 1500).is_empty());
    assert!(render(&s, 2000).is_empty());
}

#[test]
fn fade_in_scales_alpha() {
    let s = script("", "", "Dialogue: 0,0:00:00.00,0:00:04.00,Default,,0,0,0,,{\\fad(1000,0)\\bord0\\shad0}x\n");
    let a0 = render(&s, 0);
    assert!(a0.is_empty(), "fully transparent at the start");
    let half = render(&s, 500);
    let a = half[0].color & 0xFF;
    assert!((120..=135).contains(&a), "half faded: {a:#x}");
    let full = render(&s, 2000);
    assert_eq!(full[0].color & 0xFF, 0);
}

#[test]
fn transform_animates_colour() {
    let s = script("", "", "Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,{\\bord0\\shad0\\c&H000000&\\t(0,1000,\\c&HFFFFFF&)}x\n");
    let c = |t| render(&s, t)[0].color;
    assert_eq!(c(0), 0x0000_0000);
    assert_eq!(c(500) >> 8, 0x7F7F7F);
    assert_eq!(c(1500), 0xFFFF_FF00);
}

#[test]
fn colliding_dialogue_stacks_upward() {
    let s = script(
        "",
        "",
        "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,First line\n\
         Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,Second line\n",
    );
    let mut r = AssRenderer::new();
    let imgs = r.render(&s, &[0, 1], 1000, FRAME);
    let fills: Vec<&Image> = imgs.iter().filter(|i| i.color == 0xFFFF_FF00).collect();
    assert_eq!(fills.len(), 2);
    // The first (in file order) keeps the bottom; the second moves above it.
    assert!(fills[1].dst_y + fills[1].h <= fills[0].dst_y + 4, "{} {}", fills[0].dst_y, fills[1].dst_y);
}

#[test]
fn karaoke_sweep_splits_the_fill() {
    let s = script("", "", "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\bord0\\shad0\\kf100}wide syllable\n");
    let imgs = render(&s, 500);
    // Left part primary, right part secondary.
    assert_eq!(imgs.len(), 2, "{imgs:?}");
    assert_eq!(imgs[0].color, 0xFFFF_FF00);
    assert_eq!(imgs[1].color, 0xFF00_0000);
    assert_eq!(imgs[0].dst_x + imgs[0].w, imgs[1].dst_x);
}

#[test]
fn rect_clip_and_drawing() {
    let s = script(
        "",
        "",
        "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\an7\\pos(0,0)\\bord0\\shad0\\p1}m 0 0 l 100 0 100 100 0 100{\\p0}\n\
         Dialogue: 1,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\an7\\pos(200,0)\\bord0\\shad0\\clip(200,0,250,50)\\p1}m 0 0 l 100 0 100 100 0 100\n",
    );
    let imgs = render(&s, 1000);
    assert_eq!(imgs.len(), 2);
    // 100×100 drawing units at 2× = 200×200 px.
    assert_eq!((imgs[0].dst_x, imgs[0].dst_y, imgs[0].w, imgs[0].h), (0, 0, 200, 200));
    // Clipped to 50×50 script px = 100×100 px.
    assert_eq!((imgs[1].dst_x, imgs[1].dst_y, imgs[1].w, imgs[1].h), (400, 0, 100, 100));
}

#[test]
fn real_fansub_frames_render() {
    for (file, t) in [
        ("Mushoku Tensei S3 - 01_Subtitles01.ENG.ass", 165_000),
        ("Mushoku Tensei S3 - 01_Subtitles02.CZE.ass", 165_000),
        ("Mushoku Tensei S3 - 01_Subtitles02.CZE.ass", 170_000),
    ] {
        let path = format!("{}/../docs/{file}", env!("CARGO_MANIFEST_DIR"));
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let s = Arc::new(parse(&body).unwrap());
        let mut r = AssRenderer::new();
        let active: Vec<usize> = (0..s.events.len()).collect();
        let imgs = r.render(&s, &active, t, Frame { width: 1920, height: 1080, par: 1.0, font_size_coeff: 1.0, storage: None });
        assert!(!imgs.is_empty(), "{file} @ {t}");
    }
}

/// Dev/golden helper: render one frame to a PPM over black.
/// `ASS_DUMP="script.ass;time_ms;width;height;out.ppm"`, optional
/// `ASS_FONTS=<dir>` (every .ttf/.otf/.ttc in it is added).
/// `cargo test -p player --lib dump_frame -- --ignored --nocapture`
#[test]
#[ignore]
fn dump_frame() {
    let Ok(spec) = std::env::var("ASS_DUMP") else { return };
    let parts: Vec<&str> = spec.split(';').collect();
    let (path, t, w, h, out) = (parts[0], parts[1].parse::<i64>().unwrap(), parts[2].parse::<i32>().unwrap(), parts[3].parse::<i32>().unwrap(), parts[4]);
    let body = std::fs::read(path).unwrap();
    let parsed = crate::parsers::sidecar::parse(&body, crate::parsers::sidecar::SubtitleFormat::Ass, None, 0).unwrap();
    let s = parsed.script.unwrap();
    let mut r = AssRenderer::new();
    if let Ok(dir) = std::env::var("ASS_FONTS") {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
            if ["ttf", "otf", "ttc"].contains(&ext.as_str()) {
                r.add_font(std::fs::read(&p).unwrap());
            }
        }
    }
    let active: Vec<usize> = (0..s.events.len()).collect();
    let started = std::time::Instant::now();
    let imgs = r.render(&s, &active, t, Frame { width: w, height: h, par: 1.0, font_size_coeff: 1.0, storage: Some((w, h)) });
    println!("{} images in {:?}", imgs.len(), started.elapsed());
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    if let Some(c) = composite(&imgs, w, h) {
        for y in 0..c.height as i32 {
            for x in 0..c.width as i32 {
                let si = ((y * c.width as i32 + x) * 4) as usize;
                let di = (((c.top + y) * w + c.left + x) * 3) as usize;
                rgb[di..di + 3].copy_from_slice(&c.rgba[si..si + 3]);
            }
        }
    }
    ppm.extend(rgb);
    std::fs::write(out, ppm).unwrap();
}

/// Benchmark: render every frame of a script like the overlay does
/// (active events per frame, render + composite).
/// `ASS_BENCH="script.ass;width;height;fps;from_ms;to_ms"`, optional
/// `ASS_FONTS=<dir>`. Run in release:
/// `cargo test --release -p player --lib bench_script -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_script() {
    let Ok(spec) = std::env::var("ASS_BENCH") else { return };
    let p: Vec<&str> = spec.split(';').collect();
    let (path, w, h, fps) = (p[0], p[1].parse::<i32>().unwrap(), p[2].parse::<i32>().unwrap(), p[3].parse::<f64>().unwrap());
    let body = std::fs::read(path).unwrap();
    let t0 = std::time::Instant::now();
    let parsed = crate::parsers::sidecar::parse(&body, crate::parsers::sidecar::SubtitleFormat::Ass, None, 0).unwrap();
    let s = parsed.script.unwrap();
    let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let from = p.get(4).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let to = p.get(5).and_then(|v| v.parse::<i64>().ok()).unwrap_or_else(|| s.events.iter().map(|e| e.end_ms()).max().unwrap_or(0));
    let mut r = AssRenderer::new();
    if let Ok(dir) = std::env::var("ASS_FONTS") {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
            if ["ttf", "otf", "ttc"].contains(&ext.as_str()) {
                r.add_font(std::fs::read(e.path()).unwrap());
            }
        }
    }
    let frame = super::frame_for(&s, w as u32, h as u32, Some((w as u32, h as u32)), 1.0);
    let mut times = Vec::new();
    let (mut max_events, mut max_imgs, mut max_px) = (0usize, 0usize, 0u64);
    let (mut worst_t, mut worst) = (0i64, 0f64);
    let mut comp_total = 0f64;
    let mut t = from as f64;
    while (t as i64) < to {
        let ms = t as i64;
        let active: Vec<usize> = s.events.iter().enumerate().filter(|(_, e)| e.start_ms <= ms && ms < e.end_ms()).map(|(i, _)| i).collect();
        let a = std::time::Instant::now();
        let imgs = r.render(&s, &active, ms, frame);
        let b = std::time::Instant::now();
        // what the overlay does: masks for the GPU (CPU flatten only when
        // the atlas doesn't fit; ASS_BENCH_CPU=1 forces the flatten)
        let c = if std::env::var("ASS_BENCH_CPU").is_ok() {
            composite(&imgs, w, h).map(|c| (c.width, c.height))
        } else {
            super::layers(&imgs, w, h).map(|l| (l.width, l.height)).or_else(|| composite(&imgs, w, h).map(|c| (c.width, c.height)))
        };
        let total = a.elapsed().as_secs_f64() * 1000.0;
        comp_total += b.elapsed().as_secs_f64() * 1000.0;
        if total > worst {
            worst = total;
            worst_t = ms;
        }
        times.push(total);
        max_events = max_events.max(active.len());
        max_imgs = max_imgs.max(imgs.len());
        max_px = max_px.max(c.map_or(0, |(cw, ch)| cw as u64 * ch as u64));
        t += 1000.0 / fps;
    }
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len().max(1);
    let avg = times.iter().sum::<f64>() / n as f64;
    let pct = |q: f64| sorted[((n as f64 - 1.0) * q) as usize];
    let budget = 1000.0 / fps;
    let over = times.iter().filter(|&&v| v > budget).count();
    println!(
        "{path}: {} events, parse {parse_ms:.0} ms | {n} frames {w}x{h}@{fps} | avg {avg:.2} ms, p50 {:.2}, p95 {:.2}, p99 {:.2}, max {worst:.2} ms @ {worst_t} ms | composite share {:.0}% | over {budget:.1} ms budget: {over} ({:.1}%) | peak {max_events} events, {max_imgs} images, {:.1} Mpx",
        s.events.len(),
        pct(0.5),
        pct(0.95),
        pct(0.99),
        comp_total / times.iter().sum::<f64>().max(1e-9) * 100.0,
        over as f64 / n as f64 * 100.0,
        max_px as f64 / 1e6
    );
    let (lookups, hits) = r.composite_lookups();
    println!("  composite cache: {hits} hits of {lookups} runs ({:.0}%)", hits as f64 / lookups.max(1) as f64 * 100.0);
}

/// libass's own regression suite (github.com/libass/libass-tests):
/// every `regression/<case>/<script>-<ms>.png` against our frame, over
/// grey, box-blurred 3×3 like the golden test. Prints mean |Δ| and the
/// share of pixels off by more than 64 per case; with `ASS_REGRESSION_OUT`
/// writes ours | reference side by side (PPM).
/// `ASS_REGRESSION=<libass-tests>/regression cargo test --release -p player --lib libass_regression -- --ignored --nocapture`
#[test]
#[ignore]
fn libass_regression() {
    let Ok(root) = std::env::var("ASS_REGRESSION") else { return };
    let root = std::path::PathBuf::from(root);
    let out_dir = std::env::var("ASS_REGRESSION_OUT").ok().map(std::path::PathBuf::from);
    let fonts_in = |dir: &std::path::Path, r: &mut AssRenderer| {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
            if ["ttf", "otf", "ttc"].contains(&ext.as_str()) {
                r.add_font(std::fs::read(e.path()).unwrap());
            }
        }
    };
    const BG: u32 = 0x80;
    let blur3 = |rgb: &[u8], w: usize, h: usize| -> Vec<u8> {
        let mut out = vec![0u8; rgb.len()];
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let mut s = 0u32;
                    for dy in [-1i32, 0, 1] {
                        for dx in [-1i32, 0, 1] {
                            let xx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                            let yy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                            s += rgb[(yy * w + xx) * 3 + c] as u32;
                        }
                    }
                    out[(y * w + x) * 3 + c] = (s / 9) as u8;
                }
            }
        }
        out
    };
    let mut cases: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.ends_with(".fonts")).collect();
    cases.sort();
    let mut rows = Vec::new();
    for case in cases {
        let mut pngs: Vec<_> = std::fs::read_dir(&case).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "png")).collect();
        pngs.sort();
        for png_path in pngs {
            let stem = png_path.file_stem().unwrap().to_string_lossy().to_string();
            let Some((script_name, ms)) = stem.rsplit_once('-') else { continue };
            let Ok(ms) = ms.parse::<i64>() else { continue };
            let Ok(body) = std::fs::read(case.join(format!("{script_name}.ass"))) else { continue };
            let Ok(parsed) = crate::parsers::sidecar::parse(&body, crate::parsers::sidecar::SubtitleFormat::Ass, None, 0) else {
                rows.push(format!("{:<40} parse failed", format!("{}/{stem}", case.file_name().unwrap().to_string_lossy())));
                continue;
            };
            let s = parsed.script.unwrap();
            let mut dec = png::Decoder::new(std::fs::File::open(&png_path).unwrap());
            dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
            let mut reader = dec.read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut buf).unwrap();
            let (w, h, ch) = (info.width as usize, info.height as usize, info.color_type.samples());
            // reference: straight alpha over grey
            let mut refc = vec![0u8; w * h * 3];
            for (i, px) in buf[..info.buffer_size()].chunks(ch).enumerate() {
                let a = if ch == 4 { px[3] as u32 } else { 255 };
                for k in 0..3 {
                    refc[i * 3 + k] = ((px[k] as u32 * a + BG * (255 - a) + 127) / 255) as u8;
                }
            }
            let mut r = AssRenderer::new();
            fonts_in(&case, &mut r);
            fonts_in(&root.join(".fonts"), &mut r);
            let active: Vec<usize> = (0..s.events.len()).collect();
            let frame = super::frame_for(&s, w as u32, h as u32, Some((w as u32, h as u32)), 1.0);
            let imgs = r.render(&s, &active, ms, frame);
            let mut mine = vec![BG as u8; w * h * 3];
            if let Some(c) = composite(&imgs, w as i32, h as i32) {
                for y in 0..c.height as usize {
                    for x in 0..c.width as usize {
                        let si = (y * c.width as usize + x) * 4;
                        let di = ((c.top as usize + y) * w + c.left as usize + x) * 3;
                        let a = c.rgba[si + 3] as u32;
                        for k in 0..3 {
                            mine[di + k] = ((c.rgba[si + k] as u32 * 255 + BG * (255 - a) + 127) / 255).min(255) as u8;
                        }
                    }
                }
            }
            let (ba, bb) = (blur3(&mine, w, h), blur3(&refc, w, h));
            let (mut covered, mut sum, mut bad) = (0usize, 0f64, 0usize);
            for i in 0..w * h {
                let p = |v: &[u8]| (0..3).any(|k| v[i * 3 + k] as u32 != BG);
                if !p(&ba) && !p(&bb) {
                    continue;
                }
                covered += 1;
                let d: Vec<i32> = (0..3).map(|k| (ba[i * 3 + k] as i32 - bb[i * 3 + k] as i32).abs()).collect();
                sum += d.iter().sum::<i32>() as f64 / 3.0;
                if d.iter().any(|&v| v > 64) {
                    bad += 1;
                }
            }
            let mean = sum / covered.max(1) as f64;
            let bad_pct = bad as f64 / covered.max(1) as f64 * 100.0;
            let verdict = if mean <= 4.0 && bad_pct <= 0.5 { "ok" } else { "DIFF" };
            let name = format!("{}/{stem}", case.file_name().unwrap().to_string_lossy());
            rows.push(format!("{name:<40} {w}x{h} mean {mean:5.2} off {bad_pct:5.2}% {verdict}"));
            if let Some(dir) = &out_dir {
                let mut ppm = format!("P6\n{} {h}\n255\n", w * 2).into_bytes();
                for y in 0..h {
                    ppm.extend_from_slice(&mine[y * w * 3..(y + 1) * w * 3]);
                    ppm.extend_from_slice(&refc[y * w * 3..(y + 1) * w * 3]);
                }
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(format!("{}.ppm", name.replace('/', "_"))), ppm).unwrap();
            }
        }
    }
    for r in &rows {
        println!("{r}");
    }
}
