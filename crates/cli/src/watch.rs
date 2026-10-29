//! Watch-folder daemon: monitors an input directory recursively for stable .NEV and .R3D clips
//! and writes compressed copies into an output directory while preserving relative paths.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bluecompress::{Backend, Options, Resolution};
use serde::{Deserialize, Serialize};

use crate::{bpp_json, codec_name, fourcc, is_clip, parse_resolution, ps, q, resolution_name, setting_label};

const SETTINGS_FILE: &str = ".bluecompress-watch.json";

pub struct WatchConfig {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub quality: u8,
    /// Bits per pixel target instead of `quality`.
    pub bpp: Option<f64>,
    pub resolution: Resolution,
    pub backend: Backend,
    pub threads: usize,
    pub poll_seconds: u64,
    pub clear_after_process: bool,
    /// Also write a proxy to Proxies/ next to each output.
    pub proxies: bool,
    /// Also convert .nev clips to R3D NE in "R3D NE/" next to each output.
    pub r3d: bool,
    /// Also rewrap .R3D clips to N-RAW, and write the exposure .RMD (see `--to-nev`, `--rmd`).
    pub to_nev: bool,
    pub rmd: bool,
    /// Constant bitrate for the compressed clip and the proxy.
    pub cbr: bool,
    pub json: bool,
    pub quiet: bool,
    pub once: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWatchSettings {
    pub quality: Option<u8>,
    /// Bits per pixel target chosen in the Web UI; takes precedence over `quality`.
    #[serde(default)]
    pub bpp: Option<f64>,
    pub resolution: Option<String>,
    pub clear_after_process: Option<bool>,
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchSnapshot {
    pub enabled: bool,
    pub input_dir: String,
    pub output_dir: String,
    pub quality: u8,
    /// Bits per pixel target instead of `quality`, if set.
    pub bpp: Option<f64>,
    pub resolution: String,
    pub poll_seconds: u64,
    pub clear_after_process: bool,
    pub state: String,
    pub current_clip: Option<String>,
    pub fraction: f64,
    pub fps: f64,
    pub done_count: usize,
    pub failed_count: usize,
    pub saved_input_bytes: u64,
    pub saved_output_bytes: u64,
    pub last_clip: Option<String>,
    pub last_error: Option<String>,
}

pub type WatchHook = Arc<dyn Fn(&WatchSnapshot) + Send + Sync>;

type FileStamp = (u64, Option<SystemTime>);

pub fn is_ignored_name(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("@eadir")
        || name.eq_ignore_ascii_case("#recycle")
        || name.eq_ignore_ascii_case("#snapshot")
        || name.ends_with(".partial")
        || name.ends_with(".tmp")
}

/// The watch bpp target at start: a setting saved from the Web UI wins (a saved bpp, or a
/// saved quality, which means no bpp), else the configured one.
pub fn initial_bpp(persisted: &PersistedWatchSettings, configured: Option<f64>) -> Option<f64> {
    match (persisted.bpp, persisted.quality) {
        (Some(b), _) => bluecompress::Target::bpp(b).bpp_value(),
        (None, Some(_)) => None,
        (None, None) => configured,
    }
}

pub fn load_persisted_settings(output_dir: &Path) -> PersistedWatchSettings {
    let path = output_dir.join(SETTINGS_FILE);
    std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_persisted_settings(output_dir: &Path, update: impl FnOnce(&mut PersistedWatchSettings)) {
    if output_dir.as_os_str().is_empty() {
        return;
    }
    let mut current = load_persisted_settings(output_dir);
    update(&mut current);
    if let Ok(bytes) = serde_json::to_vec(&current) {
        let _ = std::fs::write(output_dir.join(SETTINGS_FILE), bytes);
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn cleanup_empty_parent_dirs(file_path: &Path, root_dir: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() || same_dir(dir, root_dir) || !dir.starts_with(root_dir) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn walk_watch(dir: &Path, skip_dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if is_ignored_name(name) {
            continue;
        }
        if p.is_dir() {
            if same_dir(&p, skip_dir) {
                continue;
            }
            walk_watch(&p, skip_dir, out);
        } else if p.is_file() && is_clip(&p) {
            out.push(p);
        }
    }
}

fn temp_output_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".bluecompress.tmp");
    PathBuf::from(s)
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() || m.len() == 0 {
        return None;
    }
    Some((m.len(), m.modified().ok()))
}

fn update_state(state: &Mutex<WatchSnapshot>, hook: Option<&WatchHook>, f: impl FnOnce(&mut WatchSnapshot)) {
    let snap = {
        let mut s = state.lock().unwrap();
        f(&mut s);
        s.clone()
    };
    if let Some(h) = hook {
        h(&snap);
    }
}

pub fn run_watch(
    cfg: WatchConfig,
    cancel: &AtomicBool,
    state: Arc<Mutex<WatchSnapshot>>,
    hook: Option<WatchHook>,
    transcode_lock: Option<Arc<Mutex<()>>>,
) -> u8 {
    let _ = std::fs::create_dir_all(&cfg.input_dir);
    let _ = std::fs::create_dir_all(&cfg.output_dir);
    if same_dir(&cfg.input_dir, &cfg.output_dir) {
        eprintln!("error: watch input and output folders must be different");
        return 2;
    }

    let persisted = load_persisted_settings(&cfg.output_dir);
    let init_quality = persisted.quality.filter(|q| (1..=4).contains(q)).unwrap_or(cfg.quality);
    let init_bpp = initial_bpp(&persisted, cfg.bpp);
    let init_resolution = persisted.resolution.as_deref().and_then(parse_resolution).unwrap_or(cfg.resolution);
    let init_clear = persisted.clear_after_process.unwrap_or(cfg.clear_after_process);

    update_state(&state, hook.as_ref(), |s| {
        s.enabled = true;
        s.input_dir = ps(&cfg.input_dir);
        s.output_dir = ps(&cfg.output_dir);
        s.quality = init_quality;
        s.bpp = init_bpp;
        s.resolution = resolution_name(init_resolution).into();
        s.poll_seconds = cfg.poll_seconds;
        s.clear_after_process = init_clear;
        s.state = "idle".into();
    });

    if !cfg.quiet && !cfg.json {
        println!(
            "[watch] watching {} -> {} ({}, {}, poll {}s, clear source: {})",
            cfg.input_dir.display(),
            cfg.output_dir.display(),
            setting_label(init_quality, init_bpp),
            resolution_name(init_resolution),
            cfg.poll_seconds,
            if init_clear { "on" } else { "off" }
        );
        let _ = std::io::stdout().flush();
    }

    let mut observed: HashMap<PathBuf, FileStamp> = HashMap::new();
    let mut failed_clips: HashSet<(PathBuf, u64, Option<SystemTime>)> = HashSet::new();
    let mut failed_total = 0usize;

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let mut clips = Vec::new();
        walk_watch(&cfg.input_dir, &cfg.output_dir, &mut clips);

        let current_set: HashSet<PathBuf> = clips.iter().cloned().collect();
        observed.retain(|k, _| current_set.contains(k));

        let mut ready = Vec::new();
        let mut stabilizing = 0usize;

        for clip in clips {
            let rel = match clip.strip_prefix(&cfg.input_dir) {
                Ok(r) => r.to_path_buf(),
                Err(_) => match clip.file_name() {
                    Some(n) => PathBuf::from(n),
                    None => continue,
                },
            };
            let target_out = cfg.output_dir.join(&rel);
            if std::fs::metadata(&target_out).map(|m| m.len() > 0).unwrap_or(false) {
                observed.remove(&clip);
                continue;
            }
            let Some(stamp) = file_stamp(&clip) else { continue };
            let key = (clip.clone(), stamp.0, stamp.1);
            if failed_clips.contains(&key) {
                continue;
            }
            match observed.get(&clip) {
                Some(&prev) if prev == stamp => {
                    ready.push((clip, rel, target_out, stamp));
                }
                _ => {
                    observed.insert(clip, stamp);
                    stabilizing += 1;
                }
            }
        }

        if ready.is_empty() {
            let next_state = if stabilizing > 0 { "stabilizing" } else { "idle" };
            update_state(&state, hook.as_ref(), |s| {
                if s.state != next_state || s.current_clip.is_some() {
                    s.state = next_state.into();
                    s.current_clip = None;
                    s.fraction = 0.0;
                    s.fps = 0.0;
                }
            });
            if cfg.once && stabilizing == 0 {
                break;
            }
        } else {
            for (clip, rel, target_out, expected_stamp) in ready {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                // Re-check right before transcoding in case the file changed while an earlier clip ran.
                let Some(now_stamp) = file_stamp(&clip) else {
                    observed.remove(&clip);
                    continue;
                };
                if now_stamp != expected_stamp {
                    observed.insert(clip, now_stamp);
                    stabilizing += 1;
                    continue;
                }
                if std::fs::metadata(&target_out).map(|m| m.len() > 0).unwrap_or(false) {
                    observed.remove(&clip);
                    continue;
                }

                let rel_s = ps(&rel);
                let ins = ps(&clip);
                let outs = ps(&target_out);

                if let Some(parent) = target_out.parent().filter(|p| !p.as_os_str().is_empty()) {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        failed_total += 1;
                        failed_clips.insert((clip.clone(), expected_stamp.0, expected_stamp.1));
                        observed.remove(&clip);
                        let msg = format!("cannot create {}: {e}", parent.display());
                        update_state(&state, hook.as_ref(), |s| {
                            s.failed_count += 1;
                            s.last_error = Some(format!("{rel_s}: {msg}"));
                            s.state = "idle".into();
                            s.current_clip = None;
                        });
                        if cfg.json {
                            println!("{{\"event\":\"error\",\"input\":{},\"error\":{}}}", q(&ins), q(&msg));
                        } else {
                            eprintln!("[watch] {rel_s}: error: {msg}");
                        }
                        continue;
                    }
                }

                let tmp_out = temp_output_path(&target_out);
                let _ = std::fs::remove_file(&tmp_out);

                let _guard = transcode_lock.as_ref().map(|l| l.lock().unwrap());
                if cancel.load(Ordering::Relaxed) {
                    break;
                }

                let (live_quality, live_bpp, live_resolution) = {
                    let s = state.lock().unwrap();
                    let qv = s.quality.clamp(1, 4);
                    let rv = parse_resolution(&s.resolution).unwrap_or(cfg.resolution);
                    (qv, s.bpp, rv)
                };

                update_state(&state, hook.as_ref(), |s| {
                    s.state = "running".into();
                    s.current_clip = Some(rel_s.clone());
                    s.fraction = 0.0;
                    s.fps = 0.0;
                });

                if let Ok(info) = bluecompress::probe(&clip) {
                    let (ow, oh) = bluecompress::output_size(info.width, info.height, live_resolution);
                    if cfg.json {
                        println!(
                            "{{\"event\":\"start\",\"input\":{},\"output\":{},\"codec\":{},\"width\":{},\"height\":{},\
                             \"frames\":{},\"out_width\":{ow},\"out_height\":{oh},\"quality\":{},\"bpp\":{},\
                             \"source_bpp\":{:.3}}}",
                            q(&ins),
                            q(&outs),
                            q(fourcc(&info)),
                            info.width,
                            info.height,
                            info.frames,
                            live_quality,
                            bpp_json(live_bpp),
                            info.bpp
                        );
                    } else if !cfg.quiet {
                        println!(
                            "[watch] compressing {rel_s}: {} {}x{}, {} frames @ {:.3} fps, {:.2} bpp -> {ow}x{oh} {}",
                            codec_name(&info),
                            info.width,
                            info.height,
                            info.frames,
                            info.fps,
                            info.bpp,
                            setting_label(live_quality, live_bpp)
                        );
                    }
                    let _ = std::io::stdout().flush();
                }

                let opt = Options {
                    input: clip.clone(),
                    output: tmp_out.clone(),
                    quality: live_quality,
                    bpp: live_bpp,
                    resolution: live_resolution,
                    backend: cfg.backend,
                    threads: cfg.threads,
                    cbr: cfg.cbr,
                };

                let mut last_emit = Instant::now() - Duration::from_secs(1);
                let result = bluecompress::transcode(&opt, cancel, &mut |fraction, fps| {
                    if fraction >= 1.0 || last_emit.elapsed() >= Duration::from_millis(150) {
                        last_emit = Instant::now();
                        update_state(&state, hook.as_ref(), |s| {
                            s.fraction = fraction;
                            s.fps = fps;
                        });
                    }
                });

                observed.remove(&clip);

                match result {
                    Ok(r) => {
                        if let Err(e) = std::fs::rename(&tmp_out, &target_out) {
                            let _ = std::fs::remove_file(&tmp_out);
                            failed_total += 1;
                            failed_clips.insert((clip, expected_stamp.0, expected_stamp.1));
                            let msg = format!("rename failed {}: {e}", target_out.display());
                            update_state(&state, hook.as_ref(), |s| {
                                s.failed_count += 1;
                                s.last_error = Some(format!("{rel_s}: {msg}"));
                                s.state = "idle".into();
                                s.current_clip = None;
                                s.fraction = 0.0;
                                s.fps = 0.0;
                            });
                            if cfg.json {
                                println!("{{\"event\":\"error\",\"input\":{},\"error\":{}}}", q(&ins), q(&msg));
                            } else {
                                eprintln!("[watch] {rel_s}: error: {msg}");
                            }
                            continue;
                        }

                        let out_ok =
                            std::fs::metadata(&target_out).map(|m| m.is_file() && m.len() > 0).unwrap_or(false);
                        // The proxy is made from the source, so before it may be cleared; a
                        // failed proxy keeps the source.
                        let proxy_ok = !(out_ok && cfg.proxies) || {
                            update_state(&state, hook.as_ref(), |s| {
                                s.fraction = 0.0;
                                s.fps = 0.0;
                            });
                            crate::write_proxy(&clip, &target_out, cfg.cbr, cfg.json, cfg.quiet)
                        };
                        // Same for R3D NE: from the source, and a failure keeps it.
                        let conv =
                            bluecompress::Convert { to_r3d: cfg.r3d, to_nev: cfg.to_nev, rewrap: false, rmd: cfg.rmd };
                        let job = Options { output: target_out.clone(), ..opt.clone() };
                        let r3d_ok =
                            !(out_ok && (cfg.r3d || cfg.to_nev)) || crate::write_r3d(&job, &conv, cfg.json, cfg.quiet);
                        let clear_now = state.lock().unwrap().clear_after_process;
                        let mut cleared = false;
                        if out_ok && clear_now && proxy_ok && r3d_ok {
                            match std::fs::remove_file(&clip) {
                                Ok(()) => {
                                    cleared = true;
                                    cleanup_empty_parent_dirs(&clip, &cfg.input_dir);
                                }
                                Err(e) => {
                                    eprintln!("[watch] {rel_s}: could not remove source file: {e}");
                                }
                            }
                        }

                        let saved = 100.0 * (1.0 - r.output_bytes as f64 / r.input_bytes.max(1) as f64);
                        update_state(&state, hook.as_ref(), |s| {
                            s.done_count += 1;
                            s.saved_input_bytes += r.input_bytes;
                            s.saved_output_bytes += r.output_bytes;
                            s.last_clip = Some(rel_s.clone());
                            s.last_error = None;
                            s.state = "idle".into();
                            s.current_clip = None;
                            s.fraction = 0.0;
                            s.fps = 0.0;
                        });
                        if cfg.json {
                            println!(
                                "{{\"event\":\"done\",\"input\":{},\"output\":{},\"input_bytes\":{},\"output_bytes\":{},\
                                 \"saved_percent\":{saved:.2},\"frames\":{},\"seconds\":{:.3},\"fps\":{:.2},\"backend\":{},\
                                 \"source_cleared\":{cleared}}}",
                                q(&ins),
                                q(&outs),
                                r.input_bytes,
                                r.output_bytes,
                                r.frames,
                                r.seconds,
                                r.fps,
                                q(&r.backend)
                            );
                        } else if !cfg.quiet {
                            let clear_note = if cleared { " (source cleared)" } else { "" };
                            println!(
                                "[watch] done {rel_s}: {:.2} GB -> {:.2} GB ({saved:.1}% smaller) in {:.1} s, {:.1} fps [{}]{clear_note}",
                                r.input_bytes as f64 / 1e9,
                                r.output_bytes as f64 / 1e9,
                                r.seconds,
                                r.fps,
                                r.backend
                            );
                        }
                        let _ = std::io::stdout().flush();
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp_out);
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        failed_total += 1;
                        failed_clips.insert((clip, expected_stamp.0, expected_stamp.1));
                        update_state(&state, hook.as_ref(), |s| {
                            s.failed_count += 1;
                            s.last_error = Some(format!("{rel_s}: {}", e.0));
                            s.state = "idle".into();
                            s.current_clip = None;
                            s.fraction = 0.0;
                            s.fps = 0.0;
                        });
                        if cfg.json {
                            println!("{{\"event\":\"error\",\"input\":{},\"error\":{}}}", q(&ins), q(&e.0));
                        } else {
                            eprintln!("[watch] {rel_s}: error: {e}");
                        }
                    }
                }
            }
            if cfg.once && stabilizing == 0 {
                break;
            }
        }

        let wait_ms = cfg.poll_seconds.max(1) * 1000;
        let mut slept = 0u64;
        while slept < wait_ms {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let step = (wait_ms - slept).min(150);
            std::thread::sleep(Duration::from_millis(step));
            slept += step;
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return 3;
    }
    if failed_total > 0 {
        1
    } else {
        0
    }
}
