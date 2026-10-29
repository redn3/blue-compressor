//! N-RAW (.NEV) to R3D NE (.R3D) conversion for Nikon ZR clips.
//!
//! An R3D NE clip from a ZR uses the same TICO RAW codestream, frame wrapper and container
//! as an N-RAW High clip (about 3 bits per sample). The two differ only in a few tags, all
//! of the same size in both formats, so the conversion copies the source byte for byte and
//! rewrites those tags in place; the codestream, audio, timecode and previews are untouched:
//!
//! - video sample entry fourcc `NRAW` to `NR3D`;
//! - NCTG 0x21 / 0x26 / 0x27 (format codes) 49 / 2 / 1 to 65 / 4 / 3;
//! - NCTG 0x2000023 gamma and gamut: "N-LOG" to "RWGLOG3G10", code 0x01 to 0x03;
//! - NCTG 0x523d: RED clip metadata (camera letter, reel, clip, name);
//! - NCTG 0x1020: RED exposure block with the ISO (see [`ISO_FACTOR`]);
//! - NCTG 0x20000c0: an R3D NE constant;
//! - per frame NRMT 0x0190/0x0011 and 0x0190/0x0012 (the camera to XYZ matrix R3D NE
//!   uses) and 0x0200/0x523d (a RED per frame block).
//!
//! Everything else (quality 0x24, white balance, exposure, maker notes) is kept. These
//! values come from comparing matched ZR clip pairs; N-RAW Normal (0x24 = 2, about 1.5
//! bits per sample) has no R3D NE equivalent and is refused.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::bmff::{fourcc, InputFile, Movie};
use crate::{Error, Result};

/// Gain compensation as a factor on the ISO written to the RED exposure tag 0x1020.
///
/// On matched ZR shots at the same ISO, shutter and aperture, R3D NE raw values above black
/// were about 2x N-RAW's (1.95 to 1.99 in every channel), so the camera exposes R3D NE
/// about a stop hotter. Tagging the N-RAW clip's ISO as 2x lets RED decoders brighten it to
/// match instead of scaling (and re-encoding) the raw data.
///
/// TODO: confirm with Erik that the light did not change between the two takes, and check a
/// converted clip next to a real R3D NE in Resolve; set this to 1.0 if the 2x was the scene.
pub const ISO_FACTOR: f64 = 2.0;

/// Message for N-RAW Normal sources.
pub const NORMAL_UNSUPPORTED: &str = "N-RAW Normal not supported for R3D (R3D NE only exists at High quality)";

/// Folder the converted clip goes to, next to the job's output.
pub const R3D_FOLDER: &str = "R3D NE";

#[derive(Debug, Clone)]
pub struct R3dOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Factor on the ISO written to 0x1020 ([`ISO_FACTOR`] by default).
    pub iso_factor: f64,
}

impl R3dOptions {
    pub fn new(input: &Path, output: &Path) -> R3dOptions {
        R3dOptions { input: input.into(), output: output.into(), iso_factor: ISO_FACTOR }
    }
}

#[derive(Debug, Clone)]
pub struct R3dOutcome {
    pub output: PathBuf,
    pub frames: u32,
    pub output_bytes: u64,
    pub seconds: f64,
    /// ISO written to the RED exposure tag.
    pub iso: u32,
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

/// 0x2000023 with the gamma and gamut set to RWG / Log3G10.
fn gamma_block(src: &[u8]) -> Result<Vec<u8>> {
    if src.len() < 50 || &src[8..13] != b"N-LOG" || &src[28..33] != b"N-LOG" {
        return Err(Error("R3D NE conversion needs a clip recorded in N-Log".into()));
    }
    let mut b = src.to_vec();
    for slot in [8usize, 28] {
        b[slot..slot + 20].fill(0);
        b[slot..slot + 10].copy_from_slice(b"RWGLOG3G10");
    }
    b[49] = 0x03;
    Ok(b)
}

/// R3D NE per frame values (bytes after the NRMT group and tag).
const FRAME_0011: [u8; 7] = [0x00, 0x0f, 0xc1, 0x14, 0x2d, 0xf0, 0xce];
/// Camera to XYZ (D65) matrix R3D NE uses, as 9 big endian f32 after a zero byte.
const FRAME_MATRIX: [u8; 37] = [
    0x00, 0x3f, 0x14, 0xd5, 0xfa, 0x3e, 0x7a, 0x88, 0x27, 0x3d, 0xfe, 0xc5, 0x6d, 0x3e, 0x50, 0xef, 0xdd, 0x3f, 0x74,
    0xf5, 0x6f, 0xbe, 0x24, 0xc5, 0x97, 0xbd, 0x3d, 0xe3, 0xfc, 0xbe, 0xb2, 0x77, 0xc4, 0x3f, 0xbd, 0xf3, 0x0e,
];
/// RED per frame block (0x0200/0x523d), zero padded to the slot. The third field changes a
/// little over a clip in real R3D NE files (0x00db, 0x0225, 0x0227; maybe a sensor reading)
/// and is written as a constant.
const FRAME_RED: [u8; 18] =
    [0x00, 0x00, 0x05, 0x30, 0xaf, 0x00, 0x00, 0x08, 0x60, 0x6c, 0x00, 0x00, 0x02, 0x25, 0x00, 0x33, 0x00, 0x90];

/// In place patches (absolute file offset, bytes) for one frame's header `h` read at `at`.
fn frame_patches(h: &[u8], at: u64, out: &mut Vec<(u64, Vec<u8>)>) -> Result<()> {
    let mut found = [false; 3];
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
            let mut q = p + 8;
            while q + 8 <= p + len {
                let l = rd32(h, q) as usize;
                if l < 8 || q + l > p + len {
                    break;
                }
                if rd32(h, q + 4) == fourcc(b"NRMT") && l >= 12 {
                    let key = rd32(h, q + 8);
                    let (v, n) = (q + 12, l - 12);
                    let value: Option<(usize, Vec<u8>)> = match key {
                        0x0190_0011 if n == FRAME_0011.len() => Some((0, FRAME_0011.to_vec())),
                        0x0190_0012 if n == FRAME_MATRIX.len() => Some((1, FRAME_MATRIX.to_vec())),
                        0x0200_523d if n >= FRAME_RED.len() => {
                            let mut b = vec![0u8; n];
                            b[..FRAME_RED.len()].copy_from_slice(&FRAME_RED);
                            Some((2, b))
                        }
                        _ => None,
                    };
                    if let Some((i, b)) = value {
                        found[i] = true;
                        out.push((at + v as u64, b));
                    }
                }
                q += l;
            }
        }
        p += len;
    }
    if found != [true; 3] {
        return Err(Error("frame header lacks the tags R3D NE needs (not a ZR N-RAW clip?)".into()));
    }
    Ok(())
}

/// Checks that a clip can be converted and returns the moov patches (relative to moov) and
/// the ISO to write.
/// In place patches inside moov: (offset, bytes).
type MoovPatches = Vec<(usize, Vec<u8>)>;

fn plan(m: &Movie, iso_factor: f64) -> Result<(MoovPatches, u32)> {
    if m.video().codec == fourcc(b"NR3D") {
        return Err(Error("already an R3D NE clip".into()));
    }
    if m.video().codec != fourcc(b"NRAW") {
        return Err(Error("not an N-RAW clip".into()));
    }
    let tags = nctg_tags(m);
    let get = |id: u32| tag_value(m, &tags, id);
    let u16v = |id: u32| get(id).filter(|v| v.len() == 2).map(|v| u16::from_be_bytes([v[0], v[1]]));
    let model = get(0x2).map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').to_string()).unwrap_or_default();
    if model != "NIKON ZR" {
        let shown = if model.is_empty() { "unknown camera".to_string() } else { model };
        return Err(Error(format!("R3D NE conversion supports Nikon ZR clips only ({shown})")));
    }
    match u16v(0x24) {
        Some(1) => {}
        Some(2) => return Err(Error(NORMAL_UNSUPPORTED.into())),
        q => return Err(Error(format!("unknown N-RAW quality {q:?}"))),
    }
    let iso = get(0x0110_8832).filter(|v| v.len() == 4).map(|v| rd32(v, 0)).unwrap_or(0);
    if iso == 0 {
        return Err(Error("clip has no ISO tag".into()));
    }
    let tagged_iso = (iso as f64 * iso_factor).round().clamp(1.0, 65535.0) as u32;
    let name = |id: u32, want: usize| -> Result<&Tag> {
        tags.iter()
            .find(|t| t.id == id && t.len == want)
            .ok_or_else(|| Error(format!("clip lacks NCTG tag {id:#x} of {want} bytes")))
    };
    let mut p: MoovPatches = Vec::new();
    for (id, v) in [(0x21u32, 65u16), (0x26, 4), (0x27, 3)] {
        p.push((name(id, 2)?.value, v.to_be_bytes().to_vec()));
    }
    p.push((name(0x1020, 12)?.value, red_exposure_block(tagged_iso).to_vec()));
    let g = name(0x0200_0023, 108)?;
    p.push((g.value, gamma_block(&m.moov[g.value..g.value + g.len])?));
    p.push((name(0x0200_00c0, 8)?.value, vec![0x48, 0x01, 0x0c, 0x00, 0x78, 0x01, 0x0c, 0x00]));
    // 0x523d is filled in once the output name is known (see convert).
    name(0x523d, 256)?;
    p.push((m.video().stsd_entry_offset + 4, b"NR3D".to_vec()));
    Ok((p, tagged_iso))
}

/// Checks that `input` can be converted to R3D NE without writing anything.
pub fn check(input: &Path) -> Result<()> {
    let f = InputFile::open(input)?;
    plan(&Movie::parse(&f)?, ISO_FACTOR).map(|_| ())
}

/// Converts an N-RAW High ZR clip to R3D NE at `o.output` (written to `<output>.partial`
/// and renamed when complete). `progress(fraction, mb_per_s)`; `cancel` stops it and removes
/// the partial file.
pub fn convert(o: &R3dOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<R3dOutcome> {
    let start = Instant::now();
    let input = InputFile::open(&o.input)?;
    let m = Movie::parse(&input)?;
    let (moov_patches, iso) = plan(&m, o.iso_factor)?;
    let moov_at = m.top.iter().find(|b| b.kind == fourcc(b"moov")).map(|b| b.offset).unwrap_or(0);
    let mut patches: Vec<(u64, Vec<u8>)> = moov_patches.into_iter().map(|(at, b)| (moov_at + at as u64, b)).collect();
    let stem = o.output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tags = nctg_tags(&m);
    let red = tags.iter().find(|t| t.id == 0x523d).unwrap();
    patches.push((moov_at + red.value as u64, red_clip_block(&stem)));
    let v = m.video();
    for s in &v.samples {
        let n = (s.size as usize).min(4096);
        let h = input.read(s.offset, n)?;
        frame_patches(&h, s.offset, &mut patches)?;
    }
    patches.sort_by_key(|p| p.0);

    if let Some(dir) = o.output.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error(format!("cannot create {}: {e}", dir.display())))?;
    }
    let mut partial = o.output.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let result = (|| -> Result<u64> {
        let mut src = File::open(&o.input).map_err(|e| Error(format!("cannot open {}: {e}", o.input.display())))?;
        let mut dst = BufWriter::with_capacity(
            1 << 20,
            File::create(&partial).map_err(|e| Error(format!("cannot create {}: {e}", partial.display())))?,
        );
        let total = input.size();
        let mut buf = vec![0u8; 8 << 20];
        let mut at = 0u64;
        let mut next = 0usize;
        let mut last = Instant::now();
        src.seek(SeekFrom::Start(0)).map_err(|e| Error(e.to_string()))?;
        while at < total {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error("cancelled".into()));
            }
            let n = ((total - at) as usize).min(buf.len());
            src.read_exact(&mut buf[..n]).map_err(|e| Error(format!("read error: {e}")))?;
            let end = at + n as u64;
            while next < patches.len() && patches[next].0 < end {
                let (pos, ref b) = patches[next];
                for (i, &x) in b.iter().enumerate() {
                    let q = pos + i as u64;
                    if q >= at && q < end {
                        buf[(q - at) as usize] = x;
                    }
                }
                if pos + b.len() as u64 > end {
                    break; // spills into the next block; applied again there
                }
                next += 1;
            }
            dst.write_all(&buf[..n]).map_err(|e| Error(format!("write error: {e}")))?;
            at = end;
            if last.elapsed().as_millis() >= 100 || at == total {
                last = Instant::now();
                let secs = start.elapsed().as_secs_f64().max(1e-3);
                progress(at as f64 / total as f64, at as f64 / 1e6 / secs);
            }
        }
        dst.flush().map_err(|e| Error(format!("write error: {e}")))?;
        Ok(total)
    })();
    match result {
        Ok(bytes) => {
            std::fs::rename(&partial, &o.output)
                .map_err(|e| Error(format!("cannot move output into place at {}: {e}", o.output.display())))?;
            Ok(R3dOutcome {
                output: o.output.clone(),
                frames: v.samples.len() as u32,
                output_bytes: bytes,
                seconds: start.elapsed().as_secs_f64(),
                iso,
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// True for a `.nev` file (any case): the only clips the R3D toggle acts on.
pub fn is_nev(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("nev"))
}

/// What the R3D NE step of a job did.
#[derive(Debug, Clone)]
pub enum R3dJob {
    Done(R3dOutcome),
    /// Not converted, with the reason (not a .nev, N-RAW Normal, not a ZR clip, ...).
    Skipped(String),
}

/// The R3D NE step of a queue job: converts the original `source` (never the compressed
/// copy, so the codestream stays the camera's N-RAW High data) to [`r3d_path`] next to the
/// job's output `job_output`. Clips that cannot become R3D NE are skipped, not failed.
pub fn convert_job(
    source: &Path,
    job_output: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<R3dJob> {
    if !is_nev(source) {
        return Ok(R3dJob::Skipped("not a .nev clip".into()));
    }
    if let Err(e) = check(source) {
        return Ok(R3dJob::Skipped(e.0));
    }
    convert(&R3dOptions::new(source, &r3d_path(job_output, source)), cancel, progress).map(R3dJob::Done)
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
    fn frame_matrix_is_the_r3d_one() {
        let f: Vec<f32> = FRAME_MATRIX[1..].chunks(4).map(|c| f32::from_be_bytes(c.try_into().unwrap())).collect();
        // Rows sum to the D65 white point in XYZ.
        let sums: Vec<f32> = f.chunks(3).map(|r| r.iter().sum()).collect();
        assert!((sums[0] - 0.9505).abs() < 2e-3 && (sums[1] - 1.0).abs() < 2e-3 && (sums[2] - 1.089).abs() < 2e-3);
    }
}
