//! Wayland screen capture goes through the desktop's screen-share portal: the desktop shows its
//! own "choose a screen or window" dialog, then hands us a PipeWire stream of it. We ask the
//! portal to remember the choice (a restore token), so the dialog appears once, not every time.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::Control;

/// A running screen-share session. The portal session stays open (on its own thread) until
/// `control.stop` is set or this is dropped; `fd` is the PipeWire connection GStreamer reads from.
pub struct PortalStream {
    pub fd: OwnedFd,
    pub node: u32,
    /// The stream's PipeWire serial, when the desktop reports it (newer PipeWire finds streams by it).
    pub serial: Option<u64>,
    pub size: Option<(i32, i32)>,
    /// A remembered choice was used (no dialog).
    pub restored: bool,
    stop: Arc<AtomicBool>,
}

impl Drop for PortalStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What the portal handed over for the chosen screen or window.
struct Shared {
    fd: OwnedFd,
    node: u32,
    serial: Option<u64>,
    size: Option<(i32, i32)>,
}

/// How the session should be set up.
#[derive(Clone, Copy)]
struct Request {
    cursor: bool,
    /// Reuse the remembered choice instead of asking.
    remember: bool,
}

/// Forget the remembered screen, so the next session asks again.
pub fn forget() -> Result<()> {
    let Some(p) = token_path() else { return Ok(()) };
    match std::fs::remove_file(&p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", p.display())),
        _ => Ok(()),
    }
}

fn token_path() -> Option<PathBuf> {
    let base =
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("streamroom-live").join("screencast-token"))
}

fn save_token(token: &str) -> Result<()> {
    let p = token_path().context("no home folder to save it in")?;
    write_private(&p, token)
}

/// Writes `text` to a file only this user can read (also tightening one that already exists).
fn write_private(p: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)
        .with_context(|| format!("creating {}", p.display()))?;
    // `mode` only applies to a new file: restrict an existing one before anything is written.
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).with_context(|| format!("restricting who can read {}", p.display()))?;
    file.write_all(text.as_bytes()).with_context(|| format!("writing {}", p.display()))
}

/// Opens a screen-share session. `remember`: reuse the last choice instead of asking. The new
/// choice is always saved, so "Ask every time" then back to "remembered" keeps the latest pick.
pub fn open(control: Arc<Control>, cursor: bool, remember: bool) -> Result<PortalStream> {
    let restored = remember && token_path().is_some_and(|p| p.exists());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<Shared>>();
    let request = Request { cursor, remember };
    portal_runtime()?.spawn(async move { serve(&control, &stop_t, request, &tx).await });
    // The user may take a while in the dialog.
    let shared = rx.recv_timeout(Duration::from_secs(180)).map_err(|_| anyhow!("no screen was chosen in time"))?.context("screen sharing")?;
    let Shared { fd, node, serial, size } = shared;
    Ok(PortalStream { fd, node, serial, size, restored, stop })
}

/// The runtime all portal work runs on, started once and kept for the life of the app. ashpd
/// keeps one D-Bus connection for the whole process, and zbus drives that connection from the
/// tokio runtime it was first used on. With a runtime per session (before 0.4.15), the second
/// go-live in the same run waited forever on a dead connection: no dialog, then "no screen was
/// chosen in time".
fn portal_runtime() -> Result<tokio::runtime::Handle> {
    static RUNTIME: Mutex<Option<tokio::runtime::Handle>> = Mutex::new(None);
    let mut slot = RUNTIME.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(h) = slot.as_ref() {
        return Ok(h.clone());
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().context("starting the screen-share thread")?;
    let handle = rt.handle().clone();
    // A current-thread runtime only runs its tasks while something blocks on it: this thread does,
    // for good.
    std::thread::Builder::new()
        .name("screen-portal".into())
        .spawn(move || rt.block_on(std::future::pending::<()>()))
        .context("starting the screen-share thread")?;
    *slot = Some(handle.clone());
    Ok(handle)
}

/// Hands the result to `open`. Returns whether `open` was still waiting for it.
fn deliver(control: &Control, tx: &Sender<Result<Shared>>, result: Result<Shared>) -> bool {
    let shared = result.is_ok();
    let waiting = tx.send(result).is_ok();
    if shared && !waiting {
        // `open` already gave up and reported "no screen was chosen in time".
        control.report_error("a screen was chosen after the wait ran out; that share was closed".into());
    }
    waiting
}

/// Runs the portal session: set it up, hand the stream over, then keep it open while streaming.
async fn serve(control: &Control, stop: &AtomicBool, request: Request, tx: &Sender<Result<Shared>>) {
    let (proxy, session) = match connect().await {
        Ok(c) => c,
        Err(e) => {
            deliver(control, tx, Err(e));
            return;
        }
    };
    let result = share(&proxy, &session, request, control).await;
    let shared = result.is_ok();
    if deliver(control, tx, result) && shared {
        while !control.stop.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    if let Err(e) = session.close().await {
        control.report_error(format!("closing the screen share: {e}"));
    }
}

async fn connect() -> Result<(Screencast, Session<Screencast>)> {
    let proxy = Screencast::new().await.map_err(|e| anyhow!("the desktop's screen sharing isn't available ({e}). Is xdg-desktop-portal running?"))?;
    let session = proxy.create_session(Default::default()).await.map_err(|e| anyhow!("couldn't start screen sharing: {e}"))?;
    Ok((proxy, session))
}

/// Asks for (or restores) the screen to share and opens its PipeWire stream.
async fn share(proxy: &Screencast, session: &Session<Screencast>, request: Request, control: &Control) -> Result<Shared> {
    let saved = if request.remember { token_path().and_then(|p| std::fs::read_to_string(p).ok()) } else { None };
    let mut opts = SelectSourcesOptions::default()
        .set_cursor_mode(if request.cursor { CursorMode::EMBEDDED } else { CursorMode::HIDDEN })
        .set_sources(SourceType::MONITOR | SourceType::WINDOW)
        .set_multiple(false)
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    if let Some(t) = saved.as_deref() {
        opts = opts.set_restore_token(t.trim());
    }
    let started = async {
        proxy.select_sources(session, opts).await?.response()?;
        proxy.start(session, None, Default::default()).await?.response()
    }
    .await;
    let streams = started.map_err(|e| anyhow!("screen sharing was cancelled or refused ({e})"))?;
    if let Some(t) = streams.restore_token() {
        if let Err(e) = save_token(t) {
            control.report_error(format!("couldn't remember the chosen screen, so you'll be asked again next time: {e:#}"));
        }
    }
    let stream = streams.streams().first().ok_or_else(|| anyhow!("nothing was chosen to share"))?;
    let (node, serial, size) = (stream.pipe_wire_node_id(), stream.pipewire_serial(), stream.size());
    let fd = proxy.open_pipe_wire_remote(session, Default::default()).await.map_err(|e| anyhow!("couldn't connect to the screen stream: {e}"))?;
    Ok(Shared { fd, node, serial, size })
}

/// A node PipeWire shows on the portal's connection.
struct Node {
    name: String,
    class: String,
    id: String,
    serial: String,
}

/// Why the nodes on the portal's connection couldn't be listed.
#[derive(Debug, thiserror::Error)]
enum NodeListError {
    #[error("GStreamer's PipeWire device provider isn't installed")]
    NoProvider,
    #[error("the PipeWire device provider can't be pointed at the portal's connection (no fd property)")]
    NoFdProperty,
    #[error("couldn't copy the portal's connection: {0}")]
    CopyFd(#[from] std::io::Error),
    #[error("couldn't connect to the portal's connection: {0}")]
    Connect(#[from] gst::glib::BoolError),
}

/// The nodes on the portal's connection (the shared screen should be listed).
fn portal_nodes(p: &PortalStream) -> std::result::Result<Vec<Node>, NodeListError> {
    let provider = gst::DeviceProviderFactory::by_name("pipewiredeviceprovider").ok_or(NodeListError::NoProvider)?;
    if provider.find_property("fd").is_none() {
        return Err(NodeListError::NoFdProperty);
    }
    let fd = p.fd.try_clone()?;
    provider.set_property("fd", fd.as_raw_fd());
    provider.start()?;
    let list = provider
        .devices()
        .iter()
        .map(|d| {
            let props = d.properties();
            let get = |k: &str| props.as_ref().and_then(|s| s.get::<String>(k).ok()).unwrap_or_default();
            Node { name: d.display_name().into(), class: d.device_class().into(), id: get("object.id"), serial: get("object.serial") }
        })
        .collect();
    provider.stop();
    drop(fd);
    Ok(list)
}

/// The screen stream's serial, looked up on the connection when the portal didn't report it
/// (GNOME doesn't). Without it the pipeline falls back to the node id.
pub fn lookup_serial(p: &PortalStream, control: &Control) -> Option<u64> {
    let nodes = portal_nodes(p).map_err(|e| control.report_error(format!("looking up the screen stream's serial: {e}"))).ok()?;
    let node = p.node.to_string();
    nodes.into_iter().find(|n| n.id == node)?.serial.parse().ok()
}

/// What PipeWire shows on the portal's connection, for troubleshooting.
pub fn visible_nodes(p: &PortalStream) -> String {
    match portal_nodes(p) {
        Err(e) => format!("can't check: {e}"),
        Ok(list) if list.is_empty() => "nothing".into(),
        Ok(list) => list.iter().map(|n| format!("{} [{}; id {}; serial {}]", n.name, n.class, n.id, n.serial)).collect::<Vec<_>>().join(", "),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    /// The token lets the screen be shared again without asking: for this user only.
    #[test]
    fn the_token_file_is_private() {
        let dir = std::env::temp_dir().join(format!("streamroom-token-test-{}", std::process::id()));
        let p = dir.join("state").join("screencast-token");
        let mode = || std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        super::write_private(&p, "first").unwrap();
        assert_eq!(mode(), 0o600);
        // A file from an older version, readable by everyone, is tightened too.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::write_private(&p, "second").unwrap();
        assert_eq!(mode(), 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
