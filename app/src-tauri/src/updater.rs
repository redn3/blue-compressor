//! In-app updates through the official Tauri updater plugin. The app asks GitHub for the
//! signed `latest.json` of the newest release (one anonymous GET, nothing else is sent),
//! and the web UI shows the prompt. Installing downloads the update for this platform,
//! checks its signature against the public key in `tauri.conf.json`, replaces the app and
//! relaunches it. It never runs while the queue is compressing.
//!
//! Linux .deb and .rpm installs cannot replace themselves, so there (and whenever an
//! install fails) the prompt offers the release page instead.
//!
//! Test builds: when `BC_UPDATER_TEST_ENDPOINT` is set at compile time, that manifest is
//! used instead of the real one and the update is offered until it has been installed once
//! (a marker file in the app data folder), so the whole flow can be tried with a test
//! manifest without publishing a real release.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::Runner;

/// Where people download releases by hand.
pub const RELEASES_PAGE: &str = "https://github.com/redn3/blue-compressor/releases/latest";

/// The test manifest of a test build (`BC_UPDATER_TEST_ENDPOINT` at compile time, if set
/// and not empty).
fn test_endpoint() -> Option<&'static str> {
    option_env!("BC_UPDATER_TEST_ENDPOINT").filter(|s| !s.trim().is_empty())
}

/// The update found by the last check, kept for [`update_install`].
#[derive(Default)]
pub struct Pending(Mutex<Option<Update>>);

#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    version: String,
    current: String,
    notes: String,
    date: Option<String>,
    /// False where the app cannot replace itself (Linux .deb or .rpm installs): offer the
    /// release page instead.
    can_install: bool,
    page: String,
    /// A test build reading a test manifest.
    test: bool,
}

/// Whether this install can update itself.
fn can_self_install() -> bool {
    if cfg!(target_os = "linux") {
        // Only the AppImage can replace itself; .deb and .rpm belong to the package manager.
        std::env::var_os("APPIMAGE").is_some()
    } else {
        true
    }
}

fn test_marker(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("updater-test-installed"))
}

fn page_for(version: &str) -> String {
    if test_endpoint().is_some() {
        return RELEASES_PAGE.into();
    }
    format!("https://github.com/redn3/blue-compressor/releases/tag/v{}", version.trim_start_matches('v'))
}

/// Checks for a newer version. `Ok(None)` when up to date; errors (offline, GitHub down)
/// are returned as text and shown only when the check was asked for.
#[tauri::command]
pub async fn update_check(app: AppHandle, pending: State<'_, Pending>) -> Result<Option<UpdateInfo>, String> {
    let mut b = app.updater_builder().timeout(Duration::from_secs(20));
    if let Some(ep) = test_endpoint() {
        let url = ep.parse().map_err(|e| format!("bad test endpoint: {e}"))?;
        b = b.endpoints(vec![url]).map_err(|e| e.to_string())?;
        let installed = test_marker(&app).is_some_and(|m| m.exists());
        b = b.version_comparator(move |_, _| !installed);
    }
    let update = b.build().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let info = update.as_ref().map(|u| UpdateInfo {
        version: u.version.clone(),
        current: u.current_version.clone(),
        notes: u.body.clone().unwrap_or_default(),
        date: u.date.map(|d| d.to_string()),
        can_install: can_self_install(),
        page: page_for(&u.version),
        test: test_endpoint().is_some(),
    });
    *pending.0.lock().unwrap() = update;
    Ok(info)
}

/// Downloads and installs the update found by the last check, then relaunches. Emits
/// `update-progress` [downloaded bytes, total bytes or null] and `update-installing`.
#[tauri::command]
pub async fn update_install(
    app: AppHandle,
    pending: State<'_, Pending>,
    runner: State<'_, Runner>,
) -> Result<(), String> {
    if runner.running.load(Ordering::SeqCst) {
        return Err("The queue is still compressing.".into());
    }
    if !can_self_install() {
        return Err("This install cannot update itself.".into());
    }
    let update = pending.0.lock().unwrap().clone().ok_or("No update to install.")?;
    let mut got = 0u64;
    let progress = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                got += chunk as u64;
                let _ = progress.emit("update-progress", (got, total));
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.emit("update-installing", ());
    // A test build offers its test update once (on Windows the installer quits the app, so
    // the marker is written first and removed again if the install fails).
    let marker = test_endpoint().and(test_marker(&app));
    if let Some(m) = &marker {
        if let Some(d) = m.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(m, &update.version);
    }
    if let Err(e) = update.install(bytes) {
        if let Some(m) = &marker {
            let _ = std::fs::remove_file(m);
        }
        return Err(e.to_string());
    }
    app.restart();
}

/// Opens the release page in the browser (only GitHub release pages of this project).
#[tauri::command]
pub fn open_release_page(url: String) -> Result<(), String> {
    let url =
        if url.starts_with("https://github.com/redn3/blue-compressor/releases") { url } else { RELEASES_PAGE.into() };
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}
