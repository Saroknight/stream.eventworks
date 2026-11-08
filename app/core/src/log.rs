//! A plain log file, so problems can be diagnosed even when nobody saw the window.
//!
//! Code logs with `tracing` (`info!`, `warn!`, ...). [`init`] sends it to the file through a
//! background writer, so logging never waits on the disk; [`flush`] before exiting writes out
//! what's still queued.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The log file starts fresh once it's bigger than this.
const MAX_SIZE: u64 = 2_000_000;

static PATH: OnceLock<PathBuf> = OnceLock::new();
/// Keeps the background writer running; dropping it writes out the queued lines.
static GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

/// Starts logging to `path`, or to `streamroom-live.log` in the temp folder. Only the first
/// call counts.
pub fn init(path: Option<PathBuf>) {
    let path = PATH.get_or_init(|| path.unwrap_or_else(default_path));
    // Nothing logs yet, so problems setting up the log can only go to the console.
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("can't create the log folder {}: {e}", dir.display());
        }
    }
    roll_over(path);
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => return eprintln!("can't open the log file {}: {e}", path.display()),
    };
    let (writer, guard) = tracing_appender::non_blocking(file);
    let layer = tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false);
    match tracing_subscriber::registry().with(layer).with(filter()).try_init() {
        Ok(()) => *GUARD.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard),
        Err(e) => eprintln!("can't log to {}: {e}", path.display()),
    }
}

/// Our own crates log from `info` up, everything else only warnings and errors.
fn filter() -> Targets {
    Targets::new().with_default(LevelFilter::WARN).with_target("streamroom", LevelFilter::INFO).with_target("wincap", LevelFilter::INFO)
}

/// Keeps the file from growing forever: past [`MAX_SIZE`] it's renamed to `*.old.log`.
fn roll_over(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_SIZE) {
        if let Err(e) = std::fs::rename(path, path.with_extension("old.log")) {
            eprintln!("can't start a fresh log file {}: {e}", path.display());
        }
    }
}

/// Writes out queued lines. Call right before exiting; anything logged after is lost.
pub fn flush() {
    drop(GUARD.lock().unwrap_or_else(PoisonError::into_inner).take());
}

pub fn path() -> PathBuf {
    PATH.get().cloned().unwrap_or_else(default_path)
}

fn default_path() -> PathBuf {
    std::env::temp_dir().join("streamroom-live.log")
}

#[cfg(test)]
mod tests {
    use tracing::Level;

    #[test]
    fn our_crates_log_info_others_only_warnings() {
        let f = super::filter();
        for ours in ["streamroom_core", "streamroom_live", "streamroom_live_cli", "wincap::linux::audio"] {
            assert!(f.would_enable(ours, &Level::INFO), "{ours}");
            assert!(!f.would_enable(ours, &Level::DEBUG), "{ours}");
        }
        assert!(!f.would_enable("zbus::connection", &Level::INFO));
        assert!(f.would_enable("zbus::connection", &Level::WARN));
    }
}
