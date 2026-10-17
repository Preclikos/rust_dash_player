# Changelog

One version line for all platforms (see `docs/RELEASING.md`). Desktop hosts
pin `desktop-vX.Y.Z`; Android, iOS and web consume the published packages.

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
