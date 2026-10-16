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
