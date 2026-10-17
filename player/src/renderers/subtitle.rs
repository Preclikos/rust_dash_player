//! WebVTT subtitle overlay rendered via wgpu.
//!
//! Text is plain (white with a dark drop shadow by default, `SubtitleStyle`
//! for colours/size); placement follows the cue's WebVTT settings
//! (`line:`, `position:`, `align:`, `size:`) with ExoPlayer's geometry —
//! see [`CueParent`] and [`place_cue`], which are a port of media3's
//! `SubtitlePainter.setupTextLayout` so the same cue lands where an
//! ExoPlayer app shows it.
//!
//! Pipeline:
//!   1. `queue_cues` — text_play task pushes parsed cues here as they
//!      arrive; we keep them sorted by start time.
//!   2. `set_pts_ms` — av_sync's video loop sets the current playback
//!      PTS just before drawing. The overlay picks the cue that's
//!      active right now and rasterizes it (cached: same text + same
//!      target width = same texture).
//!   3. `draw_into` — called from VideoRenderer's render path after the
//!      main video draw. Issues one textured-quad draw against the
//!      already-bound surface target.
//!
//! Font rasterization is on the CPU via `fontdue`. No glyph atlas: a
//! whole-cue bitmap is generated once per active cue and reused until
//! the cue expires. Cues are short (~2-5s) so this is cheaper than
//! atlas bookkeeping for our use case.
//!
//! That rasterization does NOT happen on the render thread. A dedicated
//! worker (`raster_worker`) keeps the cue under the playhead — and the
//! one after it — rasterized ahead of time; the render path only looks
//! the finished bitmap up and draws it. Rasterizing inline used to cost a
//! multi-megabyte allocation, the glyph raster and a full texture upload
//! at every cue change, all inside the frame, which dropped a frame each
//! time a new subtitle line appeared. Because the next cue is prefetched
//! as soon as the current one starts, steady-state playback never waits;
//! only a seek can land on a cue that isn't rasterized yet, and that
//! resolves within a frame or two.

use std::sync::{Arc, Mutex};

use wgpu::util::DeviceExt;

use crate::parsers::ass::{AssScript, Item, Tag};
use crate::parsers::vtt::{Anchor, CueLayout, CueLine, VttCue};
use crate::{SubtitleAnchor, SubtitleStyle};

// CPU cue rasterization (fontdue) lives in its own file (mirrors `video`).
mod rasterizer;
// ASS/SSA rendering (libass port).
pub(crate) mod ass;

// ---------------------------------------------------------------------------
// Geometry — a port of media3 `SubtitleView` / `SubtitlePainter` defaults so
// cues sit where ExoPlayer puts them. Keep the numbers in sync with upstream:
// libraries/ui/src/main/java/androidx/media3/ui/SubtitleView.java and
// SubtitlePainter.java.
// ---------------------------------------------------------------------------

/// `SubtitleView.DEFAULT_TEXT_SIZE_FRACTION`: text size as a fraction of the
/// parent (padded picture) height.
pub(crate) const TEXT_SIZE_FRACTION: f32 = 0.0533;
/// `SubtitleView.DEFAULT_BOTTOM_PADDING_FRACTION`: how far above the parent's
/// bottom edge a cue without `line:` sits, as a fraction of the parent height.
pub(crate) const BOTTOM_PADDING_FRACTION: f32 = 0.08;
/// `SubtitlePainter.INNER_PADDING_RATIO`: horizontal box padding relative to
/// the text size.
pub(crate) const INNER_PADDING_RATIO: f32 = 0.125;

/// The rectangle cues are laid out in, in target (surface) pixels.
///
/// [`SubtitleAnchor::Screen`] (default): the whole surface. A cue without
/// `line:` then sits `BOTTOM_PADDING_FRACTION` of the surface height above
/// the surface's bottom edge — in the letterbox bar on a widescreen film.
///
/// [`SubtitleAnchor::Picture`]: ExoPlayer's geometry. Its `SubtitleView`
/// lives inside `PlayerView`'s `AspectRatioFrameLayout`, so the parent rect
/// is the aspect-fitted picture — a 2.39:1 film on a 16:9 screen gets its
/// cues inside the picture, `BOTTOM_PADDING_FRACTION` of the *picture*
/// height above the picture's bottom edge.
///
/// Either way the host's bottom inset (`Player::set_subtitle_safe_insets`:
/// system bars, TV overscan) plays the role of the view's bottom padding:
/// it raises the parent's bottom edge, and — as in `CanvasSubtitleOutput`,
/// which resolves the text size against `viewHeightMinusPadding` — shrinks
/// the height the text size is derived from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueParent {
    pub target_w: u32,
    pub target_h: u32,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    /// Height the TEXT SIZE and the auto bottom padding are derived from:
    /// always the aspect-fitted picture, whichever anchor is in use.
    ///
    /// The anchor decides where a cue sits; it has no business deciding how
    /// big the glyphs are. Deriving the size from the layout box instead made
    /// `Screen` scale the text with the surface, so a letterboxed film got
    /// roughly twice ExoPlayer's size — 57px rather than 29px on a
    /// 958×1064 window — and a three-line cue then grew taller than the
    /// letterbox bar and climbed back into the picture, which is the one
    /// thing placing it in the bar was meant to avoid.
    picture_h: f32,
    /// The aspect-fitted picture itself, whatever the anchor and inset:
    /// ASS events are laid out on it (libass's frame is the video).
    picture_left: f32,
    picture_top: f32,
    picture_w: f32,
    picture_full_h: f32,
}

impl CueParent {
    /// From the aspect-fit scale the video quad is drawn with (`scale_x`,
    /// `scale_y` ∈ (0, 1], the letterbox factors) — what the GLES hook has.
    pub fn from_scale(
        target_w: u32,
        target_h: u32,
        scale_x: f32,
        scale_y: f32,
        bottom_inset_px: u32,
        anchor: SubtitleAnchor,
    ) -> Self {
        let tw = target_w as f32;
        let th = target_h as f32;
        let picture_scale_y = scale_y;
        let picture_scale_x = scale_x;
        let (scale_x, scale_y) = match anchor {
            SubtitleAnchor::Screen => (1.0, 1.0),
            SubtitleAnchor::Picture => (scale_x, scale_y),
        };
        let sane = |s: f32| if s.is_finite() && s > 0.0 { s.min(1.0) } else { 1.0 };
        // The picture's own height, taken before the anchor may have flattened
        // the scale to 1.0 — see `picture_h`.
        let picture_h = (th * sane(picture_scale_y)).round();
        let picture_w = (tw * sane(picture_scale_x)).round();
        let pw = (tw * sane(scale_x)).round();
        let ph = (th * sane(scale_y)).round();
        let left = ((tw - pw) / 2.0).floor();
        let top = ((th - ph) / 2.0).floor();
        // Clamp the inset like the old path did so a bogus inset can't push
        // the whole layout box off the top of the screen.
        let inset = (bottom_inset_px as f32).min(th * 0.45);
        let bottom = (top + ph).min(th - inset).max(top + 1.0);
        Self {
            target_w,
            target_h,
            left,
            top,
            right: left + pw,
            bottom,
            // The inset shrinks what the size derives from, as it does for the
            // box itself (media3 resolves text size against
            // `viewHeightMinusPadding`).
            picture_h: (picture_h - inset).max(1.0),
            picture_left: ((tw - picture_w) / 2.0).floor(),
            picture_top: ((th - picture_h) / 2.0).floor(),
            picture_w: picture_w.max(1.0),
            picture_full_h: picture_h.max(1.0),
        }
    }

    /// From the content (frame) size; 0×0 = unknown → the whole target.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn fit(
        target_w: u32,
        target_h: u32,
        content_w: u32,
        content_h: u32,
        bottom_inset_px: u32,
        anchor: SubtitleAnchor,
    ) -> Self {
        let (scale_x, scale_y) = if content_w > 0 && content_h > 0 && target_w > 0 && target_h > 0 {
            let wa = target_w as f32 / target_h as f32;
            let fa = content_w as f32 / content_h as f32;
            if fa > wa {
                (1.0, wa / fa)
            } else {
                (fa / wa, 1.0)
            }
        } else {
            (1.0, 1.0)
        };
        Self::from_scale(target_w, target_h, scale_x, scale_y, bottom_inset_px, anchor)
    }

    /// Layout box size the rasterizer works in.
    pub fn width(&self) -> u32 {
        (self.right - self.left).round().max(1.0) as u32
    }

    pub fn height(&self) -> u32 {
        (self.bottom - self.top).round().max(1.0) as u32
    }

    /// Height the text size and the auto bottom padding scale with. See
    /// [`CueParent::picture_h`].
    pub fn type_height(&self) -> u32 {
        self.picture_h.round().max(1.0) as u32
    }

    /// Size of the aspect-fitted picture (the ASS frame).
    pub fn picture_size(&self) -> (u32, u32) {
        (self.picture_w as u32, self.picture_full_h as u32)
    }
}

/// Top-left corner (target px) of a `bmp_w`×`bmp_h` cue bitmap inside
/// `parent`, per the cue's settings. Line by line
/// `SubtitlePainter.setupTextLayout`:
///
/// * horizontal: anchor at `position` × parent width, the box's
///   start/middle/end edge on it, then clamped into the parent;
/// * `line:N%`: anchor at N × parent height, the box's start/middle/end
///   edge on it (`line_anchor`);
/// * `line:N` (integer): `N ≥ 0` → N line heights below the top,
///   `N < 0` → the box's bottom `(N+1)` line heights above the bottom
///   (`-1` = flush with it);
/// * no `line:` → `BOTTOM_PADDING_FRACTION` of the parent height above the
///   bottom;
/// * finally clamped into the parent (bottom first, then top).
pub fn place_cue(
    bmp_w: u32,
    bmp_h: u32,
    line_height_px: u32,
    layout: &CueLayout,
    parent: &CueParent,
) -> (f32, f32) {
    let w = bmp_w as f32;
    let h = bmp_h as f32;
    let pw = parent.right - parent.left;
    let ph = parent.bottom - parent.top;

    let anchor_x = (pw * layout.position_or_derived()).round() + parent.left;
    let x = match layout.position_anchor_or_derived() {
        Anchor::Start => anchor_x,
        Anchor::Middle => anchor_x - w / 2.0,
        Anchor::End => anchor_x - w,
    };
    // media3 only clamps the left edge and clips the right; keeping the
    // whole box visible is the friendlier reading of the same intent.
    let x = x.min(parent.right - w).max(parent.left);

    let lh = line_height_px as f32;
    let mut y = match layout.line {
        CueLine::Fraction(f) => {
            let anchor_y = (ph * f).round() + parent.top;
            match layout.line_anchor {
                Anchor::Start => anchor_y,
                Anchor::Middle => anchor_y - h / 2.0,
                Anchor::End => anchor_y - h,
            }
        }
        CueLine::Number(n) if n >= 0 => (n as f32 * lh).round() + parent.top,
        CueLine::Number(n) => ((n + 1) as f32 * lh).round() + parent.bottom - h,
        // Padding off the picture height too: 8 % of a tall surface is a wide
        // empty band that pushes the block up towards the picture, which is
        // the opposite of what anchoring to the screen is for.
        CueLine::Auto => {
            parent.bottom - h - (parent.type_height() as f32 * BOTTOM_PADDING_FRACTION).floor()
        }
    };
    if y + h > parent.bottom {
        y = parent.bottom - h;
    }
    if y < parent.top {
        y = parent.top;
    }
    (x, y)
}

/// The quad both draw paths share: `[center_x, center_y, half_w, half_h]`
/// in NDC (y up) for `bitmap` placed by [`place_cue`] on `parent`'s target.
pub fn cue_quad(bitmap: &SubtitleBitmap, parent: &CueParent) -> [f32; 4] {
    let (x, y) = match bitmap.origin {
        // An ASS frame: already laid out, positioned on the picture.
        Some((ox, oy)) => (parent.picture_left + ox, parent.picture_top + oy),
        None => place_cue(bitmap.width, bitmap.height, bitmap.line_height_px, &bitmap.layout, parent),
    };
    let tw = parent.target_w as f32;
    let th = parent.target_h as f32;
    let bw = bitmap.width as f32 * bitmap.scale;
    let bh = bitmap.height as f32 * bitmap.scale;
    // Half-extent in NDC = (px/2) / (target/2) = px / target.
    [
        (x + bw / 2.0) / tw * 2.0 - 1.0,
        1.0 - (y + bh / 2.0) / th * 2.0,
        bw / tw,
        bh / th,
    ]
}

fn build_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
    fs_entry: &'static str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("subtitle_pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fs_entry),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                // Premultiplied alpha so the cue blends naturally
                // over arbitrary video content.
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadUniform {
    /// xy = NDC center, zw = NDC half-extent
    transform: [f32; 4],
    /// xy = the cue's share of the (larger, reused) texture; zw unused.
    uv_scale: [f32; 4],
}

/// GPU-side mirror of the bitmap currently on screen, owned by the render
/// thread. One texture serves every cue: it only grows (to the largest cue
/// so far, rounded up) and a cue is uploaded into its top-left corner and
/// sampled from there. Allocating a texture + bind group per cue size
/// (nearly every cue) put a create/destroy on the device at each subtitle
/// change, which on a device shared with the host UI (BlackZoneDesktop)
/// coincided with the UI hitching on every new line.
#[cfg_attr(target_os = "android", allow(dead_code))]
struct GpuCue {
    /// `SubtitleBitmap::generation` of the content currently uploaded.
    generation: u64,
    /// Texture size (capacity), at least one texel larger than any cue
    /// uploaded into it on both axes (the transparent guard texels).
    cap_w: u32,
    cap_h: u32,
    /// Size of the cue currently uploaded.
    bitmap_w: u32,
    bitmap_h: u32,
    /// Held to keep the underlying GPU resource alive for as long as `view`
    /// is referenced. Never read directly — the `view` does all the work.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    /// Same: kept alive because `bind_group` refers to it.
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    /// Last value written to `uniform_buffer`, so a static cue doesn't
    /// re-upload the same floats every frame.
    uniform: [f32; 8],
}

/// The render layer's format while it is the direct-mode subtitle overlay.
/// Core Animation composites a 10-bit `CAMetalLayer` (`rgb10a2Unorm`, 2-bit
/// alpha) as OPAQUE whatever `opaque` and the drawable say — measured on an
/// iPhone SE (iOS 15.8): a drawable cleared to (0, 0, 0, 0) in that format
/// showed black over the video layer, the same drawable as `bgra8Unorm`
/// showed through. 8-bit alpha is all subtitles need.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub const DIRECT_OVERLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

#[cfg_attr(target_os = "android", allow(dead_code))]
pub struct SubtitleOverlay {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    /// `fs_main_pq` into an rgba16float target, built on the first frame of
    /// an HDR output session (see [`Self::set_pq_output`]).
    pipeline_pq: std::sync::OnceLock<wgpu::RenderPipeline>,
    /// Draw through `pipeline_pq` — the video renderer flips it on HDR
    /// output session entry/exit.
    pq_output: std::sync::atomic::AtomicBool,
    /// `fs_main` into a [`DIRECT_OVERLAY_FORMAT`] target — the Apple direct
    /// mode keeps the render layer in that 8-bit-alpha format while it is a
    /// transparent subtitle overlay (see [`Self::set_direct_output`]).
    pipeline_direct: std::sync::OnceLock<wgpu::RenderPipeline>,
    direct_output: std::sync::atomic::AtomicBool,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,

    /// Cue state + rasterizer output, shared with the worker thread.
    shared: Arc<Shared>,
    /// Render-thread-owned GPU mirror. Never touched by the worker.
    gpu: Mutex<Option<GpuCue>>,
    /// Joined in `Drop`. `None` only if the thread failed to spawn, in
    /// which case cues simply never rasterize (rendering degrades to no
    /// subtitles rather than stalling the frame).
    worker: Option<std::thread::JoinHandle<()>>,
    /// Cue textures allocated so far (the regression gauge for the shared,
    /// grow-only texture: a handful per session, not one per cue).
    #[cfg_attr(not(test), allow(dead_code))]
    texture_allocs: std::sync::atomic::AtomicU64,
}

/// State the render path, the host-facing setters and the rasterizer
/// worker all touch. One mutex: every critical section is a handful of
/// integer comparisons, and the worker deliberately drops the lock while
/// it rasterizes.
struct Shared {
    inner: Mutex<Inner>,
    /// Signalled when `want_epoch` moves, i.e. the worker may have new
    /// work. Also signals shutdown.
    wake: std::sync::Condvar,
}

struct Inner {
    /// Sorted by start_ms ascending. Bounded only for endless streams
    /// (see `add_cues`).
    cues: Vec<VttCue>,
    /// Current playback PTS in ms, updated by the render path before
    /// each draw call.
    current_pts_ms: i64,
    /// Video frame interval (ms), averaged from the PTS steps: animated
    /// ASS frames are rendered per video frame and the next one is
    /// prefetched one interval ahead. 0 until measured.
    frame_ms: f64,
    /// How long an ASS frame takes the worker (ms, averaged): the
    /// prefetch aims that far ahead, so a slow device shows every frame it
    /// bakes instead of finishing each one just after its draw.
    ass_bake_ms: f64,
    /// Fraction of the picture size animated ASS frames render at: drops
    /// (to 1/2) while frames take longer than two video frames to bake —
    /// a weak TV stick then shows soft but moving signs instead of a few
    /// frames a second — and climbs back when there is time.
    ass_scale: f32,
    /// Bakes since `ass_scale` last changed.
    ass_scale_age: u32,
    /// fontdue::Font held behind an `Arc` so the render path can take a
    /// reference out of the lock for the price of a refcount bump.
    /// `fontdue::Font` is `Clone`, but that clone deep-copies every
    /// glyph's outline geometry (~7 ms for DejaVu Sans on a desktop x86,
    /// several times that on a phone/TV SoC) — doing it per frame stalled
    /// the render thread for as long as a cue was on screen. `set_font`
    /// swaps the whole Arc. Initialised to the embedded DejaVu default so
    /// cues render without a host-supplied font; `None` only if that
    /// default somehow fails to parse, in which case render is a no-op.
    font: Option<Arc<fontdue::Font>>,
    /// The embedded DejaVu Sans, kept as the per-glyph fallback behind
    /// the primary (embedded Roboto, or whatever `set_font` installs) —
    /// see `rasterizer::FontSet`.
    fallback: Option<Arc<fontdue::Font>>,
    /// Visual style (colours + size multiplier). Swapped by `set_style`;
    /// changing it drops the cached rasterization so the next draw rebuilds.
    style: SubtitleStyle,
    /// Rasterized cues the worker has finished, newest last. Holds the
    /// cue on screen plus the prefetched next one; `READY_SLOTS` gives it
    /// one slot of slack so an advance never evicts something still
    /// wanted. Both draw paths read from here and never rasterize.
    ready: Vec<std::sync::Arc<SubtitleBitmap>>,
    generation: u64,
    /// Layout box (`CueParent` width/height) the render path last drew
    /// at. The worker needs it to size a cue, so before the first draw
    /// (0×0) it has nothing to do.
    target_w: u32,
    target_h: u32,
    /// Picture height the text size derives from — see `CueParent::picture_h`.
    type_h: u32,
    /// Bumped whenever finished bitmaps go stale: a new style, a new font,
    /// a new layout box or text size. A raster job carries the epoch it was
    /// cut under, and a result from an older epoch is dropped. The size
    /// check alone let a job that was in flight during `set_style` or
    /// `set_font` store a bitmap in the OLD look, which then stayed on
    /// screen until the cue changed.
    epoch: u64,
    /// Cue indices the worker was last asked to have ready — `[active,
    /// next]`. Kept so `set_pts_ms`, which runs every frame, can tell a
    /// PTS update that changes nothing from one that crosses a cue
    /// boundary, and only wake the worker for the latter.
    /// What the worker last woke up for (see `wanted_key`).
    wanted: WantedKey,
    /// Set by `Drop` to retire the worker.
    shutdown: bool,
    /// Longest cue duration currently in `cues`, in ms. Bounds how far
    /// back of the current PTS an active cue can start, which is what
    /// makes the binary-search window in `active_index` exact — see
    /// there. Only ever grows while cues are pushed (a stale-large value
    /// just widens the window, never hides a cue); reset by `clear`.
    max_cue_span_ms: i64,
    /// Set when the cues are ASS events: they are then drawn as whole
    /// frames by `ass_renderer` instead of as text.
    ass: Option<AssTrack>,
    /// Shared with the worker; holds the fonts (built-in, host-added) and
    /// the collision placement state across frames.
    ass_renderer: Arc<Mutex<ass::AssRenderer>>,
    /// The aspect-fitted picture the render path last drew on (the ASS
    /// frame size).
    picture_w: u32,
    picture_h: u32,
    /// The playhead jumped back: colliding events get placed afresh.
    ass_relayout: bool,
    /// Decoded video size (libass storage size); 0×0 = unknown.
    video_w: u32,
    video_h: u32,
}

impl Inner {
    /// `SubtitleOverlay::queue_cues`.
    fn add_cues(&mut self, cues: Vec<VttCue>) {
        let script = cues.iter().find_map(|c| c.ass.as_ref().map(|a| Arc::clone(&a.script)));
        if let Some(script) = script {
            let same = self.ass.as_ref().is_some_and(|t| Arc::ptr_eq(&t.script, &script));
            if !same {
                let animated = script.events.iter().map(event_animates).collect();
                log::info!(
                    "[subs] ASS track: {} events, {}x{}",
                    script.events.len(),
                    script.play_res_x,
                    script.play_res_y
                );
                self.ass = Some(AssTrack { script, animated });
                self.ass_relayout = true;
                self.invalidate();
            }
        }
        for c in cues {
            self.max_cue_span_ms = self.max_cue_span_ms.max(c.end_ms - c.start_ms);
            self.cues.push(c);
        }
        self.cues.sort_by_key(|c| c.start_ms);
        // Cap memory for an endless (live) text stream: past this many
        // cues the oldest go. Cues are fetched once and never again, so
        // the cap must stay far above any finite track — a typeset ASS
        // script has 10–20k events (one per sign per frame), a 2 h
        // movie ~2000 dialogue cues. An ASS track is one file whose
        // events the script holds anyway: never capped.
        const MAX_CUES: usize = 100_000;
        if self.ass.is_none() && self.cues.len() > MAX_CUES {
            let excess = self.cues.len() - MAX_CUES;
            self.cues.drain(0..excess);
        }
        // Indices shifted; force the worker to recompute rather than
        // trust `wanted`.
        self.wanted = WantedKey::default();
    }

    /// ASS mode: the events active at `pts_ms`, file indices in cue order.
    fn ass_active(&self, pts_ms: i64) -> Vec<usize> {
        let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        let lo = self.cues.partition_point(|c| c.start_ms <= pts_ms - self.max_cue_span_ms);
        let mut ev: Vec<usize> = self.cues[lo..hi]
            .iter()
            .filter(|c| c.is_active(pts_ms))
            .filter_map(|c| c.ass.as_ref().map(|a| a.event))
            .collect();
        ev.sort_unstable();
        ev
    }

    /// ASS mode: what is shown at `pts_ms`.
    fn ass_show(&self, pts_ms: i64) -> Option<AssShow> {
        let track = self.ass.as_ref()?;
        let events = self.ass_active(pts_ms);
        if events.is_empty() {
            return None;
        }
        let animated = events.iter().any(|&e| track.animated.get(e).copied().unwrap_or(false));
        // Animated: rendered for this very frame (libass renders per
        // video frame).
        let tick = if animated { pts_ms } else { -1 };
        Some(AssShow { key: AssKey { events, tick }, time_ms: pts_ms })
    }

    /// ASS mode: the frame shown now and the one to prefetch — the next
    /// video frame while something animates, else the frame at the next
    /// moment an event starts or ends.
    fn ass_wanted(&self, pts_ms: i64) -> [Option<AssShow>; 2] {
        let now = self.ass_show(pts_ms);
        // How far ahead a frame must be asked for to be baked in time.
        let lead = self.ass_bake_ms * 1.25;
        let next_t = match &now {
            Some(s) if s.key.tick >= 0 => {
                let step = if self.frame_ms > 0.0 { self.frame_ms } else { ASS_TICK_MS as f64 };
                // The first frame the bake can still be in time for.
                let frames = (lead / step).ceil().max(1.0);
                Some(s.key.tick + (frames * step).round() as i64)
            }
            // Static: the next event edge — or, when edges come faster
            // than a bake (frame-by-frame typesetting: a new event every
            // frame), what is on screen one bake from now.
            _ => self.ass_next_edge(pts_ms).map(|e| e.max(pts_ms + lead.ceil() as i64)),
        };
        [now, next_t.and_then(|t| self.ass_show(t))]
    }

    /// After a bake of a brief ASS frame: lower the render scale while
    /// bakes take more than two video frames, raise it back below one
    /// (a few bakes apart, so the average follows each step). In the
    /// browser the frame renders on the page's only thread, in the draw
    /// path, where it delays the video too: there the bounds are half and
    /// a quarter of a frame.
    fn adapt_ass_scale(&mut self) {
        const MIN: f32 = 0.5;
        const SLOW: f64 = if cfg!(target_arch = "wasm32") { 0.5 } else { 2.0 };
        const FAST: f64 = if cfg!(target_arch = "wasm32") { 0.25 } else { 0.8 };
        self.ass_scale_age += 1;
        if self.ass_scale_age < 6 {
            return;
        }
        let frame = if self.frame_ms > 0.0 { self.frame_ms } else { ASS_TICK_MS as f64 };
        let old = self.ass_scale;
        if self.ass_bake_ms > SLOW * frame && self.ass_scale > MIN {
            self.ass_scale = (self.ass_scale * 0.85).max(MIN);
        } else if self.ass_bake_ms < FAST * frame && self.ass_scale < 1.0 {
            self.ass_scale = (self.ass_scale / 0.9).min(1.0);
        }
        if self.ass_scale != old {
            self.ass_scale_age = 0;
            log::debug!("[subs/ass] render scale {:.2} -> {:.2} (bake {:.1} ms)", old, self.ass_scale, self.ass_bake_ms);
        }
    }

    /// ASS mode: the first time after `pts_ms` an event starts or ends.
    fn ass_next_edge(&self, pts_ms: i64) -> Option<i64> {
        let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        let lo = self.cues.partition_point(|c| c.start_ms <= pts_ms - self.max_cue_span_ms);
        let next_start = self.cues.get(hi).map(|c| c.start_ms);
        let next_end = self.cues[lo..hi].iter().filter(|c| c.is_active(pts_ms)).map(|c| c.end_ms).min();
        match (next_start, next_end) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// ASS mode: is the frame due at `pts_ms` replaced before a bake of it
    /// could finish? Then a held frame covers it and the worker skips it.
    fn ass_now_superseded(&self, now: &AssShow, pts_ms: i64) -> bool {
        let lead = (self.ass_bake_ms * 1.25).ceil() as i64;
        now.key.tick >= 0 || self.ass_next_edge(pts_ms).is_some_and(|e| e - pts_ms < lead)
    }

    fn ready_for_ass(&self, key: &AssKey) -> Option<&std::sync::Arc<SubtitleBitmap>> {
        self.ready.iter().find(|b| {
            b.ass_key.as_ref().is_some_and(|k| {
                k.events == key.events
                    && (k.tick == key.tick || (k.tick >= 0 && key.tick >= 0 && (k.tick - key.tick).abs() <= ASS_TICK_SLACK_MS))
            })
        })
    }

    /// ASS mode: the latest finished frame from just before `pts_ms`.
    fn held_ass(&self, pts_ms: i64) -> Option<&std::sync::Arc<SubtitleBitmap>> {
        self.ready
            .iter()
            .filter(|b| b.ass_key.is_some())
            .filter(|b| (0..=ASS_HOLD_MS).contains(&(pts_ms - b.ass_time_ms)))
            .max_by_key(|b| b.ass_time_ms)
    }

    /// The finished bitmap for what is on screen at `pts_ms`, either kind.
    fn shown_bitmap(&self, pts_ms: i64, target_w: u32) -> Option<std::sync::Arc<SubtitleBitmap>> {
        if self.ass.is_some() {
            let show = self.ass_show(pts_ms)?;
            if let Some(b) = self.ready_for_ass(&show.key) {
                ass_perf(|p| p.exact += 1);
                return Some(Arc::clone(b));
            }
            // Not baked yet: the worker is behind on the animation, or an
            // event started / ended between two prefetches. Keep the
            // latest frame from just before instead of drawing nothing —
            // every subtitle on screen, static dialogue included, used to
            // blink off for that frame.
            return self
                .held_ass(pts_ms)
                .cloned()
                .inspect(|_| ass_perf(|p| p.held += 1))
                .or_else(|| {
                    ass_perf(|p| p.missing += 1);
                    None
                });
        }
        let (text, layout) = self.shown_at(pts_ms)?;
        self.ready_for(&text, &layout, target_w).cloned()
    }

    /// Record the picture size; true when it changed (ASS frames are laid
    /// out at it, so finished ones go stale).
    fn note_picture(&mut self, w: u32, h: u32) -> bool {
        if (self.picture_w, self.picture_h) == (w, h) {
            return false;
        }
        self.picture_w = w;
        self.picture_h = h;
        if self.ass.is_some() {
            self.invalidate();
        }
        true
    }

    /// Index of the cue active at `pts_ms`, or `None`.
    ///
    /// Same answer as `cues.iter().find(|c| c.is_active(pts))` — the first
    /// active cue in start order — but without the linear walk. That walk
    /// scanned every cue from the start of the list on *every frame*, so
    /// its cost grew with playback position (~5000 comparisons per frame
    /// near the end of a MAX_CUES-capped movie).
    ///
    /// `cues` is sorted by `start_ms`, so two binary searches bracket the
    /// only indices that can be active:
    ///   * `hi` — cues at or past it start after `pts`, so they're future.
    ///   * `lo` — cues before it start at or before `pts - max_cue_span_ms`,
    ///     so their end (≤ start + max span) is at or before `pts`: expired.
    ///
    /// Whatever is left in `lo..hi` is a handful of entries even for
    /// pathological overlapping-cue tracks.
    fn active_index(&self, pts_ms: i64) -> Option<usize> {
        let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        if hi == 0 {
            return None;
        }
        let lo = self
            .cues
            .partition_point(|c| c.start_ms <= pts_ms - self.max_cue_span_ms);
        self.cues[lo..hi]
            .iter()
            .position(|c| c.is_active(pts_ms))
            .map(|i| lo + i)
    }

    /// The cues the worker should keep rasterized: the one under the
    /// playhead and the one that starts next. Prefetching the second is
    /// what keeps a cue change off the render thread's critical path —
    /// by the time it becomes active its bitmap has been ready for
    /// seconds.
    fn wanted_indices(&self, pts_ms: i64) -> [Option<usize>; 2] {
        let active = self.active_index(pts_ms);
        let next = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        let next = (next < self.cues.len()).then_some(next);
        // A cue that is both active and "next" can't happen (next starts
        // strictly after pts), so these never collide.
        [active, next]
    }

    /// What is on screen at `pts_ms`: the active cue's text and layout, or,
    /// when cues overlap (two speakers, a sign plus dialogue), every active
    /// cue as one block in start order with the first one's layout. Every
    /// renderer draws a single bitmap; this used to show the first active
    /// cue only and drop the rest.
    ///
    /// Cues without text (ASS drawings, tag-only events) take no part: they
    /// would add a blank line, or leave nothing to show yet win the layout.
    fn shown_at(&self, pts_ms: i64) -> Option<(std::borrow::Cow<'_, str>, CueLayout)> {
        let first = self.active_index(pts_ms)?;
        let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
        let mut active = self.cues[first..hi]
            .iter()
            .filter(|c| c.is_active(pts_ms) && !c.text.is_empty());
        let cue = active.next()?;
        let mut rest = active.peekable();
        if rest.peek().is_none() {
            return Some((std::borrow::Cow::Borrowed(cue.text.as_str()), cue.layout));
        }
        let mut text = cue.text.clone();
        for other in rest {
            text.push('\n');
            text.push_str(&other.text);
        }
        Some((std::borrow::Cow::Owned(text), cue.layout))
    }

    /// The texts the worker should keep rasterized: what is shown now, and
    /// what will be shown when the next cue starts (prefetch).
    fn wanted_shows(&self, pts_ms: i64) -> [Option<(std::borrow::Cow<'_, str>, CueLayout)>; 2] {
        let [_, next] = self.wanted_indices(pts_ms);
        [self.shown_at(pts_ms), next.and_then(|i| self.shown_at(self.cues[i].start_ms))]
    }

    /// Identifies what the worker has to produce for `pts_ms`, cheaply: the
    /// active cues (first, last, how many) and the next one. A change means
    /// the shown block or the prefetch changed.
    fn wanted_key(&self, pts_ms: i64) -> WantedKey {
        let [first, next] = self.wanted_indices(pts_ms);
        let (mut last, mut active) = (first, 0);
        if let Some(first) = first {
            let hi = self.cues.partition_point(|c| c.start_ms <= pts_ms);
            for (i, c) in self.cues[first..hi].iter().enumerate() {
                if c.is_active(pts_ms) {
                    active += 1;
                    last = Some(first + i);
                }
            }
        }
        // ASS: an animated frame changes every tick, not only at cue edges.
        let tick = if self.ass.is_some() { self.ass_show(pts_ms).map_or(-1, |s| s.key.tick) } else { -1 };
        WantedKey { first, last, active, next, tick }
    }

    /// Finished bitmap for `text` at roughly `target_w`, if the worker has
    /// produced one. The 5% width tolerance matches the old cache rule:
    /// a window drag resizes continuously and re-rasterizing on every
    /// pixel would be pointless churn.
    fn ready_for(
        &self,
        text: &str,
        layout: &CueLayout,
        target_w: u32,
    ) -> Option<&std::sync::Arc<SubtitleBitmap>> {
        self.ready
            .iter()
            .find(|b| b.text == text && b.layout == *layout && width_close(b.target_w, target_w))
    }

    /// Record the surface size the render path is drawing at. Returns
    /// true when it moved enough to invalidate what the worker produced.
    fn note_target(&mut self, target_w: u32, target_h: u32, type_h: u32) -> bool {
        let type_changed = self.type_h != type_h;
        self.type_h = type_h;
        if !type_changed && self.target_h == target_h && width_close(self.target_w, target_w) {
            // Keep the exact numbers current even inside the tolerance so
            // the drift is measured against what we last drew.
            self.target_w = target_w;
            return false;
        }
        self.target_w = target_w;
        self.target_h = target_h;
        self.invalidate();
        true
    }

    /// Drop every finished bitmap and make results still in flight stale.
    fn invalidate(&mut self) {
        self.ready.clear();
        self.epoch += 1;
    }
}

/// See [`Inner::wanted_key`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WantedKey {
    first: Option<usize>,
    last: Option<usize>,
    active: usize,
    next: Option<usize>,
    /// ASS animation tick, -1 when nothing animates.
    tick: i64,
}

/// Rasterized-at width close enough to the drawn width to reuse. Mirrors
/// the tolerance the pre-worker cache used.
fn width_close(cached_w: u32, target_w: u32) -> bool {
    (cached_w as i64 - target_w as i64).abs() <= (target_w as i64 / 20).max(8)
}

/// Bitmaps kept around: the cue on screen, the prefetched next one, and
/// one slot of slack so producing the new prefetch can't evict either.
const READY_SLOTS: usize = 3;

/// A rasterized cue as plain pixels, for sinks that own their texture
/// upload (the Android GLES hook). The same `rasterize_cue` output the
/// wgpu path uploads — a libass backend would feed this exact shape.
pub struct SubtitleBitmap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Monotonic content identity — changes whenever the visible bitmap
    /// changes. Lets callers cache uploads and detect updates cheaply.
    pub generation: u64,
    /// One text line's height, for `line:N` placement (see `place_cue`).
    pub line_height_px: u32,
    /// The cue's placement settings — `cue_quad` positions by them.
    pub layout: CueLayout,
    /// Set for an ASS frame: the bitmap's top-left corner on the picture
    /// (px), which `cue_quad` uses instead of `layout`.
    pub origin: Option<(f32, f32)>,
    /// Picture px per bitmap px: an ASS frame rendered at reduced
    /// resolution (see `Inner::ass_scale`) is drawn this much larger.
    pub scale: f32,
    /// Identity for cache validation: same text + same settings at about
    /// the same parent width = same bitmap.
    text: String,
    target_w: u32,
    /// ASS frames: which events at which animation tick.
    ass_key: Option<AssKey>,
    /// ASS frames: the time the frame was rendered at (0 for text cues).
    ass_time_ms: i64,
}

/// What an ASS frame shows: the active events (indices into the script)
/// and, while any of them animates, the video frame time it is for
/// (`tick`, ms; -1 for a static frame).
#[derive(Clone, Debug, PartialEq, Eq)]
struct AssKey {
    events: Vec<usize>,
    tick: i64,
}

/// An ASS frame to render: its key and the time to render it at.
#[derive(Clone, Debug)]
struct AssShow {
    key: AssKey,
    time_ms: i64,
}

/// Frame interval assumed for the animation prefetch until the PTS steps
/// have been measured.
const ASS_TICK_MS: i64 = 33;

/// A frame baked for a time this close to the wanted one stands for it:
/// the prefetch predicts the next frame time from the average interval,
/// and 24 fps PTS in whole ms step by 41 / 42.
const ASS_TICK_SLACK_MS: i64 = 3;

/// How old a finished ASS frame may be and still stand in for the one the
/// worker hasn't baked yet (see `Inner::shown_bitmap`). Bounded so a seek
/// never shows a frame from somewhere else.
const ASS_HOLD_MS: i64 = 1000;

/// ASS frames render at most this large (picture px); a 4K picture gets
/// a 1080p frame drawn twice as large — subtitle detail beyond that is
/// invisible at viewing distance and costs four times the work.
const ASS_MAX_RENDER: (f32, f32) = (1920.0, 1080.0);

/// The fraction of a `w`×`h` picture an ASS frame renders at, at most 1.
fn ass_render_cap(w: u32, h: u32) -> f32 {
    (ASS_MAX_RENDER.0 / w.max(1) as f32).min(ASS_MAX_RENDER.1 / h.max(1) as f32).min(1.0)
}

/// ASS overlay performance, logged as one `[subs/ass] perf` line every
/// ~5 s while ASS frames render or show: worker render / composite times,
/// frame size, and how many draws had their frame baked (`exact`), showed
/// a held older one (`held`) or nothing for want of one (`missing`).
#[derive(Clone, Copy)]
struct AssPerf {
    since: Option<crate::rt::Instant>,
    frames: u32,
    render_ms: f64,
    render_max_ms: f64,
    composite_ms: f64,
    max_events: usize,
    max_images: usize,
    max_px: u64,
    exact: u32,
    held: u32,
    missing: u32,
    min_scale: f32,
}

const ASS_PERF_ZERO: AssPerf = AssPerf {
    since: None,
    frames: 0,
    render_ms: 0.0,
    render_max_ms: 0.0,
    composite_ms: 0.0,
    max_events: 0,
    max_images: 0,
    max_px: 0,
    exact: 0,
    held: 0,
    missing: 0,
    min_scale: 1.0,
};

static ASS_PERF: Mutex<AssPerf> = Mutex::new(ASS_PERF_ZERO);

fn ass_perf(f: impl FnOnce(&mut AssPerf)) {
    let Ok(mut p) = ASS_PERF.lock() else { return };
    f(&mut p);
    let now = crate::rt::Instant::now();
    let since = *p.since.get_or_insert(now);
    if now.duration_since(since).as_secs_f64() < 5.0 {
        return;
    }
    let n = p.frames.max(1) as f64;
    log::info!(
        "[subs/ass] perf {:.1}s: {} frames baked, render avg {:.1} max {:.1} ms, composite avg {:.1} ms, peak {} events / {} images / {:.2} Mpx | draws exact {} held {} missing {} | scale {:.2}",
        now.duration_since(since).as_secs_f64(),
        p.frames,
        p.render_ms / n,
        p.render_max_ms,
        p.composite_ms / n,
        p.max_events,
        p.max_images,
        p.max_px as f64 / 1e6,
        p.exact,
        p.held,
        p.missing,
        p.min_scale
    );
    *p = AssPerf { since: Some(now), ..ASS_PERF_ZERO };
}

/// The ASS track the cues belong to.
struct AssTrack {
    script: Arc<AssScript>,
    /// Per event: does its look change while it is on screen (`\t`,
    /// `\fad`, `\move`, karaoke, a scrolling effect)?
    animated: Vec<bool>,
}

fn event_animates(e: &crate::parsers::ass::AssEvent) -> bool {
    fn tags(t: &[Tag]) -> bool {
        t.iter().any(|t| {
            matches!(
                t,
                Tag::Transform { .. } | Tag::Fade { .. } | Tag::Move { .. } | Tag::Karaoke { .. } | Tag::KaraokeStart(_)
            )
        })
    }
    !e.effect.is_empty() || e.items.iter().any(|i| matches!(i, Item::Override(t) if tags(t)))
}

impl SubtitleOverlay {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("subtitle_bind_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("subtitle_shader"),
            source: wgpu::ShaderSource::Wgsl(crate::shader_src::subtitle().into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("subtitle_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = build_pipeline(&device, &shader, &pipeline_layout, surface_format, "fs_main");

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("subtitle_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("subtitle_uniform"),
            contents: bytemuck::cast_slice(&[QuadUniform {
                transform: [0.0, 0.0, 0.0, 0.0],
                uv_scale: [1.0, 1.0, 0.0, 0.0],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                cues: Vec::new(),
                current_pts_ms: 0,
                frame_ms: 0.0,
                ass_bake_ms: 0.0,
                ass_scale: 1.0,
                ass_scale_age: 0,
                font: rasterizer::default_font().map(Arc::new),
                fallback: rasterizer::fallback_font().map(Arc::new),
                style: SubtitleStyle::DEFAULT,
                ready: Vec::new(),
                generation: 0,
                target_w: 0,
                target_h: 0,
                type_h: 0,
                epoch: 0,
                wanted: WantedKey::default(),
                shutdown: false,
                max_cue_span_ms: 0,
                ass: None,
                ass_renderer: Arc::new(Mutex::new(ass::AssRenderer::new())),
                picture_w: 0,
                picture_h: 0,
                ass_relayout: false,
                video_w: 0,
                video_h: 0,
            }),
            wake: std::sync::Condvar::new(),
        });

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("subtitle-raster".to_string())
                .spawn(move || raster_worker(shared))
                .map_err(|e| {
                    // No threads on wasm32: expected, the render path
                    // rasterizes inline (`rasterize_inline`). Anywhere else
                    // a failed spawn is a real problem worth shouting about.
                    if cfg!(target_arch = "wasm32") {
                        log::debug!("[subs] no rasterizer thread ({}); rasterizing inline", e);
                    } else {
                        log::error!("[subs] rasterizer thread failed to spawn: {}", e);
                    }
                })
                .ok()
        };

        SubtitleOverlay {
            device,
            queue,
            surface_format,
            pipeline,
            pipeline_pq: std::sync::OnceLock::new(),
            pq_output: std::sync::atomic::AtomicBool::new(false),
            pipeline_direct: std::sync::OnceLock::new(),
            direct_output: std::sync::atomic::AtomicBool::new(false),
            texture_allocs: std::sync::atomic::AtomicU64::new(0),
            shader,
            pipeline_layout,
            bind_group_layout,
            sampler,
            uniform_buffer,
            shared,
            gpu: Mutex::new(None),
            worker,
        }
    }

    /// Lock, mutate, wake the worker. Every setter that can change what
    /// needs rasterizing goes through here so none of them forgets the
    /// notify. Waking a worker that turns out to have nothing to do is
    /// harmless — it re-parks immediately.
    fn with_inner_notify<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let out = {
            let mut inner = self.shared.inner.lock().unwrap();
            f(&mut inner)
        };
        self.shared.wake.notify_one();
        out
    }

    /// Install a TTF/OTF font for cue rasterization, replacing the
    /// embedded Roboto as the primary face (the embedded DejaVu stays the
    /// per-glyph fallback). Invalidates any cached rasterization. On
    /// invalid bytes the previous font is kept and an Err is returned.
    pub fn set_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| e.to_string())?;
        self.with_inner_notify(|inner| {
            inner.font = Some(Arc::new(font));
            inner.invalidate();
        });
        Ok(())
    }

    /// Add a font for ASS rendering (see `AssRenderer::add_font`). The
    /// frames on screen are re-rendered with it.
    pub fn add_ass_font(&self, bytes: Vec<u8>) -> usize {
        let renderer = Arc::clone(&self.shared.inner.lock().unwrap().ass_renderer);
        let n = renderer.lock().unwrap().add_font(bytes);
        if n > 0 {
            self.with_inner_notify(|inner| {
                if inner.ass.is_some() {
                    inner.invalidate();
                }
            });
        }
        n
    }

    /// The decoded video's size (ASS blur, unscaled borders and perspective
    /// are measured in video pixels, as in libass hosts).
    pub fn set_video_size(&self, w: u32, h: u32) {
        let changed = {
            let mut inner = self.shared.inner.lock().unwrap();
            let changed = (inner.video_w, inner.video_h) != (w, h);
            inner.video_w = w;
            inner.video_h = h;
            if changed && inner.ass.is_some() {
                inner.invalidate();
            }
            changed
        };
        if changed {
            self.shared.wake.notify_one();
        }
    }

    /// The layout-box anchor of the current style — the draw paths build
    /// their `CueParent` with it.
    pub fn anchor(&self) -> SubtitleAnchor {
        self.shared.inner.lock().unwrap().style.anchor
    }

    /// Replace the visual style. Drops the cached cue bitmap so the next
    /// draw re-rasterizes with the new colours/size. Cheap; safe to call
    /// from any thread at any time.
    pub fn set_style(&self, style: SubtitleStyle) {
        self.with_inner_notify(|inner| {
            inner.style = style;
            inner.invalidate();
        });
    }

    /// Push new cues into the active list. text_play sends a batch per
    /// segment; raw single-file delivery sends the whole list once.
    pub fn queue_cues(&self, cues: Vec<VttCue>) {
        self.with_inner_notify(|inner| inner.add_cues(cues));
    }

    /// Drop everything — called when the consumer switches subtitle
    /// track or disables subtitles.
    pub fn clear(&self) {
        self.with_inner_notify(|inner| {
            inner.cues.clear();
            inner.max_cue_span_ms = 0;
            inner.ass = None;
            inner.invalidate();
            inner.wanted = WantedKey::default();
        });
    }

    /// GLES-hook variant of `draw_into`: the bitmap for the cue active at
    /// the current PTS, or `None` when there's nothing to show (no cue, or
    /// the worker hasn't caught up yet after a seek). `generation`
    /// identifies the content so the hook can skip redundant uploads; the
    /// hook places it with [`cue_quad`] on the same `parent`.
    ///
    /// Pure lookup — no rasterization, no allocation. Runs on the render
    /// thread for every frame.
    #[cfg_attr(not(any(target_os = "android", target_os = "ios", target_os = "macos")), allow(dead_code))]
    pub fn active_bitmap(&self, parent: &CueParent) -> Option<std::sync::Arc<SubtitleBitmap>> {
        let (target_w, target_h) = (parent.width(), parent.height());
        let type_h = parent.type_height();
        let (pic_w, pic_h) = parent.picture_size();
        let (bitmap, resized) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let resized = inner.note_target(target_w, target_h, type_h) | inner.note_picture(pic_w, pic_h);
            let pts = inner.current_pts_ms;
            // Borrows the cue text for the lookup (a single active cue, the
            // normal case, allocates nothing).
            let bitmap = inner.shown_bitmap(pts, target_w);
            (bitmap, resized)
        };
        if resized {
            self.shared.wake.notify_one();
        }
        // A zero-size bitmap is the worker's record that the cue produced
        // no pixels (empty after layout). Kept in `ready` so it doesn't
        // retry forever; never drawn.
        bitmap.filter(|b| b.width > 0 && b.height > 0)
    }

    /// Called by the video sync loop before each render. Updates the
    /// "current PTS" the cue picker reads, nothing else.
    ///
    /// We do NOT evict cues here: time-based eviction made backward
    /// seeks lose subtitles permanently — once the user paused and
    /// rewound 10s to re-read a missed line, the cue had already been
    /// drained at the higher PTS and queue_cues never re-pushed it
    /// (text_play fetches each VTT segment once per playback). The
    /// `queue_cues` MAX_CUES cap is the only safety valve we need;
    /// for typical 2h content with 1 cue per ~3s we're well below it.
    pub fn set_pts_ms(&self, pts_ms: i64) {
        let wake = {
            let mut inner = self.shared.inner.lock().unwrap();
            // A jump back (seek): ASS events collide afresh.
            if inner.ass.is_some() && pts_ms + 1000 < inner.current_pts_ms {
                inner.ass_relayout = true;
            }
            let step = pts_ms - inner.current_pts_ms;
            if (5..=100).contains(&step) {
                inner.frame_ms = if inner.frame_ms > 0.0 { inner.frame_ms * 0.9 + step as f64 * 0.1 } else { step as f64 };
            }
            inner.current_pts_ms = pts_ms;
            // Called once per frame, so this must stay cheap: two binary
            // searches, and a notify only when the playhead actually moved
            // into a different cue (roughly once every few seconds).
            let wanted = inner.wanted_key(pts_ms);
            if wanted == inner.wanted {
                false
            } else {
                inner.wanted = wanted;
                true
            }
        };
        if wake {
            self.shared.wake.notify_one();
        }
    }

    /// Issue the draw into a caller-owned render pass. The caller has
    /// already attached the surface color target; we just emit one
    /// textured-quad draw placed by [`cue_quad`] inside `parent`.
    ///
    /// `parent` is the layout box (picture rect minus insets) on the
    /// surface being drawn; its size is what the worker rasterizes at.
    ///
    /// Draws whatever the worker has ready for the active cue. Nothing
    /// ready (no cue, or a seek the worker hasn't caught up with) simply
    /// draws nothing this frame.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn draw_into(&self, render_pass: &mut wgpu::RenderPass<'_>, parent: &CueParent) {
        if parent.target_w == 0 || parent.target_h == 0 {
            return;
        }
        let (target_w, target_h) = (parent.width(), parent.height());
        let type_h = parent.type_height();
        // No rasterizer thread (the browser build, or a spawn failure):
        // bake the wanted cue right here. Cue changes are rare (seconds
        // apart) and one rasterization is a few ms, so paying it on the
        // render path beats having no subtitles at all.
        let (pic_w, pic_h) = parent.picture_size();
        if self.worker.is_none() {
            self.shared.inner.lock().unwrap().note_picture(pic_w, pic_h);
            self.rasterize_inline(target_w, target_h, type_h);
        }
        let (bitmap, resized) = {
            let mut inner = self.shared.inner.lock().unwrap();
            let resized = inner.note_target(target_w, target_h, type_h) | inner.note_picture(pic_w, pic_h);
            let pts = inner.current_pts_ms;
            let bitmap = inner.shown_bitmap(pts, target_w);
            (bitmap, resized)
        };
        if resized {
            self.shared.wake.notify_one();
        }
        let bitmap = match bitmap {
            // Zero-size = the worker found nothing to draw for this cue.
            Some(b) if b.width > 0 && b.height > 0 => b,
            _ => return,
        };

        // Same geometry as the GLES path: `cue_quad` is the one place that
        // turns cue settings + parent rect into a quad.
        let transform = cue_quad(&bitmap, parent);

        let mut gpu = self.gpu.lock().unwrap();
        // Upload only when the content changed, into the one reused texture
        // (grown first when this cue doesn't fit, guard texels included).
        let stale = match gpu.as_ref() {
            Some(g) => g.generation != bitmap.generation,
            None => true,
        };
        if stale {
            let fits = gpu
                .as_ref()
                .is_some_and(|g| g.cap_w > bitmap.width && g.cap_h > bitmap.height);
            if !fits {
                let (old_w, old_h) = gpu.as_ref().map_or((0, 0), |g| (g.cap_w, g.cap_h));
                let max = self.device.limits().max_texture_dimension_2d;
                let cap_w = (bitmap.width + 1).max(old_w).next_multiple_of(256).min(max);
                let cap_h = (bitmap.height + 1).max(old_h).next_multiple_of(64).min(max);
                *gpu = Some(self.create_gpu_cue(cap_w, cap_h));
            }
            let g = gpu.as_mut().expect("just created");
            self.upload_cue(g, &bitmap);
            g.generation = bitmap.generation;
            g.bitmap_w = bitmap.width;
            g.bitmap_h = bitmap.height;
        }

        let g = gpu.as_mut().expect("populated above");
        let uv_scale = [g.bitmap_w as f32 / g.cap_w as f32, g.bitmap_h as f32 / g.cap_h as f32, 0.0, 0.0];
        let uniform = [
            transform[0], transform[1], transform[2], transform[3],
            uv_scale[0], uv_scale[1], uv_scale[2], uv_scale[3],
        ];
        if g.uniform != uniform {
            self.queue.write_buffer(
                &self.uniform_buffer,
                0,
                bytemuck::cast_slice(&[QuadUniform { transform, uv_scale }]),
            );
            g.uniform = uniform;
        }

        let pipeline = if self.direct_output.load(std::sync::atomic::Ordering::Relaxed) {
            self.pipeline_direct.get_or_init(|| {
                build_pipeline(
                    &self.device,
                    &self.shader,
                    &self.pipeline_layout,
                    DIRECT_OVERLAY_FORMAT,
                    "fs_main",
                )
            })
        } else if self.pq_output.load(std::sync::atomic::Ordering::Relaxed) {
            self.pipeline_pq.get_or_init(|| {
                build_pipeline(
                    &self.device,
                    &self.shader,
                    &self.pipeline_layout,
                    wgpu::TextureFormat::Rgba16Float,
                    "fs_main_pq",
                )
            })
        } else {
            &self.pipeline
        };
        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, &g.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
        // suppress unused-warning on surface_format
        let _ = self.surface_format;
    }

    /// HDR output session on/off: cues are then drawn PQ-encoded into an
    /// rgba16float target (SDR white → 203 nits). Takes effect next frame.
    #[cfg_attr(not(any(target_os = "ios", target_os = "macos")), allow(dead_code))]
    pub fn set_pq_output(&self, on: bool) {
        self.pq_output.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Apple direct mode on/off: the render layer is then a transparent
    /// overlay in [`DIRECT_OVERLAY_FORMAT`] and cues draw into that format.
    /// Takes effect next frame.
    #[cfg_attr(not(any(target_os = "ios", target_os = "macos")), allow(dead_code))]
    pub fn set_direct_output(&self, on: bool) {
        self.direct_output.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Allocate the texture/view/bind-group triple with room for cues up to
    /// `cap_w` x `cap_h` (minus the guard texel). Only called when a cue
    /// does not fit the current one.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn create_gpu_cue(&self, cap_w: u32, cap_h: u32) -> GpuCue {
        self.texture_allocs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("subtitle_cue_texture"),
            size: wgpu::Extent3d {
                width: cap_w,
                height: cap_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("subtitle_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });
        GpuCue {
            // Never a real generation (the worker starts at 1), so the
            // first draw always uploads.
            generation: 0,
            cap_w,
            cap_h,
            bitmap_w: 0,
            bitmap_h: 0,
            texture,
            view,
            bind_group,
            uniform: [f32::NAN; 8],
        }
    }

    /// Write `bitmap` into the top-left of the cue texture, plus a
    /// transparent column right of it and row below it: the quad samples
    /// bilinearly up to the cue's edge, and the texels past it may still
    /// hold an older, larger cue that would otherwise bleed in.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn upload_cue(&self, g: &GpuCue, bitmap: &SubtitleBitmap) {
        let write = |x: u32, y: u32, w: u32, h: u32, data: &[u8]| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &g.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        let (w, h) = (bitmap.width, bitmap.height);
        write(0, 0, w, h, &bitmap.rgba);
        let zeros = vec![0u8; ((w.max(h) + 1) * 4) as usize];
        write(w, 0, 1, h + 1, &zeros[..((h + 1) * 4) as usize]);
        write(0, h, w, 1, &zeros[..(w * 4) as usize]);
    }
}

impl SubtitleOverlay {
    /// One `raster_worker` iteration, run synchronously by the render path
    /// when there is no worker thread. Bakes at most one cue per call — the
    /// active one first, the prefetch on the next frame.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn rasterize_inline(&self, target_w: u32, target_h: u32, type_h: u32) {
        let job = {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.note_target(target_w, target_h, type_h);
            let job = next_job(&inner);
            if job.as_ref().is_some_and(|j| j.ass.is_some()) {
                inner.ass_relayout = false;
            }
            job
        };
        let Some(job) = job else { return };
        let started = crate::rt::Instant::now();
        let rasterized = bake(&job);
        let bake_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut inner = self.shared.inner.lock().unwrap();
        inner.store_baked(job, rasterized, bake_ms);
    }
}

impl Drop for SubtitleOverlay {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.take() {
            {
                let mut inner = self.shared.inner.lock().unwrap();
                inner.shutdown = true;
            }
            self.shared.wake.notify_all();
            // The worker only ever holds the lock for bookkeeping, and
            // rasterizing one cue is bounded work, so this can't hang.
            let _ = handle.join();
        }
    }
}

/// Rasterizer thread: keeps the cue under the playhead and the one after
/// it baked into `ready`, so the render path only ever does a lookup.
///
/// Parks on the condvar whenever everything wanted is already baked, and
/// is woken by whatever could have changed that: a cue boundary, new cues,
/// a restyle, a new font, or a resize. Rasterizes one cue at a time,
/// releasing the lock for each (the expensive part must not block a
/// frame), then re-checks — by then the playhead may have moved on.
///
/// The park condition is evaluated under the lock and `Condvar::wait`
/// releases it atomically, so a notify can't slip in between the check and
/// the wait.
fn raster_worker(shared: Arc<Shared>) {
    ass::background_priority();
    loop {
        let job = {
            let mut inner = shared.inner.lock().unwrap();
            loop {
                if inner.shutdown {
                    return;
                }
                match next_job(&inner) {
                    Some(job) => {
                        if job.ass.is_some() {
                            inner.ass_relayout = false;
                        }
                        break job;
                    }
                    // Everything wanted is ready. Park until it isn't.
                    None => inner = shared.wake.wait(inner).unwrap(),
                }
            }
        };

        // Lock released: this is the multi-millisecond part.
        let started = crate::rt::Instant::now();
        let rasterized = bake(&job);
        let bake_ms = started.elapsed().as_secs_f64() * 1000.0;

        let mut inner = shared.inner.lock().unwrap();
        if inner.shutdown {
            return;
        }
        inner.store_baked(job, rasterized, bake_ms);
        // Loop round: the other wanted cue may still need baking.
    }
}

impl Inner {
    /// File a finished job (worker or inline): into `ready`, and for an
    /// ASS frame into the bake-time average and the render scale.
    fn store_baked(&mut self, job: RasterJob, rasterized: Baked, bake_ms: f64) {
        // A resize or a restyle while we were working invalidates the
        // result — `ready` was cleared, and storing this would hand the
        // render path a bitmap for the wrong geometry.
        if self.epoch != job.epoch {
            return;
        }
        if let Some(a) = &job.ass {
            self.ass_bake_ms = if self.ass_bake_ms > 0.0 { self.ass_bake_ms * 0.8 + bake_ms * 0.2 } else { bake_ms };
            if a.brief {
                self.adapt_ass_scale();
            }
        }
        self.generation += 1;
        let generation = self.generation;
        let bitmap = finished_bitmap(job, rasterized, generation);
        log::debug!(
            "[subs] rasterized cue gen={} {}x{} at {}x{}",
            generation, bitmap.width, bitmap.height, bitmap.target_w, self.target_h
        );
        self.ready.push(std::sync::Arc::new(bitmap));
        if self.ready.len() > READY_SLOTS {
            self.ready.remove(0);
        }
    }
}

/// One cue to rasterize: everything the worker needs, copied out so the
/// lock is not held while it works.
struct RasterJob {
    fonts: rasterizer::FontSet,
    text: String,
    layout: CueLayout,
    style: SubtitleStyle,
    target_w: u32,
    /// See `CueParent::picture_h` — what the glyph size scales with.
    type_h: u32,
    /// `Inner::epoch` when the job was cut.
    epoch: u64,
    /// An ASS frame instead of a text cue.
    ass: Option<AssJob>,
}

/// One ASS frame to render.
struct AssJob {
    script: Arc<AssScript>,
    show: AssShow,
    renderer: Arc<Mutex<ass::AssRenderer>>,
    frame: ass::Frame,
    /// Forget earlier collision placements first (after a seek).
    relayout: bool,
    /// Rendered at this fraction of the picture size (see
    /// `Inner::ass_scale`).
    scale: f32,
    /// Replaced soon (animation, frame-by-frame signs): the frames the
    /// render scale adapts to.
    brief: bool,
}

/// A job's output.
enum Baked {
    Text(Option<rasterizer::CueRaster>),
    Ass(Option<ass::Composite>),
}

/// Do the expensive part of a job (no lock held).
fn bake(job: &RasterJob) -> Baked {
    match &job.ass {
        None => Baked::Text(rasterizer::rasterize_cue(
            &job.fonts, &job.text, &job.layout, job.target_w, job.type_h, &job.style,
        )),
        Some(a) => {
            let mut r = a.renderer.lock().unwrap();
            if a.relayout {
                r.reset_layout();
            }
            let started = crate::rt::Instant::now();
            let imgs = r.render(&a.script, &a.show.key.events, a.show.time_ms, a.frame);
            let rendered = crate::rt::Instant::now();
            let out = ass::composite(&imgs, a.frame.width, a.frame.height);
            let render_ms = rendered.duration_since(started).as_secs_f64() * 1000.0;
            let composite_ms = rendered.elapsed().as_secs_f64() * 1000.0;
            log::trace!(
                "[subs/ass] {} events -> {} images in {:.1} + {:.1} ms",
                a.show.key.events.len(),
                imgs.len(),
                render_ms,
                composite_ms
            );
            let px = out.as_ref().map_or(0, |c| c.width as u64 * c.height as u64);
            ass_perf(|p| {
                p.frames += 1;
                p.render_ms += render_ms;
                p.render_max_ms = p.render_max_ms.max(render_ms + composite_ms);
                p.composite_ms += composite_ms;
                p.max_events = p.max_events.max(a.show.key.events.len());
                p.max_images = p.max_images.max(imgs.len());
                p.max_px = p.max_px.max(px);
                p.min_scale = p.min_scale.min(a.scale);
            });
            Baked::Ass(out)
        }
    }
}

/// The next cue that needs rasterizing, or `None` when everything wanted
/// is already in `ready`. Split out of the worker so the borrow of
/// `inner` ends before the guard is re-assigned in the wait loop.
fn next_job(inner: &Inner) -> Option<RasterJob> {
    if let Some(track) = &inner.ass {
        if inner.picture_w == 0 || inner.picture_h == 0 {
            return None;
        }
        let font = inner.font.as_ref()?;
        let pts = inner.current_pts_ms;
        for (k, show) in inner.ass_wanted(pts).into_iter().enumerate() {
            let Some(show) = show else { continue };
            // A frame due now that a held frame already stands in for and
            // that is replaced before it could be baked (animation, or
            // frame-by-frame signs): bake the one the prefetch aims at.
            if k == 0 && inner.held_ass(pts).is_some() && inner.ass_now_superseded(&show, pts) {
                continue;
            }
            if inner.ready_for_ass(&show.key).is_none() {
                let storage = (inner.video_w > 0).then_some((inner.video_w, inner.video_h));
                // Reduced resolution only for frames that are soon
                // replaced; a line of dialogue that stays up is sharp.
                let brief = show.key.tick >= 0 || inner.ass_next_edge(show.time_ms).is_some_and(|e| e - show.time_ms < 500);
                let scale = ass_render_cap(inner.picture_w, inner.picture_h) * if brief { inner.ass_scale } else { 1.0 };
                let (fw, fh) = ((inner.picture_w as f32 * scale).round() as u32, (inner.picture_h as f32 * scale).round() as u32);
                let frame = ass::frame_for(&track.script, fw.max(1), fh.max(1), storage, inner.style.size_scale as f64);
                return Some(RasterJob {
                    fonts: rasterizer::FontSet { primary: Arc::clone(font), fallback: inner.fallback.clone() },
                    text: String::new(),
                    layout: CueLayout::DEFAULT,
                    style: inner.style,
                    target_w: inner.picture_w,
                    type_h: inner.type_h,
                    epoch: inner.epoch,
                    ass: Some(AssJob {
                        script: Arc::clone(&track.script),
                        show,
                        renderer: Arc::clone(&inner.ass_renderer),
                        frame,
                        relayout: inner.ass_relayout,
                        scale,
                        brief,
                    }),
                });
            }
        }
        return None;
    }
    let font = inner.font.as_ref()?;
    // Before the first draw we don't know the surface size, so there is
    // nothing sensible to rasterize at yet.
    if inner.target_w == 0 || inner.target_h == 0 {
        return None;
    }
    for (text, layout) in inner.wanted_shows(inner.current_pts_ms).into_iter().flatten() {
        if inner.ready_for(&text, &layout, inner.target_w).is_none() {
            return Some(RasterJob {
                fonts: rasterizer::FontSet {
                    primary: Arc::clone(font),
                    fallback: inner.fallback.clone(),
                },
                text: text.into_owned(),
                layout,
                style: inner.style,
                target_w: inner.target_w,
                type_h: inner.type_h,
                epoch: inner.epoch,
                ass: None,
            });
        }
    }
    None
}

/// Wrap a rasterizer result as the `ready` entry for `job`. `None` (the
/// cue laid out to nothing) becomes a zero-size bitmap so the job counts
/// as done and the worker doesn't spin on it; both draw paths skip
/// zero-size entries.
fn finished_bitmap(job: RasterJob, baked: Baked, generation: u64) -> SubtitleBitmap {
    let rasterized = match baked {
        Baked::Text(r) => r,
        Baked::Ass(c) => {
            let (key, time_ms, ass_scale) = match job.ass {
                Some(a) => (Some(a.show.key), a.show.time_ms, 1.0 / a.scale),
                None => (None, 0, 1.0),
            };
            let (width, height, rgba, origin) = match c {
                Some(c) => (c.width, c.height, c.rgba, Some((c.left as f32 * ass_scale, c.top as f32 * ass_scale))),
                None => (0, 0, Vec::new(), None),
            };
            return SubtitleBitmap {
                rgba,
                width,
                height,
                generation,
                line_height_px: 0,
                layout: CueLayout::DEFAULT,
                origin,
                scale: ass_scale,
                text: String::new(),
                target_w: job.target_w,
                ass_key: key,
                ass_time_ms: time_ms,
            };
        }
    };
    let (width, height, rgba, line_height_px) = match rasterized {
        Some(r) => (r.width, r.height, r.rgba, r.line_height_px),
        None => (0, 0, Vec::new(), 0),
    };
    SubtitleBitmap {
        rgba,
        width,
        height,
        generation,
        line_height_px,
        layout: job.layout,
        origin: None,
        scale: 1.0,
        text: job.text,
        target_w: job.target_w,
        ass_key: None,
        ass_time_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: i64, end_ms: i64, text: &str) -> VttCue {
        VttCue {
            start_ms,
            end_ms,
            text: text.to_string(),
            settings: String::new(),
            layout: CueLayout::DEFAULT,
            ass: None,
        }
    }

    /// Build an `Inner` the way `queue_cues` would, so `max_cue_span_ms`
    /// stays consistent with the cue list.
    pub(super) fn inner_with(cues: Vec<VttCue>) -> Inner {
        let mut sorted = cues;
        sorted.sort_by_key(|c| c.start_ms);
        let max_cue_span_ms = sorted
            .iter()
            .map(|c| c.end_ms - c.start_ms)
            .max()
            .unwrap_or(0);
        Inner {
            cues: sorted,
            current_pts_ms: 0,
            frame_ms: 0.0,
            ass_bake_ms: 0.0,
            ass_scale: 1.0,
            ass_scale_age: 0,
            font: None,
            fallback: None,
            style: SubtitleStyle::DEFAULT,
            ready: Vec::new(),
            generation: 0,
            target_w: 1920,
            target_h: 1080,
            type_h: 1080,
            epoch: 0,
            wanted: WantedKey::default(),
            shutdown: false,
            max_cue_span_ms,
            ass: None,
            ass_renderer: Arc::new(Mutex::new(ass::AssRenderer::new())),
            picture_w: 1920,
            picture_h: 1080,
            ass_relayout: false,
            video_w: 0,
            video_h: 0,
        }
    }

    /// The linear scan `active_index` replaced. The binary-searched
    /// version must agree with it for every PTS, or a cue would silently
    /// stop showing up.
    fn linear_index(inner: &Inner, pts_ms: i64) -> Option<usize> {
        inner.cues.iter().position(|c| c.is_active(pts_ms))
    }

    fn assert_matches_linear(inner: &Inner, range: std::ops::Range<i64>) {
        for pts in range {
            assert_eq!(
                inner.active_index(pts),
                linear_index(inner, pts),
                "mismatch at pts={pts}ms"
            );
        }
    }

    #[test]
    fn active_index_matches_linear_scan_on_sequential_cues() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(2000, 4000, "second"),
            cue(5000, 7500, "third after a gap"),
            cue(7500, 9000, "fourth"),
        ]);
        // Covers before the first cue, every boundary, both gaps, and past
        // the last cue.
        assert_matches_linear(&inner, -500..10_000);
    }

    #[test]
    fn active_index_matches_linear_scan_on_overlapping_cues() {
        // Overlapping cues are legal WebVTT: a long cue can still be on
        // screen when a later one starts. The window must not miss the
        // earlier (and therefore lower-indexed) one that is still active.
        let inner = inner_with(vec![
            cue(0, 8000, "a very long cue"),
            cue(1000, 2000, "short overlapping cue"),
            cue(3000, 4000, "another short one"),
            cue(9000, 10_000, "後"),
        ]);
        assert_matches_linear(&inner, -500..11_000);
        // First-in-start-order wins, exactly as `find` did.
        assert_eq!(inner.active_index(1500), Some(0));
    }

    #[test]
    fn active_index_handles_backward_seek() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(2000, 4000, "second"),
            cue(4000, 6000, "third"),
        ]);
        // No cursor state to invalidate: jumping around must be stable.
        assert_eq!(inner.active_index(5000), Some(2));
        assert_eq!(inner.active_index(1000), Some(0));
        assert_eq!(inner.active_index(5000), Some(2));
        assert_eq!(inner.active_index(3000), Some(1));
    }

    #[test]
    fn overlapping_cues_are_shown_together_in_start_order() {
        let inner = inner_with(vec![cue(1000, 5000, "Sign"), cue(2000, 3000, "Speaker"), cue(6000, 7000, "Later")]);
        let shown = |inner: &Inner, pts| inner.shown_at(pts).map(|(t, _)| t.into_owned());
        assert_eq!(shown(&inner, 1500).as_deref(), Some("Sign"));
        assert_eq!(shown(&inner, 2500).as_deref(), Some("Sign\nSpeaker"));
        assert_eq!(shown(&inner, 4000).as_deref(), Some("Sign"));
        assert_eq!(shown(&inner, 5500), None);
        // The worker wakes when the set of active cues changes, not only the first.
        assert_ne!(inner.wanted_key(1500), inner.wanted_key(2500));
        assert_ne!(inner.wanted_key(2500), inner.wanted_key(4000));
        // At 1500 the prefetch is what shows when "Speaker" starts: both.
        let [_, prefetch] = inner.wanted_shows(1500);
        assert_eq!(prefetch.map(|(t, _)| t.into_owned()).as_deref(), Some("Sign\nSpeaker"));
    }

    #[test]
    fn wanted_indices_prefetches_the_next_cue() {
        let inner = inner_with(vec![
            cue(0, 2000, "first"),
            cue(3000, 5000, "second"),
            cue(6000, 8000, "third"),
        ]);
        // Mid-cue: the active one plus the one that starts next.
        assert_eq!(inner.wanted_indices(1000), [Some(0), Some(1)]);
        // In the gap: nothing active, but the upcoming cue is prefetched
        // so it's ready the instant it starts.
        assert_eq!(inner.wanted_indices(2500), [None, Some(1)]);
        // Past the last cue: nothing left to bake.
        assert_eq!(inner.wanted_indices(9000), [None, None]);
        // Before anything starts.
        assert_eq!(inner.wanted_indices(-1000), [None, Some(0)]);
    }

    #[test]
    fn note_target_tolerates_small_drift_but_drops_on_resize() {
        let mut inner = inner_with(vec![cue(0, 2000, "first")]);
        inner.ready.push(std::sync::Arc::new(SubtitleBitmap {
            rgba: vec![0; 4],
            width: 1,
            height: 1,
            generation: 1,
            line_height_px: 1,
            layout: CueLayout::DEFAULT,
            origin: None,
            scale: 1.0,
            text: "first".to_string(),
            target_w: 1920,
            ass_key: None,
            ass_time_ms: 0,
        }));
        // Within 5%: keep the rasterization, a dragged window resizes by
        // a pixel at a time and re-baking each step is pure churn.
        assert!(!inner.note_target(1940, 1080, 1080));
        assert!(inner.ready_for("first", &CueLayout::DEFAULT, 1940).is_some());
        // A real resize invalidates it.
        assert!(inner.note_target(1280, 720, 720));
        assert!(inner.ready.is_empty());
    }

    /// Spin the real worker against a hand-built `Shared` (no GPU
    /// involved) and wait for it to produce bitmaps, with a deadline so a
    /// regression that parks the worker fails instead of hanging.
    fn run_worker_until(
        shared: &Arc<Shared>,
        label: &str,
        done: impl Fn(&Inner) -> bool,
    ) {
        let deadline = crate::rt::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            {
                let inner = shared.inner.lock().unwrap();
                if done(&inner) {
                    return;
                }
            }
            assert!(
                crate::rt::Instant::now() < deadline,
                "worker never reached: {label}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn spawn_worker(inner: Inner) -> (Arc<Shared>, std::thread::JoinHandle<()>) {
        let shared = Arc::new(Shared {
            inner: Mutex::new(inner),
            wake: std::sync::Condvar::new(),
        });
        let handle = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || raster_worker(shared))
        };
        (shared, handle)
    }

    fn stop_worker(shared: &Arc<Shared>, handle: std::thread::JoinHandle<()>) {
        shared.inner.lock().unwrap().shutdown = true;
        shared.wake.notify_all();
        handle.join().unwrap();
    }

    #[test]
    fn worker_bakes_the_active_cue_and_prefetches_the_next() {
        let mut inner = inner_with(vec![
            cue(0, 2000, "first line"),
            cue(3000, 5000, "second line"),
            cue(9000, 10_000, "third line"),
        ]);
        inner.font = rasterizer::default_font().map(Arc::new);
        inner.current_pts_ms = 500;
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();

        // Both the cue on screen and the one after it, without any
        // further prompting — that prefetch is what keeps a cue change
        // off the render thread.
        run_worker_until(&shared, "first two cues baked", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1920).is_some()
                && i.ready_for("second line", &CueLayout::DEFAULT, 1920).is_some()
        });
        {
            let inner = shared.inner.lock().unwrap();
            let bmp = inner.ready_for("first line", &CueLayout::DEFAULT, 1920).unwrap();
            assert!(bmp.width > 0 && bmp.height > 0, "cue rasterized to nothing");
            assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);
            // The third cue is neither active nor next, so it must not
            // have been baked speculatively.
            assert!(inner.ready_for("third line", &CueLayout::DEFAULT, 1920).is_none());
        }
        stop_worker(&shared, handle);
    }

    #[test]
    fn worker_follows_the_playhead_across_a_cue_boundary() {
        let mut inner = inner_with(vec![
            cue(0, 2000, "first line"),
            cue(3000, 5000, "second line"),
            cue(6000, 8000, "third line"),
        ]);
        inner.font = rasterizer::default_font().map(Arc::new);
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();
        run_worker_until(&shared, "initial bake", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1920).is_some()
        });

        // Jump the playhead onto the second cue, the way set_pts_ms does.
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.current_pts_ms = 3500;
        }
        shared.wake.notify_one();
        run_worker_until(&shared, "third cue prefetched", |i| {
            i.ready_for("third line", &CueLayout::DEFAULT, 1920).is_some()
        });
        {
            // Bounded memory: the oldest entry is evicted rather than the
            // list growing for the whole movie.
            let inner = shared.inner.lock().unwrap();
            assert!(inner.ready.len() <= READY_SLOTS);
            // The cue actually on screen must have survived the eviction.
            assert!(inner.ready_for("second line", &CueLayout::DEFAULT, 1920).is_some());
        }
        stop_worker(&shared, handle);
    }

    #[test]
    fn worker_parks_when_there_is_no_surface_size_yet() {
        // Before the first draw the target is 0x0 and nothing can be
        // sized, so the worker must idle rather than spin or panic.
        let mut inner = inner_with(vec![cue(0, 2000, "first line")]);
        inner.font = rasterizer::default_font().map(Arc::new);
        inner.target_w = 0;
        inner.target_h = 0;
        let (shared, handle) = spawn_worker(inner);
        shared.wake.notify_one();
        std::thread::sleep(std::time::Duration::from_millis(50));
        {
            let inner = shared.inner.lock().unwrap();
            assert!(inner.ready.is_empty());
        }

        // First draw reports a size — now it has something to do.
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.note_target(1280, 720, 720);
        }
        shared.wake.notify_one();
        run_worker_until(&shared, "bake after first draw", |i| {
            i.ready_for("first line", &CueLayout::DEFAULT, 1280).is_some()
        });
        stop_worker(&shared, handle);
    }

    #[test]
    fn active_index_on_empty_list_is_none() {
        let inner = inner_with(Vec::new());
        assert_eq!(inner.active_index(0), None);
        assert_eq!(inner.active_index(1_000_000), None);
    }

    #[test]
    fn queue_cues_keeps_max_span_current() {
        // A stale-small span would shrink the search window and hide a
        // long cue, so pushing a longer cue has to widen it.
        let mut inner = inner_with(vec![cue(0, 1000, "short")]);
        assert_eq!(inner.max_cue_span_ms, 1000);
        let long = cue(500, 20_000, "very long cue");
        inner.max_cue_span_ms = inner.max_cue_span_ms.max(long.end_ms - long.start_ms);
        inner.cues.push(long);
        inner.cues.sort_by_key(|c| c.start_ms);
        assert_eq!(inner.max_cue_span_ms, 19_500);
        assert_eq!(inner.active_index(19_000), Some(1));
    }

    pub(super) fn bitmap(w: u32, h: u32, lh: u32, settings: &str) -> SubtitleBitmap {
        SubtitleBitmap {
            rgba: Vec::new(),
            width: w,
            height: h,
            generation: 1,
            line_height_px: lh,
            layout: CueLayout::parse(settings),
            origin: None,
            scale: 1.0,
            text: String::new(),
            target_w: 0,
            ass_key: None,
            ass_time_ms: 0,
        }
    }

    #[test]
    fn parent_is_the_aspect_fitted_picture_minus_the_bottom_inset() {
        // 16:9 content on a 16:9 surface: the whole surface.
        let p = CueParent::fit(1920, 1080, 1920, 1080, 0, SubtitleAnchor::Picture);
        assert_eq!((p.left, p.top, p.right, p.bottom), (0.0, 0.0, 1920.0, 1080.0));
        // 2.39:1 film on 16:9: letterboxed, the parent is the picture
        // (PlayerView puts SubtitleView inside the AspectRatioFrameLayout).
        let p = CueParent::fit(1920, 1080, 2390, 1000, 0, SubtitleAnchor::Picture);
        assert_eq!((p.left, p.right), (0.0, 1920.0));
        let pic_h = (1920.0f32 / 2.39).round();
        assert!((p.top - ((1080.0 - pic_h) / 2.0).floor()).abs() <= 1.0);
        assert!((p.bottom - (p.top + pic_h)).abs() <= 1.0);
        // Pillarboxed 4:3.
        let p = CueParent::fit(1920, 1080, 640, 480, 0, SubtitleAnchor::Picture);
        assert_eq!((p.top, p.bottom), (0.0, 1080.0));
        assert_eq!(p.left, 240.0);
        assert_eq!(p.right, 1680.0);
        // The inset only raises the bottom (view padding), never below it.
        let p = CueParent::fit(1920, 1080, 1920, 1080, 100, SubtitleAnchor::Picture);
        assert_eq!(p.bottom, 980.0);
        let p = CueParent::fit(1920, 1080, 2390, 1000, 40, SubtitleAnchor::Picture);
        assert!((p.bottom - (p.top + pic_h)).abs() <= 1.0, "inset inside the bar changes nothing");
        // Unknown content size → whole target.
        assert_eq!(CueParent::fit(1280, 720, 0, 0, 0, SubtitleAnchor::Picture), CueParent::fit(1280, 720, 1280, 720, 0, SubtitleAnchor::Picture));
    }

    #[test]
    fn screen_anchor_ignores_the_letterbox() {
        // Same widescreen film: with the default Screen anchor the box is
        // the whole surface (minus the inset), so the cue lands in the bar.
        let p = CueParent::fit(1920, 1080, 2390, 1000, 0, SubtitleAnchor::Screen);
        assert_eq!((p.left, p.top, p.right, p.bottom), (0.0, 0.0, 1920.0, 1080.0));
        let (x, y) = place_cue(400, 60, 40, &CueLayout::DEFAULT, &p);
        assert_eq!(x, 760.0);
        // 8 % of the PICTURE (803px), not of the 1080px surface. Taking it
        // from the surface left a 86px gap under the cue and pushed the block
        // up towards the picture — on a tall window far enough that a
        // three-line cue ended up back inside it.
        let pic_h = (1080.0f32 * (1920.0 / 1080.0) / 2.39).round();
        assert_eq!(pic_h, 803.0);
        assert_eq!(y, 1080.0 - 60.0 - (pic_h * BOTTOM_PADDING_FRACTION).floor());
        let p = CueParent::fit(1920, 1080, 2390, 1000, 100, SubtitleAnchor::Screen);
        assert_eq!(p.bottom, 980.0);
    }

    #[test]
    fn text_size_tracks_the_picture_not_the_surface() {
        // 16:9 film on a nearly-portrait window — the shape that exposed this.
        // With the Screen anchor the box is the whole surface, so the cue
        // lands in the letterbox bar; but the glyphs must still scale with the
        // picture, or the block grows taller than the bar and climbs into the
        // picture it was deliberately placed below.
        let p = CueParent::fit(958, 1064, 1920, 1080, 0, SubtitleAnchor::Screen);
        assert_eq!(p.height(), 1064, "box is the surface");
        assert_eq!(p.type_height(), 539, "size comes from the picture");

        // Picture anchor: the two agree, which is ExoPlayer's geometry.
        let p = CueParent::fit(958, 1064, 1920, 1080, 0, SubtitleAnchor::Picture);
        assert_eq!(p.height(), p.type_height());

        // The inset shrinks what the size derives from, like view padding.
        let p = CueParent::fit(958, 1064, 1920, 1080, 39, SubtitleAnchor::Screen);
        assert_eq!(p.type_height(), 500);
    }

    #[test]
    fn default_cue_sits_bottom_padding_above_the_parent_bottom_centered() {
        let p = CueParent::fit(1920, 1080, 1920, 1080, 0, SubtitleAnchor::Picture);
        let (x, y) = place_cue(400, 60, 40, &CueLayout::DEFAULT, &p);
        assert_eq!(x, 760.0);
        // textTop = parentBottom - textHeight - (int)(parentHeight * 0.08)
        assert_eq!(y, 1080.0 - 60.0 - 86.0);
        // As NDC: centered horizontally, bottom edge 8% up.
        let q = cue_quad(&bitmap(400, 60, 40, ""), &p);
        assert!((q[0]).abs() < 1e-6);
        assert!((q[1] - (-1.0 + 60.0 / 1080.0 + 2.0 * 86.0 / 1080.0)).abs() < 1e-5);
        assert!((q[2] - 400.0 / 1920.0).abs() < 1e-6);
    }

    #[test]
    fn line_settings_place_vertically_like_subtitle_painter() {
        let p = CueParent::fit(1000, 1000, 1000, 1000, 0, SubtitleAnchor::Picture);
        // line:10% → box top at 10% (lineAnchor start).
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%"), &p).1, 100.0);
        // ,center / ,end move the anchored edge.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%,center"), &p).1, 75.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:10%,end"), &p).1, 50.0);
        // Integer lines: 0 = top, 2 = two line heights down, -1 = flush
        // with the bottom, -2 one line height up.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:0"), &p).1, 0.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:2"), &p).1, 50.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-1"), &p).1, 950.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-2"), &p).1, 925.0);
        // Clamped into the parent: 100% with a start anchor would hang
        // below the bottom; -100 lines would poke out the top.
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:100%"), &p).1, 950.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-100"), &p).1, 0.0);
        // The bottom inset raises the parent bottom for `-1` and default alike.
        let p = CueParent::fit(1000, 1000, 1000, 1000, 100, SubtitleAnchor::Picture);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::parse("line:-1"), &p).1, 850.0);
        assert_eq!(place_cue(100, 50, 25, &CueLayout::DEFAULT, &p).1, 900.0 - 50.0 - 72.0);
    }

    #[test]
    fn position_and_align_place_horizontally_like_subtitle_painter() {
        let p = CueParent::fit(1000, 1000, 1000, 1000, 0, SubtitleAnchor::Picture);
        // align:left → position 0 / anchor start → flush left; right → flush right.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:left"), &p).0, 0.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:right"), &p).0, 800.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:start"), &p).0, 500.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:end"), &p).0, 300.0);
        // Explicit position with derived anchor (center → middle).
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%"), &p).0, 200.0);
        // Explicit anchor.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%,line-left"), &p).0, 300.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:30%,line-right"), &p).0, 100.0);
        // Clamped into the parent on both sides.
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:5%"), &p).0, 0.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("position:95%"), &p).0, 800.0);
        // Inside a pillarboxed picture the parent's own edges apply.
        let p = CueParent::fit(1920, 1080, 640, 480, 0, SubtitleAnchor::Picture);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:left"), &p).0, 240.0);
        assert_eq!(place_cue(200, 50, 25, &CueLayout::parse("align:right"), &p).0, 1480.0);
    }
}

/// GPU regression cover for the wgpu cue path (desktop, web, Apple), run on
/// whatever adapter the machine has; skipped when there is none.
///
/// One cue texture serves every cue (it only grows). Asserted: few texture
/// allocations over many cue sizes, and nothing but the cue's own ink changes
/// the picture - no box behind it, and no older, larger cue bleeding in
/// around a smaller one.
#[cfg(all(test, not(target_os = "android"), not(target_arch = "wasm32")))]
mod gpu_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    const W: u32 = 1280;
    const H: u32 = 720;
    /// Opaque mid-gray: a light box (additive or opaque) and a dark one
    /// both show up as a run of changed pixels.
    const BG: [u8; 4] = [128, 128, 128, 255];

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(fut)
    }

    fn device() -> Option<(Arc<wgpu::Device>, Arc<wgpu::Queue>)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some((Arc::new(device), Arc::new(queue)))
    }

    /// Clear to `BG`, draw the overlay, read the target back (RGBA rows).
    fn frame(dev: &wgpu::Device, queue: &wgpu::Queue, overlay: &SubtitleOverlay, target: &wgpu::Texture) -> Vec<u8> {
        let view = target.create_view(&Default::default());
        let parent = CueParent::fit(W, H, W, H, 0, SubtitleAnchor::default());
        let mut enc = dev.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: BG[0] as f64 / 255.0,
                            g: BG[1] as f64 / 255.0,
                            b: BG[2] as f64 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            overlay.draw_into(&mut pass, &parent);
        }
        let padded = (W * 4).div_ceil(256) * 256;
        let buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (padded * H) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(H) },
            },
            wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = dev.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            let s = (y * padded) as usize;
            out.extend_from_slice(&data[s..s + (W * 4) as usize]);
        }
        out
    }

    /// Bounding box of every pixel the cue changed, and how many it changed.
    fn changed(px: &[u8]) -> Option<((u32, u32, u32, u32), usize)> {
        let (mut x0, mut y0, mut x1, mut y1, mut n) = (u32::MAX, u32::MAX, 0, 0, 0);
        for (i, p) in px.chunks(4).enumerate() {
            if p[..3] != BG[..3] {
                let (x, y) = (i as u32 % W, i as u32 / W);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                n += 1;
            }
        }
        (n > 0).then_some(((x0, y0, x1, y1), n))
    }

    #[test]
    fn cue_texture_is_reused_and_nothing_but_ink_changes_the_picture() {
        let Some((dev, queue)) = device() else {
            eprintln!("SKIP: no GPU adapter");
            return;
        };
        let overlay = SubtitleOverlay::new(Arc::clone(&dev), Arc::clone(&queue), wgpu::TextureFormat::Rgba8Unorm);
        let target = dev.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Big and tiny cues alternating: every cue changes the bitmap size,
        // and every tiny one lands in a texture that last held a big one.
        let big = "You don't touch anything.\nYou don't move anything.\nAnd you never, ever look back.";
        let texts: Vec<&str> = (0..16).map(|i| if i % 2 == 0 { big } else { "Ok." }).collect();
        let cues = texts
            .iter()
            .enumerate()
            .map(|(i, t)| VttCue {
                start_ms: i as i64 * 1000,
                end_ms: i as i64 * 1000 + 999,
                text: t.to_string(),
                settings: String::new(),
                layout: CueLayout::DEFAULT,
                ass: None,
            })
            .collect();
        overlay.queue_cues(cues);

        // Reference picture of each text from an overlay that has only ever
        // seen that one cue: no reused texture, no history.
        let reference = |text: &str| -> Vec<u8> {
            let fresh = SubtitleOverlay::new(Arc::clone(&dev), Arc::clone(&queue), wgpu::TextureFormat::Rgba8Unorm);
            fresh.queue_cues(vec![VttCue {
                start_ms: 0,
                end_ms: 999,
                text: text.to_string(),
                settings: String::new(),
                layout: CueLayout::DEFAULT,
                ass: None,
            }]);
            fresh.set_pts_ms(500);
            for _ in 0..400 {
                let px = frame(&dev, &queue, &fresh, &target);
                if changed(&px).is_some() {
                    return px;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("reference for {text:?} never drawn");
        };
        let (ref_big, ref_tiny) = (reference(big), reference("Ok."));
        for (name, px) in [("big", &ref_big), ("tiny", &ref_tiny)] {
            let ((x0, y0, x1, y1), n) = changed(px).unwrap();
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as usize;
            // Ink (fill + shadow + antialiasing) covers well under half of
            // its own bounding box; a box behind the text covers all of it.
            assert!(n * 10 < area * 6, "{name} cue: {n} of {area} px in its box changed — a filled box, not text");
        }

        for (i, text) in texts.iter().enumerate() {
            overlay.set_pts_ms(i as i64 * 1000 + 500);
            let want = if *text == big { &ref_big } else { &ref_tiny };
            // The worker rasterizes off-thread; redraw until this cue lands.
            // With the shared texture it must land EXACTLY as a fresh overlay
            // draws it: an older, larger cue left in the texture must not
            // show through (bleed at the edge, or the whole texture sampled).
            let mut last = Vec::new();
            let mut ok = false;
            for _ in 0..400 {
                last = frame(&dev, &queue, &overlay, &target);
                if &last == want {
                    ok = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if !ok {
                let diff = last.chunks(4).zip(want.chunks(4)).filter(|(a, b)| a != b).count();
                panic!("cue {i} ({text:?}) differs from a fresh overlay's rendering in {diff} px");
            }
        }
        let allocs = overlay.texture_allocs.load(Ordering::Relaxed);
        assert!(allocs <= 2, "{allocs} cue textures allocated for {} cues — expected reuse", texts.len());
    }

    fn ass_cues(events: &str) -> Vec<VttCue> {
        let script = format!(
            "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\n[V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Default,Roboto,30,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,2,20,20,20,1\n\n\
             [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{events}"
        );
        crate::parsers::sidecar::parse(script.as_bytes(), crate::parsers::sidecar::SubtitleFormat::Ass, None, 0)
            .unwrap()
            .cues
    }

    fn wait_frame(dev: &wgpu::Device, queue: &wgpu::Queue, overlay: &SubtitleOverlay, target: &wgpu::Texture, ok: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let mut last = Vec::new();
        for _ in 0..400 {
            last = frame(dev, queue, overlay, target);
            if ok(&last) {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        last
    }

    fn target(dev: &wgpu::Device) -> wgpu::Texture {
        dev.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    #[test]
    fn ass_frames_are_drawn_where_the_script_puts_them() {
        let Some((dev, queue)) = device() else {
            eprintln!("SKIP: no GPU adapter");
            return;
        };
        let overlay = SubtitleOverlay::new(Arc::clone(&dev), Arc::clone(&queue), wgpu::TextureFormat::Rgba8Unorm);
        let target = target(&dev);
        // A 100x100 square at script (100, 50) on a 640x360 script is
        // 200x200 px at (200, 100) on the 1280x720 picture.
        overlay.queue_cues(ass_cues(
            "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\an7\\pos(100,50)\\p1}m 0 0 l 100 0 100 100 0 100\n",
        ));
        overlay.set_pts_ms(1000);
        let px = wait_frame(&dev, &queue, &overlay, &target, |p| changed(p).is_some());
        let ((x0, y0, x1, y1), n) = changed(&px).expect("the ASS frame was drawn");
        assert_eq!((x0, y0, x1, y1), (200, 100, 399, 299));
        assert_eq!(n, 200 * 200);
        let white = &px[((150 * W + 300) * 4) as usize..][..3];
        assert_eq!(white, &[255, 255, 255]);
    }

    #[test]
    fn a_typeset_script_keeps_every_event() {
        // Frame-by-frame typesetting: one event per sign per 42 ms frame.
        let events: String = (0..12_000)
            .map(|i| {
                let t = |ms: i64| format!("0:{:02}:{:02}.{:02}", ms / 60_000, ms / 1000 % 60, ms / 10 % 100);
                format!("Dialogue: 0,{},{},Default,,0,0,0,,{{\\pos(320,180)}}sign {i}\n", t(i * 42), t(i * 42 + 42))
            })
            .collect();
        let mut inner = super::tests::inner_with(Vec::new());
        inner.add_cues(ass_cues(&events));
        assert_eq!(inner.cues.len(), 12_000);
        assert_eq!(inner.ass_active(21), vec![0], "the first sign is still there");
        assert_eq!(inner.ass_active(11_999 * 42 + 1), vec![11_999]);
    }

    #[test]
    fn animated_ass_prefetches_the_next_video_frame() {
        let mut inner = super::tests::inner_with(ass_cues("Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\fad(500,0)}Sign\n"));
        let script = inner.cues.iter().find_map(|c| c.ass.as_ref().map(|a| Arc::clone(&a.script))).unwrap();
        let animated = script.events.iter().map(event_animates).collect();
        inner.ass = Some(AssTrack { script, animated });
        inner.frame_ms = 41.7;
        let [now, next] = inner.ass_wanted(1000);
        assert_eq!(now.unwrap().key.tick, 1000);
        assert_eq!(next.unwrap().key.tick, 1042);
        // A 60 ms bake can't make the next frame: aim at the one after.
        inner.ass_bake_ms = 60.0;
        assert_eq!(inner.ass_wanted(1000)[1].as_ref().unwrap().key.tick, 1083);
    }

    #[test]
    fn ass_render_size_is_capped_and_adapts() {
        assert_eq!(ass_render_cap(1920, 858), 1.0);
        assert_eq!(ass_render_cap(3840, 1607), 0.5);
        assert_eq!(ass_render_cap(3840, 2160), 0.5);
        let mut inner = super::tests::inner_with(Vec::new());
        inner.frame_ms = 41.7;
        // Slow bakes step the scale down to its floor ...
        inner.ass_bake_ms = 200.0;
        for _ in 0..200 {
            inner.adapt_ass_scale();
        }
        assert_eq!(inner.ass_scale, 0.5);
        // ... and fast ones back up to 1.
        inner.ass_bake_ms = 10.0;
        for _ in 0..200 {
            inner.adapt_ass_scale();
        }
        assert_eq!(inner.ass_scale, 1.0);
    }

    #[test]
    fn a_reduced_ass_frame_is_drawn_at_picture_size() {
        let mut b = super::tests::bitmap(960, 429, 0, "");
        b.origin = Some((100.0, 50.0));
        b.scale = 2.0;
        let p = CueParent::fit(1920, 858, 1920, 858, 0, SubtitleAnchor::Picture);
        let q = cue_quad(&b, &p);
        // full picture width, centred
        assert!((q[2] - 1.0).abs() < 1e-6 && (q[0] - (100.0 + 960.0) / 960.0 + 1.0).abs() < 1e-6, "{q:?}");
    }

    #[test]
    fn frame_by_frame_signs_are_prefetched_a_bake_ahead() {
        // A static sign re-timed every 50 ms (a new event per frame).
        let events: String = (0..40)
            .map(|i| format!("Dialogue: 0,0:00:{:02}.{:02},0:00:{:02}.{:02},Default,,0,0,0,,{{\\pos(320,{i})}}sign\n", i * 5 / 100, i * 5 % 100, (i + 1) * 5 / 100, (i + 1) * 5 % 100))
            .collect();
        let mut inner = super::tests::inner_with(Vec::new());
        inner.add_cues(ass_cues(&events));
        let [now, next] = inner.ass_wanted(1010);
        assert_eq!(next.as_ref().unwrap().time_ms, 1050, "fast bakes: the next edge");
        assert!(!inner.ass_now_superseded(now.as_ref().unwrap(), 1010));
        // 120 ms bakes: the sign on screen 150 ms from now; the current one
        // is gone before it could be baked.
        inner.ass_bake_ms = 120.0;
        let [now, next] = inner.ass_wanted(1010);
        assert_eq!(next.unwrap().time_ms, 1160);
        assert!(inner.ass_now_superseded(now.as_ref().unwrap(), 1010));
    }

    #[test]
    fn ass_holds_the_last_frame_until_the_new_one_is_baked() {
        // An animated sign (fades in) under a line of static dialogue that
        // starts between two video frames.
        let mut inner = super::tests::inner_with(ass_cues(
            "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\fad(500,0)}Sign\n\
             Dialogue: 0,0:00:01.01,0:00:03.00,Default,,0,0,0,,Dialogue\n",
        ));
        let script = inner.cues.iter().find_map(|c| c.ass.as_ref().map(|a| Arc::clone(&a.script))).unwrap();
        let animated = script.events.iter().map(event_animates).collect();
        inner.ass = Some(AssTrack { script, animated });
        let baked = |inner: &mut Inner, pts: i64| {
            let show = inner.ass_show(pts).unwrap();
            inner.ready.push(Arc::new(SubtitleBitmap {
                rgba: vec![0; 4],
                width: 1,
                height: 1,
                generation: show.time_ms as u64,
                line_height_px: 0,
                layout: CueLayout::DEFAULT,
                origin: Some((0.0, 0.0)),
                scale: 1.0,
                text: String::new(),
                target_w: 1920,
                ass_key: Some(show.key),
                ass_time_ms: show.time_ms,
            }));
        };
        baked(&mut inner, 990);
        // A frame baked for 2 ms off stands in (the prefetch predicts the
        // next frame time from the average interval).
        assert_eq!(inner.shown_bitmap(992, 1920).unwrap().ass_time_ms, 990);
        // 1015 ms: the dialogue has started — a key nobody prefetched. The
        // sign frame from 990 stays up meanwhile.
        let held = inner.shown_bitmap(1015, 1920).expect("held frame");
        assert_eq!(held.ass_time_ms, 990);
        // Once baked, the exact frame wins.
        baked(&mut inner, 1015);
        let shown = inner.shown_bitmap(1015, 1920).unwrap();
        assert!(shown.ass_key.as_ref().unwrap().events.len() == 2);
        // Never a frame from the future (a backward seek) or from long ago.
        assert!(inner.shown_bitmap(500, 1920).is_none());
        assert!(inner.shown_bitmap(2990, 1920).is_none());
        // Nothing active: nothing drawn, held frames or not.
        assert!(inner.shown_bitmap(6000, 1920).is_none());
    }

    #[test]
    fn animated_ass_frames_follow_the_playhead() {
        let Some((dev, queue)) = device() else {
            eprintln!("SKIP: no GPU adapter");
            return;
        };
        let overlay = SubtitleOverlay::new(Arc::clone(&dev), Arc::clone(&queue), wgpu::TextureFormat::Rgba8Unorm);
        let target = target(&dev);
        // Black to white over the first second.
        overlay.queue_cues(ass_cues(
            "Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,{\\an7\\pos(0,0)\\c&H000000&\\t(0,1000,\\c&HFFFFFF&)\\p1}m 0 0 l 100 0 100 100 0 100\n",
        ));
        let at = |pts: i64| -> u8 {
            overlay.set_pts_ms(pts);
            // rendered for the frame's own time
            let want = (pts as f64 / 1000.0 * 255.0) as i32;
            let px = wait_frame(&dev, &queue, &overlay, &target, |p| (p[((50 * W + 50) * 4) as usize] as i32 - want).abs() <= 2);
            px[((50 * W + 50) * 4) as usize]
        };
        let a = at(100);
        let b = at(500);
        let c = at(2000);
        assert!(a < b && b < c, "{a} {b} {c}");
        assert_eq!(c, 255);
    }
}
