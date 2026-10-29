//! Embedded Web UI server (`bluecompress serve`): serves `app/ui` over HTTP/1.1, exposes
//! the Tauri command surface at `/api/invoke/<cmd>`, streams queue and watch-folder events
//! over Server-Sent Events (`/api/events`), and provides server-side directory browsing
//! (`/api/fs/list`, `/api/fs/mkdir`) for NAS and Docker deployments.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bluecompress::{Backend, Codec, Options, Resolution, Target};
use serde::{Deserialize, Serialize};

use crate::watch::{
    is_ignored_name, load_persisted_settings, run_watch, save_persisted_settings, WatchConfig, WatchSnapshot,
};
use crate::{is_clip, parse_resolution, ps, resolution_name};

const INDEX_HTML: &str = include_str!("../../../app/ui/index.html");
const STYLES_CSS: &str = include_str!("../../../app/ui/styles.css");
const APP_JS: &str = include_str!("../../../app/ui/app.js");
const ICON_PNG: &[u8] = include_bytes!("../../../app/ui/icon.png");

pub struct ServeConfig {
    pub listen: String,
    pub browse_root: PathBuf,
    pub initial_paths: Vec<String>,
    /// Legacy quality step for new clips (QUALITY or -q): the UI starts on the matching
    /// quick pick (1 Lossless, 2 Light, 3 Medium, 4 Strong).
    pub default_quality: u8,
    /// Bits per pixel for new clips instead of `default_quality` (BPP or --bpp).
    pub default_bpp: Option<f64>,
    pub default_resolution: Resolution,
    pub default_backend: Backend,
    /// Proxies toggle for new clips (PROXIES or --proxies).
    pub default_proxies: bool,
    pub threads: usize,
    pub watch: Option<WatchConfig>,
}

struct ServerState {
    browse_root: PathBuf,
    default_quality: u8,
    default_bpp: Option<f64>,
    default_resolution: Resolution,
    default_backend: Backend,
    default_proxies: bool,
    threads: usize,
    running: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    queue: Arc<Mutex<QueueState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    initial: Mutex<Vec<String>>,
    sse_clients: Arc<Mutex<Vec<TcpStream>>>,
    watch_state: Arc<Mutex<WatchSnapshot>>,
    transcode_lock: Arc<Mutex<()>>,
    /// Newest release seen by the update check (see [`crate::update`]).
    update: Arc<Mutex<crate::update::UpdateStatus>>,
}

#[derive(Serialize, Clone)]
struct Clip {
    path: String,
    name: String,
    ok: bool,
    error: Option<String>,
    codec: Option<String>,
    width: u32,
    height: u32,
    frames: u32,
    fps: f64,
    bytes: u64,
    /// Average coded bits per pixel (per Bayer sample) of the clip.
    bpp: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    gpu: String,
    cpu: String,
    platform: String,
    web: bool,
    browse_root: String,
    default_quality: u8,
    default_bpp: Option<f64>,
    default_resolution: String,
    default_proxies: bool,
    /// The encoder proxies use here ("hevc_nvenc (GPU)", "libx265 (CPU)") or why none.
    proxy_encoder: String,
    watch: WatchSnapshot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetWatchSettingsArgs {
    clear_after_process: Option<bool>,
    /// Lossless (1) or a legacy step (2 to 4: 0.75, 0.55, 0.35 x each clip's own bpp).
    /// Setting a quality without a bpp switches the watch folder back to it.
    quality: Option<u8>,
    bpp: Option<f64>,
    #[serde(default)]
    lossless: bool,
    resolution: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    backend: String,
    output_folder: Option<String>,
}

#[derive(Deserialize, Clone)]
struct JobRequest {
    id: u64,
    path: String,
    /// Legacy quality step (1 lossless, 2 to 4: 0.75, 0.55, 0.35 x the clip's own bpp), used
    /// when neither `bpp` nor `lossless` is given.
    #[serde(default)]
    quality: Option<u8>,
    /// Target in bits per pixel.
    #[serde(default)]
    bpp: Option<f64>,
    #[serde(default)]
    lossless: bool,
    resolution: String,
    /// Compress even when a compressed copy already exists ("Compress again").
    #[serde(default)]
    force: bool,
    /// Also write a 10-bit H.265 proxy to Proxies/ next to the output.
    #[serde(default)]
    proxy: bool,
    /// Clip details the UI shows on the card (name, codec, size, frames, fps), kept as is so
    /// a reloaded page can rebuild its queue.
    #[serde(default)]
    clip: Option<serde_json::Value>,
    /// Set for a clip the UI found already compressed: it is kept in the queue as done
    /// (so a reloaded page still shows it) without being part of the batch.
    #[serde(default)]
    existing: Option<ExistingRef>,
}

#[derive(Deserialize, Clone)]
struct ExistingRef {
    output: String,
    bytes: u64,
}

/// One job of the server-side queue, as returned by `get_queue`.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct QueueJob {
    id: u64,
    path: String,
    name: String,
    bytes: u64,
    /// 1 for the lossless repack (with no `bpp`).
    quality: u8,
    bpp: Option<f64>,
    lossless: bool,
    resolution: String,
    force: bool,
    proxy: bool,
    clip: Option<serde_json::Value>,
    /// waiting, running, done, failed or cancelled.
    state: String,
    fraction: f64,
    fps: f64,
    output: Option<String>,
    result: Option<JobDone>,
    error: Option<String>,
}

/// The queue the server is working through. It outlives the page that started it, so a
/// reloaded or second browser tab can show the same cards, progress and pause state.
#[derive(Default)]
struct QueueState {
    jobs: Vec<QueueJob>,
    settings: Settings,
    batch: Batch,
}

/// Running totals of the current batch (since the queue last started from idle), for the
/// "saved so far" counter. Skipped clips count as done but not towards the bytes.
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Batch {
    total: u32,
    done: u32,
    skipped: u32,
    failed: u32,
    input_bytes: u64,
    output_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueSnapshot<'a> {
    running: bool,
    paused: bool,
    jobs: &'a [QueueJob],
    batch: &'a Batch,
}

#[derive(Deserialize)]
struct SetPausedArgs {
    paused: bool,
}

#[derive(Deserialize)]
struct RemoveJobsArgs {
    /// Job ids to drop; without ids, every job that is not waiting or running.
    #[serde(default)]
    ids: Option<Vec<u64>>,
    /// Also drop waiting jobs (only allowed while the queue is idle).
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct StartArgs {
    jobs: Vec<JobRequest>,
    settings: Settings,
}

#[derive(Deserialize)]
struct ScanArgs {
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct PlanOutputArgs {
    width: u32,
    height: u32,
    resolution: String,
}

#[derive(Deserialize)]
struct PreviewArgs {
    path: String,
    frame: u32,
    #[serde(default)]
    quality: Option<u8>,
    #[serde(default)]
    bpp: Option<f64>,
    #[serde(default)]
    lossless: bool,
    resolution: String,
    backend: String,
    width: u32,
    crop: Option<[f64; 4]>,
}

#[derive(Deserialize)]
struct ThumbnailArgs {
    path: String,
    width: u32,
}

#[derive(Deserialize)]
struct FsListArgs {
    path: Option<String>,
}

#[derive(Deserialize)]
struct FsMkdirArgs {
    path: Option<String>,
    parent: Option<String>,
    name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FsEntry {
    name: String,
    path: String,
    kind: &'static str,
    codec: Option<&'static str>,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FsListResponse {
    path: String,
    parent: Option<String>,
    browse_root: String,
    roots: Vec<String>,
    entries: Vec<FsEntry>,
}

#[derive(Serialize, Clone)]
struct JobStart {
    id: u64,
    output: String,
}

#[derive(Serialize, Clone)]
struct JobProgress {
    id: u64,
    fraction: f64,
    fps: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct JobDone {
    id: u64,
    /// The output already existed, so the clip was not compressed again.
    skipped: bool,
    output: String,
    input_bytes: u64,
    output_bytes: u64,
    frames: u32,
    seconds: f64,
    fps: f64,
    backend: String,
    /// The proxy written for this clip, if one was asked for and made.
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy_error: Option<String>,
}

#[derive(Serialize, Clone)]
struct JobError {
    id: u64,
    error: String,
    cancelled: bool,
}

fn resolution_of(s: &str) -> Resolution {
    match s {
        "6k" => Resolution::K6,
        "4k" => Resolution::K4,
        _ => Resolution::Original,
    }
}

fn backend_of(s: &str, default_backend: Backend) -> Backend {
    match s {
        "cpu" => Backend::Cpu,
        "metal" => Backend::Metal,
        "cuda" => Backend::Cuda,
        "opencl" => Backend::OpenCl,
        _ => default_backend,
    }
}

fn collect_clips(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Compressed") || is_ignored_name(n))
        {
            return;
        }
        let Ok(rd) = std::fs::read_dir(path) else { return };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for e in entries {
            let skip = e.file_name().and_then(|n| n.to_str()).is_some_and(is_ignored_name);
            if !skip {
                collect_clips(&e, out);
            }
        }
    } else if is_clip(path) {
        out.push(path.to_path_buf());
    }
}

fn describe_clip(path: &Path) -> Clip {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let base = Clip {
        path: ps(path),
        name,
        ok: false,
        error: None,
        codec: None,
        width: 0,
        height: 0,
        frames: 0,
        fps: 0.0,
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        bpp: 0.0,
    };
    match bluecompress::probe(path) {
        Ok(i) => Clip {
            ok: true,
            codec: Some(match i.codec {
                Codec::NRaw => "N-RAW".into(),
                Codec::R3dNe => "R3D NE".into(),
            }),
            width: i.width,
            height: i.height,
            frames: i.frames,
            fps: i.fps,
            bytes: i.file_bytes,
            bpp: i.bpp,
            ..base
        },
        Err(e) => Clip { error: Some(e.to_string()), ..base },
    }
}

/// What a job for `input` compresses to: lossless, its bpp, or a legacy quality step as
/// the bpp it stands for on that clip.
fn target_of(input: &Path, quality: Option<u8>, bpp: Option<f64>, lossless: bool) -> Target {
    bluecompress::named_target(input, Target::from_fields(quality, bpp, lossless))
}

/// The target a queued job keeps.
fn job_target(job: &QueueJob) -> Target {
    if job.lossless {
        Target::lossless()
    } else {
        Target::new(job.quality, job.bpp)
    }
}

fn output_for(input: &Path, folder: Option<&str>, target: Target, resolution: &str) -> PathBuf {
    let tag = bluecompress::variant_name_for(target, resolution);
    let name = input.file_name().unwrap_or_default();
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    };
    let free = |p: &Path| !p.exists() && !same(p, input);
    let candidates: Vec<PathBuf> = match folder {
        Some(f) if !f.is_empty() => vec![Path::new(f).join(name), Path::new(f).join(&tag).join(name)],
        _ => vec![input.parent().unwrap_or(Path::new(".")).join(format!("Compressed {tag}")).join(name)],
    };
    if let Some(p) = candidates.iter().find(|p| free(p)) {
        return p.clone();
    }
    let last = candidates.last().unwrap();
    let dir = last.parent().unwrap_or(Path::new("."));
    let base = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = dir.parent().unwrap_or(Path::new("."));
    (2..).map(|n| parent.join(format!("{base} ({n})")).join(name)).find(|p| free(p)).unwrap()
}

/// A finished compressed copy of `input` that an earlier run already wrote for this
/// setting, if any: the places `output_for` would put it, in the same order.
fn existing_for(input: &Path, folder: Option<&str>, target: Target, resolution: &str) -> Option<(PathBuf, u64)> {
    bluecompress::existing_compressed_for(input, folder, target, resolution)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckExistingItem {
    id: u64,
    path: String,
    #[serde(default)]
    quality: Option<u8>,
    #[serde(default)]
    bpp: Option<f64>,
    #[serde(default)]
    lossless: bool,
    resolution: String,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckExistingArgs {
    output_folder: Option<String>,
    items: Vec<CheckExistingItem>,
}

#[derive(Serialize)]
struct ExistingOutput {
    id: u64,
    output: String,
    bytes: u64,
}

/// For each clip and its setting, the compressed copy a run would skip because it already
/// exists. Only file metadata is read, so a folder of hundreds of clips is quick.
fn check_existing_outputs(folder: Option<&str>, items: &[CheckExistingItem]) -> Vec<ExistingOutput> {
    items
        .iter()
        .filter_map(|it| {
            let res = bluecompress::effective_resolution(it.width, it.height, &it.resolution);
            let path = Path::new(&it.path);
            existing_for(path, folder, target_of(path, it.quality, it.bpp, it.lossless), res)
                .map(|(p, bytes)| ExistingOutput { id: it.id, output: p.to_string_lossy().into_owned(), bytes })
        })
        .collect()
}

fn broadcast_event<T: Serialize>(clients: &Mutex<Vec<TcpStream>>, event: &str, payload: T) {
    let line = match serde_json::to_string(&serde_json::json!({ "event": event, "payload": payload })) {
        Ok(s) => format!("data: {s}\n\n"),
        Err(_) => return,
    };
    let mut guard = clients.lock().unwrap();
    guard.retain_mut(|stream| stream.write_all(line.as_bytes()).and_then(|_| stream.flush()).is_ok());
}

fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push(c.as_os_str()),
            Component::Normal(s) => out.push(s),
            Component::ParentDir => {
                out.pop();
                if out.as_os_str().is_empty() {
                    out.push("/");
                }
            }
            Component::CurDir | Component::Prefix(_) => {}
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn within_root(path: &Path, root: &Path) -> bool {
    let cp = path.canonicalize().unwrap_or_else(|_| normalize_path(path));
    let cr = root.canonicalize().unwrap_or_else(|_| normalize_path(root));
    cp.starts_with(&cr)
}

fn default_start_dir(state: &ServerState) -> PathBuf {
    let root = state.browse_root.canonicalize().unwrap_or_else(|_| normalize_path(&state.browse_root));
    if root == Path::new("/") {
        for candidate in ["/media", "/data", "/volume1", "/watch/in"] {
            let p = Path::new(candidate);
            if p.is_dir() {
                return p.to_path_buf();
            }
        }
    }
    root
}

fn discover_roots(state: &ServerState) -> Vec<String> {
    let root = state.browse_root.canonicalize().unwrap_or_else(|_| normalize_path(&state.browse_root));
    let mut roots = Vec::new();
    let mut push = |p: &Path| {
        if p.is_dir() && within_root(p, &root) {
            let s = ps(p);
            if !roots.contains(&s) {
                roots.push(s);
            }
        }
    };
    push(&root);
    for c in ["/media", "/data", "/mnt", "/volume1", "/volume2", "/watch/in", "/watch/out"] {
        push(Path::new(c));
    }
    let ws = state.watch_state.lock().unwrap();
    if ws.enabled {
        push(Path::new(&ws.input_dir));
        push(Path::new(&ws.output_dir));
    }
    roots
}

fn handle_fs_list(state: &ServerState, requested: Option<&str>) -> Result<FsListResponse, String> {
    let root = state.browse_root.canonicalize().unwrap_or_else(|_| normalize_path(&state.browse_root));
    let target = match requested.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => PathBuf::from(s),
        None => default_start_dir(state),
    };
    let canon = target.canonicalize().map_err(|e| format!("cannot open {}: {e}", target.display()))?;
    if !canon.is_dir() {
        return Err(format!("not a folder: {}", canon.display()));
    }
    if !within_root(&canon, &root) {
        return Err(format!("path is outside browse root ({})", root.display()));
    }
    let parent = if canon != root { canon.parent().filter(|p| within_root(p, &root)).map(ps) } else { None };

    let rd = std::fs::read_dir(&canon).map_err(|e| format!("cannot read {}: {e}", canon.display()))?;
    let mut dirs = Vec::new();
    let mut clips = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if is_ignored_name(&name) {
            continue;
        }
        if p.is_dir() {
            dirs.push(FsEntry { name, path: ps(&p), kind: "dir", codec: None, size: 0 });
        } else if p.is_file() && is_clip(&p) {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            let codec = if ext.eq_ignore_ascii_case("r3d") { Some("R3D NE") } else { Some("N-RAW") };
            clips.push(FsEntry { name, path: ps(&p), kind: "clip", codec, size });
        }
    }
    dirs.sort_by_key(|a| a.name.to_lowercase());
    clips.sort_by_key(|a| a.name.to_lowercase());
    dirs.extend(clips);

    Ok(FsListResponse { path: ps(&canon), parent, browse_root: ps(&root), roots: discover_roots(state), entries: dirs })
}

fn handle_fs_mkdir(state: &ServerState, args: FsMkdirArgs) -> Result<serde_json::Value, String> {
    let root = state.browse_root.canonicalize().unwrap_or_else(|_| normalize_path(&state.browse_root));
    let target = if let Some(p) = args.path.filter(|s| !s.trim().is_empty()) {
        normalize_path(Path::new(p.trim()))
    } else if let (Some(parent), Some(name)) = (args.parent, args.name) {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') || trimmed == "." || trimmed == ".." {
            return Err("invalid folder name".into());
        }
        normalize_path(&Path::new(parent.trim()).join(trimmed))
    } else {
        return Err("missing folder path or name".into());
    };
    if !within_root(&target, &root) {
        return Err(format!("path is outside browse root ({})", root.display()));
    }
    std::fs::create_dir_all(&target).map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    let canon = target.canonicalize().unwrap_or(target);
    Ok(serde_json::json!({ "path": ps(&canon) }))
}

fn queue_snapshot(state: &ServerState) -> serde_json::Value {
    let q = state.queue.lock().unwrap();
    serde_json::to_value(QueueSnapshot {
        running: state.running.load(Ordering::SeqCst),
        paused: state.paused.load(Ordering::SeqCst),
        jobs: &q.jobs,
        batch: &q.batch,
    })
    .unwrap_or(serde_json::Value::Null)
}

fn broadcast_paused(state: &ServerState) {
    broadcast_event(
        &state.sse_clients,
        "queue-paused",
        serde_json::json!({ "paused": state.paused.load(Ordering::SeqCst) }),
    );
}

fn update_job(state: &ServerState, id: u64, f: impl FnOnce(&mut QueueJob)) {
    if let Some(j) = state.queue.lock().unwrap().jobs.iter_mut().find(|j| j.id == id) {
        f(j);
    }
}

fn job_error(state: &ServerState, id: u64, error: String, cancelled: bool) {
    update_job(state, id, |j| {
        j.state = if cancelled { "cancelled".into() } else { "failed".into() };
        j.error = if cancelled { None } else { Some(error.clone()) };
    });
    if !cancelled {
        state.queue.lock().unwrap().batch.failed += 1;
    }
    broadcast_event(&state.sse_clients, "job-error", JobError { id, error, cancelled });
}

/// Seconds a finished clip actually spent compressing, without the time it was paused.
fn active_seconds(seconds: f64, paused_for: std::time::Duration) -> f64 {
    (seconds - paused_for.as_secs_f64()).max(0.0)
}

/// Frames per second of a finished clip, without the time it was paused.
fn active_fps(frames: u32, seconds: f64, fps: f64, paused_for: std::time::Duration) -> f64 {
    let s = active_seconds(seconds, paused_for);
    if paused_for.is_zero() || s <= 0.0 {
        fps
    } else {
        f64::from(frames) / s
    }
}

/// Blocks while the queue is paused. Returns false when it was cancelled meanwhile.
fn wait_while_paused(state: &ServerState) -> bool {
    while state.paused.load(Ordering::SeqCst) && !state.cancel.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    !state.cancel.load(Ordering::SeqCst)
}

/// Adds jobs to the server queue and starts the worker when it is idle. While a queue is
/// running the new jobs are appended and picked up in turn with the run's settings.
fn start_queue(state: &Arc<ServerState>, args: StartArgs) -> Result<(), String> {
    {
        let mut q = state.queue.lock().unwrap();
        let running = state.running.load(Ordering::SeqCst);
        if !running {
            q.settings = args.settings.clone();
            q.batch = Batch::default();
            // A new batch is exactly what was sent: waiting clips left over from a stopped
            // run that are not in it (for example ones now already done) are dropped.
            let ids: Vec<u64> = args.jobs.iter().filter(|j| j.existing.is_none()).map(|j| j.id).collect();
            q.jobs.retain(|j| j.state != "waiting" || ids.contains(&j.id));
        }
        for job in args.jobs {
            let name = Path::new(&job.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let bytes = std::fs::metadata(&job.path).map(|m| m.len()).unwrap_or(0);
            let counts = job.existing.is_none();
            let (state_name, fraction, output, result) = match &job.existing {
                Some(e) => (
                    "done",
                    1.0,
                    Some(e.output.clone()),
                    Some(JobDone {
                        id: job.id,
                        skipped: true,
                        output: e.output.clone(),
                        input_bytes: bytes,
                        output_bytes: e.bytes,
                        frames: 0,
                        seconds: 0.0,
                        fps: 0.0,
                        backend: String::new(),
                        proxy: None,
                        proxy_error: None,
                    }),
                ),
                None => ("waiting", 0.0, None, None),
            };
            let target = target_of(Path::new(&job.path), job.quality, job.bpp, job.lossless);
            let (quality, bpp) = target.option_fields();
            let entry = QueueJob {
                id: job.id,
                path: job.path,
                name,
                bytes,
                quality,
                bpp,
                lossless: target.is_lossless(),
                resolution: job.resolution,
                force: job.force,
                proxy: job.proxy,
                clip: job.clip,
                state: state_name.into(),
                fraction,
                fps: 0.0,
                output,
                result,
                error: None,
            };
            match q.jobs.iter_mut().find(|j| j.id == entry.id) {
                Some(existing) if existing.state != "running" => {
                    *existing = entry;
                    q.batch.total += u32::from(counts);
                }
                Some(_) => {}
                None => {
                    q.jobs.push(entry);
                    q.batch.total += u32::from(counts);
                }
            }
        }
    }
    let already_running = state.running.swap(true, Ordering::SeqCst);
    broadcast_event(&state.sse_clients, "queue-changed", queue_snapshot(state));
    if already_running {
        return Ok(());
    }
    if let Some(h) = state.worker.lock().unwrap().take() {
        let _ = h.join();
    }
    state.cancel.store(false, Ordering::SeqCst);
    state.paused.store(false, Ordering::SeqCst);
    let st = state.clone();
    let handle = std::thread::spawn(move || {
        loop {
            if !wait_while_paused(&st) {
                break;
            }
            // Next waiting job, in queue order (jobs added during the run included).
            let (job, settings) = {
                let q = st.queue.lock().unwrap();
                match q.jobs.iter().find(|j| j.state == "waiting") {
                    Some(j) => (j.clone(), q.settings.clone()),
                    None => break,
                }
            };
            run_job(&st, &job, &settings);
        }
        st.running.store(false, Ordering::SeqCst);
        if st.paused.swap(false, Ordering::SeqCst) {
            broadcast_paused(&st);
        }
        broadcast_event(&st.sse_clients, "queue-finished", serde_json::Value::Null);
    });
    *state.worker.lock().unwrap() = Some(handle);
    Ok(())
}

fn run_job(st: &Arc<ServerState>, job: &QueueJob, settings: &Settings) {
    let backend = backend_of(&settings.backend, st.default_backend);
    let folder = settings.output_folder.as_deref();
    let target = job_target(job);
    let (quality, bpp) = target.option_fields();
    let input = PathBuf::from(&job.path);
    let info = bluecompress::probe(&input).ok();
    let resized = info
        .as_ref()
        .map(|i| bluecompress::output_size(i.width, i.height, resolution_of(&job.resolution)) != (i.width, i.height))
        .unwrap_or(false);
    let res_name = if resized { job.resolution.as_str() } else { "original" };

    // Already compressed by an earlier (for example interrupted) run: keep it.
    if !job.force {
        if let Some((out, out_bytes)) = existing_for(&input, folder, target, res_name) {
            let done = JobDone {
                id: job.id,
                skipped: true,
                output: ps(&out),
                input_bytes: job.bytes,
                output_bytes: out_bytes,
                frames: info.as_ref().map(|i| i.frames).unwrap_or(0),
                seconds: 0.0,
                fps: 0.0,
                backend: String::new(),
                proxy: None,
                proxy_error: None,
            };
            update_job(st, job.id, |j| {
                j.state = "done".into();
                j.fraction = 1.0;
                j.output = Some(done.output.clone());
                j.result = Some(done.clone());
            });
            {
                let mut q = st.queue.lock().unwrap();
                q.batch.done += 1;
                q.batch.skipped += 1;
            }
            broadcast_event(&st.sse_clients, "job-done", done);
            return;
        }
    }

    let output = output_for(&input, folder, target, res_name);
    if let Some(dir) = output.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            job_error(st, job.id, format!("cannot create {}: {e}", dir.display()), false);
            return;
        }
    }
    let lock = st.transcode_lock.clone();
    let mut guard = Some(lock.lock().unwrap());
    if st.cancel.load(Ordering::SeqCst) {
        return;
    }
    let out_s = ps(&output);
    update_job(st, job.id, |j| {
        j.state = "running".into();
        j.output = Some(out_s.clone());
        j.fraction = 0.0;
        j.fps = 0.0;
    });
    broadcast_event(&st.sse_clients, "job-start", JobStart { id: job.id, output: out_s.clone() });
    let options = Options {
        input,
        output,
        quality,
        bpp,
        resolution: resolution_of(&job.resolution),
        backend,
        threads: st.threads,
    };
    let mut last = Instant::now() - Duration::from_secs(1);
    let clients = st.sse_clients.clone();
    // The core reports frames per second since the clip started; take paused time out.
    let started = Instant::now();
    let mut paused_for = Duration::ZERO;
    let mut on_progress = |fraction: f64, fps: f64| {
        let total = started.elapsed().as_secs_f64();
        let active = total - paused_for.as_secs_f64();
        let fps = if active > 0.0 { fps * total / active } else { fps };
        if fraction >= 1.0 || last.elapsed().as_millis() >= 100 {
            last = Instant::now();
            update_job(st, job.id, |j| {
                j.fraction = fraction;
                j.fps = fps;
            });
            broadcast_event(&clients, "job-progress", JobProgress { id: job.id, fraction, fps });
        }
        // Pause mid-clip: hold the writer here, which stalls the frame workers once their
        // small read-ahead is full. The shared transcode lock is released while paused so
        // the watch folder keeps working, and taken back before the clip continues.
        if st.paused.load(Ordering::SeqCst) {
            let since = Instant::now();
            guard = None;
            wait_while_paused(st);
            if !st.cancel.load(Ordering::SeqCst) {
                guard = Some(lock.lock().unwrap());
            }
            paused_for += since.elapsed();
            last = Instant::now() - Duration::from_secs(1);
        }
    };
    let result = bluecompress::transcode_staged(&options, &st.cancel, &mut on_progress);
    // The proxy (from the source, next to the output) runs under the same lock.
    let proxy = match &result {
        Ok(_) if job.proxy && !st.cancel.load(Ordering::SeqCst) => {
            let target = bluecompress::proxy_path(&options.output);
            broadcast_event(&st.sse_clients, "job-proxy", JobProgress { id: job.id, fraction: 0.0, fps: 0.0 });
            let mut last = Instant::now() - Duration::from_secs(1);
            let clients = st.sse_clients.clone();
            let r = bluecompress::make_proxy(
                &bluecompress::ProxyOptions::new(&options.input, &target),
                &st.cancel,
                &mut |fraction, fps| {
                    if fraction >= 1.0 || last.elapsed().as_millis() >= 100 {
                        last = Instant::now();
                        broadcast_event(&clients, "job-proxy", JobProgress { id: job.id, fraction, fps });
                    }
                },
            );
            match r {
                Ok(p) => (Some(ps(&p.output)), None),
                Err(_) if st.cancel.load(Ordering::SeqCst) => (None, None),
                Err(e) => (None, Some(e.0)),
            }
        }
        _ => (None, None),
    };
    drop(guard);
    match result {
        Ok(o) => {
            let done = JobDone {
                proxy: proxy.0,
                proxy_error: proxy.1,
                id: job.id,
                skipped: false,
                output: out_s,
                input_bytes: o.input_bytes,
                output_bytes: o.output_bytes,
                frames: o.frames,
                seconds: active_seconds(o.seconds, paused_for),
                fps: active_fps(o.frames, o.seconds, o.fps, paused_for),
                backend: o.backend,
            };
            update_job(st, job.id, |j| {
                j.state = "done".into();
                j.fraction = 1.0;
                j.result = Some(done.clone());
            });
            {
                let mut q = st.queue.lock().unwrap();
                q.batch.done += 1;
                q.batch.input_bytes += done.input_bytes;
                q.batch.output_bytes += done.output_bytes;
            }
            broadcast_event(&st.sse_clients, "job-done", done);
        }
        Err(e) => {
            let cancelled = st.cancel.load(Ordering::SeqCst);
            job_error(st, job.id, e.to_string(), cancelled);
        }
    }
}

fn invoke_command(state: &Arc<ServerState>, cmd: &str, body: &[u8]) -> HttpResponse {
    match cmd {
        "app_info" => {
            let info = AppInfo {
                version: bluecompress::VERSION.into(),
                gpu: bluecompress::backend_description(Backend::Auto),
                cpu: bluecompress::backend_description(Backend::Cpu),
                platform: std::env::consts::OS.into(),
                web: true,
                browse_root: ps(&state.browse_root),
                default_quality: state.default_quality,
                default_bpp: state.default_bpp,
                default_resolution: resolution_name(state.default_resolution).into(),
                default_proxies: state.default_proxies,
                proxy_encoder: bluecompress::proxy::encoder_description(),
                watch: state.watch_state.lock().unwrap().clone(),
            };
            HttpResponse::json(200, &info)
        }
        "update_status" => {
            let s = state.update.lock().unwrap().clone();
            HttpResponse::json(200, &s)
        }
        "watch_status" => {
            let snap = state.watch_state.lock().unwrap().clone();
            HttpResponse::json(200, &snap)
        }
        "set_watch_settings" => {
            let args: SetWatchSettingsArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid set_watch_settings args: {e}")),
            };
            let snap = {
                let mut ws = state.watch_state.lock().unwrap();
                if let Some(c) = args.clear_after_process {
                    ws.clear_after_process = c;
                }
                let quality = if args.lossless { Some(1) } else { args.quality };
                if let Some(q) = quality {
                    ws.quality = q.clamp(1, 4);
                }
                if let (Some(b), false) = (args.bpp, args.lossless) {
                    ws.bpp = Target::bpp(b).bpp_value();
                } else if quality.is_some() {
                    ws.bpp = None;
                }
                if let Some(r) = args.resolution.as_deref().and_then(parse_resolution) {
                    ws.resolution = resolution_name(r).into();
                }
                if ws.enabled {
                    let out_dir = PathBuf::from(&ws.output_dir);
                    let qv = ws.quality;
                    let bv = ws.bpp;
                    let rv = ws.resolution.clone();
                    let cv = ws.clear_after_process;
                    save_persisted_settings(&out_dir, |p| {
                        if quality.is_some() || args.bpp.is_some() {
                            p.quality = Some(qv);
                            p.bpp = bv;
                        }
                        if args.resolution.is_some() {
                            p.resolution = Some(rv);
                        }
                        if args.clear_after_process.is_some() {
                            p.clear_after_process = Some(cv);
                        }
                    });
                }
                ws.clone()
            };
            broadcast_event(&state.sse_clients, "watch-status", &snap);
            HttpResponse::json(200, &snap)
        }
        "is_running" => HttpResponse::json(200, &state.running.load(Ordering::SeqCst)),
        "get_queue" => HttpResponse::json(200, &queue_snapshot(state)),
        "check_existing" => {
            let args: CheckExistingArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid check_existing args: {e}")),
            };
            HttpResponse::json(200, &check_existing_outputs(args.output_folder.as_deref(), &args.items))
        }
        "set_paused" => {
            let args: SetPausedArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid set_paused args: {e}")),
            };
            state.paused.store(args.paused && state.running.load(Ordering::SeqCst), Ordering::SeqCst);
            broadcast_paused(state);
            HttpResponse::json(200, &serde_json::json!({ "paused": state.paused.load(Ordering::SeqCst) }))
        }
        "remove_jobs" => {
            let args: RemoveJobsArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid remove_jobs args: {e}")),
            };
            let running = state.running.load(Ordering::SeqCst);
            {
                let mut q = state.queue.lock().unwrap();
                let waiting_before = q.jobs.iter().filter(|j| j.state == "waiting").count();
                q.jobs.retain(|j| {
                    // The clip being compressed always stays; waiting clips only go when
                    // asked by id or, with `all`, while nothing runs.
                    if j.state == "running" {
                        return true;
                    }
                    match &args.ids {
                        Some(ids) => !ids.contains(&j.id),
                        None => j.state == "waiting" && (running || !args.all),
                    }
                });
                // Waiting clips taken out of a running batch no longer count towards it;
                // emptying the idle list starts the counter afresh.
                let waiting_after = q.jobs.iter().filter(|j| j.state == "waiting").count();
                if running {
                    q.batch.total = q.batch.total.saturating_sub((waiting_before - waiting_after) as u32);
                } else if q.jobs.is_empty() {
                    q.batch = Batch::default();
                }
            }
            let snap = queue_snapshot(state);
            broadcast_event(&state.sse_clients, "queue-changed", &snap);
            HttpResponse::json(200, &snap)
        }
        "initial_paths" => {
            let paths: Vec<String> = std::mem::take(&mut *state.initial.lock().unwrap());
            HttpResponse::json(200, &paths)
        }
        "scan" => {
            let args: ScanArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid scan args: {e}")),
            };
            let mut files = Vec::new();
            for p in &args.paths {
                collect_clips(Path::new(p), &mut files);
            }
            let clips: Vec<Clip> = files.iter().map(|f| describe_clip(f)).collect();
            HttpResponse::json(200, &clips)
        }
        "plan_output" => {
            let args: PlanOutputArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid plan_output args: {e}")),
            };
            let (w, h) = bluecompress::output_size(args.width, args.height, resolution_of(&args.resolution));
            HttpResponse::json(200, &(w, h))
        }
        "preview" => {
            let args: PreviewArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid preview args: {e}")),
            };
            let res = resolution_of(&args.resolution);
            let backend = backend_of(&args.backend, state.default_backend);
            let crop = args.crop.unwrap_or([0.0, 0.0, 1.0, 1.0]);
            match bluecompress::preview_region_for(
                Path::new(&args.path),
                args.frame,
                target_of(Path::new(&args.path), args.quality, args.bpp, args.lossless),
                res,
                backend,
                crop,
                args.width.clamp(16, 8192),
            ) {
                Ok(p) => {
                    let mut out = Vec::with_capacity(32 + p.before_rgba.len() + p.after_rgba.len());
                    for v in [p.width, p.height, p.frames, 0] {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                    out.extend_from_slice(&p.source_frame_bytes.to_le_bytes());
                    out.extend_from_slice(&p.output_frame_bytes.to_le_bytes());
                    out.extend_from_slice(&p.before_rgba);
                    out.extend_from_slice(&p.after_rgba);
                    HttpResponse::bytes(200, "application/octet-stream", out)
                }
                Err(e) => HttpResponse::text(400, &e.to_string()),
            }
        }
        "thumbnail" => {
            let args: ThumbnailArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid thumbnail args: {e}")),
            };
            let frames = bluecompress::probe(Path::new(&args.path)).map(|i| i.frames).unwrap_or(1);
            match bluecompress::thumbnail(Path::new(&args.path), frames / 4, args.width) {
                Ok(t) => {
                    let mut out = Vec::with_capacity(8 + t.rgba.len());
                    out.extend_from_slice(&t.width.to_le_bytes());
                    out.extend_from_slice(&t.height.to_le_bytes());
                    out.extend_from_slice(&t.rgba);
                    HttpResponse::bytes(200, "application/octet-stream", out)
                }
                Err(e) => HttpResponse::text(400, &e.to_string()),
            }
        }
        "start" => {
            let args: StartArgs = match serde_json::from_slice(body) {
                Ok(a) => a,
                Err(e) => return HttpResponse::text(400, &format!("invalid start args: {e}")),
            };
            match start_queue(state, args) {
                Ok(()) => HttpResponse::json(200, &serde_json::Value::Null),
                Err(e) => HttpResponse::text(409, &e),
            }
        }
        "cancel" | "confirm_quit" => {
            state.cancel.store(true, Ordering::SeqCst);
            if state.paused.swap(false, Ordering::SeqCst) {
                broadcast_paused(state);
            }
            HttpResponse::json(200, &serde_json::Value::Null)
        }
        "reveal" => HttpResponse::json(200, &serde_json::Value::Null),
        _ => HttpResponse::text(404, &format!("unknown command: {cmd}")),
    }
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn text(status: u16, s: &str) -> Self {
        Self { status, content_type: "text/plain; charset=utf-8", body: s.as_bytes().to_vec() }
    }

    fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status, content_type, body }
    }

    fn json<T: Serialize>(status: u16, v: &T) -> Self {
        match serde_json::to_vec(v) {
            Ok(body) => Self { status, content_type: "application/json; charset=utf-8", body },
            Err(e) => Self::text(500, &e.to_string()),
        }
    }

    fn write_to(self, mut stream: TcpStream) {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Internal Server Error",
        };
        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
            self.status,
            reason,
            self.content_type,
            self.body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&self.body);
        let _ = stream.flush();
    }
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
            out.push(b[i]);
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next() == Some(key) {
            return Some(url_decode(kv.next().unwrap_or("")));
        }
    }
    None
}

fn handle_client(mut stream: TcpStream, state: Arc<ServerState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_target = parts.next().unwrap_or("/");
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_target, ""),
    };

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse::<usize>().unwrap_or(0).min(16 * 1024 * 1024);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }

    match (method, path) {
        ("GET", "/" | "/index.html") => {
            HttpResponse::bytes(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()).write_to(stream);
        }
        ("GET", "/styles.css") => {
            HttpResponse::bytes(200, "text/css; charset=utf-8", STYLES_CSS.as_bytes().to_vec()).write_to(stream);
        }
        ("GET", "/app.js") => {
            HttpResponse::bytes(200, "application/javascript; charset=utf-8", APP_JS.as_bytes().to_vec())
                .write_to(stream);
        }
        ("GET", "/icon.png" | "/favicon.ico") => {
            HttpResponse::bytes(200, "image/png", ICON_PNG.to_vec()).write_to(stream);
        }
        ("GET", "/api/health" | "/healthz") => {
            HttpResponse::json(200, &serde_json::json!({ "ok": true, "version": bluecompress::VERSION }))
                .write_to(stream);
        }
        ("GET", "/api/events") => {
            let _ = stream.set_read_timeout(None);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_nodelay(true);
            let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
            if stream.write_all(hdr.as_bytes()).is_err() {
                return;
            }
            let snap = state.watch_state.lock().unwrap().clone();
            if let Ok(s) = serde_json::to_string(&serde_json::json!({ "event": "watch-status", "payload": snap })) {
                let _ = stream.write_all(format!("data: {s}\n\n").as_bytes());
            }
            let _ = stream.flush();
            state.sse_clients.lock().unwrap().push(stream);
        }
        ("GET", "/api/fs/list") => {
            let req_path = query_param(query, "path");
            match handle_fs_list(&state, req_path.as_deref()) {
                Ok(res) => HttpResponse::json(200, &res).write_to(stream),
                Err(e) => HttpResponse::text(400, &e).write_to(stream),
            }
        }
        ("POST", "/api/fs/list") => {
            let req_path = serde_json::from_slice::<FsListArgs>(&body)
                .ok()
                .and_then(|a| a.path)
                .or_else(|| query_param(query, "path"));
            match handle_fs_list(&state, req_path.as_deref()) {
                Ok(res) => HttpResponse::json(200, &res).write_to(stream),
                Err(e) => HttpResponse::text(400, &e).write_to(stream),
            }
        }
        ("POST", "/api/fs/mkdir") => match serde_json::from_slice::<FsMkdirArgs>(&body) {
            Ok(args) => match handle_fs_mkdir(&state, args) {
                Ok(v) => HttpResponse::json(200, &v).write_to(stream),
                Err(e) => HttpResponse::text(400, &e).write_to(stream),
            },
            Err(e) => HttpResponse::text(400, &format!("invalid mkdir args: {e}")).write_to(stream),
        },
        ("GET" | "POST", p) if p.starts_with("/api/invoke/") => {
            let cmd = &p["/api/invoke/".len()..];
            invoke_command(&state, cmd, &body).write_to(stream);
        }
        _ => {
            HttpResponse::text(404, "not found").write_to(stream);
        }
    }
}

pub fn run_serve(cfg: ServeConfig, cancel: &'static AtomicBool) -> u8 {
    let listener = match TcpListener::bind(&cfg.listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", cfg.listen);
            return 1;
        }
    };

    let _ = std::fs::create_dir_all(&cfg.browse_root);
    let initial_watch = match &cfg.watch {
        Some(wcfg) => {
            let persisted = load_persisted_settings(&wcfg.output_dir);
            let qv = persisted.quality.filter(|q| (1..=4).contains(q)).unwrap_or(wcfg.quality);
            let rv = persisted.resolution.as_deref().and_then(parse_resolution).unwrap_or(wcfg.resolution);
            let cv = persisted.clear_after_process.unwrap_or(wcfg.clear_after_process);
            WatchSnapshot {
                enabled: true,
                input_dir: ps(&wcfg.input_dir),
                output_dir: ps(&wcfg.output_dir),
                quality: qv,
                bpp: crate::watch::initial_bpp(&persisted, wcfg.bpp),
                resolution: resolution_name(rv).into(),
                poll_seconds: wcfg.poll_seconds,
                clear_after_process: cv,
                state: "idle".into(),
                ..Default::default()
            }
        }
        None => WatchSnapshot::default(),
    };

    let state = Arc::new(ServerState {
        browse_root: cfg.browse_root,
        default_quality: cfg.default_quality,
        default_bpp: cfg.default_bpp,
        default_resolution: cfg.default_resolution,
        default_backend: cfg.default_backend,
        default_proxies: cfg.default_proxies,
        threads: cfg.threads,
        running: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        queue: Arc::new(Mutex::new(QueueState::default())),
        worker: Mutex::new(None),
        initial: Mutex::new(cfg.initial_paths),
        sse_clients: Arc::new(Mutex::new(Vec::new())),
        watch_state: Arc::new(Mutex::new(initial_watch)),
        transcode_lock: Arc::new(Mutex::new(())),
        update: crate::update::initial_status(),
    });
    crate::update::spawn(state.update.clone(), true);

    println!("Blue Compressor {} web UI listening on http://{}", bluecompress::VERSION, cfg.listen);
    let _ = std::io::stdout().flush();

    if let Some(wcfg) = cfg.watch {
        let watch_state = state.watch_state.clone();
        let sse = state.sse_clients.clone();
        let lock = state.transcode_lock.clone();
        let hook = Arc::new(move |snap: &WatchSnapshot| {
            broadcast_event(&sse, "watch-status", snap);
        });
        std::thread::spawn(move || {
            run_watch(wcfg, cancel, watch_state, Some(hook), Some(lock));
        });
    }

    // Heartbeat and clean shutdown thread.
    {
        let st = state.clone();
        std::thread::spawn(move || {
            let mut ticks = 0u32;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if cancel.load(Ordering::Relaxed) {
                    st.cancel.store(true, Ordering::SeqCst);
                    if let Some(h) = st.worker.lock().unwrap().take() {
                        let _ = h.join();
                    }
                    std::process::exit(0);
                }
                ticks += 1;
                if ticks >= 75 {
                    ticks = 0;
                    let mut guard = st.sse_clients.lock().unwrap();
                    guard.retain_mut(|stream| stream.write_all(b": ping\n\n").and_then(|_| stream.flush()).is_ok());
                }
            }
        });
    }

    for stream in listener.incoming().flatten() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let st = state.clone();
        std::thread::spawn(move || handle_client(stream, st));
    }
    0
}
