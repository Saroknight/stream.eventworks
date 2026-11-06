//! WASAPI loopback: captures everything the PC plays on its default output device.
//! Windows converts to 48 kHz stereo for us; we emit 10 ms chunks of 16-bit PCM.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::{AudioChunk, Control};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const CHUNK_FRAMES: usize = (RATE / 100) as usize; // 10 ms

pub fn start_audio(control: Arc<Control>, tx: SyncSender<AudioChunk>) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new().name("audio".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // Reconnect when the device changes (headphones plugged in, default device switched).
        while !control.stop.load(Ordering::Relaxed) {
            if let Err(e) = run(&control, &tx) {
                control.report_error(format!("audio capture restarting: {e:#}"));
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        unsafe { CoUninitialize() };
    })?)
}

fn run(control: &Control, tx: &SyncSender<AudioChunk>) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("default output device")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
            nBlockAlign: CHANNELS * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000, // 200 ms buffer
                0,
                &format,
                None,
            )
            .context("IAudioClient::Initialize (loopback)")?;
        let capture: IAudioCaptureClient = client.GetService().context("IAudioCaptureClient")?;
        client.Start()?;

        let mut pending: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * CHANNELS as usize * 4);
        let mut last_emit = Instant::now();
        let result: Result<()> = (|| {
            while !control.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
                let mut got_data = false;
                loop {
                    let packet = capture.GetNextPacketSize()?;
                    if packet == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let n = frames as usize * CHANNELS as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        pending.extend(std::iter::repeat_n(0.0, n));
                    } else {
                        pending.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
                    }
                    capture.ReleaseBuffer(frames)?;
                    got_data = true;
                }
                // Loopback delivers nothing while the PC is silent; fill with silence to keep the
                // stream continuous.
                if !got_data && pending.is_empty() && last_emit.elapsed() >= Duration::from_millis(10) {
                    pending.extend(std::iter::repeat_n(0.0, CHUNK_FRAMES * CHANNELS as usize));
                }
                let chunk = CHUNK_FRAMES * CHANNELS as usize;
                while pending.len() >= chunk {
                    let samples: Vec<i16> =
                        pending.drain(..chunk).map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect();
                    last_emit = Instant::now();
                    control.stats.audio_chunks.fetch_add(1, Ordering::Relaxed);
                    match tx.try_send(AudioChunk { samples, sample_rate: RATE, channels: CHANNELS }) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                }
            }
            Ok(())
        })();
        let _ = client.Stop();
        result
    }
}
