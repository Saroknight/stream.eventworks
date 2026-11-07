//! Linux video: the screen (Wayland portal + PipeWire, or X11) into a GStreamer pipeline that
//! scales and encodes to H.264 on the graphics card where it can:
//!   AMD / Intel: VA-API (vah264enc / vah264lpenc, or the older vaapih264enc)
//!   NVIDIA:      NVENC (nvh264enc)
//!   otherwise:   x264 (software), then OpenH264
//! Output: Annex B access units, same as the Windows encoder, for the same publishing path.

use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

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
    fn source(&self) -> String {
        match self {
            // System-memory frames: the safest path across desktops and drivers.
            Screen::Portal(p) => format!(
                // Screens have square pixels. Overwrite whatever pixel shape the stream claims:
                // KDE's can come out as 2147483647:1, which makes every scaler overflow.
                "pipewiresrc name=src fd={} path={} do-timestamp=true keepalive-time=1000 resend-last=true ! {SQUARE}",
                p.fd.as_raw_fd(),
                p.node
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

fn pipeline_text(screen: &Screen, enc: &Enc, opts: &VideoOptions) -> String {
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
        "{src} ! queue max-size-buffers=2 leaky=downstream ! videorate drop-only=true max-rate={fps} ! {convert} ! \
         {enc} name=enc {settings} ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false max-buffers=4 drop=true",
        src = screen.source(),
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
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    enc: gst::Element,
    first: gst::Sample,
}

/// Starts a pipeline and waits for its first encoded frame (or an error).
fn try_start(text: &str) -> Result<Running> {
    let pipeline = gst::parse::launch(text)
        .context("building the video pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or_else(|| anyhow!("no sink"))?;
    let enc = pipeline.by_name("enc").ok_or_else(|| anyhow!("no encoder"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    if pipeline.set_state(gst::State::Playing).is_err() {
        // The real reason is on the bus: which element refused, and why.
        let why = bus
            .timed_pop_filtered(gst::ClockTime::from_seconds(1), &[gst::MessageType::Error])
            .and_then(|m| match m.view() {
                gst::MessageView::Error(e) => Some(format!(
                    "{}: {} ({})",
                    e.src().map(|s| s.path_string().to_string()).unwrap_or_default(),
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                )),
                _ => None,
            })
            .unwrap_or_else(|| "an element refused to start (no details)".into());
        let _ = pipeline.set_state(gst::State::Null);
        bail!("starting the video pipeline: {why}");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            let _ = pipeline.set_state(gst::State::Null);
            if let gst::MessageView::Error(e) = msg.view() {
                bail!("{} ({})", e.error(), e.debug().map(|d| d.to_string()).unwrap_or_default());
            }
        }
        if let Some(first) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            return Ok(Running { pipeline, sink, enc, first });
        }
        if Instant::now() > deadline {
            let _ = pipeline.set_state(gst::State::Null);
            bail!("no video came through in 10 seconds");
        }
    }
}

/// Tries each available encoder in order; the first that produces video wins.
fn start_encoders(screen: &Screen, opts: &VideoOptions) -> std::result::Result<(Running, Enc, Vec<String>), Vec<String>> {
    let mut tried = Vec::new();
    for enc in ENCODERS.iter().filter(|e| available(e.element)) {
        if enc.va && !available("vapostproc") {
            continue;
        }
        match try_start(&pipeline_text(screen, enc, opts)) {
            Ok(r) => return Ok((r, *enc, tried)),
            Err(e) => tried.push(format!("{}: {e:#}", enc.element)),
        }
    }
    Err(tried)
}

pub fn start_video(
    opts: VideoOptions,
    control: Arc<Control>,
    tx: SyncSender<EncodedFrame>,
) -> Result<(VideoInfo, std::thread::JoinHandle<()>)> {
    gst::init().context("GStreamer")?;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let use_portal = wayland || std::env::var_os("DISPLAY").is_none();
    if use_portal && !available("pipewiresrc") {
        bail!("GStreamer's PipeWire plugin is missing (install gst-plugin-pipewire)");
    }
    let open_screen = |remember: bool| -> Result<Screen> {
        Ok(if use_portal {
            Screen::Portal(portal::open(control.clone(), opts.cursor, remember)?)
        } else {
            Screen::X11 { cursor: opts.cursor }
        })
    };

    let mut screen = open_screen(opts.monitor != ASK_EVERY_TIME)?;
    let mut result = start_encoders(&screen, &opts);
    // A remembered screen choice can go stale (the desktop restores a session we can't read):
    // forget it and ask once more before giving up.
    if result.is_err() && matches!(&screen, Screen::Portal(p) if p.restored) {
        let first = result.err().unwrap_or_default();
        drop(screen); // closes that portal session
        portal::forget();
        screen = open_screen(false)?;
        result = start_encoders(&screen, &opts).map_err(|mut t| {
            t.insert(0, format!("with the remembered screen: {}", first.join("; ")));
            t
        });
    }
    let (run, enc, tried) = match result {
        Ok(r) => r,
        Err(tried) => {
            drop(screen);
            if tried.is_empty() {
                bail!("no H.264 encoder found. Install gst-plugin-va (AMD/Intel) or gst-plugins-ugly (x264)");
            }
            bail!("couldn't start video with any encoder:\n{}", tried.join("\n"));
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
        capture_method: screen.method().into(),
        source_width: src.0,
        source_height: src.1,
        width: out.0,
        height: out.1,
        fps: opts.fps,
        unsupported_settings: tried,
    };

    let handle = std::thread::Builder::new().name("video".into()).spawn(move || {
        let Running { pipeline, sink, enc: enc_el, first } = run;
        let _screen = screen; // keeps the portal fd open
        let bus = pipeline.bus();
        let mut next = Some(first);
        while !control.stop.load(Ordering::Relaxed) {
            if let Some(msg) = bus.as_ref().and_then(|b| b.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])) {
                let why = match msg.view() {
                    gst::MessageView::Error(e) => format!("video stopped: {}", e.error()),
                    _ => "video stopped: the screen share ended".into(),
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
        let _ = pipeline.set_state(gst::State::Null);
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
