//! bluecompress: headless command line front end, watch-folder daemon, and Web UI server.
//!
//!   bluecompress compress [options] <input|folder>... -o <output folder>
//!   bluecompress compress [options] <input> --output <file>
//!   bluecompress watch --input <folder> --output <folder> [options]
//!   bluecompress serve [--listen 0.0.0.0:9847] [options]
//!   bluecompress probe [--json] <input|folder>...
//!   bluecompress backends [--json]
//!   bluecompress version
//!   bluecompress [options] <input> <output>          (short form of compress)
//!
//! With --json, stdout carries one JSON object per line (events: start, progress, done,
//! skipped, error, summary); human readable messages go to stderr.
//! Exit codes: 0 success, 1 at least one clip failed, 2 usage error, 3 cancelled.

mod serve;
mod watch;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bluecompress::{Backend, ClipInfo, Codec, Options, Resolution};

static CANCEL: AtomicBool = AtomicBool::new(false);

pub(crate) fn q(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub(crate) fn ps(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub(crate) fn resolution_name(r: Resolution) -> &'static str {
    match r {
        Resolution::Original => "original",
        Resolution::K6 => "6k",
        Resolution::K4 => "4k",
    }
}

fn parse_quality(s: &str) -> Option<u8> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "q1" | "lossless" => Some(1),
        "2" | "q2" | "archival" => Some(2),
        "3" | "q3" | "high" | "balanced" => Some(3),
        "4" | "q4" | "compact" | "max" => Some(4),
        _ => None,
    }
}

/// A bits per sample target: a number from 0.5 to 8 (an optional "bpp" suffix is allowed),
/// rounded to 0.1.
pub(crate) fn parse_bpp(s: &str) -> Option<f64> {
    let t = s.trim().to_ascii_lowercase();
    let v: f64 = t.strip_suffix("bpp").unwrap_or(&t).trim().parse().ok()?;
    (v.is_finite() && (bluecompress::BPP_MIN..=bluecompress::BPP_MAX).contains(&v)).then(|| (v * 10.0).round() / 10.0)
}

/// "Q2" or "1.2bpp" for log lines.
pub(crate) fn setting_label(quality: u8, bpp: Option<f64>) -> String {
    match bpp {
        Some(b) => bluecompress::bpp_tag(b),
        None => format!("Q{quality}"),
    }
}

/// JSON value of an optional bpp target.
pub(crate) fn bpp_json(bpp: Option<f64>) -> String {
    bpp.map_or_else(|| "null".into(), |b| format!("{b:.1}"))
}

pub(crate) fn parse_resolution(s: &str) -> Option<Resolution> {
    match s.trim().to_ascii_lowercase().as_str() {
        "original" | "orig" => Some(Resolution::Original),
        "6k" => Some(Resolution::K6),
        "4k" | "uhd" => Some(Resolution::K4),
        _ => None,
    }
}

fn parse_backend(s: &str) -> Option<Backend> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" | "gpu" => Some(Backend::Auto),
        "cpu" => Some(Backend::Cpu),
        "metal" => Some(Backend::Metal),
        "cuda" => Some(Backend::Cuda),
        "opencl" => Some(Backend::OpenCl),
        _ => None,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

struct Cli {
    quality: Option<u8>,
    bpp: Option<f64>,
    resolution: Option<Resolution>,
    backend: Option<Backend>,
    threads: usize,
    inputs: Vec<String>,
    input_dir: Option<String>,
    output_dir: Option<String>,
    output_file: Option<String>,
    watch_input: Option<String>,
    watch_output: Option<String>,
    listen: Option<String>,
    browse_root: Option<String>,
    poll_seconds: Option<u64>,
    clear_watch: Option<bool>,
    suffix: String,
    json: bool,
    quiet: bool,
    overwrite: bool,
    skip_existing: bool,
    once: bool,
}

fn usage() -> String {
    format!(
        "bluecompress {}: re-encode Nikon N-RAW (.NEV) and R3D NE (.R3D) TICO RAW clips

usage:
  bluecompress compress [options] <clip|folder>... -o <output folder>
  bluecompress compress [options] <clip> --output <file>
  bluecompress watch --input <folder> --output <folder> [options]
  bluecompress serve [--listen 0.0.0.0:9847] [options]
  bluecompress probe [--json] <clip|folder>...
  bluecompress backends [--json]
  bluecompress version
  bluecompress [options] <clip> <output file>

options:
  -q, --quality 1..4      1 lossless repack (pixel-identical), 2 archival (default),
                          3 high, 4 compact
      --bpp B             instead of -q: target size in bits per Bayer sample, 0.5 to 8 in
                          0.1 steps (Nikon HE stills are about 3, HE* about 5). Each frame
                          is rate controlled to fit; a target at or above the clip's own
                          gives the lossless repack
  -r, --resolution R      original (default), 6k or 4k
  -b, --backend B         auto (default), cpu, metal, cuda or opencl; --cpu / --gpu also work
  -j, --threads N         worker threads (default: all cores)
  -o, --output-dir DIR    write outputs here with the source file names
      --output FILE       single input: exact output path
      --suffix S          append S to output file names (before the extension)
      --overwrite         replace existing outputs (default: fail)
      --skip-existing     skip clips whose output already exists
      --json              JSON lines on stdout (start, progress, done, skipped, error, summary)
      --quiet             no progress on stderr

serve and watch options:
      --listen ADDR       Web UI address for serve (default: 0.0.0.0:${{PORT:-9847}})
      --browse-root DIR   root folder exposed in the Web UI browser (default: BROWSE_ROOT or /)
      --input, --watch-input DIR
                          watch input folder (default: WATCH_INPUT or /watch/in)
      --watch-output DIR  watch output folder (default: WATCH_OUTPUT or /watch/out)
      --poll-seconds N    watch poll interval in seconds (default: WATCH_POLL_SECONDS or 5)
      --clear-watch       delete source clip in watch input folder after compression succeeds
      --once              watch: run one scan pass on stable files and exit
                          watch and serve also read WATCH_QUALITY or WATCH_BPP (or BPP)

Folders are searched recursively for .nev and .r3d files. Originals are never modified;
outputs are written to <name>.partial and renamed when complete.
exit codes: 0 success, 1 a clip failed, 2 usage error, 3 cancelled
",
        bluecompress::VERSION
    )
}

pub(crate) fn is_clip(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("nev") || e.eq_ignore_ascii_case("r3d"))
        .unwrap_or(false)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() && is_clip(&p) => out.push(p),
            // Follow symlinks to files.
            Ok(t) if t.is_symlink() && p.is_file() && is_clip(&p) => out.push(p),
            _ => {}
        }
    }
}

fn expand(inputs: &[String], missing: &mut Vec<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for s in inputs {
        let p = PathBuf::from(s);
        if p.is_dir() {
            let mut found = Vec::new();
            walk(&p, &mut found);
            found.sort();
            out.extend(found);
        } else if p.exists() {
            out.push(p);
        } else {
            missing.push(s.clone());
        }
    }
    out
}

pub(crate) fn codec_name(i: &ClipInfo) -> &'static str {
    if i.codec == Codec::R3dNe {
        "R3D NE"
    } else {
        "N-RAW"
    }
}

pub(crate) fn fourcc(i: &ClipInfo) -> &'static str {
    if i.codec == Codec::R3dNe {
        "NR3D"
    } else {
        "NRAW"
    }
}

fn probe_cmd(o: &Cli) -> u8 {
    let mut missing = Vec::new();
    let mut rc = 0;
    for p in expand(&o.inputs, &mut missing) {
        let info = match bluecompress::probe(&p) {
            Ok(i) => i,
            Err(e) => {
                rc = 1;
                if o.json {
                    println!("{{\"event\":\"error\",\"input\":{},\"error\":{}}}", q(&ps(&p)), q(&e.0));
                } else {
                    eprintln!("{}: {}", p.display(), e);
                }
                continue;
            }
        };
        let (w6, h6) = bluecompress::output_size(info.width, info.height, Resolution::K6);
        let (w4, h4) = bluecompress::output_size(info.width, info.height, Resolution::K4);
        if o.json {
            println!(
                "{{\"event\":\"clip\",\"input\":{},\"codec\":{},\"format\":{},\"width\":{},\"height\":{},\
                 \"frames\":{},\"fps\":{},\"file_bytes\":{},\"video_bytes\":{},\"audio_tracks\":{},\
                 \"bpp\":{:.3},\"output_6k\":[{w6},{h6}],\"output_4k\":[{w4},{h4}]}}",
                q(&ps(&p)),
                q(fourcc(&info)),
                q(codec_name(&info)),
                info.width,
                info.height,
                info.frames,
                info.fps,
                info.file_bytes,
                info.video_bytes,
                info.audio_tracks,
                info.bpp
            );
        } else {
            println!(
                "{}\n  {} {}x{}, {} frames @ {:.3} fps, {:.2} GB, {:.2} bits per sample, {} audio track(s)\n  \
                 6K -> {w6}x{h6}, 4K -> {w4}x{h4}",
                p.display(),
                codec_name(&info),
                info.width,
                info.height,
                info.frames,
                info.fps,
                info.file_bytes as f64 / 1e9,
                info.bpp,
                info.audio_tracks
            );
        }
    }
    for m in missing {
        rc = 1;
        if o.json {
            println!("{{\"event\":\"error\",\"input\":{},\"error\":\"not found\"}}", q(&m));
        } else {
            eprintln!("{m}: not found");
        }
    }
    rc
}

fn backends_cmd(o: &Cli) -> u8 {
    let all = [
        ("auto", Backend::Auto),
        ("cpu", Backend::Cpu),
        ("metal", Backend::Metal),
        ("cuda", Backend::Cuda),
        ("opencl", Backend::OpenCl),
    ];
    if o.json {
        let items: Vec<String> =
            all.iter().map(|(n, b)| format!("{}:{}", q(n), q(&bluecompress::backend_description(*b)))).collect();
        println!(
            "{{\"event\":\"backends\",\"version\":{},\"backends\":{{{}}}}}",
            q(bluecompress::VERSION),
            items.join(",")
        );
    } else {
        for (n, b) in all {
            println!("{n:<6} {}", bluecompress::backend_description(b));
        }
    }
    0
}

fn compress_cmd(o: &Cli) -> u8 {
    let quality = o.quality.unwrap_or(2);
    let bpp = o.bpp;
    let resolution = o.resolution.unwrap_or(Resolution::Original);
    let backend = o.backend.unwrap_or(Backend::Auto);
    let mut missing = Vec::new();
    let inputs = expand(&o.inputs, &mut missing);
    if inputs.is_empty() && missing.is_empty() {
        eprintln!("error: no .nev or .r3d clips found");
        return 2;
    }
    if o.output_file.is_some() && inputs.len() + missing.len() != 1 {
        eprintln!("error: --output takes exactly one input; use -o <folder> for several");
        return 2;
    }
    if o.output_file.is_none() && o.output_dir.is_none() {
        eprintln!("error: give an output folder with -o (or --output for a single file)");
        return 2;
    }
    let (mut failed, mut done, mut skipped) = (missing.len(), 0usize, 0usize);
    let (mut in_bytes, mut out_bytes, mut seconds) = (0u64, 0u64, 0f64);
    for m in &missing {
        if o.json {
            println!("{{\"event\":\"error\",\"input\":{},\"error\":\"not found\"}}", q(m));
        } else {
            eprintln!("{m}: not found");
        }
    }
    let mut stdout = std::io::stdout();
    for input in &inputs {
        if CANCEL.load(Ordering::Relaxed) {
            break;
        }
        let out = match (&o.output_file, &o.output_dir) {
            (Some(f), _) => PathBuf::from(f),
            (None, Some(d)) => {
                let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = input.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                Path::new(d).join(format!("{stem}{}{ext}", o.suffix))
            }
            (None, None) => unreachable!(),
        };
        let (ins, outs) = (ps(input), ps(&out));
        if out.exists() && !o.overwrite {
            if o.skip_existing {
                skipped += 1;
                if o.json {
                    println!(
                        "{{\"event\":\"skipped\",\"input\":{},\"output\":{},\"reason\":\"output exists\"}}",
                        q(&ins),
                        q(&outs)
                    );
                } else if !o.quiet {
                    eprintln!("{ins}: skipped, {outs} exists");
                }
                continue;
            }
            failed += 1;
            if o.json {
                println!(
                    "{{\"event\":\"error\",\"input\":{},\"output\":{},\"error\":\"output exists (use --overwrite or --skip-existing)\"}}",
                    q(&ins),
                    q(&outs)
                );
            } else {
                eprintln!("{ins}: {outs} exists (use --overwrite or --skip-existing)");
            }
            continue;
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Ok(info) = bluecompress::probe(input) {
            let (ow, oh) = bluecompress::output_size(info.width, info.height, resolution);
            if o.json {
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
                    quality,
                    bpp_json(bpp),
                    info.bpp
                );
            } else if !o.quiet {
                eprintln!(
                    "{ins}: {} {}x{}, {} frames @ {:.3} fps, {:.2} bpp -> {ow}x{oh} {}{}",
                    codec_name(&info),
                    info.width,
                    info.height,
                    info.frames,
                    info.fps,
                    info.bpp,
                    setting_label(quality, bpp),
                    if bpp.is_some_and(|b| b >= info.bpp) { " (at or above the source: lossless repack)" } else { "" }
                );
            }
            let _ = stdout.flush();
        }

        let opt = Options {
            input: input.clone(),
            output: out.clone(),
            quality,
            bpp,
            resolution,
            backend,
            threads: o.threads,
        };
        let mut last_permille = -1i64;
        let result = bluecompress::transcode(&opt, &CANCEL, &mut |fraction, fps| {
            let pm = (fraction * 1000.0) as i64;
            if pm == last_permille {
                return;
            }
            last_permille = pm;
            if o.json {
                if pm % 10 == 0 {
                    println!(
                        "{{\"event\":\"progress\",\"input\":{},\"fraction\":{fraction:.3},\"fps\":{fps:.2}}}",
                        q(&ins)
                    );
                    let _ = std::io::stdout().flush();
                }
            } else if !o.quiet {
                eprint!("\r  {:5.1}%  {fps:6.1} fps", fraction * 100.0);
            }
        });
        if !o.json && !o.quiet {
            eprintln!();
        }
        let r = match result {
            Ok(r) => r,
            Err(e) => {
                if CANCEL.load(Ordering::Relaxed) {
                    break;
                }
                failed += 1;
                if o.json {
                    println!("{{\"event\":\"error\",\"input\":{},\"error\":{}}}", q(&ins), q(&e.0));
                } else {
                    eprintln!("{ins}: error: {e}");
                }
                continue;
            }
        };
        done += 1;
        in_bytes += r.input_bytes;
        out_bytes += r.output_bytes;
        seconds += r.seconds;
        let saved = 100.0 * (1.0 - r.output_bytes as f64 / r.input_bytes.max(1) as f64);
        if o.json {
            println!(
                "{{\"event\":\"done\",\"input\":{},\"output\":{},\"input_bytes\":{},\"output_bytes\":{},\
                 \"saved_percent\":{saved:.2},\"frames\":{},\"seconds\":{:.3},\"fps\":{:.2},\"backend\":{}}}",
                q(&ins),
                q(&outs),
                r.input_bytes,
                r.output_bytes,
                r.frames,
                r.seconds,
                r.fps,
                q(&r.backend)
            );
        } else if !o.quiet {
            eprintln!(
                "done: {:.2} GB -> {:.2} GB ({saved:.1}% smaller) in {:.1} s, {:.1} fps [{}]",
                r.input_bytes as f64 / 1e9,
                r.output_bytes as f64 / 1e9,
                r.seconds,
                r.fps,
                r.backend
            );
        }
        let _ = stdout.flush();
    }
    let cancelled = CANCEL.load(Ordering::Relaxed);
    if o.json {
        println!(
            "{{\"event\":\"summary\",\"done\":{done},\"failed\":{failed},\"skipped\":{skipped},\"cancelled\":{cancelled},\
             \"input_bytes\":{in_bytes},\"output_bytes\":{out_bytes},\"seconds\":{seconds:.3}}}"
        );
    } else if inputs.len() > 1 && !o.quiet {
        eprintln!(
            "{done} done, {failed} failed, {skipped} skipped: {:.2} GB -> {:.2} GB",
            in_bytes as f64 / 1e9,
            out_bytes as f64 / 1e9
        );
    }
    if cancelled {
        eprintln!("cancelled");
        return 3;
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

fn env_watch_quality(cli_q: Option<u8>) -> u8 {
    cli_q
        .or_else(|| env_nonempty("WATCH_QUALITY").and_then(|v| parse_quality(&v)))
        .or_else(|| env_nonempty("QUALITY").and_then(|v| parse_quality(&v)))
        .unwrap_or(3)
}

/// Watch bpp target: --bpp, else WATCH_BPP or BPP unless -q was given on the command line.
fn env_watch_bpp(cli_q: Option<u8>, cli_bpp: Option<f64>) -> Option<f64> {
    if cli_bpp.is_some() || cli_q.is_some() {
        return cli_bpp;
    }
    env_nonempty("WATCH_BPP").or_else(|| env_nonempty("BPP")).and_then(|v| parse_bpp(&v))
}

fn env_watch_resolution(cli_r: Option<Resolution>) -> Resolution {
    cli_r
        .or_else(|| env_nonempty("WATCH_RESOLUTION").and_then(|v| parse_resolution(&v)))
        .or_else(|| env_nonempty("RESOLUTION").and_then(|v| parse_resolution(&v)))
        .unwrap_or(Resolution::Original)
}

fn env_watch_backend(cli_b: Option<Backend>) -> Backend {
    cli_b
        .or_else(|| env_nonempty("WATCH_BACKEND").and_then(|v| parse_backend(&v)))
        .or_else(|| env_nonempty("BACKEND").and_then(|v| parse_backend(&v)))
        .unwrap_or(Backend::Auto)
}

fn env_watch_poll(cli_p: Option<u64>) -> u64 {
    cli_p.or_else(|| env_nonempty("WATCH_POLL_SECONDS").and_then(|v| v.parse().ok())).unwrap_or(5)
}

fn env_watch_clear(cli_c: Option<bool>) -> bool {
    cli_c
        .or_else(|| env_nonempty("WATCH_CLEAR").and_then(|v| parse_bool(&v)))
        .or_else(|| env_nonempty("CLEAR_WATCH").and_then(|v| parse_bool(&v)))
        .or_else(|| env_nonempty("WATCH_DELETE_SOURCE").and_then(|v| parse_bool(&v)))
        .unwrap_or(false)
}

fn watch_cmd(o: &Cli) -> u8 {
    let input = o
        .watch_input
        .clone()
        .or_else(|| o.input_dir.clone())
        .or_else(|| o.inputs.first().cloned())
        .or_else(|| env_nonempty("WATCH_INPUT"));
    let output = o
        .watch_output
        .clone()
        .or_else(|| o.output_dir.clone())
        .or_else(|| o.output_file.clone())
        .or_else(|| o.inputs.get(1).cloned())
        .or_else(|| env_nonempty("WATCH_OUTPUT"));

    let (Some(in_dir), Some(out_dir)) = (input, output) else {
        eprintln!("error: watch requires --input <folder> and --output <folder>");
        return 2;
    };

    let cfg = watch::WatchConfig {
        input_dir: PathBuf::from(in_dir),
        output_dir: PathBuf::from(out_dir),
        quality: env_watch_quality(o.quality),
        bpp: env_watch_bpp(o.quality, o.bpp),
        resolution: env_watch_resolution(o.resolution),
        backend: env_watch_backend(o.backend),
        threads: o.threads,
        poll_seconds: env_watch_poll(o.poll_seconds),
        clear_after_process: env_watch_clear(o.clear_watch),
        json: o.json,
        quiet: o.quiet,
        once: o.once,
    };
    let state = Arc::new(Mutex::new(watch::WatchSnapshot::default()));
    watch::run_watch(cfg, &CANCEL, state, None, None)
}

fn normalize_listen(s: &str) -> String {
    let t = s.trim();
    if t.chars().all(|c| c.is_ascii_digit()) {
        format!("0.0.0.0:{t}")
    } else if let Some(port) = t.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        t.to_string()
    }
}

fn serve_cmd(o: &Cli) -> u8 {
    let port = env_nonempty("PORT").unwrap_or_else(|| "9847".into());
    let listen = o
        .listen
        .as_deref()
        .map(normalize_listen)
        .or_else(|| env_nonempty("LISTEN").map(|s| normalize_listen(&s)))
        .unwrap_or_else(|| format!("0.0.0.0:{port}"));

    let browse_root = o
        .browse_root
        .clone()
        .or_else(|| env_nonempty("BROWSE_ROOT"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));

    let watch_enabled_env = env_nonempty("WATCH_ENABLED").map(|v| v.to_ascii_lowercase());
    let explicitly_disabled = matches!(watch_enabled_env.as_deref(), Some("false" | "0" | "no" | "off"));
    let explicitly_enabled = matches!(watch_enabled_env.as_deref(), Some("true" | "1" | "yes" | "on"));

    let watch_in = o.watch_input.clone().or_else(|| o.input_dir.clone()).or_else(|| env_nonempty("WATCH_INPUT"));
    let watch_out = o.watch_output.clone().or_else(|| o.output_dir.clone()).or_else(|| env_nonempty("WATCH_OUTPUT"));

    let watch_cfg = if explicitly_disabled {
        None
    } else if let Some(in_dir) = watch_in {
        let out_dir = watch_out.unwrap_or_else(|| {
            if Path::new("/watch/out").exists() {
                "/watch/out".into()
            } else {
                format!("{in_dir}/Compressed")
            }
        });
        Some(watch::WatchConfig {
            input_dir: PathBuf::from(in_dir),
            output_dir: PathBuf::from(out_dir),
            quality: env_watch_quality(o.quality),
            bpp: env_watch_bpp(o.quality, o.bpp),
            resolution: env_watch_resolution(o.resolution),
            backend: env_watch_backend(o.backend),
            threads: o.threads,
            poll_seconds: env_watch_poll(o.poll_seconds),
            clear_after_process: env_watch_clear(o.clear_watch),
            json: o.json,
            quiet: o.quiet,
            once: false,
        })
    } else if explicitly_enabled || (Path::new("/watch/in").is_dir() && Path::new("/watch/out").is_dir()) {
        Some(watch::WatchConfig {
            input_dir: PathBuf::from("/watch/in"),
            output_dir: PathBuf::from(watch_out.unwrap_or_else(|| "/watch/out".into())),
            quality: env_watch_quality(o.quality),
            bpp: env_watch_bpp(o.quality, o.bpp),
            resolution: env_watch_resolution(o.resolution),
            backend: env_watch_backend(o.backend),
            threads: o.threads,
            poll_seconds: env_watch_poll(o.poll_seconds),
            clear_after_process: env_watch_clear(o.clear_watch),
            json: o.json,
            quiet: o.quiet,
            once: false,
        })
    } else {
        None
    };

    let default_quality = o.quality.or_else(|| env_nonempty("QUALITY").and_then(|v| parse_quality(&v))).unwrap_or(2);
    let default_bpp =
        if o.quality.is_some() { None } else { o.bpp.or_else(|| env_nonempty("BPP").and_then(|v| parse_bpp(&v))) };
    let default_resolution = o
        .resolution
        .or_else(|| env_nonempty("RESOLUTION").and_then(|v| parse_resolution(&v)))
        .unwrap_or(Resolution::Original);

    let cfg = serve::ServeConfig {
        listen,
        browse_root,
        initial_paths: o.inputs.clone(),
        default_quality,
        default_bpp,
        default_resolution,
        default_backend: env_watch_backend(o.backend),
        threads: o.threads,
        watch: watch_cfg,
    };
    serve::run_serve(cfg, &CANCEL)
}

fn bad(what: &str) -> ExitCode {
    eprintln!("error: {what} (see bluecompress --help)");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let _ = ctrlc::set_handler(|| CANCEL.store(true, Ordering::Relaxed));
    let mut o = Cli {
        quality: None,
        bpp: None,
        resolution: None,
        backend: None,
        threads: env_nonempty("THREADS").and_then(|v| v.parse().ok()).unwrap_or(0),
        inputs: Vec::new(),
        input_dir: None,
        output_dir: None,
        output_file: None,
        watch_input: None,
        watch_output: None,
        listen: None,
        browse_root: None,
        poll_seconds: None,
        clear_watch: None,
        suffix: String::new(),
        json: false,
        quiet: false,
        overwrite: false,
        skip_existing: false,
        once: false,
    };
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let mut it = args.into_iter().peekable();
    let cmd = match it.peek().map(String::as_str) {
        Some(c @ ("compress" | "probe" | "backends" | "version" | "serve" | "watch")) => {
            let c = c.to_string();
            it.next();
            c
        }
        _ => String::new(),
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                return ExitCode::SUCCESS;
            }
            "-q" | "--quality" => match it.next().and_then(|v| parse_quality(&v)) {
                Some(v) => o.quality = Some(v),
                None => return bad("quality must be 1 to 4 (or lossless, archival, high, compact)"),
            },
            "--bpp" => match it.next().and_then(|v| parse_bpp(&v)) {
                Some(v) => o.bpp = Some(v),
                None => return bad("bpp must be a number from 0.5 to 8 (bits per Bayer sample)"),
            },
            "-r" | "--resolution" => match it.next().and_then(|v| parse_resolution(&v)) {
                Some(r) => o.resolution = Some(r),
                None => return bad("resolution must be original, 6k or 4k"),
            },
            "-b" | "--backend" => match it.next().and_then(|v| parse_backend(&v)) {
                Some(b) => o.backend = Some(b),
                None => return bad("backend must be auto, cpu, metal, cuda or opencl"),
            },
            "--cpu" => o.backend = Some(Backend::Cpu),
            "--gpu" => o.backend = Some(Backend::Auto),
            "-j" | "--threads" => match it.next().and_then(|v| v.parse::<usize>().ok()).filter(|v| *v <= 1024) {
                Some(v) => o.threads = v,
                None => return bad("threads must be a number"),
            },
            "-i" | "--input" => match it.next() {
                Some(v) => o.input_dir = Some(v),
                None => return bad("--input needs a folder"),
            },
            "-o" | "--output-dir" => match it.next() {
                Some(v) => o.output_dir = Some(v),
                None => return bad("-o needs a folder"),
            },
            "--output" => match it.next() {
                Some(v) => {
                    if cmd == "watch" {
                        o.watch_output = Some(v);
                    } else {
                        o.output_file = Some(v);
                    }
                }
                None => return bad("--output needs a path"),
            },
            "--watch-input" => match it.next() {
                Some(v) => o.watch_input = Some(v),
                None => return bad("--watch-input needs a folder"),
            },
            "--watch-output" => match it.next() {
                Some(v) => o.watch_output = Some(v),
                None => return bad("--watch-output needs a folder"),
            },
            "-l" | "--listen" | "-p" | "--port" => match it.next() {
                Some(v) => o.listen = Some(v),
                None => return bad("--listen needs an address or port"),
            },
            "--browse-root" => match it.next() {
                Some(v) => o.browse_root = Some(v),
                None => return bad("--browse-root needs a folder"),
            },
            "--poll-seconds" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => o.poll_seconds = Some(v),
                None => return bad("--poll-seconds must be a number"),
            },
            "--clear-watch" | "--delete-source" => o.clear_watch = Some(true),
            "--no-clear-watch" | "--keep-source" => o.clear_watch = Some(false),
            "--once" => o.once = true,
            "--suffix" => match it.next() {
                Some(v) => o.suffix = v,
                None => return bad("--suffix needs a value"),
            },
            "--overwrite" => o.overwrite = true,
            "--skip-existing" => o.skip_existing = true,
            "--json" => o.json = true,
            "--quiet" => o.quiet = true,
            s if s.len() > 1 && s.starts_with('-') => return bad(&format!("unknown option {s}")),
            _ => o.inputs.push(a),
        }
    }
    if o.quality.is_some() && o.bpp.is_some() {
        return bad("use either -q/--quality or --bpp, not both");
    }

    let rc = match cmd.as_str() {
        "version" => {
            if o.json {
                println!("{{\"event\":\"version\",\"version\":{}}}", q(bluecompress::VERSION));
            } else {
                println!("{}", bluecompress::VERSION);
            }
            0
        }
        "backends" => backends_cmd(&o),
        "serve" => serve_cmd(&o),
        "watch" => watch_cmd(&o),
        _ if o.inputs.is_empty() => {
            eprint!("{}", usage());
            2
        }
        "probe" => probe_cmd(&o),
        _ => {
            // Short form: bluecompress [options] <input> <output file>
            if cmd.is_empty()
                && o.inputs.len() == 2
                && o.output_dir.is_none()
                && o.output_file.is_none()
                && !Path::new(&o.inputs[1]).is_dir()
            {
                o.output_file = o.inputs.pop();
            }
            compress_cmd(&o)
        }
    };
    ExitCode::from(rc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;

    #[test]
    fn parses_bpp_targets() {
        assert_eq!(parse_bpp("3"), Some(3.0));
        assert_eq!(parse_bpp("1.2bpp"), Some(1.2));
        assert_eq!(parse_bpp(" 2.46 BPP "), Some(2.5));
        assert_eq!(parse_bpp("0.5"), Some(0.5));
        assert_eq!(parse_bpp("8"), Some(8.0));
        assert_eq!(parse_bpp("0.4"), None);
        assert_eq!(parse_bpp("8.5"), None);
        assert_eq!(parse_bpp("NaN"), None);
        assert_eq!(parse_bpp("high"), None);
        assert_eq!(setting_label(2, None), "Q2");
        assert_eq!(setting_label(2, Some(1.2)), "1.2bpp");
        assert_eq!(setting_label(2, Some(3.0)), "3bpp");
    }

    fn box_wrap(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let len = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(len as usize);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(payload);
        out
    }

    fn make_synthetic_nev(path: &Path, width: u16, height: u16, nframes: usize) {
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&[0xff, 0x10]); // SOC
        let mut pih = vec![0u8; 37];
        pih[8..10].copy_from_slice(&width.to_be_bytes());
        pih[10..12].copy_from_slice(&height.to_be_bytes());
        pih[14..16].copy_from_slice(&8u16.to_be_bytes());
        pih[16] = 4;
        pih[17] = 4;
        pih[18] = 8;
        pih[20] = 0x44;
        pih[21] = 0x10;
        pih[22] = 0x01;
        pih[23] = 0x14;
        pih[24 + 3] = 0x02;
        pih[24 + 4] = 0x58; // black = 600
        pih[24 + 7] = 0x01; // gain = 65536
        pih[24 + 10] = 0x08; // knee = 2048
        hdr.extend_from_slice(&[0xff, 0x12, 0x00, 39]);
        hdr.extend_from_slice(&pih);

        // CDT: 4 components (hl=[5,5,0,5], vl=[1,1,0,1]) -> 37 bands
        hdr.extend_from_slice(&[0xff, 0x13, 0x00, 14]);
        for (hl, vl) in [(5u8, 1u8), (5, 1), (0, 0), (5, 1)] {
            hdr.extend_from_slice(&[14, 0x11, (hl << 4) | vl]);
        }

        let nbands = 25usize;
        hdr.extend_from_slice(&[0xff, 0x14]);
        hdr.extend_from_slice(&((2 + 2 * nbands) as u16).to_be_bytes());
        for i in 0..nbands {
            hdr.extend_from_slice(&[4, i as u8]);
        }
        let mut parse_buf = hdr.clone();
        parse_buf.extend_from_slice(&[0xff, 0x20, 0x00, 0x04, 0x00, 0x00]);
        let (th, _) = bluecompress::tico::TicoHeader::parse(&parse_buf).unwrap();
        let layout = bluecompress::tico::Layout::make(&th, width as usize, height as usize).unwrap();

        let mut pl = bluecompress::frame::Planes::new(layout.cw, layout.ch);
        for (i, v) in pl.data.iter_mut().enumerate() {
            *v = ((i as i32 * 37) % 4096) - 2048;
        }
        bluecompress::frame::forward_star_tetrix(&mut pl);
        let mut coeffs = vec![0i32; bluecompress::frame::Coeffs::len(&layout)];
        bluecompress::frame::forward_wavelet(&layout, &pl, &mut coeffs);
        let cs = bluecompress::frame::encode_frame(&th, &layout, &coeffs, 10).unwrap();

        // Build NRAW sample wrapper
        let mut nrmt_w = vec![0x01, 0x10, 0x01, 0x00, 0, 0, 0, 0];
        nrmt_w.extend_from_slice(&(width as u32).to_be_bytes());
        let mut nrmt_h = vec![0x01, 0x10, 0x01, 0x01, 0, 0, 0, 0];
        nrmt_h.extend_from_slice(&(height as u32).to_be_bytes());
        let mut nrfh_payload = Vec::new();
        nrfh_payload.extend_from_slice(&box_wrap(b"NRHM", b"0100"));
        nrfh_payload.extend_from_slice(&box_wrap(b"NRMT", &nrmt_w));
        nrfh_payload.extend_from_slice(&box_wrap(b"NRMT", &nrmt_h));
        let nrfh = box_wrap(b"NRFH", &nrfh_payload);

        let mut nrti_payload = Vec::from(&b"0100"[..]);
        nrti_payload.extend_from_slice(&(cs.len() as u32).to_be_bytes());
        let nrth = box_wrap(b"NRTH", &box_wrap(b"NRTI", &nrti_payload));

        let mut raw_sample = vec![0, 0, 0, 0, b'N', b'R', b'A', b'W'];
        raw_sample.extend_from_slice(&nrfh);
        raw_sample.extend_from_slice(&nrth);
        raw_sample.extend_from_slice(&cs);
        let slen = raw_sample.len() as u32;
        raw_sample[..4].copy_from_slice(&slen.to_be_bytes());
        let nf = bluecompress::nraw::NrawFrame::parse(&raw_sample).unwrap();
        let sample = nf.build(&cs, Some((width as u32, height as u32))).unwrap();

        // Build ISO BMFF container
        let ftyp = box_wrap(b"ftyp", b"niko\x00\x00\x00\x00niko");
        let mut tkhd = vec![0u8; 84];
        tkhd[76..80].copy_from_slice(&((width as u32) << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&((height as u32) << 16).to_be_bytes());
        let tkhd_box = box_wrap(b"tkhd", &tkhd);

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&24000u32.to_be_bytes());
        let mdhd_box = box_wrap(b"mdhd", &mdhd);

        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");
        let hdlr_box = box_wrap(b"hdlr", &hdlr);

        let mut stsd_entry = vec![0u8; 86];
        stsd_entry[..4].copy_from_slice(&86u32.to_be_bytes());
        stsd_entry[4..8].copy_from_slice(b"NRAW");
        stsd_entry[32..34].copy_from_slice(&width.to_be_bytes());
        stsd_entry[34..36].copy_from_slice(&height.to_be_bytes());
        let mut stsd = vec![0u8; 8];
        stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&stsd_entry);
        let stsd_box = box_wrap(b"stsd", &stsd);

        let mut stts = vec![0u8; 16];
        stts[4..8].copy_from_slice(&1u32.to_be_bytes());
        stts[8..12].copy_from_slice(&(nframes as u32).to_be_bytes());
        stts[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let stts_box = box_wrap(b"stts", &stts);

        let mut stsc = vec![0u8; 20];
        stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsc[8..12].copy_from_slice(&1u32.to_be_bytes());
        stsc[12..16].copy_from_slice(&(nframes as u32).to_be_bytes());
        stsc[16..20].copy_from_slice(&1u32.to_be_bytes());
        let stsc_box = box_wrap(b"stsc", &stsc);

        let mut stsz = vec![0u8; 12];
        stsz[4..8].copy_from_slice(&(sample.len() as u32).to_be_bytes());
        stsz[8..12].copy_from_slice(&(nframes as u32).to_be_bytes());
        let stsz_box = box_wrap(b"stsz", &stsz);

        // Placeholder co64 (patched once moov size is known)
        let mut co64 = vec![0u8; 16];
        co64[4..8].copy_from_slice(&1u32.to_be_bytes());
        let co64_box = box_wrap(b"co64", &co64);

        let build_moov = |mdat_payload_off: u64| -> Vec<u8> {
            let mut co64 = vec![0u8; 16];
            co64[4..8].copy_from_slice(&1u32.to_be_bytes());
            co64[8..16].copy_from_slice(&mdat_payload_off.to_be_bytes());
            let co64_box = box_wrap(b"co64", &co64);
            let mut stbl_p = Vec::new();
            for b in [&stsd_box, &stts_box, &stsc_box, &stsz_box, &co64_box] {
                stbl_p.extend_from_slice(b);
            }
            let stbl = box_wrap(b"stbl", &stbl_p);
            let minf = box_wrap(b"minf", &stbl);
            let mut mdia_p = Vec::new();
            mdia_p.extend_from_slice(&mdhd_box);
            mdia_p.extend_from_slice(&hdlr_box);
            mdia_p.extend_from_slice(&minf);
            let mdia = box_wrap(b"mdia", &mdia_p);
            let mut trak_p = Vec::new();
            trak_p.extend_from_slice(&tkhd_box);
            trak_p.extend_from_slice(&mdia);
            let trak = box_wrap(b"trak", &trak_p);
            box_wrap(b"moov", &trak)
        };
        let _ = co64_box;
        let moov0 = build_moov(0);
        let mdat_payload_off = (ftyp.len() + moov0.len() + 8) as u64;
        let moov = build_moov(mdat_payload_off);

        let mut mdat_payload = Vec::with_capacity(sample.len() * nframes);
        for _ in 0..nframes {
            mdat_payload.extend_from_slice(&sample);
        }
        let mdat = box_wrap(b"mdat", &mdat_payload);

        let mut file_bytes = Vec::new();
        file_bytes.extend_from_slice(&ftyp);
        file_bytes.extend_from_slice(&moov);
        file_bytes.extend_from_slice(&mdat);
        std::fs::write(path, &file_bytes).unwrap();
    }

    #[test]
    fn watch_and_serve_end_to_end() {
        let base = std::env::temp_dir().join(format!("bluecomp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let watch_in = base.join("watch_in");
        let watch_out = base.join("watch_out");
        let sub_in = watch_in.join("Day1").join("A001");
        std::fs::create_dir_all(&sub_in).unwrap();
        std::fs::create_dir_all(&watch_out).unwrap();

        let clip_path = sub_in.join("CLIP_0001.NEV");
        make_synthetic_nev(&clip_path, 128, 64, 2);

        // Also place a corrupt clip to verify watch records it once and does not loop forever.
        let bad_path = watch_in.join("CORRUPT.NEV");
        std::fs::write(&bad_path, b"not a valid nev clip").unwrap();

        // 1. Test watch --once (with 1s stability check and clear_after_process = true).
        let state = Arc::new(Mutex::new(watch::WatchSnapshot::default()));
        let cancel = AtomicBool::new(false);
        let rc = watch::run_watch(
            watch::WatchConfig {
                input_dir: watch_in.clone(),
                output_dir: watch_out.clone(),
                quality: 3,
                bpp: None,
                resolution: Resolution::Original,
                backend: Backend::Cpu,
                threads: 2,
                poll_seconds: 1,
                clear_after_process: true,
                json: false,
                quiet: true,
                once: true,
            },
            &cancel,
            state.clone(),
            None,
            None,
        );
        // rc is 1 because CORRUPT.NEV failed while CLIP_0001.NEV succeeded.
        assert_eq!(rc, 1);
        let expected_out = watch_out.join("Day1").join("A001").join("CLIP_0001.NEV");
        assert!(expected_out.is_file(), "watch output clip should exist at relative path");
        assert!(!clip_path.exists(), "source clip should be deleted when clear_after_process is true");
        assert!(!sub_in.exists(), "empty parent subfolder should be cleaned up after source deletion");
        assert!(watch_in.is_dir(), "watch_in root folder itself must never be removed");
        assert!(bad_path.is_file(), "failed corrupt clip must not be deleted");
        let probed = bluecompress::probe(&expected_out).unwrap();
        assert_eq!((probed.width, probed.height, probed.frames), (128, 64, 2));
        let snap = state.lock().unwrap().clone();
        assert_eq!(snap.done_count, 1);
        assert_eq!(snap.failed_count, 1);
        assert!(snap.clear_after_process);

        // 2. Test HTTP + SSE endpoints via serve.rs handler directly on an ephemeral port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let media_dir = base.join("media");
        std::fs::create_dir_all(&media_dir).unwrap();
        let web_clip = media_dir.join("WEB_0001.NEV");
        make_synthetic_nev(&web_clip, 128, 64, 2);

        let serve_cfg = serve::ServeConfig {
            listen: addr.to_string(),
            browse_root: base.clone(),
            initial_paths: vec![],
            default_quality: 2,
            default_bpp: None,
            default_resolution: Resolution::Original,
            default_backend: Backend::Cpu,
            threads: 2,
            watch: Some(watch::WatchConfig {
                input_dir: watch_in.clone(),
                output_dir: watch_out.clone(),
                quality: 3,
                bpp: None,
                resolution: Resolution::Original,
                backend: Backend::Cpu,
                threads: 2,
                poll_seconds: 60,
                clear_after_process: false,
                json: false,
                quiet: true,
                once: false,
            }),
        };
        std::thread::spawn(move || {
            serve::run_serve(serve_cfg, &CANCEL);
        });
        std::thread::sleep(std::time::Duration::from_millis(120));

        let http_req = |method: &str, path: &str, body: &str| -> (u16, Vec<u8>) {
            let mut s = TcpStream::connect(addr).unwrap();
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).unwrap();
            let sep = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&resp[..sep]);
            let status: u16 = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
            (status, resp[sep + 4..].to_vec())
        };

        let (st, html) = http_req("GET", "/", "");
        assert_eq!(st, 200);
        assert!(String::from_utf8_lossy(&html).contains("Blue Compressor"));

        let (st, info_bytes) = http_req("POST", "/api/invoke/app_info", "{}");
        assert_eq!(st, 200);
        let info_val: serde_json::Value = serde_json::from_slice(&info_bytes).unwrap();
        assert_eq!(info_val["web"], serde_json::Value::Bool(true));
        assert_eq!(info_val["defaultQuality"], 2);
        assert_eq!(info_val["watch"]["clearAfterProcess"], serde_json::Value::Bool(false));

        let set_watch_body =
            serde_json::json!({ "clearAfterProcess": true, "quality": 2, "resolution": "6k" }).to_string();
        let (st, sw_bytes) = http_req("POST", "/api/invoke/set_watch_settings", &set_watch_body);
        assert_eq!(st, 200);
        let sw_val: serde_json::Value = serde_json::from_slice(&sw_bytes).unwrap();
        assert_eq!(sw_val["clearAfterProcess"], serde_json::Value::Bool(true));
        assert_eq!(sw_val["quality"], 2);
        assert_eq!(sw_val["resolution"], "6k");
        let persisted = watch::load_persisted_settings(&watch_out);
        assert_eq!(persisted.clear_after_process, Some(true));
        assert_eq!(persisted.quality, Some(2));
        assert_eq!(persisted.resolution.as_deref(), Some("6k"));

        let (st, list_bytes) = http_req("GET", &format!("/api/fs/list?path={}", media_dir.display()), "");
        assert_eq!(st, 200);
        let list_val: serde_json::Value = serde_json::from_slice(&list_bytes).unwrap();
        assert_eq!(list_val["entries"][0]["name"], "WEB_0001.NEV");

        let mkdir_body = serde_json::json!({ "parent": media_dir.to_string_lossy(), "name": "Out" }).to_string();
        let (st, _) = http_req("POST", "/api/fs/mkdir", &mkdir_body);
        assert_eq!(st, 200);
        assert!(media_dir.join("Out").is_dir());

        let scan_body = serde_json::json!({ "paths": [media_dir.to_string_lossy()] }).to_string();
        let (st, scan_bytes) = http_req("POST", "/api/invoke/scan", &scan_body);
        assert_eq!(st, 200);
        let scan_val: serde_json::Value = serde_json::from_slice(&scan_bytes).unwrap();
        assert_eq!(scan_val[0]["ok"], serde_json::Value::Bool(true));
        assert_eq!(scan_val[0]["width"], 128);
        let source_bpp = scan_val[0]["bpp"].as_f64().unwrap();
        assert!(source_bpp > 1.0, "scan reports the clip's bits per sample");

        let thumb_body = serde_json::json!({ "path": web_clip.to_string_lossy(), "width": 32 }).to_string();
        let (st, thumb_bytes) = http_req("POST", "/api/invoke/thumbnail", &thumb_body);
        assert_eq!(st, 200);
        assert!(thumb_bytes.len() > 8);

        let prev_body = serde_json::json!({
            "path": web_clip.to_string_lossy(),
            "frame": 0,
            "quality": 2,
            "resolution": "original",
            "backend": "cpu",
            "width": 32
        })
        .to_string();
        let (st, prev_bytes) = http_req("POST", "/api/invoke/preview", &prev_body);
        assert_eq!(st, 200);
        assert!(prev_bytes.len() > 32);

        // Open SSE stream, start a job, and verify job-done + queue-finished events arrive.
        let mut sse = TcpStream::connect(addr).unwrap();
        sse.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        sse.write_all(format!("GET /api/events HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()).unwrap();
        let mut sse_reader = BufReader::new(sse);

        let start_body = serde_json::json!({
            "jobs": [{ "id": 1, "path": web_clip.to_string_lossy(), "quality": 2, "resolution": "original" }],
            "settings": { "backend": "cpu", "outputFolder": media_dir.join("Out").to_string_lossy() }
        })
        .to_string();
        let (st, _) = http_req("POST", "/api/invoke/start", &start_body);
        assert_eq!(st, 200);

        let mut saw_done = false;
        let mut saw_finished = false;
        let mut line = String::new();
        while sse_reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Some(json_str) = line.trim().strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if v["event"] == "job-done" {
                        saw_done = true;
                    }
                    if v["event"] == "queue-finished" {
                        saw_finished = true;
                        break;
                    }
                }
            }
            line.clear();
        }
        assert!(saw_done && saw_finished, "SSE stream should emit job-done and queue-finished");
        assert!(media_dir.join("Out").join("WEB_0001.NEV").is_file());

        // A bits per sample job goes next to the clip in "Compressed 1bpp" and is smaller.
        let bpp_body = serde_json::json!({
            "jobs": [{ "id": 2, "path": web_clip.to_string_lossy(), "quality": 2, "bpp": 1.0, "resolution": "original" }],
            "settings": { "backend": "cpu" }
        })
        .to_string();
        let (st, _) = http_req("POST", "/api/invoke/start", &bpp_body);
        assert_eq!(st, 200);
        line.clear();
        while sse_reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line.contains("queue-finished") {
                break;
            }
            line.clear();
        }
        let bpp_out = media_dir.join("Compressed 1bpp").join("WEB_0001.NEV");
        assert!(bpp_out.is_file(), "bpp output goes into a folder named after the target");
        let out_info = bluecompress::probe(&bpp_out).unwrap();
        assert!(out_info.bpp < source_bpp, "{} should be below {source_bpp}", out_info.bpp);
        let check_body = serde_json::json!({
            "outputFolder": null,
            "items": [{ "id": 7, "path": web_clip.to_string_lossy(), "quality": 2, "bpp": 1.0,
                        "resolution": "original", "width": 128, "height": 64 }]
        })
        .to_string();
        let (st, check_bytes) = http_req("POST", "/api/invoke/check_existing", &check_body);
        assert_eq!(st, 200);
        let check_val: serde_json::Value = serde_json::from_slice(&check_bytes).unwrap();
        assert_eq!(check_val[0]["id"], 7, "the bpp output counts as already done for the same target");
        let prev_bpp = serde_json::json!({
            "path": web_clip.to_string_lossy(), "frame": 0, "quality": 2, "bpp": 1.0,
            "resolution": "original", "backend": "cpu", "width": 32
        })
        .to_string();
        let (st, prev_bytes) = http_req("POST", "/api/invoke/preview", &prev_bpp);
        assert_eq!(st, 200);
        let out_frame = u64::from_le_bytes(prev_bytes[24..32].try_into().unwrap());
        let src_frame = u64::from_le_bytes(prev_bytes[16..24].try_into().unwrap());
        assert!(out_frame < src_frame, "the preview renders the bpp setting");

        let _ = std::fs::remove_dir_all(&base);
    }
}
