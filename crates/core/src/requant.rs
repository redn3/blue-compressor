//! Coefficient-domain transcoding of TICO RAW codestreams: entropy decode, optionally
//! requantize every precinct to a coarser truncation, entropy re-encode without the
//! camera's constant-bitrate padding. No wavelet or colour transform is involved, so a
//! lossless repack reproduces the camera's decoded pixels exactly.

use std::sync::Mutex;

use crate::rate::{finest_fitting, max_rank, slice_budgets, Coded, SliceBudget};
use crate::tico::*;
use crate::util::parallel_for;
use crate::Result;

#[derive(Debug, Clone, Copy)]
pub struct RequantParams {
    /// Minimum truncation rank per precinct (see [`rank_of`]); None keeps the camera's.
    pub target_rank: Option<i32>,
    /// Choose the cheapest GCLI coding mode per band instead of the camera's.
    pub optimize_modes: bool,
    /// Drop coded-but-empty bit planes (groups whose magnitudes are all zero).
    pub tidy: bool,
    /// Target codestream size in bytes (bits per sample mode, see [`crate::rate`]). Each
    /// precinct then takes the finest rank that fits its share, never finer than the
    /// camera's, and `target_rank` is ignored. A codestream that already fits is repacked
    /// losslessly.
    pub budget: Option<usize>,
}

impl Default for RequantParams {
    fn default() -> Self {
        RequantParams { target_rank: None, optimize_modes: true, tidy: true, budget: None }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodestreamStats {
    pub precincts: u64,
    pub requantized: u64,
    /// Sum of original ranks (for averaging).
    pub rank_sum: i64,
}

/// Ranks order (Q, R) pairs from finest to coarsest; gtli is monotone in rank.
#[inline]
pub fn rank_of(q: i32, r: i32, bands: i32) -> i32 {
    q * bands + (bands - r.clamp(0, bands))
}

#[inline]
pub fn from_rank(rank: i32, bands: i32) -> (i32, i32) {
    (rank / bands, bands - rank % bands)
}

/// Clears coefficients past the band's end and lowers GCLIs of groups left without data.
fn tidy(band: &Band, bl: &mut BandLine) {
    for j in band.width..bl.mag.len() {
        bl.mag[j] = 0;
        bl.neg[j] = 0;
    }
    for g in 0..band.groups {
        let m = &bl.mag[g * GROUP..g * GROUP + GROUP];
        if m[0] | m[1] | m[2] | m[3] == 0 {
            bl.gcli[g] = bl.gtli as u8;
        }
    }
}

fn requant_line(band: &Band, bl: &mut BandLine, new_gtli: i32) {
    let t0 = bl.gtli;
    let t1 = new_gtli;
    bl.gtli = t1;
    for g in 0..band.groups {
        let g0 = bl.gcli[g] as i32;
        let mut r = [0u32; GROUP];
        let mut mx = 0;
        for j in 0..GROUP {
            if g * GROUP + j < band.width {
                r[j] = dequant_uniform(bl.mag[g * GROUP + j], g0, t0);
            }
            mx = mx.max(r[j]);
        }
        let gnew = bit_length(mx);
        let mut any = 0;
        for j in 0..GROUP {
            let i = g * GROUP + j;
            bl.mag[i] = if gnew > t1 { quant_uniform(r[j], gnew, t1) } else { 0 };
            any |= bl.mag[i];
            if bl.mag[i] == 0 {
                bl.neg[i] = 0;
            }
        }
        bl.gcli[g] = if any != 0 { gnew as u8 } else { t1 as u8 };
    }
}

/// Requantizes a decoded precinct to `target` (no-op when the camera's rank is already at
/// least as coarse) and tidies it.
fn requantize(l: &Layout, pd: &mut PrecinctData, target: i32, rank0: i32, tidy_lines: bool) -> bool {
    let nb = l.bands.len() as i32;
    if target > rank0 {
        let (q, r) = from_rank(target, nb);
        pd.q = q;
        pd.r = r;
        for lines in pd.packets.iter_mut() {
            for bl in lines.iter_mut() {
                let band = &l.bands[bl.band];
                let t1 = compute_gtli(band.gain, band.priority, q, r);
                if t1 != bl.gtli {
                    requant_line(band, bl, t1);
                }
                tidy(band, bl);
            }
        }
        true
    } else {
        if tidy_lines {
            for lines in pd.packets.iter_mut() {
                for bl in lines.iter_mut() {
                    tidy(&l.bands[bl.band], bl);
                }
            }
        }
        false
    }
}

/// Transcodes one codestream, parallel over slices.
pub fn requant_codestream(cs: &[u8], prm: &RequantParams, threads: usize) -> Result<(Vec<u8>, CodestreamStats)> {
    if let Some(budget) = prm.budget {
        // First pass: the lossless repack, which is the output when it already fits.
        let lossless = RequantParams { target_rank: None, budget: None, ..*prm };
        let (out, stats, sizes) = requant_inner(cs, &lossless, threads)?;
        return if out.len() <= budget {
            Ok((out, stats))
        } else {
            requant_to_budget(cs, prm, budget, &sizes, threads)
        };
    }
    let (out, stats, _) = requant_inner(cs, prm, threads)?;
    Ok((out, stats))
}

fn join_slices(header: &TicoHeader, slices: Vec<SliceOut>) -> (Vec<u8>, CodestreamStats, Vec<usize>) {
    let mut total = CodestreamStats::default();
    let mut parts = Vec::with_capacity(slices.len());
    let mut all_sizes = Vec::new();
    for m in slices {
        let (v, st, sizes) = m.into_inner().unwrap();
        total.precincts += st.precincts;
        total.requantized += st.requantized;
        total.rank_sum += st.rank_sum;
        parts.push(v);
        all_sizes.extend(sizes);
    }
    (assemble_codestream(header, header.width, header.height, &parts), total, all_sizes)
}

type SliceOut = Mutex<(Vec<u8>, CodestreamStats, Vec<usize>)>;

/// [`requant_codestream`] without rate control; also returns each precinct's coded size.
fn requant_inner(cs: &[u8], prm: &RequantParams, threads: usize) -> Result<(Vec<u8>, CodestreamStats, Vec<usize>)> {
    let (header, first_slice) = TicoHeader::parse(cs)?;
    let l = Layout::make(&header, header.width as usize, header.height as usize)?;
    let precincts = find_precincts(cs, first_slice, &l)?;
    let nb = l.bands.len();
    let slices: Vec<SliceOut> = (0..l.slices).map(|_| Mutex::default()).collect();
    let lossless = prm.target_rank.is_none();

    parallel_for(l.slices, threads, |s| {
        let mut dec = PredState::default();
        let mut enc = PredState::default();
        dec.reset(nb);
        enc.reset(nb);
        let mut pd = PrecinctData::default();
        let mut scratch = EncodeScratch::default();
        let mut out = Vec::with_capacity(cs.len() / l.slices + 4096);
        out.extend_from_slice(&[0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8]);
        let mut st = CodestreamStats::default();
        let p0 = s * l.slice_height;
        let p1 = l.precincts.min(p0 + l.slice_height);
        let mut sizes = Vec::with_capacity(p1 - p0);
        for p in p0..p1 {
            let before = out.len();
            decode_precinct(cs, precincts[p], &l, p, &mut dec, &mut pd, lossless)?;
            let rank0 = rank_of(pd.q, pd.r, nb as i32);
            st.precincts += 1;
            st.rank_sum += rank0 as i64;
            if let Some(target) = prm.target_rank {
                if requantize(&l, &mut pd, target, rank0, prm.tidy) {
                    st.requantized += 1;
                }
            }
            if prm.optimize_modes {
                choose_modes(&l, &mut pd, &enc);
            }
            encode_precinct(&l, &pd, Some(cs), &mut enc, &mut scratch, &mut out)?;
            sizes.push(out.len() - before);
        }
        *slices[s].lock().unwrap() = (out, st, sizes);
        Ok(())
    })?;
    Ok(join_slices(&header, slices))
}

/// Reusable buffers for coding one precinct candidate in [`requant_to_budget`].
#[derive(Default)]
struct Trial {
    pd: PrecinctData,
}

impl Trial {
    /// Codes `base` (decoded at the camera's `rank0`) at `rank`.
    fn code(
        &mut self,
        l: &Layout,
        base: &PrecinctData,
        rank0: i32,
        rank: i32,
        prm: &RequantParams,
        enc: &PredState,
        scratch: &mut EncodeScratch,
    ) -> Result<Coded> {
        copy_precinct(&mut self.pd, base);
        requantize(l, &mut self.pd, rank, rank0, prm.tidy);
        if prm.optimize_modes {
            choose_modes(l, &mut self.pd, enc);
        }
        let mut state = enc.clone();
        let mut bytes = Vec::with_capacity(base.coded_bytes + 64);
        encode_precinct(l, &self.pd, None, &mut state, scratch, &mut bytes)?;
        Ok(Coded { rank, bytes, state })
    }
}

/// `dst = src.clone()`, reusing `dst`'s allocations.
fn copy_precinct(dst: &mut PrecinctData, src: &PrecinctData) {
    dst.q = src.q;
    dst.r = src.r;
    dst.coded_bytes = src.coded_bytes;
    dst.modes.clone_from(&src.modes);
    dst.spans.clone_from(&src.spans);
    dst.packets.resize_with(src.packets.len(), Vec::new);
    for (d, s) in dst.packets.iter_mut().zip(&src.packets) {
        d.resize_with(s.len(), BandLine::default);
        for (a, b) in d.iter_mut().zip(s) {
            a.band = b.band;
            a.gtli = b.gtli;
            a.gcli.clone_from(&b.gcli);
            a.mag.clone_from(&b.mag);
            a.neg.clone_from(&b.neg);
        }
    }
}

/// [`requant_codestream`] with rate control to `budget` bytes (see [`crate::rate`]).
/// `weights` are the precincts' sizes in the lossless repack.
fn requant_to_budget(
    cs: &[u8],
    prm: &RequantParams,
    budget: usize,
    weights: &[usize],
    threads: usize,
) -> Result<(Vec<u8>, CodestreamStats)> {
    let (header, first_slice) = TicoHeader::parse(cs)?;
    let l = Layout::make(&header, header.width as usize, header.height as usize)?;
    let precincts = find_precincts(cs, first_slice, &l)?;
    let nb = l.bands.len();
    let hi = max_rank(&l);
    let budgets = slice_budgets(&l, header.raw.len(), budget, weights);
    let slices: Vec<SliceOut> = (0..l.slices).map(|_| Mutex::default()).collect();

    // Where each slice starts its search: the rank the first precinct of the middle slice
    // needs for its share. Computed once before the slices run, so it is the same for any
    // thread count, and it saves most of the search at every slice start.
    let start = {
        let s = l.slices / 2;
        let p = s * l.slice_height;
        let mut dec = PredState::default();
        let mut enc = PredState::default();
        dec.reset(nb);
        enc.reset(nb);
        let mut base = PrecinctData::default();
        decode_precinct(cs, precincts[p], &l, p, &mut dec, &mut base, false)?;
        let rank0 = rank_of(base.q, base.r, nb as i32);
        let p1 = l.precincts.min(p + l.slice_height);
        let share = SliceBudget::new(budgets[s], &weights[p..p1]).share(weights[p]);
        let mut trial = Trial::default();
        let mut scratch = EncodeScratch::default();
        finest_fitting(rank0, hi.max(rank0), rank0, share, |rank| {
            trial.code(&l, &base, rank0, rank, prm, &enc, &mut scratch)
        })?
        .rank
    };

    parallel_for(l.slices, threads, |s| {
        let mut dec = PredState::default();
        let mut enc = PredState::default();
        dec.reset(nb);
        enc.reset(nb);
        let mut base = PrecinctData::default();
        let mut scratch = EncodeScratch::default();
        let mut out = Vec::with_capacity(budgets[s] + 4096);
        out.extend_from_slice(&[0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8]);
        let mut st = CodestreamStats::default();
        let p0 = s * l.slice_height;
        let p1 = l.precincts.min(p0 + l.slice_height);
        let mut left = SliceBudget::new(budgets[s], &weights[p0..p1]);
        let mut hint = start;
        let mut trial = Trial::default();
        for p in p0..p1 {
            decode_precinct(cs, precincts[p], &l, p, &mut dec, &mut base, false)?;
            let rank0 = rank_of(base.q, base.r, nb as i32);
            st.precincts += 1;
            st.rank_sum += rank0 as i64;
            let share = left.share(weights[p]);
            let best = finest_fitting(rank0, hi.max(rank0), hint.max(rank0), share, |rank| {
                trial.code(&l, &base, rank0, rank, prm, &enc, &mut scratch)
            })?;
            if best.rank > rank0 {
                st.requantized += 1;
            }
            hint = best.rank;
            left.spend(best.bytes.len(), weights[p]);
            out.extend_from_slice(&best.bytes);
            enc = best.state;
        }
        *slices[s].lock().unwrap() = (out, st, Vec::new());
        Ok(())
    })?;
    let (out, stats, _) = join_slices(&header, slices);
    Ok((out, stats))
}
