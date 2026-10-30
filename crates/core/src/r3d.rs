//! Rewrapping Nikon N-RAW (.NEV) as R3D NE (.R3D) and back.
//!
//! R3D NE from a Nikon ZR is the same TICO RAW codestream, frame wrapper and container as
//! N-RAW; the two differ only in a few tags. A rewrap copies the image data as it is and
//! rewrites those tags, in both directions:
//!
//! - video sample entry fourcc `NRAW` / `NR3D`;
//! - NCTG 0x21 / 0x26 / 0x27 (format codes) 49 / 2 / 1 and 65 / 4 / 3;
//! - NCTG 0x2000023 gamma and gamut: "N-LOG" code 1 or "RWGLOG3G10" (Log3G10 and
//!   REDWideGamutRGB) code 3;
//! - NCTG 0x523d (RED clip metadata) and 0x1020 (RED exposure block with the ISO), zero in
//!   N-RAW;
//! - NCTG 0x20000c0, a format constant;
//! - per frame NRMT 0x0190/0x0011 and 0x0190/0x0012 (the camera to XYZ matrix each format
//!   uses) and 0x0200/0x523d (a RED per frame block, zero in N-RAW).
//!
//! Quality (0x24, N-RAW High or Normal), white balance, exposure and maker notes are kept.
//! When every tag is present (ZR files) the rewrap patches them in place while streaming
//! the file, at disk speed. Clips from cameras that never record R3D NE (Z8, Z9, Z6III)
//! lack some of them; those are rebuilt with the tags inserted, still without decoding
//! (experimental: no real clips of those cameras were available to test).
//!
//! At the same ISO, RED's decoder renders an R3D NE rewrap about half a stop darker than its
//! N-RAW original (compared side by side in REDCINE-X PRO). The R3D keeps the real ISO; an
//! `.RMD` sidecar (RSX layout) next to it carries the offset ([`zr_exposure_ev`]) so they match.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::bmff::{fourcc, InputFile, Movie};
use crate::{Error, Options, Result};

/// Folder converted clips go to, next to the job's output.
pub const R3D_FOLDER: &str = "R3D NE";
pub const NEV_FOLDER: &str = "N-RAW";

/// RMD exposure offset for ZR clips: +0.5 EV. In RED's own decoder (sidecar ignored, same
/// ISO 800) the N-RAW original renders about half a stop brighter than the R3D NE rewrap.
pub const ZR_EXPOSURE_EV: f64 = 0.5;
/// RMD exposure offset for other cameras (no measurements yet; the ZR value is the
/// starting point). TODO: check against real Z8, Z9 and Z6III clips.
pub const OTHER_EXPOSURE_EV: f64 = 0.5;

/// Exposure offset for a camera model. The model string may carry any case, padding or
/// trailing NULs ("NIKON ZR", "Nikon ZR\0\0").
pub fn zr_exposure_ev(model: &str) -> f64 {
    let m: String =
        model.trim_matches(|c: char| c == '\0' || c.is_whitespace()).split_whitespace().collect::<Vec<_>>().join(" ");
    if m.eq_ignore_ascii_case("NIKON ZR") {
        ZR_EXPOSURE_EV
    } else {
        OTHER_EXPOSURE_EV
    }
}

/// Which way a rewrap goes, from the source's codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToR3d,
    ToNev,
}

#[derive(Debug, Clone)]
pub struct R3dOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Write the exposure `.RMD` next to an R3D NE output.
    pub rmd: bool,
    /// RMD exposure in stops; `None` uses [`zr_exposure_ev`] for the camera.
    pub exposure_ev: Option<f64>,
}

impl R3dOptions {
    pub fn new(input: &Path, output: &Path) -> R3dOptions {
        R3dOptions { input: input.into(), output: output.into(), rmd: true, exposure_ev: None }
    }
}

#[derive(Debug, Clone)]
pub struct R3dOutcome {
    pub output: PathBuf,
    pub direction: Direction,
    pub frames: u32,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub seconds: f64,
    /// ISO in the RED exposure tag (R3D NE output; 0 for N-RAW).
    pub iso: u32,
    /// The camera never records R3D NE (Z8, Z9, Z6III): experimental.
    pub experimental: bool,
    /// The exposure sidecar, when written.
    pub rmd: Option<PathBuf>,
    /// Size of every video frame when they are all the same, else 0.
    pub frame_bytes: u32,
}

/// One NCTG entry; `value` is the offset of its value inside the movie's moov bytes.
#[derive(Debug, Clone)]
pub struct Tag {
    pub id: u32,
    pub ty: u16,
    pub count: u16,
    pub value: usize,
    pub len: usize,
}

fn rd32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

/// The NCTG entries of moov/udta/NCDT.
pub fn nctg_tags(m: &Movie) -> Vec<Tag> {
    const UNIT: [usize; 13] = [1, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8];
    let mm = &m.moov;
    let mut out = Vec::new();
    // Walk moov > udta > NCDT > NCTG by hand (sizes only, 32-bit boxes).
    let child = |start: usize, end: usize, kind: &[u8; 4]| -> Option<(usize, usize)> {
        let mut at = start;
        while at + 8 <= end {
            let len = rd32(mm, at) as usize;
            if len < 8 || at + len > end {
                return None;
            }
            if rd32(mm, at + 4) == fourcc(kind) {
                return Some((at + 8, at + len));
            }
            at += len;
        }
        None
    };
    if mm.len() < 8 {
        return out;
    }
    let Some((u0, u1)) = child(8, mm.len(), b"udta") else { return out };
    let Some((n0, n1)) = child(u0, u1, b"NCDT") else { return out };
    let Some((t0, t1)) = child(n0, n1, b"NCTG") else { return out };
    let mut q = t0;
    while q + 8 <= t1 {
        let id = rd32(mm, q);
        let ty = u16::from_be_bytes([mm[q + 4], mm[q + 5]]);
        let count = u16::from_be_bytes([mm[q + 6], mm[q + 7]]);
        let len = if (ty as usize) < 13 { UNIT[ty as usize] } else { 1 } * count as usize;
        if q + 8 + len > t1 {
            break;
        }
        out.push(Tag { id, ty, count, value: q + 8, len });
        q += 8 + len;
    }
    out
}

/// Value bytes of NCTG tag `id`.
pub fn tag_value<'a>(m: &'a Movie, tags: &[Tag], id: u32) -> Option<&'a [u8]> {
    tags.iter().find(|t| t.id == id).map(|t| &m.moov[t.value..t.value + t.len])
}

/// Where the converted clip of a job with output `out` goes: `<out folder>/R3D NE/<name>.R3D`.
pub fn r3d_path(out: &Path, source: &Path) -> PathBuf {
    let dir = out.parent().unwrap_or(Path::new(".")).join(R3D_FOLDER);
    dir.join(format!("{}.R3D", red_name(source)))
}

/// RED style clip name `A001_C001_MMDDxx` for a source. ZR files are already named that way
/// (A007_C002_10299S.NEV), so their stem is kept; other names get reel 001, clip 001 and a
/// two character suffix derived from the name, so different sources do not collide.
pub fn red_name(source: &Path) -> String {
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if parse_red_name(&stem).is_some() {
        return stem.to_ascii_uppercase();
    }
    const ALNUM: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let h = stem.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let mmdd = std::fs::metadata(source)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| mmdd_of(d.as_secs()))
        .unwrap_or_else(|| "0101".into());
    format!("A001_C001_{mmdd}{}{}", ALNUM[(h % 36) as usize] as char, ALNUM[((h / 36) % 36) as usize] as char)
}

fn mmdd_of(secs: u64) -> String {
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}{d:02}")
}

/// (camera letter, reel, clip, suffix) of `A007_C002_10299S`.
pub fn parse_red_name(stem: &str) -> Option<(char, String, String, String)> {
    let b = stem.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if b.len() != 16 || !b[0].is_ascii_alphabetic() || b[4] != b'_' || !b[5].eq_ignore_ascii_case(&b'C') || b[9] != b'_'
    {
        return None;
    }
    if !digits(1..4) || !digits(6..9) || !digits(10..14) || !b[14..16].iter().all(u8::is_ascii_alphanumeric) {
        return None;
    }
    Some((b[0].to_ascii_uppercase() as char, stem[1..4].into(), stem[6..9].into(), stem[14..16].to_ascii_uppercase()))
}

/// The 256 byte RED clip metadata of NCTG 0x523d: entries of u16 length (itself and the id
/// included), u16 id, value; the rest is one 0x0090 padding entry.
pub fn red_clip_block(name: &str) -> Vec<u8> {
    let (cam, reel, clip, suffix) = parse_red_name(name).unwrap_or(('A', "001".into(), "001".into(), "00".into()));
    let file = format!("{name}.R3D");
    let mut b = Vec::with_capacity(256);
    let mut put = |id: u16, v: &[u8]| {
        b.extend_from_slice(&((v.len() + 4) as u16).to_be_bytes());
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(v);
    };
    put(0x1019, cam.to_string().as_bytes());
    put(0x101a, reel.as_bytes());
    put(0x101b, clip.as_bytes());
    // Same in every ZR R3D NE seen so far; meaning not known.
    put(0x1051, b"C");
    put(0x1052, suffix.as_bytes());
    put(0x1056, file.as_bytes());
    let pad = 256 - b.len();
    b.extend_from_slice(&(pad as u16).to_be_bytes());
    b.extend_from_slice(&0x0090u16.to_be_bytes());
    b.resize(256, 0);
    b
}

/// RED exposure block of NCTG 0x1020 for an ISO.
pub fn red_exposure_block(iso: u32) -> [u8; 12] {
    let mut b = [0u8, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0];
    b[10..12].copy_from_slice(&(iso.min(65535) as u16).to_be_bytes());
    b
}

/// Where the N-RAW rewrap of a job with output `out` goes: `<out folder>/N-RAW/<name>.NEV`.
pub fn nev_path(out: &Path, source: &Path) -> PathBuf {
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    out.parent().unwrap_or(Path::new(".")).join(NEV_FOLDER).join(format!("{stem}.NEV"))
}

/// The exposure sidecar of an R3D NE clip: same name, `.RMD`.
pub fn rmd_path(r3d: &Path) -> PathBuf {
    r3d.with_extension("RMD")
}

/// A GUID in RED's sidecar style (8-4-4-4-12 upper case hex), from a seed mixed with the
/// clock so every sidecar gets its own.
fn guid(seed: u64, n: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let h = |k: u64| {
        let mut x = std::collections::hash_map::RandomState::new().build_hasher();
        x.write_u64(seed ^ now ^ k.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        x.finish()
    };
    let (a, b) = (h(2 * n), h(2 * n + 1));
    format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        a >> 32,
        (a >> 16) & 0xffff,
        (a & 0x0fff) | 0x4000,
        ((b >> 48) & 0x3fff) | 0x8000,
        b & 0xffff_ffff_ffff
    )
}

/// The settings written to the `.RMD` sidecar of a rewrapped clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RmdLook {
    pub iso: u32,
    pub kelvin: f64,
    pub tint: f64,
    /// Exposure Adjust in stops.
    pub exposure_ev: f64,
}

/// The `.RMD` sidecar text in RED's RSX layout (the one RED's own software writes): a legacy
/// `<looks>` block and a `<looks2 colorversion="3">` block for the IPP2 pipeline R3D NE uses,
/// each with one look whose `R3DIPS` module carries ISO and white balance. The exposure
/// offset is written only to the `<looks2>` Exposure Adjust; the legacy block keeps exposure
/// compensation at 0 so it is never applied twice. Everything else is at RED's defaults (Medium roll off and tone map, REDWideGamutRGB
/// and Log3G10).
pub fn rmd_text(l: RmdLook) -> String {
    let seed = (l.iso as u64) << 32 ^ l.kelvin.to_bits() ^ l.exposure_ev.to_bits();
    let (g1, g2) = (guid(seed, 0), guid(seed, 1));
    let (iso, k, t, ev) = (l.iso, l.kelvin, l.tint, l.exposure_ev);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<sidecar version="2.0" type="RSX">
    <looks>
        <primary type="guid" value="{g1}"/>
        <looklist>
            <look id="{g1}" name="">
                <lookmodules>
                    <module name="R3DIPS" order="0">
                        <iso type="int" value="{iso}"/>
                        <kelvin type="float" value="{k:.5}"/>
                        <tint type="float" value="{t:.5}"/>
                        <exposurecompensation type="float" value="0.00000"/>
                    </module>
                </lookmodules>
            </look>
        </looklist>
    </looks>
    <looks2 colorversion="3">
        <primary type="guid" value="{g2}"/>
        <looklist>
            <look id="{g2}" name="">
                <lookmodules>
                    <module name="R3DIPS" order="0">
                        <colorversion type="int" value="3"/>
                        <iso type="int" value="{iso}"/>
                        <kelvin type="float" value="{k:.5}"/>
                        <tint type="float" value="{t:.5}"/>
                        <exposureadjust type="float" value="{ev:.5}"/>
                        <exposurecompensation type="float" value="0.00000"/>
                        <highlightrolloff type="int" value="3"/>
                        <outputtonemap type="int" value="1"/>
                        <colorspace type="int" value="1"/>
                        <gammaspace type="string" value="12BT"/>
                    </module>
                </lookmodules>
            </look>
        </looklist>
    </looks2>
</sidecar>
"#
    )
}

// Per direction values. Frame values are the NRMT bytes after group and tag.
const R3D_CODES: [u16; 3] = [65, 4, 3];
pub(crate) const NEV_CODES: [u16; 3] = [49, 2, 1];
const R3D_C0: [u8; 8] = [0x48, 0x01, 0x0c, 0x00, 0x78, 0x01, 0x0c, 0x00];
pub(crate) const NEV_C0: [u8; 8] = [0x60, 0x01, 0x0c, 0x00, 0xa8, 0x01, 0x0c, 0x00];
const R3D_0011: [u8; 7] = [0x00, 0x0f, 0xc1, 0x14, 0x2d, 0xf0, 0xce];
const NEV_0011: [u8; 7] = [0x00, 0x0f, 0xc1, 0x12, 0xe1, 0xf0, 0xce];
/// Camera to XYZ (D65) matrices, 9 big endian f32 after a zero byte (rows sum to the D65
/// white point). Cameras that never record R3D NE get the ZR's: the Z6III shares the ZR's
/// sensor; for the Z8 and Z9 it is an approximation.
const R3D_MATRIX: [u8; 37] = [
    0x00, 0x3f, 0x14, 0xd5, 0xfa, 0x3e, 0x7a, 0x88, 0x27, 0x3d, 0xfe, 0xc5, 0x6d, 0x3e, 0x50, 0xef, 0xdd, 0x3f, 0x74,
    0xf5, 0x6f, 0xbe, 0x24, 0xc5, 0x97, 0xbd, 0x3d, 0xe3, 0xfc, 0xbe, 0xb2, 0x77, 0xc4, 0x3f, 0xbd, 0xf3, 0x0e,
];
const NEV_MATRIX: [u8; 37] = [
    0x00, 0x3f, 0x1b, 0x09, 0x0e, 0x3e, 0xc3, 0x35, 0xe2, 0xbd, 0x15, 0x2e, 0xa6, 0x3e, 0x69, 0xb7, 0x6a, 0x3f, 0x86,
    0x2f, 0xfe, 0xbe, 0x8d, 0x9b, 0xac, 0x3b, 0xcd, 0x14, 0x18, 0xbe, 0x31, 0x39, 0xd2, 0x3f, 0xa0, 0xc0, 0x65,
];
/// RED per frame block (0x0200/0x523d), zero padded. The third field changes a little over
/// a camera clip (0x00db, 0x0225, 0x0227; maybe a sensor reading) and is a constant here.
const FRAME_RED: [u8; 18] =
    [0x00, 0x00, 0x05, 0x30, 0xaf, 0x00, 0x00, 0x08, 0x60, 0x6c, 0x00, 0x00, 0x02, 0x25, 0x00, 0x33, 0x00, 0x90];
/// Length of 0x0200/0x523d when it has to be inserted.
const FRAME_RED_LEN: usize = 65;

/// 0x2000023 for a direction: the source's block with both strings and the code changed,
/// or a full block when the source has none.
fn gamma_block(src: Option<&[u8]>, dir: Direction) -> Vec<u8> {
    let mut b = match src {
        Some(v) if v.len() >= 50 => v.to_vec(),
        _ => {
            let mut v = b"03110310".to_vec();
            v.resize(48, 0);
            v.extend([0x80, 0x01, 0, 0, 0]);
            v.extend([0xff, 0x01, 0xff, 0x0a]);
            for _ in 0..7 {
                v.extend([0xff, 0x04]);
            }
            v.extend([0xff, 0xff, 0xff, 0x04, 0xff, 0x04, 0xff, 0x04]);
            v.resize(97, 0);
            for _ in 0..5 {
                v.extend([0xff, 0x01]);
            }
            v.resize(108, 0);
            v
        }
    };
    let name: &[u8] = if dir == Direction::ToR3d { b"RWGLOG3G10" } else { b"N-LOG" };
    for slot in [8usize, 28] {
        b[slot..slot + 20].fill(0);
        b[slot..slot + name.len()].copy_from_slice(name);
    }
    b[49] = if dir == Direction::ToR3d { 0x03 } else { 0x01 };
    b
}

/// What a rewrap of one clip writes.
#[derive(Debug, Clone)]
pub struct R3dPlan {
    pub direction: Direction,
    pub model: String,
    pub iso: u32,
    pub experimental: bool,
    /// The new NCTG entries (id, type, value), in id order.
    tags: Vec<(u32, u16, Vec<u8>)>,
    /// The rebuilt moov/udta box (for the rebuilding path).
    pub(crate) udta: Vec<u8>,
}

fn u16v(v: u16) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

/// Plans a rewrap of `m` to the direction its codec implies, naming the output `name`
/// (file stem).
pub fn plan(m: &Movie, name: &str) -> Result<R3dPlan> {
    let direction = match m.video().codec {
        c if c == fourcc(b"NRAW") => Direction::ToR3d,
        c if c == fourcc(b"NR3D") => Direction::ToNev,
        _ => return Err(Error("not a Nikon N-RAW or R3D NE clip".into())),
    };
    let tags = nctg_tags(m);
    let get = |id: u32| tag_value(m, &tags, id);
    let model = get(0x2).map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').to_string()).unwrap_or_default();
    if !model.starts_with("NIKON") {
        return Err(Error("R3D NE and N-RAW rewraps need a Nikon clip".into()));
    }
    let experimental = model != "NIKON ZR";
    let iso = get(0x0110_8832).filter(|v| v.len() == 4).map(|v| rd32(v, 0)).unwrap_or(0);
    let r3d = direction == Direction::ToR3d;
    let codes = if r3d { R3D_CODES } else { NEV_CODES };
    let mut set: Vec<(u32, u16, Vec<u8>)> = vec![
        (0x21, 3, u16v(codes[0])),
        (0x26, 3, u16v(codes[1])),
        (0x27, 3, u16v(codes[2])),
        (0x1020, 7, if r3d { red_exposure_block(iso).to_vec() } else { vec![0; 12] }),
        (0x523d, 7, if r3d { red_clip_block(name) } else { vec![0; 256] }),
        (0x0200_0023, 7, gamma_block(get(0x0200_0023), direction)),
        (0x0200_00c0, 7, if r3d { R3D_C0.to_vec() } else { NEV_C0.to_vec() }),
    ];
    let mut all: Vec<(u32, u16, Vec<u8>)> = tags
        .iter()
        .map(|t| (t.id, t.ty, m.moov[t.value..t.value + t.len].to_vec()))
        .filter(|(id, _, _)| !set.iter().any(|s| s.0 == *id))
        .collect();
    all.append(&mut set);
    all.sort_by_key(|t| t.0);
    let udta = build_udta(m, &all)?;
    Ok(R3dPlan { direction, model, iso, experimental, tags: all, udta })
}

/// moov/udta with NCDT/NCTG replaced by `tags` (everything else as it was).
pub(crate) fn build_udta(m: &Movie, tags: &[(u32, u16, Vec<u8>)]) -> Result<Vec<u8>> {
    let mm = &m.moov;
    let boxes = |start: usize, end: usize| -> Vec<(usize, usize, u32)> {
        let mut v = Vec::new();
        let mut at = start;
        while at + 8 <= end {
            let len = rd32(mm, at) as usize;
            if len < 8 || at + len > end {
                break;
            }
            v.push((at, len, rd32(mm, at + 4)));
            at += len;
        }
        v
    };
    let wrap = |kind: &[u8; 4], payload: &[u8]| {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    };
    let mut nctg = Vec::new();
    for (id, ty, v) in tags {
        let unit = match ty {
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => 1,
        };
        nctg.extend(id.to_be_bytes());
        nctg.extend(ty.to_be_bytes());
        nctg.extend(((v.len() / unit) as u16).to_be_bytes());
        nctg.extend(v);
    }
    let udta = boxes(8, mm.len()).into_iter().find(|b| b.2 == fourcc(b"udta"));
    let Some((u0, ul, _)) = udta else { return Err(Error("clip has no camera metadata (udta)".into())) };
    let mut udta_p = Vec::new();
    for (a, l, k) in boxes(u0 + 8, u0 + ul) {
        if k != fourcc(b"NCDT") {
            udta_p.extend_from_slice(&mm[a..a + l]);
            continue;
        }
        let mut ncdt = Vec::new();
        let mut had = false;
        for (c, cl, ck) in boxes(a + 8, a + l) {
            if ck == fourcc(b"NCTG") {
                ncdt.extend(wrap(b"NCTG", &nctg));
                had = true;
            } else {
                ncdt.extend_from_slice(&mm[c..c + cl]);
            }
        }
        if !had {
            ncdt.extend(wrap(b"NCTG", &nctg));
        }
        udta_p.extend(wrap(b"NCDT", &ncdt));
    }
    Ok(wrap(b"udta", &udta_p))
}

/// Frame values for a direction: (key, bytes).
fn frame_values(dir: Direction, red_len: usize) -> [(u32, Vec<u8>); 3] {
    let r3d = dir == Direction::ToR3d;
    let mut red = vec![0u8; red_len.max(FRAME_RED.len())];
    if r3d {
        red[..FRAME_RED.len()].copy_from_slice(&FRAME_RED);
    }
    [
        (0x0190_0011, if r3d { R3D_0011.to_vec() } else { NEV_0011.to_vec() }),
        (0x0190_0012, if r3d { R3D_MATRIX.to_vec() } else { NEV_MATRIX.to_vec() }),
        (0x0200_523d, red),
    ]
}

/// The NRMT boxes of a frame header: (key, value offset, value length) inside `h`, and the
/// NRFH box (offset, length).
/// NRMT (tag, value offset, value length) entries, and the NRTI (offset, length).
/// The NRMT tags of a frame sample header: (tag, value bytes).
pub fn frame_tags(sample: &[u8]) -> Vec<(u32, &[u8])> {
    frame_nrmts(sample).0.into_iter().map(|(t, at, len)| (t, &sample[at..at + len])).collect()
}

/// The N-RAW camera to XYZ matrix, row major (for frames that carry none).
pub fn default_camera_matrix() -> [f32; 9] {
    let mut m = [0f32; 9];
    for (i, c) in NEV_MATRIX[1..].chunks(4).enumerate() {
        m[i] = f32::from_be_bytes(c.try_into().unwrap());
    }
    m
}

type FrameNrmts = (Vec<(u32, usize, usize)>, Option<(usize, usize)>);

fn frame_nrmts(h: &[u8]) -> FrameNrmts {
    let mut out = Vec::new();
    let mut nrfh = None;
    let mut p = 8;
    while p + 8 <= h.len() {
        if h[p] == 0xff && h[p + 1] == 0x10 {
            break;
        }
        let len = rd32(h, p) as usize;
        if len < 8 || p + len > h.len() {
            break;
        }
        if rd32(h, p + 4) == fourcc(b"NRFH") {
            nrfh = Some((p, len));
            let mut q = p + 8;
            while q + 8 <= p + len {
                let l = rd32(h, q) as usize;
                if l < 8 || q + l > p + len {
                    break;
                }
                if rd32(h, q + 4) == fourcc(b"NRMT") && l >= 12 {
                    out.push((rd32(h, q + 8), q + 12, l - 12));
                }
                q += l;
            }
        }
        p += len;
    }
    (out, nrfh)
}

/// In place frame patches (absolute offsets) for a header `h` read at `at`, or None when a
/// value is missing or has another size.
fn frame_patches(h: &[u8], at: u64, dir: Direction, out: &mut Vec<(u64, Vec<u8>)>) -> Option<()> {
    let (nrmts, _) = frame_nrmts(h);
    let red_len = nrmts.iter().find(|n| n.0 == 0x0200_523d).map_or(0, |n| n.2);
    for (key, v) in frame_values(dir, red_len) {
        let (_, off, len) = nrmts.iter().find(|n| n.0 == key)?;
        if *len != v.len() {
            return None;
        }
        out.push((at + *off as u64, v));
    }
    Some(())
}

/// Rewrites one frame (a whole NRAW sample) for `dir`, replacing or inserting the frame
/// values. The header is padded (NRTA) to whole KiB so the codestream starts where cameras
/// put it, and every frame of a clip keeps the same header size.
pub(crate) fn rewrite_frame(sample: &[u8], dir: Direction) -> Result<Vec<u8>> {
    let f = crate::nraw::NrawFrame::parse(sample)?;
    let h = &sample[..f.codestream_offset];
    let (nrmts, nrfh) = frame_nrmts(h);
    let Some((fh, fl)) = nrfh else { return Err(Error("frame has no NRFH header".into())) };
    let red_len = nrmts.iter().find(|n| n.0 == 0x0200_523d).map_or(FRAME_RED_LEN, |n| n.2);
    let values = frame_values(dir, red_len);
    let nrmt = |key: u32, v: &[u8]| {
        let mut b = ((v.len() + 12) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"NRMT");
        b.extend(key.to_be_bytes());
        b.extend_from_slice(v);
        b
    };
    // NRFH children with the values replaced, missing ones appended in key order.
    let mut nrfh_p = Vec::new();
    let mut q = fh + 8;
    while q + 8 <= fh + fl {
        let l = rd32(h, q) as usize;
        let key = if rd32(h, q + 4) == fourcc(b"NRMT") && l >= 12 { Some(rd32(h, q + 8)) } else { None };
        match key.and_then(|k| values.iter().find(|v| v.0 == k)) {
            Some((k, v)) => nrfh_p.extend(nrmt(*k, v)),
            None => nrfh_p.extend_from_slice(&h[q..q + l]),
        }
        q += l;
    }
    for (k, v) in &values {
        if !nrmts.iter().any(|n| n.0 == *k) {
            nrfh_p.extend(nrmt(*k, v));
        }
    }
    // Rebuild the header: NRAW, children before NRFH, NRFH, children after it without
    // the NRTA padding, then NRTA to the next KiB inside NRTH.
    let mut body = Vec::new();
    let mut p = 8;
    let mut nrth: Option<Vec<u8>> = None;
    while p + 8 <= h.len() {
        let len = rd32(h, p) as usize;
        if len < 8 || p + len > h.len() {
            break;
        }
        let kind = rd32(h, p + 4);
        if kind == fourcc(b"NRFH") {
            body.extend(((nrfh_p.len() + 8) as u32).to_be_bytes());
            body.extend_from_slice(b"NRFH");
            body.extend_from_slice(&nrfh_p);
        } else if kind == fourcc(b"NRTH") {
            // Keep NRTH's children except NRTA.
            let mut c = Vec::new();
            let mut r = p + 8;
            while r + 8 <= p + len {
                let l = rd32(h, r) as usize;
                if l < 8 || r + l > p + len {
                    break;
                }
                if rd32(h, r + 4) != fourcc(b"NRTA") {
                    c.extend_from_slice(&h[r..r + l]);
                }
                r += l;
            }
            nrth = Some(c);
        } else {
            body.extend_from_slice(&h[p..p + len]);
        }
        p += len;
    }
    let nrth = nrth.ok_or_else(|| Error("frame has no NRTH header".into()))?;
    let fixed = 8 + body.len() + 8 + nrth.len() + 8;
    let total = fixed.div_ceil(1024) * 1024;
    let mut out = Vec::with_capacity(total + sample.len() - f.codestream_offset);
    out.extend([0u8; 4]);
    out.extend_from_slice(b"NRAW");
    out.extend(body);
    out.extend(((total - out.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"NRTH");
    out.extend(nrth);
    let pad = total - out.len();
    out.extend((pad as u32).to_be_bytes());
    out.extend_from_slice(b"NRTA");
    out.resize(total, 0);
    out.extend_from_slice(&sample[f.codestream_offset..]);
    let n = out.len() as u32;
    out[..4].copy_from_slice(&n.to_be_bytes());
    Ok(out)
}

/// Checks that `input` can be rewrapped, without writing anything; returns the plan.
pub fn check(input: &Path) -> Result<R3dPlan> {
    let f = InputFile::open(input)?;
    let m = Movie::parse(&f)?;
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    plan(&m, &stem)
}

fn finish_outcome(
    o: &R3dOptions,
    p: &R3dPlan,
    start: Instant,
    input_bytes: u64,
    output_bytes: u64,
) -> Result<R3dOutcome> {
    let m = Movie::parse(&InputFile::open(&o.output)?)?;
    let v = m.video();
    let frame_bytes = v.samples.first().map_or(0, |s| s.size);
    let frame_bytes = if v.samples.iter().all(|s| s.size == frame_bytes) { frame_bytes } else { 0 };
    let rmd = if p.direction == Direction::ToR3d && o.rmd {
        let path = rmd_path(&o.output);
        let ev = o.exposure_ev.unwrap_or_else(|| zr_exposure_ev(&p.model));
        let kelvin = crate::develop::clip_info(&o.output).ok().map(|c| c.kelvin).filter(|&k| k > 0).unwrap_or(5600);
        let look = RmdLook { iso: p.iso, kelvin: kelvin as f64, tint: 0.0, exposure_ev: ev };
        std::fs::write(&path, rmd_text(look)).map_err(|e| Error(format!("cannot write {}: {e}", path.display())))?;
        Some(path)
    } else {
        None
    };
    Ok(R3dOutcome {
        output: o.output.clone(),
        direction: p.direction,
        frames: v.samples.len() as u32,
        input_bytes,
        output_bytes,
        seconds: start.elapsed().as_secs_f64(),
        iso: if p.direction == Direction::ToR3d { p.iso } else { 0 },
        experimental: p.experimental,
        rmd,
        frame_bytes,
    })
}

/// Rewraps `o.input` (N-RAW to R3D NE, or R3D NE to N-RAW, from its codec) to `o.output`
/// without decoding: the image data is copied as it is, so frame sizes stay the source's
/// (constant for camera files). Written to `<output>.partial` and renamed when complete.
/// `progress(fraction, mb_per_s)`; `cancel` removes the partial file.
pub fn convert(o: &R3dOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<R3dOutcome> {
    let start = Instant::now();
    let input = InputFile::open(&o.input)?;
    let m = Movie::parse(&input)?;
    let stem = o.output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let p = plan(&m, &stem)?;
    if let Some(dir) = o.output.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error(format!("cannot create {}: {e}", dir.display())))?;
    }
    match in_place_patches(&input, &m, &p)? {
        Some(patches) => {
            let bytes = stream_patched(&o.input, &o.output, input.size(), &patches, cancel, start, progress)?;
            finish_outcome(o, &p, start, input.size(), bytes)
        }
        None => {
            // Tags missing (other cameras): rebuild without decoding.
            let job = Options {
                input: o.input.clone(),
                output: o.output.clone(),
                quality: 1,
                bpp: None,
                resolution: crate::Resolution::Original,
                backend: crate::Backend::Cpu,
                threads: 0,
                cbr: false,
                template: None,
            };
            let r = crate::transcode::transcode_with(&job, Some(&p), cancel, progress)?;
            finish_outcome(o, &p, start, r.input_bytes, r.output_bytes)
        }
    }
}

/// [`convert`] for a recompressed or resized R3D NE: the job's target and resolution with
/// the R3D NE tags (always constant bitrate). N-RAW sources only.
pub fn convert_compressed(
    job: &Options,
    o: &R3dOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<R3dOutcome> {
    let start = Instant::now();
    let m = Movie::parse(&InputFile::open(&job.input)?)?;
    let stem = o.output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let p = plan(&m, &stem)?;
    if p.direction != Direction::ToR3d {
        return Err(Error("only N-RAW clips are compressed to R3D NE".into()));
    }
    if let Some(dir) = o.output.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error(format!("cannot create {}: {e}", dir.display())))?;
    }
    let opt = Options { output: o.output.clone(), ..job.clone() };
    let r = crate::transcode::transcode_with(&opt, Some(&p), cancel, progress)?;
    finish_outcome(o, &p, start, r.input_bytes, r.output_bytes)
}

/// All in place patches (absolute offsets, sorted) when every tag the plan changes exists
/// at its new size; None otherwise.
/// (file offset, new bytes) writes.
type Patches = Vec<(u64, Vec<u8>)>;

fn in_place_patches(input: &InputFile, m: &Movie, p: &R3dPlan) -> Result<Option<Patches>> {
    let moov_at = m.top.iter().find(|b| b.kind == fourcc(b"moov")).map_or(0, |b| b.offset);
    let tags = nctg_tags(m);
    let mut out = Vec::new();
    for (id, _, v) in &p.tags {
        match tags.iter().find(|t| t.id == *id) {
            Some(t) if t.len == v.len() => {
                if m.moov[t.value..t.value + t.len] != v[..] {
                    out.push((moov_at + t.value as u64, v.clone()));
                }
            }
            _ => return Ok(None),
        }
    }
    if tags.len() != p.tags.len() {
        return Ok(None);
    }
    let codec = if p.direction == Direction::ToR3d { b"NR3D" } else { b"NRAW" };
    out.push((moov_at + m.video().stsd_entry_offset as u64 + 4, codec.to_vec()));
    for s in &m.video().samples {
        let h = input.read(s.offset, (s.size as usize).min(4096))?;
        if frame_patches(&h, s.offset, p.direction, &mut out).is_none() {
            return Ok(None);
        }
    }
    out.sort_by_key(|x| x.0);
    Ok(Some(out))
}

fn stream_patched(
    src_path: &Path,
    dst_path: &Path,
    total: u64,
    patches: &[(u64, Vec<u8>)],
    cancel: &AtomicBool,
    start: Instant,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<u64> {
    let mut partial = dst_path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let r = (|| -> Result<()> {
        let mut src = File::open(src_path).map_err(|e| Error(format!("cannot open {}: {e}", src_path.display())))?;
        let mut dst = BufWriter::with_capacity(
            1 << 20,
            File::create(&partial).map_err(|e| Error(format!("cannot create {}: {e}", partial.display())))?,
        );
        let mut buf = vec![0u8; 8 << 20];
        let (mut at, mut next) = (0u64, 0usize);
        let mut last = Instant::now();
        while at < total {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error("cancelled".into()));
            }
            let n = ((total - at) as usize).min(buf.len());
            src.read_exact(&mut buf[..n]).map_err(|e| Error(format!("read error: {e}")))?;
            let end = at + n as u64;
            // Patches never overlap; one may straddle two blocks and is applied to both.
            let mut k = next;
            while k < patches.len() && patches[k].0 < end {
                let (pos, ref b) = patches[k];
                for (i, &x) in b.iter().enumerate() {
                    let q = pos + i as u64;
                    if q >= at && q < end {
                        buf[(q - at) as usize] = x;
                    }
                }
                if pos + b.len() as u64 <= end {
                    next = k + 1;
                }
                k += 1;
            }
            dst.write_all(&buf[..n]).map_err(|e| Error(format!("write error: {e}")))?;
            at = end;
            if last.elapsed().as_millis() >= 100 || at == total {
                last = Instant::now();
                let secs = start.elapsed().as_secs_f64().max(1e-3);
                progress(at as f64 / total as f64, at as f64 / 1e6 / secs);
            }
        }
        dst.flush().map_err(|e| Error(format!("write error: {e}")))
    })();
    match r {
        Ok(()) => {
            std::fs::rename(&partial, dst_path)
                .map_err(|e| Error(format!("cannot move output into place at {}: {e}", dst_path.display())))?;
            Ok(total)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// True for a `.nev` file (any case).
pub fn is_nev(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("nev"))
}

/// True for a clip whose compressed copy is N-RAW, so it can also become R3D NE: a `.nev`,
/// or a ProRes RAW `.mov`.
pub fn becomes_nev(path: &Path) -> bool {
    is_nev(path) || crate::prores_raw::is_prores_raw(path)
}

/// True for a `.r3d` file (any case).
pub fn is_r3d(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("r3d"))
}

/// The conversion settings of a job.
#[derive(Debug, Clone, Copy, Default)]
pub struct Convert {
    /// .nev clips also become R3D NE.
    pub to_r3d: bool,
    /// .R3D clips also become N-RAW.
    pub to_nev: bool,
    /// Rewrap only: no compressed copy; the conversion is the job's only output and copies
    /// the image data as it is.
    pub rewrap: bool,
    /// Exposure in the .RMD, in stops; None for the camera default (+1 ZR, +0.5 others).
    pub exposure_ev: Option<f64>,
    /// Write the exposure .RMD next to R3D NE outputs.
    pub rmd: bool,
    /// With `to_r3d`: the R3D NE (and its .RMD) is the only output of N-RAW and ProRes RAW
    /// clips, no N-RAW copy is kept. N-RAW clips are coded straight into R3D NE; ProRes
    /// RAW is coded to a temporary N-RAW that is removed once rewrapped.
    pub r3d_only: bool,
}

impl Convert {
    /// True when the conversion is the job's only output for `src` (rewrap, or R3D only).
    pub fn only_output(&self, src: &Path) -> bool {
        self.rewrap || (self.r3d_only && self.to_r3d && becomes_nev(src))
    }
}

/// What the conversion step of a job did.
#[derive(Debug, Clone)]
pub enum R3dJob {
    Done(R3dOutcome),
    /// Not converted, with the reason.
    Skipped(String),
}

/// The conversion step of a job with options `job` (its output path decides the folder):
/// a .nev with `to_r3d` becomes R3D NE in `R3D NE/` (rewrapped, or at the job's
/// compression and resolution unless `rewrap`), a .R3D with `to_nev` is rewrapped to N-RAW
/// in `N-RAW/`. Other clips are skipped.
pub fn convert_job(
    job: &Options,
    c: &Convert,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<R3dJob> {
    let src = &job.input;
    if crate::prores_raw::is_prores_raw(src) {
        // The job's N-RAW output is already coded at the job's setting: rewrap it.
        if !c.to_r3d {
            return Ok(R3dJob::Skipped("no conversion for this clip".into()));
        }
        if c.r3d_only {
            // Code to a temporary N-RAW at the job's setting, rewrap it, then remove it.
            let t0 = Instant::now();
            // R3D NE is always constant bitrate: frames padded to the target, as the N-RAW path does.
            let staged = Options { output: job.output.clone(), cbr: true, ..job.clone() };
            crate::transcode_staged(&staged, cancel, &mut |f, fps| progress(f * 0.9, fps))?;
            let o = R3dOptions {
                rmd: c.rmd,
                exposure_ev: c.exposure_ev,
                ..R3dOptions::new(&job.output, &r3d_path(&job.output, src))
            };
            let r = convert(&o, cancel, &mut |f, fps| progress(0.9 + f * 0.1, fps));
            let _ = std::fs::remove_file(&job.output);
            let mut d = r?;
            d.input_bytes = std::fs::metadata(src).map(|m| m.len()).unwrap_or(d.input_bytes);
            d.seconds = t0.elapsed().as_secs_f64();
            return Ok(R3dJob::Done(d));
        }
        if c.rewrap {
            return Ok(R3dJob::Skipped("ProRes RAW is re-encoded, there is nothing to rewrap".into()));
        }
        let nev = &job.output;
        if let Err(e) = check(nev) {
            return Ok(R3dJob::Skipped(e.0));
        }
        let o =
            R3dOptions { rmd: c.rmd, exposure_ev: c.exposure_ev, ..R3dOptions::new(nev, &r3d_path(&job.output, src)) };
        return convert(&o, cancel, progress).map(R3dJob::Done);
    }
    let (wanted, out) = if is_nev(src) {
        (c.to_r3d, r3d_path(&job.output, src))
    } else if is_r3d(src) {
        (c.to_nev, nev_path(&job.output, src))
    } else {
        (false, PathBuf::new())
    };
    if !wanted {
        return Ok(R3dJob::Skipped("no conversion for this clip".into()));
    }
    if let Err(e) = check(src) {
        return Ok(R3dJob::Skipped(e.0));
    }
    let o = R3dOptions { rmd: c.rmd, exposure_ev: c.exposure_ev, ..R3dOptions::new(src, &out) };
    let plain = c.rewrap
        || is_r3d(src)
        || (job.quality <= 1 && job.bpp.is_none() && job.resolution == crate::Resolution::Original);
    let r = if plain { convert(&o, cancel, progress) } else { convert_compressed(job, &o, cancel, progress) };
    r.map(R3dJob::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_names() {
        assert_eq!(parse_red_name("A007_C002_10299S"), Some(('A', "007".into(), "002".into(), "9S".into())));
        assert_eq!(parse_red_name("DSC_0001"), None);
        assert_eq!(red_name(Path::new("/x/A007_C002_10299S.NEV")), "A007_C002_10299S");
        let n = red_name(Path::new("/nonexistent/clip one.nev"));
        assert!(parse_red_name(&n).is_some(), "{n}");
    }

    #[test]
    fn clip_block_matches_camera_layout() {
        let b = red_clip_block("A007_C001_1029E2");
        assert_eq!(b.len(), 256);
        assert_eq!(&b[..5], &[0x00, 0x05, 0x10, 0x19, b'A']);
        let file = b"A007_C001_1029E2.R3D";
        let at = b.windows(file.len()).position(|w| w == file).unwrap();
        assert_eq!(&b[at - 4..at], &[0x00, 0x18, 0x10, 0x56]);
        assert_eq!(red_exposure_block(1600), [0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0x06, 0x40]);
    }

    #[test]
    fn frame_matrices_map_white_to_d65() {
        for m in [R3D_MATRIX, NEV_MATRIX] {
            let f: Vec<f32> = m[1..].chunks(4).map(|c| f32::from_be_bytes(c.try_into().unwrap())).collect();
            // Rows sum to the D65 white point in XYZ.
            let sums: Vec<f32> = f.chunks(3).map(|r| r.iter().sum()).collect();
            assert!((sums[0] - 0.9505).abs() < 2e-3 && (sums[1] - 1.0).abs() < 2e-3 && (sums[2] - 1.089).abs() < 2e-3);
        }
    }

    #[test]
    fn rmd_is_rsx() {
        let t = rmd_text(RmdLook { iso: 800, kelvin: 4749.0, tint: 0.0, exposure_ev: 0.5 });
        assert!(t.contains("<sidecar version=\"2.0\" type=\"RSX\">") && t.contains("<looks2 colorversion=\"3\">"));
        assert!(
            t.contains("<iso type=\"int\" value=\"800\"/>")
                && t.contains("<kelvin type=\"float\" value=\"4749.00000\"/>")
        );
        assert!(t.contains("<exposureadjust type=\"float\" value=\"0.50000\"/>"));
        assert_eq!(t.matches("<look id=").count(), 2);
        assert!(!t.contains('\u{2014}'));
        // The exposure offset lives only in <looks2>; the legacy block stays at 0 so no
        // decoder applies it twice.
        let legacy = &t[t.find("<looks>").unwrap()..t.find("</looks>").unwrap()];
        assert!(
            legacy.contains("<exposurecompensation type=\"float\" value=\"0.00000\"/>") && !legacy.contains("0.50000")
        );
        assert!(
            t.matches("<lookmodules>").count() == 2 && t.matches("<module name=\"R3DIPS\" order=\"0\">").count() == 2
        );
        assert_eq!(t.matches("name=\"\">").count(), 2);
        let g: Vec<&str> = t.match_indices("type=\"guid\" value=\"").map(|(i, _)| &t[i + 19..i + 55]).collect();
        assert!(g.len() == 2 && g[0] != g[1] && g.iter().all(|x| x.len() == 36 && x.matches('-').count() == 4));
    }

    #[test]
    fn model_match_is_tolerant() {
        for m in ["NIKON ZR", "Nikon ZR", " nikon  zr \0\0", "NIKON ZR\0"] {
            assert_eq!(zr_exposure_ev(m), ZR_EXPOSURE_EV, "{m:?}");
        }
        assert_eq!(zr_exposure_ev("NIKON Z 8"), OTHER_EXPOSURE_EV);
    }
}
