//! Linux sound through PipeWire (CachyOS, Arch and most current distros):
//!
//! - all PC audio: the default output's monitor,
//! - everything except Discord / only chosen apps: each app's own output stream, captured
//!   separately and mixed. PipeWire exposes every app's sound as its own stream, so this
//!   doesn't need anything special from the desktop.
//!
//! App streams are found with `pw-dump` (part of PipeWire) and re-checked every few seconds, so
//! apps that start or stop playing are picked up. Output: 48 kHz stereo, 10 ms chunks of
//! 16-bit PCM, continuous (silence when nothing plays), except while per-app capture is paused
//! after repeated failures (nothing is sent then).

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use super::launch::{launch, next_error};
use super::record;
use crate::{AudioChunk, AudioMode, Control};

pub(super) const RATE: u32 = 48_000;
pub(super) const CHANNELS: u16 = 2;
/// Samples (all channels) in one 10 ms chunk.
pub(super) const CHUNK: usize = (RATE / 100) as usize * CHANNELS as usize;
/// `pw-dump` answers in a few milliseconds; longer means PipeWire is stuck.
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-app capture failing this many times in a row: share no sound for a while.
const FAILS_BEFORE_FALLBACK: u32 = 3;
/// How long no sound is shared before per-app capture is tried again.
const FALLBACK_TIME: Duration = Duration::from_secs(30);
/// A pipeline that ran this long was healthy: earlier failures no longer count.
const HEALTHY_RUN: Duration = Duration::from_secs(10);

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

/// PipeWire's object list as JSON, from `pw-dump`. Killed if it doesn't answer in time, so a
/// stuck PipeWire can't hang the sound thread (and with it, stopping the stream).
fn pw_dump() -> Result<Vec<u8>> {
    let mut child = Command::new("pw-dump").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("starting pw-dump")?;
    let mut stdout = child.stdout.take().context("pw-dump has no output")?;
    // Read while it runs: its output is bigger than the pipe holds.
    let reader = std::thread::Builder::new()
        .name("pw-dump".into())
        .spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).map(|_| out)
        })
        .context("reading pw-dump")?;
    let deadline = Instant::now() + PW_DUMP_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for pw-dump")? {
            break status;
        }
        if Instant::now() >= deadline {
            // Killing it closes the pipe, so the reader finishes on its own.
            child.kill().context("stopping pw-dump")?;
            child.wait().context("waiting for pw-dump to stop")?;
            bail!("pw-dump didn't answer within {} s (is PipeWire running?)", PW_DUMP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = reader.join().map_err(|_| anyhow!("reading pw-dump's output failed"))?.context("reading pw-dump's output")?;
    ensure!(status.success(), "pw-dump failed ({status})");
    Ok(out)
}

/// App output streams playing (or open) right now.
fn app_streams() -> Result<Vec<AppStream>> {
    let json = pw_dump()?;
    parse_app_streams(&json, &std::process::id().to_string())
}

/// The app output streams in `pw-dump` output, except this process's own (`me`: our pid).
fn parse_app_streams(json: &[u8], me: &str) -> Result<Vec<AppStream>> {
    let v: serde_json::Value = serde_json::from_slice(json).context("reading pw-dump's output")?;
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
    Ok(list)
}

/// Program names of apps with sound streams now, for "Only certain apps". Empty if PipeWire
/// can't be asked (the list is only a suggestion in Settings).
pub fn audio_apps() -> Vec<String> {
    app_streams().unwrap_or_default().into_iter().map(|s| s.app).collect::<BTreeSet<_>>().into_iter().collect()
}

#[derive(Clone, Debug, PartialEq)]
enum Plan {
    /// The default output's monitor: everything.
    All,
    /// These app streams, mixed (may be none: silence).
    Streams(Vec<u64>),
}

/// Which streams to capture for `mode` right now.
fn plan(mode: &AudioMode) -> Result<Plan> {
    let keep = |wanted: &dyn Fn(&str) -> bool| -> Result<Plan> {
        Ok(Plan::Streams(app_streams()?.into_iter().filter(|a| wanted(&a.app)).map(|a| a.serial).collect()))
    };
    match mode {
        AudioMode::All => Ok(Plan::All),
        AudioMode::ExceptDiscord => keep(&|app| !is_discord(app)),
        AudioMode::Apps(names) => keep(&|app| names.iter().any(|n| n.eq_ignore_ascii_case(app))),
    }
}

/// What `mode` shares, in words for the app.
fn describe(mode: &AudioMode) -> String {
    match mode {
        AudioMode::All => "all PC audio".into(),
        AudioMode::ExceptDiscord => "everything except Discord".into(),
        AudioMode::Apps(names) => format!("only {}", names.join(", ")),
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
                 audiotestsrc is-live=true wave=silence ! {caps} ! mix."
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
    let desc = describe(&mode);
    let handle = std::thread::Builder::new().name("audio".into()).spawn(move || capture(&control, &tx, &mode))?;
    Ok((handle, desc))
}

/// Captures sound until stopped, restarting the pipeline whenever the plan changes or it fails.
fn capture(control: &Arc<Control>, tx: &SyncSender<AudioChunk>, mode: &AudioMode) {
    let mut fails = 0;
    while !control.stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let result = plan(mode).and_then(|p| run(control, tx, mode, &p));
        if started.elapsed() >= HEALTHY_RUN {
            fails = 0;
        }
        let Err(e) = result else {
            fails = 0;
            continue;
        };
        fails += 1;
        control.report_error(format!("audio capture restarting: {e:#}"));
        // Per-app capture keeps failing: share no sound for a while (all PC audio would include
        // Discord), then try per-app again (one more failure comes straight back here).
        if fails >= FAILS_BEFORE_FALLBACK && !matches!(mode, AudioMode::All) {
            control.report_error(format!("sharing no sound for {} s, then trying {} again", FALLBACK_TIME.as_secs(), describe(mode)));
            pause(control, FALLBACK_TIME);
            fails = FAILS_BEFORE_FALLBACK - 1;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Waits `time`, or less if we're stopped.
fn pause(control: &Control, time: Duration) {
    let until = Instant::now() + time;
    while !control.stop.load(Ordering::Relaxed) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs one pipeline until the plan changes (apps start/stop) or we're
/// stopped. The pipeline is shut down however this exits.
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
            if plan(mode)? != *p {
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
            // Saved before the network side, which may drop it.
            record::audio(control, &samples);
            match tx.try_send(AudioChunk { samples, sample_rate: RATE, channels: CHANNELS }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_app_streams, AppStream};

    /// Trimmed `pw-dump` output: two app streams, our own stream, a sink and a stream with no
    /// binary name.
    const PW_DUMP: &str = r#"[
      { "id": 30, "type": "PipeWire:Interface:Node", "info": { "props": {
          "media.class": "Audio/Sink", "object.serial": 30, "node.name": "alsa_output.pci" } } },
      { "id": 81, "type": "PipeWire:Interface:Node", "info": { "props": {
          "media.class": "Stream/Output/Audio", "object.serial": 812, "application.process.id": 4242,
          "application.process.binary": "Discord", "application.name": "WEBRTC VoiceEngine" } } },
      { "id": 82, "type": "PipeWire:Interface:Node", "info": { "props": {
          "media.class": "Stream/Output/Audio", "object.serial": 820, "application.process.id": "777",
          "application.process.binary": "streamroom-live" } } },
      { "id": 83, "type": "PipeWire:Interface:Node", "info": { "props": {
          "media.class": "Stream/Output/Audio", "object.serial": 830, "application.name": "Firefox" } } },
      { "id": 84, "type": "PipeWire:Interface:Client", "info": { "props": { "application.name": "pw-dump" } } }
    ]"#;

    #[test]
    fn lists_app_output_streams_except_our_own() {
        let list = parse_app_streams(PW_DUMP.as_bytes(), "777").unwrap();
        assert_eq!(list, vec![AppStream { serial: 812, app: "discord".into() }, AppStream { serial: 830, app: "firefox".into() }]);
    }

    #[test]
    fn rejects_output_that_isnt_json() {
        assert!(parse_app_streams(b"pw-dump: can't connect", "1").is_err());
    }
}
