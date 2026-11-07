//! Linux video: the screen (Wayland portal + PipeWire, or X11) into a GStreamer pipeline that
//! scales and encodes to H.264 on the graphics card where it can:
//!   AMD / Intel: VA-API (vah264enc / vah264lpenc, or the older vaapih264enc)
//!   NVIDIA:      NVENC (nvh264enc)
//!   otherwise:   x264 (software), then OpenH264
//! Output: Annex B access units, same as the Windows encoder, for the same publishing path.

use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::launch::{launch, next_error, Pipeline, PipelineError};
use super::portal;
use crate::{Control, EncodedFrame, VideoInfo, VideoOptions};

/// Monitor index 1 on Linux = ask which screen or window every time.
pub const ASK_EVERY_TIME: u32 = 1;

#[derive(Clone, Copy, Debug)]
struct Enc {
    element: &'static str,
    label: &'static str,
    /// Runs on the GPU and wants VA surfaces from vapostproc.
    va: bool,
}

const ENCODERS: &[Enc] = &[
    Enc { element: "vah264enc", label: "VA-API H.264 (graphics card)", va: true },
    Enc { element: "vah264lpenc", label: "VA-API H.264 low power (graphics card)", va: true },
    Enc { element: "vaapih264enc", label: "VA-API H.264 (graphics card, legacy plugin)", va: false },
    Enc { element: "nvh264enc", label: "NVIDIA NVENC H.264 (graphics card)", va: false },
    Enc { element: "x264enc", label: "x264 (processor)", va: false },
    Enc { element: "openh264enc", label: "OpenH264 (processor)", va: false },
];

fn encoder_settings(e: &Enc, kbps: u32, gop: u32) -> String {
    match e.element {
        "vah264enc" | "vah264lpenc" => format!("rate-control=cbr bitrate={kbps} key-int-max={gop} b-frames=0 target-usage=7"),
        "vaapih264enc" => format!("rate-control=cbr bitrate={kbps} keyframe-period={gop} max-bframes=0 tune=low-power"),
        "nvh264enc" => format!("rc-mode=cbr bitrate={kbps} gop-size={gop} bframes=0 zerolatency=true preset=low-latency-hq"),
        "x264enc" => format!("tune=zerolatency speed-preset=veryfast bitrate={kbps} key-int-max={gop} bframes=0 vbv-buf-capacity=500"),
        _ => format!("bitrate={} gop-size={gop} rate-control=bitrate", kbps * 1000),
    }
}

fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Sets square pixels on the screen's frames (see `Screen::source`).
const SQUARE: &str = "capssetter caps=\"video/x-raw,pixel-aspect-ratio=(fraction)1/1\"";

/// The screen part of the pipeline.
enum Screen {
    Portal(portal::PortalStream),
    X11 { cursor: bool },
}

impl Screen {
    /// Ways of pointing pipewiresrc at the screen stream, best first. PipeWire versions differ:
    /// newer ones find a stream by its serial (`target-object`), older ones by node id (`path`,
    /// deprecated but still what many installs use); last resort, no target (the portal's
    /// connection only contains the shared screen).
    fn targets(&self) -> Vec<String> {
        match self {
            Screen::Portal(p) => {
                let mut t = Vec::new();
                if let Some(s) = p.serial {
                    t.push(format!("target-object={s}"));
                }
                t.push(format!("path={}", p.node));
                if p.serial.is_none() {
                    t.push(format!("target-object={}", p.node));
                }
                t.push(String::new());
                t
            }
            Screen::X11 { .. } => vec![String::new()],
        }
    }
    fn source(&self, target: &str) -> String {
        match self {
            // System-memory frames: the safest path across desktops and drivers.
            Screen::Portal(p) => format!(
                // Say this is a video stream: WirePlumber only links nodes of the same media type,
                // and recent pipewiresrc connects as "Unknown", which matches no screen
                // ("target not found" for every target; seen on GNOME, PipeWire 1.6).
                // Screens have square pixels. Overwrite whatever pixel shape the stream claims:
                // KDE's can come out as 2147483647:1, which makes every scaler overflow.
                "pipewiresrc name=src fd={} {target} \
                 stream-properties=\"props,media.type=Video,media.category=Capture,media.role=Screen\" \
                 do-timestamp=true keepalive-time=1000 resend-last=true ! {SQUARE}",
                p.fd.as_raw_fd(),
            ),
            Screen::X11 { cursor } => format!("ximagesrc name=src use-damage=false show-pointer={cursor} ! {SQUARE}"),
        }
    }
    /// The screen's shape as the portal reported it (X11: unknown until it runs).
    fn size(&self) -> Option<(u32, u32)> {
        match self {
            Screen::Portal(p) => p.size.filter(|(w, h)| *w > 0 && *h > 0).map(|(w, h)| (w as u32, h as u32)),
            Screen::X11 { .. } => None,
        }
    }
    fn method(&self) -> &'static str {
        match self {
            Screen::Portal(_) => "Wayland screen share (PipeWire)",
            Screen::X11 { .. } => "X11",
        }
    }
}

fn pipeline_text(screen: &Screen, target: &str, enc: &Enc, opts: &VideoOptions) -> String {
    let fps = opts.fps.max(1);
    let kbps = (opts.bitrate_bps / 1000).max(500);
    // Output size: when the screen's shape is known, spell out width and height (even numbers,
    // as encoders need), so nothing has to be inferred. Otherwise the height plus square pixels
    // lets the scaler pick the width that keeps the shape.
    let size = match (opts.height, screen.size()) {
        (0, _) => ",pixel-aspect-ratio=1/1".to_string(),
        (h, Some((sw, sh))) => {
            let w = ((sw as u64 * h as u64 / sh as u64) as u32 + 1) & !1;
            format!(",width={w},height={},pixel-aspect-ratio=1/1", h & !1)
        }
        (h, None) => format!(",height={h},pixel-aspect-ratio=1/1"),
    };
    let convert = if enc.va {
        format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12{size}")
    } else {
        let fmt = if enc.element == "x264enc" || enc.element == "openh264enc" { "I420" } else { "NV12" };
        format!("videoconvert ! videoscale ! video/x-raw,format={fmt}{size}")
    };
    format!(
        // The frame rate is capped by `FrameGate` at the encoder's input, not here: PipeWire
        // screens have a variable rate (0/1), so max-rate alone lets everything through, and a
        // fixed framerate in the caps makes GNOME's screen stream fail to negotiate.
        "{src} ! queue max-size-buffers=2 leaky=downstream ! videorate drop-only=true max-rate={fps} ! \
         {convert} ! \
         {enc} name=enc {settings} ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false max-buffers=4 drop=true",
        src = screen.source(target),
        enc = enc.element,
        settings = encoder_settings(enc, kbps, fps * 2),
    )
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

fn caps_size(caps: &gst::CapsRef) -> Option<(u32, u32)> {
    let s = caps.structure(0)?;
    Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
}

struct Running {
    pipeline: Pipeline,
    sink: gst_app::AppSink,
    enc: gst::Element,
    first: gst::Sample,
}

/// Lets frames through at no more than the chosen rate, by their timestamps. Screens can send
/// far more (a 165 Hz monitor sends 165 fps), which starves each frame of bitrate. Runs before
/// the encoder: dropping encoded H.264 frames would break the ones that refer to them.
#[derive(Debug)]
struct FrameGate {
    /// Nanoseconds between frames at the chosen rate.
    interval: u64,
    /// How early a frame may be and still count as on time (timestamps jitter).
    slack: u64,
    /// When the next frame is due; `None` before the first one.
    next: Option<u64>,
}

impl FrameGate {
    fn new(fps: u32) -> Self {
        let interval = 1_000_000_000 / u64::from(fps.max(1));
        Self { interval, slack: interval / 4, next: None }
    }

    /// Whether the frame stamped `pts` (nanoseconds) goes through.
    fn admit(&mut self, pts: u64) -> bool {
        let next = match self.next {
            // A timestamp well before the schedule means the clock restarted: start over.
            Some(next) if pts + self.interval + self.slack < next => pts + self.interval,
            Some(next) if pts + self.slack < next => return false,
            // After a pause (a still screen sends nothing), don't let frames burst to catch up.
            Some(next) if pts > next + self.interval => pts + self.interval,
            Some(next) => next + self.interval,
            None => pts + self.interval,
        };
        self.next = Some(next);
        true
    }
}

/// Drops raw frames that arrive sooner than `fps` allows, at the encoder's input.
fn cap_frame_rate(enc: &gst::Element, fps: u32) -> Result<()> {
    let pad = enc.static_pad("sink").ok_or_else(|| anyhow!("the encoder has no input"))?;
    let gate = Mutex::new(FrameGate::new(fps));
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
        // A poisoned lock only means an earlier call panicked; the gate's state is still usable.
        let mut gate = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if gate.admit(pts.nseconds()) {
            gst::PadProbeReturn::Ok
        } else {
            gst::PadProbeReturn::Drop
        }
    })
    .ok_or_else(|| anyhow!("couldn't watch the encoder's input"))?;
    Ok(())
}

/// Starts a pipeline and waits for its first encoded frame (or an error). On failure the
/// pipeline is shut down as it's dropped.
fn try_start(text: &str, fps: u32, control: &Arc<Control>) -> Result<Running> {
    let (pipeline, sink) = launch(text, "video", control)?;
    let enc = pipeline.by_name("enc").ok_or_else(|| anyhow!("the video pipeline has no encoder"))?;
    cap_frame_rate(&enc, fps).context("capping the frame rate")?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("the video pipeline has no message bus"))?;
    if pipeline.set_state(gst::State::Playing).is_err() {
        // The real reason is on the bus: which element refused, and why.
        let Some(why) = next_error(&bus, gst::ClockTime::from_seconds(1)) else {
            bail!("starting the video pipeline: an element refused to start (no details)");
        };
        return Err(anyhow::Error::new(why).context("starting the video pipeline"));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(why) = next_error(&bus, gst::ClockTime::ZERO) {
            return Err(why.into());
        }
        if let Some(first) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            return Ok(Running { pipeline, sink, enc, first });
        }
        if Instant::now() > deadline {
            bail!("no video came through in 10 seconds");
        }
    }
}

/// PipeWire couldn't find the screen stream the way this pipeline named it.
fn is_target_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<PipelineError>().is_some_and(PipelineError::target_not_found)
}

/// A node PipeWire shows on the portal's connection.
struct Node {
    name: String,
    class: String,
    id: String,
    serial: String,
}

/// The nodes on the portal's connection (the shared screen should be listed).
fn portal_nodes(p: &portal::PortalStream) -> std::result::Result<Vec<Node>, &'static str> {
    let provider = gst::DeviceProviderFactory::by_name("pipewiredeviceprovider").ok_or("no PipeWire device provider")?;
    let fd = p.fd.try_clone().map_err(|_| "fd")?;
    if provider.find_property("fd").is_none() {
        return Err("device provider has no fd");
    }
    provider.set_property("fd", fd.as_raw_fd());
    provider.start().map_err(|_| "couldn't connect")?;
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
/// (GNOME doesn't).
fn lookup_serial(p: &portal::PortalStream) -> Option<u64> {
    let node = p.node.to_string();
    portal_nodes(p).ok()?.into_iter().find(|n| n.id == node)?.serial.parse().ok()
}

/// What PipeWire shows on the portal's connection, for troubleshooting.
fn visible_nodes(p: &portal::PortalStream) -> String {
    match portal_nodes(p) {
        Err(e) => format!("can't check: {e}"),
        Ok(list) if list.is_empty() => "nothing".into(),
        Ok(list) => list.iter().map(|n| format!("{} [{}; id {}; serial {}]", n.name, n.class, n.id, n.serial)).collect::<Vec<_>>().join(", "),
    }
}

/// Tries each available encoder in order; the first that produces video wins.
fn start_encoders(
    screen: &Screen,
    opts: &VideoOptions,
    control: &Arc<Control>,
) -> std::result::Result<(Running, Enc, Vec<String>, String), Vec<String>> {
    let mut tried = Vec::new();
    let mut targets = screen.targets();
    for enc in ENCODERS.iter().filter(|e| available(e.element)) {
        if enc.va && !available("vapostproc") {
            continue;
        }
        // "target not found" means this PipeWire wants the stream named another way: move on
        // to the next way (for this and the following encoders).
        loop {
            let target = targets[0].clone();
            match try_start(&pipeline_text(screen, &target, enc, opts), opts.fps, control) {
                Ok(r) => {
                    return Ok((r, *enc, tried, target));
                }
                Err(e) => {
                    let label = if target.is_empty() { "no target".to_string() } else { target.clone() };
                    tried.push(format!("{} [{label}]: {e:#}", enc.element));
                    if is_target_not_found(&e) && targets.len() > 1 {
                        targets.remove(0);
                        continue;
                    }
                    break;
                }
            }
        }
    }
    Err(tried)
}

pub fn start_video(opts: VideoOptions, control: Arc<Control>, tx: SyncSender<EncodedFrame>) -> Result<(VideoInfo, std::thread::JoinHandle<()>)> {
    gst::init().context("GStreamer")?;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let use_portal = wayland || std::env::var_os("DISPLAY").is_none();
    if use_portal && !available("pipewiresrc") {
        bail!("GStreamer's PipeWire plugin is missing (install gst-plugin-pipewire)");
    }
    let open_screen = |remember: bool| -> Result<Screen> {
        Ok(if use_portal {
            let mut p = portal::open(control.clone(), opts.cursor, remember)?;
            if p.serial.is_none() {
                p.serial = lookup_serial(&p);
            }
            Screen::Portal(p)
        } else {
            Screen::X11 { cursor: opts.cursor }
        })
    };

    let mut screen = open_screen(opts.monitor != ASK_EVERY_TIME)?;
    let mut result = start_encoders(&screen, &opts, &control);
    // A remembered screen choice can go stale (the desktop restores a session we can't read):
    // forget it and ask once more before giving up.
    if result.is_err() && matches!(&screen, Screen::Portal(p) if p.restored) {
        let first = result.err().unwrap_or_default();
        drop(screen); // closes that portal session
        if let Err(e) = portal::forget() {
            control.report_error(format!("couldn't forget the remembered screen: {e:#}"));
        }
        screen = open_screen(false)?;
        result = start_encoders(&screen, &opts, &control).map_err(|mut t| {
            t.insert(0, format!("with the remembered screen: {}", first.join("; ")));
            t
        });
    }
    let (run, enc, tried, target) = match result {
        Ok(r) => r,
        Err(tried) => {
            // What the desktop handed over, for troubleshooting.
            let detail = match &screen {
                Screen::Portal(p) => {
                    let fd = p.fd.as_raw_fd();
                    let open = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                        .map(|l| l.display().to_string())
                        .unwrap_or_else(|e| format!("not open: {e}"));
                    format!(
                        "\n(portal stream: node {}, serial {:?}, size {:?}, fd {fd} = {open})\n(visible on that connection: {})",
                        p.node,
                        p.serial,
                        p.size,
                        visible_nodes(p)
                    )
                }
                Screen::X11 { .. } => String::new(),
            };
            drop(screen);
            if tried.is_empty() {
                bail!("no H.264 encoder found. Install gst-plugin-va (AMD/Intel) or gst-plugins-ugly (x264)");
            }
            bail!("couldn't start video with any encoder:\n{}{detail}", tried.join("\n"));
        }
    };

    let out = run.first.caps().and_then(caps_size).unwrap_or((0, 0));
    let src = run
        .pipeline
        .by_name("src")
        .and_then(|s| s.static_pad("src"))
        .and_then(|p| p.current_caps())
        .and_then(|c| caps_size(&c))
        .or(match &screen {
            Screen::Portal(p) => p.size.map(|(w, h)| (w as u32, h as u32)),
            _ => None,
        })
        .unwrap_or(out);
    let info = VideoInfo {
        adapter: if enc.label.contains("graphics card") { "graphics card".into() } else { "processor".into() },
        encoder: enc.label.into(),
        capture_method: if target.is_empty() { screen.method().into() } else { format!("{} ({target})", screen.method()) },
        source_width: src.0,
        source_height: src.1,
        width: out.0,
        height: out.1,
        fps: opts.fps,
        unsupported_settings: tried,
    };

    let handle = std::thread::Builder::new().name("video".into()).spawn(move || {
        let Running { pipeline, sink, enc: enc_el, first } = run;
        let bus = pipeline.bus();
        let mut next = Some(first);
        while !control.stop.load(Ordering::Relaxed) {
            if let Some(msg) = bus.as_ref().and_then(|b| b.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])) {
                let why = match PipelineError::from_message(&msg) {
                    Some(e) => format!("video stopped: {}", e.message),
                    None => "video stopped: the screen share ended".into(),
                };
                control.report_error(why);
                control.stop.store(true, Ordering::Relaxed);
                break;
            }
            if control.keyframe_requested.swap(false, Ordering::Relaxed) {
                let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
                sink.send_event(ev);
            }
            let bps = control.bitrate_request.swap(0, Ordering::Relaxed);
            if bps > 0 && enc_el.has_property("bitrate") {
                let v = if enc.element == "openh264enc" { bps } else { bps / 1000 };
                enc_el.set_property_from_str("bitrate", &v.to_string());
            }
            let Some(sample) = next.take().or_else(|| sink.try_pull_sample(gst::ClockTime::from_mseconds(50))) else {
                continue;
            };
            let Some(buf) = sample.buffer() else { continue };
            let Ok(map) = buf.map_readable() else { continue };
            let keyframe = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let (width, height) = sample.caps().and_then(caps_size).unwrap_or(out);
            let s = &control.stats;
            s.captured.fetch_add(1, Ordering::Relaxed);
            s.submitted.fetch_add(1, Ordering::Relaxed);
            s.encoded.fetch_add(1, Ordering::Relaxed);
            s.bytes.fetch_add(map.len() as u64, Ordering::Relaxed);
            if keyframe {
                s.keyframes.fetch_add(1, Ordering::Relaxed);
            }
            let frame = EncodedFrame { data: map.to_vec(), keyframe, width, height, capture_unix_us: now_us(), encode_latency_us: 0 };
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    s.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        // The screen share (portal fd) stays open until here; stop the pipeline reading it first.
        drop(pipeline);
        drop(screen);
        control.stop.store(true, Ordering::Relaxed);
    })?;
    Ok((info, handle))
}

/// Linux monitors: Wayland lets the desktop's dialog pick the screen, so the choice here is
/// whether to remember it.
pub fn list_monitors() -> Vec<crate::Monitor> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland {
        vec![
            crate::Monitor { index: 0, name: "Screen you picked (remembered)".into(), width: 0, height: 0, primary: true, gpu: String::new() },
            crate::Monitor { index: ASK_EVERY_TIME, name: "Ask every time".into(), width: 0, height: 0, primary: false, gpu: String::new() },
        ]
    } else {
        vec![crate::Monitor { index: 0, name: "Whole desktop".into(), width: 0, height: 0, primary: true, gpu: String::new() }]
    }
}

#[cfg(test)]
mod tests {
    use super::FrameGate;

    const SEC: u64 = 1_000_000_000;

    /// How many of `frames` timestamps the gate lets through.
    fn admitted(fps: u32, frames: impl Iterator<Item = u64>) -> usize {
        let mut gate = FrameGate::new(fps);
        frames.filter(|&pts| gate.admit(pts)).count()
    }

    #[test]
    fn caps_a_165_hz_screen_to_60_fps() {
        let n = admitted(60, (0..165 * 2).map(|i| i * SEC / 165));
        assert!((118..=121).contains(&n), "{n} frames in 2 s");
    }

    #[test]
    fn keeps_every_frame_of_a_jittery_60_hz_screen() {
        // ±2 ms around each 16.7 ms tick.
        let jitter = [0i64, 2_000_000, -2_000_000, 1_000_000, -1_500_000];
        let frames = (0..120u64).map(|i| (i * SEC / 60 + 10_000_000).saturating_add_signed(jitter[i as usize % jitter.len()]));
        assert_eq!(admitted(60, frames), 120);
    }

    #[test]
    fn halves_a_60_hz_screen_capped_at_30() {
        let n = admitted(30, (0..120).map(|i| i * SEC / 60));
        assert!((59..=61).contains(&n), "{n} frames in 2 s");
    }

    #[test]
    fn no_burst_after_a_still_screen() {
        let mut gate = FrameGate::new(60);
        assert!(gate.admit(0));
        // Nothing for 5 s, then 165 Hz again: still at most ~60 in the next second.
        let n = (0..165).map(|i| 5 * SEC + i * SEC / 165).filter(|&pts| gate.admit(pts)).count();
        assert!((59..=61).contains(&n), "{n} frames in 1 s");
    }

    #[test]
    fn starts_over_when_timestamps_go_back() {
        let mut gate = FrameGate::new(60);
        assert!(gate.admit(10 * SEC));
        assert!(gate.admit(0));
        assert!(!gate.admit(SEC / 165));
        assert!(gate.admit(SEC / 60));
    }
}
