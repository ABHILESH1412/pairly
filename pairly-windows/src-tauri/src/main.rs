//! Pairly for Windows: the tray app. The window shows paired devices and what you can do with
//! them; closing it keeps Pairly running in the tray (Quit from the tray stops it).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod platform;
mod settings;
mod update;

use std::sync::Arc;

use backend::Backend;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, DragDropEvent, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

type Backends<'a> = State<'a, Arc<Backend>>;
type Answer<T = ()> = Result<T, String>;

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Bring the window up (it starts hidden when launched at login).
pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn refresh(backend: Backends<'_>) {
    backend.emit();
}

#[tauri::command]
async fn pair(backend: Backends<'_>, id: String) -> Answer {
    backend.pair(&id).await.map_err(text)
}

#[tauri::command]
fn confirm_pair(backend: Backends<'_>, id: String, accept: bool) -> Answer {
    backend.confirm_pair(&id, accept).map_err(text)
}

#[tauri::command]
fn start_qr(backend: Backends<'_>) -> Answer {
    backend.start_qr().map_err(text)
}

#[tauri::command]
fn cancel_qr(backend: Backends<'_>) {
    backend.cancel_qr();
}

#[tauri::command]
fn unpair(backend: Backends<'_>, id: String) -> Answer {
    backend.unpair(&id).map_err(text)
}

#[tauri::command]
async fn set_paused(backend: Backends<'_>, id: String, paused: bool) -> Answer {
    backend.set_paused(&id, paused).await.map_err(text)
}

#[tauri::command]
fn ping(backend: Backends<'_>, id: String) -> Answer {
    backend.ping(&id).map_err(text)
}

#[tauri::command]
fn ring(backend: Backends<'_>, id: String, on: bool) -> Answer {
    backend.ring(&id, on).map_err(text)
}

#[tauri::command]
fn send_clipboard(backend: Backends<'_>, id: String) -> Answer {
    backend.send_clipboard(&id).map_err(text)
}

#[tauri::command]
fn send_text(backend: Backends<'_>, id: String, text: String) -> Answer {
    backend.send_text(&id, &text).map_err(self::text)
}

#[tauri::command]
fn send_files(backend: Backends<'_>, id: String, paths: Vec<String>) -> Answer<usize> {
    backend.send_files(&id, &paths).map_err(text)
}

#[tauri::command]
fn cancel_transfer(backend: Backends<'_>, id: u64) -> Answer {
    backend.cancel_transfer(id).map_err(text)
}

#[tauri::command]
async fn phone_power(backend: Backends<'_>, id: String, action: String) -> Answer {
    backend.phone_power(&id, &action).await.map_err(text)
}

#[tauri::command]
fn open_downloads(backend: Backends<'_>) {
    backend.open_downloads();
}

#[tauri::command]
async fn set_enabled(backend: Backends<'_>, on: bool) -> Answer {
    backend.inner().set_enabled(on).await.map_err(text)
}

#[tauri::command]
async fn rename(backend: Backends<'_>, name: String) -> Answer {
    backend.inner().rename(&name).await.map_err(text)
}

#[tauri::command]
fn set_theme(backend: Backends<'_>, theme: String) {
    backend.change_settings(|s| s.theme = theme);
    backend.emit();
}

#[tauri::command]
fn set_option(backend: Backends<'_>, key: String, on: bool) {
    backend.change_settings(|s| match key.as_str() {
        "auto_update" => s.auto_update = on,
        "clipboard_auto" => s.clipboard_auto = on,
        "sidebar" => s.sidebar = on,
        _ => {}
    });
    backend.emit();
}

#[tauri::command]
fn autostart(app: AppHandle, on: Option<bool>) -> Answer<bool> {
    let launcher = app.autolaunch();
    match on {
        Some(true) => launcher.enable().map_err(text)?,
        Some(false) => launcher.disable().map_err(text)?,
        None => {}
    }
    launcher.is_enabled().map_err(text)
}

#[tauri::command]
async fn check_update(app: AppHandle, backend: Backends<'_>, install: bool) -> Answer {
    update::check(&app, backend.inner(), install).await;
    Ok(())
}

fn tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Pairly", true, None::<&str>)?;
    let pair = MenuItem::with_id(app, "pair", "Pair a New Device", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Pairly", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &pair, &PredefinedMenuItem::separator(app)?, &quit],
    )?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id("pairly")
        .icon(icon)
        .tooltip("Pairly")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "pair" => {
                show_main_window(app);
                let _ = app.emit("show-pairing", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PAIRLY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let background = std::env::args().any(|a| a == "--background");
    tauri::Builder::default()
        // A second launch (Start menu, notification click) shows the running one.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app)
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .setup(move |app| {
            let data = app.path().app_data_dir()?;
            let downloads = app.path().download_dir()?.join("Pairly");
            let backend = Backend::new(app.handle().clone(), data.clone(), downloads);
            app.manage(backend.clone());
            tray(app)?;
            // Start with Windows by default, once (the user can turn it off in Settings).
            let marker = data.join(".autostart-set");
            if !marker.exists() {
                let _ = app.autolaunch().enable();
                let _ = std::fs::create_dir_all(&data);
                let _ = std::fs::write(&marker, b"");
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = backend.start().await {
                    tracing::error!(error = %e, "Pairly didn't start");
                    backend.notify("Pairly didn't start", &format!("{e:#}"));
                }
                update::run(handle, backend).await;
            });
            if !background {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Closing the window keeps Pairly in the tray.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // Files dropped on the window go to the device on screen (the page decides).
            WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                let paths: Vec<String> = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let _ = window.emit("files-dropped", paths);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            refresh,
            pair,
            confirm_pair,
            start_qr,
            cancel_qr,
            unpair,
            set_paused,
            ping,
            ring,
            send_clipboard,
            send_text,
            send_files,
            cancel_transfer,
            phone_power,
            open_downloads,
            set_enabled,
            rename,
            set_theme,
            set_option,
            autostart,
            check_update,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "Pairly failed");
            std::process::exit(1);
        });
}
