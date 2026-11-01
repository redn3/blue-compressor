//! Develop tab commands: as shot info, preview, auto exposure and export.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bluecompress::develop::{self, Export, Output, Params, RollOff, ToneMap};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

#[derive(Default)]
pub struct Exporting {
    cancel: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    r3d: bool,
    camera: String,
    width: u32,
    height: u32,
    frames: u32,
    fps: f64,
    iso: u32,
    kelvin: u32,
    has_audio: bool,
    renderer: String,
    output_path: String,
    as_shot: Settings,
    sidecar: Option<SidecarInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarInfo {
    name: String,
    exposure: Option<f32>,
    iso: Option<u32>,
    kelvin: Option<f32>,
    tint: Option<f32>,
}

/// The develop settings as the UI keeps them.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    output: String,
    kelvin: f32,
    tint: f32,
    exposure: f32,
    iso: f32,
    roll_off: String,
    tone_map: String,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    saturation: f32,
    color_boost: f32,
    sharpness: f32,
    #[serde(default)]
    nr_luma: f32,
    #[serde(default)]
    nr_chroma: f32,
}

fn output_name(o: Output) -> &'static str {
    match o {
        Output::Log3G10 => "log3g10",
        Output::NLog => "nlog",
        Output::Rec709 => "rec709",
    }
}

impl From<&Params> for Settings {
    fn from(p: &Params) -> Self {
        Settings {
            output: output_name(p.output).into(),
            kelvin: p.kelvin,
            tint: p.tint,
            exposure: p.exposure,
            iso: p.iso,
            roll_off: match p.roll_off {
                RollOff::None => "none",
                RollOff::Hard => "hard",
                RollOff::Medium => "medium",
                RollOff::Soft => "soft",
                RollOff::VerySoft => "verySoft",
            }
            .into(),
            tone_map: match p.tone_map {
                ToneMap::None => "none",
                ToneMap::Low => "low",
                ToneMap::Medium => "medium",
                ToneMap::High => "high",
            }
            .into(),
            contrast: p.contrast,
            highlights: p.highlights,
            shadows: p.shadows,
            saturation: p.saturation,
            color_boost: p.color_boost,
            sharpness: p.sharpness,
            nr_luma: p.nr_luma,
            nr_chroma: p.nr_chroma,
        }
    }
}

impl Settings {
    fn params(&self) -> Params {
        Params {
            output: match self.output.as_str() {
                "log3g10" => Output::Log3G10,
                "nlog" => Output::NLog,
                _ => Output::Rec709,
            },
            kelvin: self.kelvin,
            tint: self.tint,
            exposure: self.exposure,
            iso: self.iso,
            roll_off: match self.roll_off.as_str() {
                "none" => RollOff::None,
                "hard" => RollOff::Hard,
                "soft" => RollOff::Soft,
                "verySoft" => RollOff::VerySoft,
                _ => RollOff::Medium,
            },
            tone_map: match self.tone_map.as_str() {
                "none" => ToneMap::None,
                "low" => ToneMap::Low,
                "high" => ToneMap::High,
                _ => ToneMap::Medium,
            },
            contrast: self.contrast,
            highlights: self.highlights,
            shadows: self.shadows,
            saturation: self.saturation,
            color_boost: self.color_boost,
            sharpness: self.sharpness,
            nr_luma: self.nr_luma,
            nr_chroma: self.nr_chroma,
        }
    }
}

#[tauri::command]
pub async fn develop_info(path: String) -> Result<Info, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let c = develop::clip_info(Path::new(&path))?;
        let p = Params::as_shot(&c);
        Ok::<_, bluecompress::Error>(Info {
            r3d: c.r3d,
            camera: c.camera.clone(),
            width: c.width,
            height: c.height,
            frames: c.frames,
            fps: c.fps,
            iso: c.iso,
            kelvin: c.kelvin,
            has_audio: c.has_audio,
            renderer: develop::renderer_name(),
            output_path: develop::export_path(Path::new(&path), p.output).display().to_string(),
            as_shot: Settings::from(&p),
            sidecar: c.sidecar.as_ref().map(|s| SidecarInfo {
                name: s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                exposure: s.exposure,
                iso: s.iso,
                kelvin: s.kelvin,
                tint: s.tint,
            }),
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// RGBA preview: 8-byte header (width, height as u32 LE), then the pixels.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn develop_preview(
    path: String,
    frame: u32,
    settings: Settings,
    crop: Option<[f32; 4]>,
    width: u32,
    height: u32,
    view709: bool,
) -> Result<tauri::ipc::Response, String> {
    let (w, h) = (width.clamp(16, 8192) as usize, height.clamp(16, 8192) as usize);
    let rgba = tauri::async_runtime::spawn_blocking(move || {
        develop::preview(
            Path::new(&path),
            frame as usize,
            &settings.params(),
            crop.unwrap_or([0.0, 0.0, 1.0, 1.0]),
            w,
            h,
            view709,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(8 + rgba.len());
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(&rgba);
    Ok(tauri::ipc::Response::new(out))
}

#[tauri::command]
pub async fn develop_auto_exposure(path: String, frame: u32, settings: Settings) -> Result<f32, String> {
    tauri::async_runtime::spawn_blocking(move || {
        develop::auto_exposure_for(Path::new(&path), frame as usize, &settings.params())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    path: String,
    settings: Settings,
    width: u32,
    kbps: Option<u32>,
    cbr: bool,
    #[serde(default)]
    high: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress {
    path: String,
    fraction: f64,
    fps: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Finished {
    path: String,
    ok: bool,
    output: Option<String>,
    encoder: Option<String>,
    renderer: Option<String>,
    seconds: f64,
    bytes: u64,
    error: Option<String>,
}

#[tauri::command]
pub fn develop_default_kbps(width: u32, height: u32, fps: f64, high: Option<bool>) -> u32 {
    develop::quality_kbps(width, height, fps, high.unwrap_or(false))
}

/// Exports clips one after another; emits develop-progress and develop-done per clip and
/// develop-finished at the end.
#[tauri::command]
pub fn develop_export(app: AppHandle, state: State<'_, Exporting>, jobs: Vec<ExportRequest>) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err("busy".into());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let (cancel, running) = (state.cancel.clone(), state.running.clone());
    std::thread::spawn(move || {
        for j in jobs {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let params = j.settings.params();
            let input = PathBuf::from(&j.path);
            let output = develop::export_path(&input, params.output);
            let kbps = j.kbps.unwrap_or_else(|| {
                develop::clip_info(&input)
                    .map(|c| {
                        let (w, h) = develop::output_size(&c, j.width);
                        develop::quality_kbps(w, h, c.fps, j.high)
                    })
                    .unwrap_or(develop::UHD_KBPS)
            });
            let e = Export { input, output, params, width: j.width, kbps, cbr: j.cbr, encoder: None };
            let path = j.path.clone();
            let mut last = 0.0;
            let r = develop::export(&e, &cancel, &mut |fraction, fps| {
                if fraction - last >= 0.005 || fraction >= 1.0 {
                    last = fraction;
                    let _ = app.emit("develop-progress", Progress { path: path.clone(), fraction, fps });
                }
            });
            let done = match r {
                Ok(d) => Finished {
                    path: j.path,
                    ok: true,
                    output: Some(d.output.display().to_string()),
                    encoder: Some(d.encoder),
                    renderer: Some(d.renderer),
                    seconds: d.seconds,
                    bytes: d.output_bytes,
                    error: None,
                },
                Err(err) => Finished {
                    path: j.path,
                    ok: false,
                    output: None,
                    encoder: None,
                    renderer: None,
                    seconds: 0.0,
                    bytes: 0,
                    error: Some(err.to_string()),
                },
            };
            let _ = app.emit("develop-done", done);
        }
        running.store(false, Ordering::SeqCst);
        let _ = app.emit("develop-finished", ());
    });
    Ok(())
}

#[tauri::command]
pub fn develop_cancel(state: State<'_, Exporting>) {
    state.cancel.store(true, Ordering::SeqCst);
}

/// Playback in the Develop tab (one clip at a time).
#[derive(Default)]
pub struct Playing(std::sync::Mutex<Option<develop::Player>>);

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn develop_play_start(
    state: State<'_, Playing>,
    path: String,
    frame: u32,
    settings: Settings,
    width: u32,
    height: u32,
    view709: bool,
) -> Result<(), String> {
    let (w, h) = (width.clamp(16, 4096) as usize, height.clamp(16, 4096) as usize);
    // Stop what plays before starting anew.
    let old = state.0.lock().map_err(|e| e.to_string())?.take();
    let p = settings.params();
    let player = tauri::async_runtime::spawn_blocking(move || {
        drop(old);
        develop::Player::start(Path::new(&path), frame as usize, &p, w, h, view709)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    *state.0.lock().map_err(|e| e.to_string())? = Some(player);
    Ok(())
}

#[tauri::command]
pub async fn develop_play_settings(app: AppHandle, settings: Settings, view709: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        if let Ok(g) = app.state::<Playing>().0.lock() {
            if let Some(p) = g.as_ref() {
                p.set(&settings.params(), view709);
            }
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// The next playback frame: 12-byte header (width, height, frame as u32 LE) then RGBA.
/// Only the header when no frame came in time; frame u32::MAX at the end of the clip.
#[tauri::command]
pub async fn develop_play_frame(app: AppHandle) -> Result<tauri::ipc::Response, String> {
    let r =
        tauri::async_runtime::spawn_blocking(move || {
            use tauri::Manager;
            let state = app.state::<Playing>();
            let g = state.0.lock().map_err(|e| e.to_string())?;
            match g.as_ref() {
                None => Ok::<_, String>(develop::Played::End),
                Some(p) => Ok(p.next(std::time::Duration::from_millis(60)).unwrap_or(develop::Played::Frame(
                    develop::PlayFrame { index: u32::MAX as usize - 1, w: 0, h: 0, rgba: Vec::new() },
                ))),
            }
        })
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    match r? {
        develop::Played::Frame(f) => {
            out.reserve(12 + f.rgba.len());
            out.extend_from_slice(&(f.w as u32).to_le_bytes());
            out.extend_from_slice(&(f.h as u32).to_le_bytes());
            out.extend_from_slice(&(f.index as u32).to_le_bytes());
            out.extend_from_slice(&f.rgba);
        }
        develop::Played::End => {
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        develop::Played::Failed(e) => return Err(e),
    }
    Ok(tauri::ipc::Response::new(out))
}

#[tauri::command]
pub async fn develop_play_stop(state: State<'_, Playing>) -> Result<(), String> {
    let old = state.0.lock().map_err(|e| e.to_string())?.take();
    tauri::async_runtime::spawn_blocking(move || drop(old)).await.map_err(|e| e.to_string())
}
