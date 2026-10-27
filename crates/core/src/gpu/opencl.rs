//! OpenCL backend for the resize pipeline, aimed at AMD GPUs on Windows and Linux (it also
//! runs on other OpenCL GPUs). The same kernels and buffer plan as the CUDA backend
//! (kernels.cl is a port of kernels.cu), compiled at run time by the driver. The OpenCL
//! loader (OpenCL.dll / libOpenCL.so) is opened at run time, so builds need no SDK and
//! machines without a driver fall back to the CPU. Entropy coding stays on the CPU.
//!
//! Unlike the CUDA and Metal backends, the kernels are written to match the CPU path bit
//! for bit (see the tests below, which run on any OpenCL device, e.g. PoCL on a CPU). That
//! needs double precision (cl_khr_fp64) for the transfer curve; devices without it are
//! skipped.
//!
//! UNTESTED ON AMD HARDWARE: verified only against PoCL (OpenCL on the CPU).

use std::collections::HashMap;
use std::ptr;
use std::sync::Mutex;

use opencl3::command_queue::CommandQueue;
use opencl3::context::Context as ClContext;
use opencl3::device::{Device, CL_DEVICE_TYPE_ALL, CL_DEVICE_TYPE_CPU, CL_DEVICE_TYPE_GPU};
use opencl3::kernel::Kernel;
use opencl3::memory::{Buffer, ClMem, CL_MEM_READ_ONLY, CL_MEM_READ_WRITE};
use opencl3::program::Program;
use opencl3::types::{cl_device_id, CL_BLOCKING};

use crate::frame::{decode_frame, level_widths, Coeffs};
use crate::quality::Geometry;
use crate::resample::Taps;
use crate::resize::{parse_source, plane_taps, Resizer};
use crate::tico::{Layout, TicoHeader};
use crate::{Error, Result};

const SOURCE: &str = include_str!("kernels.cl");

const KERNELS: &[&str] = &[
    "ih_even",
    "ih_odd",
    "fh_high",
    "fh_low",
    "copy_rows",
    "copy_span",
    "iv",
    "fv_high",
    "fv_low",
    "star",
    "to_raw",
    "to_code",
    "rs_h",
    "rs_v",
];

const VENDOR_AMD: u32 = 0x1002;
const VENDOR_NVIDIA: u32 = 0x10de;

fn err(e: impl std::fmt::Debug) -> Error {
    Error(format!("OpenCL: {e:?}"))
}

/// Which devices may be picked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// Automatic GPU choice: AMD first, then other GPUs, but never NVIDIA (those use the
    /// CUDA backend, or the CPU, as before).
    Auto,
    /// Any GPU, NVIDIA included (`-b opencl`).
    AnyGpu,
    /// Any device, CPU OpenCL implementations included (tests).
    AnyDevice,
}

struct Chosen {
    id: cl_device_id,
    name: String,
}

// cl_device_id is a plain handle that OpenCL allows to be used from any thread.
unsafe impl Send for Chosen {}

fn choose(pick: Pick) -> Option<Chosen> {
    let platforms = opencl3::platform::get_platforms().ok()?;
    let mut best: Option<(u32, Chosen)> = None;
    for p in platforms {
        let ty = if pick == Pick::AnyDevice { CL_DEVICE_TYPE_ALL } else { CL_DEVICE_TYPE_GPU };
        for id in p.get_devices(ty).unwrap_or_default() {
            let d = Device::new(id);
            if !d.available().unwrap_or(false) {
                continue;
            }
            // Double precision is required for results identical to the CPU path.
            if d.double_fp_config().unwrap_or(0) == 0 && !d.extensions().unwrap_or_default().contains("cl_khr_fp64") {
                continue;
            }
            let vendor = d.vendor_id().unwrap_or(0);
            let is_cpu = d.dev_type().unwrap_or(0) & CL_DEVICE_TYPE_CPU != 0;
            if pick == Pick::Auto && vendor == VENDOR_NVIDIA {
                continue;
            }
            let rank = match (vendor, is_cpu) {
                (_, true) => 3,
                (VENDOR_AMD, _) => 0,
                (VENDOR_NVIDIA, _) => 2,
                _ => 1,
            };
            let name = d.board_name_amd().ok().filter(|n| !n.trim().is_empty()).or_else(|| d.name().ok());
            let name = name.unwrap_or_else(|| "GPU".into()).trim().to_string();
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, Chosen { id, name }));
            }
        }
    }
    best.map(|(_, c)| c)
}

/// True when an OpenCL loader can be opened at all.
fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| opencl3::platform::get_platforms().is_ok_and(|p| !p.is_empty()))
}

pub fn device_name(pick: Pick) -> Option<String> {
    if !available() {
        return None;
    }
    std::panic::catch_unwind(|| choose(pick).map(|c| c.name)).ok().flatten()
}

struct DevTaps {
    idx: Buffer<i32>,
    wt: Buffer<f32>,
}

/// Per-worker state: OpenCL kernels hold their arguments, so each worker gets its own.
struct Context {
    queue: CommandQueue,
    k: HashMap<&'static str, Kernel>,
    a: Buffer<i32>,
    b: Buffer<i32>,
    host: Vec<i32>,
}

// OpenCL objects are thread-safe handles except kernels while their arguments are being
// set; each Context (and its kernels) is used by one thread at a time.
unsafe impl Send for Context {}

pub struct OpenClResizer {
    ctx: ClContext,
    program: Program,
    tx: Vec<DevTaps>,
    ty: Vec<DevTaps>,
    src: Geometry,
    dst: Geometry,
    name: String,
    free: Mutex<Vec<Context>>,
}

unsafe impl Send for OpenClResizer {}
unsafe impl Sync for OpenClResizer {}

fn buffer<T: Copy>(ctx: &ClContext, q: &CommandQueue, data: &[T]) -> Result<Buffer<T>> {
    let mut b = unsafe { Buffer::<T>::create(ctx, CL_MEM_READ_ONLY, data.len(), ptr::null_mut()) }.map_err(err)?;
    unsafe { q.enqueue_write_buffer(&mut b, CL_BLOCKING, 0, data, &[]) }.map_err(err)?;
    Ok(b)
}

fn upload(ctx: &ClContext, q: &CommandQueue, t: &Taps) -> Result<DevTaps> {
    let mut idx = Vec::with_capacity(t.first.len() * 3);
    for i in 0..t.first.len() {
        idx.extend_from_slice(&[t.first[i], t.count[i], t.offset[i]]);
    }
    let mut wt = t.weight.clone();
    if wt.is_empty() {
        wt.push(0.0);
    }
    Ok(DevTaps { idx: buffer(ctx, q, &idx)?, wt: buffer(ctx, q, &wt)? })
}

fn new_queue(ctx: &ClContext) -> Result<CommandQueue> {
    #[allow(deprecated)]
    CommandQueue::create_default(ctx, 0).map_err(err)
}

/// Sets the kernel's arguments in order and enqueues it over a 1D or 2D range.
macro_rules! launch {
    ($c:expr, $name:expr, [$($g:expr),+], $($arg:expr),* $(,)?) => {{
        let k = &$c.k[$name];
        let mut _i = 0u32;
        $(
            unsafe { k.set_arg(_i, &$arg) }.map_err(err)?;
            _i += 1;
        )*
        let g: Vec<usize> = vec![$(($g).max(1)),+];
        unsafe {
            $c.queue.enqueue_nd_range_kernel(k.get(), g.len() as u32, ptr::null(), g.as_ptr(), ptr::null(), &[])
        }
        .map_err(err)?;
    }};
}

impl OpenClResizer {
    pub fn with_devices(src: Geometry, dst: Geometry, pick: Pick) -> Option<OpenClResizer> {
        if !available() {
            return None;
        }
        std::panic::catch_unwind(|| Self::create(src, dst, pick)).ok().flatten()
    }

    fn create(src: Geometry, dst: Geometry, pick: Pick) -> Option<OpenClResizer> {
        let chosen = choose(pick)?;
        let device = Device::new(chosen.id);
        let ctx = ClContext::from_device(&device).ok()?;
        // Fuse a*b+c only where the CPU path does (see resample::madd).
        let fma = cfg!(any(target_arch = "aarch64", target_feature = "fma"));
        let options = if fma { "-DBC_FMA=1" } else { "" };
        let program = match Program::create_and_build_from_source(&ctx, SOURCE, options) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bluecompress: OpenCL kernel compile failed: {e}");
                return None;
            }
        };
        let q = new_queue(&ctx).ok()?;
        let (tx, ty) = plane_taps(&src, &dst);
        let tx = tx.iter().map(|t| upload(&ctx, &q, t)).collect::<Result<Vec<_>>>().ok()?;
        let ty = ty.iter().map(|t| upload(&ctx, &q, t)).collect::<Result<Vec<_>>>().ok()?;
        q.finish().ok()?;
        let name = format!("OpenCL ({})", chosen.name);
        Some(OpenClResizer { ctx, program, tx, ty, src, dst, name, free: Mutex::new(Vec::new()) })
    }

    fn acquire(&self, ls: &Layout, ld: &Layout) -> Result<Context> {
        if let Some(c) = self.free.lock().unwrap().pop() {
            return Ok(c);
        }
        let mid = 4 * ls.ch * ld.cw;
        let n = Coeffs::len(ls).max(Coeffs::len(ld)).max(mid);
        let queue = new_queue(&self.ctx)?;
        let mut k = HashMap::new();
        for &name in KERNELS {
            k.insert(name, Kernel::create(&self.program, name).map_err(err)?);
        }
        let mk = || unsafe { Buffer::<i32>::create(&self.ctx, CL_MEM_READ_WRITE, n, ptr::null_mut()) }.map_err(err);
        Ok(Context { queue, k, a: mk()?, b: mk()?, host: vec![0; n] })
    }

    /// Horizontal 5/3 over `rows` rows starting at element `base` of A (tmp: B).
    fn horizontal(
        &self,
        c: &mut Context,
        base: usize,
        stride: usize,
        rows: usize,
        len: usize,
        inverse: bool,
    ) -> Result<()> {
        let (lows, highs) = (len - len / 2, len / 2);
        let (a, b) = (c.a.get(), c.b.get());
        let (base_u, stride_u, leni, rowsu) = (base as u32, stride as u32, len as i32, rows as u32);
        let steps: [(&str, usize); 2] =
            if inverse { [("ih_even", lows), ("ih_odd", highs)] } else { [("fh_high", highs), ("fh_low", lows)] };
        for (k, n) in steps {
            launch!(c, k, [n, rows], a, b, base_u, stride_u, leni, rowsu);
        }
        launch!(c, "copy_rows", [len, rows], a, b, base_u, stride_u, leni, rowsu);
        Ok(())
    }

    fn star(&self, c: &mut Context, l: &Layout, steps: std::ops::RangeInclusive<i32>) -> Result<()> {
        let (w, h, b) = (l.cw as u32, l.ch as u32, c.b.get());
        for s in steps {
            launch!(c, "star", [l.cw, l.ch], b, w, h, s);
        }
        Ok(())
    }

    fn inverse(&self, c: &mut Context, l: &Layout) -> Result<()> {
        let (cw, pn, ch) = (l.cw, l.precincts, l.ch);
        let plane = pn * cw;
        for comp in 0..4 {
            if l.levels[comp] == 0 {
                continue;
            }
            for &w in level_widths(cw, l.levels[comp]).iter().rev() {
                self.horizontal(c, comp * plane, cw, pn, w, true)?;
            }
            self.horizontal(c, (4 + comp) * plane, cw, ch / 2, cw, true)?;
        }
        let (a, b) = (c.a.get(), c.b.get());
        for comp in 0..4 {
            let (lo, hi, im) = ((comp * plane) as u64, ((4 + comp) * plane) as u64, (comp * cw * ch) as u64);
            let (w, chi, copy) = (cw as u32, ch as i32, (l.levels[comp] == 0) as i32);
            launch!(c, "iv", [cw, ch.div_ceil(2)], a, b, lo, hi, im, w, chi, copy);
        }
        self.star(c, l, 1..=5)
    }

    fn resample(&self, c: &mut Context, h: &TicoHeader, ls: &Layout, ld: &Layout) -> Result<()> {
        let (sw, sh, dw, dh) = (ls.cw, ls.ch, ld.cw, ld.ch);
        let (black, gain, knee) = (h.black as f64, h.gain as f64, h.knee as f64);
        let (a, b) = (c.a.get(), c.b.get());
        // B (int code planes) -> A (float raw planes).
        let n = (4 * sw * sh) as u64;
        launch!(c, "to_raw", [n as usize], b, a, black, gain, knee, n);
        for comp in 0..4 {
            // Horizontal A -> B (sh rows of dw).
            let (dwu, rows, stride) = (dw as u32, sh as u32, sw as u32);
            let (so, dof) = ((comp * sw * sh) as u64, (comp * dw * sh) as u64);
            let (ti, tw) = (self.tx[comp].idx.get(), self.tx[comp].wt.get());
            launch!(c, "rs_h", [dw, sh], a, b, ti, tw, dwu, rows, stride, so, dof);
        }
        for comp in 0..4 {
            // Vertical B -> A (dh rows of dw).
            let (dwu, rows) = (dw as u32, dh as u32);
            let (so, dof) = ((comp * dw * sh) as u64, (comp * dw * dh) as u64);
            let (ti, tw) = (self.ty[comp].idx.get(), self.ty[comp].wt.get());
            launch!(c, "rs_v", [dw, dh], b, a, ti, tw, dwu, rows, so, dof);
        }
        // A (float) -> B (int code planes).
        let n = (4 * dw * dh) as u64;
        launch!(c, "to_code", [n as usize], a, b, black, gain, knee, n);
        Ok(())
    }

    fn forward(&self, c: &mut Context, l: &Layout) -> Result<()> {
        let (cw, pn, ch) = (l.cw, l.precincts, l.ch);
        let plane = pn * cw;
        let high_rows = ch / 2;
        self.star(c, l, 6..=10)?;
        let (a, b) = (c.a.get(), c.b.get());
        // Vertical B planes -> A coefficients: high rows first, then low rows.
        for comp in 0..4 {
            let (lo, hi, im) = ((comp * plane) as u64, ((4 + comp) * plane) as u64, (comp * cw * ch) as u64);
            let (w, chi, copy) = (cw as u32, ch as i32, (l.levels[comp] == 0) as i32);
            if copy == 0 {
                launch!(c, "fv_high", [cw, high_rows], b, a, hi, im, w, chi);
            }
            launch!(c, "fv_low", [cw, ch.div_ceil(2)], b, a, lo, hi, im, w, chi, copy);
        }
        // Horizontal (tmp = B, free now).
        for comp in 0..4 {
            if l.levels[comp] == 0 {
                continue;
            }
            for w in level_widths(cw, l.levels[comp]) {
                self.horizontal(c, comp * plane, cw, pn, w, false)?;
            }
            self.horizontal(c, (4 + comp) * plane, cw, high_rows, cw, false)?;
        }
        // Phantom line of an odd height (see frame::forward_wavelet), copied through B.
        if high_rows < pn {
            for comp in 0..4 {
                let dst = ((4 + comp) * plane + (pn - 1) * cw) as u64;
                let src =
                    if l.levels[comp] == 0 { comp * plane + (pn - 1) * cw } else { (4 + comp) * plane + (pn - 2) * cw }
                        as u64;
                let (zero, n) = (0u64, cw as u32);
                launch!(c, "copy_span", [cw], b, a, zero, src, n);
                launch!(c, "copy_span", [cw], a, b, dst, zero, n);
            }
        }
        Ok(())
    }

    /// The pixel stages on the GPU: the OpenCL counterpart of [`crate::resize::resize_coeffs`].
    fn run(&self, c: &mut Context, h: &TicoHeader, ls: &Layout, ld: &Layout) -> Result<Vec<i32>> {
        let ns = Coeffs::len(ls);
        unsafe { c.queue.enqueue_write_buffer(&mut c.a, CL_BLOCKING, 0, &c.host[..ns], &[]) }.map_err(err)?;
        self.inverse(c, ls)?;
        self.resample(c, h, ls, ld)?;
        self.forward(c, ld)?;
        let nd = Coeffs::len(ld);
        let mut out = std::mem::take(&mut c.host);
        unsafe { c.queue.enqueue_read_buffer(&c.a, CL_BLOCKING, 0, &mut out[..nd], &[]) }.map_err(err)?;
        c.queue.finish().map_err(err)?;
        c.host = out;
        Ok(c.host[..nd].to_vec())
    }

    /// Resizes coefficients (layout `ls` to `ld`) without entropy coding, for tests.
    #[cfg(test)]
    pub fn resize_coeffs(&self, h: &TicoHeader, ls: &Layout, ld: &Layout, coeffs: &[i32]) -> Result<Vec<i32>> {
        let mut c = self.acquire(ls, ld)?;
        c.host[..coeffs.len()].copy_from_slice(coeffs);
        let r = self.run(&mut c, h, ls, ld);
        self.free.lock().unwrap().push(c);
        r
    }
}

impl Resizer for OpenClResizer {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn process(&self, cs: &[u8], target: crate::quality::Target, seed: &mut Vec<i32>) -> Result<Vec<u8>> {
        let (h, ls, ld) = parse_source(cs, &self.src, &self.dst)?;
        let mut c = self.acquire(&ls, &ld)?;
        let result = (|| -> Result<Vec<u8>> {
            let ns = Coeffs::len(&ls);
            decode_frame(cs, &h, &ls, &mut c.host[..ns])?;
            let out = self.run(&mut c, &h, &ls, &ld)?;
            crate::resize::encode_target(&h, &ld, &out, target, seed)
        })();
        self.free.lock().unwrap().push(c);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{forward_star_tetrix, forward_wavelet, Planes};
    use crate::resize::resize_coeffs;

    fn rng(seed: &mut u64) -> i32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed % 4096) as i32 - 2048
    }

    /// Plausible coefficients: a smooth gradient plus noise in the code domain, run
    /// through the CPU forward transforms.
    fn coefficients(h: &TicoHeader, l: &Layout, seed: u64) -> Vec<i32> {
        let mut s = seed;
        let mut pl = Planes::new(l.cw, l.ch);
        for c in 0..4 {
            for y in 0..l.ch {
                for x in 0..l.cw {
                    let base = -30000 + (x * 40000 / l.cw + y * 20000 / l.ch) as i32 + c as i32 * 500;
                    pl.data[(c * l.ch + y) * l.cw + x] = (base + rng(&mut s)).clamp(-32768, 32767);
                }
            }
        }
        forward_star_tetrix(&mut pl);
        let mut out = vec![0; Coeffs::len(l)];
        forward_wavelet(l, &pl, &mut out);
        let _ = h;
        out
    }

    fn header(w: i32, hgt: i32, levels: [i32; 4], black: i32, gain: i32, knee: i32) -> TicoHeader {
        TicoHeader {
            width: w,
            height: hgt,
            slice_height: 8,
            components: 4,
            levels,
            black,
            gain,
            knee,
            ..Default::default()
        }
    }

    fn geometry(w: u32, h: u32) -> Geometry {
        Geometry { width: w, height: h, active_width: w - 12, active_height: h - 8 }
    }

    /// The OpenCL pipeline must give exactly the CPU's coefficients. Runs on any OpenCL
    /// device with double precision (PoCL on a CPU in CI); skipped when there is none.
    #[test]
    fn opencl_matches_cpu_bit_for_bit() {
        let cases = [
            // (src w, h) -> (dst w, h), levels, black, gain, knee
            ((1036, 588), (780, 444), [5, 5, 5, 0], 600, 65536, 2048),
            ((1000, 562), (640, 362), [3, 4, 2, 1], 512, 70000, 0),
            ((800, 450), (800, 450), [5, 5, 5, 0], 1000, 50000, 4000),
            ((780, 446), (524, 298), [2, 2, 2, 2], 0, 65536, 30000),
        ];
        let mut ran = 0;
        for (i, ((sw, sh), (dw, dh), levels, black, gain, knee)) in cases.into_iter().enumerate() {
            let (src, dst) = (geometry(sw, sh), geometry(dw, dh));
            let Some(gpu) = OpenClResizer::with_devices(src, dst, Pick::AnyDevice) else {
                eprintln!("no OpenCL device with double precision, skipping");
                return;
            };
            if i == 0 {
                eprintln!("testing on {}", gpu.name());
            }
            let h = header(sw as i32, sh as i32, levels, black, gain, knee);
            // The band count depends on the levels; give the header one weight per band.
            let mut h = h;
            h.weights = (1..400)
                .map(|n| vec![(1u8, 1u8); n])
                .find(|w| {
                    Layout::make(&TicoHeader { weights: w.clone(), ..h.clone() }, sw as usize, sh as usize).is_ok()
                })
                .expect("no band count fits");
            let ls = Layout::make(&h, sw as usize, sh as usize).unwrap();
            let ld = Layout::make(&h, dw as usize, dh as usize).unwrap();
            let input = coefficients(&h, &ls, 0x9e3779b97f4a7c15 ^ i as u64);
            let (tx, ty) = plane_taps(&src, &dst);
            let want = resize_coeffs(&h, &ls, &ld, input.clone(), &tx, &ty);
            let got = gpu.resize_coeffs(&h, &ls, &ld, &input).unwrap();
            let diff = want.iter().zip(&got).filter(|(a, b)| a != b).count();
            assert_eq!(diff, 0, "case {i}: {diff} of {} coefficients differ", want.len());
            // Twice through the same pooled context gives the same again.
            assert_eq!(gpu.resize_coeffs(&h, &ls, &ld, &input).unwrap(), got, "case {i}: second run");
            ran += 1;
        }
        eprintln!("{ran} cases identical");
    }
}
