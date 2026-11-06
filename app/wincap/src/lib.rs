//! Windows capture and hardware encoding for Streamroom Live.
//!
//! Video: DXGI Desktop Duplication -> D3D11 video processor (BGRA -> NV12, scaling, all on the GPU)
//! -> Media Foundation hardware H.264 encoder (NVENC / AMF / QuickSync behind one interface).
//! Frames never leave the GPU until they are compressed.
//!
//! Audio: WASAPI loopback of the default output device (everything the PC plays).

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
    /// Monitor index, 0 = primary.
    pub monitor: u32,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self { height: 1080, fps: 60, bitrate_bps: 8_000_000, monitor: 0 }
    }
}

#[derive(Clone, Debug)]
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
mod d3d;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod pipeline;

#[cfg(windows)]
pub use audio::start_audio;
#[cfg(windows)]
pub use pipeline::start_video;

#[cfg(not(windows))]
pub fn start_video(
    _opts: VideoOptions,
    _control: std::sync::Arc<Control>,
    _tx: std::sync::mpsc::SyncSender<EncodedFrame>,
) -> anyhow::Result<(VideoInfo, std::thread::JoinHandle<()>)> {
    anyhow::bail!("screen capture is only available on Windows")
}

#[cfg(not(windows))]
pub fn start_audio(
    _control: std::sync::Arc<Control>,
    _tx: std::sync::mpsc::SyncSender<AudioChunk>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    anyhow::bail!("audio capture is only available on Windows")
}
