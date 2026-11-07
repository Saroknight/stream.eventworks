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
    /// Errors from the capture threads, for the app to show and log.
    pub errors: std::sync::Mutex<Vec<String>>,
}

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
    pub fn report_error(&self, msg: String) {
        eprintln!("{msg}");
        if let Ok(mut e) = self.errors.lock() {
            e.push(msg);
        }
    }
    pub fn take_errors(&self) -> Vec<String> {
        self.errors.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
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
pub use linux::{audio_apps, list_monitors, start_audio, start_video};

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
