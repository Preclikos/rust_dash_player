//! Playback rate: pitch-preserving time stretch (WSOLA) and the map from
//! audio OUTPUT time to MEDIA time the master clock needs once the two stop
//! advancing in step.
//!
//! Built for small, continuous corrections (a watch-party follower catching
//! up 2-5 % faster or slower), not for 2x "chipmunk" playback — although the
//! algorithm works across the whole [`MIN_RATE`]..=[`MAX_RATE`] range.
//!
//! # Zero cost at 1.0
//!
//! Nothing here runs until a rate other than 1.0 is set: `audio_sync_loop`
//! hands PCM straight to the sink and [`RateMap::media_us`] is the identity
//! while the map is not engaged — the exact code path (and the exact integer
//! arithmetic) playback had before rates existed. Once a pipeline engaged the
//! stretcher it keeps running for that pipeline even back at 1.0 (WSOLA at
//! 1.0 is a perfect overlap-add reconstruction); the next rebuild (seek,
//! track switch) starts on the bypass again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

/// Slowest supported rate.
pub const MIN_RATE: f64 = 0.5;
/// Fastest supported rate.
pub const MAX_RATE: f64 = 2.0;

/// Clamp a requested rate into the supported range; NaN / infinities -> 1.0.
pub fn clamp_rate(rate: f64) -> f64 {
    if !rate.is_finite() {
        return 1.0;
    }
    rate.clamp(MIN_RATE, MAX_RATE)
}

/// The rate the host asked for, shared player-wide. Stored as parts per
/// million so it fits an atomic; 0 (the `Default`) reads as 1.0.
#[derive(Default)]
pub(crate) struct RateControl {
    ppm: AtomicU32,
}

impl RateControl {
    pub(crate) fn get(&self) -> f64 {
        match self.ppm.load(Ordering::Relaxed) {
            0 => 1.0,
            ppm => ppm as f64 / 1_000_000.0,
        }
    }

    pub(crate) fn set(&self, rate: f64) {
        let ppm = (clamp_rate(rate) * 1_000_000.0).round() as u32;
        // 1.0 is stored as 0 so `get` returns the literal 1.0 (the bypass
        // compares against it exactly).
        let ppm = if ppm == 1_000_000 { 0 } else { ppm };
        self.ppm.store(ppm, Ordering::Relaxed);
    }
}

/// Per-pipeline map from audio output position to media position.
///
/// The master clock reads how much audio the device has PLAYED; with a rate
/// other than 1.0, 1 s played is `rate` s of media. The stretcher records an
/// anchor (output frame, input frame, rate) for every hop it emits, and the
/// clock converts through them. Until engaged it is the identity.
pub(crate) struct RateMap {
    engaged: AtomicBool,
    sample_rate: u32,
    /// Rate the stretcher is applying right now (f64 bits as ppm, 0 = 1.0);
    /// what the video pacing scales its waits by.
    applied_ppm: AtomicU32,
    /// (output frame, input frame, rate from here on), ascending.
    anchors: Mutex<VecDeque<(u64, f64, f64)>>,
}

/// Anchors kept at most (≈ 15 s of 30 ms hops) if nothing reads the map.
const MAX_ANCHORS: usize = 1024;

impl RateMap {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            engaged: AtomicBool::new(false),
            sample_rate: sample_rate.max(1),
            applied_ppm: AtomicU32::new(0),
            anchors: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
    }

    /// Rate the audio is being stretched at; exactly 1.0 when not engaged.
    pub(crate) fn applied_rate(&self) -> f64 {
        match self.applied_ppm.load(Ordering::Relaxed) {
            0 => 1.0,
            ppm => ppm as f64 / 1_000_000.0,
        }
    }

    fn record(&self, out_frame: u64, in_frame: f64, rate: f64) {
        let mut a = self.anchors.lock().unwrap();
        if a.len() >= MAX_ANCHORS {
            a.pop_front();
        }
        a.push_back((out_frame, in_frame, rate));
        drop(a);
        let ppm = (rate * 1_000_000.0).round() as u32;
        self.applied_ppm
            .store(if ppm == 1_000_000 { 0 } else { ppm }, Ordering::Relaxed);
        self.engaged.store(true, Ordering::Relaxed);
    }

    /// Media time (µs since this pipeline's start) for an output time (µs
    /// of audio played since the flush). Identity while not engaged — and
    /// for times before the first anchor, which by construction is where
    /// output and media were still in step.
    pub(crate) fn media_us(&self, out_us: i64) -> i64 {
        if !self.is_engaged() || out_us <= 0 {
            return out_us;
        }
        let sr = self.sample_rate as f64;
        let out_f = out_us as f64 * sr / 1e6;
        let mut a = self.anchors.lock().unwrap();
        // Drop anchors the clock has moved past (it only moves forward
        // within a pipeline; keep the last one at/below `out_f`).
        while a.len() >= 2 && (a[1].0 as f64) <= out_f {
            a.pop_front();
        }
        let in_f = match (a.front(), a.get(1)) {
            (None, _) => out_f,
            (Some(&(o0, i0, _)), _) if out_f < o0 as f64 => i0 - (o0 as f64 - out_f),
            (Some(&(o0, i0, _)), Some(&(o1, i1, _))) => {
                let t = (out_f - o0 as f64) / (o1 - o0).max(1) as f64;
                i0 + t * (i1 - i0)
            }
            (Some(&(o0, i0, r)), None) => i0 + (out_f - o0 as f64) * r,
        };
        (in_f * 1e6 / sr).round() as i64
    }
}

/// Waveform-similarity overlap-add time stretcher over interleaved f32 PCM.
pub(crate) struct TimeStretch {
    ch: usize,
    /// Window length, frames.
    n: usize,
    /// Synthesis hop = n / 2 (50 % overlap, periodic Hann sums to 1).
    hs: usize,
    /// Search radius around the nominal analysis position, frames.
    delta: usize,
    window: Vec<f32>,
    /// Buffered input, interleaved; `input[0]` is absolute frame `in_base`.
    input: Vec<f32>,
    in_base: u64,
    /// Nominal (fractional) absolute input frame of the next window.
    ana_pos: f64,
    /// Absolute input frame the previous window started at.
    prev_pos: Option<u64>,
    /// Windowed second half of the previous segment, waiting for overlap.
    ola: Vec<f32>,
    /// Absolute output frame of the next emitted frame.
    out_frame: u64,
}

impl TimeStretch {
    /// `in_frame` / `out_frame`: the absolute frame counters at the moment
    /// the stretcher takes over (equal when it engages mid-pipeline at 1.0).
    pub(crate) fn new(sample_rate: u32, channels: usize, in_frame: u64, out_frame: u64) -> Self {
        let sr = sample_rate.max(8_000) as usize;
        // 30 ms windows / 8 ms search: the usual speech+music compromise.
        let n = ((sr * 30 / 1000) / 2) * 2;
        let hs = n / 2;
        let delta = sr * 8 / 1000;
        let window = (0..n)
            .map(|i| {
                let x = std::f64::consts::PI * 2.0 * i as f64 / n as f64;
                (0.5 - 0.5 * x.cos()) as f32
            })
            .collect();
        Self {
            ch: channels.max(1),
            n,
            hs,
            delta,
            window,
            input: Vec::new(),
            in_base: in_frame,
            ana_pos: in_frame as f64,
            prev_pos: None,
            ola: vec![0.0; hs * channels.max(1)],
            out_frame,
        }
    }

    fn frames_buffered(&self) -> usize {
        self.input.len() / self.ch
    }

    /// Mono sample at absolute input frame `f` (must be buffered).
    #[inline]
    fn mono(&self, f: u64) -> f32 {
        let i = (f - self.in_base) as usize * self.ch;
        self.input[i..i + self.ch].iter().sum::<f32>()
    }

    /// Normalised cross-correlation of `len` frames at `a` and `b`, sampling
    /// every `step` frames.
    fn similarity(&self, a: u64, b: u64, len: usize, step: usize) -> f32 {
        let (mut dot, mut ea) = (0.0f32, 0.0f32);
        let mut k = 0;
        while k < len {
            let x = self.mono(a + k as u64);
            let y = self.mono(b + k as u64);
            dot += x * y;
            ea += x * x;
            k += step;
        }
        if ea <= 1e-9 {
            0.0
        } else {
            dot / ea.sqrt()
        }
    }

    /// Feed input, append stretched output to `out`, anchors to `map`.
    pub(crate) fn process(&mut self, samples: &[f32], rate: f64, out: &mut Vec<f32>, map: &RateMap) {
        let rate = clamp_rate(rate);
        self.input.extend_from_slice(samples);
        let ch = self.ch;
        loop {
            let end = self.in_base + self.frames_buffered() as u64;
            match self.prev_pos {
                None => {
                    // First window: emit its first half unwindowed so taking
                    // over mid-stream neither fades nor clicks.
                    let x = self.ana_pos.round().max(self.in_base as f64) as u64;
                    if x + self.n as u64 > end {
                        break;
                    }
                    map.record(self.out_frame, x as f64, rate);
                    let i0 = (x - self.in_base) as usize * ch;
                    out.extend_from_slice(&self.input[i0..i0 + self.hs * ch]);
                    for f in 0..self.hs {
                        let w = self.window[self.hs + f];
                        for c in 0..ch {
                            self.ola[f * ch + c] = self.input[i0 + (self.hs + f) * ch + c] * w;
                        }
                    }
                    self.out_frame += self.hs as u64;
                    self.prev_pos = Some(x);
                    self.ana_pos = x as f64 + rate * self.hs as f64;
                }
                Some(prev) => {
                    let natural = prev + self.hs as u64;
                    let nominal = self.ana_pos.round() as u64;
                    let lo = nominal.saturating_sub(self.delta as u64).max(self.in_base);
                    let hi = nominal + self.delta as u64;
                    if hi + self.n as u64 > end || natural + self.hs as u64 > end {
                        break;
                    }
                    // Coarse search (every 4th offset / sample), then refine.
                    let (mut best, mut best_s) = (nominal.max(lo), f32::MIN);
                    let mut x = lo;
                    while x <= hi {
                        let s = self.similarity(natural, x, self.hs, 4);
                        if s > best_s {
                            best_s = s;
                            best = x;
                        }
                        x += 4;
                    }
                    let (r_lo, r_hi) = (best.saturating_sub(3).max(lo), (best + 3).min(hi));
                    best_s = f32::MIN;
                    for x in r_lo..=r_hi {
                        let s = self.similarity(natural, x, self.hs, 1);
                        if s > best_s {
                            best_s = s;
                            best = x;
                        }
                    }
                    // At exactly 1.0 the natural continuation is the nominal
                    // position and correlates perfectly: prefer it outright
                    // so 1.0 is a bit-exact reconstruction.
                    if rate == 1.0 && nominal == natural {
                        best = natural;
                    }
                    let x = best;
                    map.record(self.out_frame, x as f64, rate);
                    let i0 = (x - self.in_base) as usize * ch;
                    let base = out.len();
                    out.resize(base + self.hs * ch, 0.0);
                    for f in 0..self.hs {
                        let w1 = self.window[f];
                        let w2 = self.window[self.hs + f];
                        for c in 0..ch {
                            out[base + f * ch + c] =
                                self.ola[f * ch + c] + self.input[i0 + f * ch + c] * w1;
                            self.ola[f * ch + c] = self.input[i0 + (self.hs + f) * ch + c] * w2;
                        }
                    }
                    self.out_frame += self.hs as u64;
                    self.prev_pos = Some(x);
                    self.ana_pos += rate * self.hs as f64;
                }
            }
            // Forget input no future window can reach.
            let keep_from = self
                .prev_pos
                .map(|p| p + self.hs as u64)
                .unwrap_or(self.in_base)
                .min((self.ana_pos.floor() as u64).saturating_sub(self.delta as u64))
                .max(self.in_base);
            let drop = (keep_from - self.in_base) as usize;
            if drop >= 4096 {
                self.input.drain(..drop * ch);
                self.in_base = keep_from;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: u32, ch: usize, secs: f64, hz: f64) -> Vec<f32> {
        let frames = (sr as f64 * secs) as usize;
        let mut v = Vec::with_capacity(frames * ch);
        for i in 0..frames {
            let s = (2.0 * std::f64::consts::PI * hz * i as f64 / sr as f64).sin() as f32 * 0.5;
            for _ in 0..ch {
                v.push(s);
            }
        }
        v
    }

    fn run(rate: f64, input: &[f32], sr: u32, ch: usize) -> (Vec<f32>, RateMap) {
        let map = RateMap::new(sr);
        let mut ts = TimeStretch::new(sr, ch, 0, 0);
        let mut out = Vec::new();
        for chunk in input.chunks(1024 * ch) {
            ts.process(chunk, rate, &mut out, &map);
        }
        (out, map)
    }

    #[test]
    fn rate_control_round_trips_and_defaults_to_one() {
        let rc = RateControl::default();
        assert_eq!(rc.get(), 1.0);
        rc.set(1.05);
        assert!((rc.get() - 1.05).abs() < 1e-6);
        rc.set(1.0);
        assert_eq!(rc.get(), 1.0, "1.0 must read back exactly");
        rc.set(10.0);
        assert_eq!(rc.get(), MAX_RATE);
        rc.set(f64::NAN);
        assert_eq!(rc.get(), 1.0);
    }

    #[test]
    fn unengaged_map_is_the_identity() {
        let map = RateMap::new(48_000);
        for us in [-5_000, 0, 1, 41_708, 123_456_789] {
            assert_eq!(map.media_us(us), us);
        }
        assert_eq!(map.applied_rate(), 1.0);
    }

    #[test]
    fn unit_rate_reconstructs_the_input() {
        let sr = 48_000;
        let input = sine(sr, 2, 1.0, 440.0);
        let (out, _) = run(1.0, &input, sr, 2);
        assert!(out.len() > input.len() / 2);
        let max_err = out
            .iter()
            .zip(&input)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 1e-4, "1.0 must be transparent, max error {max_err}");
    }

    #[test]
    fn output_length_follows_the_rate() {
        let sr = 48_000;
        let input = sine(sr, 2, 4.0, 300.0);
        for rate in [0.9, 0.95, 1.05, 1.1, 1.5] {
            let (out, _) = run(rate, &input, sr, 2);
            let in_frames = input.len() / 2;
            let out_frames = out.len() / 2;
            // Output covers (consumed input) / rate; the tail still buffered
            // is at most one window + search radius.
            let expect = in_frames as f64 / rate;
            let slack = (sr as f64 * 0.05) / rate;
            assert!(
                (out_frames as f64 - expect).abs() < slack,
                "rate {rate}: {out_frames} frames out, expected ≈{expect:.0}"
            );
        }
    }

    #[test]
    fn map_converts_output_time_to_media_time() {
        let sr = 48_000;
        let input = sine(sr, 1, 4.0, 300.0);
        let (_, map) = run(1.05, &input, sr, 1);
        // 2 s of output at 1.05 ≈ 2.1 s of media (± one search radius).
        let media = map.media_us(2_000_000);
        assert!((media - 2_100_000).abs() < 10_000, "got {media}us");
        assert!((map.applied_rate() - 1.05).abs() < 1e-6);
    }

    #[test]
    fn stretched_sine_keeps_its_pitch() {
        // Zero crossings per second stay ~2·f whatever the rate.
        let sr = 48_000;
        let input = sine(sr, 1, 3.0, 500.0);
        let (out, _) = run(1.08, &input, sr, 1);
        let body = &out[sr as usize / 2..out.len() - sr as usize / 2];
        let crossings = body.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let hz = crossings as f64 / 2.0 / (body.len() as f64 / sr as f64);
        assert!((hz - 500.0).abs() < 10.0, "pitch moved to {hz:.1} Hz");
    }
}
