//! Quality presets and output geometry.

use crate::bmff::{InputFile, Movie};
use crate::Resolution;

/// Target ranks, measured on Z8 and ZR footage (rank = Q*25 + 25-R,
/// higher is coarser; the Z8 records around rank 139, the ZR around 117 to 130):
/// - rank 135: 84% (Z8) / 75% (ZR) of the size, mean error ~3/16383 raw, below sensor noise
/// - rank 140: 67% / 58%, shadow noise slightly smoothed
/// - rank 160: 43% / 43%, visible smoothing in deep shadows on the Z8
///
/// Precincts the camera already coded coarser than the target are left untouched.
/// Returns None for the lossless repack.
pub fn rank_for_quality(quality: u8) -> Option<i32> {
    match quality {
        0 | 1 => None,
        2 => Some(135),
        3 => Some(140),
        _ => Some(160),
    }
}

/// Range of the bits per pixel setting.
pub const BPP_MIN: f64 = 0.5;
pub const BPP_MAX: f64 = 8.0;

/// What the legacy quality steps 2, 3 and 4 mean now: these fractions of each clip's own
/// bits per pixel, set to the size each pick is meant to give: Light about a quarter
/// smaller (20 to 30%) with minimal quality loss, Medium about half the size (40 to 50%)
/// for archive copies, Strong about two thirds smaller (60 to 70%) for reference copies.
/// Every frame is rate controlled to its target, so these hold on any footage unless the
/// 0.5 bpp floor comes first. `bluecompress calibrate` compares them with the old fixed
/// Q2 to Q4 ranks on real clips. Quality 1 is the lossless repack.
pub const LEGACY_FRACTIONS: [f64; 3] = [0.75, 0.55, 0.35];

/// The bpp a legacy quality step stands for on a clip of `source_bpp`: None for quality 1
/// (lossless), else the step's fraction of the source rounded to 0.1, at least
/// [`BPP_MIN`].
pub fn legacy_bpp(quality: u8, source_bpp: f64) -> Option<f64> {
    if quality <= 1 {
        return None;
    }
    let f = LEGACY_FRACTIONS[(quality.min(4) - 2) as usize];
    Some(((f * source_bpp * 10.0).round() / 10.0).clamp(BPP_MIN, BPP_MAX))
}

/// What a job compresses to: the lossless repack, a target size in bits per pixel (bits
/// per Bayer sample, kept in tenths so it can be compared and used as a key), or a legacy
/// quality step (2 to 4), which [`Target::resolve`] turns into a bpp for each clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Quality(u8),
    /// Tenths of a bit per sample.
    Bpp(u16),
}

impl Target {
    /// A preset, clamped to 1 to 4.
    pub fn quality(q: u8) -> Target {
        Target::Quality(q.clamp(1, 4))
    }

    /// A bits per pixel target rounded to 0.1 and clamped to [`BPP_MIN`, `BPP_MAX`].
    pub fn bpp(v: f64) -> Target {
        let v = if v.is_finite() { v } else { BPP_MAX };
        Target::Bpp((v.clamp(BPP_MIN, BPP_MAX) * 10.0).round() as u16)
    }

    /// The lossless repack.
    pub fn lossless() -> Target {
        Target::Quality(1)
    }

    pub fn is_lossless(&self) -> bool {
        matches!(*self, Target::Quality(q) if q <= 1)
    }

    /// The bpp target when one is given, else the quality preset.
    pub fn new(quality: u8, bpp: Option<f64>) -> Target {
        bpp.map_or_else(|| Target::quality(quality), Target::bpp)
    }

    /// A target from the fields of a request: `lossless`, else `bpp`, else the legacy
    /// `quality` step (2 when none is given).
    pub fn from_fields(quality: Option<u8>, bpp: Option<f64>, lossless: bool) -> Target {
        if lossless {
            Target::lossless()
        } else {
            Target::new(quality.unwrap_or(2), bpp)
        }
    }

    /// The `quality` and `bpp` fields of [`crate::Options`] for this target.
    pub fn option_fields(&self) -> (u8, Option<f64>) {
        match *self {
            Target::Quality(q) => (q, None),
            Target::Bpp(_) => (1, self.bpp_value()),
        }
    }

    /// A legacy quality step as the bpp it stands for on a clip of `source_bpp` (see
    /// [`legacy_bpp`]); other targets as they are.
    pub fn resolve_legacy(self, source_bpp: f64) -> Target {
        match self {
            Target::Quality(q) if q > 1 => legacy_bpp(q, source_bpp).map_or(Target::lossless(), Target::bpp),
            t => t,
        }
    }

    /// What a job really codes on a clip of `source_bpp`: legacy steps resolved, and a bpp
    /// at or above the source's the lossless repack (it cannot add anything).
    pub fn resolve(self, source_bpp: f64) -> Target {
        match self.resolve_legacy(source_bpp) {
            Target::Bpp(t) if t as f64 / 10.0 >= source_bpp => Target::lossless(),
            t => t,
        }
    }

    pub fn bpp_value(&self) -> Option<f64> {
        match *self {
            Target::Bpp(t) => Some(t as f64 / 10.0),
            Target::Quality(_) => None,
        }
    }

    /// Codestream byte budget of one `width` x `height` frame (bpp mode).
    pub fn budget(&self, width: u32, height: u32) -> Option<usize> {
        match *self {
            Target::Bpp(t) => Some((t as u64 * width as u64 * height as u64 / 80) as usize),
            Target::Quality(_) => None,
        }
    }

    /// [`Target::budget`] for a frame as it is written: codestreams go into the file zero
    /// padded to whole KiB ([`crate::nraw::CODESTREAM_ALIGN`]), so the budget is rounded down
    /// to that unit and the padded codestream still fits the target.
    pub fn frame_budget(&self, width: u32, height: u32) -> Option<usize> {
        const A: usize = crate::nraw::CODESTREAM_ALIGN;
        self.budget(width, height).map(|b| if b >= A { b / A * A } else { b })
    }

    /// The quality preset (1 when in bpp mode).
    pub fn preset(&self) -> u8 {
        match *self {
            Target::Quality(q) => q,
            Target::Bpp(_) => 1,
        }
    }
}

/// Bits per pixel (per Bayer sample) of `bytes` per frame of `width` x `height`.
pub fn bpp_of(bytes: f64, width: u32, height: u32) -> f64 {
    let n = width as f64 * height as f64;
    if n > 0.0 {
        8.0 * bytes / n
    } else {
        0.0
    }
}

/// Folder name form of a bpp value: "1.2bpp", "3bpp".
pub fn bpp_tag(bpp: f64) -> String {
    let t = (bpp * 10.0).round() as i64;
    if t % 10 == 0 {
        format!("{}bpp", t / 10)
    } else {
        format!("{}.{}bpp", t / 10, t % 10)
    }
}

/// Finest rank the resize re-encoder uses (the Q1 truncation for resized output).
pub const RESIZE_FINEST_RANK: i32 = 125;

/// Rank used by the re-encoder for resized output. Resized output has no camera coding to
/// preserve; Q1 uses a fine truncation that is transparent to the resampled signal.
pub fn encode_rank_for_quality(quality: u8) -> i32 {
    if let Some(r) = std::env::var("BC_ENCODE_RANK").ok().and_then(|v| v.parse().ok()) {
        return r; // calibration only
    }
    rank_for_quality(quality).unwrap_or(RESIZE_FINEST_RANK)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Geometry {
    /// Codestream (Bayer) size.
    pub width: u32,
    pub height: u32,
    /// Image area the decoder shows.
    pub active_width: u32,
    pub active_height: u32,
}

/// Bytes of TICO codestream in a clip's video frames, the numerator of its bits per pixel
/// (see [`crate::nraw::codestream_bytes`]), and whether it is exact. When the frame headers
/// cannot be read the whole video samples (headers included) are counted instead, and a
/// note says so on stderr.
pub fn source_codestream_bytes(input: &InputFile, m: &Movie) -> (u64, bool) {
    match crate::nraw::codestream_bytes(input, m) {
        Ok(b) => (b, true),
        Err(e) => {
            let bytes: u64 = m.video().samples.iter().map(|s| s.size as u64).sum();
            eprintln!(
                "bluecompress: {}: frame headers not readable ({e}); bits per pixel estimated from whole video samples",
                input.path().display()
            );
            (bytes, false)
        }
    }
}

/// Average bits per pixel of a clip's video frames: 8 x codestream bytes / (frames x Wf x
/// Hf), as in the HE / HE* reference (container, audio, metadata and frame headers left
/// out).
pub fn source_bpp(input: &InputFile, m: &Movie) -> f64 {
    let v = m.video();
    let (bytes, _) = source_codestream_bytes(input, m);
    bpp_of(bytes as f64 / v.samples.len().max(1) as f64, v.width, v.height)
}

pub fn source_geometry(m: &Movie) -> Geometry {
    let v = m.video();
    let (w, h) = (v.width, v.height);
    Geometry {
        width: w,
        height: h,
        active_width: if m.active_width != 0 {
            m.active_width
        } else if w > 12 {
            w - 12
        } else {
            w
        },
        active_height: if m.active_height != 0 {
            m.active_height
        } else if h > 8 {
            h - 8
        } else {
            h
        },
    }
}

/// Output geometry for a resolution preset. Presets set the active area (6K: 6048x3402,
/// 4K: 3840x2160 UHD) and keep the source's margin; the source geometry is returned when
/// the preset is not smaller than the source.
pub fn target_geometry(src: Geometry, resolution: Resolution) -> Geometry {
    let (aw, ah) = match resolution {
        Resolution::K6 => (6048, 3402),
        Resolution::K4 => (3840, 2160),
        Resolution::Original => return src,
    };
    if src.active_width <= aw {
        return src;
    }
    Geometry {
        active_width: aw,
        active_height: ah,
        width: aw + (src.width - src.active_width),
        height: ah + (src.height - src.active_height),
    }
}

pub fn video_frame_rate(m: &Movie) -> f64 {
    let v = m.video();
    if v.timescale == 0 || v.sample_delta == 0 {
        0.0
    } else {
        v.timescale as f64 / v.sample_delta as f64
    }
}
