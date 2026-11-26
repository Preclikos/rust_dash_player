//! Put one black frame on a video window between two direct codecs.
//!
//! Some OMX vendor pipelines (Realtek DVO) bind the window's layer to the
//! decoder that rendered into it, and that binding outlives the codec: while
//! the layer still shows the last hardware frame, a new decoder on the same
//! window fails to start ("VideoClient: failed to run decoder" -> every
//! dequeueOutputBuffer errors). A plain GL frame replaces that buffer and the
//! binding goes with it.
//!
//! EGL and GLES are dlopen'd: ndk-sys links neither, and a build-time extern
//! broke the 32-bit .so load before (see `set_window_frame_rate`).

use std::ffi::c_void;
use std::sync::OnceLock;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLSurface = *mut c_void;
type EGLint = i32;
type EGLBoolean = u32;

const EGL_NONE: EGLint = 0x3038;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_WINDOW_BIT: EGLint = 0x0004;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

struct Egl {
    get_display: unsafe extern "C" fn(*mut c_void) -> EGLDisplay,
    initialize: unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean,
    choose_config:
        unsafe extern "C" fn(EGLDisplay, *const EGLint, *mut EGLConfig, EGLint, *mut EGLint) -> EGLBoolean,
    create_context: unsafe extern "C" fn(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext,
    create_window_surface:
        unsafe extern "C" fn(EGLDisplay, EGLConfig, *mut c_void, *const EGLint) -> EGLSurface,
    make_current: unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> EGLBoolean,
    swap_buffers: unsafe extern "C" fn(EGLDisplay, EGLSurface) -> EGLBoolean,
    destroy_surface: unsafe extern "C" fn(EGLDisplay, EGLSurface) -> EGLBoolean,
    destroy_context: unsafe extern "C" fn(EGLDisplay, EGLContext) -> EGLBoolean,
    get_error: unsafe extern "C" fn() -> EGLint,
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    clear: unsafe extern "C" fn(u32),
}

unsafe fn sym(lib: &[u8], name: &[u8]) -> *mut c_void {
    let mut s = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const _);
    if s.is_null() {
        let h = libc::dlopen(lib.as_ptr() as *const _, libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if !h.is_null() {
            s = libc::dlsym(h, name.as_ptr() as *const _);
        }
    }
    s
}

fn egl() -> Option<&'static Egl> {
    static EGL: OnceLock<Option<Egl>> = OnceLock::new();
    EGL.get_or_init(|| unsafe {
        macro_rules! f {
            ($lib:expr, $name:expr) => {{
                let p = sym($lib, $name);
                if p.is_null() {
                    log::warn!("[mc-direct] window clear: {} missing", String::from_utf8_lossy($name));
                    return None;
                }
                std::mem::transmute(p)
            }};
        }
        let e = b"libEGL.so\0";
        let g = b"libGLESv2.so\0";
        Some(Egl {
            get_display: f!(e, b"eglGetDisplay\0"),
            initialize: f!(e, b"eglInitialize\0"),
            choose_config: f!(e, b"eglChooseConfig\0"),
            create_context: f!(e, b"eglCreateContext\0"),
            create_window_surface: f!(e, b"eglCreateWindowSurface\0"),
            make_current: f!(e, b"eglMakeCurrent\0"),
            swap_buffers: f!(e, b"eglSwapBuffers\0"),
            destroy_surface: f!(e, b"eglDestroySurface\0"),
            destroy_context: f!(e, b"eglDestroyContext\0"),
            get_error: f!(e, b"eglGetError\0"),
            clear_color: f!(g, b"glClearColor\0"),
            clear: f!(g, b"glClear\0"),
        })
    })
    .as_ref()
}

/// Draw one black frame into `window` and disconnect again, so a codec can
/// connect to it next. Must run while no producer is connected (the previous
/// codec released or moved away). Runs on the calling thread; leaves no EGL
/// context current there.
pub fn clear_window(window: *mut ndk_sys::ANativeWindow) -> Result<(), String> {
    let egl = egl().ok_or("EGL unavailable")?;
    unsafe {
        let dpy = (egl.get_display)(std::ptr::null_mut());
        if dpy.is_null() || (egl.initialize)(dpy, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
            return Err(format!("eglInitialize: 0x{:x}", (egl.get_error)()));
        }
        let attrs = [
            EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
            EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
            EGL_NONE,
        ];
        let mut cfg: EGLConfig = std::ptr::null_mut();
        let mut n: EGLint = 0;
        if (egl.choose_config)(dpy, attrs.as_ptr(), &mut cfg, 1, &mut n) == 0 || n < 1 {
            return Err(format!("eglChooseConfig: 0x{:x}", (egl.get_error)()));
        }
        let ctx_attrs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let ctx = (egl.create_context)(dpy, cfg, std::ptr::null_mut(), ctx_attrs.as_ptr());
        if ctx.is_null() {
            return Err(format!("eglCreateContext: 0x{:x}", (egl.get_error)()));
        }
        let surf = (egl.create_window_surface)(dpy, cfg, window as *mut c_void, [EGL_NONE].as_ptr());
        if surf.is_null() {
            let err = (egl.get_error)();
            (egl.destroy_context)(dpy, ctx);
            return Err(format!("eglCreateWindowSurface: 0x{err:x}"));
        }
        let mut result = Ok(());
        if (egl.make_current)(dpy, surf, surf, ctx) == 0 {
            result = Err(format!("eglMakeCurrent: 0x{:x}", (egl.get_error)()));
        } else {
            (egl.clear_color)(0.0, 0.0, 0.0, 1.0);
            (egl.clear)(GL_COLOR_BUFFER_BIT);
            if (egl.swap_buffers)(dpy, surf) == 0 {
                result = Err(format!("eglSwapBuffers: 0x{:x}", (egl.get_error)()));
            }
            (egl.make_current)(dpy, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        }
        // Destroying the surface disconnects EGL from the window.
        (egl.destroy_surface)(dpy, surf);
        (egl.destroy_context)(dpy, ctx);
        result
    }
}
