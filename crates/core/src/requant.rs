//! Coefficient-domain transcoding of TICO RAW codestreams: entropy decode, optionally
//! requantize every precinct to a coarser truncation, entropy re-encode without the
//! camera's constant-bitrate padding. No wavelet or colour transform is involved, so a
//! lossless repack reproduces the camera's decoded pixels exactly.

use std::sync::Mutex;

use crate::rate::{finest_fitting, max_rank, seed_slack, slice_budgets, Coded, Measured, SliceBudget};
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
    /// Target codestream size in bytes (bits per pixel mode, see [`crate::rate`]). Each
    /// precinct then takes the finest rank that fits its share, never finer than the
    /// camera's, and `target_rank` is ignored. A codestream that already fits is repacked
    /// losslessly.
    pub budget: Option<usize>,
    /// Run the rate controlled pass on the GPU when there is one (see
    /// [`crate::gpu::requant_wgpu`]); the output is the same either way.
    pub gpu: bool,
}

impl Default for RequantParams {
    fn default() -> Self {
        RequantParams { target_rank: None, optimize_modes: true, tidy: true, budget: None, gpu: false }
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
    requant_codestream_seeded(cs, prm, threads, &mut Vec::new())
}

/// [`requant_codestream`] for consecutive frames of a clip: with a bpp budget, `seed` holds
/// the ranks the previous frame chose (empty for the first) and is updated for the next.
pub fn requant_codestream_seeded(
    cs: &[u8],
    prm: &RequantParams,
    threads: usize,
    seed: &mut Vec<i32>,
) -> Result<(Vec<u8>, CodestreamStats)> {
    if let Some(budget) = prm.budget {
        // First pass: the lossless repack, which is the output when it already fits.
        let lossless = RequantParams { target_rank: None, budget: None, ..*prm };
        let (out, stats, sizes) = requant_inner(cs, &lossless, threads)?;
        return if out.len() <= budget {
            seed.clear();
            Ok((out, stats))
        } else {
            requant_to_budget(cs, prm, budget, &sizes, threads, seed)
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
    /// Codes `base` (decoded at the camera's `rank0`) at `rank` the direct way: copy,
    /// requantize, choose modes, encode. [`Fast`] gives the same bytes; this stays as the
    /// reference it is tested against and for jobs without tidying.
    #[allow(clippy::too_many_arguments)]
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

/// The rate search without coding every candidate. Everything a candidate rank needs from
/// the camera's precinct is worked out once: the dequantized magnitudes and the new GCLI
/// of every code group (both independent of the rank), and the tidied GCLIs for lines a
/// rank leaves alone. A candidate is then measured exactly (GCLIs, coding modes and the
/// size of every sub-packet) without writing magnitudes, signs or bits, and only the rank
/// the search settles on is coded. The search sees the same sizes as coding each
/// candidate, so it makes the same choices and the output is byte for byte the same.
#[derive(Default)]
pub(crate) struct Fast {
    /// First code group of each band line (lines in packet order), and one past the last.
    first: Vec<usize>,
    /// Per code group: dequantized magnitudes, their bit length, and the tidied GCLI.
    deq: Vec<[u32; GROUP]>,
    gnew: Vec<u8>,
    keep: Vec<u8>,
    /// Per band line: nonzero magnitudes (sign bits) when the line keeps its truncation.
    keep_nz: Vec<u32>,
    /// Per band line of the candidate: nonzero magnitudes.
    nz: Vec<u32>,
    /// Per band: the band line of the candidate that predicts the next one of the band.
    last: Vec<Option<(usize, usize)>>,
    pd: PrecinctData,
}

impl Fast {
    /// Prepares for candidates of `base`, a precinct decoded with magnitudes.
    pub(crate) fn prepare(&mut self, l: &Layout, base: &PrecinctData) {
        self.first.clear();
        self.deq.clear();
        self.gnew.clear();
        self.keep.clear();
        self.keep_nz.clear();
        for bl in base.packets.iter().flatten() {
            let band = &l.bands[bl.band];
            self.first.push(self.deq.len());
            let t0 = bl.gtli;
            let mut nz = 0;
            for g in 0..band.groups {
                let g0 = bl.gcli[g] as i32;
                let mut r = [0u32; GROUP];
                let mut mx = 0;
                let mut any = 0;
                for j in 0..GROUP {
                    if g * GROUP + j < band.width {
                        let m = bl.mag[g * GROUP + j];
                        r[j] = dequant_uniform(m, g0, t0);
                        any |= m;
                        nz += (m != 0) as u32;
                    }
                    mx = mx.max(r[j]);
                }
                self.deq.push(r);
                self.gnew.push(bit_length(mx) as u8);
                self.keep.push(if any == 0 { t0 as u8 } else { g0 as u8 });
            }
            self.keep_nz.push(nz);
        }
        self.first.push(self.deq.len());
    }

    /// Fills `self.pd` with `base` at `rank` (as [`requantize`] with tidying would), with
    /// magnitudes and signs only when `mags`.
    fn build(&mut self, l: &Layout, base: &PrecinctData, rank0: i32, rank: i32, mags: bool) {
        let nb = l.bands.len() as i32;
        let (q, r) = if rank > rank0 { from_rank(rank, nb) } else { (base.q, base.r) };
        let pd = &mut self.pd;
        pd.q = q;
        pd.r = r;
        pd.coded_bytes = base.coded_bytes;
        pd.modes.clone_from(&base.modes);
        pd.spans.clear();
        pd.packets.resize_with(base.packets.len(), Vec::new);
        self.nz.clear();
        let mut line = 0;
        for (dst, src) in pd.packets.iter_mut().zip(&base.packets) {
            dst.resize_with(src.len(), BandLine::default);
            for (d, b) in dst.iter_mut().zip(src) {
                let band = &l.bands[b.band];
                let t0 = b.gtli;
                let t1 = if rank > rank0 { compute_gtli(band.gain, band.priority, q, r) } else { t0 };
                let g0 = self.first[line];
                d.band = b.band;
                d.gtli = t1;
                d.gcli.clear();
                if mags {
                    d.mag.clear();
                    d.mag.resize(band.groups * GROUP, 0);
                    d.neg.clear();
                    d.neg.resize(band.groups * GROUP, 0);
                }
                if t1 == t0 {
                    d.gcli.extend_from_slice(&self.keep[g0..g0 + band.groups]);
                    self.nz.push(self.keep_nz[line]);
                    if mags {
                        let w = band.width.min(b.mag.len());
                        d.mag[..w].copy_from_slice(&b.mag[..w]);
                        d.neg[..w].copy_from_slice(&b.neg[..w]);
                    }
                } else {
                    let mut nz = 0;
                    for g in 0..band.groups {
                        let gnew = self.gnew[g0 + g] as i32;
                        let mut any = 0;
                        if gnew > t1 {
                            let rr = &self.deq[g0 + g];
                            for j in 0..GROUP {
                                let m = quant_uniform(rr[j], gnew, t1);
                                any |= m;
                                nz += (m != 0) as u32;
                                if mags && m != 0 {
                                    d.mag[g * GROUP + j] = m;
                                    d.neg[g * GROUP + j] = b.neg[g * GROUP + j];
                                }
                            }
                        }
                        d.gcli.push(if any != 0 { gnew as u8 } else { t1 as u8 });
                    }
                    self.nz.push(nz);
                }
                line += 1;
            }
        }
    }

    /// Exact coded size of `self.pd` after [`choose_modes`], the way [`encode_precinct`]
    /// would write it after `enc`.
    fn size(&mut self, l: &Layout, enc: &PredState) -> usize {
        let pd = &self.pd;
        self.last.clear();
        self.last.resize(l.bands.len(), None);
        let mut total = l.precinct_header_bytes();
        let mut line = 0;
        for (k, lines) in pd.packets.iter().enumerate() {
            let (mut sig, mut gcli, mut data, mut sign) = (0usize, 0usize, 0usize, 0usize);
            for bl in lines {
                let b = bl.band;
                let band = &l.bands[b];
                let t = bl.gtli;
                let mode = pd.modes[b];
                let top: Option<(&[u8], i32)> = match self.last[b] {
                    Some((pk, pi)) => Some((&pd.packets[pk][pi].gcli[..], pd.packets[pk][pi].gtli)),
                    None if enc.valid[b] => Some((&enc.gcli[b][..], enc.gtli[b])),
                    None => None,
                };
                let vertical = if mode & 1 != 0 { top } else { None };
                let floor = vertical.map_or(t, |(_, tg)| tg.max(t));
                if mode & 2 != 0 {
                    sig += band.sig_groups;
                }
                for sg in 0..band.sig_groups {
                    let g0 = sg * SIG_GROUP;
                    let g1 = band.groups.min(g0 + SIG_GROUP);
                    let mut bits = 0usize;
                    let mut all_zero = true;
                    for g in g0..g1 {
                        let x = match vertical {
                            Some((tp, _)) => {
                                let room = ((tp[g] as i32).max(floor) - t).max(0);
                                let delta = bl.gcli[g] as i32 - (tp[g] as i32).max(floor);
                                if delta > room {
                                    (delta + room) as u32
                                } else if delta >= 0 {
                                    (2 * delta) as u32
                                } else {
                                    (-2 * delta - 1) as u32
                                }
                            }
                            None => (bl.gcli[g] as i32 - t) as u32,
                        };
                        bits += x as usize + 1;
                        all_zero &= x == 0;
                    }
                    if !(mode & 2 != 0 && all_zero) {
                        gcli += bits;
                    }
                }
                for g in 0..band.groups {
                    let planes = bl.gcli[g] as i32 - t;
                    if planes > 0 {
                        data += planes as usize * 4;
                    }
                }
                sign += self.nz[line] as usize;
                line += 1;
            }
            total += 7 + sig.div_ceil(8) + gcli.div_ceil(8) + data.div_ceil(8) + sign.div_ceil(8);
            for (i, bl) in lines.iter().enumerate() {
                self.last[bl.band] = Some((k, i));
            }
        }
        total
    }

    /// Exact size of `base` coded at `rank` after `enc`.
    pub(crate) fn measure(
        &mut self,
        l: &Layout,
        base: &PrecinctData,
        rank0: i32,
        rank: i32,
        optimize_modes: bool,
        enc: &PredState,
    ) -> Measured {
        self.build(l, base, rank0, rank, false);
        if optimize_modes {
            choose_modes(l, &mut self.pd, enc);
        }
        Measured { rank, size: self.size(l, enc) }
    }

    /// `base` coded at `rank` after `enc`: the same bytes as [`Trial::code`].
    pub(crate) fn code(
        &mut self,
        l: &Layout,
        base: &PrecinctData,
        rank0: i32,
        rank: i32,
        optimize_modes: bool,
        enc: &PredState,
        scratch: &mut EncodeScratch,
    ) -> Result<Coded> {
        self.build(l, base, rank0, rank, true);
        if optimize_modes {
            choose_modes(l, &mut self.pd, enc);
        }
        let mut state = enc.clone();
        let mut bytes = Vec::with_capacity(base.coded_bytes + 64);
        encode_precinct(l, &self.pd, None, &mut state, scratch, &mut bytes)?;
        Ok(Coded { rank, bytes, state })
    }
}

/// The finest rank in [`rank0`, `hi`] whose coding fits `share` (see
/// [`finest_fitting`]), coded. Measures candidates with [`Fast`] unless `prm` turns
/// tidying off, then codes each one.
#[allow(clippy::too_many_arguments)]
fn fit_precinct(
    l: &Layout,
    base: &PrecinctData,
    rank0: i32,
    hi: i32,
    guess: i32,
    share: usize,
    slack: usize,
    prm: &RequantParams,
    enc: &PredState,
    fast: &mut Fast,
    trial: &mut Trial,
    scratch: &mut EncodeScratch,
) -> Result<Coded> {
    if !prm.tidy {
        return finest_fitting(rank0, hi.max(rank0), guess.max(rank0), share, slack, |rank| {
            trial.code(l, base, rank0, rank, prm, enc, scratch)
        });
    }
    fast.prepare(l, base);
    let best = finest_fitting(rank0, hi.max(rank0), guess.max(rank0), share, slack, |rank| {
        Ok(fast.measure(l, base, rank0, rank, prm.optimize_modes, enc))
    })?;
    let coded = fast.code(l, base, rank0, best.rank, prm.optimize_modes, enc, scratch)?;
    debug_assert_eq!(coded.bytes.len(), best.size, "measured size differs from coded size");
    Ok(coded)
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
    seed: &mut Vec<i32>,
) -> Result<(Vec<u8>, CodestreamStats)> {
    let (header, first_slice) = TicoHeader::parse(cs)?;
    let l = Layout::make(&header, header.width as usize, header.height as usize)?;
    let precincts = find_precincts(cs, first_slice, &l)?;
    let nb = l.bands.len();
    let hi = max_rank(&l);
    let budgets = slice_budgets(&l, header.raw.len(), budget, weights);
    let slices: Vec<SliceOut> = (0..l.slices).map(|_| Mutex::default()).collect();

    let prev = std::mem::take(seed);
    let seeded = prev.len() == l.precincts;
    // Without a previous frame, where each slice starts its search: the rank the first
    // precinct of the middle slice needs for its share. Computed once before the slices
    // run, so it is the same for any thread count, and it saves most of the search at
    // every slice start.
    let start = if seeded {
        0
    } else {
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
        let mut fast = Fast::default();
        let mut scratch = EncodeScratch::default();
        fit_precinct(&l, &base, rank0, hi, rank0, share, 0, prm, &enc, &mut fast, &mut trial, &mut scratch)?.rank
    };

    #[cfg(feature = "wgpu")]
    let mut on_gpu = None;
    #[cfg(feature = "wgpu")]
    if prm.gpu && prm.tidy && prm.optimize_modes && std::env::var_os("BC_FORCE_MODE").is_none() {
        let job = crate::gpu::requant_wgpu::Job {
            cs,
            layout: &l,
            precincts: &precincts,
            weights,
            budgets: &budgets,
            seed: seeded.then_some(&prev[..]),
            start,
            hi,
        };
        if let Some(c) = crate::gpu::requant_wgpu::run(&job) {
            let stats =
                CodestreamStats { precincts: l.precincts as u64, requantized: c.requantized, rank_sum: c.rank_sum };
            let out = assemble_codestream(&header, header.width, header.height, &c.slices);
            if !crate::gpu::requant_wgpu::verify() {
                *seed = c.ranks;
                return Ok((out, stats));
            }
            on_gpu = Some((out, c.ranks));
        }
    }

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
        let mut fast = Fast::default();
        let mut ranks = Vec::with_capacity(p1 - p0);
        for p in p0..p1 {
            decode_precinct(cs, precincts[p], &l, p, &mut dec, &mut base, false)?;
            let rank0 = rank_of(base.q, base.r, nb as i32);
            st.precincts += 1;
            st.rank_sum += rank0 as i64;
            let share = left.share(weights[p]);
            let (guess, slack) = if seeded { (prev[p], seed_slack(share)) } else { (hint, 0) };
            let best = fit_precinct(
                &l,
                &base,
                rank0,
                hi,
                guess,
                share,
                slack,
                prm,
                &enc,
                &mut fast,
                &mut trial,
                &mut scratch,
            )?;
            ranks.push(best.rank as usize);
            if best.rank > rank0 {
                st.requantized += 1;
            }
            hint = best.rank;
            left.spend(best.bytes.len(), weights[p]);
            out.extend_from_slice(&best.bytes);
            enc = best.state;
        }
        *slices[s].lock().unwrap() = (out, st, ranks);
        Ok(())
    })?;
    let (out, stats, ranks) = join_slices(&header, slices);
    *seed = ranks.into_iter().map(|r| r as i32).collect();
    #[cfg(feature = "wgpu")]
    if let Some((g, r)) = on_gpu {
        // BLUECOMPRESS_GPU_VERIFY: the CPU's frame is the output; report any difference.
        if g != out || r != *seed {
            eprintln!(
                "bluecompress: GPU frame differs from the CPU's ({} vs {} bytes); using the CPU's",
                g.len(),
                out.len()
            );
            crate::gpu::requant_wgpu::note_mismatch();
        }
    }
    Ok((out, stats))
}
