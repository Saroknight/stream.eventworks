//! Screen capture and hardware encoding for Streamroom Live: Windows (below) and Linux
//! (`linux/`: Wayland portal or X11, GStreamer with VA-API / NVENC / x264, PipeWire sound).
//!
//! Windows:
//! Video: DXGI Desktop Duplication -> D3D11 video processor (BGRA -> NV12, scaling, all on the GPU)
//! -> Media Foundation hardware H.264 encoder (NVENC / AMF / QuickSync behind one interface).
//! Frames never leave the GPU until they are compressed.
//!
//! Audio: WASAPI loopback: everything the PC plays, everything except Discord, or only chosen
//! apps (per-app process loopback on Windows 10 build 20348+ / Windows 11).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// One compressed H.264 access unit in Annex B format (start codes), SPS/PPS included on keyframes.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    /// Capture time, wall clock, microseconds since the Unix epoch.
    pub capture_unix_us: i64,
    /// Time from screen capture to encoder output.
    pub encode_latency_us: u32,
}

/// Which sound to share.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum AudioMode {
    /// Everything the PC plays.
    All,
    /// Everything except Discord (so viewers don't hear voice chat echoed back).
    #[default]
    ExceptDiscord,
    /// Only these apps (exe names, e.g. "eldenring.exe").
    Apps(Vec<String>),
}

/// Interleaved 16-bit PCM, 10 ms per chunk.
pub struct AudioChunk {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Clone, Debug)]
pub struct VideoOptions {
    /// Output height (width follows the screen's aspect ratio). 0 = native.
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Monitor index, 0 = primary (see `list_monitors`).
    pub monitor: u32,
    /// Draw the mouse cursor into the stream.
    pub cursor: bool,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self { height: 1080, fps: 60, bitrate_bps: 8_000_000, monitor: 0, cursor: true }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Monitor {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub gpu: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct VideoInfo {
    pub adapter: String,
    pub encoder: String,
    pub capture_method: String,
    pub source_width: u32,
    pub source_height: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Codec settings the encoder refused (it still works, just with its defaults for these).
    pub unsupported_settings: Vec<String>,
}

/// Shared between the publishing side and the capture/encode threads.
#[derive(Default)]
pub struct Control {
    pub stop: AtomicBool,
    pub keyframe_requested: AtomicBool,
    /// 0 = no change requested.
    pub bitrate_request: AtomicU32,
    pub stats: Stats,
    /// Why video capture stopped on its own, if it did (the first reason given).
    stopped: Mutex<Option<String>>,
    /// The copy of the stream being saved, if any.
    #[cfg(target_os = "linux")]
    recording: Mutex<linux::record::Slot>,
}

/// Saving a copy of the stream on this computer (Linux only, see `CAN_RECORD`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Recording {
    #[default]
    Off,
    On {
        path: String,
    },
    /// Couldn't start, or stopped on its own (disk full, ...). The stream goes on.
    Failed {
        path: String,
        why: String,
    },
}

/// This build can save a copy of the stream.
pub const CAN_RECORD: bool = cfg!(target_os = "linux");

#[derive(Default)]
pub struct Stats {
    /// New desktop images captured.
    pub captured: AtomicU64,
    /// Frames handed to the encoder (includes repeats of a still screen).
    pub submitted: AtomicU64,
    pub encoded: AtomicU64,
    pub keyframes: AtomicU64,
    pub bytes: AtomicU64,
    /// Encoded frames dropped because the network side was behind.
    pub dropped: AtomicU64,
    pub last_latency_us: AtomicU32,
    pub audio_chunks: AtomicU64,
}

impl Control {
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Relaxed);
    }
    /// Logs a problem in a capture thread.
    pub fn report_error(&self, msg: String) {
        tracing::warn!("{msg}");
    }
    /// Video capture stopped on its own (the screen share ended, the encoder failed): records
    /// why for the app and stops everything.
    pub fn report_stopped(&self, why: String) {
        self.report_error(format!("video stopped: {why}"));
        self.stopped.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(why);
        self.stop.store(true, Ordering::Relaxed);
    }
    /// Why video capture stopped on its own, if it did.
    pub fn stopped_reason(&self) -> Option<String> {
        self.stopped.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
    pub fn request_bitrate(&self, bps: u32) {
        self.bitrate_request.store(bps, Ordering::Relaxed);
    }
}

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod cursor;
#[cfg(windows)]
mod d3d;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod pipeline;

#[cfg(windows)]
pub use audio::{audio_apps, start_audio};
#[cfg(windows)]
pub use pipeline::start_video;

/// Monitors in the order the `monitor` option uses (primary first).
#[cfg(windows)]
pub fn list_monitors() -> Vec<Monitor> {
    d3d::list_monitors()
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{audio_apps, list_monitors, recording, screen_dialog, start_audio, start_recording, start_video, stop_recording};

/// Going live may show a "choose a screen" dialog from the desktop (Linux on Wayland only).
#[cfg(not(target_os = "linux"))]
pub fn screen_dialog() -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub fn start_recording(_control: &Control, _with_audio: bool) -> anyhow::Result<std::path::PathBuf> {
    anyhow::bail!("saving a copy of the stream is only available on Linux")
}

#[cfg(not(target_os = "linux"))]
pub fn stop_recording(_control: &Control) {}

#[cfg(not(target_os = "linux"))]
pub fn recording(_control: &Control) -> Recording {
    Recording::Off
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn list_monitors() -> Vec<Monitor> {
    Vec::new()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn start_video(
    _opts: VideoOptions,
    _control: std::sync::Arc<Control>,
    _tx: std::sync::mpsc::SyncSender<EncodedFrame>,
) -> anyhow::Result<(VideoInfo, std::thread::JoinHandle<()>)> {
    anyhow::bail!("screen capture is only available on Windows")
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn start_audio(
    _control: std::sync::Arc<Control>,
    _tx: std::sync::mpsc::SyncSender<AudioChunk>,
    _mode: AudioMode,
) -> anyhow::Result<(std::thread::JoinHandle<()>, String)> {
    anyhow::bail!("audio capture is only available on Windows")
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn audio_apps() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::Control;
    use std::sync::atomic::Ordering;

    #[test]
    fn first_stop_reason_wins_and_stops_everything() {
        let control = Control::default();
        assert_eq!(control.stopped_reason(), None);
        control.report_stopped("the screen share ended".into());
        control.report_stopped("a later, knock-on failure".into());
        assert_eq!(control.stopped_reason().as_deref(), Some("the screen share ended"));
        assert!(control.stop.load(Ordering::Relaxed));
    }
}
