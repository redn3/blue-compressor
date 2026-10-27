//! Metal backend for the resize pipeline. Entropy decoding and encoding stay on the CPU
//! (they are bit serial); everything between them runs on the GPU in one command buffer:
//! inverse wavelet, inverse Star-Tetrix, transfer curve, CFA plane resampling, inverse
//! curve, forward Star-Tetrix and forward wavelet. The integer stages match frame.rs bit
//! for bit; the float stages agree within float rounding (about 100 dB raw PSNR).
//!
//! Buffer plan per frame (two shared buffers A and B, reused across frames):
//! A: source coefficients (decoded straight into it) -> float raw planes -> resampled
//! float planes -> destination coefficients (read straight out of it).
//! B: horizontal scratch -> source code planes -> resample scratch -> destination code
//! planes -> horizontal scratch.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLCompileOptions,
    MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary,
    MTLMathMode, MTLResourceOptions, MTLSize,
};

use crate::frame::{decode_frame, level_widths, Coeffs};
use crate::quality::Geometry;
use crate::resample::Taps;
use crate::resize::{parse_source, plane_taps, Resizer};
use crate::tico::{Layout, TicoHeader};
use crate::{Error, Result};

const SHADERS: &str = include_str!("kernels.metal");

type Device = ProtocolObject<dyn MTLDevice>;
type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;
type Encoder = ProtocolObject<dyn MTLComputeCommandEncoder>;

#[repr(C)]
#[derive(Clone, Copy)]
struct RowsP {
    base: u32,
    stride: u32,
    len: u32,
    rows: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct VertP {
    lo: u32,
    hi: u32,
    img: u32,
    w: u32,
    h: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct PlaneP {
    w: u32,
    h: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CurveP {
    black: f32,
    gain: f32,
    knee: f32,
    n: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TapsP {
    dw: u32,
    rows: u32,
    src_stride: u32,
    src_off: u32,
    dst_off: u32,
}

const KERNELS: &[&str] = &[
    "ih_even",
    "ih_odd",
    "fh_high",
    "fh_low",
    "copy_rows",
    "copy_span",
    "iv",
    "iv_copy",
    "fv_high",
    "fv_low",
    "fv_copy",
    "ist1",
    "ist2",
    "ist3",
    "ist4",
    "ist5",
    "fst1",
    "fst2",
    "fst3",
    "fst4",
    "fst5",
    "to_raw",
    "to_code",
    "rs_h",
    "rs_v",
];

struct GpuTaps {
    idx: Buffer,
    wt: Buffer,
}

struct Context {
    a: Buffer,
    b: Buffer,
}

pub struct MetalResizer {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pso: HashMap<&'static str, Pipeline>,
    tx: Vec<GpuTaps>,
    ty: Vec<GpuTaps>,
    src: Geometry,
    dst: Geometry,
    name: String,
    free: Mutex<Vec<Context>>,
}

// Metal objects are thread-safe (MTLDevice, MTLCommandQueue, pipelines and buffers may
// be used from any thread); each frame uses its own command buffer and Context.
unsafe impl Send for MetalResizer {}
unsafe impl Sync for MetalResizer {}

pub fn device_name() -> Option<String> {
    let d = MTLCreateSystemDefaultDevice()?;
    Some(d.name().to_string())
}

fn upload(device: &Device, t: &Taps) -> Option<GpuTaps> {
    let mut idx = Vec::with_capacity(t.first.len() * 3);
    for i in 0..t.first.len() {
        idx.extend_from_slice(&[t.first[i], t.count[i], t.offset[i]]);
    }
    let mut wt = t.weight.clone();
    if wt.is_empty() {
        wt.push(0.0);
    }
    unsafe {
        Some(GpuTaps {
            idx: device.newBufferWithBytes_length_options(
                NonNull::new(idx.as_ptr() as *mut c_void)?,
                idx.len() * 4,
                MTLResourceOptions::StorageModeShared,
            )?,
            wt: device.newBufferWithBytes_length_options(
                NonNull::new(wt.as_ptr() as *mut c_void)?,
                wt.len() * 4,
                MTLResourceOptions::StorageModeShared,
            )?,
        })
    }
}

impl MetalResizer {
    pub fn new(src: Geometry, dst: Geometry) -> Option<MetalResizer> {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        let opts = MTLCompileOptions::new();
        // Keep float results stable: no fast-math reassociation.
        opts.setMathMode(MTLMathMode::Safe);
        let library = match device.newLibraryWithSource_options_error(&NSString::from_str(SHADERS), Some(&opts)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bluecompress: Metal shader compile failed: {}", e.localizedDescription());
                return None;
            }
        };
        let mut pso = HashMap::new();
        for &k in KERNELS {
            let f = library.newFunctionWithName(&NSString::from_str(k))?;
            pso.insert(k, device.newComputePipelineStateWithFunction_error(&f).ok()?);
        }
        let (tx, ty) = plane_taps(&src, &dst);
        let tx = tx.iter().map(|t| upload(&device, t)).collect::<Option<Vec<_>>>()?;
        let ty = ty.iter().map(|t| upload(&device, t)).collect::<Option<Vec<_>>>()?;
        let name = format!("Metal ({})", device.name());
        Some(MetalResizer { device, queue, pso, tx, ty, src, dst, name, free: Mutex::new(Vec::new()) })
    }

    fn acquire(&self, ls: &Layout, ld: &Layout) -> Result<Context> {
        if let Some(c) = self.free.lock().unwrap().pop() {
            return Ok(c);
        }
        let mid = 4 * ls.ch * ld.cw * 4;
        let bytes = (Coeffs::len(ls) * 4).max(Coeffs::len(ld) * 4).max(mid);
        let mk = || self.device.newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared);
        match (mk(), mk()) {
            (Some(a), Some(b)) => Ok(Context { a, b }),
            _ => Err(Error("out of GPU memory".into())),
        }
    }

    fn dispatch2(&self, enc: &Encoder, k: &str, w: usize, h: usize) {
        let ps = &self.pso[k];
        enc.setComputePipelineState(ps);
        let tw = ps.threadExecutionWidth().min(64);
        let th = (ps.maxTotalThreadsPerThreadgroup() / tw).clamp(1, 4);
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize { width: w, height: h, depth: 1 },
            MTLSize { width: tw, height: th, depth: 1 },
        );
    }

    fn dispatch1(&self, enc: &Encoder, k: &str, n: usize) {
        let ps = &self.pso[k];
        enc.setComputePipelineState(ps);
        let tw = ps.maxTotalThreadsPerThreadgroup().min(256);
        enc.dispatchThreads_threadsPerThreadgroup(
            MTLSize { width: n, height: 1, depth: 1 },
            MTLSize { width: tw, height: 1, depth: 1 },
        );
    }
}

unsafe fn set_bytes<T: Copy>(enc: &Encoder, v: &T, index: usize) {
    enc.setBytes_length_atIndex(NonNull::from(v).cast(), std::mem::size_of::<T>(), index);
}

unsafe fn set_buf(enc: &Encoder, b: &Buffer, index: usize) {
    enc.setBuffer_offset_atIndex(Some(b), 0, index);
}

impl MetalResizer {
    /// Horizontal 5/3 over `rows` rows starting at element `base` of A (tmp: B).
    unsafe fn horizontal(
        &self,
        enc: &Encoder,
        ctx: &Context,
        base: usize,
        stride: usize,
        rows: usize,
        len: usize,
        inverse: bool,
    ) {
        let p = RowsP { base: base as u32, stride: stride as u32, len: len as u32, rows: rows as u32 };
        set_buf(enc, &ctx.a, 0);
        set_buf(enc, &ctx.b, 1);
        set_bytes(enc, &p, 2);
        let (lows, highs) = (len - len / 2, len / 2);
        if inverse {
            self.dispatch2(enc, "ih_even", lows, rows);
            self.dispatch2(enc, "ih_odd", highs, rows);
        } else {
            self.dispatch2(enc, "fh_high", highs, rows);
            self.dispatch2(enc, "fh_low", lows, rows);
        }
        self.dispatch2(enc, "copy_rows", len, rows);
    }

    unsafe fn encode_inverse(&self, enc: &Encoder, ctx: &Context, l: &Layout) {
        let (cw, pn, ch) = (l.cw, l.precincts, l.ch);
        let plane = pn * cw;
        for c in 0..4 {
            if l.levels[c] == 0 {
                continue;
            }
            for &w in level_widths(cw, l.levels[c]).iter().rev() {
                self.horizontal(enc, ctx, c * plane, cw, pn, w, true);
            }
            self.horizontal(enc, ctx, (4 + c) * plane, cw, ch / 2, cw, true);
        }
        for c in 0..4 {
            let v = VertP {
                lo: (c * plane) as u32,
                hi: ((4 + c) * plane) as u32,
                img: (c * cw * ch) as u32,
                w: cw as u32,
                h: ch as u32,
            };
            set_buf(enc, &ctx.a, 0);
            set_buf(enc, &ctx.b, 1);
            set_bytes(enc, &v, 2);
            self.dispatch2(enc, if l.levels[c] == 0 { "iv_copy" } else { "iv" }, cw, ch.div_ceil(2));
        }
        let pl = PlaneP { w: cw as u32, h: ch as u32 };
        for k in ["ist1", "ist2", "ist3", "ist4", "ist5"] {
            set_buf(enc, &ctx.b, 0);
            set_bytes(enc, &pl, 1);
            self.dispatch2(enc, k, cw, ch);
        }
    }

    unsafe fn encode_resample(&self, enc: &Encoder, ctx: &Context, h: &TicoHeader, ls: &Layout, ld: &Layout) {
        let (sw, sh, dw, dh) = (ls.cw, ls.ch, ld.cw, ld.ch);
        let mut cv =
            CurveP { black: h.black as f32, gain: h.gain as f32, knee: h.knee as f32, n: (4 * sw * sh) as u32 };
        // B (int code planes) -> A (float raw planes).
        set_buf(enc, &ctx.b, 0);
        set_buf(enc, &ctx.a, 1);
        set_bytes(enc, &cv, 2);
        self.dispatch1(enc, "to_raw", cv.n as usize);
        for c in 0..4 {
            // Horizontal A -> B (sh rows of dw).
            let t = TapsP {
                dw: dw as u32,
                rows: sh as u32,
                src_stride: sw as u32,
                src_off: (c * sw * sh) as u32,
                dst_off: (c * dw * sh) as u32,
            };
            set_buf(enc, &ctx.a, 0);
            set_buf(enc, &ctx.b, 1);
            set_buf(enc, &self.tx[c].idx, 2);
            set_buf(enc, &self.tx[c].wt, 3);
            set_bytes(enc, &t, 4);
            self.dispatch2(enc, "rs_h", dw, sh);
        }
        for c in 0..4 {
            // Vertical B -> A (dh rows of dw).
            let t = TapsP {
                dw: dw as u32,
                rows: dh as u32,
                src_stride: dw as u32,
                src_off: (c * dw * sh) as u32,
                dst_off: (c * dw * dh) as u32,
            };
            set_buf(enc, &ctx.b, 0);
            set_buf(enc, &ctx.a, 1);
            set_buf(enc, &self.ty[c].idx, 2);
            set_buf(enc, &self.ty[c].wt, 3);
            set_bytes(enc, &t, 4);
            self.dispatch2(enc, "rs_v", dw, dh);
        }
        // A (float) -> B (int code planes).
        cv.n = (4 * dw * dh) as u32;
        set_buf(enc, &ctx.a, 0);
        set_buf(enc, &ctx.b, 1);
        set_bytes(enc, &cv, 2);
        self.dispatch1(enc, "to_code", cv.n as usize);
    }

    unsafe fn encode_forward(&self, enc: &Encoder, ctx: &Context, l: &Layout) {
        let (cw, pn, ch) = (l.cw, l.precincts, l.ch);
        let plane = pn * cw;
        let high_rows = ch / 2;
        let pl = PlaneP { w: cw as u32, h: ch as u32 };
        for k in ["fst1", "fst2", "fst3", "fst4", "fst5"] {
            set_buf(enc, &ctx.b, 0);
            set_bytes(enc, &pl, 1);
            self.dispatch2(enc, k, cw, ch);
        }
        // Vertical B planes -> A coefficients: high rows first, then low rows.
        for c in 0..4 {
            let v = VertP {
                lo: (c * plane) as u32,
                hi: ((4 + c) * plane) as u32,
                img: (c * cw * ch) as u32,
                w: cw as u32,
                h: ch as u32,
            };
            set_buf(enc, &ctx.b, 0);
            set_buf(enc, &ctx.a, 1);
            set_bytes(enc, &v, 2);
            if l.levels[c] == 0 {
                self.dispatch2(enc, "fv_copy", cw, ch.div_ceil(2));
            } else {
                self.dispatch2(enc, "fv_high", cw, high_rows);
                self.dispatch2(enc, "fv_low", cw, ch.div_ceil(2));
            }
        }
        // Horizontal (tmp = B, free now).
        for c in 0..4 {
            if l.levels[c] == 0 {
                continue;
            }
            for w in level_widths(cw, l.levels[c]) {
                self.horizontal(enc, ctx, c * plane, cw, pn, w, false);
            }
            self.horizontal(enc, ctx, (4 + c) * plane, cw, high_rows, cw, false);
        }
        // Phantom line of an odd height (see frame::forward_wavelet).
        if high_rows < pn {
            for c in 0..4 {
                let dst = (4 + c) * plane + (pn - 1) * cw;
                let src = if l.levels[c] == 0 { c * plane + (pn - 1) * cw } else { (4 + c) * plane + (pn - 2) * cw };
                let p = RowsP { base: dst as u32, stride: src as u32, len: cw as u32, rows: 1 };
                set_buf(enc, &ctx.a, 0);
                set_buf(enc, &ctx.a, 1);
                set_bytes(enc, &p, 2);
                self.dispatch1(enc, "copy_span", cw);
            }
        }
    }
}

impl Resizer for MetalResizer {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn process(&self, cs: &[u8], target: crate::quality::Target) -> Result<Vec<u8>> {
        let (h, ls, ld) = parse_source(cs, &self.src, &self.dst)?;
        let ctx = self.acquire(&ls, &ld)?;
        let result = (|| -> Result<Vec<u8>> {
            // Decode straight into the shared buffer A.
            let a_ptr = ctx.a.contents().as_ptr() as *mut i32;
            let a_len = ctx.a.length() / 4;
            let a = unsafe { std::slice::from_raw_parts_mut(a_ptr, a_len) };
            decode_frame(cs, &h, &ls, &mut a[..Coeffs::len(&ls)])?;

            let cb = self.queue.commandBuffer().ok_or_else(|| Error("Metal command buffer failed".into()))?;
            let enc = cb.computeCommandEncoder().ok_or_else(|| Error("Metal encoder failed".into()))?;
            unsafe {
                self.encode_inverse(&enc, &ctx, &ls);
                self.encode_resample(&enc, &ctx, &h, &ls, &ld);
                self.encode_forward(&enc, &ctx, &ld);
            }
            enc.endEncoding();
            cb.commit();
            cb.waitUntilCompleted();
            if cb.status() != MTLCommandBufferStatus::Completed {
                return Err(Error("Metal command buffer failed".into()));
            }
            // Destination coefficients are in A, in the same layout as Coeffs.
            let a = unsafe { std::slice::from_raw_parts(a_ptr, a_len) };
            crate::resize::encode_target(&h, &ld, &a[..Coeffs::len(&ld)], target)
        })();
        self.free.lock().unwrap().push(ctx);
        result
    }
}
