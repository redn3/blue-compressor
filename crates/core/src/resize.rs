//! Resolution change: full decode to the Bayer mosaic, area resampling of each CFA plane,
//! and re-encode. Backends: CPU (rayon), Metal (macOS), CUDA and OpenCL
//! (optional features).

use rayon::prelude::*;

use crate::frame::*;
use crate::quality::{encode_rank_for_quality, Geometry, Target, RESIZE_FINEST_RANK};
use crate::resample::{make_taps, resample_plane, Taps};
use crate::tico::{Layout, TicoHeader};
use crate::{Backend, Error, Result};

/// Phase of each CFA plane inside the 2x2 quad (x, y): G1 at (1,0), R (0,0), G2 (0,1), B (1,1).
pub const PHASE_X: [usize; 4] = [1, 0, 0, 1];
pub const PHASE_Y: [usize; 4] = [0, 0, 1, 1];

/// Per-plane taps for a geometry change (index = CFA plane).
pub fn plane_taps(src: &Geometry, dst: &Geometry) -> ([Taps; 4], [Taps; 4]) {
    let (sw, sh, dw, dh) = (src.width as usize, src.height as usize, dst.width as usize, dst.height as usize);
    let sx = src.active_width as f64 / dst.active_width as f64;
    let sy = src.active_height as f64 / dst.active_height as f64;
    (
        std::array::from_fn(|c| make_taps(sw / 2, dw / 2, sw, dw, PHASE_X[c], sx)),
        std::array::from_fn(|c| make_taps(sh / 2, dh / 2, sh, dh, PHASE_Y[c], sy)),
    )
}

pub trait Resizer: Send + Sync {
    fn name(&self) -> String;
    /// Re-encodes one codestream at the target size. Called concurrently from workers.
    fn process(&self, cs: &[u8], target: Target) -> Result<Vec<u8>>;
}

/// The entropy coding stage of every resizer (CPU and GPU): a uniform rank for a quality
/// preset, or rate control to the frame budget for a bpp target. It always runs on the
/// CPU; the GPU backends only do the pixel stages before it.
pub fn encode_target(h: &TicoHeader, ld: &Layout, coeffs: &[i32], target: Target) -> Result<Vec<u8>> {
    match target.budget(ld.width as u32, ld.height as u32) {
        Some(budget) => encode_frame_budget(h, ld, coeffs, RESIZE_FINEST_RANK, budget),
        None => encode_frame(h, ld, coeffs, encode_rank_for_quality(target.preset())),
    }
}

/// Parses a source codestream and checks it matches the clip geometry.
pub fn parse_source(cs: &[u8], src: &Geometry, dst: &Geometry) -> Result<(TicoHeader, Layout, Layout)> {
    let (h, _) = TicoHeader::parse(cs)?;
    if h.width as u32 != src.width || h.height as u32 != src.height {
        return Err(Error("frame size differs from the clip's".into()));
    }
    let ls = Layout::make(&h, src.width as usize, src.height as usize)?;
    let ld = Layout::make(&h, dst.width as usize, dst.height as usize)?;
    Ok((h, ls, ld))
}

pub struct CpuResizer {
    src: Geometry,
    dst: Geometry,
    tx: [Taps; 4],
    ty: [Taps; 4],
}

impl CpuResizer {
    pub fn new(src: Geometry, dst: Geometry) -> CpuResizer {
        let (tx, ty) = plane_taps(&src, &dst);
        CpuResizer { src, dst, tx, ty }
    }
}

impl Resizer for CpuResizer {
    fn name(&self) -> String {
        "CPU".into()
    }

    fn process(&self, cs: &[u8], target: Target) -> Result<Vec<u8>> {
        let (h, ls, ld) = parse_source(cs, &self.src, &self.dst)?;
        let mut coeffs = vec![0i32; Coeffs::len(&ls)];
        decode_frame(cs, &h, &ls, &mut coeffs)?;
        let dc = resize_coeffs(&h, &ls, &ld, coeffs, &self.tx, &self.ty);
        encode_target(&h, &ld, &dc, target)
    }
}

/// The pixel stages of a resize on the CPU: source coefficients (layout `ls`) to
/// destination coefficients (layout `ld`). The GPU backends implement the same function.
pub fn resize_coeffs(
    h: &TicoHeader,
    ls: &Layout,
    ld: &Layout,
    mut coeffs: Vec<i32>,
    tx: &[Taps; 4],
    ty: &[Taps; 4],
) -> Vec<i32> {
    let mut pl = Planes::default();
    inverse_wavelet(ls, &mut coeffs, &mut pl);
    drop(coeffs);
    inverse_star_tetrix(&mut pl);

    let tr = Transfer::new(h);
    let mut dp = Planes::new(ld.cw, ld.ch);
    let (sn, dn) = (pl.w * pl.h, dp.w * dp.h);
    dp.data.par_chunks_mut(dn).enumerate().for_each(|(c, out)| {
        let src: Vec<f32> = pl.data[c * sn..(c + 1) * sn].iter().map(|&v| tr.to_raw_f(v as f32)).collect();
        let mut dst = vec![0f32; dn];
        let mut scratch = Vec::new();
        resample_plane(&src, pl.w, pl.h, &mut dst, ld.cw, ld.ch, &tx[c], &ty[c], &mut scratch);
        for (o, &v) in out.iter_mut().zip(&dst) {
            *o = tr.to_code(v);
        }
    });
    drop(pl);
    forward_star_tetrix(&mut dp);
    let mut dc = vec![0i32; Coeffs::len(ld)];
    forward_wavelet(ld, &dp, &mut dc);
    dc
}

/// Picks a resizer for the backend: GPU when requested or available, else CPU.
pub fn create(backend: Backend, src: Geometry, dst: Geometry) -> Result<Box<dyn Resizer>> {
    if backend != Backend::Cpu {
        if let Some(gpu) = crate::gpu::create(backend, src, dst) {
            return Ok(gpu);
        }
        if matches!(backend, Backend::Metal | Backend::Cuda | Backend::OpenCl) {
            return Err(Error(format!("{backend:?} backend is not available on this machine")));
        }
    }
    Ok(Box::new(CpuResizer::new(src, dst)))
}
