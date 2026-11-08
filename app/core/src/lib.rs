//! Streamroom Live engine, shared by the desktop app and the console tool.
//!
//! A [`Session`] fetches a LiveKit token with an app link, captures the screen on the GPU
//! (wincap: DXGI Desktop Duplication -> hardware H.264), publishes the already-encoded video
//! plus PC audio, and reports stats. Stopping it, or the room ending, tears everything down.

pub mod api;
pub mod link;
pub mod log;
#[cfg(feature = "testsrc")]
mod testsrc;

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use livekit::options::{AudioEncoding, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

pub use api::{Account, Api, Launch, SignedOut};
pub use link::{AppLink, Joined};
pub use wincap::{audio_apps, list_monitors, AudioMode, Control, Monitor, VideoInfo};

/// Call once at startup. Two TLS backends end up compiled in through different dependencies;
/// without picking one, the first secure (wss://) connection panics.
pub fn init() {
    if rustls::crypto::aws_lc_rs::default_provider().install_default().is_err() {
        info!("a TLS backend was already chosen; keeping it");
    }
}

/// The sound setting as saved. The names are what `settings.json` and the window use
/// ("except_discord", "apps", "all").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioChoice {
    /// Only `Settings::audio_apps`.
    Apps,
    All,
    /// Also what an unknown value reads as, so one bad value doesn't reset every setting.
    /// (`serde(other)` has to be on the last variant.)
    #[default]
    #[serde(other)]
    ExceptDiscord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Output height: 0 = native, otherwise 720 / 1080 / 1440 (width follows the screen).
    pub height: u32,
    pub fps: u32,
    pub bitrate_mbps: f32,
    /// Monitor index (see `list_monitors`), 0 = primary.
    pub monitor: u32,
    pub audio: bool,
    /// Which sound to share.
    pub audio_mode: AudioChoice,
    /// Exe names for "apps", e.g. ["eldenring.exe"].
    pub audio_apps: Vec<String>,
    pub cursor: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            height: 1080,
            fps: 60,
            bitrate_mbps: 8.0,
            monitor: 0,
            audio: true,
            audio_mode: AudioChoice::ExceptDiscord,
            audio_apps: Vec::new(),
            cursor: true,
        }
    }
}

impl Settings {
    pub fn audio_mode(&self) -> wincap::AudioMode {
        match self.audio_mode {
            AudioChoice::All => wincap::AudioMode::All,
            AudioChoice::Apps if !self.audio_apps.is_empty() => wincap::AudioMode::Apps(self.audio_apps.clone()),
            AudioChoice::Apps => wincap::AudioMode::All,
            AudioChoice::ExceptDiscord => wincap::AudioMode::ExceptDiscord,
        }
    }

    /// Video bitrate in bits per second, held to 1-50 Mbps.
    fn bitrate_bps(&self) -> u32 {
        (self.bitrate_mbps.clamp(1.0, 50.0) * 1e6) as u32
    }

    fn video_options(&self) -> wincap::VideoOptions {
        wincap::VideoOptions { height: self.height, fps: self.fps, bitrate_bps: self.bitrate_bps(), monitor: self.monitor, cursor: self.cursor }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Screen,
    /// Moving test pattern (software encoded). Needs the `testsrc` feature.
    TestPattern,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveInfo {
    pub room_title: String,
    pub server: String,
    pub video: VideoInfo,
    pub bitrate_mbps: f32,
    pub audio: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub capture_fps: f64,
    pub sent_fps: f64,
    pub mbps: f64,
    pub encode_ms: f64,
    pub keyframes: u64,
    pub dropped: u64,
    pub seconds: u64,
}

struct Meter {
    at: Instant,
    started: Instant,
    prev: (u64, u64, u64, u64),
}

pub struct Session {
    control: Arc<Control>,
    room: Room,
    ended: Arc<Mutex<Option<String>>>,
    threads: Vec<JoinHandle<()>>,
    events: tokio::task::JoinHandle<()>,
    meter: Mutex<Meter>,
}

impl Session {
    /// Older flow: an app link with a 12-hour ticket (still accepted by the server).
    pub async fn start(link: &AppLink, settings: &Settings, source: Source) -> Result<(Session, LiveInfo)> {
        info!("connecting to {} (room {})", link.base, link.room_id);
        let joined = link::fetch_token(link).await?;
        Self::start_joined(&link.base, joined, settings, source).await
    }

    /// Start streaming with a LiveKit pass already in hand (e.g. from [`Api::app_token`]).
    pub async fn start_joined(server: &str, joined: Joined, settings: &Settings, source: Source) -> Result<(Session, LiveInfo)> {
        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = false;
        let (room, mut events) = Room::connect(&joined.livekit_url, &joined.token, room_options)
            .await
            .with_context(|| format!("could not connect to the media server at {}", joined.livekit_url))?;
        info!("connected to \"{}\" via {}", joined.title, joined.livekit_url);

        let control = Arc::new(Control::default());
        let result = Self::publish(&room, &control, settings, source).await;
        let (info, threads) = match result {
            Ok(r) => r,
            Err(e) => {
                control.stop.store(true, Ordering::Relaxed);
                if let Err(close) = room.close().await {
                    warn!("leaving the room after a failed start: {close}");
                }
                return Err(e);
            }
        };

        // Watch for the room ending or us being removed.
        let ended = Arc::new(Mutex::new(None::<String>));
        let (ended2, control2) = (ended.clone(), control.clone());
        let events = tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if let RoomEvent::Disconnected { reason } = ev {
                    let msg = match reason {
                        livekit::DisconnectReason::RoomDeleted => "The room was ended.".to_string(),
                        livekit::DisconnectReason::ParticipantRemoved => "You were removed from the room.".to_string(),
                        livekit::DisconnectReason::ClientInitiated => return,
                        other => format!("Disconnected from the server ({other:?})."),
                    };
                    *ended2.lock().unwrap_or_else(PoisonError::into_inner) = Some(msg);
                    control2.stop.store(true, Ordering::Relaxed);
                    return;
                }
            }
        });

        let now = Instant::now();
        let live =
            LiveInfo { room_title: joined.title, server: server.to_string(), video: info.0, bitrate_mbps: settings.bitrate_mbps, audio: info.1 };
        info!(
            "live: {} {}x{} -> {}x{} @ {} fps, {} ({:.1} Mbps), GPU {}, audio {}",
            live.video.capture_method,
            live.video.source_width,
            live.video.source_height,
            live.video.width,
            live.video.height,
            live.video.fps,
            live.video.encoder,
            live.bitrate_mbps,
            live.video.adapter,
            live.audio
        );
        Ok((Session { control, room, ended, threads, events, meter: Mutex::new(Meter { at: now, started: now, prev: (0, 0, 0, 0) }) }, live))
    }

    async fn publish(room: &Room, control: &Arc<Control>, settings: &Settings, source: Source) -> Result<((VideoInfo, String), Vec<JoinHandle<()>>)> {
        let bitrate = settings.bitrate_bps();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<wincap::EncodedFrame>(8);
        let (info, video_thread) = match source {
            Source::Screen => wincap::start_video(settings.video_options(), control.clone(), frame_tx)?,
            Source::TestPattern => start_test_pattern(control.clone(), frame_tx)?,
        };
        let mut threads = vec![video_thread];

        // Video is already encoded on the GPU: LiveKit only packetizes it.
        let video_source = NativeVideoSource::new_encoded(VideoResolution { width: info.width, height: info.height });
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video_source.clone()));
        room.local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    video_codec: VideoCodec::H264,
                    video_encoder: VideoEncoderBackend::PreEncoded,
                    simulcast: false,
                    source: TrackSource::Screenshare,
                    video_encoding: Some(VideoEncoding { max_bitrate: bitrate as u64, max_framerate: info.fps as f64 }),
                    ..Default::default()
                },
            )
            .await
            .context("publishing the video track")?;

        // Encoded frames -> LiveKit, plus keyframe and bitrate requests back to the encoder.
        let pump_control = control.clone();
        threads.push(std::thread::Builder::new().name("video-pump".into()).spawn(move || {
            let mut last_rate: u32 = 0;
            while let Ok(f) = frame_rx.recv() {
                let frame = EncodedVideoFrame {
                    codec: EncodedVideoCodec::H264,
                    payload: &f.data,
                    timestamp_us: f.capture_unix_us,
                    frame_type: if f.keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
                    resolution: VideoResolution { width: f.width, height: f.height },
                    frame_metadata: None,
                };
                video_source.capture_encoded_frame(&frame);
                if video_source.take_keyframe_request() {
                    pump_control.request_keyframe();
                }
                if let Some(rc) = video_source.take_rate_control_request() {
                    let bps = (rc.target_bitrate_bps as u32).min(bitrate);
                    // Only pass on meaningful changes; encoders don't like constant tweaking.
                    if bps > 0 && (last_rate == 0 || (bps as i64 - last_rate as i64).unsigned_abs() > last_rate as u64 / 10) {
                        last_rate = bps;
                        pump_control.request_bitrate(bps);
                    }
                }
            }
        })?);

        // PC audio: everything, everything except Discord, or only chosen apps.
        let mut audio = "off".to_string();
        if settings.audio && source == Source::Screen {
            let (audio_tx, audio_rx) = mpsc::sync_channel::<wincap::AudioChunk>(50);
            match wincap::start_audio(control.clone(), audio_tx, settings.audio_mode()) {
                Ok((handle, what)) => {
                    threads.push(handle);
                    audio = what;
                    let source = NativeAudioSource::new(
                        AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false },
                        48_000,
                        2,
                        100,
                    );
                    let track = LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(source.clone()));
                    room.local_participant()
                        .publish_track(
                            LocalTrack::Audio(track),
                            TrackPublishOptions {
                                source: TrackSource::ScreenshareAudio,
                                dtx: false,
                                red: false,
                                audio_encoding: Some(AudioEncoding { max_bitrate: 128_000 }),
                                ..Default::default()
                            },
                        )
                        .await
                        .context("publishing the audio track")?;
                    let rt = tokio::runtime::Handle::current();
                    threads.push(std::thread::Builder::new().name("audio-pump".into()).spawn(move || {
                        let mut failed = 0u64;
                        while let Ok(chunk) = audio_rx.recv() {
                            let frames = chunk.samples.len() as u32 / chunk.channels as u32;
                            let frame = AudioFrame {
                                data: chunk.samples.into(),
                                sample_rate: chunk.sample_rate,
                                num_channels: chunk.channels as u32,
                                samples_per_channel: frames,
                            };
                            if let Err(e) = rt.block_on(source.capture_frame(&frame)) {
                                // Once, not 100 times a second.
                                if failed == 0 {
                                    warn!("sending sound failed: {e}");
                                }
                                failed += 1;
                            }
                        }
                        if failed > 1 {
                            warn!("sending sound failed {failed} times in all");
                        }
                    })?);
                }
                Err(e) => {
                    warn!("audio not available: {e:#}");
                    audio = "unavailable".into();
                }
            }
        }
        Ok(((info, audio), threads))
    }

    /// Frame rates and bitrate since the previous call.
    pub fn stats(&self) -> Stats {
        let s = &self.control.stats;
        let mut m = self.meter.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (
            s.captured.load(Ordering::Relaxed),
            s.encoded.load(Ordering::Relaxed),
            s.bytes.load(Ordering::Relaxed),
            s.dropped.load(Ordering::Relaxed),
        );
        let dt = m.at.elapsed().as_secs_f64().max(0.001);
        let out = Stats {
            capture_fps: (now.0 - m.prev.0) as f64 / dt,
            sent_fps: (now.1 - m.prev.1) as f64 / dt,
            mbps: (now.2 - m.prev.2) as f64 * 8.0 / dt / 1e6,
            encode_ms: s.last_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
            keyframes: s.keyframes.load(Ordering::Relaxed),
            dropped: now.3 - m.prev.3,
            seconds: m.started.elapsed().as_secs(),
        };
        m.prev = now;
        m.at = Instant::now();
        out
    }

    /// Why the session ended on its own (room ended, removed, capture failed), if it did.
    pub fn ended(&self) -> Option<String> {
        if let Some(m) = self.ended.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Some(m);
        }
        if let Some(why) = self.control.stopped_reason() {
            let msg = format!("Screen capture stopped: {why}");
            *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = Some(msg.clone());
            return Some(msg);
        }
        if self.control.stop.load(Ordering::Relaxed) {
            return Some("Streaming stopped.".into());
        }
        None
    }

    pub async fn stop(self) {
        self.control.stop.store(true, Ordering::Relaxed);
        self.events.abort();
        if let Err(e) = self.room.close().await {
            warn!("leaving the room: {e}");
        }
        let threads = self.threads;
        let joined = tokio::task::spawn_blocking(move || {
            for t in threads {
                let name = t.thread().name().unwrap_or("capture").to_string();
                if t.join().is_err() {
                    error!("the {name} thread crashed");
                }
            }
        })
        .await;
        if let Err(e) = joined {
            warn!("waiting for the capture threads: {e}");
        }
        info!("stopped");
    }
}

#[cfg(feature = "testsrc")]
fn start_test_pattern(control: Arc<Control>, tx: mpsc::SyncSender<wincap::EncodedFrame>) -> Result<(VideoInfo, JoinHandle<()>)> {
    testsrc::start(1280, 720, 30, 2_500_000, control, tx)
}

#[cfg(not(feature = "testsrc"))]
fn start_test_pattern(_control: Arc<Control>, _tx: mpsc::SyncSender<wincap::EncodedFrame>) -> Result<(VideoInfo, JoinHandle<()>)> {
    anyhow::bail!("this build has no test pattern")
}

/// Milliseconds since the Unix epoch (for "live since" in UIs).
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Waits up to `timeout` for `f` to return true. Small helper for callers polling sessions.
pub async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Captures and encodes the screen for a few seconds without any server, and reports what
/// happened. For troubleshooting (`streamroom-live-cli --capture-test`).
pub fn capture_test(settings: &Settings, seconds: u64) -> Result<String> {
    let control = Arc::new(Control::default());
    let (tx, rx) = mpsc::sync_channel::<wincap::EncodedFrame>(64);
    let (info, thread) = wincap::start_video(settings.video_options(), control.clone(), tx)?;
    let (mut frames, mut bytes, mut keys) = (0u64, 0u64, 0u64);
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        if let Ok(f) = rx.recv_timeout(Duration::from_millis(200)) {
            frames += 1;
            bytes += f.data.len() as u64;
            keys += f.keyframe as u64;
        }
    }
    control.stop.store(true, Ordering::Relaxed);
    drop(rx);
    if thread.join().is_err() {
        error!("the video thread crashed during the capture test");
    }
    let st = &control.stats;
    let (captured, submitted) = (st.captured.load(Ordering::Relaxed), st.submitted.load(Ordering::Relaxed));
    Ok(format!(
        "Capture:  {} {}x{} -> {}x{} @ {} fps\nEncoder:  {}\nResult:   {frames} frames ({keys} keyframes, {:.1} Mbps) in {seconds} s\n\
         Screen:   {captured} frames captured, {submitted} sent to the encoder (the rest were over the fps cap){}",
        info.capture_method,
        info.source_width,
        info.source_height,
        info.width,
        info.height,
        info.fps,
        info.encoder,
        bytes as f64 * 8.0 / seconds as f64 / 1e6,
        if frames == 0 { "\nNo frames arrived: the screen stream started but sent nothing." } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::{AudioChoice, Settings};

    #[test]
    fn audio_choice_keeps_the_saved_names() {
        for (choice, name) in [(AudioChoice::ExceptDiscord, "except_discord"), (AudioChoice::Apps, "apps"), (AudioChoice::All, "all")] {
            let json = format!("\"{name}\"");
            assert_eq!(serde_json::to_string(&choice).unwrap(), json);
            assert_eq!(serde_json::from_str::<AudioChoice>(&json).unwrap(), choice);
        }
    }

    #[test]
    fn unknown_audio_choice_keeps_the_other_settings() {
        let s: Settings = serde_json::from_str(r#"{"fps": 30, "audio_mode": "something_new"}"#).unwrap();
        assert_eq!(s.audio_mode, AudioChoice::ExceptDiscord);
        assert_eq!(s.fps, 30);
    }
}
