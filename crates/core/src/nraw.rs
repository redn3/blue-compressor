//! Nikon per-frame sample wrapper shared by N-RAW (.NEV) and R3D NE (.R3D):
//!
//! ```text
//! NRAW box
//!   NRFM  "0100"
//!   NRFH  NRHM "0100" + NRMT tag boxes (width 0x0100, height 0x0101, EXIF-like
//!         exposure tags, CFA pattern, Nikon maker note IFD, ...)
//!   NRTH  NRTI "0100" + u32 codestream length, NRTA (reserved, zero)
//!   <TICO RAW codestream: SOC ... EOC>
//! ```

use crate::bmff::{fourcc, InputFile, Movie, Sample};
use crate::{Error, Result};

/// Cameras write codestreams in whole 1 KiB units, zero padded before EOC; outputs keep that
/// layout so they are structured like camera files.
pub const CODESTREAM_ALIGN: usize = 1024;

#[inline]
fn rd32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

fn require(c: bool, what: &str) -> Result<()> {
    if c {
        Ok(())
    } else {
        Err(Error(what.into()))
    }
}

#[derive(Debug, Clone, Default)]
pub struct NrawFrame {
    /// Bytes before the codestream.
    pub header: Vec<u8>,
    /// Offset of the codestream inside the sample.
    pub codestream_offset: usize,
    pub codestream_size: usize,
    /// Offset of NRTI's u32 length inside header.
    pub nrti_offset: Option<usize>,
    /// NRMT width/height tag value offsets inside header.
    pub width_tag: Option<usize>,
    pub height_tag: Option<usize>,
}

impl NrawFrame {
    pub fn parse(s: &[u8]) -> Result<NrawFrame> {
        let size = s.len();
        require(size >= 16 && rd32(s, 4) == fourcc(b"NRAW"), "video sample is not an NRAW frame")?;
        require(rd32(s, 0) as usize == size, "NRAW frame size mismatch")?;
        let mut f = NrawFrame::default();
        let mut at = 8;
        while at + 8 <= size {
            if s[at] == 0xff && s[at + 1] == 0x10 {
                break; // SOC
            }
            let len = rd32(s, at) as usize;
            let kind = rd32(s, at + 4);
            require(len >= 8 && at + len <= size, "bad NRAW child box")?;
            if kind == fourcc(b"NRFH") {
                // NRHM followed by NRMT tag boxes.
                let mut p = at + 8;
                while p + 8 <= at + len {
                    let l = rd32(s, p) as usize;
                    require(l >= 8 && p + l <= at + len, "bad NRFH child")?;
                    if rd32(s, p + 4) == fourcc(b"NRMT") && l >= 16 {
                        let group = u16::from_be_bytes([s[p + 8], s[p + 9]]);
                        let tag = u16::from_be_bytes([s[p + 10], s[p + 11]]);
                        if group == 0x0110 && tag == 0x0100 {
                            f.width_tag = Some(p + l - 4);
                        }
                        if group == 0x0110 && tag == 0x0101 {
                            f.height_tag = Some(p + l - 4);
                        }
                    }
                    p += l;
                }
            } else if kind == fourcc(b"NRTH") {
                let mut p = at + 8;
                while p + 8 <= at + len {
                    let l = rd32(s, p) as usize;
                    require(l >= 8 && p + l <= at + len, "bad NRTH child")?;
                    if rd32(s, p + 4) == fourcc(b"NRTI") && l >= 16 {
                        f.nrti_offset = Some(p + 12);
                    }
                    p += l;
                }
            }
            at += len;
        }
        require(at + 4 <= size && s[at] == 0xff && s[at + 1] == 0x10, "codestream not found in NRAW frame")?;
        f.codestream_offset = at;
        f.codestream_size = size - at;
        f.header = s[..at].to_vec();
        Ok(f)
    }

    /// Builds a new sample around `cs`, zero padding it to whole KiB before EOC (as the
    /// cameras do) and patching the sizes; optionally rewrites the frame size tags.
    pub fn build(&self, cs: &[u8], size: Option<(u32, u32)>) -> Result<Vec<u8>> {
        self.build_padded(cs, size, None)
    }

    /// [`NrawFrame::build`] padding the codestream to exactly `pad_to` bytes when given
    /// (constant bitrate: every frame the same size, like the cameras write). A codestream
    /// longer than `pad_to` is an error, never truncated.
    pub fn build_padded(&self, cs: &[u8], size: Option<(u32, u32)>, pad_to: Option<usize>) -> Result<Vec<u8>> {
        let n = cs.len();
        require(n >= 4 && cs[n - 2] == 0xff && cs[n - 1] == 0x11, "codestream does not end in EOC")?;
        let mut padded = n.div_ceil(CODESTREAM_ALIGN) * CODESTREAM_ALIGN;
        if let Some(t) = pad_to {
            require(n <= t, "codestream over the constant bitrate frame size")?;
            padded = t;
        }
        let mut out = Vec::with_capacity(self.header.len() + padded);
        out.extend_from_slice(&self.header);
        let cs_at = out.len();
        out.extend_from_slice(&cs[..n - 2]);
        out.resize(cs_at + padded - 2, 0);
        out.extend_from_slice(&[0xff, 0x11]);
        // Lcod sits right after the PIH marker and its length field.
        let limit = (cs_at + 256).min(out.len().saturating_sub(8));
        if let Some(p) = (cs_at..limit).find(|&p| out[p] == 0xff && out[p + 1] == 0x12) {
            out[p + 4..p + 8].copy_from_slice(&(padded as u32).to_be_bytes());
        }
        let total = out.len() as u32;
        out[..4].copy_from_slice(&total.to_be_bytes());
        if let Some(o) = self.nrti_offset {
            out[o..o + 4].copy_from_slice(&(padded as u32).to_be_bytes());
        }
        if let Some((w, h)) = size {
            if let Some(o) = self.width_tag {
                out[o..o + 4].copy_from_slice(&w.to_be_bytes());
            }
            if let Some(o) = self.height_tag {
                out[o..o + 4].copy_from_slice(&h.to_be_bytes());
            }
        }
        Ok(out)
    }
}

/// Length of the NRAW frame header (everything before the codestream's SOC) of the video
/// sample `s`, read from the file box by box: a few 8 byte reads, not the frame.
pub fn sample_header_len(input: &InputFile, s: Sample) -> Result<u64> {
    let size = s.size as u64;
    let mut b = [0u8; 8];
    require(size >= 16, "video sample is not an NRAW frame")?;
    input.read_into(s.offset, &mut b)?;
    require(rd32(&b, 4) == fourcc(b"NRAW"), "video sample is not an NRAW frame")?;
    require(rd32(&b, 0) as u64 == size, "NRAW frame size mismatch")?;
    let mut at = 8u64;
    while at + 8 <= size {
        input.read_into(s.offset + at, &mut b)?;
        if b[0] == 0xff && b[1] == 0x10 {
            return Ok(at); // SOC
        }
        let len = rd32(&b, 0) as u64;
        require(len >= 8 && at + len <= size, "bad NRAW child box")?;
        at += len;
    }
    Err(Error("codestream not found in NRAW frame".into()))
}

/// Frames whose headers [`codestream_bytes`] reads before it takes the header length as
/// the same for the whole clip.
const HEADER_PROBES: usize = 8;

/// Total TICO codestream bytes of a clip's video frames: the frame sizes from the sample
/// table less each frame's NRAW header. Header lengths are read from up to
/// [`HEADER_PROBES`] frames spread over the clip (first and last included); when they all
/// agree, as they do in camera files, that length is used for every frame, otherwise every
/// frame's header is read. Codestream padding (cameras write whole KiB) counts as
/// codestream, as Lcod does.
pub fn codestream_bytes(input: &InputFile, m: &Movie) -> Result<u64> {
    let samples = &m.video().samples;
    let n = samples.len();
    let total: u64 = samples.iter().map(|s| s.size as u64).sum();
    if n == 0 {
        return Ok(0);
    }
    let k = n.min(HEADER_PROBES);
    let mut lens = Vec::with_capacity(k);
    for i in 0..k {
        let at = if k == 1 { 0 } else { i * (n - 1) / (k - 1) };
        lens.push(sample_header_len(input, samples[at])?);
    }
    if lens.iter().all(|&l| l == lens[0]) {
        return Ok(total - lens[0] * n as u64);
    }
    let mut headers = 0u64;
    for &s in samples {
        headers += sample_header_len(input, s)?;
    }
    Ok(total - headers)
}
