//! The video thread: capture -> convert -> encode, paced to the target frame rate.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use crate::capture::{Duplicator, Grab};
use crate::convert::{texture_desc, Converter};
use crate::encoder::{EncodedOut, Encoder};
use crate::{Control, EncodedFrame, VideoInfo, VideoOptions};

/// A still screen produces no new desktop images; resend the last one this often so new
/// viewers and keyframe requests still get a picture.
const STILL_RESEND: Duration = Duration::from_millis(500);

/// Output size: requested height, width from the screen's aspect ratio, both even (NV12 needs that).
fn output_size(src_w: u32, src_h: u32, height: u32) -> (u32, u32) {
    let h = if height == 0 || height >= src_h { src_h } else { height };
    let w = ((src_w as u64 * h as u64 + src_h as u64 / 2) / src_h as u64) as u32;
    (w & !1, h & !1)
}

pub fn start_video(opts: VideoOptions, control: Arc<Control>, tx: SyncSender<EncodedFrame>) -> Result<(VideoInfo, JoinHandle<()>)> {
    let (info_tx, info_rx) = mpsc::channel::<Result<VideoInfo>>();
    let handle = std::thread::Builder::new().name("video".into()).spawn(move || {
        let ready = info_tx.clone();
        if let Err(e) = run(opts, control.clone(), tx, info_tx) {
            // Before startup finished: report to the caller. After: log it.
            if ready.send(Err(anyhow::anyhow!("{e:#}"))).is_err() {
                control.report_error(format!("video stopped: {e:#}"));
            }
            control.stop.store(true, Ordering::Relaxed);
        }
    })?;
    let info = info_rx.recv().context("video thread exited during startup")??;
    Ok((info, handle))
}

fn run(opts: VideoOptions, control: Arc<Control>, tx: SyncSender<EncodedFrame>, info_tx: mpsc::Sender<Result<VideoInfo>>) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().context("CoInitializeEx")?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
        // Without this, DXGI duplication can report scaled sizes on high-DPI screens.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let result = run_inner(opts, control, tx, info_tx);
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run_inner(opts: VideoOptions, control: Arc<Control>, tx: SyncSender<EncodedFrame>, info_tx: mpsc::Sender<Result<VideoInfo>>) -> Result<()> {
    let fps = opts.fps.clamp(1, 240);
    let gpu = crate::d3d::open(opts.monitor)?;
    let mut dup = Duplicator::new(&gpu.output, &gpu.device)?;

    // Wait for the first image to learn the desktop's real texture format and size.
    let first = loop {
        match dup.acquire(100)? {
            Grab::Frame(t) => break t,
            _ => {
                if control.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
            }
        }
    };
    let d = texture_desc(&first);
    let (src_w, src_h) = (d.Width, d.Height);
    let (out_w, out_h) = output_size(src_w, src_h, opts.height);
    let mut conv = Converter::new(&gpu.device, &gpu.context, (src_w, src_h, d.Format), (out_w, out_h), fps, 8)?;
    let mut enc = Encoder::new(&gpu.device, gpu.luid, out_w, out_h, fps, opts.bitrate_bps)?;
    conv.stage(&first);
    drop(first);
    dup.release();
    let mut last_nv12: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> = None;

    let _ = info_tx.send(Ok(VideoInfo {
        adapter: gpu.adapter_name.clone(),
        encoder: enc.name.clone(),
        capture_method: "DXGI Desktop Duplication".into(),
        source_width: src_w,
        source_height: src_h,
        width: out_w,
        height: out_h,
        fps,
        unsupported_settings: enc.unsupported.clone(),
    }));

    let stats = &control.stats;
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let start_unix_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as i64;
    let mut next_due = start;
    let mut last_submit = start - STILL_RESEND;
    let mut have_fresh = true; // the first image hasn't been encoded yet
    let mut in_flight: VecDeque<(i64, Instant)> = VecDeque::new();
    let mut outputs: Vec<EncodedOut> = Vec::new();

    while !control.stop.load(Ordering::Relaxed) {
        enc.poll(&mut outputs)?;
        for o in outputs.drain(..) {
            let captured_at = loop {
                match in_flight.front() {
                    Some(&(t, at)) if t < o.sample_time => {
                        in_flight.pop_front();
                        let _ = at;
                    }
                    Some(&(t, at)) if t == o.sample_time => {
                        in_flight.pop_front();
                        break Some(at);
                    }
                    _ => break None,
                }
            };
            let latency = captured_at.map(|at| at.elapsed().as_micros() as u32).unwrap_or(0);
            stats.encoded.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(o.data.len() as u64, Ordering::Relaxed);
            stats.last_latency_us.store(latency, Ordering::Relaxed);
            if o.keyframe {
                stats.keyframes.fetch_add(1, Ordering::Relaxed);
            }
            let frame = EncodedFrame {
                data: o.data,
                keyframe: o.keyframe,
                width: out_w,
                height: out_h,
                capture_unix_us: start_unix_us + o.sample_time / 10,
                encode_latency_us: latency,
            };
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    // The network side is behind. Dropping a frame breaks the decoder's chain,
                    // so ask for a fresh keyframe.
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                    control.request_keyframe();
                }
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }

        let bitrate = control.bitrate_request.swap(0, Ordering::Relaxed);
        if bitrate > 0 {
            enc.set_bitrate(bitrate);
        }

        let now = Instant::now();
        if enc.need_input == 0 || now < next_due {
            // Wait for the encoder or the next frame slot without busy-spinning.
            let wait = if enc.need_input == 0 { Duration::from_millis(1) } else { (next_due - now).min(Duration::from_millis(2)) };
            // Meanwhile keep the newest desktop image so we never send a stale one.
            if !wait.is_zero() {
                match dup.acquire(wait.as_millis().max(1) as u32)? {
                    Grab::Frame(tex) => {
                        stage_checked(&mut conv, &gpu, &tex, fps)?;
                        dup.release();
                        have_fresh = true;
                        stats.captured.fetch_add(1, Ordering::Relaxed);
                    }
                    Grab::Cursor if opts.cursor => have_fresh = true,
                    _ => {}
                }
            }
            continue;
        }

        // A frame slot is due: grab the newest image, waiting up to half a frame for one.
        if !have_fresh {
            let half = (interval.as_millis() as u32 / 2).max(1);
            match dup.acquire(half)? {
                Grab::Frame(tex) => {
                    stage_checked(&mut conv, &gpu, &tex, fps)?;
                    dup.release();
                    have_fresh = true;
                    stats.captured.fetch_add(1, Ordering::Relaxed);
                }
                Grab::Cursor if opts.cursor => have_fresh = true,
                _ => {}
            }
        }

        let keyframe = control.keyframe_requested.swap(false, Ordering::Relaxed);
        let still_due = last_submit.elapsed() >= STILL_RESEND;
        if have_fresh {
            last_nv12 = Some(conv.convert_staged(if opts.cursor { Some(&dup.cursor) } else { None })?);
        }
        if have_fresh || keyframe || still_due {
            if let Some(tex) = &last_nv12 {
                let at = Instant::now();
                let sample_time = (at - start).as_nanos() as i64 / 100;
                enc.submit(tex, sample_time, keyframe)?;
                in_flight.push_back((sample_time, at));
                if in_flight.len() > 64 {
                    in_flight.pop_front();
                }
                stats.submitted.fetch_add(1, Ordering::Relaxed);
                last_submit = at;
                have_fresh = false;
            }
        }

        next_due += interval;
        let now = Instant::now();
        if next_due + interval < now {
            next_due = now; // fell behind (e.g. a hitch): don't try to catch up in a burst
        }
    }
    Ok(())
}

/// Stages the desktop image, rebuilding the converter if the desktop changed size or format
/// (resolution switch). The output size stays the same, so the encoder keeps running.
fn stage_checked(conv: &mut Converter, gpu: &crate::d3d::Gpu, tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, fps: u32) -> Result<()> {
    let d = texture_desc(tex);
    if d.Width != conv.src_width || d.Height != conv.src_height || d.Format != conv.src_format {
        let out = conv.output_size();
        *conv = Converter::new(&gpu.device, &gpu.context, (d.Width, d.Height, d.Format), out, fps, 8)?;
    }
    conv.stage(tex);
    Ok(())
}
