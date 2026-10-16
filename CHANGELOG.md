# Changelog

One version line for all platforms (see `docs/RELEASING.md`). Desktop hosts
pin `desktop-vX.Y.Z`; Android, iOS and web consume the published packages.

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
