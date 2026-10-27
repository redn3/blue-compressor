//! Blue Compressor core: lossless repacking, requantization and raw resizing of Nikon
//! N-RAW (.NEV) and R3D NE (.R3D) TICO RAW clips, keeping container, metadata and audio.
//!
//! The public surface is small: [`probe`], [`transcode`], [`output_size`] and
//! [`backend_description`]. A C API with the same shape lives in [`ffi`]
//! (header: `crates/core/include/bluecompress.h`).

// Codec loops index several parallel arrays by the same position; iterators would hide that.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

mod bitio;
pub mod bmff;
pub mod ffi;
pub mod frame;
mod gpu;
pub mod nraw;
mod preview;
pub mod quality;
mod rate;
pub mod requant;
pub mod resample;
pub mod resize;
pub mod tico;
mod transcode;
mod util;

pub use preview::{Preview, Thumbnail};
pub use quality::{bpp_of, bpp_tag, legacy_bpp, Target, BPP_MAX, BPP_MIN, LEGACY_FRACTIONS};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Video codec of a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Nikon N-RAW (.NEV), e.g. Z8, Z9.
    NRaw,
    /// R3D NE (.R3D), e.g. Nikon ZR.
    R3dNe,
}

#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub codec: Codec,
    /// Coded Bayer size.
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub fps: f64,
    pub file_bytes: u64,
    pub video_bytes: u64,
    pub audio_tracks: u32,
    /// Average coded bits per Bayer sample of the video frames (8 * bytes / (width * height)).
    pub bpp: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Original,
    /// 6048 x 3402 active (6060 x 3410 coded), only from larger sources.
    K6,
    /// 3840 x 2160 active UHD (3852 x 2168 coded).
    K4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Cpu,
    Metal,
    Cuda,
    /// OpenCL (AMD and other GPUs; `opencl` feature).
    OpenCl,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 1 lossless repack, 2 archival, 3 high, 4 compact.
    pub quality: u8,
    /// Target bits per Bayer sample (0.5 to 8, 0.1 steps) instead of a quality preset: each
    /// frame is rate controlled to fit. A target at or above the source's gives the
    /// lossless repack.
    pub bpp: Option<f64>,
    pub resolution: Resolution,
    pub backend: Backend,
    /// 0 = all cores.
    pub threads: usize,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub frames: u32,
    pub seconds: f64,
    pub fps: f64,
    pub backend: String,
}

/// Reads a clip's container.
pub fn probe(path: &std::path::Path) -> Result<ClipInfo> {
    let input = bmff::InputFile::open(path)?;
    let m = bmff::Movie::parse(&input)?;
    let v = m.video();
    let codec = match v.codec {
        c if c == bmff::fourcc(b"NRAW") => Codec::NRaw,
        c if c == bmff::fourcc(b"NR3D") => Codec::R3dNe,
        c => return Err(Error(format!("not a Nikon N-RAW or R3D NE clip (codec {})", bmff::fourcc_str(c)))),
    };
    let video_bytes: u64 = v.samples.iter().map(|s| s.size as u64).sum();
    let frames = v.samples.len() as u32;
    Ok(ClipInfo {
        codec,
        width: v.width,
        height: v.height,
        frames,
        fps: quality::video_frame_rate(&m),
        file_bytes: input.size(),
        video_bytes,
        audio_tracks: m.tracks.iter().filter(|t| t.handler == bmff::fourcc(b"soun")).count() as u32,
        bpp: bpp_of(video_bytes as f64 / frames.max(1) as f64, v.width, v.height),
    })
}

/// Output Bayer size for a source size and requested resolution (the source's camera
/// margin of 12 x 8 is kept).
pub fn output_size(width: u32, height: u32, resolution: Resolution) -> (u32, u32) {
    let src = quality::Geometry {
        width,
        height,
        active_width: if width > 12 { width - 12 } else { width },
        active_height: if height > 8 { height - 8 } else { height },
    };
    let g = quality::target_geometry(src, resolution);
    (g.width, g.height)
}

/// Human readable description of a backend ("Metal: Apple GPU", "CPU (12 threads)").
pub fn backend_description(backend: Backend) -> String {
    match backend {
        Backend::Cpu => format!("CPU ({} threads)", transcode::hardware_threads()),
        b => gpu::description(b).unwrap_or_else(|| match b {
            Backend::Metal => "not available (Metal needs macOS)".into(),
            Backend::Cuda => "not available (needs the cuda feature, an NVIDIA GPU and driver, and NVRTC 12.x)".into(),
            Backend::OpenCl => {
                "not available (built without the opencl feature, or no OpenCL GPU with double precision)".into()
            }
            _ => format!("CPU only ({} threads)", transcode::hardware_threads()),
        }),
    }
}

/// Transcodes one clip. `progress(fraction, fps)` is called from the calling thread about
/// ten times a second; setting `cancel` stops the job and removes the partial output.
/// The output is written to `<output>.partial` and renamed when complete.
pub fn transcode(options: &Options, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<Outcome> {
    transcode::transcode(options, cancel, progress)
}

/// Path of the staging file a queue writes a job to before renaming it to `target`:
/// `<target>.bluecompress.tmp`. The core writer adds its own `.partial` suffix while writing.
pub fn staging_path(target: &std::path::Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bluecompress.tmp");
    PathBuf::from(s)
}

/// Removes leftovers of an interrupted run for `target` (for example after a crash or a
/// container restart): `<target>.bluecompress.tmp`, its `.partial`, and `<target>.partial`.
pub fn remove_stale_staging(target: &std::path::Path) {
    let staging = staging_path(target);
    let mut partial = staging.as_os_str().to_owned();
    partial.push(".partial");
    let mut direct = target.as_os_str().to_owned();
    direct.push(".partial");
    for p in [staging, PathBuf::from(partial), PathBuf::from(direct)] {
        let _ = std::fs::remove_file(p);
    }
}

/// Transcodes like [`transcode`], but into [`staging_path`] of `options.output`, renaming it
/// into place only when the whole clip succeeded. On error or cancel the staging file is
/// removed, so an interrupted job never leaves a file under the final name.
pub fn transcode_staged(options: &Options, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<Outcome> {
    let target = options.output.clone();
    remove_stale_staging(&target);
    let staging = staging_path(&target);
    let staged = Options { output: staging.clone(), ..options.clone() };
    match transcode(&staged, cancel, progress) {
        Ok(o) => match std::fs::rename(&staging, &target) {
            Ok(()) => Ok(o),
            Err(e) => {
                let _ = std::fs::remove_file(&staging);
                Err(Error(format!("cannot move output into place at {}: {e}", target.display())))
            }
        },
        Err(e) => {
            remove_stale_staging(&target);
            Err(e)
        }
    }
}

/// Folder tag of a quality step: "Lossless", "Q3 6K" (legacy steps unresolved; see
/// [`named_target`]).
pub fn variant_name(quality: u8, resolution: &str) -> String {
    variant_name_for(Target::quality(quality), resolution)
}

/// A target as it is named for the clip `input`: a legacy quality step 2 to 4 becomes the
/// bpp it stands for on that clip (probing it), so its folder is "Compressed 1.2bpp" like
/// any bpp job's. Other targets as they are.
pub fn named_target(input: &std::path::Path, target: Target) -> Target {
    match target {
        Target::Quality(q) if q > 1 => probe(input).map_or(target, |ci| target.resolve_legacy(ci.bpp)),
        t => t,
    }
}

/// Folder tag of a target, as the queues name output folders: "Lossless", "1.2bpp",
/// "2bpp 4K".
pub fn variant_name_for(target: Target, resolution: &str) -> String {
    let base = match target {
        Target::Quality(q) if q <= 1 => "Lossless".to_string(),
        Target::Quality(q) => format!("Q{q}"),
        Target::Bpp(_) => bpp_tag(target.bpp_value().unwrap_or_default()),
    };
    match resolution {
        "6k" => format!("{base} 6K"),
        "4k" => format!("{base} 4K"),
        _ => base,
    }
}

/// The resolution name a job really runs at: "original" when `resolution` ("original",
/// "6k" or "4k") would not change a `width` x `height` clip, so a 6K setting on a 6K clip
/// is named like no resize.
pub fn effective_resolution(width: u32, height: u32, resolution: &str) -> &'static str {
    let r = match resolution {
        "6k" => Resolution::K6,
        "4k" => Resolution::K4,
        _ => return "original",
    };
    if output_size(width, height, r) == (width, height) {
        "original"
    } else if r == Resolution::K6 {
        "6k"
    } else {
        "4k"
    }
}

/// A finished compressed copy of `input` for this setting (`resolution` as returned by
/// [`effective_resolution`]), if one exists: the places a queue writes it, in order. Next to
/// the clips that is `Compressed <tag>/<name>`; in a chosen `folder` it is `<folder>/<name>`
/// or `<folder>/<tag>/<name>`. Used both to mark clips as already done when they are added
/// and to skip them at run time.
pub fn existing_compressed(
    input: &std::path::Path,
    folder: Option<&str>,
    quality: u8,
    resolution: &str,
) -> Option<(PathBuf, u64)> {
    existing_compressed_for(input, folder, Target::quality(quality), resolution)
}

/// [`existing_compressed`] for any target (bpp outputs live in `Compressed 1.2bpp/`).
pub fn existing_compressed_for(
    input: &std::path::Path,
    folder: Option<&str>,
    target: Target,
    resolution: &str,
) -> Option<(PathBuf, u64)> {
    use std::path::Path;
    let tag = variant_name_for(target, resolution);
    let name = input.file_name()?;
    let candidates: Vec<PathBuf> = match folder {
        Some(f) if !f.is_empty() => vec![Path::new(f).join(name), Path::new(f).join(&tag).join(name)],
        _ => vec![input.parent().unwrap_or(Path::new(".")).join(format!("Compressed {tag}")).join(name)],
    };
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    };
    candidates.into_iter().filter(|p| !same(p, input)).find_map(|p| existing_output(&p).map(|n| (p, n)))
}

/// An existing, non-empty compressed copy at `path`, if there is one. Queues use it to skip
/// clips that an earlier, interrupted run already finished.
pub fn existing_output(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path).ok().filter(|m| m.is_file() && m.len() > 0).map(|m| m.len())
}

/// Renders frame `frame` (clamped) of the clip before and after compression with the given
/// quality and resolution, as a simple debayered preview `width` pixels wide. Both halves
/// go through the same rendering, so differences come from compression alone.
pub fn preview_frame(
    path: &std::path::Path,
    frame: u32,
    quality: u8,
    resolution: Resolution,
    backend: Backend,
    width: u32,
) -> Result<Preview> {
    preview::preview_frame(path, frame, Target::quality(quality), resolution, backend, [0.0, 0.0, 1.0, 1.0], width)
}

/// Like [`preview_frame`] for the region `crop` = [x, y, width, height] in fractions of the
/// frame (for zooming in). The last decoded frame is cached, so panning and zooming around
/// one frame with the same settings costs only the rendering.
pub fn preview_region(
    path: &std::path::Path,
    frame: u32,
    quality: u8,
    resolution: Resolution,
    backend: Backend,
    crop: [f64; 4],
    width: u32,
) -> Result<Preview> {
    preview::preview_frame(path, frame, Target::quality(quality), resolution, backend, crop, width)
}

/// [`preview_region`] for any target, including a bpp target.
pub fn preview_region_for(
    path: &std::path::Path,
    frame: u32,
    target: Target,
    resolution: Resolution,
    backend: Backend,
    crop: [f64; 4],
    width: u32,
) -> Result<Preview> {
    preview::preview_frame(path, frame, target, resolution, backend, crop, width)
}

/// A small picture of frame `frame` (clamped) of a clip, `width` pixels wide, for lists and
/// thumbnails. Decodes one frame; does not disturb the [`preview_region`] cache.
pub fn thumbnail(path: &std::path::Path, frame: u32, width: u32) -> Result<Thumbnail> {
    preview::thumbnail(path, frame, width)
}
