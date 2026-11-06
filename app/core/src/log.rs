//! A plain log file, so problems can be diagnosed even when nobody saw the window.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Sets the log file. Without this, logging goes to `streamroom-live.log` in the temp folder.
pub fn set_path(path: PathBuf) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep it from growing forever: start fresh once it passes 2 MB.
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    *PATH.lock().unwrap() = Some(path);
}

pub fn path() -> PathBuf {
    PATH.lock().unwrap().clone().unwrap_or_else(|| std::env::temp_dir().join("streamroom-live.log"))
}

pub fn info(line: &str) {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = writeln!(f, "[{secs}] {line}");
    }
}
