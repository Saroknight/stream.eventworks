//! Linux sound through PipeWire (CachyOS, Arch and most current distros):
//!
//! - all PC audio: the default output's monitor,
//! - everything except Discord / only chosen apps: each app's own output stream, captured
//!   separately and mixed. PipeWire exposes every app's sound as its own stream, so this
//!   doesn't need anything special from the desktop.
//!
//! App streams are found with `pw-dump` (part of PipeWire) and re-checked every few seconds, so
//! apps that start or stop playing are picked up. Output: 48 kHz stereo, 10 ms chunks of
//! 16-bit PCM, continuous (silence when nothing plays).

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use super::launch::{launch, next_error};
use crate::{AudioChunk, AudioMode, Control};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const CHUNK: usize = (RATE / 100) as usize * CHANNELS as usize;

/// An app's sound stream, as PipeWire lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AppStream {
    serial: u64,
    /// Lowercase program name (e.g. "firefox", "discord", "wine64-preloader").
    app: String,
}

fn is_discord(app: &str) -> bool {
    app.contains("discord") || app.contains("vesktop") || app.contains("webcord")
}

/// App output streams playing (or open) right now.
fn app_streams() -> Vec<AppStream> {
    let Ok(out) = Command::new("pw-dump").output() else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return Vec::new() };
    let me = std::process::id().to_string();
    let mut list: Vec<AppStream> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let props = o.pointer("/info/props")?;
            if props.get("media.class")?.as_str()? != "Stream/Output/Audio" {
                return None;
            }
            if props.get("application.process.id").map(|p| p.to_string().trim_matches('"') == me).unwrap_or(false) {
                return None;
            }
            let serial = props.get("object.serial")?.as_u64()?;
            let app = ["application.process.binary", "application.name", "node.name"]
                .iter()
                .find_map(|k| props.get(*k).and_then(|x| x.as_str()))
                .unwrap_or("app")
                .to_ascii_lowercase();
            Some(AppStream { serial, app })
        })
        .collect();
    list.sort();
    list
}

/// Program names of apps with sound streams now, for "Only certain apps".
pub fn audio_apps() -> Vec<String> {
    app_streams().into_iter().map(|s| s.app).collect::<BTreeSet<_>>().into_iter().collect()
}

#[derive(Clone, Debug, PartialEq)]
enum Plan {
    /// The default output's monitor: everything.
    All,
    /// These app streams, mixed (may be none: silence).
    Streams(Vec<u64>),
}

fn plan(mode: &AudioMode) -> (Plan, String) {
    match mode {
        AudioMode::All => (Plan::All, "all PC audio".into()),
        AudioMode::ExceptDiscord => {
            let s = app_streams();
            let keep: Vec<u64> = s.iter().filter(|a| !is_discord(&a.app)).map(|a| a.serial).collect();
            (Plan::Streams(keep), "everything except Discord".into())
        }
        AudioMode::Apps(names) => {
            let want: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
            let keep: Vec<u64> = app_streams().iter().filter(|a| want.contains(&a.app)).map(|a| a.serial).collect();
            (Plan::Streams(keep), format!("only {}", names.join(", ")))
        }
    }
}

fn pipeline_text(plan: &Plan) -> String {
    let caps = format!("audio/x-raw,format=S16LE,rate={RATE},channels={CHANNELS},layout=interleaved");
    let tail = format!("audioconvert ! audioresample ! {caps} ! appsink name=sink sync=false max-buffers=50 drop=true");
    match plan {
        Plan::All => format!("pulsesrc device=@DEFAULT_MONITOR@ provide-clock=false ! {tail}"),
        Plan::Streams(serials) => {
            // A silent source keeps the mix running when no app is playing.
            let mut s = format!(
                "audiomixer name=mix latency=20000000 ! {tail} \
                 audiotestsrc is-live=true wave=silence ! audioconvert ! audioresample ! {caps} ! mix."
            );
            // media.type=Audio: WirePlumber only links nodes of the same media type, and recent
            // pipewiresrc connects as "Unknown" unless told (then: "target not found").
            for serial in serials {
                s.push_str(&format!(
                    " pipewiresrc target-object={serial} always-copy=true \
                     stream-properties=\"props,node.dont-reconnect=true,media.type=Audio,media.category=Capture,media.role=Production\" \
                     ! audioconvert ! audioresample ! {caps} ! queue max-size-time=200000000 leaky=downstream ! mix."
                ));
            }
            s
        }
    }
}

pub fn start_audio(control: Arc<Control>, tx: SyncSender<AudioChunk>, mode: AudioMode) -> Result<(std::thread::JoinHandle<()>, String)> {
    gst::init().context("GStreamer")?;
    let (_, desc) = plan(&mode);
    let handle = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut fails = 0;
        while !control.stop.load(Ordering::Relaxed) {
            let (p, _) = plan(&mode);
            match run(&control, &tx, &mode, &p) {
                Ok(()) => fails = 0,
                Err(e) => {
                    fails += 1;
                    control.report_error(format!("audio capture restarting: {e:#}"));
                    // Per-app capture keeps failing: fall back to everything rather than silence.
                    if fails >= 3 && p != Plan::All {
                        match run(&control, &tx, &AudioMode::All, &Plan::All) {
                            Ok(()) => fails = 0,
                            Err(e) => control.report_error(format!("all-audio fallback failed: {e:#}")),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    })?;
    Ok((handle, desc))
}

/// Runs one pipeline until the plan changes (apps start/stop) or we're stopped.
/// The pipeline is shut down however this exits.
fn run(control: &Arc<Control>, tx: &SyncSender<AudioChunk>, mode: &AudioMode, p: &Plan) -> Result<()> {
    let (pipeline, sink) = launch(&pipeline_text(p), "audio", control)?;
    pipeline.set_state(gst::State::Playing).context("starting audio")?;
    let bus = pipeline.bus();
    let mut pending: Vec<i16> = Vec::with_capacity(CHUNK * 4);
    let mut last_check = Instant::now();
    loop {
        if control.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if let Some(e) = bus.as_ref().and_then(|b| next_error(b, gst::ClockTime::ZERO)) {
            break Err(anyhow!(e.message));
        }
        if !matches!(mode, AudioMode::All) && last_check.elapsed() > Duration::from_secs(3) {
            last_check = Instant::now();
            if plan(mode).0 != *p {
                break Ok(());
            }
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) else { continue };
        let Some(buf) = sample.buffer() else { continue };
        let Ok(map) = buf.map_readable() else { continue };
        pending.extend(map.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)));
        while pending.len() >= CHUNK {
            let samples: Vec<i16> = pending.drain(..CHUNK).collect();
            control.stats.audio_chunks.fetch_add(1, Ordering::Relaxed);
            match tx.try_send(AudioChunk { samples, sample_rate: RATE, channels: CHANNELS }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
    }
}
