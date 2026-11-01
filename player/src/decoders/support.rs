//! Decoder support probe: drop representations this platform cannot decode
//! BEFORE track selection and ABR see them, so the default pick, the auto
//! switch and the host's track list only ever name playable rungs.
//!
//! Browser: WebCodecs `VideoDecoder.isConfigSupported` /
//! `AudioDecoder.isConfigSupported`, asked per representation with the
//! manifest's `codecs` string (`hvc1.2.4.L150.90`, `mp4a.40.2`, `ec-3`, …).
//! This is the browser's own answer — no JS glue in the host, and no
//! maintained table of what each browser can do.
//!
//! Native (Android, Windows, Linux): the hardware decoder the player will
//! actually use is asked about every video representation (codec, 10-bit
//! profile, picture size and, on Android, frame rate), like ExoPlayer's
//! renderer capabilities: Android `MediaCodecList` (the decoder
//! `createDecoderByType` picks), Windows the D3D11 video device on the
//! renderer's GPU, Linux VAAPI. These platforms decode in hardware only, so
//! a rung the decoder cannot do (4K on a 1080p tablet) would otherwise fail
//! at configure time, mid-playback. Apple keeps the manifest as-is.

use crate::tracks::Tracks;

/// Remove unsupported representations (and adaptations left empty) from
/// `tracks`. Logs what it dropped. Never removes the last video or audio
/// representation on a probe FAILURE (API missing, promise rejected) — only
/// on a definite "not supported".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn prune_unsupported(tracks: &mut Tracks) {
    #[cfg(any(target_os = "android", target_os = "windows", target_os = "linux"))]
    native::prune_video(tracks);
    #[cfg(not(any(target_os = "android", target_os = "windows", target_os = "linux")))]
    let _ = tracks;
}

/// What a video representation asks of the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VideoNeed {
    pub codec: VideoCodecNeed,
    /// HEVC Main10 (and Dolby Vision's base layer) / H.264 High 10.
    pub ten_bit: bool,
    pub width: u32,
    pub height: u32,
    /// Frames per second x1000; 0 = unknown.
    pub fps_milli: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum VideoCodecNeed {
    /// H.264 with `profile_idc` from the codecs string (66 baseline, 77
    /// main, 100 high, 110 high 10), 0 = unknown.
    Avc { profile_idc: u8 },
    Hevc,
}

#[cfg_attr(not(any(target_os = "android", target_os = "windows", target_os = "linux")), allow(dead_code))]
impl VideoNeed {
    /// From the manifest's `codecs` (`hvc1.2.4.L150.90`, `avc1.640028`,
    /// `dvh1.08.06`), picture size and frame rate. `None` for a codec the
    /// probes do not know (it is kept; configure has the final word).
    pub(crate) fn from_codecs(codecs: &str, width: u32, height: u32, fps: Option<f32>) -> Option<Self> {
        let codecs = codecs.trim().to_ascii_lowercase();
        let mut parts = codecs.split('.');
        let fourcc = parts.next()?;
        let (codec, ten_bit) = match fourcc {
            "hvc1" | "hev1" => {
                // A general_profile_space letter (A-C) may prefix the idc.
                let idc = parts.next().map(|p| p.trim_start_matches(|c: char| c.is_ascii_alphabetic()));
                (VideoCodecNeed::Hevc, idc == Some("2"))
            }
            // Dolby Vision decodes through its HEVC base layer (or the
            // platform DV decoder, on the same hardware).
            "dvh1" | "dvhe" => (VideoCodecNeed::Hevc, true),
            "avc1" | "avc3" => {
                let profile_idc = parts
                    .next()
                    .and_then(|p| p.get(0..2))
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .unwrap_or(0);
                (VideoCodecNeed::Avc { profile_idc }, profile_idc == 110)
            }
            _ => return None,
        };
        if width == 0 || height == 0 {
            return None;
        }
        let fps_milli = fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| (f * 1000.0).round() as u32)
            .unwrap_or(0);
        Some(VideoNeed { codec, ten_bit, width, height, fps_milli })
    }

    pub(crate) fn fps(&self) -> Option<f64> {
        (self.fps_milli > 0).then(|| self.fps_milli as f64 / 1000.0)
    }
}

impl std::fmt::Display for VideoNeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.codec {
            VideoCodecNeed::Hevc => write!(f, "HEVC")?,
            VideoCodecNeed::Avc { profile_idc } => write!(f, "H.264 profile {profile_idc}")?,
        }
        write!(f, "{} {}x{}", if self.ten_bit { " 10-bit" } else { "" }, self.width, self.height)?;
        if let Some(fps) = self.fps() {
            write!(f, "@{fps:.3}")?;
        }
        Ok(())
    }
}

#[cfg(any(target_os = "android", target_os = "windows", target_os = "linux"))]
mod native {
    use super::VideoNeed;
    use crate::tracks::Tracks;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Answers per process: the decoders do not change while it runs, and a
    /// probe costs a device (D3D11 / VAAPI) or a codec-list walk (Android).
    /// Only definite answers are kept; a failed probe is asked again.
    static CACHE: Mutex<Option<HashMap<VideoNeed, bool>>> = Mutex::new(None);

    /// Remove the video representations the hardware decoder cannot play.
    /// Leaves the tree untouched when the probe would leave no video at all:
    /// a probe wrong about every rung must not stop playback that configure
    /// might still manage (that failure then surfaces as before).
    pub(super) fn prune_video(tracks: &mut Tracks) {
        let t0 = crate::rt::Instant::now();
        let mut probe = Probe::default();
        let mut verdicts: Vec<Vec<Option<bool>>> = Vec::new();
        for adapt in &tracks.video {
            let fps = adapt.fps().map(|f| f.as_f32());
            verdicts.push(
                adapt
                    .representations
                    .iter()
                    .map(|r| {
                        let need = VideoNeed::from_codecs(&r.codecs, r.width, r.height, fps)?;
                        probe.supports(&need)
                    })
                    .collect(),
            );
        }
        let total: usize = verdicts.iter().map(|v| v.len()).sum();
        let unsupported = verdicts.iter().flatten().filter(|v| **v == Some(false)).count();
        let unknown = verdicts.iter().flatten().filter(|v| v.is_none()).count();
        let ms = t0.elapsed().as_millis();
        if unsupported == 0 {
            if unknown == 0 {
                log::info!("[support] all {total} video representation(s) decodable here ({} probe, {ms} ms)", probe.name());
            } else {
                log::info!(
                    "[support] {unknown} of {total} video representation(s) could not be checked ({} probe), all kept",
                    probe.name()
                );
            }
            return;
        }
        if unsupported == total {
            log::warn!(
                "[support] the {} probe rejects every video representation; keeping them all for the decoder to decide",
                probe.name()
            );
            return;
        }
        let mut dropped = Vec::new();
        for (adapt, verdict) in tracks.video.iter_mut().zip(verdicts) {
            let mut keep = Vec::with_capacity(adapt.representations.len());
            for (r, v) in adapt.representations.drain(..).zip(verdict) {
                if v == Some(false) {
                    dropped.push(format!("{}x{} {} ({} kbps)", r.width, r.height, r.codecs, r.bandwidth / 1000));
                } else {
                    keep.push(r);
                }
            }
            adapt.representations = keep;
        }
        tracks.video.retain(|a| !a.representations.is_empty());
        log::info!(
            "[support] removed {} video representation(s) this device's decoder cannot play ({} probe, {ms} ms): {}",
            dropped.len(),
            probe.name(),
            dropped.join("; ")
        );
    }

    /// The platform probe. Windows / Linux open their device on first use
    /// and close it with the pass.
    #[derive(Default)]
    struct Probe {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        device: Option<Option<crate::decoders::ffmpeg_hw::VideoProbe>>,
    }

    impl Probe {
        fn name(&self) -> &'static str {
            if cfg!(target_os = "android") {
                "MediaCodec"
            } else if cfg!(target_os = "windows") {
                "D3D11VA"
            } else {
                "VAAPI"
            }
        }

        fn supports(&mut self, need: &VideoNeed) -> Option<bool> {
            if let Some(known) = CACHE.lock().unwrap().as_ref().and_then(|c| c.get(need).copied()) {
                return Some(known);
            }
            let answer = self.ask(need);
            match answer {
                Some(ok) => {
                    log::info!("[support] {need}: {}", if ok { "supported" } else { "NOT supported" });
                    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(*need, ok);
                }
                None => log::info!("[support] {need}: probe failed, kept"),
            }
            answer
        }

        #[cfg(target_os = "android")]
        fn ask(&mut self, need: &VideoNeed) -> Option<bool> {
            crate::decoders::mediacodec::probe_video(need)
        }

        #[cfg(any(target_os = "windows", target_os = "linux"))]
        fn ask(&mut self, need: &VideoNeed) -> Option<bool> {
            self.device
                .get_or_insert_with(crate::decoders::ffmpeg_hw::VideoProbe::open)
                .as_ref()?
                .supports(need)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{VideoCodecNeed, VideoNeed};

    #[test]
    fn hevc_main10_is_ten_bit() {
        let n = VideoNeed::from_codecs("hvc1.2.4.L150.90", 3840, 2160, Some(23.976)).unwrap();
        assert_eq!(n.codec, VideoCodecNeed::Hevc);
        assert!(n.ten_bit);
        assert_eq!(n.fps_milli, 23976);
    }

    #[test]
    fn hevc_main_is_eight_bit() {
        let n = VideoNeed::from_codecs("hev1.1.6.L93.B0", 1280, 720, None).unwrap();
        assert!(!n.ten_bit);
        assert_eq!(n.fps(), None);
    }

    #[test]
    fn hevc_profile_space_prefix() {
        assert!(VideoNeed::from_codecs("hvc1.A2.4.L150", 1920, 1080, None).unwrap().ten_bit);
    }

    #[test]
    fn dolby_vision_needs_hevc_main10() {
        let n = VideoNeed::from_codecs("dvh1.08.06", 3840, 2160, None).unwrap();
        assert_eq!(n.codec, VideoCodecNeed::Hevc);
        assert!(n.ten_bit);
    }

    #[test]
    fn avc_profile_from_hex() {
        let n = VideoNeed::from_codecs("avc1.640028", 1920, 1080, Some(25.0)).unwrap();
        assert_eq!(n.codec, VideoCodecNeed::Avc { profile_idc: 100 });
        assert!(!n.ten_bit);
        assert!(VideoNeed::from_codecs("avc1.6E0028", 1920, 1080, None).unwrap().ten_bit);
        assert_eq!(
            VideoNeed::from_codecs("AVC1.42E01E", 640, 360, None).unwrap().codec,
            VideoCodecNeed::Avc { profile_idc: 66 }
        );
    }

    #[test]
    fn unknown_codec_or_size_is_not_probed() {
        assert!(VideoNeed::from_codecs("av01.0.08M.10", 3840, 2160, None).is_none());
        assert!(VideoNeed::from_codecs("vp09.00.10.08", 1920, 1080, None).is_none());
        assert!(VideoNeed::from_codecs("hvc1.1.6.L93", 0, 0, None).is_none());
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn prune_unsupported(tracks: &mut Tracks) {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    async fn video_supported(codecs: &str, width: u32, height: u32) -> Option<bool> {
        let cfg = web_sys::VideoDecoderConfig::new(codecs);
        if width > 0 && height > 0 {
            cfg.set_coded_width(width);
            cfg.set_coded_height(height);
        }
        let support: web_sys::VideoDecoderSupport =
            JsFuture::from(web_sys::VideoDecoder::is_config_supported(&cfg)).await.ok()?.unchecked_into();
        support.get_supported()
    }

    async fn audio_supported(codecs: &str, channels: u32, sample_rate: u32) -> Option<bool> {
        let cfg = web_sys::AudioDecoderConfig::new(codecs, channels.max(1), sample_rate.max(8_000));
        let support: web_sys::AudioDecoderSupport =
            JsFuture::from(web_sys::AudioDecoder::is_config_supported(&cfg)).await.ok()?.unchecked_into();
        support.get_supported()
    }

    let mut dropped: Vec<String> = Vec::new();

    for adapt in &mut tracks.video {
        let mut keep = Vec::with_capacity(adapt.representations.len());
        for r in adapt.representations.drain(..) {
            match video_supported(&r.codecs, r.width, r.height).await {
                Some(false) => dropped.push(format!("video {}x{} {}", r.width, r.height, r.codecs)),
                // Supported, or the probe itself failed: keep and let the
                // decoder's configure() have the final word.
                _ => keep.push(r),
            }
        }
        adapt.representations = keep;
    }
    tracks.video.retain(|a| !a.representations.is_empty());

    for adapt in &mut tracks.audio {
        let mut keep = Vec::with_capacity(adapt.representations.len());
        for r in adapt.representations.drain(..) {
            let ch = r.channels.unwrap_or(2);
            match audio_supported(&r.codecs, ch, r.audio_sampling_rate).await {
                Some(false) => dropped.push(format!("audio {} {}ch {}Hz", r.codecs, ch, r.audio_sampling_rate)),
                _ => keep.push(r),
            }
        }
        adapt.representations = keep;
    }
    tracks.audio.retain(|a| !a.representations.is_empty());

    if dropped.is_empty() {
        log::info!("[support] every representation is decodable here (WebCodecs isConfigSupported)");
    } else {
        log::info!(
            "[support] {} representation(s) this browser cannot decode were removed: {}",
            dropped.len(),
            dropped.join("; ")
        );
    }
}
