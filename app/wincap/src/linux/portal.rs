//! Wayland screen capture goes through the desktop's screen-share portal: the desktop shows its
//! own "choose a screen or window" dialog, then hands us a PipeWire stream of it. We ask the
//! portal to remember the choice (a restore token), so the dialog appears once, not every time.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;

use crate::Control;

/// A running screen-share session. The portal session stays open (on its own thread) until
/// `control.stop` is set; `fd` is the PipeWire connection GStreamer reads from.
pub struct PortalStream {
    pub fd: OwnedFd,
    pub node: u32,
    pub size: Option<(i32, i32)>,
    pub _thread: std::thread::JoinHandle<()>,
}

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("streamroom-live").join("screencast-token"))
}

/// Opens a screen-share session. `remember`: reuse the last choice instead of asking. The new
/// choice is always saved, so "Ask every time" then back to "remembered" keeps the latest pick.
pub fn open(control: Arc<Control>, cursor: bool, remember: bool) -> Result<PortalStream> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32, Option<(i32, i32)>)>>();
    let thread = std::thread::Builder::new().name("screen-portal".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let proxy = match Screencast::new().await {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("the desktop's screen sharing isn't available ({e}). Is xdg-desktop-portal running?")));
                    return;
                }
            };
            let session = match proxy.create_session(Default::default()).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("couldn't start screen sharing: {e}")));
                    return;
                }
            };
            let saved = if remember { token_path().and_then(|p| std::fs::read_to_string(p).ok()) } else { None };
            let mut opts = SelectSourcesOptions::default()
                .set_cursor_mode(if cursor { CursorMode::EMBEDDED } else { CursorMode::HIDDEN })
                .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked);
            if let Some(t) = saved.as_deref() {
                opts = opts.set_restore_token(t.trim());
            }
            let started = async {
                proxy.select_sources(&session, opts).await?.response()?;
                proxy.start(&session, None, Default::default()).await?.response()
            }
            .await;
            let streams = match started {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("screen sharing was cancelled or refused ({e})")));
                    return;
                }
            };
            if let (Some(t), Some(p)) = (streams.restore_token(), token_path()) {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, t);
            }
            let Some(stream) = streams.streams().first() else {
                let _ = tx.send(Err(anyhow!("nothing was chosen to share")));
                return;
            };
            let (node, size) = (stream.pipe_wire_node_id(), stream.size());
            match proxy.open_pipe_wire_remote(&session, Default::default()).await {
                Ok(fd) => {
                    let _ = tx.send(Ok((fd, node, size)));
                }
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("couldn't connect to the screen stream: {e}")));
                    return;
                }
            }
            // Keep the session open while streaming.
            while !control.stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let _ = session.close().await;
        });
    })?;
    // The user may take a while in the dialog.
    let (fd, node, size) = rx
        .recv_timeout(Duration::from_secs(180))
        .map_err(|_| anyhow!("no screen was chosen in time"))?
        .context("screen sharing")?;
    Ok(PortalStream { fd, node, size, _thread: thread })
}
