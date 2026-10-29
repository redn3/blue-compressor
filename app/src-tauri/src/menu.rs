//! The native menu bar. "Check for Updates..." sits in the app menu on macOS (after About)
//! and in the Help menu elsewhere; it asks the page to run a manual update check, which
//! always reports back (the update prompt, "up to date" or an error). The page sends the
//! labels in its language with `set_menu_labels`, and the menu is rebuilt with them.

use std::collections::HashMap;

use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Runtime};

pub const CHECK_UPDATES: &str = "check-updates";

fn label<'a>(l: &'a HashMap<String, String>, key: &'a str) -> &'a str {
    l.get(key).map(String::as_str).filter(|s| !s.is_empty()).unwrap_or(key)
}

pub fn build<R: Runtime>(app: &AppHandle<R>, l: &HashMap<String, String>) -> tauri::Result<Menu<R>> {
    let name = app.package_info().name.clone();
    let about = PredefinedMenuItem::about(
        app,
        Some(label(l, "About Blue Compressor")),
        Some(AboutMetadata {
            name: Some(name),
            version: Some(app.package_info().version.to_string()),
            ..Default::default()
        }),
    )?;
    let check = MenuItem::with_id(app, CHECK_UPDATES, label(l, "Check for Updates..."), true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let edit = Submenu::with_items(
        app,
        label(l, "Edit"),
        true,
        &[
            &PredefinedMenuItem::undo(app, Some(label(l, "Undo")))?,
            &PredefinedMenuItem::redo(app, Some(label(l, "Redo")))?,
            &sep()?,
            &PredefinedMenuItem::cut(app, Some(label(l, "Cut")))?,
            &PredefinedMenuItem::copy(app, Some(label(l, "Copy")))?,
            &PredefinedMenuItem::paste(app, Some(label(l, "Paste")))?,
            &PredefinedMenuItem::select_all(app, Some(label(l, "Select All")))?,
        ],
    )?;
    #[cfg(target_os = "macos")]
    {
        let app_menu = Submenu::with_items(
            app,
            "Blue Compressor",
            true,
            &[
                &about,
                &sep()?,
                &check,
                &sep()?,
                &PredefinedMenuItem::services(app, Some(label(l, "Services")))?,
                &sep()?,
                &PredefinedMenuItem::hide(app, Some(label(l, "Hide Blue Compressor")))?,
                &PredefinedMenuItem::hide_others(app, Some(label(l, "Hide Others")))?,
                &PredefinedMenuItem::show_all(app, Some(label(l, "Show All")))?,
                &sep()?,
                &PredefinedMenuItem::quit(app, Some(label(l, "Quit Blue Compressor")))?,
            ],
        )?;
        let window = Submenu::with_items(
            app,
            label(l, "Window"),
            true,
            &[
                &PredefinedMenuItem::minimize(app, Some(label(l, "Minimize")))?,
                &PredefinedMenuItem::maximize(app, Some(label(l, "Zoom")))?,
                &sep()?,
                &PredefinedMenuItem::close_window(app, Some(label(l, "Close Window")))?,
            ],
        )?;
        Menu::with_items(app, &[&app_menu, &edit, &window])
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &edit;
        let help = Submenu::with_items(app, label(l, "Help"), true, &[&check, &sep()?, &about])?;
        Menu::with_items(app, &[&help])
    }
}

/// Builds the menu with English labels and forwards "Check for Updates..." to the page.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    app.set_menu(build(app, &HashMap::new())?)?;
    app.on_menu_event(|app, event| {
        if event.id() == CHECK_UPDATES {
            let _ = app.emit("menu-check-updates", ());
        }
    });
    Ok(())
}

/// Rebuilds the menu with the page's translated labels (keys are the English labels).
#[tauri::command]
pub fn set_menu_labels(app: AppHandle, labels: HashMap<String, String>) -> Result<(), String> {
    let menu = build(&app, &labels).map_err(|e| e.to_string())?;
    app.set_menu(menu).map(|_| ()).map_err(|e| e.to_string())
}
