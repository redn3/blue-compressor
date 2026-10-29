//! 10-bit H.265 (HEVC Main10) proxies: every frame is decoded to its Bayer planes, debayered
//! at half resolution (one RGB pixel per 2x2 sensor quad), scaled down by area averaging,
//! graded to Rec.709 and piped as 16-bit RGB into an `ffmpeg` sidecar that encodes it on the
//! GPU when it can (VideoToolbox, NVENC, Quick Sync, AMF) and with libx265 otherwise.
//!
//! The grade is a display grade for editing, not a color managed transform: white balance
//! (gray world) and exposure are measured once per clip, then a fixed saturation boost, a
//! soft shoulder and the Rec.709 transfer curve are applied. Audio (the clip's first track)
//! is copied as AAC when ffmpeg can read it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use rayon::prelude::*;

use crate::bmff::{InputFile, Movie};
use crate::frame::{decode_frame, inverse_star_tetrix, inverse_wavelet, Coeffs, Planes, Transfer};
use crate::nraw::NrawFrame;
use crate::tico::{Layout, TicoHeader};
use crate::transcode::is_supported_codec;
use crate::{quality, Error, Result};

/// Name of the folder proxies are written to, next to the clip they belong to.
pub const PROXY_FOLDER: &str = "Proxies";

/// Default proxy width cap (1920 wide, the height follows the clip's aspect ratio).
pub const DEFAULT_MAX_WIDTH: u32 = 1920;

/// HEVC encoders in the order they are tried. All but libx265 are hardware encoders.
pub const ENCODERS: [&str; 5] = ["hevc_videotoolbox", "hevc_nvenc", "hevc_qsv", "hevc_amf", "libx265"];

#[derive(Debug, Clone)]
pub struct ProxyOptions {
    pub input: PathBuf,
    /// The .mov to write (see [`proxy_path`]).
    pub output: PathBuf,
    /// Width cap in pixels; the proxy is never larger than the half resolution debayer.
    pub max_width: u32,
    /// Force one encoder from [`ENCODERS`]; `None` picks the first that works.
    pub encoder: Option<String>,
    /// ffmpeg to run; `None` uses [`find_ffmpeg`].
    pub ffmpeg: Option<PathBuf>,
    /// Constant bitrate at [`cbr_kbps`] instead of the default quality based VBR.
    pub cbr: bool,
}

impl ProxyOptions {
    pub fn new(input: &Path, output: &Path) -> Self {
        ProxyOptions {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            max_width: DEFAULT_MAX_WIDTH,
            encoder: None,
            ffmpeg: None,
            cbr: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProxyOutcome {
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub encoder: String,
    pub seconds: f64,
    pub output_bytes: u64,
}

/// Where the proxy of a clip written to `clip` goes: `<clip folder>/Proxies/<clip name>.mov`.
pub fn proxy_path(clip: &Path) -> PathBuf {
    let dir = clip.parent().unwrap_or(Path::new("."));
    let stem = clip.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "proxy".into());
    dir.join(PROXY_FOLDER).join(format!("{stem}.mov"))
}

/// The ffmpeg to use: `$BLUECOMPRESS_FFMPEG`, then an `ffmpeg` next to this executable (or in
/// a macOS bundle's Resources), then `ffmpeg` on the PATH or in Homebrew's folders.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("BLUECOMPRESS_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        for c in [dir.join(exe_name), dir.join("../Resources").join(exe_name)] {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    // Apps started from the macOS Finder do not get Homebrew's folders on their PATH.
    let extra = ["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from);
    std::env::split_paths(&path).chain(extra).map(|d| d.join(exe_name)).find(|p| p.is_file())
}

fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Bits per pixel of a constant bitrate proxy: 0.4 gives about 20 Mb/s at 1920x1080 25p,
/// comfortably above the quality based VBR's typical rate for editing proxies.
pub const CBR_BITS_PER_PIXEL: f64 = 0.4;

/// Constant bitrate of a proxy in kb/s for its size and frame rate (2 to 80 Mb/s).
pub fn cbr_kbps(width: usize, height: usize, fps: f64) -> u32 {
    let bps = width as f64 * height as f64 * fps.max(1.0) * CBR_BITS_PER_PIXEL;
    (bps / 1000.0).round().clamp(2_000.0, 80_000.0) as u32
}

/// Encoder specific constant bitrate arguments at `kbps`: average, minimum and maximum
/// rate equal with a one second buffer, plus the encoder's own CBR mode where it has one
/// (x265 strict CBR adds filler so the rate never dips).
fn cbr_args(encoder: &str, kbps: u32) -> Vec<String> {
    let r = format!("{kbps}k");
    let mut a: Vec<String> = vec!["-c:v".into(), encoder.into(), "-profile:v".into(), "main10".into()];
    let rate = |a: &mut Vec<String>| {
        for (k, v) in [("-b:v", &r), ("-minrate", &r), ("-maxrate", &r), ("-bufsize", &r)] {
            a.push(k.into());
            a.push(v.clone());
        }
    };
    match encoder {
        "hevc_videotoolbox" => {
            a.extend(["-b:v".into(), r.clone(), "-constant_bit_rate".into(), "1".into()]);
            a.extend(["-pix_fmt".into(), "p010le".into()]);
        }
        "hevc_nvenc" => {
            a.extend(["-preset", "p5", "-rc", "cbr"].map(String::from));
            rate(&mut a);
            a.extend(["-pix_fmt".into(), "p010le".into()]);
        }
        "hevc_qsv" => {
            // QSV picks its CBR mode when the average and maximum rate are equal.
            rate(&mut a);
            a.extend(["-pix_fmt".into(), "p010le".into()]);
        }
        "hevc_amf" => {
            a.extend(["-rc".into(), "cbr".into()]);
            rate(&mut a);
            a.extend(["-filler_data".into(), "1".into(), "-pix_fmt".into(), "p010le".into()]);
        }
        _ => {
            a[1] = "libx265".into();
            a.extend(["-preset".into(), "fast".into()]);
            rate(&mut a);
            a.extend(["-pix_fmt", "yuv420p10le", "-x265-params", "log-level=error:strict-cbr=1"].map(String::from));
        }
    }
    a.extend(["-tag:v".into(), "hvc1".into()]);
    a
}

/// Encoder specific arguments for a 10-bit Main10 encode: quality based VBR, or constant
/// bitrate at `cbr` kb/s.
fn encoder_args(encoder: &str, cbr: Option<u32>) -> Vec<String> {
    if let Some(kbps) = cbr {
        return cbr_args(encoder, kbps);
    }
    let mut a = match encoder {
        "hevc_videotoolbox" => {
            vec!["-c:v", "hevc_videotoolbox", "-profile:v", "main10", "-q:v", "55", "-pix_fmt", "p010le"]
        }
        "hevc_nvenc" => vec![
            "-c:v",
            "hevc_nvenc",
            "-profile:v",
            "main10",
            "-preset",
            "p5",
            "-rc",
            "vbr",
            "-cq",
            "24",
            "-pix_fmt",
            "p010le",
        ],
        "hevc_qsv" => vec!["-c:v", "hevc_qsv", "-profile:v", "main10", "-global_quality", "24", "-pix_fmt", "p010le"],
        "hevc_amf" => vec![
            "-c:v",
            "hevc_amf",
            "-profile:v",
            "main10",
            "-rc",
            "cqp",
            "-qp_i",
            "22",
            "-qp_p",
            "24",
            "-pix_fmt",
            "p010le",
        ],
        _ => vec![
            "-c:v",
            "libx265",
            "-profile:v",
            "main10",
            "-preset",
            "fast",
            "-crf",
            "22",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error",
        ],
    };
    a.extend(["-tag:v", "hvc1"]);
    a.into_iter().map(String::from).collect()
}

/// Whether `encoder` really encodes a 10-bit frame here (a listed encoder can still lack its
/// GPU or driver).
pub fn encoder_works(ffmpeg: &Path, encoder: &str) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i", "color=c=gray:s=256x144:r=25:d=0.2"])
        .args(encoder_args(encoder, None))
        .args(["-frames:v", "2", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    quiet(&mut cmd).status().map(|s| s.success()).unwrap_or(false)
}

/// The encoders that work with `ffmpeg` on this machine, in [`ENCODERS`] order.
pub fn available_encoders(ffmpeg: &Path) -> Vec<String> {
    ENCODERS.iter().filter(|e| encoder_works(ffmpeg, e)).map(|e| e.to_string()).collect()
}

/// The encoder [`make_proxy`] uses by default (probed once per process), or why there is none.
pub fn default_encoder() -> Result<(PathBuf, String)> {
    static PICK: OnceLock<Result<(PathBuf, String)>> = OnceLock::new();
    PICK.get_or_init(|| {
        let ff = find_ffmpeg().ok_or_else(|| {
            Error("proxies need ffmpeg: install it, put it next to bluecompress, or set BLUECOMPRESS_FFMPEG".into())
        })?;
        let enc = available_encoders(&ff).into_iter().next().ok_or_else(|| {
            Error(format!("{} has no working HEVC encoder (needs libx265 or a GPU encoder)", ff.display()))
        })?;
        Ok((ff, enc))
    })
    .clone()
}

/// Human readable proxy encoder ("hevc_videotoolbox (GPU)", "libx265 (CPU)").
pub fn encoder_description() -> String {
    match default_encoder() {
        Ok((_, e)) if e == "libx265" => "libx265 (CPU)".into(),
        Ok((_, e)) => format!("{e} (GPU)"),
        Err(e) => e.0,
    }
}

/// Proxy size for a coded Bayer size: half resolution, capped at `max_width`, even.
pub fn proxy_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let (hw, hh) = ((width / 2).max(2) as f64, (height / 2).max(2) as f64);
    let w = hw.min(max_width.max(16) as f64);
    let h = hh * w / hw;
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    (even(w), even(h))
}

/// Linear camera RGB of one frame at ow x oh (black subtracted, 0..1), area averaged from the
/// quad planes (G1, R, G2, B).
fn frame_rgb(cs: &[u8], ow: usize, oh: usize) -> Result<Vec<[f32; 3]>> {
    let (h, _) = TicoHeader::parse(cs)?;
    let l = Layout::make(&h, h.width as usize, h.height as usize)?;
    let mut coeffs = vec![0i32; Coeffs::len(&l)];
    decode_frame(cs, &h, &l, &mut coeffs)?;
    let mut pl = Planes::default();
    inverse_wavelet(&l, &mut coeffs, &mut pl);
    inverse_star_tetrix(&mut pl);
    let tr = Transfer::new(&h);
    let (w, hgt) = (pl.w, pl.h);
    let planes: Vec<Vec<f32>> = (0..4).map(|c| pl.plane(c).iter().map(|&v| tr.to_raw(v) as f32).collect()).collect();
    Ok(downscale(&planes, w, hgt, tr.black as f32, ow, oh))
}

fn downscale(planes: &[Vec<f32>], w: usize, h: usize, black: f32, ow: usize, oh: usize) -> Vec<[f32; 3]> {
    let white = 16383.0 - black;
    let (sx, sy) = (w as f64 / ow as f64, h as f64 / oh as f64);
    let mut out = vec![[0f32; 3]; ow * oh];
    out.par_chunks_mut(ow).enumerate().for_each(|(y, row_out)| {
        let y0 = ((y as f64 * sy) as usize).min(h - 1);
        let y1 = (((y + 1) as f64 * sy) as usize).clamp(y0 + 1, h);
        for (x, px) in row_out.iter_mut().enumerate() {
            let x0 = ((x as f64 * sx) as usize).min(w - 1);
            let x1 = (((x + 1) as f64 * sx) as usize).clamp(x0 + 1, w);
            let mut acc = [0f32; 4];
            for yy in y0..y1 {
                let r = yy * w;
                for (c, a) in acc.iter_mut().enumerate() {
                    *a += planes[c][r + x0..r + x1].iter().sum::<f32>();
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            let v = |c: usize| ((acc[c] / n - black) / white).max(0.0);
            *px = [v(1), 0.5 * (v(0) + v(2)), v(3)];
        }
    });
    out
}

/// Clip grade: white balance gains (gray world over mid tones) and exposure (average green
/// to 18%), measured on a few frames.
#[derive(Debug, Clone, Copy)]
pub struct Grade {
    pub gains: [f32; 3],
    pub exposure: f32,
}

impl Grade {
    pub fn measure(frames: &[Vec<[f32; 3]>]) -> Grade {
        let mut sum = [0f64; 3];
        let mut n = 0f64;
        for p in frames.iter().flatten() {
            if p[1] > 0.002 && p[1] < 0.6 {
                for c in 0..3 {
                    sum[c] += p[c] as f64;
                }
                n += 1.0;
            }
        }
        if n < 1.0 {
            return Grade { gains: [1.0; 3], exposure: 1.0 };
        }
        let g = sum[1] / n;
        let gains = [(sum[1] / sum[0].max(1e-9)) as f32, 1.0, (sum[1] / sum[2].max(1e-9)) as f32];
        Grade { gains: gains.map(|v| v.clamp(0.25, 4.0)), exposure: ((0.18 / g.max(1e-6)) as f32).clamp(0.1, 64.0) }
    }

    /// Rec.709 encoded value (0..1) of a linear camera RGB pixel.
    pub fn rec709(&self, p: [f32; 3]) -> [f32; 3] {
        let px = [0, 1, 2].map(|c| p[c] * self.gains[c] * self.exposure);
        let y = 0.2126 * px[0] + 0.7152 * px[1] + 0.0722 * px[2];
        [0, 1, 2].map(|c| {
            let x = (y + 1.35 * (px[c] - y)).max(0.0);
            let x = (x * (1.0 + x / 16.0) / (1.0 + x)).min(1.0);
            if x < 0.018 {
                4.5 * x
            } else {
                1.099 * x.powf(0.45) - 0.099
            }
        })
    }
}

fn to_rgb48(rgb: &[[f32; 3]], grade: &Grade, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(rgb.len() * 6);
    for p in rgb {
        for v in grade.rec709(*p) {
            out.extend_from_slice(&((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16).to_le_bytes());
        }
    }
}

/// Whether ffmpeg can read an audio track from the clip.
fn has_readable_audio(ffmpeg: &Path, clip: &Path) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-v", "error", "-i"])
        .arg(clip)
        .args(["-map", "0:a:0", "-t", "0.05", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    quiet(&mut cmd).status().map(|s| s.success()).unwrap_or(false)
}

/// Writes the proxy of one clip. `progress(fraction, fps)` is called after every batch of
/// frames; `cancel` stops it and removes the partial file. The proxy is written next to
/// its final name and renamed into place when complete.
pub fn make_proxy(o: &ProxyOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<ProxyOutcome> {
    let start = Instant::now();
    let (ffmpeg, encoder) = match (&o.ffmpeg, &o.encoder) {
        (Some(f), Some(e)) => (f.clone(), e.clone()),
        (f, e) => {
            let (df, de) = default_encoder()?;
            let ff = f.clone().unwrap_or(df);
            let enc = match e {
                Some(e) => e.clone(),
                None if f.is_some() => {
                    available_encoders(&ff).into_iter().next().ok_or_else(|| Error("no working HEVC encoder".into()))?
                }
                None => de,
            };
            (ff, enc)
        }
    };
    let input = InputFile::open(&o.input)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !is_supported_codec(video.codec) {
        return Err(Error("not a Nikon N-RAW or R3D NE clip".into()));
    }
    let n = video.samples.len();
    if n == 0 {
        return Err(Error("clip has no frames".into()));
    }
    let fps = quality::video_frame_rate(&movie);
    let fps = if fps > 0.0 { fps } else { 25.0 };
    let (ow, oh) = proxy_size(video.width, video.height, o.max_width);
    let (ow, oh) = (ow as usize, oh as usize);

    let decode = |i: usize| -> Result<Vec<[f32; 3]>> {
        let s = video.samples[i];
        let sample = input.read(s.offset, s.size as usize)?;
        let nf = NrawFrame::parse(&sample)?;
        frame_rgb(&sample[nf.codestream_offset..], ow, oh)
    };
    // One grade for the whole clip, from frames at 10%, 50% and 90%.
    let probes: Vec<usize> = [n / 10, n / 2, n * 9 / 10].into_iter().map(|i| i.min(n - 1)).collect();
    let samples = probes.par_iter().map(|&i| decode(i)).collect::<Result<Vec<_>>>()?;
    let grade = Grade::measure(&samples);

    let produce = |range: std::ops::Range<usize>| range.into_par_iter().map(decode).collect::<Result<Vec<_>>>();
    let audio = has_readable_audio(&ffmpeg, &o.input).then_some(o.input.as_path());
    let cbr_kbps = o.cbr.then(|| cbr_kbps(ow, oh, fps));
    let job = Encode {
        ffmpeg: &ffmpeg,
        encoder: &encoder,
        width: ow,
        height: oh,
        fps,
        frames: n,
        audio,
        output: &o.output,
        cbr_kbps,
    };
    encode(
        &job,
        &grade,
        cancel,
        &mut |done, total| progress(done as f64 / total as f64, done as f64 / start.elapsed().as_secs_f64().max(1e-6)),
        &produce,
    )?;
    Ok(ProxyOutcome {
        output: o.output.clone(),
        width: ow as u32,
        height: oh as u32,
        frames: n as u32,
        encoder,
        seconds: start.elapsed().as_secs_f64(),
        output_bytes: std::fs::metadata(&o.output).map(|m| m.len()).unwrap_or(0),
    })
}

/// One ffmpeg encode of `frames` linear camera RGB frames of `width` x `height`.
pub struct Encode<'a> {
    pub ffmpeg: &'a Path,
    pub encoder: &'a str,
    pub width: usize,
    pub height: usize,
    pub fps: f64,
    pub frames: usize,
    /// A file whose first audio track is muxed in as AAC.
    pub audio: Option<&'a Path>,
    pub output: &'a Path,
    /// Constant bitrate in kb/s; `None` is quality based VBR.
    pub cbr_kbps: Option<u32>,
}

/// Runs ffmpeg for `job`, feeding it the frames `produce` returns for consecutive ranges
/// (decoded in parallel batches), graded with `grade`. Writes `<output>.bluecompress.tmp`
/// and renames it into place on success; removes it on error or cancel.
pub fn encode(
    job: &Encode,
    grade: &Grade,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, usize),
    produce: &(dyn Fn(std::ops::Range<usize>) -> Result<Vec<Vec<[f32; 3]>>> + Sync),
) -> Result<()> {
    let (ffmpeg, encoder, ow, oh, fps, n, o_output) =
        (job.ffmpeg, job.encoder, job.width, job.height, job.fps, job.frames, job.output);
    if let Some(dir) = o_output.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error(format!("cannot create {}: {e}", dir.display())))?;
    }
    let mut tmp = o_output.as_os_str().to_owned();
    tmp.push(".bluecompress.tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb48le"]).args([
        "-s",
        &format!("{ow}x{oh}"),
        "-r",
        &format!("{fps}"),
        "-i",
        "-",
    ]);
    if let Some(a) = job.audio {
        cmd.arg("-i").arg(a).args(["-map", "0:v:0", "-map", "1:a:0", "-c:a", "aac", "-b:a", "256k", "-shortest"]);
    }
    cmd.args(["-vf", "scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv"])
        .args(encoder_args(encoder, job.cbr_kbps))
        .args(["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"])
        .args(["-f", "mov"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = quiet(&mut cmd).spawn().map_err(|e| Error(format!("cannot run {}: {e}", ffmpeg.display())))?;
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut stdin = child.stdin.take().unwrap();

    let batch = rayon::current_num_threads().clamp(2, 16);
    let mut buf = Vec::new();
    let mut result: Result<()> = Ok(());
    'outer: for chunk_start in (0..n).step_by(batch) {
        if cancel.load(Ordering::Relaxed) {
            result = Err(Error("cancelled".into()));
            break;
        }
        let frames = match produce(chunk_start..(chunk_start + batch).min(n)) {
            Ok(f) => f,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        for f in &frames {
            to_rgb48(f, grade, &mut buf);
            if let Err(e) = stdin.write_all(&buf) {
                result = Err(Error(format!("ffmpeg stopped reading: {e}")));
                break 'outer;
            }
        }
        progress(chunk_start + frames.len(), n);
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
    if let Err(e) = finish {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, o_output).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error(format!("cannot move proxy into place at {}: {e}", o_output.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(proxy_size(6060, 3410, 1920), (1920, 1080));
        assert_eq!(proxy_size(3852, 2168, 4096), (1926, 1084));
        assert_eq!(proxy_size(8280, 4664, 1920), (1920, 1082));
        let (w, h) = proxy_size(101, 51, 1920);
        assert!(w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn path_next_to_output() {
        let p = proxy_path(Path::new("/a/Compressed 1.6bpp/C0001.NEV"));
        assert_eq!(p, PathBuf::from("/a/Compressed 1.6bpp/Proxies/C0001.mov"));
    }

    #[test]
    fn grade_neutral_gray() {
        let g = Grade::measure(&[vec![[0.09, 0.18, 0.06]; 100]]);
        let v = g.rec709([0.09, 0.18, 0.06]);
        assert!((v[0] - v[1]).abs() < 1e-3 && (v[2] - v[1]).abs() < 1e-3);
        assert!(v[1] > 0.3 && v[1] < 0.5, "{v:?}");
        assert_eq!(g.rec709([0.0; 3]), [0.0; 3]);
    }

    #[test]
    fn downscale_quads() {
        // 4x2 quads of constant planes average to the same RGB.
        let planes = vec![vec![1000f32; 8], vec![2000f32; 8], vec![1000f32; 8], vec![3000f32; 8]];
        let out = downscale(&planes, 4, 2, 0.0, 2, 1);
        assert_eq!(out.len(), 2);
        assert!((out[0][0] - 2000.0 / 16383.0).abs() < 1e-6);
        assert!((out[0][1] - 1000.0 / 16383.0).abs() < 1e-6);
    }
}
