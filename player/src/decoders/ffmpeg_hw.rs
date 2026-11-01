#![cfg(any(target_os = "windows", target_os = "linux"))]

use std::collections::VecDeque;
use std::sync::Arc;

use ffmpeg_next::Packet;
use ffmpeg_sys_next::{
    av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, AVBufferRef, AVCodecContext,
    AVHWDeviceType, AVPixelFormat,
};
use crate::parsers::mp4::{append_hevc_header, hevc_nalu_bodies};

use super::{DecodedVideoFrame, DecoderError, HwVideoDecoder, PlatformFrame, VideoCodec, VideoDecoderParams};

/// See `configure`: frames the pipeline keeps out of the decoder's own pool.
/// Each costs one decoder surface (4K P010 ~25 MB, 1080p NV12 ~3 MB).
const EXTRA_HW_FRAMES: i32 = 10;

pub struct FfmpegHwDecoder {
    decoder: Option<ffmpeg_next::decoder::Video>,
    /// Owned hw-device — only created+held when this decoder was NOT handed a
    /// `shared_device` (the legacy / fallback path). Null otherwise.
    hw_device_ctx: *mut AVBufferRef,
    /// Preferred path: a hw-device created once per play() cycle and shared
    /// across the initial decoder + every ABR-swap decoder, so a swap never
    /// recreates the GPU device. See [`SharedHwDevice`].
    shared_device: Option<Arc<SharedHwDevice>>,
    /// Stamped onto every decoded frame (from configure params).
    color: crate::decoders::VideoColorInfo,
    /// Frames taken out of the decoder inside `submit` (to make room when
    /// `send_packet` said EAGAIN), handed out by `try_recv` first.
    pending: VecDeque<ffmpeg_next::util::frame::Video>,
    /// The hw frame pool size was logged (once per decoder).
    pool_logged: bool,
}

unsafe impl Send for FfmpegHwDecoder {}

/// LUID of the GPU the video renderer runs on (0 = not known yet). Set by the
/// renderer when it creates its device; one renderer per process in practice.
#[cfg(target_os = "windows")]
static RENDER_ADAPTER_LUID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record the renderer's GPU, so decoders open their D3D11VA device on it.
#[cfg(target_os = "windows")]
pub fn set_render_adapter_luid(luid: u64) {
    RENDER_ADAPTER_LUID.store(luid, std::sync::atomic::Ordering::Relaxed);
}

/// DXGI adapter index of the renderer's GPU, the form FFmpeg's D3D11VA
/// device takes ("0", "1", ... in `EnumAdapters` order). `None` when the
/// renderer's GPU is not known or not found: FFmpeg then uses the default
/// adapter, as before.
#[cfg(target_os = "windows")]
fn render_adapter_index() -> Option<(u32, String)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let luid = RENDER_ADAPTER_LUID.load(std::sync::atomic::Ordering::Relaxed);
    if luid == 0 {
        return None;
    }
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            if let Ok(desc) = adapter.GetDesc1() {
                let l = desc.AdapterLuid;
                if ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64 == luid {
                    let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
                    return Some((i, String::from_utf16_lossy(&desc.Description[..name_len])));
                }
            }
            i += 1;
        }
    }
    None
}

/// Create a fresh platform hw-device context (D3D11VA on Windows, VAAPI on
/// Linux). The caller owns the returned ref and must `av_buffer_unref` it.
fn create_hwdevice_ctx() -> Result<*mut AVBufferRef, DecoderError> {
    #[cfg(target_os = "windows")]
    let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
    #[cfg(target_os = "linux")]
    let device_type = AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;

    // Bump FFmpeg log level to VERBOSE and wire up the Rust log forwarder
    // (installs av_log_set_callback the first time; idempotent after that).
    // VERBOSE is needed to surface D3D11VA HRESULTs like 0x80070057 from
    // CreateTexture2D — without it the error is wrapped as AVERROR_UNKNOWN
    // with no driver detail.
    crate::ffmpeg_log::set_log_level(crate::ffmpeg_log::LogLevel::Verbose);

    // Windows: decode on the renderer's GPU (see render_adapter_index).
    #[cfg(target_os = "windows")]
    let device_name: Option<std::ffi::CString> = match render_adapter_index() {
        Some((index, name)) => {
            log::info!("[ffmpeg_hw] D3D11VA on adapter {} ({}), the renderer's GPU", index, name);
            std::ffi::CString::new(index.to_string()).ok()
        }
        None => {
            log::info!("[ffmpeg_hw] D3D11VA on the default adapter (renderer GPU unknown)");
            None
        }
    };
    #[cfg(not(target_os = "windows"))]
    let device_name: Option<std::ffi::CString> = None;

    let mut ctx: *mut AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        av_hwdevice_ctx_create(
            &mut ctx,
            device_type,
            device_name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        return Err(format!("av_hwdevice_ctx_create failed: {}", ret).into());
    }
    Ok(ctx)
}

/// A platform hw-device context created ONCE and shared (ref-counted) across
/// every `FfmpegHwDecoder` spawned within a play() cycle.
///
/// Creating a D3D11VA / VAAPI device is slow, and on Windows it takes a
/// driver/DXGI-global lock that serialises with the wgpu D3D12 present and the
/// DWM compositor — so doing it on *every* ABR swap (the old behaviour, where
/// each decoder called `av_hwdevice_ctx_create` in `configure`) hitched the
/// whole desktop UI for a moment per switch. Sharing one device means a swap
/// only re-opens the codec (cheap, no device creation, no compositor stall).
///
/// The underlying device is multithread-safe and `AVBufferRef` refcounting is
/// atomic; we only ever `av_buffer_ref` / `av_buffer_unref` the pointer, so
/// sharing it across threads is sound.
pub struct SharedHwDevice {
    ctx: *mut AVBufferRef,
}

unsafe impl Send for SharedHwDevice {}
unsafe impl Sync for SharedHwDevice {}

impl SharedHwDevice {
    /// Create the device. Do this once at play() setup and hand clones of the
    /// `Arc` to every decoder via [`FfmpegHwDecoder::new_shared`].
    pub fn new() -> Result<Arc<Self>, DecoderError> {
        Ok(Arc::new(Self {
            ctx: create_hwdevice_ctx()?,
        }))
    }

    /// A new owned reference to the shared context, for handing to a codec
    /// context's `hw_device_ctx` (which takes ownership of the ref given).
    fn new_ref(&self) -> *mut AVBufferRef {
        unsafe { av_buffer_ref(self.ctx) }
    }
}

impl Drop for SharedHwDevice {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { av_buffer_unref(&mut self.ctx) };
        }
    }
}

/// Asks the hardware decoder about a video representation before playback
/// (see `decoders::support`): the same device type, on the same GPU, that
/// [`SharedHwDevice`] decodes with.
pub(crate) struct VideoProbe {
    device: SharedHwDevice,
}

impl VideoProbe {
    /// `None` when no hw device opens (then nothing is pruned; configure
    /// reports the failure as before).
    pub(crate) fn open() -> Option<Self> {
        match create_hwdevice_ctx() {
            Ok(ctx) => Some(Self { device: SharedHwDevice { ctx } }),
            Err(e) => {
                log::warn!("[support] no hw device to probe with: {e}");
                None
            }
        }
    }

    /// `Some(false)` only on a definite "cannot decode"; `None` when the
    /// question could not be asked.
    pub(crate) fn supports(&self, need: &super::support::VideoNeed) -> Option<bool> {
        unsafe {
            let dev = (*self.device.ctx).data as *mut ffmpeg_sys_next::AVHWDeviceContext;
            if dev.is_null() || (*dev).hwctx.is_null() {
                return None;
            }
            platform_supports(dev, self.device.ctx, need)
        }
    }
}

/// D3D11 video decoding: the profile must be on the device's list and take
/// the output format, and a decoder must actually be created at this picture
/// size, then dropped. The configuration count alone does not size-check
/// (an Intel UHD driver lists configurations for 16384x16384); creating the
/// decoder is how Chromium finds DXVA's resolution limits.
#[cfg(target_os = "windows")]
unsafe fn platform_supports(
    dev: *mut ffmpeg_sys_next::AVHWDeviceContext,
    _ctx: *mut AVBufferRef,
    need: &super::support::VideoNeed,
) -> Option<bool> {
    use super::support::VideoCodecNeed;
    use windows::core::{Interface, GUID};
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11VideoDevice, D3D11_DECODER_PROFILE_H264_VLD_NOFGT, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, D3D11_VIDEO_DECODER_CONFIG, D3D11_VIDEO_DECODER_DESC,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

    let hwctx = (*dev).hwctx as *mut crate::renderers::video::video_directx::AVD3D11VADeviceContext;
    let raw = (*hwctx).device;
    let device = ID3D11Device::from_raw_borrowed(&raw)?;
    let video: ID3D11VideoDevice = device.cast().ok()?;
    let guid: GUID = match (need.codec, need.ten_bit) {
        (VideoCodecNeed::Hevc, false) => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        (VideoCodecNeed::Hevc, true) => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
        (VideoCodecNeed::Avc { .. }, false) => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        // DXVA has no H.264 High 10 profile; FFmpeg's d3d11va cannot decode it.
        (VideoCodecNeed::Avc { .. }, true) => return Some(false),
    };
    let profiles = video.GetVideoDecoderProfileCount();
    let listed = (0..profiles).any(|i| video.GetVideoDecoderProfile(i).ok() == Some(guid));
    if !listed {
        return Some(false);
    }
    let format = if need.ten_bit { DXGI_FORMAT_P010 } else { DXGI_FORMAT_NV12 };
    if !video.CheckVideoDecoderFormat(&guid, format).map(|b| b.as_bool()).unwrap_or(false) {
        return Some(false);
    }
    let desc = D3D11_VIDEO_DECODER_DESC {
        Guid: guid,
        SampleWidth: need.width,
        SampleHeight: need.height,
        OutputFormat: format,
    };
    if !matches!(video.GetVideoDecoderConfigCount(&desc), Ok(n) if n > 0) {
        return Some(false);
    }
    let mut config = D3D11_VIDEO_DECODER_CONFIG::default();
    if video.GetVideoDecoderConfig(&desc, 0, &mut config).is_err() {
        return Some(false);
    }
    Some(video.CreateVideoDecoder(&desc, &config).is_ok())
}

/// VAAPI: the profile must be on the driver's list and take a VLD (decode)
/// config, and FFmpeg's frame constraints for that config give the largest
/// surface the driver decodes into.
#[cfg(target_os = "linux")]
unsafe fn platform_supports(
    dev: *mut ffmpeg_sys_next::AVHWDeviceContext,
    ctx: *mut AVBufferRef,
    need: &super::support::VideoNeed,
) -> Option<bool> {
    use super::support::VideoCodecNeed;
    type VADisplay = *mut std::ffi::c_void;
    extern "C" {
        fn vaMaxNumProfiles(dpy: VADisplay) -> i32;
        fn vaQueryConfigProfiles(dpy: VADisplay, profile_list: *mut i32, num_profiles: *mut i32) -> i32;
        fn vaCreateConfig(
            dpy: VADisplay,
            profile: i32,
            entrypoint: i32,
            attrib_list: *mut std::ffi::c_void,
            num_attribs: i32,
            config_id: *mut u32,
        ) -> i32;
        fn vaDestroyConfig(dpy: VADisplay, config_id: u32) -> i32;
    }
    // va.h VAProfile / VAEntrypoint values.
    const VA_PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
    const VA_PROFILE_H264_MAIN: i32 = 6;
    const VA_PROFILE_H264_HIGH: i32 = 7;
    const VA_PROFILE_HEVC_MAIN: i32 = 17;
    const VA_PROFILE_HEVC_MAIN10: i32 = 18;
    const VA_ENTRYPOINT_VLD: i32 = 1;

    let hwctx = (*dev).hwctx as *mut crate::renderers::video::video_vaapi::AVVAAPIDeviceContext;
    let display = (*hwctx).display as VADisplay;
    if display.is_null() {
        return None;
    }
    let profile = match need.codec {
        VideoCodecNeed::Hevc if need.ten_bit => VA_PROFILE_HEVC_MAIN10,
        VideoCodecNeed::Hevc => VA_PROFILE_HEVC_MAIN,
        // No VA profile for High 10.
        VideoCodecNeed::Avc { .. } if need.ten_bit => return Some(false),
        VideoCodecNeed::Avc { profile_idc: 66 } => VA_PROFILE_H264_CONSTRAINED_BASELINE,
        VideoCodecNeed::Avc { profile_idc: 77 } => VA_PROFILE_H264_MAIN,
        VideoCodecNeed::Avc { .. } => VA_PROFILE_H264_HIGH,
    };
    let max = vaMaxNumProfiles(display);
    if max <= 0 {
        return None;
    }
    let mut list = vec![0i32; max as usize];
    let mut n = 0i32;
    if vaQueryConfigProfiles(display, list.as_mut_ptr(), &mut n) != 0 {
        return None;
    }
    if !list[..n.clamp(0, max) as usize].contains(&profile) {
        return Some(false);
    }
    let mut config = 0u32;
    if vaCreateConfig(display, profile, VA_ENTRYPOINT_VLD, std::ptr::null_mut(), 0, &mut config) != 0 {
        // The profile exists but not for decoding (encode-only).
        return Some(false);
    }
    // AVVAAPIHWConfig { VAConfigID config_id; }
    let hwconfig = ffmpeg_sys_next::av_hwdevice_hwconfig_alloc(ctx) as *mut u32;
    let mut verdict = None;
    if !hwconfig.is_null() {
        *hwconfig = config;
        let mut c = ffmpeg_sys_next::av_hwdevice_get_hwframe_constraints(ctx, hwconfig as *const _);
        if !c.is_null() {
            let (mw, mh) = ((*c).max_width, (*c).max_height);
            verdict = if mw > 0 && mh > 0 {
                Some(need.width as i32 <= mw && need.height as i32 <= mh)
            } else {
                // The driver reports no limit: the profile is there.
                Some(true)
            };
            ffmpeg_sys_next::av_hwframe_constraints_free(&mut c);
        }
        ffmpeg_sys_next::av_free(hwconfig as *mut _);
    }
    vaDestroyConfig(display, config);
    verdict
}

impl FfmpegHwDecoder {
    /// Legacy / fallback: this decoder owns a freshly-created hw-device.
    pub fn new() -> Self {
        Self {
            decoder: None,
            hw_device_ctx: std::ptr::null_mut(),
            shared_device: None,
            color: Default::default(),
            pending: VecDeque::new(),
            pool_logged: false,
        }
    }

    /// Preferred: reuse a [`SharedHwDevice`] created once per play() cycle so
    /// an ABR swap re-opens only the codec, never recreates the GPU device.
    pub fn new_shared(device: Arc<SharedHwDevice>) -> Self {
        Self {
            decoder: None,
            hw_device_ctx: std::ptr::null_mut(),
            shared_device: Some(device),
            color: Default::default(),
            pending: VecDeque::new(),
            pool_logged: false,
        }
    }

    fn create_hw_device(&mut self) -> Result<(), DecoderError> {
        self.hw_device_ctx = create_hwdevice_ctx()?;
        Ok(())
    }
}

impl Drop for FfmpegHwDecoder {
    fn drop(&mut self) {
        // configure() now hands the codec context its OWN refcounted reference
        // (via av_buffer_ref) so the decoder's drop unrefs that one without
        // invalidating ours. Release ours here.
        if !self.hw_device_ctx.is_null() {
            unsafe { av_buffer_unref(&mut self.hw_device_ctx) };
        }
    }
}

// libavcodec calls `get_format` during stream-header parsing with a
// null-terminated array of pixel formats the decoder can produce for
// the negotiated stream. The default impl picks the first software
// format, which leaves `frame.hw_frames_ctx == NULL` and breaks
// renderers/video/video_frame.rs:101's hw_frames_ctx expectation.
//
// We pick the platform's HW format if it's on offer and otherwise
// return AV_PIX_FMT_NONE so libavcodec aborts decoder open — louder
// than silently sliding into software and panicking downstream.
#[cfg(target_os = "windows")]
const WANTED_HW: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_D3D11;
#[cfg(target_os = "linux")]
const WANTED_HW: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_VAAPI;

unsafe extern "C" fn select_hw_format(
    _ctx: *mut AVCodecContext,
    fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    // Snapshot the offered list so we can log it once. Critical for
    // diagnosing "decoder fails on this driver but works on another"
    // reports — different FFmpeg builds / GPU drivers offer different
    // sets, and a mismatch with WANTED_HW silently kills decoding.
    let mut offered = Vec::with_capacity(8);
    let mut p = fmts;
    while unsafe { *p } != AVPixelFormat::AV_PIX_FMT_NONE {
        offered.push(unsafe { *p });
        p = unsafe { p.add(1) };
    }
    log::info!(
        "[ffmpeg_hw] get_format offered: {:?}; want {:?}",
        offered,
        WANTED_HW
    );
    if offered.contains(&WANTED_HW) {
        WANTED_HW
    } else {
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}

impl HwVideoDecoder for FfmpegHwDecoder {
    fn name(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "D3D11VA (FFmpeg)"
        }
        #[cfg(target_os = "linux")]
        {
            "VAAPI (FFmpeg)"
        }
    }

    fn configure(&mut self, params: VideoDecoderParams) -> Result<(), DecoderError> {
        let codec_id = match params.codec {
            VideoCodec::Hevc => ffmpeg_next::codec::Id::HEVC,
            VideoCodec::H264 => ffmpeg_next::codec::Id::H264,
        };
        let codec = ffmpeg_next::decoder::find(codec_id)
            .ok_or_else(|| -> DecoderError { "cannot find FFmpeg decoder for codec".into() })?;

        let mut ctx = ffmpeg_next::codec::Context::new_with_codec(codec);

        // Hand the codec context a reference to the hw-device. Prefer the
        // shared device (so an ABR swap never recreates the D3D11/VAAPI device
        // — see SharedHwDevice); otherwise lazily create a decoder-owned one.
        // The codec context takes ownership of the ref it is given and unrefs
        // it on its own drop.
        let device_ref = if let Some(shared) = &self.shared_device {
            shared.new_ref()
        } else {
            if self.hw_device_ctx.is_null() {
                self.create_hw_device()?;
            }
            unsafe { av_buffer_ref(self.hw_device_ctx) }
        };
        unsafe {
            (*ctx.as_mut_ptr()).hw_device_ctx = device_ref;
            (*ctx.as_mut_ptr()).get_format = Some(select_hw_format);
            // Surfaces the pipeline holds OUTSIDE the decoder. The auto-sized
            // pool (20 for HEVC: DPB + threading) only covers the decoder
            // itself, while we keep up to 8 frames in the frame channel, 4 in
            // the reorder buffer and 1-2 at the renderer, plus 2 in the gate
            // and 1 in the pump while a warm ABR handoff waits. That ran the
            // pool dry ("Static surface pool size exceeded" -> send_packet
            // ENOMEM -> pipeline retry, a frozen second) on 3 of 4 switch runs.
            (*ctx.as_mut_ptr()).extra_hw_frames = EXTRA_HW_FRAMES;
        }

        // No pre-allocated hw_frames_ctx: let hevc_d3d11va2 auto-create it
        // inside its hwaccel->init() after get_format returns AV_PIX_FMT_D3D11.
        // A manually pre-set context with format=AV_PIX_FMT_D3D11 caused the
        // hwaccel to log "Invalid pixfmt for hwaccel!" and abort because the
        // frames context format was evaluated before avctx->pix_fmt was set.
        // FFmpeg auto-derives sw_format from the SPS (NV12 for Main 8-bit,
        // P010 for Main 10), so the auto-allocated context is always correct.

        let mut decoder = ctx
            .decoder()
            .video()
            .map_err(|e| -> DecoderError { format!("decoder open: {}", e).into() })?;

        // Feed the hvcC NALUs (VPS/SPS/PPS) so the decoder can parse subsequent slices.
        // Each element of `hvcc_nalus` is a raw NALU body (no prefix); append_hevc_header
        // prepends the 00 00 00 01 start code.
        for nalu_data in &params.hvcc_nalus {
            let nalu = append_hevc_header(nalu_data.clone());
            let mut packet = Packet::new(nalu.len());
            packet.data_mut().unwrap().clone_from_slice(&nalu);
            decoder
                .send_packet(&packet)
                .map_err(|e| -> DecoderError { format!("send hvcC NALU: {}", e).into() })?;
        }

        self.decoder = Some(decoder);
        self.pending.clear();
        self.color = params.color;
        Ok(())
    }

    fn submit(&mut self, sample: &[u8], pts_us: i64) -> Result<(), DecoderError> {
        let Self { decoder, pending, .. } = self;
        let decoder = decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "submit before configure".into() })?;

        let nalus = hevc_nalu_bodies(sample)
            .map_err(|e| -> DecoderError { format!("sample NALU parse: {}", e).into() })?;

        // The whole access unit as ONE Annex-B packet: FFmpeg's decoders take
        // a packet as a full picture (no parser in front of them), so one
        // packet per NALU split a multi-slice picture across packets. Each
        // NALU body is copied once, straight from the sample into the packet.
        const START_CODE: [u8; 4] = [0, 0, 0, 1];
        let size = nalus.iter().map(|n| START_CODE.len() + n.len()).sum();
        let mut packet = Packet::new(size);
        let data = packet
            .data_mut()
            .ok_or_else(|| -> DecoderError { "packet has no data buffer".into() })?;
        let mut at = 0;
        for nalu in &nalus {
            data[at..at + START_CODE.len()].copy_from_slice(&START_CODE);
            at += START_CODE.len();
            data[at..at + nalu.len()].copy_from_slice(nalu);
            at += nalu.len();
        }
        // Store pts in milliseconds — FFmpeg's time base for this decoder is 1ms.
        packet.set_pts(Some(pts_us / 1000));

        loop {
            match decoder.send_packet(&packet) {
                Ok(()) => return Ok(()),
                // The decoder holds output it wants taken before it accepts
                // more input: take a frame and send again.
                Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                    let mut frame = ffmpeg_next::util::frame::Video::empty();
                    decoder.receive_frame(&mut frame).map_err(|e| -> DecoderError {
                        format!("send_packet EAGAIN, then receive_frame: {}", e).into()
                    })?;
                    pending.push_back(frame);
                }
                Err(e) => {
                    // Include errno + Debug repr so opaque strerror strings like
                    // "Not enough space" (Windows-localised ENOSPC?
                    // AVERROR_BUFFER_TOO_SMALL?) can be cross-referenced against
                    // FFmpeg's error codes during triage.
                    let errno = match &e {
                        ffmpeg_next::Error::Other { errno } => Some(*errno),
                        _ => None,
                    };
                    return Err(format!(
                        "send_packet: {} (errno={:?}, au_len={}, nalus={}, pts_ms={})",
                        e,
                        errno,
                        size,
                        nalus.len(),
                        pts_us / 1000
                    )
                    .into());
                }
            }
        }
    }

    fn try_recv(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| -> DecoderError { "try_recv before configure".into() })?;

        let mut frame = ffmpeg_next::util::frame::Video::empty();
        let received = match self.pending.pop_front() {
            Some(held) => {
                frame = held;
                Ok(())
            }
            None => decoder.receive_frame(&mut frame),
        };
        match received {
            Ok(()) => {
                // pts was stored in milliseconds; convert back to microseconds for the trait.
                let pts_us = frame.pts().unwrap_or(0) * 1000;
                let width = frame.width();
                let height = frame.height();
                if !self.pool_logged {
                    self.pool_logged = true;
                    unsafe {
                        let hwf = (*frame.as_ptr()).hw_frames_ctx;
                        if !hwf.is_null() {
                            let fc = (*hwf).data as *const ffmpeg_sys_next::AVHWFramesContext;
                            log::info!(
                                "[ffmpeg_hw] hw frame pool: {} surfaces ({}x{})",
                                (*fc).initial_pool_size, (*fc).width, (*fc).height
                            );
                        }
                    }
                }

                // macOS: keep the AVFrame as AV_PIX_FMT_VIDEOTOOLBOX — the CVPixelBufferRef
                // lives in frame.data[3] and the video renderer wraps it as two MTLTextures
                // (Y + UV planes) via CVMetalTextureCache. Zero-copy GPU import.

                Ok(Some(DecodedVideoFrame {
                    pts_us,
                    width,
                    height,
                    native: PlatformFrame::FfmpegVideo(Arc::new(frame)),
                    desired_present_ns: 0,
                    color: self.color,
                    // Desktop tonemap uses the wgpu detection passes; SEI
                    // metadata isn't needed there.
                    hdr_meta: None,
                }))
            }
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_sys_next::EAGAIN => {
                Ok(None)
            }
            Err(e) => Err(format!("receive_frame: {}", e).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Platform smoke tests for the FFmpeg HW decoder path.
    //!
    //! These run on Windows + Linux only (mirroring the module's own
    //! cfg gate) and verify the assumptions the rest of the player
    //! makes about the local FFmpeg build:
    //!
    //!   - HEVC + H.264 decoders are linked in (we'd fail later at
    //!     `decoder::find` with a less obvious message).
    //!   - The HW pixel-format constant we look for matches the
    //!     platform's hwaccel framework.
    //!   - The HW device context can be created on this host.
    //!     Skipped (not failed) on CI / headless boxes where no GPU
    //!     is available — distinguishable from a code-level failure
    //!     because the test runner reports "ok" with a log warning.
    //!
    //! Anything past this — `decoder.open(codec)`, `send_packet`,
    //! `receive_frame` — needs real HEVC NALUs (SPS/PPS/VPS plus
    //! sample data) and isn't worth carrying as test fixtures here.

    use super::*;

    #[test]
    fn ffmpeg_finds_hevc_decoder() {
        // This player only configures HEVC and H.264; if HEVC isn't
        // in the local FFmpeg, the rest of the test suite is moot.
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).is_some(),
            "FFmpeg build is missing the HEVC decoder",
        );
    }

    #[test]
    fn ffmpeg_finds_h264_decoder() {
        assert!(
            ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::H264).is_some(),
            "FFmpeg build is missing the H.264 decoder",
        );
    }

    #[test]
    fn wanted_hw_pixel_format_matches_platform() {
        // Catches accidental cfg flips during a refactor — the format
        // we ask the decoder to negotiate must match the device-type
        // we register, otherwise get_format returns AV_PIX_FMT_NONE
        // and decoder open aborts.
        #[cfg(target_os = "windows")]
        assert_eq!(WANTED_HW, AVPixelFormat::AV_PIX_FMT_D3D11);
        #[cfg(target_os = "linux")]
        assert_eq!(WANTED_HW, AVPixelFormat::AV_PIX_FMT_VAAPI);
    }

    #[test]
    fn hw_device_create_against_real_driver() {
        // Try to allocate the real HW device context. On a workstation
        // with a working GPU this passes; on a headless CI runner
        // av_hwdevice_ctx_create returns < 0 (no D3D11 device / no
        // /dev/dri/renderD128). Skip-with-warning rather than fail so
        // we don't break unrelated PR builds.
        let mut decoder = FfmpegHwDecoder::new();
        match decoder.create_hw_device() {
            Ok(()) => {
                assert!(
                    !decoder.hw_device_ctx.is_null(),
                    "create_hw_device returned Ok but left ctx null",
                );
                // Cleanup happens via Drop.
            }
            Err(e) => {
                eprintln!(
                    "hw_device_create: skipping live-driver check — {} \
                     (headless / no GPU is OK here)",
                    e
                );
            }
        }
    }

    #[test]
    fn video_probe_against_real_driver() {
        // Same skip rule as above: no GPU, nothing to ask.
        // Sizes are not asserted: some drivers (Intel UHD on D3D11) accept
        // any size up to the texture limit and fail only when decoding.
        use crate::decoders::support::VideoNeed;
        let Some(probe) = VideoProbe::open() else {
            eprintln!("video_probe: skipping live-driver check (no hw device)");
            return;
        };
        let fhd = VideoNeed::from_codecs("hvc1.1.6.L120.90", 1920, 1080, Some(25.0)).unwrap();
        let high10 = VideoNeed::from_codecs("avc1.6E0028", 1920, 1080, Some(25.0)).unwrap();
        let huge = VideoNeed::from_codecs("hvc1.1.6.L186.90", 16384, 16384, Some(25.0)).unwrap();
        eprintln!(
            "video_probe: HEVC 1080p {:?}, H.264 High 10 {:?}, HEVC 16384x16384 {:?} (drivers differ on sizes)",
            probe.supports(&fhd),
            probe.supports(&high10),
            probe.supports(&huge)
        );
        // Neither DXVA nor VAAPI has an H.264 High 10 decode profile.
        assert_eq!(probe.supports(&high10), Some(false));
    }
}
