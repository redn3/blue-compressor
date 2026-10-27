//! Size calibration on real clips (a developer tool behind the hidden `bluecompress
//! calibrate` command): codes sample frames of a clip with the fixed truncation ranks the
//! old quality steps used (Q2 rank 135, Q3 rank 140, Q4 rank 160, as in v0.1.6) and with
//! the Light, Medium and Strong bits per pixel picks, and reports the sizes, so the picks
//! can be matched to the old steps on footage instead of on paper.
//!
//! Only the container index, a few frame headers and the sample frames are read, so it
//! runs over a network share without reading whole clips.
//!
//! Sizes are TICO codestream bytes as they are written (padded to whole KiB), the same
//! count a clip's own bits per pixel uses, so every number compares with the source's.

use std::path::Path;

use crate::bmff::{InputFile, Movie};
use crate::nraw::{NrawFrame, CODESTREAM_ALIGN};
use crate::quality::{rank_for_quality, Target};
use crate::requant::{requant_codestream_seeded, RequantParams};
use crate::{bpp_of, probe, ClipInfo, Result};

/// Frames per rate controlled run, as in `transcode` (each run starts its search afresh).
const RUN: usize = 4;

/// One way of coding the sample frames.
#[derive(Debug, Clone)]
pub struct Variant {
    /// "Lossless", "Q2 rank 135", "Light 2.4bpp", ...
    pub label: String,
    /// The fixed truncation rank (old quality steps), if any.
    pub rank: Option<i32>,
    /// The bpp target the pick resolved to on this clip (None: lossless repack).
    pub bpp_target: Option<f64>,
    /// Output codestream bytes of the sample frames, padded as written.
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Calibration {
    pub info: ClipInfo,
    /// Indices of the frames coded.
    pub frames: Vec<usize>,
    /// Source codestream bytes of those frames (camera padding included, like Lcod).
    pub source_bytes: u64,
    /// Mean truncation rank the camera coded the sample precincts with.
    pub camera_rank: f64,
    pub variants: Vec<Variant>,
}

impl Calibration {
    /// Bits per pixel of `bytes` over the sample frames.
    pub fn bpp(&self, bytes: u64) -> f64 {
        bpp_of(bytes as f64 / self.frames.len().max(1) as f64, self.info.width, self.info.height)
    }

    pub fn source_bpp(&self) -> f64 {
        self.bpp(self.source_bytes)
    }
}

/// Frames to sample from a clip of `total`: the first `n`, or with `spread` runs of four
/// consecutive frames spaced evenly over the clip (`n` rounded up to whole runs).
pub fn sample_frames(total: usize, n: usize, spread: bool) -> Vec<usize> {
    let n = n.clamp(1, total.max(1)).min(total);
    if !spread || n >= total {
        return (0..n).collect();
    }
    let runs = n.div_ceil(RUN);
    let slots = total / RUN;
    if runs >= slots {
        return (0..n).collect();
    }
    let mut v = Vec::with_capacity(runs * RUN);
    for k in 0..runs {
        let slot = if runs == 1 { 0 } else { k * (slots - 1) / (runs - 1) };
        v.extend((slot * RUN..(slot * RUN + RUN).min(total)).collect::<Vec<_>>());
    }
    v
}

fn padded(n: usize) -> u64 {
    (n.div_ceil(CODESTREAM_ALIGN) * CODESTREAM_ALIGN) as u64
}

/// The ways a calibration codes each frame, for a clip of `clip_bpp`.
fn plans(clip_bpp: f64) -> Vec<(String, Option<i32>, Target)> {
    let mut v = vec![("Lossless".to_string(), None, Target::lossless())];
    for q in 2..=4u8 {
        let r = rank_for_quality(q);
        v.push((format!("Q{q} rank {}", r.unwrap_or_default()), r, Target::lossless()));
    }
    for (q, name) in [(2u8, "Light"), (3, "Medium"), (4, "Strong")] {
        let t = Target::quality(q).resolve(clip_bpp);
        let label = match t.bpp_value() {
            Some(b) => format!("{name} {}", crate::bpp_tag(b)),
            None => format!("{name} (lossless)"),
        };
        v.push((label, None, t));
    }
    v
}

/// Calibration of the codestreams `frames` of a `width` x `height` clip whose whole-clip
/// rate is `clip_bpp`. Each item is a frame's codestream and whether a new rate controlled
/// run starts there. Returns the source bytes, the camera's mean rank and the variants.
pub fn calibrate_codestreams(
    frames: impl IntoIterator<Item = Result<(Vec<u8>, bool)>>,
    width: u32,
    height: u32,
    clip_bpp: f64,
    threads: usize,
) -> Result<(u64, f64, Vec<Variant>)> {
    let plans = plans(clip_bpp);
    let mut bytes = vec![0u64; plans.len()];
    let mut seeds: Vec<Vec<i32>> = vec![Vec::new(); plans.len()];
    let (mut source, mut rank_sum, mut precincts) = (0u64, 0i64, 0u64);
    for f in frames {
        let (cs, new_run) = f?;
        source += cs.len() as u64;
        for (i, (_, rank, target)) in plans.iter().enumerate() {
            if new_run {
                seeds[i].clear();
            }
            let prm =
                RequantParams { target_rank: *rank, budget: target.frame_budget(width, height), ..Default::default() };
            let (out, st) = requant_codestream_seeded(&cs, &prm, threads, &mut seeds[i])?;
            bytes[i] += padded(out.len());
            if i == 0 {
                rank_sum += st.rank_sum;
                precincts += st.precincts;
            }
        }
    }
    let variants = plans
        .into_iter()
        .zip(bytes)
        .map(|((label, rank, target), bytes)| Variant { label, rank, bpp_target: target.bpp_value(), bytes })
        .collect();
    Ok((source, rank_sum as f64 / precincts.max(1) as f64, variants))
}

/// Calibrates on `n` frames of the clip at `path` (see [`sample_frames`]); `threads` 0 is
/// all cores.
pub fn calibrate(path: &Path, n: usize, spread: bool, threads: usize) -> Result<Calibration> {
    let info = probe(path)?;
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    let clip_bpp = info.bpp;
    let frames = sample_frames(video.samples.len(), n, spread);
    let threads = if threads > 0 { threads } else { crate::transcode::hardware_threads() };
    let mut prev: Option<usize> = None;
    let iter = frames.iter().map(|&i| {
        // A run starts every RUN frames of the clip, and wherever the sample skips ahead.
        let new_run = i % RUN == 0 || prev.is_none_or(|p| p + 1 != i);
        prev = Some(i);
        let s = video.samples[i];
        let sample = input.read(s.offset, s.size as usize)?;
        let f = NrawFrame::parse(&sample)?;
        Ok((sample[f.codestream_offset..].to_vec(), new_run))
    });
    let (source_bytes, camera_rank, variants) =
        calibrate_codestreams(iter, video.width, video.height, clip_bpp, threads)?;
    Ok(Calibration { info, frames, source_bytes, camera_rank, variants })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_the_first_frames_or_runs_spread_over_the_clip() {
        assert_eq!(sample_frames(100, 6, false), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(sample_frames(3, 24, false), vec![0, 1, 2]);
        assert_eq!(sample_frames(100, 8, true), vec![0, 1, 2, 3, 96, 97, 98, 99]);
        assert_eq!(sample_frames(100, 12, true), vec![0, 1, 2, 3, 48, 49, 50, 51, 96, 97, 98, 99]);
        assert_eq!(sample_frames(10, 24, true), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn plans_cover_the_old_steps_and_the_picks() {
        let p = plans(3.0);
        let labels: Vec<&str> = p.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(labels[..4], ["Lossless", "Q2 rank 135", "Q3 rank 140", "Q4 rank 160"]);
        assert!(
            labels[4].starts_with("Light ") && labels[5].starts_with("Medium ") && labels[6].starts_with("Strong ")
        );
        assert_eq!(p[1].1, Some(135));
        assert_eq!(p[3].1, Some(160));
    }
}
