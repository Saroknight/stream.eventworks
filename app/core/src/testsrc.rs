//! Software test pattern encoded with OpenH264. Lets the publishing path be tested on machines
//! without a hardware encoder (and on Linux). Not used for real streaming.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use wincap::{Control, EncodedFrame, VideoInfo};

pub fn start(
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    control: Arc<Control>,
    tx: SyncSender<EncodedFrame>,
) -> Result<(VideoInfo, JoinHandle<()>)> {
    let config = EncoderConfig::new().bitrate(BitRate::from_bps(bitrate)).max_frame_rate(FrameRate::from_hz(fps as f32)).skip_frames(false);
    let mut encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
    let info = VideoInfo {
        adapter: "none".into(),
        encoder: "OpenH264 (software test pattern)".into(),
        capture_method: "test pattern".into(),
        source_width: width,
        source_height: height,
        width,
        height,
        fps,
        unsupported_settings: vec![],
    };
    let handle = std::thread::spawn(move || {
        let (w, h) = (width as usize, height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let mut next = Instant::now();
        let mut n: usize = 0;
        while !control.stop.load(Ordering::Relaxed) {
            // Moving bars plus a sweeping block, so motion and frame rate are easy to judge.
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 3;
                    let bar = ((x + n * 8) / 80) % 3;
                    rgb[i] = if bar == 0 { 220 } else { 40 };
                    rgb[i + 1] = if bar == 1 { 200 } else { 40 };
                    rgb[i + 2] = if bar == 2 { 230 } else { 60 };
                }
            }
            let bx = (n * 12) % (w.saturating_sub(120).max(1));
            for y in h / 2 - 60..h / 2 + 60 {
                for x in bx..bx + 120 {
                    let i = (y * w + x) * 3;
                    rgb[i..i + 3].copy_from_slice(&[255, 255, 255]);
                }
            }
            if control.keyframe_requested.swap(false, Ordering::Relaxed) {
                encoder.force_intra_frame();
            }
            let captured = Instant::now();
            let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (w, h)));
            if let Ok(bits) = encoder.encode(&yuv) {
                let data = bits.to_vec();
                if !data.is_empty() {
                    let keyframe = wincap_nal_has_idr(&data);
                    let stats = &control.stats;
                    stats.captured.fetch_add(1, Ordering::Relaxed);
                    stats.submitted.fetch_add(1, Ordering::Relaxed);
                    stats.encoded.fetch_add(1, Ordering::Relaxed);
                    stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    if keyframe {
                        stats.keyframes.fetch_add(1, Ordering::Relaxed);
                    }
                    let latency = captured.elapsed().as_micros() as u32;
                    stats.last_latency_us.store(latency, Ordering::Relaxed);
                    let frame = EncodedFrame {
                        data,
                        keyframe,
                        width,
                        height,
                        capture_unix_us: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as i64,
                        encode_latency_us: latency,
                    };
                    match tx.try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            stats.dropped.fetch_add(1, Ordering::Relaxed);
                            encoder.force_intra_frame();
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
            }
            n += 1;
            next += interval;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    });
    Ok((info, handle))
}

fn wincap_nal_has_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}
