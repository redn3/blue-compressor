//! Before/after preview of one frame: the original and the compressed codestream are both
//! decoded to raw and rendered through the same simple pipeline (quad debayer, black
//! level, gray-world white balance and exposure taken from the original, a little extra
//! saturation in place of a camera matrix, soft tone curve, sRGB), so any visible
//! difference comes from compression alone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::bmff::{InputFile, Movie};
use crate::frame::{decode_frame, inverse_star_tetrix, inverse_wavelet, Coeffs, Planes, Transfer};
use crate::nraw::NrawFrame;
use crate::quality::{rank_for_quality, source_bpp, source_geometry, target_geometry, Target};
use crate::requant::{requant_codestream, RequantParams};
use crate::tico::{Layout, TicoHeader};
use crate::transcode::is_supported_codec;
use crate::{resize, Backend, Error, Resolution, Result};

#[derive(Debug, Clone)]
pub struct Preview {
    /// Display image size (same for before and after).
    pub width: u32,
    pub height: u32,
    /// width * height * 4, sRGB 8-bit RGBA, original frame.
    pub before_rgba: Vec<u8>,
    /// Same size: the frame after compression (scaled to the same display size when resized).
    pub after_rgba: Vec<u8>,
    /// This frame's TICO codestream bytes in the source and after compression (as written,
    /// so KiB padded), the measure bits per pixel uses.
    pub source_frame_bytes: u64,
    pub output_frame_bytes: u64,
    /// Frames in the clip.
    pub frames: u32,
}

/// Raw CFA planes (G1, R, G2, B), each w x h, 14-bit.
struct Raw {
    w: usize,
    h: usize,
    planes: Vec<Vec<u16>>,
    black: f32,
}

/// The last decoded frame pair, so zooming and panning around one frame stay fast.
struct Decoded {
    key: (PathBuf, u32, Target, Resolution),
    before: Raw,
    after: Raw,
    source_frame_bytes: u64,
    output_frame_bytes: u64,
    frames: u32,
}

static CACHE: Mutex<Option<Arc<Decoded>>> = Mutex::new(None);

fn decode_raw(cs: &[u8]) -> Result<Raw> {
    let (h, _) = TicoHeader::parse(cs)?;
    let l = Layout::make(&h, h.width as usize, h.height as usize)?;
    let mut coeffs = vec![0i32; Coeffs::len(&l)];
    decode_frame(cs, &h, &l, &mut coeffs)?;
    let mut pl = Planes::default();
    inverse_wavelet(&l, &mut coeffs, &mut pl);
    inverse_star_tetrix(&mut pl);
    let tr = Transfer::new(&h);
    let planes = (0..4).map(|c| pl.plane(c).iter().map(|&v| tr.to_raw(v)).collect()).collect();
    Ok(Raw { w: pl.w, h: pl.h, planes, black: tr.black as f32 })
}

/// Linear camera RGB (black subtracted, 0..1) of the region `crop` (fractions of the frame:
/// x, y, width, height) at ow x oh, by area averaging quads (nearest quad when zoomed in).
fn to_rgb(raw: &Raw, crop: [f64; 4], ow: usize, oh: usize) -> Vec<[f32; 3]> {
    let white = 16383.0 - raw.black;
    let (cx, cy) = (crop[0] * raw.w as f64, crop[1] * raw.h as f64);
    let sx = crop[2] * raw.w as f64 / ow as f64;
    let sy = crop[3] * raw.h as f64 / oh as f64;
    let mut out = vec![[0f32; 3]; ow * oh];
    for y in 0..oh {
        let y0 = ((cy + y as f64 * sy) as usize).min(raw.h - 1);
        let y1 = ((cy + (y + 1) as f64 * sy) as usize).clamp(y0 + 1, raw.h);
        for x in 0..ow {
            let x0 = ((cx + x as f64 * sx) as usize).min(raw.w - 1);
            let x1 = ((cx + (x + 1) as f64 * sx) as usize).clamp(x0 + 1, raw.w);
            let mut acc = [0f64; 4];
            for yy in y0..y1 {
                let row = yy * raw.w;
                for (c, a) in acc.iter_mut().enumerate() {
                    *a += raw.planes[c][row + x0..row + x1].iter().map(|&v| v as f64).sum::<f64>();
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f64;
            let v = |c: usize| (((acc[c] / n) as f32 - raw.black) / white).max(0.0);
            out[y * ow + x] = [v(1), 0.5 * (v(0) + v(2)), v(3)];
        }
    }
    out
}

/// White balance gains and exposure from the original (gray world over mid tones).
fn grade(rgb: &[[f32; 3]]) -> ([f32; 3], f32) {
    let mut sum = [0f64; 3];
    let mut n = 0f64;
    for p in rgb {
        let lum = p[1];
        if lum > 0.002 && lum < 0.6 {
            for c in 0..3 {
                sum[c] += p[c] as f64;
            }
            n += 1.0;
        }
    }
    if n < 1.0 {
        return ([1.0; 3], 1.0);
    }
    let g = sum[1] / n;
    let gains = [(g / sum[0].max(1e-9) * n) as f32, 1.0, (g / sum[2].max(1e-9) * n) as f32];
    // Expose so the average green sits at 18% before the tone curve.
    let exposure = (0.18 / g.max(1e-6)) as f32;
    (gains.map(|v| v.clamp(0.25, 4.0)), exposure.clamp(0.1, 64.0))
}

fn render(rgb: &[[f32; 3]], gains: [f32; 3], exposure: f32) -> Vec<u8> {
    let srgb = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        (s * 255.0 + 0.5) as u8
    };
    let mut out = Vec::with_capacity(rgb.len() * 4);
    for p in rgb {
        let px = [0, 1, 2].map(|c| p[c] * gains[c] * exposure);
        let y = 0.2126 * px[0] + 0.7152 * px[1] + 0.0722 * px[2];
        for c in 0..3 {
            let x = (y + 1.35 * (px[c] - y)).max(0.0);
            // Soft shoulder (Reinhard with a white point of 4).
            out.push(srgb(x * (1.0 + x / 16.0) / (1.0 + x)));
        }
        out.push(255);
    }
    out
}

fn decode_pair(
    path: &Path,
    frame: u32,
    target: Target,
    resolution: Resolution,
    backend: Backend,
) -> Result<Arc<Decoded>> {
    let key = (path.to_path_buf(), frame, target, resolution);
    if let Some(d) = CACHE.lock().unwrap().as_ref().filter(|d| d.key == key) {
        return Ok(d.clone());
    }
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !is_supported_codec(video.codec) {
        return Err(Error("not a Nikon N-RAW or R3D NE clip".into()));
    }
    if video.samples.is_empty() {
        return Err(Error("clip has no frames".into()));
    }
    let s = video.samples[frame as usize];
    let sample = input.read(s.offset, s.size as usize)?;
    let nf = NrawFrame::parse(&sample)?;
    let cs = &sample[nf.codestream_offset..];

    let src = source_geometry(&movie);
    let dst = target_geometry(src, resolution);
    // As in a real run: legacy steps resolved for this clip, a bpp at or above the clip's
    // the lossless repack.
    let target = target.resolve(source_bpp(&input, &movie));
    let out_cs = if dst.width != video.width || dst.height != video.height {
        resize::create(backend, src, dst)?.process(cs, target, &mut Vec::new())?
    } else {
        let p = RequantParams {
            target_rank: rank_for_quality(target.preset()),
            budget: target.frame_budget(video.width, video.height),
            ..Default::default()
        };
        requant_codestream(cs, &p, 0)?.0
    };
    let d = Arc::new(Decoded {
        key,
        before: decode_raw(cs)?,
        after: decode_raw(&out_cs)?,
        // Codestream bytes, as bits per pixel counts them (the output as written, KiB padded).
        source_frame_bytes: nf.codestream_size as u64,
        output_frame_bytes: (nf.build(&out_cs, None)?.len() - nf.codestream_offset) as u64,
        frames: video.samples.len() as u32,
    });
    *CACHE.lock().unwrap() = Some(d.clone());
    Ok(d)
}

fn frame_count(path: &Path) -> Result<u32> {
    let input = InputFile::open(path)?;
    Ok(Movie::parse(&input)?.video().samples.len() as u32)
}

/// Renders the region `crop` (fractions of the frame: x, y, width, height) of one frame
/// before and after compression, `width` pixels wide. One display pixel is one 2x2 sensor
/// quad at most; crops smaller than `width` quads show each quad as a block.
#[allow(clippy::too_many_arguments)]
pub fn preview_frame(
    path: &Path,
    frame: u32,
    target: Target,
    resolution: Resolution,
    backend: Backend,
    crop: [f64; 4],
    width: u32,
) -> Result<Preview> {
    let frames = frame_count(path)?;
    let frame = frame.min(frames.saturating_sub(1));
    let target = match target {
        Target::Quality(q) => Target::quality(q),
        t => t,
    };
    let d = decode_pair(path, frame, target, resolution, backend)?;
    let x = crop[0].clamp(0.0, 1.0);
    let y = crop[1].clamp(0.0, 1.0);
    let crop = [x, y, crop[2].clamp(1e-4, 1.0 - x).max(1e-4), crop[3].clamp(1e-4, 1.0 - y).max(1e-4)];
    let (bw, bh) = (d.before.w as f64 * crop[2], d.before.h as f64 * crop[3]);
    let ow = width.clamp(16, 8192) as usize;
    let oh = ((ow as f64 * bh / bw).round() as usize).max(1);
    // Grade from the whole original frame, so panning does not shift the colors.
    let (gains, exposure) = grade(&to_rgb(&d.before, [0.0, 0.0, 1.0, 1.0], 480, 270));
    let b = to_rgb(&d.before, crop, ow, oh);
    let a = to_rgb(&d.after, crop, ow, oh);
    Ok(Preview {
        width: ow as u32,
        height: oh as u32,
        before_rgba: render(&b, gains, exposure),
        after_rgba: render(&a, gains, exposure),
        source_frame_bytes: d.source_frame_bytes,
        output_frame_bytes: d.output_frame_bytes,
        frames: d.frames,
    })
}

/// A small picture of one frame (sRGB 8-bit RGBA).
#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Renders frame `frame` (clamped) of the original clip `width` pixels wide, with the same
/// look as the previewer. It decodes only, so it is cheap and leaves the preview cache alone.
pub fn thumbnail(path: &Path, frame: u32, width: u32) -> Result<Thumbnail> {
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !is_supported_codec(video.codec) || video.samples.is_empty() {
        return Err(Error("not a Nikon N-RAW or R3D NE clip".into()));
    }
    let s = video.samples[(frame as usize).min(video.samples.len() - 1)];
    let sample = input.read(s.offset, s.size as usize)?;
    let nf = NrawFrame::parse(&sample)?;
    let raw = decode_raw(&sample[nf.codestream_offset..])?;
    let ow = (width.clamp(16, 2048) as usize).min(raw.w);
    let oh = (raw.h * ow / raw.w).max(1);
    let rgb = to_rgb(&raw, [0.0, 0.0, 1.0, 1.0], ow, oh);
    let (gains, exposure) = grade(&rgb);
    Ok(Thumbnail { width: ow as u32, height: oh as u32, rgba: render(&rgb, gains, exposure) })
}
