//! CUDA backend for the resize pipeline: the same kernels and buffer plan as the Metal
//! backend, compiled at run time with NVRTC and driven through the CUDA driver API
//! (both loaded dynamically, so builds need no CUDA toolkit). Entropy coding stays on
//! the CPU. The kernels are written to give results bit-identical to the CPU path (double
//! precision transfer curve, no fused multiply-add unless the CPU build fuses too).
//!
//! UNTESTED: written without an NVIDIA GPU. Before trusting it, compare against the CPU
//! path, for example by transcoding the same clip with `-b cuda` and `-b cpu` at 6K and
//! checking the outputs decode to near-identical images.

use std::sync::{Arc, Mutex};

use cudarc::driver::{CudaContext, CudaFunction, CudaSlice, CudaStream, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::{compile_ptx_with_opts, CompileOptions};

use crate::frame::{decode_frame, level_widths, Coeffs};
use crate::quality::Geometry;
use crate::resample::Taps;
use crate::resize::{parse_source, plane_taps, Resizer};
use crate::tico::{Layout, TicoHeader};
use crate::{Error, Result};

const SOURCE: &str = include_str!("kernels.cu");

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

fn err(e: impl std::fmt::Debug) -> Error {
    Error(format!("CUDA: {e:?}"))
}

/// True when the NVIDIA driver and NVRTC libraries can be loaded (cudarc panics on first
/// use otherwise).
fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| unsafe { cudarc::driver::sys::is_culib_present() && cudarc::nvrtc::sys::is_culib_present() })
}

pub fn device_name() -> Option<String> {
    if !available() {
        return None;
    }
    std::panic::catch_unwind(|| CudaContext::new(0).ok()?.name().ok()).ok().flatten()
}

struct DevTaps {
    idx: CudaSlice<i32>,
    wt: CudaSlice<f32>,
}

struct Context {
    stream: Arc<CudaStream>,
    a: CudaSlice<i32>,
    b: CudaSlice<i32>,
    host: Vec<i32>,
}

pub struct CudaResizer {
    ctx: Arc<CudaContext>,
    f: std::collections::HashMap<&'static str, CudaFunction>,
    tx: Vec<DevTaps>,
    ty: Vec<DevTaps>,
    src: Geometry,
    dst: Geometry,
    name: String,
    free: Mutex<Vec<Context>>,
}

fn upload(stream: &Arc<CudaStream>, t: &Taps) -> Result<DevTaps> {
    let mut idx = Vec::with_capacity(t.first.len() * 3);
    for i in 0..t.first.len() {
        idx.extend_from_slice(&[t.first[i], t.count[i], t.offset[i]]);
    }
    let mut wt = t.weight.clone();
    if wt.is_empty() {
        wt.push(0.0);
    }
    Ok(DevTaps { idx: stream.clone_htod(&idx).map_err(err)?, wt: stream.clone_htod(&wt).map_err(err)? })
}

fn grid2(w: usize, h: usize) -> LaunchConfig {
    LaunchConfig {
        grid_dim: (w.div_ceil(128).max(1) as u32, h.div_ceil(2).max(1) as u32, 1),
        block_dim: (128, 2, 1),
        shared_mem_bytes: 0,
    }
}

fn grid1(n: usize) -> LaunchConfig {
    LaunchConfig { grid_dim: (n.div_ceil(256).max(1) as u32, 1, 1), block_dim: (256, 1, 1), shared_mem_bytes: 0 }
}

impl CudaResizer {
    pub fn new(src: Geometry, dst: Geometry) -> Option<CudaResizer> {
        if !available() {
            return None;
        }
        std::panic::catch_unwind(|| Self::create(src, dst)).ok().flatten()
    }

    fn compile_kernels() -> std::result::Result<cudarc::nvrtc::Ptx, cudarc::nvrtc::CompileError> {
        // Match the CPU path bit for bit: no contraction of a*b+c unless the CPU build
        // fuses it too (see resample::madd), IEEE division and square root, no flush to zero.
        let fma = cfg!(any(target_arch = "aarch64", target_feature = "fma"));
        let opts = CompileOptions {
            fmad: Some(false),
            ftz: Some(false),
            prec_div: Some(true),
            prec_sqrt: Some(true),
            options: if fma { vec!["-DBC_FMA=1".into()] } else { Vec::new() },
            ..Default::default()
        };
        compile_ptx_with_opts(SOURCE, opts)
    }

    fn create(src: Geometry, dst: Geometry) -> Option<CudaResizer> {
        let ctx = CudaContext::new(0).ok()?;
        let ptx = match Self::compile_kernels() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bluecompress: CUDA kernel compile failed: {e:?}");
                return None;
            }
        };
        let module = ctx.load_module(ptx).ok()?;
        let mut f = std::collections::HashMap::new();
        for &k in KERNELS {
            f.insert(k, module.load_function(k).ok()?);
        }
        let stream = ctx.default_stream();
        let (tx, ty) = plane_taps(&src, &dst);
        let tx = tx.iter().map(|t| upload(&stream, t)).collect::<Result<Vec<_>>>().ok()?;
        let ty = ty.iter().map(|t| upload(&stream, t)).collect::<Result<Vec<_>>>().ok()?;
        stream.synchronize().ok()?;
        let name = format!("CUDA ({})", ctx.name().unwrap_or_else(|_| "GPU".into()));
        Some(CudaResizer { ctx, f, tx, ty, src, dst, name, free: Mutex::new(Vec::new()) })
    }

    fn acquire(&self, ls: &Layout, ld: &Layout) -> Result<Context> {
        if let Some(c) = self.free.lock().unwrap().pop() {
            return Ok(c);
        }
        let mid = 4 * ls.ch * ld.cw;
        let n = Coeffs::len(ls).max(Coeffs::len(ld)).max(mid);
        let stream = self.ctx.new_stream().map_err(err)?;
        let a = stream.alloc_zeros::<i32>(n).map_err(err)?;
        let b = stream.alloc_zeros::<i32>(n).map_err(err)?;
        Ok(Context { stream, a, b, host: vec![0; n] })
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
        let (base, stride, leni, rowsu) = (base as u32, stride as u32, len as i32, rows as u32);
        let steps: [(&str, usize); 2] =
            if inverse { [("ih_even", lows), ("ih_odd", highs)] } else { [("fh_high", highs), ("fh_low", lows)] };
        for (k, n) in steps {
            let mut l = c.stream.launch_builder(&self.f[k]);
            l.arg(&mut c.a).arg(&mut c.b).arg(&base).arg(&stride).arg(&leni).arg(&rowsu);
            unsafe { l.launch(grid2(n, rows)) }.map_err(err)?;
        }
        let mut l = c.stream.launch_builder(&self.f["copy_rows"]);
        l.arg(&mut c.a).arg(&c.b).arg(&base).arg(&stride).arg(&leni).arg(&rowsu);
        unsafe { l.launch(grid2(len, rows)) }.map_err(err)?;
        Ok(())
    }

    fn star(&self, c: &mut Context, l: &Layout, steps: std::ops::RangeInclusive<i32>) -> Result<()> {
        let (w, h) = (l.cw as u32, l.ch as u32);
        for s in steps {
            let mut b = c.stream.launch_builder(&self.f["star"]);
            b.arg(&mut c.b).arg(&w).arg(&h).arg(&s);
            unsafe { b.launch(grid2(l.cw, l.ch)) }.map_err(err)?;
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
        for comp in 0..4 {
            let (lo, hi, im) = ((comp * plane) as u64, ((4 + comp) * plane) as u64, (comp * cw * ch) as u64);
            let (w, chi, copy) = (cw as u32, ch as i32, (l.levels[comp] == 0) as i32);
            let mut b = c.stream.launch_builder(&self.f["iv"]);
            b.arg(&c.a).arg(&mut c.b).arg(&lo).arg(&hi).arg(&im).arg(&w).arg(&chi).arg(&copy);
            unsafe { b.launch(grid2(cw, ch.div_ceil(2))) }.map_err(err)?;
        }
        self.star(c, l, 1..=5)
    }

    fn resample(&self, c: &mut Context, h: &TicoHeader, ls: &Layout, ld: &Layout) -> Result<()> {
        let (sw, sh, dw, dh) = (ls.cw, ls.ch, ld.cw, ld.ch);
        let (black, gain, knee) = (h.black as f64, h.gain as f64, h.knee as f64);
        // B (int code planes) -> A (float raw planes).
        let n = (4 * sw * sh) as u64;
        let mut b = c.stream.launch_builder(&self.f["to_raw"]);
        b.arg(&c.b).arg(&mut c.a).arg(&black).arg(&gain).arg(&knee).arg(&n);
        unsafe { b.launch(grid1(n as usize)) }.map_err(err)?;
        for comp in 0..4 {
            // Horizontal A -> B (sh rows of dw).
            let (dwu, rows, stride) = (dw as u32, sh as u32, sw as u32);
            let (so, dof) = ((comp * sw * sh) as u64, (comp * dw * sh) as u64);
            let t = &self.tx[comp];
            let mut b = c.stream.launch_builder(&self.f["rs_h"]);
            b.arg(&c.a).arg(&mut c.b).arg(&t.idx).arg(&t.wt).arg(&dwu).arg(&rows).arg(&stride).arg(&so).arg(&dof);
            unsafe { b.launch(grid2(dw, sh)) }.map_err(err)?;
        }
        for comp in 0..4 {
            // Vertical B -> A (dh rows of dw).
            let (dwu, rows) = (dw as u32, dh as u32);
            let (so, dof) = ((comp * dw * sh) as u64, (comp * dw * dh) as u64);
            let t = &self.ty[comp];
            let mut b = c.stream.launch_builder(&self.f["rs_v"]);
            b.arg(&c.b).arg(&mut c.a).arg(&t.idx).arg(&t.wt).arg(&dwu).arg(&rows).arg(&so).arg(&dof);
            unsafe { b.launch(grid2(dw, dh)) }.map_err(err)?;
        }
        // A (float) -> B (int code planes).
        let n = (4 * dw * dh) as u64;
        let mut b = c.stream.launch_builder(&self.f["to_code"]);
        b.arg(&c.a).arg(&mut c.b).arg(&black).arg(&gain).arg(&knee).arg(&n);
        unsafe { b.launch(grid1(n as usize)) }.map_err(err)?;
        Ok(())
    }

    fn forward(&self, c: &mut Context, l: &Layout) -> Result<()> {
        let (cw, pn, ch) = (l.cw, l.precincts, l.ch);
        let plane = pn * cw;
        let high_rows = ch / 2;
        self.star(c, l, 6..=10)?;
        // Vertical B planes -> A coefficients: high rows first, then low rows.
        for comp in 0..4 {
            let (lo, hi, im) = ((comp * plane) as u64, ((4 + comp) * plane) as u64, (comp * cw * ch) as u64);
            let (w, chi, copy) = (cw as u32, ch as i32, (l.levels[comp] == 0) as i32);
            if copy == 0 {
                let mut b = c.stream.launch_builder(&self.f["fv_high"]);
                b.arg(&c.b).arg(&mut c.a).arg(&hi).arg(&im).arg(&w).arg(&chi);
                unsafe { b.launch(grid2(cw, high_rows)) }.map_err(err)?;
            }
            let mut b = c.stream.launch_builder(&self.f["fv_low"]);
            b.arg(&c.b).arg(&mut c.a).arg(&lo).arg(&hi).arg(&im).arg(&w).arg(&chi).arg(&copy);
            unsafe { b.launch(grid2(cw, ch.div_ceil(2))) }.map_err(err)?;
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
        // Phantom line of an odd height (see frame::forward_wavelet). Copied through B so
        // the kernel never reads and writes the same buffer through two pointers.
        if high_rows < pn {
            for comp in 0..4 {
                let dst = ((4 + comp) * plane + (pn - 1) * cw) as u64;
                let src =
                    if l.levels[comp] == 0 { comp * plane + (pn - 1) * cw } else { (4 + comp) * plane + (pn - 2) * cw }
                        as u64;
                let (zero, n) = (0u64, cw as u32);
                let mut b = c.stream.launch_builder(&self.f["copy_span"]);
                b.arg(&mut c.b).arg(&c.a).arg(&zero).arg(&src).arg(&n);
                unsafe { b.launch(grid1(cw)) }.map_err(err)?;
                let mut b = c.stream.launch_builder(&self.f["copy_span"]);
                b.arg(&mut c.a).arg(&c.b).arg(&dst).arg(&zero).arg(&n);
                unsafe { b.launch(grid1(cw)) }.map_err(err)?;
            }
        }
        Ok(())
    }
}

impl Resizer for CudaResizer {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn process(&self, cs: &[u8], target: crate::quality::Target, seed: &mut Vec<i32>) -> Result<Vec<u8>> {
        let (h, ls, ld) = parse_source(cs, &self.src, &self.dst)?;
        let mut c = self.acquire(&ls, &ld)?;
        let result = (|| -> Result<Vec<u8>> {
            let ns = Coeffs::len(&ls);
            decode_frame(cs, &h, &ls, &mut c.host[..ns])?;
            c.stream.memcpy_htod(&c.host[..ns], &mut c.a.slice_mut(0..ns)).map_err(err)?;
            self.inverse(&mut c, &ls)?;
            self.resample(&mut c, &h, &ls, &ld)?;
            self.forward(&mut c, &ld)?;
            let nd = Coeffs::len(&ld);
            c.stream.memcpy_dtoh(&c.a.slice(0..nd), &mut c.host[..nd]).map_err(err)?;
            c.stream.synchronize().map_err(err)?;
            crate::resize::encode_target(&h, &ld, &c.host[..nd], target, seed)
        })();
        self.free.lock().unwrap().push(c);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVRTC compiles kernels.cu and every kernel in KERNELS loads. Skipped without a GPU.
    #[test]
    fn kernels_compile_and_load() {
        if !unsafe { cudarc::nvrtc::sys::is_culib_present() } {
            eprintln!("NVRTC not available, skipping");
            return;
        }
        let ptx = CudaResizer::compile_kernels().expect("NVRTC compile");
        if !available() {
            eprintln!("CUDA driver not available, skipping module load");
            return;
        }
        let ctx = CudaContext::new(0).expect("CUDA context");
        let module = ctx.load_module(ptx).expect("load module");
        for &k in KERNELS {
            module.load_function(k).unwrap_or_else(|e| panic!("load {k}: {e:?}"));
        }
    }
}
