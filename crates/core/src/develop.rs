//! Develop: a quick turnaround development of N-RAW and R3D NE clips to 10-bit HEVC, with
//! basic RAW controls (white balance, exposure, highlight roll-off and a few tone and color
//! controls), not a grading tool.
//!
//! Each frame is decoded to its four Bayer planes (G1, R, G2, B, one sample per 2x2 sensor
//! quad) on the CPU, then one pass per output pixel (on the GPU with Metal on a Mac, on the
//! CPU elsewhere) samples the planes bilinearly at the output position, applies the white
//! balance gains, clips at sensor saturation, converts with the clip's camera to XYZ matrix
//! into the output gamut, applies exposure and the tone and color controls in scene linear,
//! and encodes Log3G10 (REDWideGamutRGB), N-Log (Rec.2020) or Rec.709 (BT.1886, with a
//! highlight roll-off and tone map). The same Rec.709 transform is the viewing transform for
//! log output in the live preview. Frames are piped as 16-bit RGB into ffmpeg, which encodes
//! HEVC Main10 (VideoToolbox on a Mac, NVENC, Quick Sync or AMF elsewhere, libx265 otherwise)
//! at a target bitrate and copies the clip's audio as 32-bit float PCM into a MOV.
//!
//! Log curves: N-Log from Nikon's N-Log specification; Log3G10 and REDWideGamutRGB from RED's
//! IPP2 white paper. Scene linear has 18% gray at 0.18; sensor saturation sits 6 stops above.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use crate::bmff::{fourcc, InputFile, Movie};
use crate::frame::{decode_frame, inverse_star_tetrix, inverse_wavelet, Coeffs, Planes, Transfer};
use crate::nraw::NrawFrame;
use crate::tico::{Layout, TicoHeader};
use crate::transcode::is_supported_codec;
use crate::{proxy, quality, r3d, Error, Result};

/// Folder developed clips go to, next to the clip.
pub const DEVELOP_FOLDER: &str = "Developed";

/// Linear scene value of sensor saturation: 6 stops over 18% gray.
pub const SENSOR_WHITE: f32 = 0.18 * 64.0;

/// Default bitrate for a UHD (3840x2160) export, scaled by pixel count for other sizes.
pub const UHD_KBPS: u32 = 100_000;

// Metadata ---------------------------------------------------------------------

/// What a clip says about how it was shot: the "as shot" starting point.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    /// R3D NE (IPP2 controls) or N-RAW (Nikon RAW controls).
    pub r3d: bool,
    pub camera: String,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub fps: f64,
    /// Recorded ISO (0 when missing).
    pub iso: u32,
    /// Recorded white balance in kelvin (0 when missing).
    pub kelvin: u32,
    /// As shot white balance gains for R and B (G is 1).
    pub wb: [f32; 2],
    /// Camera to XYZ (D65), row major, for white balanced sensor RGB.
    pub matrix: [f32; 9],
    pub has_audio: bool,
}

fn floats(v: &[u8], n: usize) -> Option<Vec<f32>> {
    (v.len() == 1 + 4 * n)
        .then(|| v[1..].chunks(4).map(|c| f32::from_be_bytes(c.try_into().unwrap())).collect::<Vec<f32>>())
        .filter(|f| f.iter().all(|x| x.is_finite()))
}

/// Reads the as shot settings of a clip.
pub fn clip_info(path: &Path) -> Result<ClipInfo> {
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !is_supported_codec(video.codec) {
        return Err(Error("not a Nikon N-RAW or R3D NE clip".into()));
    }
    let tags = r3d::nctg_tags(&movie);
    let get = |id: u32| r3d::tag_value(&movie, &tags, id);
    let text = |id: u32| get(id).map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').trim().to_string());
    let camera = text(0x2).unwrap_or_default();
    let iso = get(0x0110_8832).filter(|v| v.len() == 4).map(|v| u32::from_be_bytes(v.try_into().unwrap())).unwrap_or(0);
    let kelvin = text(0x0200_0005)
        .and_then(|s| s.trim_end_matches(['K', 'k']).trim().parse::<u32>().ok())
        .filter(|k| (1000..=50_000).contains(k))
        .unwrap_or(0);
    let (mut wb, mut matrix) = ([1.7, 1.5], r3d::default_camera_matrix());
    if let Some(s) = video.samples.first() {
        let head = input.read(s.offset, (s.size as usize).min(64 << 10))?;
        for (tag, v) in r3d::frame_tags(&head) {
            match tag {
                0x0190_0009 => {
                    if let Some(f) = floats(v, 2).filter(|f| f.iter().all(|&x| (0.2..8.0).contains(&x))) {
                        wb = [f[0], f[1]];
                    }
                }
                0x0190_0012 => {
                    if let Some(f) = floats(v, 9).filter(|f| (f[3] + f[4] + f[5] - 1.0).abs() < 0.05) {
                        matrix.copy_from_slice(&f);
                    }
                }
                _ => {}
            }
        }
    }
    let fps = quality::video_frame_rate(&movie);
    Ok(ClipInfo {
        r3d: video.codec == fourcc(b"NR3D"),
        camera,
        width: video.width,
        height: video.height,
        frames: video.samples.len() as u32,
        fps: if fps > 0.0 { fps } else { 25.0 },
        iso,
        kelvin,
        wb,
        matrix,
        has_audio: movie.tracks.iter().any(|t| t.handler == fourcc(b"soun")),
    })
}

// Controls ---------------------------------------------------------------------

/// Output color space and gamma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Log3G10 / REDWideGamutRGB.
    Log3G10,
    /// N-Log / Rec.2020.
    NLog,
    /// Rec.709 / BT.1886 (gamma 2.4), with roll-off and tone map.
    Rec709,
}

/// IPP2 style highlight roll-off for Rec.709 output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollOff {
    None,
    Hard,
    Medium,
    Soft,
    VerySoft,
}

/// IPP2 style output tone map (contrast of the Rec.709 transform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMap {
    None,
    Low,
    Medium,
    High,
}

/// The develop settings of one clip. Fields that a format does not have keep their unity
/// value (R3D NE: highlights to sharpness; N-RAW: ISO, contrast).
#[derive(Debug, Clone)]
pub struct Params {
    pub output: Output,
    /// Color temperature in kelvin and tint (R3D: -100 to 100, N-RAW: -150 to 150).
    pub kelvin: f32,
    pub tint: f32,
    /// Exposure in stops (R3D Exposure Adjust -7 to 7, N-RAW Exposure -5 to 5).
    pub exposure: f32,
    /// R3D ISO (a gain relative to the recorded ISO).
    pub iso: f32,
    pub roll_off: RollOff,
    pub tone_map: ToneMap,
    /// R3D contrast, -1 to 1.
    pub contrast: f32,
    /// N-RAW, -100 to 100.
    pub highlights: f32,
    pub shadows: f32,
    pub saturation: f32,
    pub color_boost: f32,
    /// N-RAW sharpness, 0 to 100, 20 is unity.
    pub sharpness: f32,
}

impl Params {
    /// The as shot settings of a clip: its white balance and ISO, native log output.
    pub fn as_shot(c: &ClipInfo) -> Params {
        Params {
            output: if c.r3d { Output::Log3G10 } else { Output::NLog },
            kelvin: if c.kelvin > 0 { c.kelvin as f32 } else { 5600.0 },
            tint: 0.0,
            exposure: 0.0,
            iso: if c.iso > 0 { c.iso as f32 } else { 800.0 },
            roll_off: RollOff::Medium,
            tone_map: ToneMap::Medium,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            saturation: 0.0,
            color_boost: 0.0,
            sharpness: 20.0,
        }
    }
}

// Color math -------------------------------------------------------------------

type M3 = [[f32; 3]; 3];

fn mul(a: &M3, b: &M3) -> M3 {
    let mut m = [[0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    m
}

fn apply(m: &M3, v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

fn inverse(m: &M3) -> M3 {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    let k = if det.abs() > 1e-12 { 1.0 / det } else { 0.0 };
    [
        [(e * i - f * h) * k, (c * h - b * i) * k, (b * f - c * e) * k],
        [(f * g - d * i) * k, (a * i - c * g) * k, (c * d - a * f) * k],
        [(d * h - e * g) * k, (b * g - a * h) * k, (a * e - b * d) * k],
    ]
}

/// XYZ (D65) to Rec.2020, REDWideGamutRGB and Rec.709 RGB.
const XYZ_TO_REC2020: M3 =
    [[1.716_651, -0.355_671, -0.253_366], [-0.666_684, 1.616_481, 0.015_769], [0.017_640, -0.042_771, 0.942_103]];
const XYZ_TO_RWG: M3 =
    [[1.412_807, -0.177_523, -0.151_771], [-0.486_203, 1.290_696, 0.157_401], [-0.037_139, 0.286_376, 0.687_680]];
const XYZ_TO_REC709: M3 =
    [[3.240_97, -1.537_383, -0.498_611], [-0.969_244, 1.875_968, 0.041_555], [0.055_630, -0.203_977, 1.056_972]];

/// CIE xy of a Planckian radiator (Kim et al. cubic fit, 1667 K to 25000 K).
pub fn planck_xy(t: f32) -> (f32, f32) {
    let t = t.clamp(1667.0, 25_000.0) as f64;
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / (t * t * t) - 0.234_358_9e6 / (t * t) + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / (t * t * t) + 2.107_037_9e6 / (t * t) + 0.222_634_7e3 / t + 0.240_390
    };
    let y = if t <= 2222.0 {
        -1.106_381_4 * x * x * x - 1.348_110_20 * x * x + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x * x * x - 1.374_185_93 * x * x + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x * x * x - 5.873_386_7 * x * x + 3.751_129_90 * x - 0.370_014_83
    };
    (x as f32, y as f32)
}

fn cam_matrix(c: &ClipInfo) -> M3 {
    let m = c.matrix;
    [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]]
}

/// White balance gains (R, G, B) for a color temperature and tint, relative to the as shot
/// gains: the sensor's response to a Planckian light at `kelvin` against the one at the
/// recorded temperature. Positive tint adds magenta.
pub fn wb_gains(c: &ClipInfo, kelvin: f32, tint: f32) -> [f32; 3] {
    let inv = inverse(&cam_matrix(c));
    let neutral = |t: f32| {
        let (x, y) = planck_xy(t);
        let n = apply(&inv, [x / y, 1.0, (1.0 - x - y) / y]);
        [n[0] / n[1], 1.0, n[2] / n[1]]
    };
    let shot = if c.kelvin > 0 { c.kelvin as f32 } else { 5600.0 };
    let (a, b) = (neutral(shot), neutral(kelvin));
    let g = (2f32).powf(-tint / 300.0);
    let gains = [c.wb[0] * a[0] / b[0], g, c.wb[1] * a[2] / b[2]];
    gains.map(|v| v.clamp(0.1, 16.0))
}

/// Everything the per pixel pass needs, laid out as the Metal kernel reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Shade {
    /// White balance gains (x, y, z), sensor black and 1 / (white - black) in raw units.
    pub gains: [f32; 4],
    pub black: f32,
    pub inv_range: f32,
    /// 0 Log3G10, 1 N-Log, 2 Rec.709.
    pub mode: u32,
    /// Show the Rec.709 transform of the log output (preview of log output).
    pub view: u32,
    /// White balanced sensor RGB to scene linear output RGB (rows padded to 4).
    pub m_out: [[f32; 4]; 3],
    /// White balanced sensor RGB to scene linear Rec.709.
    pub m_709: [[f32; 4]; 3],
    /// Highlights, shadows, saturation, color boost (-1 to 1), contrast (-1 to 1).
    pub highlights: f32,
    pub shadows: f32,
    pub saturation: f32,
    pub color_boost: f32,
    pub contrast: f32,
    /// Rec.709 transform: white point of the shoulder (0: hard clip) and tone map slope.
    pub shoulder: f32,
    pub slope: f32,
    pub _pad: f32,
}

/// The per pixel settings for a clip and its controls.
pub fn shade(c: &ClipInfo, p: &Params, black: f32, white: f32, view: bool) -> Shade {
    let g = wb_gains(c, p.kelvin, p.tint);
    let gain = SENSOR_WHITE
        * 2f32.powf(p.exposure)
        * if c.r3d { p.iso / (if c.iso > 0 { c.iso as f32 } else { 800.0 }) } else { 1.0 };
    let cam = cam_matrix(c);
    let to = match p.output {
        Output::Log3G10 => XYZ_TO_RWG,
        Output::NLog => XYZ_TO_REC2020,
        Output::Rec709 => XYZ_TO_REC709,
    };
    let pad = |m: M3| m.map(|r| [r[0] * gain, r[1] * gain, r[2] * gain, 0.0]);
    let (shoulder, slope) = (
        match p.roll_off {
            RollOff::None => 0.0,
            RollOff::Hard => 2.0,
            RollOff::Medium => 4.0,
            RollOff::Soft => 8.0,
            RollOff::VerySoft => 16.0,
        },
        match p.tone_map {
            ToneMap::None => 1.0,
            ToneMap::Low => 1.08,
            ToneMap::Medium => 1.16,
            ToneMap::High => 1.26,
        },
    );
    Shade {
        gains: [g[0], g[1], g[2], 0.0],
        black,
        inv_range: 1.0 / (white - black).max(1.0),
        mode: match p.output {
            Output::Log3G10 => 0,
            Output::NLog => 1,
            Output::Rec709 => 2,
        },
        view: u32::from(view && p.output != Output::Rec709),
        m_out: pad(mul(&to, &cam)),
        m_709: pad(mul(&XYZ_TO_REC709, &cam)),
        highlights: p.highlights / 100.0,
        shadows: p.shadows / 100.0,
        saturation: p.saturation / 100.0,
        color_boost: p.color_boost / 100.0,
        contrast: p.contrast,
        shoulder,
        slope,
        _pad: 0.0,
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Tone and color controls in scene linear (pivot 18% gray).
fn look(s: &Shade, v: [f32; 3]) -> [f32; 3] {
    let l = (0.2627 * v[0] + 0.678 * v[1] + 0.0593 * v[2]).max(1e-7);
    let ev = (l / 0.18).log2();
    let mut ev2 = ev + s.highlights * 2.0 * smoothstep(0.0, 4.0, ev) + s.shadows * 2.0 * smoothstep(0.0, -6.0, ev);
    ev2 *= 1.0 + 0.5 * s.contrast;
    let k = (ev2 - ev).exp2();
    let l2 = l * k;
    let chroma = v.iter().map(|&c| (c - l).abs()).fold(0.0f32, f32::max) / l;
    let sat = (1.0 + s.saturation) * (1.0 + s.color_boost * (1.0 - (chroma * 2.0).min(1.0)));
    v.map(|c| l2 + (c * k - l2) * sat)
}

/// Log3G10 (RED IPP2 white paper).
pub fn log3g10(x: f32) -> f32 {
    let x = x + 0.01;
    if x < 0.0 {
        x * 15.1927
    } else {
        0.224_282 * (x * 155.975_33 + 1.0).log10()
    }
}

/// N-Log (Nikon N-Log specification), 10-bit code / 1023.
pub fn nlog(x: f32) -> f32 {
    if x < 0.328 {
        650.0 * (x + 0.0075).max(0.0).cbrt() / 1023.0
    } else {
        (150.0 * x.ln() + 619.0) / 1023.0
    }
}

/// Scene linear Rec.709 to a BT.1886 display signal: tone map slope around gray, then a
/// shoulder (extended Reinhard, white point `shoulder` times 1) or a hard clip.
pub fn rec709(s: &Shade, v: [f32; 3]) -> [f32; 3] {
    v.map(|x| {
        let x = 0.18 * (x.max(0.0) / 0.18).powf(s.slope) * 1.1;
        let y = if s.shoulder > 0.0 {
            x * (1.0 + x / (s.shoulder * s.shoulder)) / (1.0 + x) * (1.0 + 1.0 / s.shoulder).min(2.0)
        } else {
            x
        };
        y.clamp(0.0, 1.0).powf(1.0 / 2.4)
    })
}

/// One output pixel from white balance ready sensor RGB (0..1 of the sensor range).
pub fn shade_pixel(s: &Shade, cam: [f32; 3]) -> [f32; 3] {
    let w = [0, 1, 2].map(|c| (cam[c] * s.gains[c]).clamp(0.0, 1.0));
    let row = |m: &[[f32; 4]; 3]| [0, 1, 2].map(|r| m[r][0] * w[0] + m[r][1] * w[1] + m[r][2] * w[2]);
    if s.mode == 2 || s.view != 0 {
        let v = look(s, row(&s.m_709));
        return rec709(s, v);
    }
    let v = look(s, row(&s.m_out));
    if s.mode == 0 {
        v.map(log3g10)
    } else {
        v.map(nlog)
    }
}

// Frames -----------------------------------------------------------------------

/// The four Bayer planes of a frame (G1, R, G2, B) as raw values, quad resolution.
pub struct RawPlanes {
    pub w: usize,
    pub h: usize,
    pub data: [Vec<f32>; 4],
    pub black: f32,
    pub white: f32,
}

/// Decodes frame `index` of a clip to its raw planes.
pub fn decode_planes(input: &InputFile, movie: &Movie, index: usize) -> Result<RawPlanes> {
    let video = movie.video();
    let s = *video.samples.get(index).ok_or_else(|| Error("no such frame".into()))?;
    let sample = input.read(s.offset, s.size as usize)?;
    let nf = NrawFrame::parse(&sample)?;
    let cs = &sample[nf.codestream_offset..];
    let (h, _) = TicoHeader::parse(cs)?;
    let l = Layout::make(&h, h.width as usize, h.height as usize)?;
    let mut coeffs = vec![0i32; Coeffs::len(&l)];
    decode_frame(cs, &h, &l, &mut coeffs)?;
    let mut pl = Planes::default();
    inverse_wavelet(&l, &mut coeffs, &mut pl);
    inverse_star_tetrix(&mut pl);
    let tr = Transfer::new(&h);
    let data = [0, 1, 2, 3].map(|c| pl.plane(c).par_iter().map(|&v| tr.to_raw(v) as f32).collect::<Vec<f32>>());
    Ok(RawPlanes { w: pl.w, h: pl.h, data, black: tr.black as f32, white: 16383.0 })
}

/// Bilinear sample of a plane at quad coordinates.
#[inline]
fn sample(p: &[f32], w: usize, h: usize, u: f32, v: f32) -> f32 {
    let u = u.clamp(0.0, (w - 1) as f32);
    let v = v.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (u as usize, v as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (u - x0 as f32, v - y0 as f32);
    let a = p[y0 * w + x0] + (p[y0 * w + x1] - p[y0 * w + x0]) * fx;
    let b = p[y1 * w + x0] + (p[y1 * w + x1] - p[y1 * w + x0]) * fx;
    a + (b - a) * fy
}

/// Renders a crop (fractions x, y, w, h of the frame) to `ow` x `oh` 16-bit RGB on the CPU.
pub fn render_cpu(pl: &RawPlanes, s: &Shade, crop: [f32; 4], ow: usize, oh: usize) -> Vec<u16> {
    let mut out = vec![0u16; ow * oh * 3];
    let (sx, sy) = (crop[2] * pl.w as f32 / ow as f32, crop[3] * pl.h as f32 / oh as f32);
    let (ox, oy) = (crop[0] * pl.w as f32, crop[1] * pl.h as f32);
    out.par_chunks_mut(ow * 3).enumerate().for_each(|(y, row)| {
        let v = oy + (y as f32 + 0.5) * sy - 0.5;
        for x in 0..ow {
            let u = ox + (x as f32 + 0.5) * sx - 0.5;
            let c = |i: usize| ((sample(&pl.data[i], pl.w, pl.h, u, v) - s.black) * s.inv_range).max(0.0);
            let cam = [c(1), 0.5 * (c(0) + c(2)), c(3)];
            let px = shade_pixel(s, cam);
            for k in 0..3 {
                row[x * 3 + k] = (px[k].clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
            }
        }
    });
    out
}

/// The renderer: Metal on a Mac when it works, the CPU otherwise.
pub fn render(pl: &RawPlanes, s: &Shade, crop: [f32; 4], ow: usize, oh: usize) -> Vec<u16> {
    #[cfg(all(target_os = "macos", feature = "metal"))]
    if std::env::var_os("BLUECOMPRESS_DEVELOP_CPU").is_none() {
        if let Some(r) = crate::gpu::develop_metal(pl, s, crop, ow, oh) {
            return r;
        }
    }
    render_cpu(pl, s, crop, ow, oh)
}

/// Which renderer [`render`] uses here.
pub fn renderer_name() -> String {
    #[cfg(all(target_os = "macos", feature = "metal"))]
    if std::env::var_os("BLUECOMPRESS_DEVELOP_CPU").is_none() {
        if let Some(n) = crate::gpu::develop_metal_name() {
            return n;
        }
    }
    "CPU".into()
}

/// Sharpness (N-RAW): an unsharp mask on the encoded image, 20 is unity (no change).
pub fn sharpen(img: &mut [u16], w: usize, h: usize, sharpness: f32) {
    let amount = (sharpness - 20.0) / 80.0;
    if amount.abs() < 1e-3 || w < 3 || h < 3 {
        return;
    }
    let src = img.to_vec();
    img.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
            for k in 0..3 {
                let at = |xx: usize, yy: usize| src[(yy * w + xx) * 3 + k] as f32;
                let blur = (at(x0, y) + at(x1, y) + at(x, y0) + at(x, y1) + 4.0 * at(x, y)) / 8.0;
                let c = at(x, y);
                row[x * 3 + k] = (c + (c - blur) * amount * 2.0).clamp(0.0, 65535.0) as u16;
            }
        }
    });
}

/// Exposure (stops) that puts the frame's average scene luminance at 18% gray with the
/// current white balance, rounded to 0.1, within the format's range.
pub fn auto_exposure(c: &ClipInfo, p: &Params, pl: &RawPlanes) -> f32 {
    let mut q = p.clone();
    q.exposure = 0.0;
    q.output = Output::Rec709;
    let mut s = shade(c, &q, pl.black, pl.white, false);
    // Measure scene linear luminance: an identity tone setup.
    s.highlights = 0.0;
    s.shadows = 0.0;
    s.contrast = 0.0;
    let step = (pl.w * pl.h / 20_000).max(1);
    let mut logs = Vec::new();
    for i in (0..pl.w * pl.h).step_by(step) {
        let c = |k: usize| ((pl.data[k][i] - s.black) * s.inv_range).max(0.0);
        let cam = [c(1), 0.5 * (c(0) + c(2)), c(3)];
        let w = [0, 1, 2].map(|k| (cam[k] * s.gains[k]).clamp(0.0, 1.0));
        let v = [0, 1, 2].map(|r| s.m_709[r][0] * w[0] + s.m_709[r][1] * w[1] + s.m_709[r][2] * w[2]);
        let l = 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        logs.push(l.max(1e-5).log2());
    }
    // Geometric mean, the way a center weighted meter averages in stops.
    let mean = logs.iter().sum::<f32>() / logs.len().max(1) as f32;
    let ev = (0.18f32).log2() - mean;
    let lim = if c.r3d { 7.0 } else { 5.0 };
    ((ev * 10.0).round() / 10.0).clamp(-lim, lim)
}

/// Output size for a target width (even, keeping the aspect ratio of the sensor).
pub fn output_size(c: &ClipInfo, width: u32) -> (u32, u32) {
    let w = width.max(16);
    let h = (w as f64 * c.height as f64 / c.width as f64 / 2.0).round() as u32 * 2;
    (w, h.max(2))
}

/// Bitrate for a size: 100 Mb/s at UHD 25p, scaled by pixels and frame rate (8 to 400 Mb/s).
pub fn default_kbps(w: u32, h: u32, fps: f64) -> u32 {
    let k = UHD_KBPS as f64 * (w as f64 * h as f64) / (3840.0 * 2160.0) * (fps / 25.0).max(0.5);
    (k.round() as u32).clamp(8_000, 400_000)
}

// Export -----------------------------------------------------------------------

/// One export.
#[derive(Debug, Clone)]
pub struct Export {
    pub input: PathBuf,
    pub output: PathBuf,
    pub params: Params,
    pub width: u32,
    pub kbps: u32,
    pub cbr: bool,
    /// Force an encoder; `None` takes the first that works (VideoToolbox on a Mac).
    pub encoder: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExportDone {
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub encoder: String,
    pub renderer: String,
    pub seconds: f64,
    pub output_bytes: u64,
}

/// Where a developed clip goes: `<clip folder>/Developed/<clip name>_<suffix>.mov`.
pub fn export_path(clip: &Path, output: Output) -> PathBuf {
    let dir = clip.parent().unwrap_or(Path::new(".")).join(DEVELOP_FOLDER);
    let stem = clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "clip".into());
    let tag = match output {
        Output::Log3G10 => "Log3G10",
        Output::NLog => "NLog",
        Output::Rec709 => "Rec709",
    };
    dir.join(format!("{stem}_{tag}.mov"))
}

fn tags(o: Output) -> [&'static str; 3] {
    match o {
        Output::Rec709 => ["bt709", "bt709", "bt709"],
        Output::NLog => ["bt2020", "unknown", "bt2020nc"],
        Output::Log3G10 => ["unknown", "unknown", "bt2020nc"],
    }
}

/// Encoder arguments for a 10-bit HEVC encode at `kbps` (average bitrate, or CBR).
fn rate_args(encoder: &str, kbps: u32, cbr: bool) -> Vec<String> {
    if cbr {
        return proxy::cbr_args(encoder, kbps);
    }
    let r = format!("{kbps}k");
    let max = format!("{}k", kbps * 3 / 2);
    let mut a: Vec<String> =
        vec!["-c:v".into(), encoder.into(), "-profile:v".into(), "main10".into(), "-b:v".into(), r.clone()];
    match encoder {
        "hevc_videotoolbox" => a.extend(["-pix_fmt".into(), "p010le".into()]),
        "libx265" => a.extend(
            ["-preset", "fast", "-pix_fmt", "yuv420p10le", "-x265-params", "log-level=error"].map(String::from),
        ),
        _ => a.extend(["-maxrate".into(), max, "-bufsize".into(), r, "-pix_fmt".into(), "p010le".into()]),
    }
    a.extend(["-tag:v".into(), "hvc1".into()]);
    a
}

/// Develops a clip to a 10-bit HEVC MOV with its audio as 32-bit float PCM. `progress`
/// gets (fraction, fps); `cancel` stops it and removes the partial file.
pub fn export(e: &Export, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<ExportDone> {
    let start = Instant::now();
    let info = clip_info(&e.input)?;
    let (ffmpeg, encoder) = match &e.encoder {
        Some(enc) => (proxy::find_ffmpeg().ok_or_else(|| Error("ffmpeg not found".into()))?, enc.clone()),
        None => proxy::default_encoder()?,
    };
    let input = InputFile::open(&e.input)?;
    let movie = Movie::parse(&input)?;
    let n = movie.video().samples.len();
    let (ow, oh) = output_size(&info, e.width);
    let (ow, oh) = (ow as usize, oh as usize);
    if let Some(dir) = e.output.parent() {
        std::fs::create_dir_all(dir).map_err(|err| Error(format!("cannot create {}: {err}", dir.display())))?;
    }
    let mut tmp = e.output.as_os_str().to_owned();
    tmp.push(".bluecompress.tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let audio = proxy::has_readable_audio(&ffmpeg, &e.input);
    let [prim, trc, mat] = tags(e.params.output);
    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-hide_banner", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb48le"]).args([
        "-s",
        &format!("{ow}x{oh}"),
        "-r",
        &format!("{}", info.fps),
        "-i",
        "-",
    ]);
    if audio {
        cmd.arg("-i").arg(&e.input).args(["-map", "0:v:0", "-map", "1:a:0", "-c:a", "pcm_f32le", "-shortest"]);
    }
    let matrix = if mat == "bt709" { "bt709" } else { "bt2020" };
    cmd.args(["-vf", &format!("scale=out_color_matrix={matrix}:out_range=tv,setparams=color_primaries={prim}:color_trc={trc}:colorspace={mat}:range=tv")])
        .args(rate_args(&encoder, e.kbps, e.cbr))
        .args(["-color_primaries", prim, "-color_trc", trc, "-colorspace", mat, "-color_range", "tv"])
        .args(["-f", "mov"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child =
        proxy::quiet(&mut cmd).spawn().map_err(|err| Error(format!("cannot run {}: {err}", ffmpeg.display())))?;
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut stdin = child.stdin.take().unwrap();
    let mut result: Result<()> = Ok(());
    let renderer = renderer_name();
    // Decode the next frames in parallel while the current batch renders and encodes.
    let batch = rayon::current_num_threads().clamp(2, 8);
    let mut bytes = Vec::with_capacity(ow * oh * 6);
    'outer: for b0 in (0..n).step_by(batch) {
        if cancel.load(Ordering::Relaxed) {
            result = Err(Error("cancelled".into()));
            break;
        }
        let planes = match (b0..(b0 + batch).min(n))
            .into_par_iter()
            .map(|i| decode_planes(&input, &movie, i))
            .collect::<Result<Vec<_>>>()
        {
            Ok(p) => p,
            Err(err) => {
                result = Err(err);
                break;
            }
        };
        for (k, pl) in planes.iter().enumerate() {
            let s = shade(&info, &e.params, pl.black, pl.white, false);
            let mut img = render(pl, &s, [0.0, 0.0, 1.0, 1.0], ow, oh);
            if !info.r3d {
                sharpen(&mut img, ow, oh, e.params.sharpness);
            }
            bytes.clear();
            for v in &img {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            if let Err(err) = stdin.write_all(&bytes) {
                result = Err(Error(format!("ffmpeg stopped reading: {err}")));
                break 'outer;
            }
            let done = b0 + k + 1;
            progress(done as f64 / n as f64, done as f64 / start.elapsed().as_secs_f64().max(1e-6));
        }
    }
    drop(stdin);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let log = err_reader.join().unwrap_or_default();
    let finish = result.and_then(|_| match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(Error(format!("ffmpeg ({encoder}) failed: {}", log.trim()))),
    });
    if let Err(err) = finish {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    std::fs::rename(&tmp, &e.output).map_err(|err| Error(format!("cannot rename to {}: {err}", e.output.display())))?;
    Ok(ExportDone {
        output: e.output.clone(),
        width: ow as u32,
        height: oh as u32,
        frames: n as u32,
        encoder,
        renderer,
        seconds: start.elapsed().as_secs_f64(),
        output_bytes: std::fs::metadata(&e.output).map(|m| m.len()).unwrap_or(0),
    })
}

/// A preview of one frame: `crop` of the frame at `ow` x `oh`, RGBA 8-bit. Log output is
/// shown through the Rec.709 viewing transform when `view709` is set.
pub fn preview(
    path: &Path,
    frame: usize,
    p: &Params,
    crop: [f32; 4],
    ow: usize,
    oh: usize,
    view709: bool,
) -> Result<Vec<u8>> {
    let info = clip_info(path)?;
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let pl = decode_planes(&input, &movie, frame.min(movie.video().samples.len().saturating_sub(1)))?;
    let s = shade(&info, p, pl.black, pl.white, view709);
    let mut img = render(&pl, &s, crop, ow, oh);
    if !info.r3d {
        sharpen(&mut img, ow, oh, p.sharpness);
    }
    let mut rgba = Vec::with_capacity(ow * oh * 4);
    for px in img.chunks(3) {
        rgba.extend_from_slice(&[(px[0] >> 8) as u8, (px[1] >> 8) as u8, (px[2] >> 8) as u8, 255]);
    }
    Ok(rgba)
}

/// Auto exposure for a frame of a clip with the given settings.
pub fn auto_exposure_for(path: &Path, frame: usize, p: &Params) -> Result<f32> {
    let info = clip_info(path)?;
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let pl = decode_planes(&input, &movie, frame.min(movie.video().samples.len().saturating_sub(1)))?;
    Ok(auto_exposure(&info, p, &pl))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> ClipInfo {
        ClipInfo {
            r3d: false,
            camera: "NIKON ZR".into(),
            width: 6048,
            height: 3402,
            frames: 1,
            fps: 25.0,
            iso: 800,
            kelvin: 4760,
            wb: [1.7, 1.49],
            matrix: r3d::default_camera_matrix(),
            has_audio: false,
        }
    }

    #[test]
    fn curves_put_gray_where_the_papers_do() {
        assert!((log3g10(0.18) - 1.0 / 3.0).abs() < 1e-3);
        assert!((nlog(0.18) - 0.3637).abs() < 1e-3);
        assert!(log3g10(0.0) > 0.09 && log3g10(0.0) < 0.1);
    }

    #[test]
    fn as_shot_white_balance_is_the_recorded_gains() {
        let c = clip();
        let g = wb_gains(&c, 4760.0, 0.0);
        assert!((g[0] - 1.7).abs() < 1e-3 && (g[1] - 1.0).abs() < 1e-6 && (g[2] - 1.49).abs() < 1e-3);
        // Warmer light needs less red gain and more blue gain.
        let t = wb_gains(&c, 3200.0, 0.0);
        assert!(t[0] < g[0] && t[2] > g[2], "{t:?}");
        assert!(wb_gains(&c, 4760.0, 50.0)[1] < 1.0);
    }

    #[test]
    fn neutral_gray_stays_neutral() {
        let c = clip();
        let p = Params::as_shot(&c);
        let s = shade(&c, &p, 0.0, 1.0, false);
        // A sensor gray that the as shot gains balance: R and B scaled by 1 / gains.
        let v = 0.18 / SENSOR_WHITE;
        let px = shade_pixel(&s, [v / 1.7, v, v / 1.49]);
        assert!((px[0] - px[1]).abs() < 2e-3 && (px[2] - px[1]).abs() < 2e-3, "{px:?}");
        assert!((px[1] - nlog(0.18)).abs() < 2e-3, "{px:?}");
    }

    #[test]
    fn rec709_roll_off_and_view() {
        let c = clip();
        let mut p = Params::as_shot(&c);
        p.output = Output::Rec709;
        let s = shade(&c, &p, 0.0, 1.0, false);
        let a = rec709(&s, [8.0; 3])[0];
        assert!(a > 0.9 && a <= 1.0);
        p.roll_off = RollOff::None;
        let h = shade(&c, &p, 0.0, 1.0, false);
        assert_eq!(rec709(&h, [8.0; 3])[0], 1.0);
        let mid = rec709(&s, [0.18; 3])[0];
        assert!(mid > 0.35 && mid < 0.55, "{mid}");
    }

    #[test]
    fn planck_matches_known_points() {
        let (x, y) = planck_xy(6504.0);
        assert!((x - 0.3135).abs() < 0.003 && (y - 0.3237).abs() < 0.004);
    }
}
