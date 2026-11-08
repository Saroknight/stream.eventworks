//! Linux video: the screen (Wayland portal + PipeWire, or X11) into a GStreamer pipeline that
//! scales and encodes to H.264 on the graphics card where it can:
//!   AMD / Intel: VA-API (vah264enc / vah264lpenc, or the older vaapih264enc)
//!   NVIDIA:      NVENC (nvh264enc)
//!   otherwise:   x264 (software), then OpenH264
//! Output: Annex B access units, same as the Windows encoder, for the same publishing path.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::encoder::Encoder;
use super::gate::FrameGate;
use super::launch::{available, launch, next_error, Pipeline, PipelineError};
use super::portal;
use super::record;
use crate::{Control, EncodedFrame, VideoInfo, VideoOptions};

/// Monitor index 1 on Linux = ask which screen or window every time.
pub const ASK_EVERY_TIME: u32 = 1;

/// Wayland, or no X display either: the screen comes from the desktop's screen-share portal.
fn use_portal() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_none()
}

/// Going live may show the desktop's "choose a screen or window" dialog (the Wayland portal).
pub fn screen_dialog() -> bool {
    use_portal()
}

/// Sets square pixels on the screen's frames (see `Screen::source`).
const SQUARE: &str = "capssetter caps=\"video/x-raw,pixel-aspect-ratio=(fraction)1/1\"";

/// The screen part of the pipeline.
enum Screen {
    Portal(portal::PortalStream),
    /// `size`: the X screen's, when it could be read before starting.
    X11 {
        cursor: bool,
        size: Option<(u32, u32)>,
    },
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
    fn source(&self, target: &str, fps: u32) -> String {
        match self {
            // No memory type is set here, so the encoder path picks it: `vapostproc` (vah264enc)
            // accepts DMA-BUF frames, so they stay on the graphics card (GNOME + AMD: XR24 with a
            // tiling modifier); `videoconvert` (the other encoders) only takes system memory,
            // which PipeWire then sends instead.
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
            // ximagesrc runs at a fixed rate, 25 fps unless told otherwise.
            Screen::X11 { cursor, .. } => {
                format!("ximagesrc name=src use-damage=false show-pointer={cursor} ! video/x-raw,framerate={fps}/1 ! {SQUARE}")
            }
        }
    }
    /// The screen's shape, as the portal reported it or as X11 says.
    fn size(&self) -> Option<(u32, u32)> {
        match self {
            Screen::Portal(p) => p.size.filter(|(w, h)| *w > 0 && *h > 0).map(|(w, h)| (w as u32, h as u32)),
            Screen::X11 { size, .. } => size.filter(|(w, h)| *w > 0 && *h > 0),
        }
    }
    fn method(&self) -> &'static str {
        match self {
            Screen::Portal(_) => "Wayland screen share (PipeWire)",
            Screen::X11 { .. } => "X11",
        }
    }
}

fn pipeline_text(screen: &Screen, target: &str, enc: Encoder, opts: &VideoOptions) -> String {
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
    format!(
        // The frame rate is capped by `FrameGate` at the encoder's input, not here: PipeWire
        // screens have a variable rate (0/1), so videorate's max-rate lets everything through,
        // and a fixed framerate in the caps makes GNOME's screen stream fail to negotiate.
        // The encoder still has to be told the rate: it splits its bitrate by the frame rate in
        // the caps, and a screen's variable rate comes with a max-framerate (about 360 fps on
        // GNOME), so each frame got a sixth of its share (16 Mbps came out as 1.5). capssetter
        // only changes what goes downstream, so it doesn't upset negotiation with the screen.
        "{src} ! queue max-size-buffers=2 leaky=downstream ! \
         {convert} ! \
         capssetter caps=\"video/x-raw,framerate=(fraction){fps}/1\" ! \
         {element} name=enc {settings} ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false max-buffers=4 drop=true",
        src = screen.source(target, fps),
        convert = enc.convert(&size),
        element = enc.element(),
        settings = enc.settings(kbps, fps * 2),
    )
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

fn caps_size(caps: &gst::CapsRef) -> Option<(u32, u32)> {
    let s = caps.structure(0)?;
    Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
}

/// A pipeline that has produced its first encoded frame.
struct Running {
    pipeline: Pipeline,
    sink: gst_app::AppSink,
    enc: gst::Element,
    first: gst::Sample,
}

/// What `start_encoders` settled on.
struct Started {
    run: Running,
    encoder: Encoder,
    /// The pipeline's target for the screen stream ("" = none).
    target: String,
    /// What failed before this worked.
    tried: Vec<String>,
}

/// Counts the frames leaving `element` into one of the `Stats` counters.
fn count_frames(element: &gst::Element, control: &Arc<Control>, counter: fn(&crate::Stats) -> &AtomicU64) -> Result<()> {
    let pad = element.static_pad("src").ok_or_else(|| anyhow!("{} has no output", element.name()))?;
    let control = control.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter(&control.stats).fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    })
    .ok_or_else(|| anyhow!("couldn't watch {}", element.name()))?;
    Ok(())
}

/// Drops raw frames that arrive sooner than `fps` allows, at the encoder's input, and counts
/// the ones it lets through.
fn cap_frame_rate(enc: &gst::Element, fps: u32, control: &Arc<Control>) -> Result<()> {
    let pad = enc.static_pad("sink").ok_or_else(|| anyhow!("the encoder has no input"))?;
    let gate = Mutex::new(FrameGate::new(fps));
    let control = control.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()) else { return gst::PadProbeReturn::Ok };
        // A poisoned lock only means an earlier call panicked; the gate's state is still usable.
        let mut gate = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if gate.admit(pts.nseconds()) {
            control.stats.submitted.fetch_add(1, Ordering::Relaxed);
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
    cap_frame_rate(&enc, fps, control).context("capping the frame rate")?;
    // Every frame the screen sends, before the frame-rate cap: "fps captured" in the app.
    let src = pipeline.by_name("src").ok_or_else(|| anyhow!("the video pipeline has no screen source"))?;
    count_frames(&src, control, |s| &s.captured).context("counting captured frames")?;
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

/// Tries each available encoder in order; the first that produces video wins. On failure,
/// what was tried and why each failed.
fn start_encoders(screen: &Screen, opts: &VideoOptions, control: &Arc<Control>) -> std::result::Result<Started, Vec<String>> {
    let mut tried = Vec::new();
    let targets = screen.targets();
    let mut t = 0;
    for encoder in Encoder::ALL.into_iter().filter(|e| e.available()) {
        // "target not found" means this PipeWire wants the stream named another way: move on
        // to the next way (for this and the following encoders).
        // `targets` always ends with "no target", and `t` only moves on while there's another.
        while let Some(target) = targets.get(t) {
            match try_start(&pipeline_text(screen, target, encoder, opts), opts.fps, control) {
                Ok(run) => return Ok(Started { run, encoder, target: target.clone(), tried }),
                Err(e) => {
                    let label = if target.is_empty() { "no target" } else { target };
                    tried.push(format!("{} [{label}]: {e:#}", encoder.element()));
                    if is_target_not_found(&e) && t + 1 < targets.len() {
                        t += 1;
                        continue;
                    }
                    break;
                }
            }
        }
    }
    Err(tried)
}

fn open_screen(control: &Arc<Control>, opts: &VideoOptions, remember: bool) -> Result<Screen> {
    if !use_portal() {
        return Ok(Screen::X11 { cursor: opts.cursor, size: x11_size(control) });
    }
    let mut p = portal::open(control.clone(), opts.cursor, remember)?;
    if p.serial.is_none() {
        p.serial = portal::lookup_serial(&p, control);
    }
    Ok(Screen::Portal(p))
}

/// The X screen's size: ximagesrc reports it in its caps once it has opened the display.
/// Without it the scaler picks the output width, which can be odd (1365x720 from a 3640x1920
/// desktop), and x264 refuses odd widths.
fn x11_size(control: &Control) -> Option<(u32, u32)> {
    let src = gst::ElementFactory::make("ximagesrc").build().ok()?;
    // A live source: Paused opens the display without waiting for a frame.
    let size = src.set_state(gst::State::Paused).ok().and_then(|_| src.static_pad("src")).and_then(|pad| caps_size(&pad.query_caps(None)));
    if let Err(e) = src.set_state(gst::State::Null) {
        control.report_error(format!("stopping the X11 screen size check: {e}"));
    }
    size
}

/// What the desktop handed over, for troubleshooting a failed start.
fn failure_detail(screen: &Screen) -> String {
    match screen {
        Screen::Portal(p) => {
            let fd = p.fd.as_raw_fd();
            let open =
                std::fs::read_link(format!("/proc/self/fd/{fd}")).map(|l| l.display().to_string()).unwrap_or_else(|e| format!("not open: {e}"));
            format!(
                "\n(portal stream: node {}, serial {:?}, size {:?}, fd {fd} = {open})\n(visible on that connection: {})",
                p.node,
                p.serial,
                p.size,
                portal::visible_nodes(p)
            )
        }
        Screen::X11 { .. } => String::new(),
    }
}

/// Opens the screen and starts the first encoder that works. A remembered screen choice can go
/// stale (the desktop restores a session we can't read): then it's forgotten and the desktop
/// asks once more before giving up.
fn start_screen(opts: &VideoOptions, control: &Arc<Control>) -> Result<(Screen, Started)> {
    let mut screen = open_screen(control, opts, opts.monitor != ASK_EVERY_TIME)?;
    let mut result = start_encoders(&screen, opts, control);
    if let Err(first) = &result {
        if matches!(&screen, Screen::Portal(p) if p.restored) {
            let first = first.join("; ");
            drop(screen); // closes that portal session
            if let Err(e) = portal::forget() {
                control.report_error(format!("couldn't forget the remembered screen: {e:#}"));
            }
            screen = open_screen(control, opts, false)?;
            result = start_encoders(&screen, opts, control).map_err(|mut t| {
                t.insert(0, format!("with the remembered screen: {first}"));
                t
            });
        }
    }
    match result {
        Ok(started) => Ok((screen, started)),
        Err(tried) if tried.is_empty() => bail!("no H.264 encoder found. Install gst-plugin-va (AMD/Intel) or gst-plugins-ugly (x264)"),
        Err(tried) => bail!("couldn't start video with any encoder:\n{}{}", tried.join("\n"), failure_detail(&screen)),
    }
}

fn video_info(screen: &Screen, started: &Started, fps: u32) -> VideoInfo {
    let run = &started.run;
    let out = run.first.caps().and_then(caps_size).unwrap_or((0, 0));
    let src = run
        .pipeline
        .by_name("src")
        .and_then(|s| s.static_pad("src"))
        .and_then(|p| p.current_caps())
        .and_then(|c| caps_size(&c))
        .or_else(|| screen.size())
        .unwrap_or(out);
    let target = &started.target;
    VideoInfo {
        adapter: if started.encoder.on_gpu() { "graphics card".into() } else { "processor".into() },
        encoder: started.encoder.label().into(),
        capture_method: if target.is_empty() { screen.method().into() } else { format!("{} ({target})", screen.method()) },
        source_width: src.0,
        source_height: src.1,
        width: out.0,
        height: out.1,
        fps,
        unsupported_settings: started.tried.clone(),
    }
}

pub fn start_video(opts: VideoOptions, control: Arc<Control>, tx: SyncSender<EncodedFrame>) -> Result<(VideoInfo, std::thread::JoinHandle<()>)> {
    gst::init().context("GStreamer")?;
    if use_portal() && !available("pipewiresrc") {
        bail!("GStreamer's PipeWire plugin is missing (install gst-plugin-pipewire)");
    }
    let (screen, started) = start_screen(&opts, &control)?;
    let info = video_info(&screen, &started, opts.fps);
    let out = (info.width, info.height);
    let handle = std::thread::Builder::new().name("video".into()).spawn(move || {
        stream(started.run, started.encoder, &control, &tx, out);
        // The screen share (portal fd) stays open until here, after `stream` has stopped and
        // dropped the pipeline reading it.
        drop(screen);
        control.stop.store(true, Ordering::Relaxed);
    })?;
    Ok((info, handle))
}

/// Hands encoded frames to `tx` until stopped, the screen share ends, or nobody is receiving.
fn stream(run: Running, encoder: Encoder, control: &Control, tx: &SyncSender<EncodedFrame>, out: (u32, u32)) {
    let Running { pipeline, sink, enc, first } = run;
    let bus = pipeline.bus();
    let mut next = Some(first);
    let mut keyframe_after_drop = KeyframeLimit::default();
    while !control.stop.load(Ordering::Relaxed) {
        if let Some(msg) = bus.as_ref().and_then(|b| b.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])) {
            let why = match PipelineError::from_message(&msg) {
                Some(e) => e.message,
                None => "the screen share ended".into(),
            };
            control.report_stopped(why);
            return;
        }
        apply_requests(control, &sink, &enc, encoder);
        let Some(sample) = next.take().or_else(|| sink.try_pull_sample(gst::ClockTime::from_mseconds(50))) else { continue };
        match send(&sample, &pipeline, control, tx, out) {
            Sent::Delivered => {}
            // Dropping a frame breaks the decoder's chain, so ask for a fresh keyframe (as on
            // Windows), but not on every drop: keyframes are big and the network is behind.
            Sent::Dropped => {
                if keyframe_after_drop.allow(Instant::now()) {
                    control.request_keyframe();
                }
            }
            Sent::Closed => return,
        }
    }
}

/// Drops ask for a keyframe at most this often.
const KEYFRAME_AFTER_DROP_GAP: Duration = Duration::from_secs(1);

/// Spaces out keyframe requests caused by dropped frames.
#[derive(Default)]
struct KeyframeLimit {
    last: Option<Instant>,
}

impl KeyframeLimit {
    /// Whether a drop at `now` may ask for a keyframe (and if so, counts it as asked).
    fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_AFTER_DROP_GAP) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// Keyframe and bitrate requests from the network side.
fn apply_requests(control: &Control, sink: &gst_app::AppSink, enc: &gst::Element, encoder: Encoder) {
    if control.keyframe_requested.swap(false, Ordering::Relaxed) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        sink.send_event(ev);
    }
    let bps = control.bitrate_request.swap(0, Ordering::Relaxed);
    if bps == 0 {
        return;
    }
    // Logged so a soft stream can be told apart from a slow network: the network side's
    // bandwidth estimate sets the bitrate (up to the chosen one). Only changes over 10% get here,
    // so a few lines while it ramps up, then the odd one.
    let applied = enc.has_property("bitrate");
    tracing::info!(kbps = bps / 1000, applied, "network side set the video bitrate");
    if applied {
        enc.set_property_from_str("bitrate", &encoder.bitrate_value(bps).to_string());
    }
}

/// How long ago the encoded frame in `sample` was captured. The screen source stamps each
/// frame with the pipeline's running time as it arrives (`do-timestamp` on pipewiresrc;
/// ximagesrc always does). Encoders shift their output's timestamps (by 1000 hours, so decode
/// times never go negative), so the stamp is turned back into running time with the sample's
/// segment.
fn since_capture(sample: &gst::Sample, pipeline: &gst::Pipeline) -> Option<Duration> {
    let pts = sample.buffer()?.pts()?;
    let captured = sample.segment()?.downcast_ref::<gst::ClockTime>()?.to_running_time(pts)?;
    let age = pipeline.current_running_time()?.checked_sub(captured)?;
    Some(Duration::from_nanos(age.nseconds()))
}

/// What became of an encoded frame.
enum Sent {
    /// Handed on (or there was nothing to hand on).
    Delivered,
    /// Dropped: the network side is behind.
    Dropped,
    /// Nobody is receiving any more.
    Closed,
}

/// Sends one encoded frame on.
fn send(sample: &gst::Sample, pipeline: &gst::Pipeline, control: &Control, tx: &SyncSender<EncodedFrame>, out: (u32, u32)) -> Sent {
    let Some(buf) = sample.buffer() else { return Sent::Delivered };
    let Ok(map) = buf.map_readable() else { return Sent::Delivered };
    let keyframe = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
    let (width, height) = sample.caps().and_then(caps_size).unwrap_or(out);
    let s = &control.stats;
    s.encoded.fetch_add(1, Ordering::Relaxed);
    s.bytes.fetch_add(map.len() as u64, Ordering::Relaxed);
    if keyframe {
        s.keyframes.fetch_add(1, Ordering::Relaxed);
    }
    let latency = since_capture(sample, pipeline).unwrap_or_default();
    let latency_us = u32::try_from(latency.as_micros()).unwrap_or(u32::MAX);
    s.last_latency_us.store(latency_us, Ordering::Relaxed);
    let capture_unix_us = now_us() - i64::from(latency_us);
    // Saved before the network side, which may drop it (later frames would then not decode).
    record::video(control, &map, keyframe, Instant::now().checked_sub(latency).unwrap_or_else(Instant::now));
    let frame = EncodedFrame { data: map.to_vec(), keyframe, width, height, capture_unix_us, encode_latency_us: latency_us };
    match tx.try_send(frame) {
        Ok(()) => Sent::Delivered,
        Err(TrySendError::Full(_)) => {
            s.dropped.fetch_add(1, Ordering::Relaxed);
            Sent::Dropped
        }
        Err(TrySendError::Disconnected(_)) => Sent::Closed,
    }
}

/// Linux monitors: on the portal the desktop's dialog picks the screen, so the choice here is
/// whether to remember it.
pub fn list_monitors() -> Vec<crate::Monitor> {
    if use_portal() {
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
    use super::{pipeline_text, Encoder, KeyframeLimit, Screen, KEYFRAME_AFTER_DROP_GAP};
    use std::time::{Duration, Instant};

    /// The encoder splits its bitrate by the frame rate in the caps. Screens report a variable
    /// rate (0/1) with a max-framerate (about 360 fps seen on GNOME), and the encoder used that:
    /// at 60 fps delivered, 16 Mbps came out as 1.5 Mbps.
    #[test]
    fn the_encoder_is_told_the_capped_frame_rate() {
        let screen = Screen::X11 { cursor: true, size: Some((2560, 1440)) };
        let opts = crate::VideoOptions { fps: 60, ..Default::default() };
        for encoder in Encoder::ALL {
            let text = pipeline_text(&screen, "", encoder, &opts);
            let before_encoder = text.split(" name=enc ").next().unwrap_or_default();
            let last_link = before_encoder.rsplit('!').nth(1).unwrap_or_default();
            assert!(last_link.contains("framerate=(fraction)60/1"), "{encoder:?}: {last_link}");
        }
    }

    #[test]
    fn drops_ask_for_a_keyframe_at_most_once_a_gap() {
        let start = Instant::now();
        let mut limit = KeyframeLimit::default();
        assert!(limit.allow(start), "the first drop asks");
        assert!(!limit.allow(start + Duration::from_millis(10)), "a drop right after doesn't");
        assert!(!limit.allow(start + KEYFRAME_AFTER_DROP_GAP - Duration::from_millis(1)));
        assert!(limit.allow(start + KEYFRAME_AFTER_DROP_GAP), "one gap later it asks again");
        assert!(!limit.allow(start + KEYFRAME_AFTER_DROP_GAP + Duration::from_millis(500)), "counted from the last ask");
    }
}
