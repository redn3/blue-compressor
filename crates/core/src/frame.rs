//! Whole-frame coefficient and pixel representations, and the CPU implementations of
//! every pixel stage (the GPU backends mirror these exactly).
//!
//! Coefficient domain ([`Coeffs`]): one contiguous buffer holding, per component, one
//! "low" row and one "high" row per precinct, cw wide: the dequantized wavelet
//! coefficients (sign applied, << Fq) at the band offsets of the layout. Planes are
//! stored low c0..c3 then high c0..c3. For the component without a wavelet the low row is
//! the even component row and the high row the odd one.
//!
//! Code domain ([`Planes`]): component planes (cw x ch, i32) after the inverse wavelet,
//! still Star-Tetrix decorrelated (Y, Cr, dG, Cb); after the inverse Star-Tetrix they are
//! the CFA planes (G at the red row, R, G at the blue row, B) in the square-root companded
//! 16-bit code domain (index - 32768). [`Transfer`] maps codes to 14-bit raw.
//!
//! The Star-Tetrix lifting steps, the 5/3 wavelet and the transfer curve follow rawmakase
//! (https://github.com/pch/rawmakase, Copyright (c) 2026 RAWmakase contributors, MIT); see
//! NOTICE and LICENSES/rawmakase-MIT.txt.

use rayon::prelude::*;

use crate::rate::{finest_fitting, max_rank, seed_slack, slice_budgets, Coded, SliceBudget};
use crate::requant::from_rank;
use crate::tico::*;
use crate::util::{parallel_for, SyncPtr};
use crate::Result;

/// Wavelet coefficients of a frame; see the module docs for the layout.
#[derive(Debug, Clone, Default)]
pub struct Coeffs {
    pub cw: usize,
    pub precincts: usize,
    pub data: Vec<i32>,
}

impl Coeffs {
    pub fn new(l: &Layout) -> Coeffs {
        Coeffs { cw: l.cw, precincts: l.precincts, data: vec![0; Self::len(l)] }
    }
    /// Elements for a layout (8 planes of precincts x cw).
    pub fn len(l: &Layout) -> usize {
        8 * l.precincts * l.cw
    }
    #[inline]
    pub fn plane_len(&self) -> usize {
        self.precincts * self.cw
    }
    /// Element offset of row `p` of the low (or high) plane of component `c`.
    #[inline]
    pub fn row_offset(&self, c: usize, high: bool, p: usize) -> usize {
        ((if high { 4 } else { 0 }) + c) * self.plane_len() + p * self.cw
    }
}

/// Four component planes, contiguous (plane c at c * w * h).
#[derive(Debug, Clone, Default)]
pub struct Planes {
    pub w: usize,
    pub h: usize,
    pub data: Vec<i32>,
}

impl Planes {
    pub fn new(w: usize, h: usize) -> Planes {
        Planes { w, h, data: vec![0; 4 * w * h] }
    }
    pub fn plane(&self, c: usize) -> &[i32] {
        &self.data[c * self.w * self.h..(c + 1) * self.w * self.h]
    }
}

/// The camera's square-root transfer curve (parameters from the PIH vendor bytes).
pub struct Transfer {
    pub black: i32,
    pub gain: i32,
    pub knee: i32,
    table: Vec<u16>,
}

impl Transfer {
    pub fn new(h: &TicoHeader) -> Transfer {
        let (black, gain, knee) = (h.black, h.gain, h.knee);
        let table = (0..=65535i64)
            .map(|i| {
                let d = i - knee as i64;
                let q = if d < 0 {
                    -d * d
                } else {
                    let t = (d * gain as i64 + 32768) >> 16;
                    t * t
                };
                (black as i64 + ((q + 163840) >> 18)).clamp(0, 16383) as u16
            })
            .collect();
        Transfer { black, gain, knee, table }
    }

    /// Code (v domain, index - 32768) to raw 14-bit, as the camera's decoder applies it.
    #[inline]
    pub fn to_raw(&self, v: i32) -> u16 {
        self.table[(v + 32768).clamp(0, 65535) as usize]
    }

    /// Continuous curve: code to raw.
    #[inline]
    pub fn to_raw_f(&self, code: f32) -> f32 {
        let d = code as f64 + 32768.0 - self.knee as f64;
        let q = if d < 0.0 {
            -d * d
        } else {
            let t = d * self.gain as f64 / 65536.0;
            t * t
        };
        (self.black as f64 + (q + 163840.0) / 262144.0) as f32
    }

    /// Inverse of [`Transfer::to_raw_f`]: raw to code.
    #[inline]
    pub fn to_code(&self, raw: f32) -> i32 {
        let q = (raw as f64 - self.black as f64) * 262144.0 - 163840.0;
        let d = if q >= 0.0 { q.sqrt() * 65536.0 / self.gain as f64 } else { -(-q).sqrt() };
        let i = (d + self.knee as f64).round().clamp(0.0, 65535.0) as i32;
        i - 32768
    }
}

/// Entropy-decodes a frame into `out` (length [`Coeffs::len`]), parallel over slices.
pub fn decode_frame(cs: &[u8], h: &TicoHeader, l: &Layout, out: &mut [i32]) -> Result<()> {
    let pre = find_precincts(cs, h.raw.len(), l)?;
    let fq = h.fq;
    let cw = l.cw;
    let plane = l.precincts * cw;
    let ptr = SyncPtr(out.as_mut_ptr());
    parallel_for(l.slices, 0, |s| {
        let mut st = PredState::default();
        st.reset(l.bands.len());
        let mut pd = PrecinctData::default();
        let p0 = s * l.slice_height;
        let p1 = l.precincts.min(p0 + l.slice_height);
        for p in p0..p1 {
            decode_precinct(cs, pre[p], l, p, &mut st, &mut pd, false)?;
            for (k, pk) in l.packets.iter().enumerate() {
                for bl in &pd.packets[k] {
                    let band = &l.bands[bl.band];
                    let hi = band.high || pk.line == 1;
                    let at = ((if hi { 4 } else { 0 }) + band.comp) * plane + p * cw + band.offset;
                    // Safety: slices own disjoint precinct rows.
                    let row = unsafe { ptr.slice(at, band.width) };
                    for (i, r) in row.iter_mut().enumerate() {
                        let v = (dequant_uniform(bl.mag[i], bl.gcli[i / GROUP] as i32, bl.gtli) << fq) as i32;
                        *r = if bl.neg[i] != 0 { -v } else { v };
                    }
                }
            }
        }
        Ok(())
    })
}

/// Quantizes precinct `p` of `coeffs` at (q, r) into `pd`.
fn fill_precinct(h: &TicoHeader, l: &Layout, coeffs: &[i32], p: usize, q: i32, r: i32, pd: &mut PrecinctData) {
    let fq = h.fq;
    let round = if fq > 0 { 1u32 << (fq - 1) } else { 0 };
    let max_mag = (1u32 << TRUNCATION_MAX) - 1;
    let cw = l.cw;
    let plane = l.precincts * cw;
    pd.q = q;
    pd.r = r;
    pd.packets.resize_with(l.packets.len(), Vec::new);
    for (k, pk) in l.packets.iter().enumerate() {
        let lines = &mut pd.packets[k];
        lines.resize_with(pk.band_end - pk.band_begin, BandLine::default);
        for (i, bl) in lines.iter_mut().enumerate() {
            let b = pk.band_begin + i;
            let band = &l.bands[b];
            bl.band = b;
            bl.gtli = compute_gtli(band.gain, band.priority, q, r);
            let hi = band.high || pk.line == 1;
            let at = ((if hi { 4 } else { 0 }) + band.comp) * plane + p * cw + band.offset;
            let row = &coeffs[at..at + band.width];
            bl.gcli.clear();
            bl.gcli.resize(band.groups, 0);
            bl.mag.clear();
            bl.mag.resize(band.groups * GROUP, 0);
            bl.neg.clear();
            bl.neg.resize(band.groups * GROUP, 0);
            let t = bl.gtli;
            for g in 0..band.groups {
                let mut d = [0u32; GROUP];
                let mut mx = 0;
                for j in 0..GROUP {
                    let x = g * GROUP + j;
                    if x >= band.width {
                        break;
                    }
                    let c = row[x];
                    let a = ((c.unsigned_abs() + round) >> fq).min(max_mag);
                    d[j] = a;
                    mx = mx.max(a);
                    bl.neg[x] = (c < 0) as u8;
                }
                let gc = bit_length(mx);
                let mut any = 0;
                if gc > t {
                    for j in 0..GROUP {
                        let m = quant_uniform(d[j], gc, t);
                        bl.mag[g * GROUP + j] = m;
                        any |= m;
                    }
                }
                for j in 0..GROUP {
                    if bl.mag[g * GROUP + j] == 0 {
                        bl.neg[g * GROUP + j] = 0;
                    }
                }
                bl.gcli[g] = if any != 0 { gc as u8 } else { t as u8 };
            }
        }
    }
}

/// Entropy-encodes a frame from its coefficients at a uniform truncation rank.
pub fn encode_frame(h: &TicoHeader, l: &Layout, coeffs: &[i32], rank: i32) -> Result<Vec<u8>> {
    Ok(encode_uniform(h, l, coeffs, rank)?.0)
}

/// [`encode_frame`], also returning each precinct's coded size.
fn encode_uniform(h: &TicoHeader, l: &Layout, coeffs: &[i32], rank: i32) -> Result<(Vec<u8>, Vec<usize>)> {
    let nb = l.bands.len();
    let (q, r) = from_rank(rank, nb as i32);
    let slices: Vec<Result<(Vec<u8>, Vec<usize>)>> = (0..l.slices)
        .into_par_iter()
        .map(|s| {
            let mut st = PredState::default();
            st.reset(nb);
            let mut pd = PrecinctData::default();
            let mut scratch = EncodeScratch::default();
            let mut out = vec![0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8];
            let p0 = s * l.slice_height;
            let p1 = l.precincts.min(p0 + l.slice_height);
            let mut sizes = Vec::with_capacity(p1 - p0);
            for p in p0..p1 {
                let before = out.len();
                fill_precinct(h, l, coeffs, p, q, r, &mut pd);
                choose_modes(l, &mut pd, &st);
                encode_precinct(l, &pd, None, &mut st, &mut scratch, &mut out)?;
                sizes.push(out.len() - before);
            }
            Ok((out, sizes))
        })
        .collect();
    let mut parts = Vec::with_capacity(l.slices);
    let mut sizes = Vec::with_capacity(l.precincts);
    for r in slices {
        let (v, sz) = r?;
        parts.push(v);
        sizes.extend(sz);
    }
    Ok((assemble_codestream(h, l.width as i32, l.height as i32, &parts), sizes))
}

/// Entropy-encodes a frame to fit `budget` codestream bytes (see [`crate::rate`]): each
/// precinct takes the finest rank, but never finer than `finest`, that fits its share.
pub fn encode_frame_budget(h: &TicoHeader, l: &Layout, coeffs: &[i32], finest: i32, budget: usize) -> Result<Vec<u8>> {
    encode_frame_budget_seeded(h, l, coeffs, finest, budget, &mut Vec::new())
}

/// [`encode_frame_budget`] for consecutive frames of a clip: `seed` holds the ranks the
/// previous frame chose (empty for the first) and is updated for the next.
pub fn encode_frame_budget_seeded(
    h: &TicoHeader,
    l: &Layout,
    coeffs: &[i32],
    finest: i32,
    budget: usize,
    seed: &mut Vec<i32>,
) -> Result<Vec<u8>> {
    // First pass at the finest rank: the output when it fits, else the precinct weights.
    let (first, weights) = encode_uniform(h, l, coeffs, finest)?;
    if first.len() <= budget {
        seed.clear();
        return Ok(first);
    }
    let prev = std::mem::take(seed);
    let seeded = prev.len() == l.precincts;
    let nb = l.bands.len();
    let hi = max_rank(l).max(finest);
    let budgets = slice_budgets(l, h.raw.len(), budget, &weights);
    let code = |p: usize, rank: i32, enc: &PredState, pd: &mut PrecinctData, scratch: &mut EncodeScratch| {
        let (q, r) = from_rank(rank, nb as i32);
        fill_precinct(h, l, coeffs, p, q, r, pd);
        choose_modes(l, pd, enc);
        let mut state = enc.clone();
        let mut bytes = Vec::new();
        encode_precinct(l, pd, None, &mut state, scratch, &mut bytes)?;
        Ok(Coded { rank, bytes, state })
    };
    // Every slice starts its search where the first precinct of the middle slice lands
    // (computed once, so the output does not depend on the thread count).
    let start = if seeded {
        finest
    } else {
        let s = l.slices / 2;
        let p = s * l.slice_height;
        let p1 = l.precincts.min(p + l.slice_height);
        let share = SliceBudget::new(budgets[s], &weights[p..p1]).share(weights[p]);
        let mut enc = PredState::default();
        enc.reset(nb);
        let (mut pd, mut scratch) = (PrecinctData::default(), EncodeScratch::default());
        finest_fitting(finest, hi, finest, share, 0, |rank| code(p, rank, &enc, &mut pd, &mut scratch))?.rank
    };
    let slices: Vec<Result<(Vec<u8>, Vec<i32>)>> = (0..l.slices)
        .into_par_iter()
        .map(|s| {
            let mut enc = PredState::default();
            enc.reset(nb);
            let mut pd = PrecinctData::default();
            let mut scratch = EncodeScratch::default();
            let mut out = Vec::with_capacity(budgets[s] + 4096);
            out.extend_from_slice(&[0xff, 0x20, 0x00, 0x04, (s >> 8) as u8, s as u8]);
            let p0 = s * l.slice_height;
            let p1 = l.precincts.min(p0 + l.slice_height);
            let mut left = SliceBudget::new(budgets[s], &weights[p0..p1]);
            let mut hint = start;
            let mut ranks = Vec::with_capacity(p1 - p0);
            for p in p0..p1 {
                let share = left.share(weights[p]);
                let (guess, slack) = if seeded { (prev[p], seed_slack(share)) } else { (hint, 0) };
                let best = finest_fitting(finest, hi, guess.clamp(finest, hi), share, slack, |rank| {
                    code(p, rank, &enc, &mut pd, &mut scratch)
                })?;
                hint = best.rank;
                ranks.push(best.rank);
                left.spend(best.bytes.len(), weights[p]);
                out.extend_from_slice(&best.bytes);
                enc = best.state;
            }
            Ok((out, ranks))
        })
        .collect();
    let mut parts = Vec::with_capacity(l.slices);
    for r in slices {
        let (out, ranks) = r?;
        parts.push(out);
        seed.extend(ranks);
    }
    Ok(assemble_codestream(h, l.width as i32, l.height as i32, &parts))
}

/// One level of the reversible 5/3 inverse on line[0, len): low half first (the larger
/// when len is odd), then the high half; symmetric extension.
fn inverse53(line: &mut [i32], len: usize, tmp: &mut [i32]) {
    if len < 2 {
        return;
    }
    let lows = len - len / 2;
    let highs = len / 2;
    {
        let (lo, hi) = line[..len].split_at(lows);
        let h = |k: isize| hi[k.clamp(0, highs as isize - 1) as usize];
        for k in 0..lows {
            tmp[2 * k] = lo[k] - ((h(k as isize - 1) + h(k as isize) + 2) >> 2);
        }
        for k in 0..highs {
            let right = if 2 * k + 2 < len { tmp[2 * k + 2] } else { tmp[2 * k] };
            tmp[2 * k + 1] = hi[k] + ((tmp[2 * k] + right) >> 1);
        }
    }
    line[..len].copy_from_slice(&tmp[..len]);
}

/// Forward counterpart of [`inverse53`].
fn forward53(line: &mut [i32], len: usize, tmp: &mut [i32]) {
    if len < 2 {
        return;
    }
    let lows = len - len / 2;
    let highs = len / 2;
    {
        let (lo, hi) = tmp[..len].split_at_mut(lows);
        for k in 0..highs {
            let right = if 2 * k + 2 < len { line[2 * k + 2] } else { line[2 * k] };
            hi[k] = line[2 * k + 1] - ((line[2 * k] + right) >> 1);
        }
        let h = |k: isize| hi[k.clamp(0, highs as isize - 1) as usize];
        for k in 0..lows {
            lo[k] = line[2 * k] + ((h(k as isize - 1) + h(k as isize) + 2) >> 2);
        }
    }
    line[..len].copy_from_slice(&tmp[..len]);
}

pub fn level_widths(cw: usize, levels: i32) -> Vec<usize> {
    let mut w = Vec::new();
    let mut len = cw;
    for _ in 0..levels {
        w.push(len);
        len -= len / 2;
    }
    w
}

/// Inverse wavelet: coefficients (modified in place by the horizontal pass) to planes.
pub fn inverse_wavelet(l: &Layout, coeffs: &mut [i32], out: &mut Planes) {
    let (cw, ch, pn) = (l.cw, l.ch, l.precincts);
    let plane = pn * cw;
    *out = Planes::new(cw, ch);
    let low_rows = ch.div_ceil(2);
    let high_rows = ch / 2;
    // Horizontal inverse per row: rows are cw-long chunks of the coefficient buffer.
    coeffs.par_chunks_mut(cw).enumerate().for_each_init(
        || vec![0i32; cw],
        |tmp, (row, line)| {
            let (plane_idx, p) = (row / pn, row % pn);
            let (c, high) = (plane_idx % 4, plane_idx >= 4);
            if l.levels[c] == 0 {
                return;
            }
            if !high {
                for &w in level_widths(cw, l.levels[c]).iter().rev() {
                    inverse53(line, w, tmp);
                }
            } else if p < high_rows {
                inverse53(line, cw, tmp);
            }
        },
    );
    // Vertical inverse per row pair.
    let coeffs = &*coeffs;
    let dst = SyncPtr(out.data.as_mut_ptr());
    (0..4 * low_rows).into_par_iter().for_each(|task| {
        let (c, k) = (task / low_rows, task % low_rows);
        let lo = &coeffs[c * plane..(c + 1) * plane];
        let hi = &coeffs[(4 + c) * plane..(5 + c) * plane];
        let base = c * cw * ch;
        // Safety: each task writes component rows 2k and 2k+1 of its plane only.
        let e = unsafe { dst.slice(base + 2 * k * cw, cw) };
        if l.levels[c] == 0 {
            e.copy_from_slice(&lo[k * cw..(k + 1) * cw]);
            if 2 * k + 1 < ch {
                unsafe { dst.slice(base + (2 * k + 1) * cw, cw) }.copy_from_slice(&hi[k * cw..(k + 1) * cw]);
            }
            return;
        }
        let hrow = |j: isize| &hi[(j.clamp(0, high_rows as isize - 1) as usize) * cw..][..cw];
        let even = |j: usize, x: usize| lo[j * cw + x] - ((hrow(j as isize - 1)[x] + hrow(j as isize)[x] + 2) >> 2);
        for x in 0..cw {
            e[x] = even(k, x);
        }
        if 2 * k + 1 < ch {
            let o = unsafe { dst.slice(base + (2 * k + 1) * cw, cw) };
            let has_next = 2 * k + 2 < ch;
            for x in 0..cw {
                let next = if has_next { even(k + 1, x) } else { e[x] };
                o[x] = hi[k * cw + x] + ((e[x] + next) >> 1);
            }
        }
    });
}

/// Forward wavelet: planes to coefficients (`out` has length [`Coeffs::len`]).
pub fn forward_wavelet(l: &Layout, input: &Planes, out: &mut [i32]) {
    let (cw, ch, pn) = (l.cw, l.ch, l.precincts);
    let plane = pn * cw;
    out.fill(0);
    let low_rows = ch.div_ceil(2);
    let high_rows = ch / 2;
    let src = &input.data;
    let optr = SyncPtr(out.as_mut_ptr());
    // Vertical forward: high rows first, then low rows.
    (0..4 * low_rows).into_par_iter().for_each(|task| {
        let (c, k) = (task / low_rows, task % low_rows);
        let s = &src[c * cw * ch..(c + 1) * cw * ch];
        let row = |r: usize| &s[r * cw..(r + 1) * cw];
        // Safety: each task writes row k of this component's low and high planes only.
        let lo = unsafe { optr.slice(c * plane + k * cw, cw) };
        if l.levels[c] == 0 {
            lo.copy_from_slice(row(2 * k));
            if 2 * k + 1 < ch {
                unsafe { optr.slice((4 + c) * plane + k * cw, cw) }.copy_from_slice(row(2 * k + 1));
            }
            return;
        }
        if 2 * k + 1 < ch {
            let (e, o) = (row(2 * k), row(2 * k + 1));
            let n = if 2 * k + 2 < ch { row(2 * k + 2) } else { e };
            let hi = unsafe { optr.slice((4 + c) * plane + k * cw, cw) };
            for x in 0..cw {
                hi[x] = o[x] - ((e[x] + n[x]) >> 1);
            }
        }
    });
    (0..4 * low_rows).into_par_iter().for_each(|task| {
        let (c, k) = (task / low_rows, task % low_rows);
        if l.levels[c] == 0 {
            return;
        }
        let e = &src[c * cw * ch + 2 * k * cw..][..cw];
        // Safety: reads high rows (finished above), writes low row k only.
        let hi = |j: isize| unsafe {
            &*optr.slice((4 + c) * plane + (j.clamp(0, high_rows as isize - 1) as usize) * cw, cw)
        };
        let (a, b) = (hi(k as isize - 1), hi(k as isize));
        let lo = unsafe { optr.slice(c * plane + k * cw, cw) };
        for x in 0..cw {
            lo[x] = e[x] + ((a[x] + b[x] + 2) >> 2);
        }
    });
    // Horizontal forward per row.
    out.par_chunks_mut(cw).enumerate().for_each_init(
        || vec![0i32; cw],
        |tmp, (row, line)| {
            let (plane_idx, p) = (row / pn, row % pn);
            let (c, high) = (plane_idx % 4, plane_idx >= 4);
            if l.levels[c] == 0 {
                return;
            }
            if !high {
                for w in level_widths(cw, l.levels[c]) {
                    forward53(line, w, tmp);
                }
            } else if p < high_rows {
                forward53(line, cw, tmp);
            }
        },
    );
    // Odd height: fill the phantom second line of the last precinct (see
    // Layout::line_exists). Repeating the previous high row is what symmetric extension
    // assumes, so the image decodes the same whether a decoder uses the phantom line or
    // ignores it.
    if high_rows < pn {
        for c in 0..4 {
            let dst = (4 + c) * plane + (pn - 1) * cw;
            let src = if l.levels[c] == 0 { c * plane + (pn - 1) * cw } else { (4 + c) * plane + (pn - 2) * cw };
            out.copy_within(src..src + cw, dst);
        }
    }
}

#[inline]
fn clampu(v: isize, n: usize) -> usize {
    v.clamp(0, n as isize - 1) as usize
}

/// One lifting step of the Star-Tetrix transform applied to plane `dst`, reading the
/// other planes. `f(planes, r, x, l, rr, u, d)` returns the new value; neighbours are
/// clamped indices (left, right, up, down).
fn star_step<F>(pl: &mut Planes, dst: usize, f: F)
where
    F: Fn(&[&[i32]; 4], usize, usize, usize, usize, usize, usize, usize) -> i32 + Sync,
{
    let (w, h) = (pl.w, pl.h);
    let n = w * h;
    let base = SyncPtr(pl.data.as_mut_ptr());
    (0..h).into_par_iter().for_each(|r| {
        // Safety: every task reads all planes and writes only row r of plane `dst`; each
        // step only reads `dst` at the position it writes.
        let planes: [&[i32]; 4] = std::array::from_fn(|c| unsafe { &*base.slice(c * n, n) });
        let u = clampu(r as isize - 1, h);
        let d = clampu(r as isize + 1, h);
        let mut row = vec![0i32; w];
        for (x, v) in row.iter_mut().enumerate() {
            let lft = clampu(x as isize - 1, w);
            let rgt = clampu(x as isize + 1, w);
            *v = f(&planes, r * w, x, lft, rgt, u * w, d * w, w);
        }
        unsafe { base.slice(dst * n + r * w, w) }.copy_from_slice(&row);
    });
}

/// Planes in: (Y, Cr, dG, Cb). Out: (G1, R, G2, B). See nikon_he.rs (rawmakase, MIT).
pub fn inverse_star_tetrix(pl: &mut Planes) {
    // p: planes, r: row offset, x: column, l/rr: left/right columns, u/d: up/down row offsets.
    star_step(pl, 0, |p, r, x, _l, rr, u, _d, _w| {
        let s = p[2][u + x] + p[2][u + rr] + p[2][r + x] + p[2][r + rr];
        p[0][r + x] - (s >> 3)
    });
    star_step(pl, 2, |p, r, x, l, _rr, _u, d, _w| {
        let chroma = p[1][r + x] + p[1][d + x] + p[3][r + l] + p[3][r + x];
        let greens = p[0][r + l] + p[0][r + x] + p[0][d + l] + p[0][d + x];
        p[2][r + x] - (chroma >> 3) + (greens >> 2)
    });
    star_step(pl, 0, |p, r, x, _l, rr, u, _d, _w| {
        let chroma = p[1][r + x] + p[1][r + rr] + p[3][u + x] + p[3][r + x];
        p[0][r + x] - (chroma >> 3)
    });
    star_step(pl, 1, |p, r, x, l, _rr, u, _d, _w| {
        let greens = p[0][r + l] + p[0][r + x] + p[2][u + x] + p[2][r + x];
        p[1][r + x] + (greens >> 2)
    });
    star_step(pl, 3, |p, r, x, _l, rr, _u, d, _w| {
        let greens = p[0][r + x] + p[0][d + x] + p[2][r + x] + p[2][r + rr];
        p[3][r + x] + (greens >> 2)
    });
}

/// Reverse order of the inverse, opposite signs.
pub fn forward_star_tetrix(pl: &mut Planes) {
    star_step(pl, 3, |p, r, x, _l, rr, _u, d, _w| {
        let greens = p[0][r + x] + p[0][d + x] + p[2][r + x] + p[2][r + rr];
        p[3][r + x] - (greens >> 2)
    });
    star_step(pl, 1, |p, r, x, l, _rr, u, _d, _w| {
        let greens = p[0][r + l] + p[0][r + x] + p[2][u + x] + p[2][r + x];
        p[1][r + x] - (greens >> 2)
    });
    star_step(pl, 0, |p, r, x, _l, rr, u, _d, _w| {
        let chroma = p[1][r + x] + p[1][r + rr] + p[3][u + x] + p[3][r + x];
        p[0][r + x] + (chroma >> 3)
    });
    star_step(pl, 2, |p, r, x, l, _rr, _u, d, _w| {
        let chroma = p[1][r + x] + p[1][d + x] + p[3][r + l] + p[3][r + x];
        let greens = p[0][r + l] + p[0][r + x] + p[0][d + l] + p[0][d + x];
        p[2][r + x] + (chroma >> 3) - (greens >> 2)
    });
    star_step(pl, 0, |p, r, x, _l, rr, u, _d, _w| {
        let s = p[2][u + x] + p[2][u + rr] + p[2][r + x] + p[2][r + rr];
        p[0][r + x] + (s >> 3)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> i32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed % 65536) as i32 - 32768
    }

    #[test]
    fn wavelet_53_is_lossless() {
        let mut seed = 12345;
        for len in [2usize, 3, 4, 5, 17, 64, 1031, 4134] {
            let orig: Vec<i32> = (0..len).map(|_| rng(&mut seed)).collect();
            let mut line = orig.clone();
            let mut tmp = vec![0; len];
            forward53(&mut line, len, &mut tmp);
            inverse53(&mut line, len, &mut tmp);
            assert_eq!(line, orig, "length {len}");
        }
    }

    #[test]
    fn star_tetrix_is_lossless() {
        let mut seed = 777;
        for (w, h) in [(1usize, 1usize), (2, 3), (7, 5), (64, 33)] {
            let mut pl = Planes::new(w, h);
            pl.data.iter_mut().for_each(|v| *v = rng(&mut seed) >> 2);
            let orig = pl.data.clone();
            forward_star_tetrix(&mut pl);
            inverse_star_tetrix(&mut pl);
            assert_eq!(pl.data, orig, "{w} x {h}");
        }
    }
}
