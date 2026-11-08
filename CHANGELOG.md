# Changelog

One version line for all platforms (see `docs/RELEASING.md`). Desktop hosts
pin `desktop-vX.Y.Z`; Android, iOS and web consume the published packages.

## 0.3.5

### iOS

- **Watch-party surface on the SwiftPM package**, the same API.
  `RustPlayer.setPlaybackRate(_:) -> Bool` (`playbackRateRange`
  0.5…2.0), `clockPosition()` → `ClockPosition(positionMs, advancing,
  rate)`, `resumeAfter(ms:)`, `RustPlayerDelegate.rustPlayer(_:readyAt:)`
  (the `ready` event, default no-op), and two `start` parameters,
  `startPositionMs` and `startPaused`, defaulted so existing callers are
  unchanged. C ABI: `rustplayer_player_create_ex2` (`create_ex` plus
  `start_position_ms` / `start_paused`; `create_ex` stays),
  `rustplayer_player_set_playback_rate`, `rustplayer_player_clock_position`
  (out pointers) and `rustplayer_player_resume_after`. A non-1.0 rate is
  still not tuned for the direct (HDR layer) output.

## 0.3.4

### Android

- **Watch-party surface on the AAR** (the 0.3.2 `Player` API, as the web
  package got in 0.3.3). `RustPlayer.setPlaybackRate(rate): Boolean`
  (false while audio goes out as a bitstream), `clockPosition()` →
  `ClockPosition(positionMs, advancing, rate)`, `resumeAfter(delayMs)`,
  `Listener.onReady(positionMs)`, and two trailing `start` parameters:
  `startPositionMs` (exact start, wins over `startFraction`) and
  `startPaused` (prime the pipeline, then wait for `play()` /
  `resumeAfter()`). Defaults keep existing callers unchanged.

## 0.3.3

### Web

- **Watch-party surface on the web package.** `RustPlayer.setPlaybackRate(rate)`,
  `clockPosition()` → `{ positionMs, advancing, rate }` and
  `resumeAfter(delayMs)` expose the 0.3.2 `Player` API, and the new
  `startPaused: true` option of `create` primes the pipeline (first frame,
  `ready` event) and waits for `play()` / `resumeAfter()`.

## 0.3.2

### All platforms

- **Playback rate.** `Player::set_playback_rate(f64)` (0.5–2.0,
  `MIN_PLAYBACK_RATE` / `MAX_PLAYBACK_RATE`) with pitch-preserving time
  stretch (WSOLA); the picture follows the audio-mastered clock. Built for
  small continuous corrections (watch-party drift), works across the range.
  Refused (`false`) while audio goes out as a bitstream (passthrough). At
  1.0 the audio and pacing paths are the unchanged originals; the stretcher
  only runs once another rate is set. Not yet tuned for the Apple direct
  (HDR layer) output, where a non-1.0 rate re-anchors the layer timebase
  every few frames.
- **`PlayerEvent::Ready { position }`**: the pipeline is primed at
  `position` (first frame at the target on screen, audio pre-roll queued),
  emitted once per pipeline build, also while paused (a paused seek ends in
  `Ready`). Bridge JSON: `{"type":"ready","position_ms":…}`. Hosts that
  match `PlayerEvent` exhaustively need an arm for it.
- **`Player::clock_position()`** → `ClockPosition { position, advancing,
  rate }`: the media position audible now (master clock), finer than
  `position()` (last rendered frame).
- **`Player::resume_after(delay)`**: resume so the current position becomes
  audible `delay` from now (output latency taken off); cancelled by any
  pause / resume / seek. For synchronised starts across devices.
- Conformance harness: `--rate <r>` and `--mute` (the beep tap reads the PCM
  before the volume stage).

## 0.3.1

### All platforms

- **Subtitle font cache.** The fonts a manifest lists for an ASS track
  (DVB font download) now download in parallel, and with
  `Player::set_font_cache_dir` (Android `RustPlayer.fontCacheDir`, iOS
  `RustPlayer.fontCacheDirectory`) they are kept on disk, so the next
  episode naming the same font reads it from there. Off by default; the
  host bounds the folder (oldest by modification time). See
  `PLAYER_INTEGRATION.md` §9.
- The same subtitle font added twice (a track picked again) is no longer
  kept twice in memory.

## 0.3.0

### All platforms

- **wgpu 30.** The renderer moves to the Preclikos wgpu fork at tag
  `v30.0.1-fork` (upstream 30.0.1 plus the Android GLES present hook, the
  Vulkan AHardwareBuffer extensions and the Win32 external semaphore; the
  other former fork patches are upstream now). Imported decoder textures
  keep the wrap state wgpu 29 used, so rendering is unchanged.
- Dependency refresh: `harfrust` 0.15 and `skrifa` 0.49 (ASS shaping and
  font reading; output unchanged against the libass reference frames),
  lockfile patch updates.

### Desktop hosts

- **Hosts that share the wgpu device move to wgpu 30 with this tag.** A
  Slint host needs Slint 1.18 with `unstable-wgpu-30` (`require_wgpu_30`,
  `slint::wgpu_30`) and `[patch.crates-io] wgpu = { git =
  "https://github.com/Preclikos/wgpu.git", tag = "v30.0.1-fork" }`. wgpu 30
  API changes a host hits: `RequestAdapterOptions::apply_limit_buckets`,
  `SurfaceConfiguration::color_space`, `queue.present(surface_texture)`,
  `Option` vertex buffer slots, `get_mapped_range()` returning a `Result`.
- **`ffmpeg-sys-next` is held at exactly 9.0.0.** 9.0.1 generates bindings
  that do not compile on Windows. Hosts that depend on the player get the
  pin with this tag; on an older tag, run
  `cargo update -p ffmpeg-sys-next --precise 9.0.0` after a `cargo update`.

## 0.2.32

### Android, Windows, Linux

- **Representations the decoder cannot play are removed before playback.**
  `prepare()` now asks the hardware decoder the player will use about every
  video representation and drops the ones it refuses, so they never reach
  track selection, ABR or the host's track list (the web build already does
  this with `isConfigSupported`). Android asks `MediaCodecList` for
  the decoder `createDecoderByType` picks (`areSizeAndRateSupported`, and a
  Main10 / High 10 profile for 10-bit); Windows asks the D3D11 video device
  on the renderer's GPU (profile, output format, a decoder created at the
  picture size); Linux asks VAAPI (profile, VLD entrypoint, surface size
  limits). On a device without a 4K decoder (the report came from a Lenovo
  Tab M10 Plus Gen 3) the 4K rung is removed instead of failing when ABR or
  a rebuild reaches it. Only a definite "no" removes a
  representation, and a probe that refuses every one removes none. Logged
  as `[support] ...`. Known gaps in `docs/KNOWN_ISSUES.md`.

### All platforms

- **`Error` is sent at the moment of the failure again**, as before 0.2.31.
  Holding a pipeline failure back when a seek or track switch restarted
  playback hid real decoder failures from the host; the 0.2.31 change is
  reverted.

## 0.2.31

### All platforms

- **`Error` only when playback really ends.** A video pipeline that failed
  for good (retries exhausted, or a failure during an ABR switch) reported
  `PlayerEvent::Error` at once, even when a seek or track switch that came
  in meanwhile restarted playback from its position a moment later. Hosts
  that show `Error` as fatal then blocked a film that played on (reported
  on an audio switch on a Lenovo Tab M10 Plus Gen 3). The failure is now
  reported after the pipeline has stopped, and only when no restart
  follows; otherwise it is logged (`failure superseded by a restart`).

## 0.2.30

### Android

- **Devices without OpenGL ES 3.0 play.** When the GPU renderer cannot
  start (GLES 2.0-only GPUs such as Mali-450 in the Mi Box 4) and the host
  passes a video Surface, the player now runs without a GPU: MediaCodec
  renders the picture into the video Surface as in normal direct mode, and
  subtitles (SRT, VTT and ASS) are composited by the CPU into the overlay
  window, with the same placement and look as on the GPU path.
  `onError("gpu_unsupported", …)` is now reported only when `start` got no
  video Surface. See `docs/KNOWN_ISSUES.md` for the limits of this mode.

## 0.2.29

### Android

- **Devices without OpenGL ES 3.0 report it instead of doing nothing.**
  The renderer needs OpenGL ES 3.0; on GLES 2.0-only GPUs (Mali-450 in
  older Amlogic TV boxes such as the Mi Box 4 on Android 9) no player could
  be created and the host got no callback at all. `RustPlayer.start` now
  reports `onError("gpu_unsupported", detail)` (new
  `PlayerErrorKind::UnsupportedGpu`) and `isStarted` stays false. The error
  is permanent for the device: show an "unsupported device" message or use
  another player (see `docs/KNOWN_ISSUES.md`).

### All platforms

- `PlayerErrorKind` has a new variant, `UnsupportedGpu`; Rust hosts that
  match it exhaustively need an arm for it.

## 0.2.28

### Android

- **The passthrough clock runs between audio timestamps.** With E-AC-3 /
  AC-3 passthrough the clock stood on the last `AudioTimestamp` until the
  next one arrived, so the picture ran up to one timestamp period behind
  the sound (0–32 ms on a Google TV Streamer, more on outputs that update
  their timestamps rarely, such as eARC to an AV receiver). The position
  is now carried forward from the timestamp's instant, as ExoPlayer does;
  the guard against the too-early timestamps right after `play()` stays.
  The verbose `[audio-pt] played_ms` line reports the timestamp age.

## 0.2.27

### Android

- **Video decoders are sized for the largest picture of the ladder.** The
  player now sets `max-input-size` on every video decoder (ExoPlayer's HEVC
  rule, at least 1 MiB). Before, the decoder chose its own input buffer
  size, and a sample larger than that buffer was cut without a word: the
  decoder then concealed the missing slices from older pictures, which
  shows as horizontal bands of stale, differently bright image. Dolby
  Vision decoders on TV SoCs are the ones most likely to choose a small
  default. A sample that still does not fit is logged once per codec
  (`[mc] input sample … truncated, picture corrupt`).

## 0.2.26

### All platforms

- **The A/V drift gauge no longer creeps with every pause.** `Stats.av_drift_ms`
  (and the HEALTH log line) measured the picture's wall clock against the
  audio output across pauses and stalls, but the two stop and restart a few
  tens of ms apart, and each hold left that difference in the gauge: on a
  Google TV Streamer it read -115 ms after the first pause and -1420 ms
  after 40. Lip-sync itself was unaffected (measured with the in-engine
  probe: -13..-57 ms before and after pauses of 30 s to 10 min). The gauge
  now starts a new baseline after every pause or stall.

## 0.2.25

### Android

- **No more diagonal streaks on animated ASS subtitles.** On devices whose
  GL runs on ANGLE over Vulkan (Galaxy S25 FE), the newest karaoke syllable
  and rotating text were drawn with diagonal streaks: the driver read the
  last vertex of the subtitle mask buffer as zeros, folding that image's
  second triangle to the frame's corner. The buffer now carries a spare
  vertex past the last one drawn.
- Subtitle textures upload with their own pixel-unpack state instead of
  whatever the renderer's last texture upload left set.

### All platforms

- **A styled subtitle track loads completely or reports why not.**
  Selecting an ASS track holds playback until its script and fonts are in,
  with no 10 s cut-off any more (the HTTP client's timeouts and retry policy
  bound the wait). When the script or a font the manifest marks essential
  does not download, or the script does not parse, playback goes on
  without the track and the host gets the new, non-fatal
  `PlayerEvent::SubtitleTrackFailed { representation_id, kind, detail }`
  (bridge JSON `subtitle_track_failed`, with `kind` like `error`'s). A
  single-file WebVTT track that does not download reports the same way.

## 0.2.24

### All platforms

- **ASS lines no longer climb the screen.** Picking a subtitle track twice
  (an automatic pick and then the host's, or the same track again) left two
  downloads running; both queued their cues, so every event was on screen
  twice and collision stacking pushed one copy above the other, a bit
  further each frame (desktop and web hosts; libass/VLC were fine). A
  download started for an older selection now queues nothing, the overlay
  keeps each cue once, and the renderer each event once.
- **Buffering while a styled track downloads.** Selecting an ASS track holds
  playback (`Buffering { TrackSwitch }`) until its script and fonts are in,
  instead of playing on with no subtitles or fallback faces; at most 10 s,
  then it plays on and the subtitles appear when ready. A host `pause()`
  during the hold stays paused.

### Tests and tooling

- `examples/desktop`: `t <i>` picks a manifest subtitle track, `t off`
  turns subtitles off.

## 0.2.23

### Web (web only)

- The npm package ships `snippets/` (the ASS worker's module locator), so
  0.2.22's package resolves in bundlers; a module loaded from a `blob:` /
  `data:` URL renders ASS on the page's thread instead of failing.

## 0.2.22

### All platforms

- **ASS frames are blended on the GPU.** The worker no longer flattens a
  frame into RGBA: it packs every image's 8-bit coverage into one atlas and
  hands the overlay a triangle list (`ass::Layers`); one premultiplied draw
  blends them in paint order (wgpu: desktop, web, Apple, Android; the
  Android GLES present path too). A quarter of the upload, and the per-pixel
  blend is gone from the CPU: on the Google TV Streamer and the Chromecast
  with Google TV the frame preparation went from 6–8 ms to 1–2 ms, and in
  the browser a karaoke OP no longer adds video judder. Frames whose masks
  don't fit a 2048² atlas are still flattened on the CPU.
- **Composite cache (libass).** A run of glyphs that comes back with the
  same bitmaps at the same relative places, blur, border and shadow reuses
  its blurred / combined bitmaps, only moved: colour, alpha, karaoke and
  position animation (75 % of runs in a typeset karaoke OP). Glyph bitmaps
  from the cache are no longer copied into each run.
- **Faster `\blur`.** Large blurs go libass's way: the bitmap is halved
  with libass's [1 5 10 10 5 1] filter, blurred small and doubled back, so
  the cost per pixel no longer grows with the radius (the pyramid's own
  spread is measured and taken off the small blur — goldens and libass's
  blur regressions unchanged). The vertical pass gathers one output row at
  a time instead of scattering into many (memory-bound on 32-bit ARM).
  Desktop: big blurred drawings 6.0 → 2.4 ms per frame.
- **Less work per glyph.** Shaping plans are compiled once per face /
  script / language / features instead of for every run of every frame;
  the renderer's maps use FxHash (SipHash was slow on 32-bit ARM); a
  glyph's control box is computed once, a border's once per stroke. A
  karaoke OP on the Google TV Streamer: 26 → 20 ms per frame.
- `[subs/ass] perf` also reports the composite cache hit rate.

### Web

- **ASS subtitles render in a Web Worker.** The browser build used to
  render ASS frames on the page's one thread, in the draw path, next to the
  video. A dedicated worker now runs the same renderer (a second instance
  of the module, started from a Blob URL — nothing extra to serve) and
  hands frames back as GPU masks, transferred. The heaviest stress frame
  renders at full resolution with no extra video judder over playback
  without subtitles. `assWorker: false` keeps the old behaviour,
  `assWorkerModuleUrl` points the worker at `rustplayer.js` when a bundler
  moved it. Engine side: `Player::set_ass_offload` /
  `ass_render::AssOffload` (native players keep their render thread).

### Android

- The GLES present path blended subtitle bitmaps as straight alpha although
  they are premultiplied (semi-transparent edges came out too dark).

### Docs

- `docs/examples/ass-reference.mpd`: a complete reference manifest with
  two ASS tracks and DVB font descriptors (required and optional).

## 0.2.21

### All platforms

- **ASS: every event shows.** The overlay kept only the last 5000 cues, so
  in a typeset script (10–20k events: one per sign per frame) everything
  before them never appeared. ASS tracks are no longer capped; other
  tracks only past 100 000 cues (endless live text).
- **ASS animation per video frame.** Animated frames are rendered for each
  video frame instead of a fixed 30 Hz tick, and the worker prefetches the
  frame its measured render time can still make — also for frame-by-frame
  signs (a new event every frame) — instead of finishing frames just after
  their draw. On the Galaxy S21 nearly every draw now has its own frame.
- **ASS render cost.** A busy frame renders on up to three more threads;
  glyph bitmaps are cached across frames (libass's bitmap cache); glyphs
  that cannot reach the frame are not rasterized (a page of text, a long
  `\move` from off screen: 6.4 s → 0.17 s per frame); the gaussian `\blur`
  is fixed point; the final flatten runs in bands. Frames render at most
  1920×1080, and at down to half resolution while animation frames take
  longer than two video frames (weak TV sticks: softer, not slower signs).
  The ASS threads run at low priority on Android. Desktop: a karaoke OP
  with 580 events per frame went from 7.8 to 2.3 ms per frame.
- **Web:** the browser renders ASS frames in the draw path (no threads);
  that path now feeds the same render-time average, so the prefetch and the
  render scale work there too, and the scale steps down already at half a
  video frame (the frame also delays the video on the page's only thread).
- `[subs/ass] perf` log line every 5 s while ASS is on screen.
- **MPD parsing.** `Representation@id` may be any string (it had to be a
  number); `mimeType` / `codecs` on the AdaptationSet apply to its
  Representations; `contentType` is derived from `mimeType` when missing.
  The documented ASS AdaptationSet example failed on all three.
- **Base URL of a manifest at the server root** no longer comes out with a
  double slash (`http://host//seg.mp4`), and the manifest's query string is
  not part of it.

### Android

- armeabi-v7a builds with NEON (required by the ABI): loops vectorize on
  32-bit ARM TV boxes.

### Tests and tooling

- `libass_regression`: libass's own regression suite against the renderer
  (`ASS_REGRESSION=<libass-tests>/regression`); 89 of 103 frames match.
- `bench_script`: per-frame render benchmark of a script
  (`ASS_BENCH`, `ASS_PROFILE` phase timings).

## 0.2.20

### All platforms

- **ASS subtitles no longer blink.** The overlay drew nothing for a frame
  whose ASS bitmap the worker hadn't finished — every animation tick it
  fell behind on, and every event that started or ended mid-tick (the
  prefetch only covers the next tick boundary). Because one bitmap holds
  everything on screen, static dialogue under an animated sign blinked
  with it. Until the wanted frame is baked, the latest finished frame from
  the last second stays up (only while events are active, never one from
  after the playhead), the way async libass renderers keep their last
  canvas.

## 0.2.19

### All platforms

- **ASS/SSA subtitles, styled.** A libass-compatible renderer in pure Rust
  (harfrust shaping, skrifa outlines, zeno rasterization; builds for every
  target including wasm): styles, `\pos`/`\move`/`\org`, 3D rotation and
  shear, `\clip`/`\iclip` with rectangles or drawings, `\p` drawings,
  borders, shadows, `\blur`/`\be`, opaque boxes, `\fad`/`\t` animation,
  karaoke, collision stacking, `WrapStyle` 0–3, right-to-left text,
  vertical `@Font` text, the script's `[Fonts]`. Golden tests against libass
  frames (`player/tests/ass_golden`, `scripts/ass_golden.sh`). See
  `PLAYER_INTEGRATION.md` §9.
- ASS tracks from a manifest: a text AdaptationSet with `codecs="ass"` /
  `mimeType="text/x-ssa"` and a single file; fonts listed with DVB font
  download descriptors (`urn:dvb:dash:fontdownload:2014`, any number per
  track) are fetched before the track's first frame.
- Sidecar `.ass`/`.ssa` files through `add_external_subtitle_track`
  (`SubtitleFormat::Ass`, detected from the header, charset detected).
- `Player::add_subtitle_font(bytes)`: fonts for ASS scripts from the host
  (TTF/OTF/TTC).
- `VttCue::ass` links a cue to its ASS event (`player::ass`); its `text` /
  `layout` are a plain rendering for sinks that draw text only.

### Desktop

- Test shell: drop a subtitle or font file on the window; `subfile
  <path>` takes paths with spaces; `subfonts <dir>`;
  `RUST_PLAYER_SUB_FONTS=<dir>`.

## 0.2.18

### All platforms

- **`open_url` starts a fresh session on a reused `Player`.** The next item
  plays from the start (or from `set_start_position`, set after `open_url`)
  instead of from where the previous one stopped: picking tracks before the
  first frame no longer rebuilds at the previous stream's position,
  `position()` reads 0 right after `open_url`, a resume position parked by a
  failed previous stream is dropped, and track selections from the old
  manifest are cleared. Hosts that build a new `Player` per item (the
  Android, iOS and web shells) were not affected. See
  `PLAYER_INTEGRATION.md` §2.

### Tests and tooling

- Unit tests for a reused `Player` (offline manifest, null video sink).

## 0.2.17

### Desktop (Windows)

- **Vulkan renderer with zero-copy D3D11VA frames.** A desktop host can run
  the player on wgpu's Vulkan backend; decoding stays on D3D11VA. Decoded
  frames are copied into a small pool of shared textures, each imported into
  Vulkan once (`VK_KHR_external_memory_win32`), and the queue waits for the
  copy on the GPU through the D3D11 fence imported as a timeline semaphore
  (`VK_KHR_external_semaphore_win32`); drivers without the semaphore
  extension fall back to a CPU wait. DX12 stays the default and is unchanged.
  Hosts that create the wgpu device themselves (`Player::new_offscreen`)
  request `Features::VULKAN_EXTERNAL_MEMORY_WIN32` on Vulkan — see
  `PLAYER_INTEGRATION.md` §3.1.1.
- The D3D11VA decoder opens on the renderer's GPU on both backends (adapter
  LUID from DX12 or from `VkPhysicalDeviceIDProperties`).
- wgpu fork: `Queue::add_wait_semaphore` (Vulkan), and
  `VK_KHR_external_semaphore_win32` enabled when the driver has it.

### Android

- The 5 s audio-writer heartbeat and the playback-position-jump line log at
  debug level (verbose) instead of info.

### Tests and tooling

- GPU test of the D3D11 → Vulkan import (six NV12 frames through the pool,
  read back plane by plane, byte-exact).
- `examples/desktop-slint`: `--backend vulkan|dx12` (Vulkan by default on
  Windows).

## 0.2.10 – 0.2.16 (summary)

- **Subtitles (desktop, web, Apple):** one cue texture, reused and only grown,
  instead of a new texture for every cue size; GPU regression test.
- **Logs:** the periodic diagnostics (`[vsync] f#N`, `[watchdog gen]`,
  `[sidx]`, `[dec] seg done`) log at debug level, so host log files only
  get events.
- **`examples/desktop-slint`:** the in-app composition desktop hosts use —
  Slint (FemtoVG on wgpu) and the player on one shared device, frames handed
  over as `slint::Image::try_from`; UI-present gauge next to the player's
  stats, JSON summary per run (`--sub`, `--ui-load-ms`, `--no-play`).
- **`examples/desktop`:** `subfile <path> [offset_ms]` console command loads a
  sidecar `.vtt`/`.srt`.
- **CI:** the conformance run also triggers on `Cargo.lock` / `Cargo.toml`,
  and the release gate counts them — a dependency bump needs its own green
  run.
