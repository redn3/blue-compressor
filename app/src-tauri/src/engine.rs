//! The engine the app drives: the real core, or (feature `mock`) a stand-in with realistic
//! numbers for working on the interface without decoding anything.

pub use bluecompress::{Backend, Codec, Options, Resolution, Target};

#[cfg(not(feature = "mock"))]
pub use bluecompress::{backend_description, output_size, preview_region_for, probe, thumbnail, transcode_staged};
#[cfg(not(feature = "mock"))]
pub use bluecompress::{make_proxy, proxy::encoder_description as proxy_encoder, r3d_job};

#[cfg(feature = "mock")]
pub use mock::{backend_description, output_size, preview_region_for, probe, thumbnail, transcode_staged};
#[cfg(feature = "mock")]
pub use mock::{make_proxy, proxy_encoder, r3d_job};

#[cfg(feature = "mock")]
mod mock {
    use super::*;
    use bluecompress::{ClipInfo, Outcome, Preview, Thumbnail};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Stand-in container bytes (audio, metadata, index) and NRAW header bytes per frame.
    const CONTAINER: u64 = 5_000_000;
    const FRAME_HEADER: u64 = 4096;

    /// Stand-in codestream bytes: what bits per pixel counts, as the core measures it.
    fn codestream(file_bytes: u64, frames: u64) -> u64 {
        file_bytes.saturating_sub(CONTAINER + FRAME_HEADER * frames)
    }

    pub fn probe(path: &Path) -> bluecompress::Result<ClipInfo> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(2_601_911_808);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains("broken") {
            return Err(bluecompress::Error("not a Nikon N-RAW or R3D NE clip".into()));
        }
        Ok(if ext == "r3d" {
            ClipInfo {
                codec: Codec::R3dNe,
                width: 6060,
                height: 3410,
                frames: 276,
                fps: 25.0,
                file_bytes,
                video_bytes: file_bytes - CONTAINER,
                codestream_bytes: codestream(file_bytes, 276),
                audio_tracks: 1,
                bpp: 8.0 * codestream(file_bytes, 276) as f64 / (276.0 * 6060.0 * 3410.0),
            }
        } else {
            ClipInfo {
                codec: Codec::NRaw,
                width: 8268,
                height: 4652,
                frames: 360,
                fps: 25.0,
                file_bytes,
                video_bytes: file_bytes - CONTAINER,
                codestream_bytes: codestream(file_bytes, 360),
                audio_tracks: 1,
                bpp: 8.0 * codestream(file_bytes, 360) as f64 / (360.0 * 8268.0 * 4652.0),
            }
        })
    }

    pub fn output_size(width: u32, height: u32, resolution: Resolution) -> (u32, u32) {
        match resolution {
            Resolution::K6 if width > 6060 => (6060, 3410),
            Resolution::K4 if width > 3852 => (3852, 2168),
            _ => (width, height),
        }
    }

    pub fn backend_description(backend: Backend) -> String {
        match backend {
            Backend::Cpu => "CPU (12 threads)".into(),
            Backend::Cuda => "not available (CUDA backend not built)".into(),
            Backend::OpenCl => "not available (OpenCL backend not built)".into(),
            _ => "Metal: Apple GPU".into(),
        }
    }

    fn keep_ratio(quality: u8, resolution: Resolution, width: u32) -> f64 {
        let q = match quality {
            1 => 0.91,
            2 => 0.82,
            3 => 0.63,
            _ => 0.43,
        };
        let (w, _) = output_size(width, 1, resolution);
        q * (f64::from(w) / f64::from(width)).powi(2)
    }

    /// Output size as a fraction of the source for any target: a bpp target fills its
    /// budget, but never more than the lossless repack.
    fn keep_for(target: Target, info: &ClipInfo, resolution: Resolution) -> f64 {
        match target.bpp_value() {
            None => keep_ratio(target.preset(), resolution, info.width),
            Some(b) => {
                let (w, h) = output_size(info.width, info.height, resolution);
                let area = f64::from(w) * f64::from(h) / (f64::from(info.width) * f64::from(info.height));
                (b / info.bpp).min(0.91) * area
            }
        }
    }

    /// The scene at quad (qx, qy) of a `qw` x `qh` quad grid: sky, hills and foliage-like
    /// texture, linear light. Defined per quad so any crop or zoom samples the same picture.
    fn scene(qx: f64, qy: f64, qw: f64, qh: f64, t: f64) -> [f64; 3] {
        let (fx, fy) = (qx / qw, qy / qh);
        let hill = 0.55 + 0.08 * (fx * 7.0 + t * 3.0).sin() + 0.04 * (fx * 23.0).sin();
        if fy < hill {
            [0.35 + 0.25 * (1.0 - fy), 0.5 + 0.2 * (1.0 - fy), 0.75 + 0.15 * (1.0 - fy)]
        } else {
            let leaf = 0.5 + 0.5 * ((fx * 180.0).sin() * (fy * 140.0 + t * 20.0).cos());
            let shade = 0.25 + 0.55 * leaf * (1.0 - (fy - hill) * 1.2);
            [0.18 * shade, 0.42 * shade + 0.05, 0.16 * shade]
        }
    }

    /// Sensor-like noise for one quad, fixed per frame.
    fn noise(qx: i64, qy: i64, frame: u32) -> f64 {
        let mut h = (qx as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (qy as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
            ^ u64::from(frame) << 32;
        h ^= h >> 31;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 29;
        (h >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }

    static LAST: std::sync::Mutex<Option<(std::path::PathBuf, u32, Target, Resolution)>> = std::sync::Mutex::new(None);

    /// Before and after of `crop` (fractions of the frame) at `width` pixels. The after image
    /// smooths the noise by the quality step and averages quad blocks when downscaling. A new
    /// frame or setting costs a simulated decode; panning and zooming only re-render.
    pub fn preview_region_for(
        path: &Path,
        frame: u32,
        target: Target,
        resolution: Resolution,
        _b: Backend,
        crop: [f64; 4],
        width: u32,
    ) -> bluecompress::Result<Preview> {
        let info = probe(path)?;
        let frame = frame.min(info.frames - 1);
        {
            let key = (path.to_path_buf(), frame, target, resolution);
            let mut last = LAST.lock().unwrap();
            if last.as_ref() != Some(&key) {
                std::thread::sleep(Duration::from_millis(450));
                *last = Some(key);
            } else {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        let (qw, qh) = (f64::from(info.width / 2), f64::from(info.height / 2));
        let w = width.clamp(16, 8192) as usize;
        let h = ((w as f64 * (crop[3] * qh) / (crop[2] * qw)).round() as usize).max(1);
        let t = f64::from(frame) / f64::from(info.frames.max(1));
        let strength = match target {
            Target::Quality(1) => 0.0,
            Target::Quality(2) => 0.25,
            Target::Quality(3) => 0.5,
            Target::Quality(_) => 0.85,
            Target::Bpp(_) => ((0.91 - keep_for(target, &info, Resolution::Original)) / 0.56).clamp(0.0, 0.85),
        };
        let (ow, _) = output_size(info.width, info.height, resolution);
        let block = ((f64::from(info.width) / f64::from(ow)).round() as i64).max(1);
        let mut before = Vec::with_capacity(w * h * 4);
        let mut after = Vec::with_capacity(w * h * 4);
        let to8 = |v: f64| (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
        for y in 0..h {
            let qy = ((crop[1] + crop[3] * (y as f64 + 0.5) / h as f64) * qh).floor();
            for x in 0..w {
                let qx = ((crop[0] + crop[2] * (x as f64 + 0.5) / w as f64) * qw).floor();
                let s = scene(qx, qy, qw, qh, t);
                let n = 0.035 * noise(qx as i64, qy as i64, frame);
                // After: the noise of a quad block (downscale) and its neighbourhood (requant).
                let (bx, by) = (qx as i64 / block * block, qy as i64 / block * block);
                let mut acc = 0.0;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        acc += noise(bx + dx * block, by + dy * block, frame);
                    }
                }
                let smooth = 0.035 * acc / 9.0;
                let own = if block > 1 { 0.035 * noise(bx, by, frame) } else { n };
                let na = own * (1.0 - strength) + smooth * strength;
                let sa = if block > 1 {
                    scene((bx as f64) + block as f64 / 2.0, (by as f64) + block as f64 / 2.0, qw, qh, t)
                } else {
                    s
                };
                for (c, k) in [1.0, 0.9, 1.1].iter().enumerate() {
                    before.push(to8(s[c] + n * k));
                    after.push(to8(sa[c] + na * k));
                }
                before.push(255);
                after.push(255);
            }
        }
        let source = info.codestream_bytes / u64::from(info.frames.max(1));
        Ok(Preview {
            width: w as u32,
            height: h as u32,
            before_rgba: before,
            after_rgba: after,
            source_frame_bytes: source,
            output_frame_bytes: (source as f64 * keep_for(target, &info, resolution) * (0.97 + 0.06 * t)) as u64,
            frames: info.frames,
        })
    }

    pub fn thumbnail(path: &Path, frame: u32, width: u32) -> bluecompress::Result<Thumbnail> {
        let p = preview_region_for(
            path,
            frame,
            Target::quality(1),
            Resolution::Original,
            Backend::Cpu,
            [0.0, 0.0, 1.0, 1.0],
            width,
        )?;
        Ok(Thumbnail { width: p.width, height: p.height, rgba: p.before_rgba })
    }

    pub fn proxy_encoder() -> String {
        "hevc_videotoolbox (GPU)".into()
    }

    /// Stand-in proxy: progress at about 200 fps, no file.
    pub fn make_proxy(
        o: &bluecompress::ProxyOptions,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f64, f64),
    ) -> bluecompress::Result<bluecompress::ProxyOutcome> {
        let info = probe(&o.input)?;
        let start = Instant::now();
        for f in 0..info.frames {
            if cancel.load(Ordering::Relaxed) {
                return Err(bluecompress::Error("cancelled".into()));
            }
            std::thread::sleep(Duration::from_secs_f64(1.0 / 200.0));
            progress(
                f64::from(f + 1) / f64::from(info.frames),
                f64::from(f + 1) / start.elapsed().as_secs_f64().max(1e-3),
            );
        }
        let (width, height) = bluecompress::proxy::proxy_size(info.width, info.height, o.max_width);
        Ok(bluecompress::ProxyOutcome {
            output: o.output.clone(),
            width,
            height,
            frames: info.frames,
            encoder: "hevc_videotoolbox".into(),
            seconds: start.elapsed().as_secs_f64(),
            output_bytes: u64::from(info.frames) * 60_000,
        })
    }

    /// Stand-in R3D NE step: progress over about half a second, no file.
    pub fn r3d_job(
        source: &std::path::Path,
        out: &std::path::Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f64, f64),
    ) -> bluecompress::Result<bluecompress::R3dJob> {
        let info = probe(source)?;
        for i in 1..=10 {
            if cancel.load(Ordering::Relaxed) {
                return Err(bluecompress::Error("cancelled".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
            progress(f64::from(i) / 10.0, 0.0);
        }
        Ok(bluecompress::R3dJob::Done(bluecompress::R3dOutcome {
            output: bluecompress::r3d_path(out, source),
            frames: info.frames,
            output_bytes: info.file_bytes,
            seconds: 0.5,
            iso: 1600,
        }))
    }

    /// The mock writes no files, so staging is the same as a plain run.
    pub fn transcode_staged(
        o: &Options,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f64, f64),
    ) -> bluecompress::Result<Outcome> {
        transcode(o, cancel, progress)
    }

    pub fn transcode(
        o: &Options,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f64, f64),
    ) -> bluecompress::Result<Outcome> {
        let info = probe(&o.input)?;
        let start = Instant::now();
        let resize = output_size(info.width, info.height, o.resolution) != (info.width, info.height);
        let fps_target = if resize {
            32.0
        } else if o.quality == 1 {
            120.0
        } else {
            48.0
        };
        for f in 0..info.frames {
            if cancel.load(Ordering::Relaxed) {
                return Err(bluecompress::Error("cancelled".into()));
            }
            std::thread::sleep(Duration::from_secs_f64(1.0 / fps_target));
            let el = start.elapsed().as_secs_f64();
            progress(f64::from(f + 1) / f64::from(info.frames), f64::from(f + 1) / el.max(1e-3));
        }
        let ratio = keep_for(Target::new(o.quality, o.bpp), &info, o.resolution);
        let seconds = start.elapsed().as_secs_f64();
        Ok(Outcome {
            input_bytes: info.file_bytes,
            output_bytes: (info.file_bytes as f64 * ratio) as u64,
            frames: info.frames,
            seconds,
            fps: f64::from(info.frames) / seconds,
            backend: if resize && o.backend != Backend::Cpu { "Metal (Apple GPU)".into() } else { "CPU".into() },
        })
    }
}
