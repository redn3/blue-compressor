//! C API (see `include/bluecompress.h`). Every function catches panics, so no unwind ever
//! crosses into C.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use crate::{Backend, Codec, Options, Resolution};

#[repr(C)]
pub struct bc_clip_info {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub fps: f64,
    pub file_bytes: u64,
    pub video_bytes: u64,
    pub codec: [c_char; 8],
    pub audio_tracks: c_int,
    /// TICO codestream bytes of the video frames (video_bytes less frame headers).
    pub codestream_bytes: u64,
}

#[repr(C)]
pub struct bc_options {
    pub input: *const c_char,
    pub output: *const c_char,
    pub quality: c_int,
    pub resolution: c_int,
    pub backend: c_int,
    pub threads: c_int,
}

#[repr(C)]
pub struct bc_result {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub frames: u32,
    pub seconds: f64,
    pub fps: f64,
    pub backend: [c_char; 32],
    pub error: [c_char; 512],
}

pub type bc_progress_fn = Option<unsafe extern "C" fn(user: *mut c_void, fraction: f64, fps: f64) -> c_int>;

fn copy_str(dst: &mut [c_char], s: &str) {
    if dst.is_empty() {
        return;
    }
    let mut n = s.len().min(dst.len() - 1);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    for (d, &b) in dst.iter_mut().zip(&s.as_bytes()[..n]) {
        *d = b as c_char;
    }
    dst[n] = 0;
}

unsafe fn path(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(PathBuf::from)
}

fn resolution(v: c_int) -> Resolution {
    match v {
        6 => Resolution::K6,
        4 => Resolution::K4,
        _ => Resolution::Original,
    }
}

fn backend(v: c_int) -> Backend {
    match v {
        1 => Backend::Cpu,
        2 => Backend::Metal,
        3 => Backend::Cuda,
        4 => Backend::OpenCl,
        _ => Backend::Auto,
    }
}

fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "internal error".into())
}

/// # Safety
/// `path` must be a NUL-terminated UTF-8 string; `info` must point to a bc_clip_info;
/// `error` may be null or point to `error_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn bc_probe(
    path_: *const c_char,
    info: *mut bc_clip_info,
    error: *mut c_char,
    error_len: usize,
) -> c_int {
    let set_error = |msg: &str| {
        if !error.is_null() && error_len > 0 {
            copy_str(std::slice::from_raw_parts_mut(error, error_len), msg);
        }
    };
    let Some(p) = path(path_) else {
        set_error("invalid path");
        return 1;
    };
    match std::panic::catch_unwind(|| crate::probe(&p)) {
        Ok(Ok(ci)) => {
            if let Some(out) = info.as_mut() {
                out.width = ci.width;
                out.height = ci.height;
                out.frames = ci.frames;
                out.fps = ci.fps;
                out.file_bytes = ci.file_bytes;
                out.video_bytes = ci.video_bytes;
                out.codec = [0; 8];
                copy_str(&mut out.codec, if ci.codec == Codec::NRaw { "NRAW" } else { "NR3D" });
                out.audio_tracks = ci.audio_tracks as c_int;
                out.codestream_bytes = ci.codestream_bytes;
            }
            0
        }
        Ok(Err(e)) => {
            set_error(&e.0);
            1
        }
        Err(e) => {
            set_error(&panic_message(e));
            1
        }
    }
}

/// # Safety
/// `out_w` and `out_h` may be null or point to writable u32s.
#[no_mangle]
pub unsafe extern "C" fn bc_output_size(w: u32, h: u32, res: c_int, out_w: *mut u32, out_h: *mut u32) {
    let (ow, oh) = crate::output_size(w, h, resolution(res));
    if let Some(p) = out_w.as_mut() {
        *p = ow;
    }
    if let Some(p) = out_h.as_mut() {
        *p = oh;
    }
}

/// Runs synchronously; call from a background thread. Returns 0 on success. `quality` 1
/// is the lossless repack; the legacy steps 2 to 4 are 0.75, 0.55 and 0.35 times each clip's
/// own bits per pixel.
///
/// # Safety
/// `options` must be valid with NUL-terminated UTF-8 paths; `result` may be null.
#[no_mangle]
pub unsafe extern "C" fn bc_transcode(
    options: *const bc_options,
    progress: bc_progress_fn,
    user: *mut c_void,
    result: *mut bc_result,
) -> c_int {
    transcode_with(options, None, progress, user, result)
}

/// [`bc_transcode`] to a target in bits per pixel (0.5 to 8, rounded to 0.1), which takes
/// the place of `options.quality`; 0 or less gives the lossless repack.
///
/// # Safety
/// As [`bc_transcode`].
#[no_mangle]
pub unsafe extern "C" fn bc_transcode_bpp(
    options: *const bc_options,
    bpp: f64,
    progress: bc_progress_fn,
    user: *mut c_void,
    result: *mut bc_result,
) -> c_int {
    transcode_with(options, Some(bpp), progress, user, result)
}

/// Average bits per pixel of a probed clip's video, 8 x codestream bytes / (frames x
/// width x height) (0 when it has no frames). Falls back to `video_bytes` when
/// `codestream_bytes` is 0.
///
/// # Safety
/// `info` may be null or point to a bc_clip_info filled by [`bc_probe`].
#[no_mangle]
pub unsafe extern "C" fn bc_clip_bpp(info: *const bc_clip_info) -> f64 {
    match info.as_ref() {
        Some(i) if i.frames > 0 => {
            let bytes = if i.codestream_bytes > 0 { i.codestream_bytes } else { i.video_bytes };
            crate::bpp_of(bytes as f64 / i.frames as f64, i.width, i.height)
        }
        _ => 0.0,
    }
}

unsafe fn transcode_with(
    options: *const bc_options,
    bpp: Option<f64>,
    progress: bc_progress_fn,
    user: *mut c_void,
    result: *mut bc_result,
) -> c_int {
    let mut local = std::mem::zeroed::<bc_result>();
    let r = result.as_mut().unwrap_or(&mut local);
    *r = std::mem::zeroed();
    let Some(o) = options.as_ref() else {
        copy_str(&mut r.error, "missing options");
        return 1;
    };
    let (Some(input), Some(output)) = (path(o.input), path(o.output)) else {
        copy_str(&mut r.error, "missing or non UTF-8 input or output path");
        return 1;
    };
    let opts = Options {
        input,
        output,
        quality: if bpp.is_some() { 1 } else { o.quality.clamp(1, 4) as u8 },
        bpp: bpp.filter(|&b| b > 0.0),
        resolution: resolution(o.resolution),
        backend: backend(o.backend),
        threads: o.threads.max(0) as usize,
    };
    let cancel = AtomicBool::new(false);
    let user = user as usize;
    let run = std::panic::AssertUnwindSafe(|| {
        crate::transcode(&opts, &cancel, &mut |fraction, fps| {
            if let Some(cb) = progress {
                if cb(user as *mut c_void, fraction, fps) != 0 {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        })
    });
    match std::panic::catch_unwind(run) {
        Ok(Ok(out)) => {
            r.input_bytes = out.input_bytes;
            r.output_bytes = out.output_bytes;
            r.frames = out.frames;
            r.seconds = out.seconds;
            r.fps = out.fps;
            copy_str(&mut r.backend, &out.backend);
            0
        }
        Ok(Err(e)) => {
            copy_str(&mut r.error, &e.0);
            1
        }
        Err(e) => {
            copy_str(&mut r.error, &panic_message(e));
            1
        }
    }
}

/// Returns a static, NUL-terminated description (valid until the next call for the same backend).
#[no_mangle]
pub extern "C" fn bc_backend_description(b: c_int) -> *const c_char {
    static CACHE: Mutex<Vec<(c_int, std::ffi::CString)>> = Mutex::new(Vec::new());
    let mut cache = CACHE.lock().unwrap();
    if let Some((_, s)) = cache.iter().find(|(k, _)| *k == b) {
        return s.as_ptr();
    }
    let s = std::ffi::CString::new(crate::backend_description(backend(b))).unwrap_or_default();
    cache.push((b, s));
    cache.last().unwrap().1.as_ptr()
}

#[no_mangle]
pub extern "C" fn bc_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}
