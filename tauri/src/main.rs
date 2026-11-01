//! TokenScope (Tauri): native dashboard window + tray icon with a quota popup.
//! The UI pages live in `ui-src/`; build.rs copies them to `ui/` with the API shim.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alerts;
mod events;
mod paths;
mod powerbi;
mod pricing;
mod util;

use serde_json::{json, Value};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

const POPUP_W: f64 = 304.0;
const POPUP_H: f64 = 470.0;
static THEME: Mutex<Option<String>> = Mutex::new(None);
// When the popup hid on focus loss; a tray click right after must not reopen it.
static POPUP_HIDDEN_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

// ----- data commands (blocking file IO runs off the main thread) -----
#[tauri::command]
async fn summary(days: i64, tool: Option<String>, project: Option<String>) -> Result<Value, String> {
    let tool = tool.filter(|t| matches!(t.as_str(), "claude_api" | "codex" | "antigravity"));
    let project = project.filter(|p| !p.is_empty() && p.len() <= 400);
    tauri::async_runtime::spawn_blocking(move || {
        events::read_local_summary(days.clamp(1, 100_000), tool.as_deref(), project.as_deref())
    })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn utilization() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(util::read_local_utilization).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn status() -> Value {
    util::read_local_status()
}

fn theme_file() -> std::path::PathBuf {
    paths::tokenscope_dir().join("theme.json")
}

/// Shared dark/light choice between the dashboard and the popup (persisted).
#[tauri::command]
fn theme(theme: Option<String>) -> Value {
    let mut cur = THEME.lock().unwrap();
    if cur.is_none() {
        *cur = std::fs::read_to_string(theme_file()).ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["theme"].as_str().map(str::to_string));
    }
    if let Some(t) = theme.filter(|t| t == "dark" || t == "light") {
        let _ = std::fs::create_dir_all(paths::tokenscope_dir());
        let _ = std::fs::write(theme_file(), json!({ "theme": t }).to_string());
        *cur = Some(t);
    }
    json!({ "theme": cur.clone().unwrap_or_else(|| "dark".into()) })
}

// ----- "Start with Windows" (HKCU Run key, launches minimized to tray) -----
#[cfg(windows)]
mod startup {
    use winreg::{enums::*, RegKey};
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    pub fn enabled() -> bool {
        RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN)
            .and_then(|k| k.get_value::<String, _>("TokenScope")).is_ok_and(|v| !v.is_empty())
    }
    pub fn set(on: bool) {
        let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN, KEY_SET_VALUE) else { return };
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = k.set_value("TokenScope", &format!("\"{}\" --minimized", exe.display()));
            }
        } else {
            let _ = k.delete_value("TokenScope");
        }
    }
}
#[cfg(not(windows))]
mod startup {
    pub fn enabled() -> bool { false }
    pub fn set(_: bool) {}
}

#[tauri::command]
fn startup_enabled() -> bool {
    startup::enabled()
}

#[tauri::command]
fn toggle_startup() -> bool {
    startup::set(!startup::enabled());
    startup::enabled()
}

#[tauri::command]
fn startup_label() -> String {
    "Start with Windows".into()
}

// ----- windows -----
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}

fn show_main(app: &AppHandle) {
    if let Some(w) = main_window(app) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn show_dashboard(app: AppHandle) {
    hide_popup(&app);
    show_main(&app);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

fn hide_popup(app: &AppHandle) {
    if let Some(p) = app.get_webview_window("tray") {
        let _ = p.hide();
    }
}

#[tauri::command]
fn dismiss(app: AppHandle) {
    hide_popup(&app);
}

/// The popup measures its own content and asks to fit it; keep it anchored bottom-right.
#[tauri::command]
fn popup_resize(app: AppHandle, w: f64, h: f64) {
    let Some(p) = app.get_webview_window("tray") else { return };
    let scale = p.scale_factor().unwrap_or(1.0);
    let (Ok(pos), Ok(size)) = (p.outer_position(), p.outer_size()) else { return };
    let (nw, nh) = ((w.clamp(200.0, 600.0) * scale) as i32, (h.clamp(120.0, 900.0) * scale) as i32);
    let bottom = pos.y + size.height as i32;
    let right = pos.x + size.width as i32;
    let _ = p.set_size(PhysicalSize::new(nw as u32, nh as u32));
    let _ = p.set_position(PhysicalPosition::new(right - nw, bottom - nh));
}

/// Opens https links (e.g. the GitHub profile) in the system browser, never in the app.
#[tauri::command]
fn open_url(url: String) {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c == '"') {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &url])
            .creation_flags(0x0800_0000).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
}

/// Saves a dashboard export to ~/Downloads and shows it in Explorer.
#[tauri::command]
fn save_export(name: String, content: String) -> Result<String, String> {
    let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
    if safe.is_empty() || safe.starts_with('.') {
        return Err("invalid file name".into());
    }
    let dir = paths::home().join("Downloads");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(safe);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    Ok(path.display().to_string())
}

/// Writes the Power BI star schema (whole history) to ~/Downloads/TokenScope-PowerBI.
#[tauri::command]
async fn export_powerbi(window: WebviewWindow) -> Result<Option<String>, String> {
    // Ask where to save (native folder dialog), starting from the last folder used.
    let last_file = paths::tokenscope_dir().join("powerbi.json");
    let last = std::fs::read_to_string(&last_file).ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["folder"].as_str().map(std::path::PathBuf::from))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| paths::home().join("Downloads"));
    let picked = tauri::async_runtime::spawn_blocking(move || {
        rfd::FileDialog::new()
            .set_title("Power BI export: choose a folder")
            .set_directory(&last)
            .set_parent(&window)
            .pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(dir) = picked else { return Ok(None) }; // cancelled
    let _ = std::fs::create_dir_all(paths::tokenscope_dir());
    let _ = std::fs::write(&last_file, json!({ "folder": dir }).to_string());
    let dir = tauri::async_runtime::spawn_blocking(move || powerbi::export(&dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    Ok(Some(dir.display().to_string()))
}

/// Toggle the quota popup just above the tray icon that was clicked.
fn toggle_popup(app: &AppHandle, anchor: PhysicalPosition<f64>) {
    let Some(p) = app.get_webview_window("tray") else { return };
    let just_hidden = POPUP_HIDDEN_AT.lock().unwrap().is_some_and(|t| t.elapsed().as_millis() < 400);
    if p.is_visible().unwrap_or(false) || just_hidden {
        let _ = p.hide();
        return;
    }
    let size = p.outer_size().unwrap_or(PhysicalSize::new(POPUP_W as u32, POPUP_H as u32));
    let mut x = anchor.x as i32 - size.width as i32 / 2;
    let mut y = anchor.y as i32 - size.height as i32 - 12;
    if let Ok(Some(m)) = p.current_monitor().or_else(|_| p.primary_monitor()) {
        let (mp, ms) = (m.position(), m.size());
        x = x.clamp(mp.x + 8, mp.x + ms.width as i32 - size.width as i32 - 8);
        if y < mp.y {
            y = anchor.y as i32 + 12; // taskbar at the top
        }
    }
    let _ = p.set_position(PhysicalPosition::new(x, y));
    let _ = p.show();
    let _ = p.set_focus();
    let _ = p.eval("typeof refreshTray==='function'&&refreshTray()");
}

fn main() {
    let minimized = std::env::args().any(|a| a == "--minimized");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            summary, utilization, status, theme, startup_enabled, toggle_startup, startup_label,
            show_dashboard, quit_app, dismiss, popup_resize, open_url, save_export, export_powerbi
        ])
        .setup(move |app| {
            let h = app.handle().clone();
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("TokenScope")
                // No native title bar: the page draws its own (drag area + 3 buttons), so no
                // caption icon shows while the taskbar keeps the app icon.
                .decorations(false)
                .shadow(true)
                .inner_size(1320.0, 900.0)
                .min_inner_size(900.0, 600.0)
                .background_color(tauri::window::Color(0, 0, 0, 255))
                .visible(!minimized)
                .build()?;
            WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("tray.html".into()))
                .title("TokenScope")
                .inner_size(POPUP_W, POPUP_H)
                .decorations(false)
                .transparent(true)
                .resizable(false)
                .skip_taskbar(true)
                .always_on_top(true)
                .visible(false)
                .build()?;

            let open = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("bundled icon"))
                .tooltip("TokenScope")
                .menu(&Menu::with_items(app, &[&open, &quit])?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id().as_ref() {
                    "open" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(move |_tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } = e {
                        toggle_popup(&h, position);
                    }
                })
                .build(app)?;
            alerts::spawn(app.handle().clone());
            // Pre-parse the whole history (incl. ~900 compressed Codex rollouts) once, after the
            // first paint, so switching to 90d / lifetime is instant (same as the Python app).
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(4));
                events::read_local_summary(100_000, None, None);
            });
            Ok(())
        })
        .on_window_event(|w, e| match e {
            // Closing the dashboard keeps the app in the tray (Quit is in the tray menu/popup).
            WindowEvent::CloseRequested { api, .. } if w.label() == "main" => {
                api.prevent_close();
                let _ = w.hide();
            }
            WindowEvent::Focused(false) if w.label() == "tray" => {
                *POPUP_HIDDEN_AT.lock().unwrap() = Some(std::time::Instant::now());
                let _ = w.hide();
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running TokenScope");
}
