//! Streamroom Live: native screen streaming into a Streamroom room.
//!
//!   streamroom-live "https://stream.example.com/live/ROOM#k=KEY" [options]
//!
//! Captures the screen with DXGI Desktop Duplication, encodes on the graphics card
//! (Media Foundation: NVENC / AMF / QuickSync), and publishes the already-encoded H.264
//! to LiveKit, so the browser's capture throttle and software encoder are out of the picture.

mod link;
#[cfg(feature = "testsrc")]
mod testsrc;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use livekit::options::{AudioEncoding, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use wincap::{Control, VideoOptions};

struct Args {
    link: Option<String>,
    name: Option<String>,
    video: VideoOptions,
    audio: bool,
    test_pattern: bool,
}

const HELP: &str = "Streamroom Live: stream your screen into a Streamroom room

Usage: streamroom-live [APP_LINK] [options]

Options:
  --height N         Output height: 1080 (default), 720, 1440, or 0 for native
  --fps N            Frame rate (default 60)
  --bitrate MBPS     Video bitrate in Mbps (default 8)
  --monitor N        Monitor to capture, 0 = primary (default)
  --no-audio         Don't share game audio
  --test-pattern     Send a moving test pattern instead of the screen (testing only)
";

fn parse_args() -> Result<Args> {
    let mut a = Args { link: None, name: None, video: VideoOptions::default(), audio: true, test_pattern: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().with_context(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--name" => a.name = Some(value("--name")?),
            "--height" => a.video.height = value("--height")?.parse().context("--height")?,
            "--fps" => a.video.fps = value("--fps")?.parse().context("--fps")?,
            "--bitrate" => {
                let mbps: f64 = value("--bitrate")?.parse().context("--bitrate")?;
                a.video.bitrate_bps = (mbps * 1_000_000.0) as u32;
            }
            "--monitor" => a.video.monitor = value("--monitor")?.parse().context("--monitor")?,
            "--no-audio" => a.audio = false,
            "--test-pattern" => a.test_pattern = true,
            s if s.starts_with("--") => bail!("unknown option {s}\n\n{HELP}"),
            s => a.link = Some(s.to_string()),
        }
    }
    Ok(a)
}

#[cfg(feature = "testsrc")]
fn start_test_pattern(
    control: Arc<Control>,
    tx: mpsc::SyncSender<wincap::EncodedFrame>,
) -> Result<(wincap::VideoInfo, std::thread::JoinHandle<()>)> {
    testsrc::start(1280, 720, 30, 2_500_000, control, tx)
}

#[cfg(not(feature = "testsrc"))]
fn start_test_pattern(
    _control: Arc<Control>,
    _tx: mpsc::SyncSender<wincap::EncodedFrame>,
) -> Result<(wincap::VideoInfo, std::thread::JoinHandle<()>)> {
    bail!("this build has no test pattern (built without the testsrc feature)")
}

fn prompt(question: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(d) => print!("{question} [{d}]: "),
        None => print!("{question}: "),
    }
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim().to_string();
    Ok(if s.is_empty() { default.unwrap_or("").to_string() } else { s })
}

/// Log file next to the exe (falls back to the temp folder), so problems can be diagnosed
/// even if the window closes.
fn log_path() -> std::path::PathBuf {
    let name = "streamroom-live.log";
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).is_ok())
        .unwrap_or_else(|| std::env::temp_dir().join(name))
}

pub fn log(line: &str) {
    use std::sync::OnceLock;
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    let path = PATH.get_or_init(log_path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{t}] {line}");
    }
}

/// Prints to the console and the log file.
macro_rules! say {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        println!("{line}");
        log(&line);
    }};
}

fn main() {
    // Two TLS backends end up compiled in (via different dependencies); without picking one,
    // the first secure (wss://) connection panics.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    log(&format!("---- Streamroom Live {} starting", env!("CARGO_PKG_VERSION")));
    // Any crash: show it, log it, and keep the window open instead of vanishing.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("Unexpected crash: {info}");
        eprintln!("\n{msg}");
        log(&msg);
    }));

    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|rt| rt.block_on(run()))
    });
    let failed = match result {
        Ok(Ok(())) => false,
        Ok(Err(e)) => {
            eprintln!("\nError: {e:#}");
            log(&format!("Error: {e:#}"));
            true
        }
        Err(_) => true, // already printed by the panic hook
    };
    if failed {
        eprintln!("\nA log was saved to {}", log_path().display());
        // Double-clicked from Explorer: keep the window open long enough to read the error.
        let _ = prompt("Press Enter to close", None);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    say!("Streamroom Live {}", env!("CARGO_PKG_VERSION"));

    let link_text = match args.link {
        Some(l) => l,
        None => prompt("Paste your app link (Go Live → Copy app link)", None)?,
    };
    let link = link::parse(&link_text)?;
    // Streamroom uses the name on your account; this is only sent for older servers.
    let name = args.name.unwrap_or_else(|| std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "Streamer".into()));

    print!("Connecting to {} ... ", link.base);
    std::io::stdout().flush()?;
    let session = link::fetch_token(&link, &name).await?;
    let mut room_options = RoomOptions::default();
    room_options.auto_subscribe = false;
    let (room, mut events) = Room::connect(&session.livekit_url, &session.token, room_options)
        .await
        .with_context(|| format!("could not connect to the media server at {}", session.livekit_url))?;
    say!("connected to \"{}\" via {}", session.title, session.livekit_url);

    // ---- capture + encode ----
    let control = Arc::new(Control::default());
    let (frame_tx, frame_rx) = mpsc::sync_channel::<wincap::EncodedFrame>(8);
    let (info, video_thread) = if args.test_pattern {
        start_test_pattern(control.clone(), frame_tx)?
    } else {
        wincap::start_video(args.video.clone(), control.clone(), frame_tx)?
    };
    say!("GPU:      {}", info.adapter);
    say!("Capture:  {} {}x{} -> {}x{} @ {} fps", info.capture_method, info.source_width, info.source_height, info.width, info.height, info.fps);
    say!("Encoder:  {} (H.264, {:.1} Mbps)", info.encoder, args.video.bitrate_bps as f64 / 1e6);
    if !info.unsupported_settings.is_empty() {
        say!("          (encoder ignored: {})", info.unsupported_settings.join(", "));
    }

    // ---- publish video (already encoded: LiveKit only packetizes it) ----
    let resolution = VideoResolution { width: info.width, height: info.height };
    let video_source = NativeVideoSource::new_encoded(resolution.clone());
    let video_track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video_source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Video(video_track),
            TrackPublishOptions {
                video_codec: VideoCodec::H264,
                video_encoder: VideoEncoderBackend::PreEncoded,
                simulcast: false,
                source: TrackSource::Screenshare,
                video_encoding: Some(VideoEncoding { max_bitrate: args.video.bitrate_bps as u64, max_framerate: info.fps as f64 }),
                ..Default::default()
            },
        )
        .await
        .context("publishing the video track")?;

    // Encoded frames -> LiveKit, plus keyframe and bitrate requests back to the encoder.
    let pump_control = control.clone();
    let video_pump = std::thread::Builder::new().name("video-pump".into()).spawn(move || {
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
                let bps = rc.target_bitrate_bps as u32;
                // Only pass on meaningful changes; encoders don't like constant tweaking.
                if bps > 0 && (last_rate == 0 || (bps as i64 - last_rate as i64).unsigned_abs() > last_rate as u64 / 10) {
                    last_rate = bps;
                    pump_control.request_bitrate(bps);
                }
            }
        }
    })?;

    // ---- game audio ----
    let mut audio_status = "off";
    if args.audio && !args.test_pattern {
        let (audio_tx, audio_rx) = mpsc::sync_channel::<wincap::AudioChunk>(50);
        match wincap::start_audio(control.clone(), audio_tx) {
            Ok(_handle) => {
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
                std::thread::Builder::new().name("audio-pump".into()).spawn(move || {
                    while let Ok(chunk) = audio_rx.recv() {
                        let frames = chunk.samples.len() as u32 / chunk.channels as u32;
                        let frame = AudioFrame {
                            data: chunk.samples.into(),
                            sample_rate: chunk.sample_rate,
                            num_channels: chunk.channels as u32,
                            samples_per_channel: frames,
                        };
                        let _ = rt.block_on(source.capture_frame(&frame));
                    }
                })?;
                audio_status = "sharing PC audio";
            }
            Err(e) => say!("Audio: not available ({e:#}). Streaming video only."),
        }
    }

    say!("Audio:    {audio_status}");
    println!("\nLIVE. Press Ctrl+C to stop.\n");

    // ---- stats until Ctrl+C or disconnect ----
    let stats_control = control.clone();
    let stats_task = tokio::spawn(async move {
        let s = &stats_control.stats;
        let mut prev = (0u64, 0u64, 0u64, 0u64);
        let mut t = Instant::now();
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now = (s.captured.load(Ordering::Relaxed), s.encoded.load(Ordering::Relaxed), s.bytes.load(Ordering::Relaxed), s.dropped.load(Ordering::Relaxed));
            let dt = t.elapsed().as_secs_f64();
            t = Instant::now();
            let cap = (now.0 - prev.0) as f64 / dt;
            let enc = (now.1 - prev.1) as f64 / dt;
            let mbps = (now.2 - prev.2) as f64 * 8.0 / dt / 1e6;
            let dropped = now.3 - prev.3;
            prev = now;
            print!(
                "\rcapture {cap:5.1} fps | sent {enc:5.1} fps | {mbps:5.2} Mbps | encode {:4.1} ms | keyframes {} | dropped {dropped}   ",
                s.last_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
                s.keyframes.load(Ordering::Relaxed),
            );
            let _ = std::io::stdout().flush();
            for e in stats_control.take_errors() {
                println!();
                say!("{e}");
            }
            ticks += 1;
            if ticks % 10 == 0 {
                log(&format!("stats: capture {cap:.1} fps, sent {enc:.1} fps, {mbps:.2} Mbps, encode {:.1} ms, keyframes {}, dropped {dropped}",
                    s.last_latency_us.load(Ordering::Relaxed) as f64 / 1000.0, s.keyframes.load(Ordering::Relaxed)));
            }
            if stats_control.stop.load(Ordering::Relaxed) {
                break;
            }
        }
    });

    let reason = tokio::select! {
        _ = tokio::signal::ctrl_c() => "stopped".to_string(),
        r = async {
            while let Some(ev) = events.recv().await {
                if let RoomEvent::Disconnected { reason } = ev {
                    return format!("disconnected by the server ({reason:?})");
                }
            }
            "connection closed".to_string()
        } => r,
        _ = async {
            while !control.stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        } => "capture stopped (see error above)".to_string(),
    };
    say!("\n\n{reason}. Shutting down...");
    control.stop.store(true, Ordering::Relaxed);
    stats_task.abort();
    for e in control.take_errors() {
        say!("{e}");
    }
    let _ = room.close().await;
    let _ = video_thread.join();
    let _ = video_pump.join();
    Ok(())
}
