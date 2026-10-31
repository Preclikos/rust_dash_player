//! Android's video sink: the wgpu renderer, or — on GPUs without OpenGL ES
//! 3.0, which wgpu's GL backend needs — a GPU-less overlay.
//!
//! The GPU-less overlay only works in direct mode: MediaCodec renders the
//! picture straight into the host's video Surface (no GPU involved), and
//! this sink writes the subtitle bitmaps into the overlay window with the
//! CPU (`ANativeWindow_lock`). The bitmaps come from the same
//! [`SubtitleOverlay`] the GPU path uses — cue timing, layout and
//! rasterization are shared — so SRT/VTT and ASS look the same; only the
//! final blend differs.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::subtitle::{cue_quad, CueParent, SubtitleBitmap, SubtitleOverlay};
use super::video::VideoRenderer;
use super::VideoSink;
use crate::decoders::{DecodedVideoFrame, PlatformFrame};
use crate::parsers::vtt::VttCue;
use crate::PhysicalSize;

/// What the player renders through on Android.
pub enum AndroidVideo {
    Gpu(VideoRenderer),
    /// No usable GPU (see [`CpuOverlay`]).
    Cpu(CpuOverlay),
}

impl VideoSink for AndroidVideo {
    fn render_frame(&self, frame: DecodedVideoFrame) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            match self {
                AndroidVideo::Gpu(r) => r.render_frame(frame).await,
                AndroidVideo::Cpu(c) => c.render_frame(frame),
            }
        }
    }

    fn debug_output(&self) -> String {
        match self {
            AndroidVideo::Gpu(r) => VideoSink::debug_output(r),
            AndroidVideo::Cpu(c) => c.debug_output(),
        }
    }

    fn resize(&self, size: PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            if let AndroidVideo::Gpu(r) = self {
                r.resize(size).await;
            }
        }
    }

    fn change_frame_size(&self, size: PhysicalSize<u32>) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            match self {
                AndroidVideo::Gpu(r) => VideoSink::change_frame_size(r, size).await,
                AndroidVideo::Cpu(c) => c.set_frame_size(size),
            }
        }
    }

    fn set_android_surface(&self, window: usize) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            match self {
                AndroidVideo::Gpu(r) => r.set_android_surface(window).await,
                AndroidVideo::Cpu(c) => c.set_window(window),
            }
        }
    }

    fn set_subtitle_font(&self, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            AndroidVideo::Gpu(r) => r.set_subtitle_font(bytes),
            AndroidVideo::Cpu(c) => c.overlay.set_font(bytes),
        }
    }

    fn add_subtitle_font(&self, bytes: Vec<u8>) -> usize {
        match self {
            AndroidVideo::Gpu(r) => r.add_subtitle_font(bytes),
            AndroidVideo::Cpu(c) => c.overlay.add_ass_font(bytes),
        }
    }

    fn set_ass_offload(&self, offload: Arc<dyn super::subtitle::AssOffload>) {
        // The CPU overlay keeps ASS on its own worker: an offload hands back
        // GPU masks it could not blend.
        if let AndroidVideo::Gpu(r) = self {
            r.set_ass_offload(offload);
        }
    }

    fn set_subtitle_style(&self, style: crate::SubtitleStyle) {
        match self {
            AndroidVideo::Gpu(r) => r.set_subtitle_style(style),
            AndroidVideo::Cpu(c) => c.overlay.set_style(style),
        }
    }

    fn queue_subtitle_cues(&self, cues: Vec<VttCue>) {
        match self {
            AndroidVideo::Gpu(r) => r.queue_subtitle_cues(cues),
            AndroidVideo::Cpu(c) => {
                if !cues.is_empty() {
                    c.overlay.queue_cues(cues);
                }
            }
        }
    }

    fn clear_subtitles(&self) {
        match self {
            AndroidVideo::Gpu(r) => r.clear_subtitles(),
            AndroidVideo::Cpu(c) => c.overlay.clear(),
        }
    }

    fn set_subtitle_pts(&self, pts_ms: i64) {
        match self {
            AndroidVideo::Gpu(r) => r.set_subtitle_pts(pts_ms),
            AndroidVideo::Cpu(c) => c.overlay.set_pts_ms(pts_ms),
        }
    }

    fn set_hdr_tonemap_params(&self, params: crate::HdrTonemapParams) {
        if let AndroidVideo::Gpu(r) = self {
            r.set_hdr_tonemap_params(params);
        }
    }

    fn set_display_hdr_types(&self, mask: u32) {
        if let AndroidVideo::Gpu(r) = self {
            r.set_display_hdr_types(mask);
        }
    }

    fn set_subtitle_safe_bottom_px(&self, px: u32) {
        match self {
            AndroidVideo::Gpu(r) => r.set_subtitle_safe_bottom_px(px),
            AndroidVideo::Cpu(c) => c.safe_bottom_px.store(px, Ordering::Relaxed),
        }
    }

    fn begin_presentation(&self) {
        match self {
            AndroidVideo::Gpu(r) => r.begin_presentation(),
            AndroidVideo::Cpu(c) => c.invalidate(),
        }
    }
}

/// Subtitles without a GPU: the overlay window is filled by the CPU.
///
/// The window is written only when the visible bitmap changes (a cue
/// starts or ends, an ASS animation tick), and only the area of the old
/// and the new bitmap: the rest of the buffer keeps its transparent pixels
/// (the window copies the previous buffer back for a dirty-rect lock).
pub struct CpuOverlay {
    overlay: Arc<SubtitleOverlay>,
    window: Mutex<OverlayWindow>,
    /// Decoded picture size, packed `w << 32 | h` (0 = unknown).
    frame_size: AtomicU64,
    safe_bottom_px: AtomicU32,
}

struct OverlayWindow {
    /// `ANativeWindow*` (owned by the host shell), 0 = detached.
    ptr: usize,
    /// Bitmap generation on the window: 0 = none, `u64::MAX` = unknown
    /// (fresh window), the next frame redraws.
    shown_gen: u64,
    /// Where the shown bitmap sits; cleared when it changes.
    drawn: Option<Rect>,
    lock_failures: u32,
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn union(self, o: Rect) -> Rect {
        Rect {
            left: self.left.min(o.left),
            top: self.top.min(o.top),
            right: self.right.max(o.right),
            bottom: self.bottom.max(o.bottom),
        }
    }

    fn clamp(self, w: i32, h: i32) -> Rect {
        Rect {
            left: self.left.clamp(0, w),
            top: self.top.clamp(0, h),
            right: self.right.clamp(0, w),
            bottom: self.bottom.clamp(0, h),
        }
    }
}

/// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM`, the layout of the cue bitmaps
/// (premultiplied RGBA, which is what SurfaceFlinger blends).
const RGBA_8888: i32 = ndk_sys::ANativeWindow_LegacyFormat::WINDOW_FORMAT_RGBA_8888.0 as i32;

impl CpuOverlay {
    /// `window` is the host's overlay `ANativeWindow*` (may be 0).
    pub fn new(window: usize) -> Self {
        let sink = CpuOverlay {
            overlay: Arc::new(SubtitleOverlay::new_cpu()),
            window: Mutex::new(OverlayWindow {
                ptr: 0,
                shown_gen: u64::MAX,
                drawn: None,
                lock_failures: 0,
            }),
            frame_size: AtomicU64::new(0),
            safe_bottom_px: AtomicU32::new(0),
        };
        sink.set_window(window);
        log::warn!("[renderer] no GPU renderer: direct-mode video, subtitles composited on the CPU");
        sink
    }

    fn render_frame(&self, frame: DecodedVideoFrame) {
        let desired_present_ns = frame.desired_present_ns;
        match frame.native {
            PlatformFrame::MediaCodecDirect(direct) => {
                direct.render_at(desired_present_ns.max(1));
                self.present();
            }
            #[allow(unreachable_patterns)]
            _ => {
                static WARN_ONCE: std::sync::Once = std::sync::Once::new();
                WARN_ONCE.call_once(|| {
                    log::warn!("[renderer] no GPU: only direct-mode frames can be shown; frame dropped (no video Surface?)");
                });
            }
        }
    }

    fn debug_output(&self) -> String {
        let w = self.window.lock().unwrap();
        let (ww, wh) = if w.ptr == 0 {
            (0, 0)
        } else {
            let win = w.ptr as *mut ndk_sys::ANativeWindow;
            unsafe { (ndk_sys::ANativeWindow_getWidth(win), ndk_sys::ANativeWindow_getHeight(win)) }
        };
        format!("cpu overlay (no GPU)  window {ww}x{wh}")
    }

    fn set_frame_size(&self, size: PhysicalSize<u32>) {
        self.frame_size
            .store((size.width as u64) << 32 | size.height as u64, Ordering::Relaxed);
        self.overlay.set_video_size(size.width, size.height);
        self.invalidate();
    }

    /// Attach (non-zero) or detach (0) the overlay window. Returns once the
    /// previous window is no longer touched.
    fn set_window(&self, window: usize) {
        let mut w = self.window.lock().unwrap();
        w.ptr = window;
        w.shown_gen = u64::MAX;
        w.drawn = None;
        if window != 0 {
            let win = window as *mut ndk_sys::ANativeWindow;
            let st = unsafe { ndk_sys::ANativeWindow_setBuffersGeometry(win, 0, 0, RGBA_8888) };
            log::info!(
                "[renderer] cpu overlay window attached ({}x{}, geometry={})",
                unsafe { ndk_sys::ANativeWindow_getWidth(win) },
                unsafe { ndk_sys::ANativeWindow_getHeight(win) },
                st
            );
        } else {
            log::info!("[renderer] cpu overlay window detached");
        }
    }

    /// Redraw on the next frame (new title, new picture size).
    fn invalidate(&self) {
        self.window.lock().unwrap().shown_gen = u64::MAX;
    }

    /// Bring the window up to the bitmap active now. Cheap when nothing
    /// changed (a generation compare).
    fn present(&self) {
        let mut w = self.window.lock().unwrap();
        if w.ptr == 0 {
            return;
        }
        let win = w.ptr as *mut ndk_sys::ANativeWindow;
        let (ww, wh) = unsafe { (ndk_sys::ANativeWindow_getWidth(win), ndk_sys::ANativeWindow_getHeight(win)) };
        if ww <= 0 || wh <= 0 {
            return;
        }
        let packed = self.frame_size.load(Ordering::Relaxed);
        let (fw, fh) = ((packed >> 32) as u32, packed as u32);
        let parent = CueParent::fit(
            ww as u32,
            wh as u32,
            fw,
            fh,
            self.safe_bottom_px.load(Ordering::Relaxed),
            self.overlay.anchor(),
        );
        // An ASS frame left as GPU masks has no pixels to copy; the CPU
        // overlay asks for composited frames, so this is only a guard.
        let bitmap = self
            .overlay
            .active_bitmap(&parent)
            .filter(|b| b.masks.is_none() && b.rgba.len() >= b.width as usize * b.height as usize * 4);
        let gen = bitmap.as_ref().map(|b| b.generation).unwrap_or(0);
        if gen == w.shown_gen {
            return;
        }
        let placed = bitmap.as_ref().map(|b| (b, placement(b, &parent, ww, wh)));
        let full = Rect { left: 0, top: 0, right: ww, bottom: wh };
        let dirty = if w.shown_gen == u64::MAX {
            full
        } else {
            match (w.drawn, placed.as_ref().map(|(_, r)| *r)) {
                (Some(a), Some(b)) => a.union(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => {
                    w.shown_gen = gen;
                    return;
                }
            }
        }
        .clamp(ww, wh);

        let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
        let mut bounds = ndk_sys::ARect {
            left: dirty.left,
            top: dirty.top,
            right: dirty.right,
            bottom: dirty.bottom,
        };
        let st = unsafe { ndk_sys::ANativeWindow_lock(win, &mut buf, &mut bounds) };
        if st != 0 || buf.bits.is_null() {
            w.lock_failures += 1;
            if w.lock_failures == 1 || w.lock_failures % 100 == 0 {
                log::warn!("[renderer] cpu overlay: ANativeWindow_lock failed ({st}), {} times", w.lock_failures);
            }
            // Not drawn: try again on the next frame.
            w.shown_gen = u64::MAX;
            return;
        }
        if buf.format != RGBA_8888 {
            log::warn!("[renderer] cpu overlay: window format {} is not RGBA_8888; subtitles off", buf.format);
            unsafe { ndk_sys::ANativeWindow_unlockAndPost(win) };
            w.shown_gen = gen;
            return;
        }
        // The window may grow the dirty area (e.g. to the whole buffer when
        // it cannot copy the previous one back): clear what it hands out.
        let area = Rect {
            left: bounds.left,
            top: bounds.top,
            right: bounds.right,
            bottom: bounds.bottom,
        }
        .clamp(buf.width, buf.height);
        let stride = buf.stride.max(buf.width) as usize * 4;
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(buf.bits as *mut u8, stride * buf.height.max(0) as usize)
        };
        for y in area.top..area.bottom {
            let row = y as usize * stride;
            pixels[row + area.left as usize * 4..row + area.right as usize * 4].fill(0);
        }
        if let Some((b, r)) = &placed {
            blit(pixels, stride, buf.width, buf.height, b, *r);
        }
        unsafe { ndk_sys::ANativeWindow_unlockAndPost(win) };
        w.drawn = placed.map(|(_, r)| r.clamp(ww, wh));
        w.shown_gen = gen;
        w.lock_failures = 0;
    }
}

/// The bitmap's rectangle on the window (px), from the same quad the GPU
/// paths draw.
fn placement(b: &SubtitleBitmap, parent: &CueParent, ww: i32, wh: i32) -> Rect {
    let [cx, cy, hw, hh] = cue_quad(b, parent);
    let left = ((cx - hw + 1.0) * 0.5 * ww as f32).round() as i32;
    let top = ((1.0 - (cy + hh)) * 0.5 * wh as f32).round() as i32;
    let width = (hw * ww as f32).round() as i32;
    let height = (hh * wh as f32).round() as i32;
    Rect { left, top, right: left + width.max(1), bottom: top + height.max(1) }
}

/// Copy premultiplied RGBA `b` into the (cleared) buffer at `r`, scaling
/// nearest-neighbour when `r` is not the bitmap's size (an ASS frame
/// rendered at reduced resolution). Clipped to the buffer.
fn blit(dst: &mut [u8], stride: usize, buf_w: i32, buf_h: i32, b: &SubtitleBitmap, r: Rect) {
    let (bw, bh) = (b.width as i32, b.height as i32);
    let (rw, rh) = (r.right - r.left, r.bottom - r.top);
    if bw <= 0 || bh <= 0 || rw <= 0 || rh <= 0 {
        return;
    }
    let x0 = r.left.max(0);
    let x1 = r.right.min(buf_w);
    let y0 = r.top.max(0);
    let y1 = r.bottom.min(buf_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let src_stride = bw as usize * 4;
    if rw == bw && rh == bh {
        for y in y0..y1 {
            let sy = (y - r.top) as usize;
            let sx0 = (x0 - r.left) as usize;
            let n = (x1 - x0) as usize * 4;
            let s = sy * src_stride + sx0 * 4;
            let d = y as usize * stride + x0 as usize * 4;
            dst[d..d + n].copy_from_slice(&b.rgba[s..s + n]);
        }
        return;
    }
    for y in y0..y1 {
        let sy = (((y - r.top) as i64 * bh as i64) / rh as i64).min(bh as i64 - 1) as usize;
        let drow = y as usize * stride;
        let srow = sy * src_stride;
        for x in x0..x1 {
            let sx = (((x - r.left) as i64 * bw as i64) / rw as i64).min(bw as i64 - 1) as usize;
            let d = drow + x as usize * 4;
            let s = srow + sx * 4;
            dst[d..d + 4].copy_from_slice(&b.rgba[s..s + 4]);
        }
    }
}
