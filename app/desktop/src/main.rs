//! Streamroom Live: a small window around the streaming engine.
//!
//! - Opened from the site's "Open the app" button via streamroom:// links (one running copy;
//!   a second launch hands its link to the first and exits).
//! - Shows LIVE status and stats, Stop, and settings (quality, monitor, audio, cursor).
//! - Closing the window while live keeps streaming from the tray.
//! - Updates itself from GitHub Releases.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod update;

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use streamroom_core::{link, log, now_ms, AppLink, LiveInfo, Monitor, Session, Settings, Source};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};
use tauri_plugin_deep_link::DeepLinkExt;

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Status {
    Idle,
    Connecting { server: String },
    Live { info: LiveInfo, since: u64 },
    Ended { message: String },
    Error { message: String },
}

struct AppState {
    session: tokio::sync::Mutex<Option<Session>>,
    link: Mutex<Option<AppLink>>,
    status: Mutex<Status>,
    settings: Mutex<Settings>,
    update: Mutex<Option<update::Available>>,
    tray_stop: Mutex<Option<MenuItem<Wry>>>,
}

#[derive(Serialize)]
struct Snapshot {
    status: Status,
    settings: Settings,
    monitors: Vec<Monitor>,
    version: &'static str,
    update: Option<update::Available>,
    log_path: String,
}

// ---------- status ----------

fn is_busy(app: &AppHandle) -> bool {
    matches!(*app.state::<AppState>().status.lock().unwrap(), Status::Live { .. } | Status::Connecting { .. })
}

fn set_status(app: &AppHandle, status: Status) {
    let state = app.state::<AppState>();
    let live = matches!(status, Status::Live { .. });
    let tip = match &status {
        Status::Live { info, .. } => format!("Streamroom Live: live in {}", info.room_title),
        Status::Connecting { .. } => "Streamroom Live: connecting…".into(),
        _ => "Streamroom Live".into(),
    };
    *state.status.lock().unwrap() = status.clone();
    if let Some(item) = state.tray_stop.lock().unwrap().as_ref() {
        let _ = item.set_enabled(live);
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(tip));
    }
    let _ = app.emit("status", status);
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

// ---------- sessions ----------

async fn begin(app: AppHandle, link: AppLink) {
    let state = app.state::<AppState>();
    let mut slot = state.session.lock().await;
    // Same room and already live: nothing to do but show the window.
    let same = state.link.lock().unwrap().as_ref().map(|l| l.base == link.base && l.room_id == link.room_id).unwrap_or(false);
    if same && slot.is_some() {
        return;
    }
    if let Some(s) = slot.take() {
        s.stop().await;
    }
    *state.link.lock().unwrap() = Some(link.clone());
    set_status(&app, Status::Connecting { server: link.base.clone() });
    let settings = state.settings.lock().unwrap().clone();
    match Session::start(&link, &settings, Source::Screen).await {
        Ok((session, info)) => {
            *slot = Some(session);
            set_status(&app, Status::Live { info, since: now_ms() });
        }
        Err(e) => {
            log::info(&format!("could not go live: {e:#}"));
            set_status(&app, Status::Error { message: format!("{e:#}") });
        }
    }
}

async fn end(app: &AppHandle, reason: Option<String>) {
    let state = app.state::<AppState>();
    let mut slot = state.session.lock().await;
    if let Some(s) = slot.take() {
        s.stop().await;
    }
    set_status(app, match reason {
        Some(message) => Status::Ended { message },
        None => Status::Idle,
    });
}

/// Starts streaming from launch arguments / deep links, if one of them is an app link.
fn handle_links(app: &AppHandle, args: impl IntoIterator<Item = String>) {
    for a in args {
        if !(a.starts_with("streamroom://") || a.contains("/live/")) {
            continue;
        }
        match link::parse(&a) {
            Ok(l) => {
                show_main(app);
                let app = app.clone();
                tauri::async_runtime::spawn(async move { begin(app, l).await });
                return;
            }
            Err(e) => {
                show_main(app);
                set_status(app, Status::Error { message: format!("{e:#}") });
            }
        }
    }
}

// ---------- settings ----------

fn settings_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ---------- commands (called from the window) ----------

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> Snapshot {
    Snapshot {
        status: state.status.lock().unwrap().clone(),
        settings: state.settings.lock().unwrap().clone(),
        monitors: streamroom_core::list_monitors(),
        version: env!("CARGO_PKG_VERSION"),
        update: state.update.lock().unwrap().clone(),
        log_path: log::path().display().to_string(),
    }
}

#[tauri::command]
async fn start_link(app: AppHandle, link: String) -> Result<(), String> {
    let link = link::parse(&link).map_err(|e| format!("{e:#}"))?;
    begin(app, link).await;
    Ok(())
}

#[tauri::command]
async fn stop(app: AppHandle) {
    end(&app, None).await;
}

/// Restart with the same link (e.g. to apply new settings while live).
#[tauri::command]
async fn restart(app: AppHandle) -> Result<(), String> {
    let link = app.state::<AppState>().link.lock().unwrap().clone().ok_or("Not streaming.")?;
    end(&app, None).await;
    begin(app, link).await;
    Ok(())
}

#[tauri::command]
fn dismiss(app: AppHandle) {
    if !is_busy(&app) {
        set_status(&app, Status::Idle);
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    *app.state::<AppState>().settings.lock().unwrap() = settings.clone();
    let path = settings_path(&app).ok_or("No settings folder.")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&settings).unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_logs() {
    let path = log::path();
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(&path)).spawn();
    }
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Option<update::Available> {
    let found = match update::check().await {
        Ok(a) => a,
        Err(e) => {
            log::info(&format!("update check failed: {e:#}"));
            None
        }
    };
    *app.state::<AppState>().update.lock().unwrap() = found.clone();
    let _ = app.emit("update", found.clone());
    found
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    if is_busy(&app) {
        return Err("Stop streaming first, then update.".into());
    }
    let a = app.state::<AppState>().update.lock().unwrap().clone().ok_or("No update available.")?;
    log::info(&format!("installing update {}", a.version));
    update::install(&a).await.map_err(|e| format!("{e:#}"))?;
    app.exit(0);
    Ok(())
}

// ---------- app ----------

fn main() {
    streamroom_core::init();
    tauri::Builder::default()
        // Must be first: a second launch (e.g. clicking "Open the app" again) passes its link
        // to this copy, which the deep-link plugin turns into an open-url event.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            show_main(app);
            // Plain https app links on the command line (streamroom:// ones arrive via deep link).
            handle_links(app, argv.into_iter().skip(1).filter(|a| !a.starts_with("streamroom://")));
        }))
        .plugin(tauri_plugin_deep_link::init())
        .manage(AppState {
            session: tokio::sync::Mutex::new(None),
            link: Mutex::new(None),
            status: Mutex::new(Status::Idle),
            settings: Mutex::new(Settings::default()),
            update: Mutex::new(None),
            tray_stop: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_state, start_link, stop, restart, dismiss, save_settings, open_logs, check_update, install_update
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            if let Ok(dir) = app.path().app_log_dir() {
                log::set_path(dir.join("streamroom-live.log"));
            }
            log::info(&format!("---- Streamroom Live {} starting", env!("CARGO_PKG_VERSION")));
            *app.state::<AppState>().settings.lock().unwrap() = load_settings(&handle);

            // streamroom:// links: make sure they point at this copy (the installer registers
            // them too; this also covers a moved install).
            #[cfg(windows)]
            {
                let _ = app.deep_link().register_all();
            }
            let h = handle.clone();
            app.deep_link().on_open_url(move |event| {
                handle_links(&h, event.urls().into_iter().map(|u| u.to_string()));
            });
            match app.deep_link().get_current() {
                Ok(Some(urls)) => handle_links(&handle, urls.into_iter().map(|u| u.to_string())),
                _ => handle_links(&handle, std::env::args().skip(1)),
            }

            // Tray: keeps streaming visible (and stoppable) with the window closed.
            let show = MenuItem::with_id(app, "show", "Show Streamroom Live", true, None::<&str>)?;
            let stop_item = MenuItem::with_id(app, "stop", "Stop streaming", false, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &stop_item, &quit])?;
            *app.state::<AppState>().tray_stop.lock().unwrap() = Some(stop_item);
            let mut tray = TrayIconBuilder::with_id("main").tooltip("Streamroom Live").menu(&menu).show_menu_on_left_click(false);
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.on_menu_event(|app, event| match event.id().0.as_str() {
                "show" => show_main(app),
                "stop" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move { end(&app, None).await });
                }
                "quit" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        end(&app, None).await;
                        app.exit(0);
                    });
                }
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                    show_main(tray.app_handle());
                }
            })
            .build(app)?;

            // Once a second while live: stats to the window, and notice if the session ended
            // on its own (room ended, removed, capture failed).
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let ended = {
                        let state = h.state::<AppState>();
                        let Ok(slot) = state.session.try_lock() else { continue };
                        match slot.as_ref() {
                            Some(s) => match s.ended() {
                                Some(reason) => Some(reason),
                                None => {
                                    let _ = h.emit("stats", s.stats());
                                    None
                                }
                            },
                            None => None,
                        }
                    };
                    if let Some(reason) = ended {
                        log::info(&format!("session ended: {reason}"));
                        end(&h, Some(reason)).await;
                        show_main(&h);
                    }
                }
            });

            // Look for updates a few seconds after start, then every 6 hours.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                loop {
                    let _ = check_update(h.clone()).await;
                    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window while live keeps streaming in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if is_busy(window.app_handle()) {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Streamroom Live");
}
