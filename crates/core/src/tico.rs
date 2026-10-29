//! TICO RAW codestream (intoPIX's JPEG XS variant used by Nikon N-RAW and R3D NE).
//!
//! Structure, as recorded by the Z 8 / Z6 III / Z 9 / ZR:
//!   SOC, CAP ("CONTACT_INTOPIX_" + 16 bytes), PIH (standard 24 bytes + 13 vendor
//!   bytes holding the square-root transfer curve), CDT (3 bytes per component: depth,
//!   sampling, per-component wavelet decomposition), WGT (gain, priority per band),
//!   then slices of precincts, EOC.
//!
//! Four components on the W/2 x H/2 quad grid: a Star-Tetrix decorrelation of the
//! Bayer quad (Y, Cr, delta-G, Cb). The third component (delta-G) has no wavelet; the
//! others use 5 horizontal levels and 1 vertical level of the reversible 5/3 wavelet.
//! One precinct covers two component rows (one low-pass and one high-pass line).
//! Packets: for each precinct line (0 = low, 1 = high), for each component, that
//! component's bands on that line. Weights are listed in that band order.

use std::ops::Range;
use std::sync::OnceLock;

use crate::bitio::{compact16, spread16, BitReader, BitWriter};
use crate::{Error, Result};

pub const MAX_COMPONENTS: usize = 4;
/// Coefficients per code group (Ng).
pub const GROUP: usize = 4;
/// Code groups per significance group (Ss).
pub const SIG_GROUP: usize = 8;
pub const TRUNCATION_MAX: i32 = 15;

const SOC: u16 = 0xff10;
const EOC: u16 = 0xff11;
const PIH: u16 = 0xff12;
const CDT: u16 = 0xff13;
const WGT: u16 = 0xff14;
const SLH: u16 = 0xff20;

#[inline]
pub(crate) fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

#[inline]
pub(crate) fn be24(d: &[u8], at: usize) -> u32 {
    (d[at] as u32) << 16 | (d[at + 1] as u32) << 8 | d[at + 2] as u32
}

fn require(cond: bool, what: &str) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(Error(what.into()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct TicoHeader {
    /// SOC .. last byte before the first SLH, re-emitted verbatim.
    pub raw: Vec<u8>,
    /// Offset of Lcod (u32) inside raw.
    pub lcod_offset: usize,
    /// Offset of Wf (u16), Hf follows.
    pub size_offset: usize,
    /// Full Bayer size.
    pub width: i32,
    pub height: i32,
    /// Precincts per slice (Hsl).
    pub slice_height: i32,
    pub components: i32,
    /// Horizontal levels per component, 0 = no wavelet.
    pub levels: [i32; MAX_COMPONENTS],
    pub fq: i32,
    pub br: i32,
    /// (gain, priority) per band.
    pub weights: Vec<(u8, u8)>,
    /// Transfer curve parameters.
    pub black: i32,
    pub gain: i32,
    pub knee: i32,
}

impl TicoHeader {
    /// Parses the header; returns it and the offset of the first slice.
    pub fn parse(data: &[u8]) -> Result<(TicoHeader, usize)> {
        let size = data.len();
        require(size >= 4 && be16(data, 0) == SOC, "not a JPEG XS codestream")?;
        let mut h = TicoHeader::default();
        let mut at = 2;
        let (mut have_pih, mut have_cdt) = (false, false);
        loop {
            require(at + 4 <= size, "codestream truncated in header")?;
            let marker = be16(data, at);
            if marker == SLH {
                break;
            }
            require(marker != EOC, "codestream without slices")?;
            let len = be16(data, at + 2) as usize;
            require(len >= 2 && at + 2 + len <= size, "bad marker length")?;
            let body = &data[at + 4..at + 2 + len];
            let blen = len - 2;
            if marker == PIH {
                require(blen == 37, "unsupported TICO RAW picture header")?;
                h.lcod_offset = at + 4;
                h.size_offset = at + 4 + 8;
                h.width = be16(body, 8) as i32;
                h.height = be16(body, 10) as i32;
                let precinct_width = be16(body, 12);
                h.slice_height = be16(body, 14) as i32;
                h.components = body[16] as i32;
                let (ng, ss) = (body[17] as usize, body[18] as usize);
                let fqbr = body[20];
                let order = body[21];
                let decomp = body[22];
                let flags = body[23];
                require(
                    precinct_width == 0
                        && h.components == 4
                        && ng == GROUP
                        && ss == SIG_GROUP
                        && (order >> 4) & 7 == 1
                        && decomp & 15 == 1,
                    "unsupported TICO RAW layout",
                )?;
                require(flags & 0x3f == 0x14, "unsupported TICO RAW coding flags")?;
                h.fq = (fqbr >> 4) as i32;
                h.br = (fqbr & 15) as i32;
                require(h.fq == 4 && h.br == 4, "unsupported TICO RAW precision")?;
                let t = &body[24..37];
                h.black = ((t[3] & 15) as i32) << 8 | t[4] as i32;
                h.gain = ((t[7] & 3) as i32) << 16 | (t[8] as i32) << 8 | t[9] as i32;
                h.knee = (t[10] as i32) << 8 | t[11] as i32;
                have_pih = true;
            } else if marker == CDT {
                require(have_pih && blen == 3 * h.components as usize, "unsupported TICO RAW component table")?;
                for c in 0..h.components as usize {
                    let (depth, samp, dec) = (body[3 * c], body[3 * c + 1], body[3 * c + 2]);
                    require(depth == 14 && samp == 0x11, "unsupported TICO RAW component")?;
                    let (hl, vl) = ((dec >> 4) as i32, (dec & 15) as i32);
                    require(
                        (hl == 0 && vl == 0) || (vl == 1 && (1..=8).contains(&hl)),
                        "unsupported TICO RAW decomposition",
                    )?;
                    h.levels[c] = hl;
                }
                have_cdt = true;
            } else if marker == WGT {
                require(blen.is_multiple_of(2), "bad weights table")?;
                h.weights = body.as_chunks::<2>().0.iter().map(|w| (w[0], w[1])).collect();
            }
            at += 2 + len;
        }
        require(have_pih && have_cdt && !h.weights.is_empty(), "missing mandatory markers")?;
        h.raw = data[..at].to_vec();
        Ok((h, at))
    }

    /// Re-emits the header for a (possibly) new size, with Lcod set to `lcod`.
    pub fn emit(&self, width: i32, height: i32, lcod: u32) -> Vec<u8> {
        let mut out = self.raw.clone();
        out[self.lcod_offset..self.lcod_offset + 4].copy_from_slice(&lcod.to_be_bytes());
        out[self.size_offset..self.size_offset + 2].copy_from_slice(&(width as u16).to_be_bytes());
        out[self.size_offset + 2..self.size_offset + 4].copy_from_slice(&(height as u16).to_be_bytes());
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct Band {
    pub comp: usize,
    /// Offset inside the component row it lives in.
    pub offset: usize,
    pub width: usize,
    /// Lives in the vertical high-pass row.
    pub high: bool,
    pub gain: u8,
    pub priority: u8,
    /// Code groups.
    pub groups: usize,
    /// Significance groups.
    pub sig_groups: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Packet {
    pub band_begin: usize,
    pub band_end: usize,
    /// Precinct line, 0 low / 1 high.
    pub line: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Bayer size.
    pub width: usize,
    pub height: usize,
    /// Component size.
    pub cw: usize,
    pub ch: usize,
    /// Precinct rows.
    pub precincts: usize,
    pub slice_height: usize,
    pub slices: usize,
    pub components: usize,
    pub levels: [i32; MAX_COMPONENTS],
    pub bands: Vec<Band>,
    pub packets: Vec<Packet>,
}

impl Layout {
    pub fn make(h: &TicoHeader, width: usize, height: usize) -> Result<Layout> {
        let cw = width / 2;
        let ch = height / 2;
        require(cw >= 64 && ch >= 4 && width.is_multiple_of(2) && height.is_multiple_of(2), "unsupported image size")?;
        let precincts = ch.div_ceil(2);
        let slice_height = h.slice_height as usize;
        require(slice_height > 0, "bad slice height")?;
        let mut l = Layout {
            width,
            height,
            cw,
            ch,
            precincts,
            slice_height,
            slices: precincts.div_ceil(slice_height),
            components: h.components as usize,
            levels: h.levels,
            ..Default::default()
        };
        let mut undecomposed = vec![usize::MAX; l.components];
        for line in 0..2 {
            for c in 0..l.components {
                let mut pk = Packet { line, band_begin: l.bands.len(), band_end: 0 };
                let add = |bands: &mut Vec<Band>, offset: usize, w: usize, high: bool| {
                    let groups = w.div_ceil(GROUP);
                    bands.push(Band {
                        comp: c,
                        offset,
                        width: w,
                        high,
                        groups,
                        sig_groups: groups.div_ceil(SIG_GROUP),
                        ..Default::default()
                    });
                };
                if h.levels[c] == 0 {
                    if line == 0 {
                        undecomposed[c] = l.bands.len();
                        add(&mut l.bands, 0, cw, false);
                    } else {
                        pk.band_begin = undecomposed[c];
                        pk.band_end = pk.band_begin + 1;
                        l.packets.push(pk);
                        continue;
                    }
                } else if line == 0 {
                    let mut low = cw;
                    let mut highs = Vec::new();
                    for _ in 0..h.levels[c] {
                        highs.push(low / 2);
                        low -= low / 2;
                    }
                    let mut offset = 0;
                    add(&mut l.bands, offset, low, false);
                    offset += low;
                    for &w in highs.iter().rev() {
                        add(&mut l.bands, offset, w, false);
                        offset += w;
                    }
                } else {
                    add(&mut l.bands, 0, cw - cw / 2, true);
                    add(&mut l.bands, cw - cw / 2, cw / 2, true);
                }
                pk.band_end = l.bands.len();
                l.packets.push(pk);
            }
        }
        require(l.bands.len() == h.weights.len(), "weights do not match the band layout")?;
        for (b, w) in l.bands.iter_mut().zip(&h.weights) {
            b.gain = w.0;
            b.priority = w.1;
        }
        Ok(l)
    }

    /// Cameras code both lines of every precinct, including the last precinct of an odd
    /// component height, whose second line lies outside the image (a phantom line that
    /// decoders discard). Outputs keep that structure, so the line is always present.
    #[inline]
    pub fn line_exists(&self, _precinct: usize, _line: usize) -> bool {
        true
    }

    pub fn precinct_header_bytes(&self) -> usize {
        5 + (2 * self.bands.len()).div_ceil(8)
    }
}

#[inline]
pub fn compute_gtli(gain: u8, priority: u8, q: i32, r: i32) -> i32 {
    let t = q - gain as i32 - if (priority as i32) < r { 1 } else { 0 };
    t.clamp(0, TRUNCATION_MAX)
}

/// One coded band line: GCLIs per code group, magnitudes per coefficient (already
/// shifted left by the truncation position, padding of the last group included), and signs.
#[derive(Debug, Clone, Default)]
pub struct BandLine {
    pub band: usize,
    pub gtli: i32,
    pub gcli: Vec<u8>,
    /// groups * GROUP entries.
    pub mag: Vec<u32>,
    /// groups * GROUP entries (0/1).
    pub neg: Vec<u8>,
}

/// Verbatim data and sign sub-packets of a packet (lossless pass-through), as byte
/// ranges of the source codestream.
#[derive(Debug, Clone, Default)]
pub struct PacketSpans {
    pub data: Range<usize>,
    pub sign: Range<usize>,
    pub valid: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PrecinctData {
    pub q: i32,
    pub r: i32,
    /// Coding mode per band (bit0 vertical, bit1 significance).
    pub modes: Vec<u8>,
    /// Indexed by packet; each holds the band lines of that packet.
    pub packets: Vec<Vec<BandLine>>,
    /// Set by a GCLI-only decode: magnitudes and signs are not decoded and the encoder
    /// copies these spans instead.
    pub spans: Vec<PacketSpans>,
    /// Lprc + header of the original.
    pub coded_bytes: usize,
}

/// Per-slice prediction state: previous line's GCLIs and gtli per band.
#[derive(Debug, Clone, Default)]
pub struct PredState {
    pub gcli: Vec<Vec<u8>>,
    pub gtli: Vec<i32>,
    pub valid: Vec<bool>,
}

impl PredState {
    pub fn reset(&mut self, bands: usize) {
        self.gcli.clear();
        self.gcli.resize(bands, Vec::new());
        self.gtli.clear();
        self.gtli.resize(bands, 0);
        self.valid.clear();
        self.valid.resize(bands, false);
    }
}

/// Offsets (into the codestream) of every precinct header, slice headers skipped.
pub fn find_precincts(cs: &[u8], first_slice: usize, l: &Layout) -> Result<Vec<usize>> {
    let size = cs.len();
    let mut offsets = Vec::with_capacity(l.precincts);
    let mut at = first_slice;
    let hdr = l.precinct_header_bytes();
    for p in 0..l.precincts {
        if p % l.slice_height == 0 {
            require(at + 6 <= size && be16(cs, at) == SLH, "slice header missing")?;
            at += 6;
        }
        require(at + hdr <= size, "codestream truncated")?;
        offsets.push(at);
        at += hdr + be24(cs, at) as usize;
    }
    // Camera streams may pad (zeros) between the last precinct and EOC.
    let mut eoc = at;
    while eoc + 1 < size && be16(cs, eoc) != EOC {
        eoc += 1;
    }
    require(eoc + 1 < size, "EOC missing")?;
    Ok(offsets)
}

#[inline]
fn mode_of(head: &[u8], b: usize) -> u8 {
    (head[5 + b / 4] >> (6 - 2 * (b % 4))) & 3
}

/// Decodes the precinct at `offset` and updates `state`. With `gcli_only`, only GCLIs
/// are decoded and the data/sign sub-packets are recorded as spans for verbatim re-emission.
pub fn decode_precinct(
    cs: &[u8],
    offset: usize,
    l: &Layout,
    _precinct: usize,
    state: &mut PredState,
    out: &mut PrecinctData,
    gcli_only: bool,
) -> Result<()> {
    let size = cs.len();
    let nb = l.bands.len();
    let hlen = l.precinct_header_bytes();
    require(offset + hlen <= size, "precinct header truncated")?;
    let head = &cs[offset..offset + hlen];
    let length = be24(head, 0) as usize;
    out.q = head[3] as i32;
    out.r = head[4] as i32;
    out.modes.clear();
    out.modes.extend((0..nb).map(|b| mode_of(head, b)));
    let start = offset + hlen;
    let end = start + length;
    require(end <= size, "precinct truncated")?;
    out.coded_bytes = hlen + length;
    out.packets.resize_with(l.packets.len(), Vec::new);
    out.spans.clear();
    out.spans.resize(l.packets.len(), PacketSpans::default());

    let mut br = BitReader::new(cs, start, end);
    let mut sig: Vec<u8> = Vec::new();
    let mut sig_offset: Vec<Option<usize>> = Vec::new();

    for (k, pk) in l.packets.iter().enumerate() {
        let lines = &mut out.packets[k];
        let ph = br.read(56);
        let raw = (ph >> 55) & 1 != 0;
        let data_len = ((ph >> 35) & 0xfffff) as usize;
        let gcli_len = ((ph >> 15) & 0xfffff) as usize;
        let sign_len = (ph & 0x7fff) as usize;

        let nbands = pk.band_end - pk.band_begin;
        lines.resize_with(nbands, BandLine::default);
        sig.clear();
        sig_offset.clear();
        sig_offset.resize(nbands, None);
        if !raw {
            for i in 0..nbands {
                let b = pk.band_begin + i;
                if out.modes[b] & 2 != 0 {
                    sig_offset[i] = Some(sig.len());
                    for _ in 0..l.bands[b].sig_groups {
                        sig.push(br.bit() as u8);
                    }
                }
            }
            br.align();
        }

        let gcli_start = br.byte();
        for i in 0..nbands {
            let b = pk.band_begin + i;
            let band = &l.bands[b];
            let bl = &mut lines[i];
            bl.band = b;
            let t = compute_gtli(band.gain, band.priority, out.q, out.r);
            bl.gtli = t;
            bl.gcli.clear();
            bl.gcli.resize(band.groups, 0);
            if raw {
                for g in 0..band.groups {
                    bl.gcli[g] = br.read(4) as u8;
                }
                continue;
            }
            let s = sig_offset[i].map(|o| &sig[o..]);
            if state.valid[b] && out.modes[b] & 1 != 0 {
                let top = &state.gcli[b];
                let floor = state.gtli[b].max(t);
                for g in 0..band.groups {
                    let m = (top[g] as i32).max(floor);
                    if s.is_some_and(|s| s[g / SIG_GROUP] != 0) {
                        bl.gcli[g] = m as u8;
                    } else {
                        let x = br.unary()? as i32;
                        let room = (m - t).max(0);
                        let delta = if x > 2 * room {
                            x - room
                        } else if x & 1 != 0 {
                            -(x + 1) / 2
                        } else {
                            x / 2
                        };
                        let v = m + delta;
                        require((0..=TRUNCATION_MAX).contains(&v), "bad GCLI")?;
                        bl.gcli[g] = v as u8;
                    }
                }
            } else {
                for g in 0..band.groups {
                    if s.is_some_and(|s| s[g / SIG_GROUP] != 0) {
                        bl.gcli[g] = t as u8;
                    } else {
                        let v = t + br.unary()? as i32;
                        require(v <= TRUNCATION_MAX, "bad GCLI")?;
                        bl.gcli[g] = v as u8;
                    }
                }
            }
        }
        br.align();
        br.skip_to(gcli_start + gcli_len)?;

        if gcli_only {
            let data_start = br.byte();
            require(data_start + data_len + sign_len <= end, "packet overruns precinct")?;
            out.spans[k] = PacketSpans {
                data: data_start..data_start + data_len,
                sign: data_start + data_len..data_start + data_len + sign_len,
                valid: true,
            };
            br.skip_to(data_start + data_len + sign_len)?;
        } else {
            let data_start = br.byte();
            for bl in lines.iter_mut() {
                let band = &l.bands[bl.band];
                let t = bl.gtli;
                bl.mag.clear();
                bl.mag.resize(band.groups * GROUP, 0);
                for g in 0..band.groups {
                    let planes = bl.gcli[g] as i32 - t;
                    if planes <= 0 {
                        continue;
                    }
                    let n = planes as u32 * 4;
                    let w = if n > 56 {
                        let hi = br.read(n - 32);
                        (hi << 32) | br.read(32)
                    } else {
                        br.read(n)
                    };
                    let m = &mut bl.mag[g * GROUP..g * GROUP + GROUP];
                    m[0] = compact16(w >> 3) << t;
                    m[1] = compact16(w >> 2) << t;
                    m[2] = compact16(w >> 1) << t;
                    m[3] = compact16(w) << t;
                }
            }
            br.align();
            br.skip_to(data_start + data_len)?;

            let sign_start = br.byte();
            for bl in lines.iter_mut() {
                bl.neg.clear();
                bl.neg.resize(bl.mag.len(), 0);
                for j in 0..bl.mag.len() {
                    if bl.mag[j] != 0 {
                        bl.neg[j] = br.bit() as u8;
                    }
                }
            }
            br.align();
            br.skip_to(sign_start + sign_len)?;
        }

        for bl in lines.iter() {
            state.gcli[bl.band].clone_from(&bl.gcli);
            state.gtli[bl.band] = bl.gtli;
            state.valid[bl.band] = true;
        }
    }
    Ok(())
}

/// GCLI residual code for vertical prediction.
#[inline]
fn vertical_code(g: i32, m: i32, t: i32) -> u32 {
    let room = (m - t).max(0);
    let delta = g - m;
    if delta > room {
        (delta + room) as u32
    } else if delta >= 0 {
        (2 * delta) as u32
    } else {
        (-2 * delta - 1) as u32
    }
}

/// Cost in bits of the GCLI (+ significance) content of a band line under each coding
/// mode. `top` describes the prediction source (None = none).
fn accumulate_cost(band: &Band, gcli: &[u8], t: i32, top: Option<(&[u8], i32)>, cost: &mut [i64; 4]) {
    let groups = band.groups;
    let floor = top.map_or(t, |(_, tg)| tg.max(t));
    let (mut z, mut v, mut zs, mut vs) = (0i64, 0i64, 0i64, 0i64);
    for sg in 0..band.sig_groups {
        let g0 = sg * SIG_GROUP;
        let g1 = groups.min(g0 + SIG_GROUP);
        let (mut bz, mut bv) = (0i64, 0i64);
        let (mut any_z, mut any_v) = (0u32, 0u32);
        for g in g0..g1 {
            let xz = (gcli[g] as i32 - t) as u32;
            bz += xz as i64 + 1;
            any_z |= xz;
            if let Some((tp, _)) = top {
                let m = (tp[g] as i32).max(floor);
                let xv = vertical_code(gcli[g] as i32, m, t);
                bv += xv as i64 + 1;
                any_v |= xv;
            }
        }
        z += bz;
        zs += 1 + if any_z != 0 { bz } else { 0 };
        if top.is_some() {
            v += bv;
            vs += 1 + if any_v != 0 { bv } else { 0 };
        }
    }
    if top.is_none() {
        v = z;
        vs = zs;
    }
    cost[0] += z;
    cost[1] += v;
    cost[2] += zs;
    cost[3] += vs;
}

fn forced_mode() -> Option<u8> {
    static FORCED: OnceLock<Option<u8>> = OnceLock::new();
    *FORCED.get_or_init(|| std::env::var("BC_FORCE_MODE").ok().and_then(|v| v.parse().ok()))
}

/// Chooses the cheapest coding mode per band for a precinct and stores it in pd.modes.
pub fn choose_modes(l: &Layout, pd: &mut PrecinctData, state: &PredState) {
    let nb = l.bands.len();
    let mut costs = vec![[0i64; 4]; nb];
    // Track a band's line 0 so the undecomposed component's line 1 is predicted from it.
    let mut line0: Vec<Option<(usize, usize)>> = vec![None; nb];
    for (k, lines) in pd.packets.iter().enumerate() {
        for (i, bl) in lines.iter().enumerate() {
            let b = bl.band;
            let top: Option<(&[u8], i32)> = if let Some((pk, pi)) = line0[b] {
                let src = &pd.packets[pk][pi];
                Some((&src.gcli[..], src.gtli))
            } else if state.valid[b] {
                Some((&state.gcli[b][..], state.gtli[b]))
            } else {
                None
            };
            accumulate_cost(&l.bands[b], &bl.gcli, bl.gtli, top, &mut costs[b]);
            line0[b] = Some((k, i));
        }
    }
    if let Some(f) = forced_mode() {
        pd.modes = vec![f; nb];
        return;
    }
    pd.modes = costs
        .iter()
        .map(|c| {
            let mut best = 3;
            for m in 0..4 {
                if c[m] < c[best] {
                    best = m;
                }
            }
            best as u8
        })
        .collect();
}

/// Joins encoded slices (each starting with its SLH) into a codestream: header, slices, EOC.
pub fn assemble_codestream(h: &TicoHeader, width: i32, height: i32, slices: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = slices.iter().map(|v| v.len()).sum();
    let lcod = (h.raw.len() + total + 2) as u32;
    let mut cs = Vec::with_capacity(lcod as usize);
    cs.extend_from_slice(&h.emit(width, height, lcod));
    for v in slices {
        cs.extend_from_slice(v);
    }
    cs.extend_from_slice(&[0xff, 0x11]);
    cs
}

/// Reusable buffers for [`encode_precinct`].
#[derive(Default)]
pub struct EncodeScratch {
    sig: Vec<u8>,
    gcli: Vec<u8>,
    data: Vec<u8>,
    sign: Vec<u8>,
}

/// Encodes a precinct (header + packets) and appends it to `out`. Coding modes come from
/// pd.modes (the vertical bit is ignored where no prediction exists). `src` is the
/// codestream that pd.spans refer to, if any. Updates `state`.
pub fn encode_precinct(
    l: &Layout,
    pd: &PrecinctData,
    src: Option<&[u8]>,
    state: &mut PredState,
    scratch: &mut EncodeScratch,
    out: &mut Vec<u8>,
) -> Result<()> {
    let nb = l.bands.len();
    let header_at = out.len();
    let hlen = l.precinct_header_bytes();
    out.resize(out.len() + hlen, 0);
    out[header_at + 3] = pd.q as u8;
    out[header_at + 4] = pd.r as u8;
    for b in 0..nb {
        out[header_at + 5 + b / 4] |= (pd.modes[b] & 3) << (6 - 2 * (b % 4));
    }
    let payload_at = out.len();

    for (k, lines) in pd.packets.iter().enumerate() {
        let EncodeScratch { sig, gcli, data, sign } = scratch;
        sig.clear();
        gcli.clear();
        data.clear();
        sign.clear();

        // Significance flags first (all bands of the packet), then GCLIs.
        {
            let mut sw = BitWriter::new(sig);
            for bl in lines {
                let b = bl.band;
                let band = &l.bands[b];
                let t = bl.gtli;
                let mode = pd.modes[b];
                if mode & 2 == 0 {
                    continue;
                }
                let vertical = mode & 1 != 0 && state.valid[b];
                let floor = if vertical { state.gtli[b].max(t) } else { t };
                for sg in 0..band.sig_groups {
                    let g0 = sg * SIG_GROUP;
                    let g1 = band.groups.min(g0 + SIG_GROUP);
                    let insignificant = (g0..g1).all(|g| {
                        let want = if vertical { (state.gcli[b][g] as i32).max(floor) } else { t };
                        bl.gcli[g] as i32 == want
                    });
                    sw.put(insignificant as u64, 1);
                }
            }
            sw.align();
        }
        {
            let mut gw = BitWriter::new(gcli);
            let mut codes = [0u32; SIG_GROUP];
            for bl in lines {
                let b = bl.band;
                let band = &l.bands[b];
                let t = bl.gtli;
                let mode = pd.modes[b];
                let vertical = mode & 1 != 0 && state.valid[b];
                let floor = if vertical { state.gtli[b].max(t) } else { t };
                for sg in 0..band.sig_groups {
                    let g0 = sg * SIG_GROUP;
                    let g1 = band.groups.min(g0 + SIG_GROUP);
                    let mut all_zero = true;
                    for g in g0..g1 {
                        let x = if vertical {
                            let m = (state.gcli[b][g] as i32).max(floor);
                            vertical_code(bl.gcli[g] as i32, m, t)
                        } else {
                            (bl.gcli[g] as i32 - t) as u32
                        };
                        codes[g - g0] = x;
                        all_zero &= x == 0;
                    }
                    if mode & 2 != 0 && all_zero {
                        continue;
                    }
                    for &c in &codes[..g1 - g0] {
                        gw.put_unary(c);
                    }
                }
            }
            gw.align();
        }

        let spans = pd.spans.get(k).filter(|s| s.valid);
        if let (Some(sp), Some(src)) = (spans, src) {
            data.extend_from_slice(&src[sp.data.clone()]);
            sign.extend_from_slice(&src[sp.sign.clone()]);
        } else {
            {
                let mut dw = BitWriter::new(data);
                for bl in lines {
                    let band = &l.bands[bl.band];
                    let t = bl.gtli;
                    for g in 0..band.groups {
                        let planes = bl.gcli[g] as i32 - t;
                        if planes <= 0 {
                            continue;
                        }
                        let m = &bl.mag[g * GROUP..g * GROUP + GROUP];
                        let w = spread16(m[0] >> t) << 3
                            | spread16(m[1] >> t) << 2
                            | spread16(m[2] >> t) << 1
                            | spread16(m[3] >> t);
                        dw.put_long(w, planes as u32 * 4);
                    }
                }
                dw.align();
            }
            let mut xw = BitWriter::new(sign);
            for bl in lines {
                for (m, ng) in bl.mag.as_chunks::<GROUP>().0.iter().zip(bl.neg.as_chunks::<GROUP>().0.iter()) {
                    if m[0] | m[1] | m[2] | m[3] == 0 {
                        continue;
                    }
                    let (mut bits, mut cnt) = (0u64, 0u32);
                    for j in 0..GROUP {
                        if m[j] != 0 {
                            bits = (bits << 1) | ng[j] as u64;
                            cnt += 1;
                        }
                    }
                    xw.put(bits, cnt);
                }
            }
            xw.align();
        }

        require(data.len() < (1 << 20) && gcli.len() < (1 << 20) && sign.len() < (1 << 15), "packet too large")?;
        let ph = (data.len() as u64) << 35 | (gcli.len() as u64) << 15 | sign.len() as u64;
        out.extend_from_slice(&ph.to_be_bytes()[1..]);
        out.extend_from_slice(sig);
        out.extend_from_slice(gcli);
        out.extend_from_slice(data);
        out.extend_from_slice(sign);

        for bl in lines {
            state.gcli[bl.band].clone_from(&bl.gcli);
            state.gtli[bl.band] = bl.gtli;
            state.valid[bl.band] = true;
        }
    }
    let length = out.len() - payload_at;
    require(length < (1 << 24), "precinct too large")?;
    out[header_at..header_at + 3].copy_from_slice(&(length as u32).to_be_bytes()[1..]);
    Ok(())
}

/// Uniform inverse quantizer: magnitude reconstruction from a coded magnitude.
#[inline]
pub fn dequant_uniform(m: u32, gcli: i32, gtli: i32) -> u32 {
    if gtli == 0 || gcli <= gtli || m == 0 {
        return m;
    }
    let scale = gcli - gtli + 1;
    let mut acc = 0u32;
    let mut rest = m;
    while rest > 0 {
        acc = acc.wrapping_add(rest);
        rest >>= scale;
    }
    acc
}

/// Uniform forward quantizer (ISO 21122 style); returns the coded magnitude (shifted by gtli).
#[inline]
pub fn quant_uniform(d: u32, gcli: i32, gtli: i32) -> u32 {
    if gcli <= gtli {
        return 0;
    }
    let scale = gcli - gtli + 1;
    let v = (((d as u64) << scale) - d as u64 + (1u64 << gcli)) >> (gcli + 1);
    (v << gtli) as u32
}

#[inline]
pub fn bit_length(v: u32) -> i32 {
    32 - v.leading_zeros() as i32
}
