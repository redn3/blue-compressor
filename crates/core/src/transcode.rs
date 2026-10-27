//! Clip-level transcoding: frame workers feeding an ordered writer.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bmff::{chunk_order, fourcc, fourcc_str, InputFile, Movie, OutputWriter, WriteOptions};
use crate::nraw::NrawFrame;
use crate::quality::{bpp_of, rank_for_quality, source_geometry, target_geometry, Target};
use crate::requant::{requant_codestream, RequantParams};
use crate::resize::{self, Resizer};
use crate::{Error, Options, Outcome, Result};

pub(crate) fn hardware_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

pub(crate) fn is_supported_codec(codec: u32) -> bool {
    codec == fourcc(b"NRAW") || codec == fourcc(b"NR3D")
}

/// True when `out` names the same file as `input` (the output may not exist yet).
fn same_file(input: &Path, out: &Path) -> bool {
    if input == out {
        return true;
    }
    let Ok(a) = input.canonicalize() else { return false };
    if let Ok(b) = out.canonicalize() {
        return a == b;
    }
    match (out.parent(), out.file_name()) {
        (Some(dir), Some(name)) => {
            let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
            dir.canonicalize().map(|d| d.join(name) == a).unwrap_or(false)
        }
        _ => false,
    }
}

struct Shared {
    done: Vec<Option<Vec<u8>>>,
    next: usize,
    written: usize,
    stop: bool,
    error: Option<String>,
}

pub fn transcode(opt: &Options, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<Outcome> {
    let t0 = Instant::now();
    if same_file(&opt.input, &opt.output) {
        return Err(Error("output must differ from input".into()));
    }
    let input = InputFile::open(&opt.input)?;
    let movie = Movie::parse(&input)?;
    let video = movie.video();
    if !is_supported_codec(video.codec) {
        return Err(Error(format!("not a Nikon N-RAW or R3D NE clip (codec {})", fourcc_str(video.codec))));
    }
    // A bpp target at or above the source's cannot add quality: lossless repack instead.
    let mut target = Target::new(opt.quality, opt.bpp);
    if let Some(b) = target.bpp_value() {
        let video_bytes: u64 = video.samples.iter().map(|s| s.size as u64).sum();
        let src_bpp = bpp_of(video_bytes as f64 / video.samples.len().max(1) as f64, video.width, video.height);
        if b >= src_bpp {
            target = Target::quality(1);
        }
    }
    let quality = target.preset();
    let src = source_geometry(&movie);
    let dst = target_geometry(src, opt.resolution);
    let resizing = dst.width != video.width || dst.height != video.height;

    let resizer: Option<Box<dyn Resizer>> = if resizing { Some(resize::create(opt.backend, src, dst)?) } else { None };
    let backend = resizer.as_ref().map_or_else(|| "CPU".to_string(), |r| r.name());

    let wopt = if resizing {
        WriteOptions {
            width: dst.width,
            height: dst.height,
            active_width: dst.active_width,
            active_height: dst.active_height,
        }
    } else {
        WriteOptions::default()
    };
    let mut writer = OutputWriter::create(&opt.output, &input, &movie, wopt)?;

    let threads = if opt.threads > 0 { opt.threads } else { hardware_threads() };
    let nframes = video.samples.len();
    // Resize jobs hold whole decoded frames; keep fewer in flight.
    let workers = if resizing { threads.clamp(2, 4) } else { threads.clamp(1, 16) };
    let max_in_flight = workers * 2;
    let target_rank = rank_for_quality(quality);
    let budget = target.budget(video.width, video.height);

    let shared = Mutex::new(Shared { done: vec![None; nframes], next: 0, written: 0, stop: false, error: None });
    let cv = Condvar::new();

    let process = |i: usize| -> Result<Vec<u8>> {
        let s = video.samples[i];
        let sample = input.read(s.offset, s.size as usize)?;
        let frame = NrawFrame::parse(&sample)?;
        let cs = &sample[frame.codestream_offset..];
        if let Some(r) = &resizer {
            frame.build(&r.process(cs, target)?, Some((dst.width, dst.height)))
        } else {
            let p = RequantParams { target_rank, budget, ..Default::default() };
            frame.build(&requant_codestream(cs, &p, 1)?.0, None)
        }
    };

    let mut input_bytes = 0u64;
    let mut frames = 0u32;
    let result: Result<()> = std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = {
                    let mut g = shared.lock().unwrap();
                    while !g.stop && g.next < nframes && g.next >= g.written + max_in_flight {
                        g = cv.wait(g).unwrap();
                    }
                    if g.stop || g.next >= nframes {
                        return;
                    }
                    g.next += 1;
                    g.next - 1
                };
                let r = process(i);
                let mut g = shared.lock().unwrap();
                match r {
                    Ok(out) => g.done[i] = Some(out),
                    Err(e) => {
                        if g.error.is_none() {
                            g.error = Some(format!("frame {i}: {e}"));
                        }
                        g.stop = true;
                    }
                }
                cv.notify_all();
            });
        }

        let mut run = || -> Result<()> {
            let mut last = Instant::now() - Duration::from_secs(1);
            for r in chunk_order(&movie) {
                if r.track != movie.video_track {
                    writer.copy_chunk(r.track, r.chunk)?;
                    continue;
                }
                let ch = video.chunks[r.chunk];
                for s in ch.first_sample as usize..(ch.first_sample + ch.samples) as usize {
                    let data = {
                        let mut g = shared.lock().unwrap();
                        while !g.stop && g.done[s].is_none() {
                            g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
                            if cancel.load(Ordering::Relaxed) {
                                g.stop = true;
                            }
                        }
                        let Some(d) = g.done[s].take() else {
                            return Err(Error(g.error.clone().unwrap_or_else(|| "cancelled by user".into())));
                        };
                        g.written = s + 1;
                        cv.notify_all();
                        d
                    };
                    input_bytes += video.samples[s].size as u64;
                    writer.write_video_sample(s, &data)?;
                    frames += 1;
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error("cancelled by user".into()));
                    }
                    let now = Instant::now();
                    if now - last > Duration::from_millis(100) || frames as usize == nframes {
                        last = now;
                        let secs = (now - t0).as_secs_f64();
                        progress(frames as f64 / nframes as f64, if secs > 0.0 { frames as f64 / secs } else { 0.0 });
                    }
                }
            }
            Ok(())
        };
        let r = run();
        let mut g = shared.lock().unwrap();
        g.stop = true;
        cv.notify_all();
        r
    });
    result?;
    let _ = input_bytes;
    let output_bytes = writer.finish()?;
    let seconds = t0.elapsed().as_secs_f64();
    Ok(Outcome {
        input_bytes: input.size(),
        output_bytes,
        frames,
        seconds,
        fps: if seconds > 0.0 { frames as f64 / seconds } else { 0.0 },
        backend,
    })
}
