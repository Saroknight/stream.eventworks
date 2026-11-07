//! Streamroom Live: a small window around the streaming engine.
//!
//! - Signs in once: the site's "Open Streamroom Live" button launches a streamroom:// link with
//!   a single-use code, which the app trades for its own key (saved encrypted for this Windows
//!   user). One running copy; a second launch hands its link to the first and exits.
//! - Lists the rooms, starts and ends rooms, and goes live, without the site.
//! - Shows LIVE status and stats, Stop, and settings (quality, monitor, audio, cursor).
//! - Closing the window while live keeps streaming from the tray.
//! - Updates itself from GitHub Releases.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod update;

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use streamroom_core::api::parse_launch;
use streamroom_core::{log, now_ms, Account, Api, AppLink, Launch, LiveInfo, Monitor, Session, Settings, SignedOut, Source};
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

/// Where we're streaming to.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// A room on the signed-in server.
    Room(String),
    /// An older app link with its own 12-hour ticket.
    Legacy(AppLink),
}

struct AppState {
    session: tokio::sync::Mutex<Option<Session>>,
    target: Mutex<Option<Target>>,
    account: Mutex<Option<Account>>,
    status: Mutex<Status>,
    settings: Mutex<Settings>,
    update: Mutex<Option<update::Available>>,
    tray_stop: Mutex<Option<MenuItem<Wry>>>,
}

/// What the window may know about the sign-in (never the key).
#[derive(Clone, Serialize)]
struct AccountView {
    name: String,
    server: String,
    host: String,
}

fn account_view(a: &Option<Account>) -> Option<AccountView> {
    a.as_ref().map(|a| AccountView {
        name: a.user_name.clone(),
        server: a.server.clone(),
        host: a.server.split("://").nth(1).unwrap_or(&a.server).to_string(),
    })
}

#[derive(Serialize)]
struct Snapshot {
    status: Status,
    account: Option<AccountView>,
    room: Option<String>,
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

// ---------- sign-in ----------

fn config_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok()
}

fn current_account(app: &AppHandle) -> Option<Account> {
    app.state::<AppState>().account.lock().unwrap().clone()
}

fn set_account(app: &AppHandle, a: Option<Account>) {
    if let Some(dir) = config_dir(app) {
        match &a {
            Some(a) => {
                if let Err(e) = account::save(&dir, a) {
                    log::info(&format!("couldn't save the sign-in: {e:#}"));
                }
            }
            None => account::forget(&dir),
        }
    }
    let view = account_view(&a);
    *app.state::<AppState>().account.lock().unwrap() = a;
    let _ = app.emit("account", view);
}

/// The API for the signed-in server, or a friendly error.
fn api(app: &AppHandle) -> Result<Api, String> {
    let a = current_account(app).ok_or("Not signed in yet. In a room on the site, click Go Live, then Open Streamroom Live.")?;
    Api::for_account(&a).map_err(|e| format!("{e:#}"))
}

/// Turns an error into text for the window; a revoked key also signs this PC out.
fn explain(app: &AppHandle, e: anyhow::Error) -> String {
    if e.downcast_ref::<SignedOut>().is_some() {
        log::info("the server no longer accepts this PC's key: signing out");
        set_account(app, None);
    }
    format!("{e:#}")
}

// ---------- sessions ----------

async fn begin(app: AppHandle, target: Target) {
    let state = app.state::<AppState>();
    let mut slot = state.session.lock().await;
    // Already live in that room: nothing to do but show the window.
    if slot.is_some() && state.target.lock().unwrap().as_ref() == Some(&target) {
        return;
    }
    if let Some(s) = slot.take() {
        s.stop().await;
    }
    *state.target.lock().unwrap() = Some(target.clone());
    let settings = state.settings.lock().unwrap().clone();
    let started = match &target {
        Target::Legacy(link) => {
            set_status(&app, Status::Connecting { server: link.base.clone() });
            Session::start(link, &settings, Source::Screen).await
        }
        Target::Room(id) => match api(&app) {
            Err(message) => {
                set_status(&app, Status::Error { message });
                return;
            }
            Ok(api) => {
                set_status(&app, Status::Connecting { server: api.server.clone() });
                log::info(&format!("going live in room {id} on {}", api.server));
                match api.app_token(id).await {
                    Ok(joined) => Session::start_joined(&api.server, joined, &settings, Source::Screen).await,
                    Err(e) => Err(e),
                }
            }
        },
    };
    match started {
        Ok((session, info)) => {
            *slot = Some(session);
            set_status(&app, Status::Live { info, since: now_ms() });
        }
        Err(e) => {
            log::info(&format!("could not go live: {e:#}"));
            let message = explain(&app, e);
            if let (Target::Room(id), Ok(api)) = (&target, api(&app)) {
                api.report("app-error", &format!("could not go live: {message}"), Some(id)).await;
            }
            set_status(&app, Status::Error { message });
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

/// A link from the site (or pasted): sign in with its code if it has one, then go live in its
/// room if it names one.
async fn launch(app: AppHandle, l: Launch) -> Result<(), String> {
    show_main(&app);
    let (server, room, pair) = match l {
        Launch::Legacy(link) => {
            begin(app, Target::Legacy(link)).await;
            return Ok(());
        }
        Launch::Open { server, room, pair } => (server, room, pair),
    };
    let current = current_account(&app);
    let signed_in_here = current.as_ref().map(|a| a.server == server).unwrap_or(false);
    if let Some(code) = pair {
        let api = Api::new(&server, None).map_err(|e| format!("{e:#}"))?;
        match api.pair(&code, current.as_ref(), &account::device_name()).await {
            Ok(a) => {
                if current.as_ref() != Some(&a) {
                    log::info(&format!("signed in to {} as {}", a.server, a.user_name));
                }
                set_account(&app, Some(a));
            }
            // An old or reused code is fine if we're already signed in to that server.
            Err(e) if signed_in_here => log::info(&format!("sign-in code not used ({e:#}); keeping the current sign-in")),
            Err(e) => {
                let message = format!("{e:#}");
                log::info(&format!("sign-in with the link failed: {message}"));
                set_status(&app, Status::Error { message: message.clone() });
                return Err(message);
            }
        }
    } else if !signed_in_here {
        let message = format!("This PC isn't signed in to {server} yet. In a room there, click Go Live, then Open Streamroom Live.");
        set_status(&app, Status::Error { message: message.clone() });
        return Err(message);
    }
    if let Some(id) = room {
        begin(app, Target::Room(id)).await;
    }
    Ok(())
}

/// Launch arguments and deep links: act on the first Streamroom link among them.
fn handle_links(app: &AppHandle, args: impl IntoIterator<Item = String>) {
    for a in args {
        if !(a.starts_with("streamroom://") || a.contains("/live/")) {
            continue;
        }
        log::info(&format!("received link: {}", redact(&a)));
        match parse_launch(&a) {
            Ok(l) => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = launch(app, l).await;
                });
            }
            Err(e) => {
                log::info(&format!("couldn't read that link: {e:#}"));
                show_main(app);
                set_status(app, Status::Error { message: format!("{e:#}") });
            }
        }
        return;
    }
}

/// Links in the log without their secrets (sign-in codes, tickets).
fn redact(link: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    let mut i = 0;
    let b = link.as_bytes();
    while i < b.len() {
        let rest = &link[i..];
        if rest.starts_with("pair=") || rest.starts_with("k=") || rest.starts_with("k%3D") {
            let key = &rest[..rest.find(['=', 'D']).map(|p| p + 1).unwrap_or(0)];
            out.push_str(key);
            out.push('…');
            i += key.len();
            skip = true;
            continue;
        }
        let c = rest.chars().next().unwrap();
        if skip && (c == '&' || c == '#') {
            skip = false;
        }
        if !skip {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

// ---------- settings ----------

fn settings_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    let Some(text) = settings_path(app).and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Settings::default(); // new install: everything except Discord
    };
    let mut s: Settings = serde_json::from_str(&text).unwrap_or_default();
    // Installs from before per-app audio keep sharing all PC audio until changed in Settings.
    if !text.contains("\"audio_mode\"") {
        s.audio_mode = "all".into();
    }
    s
}

// ---------- commands (called from the window) ----------

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> Snapshot {
    Snapshot {
        status: state.status.lock().unwrap().clone(),
        account: account_view(&state.account.lock().unwrap()),
        room: match state.target.lock().unwrap().as_ref() {
            Some(Target::Room(id)) => Some(id.clone()),
            _ => None,
        },
        settings: state.settings.lock().unwrap().clone(),
        monitors: streamroom_core::list_monitors(),
        version: env!("CARGO_PKG_VERSION"),
        update: state.update.lock().unwrap().clone(),
        log_path: log::path().display().to_string(),
    }
}

/// Apps playing sound right now, for "Only these apps".
#[tauri::command]
async fn audio_apps() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(streamroom_core::audio_apps).await.unwrap_or_default()
}

/// A pasted link.
#[tauri::command]
async fn start_link(app: AppHandle, link: String) -> Result<(), String> {
    let l = parse_launch(&link).map_err(|e| format!("{e:#}"))?;
    launch(app, l).await
}

/// The lobby, as the site shows it.
#[tauri::command]
async fn list_rooms(app: AppHandle) -> Result<serde_json::Value, String> {
    let api = api(&app)?;
    api.rooms().await.map_err(|e| explain(&app, e))
}

#[tauri::command]
async fn go_live(app: AppHandle, room: String) -> Result<(), String> {
    api(&app)?;
    begin(app, Target::Room(room)).await;
    Ok(())
}

/// Start a room and go live in it.
#[tauri::command]
async fn create_room(app: AppHandle, title: String) -> Result<(), String> {
    let api = api(&app)?;
    let id = api.create_room(title.trim()).await.map_err(|e| explain(&app, e))?;
    log::info(&format!("started room {id}"));
    begin(app, Target::Room(id)).await;
    Ok(())
}

/// End a room for everyone (yours, or any room if you're the admin).
#[tauri::command]
async fn end_room(app: AppHandle, room: String) -> Result<(), String> {
    let api = api(&app)?;
    let streaming_there = matches!(app.state::<AppState>().target.lock().unwrap().as_ref(), Some(Target::Room(r)) if *r == room);
    api.end_room(&room).await.map_err(|e| explain(&app, e))?;
    if streaming_there {
        end(&app, Some("You ended the room.".into())).await;
    }
    Ok(())
}

/// Watch a room in the browser.
#[tauri::command]
fn open_room(app: AppHandle, room: String) -> Result<(), String> {
    let a = current_account(&app).ok_or("Not signed in.")?;
    if !room.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("Bad room.".into());
    }
    open_url(&format!("{}/r/{room}", a.server));
    Ok(())
}

#[tauri::command]
fn open_site(app: AppHandle) {
    if let Some(a) = current_account(&app) {
        open_url(&a.server);
    }
}

#[tauri::command]
async fn sign_out(app: AppHandle) -> Result<(), String> {
    if is_busy(&app) {
        end(&app, None).await;
    }
    if let Ok(api) = api(&app) {
        let _ = api.signout().await; // best effort: the key is forgotten here either way
    }
    log::info("signed out");
    set_account(&app, None);
    Ok(())
}

fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[tauri::command]
async fn stop(app: AppHandle) {
    end(&app, None).await;
}

/// Go live again in the same place (e.g. to apply new settings while live).
#[tauri::command]
async fn restart(app: AppHandle) -> Result<(), String> {
    let target = app.state::<AppState>().target.lock().unwrap().clone().ok_or("Not streaming.")?;
    end(&app, None).await;
    begin(app, target).await;
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
            target: Mutex::new(None),
            account: Mutex::new(None),
            status: Mutex::new(Status::Idle),
            settings: Mutex::new(Settings::default()),
            update: Mutex::new(None),
            tray_stop: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_state, start_link, stop, restart, dismiss, save_settings, open_logs, check_update, install_update,
            list_rooms, go_live, create_room, end_room, open_room, open_site, sign_out, audio_apps
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            if let Ok(dir) = app.path().app_log_dir() {
                log::set_path(dir.join("streamroom-live.log"));
            }
            log::info(&format!("---- Streamroom Live {} starting", env!("CARGO_PKG_VERSION")));
            *app.state::<AppState>().settings.lock().unwrap() = load_settings(&handle);
            if let Some(dir) = config_dir(&handle) {
                let saved = account::load(&dir);
                if let Some(a) = &saved {
                    log::info(&format!("signed in to {} as {}", a.server, a.user_name));
                }
                *app.state::<AppState>().account.lock().unwrap() = saved;
            }

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
                        if reason.starts_with("Disconnected") {
                            let room = match h.state::<AppState>().target.lock().unwrap().as_ref() {
                                Some(Target::Room(id)) => Some(id.clone()),
                                _ => None,
                            };
                            if let Ok(api) = api(&h) {
                                api.report("dropped", &reason, room.as_deref()).await;
                            }
                        }
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
