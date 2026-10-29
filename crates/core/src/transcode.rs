//! Clip-level transcoding: frame workers feeding an ordered writer.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bmff::{chunk_order, fourcc, fourcc_str, InputFile, Movie, OutputWriter, WriteOptions};
use crate::nraw::NrawFrame;
use crate::quality::{rank_for_quality, source_bpp, source_geometry, target_geometry, Target};
use crate::requant::{requant_codestream_seeded, RequantParams};
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

/// Frames per run of rate controlled frames (see `transcode`).
const FRAMES_PER_RUN: usize = 4;

struct Shared {
    done: Vec<Option<Vec<u8>>>,
    next: usize,
    written: usize,
    stop: bool,
    error: Option<String>,
}

pub fn transcode(opt: &Options, cancel: &AtomicBool, progress: &mut dyn FnMut(f64, f64)) -> Result<Outcome> {
    transcode_with(opt, None, cancel, progress)
}

/// [`transcode`], writing R3D NE with `r3d` (see [`crate::r3d`]): R3D NE output is always
/// constant bitrate. A lossless job copies the camera codestreams as they are (padded to
/// the largest frame); a resize without a bpp target codes to the source's bpp.
pub(crate) fn transcode_with(
    opt: &Options,
    r3d: Option<&crate::r3d::R3dPlan>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64, f64),
) -> Result<Outcome> {
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
    // Legacy quality steps become a bpp for this clip; a bpp at or above the source's
    // cannot add anything and gives the lossless repack.
    let src_bpp = source_bpp(&input, &movie);
    let mut target = Target::new(opt.quality, opt.bpp).resolve(src_bpp);
    let src = source_geometry(&movie);
    let dst = target_geometry(src, opt.resolution);
    let resizing = dst.width != video.width || dst.height != video.height;
    if r3d.is_some() && resizing && target.is_lossless() {
        // A resized frame has no camera size to keep; code it at the source's density.
        target = Target::bpp(src_bpp.max(crate::BPP_MIN));
    }
    let passthrough = r3d.is_some() && !resizing && target.is_lossless();
    let quality = target.preset();

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
    if let Some(p) = r3d {
        let codec = if p.direction == crate::r3d::Direction::ToR3d { b"NR3D" } else { b"NRAW" };
        writer.set_metadata(p.udta.clone(), fourcc(codec));
    }

    let threads = if opt.threads > 0 { opt.threads } else { hardware_threads() };
    let nframes = video.samples.len();
    // Resize jobs hold whole decoded frames; keep fewer in flight.
    let workers = if resizing { threads.clamp(2, 4) } else { threads.clamp(1, 16) };
    let target_rank = rank_for_quality(quality);
    let budget = target.frame_budget(video.width, video.height);
    // Constant bitrate pads to the budget of the frame as written (the resized size when
    // resizing), the same budget the rate control codes to.
    let pad_to = if passthrough {
        // Camera codestreams as they are, padded to the largest so every frame matches.
        let header = crate::nraw::sample_header_len(&input, video.samples[0])?;
        video.samples.iter().map(|s| (s.size as u64).saturating_sub(header) as usize).max()
    } else {
        match (opt.cbr || r3d.is_some(), &resizer) {
            (false, _) => None,
            (true, Some(_)) => target.frame_budget(dst.width, dst.height),
            (true, None) => budget,
        }
    };
    // Rate controlled frames go to the workers in fixed runs, coded in order within a run,
    // so each frame starts its search from the previous one's choices. The runs do not
    // depend on the thread count, so neither does the output.
    let run = if budget.is_some() { FRAMES_PER_RUN } else { 1 };
    let max_in_flight = workers * (run + 1);

    let shared = Mutex::new(Shared { done: vec![None; nframes], next: 0, written: 0, stop: false, error: None });
    let cv = Condvar::new();

    let process = |i: usize, seed: &mut Vec<i32>| -> Result<Vec<u8>> {
        let s = video.samples[i];
        let sample = input.read(s.offset, s.size as usize)?;
        let frame = NrawFrame::parse(&sample)?;
        let cs = &sample[frame.codestream_offset..];
        let built = if passthrough {
            frame.build_padded(cs, None, pad_to)?
        } else if let Some(r) = &resizer {
            frame.build_padded(&r.process(cs, target, seed)?, Some((dst.width, dst.height)), pad_to)?
        } else {
            let p = RequantParams { target_rank, budget, ..Default::default() };
            frame.build_padded(&requant_codestream_seeded(cs, &p, 1, seed)?.0, None, pad_to)?
        };
        match r3d {
            Some(p) => crate::r3d::rewrite_frame(&built, p.direction),
            None => Ok(built),
        }
    };

    let mut input_bytes = 0u64;
    let mut frames = 0u32;
    let result: Result<()> = std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let first = {
                    let mut g = shared.lock().unwrap();
                    while !g.stop && g.next < nframes && g.next >= g.written + max_in_flight {
                        g = cv.wait(g).unwrap();
                    }
                    if g.stop || g.next >= nframes {
                        return;
                    }
                    g.next += run;
                    g.next - run
                };
                let mut seed = Vec::new();
                for i in first..nframes.min(first + run) {
                    let r = process(i, &mut seed);
                    let mut g = shared.lock().unwrap();
                    let failed = r.is_err();
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
                    if failed || g.stop {
                        return;
                    }
                }
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
