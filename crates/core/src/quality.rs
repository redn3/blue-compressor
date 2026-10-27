//! Quality presets and output geometry.

use crate::bmff::Movie;
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

/// Range of the bits per sample setting.
pub const BPP_MIN: f64 = 0.5;
pub const BPP_MAX: f64 = 8.0;

/// What a job compresses to: a quality preset (1 to 4) or a target size in bits per Bayer
/// sample, kept in tenths (0.1 resolution) so it can be compared and used as a key.
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

    /// A bits per sample target rounded to 0.1 and clamped to [`BPP_MIN`, `BPP_MAX`].
    pub fn bpp(v: f64) -> Target {
        let v = if v.is_finite() { v } else { BPP_MAX };
        Target::Bpp((v.clamp(BPP_MIN, BPP_MAX) * 10.0).round() as u16)
    }

    /// The bpp target when one is given, else the quality preset.
    pub fn new(quality: u8, bpp: Option<f64>) -> Target {
        bpp.map_or_else(|| Target::quality(quality), Target::bpp)
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

    /// The quality preset (1 when in bpp mode).
    pub fn preset(&self) -> u8 {
        match *self {
            Target::Quality(q) => q,
            Target::Bpp(_) => 1,
        }
    }
}

/// Bits per Bayer sample of `bytes` per frame of `width` x `height`.
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
