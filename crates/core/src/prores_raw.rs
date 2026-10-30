//! ProRes RAW input: a Nikon ProRes RAW clip (.MOV) re-encoded to compressed N-RAW, and from
//! there to R3D NE, using a clip recorded by the same camera as the template.
//!
//! This is not lossless: ProRes RAW frames are decoded (by ffmpeg's ProRes RAW decoder, to
//! the linear Bayer mosaic before demosaicing), mapped to the template's sensor encoding and
//! coded again with our TICO RAW encoder. What comes from where:
//!
//! - template: sample descriptions, track layout, the per-frame N-RAW header, the TICO RAW
//!   picture header (transfer curve, weights) and the tags that describe the codec;
//! - ProRes RAW clip: the image, frame count and rate, durations, audio and timecode (copied
//!   as they are; Nikon writes the same PCM and timecode formats in both), and every other
//!   camera tag (dates, exposure, lens, white balance, maker notes, preview images).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::bmff::{chunk_order, fourcc, InputFile, Movie};
use crate::frame::{forward_star_tetrix, forward_wavelet, Coeffs, Planes, Transfer};
use crate::nraw::NrawFrame;
use crate::quality::{bpp_of, Target};
use crate::r3d::{self, nctg_tags, tag_value, Direction};
use crate::resize::{encode_target, PHASE_X, PHASE_Y};
use crate::tico::{Layout, TicoHeader};
use crate::{Error, Outcome, Result};

/// Video sample entry fourccs of ProRes RAW and ProRes RAW HQ.
const CODECS: [&[u8; 4]; 2] = [b"aprn", b"aprh"];

/// Raw value of the sensor's white in N-RAW (14 bit).
const NRAW_WHITE: f32 = 16383.0;

/// Nikon writes ProRes RAW at twice the scale of its N-RAW sensor values (above black),
/// whatever range the frame header declares: measured on a same-scene, same-setting ZR pair
/// (ProRes RAW and R3D NE), the green channel above black is 1.92, 2.01, 2.01 and 2.02 times
/// the R3D NE value at the 10th, 50th, 90th and 99th percentiles, and 2.00 at the maximum.
pub const NIKON_SCALE: f32 = 0.5;

fn err<T>(s: impl Into<String>) -> Result<T> {
    Err(Error(s.into()))
}

/// True when `path` is a ProRes RAW QuickTime clip.
pub fn is_prores_raw(path: &Path) -> bool {
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mov")) {
        return false;
    }
    let Ok(i) = InputFile::open(path) else { return false };
    Movie::parse(&i).is_ok_and(|m| CODECS.iter().any(|c| m.video().codec == fourcc(c)))
}

/// What the header of a ProRes RAW frame (`prrf`) says about the sensor data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub vendor: [u8; 4],
    /// 0 = RGGB (the only layout the decoder takes).
    pub bayer: u16,
    pub black: u16,
    pub white: u16,
    pub wb: [f32; 2],
}

/// Parses the header of one ProRes RAW frame (at least its first 30 bytes).
pub fn frame_header(p: &[u8]) -> Result<FrameHeader> {
    if p.len() < 30 || &p[4..8] != b"prrf" {
        return err("not a ProRes RAW frame");
    }
    let h = &p[10..];
    let be16 = |o: usize| u16::from_be_bytes([h[o], h[o + 1]]);
    let f32at = |o: usize| f32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
    if h[1] > 1 {
        return err(format!("ProRes RAW frame version {} is not supported", h[1]));
    }
    Ok(FrameHeader {
        vendor: [h[2], h[3], h[4], h[5]],
        width: be16(6) as u32,
        height: be16(8) as u32,
        bayer: be16(14) & 3,
        black: 0x100,
        white: be16(16).saturating_add(0x100),
        wb: if p.len() >= 36 { [f32at(18), f32at(22)] } else { [0.0; 2] },
    })
}

/// Linear map from ProRes RAW values to N-RAW raw values: raw = black + (v - from) * scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelMap {
    pub from: f32,
    pub black: f32,
    pub scale: f32,
}

impl LevelMap {
    /// The map for frames with header `h` into a template with sensor black `black`.
    pub fn new(h: &FrameHeader, black: f32) -> LevelMap {
        let scale = if &h.vendor == b"nik0" {
            NIKON_SCALE
        } else {
            (NRAW_WHITE - black) / (h.white as f32 - h.black as f32).max(1.0)
        };
        LevelMap { from: h.black as f32, black, scale }
    }

    #[inline]
    pub fn apply(&self, v: u16) -> f32 {
        (self.black + (v as f32 - self.from) * self.scale).clamp(0.0, NRAW_WHITE)
    }
}

/// An RGGB mosaic (`w` x `h`, row major) to the encoder's four quad planes (G1, R, G2, B)
/// in the template's code domain.
pub fn mosaic_to_planes(mosaic: &[u16], w: usize, h: usize, map: &LevelMap, tr: &Transfer) -> Planes {
    let (cw, ch) = (w / 2, h / 2);
    let mut pl = Planes::new(cw, ch);
    pl.data.par_chunks_mut(cw).enumerate().for_each(|(row, out)| {
        let (c, y) = (row / ch, row % ch);
        let line = &mosaic[(2 * y + PHASE_Y[c]) * w..];
        for (x, o) in out.iter_mut().enumerate() {
            *o = tr.to_code(map.apply(line[2 * x + PHASE_X[c]]));
        }
    });
    pl
}

/// The template in N-RAW form: its movie, the N-RAW frame header to give every frame, the
/// TICO RAW picture header and the codec tags to set on the output.
struct Template {
    input: InputFile,
    movie: Movie,
    frame: NrawFrame,
    tico: TicoHeader,
    tags: Vec<(u32, u16, Vec<u8>)>,
    model: String,
}

fn model_of(m: &Movie) -> String {
    let t = nctg_tags(m);
    tag_value(m, &t, 0x2)
        .map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').trim().to_string())
        .unwrap_or_default()
}

/// Tags that describe the codec rather than the shot: from the template (in N-RAW form).
const TEMPLATE_TAGS: [u32; 3] = [0x24, 0x1017, 0x1018];

fn load_template(path: &Path) -> Result<Template> {
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let v = movie.video();
    let r3d = v.codec == fourcc(b"NR3D");
    if v.codec != fourcc(b"NRAW") && !r3d {
        return err(format!("{} is not an N-RAW or R3D NE clip", path.display()));
    }
    let s = *v.samples.first().ok_or_else(|| Error("template has no frames".into()))?;
    let mut sample = input.read(s.offset, s.size as usize)?;
    if r3d {
        sample = r3d::rewrite_frame(&sample, Direction::ToNev)?;
    }
    let frame = NrawFrame::parse(&sample)?;
    let (tico, _) = TicoHeader::parse(&sample[frame.codestream_offset..])?;
    let t = nctg_tags(&movie);
    let mut tags: Vec<(u32, u16, Vec<u8>)> = t
        .iter()
        .filter(|x| TEMPLATE_TAGS.contains(&x.id))
        .map(|x| (x.id, x.ty, movie.moov[x.value..x.value + x.len].to_vec()))
        .collect();
    // The N-RAW codec tags (an R3D NE template carries the R3D NE ones).
    let c = r3d::NEV_CODES;
    tags.extend([
        (0x21, 3, c[0].to_be_bytes().to_vec()),
        (0x26, 3, c[1].to_be_bytes().to_vec()),
        (0x27, 3, c[2].to_be_bytes().to_vec()),
        (0x1020, 7, vec![0; 12]),
        (0x523d, 7, vec![0; 256]),
        (0x0200_00c0, 7, r3d::NEV_C0.to_vec()),
    ]);
    let model = model_of(&movie);
    Ok(Template { input, movie, frame, tico, tags, model })
}

/// Looks for a same-camera N-RAW or R3D NE clip of the same frame size near `clip`: in its
/// folder, the folder above, and their subfolders (two levels), N-RAW first, newest first.
pub fn find_template(clip: &Path) -> Option<PathBuf> {
    let i = InputFile::open(clip).ok()?;
    let m = Movie::parse(&i).ok()?;
    let (model, w, h) = (model_of(&m), m.video().width, m.video().height);
    if model.is_empty() {
        return None;
    }
    let dir = clip.parent()?.to_path_buf();
    let mut roots = vec![dir.clone()];
    if let Some(p) = dir.parent() {
        roots.push(p.to_path_buf());
    }
    let mut found: Vec<(bool, std::time::SystemTime, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    fn walk(d: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if p.is_dir() {
                if depth > 0 {
                    walk(&p, depth - 1, out);
                }
            } else if r3d::is_nev(&p) || r3d::is_r3d(&p) {
                out.push(p);
            }
        }
    }
    for r in roots {
        let mut files = Vec::new();
        walk(&r, 2, &mut files);
        for p in files {
            if !seen.insert(p.clone()) {
                continue;
            }
            let ok = InputFile::open(&p)
                .ok()
                .and_then(|i| Movie::parse(&i).ok())
                .is_some_and(|t| t.video().width == w && t.video().height == h && model_of(&t) == model);
            if ok {
                let t = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((r3d::is_nev(&p), t, p));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().next().map(|f| f.2)
}

/// An ffmpeg with the ProRes RAW decoder (FFmpeg 8 or later).
pub fn find_decoder() -> Result<PathBuf> {
    let Some(ff) = crate::proxy::find_ffmpeg() else {
        return err("ProRes RAW needs ffmpeg 8 or later (none found; set BLUECOMPRESS_FFMPEG)");
    };
    let mut cmd = Command::new(&ff);
    crate::proxy::quiet(&mut cmd);
    let out = cmd.args(["-hide_banner", "-decoders"]).stdin(Stdio::null()).stderr(Stdio::null()).output();
    match out {
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("prores_raw") => Ok(ff),
        _ => err(format!("{} has no ProRes RAW decoder (needs ffmpeg 8 or later)", ff.display())),
    }
}

/// Options of a ProRes RAW job.
#[derive(Debug, Clone)]
pub struct PrrOptions {
    pub input: PathBuf,
    /// The N-RAW output.
    pub output: PathBuf,
    /// Same-camera N-RAW or R3D NE clip; None picks one with [`find_template`].
    pub template: Option<PathBuf>,
    /// Size target, resolved against the ProRes RAW clip's own bits per pixel; lossless
    /// (there is no lossless path) codes at that density.
    pub target: Target,
    /// Pad every frame to the frame budget.
    pub cbr: bool,
    pub threads: usize,
}

/// The size target of a job from a ProRes RAW clip with `source_bpp` bits per pixel.
pub fn resolve_target(t: Target, source_bpp: f64) -> Target {
    let t = t.resolve(source_bpp);
    if t.is_lossless() {
        Target::bpp((source_bpp * 10.0).round().clamp(crate::BPP_MIN * 10.0, crate::BPP_MAX * 10.0) / 10.0)
    } else {
        t
    }
}

struct Shared {
    done: Vec<Option<Vec<u8>>>,
    written: usize,
    stop: bool,
    error: Option<String>,
}

/// Re-encodes a ProRes RAW clip to N-RAW (see the module docs). The output is written to
/// `<output>.partial` and renamed when complete.
pub fn convert(o: &PrrOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<Outcome> {
    let t0 = Instant::now();
    let input = InputFile::open(&o.input)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !CODECS.iter().any(|c| video.codec == fourcc(c)) {
        return err("not a ProRes RAW clip");
    }
    let nframes = video.samples.len();
    if nframes == 0 {
        return err("clip has no frames");
    }
    let head = input.read(video.samples[0].offset, (video.samples[0].size as usize).min(256))?;
    let fh = frame_header(&head)?;
    if fh.bayer != 0 {
        return err("only RGGB ProRes RAW is supported");
    }
    let tpath = match &o.template {
        Some(p) => p.clone(),
        None => find_template(&o.input).ok_or_else(|| {
            Error(
                "no same-camera N-RAW or R3D NE clip of this frame size found near the clip to use as a template"
                    .into(),
            )
        })?,
    };
    let tpl = load_template(&tpath)?;
    let model = model_of(&movie);
    if !model.is_empty() && !tpl.model.is_empty() && model != tpl.model {
        return err(format!("template is from a {}, the clip from a {}", tpl.model, model));
    }
    let (w, h) = (fh.width as usize, fh.height as usize);
    if (tpl.tico.width as usize, tpl.tico.height as usize) != (w, h) {
        return err(format!(
            "template frames are {}x{}, the clip's {w}x{h}: use a template recorded at the same size",
            tpl.tico.width, tpl.tico.height
        ));
    }
    let layout = Layout::make(&tpl.tico, w, h)?;
    let tr = Transfer::new(&tpl.tico);
    let map = LevelMap::new(&fh, tr.black as f32);

    let src_bytes: u64 = video.samples.iter().map(|s| s.size as u64).sum();
    let src_bpp = bpp_of(src_bytes as f64 / nframes as f64, w as u32, h as u32);
    let target = resolve_target(o.target, src_bpp);
    let budget = target.frame_budget(w as u32, h as u32);
    let pad_to = if o.cbr { budget } else { None };

    // Output metadata: the clip's own tags with the template's codec tags.
    let mut tags: Vec<(u32, u16, Vec<u8>)> = nctg_tags(&movie)
        .iter()
        .map(|t| (t.id, t.ty, movie.moov[t.value..t.value + t.len].to_vec()))
        .filter(|t| !tpl.tags.iter().any(|x| x.0 == t.0))
        .collect();
    tags.extend(tpl.tags.iter().cloned());
    tags.sort_by_key(|t| t.0);
    let udta = r3d::build_udta(&movie, &tags)?;

    let pairs = crate::bmff::track_pairs(&tpl.movie, &movie);
    let vpair = pairs.iter().find(|p| p.0 == tpl.movie.video_track).copied();
    if vpair.map(|p| p.1) != Some(movie.video_track) {
        return err("template and clip video tracks do not match");
    }
    let copy: Vec<bool> = (0..movie.tracks.len()).map(|i| pairs.iter().any(|p| p.1 == i)).collect();

    // Decoder: the whole sensor area (no recommended crop), 16-bit little endian RGGB.
    let ff = find_decoder()?;
    let mut cmd = Command::new(&ff);
    crate::proxy::quiet(&mut cmd);
    cmd.args(["-v", "error", "-nostdin", "-apply_cropping", "0", "-i"])
        .arg(&o.input)
        .args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "bayer_rggb16le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error(format!("cannot start {}: {e}", ff.display())))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let errtext = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let threads = if o.threads > 0 { o.threads } else { crate::transcode::hardware_threads() };
    let workers = threads.clamp(1, 4);
    let shared = Mutex::new(Shared { done: vec![None; nframes], written: 0, stop: false, error: None });
    let cv = Condvar::new();
    let queue: Mutex<Vec<(usize, Vec<u16>)>> = Mutex::new(Vec::new());
    let qcv = Condvar::new();
    let reading_done = AtomicBool::new(false);
    let frame_bytes = w * h * 2;

    let encode = |mosaic: &[u16]| -> Result<Vec<u8>> {
        let mut pl = mosaic_to_planes(mosaic, w, h, &map, &tr);
        forward_star_tetrix(&mut pl);
        let mut coeffs = vec![0i32; Coeffs::len(&layout)];
        forward_wavelet(&layout, &pl, &mut coeffs);
        drop(pl);
        let cs = encode_target(&tpl.tico, &layout, &coeffs, target, &mut Vec::new())?;
        tpl.frame.build_padded(&cs, None, pad_to)
    };

    let fail = |g: &mut Shared, e: String| {
        if g.error.is_none() {
            g.error = Some(e);
        }
        g.stop = true;
    };

    let mut frames = 0u32;
    let out_tmp = {
        let mut s = o.output.as_os_str().to_owned();
        s.push(".partial");
        PathBuf::from(s)
    };
    let result: Result<u64> = std::thread::scope(|scope| {
        // Reader: frames from the decoder, a few ahead of the encoders.
        scope.spawn(|| {
            for i in 0..nframes {
                let mut buf = vec![0u8; frame_bytes];
                if let Err(e) = stdout.read_exact(&mut buf) {
                    fail(&mut shared.lock().unwrap(), format!("frame {i}: decoder stopped ({e})"));
                    cv.notify_all();
                    break;
                }
                let px: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
                drop(buf);
                let mut q = queue.lock().unwrap();
                while q.len() >= workers && !shared.lock().unwrap().stop {
                    q = qcv.wait_timeout(q, Duration::from_millis(50)).unwrap().0;
                }
                if shared.lock().unwrap().stop {
                    break;
                }
                q.push((i, px));
                qcv.notify_all();
            }
            reading_done.store(true, Ordering::SeqCst);
            qcv.notify_all();
        });
        for _ in 0..workers {
            scope.spawn(|| loop {
                let job = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if shared.lock().unwrap().stop {
                            return;
                        }
                        if !q.is_empty() {
                            // Oldest first, and not too far ahead of the writer.
                            let k = (0..q.len()).min_by_key(|&k| q[k].0).unwrap();
                            break q.swap_remove(k);
                        }
                        if reading_done.load(Ordering::SeqCst) {
                            return;
                        }
                        q = qcv.wait_timeout(q, Duration::from_millis(50)).unwrap().0;
                    }
                };
                qcv.notify_all();
                let r = encode(&job.1);
                drop(job.1);
                let mut g = shared.lock().unwrap();
                match r {
                    Ok(b) => g.done[job.0] = Some(b),
                    Err(e) => fail(&mut g, format!("frame {}: {e}", job.0)),
                }
                cv.notify_all();
            });
        }

        let r = (|| -> Result<u64> {
            let mut w = crate::bmff::RawWriter::create(&out_tmp, &tpl.input, &tpl.movie, &movie)?;
            let mut offsets: Vec<Vec<u64>> = movie.tracks.iter().map(|t| vec![0; t.chunks.len()]).collect();
            let mut sizes = vec![0u32; nframes];
            let mut last = Instant::now() - Duration::from_secs(1);
            for c in chunk_order(&movie) {
                if c.track != movie.video_track {
                    if copy[c.track] {
                        offsets[c.track][c.chunk] = w.pos();
                        let ch = movie.tracks[c.track].chunks[c.chunk];
                        w.copy_from(&input, ch.offset, ch.size)?;
                    }
                    continue;
                }
                let ch = video.chunks[c.chunk];
                offsets[c.track][c.chunk] = w.pos();
                for s in ch.first_sample as usize..(ch.first_sample + ch.samples) as usize {
                    let data = {
                        let mut g = shared.lock().unwrap();
                        while !g.stop && g.done[s].is_none() {
                            g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
                            if cancel.load(Ordering::Relaxed) {
                                fail(&mut g, "cancelled by user".into());
                            }
                        }
                        let Some(d) = g.done[s].take() else {
                            return Err(Error(g.error.clone().unwrap_or_else(|| "cancelled by user".into())));
                        };
                        g.written = s + 1;
                        d
                    };
                    sizes[s] = data.len() as u32;
                    w.write(&data)?;
                    frames += 1;
                    let now = Instant::now();
                    if now - last > Duration::from_millis(100) || frames as usize == nframes {
                        last = now;
                        let secs = (now - t0).as_secs_f64();
                        progress(frames as f64 / nframes as f64, if secs > 0.0 { frames as f64 / secs } else { 0.0 });
                    }
                }
            }
            let moov = crate::bmff::moov_from_template(&tpl.movie, &movie, &sizes, &offsets, &udta, fourcc(b"NRAW"))?;
            w.finish(&moov, &o.output)
        })();
        let mut g = shared.lock().unwrap();
        g.stop = true;
        drop(g);
        cv.notify_all();
        qcv.notify_all();
        r
    });
    let _ = child.kill();
    let _ = child.wait();
    let etext = errtext.join().unwrap_or_default();
    let output_bytes = match result {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&out_tmp);
            let extra = etext.lines().last().map(|l| format!(" ({l})")).unwrap_or_default();
            return Err(Error(format!("{}{}", e.0, if e.0.contains("decoder") { extra } else { String::new() })));
        }
    };
    let seconds = t0.elapsed().as_secs_f64();
    Ok(Outcome {
        input_bytes: input.size(),
        output_bytes,
        frames,
        seconds,
        fps: if seconds > 0.0 { frames as f64 / seconds } else { 0.0 },
        backend: format!(
            "CPU, ProRes RAW re-encode, template {}",
            tpath.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        ),
    })
}

/// One frame of a ProRes RAW clip for the previewer, in N-RAW raw values: the four quad
/// planes (G1, R, G2, B, each `w` x `h`) as decoded, and as they come out of the re-encode
/// at `target` (the same as the decoded frame when no template is found near the clip).
pub struct PreviewFrame {
    pub w: usize,
    pub h: usize,
    pub black: f32,
    pub before: Vec<Vec<u16>>,
    pub after: Vec<Vec<u16>>,
    pub source_frame_bytes: u64,
    pub output_frame_bytes: u64,
    pub frames: u32,
}

/// Number of frames of a ProRes RAW clip.
pub fn frame_count(path: &Path) -> Result<u32> {
    let input = InputFile::open(path)?;
    Ok(Movie::parse(&input)?.video().samples.len() as u32)
}

/// Decodes frame `frame` (clamped) of a ProRes RAW clip to its full RGGB mosaic with ffmpeg.
fn decode_mosaic(path: &Path, movie: &Movie, frame: usize, w: usize, h: usize) -> Result<Vec<u16>> {
    let v = movie.video();
    let ts = f64::from(v.timescale.max(1));
    let delta = f64::from(v.sample_delta.max(1));
    // A quarter frame before the frame's start time: accurate seeking keeps that frame.
    let t = ((frame as f64 - 0.25) * delta / ts).max(0.0);
    let ff = find_decoder()?;
    let mut cmd = Command::new(&ff);
    crate::proxy::quiet(&mut cmd);
    cmd.args(["-v", "error", "-nostdin", "-apply_cropping", "0", "-ss", &format!("{t:.6}"), "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "bayer_rggb16le", "-"])
        .stdin(Stdio::null());
    let out = cmd.output().map_err(|e| Error(format!("cannot start {}: {e}", ff.display())))?;
    if out.stdout.len() < w * h * 2 {
        let e = String::from_utf8_lossy(&out.stderr);
        return err(format!("ProRes RAW frame {frame}: decoder gave no picture ({})", e.trim()));
    }
    Ok(out.stdout[..w * h * 2].as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect())
}

/// Black of N-RAW clips, used when no template is near the clip.
const NRAW_BLACK: f32 = 1008.0;

/// The previewer's frame of a ProRes RAW clip (see [`PreviewFrame`]).
/// With `target` None only decodes (`after` is then the decoded frame too).
pub fn preview_frame(path: &Path, frame: u32, target: Option<Target>) -> Result<PreviewFrame> {
    let input = InputFile::open(path)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !CODECS.iter().any(|c| video.codec == fourcc(c)) {
        return err("not a ProRes RAW clip");
    }
    let nframes = video.samples.len();
    if nframes == 0 {
        return err("clip has no frames");
    }
    let i = (frame as usize).min(nframes - 1);
    let head = input.read(video.samples[i].offset, (video.samples[i].size as usize).min(256))?;
    let fh = frame_header(&head)?;
    if fh.bayer != 0 {
        return err("only RGGB ProRes RAW is supported");
    }
    let (w, h) = (fh.width as usize, fh.height as usize);
    let mosaic = decode_mosaic(path, &movie, i, w, h)?;
    let (cw, ch) = (w / 2, h / 2);
    let source_frame_bytes = video.samples[i].size as u64;
    let tpl = target
        .and_then(|_| find_template(path))
        .and_then(|t| load_template(&t).ok())
        .filter(|t| (t.tico.width as usize, t.tico.height as usize) == (w, h));
    let black = tpl.as_ref().map_or(NRAW_BLACK, |t| Transfer::new(&t.tico).black as f32);
    let map = LevelMap::new(&fh, black);
    let before: Vec<Vec<u16>> = (0..4)
        .map(|c| {
            let mut pl = vec![0u16; cw * ch];
            pl.par_chunks_mut(cw).enumerate().for_each(|(y, out)| {
                let line = &mosaic[(2 * y + PHASE_Y[c]) * w..];
                for (x, o) in out.iter_mut().enumerate() {
                    *o = map.apply(line[2 * x + PHASE_X[c]]).round() as u16;
                }
            });
            pl
        })
        .collect();
    let Some(tpl) = tpl else {
        return Ok(PreviewFrame {
            w: cw,
            h: ch,
            black,
            after: before.clone(),
            before,
            source_frame_bytes,
            output_frame_bytes: source_frame_bytes,
            frames: nframes as u32,
        });
    };
    let layout = Layout::make(&tpl.tico, w, h)?;
    let tr = Transfer::new(&tpl.tico);
    let src_bytes: u64 = video.samples.iter().map(|s| s.size as u64).sum();
    let target = resolve_target(target.unwrap_or(Target::quality(1)), bpp_of(src_bytes as f64 / nframes as f64, w as u32, h as u32));
    let mut pl = mosaic_to_planes(&mosaic, w, h, &map, &tr);
    drop(mosaic);
    forward_star_tetrix(&mut pl);
    let mut coeffs = vec![0i32; Coeffs::len(&layout)];
    forward_wavelet(&layout, &pl, &mut coeffs);
    drop(pl);
    let cs = encode_target(&tpl.tico, &layout, &coeffs, target, &mut Vec::new())?;
    let after = crate::preview::decode_planes(&cs)?;
    Ok(PreviewFrame {
        w: cw,
        h: ch,
        black,
        before,
        after,
        source_frame_bytes,
        output_frame_bytes: cs.len() as u64,
        frames: nframes as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(range: u16, vendor: &[u8; 4], bayer: u16) -> Vec<u8> {
        let mut p = vec![0u8; 72];
        p[4..8].copy_from_slice(b"prrf");
        p[8..10].copy_from_slice(&72u16.to_be_bytes());
        let h = &mut p[10..];
        h[2..6].copy_from_slice(vendor);
        h[6..8].copy_from_slice(&6060u16.to_be_bytes());
        h[8..10].copy_from_slice(&3410u16.to_be_bytes());
        h[10..14].copy_from_slice(&[6, 6, 4, 4]);
        h[14..16].copy_from_slice(&bayer.to_be_bytes());
        h[16..18].copy_from_slice(&range.to_be_bytes());
        h[18..22].copy_from_slice(&1.70117f32.to_be_bytes());
        h[22..26].copy_from_slice(&1.49023f32.to_be_bytes());
        p
    }

    #[test]
    fn reads_the_frame_header() {
        let h = frame_header(&header(65279, b"nik0", 0)).unwrap();
        assert_eq!((h.width, h.height, h.bayer, h.black, h.white), (6060, 3410, 0, 256, 65535));
        assert_eq!(&h.vendor, b"nik0");
        assert!((h.wb[0] - 1.70117).abs() < 1e-4);
        assert!(frame_header(b"\0\0\0\x48icpf..........................").is_err());
    }

    #[test]
    fn nikon_levels_halve_above_black() {
        let h = frame_header(&header(65279, b"nik0", 0)).unwrap();
        let m = LevelMap::new(&h, 1008.0);
        assert_eq!(m.apply(256), 1008.0);
        assert_eq!(m.apply(256 + 2000), 2008.0);
        assert_eq!(m.apply(65535), 16383.0);
        assert_eq!(m.apply(0), 880.0);
        // Other vendors: declared black and white map to the template's.
        let o = LevelMap::new(&frame_header(&header(4095 - 256, b"xxxx", 0)).unwrap(), 1008.0);
        assert!((o.apply(4095) - 16383.0).abs() < 0.01 && o.apply(256) == 1008.0);
    }

    fn test_header() -> TicoHeader {
        TicoHeader { black: 1008, gain: 131072, knee: 32768, ..Default::default() }
    }

    #[test]
    fn mosaic_lands_on_the_right_planes() {
        let (w, h) = (8usize, 6usize);
        // R, G1, G2, B each with its own level, RGGB.
        let level = |x: usize, y: usize| -> u16 {
            match (y % 2, x % 2) {
                (0, 0) => 1256,
                (0, 1) => 2256,
                (1, 0) => 3256,
                _ => 4256,
            }
        };
        let mosaic: Vec<u16> = (0..w * h).map(|i| level(i % w, i / w)).collect();
        let hd = frame_header(&header(65279, b"nik0", 0)).unwrap();
        let map = LevelMap::new(&hd, 1008.0);
        let tr = Transfer::new(&test_header());
        let pl = mosaic_to_planes(&mosaic, w, h, &map, &tr);
        // Planes are G1 (row 0, col 1), R, G2 (row 1, col 0), B.
        let want = [1008.0 + 1000.0, 1008.0 + 500.0, 1008.0 + 1500.0, 1008.0 + 2000.0];
        for c in 0..4 {
            for &v in pl.plane(c) {
                let raw = tr.to_raw_f(v as f32);
                assert!((raw - want[c]).abs() < 2.0, "plane {c}: {raw} vs {}", want[c]);
            }
        }
    }

    #[test]
    fn lossless_target_codes_at_the_source_density() {
        assert_eq!(resolve_target(Target::lossless(), 3.62).bpp_value(), Some(3.6));
        assert_eq!(resolve_target(Target::bpp(2.0), 3.62).bpp_value(), Some(2.0));
        assert_eq!(resolve_target(Target::lossless(), 20.0).bpp_value(), Some(crate::BPP_MAX));
    }
}
