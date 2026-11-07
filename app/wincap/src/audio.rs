//! PC audio via WASAPI loopback, with a choice of what to include:
//!
//! - everything the PC plays (classic loopback of the default output device),
//! - everything except one app's process tree (Discord, so viewers don't hear voice chat
//!   echoed back), or
//! - only chosen apps (each app's process tree captured separately and mixed).
//!
//! Per-app capture uses Windows' process loopback (Windows 10 build 20348+ / Windows 11).
//! Where that isn't available, or the app isn't running, it falls back sensibly and says so.
//! Output: 48 kHz stereo, 10 ms chunks of 16-bit PCM, continuous (silence when nothing plays).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::{implement, Interface, Ref, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::Variant::VT_BLOB;

use crate::{AudioChunk, AudioMode, Control};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const CHUNK: usize = (RATE / 100) as usize * CHANNELS as usize; // 10 ms of interleaved samples
const MAX_BUFFER: usize = CHUNK * 20; // 200 ms: beyond this a source is running ahead, drop old audio

/// The apps whose sound "except Discord" leaves out.
const DISCORD: &[&str] = &["discord.exe", "discordptb.exe", "discordcanary.exe"];

#[derive(Clone, Debug, PartialEq)]
enum Source {
    System,
    /// Process loopback of one process tree.
    Process {
        pid: u32,
        exclude: bool,
    },
    /// Nothing to capture right now (e.g. none of the chosen apps is running).
    Silence,
}

/// Starts the audio thread. Returns it plus a description of what's being captured.
pub fn start_audio(control: Arc<Control>, tx: SyncSender<AudioChunk>, mode: AudioMode) -> Result<(JoinHandle<()>, String)> {
    let (desc_tx, desc_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::Builder::new().name("audio".into()).spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let per_app = process_loopback_works();
        let mut desc_tx = Some(desc_tx);
        // Re-plans when apps start or quit, and reconnects when the output device changes.
        while !control.stop.load(Ordering::Relaxed) {
            let (sources, desc) = plan(&mode, per_app);
            if let Some(t) = desc_tx.take() {
                let _ = t.send(desc);
            }
            if let Err(e) = run(&control, &tx, &mode, per_app, &sources) {
                control.report_error(format!("audio capture restarting: {e:#}"));
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        unsafe { CoUninitialize() };
    })?;
    let desc = desc_rx.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| "PC audio".into());
    Ok((handle, desc))
}

/// Exe names (lowercase) of apps that currently have sound sessions on the default output
/// device, i.e. what Windows' volume mixer lists. For picking "only these apps".
pub fn audio_apps() -> Vec<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let names = (|| -> Result<Vec<String>> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            let procs = processes();
            let me = std::process::id();
            let mut out = HashSet::new();
            for i in 0..sessions.GetCount()? {
                let Ok(s) = sessions.GetSession(i) else { continue };
                let Ok(s2) = s.cast::<IAudioSessionControl2>() else { continue };
                let Ok(pid) = s2.GetProcessId() else { continue };
                if pid == 0 || pid == me {
                    continue;
                }
                if let Some((name, _)) = procs.get(&pid) {
                    out.insert(name.clone());
                }
            }
            let mut v: Vec<String> = out.into_iter().collect();
            v.sort();
            Ok(v)
        }
    })()
    .unwrap_or_default();
    unsafe { CoUninitialize() };
    names
}

/// pid -> (lowercase exe name, parent pid)
fn processes() -> HashMap<u32, (String, u32)> {
    let mut out = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_ascii_lowercase();
                out.insert(e.th32ProcessID, (name, e.th32ParentProcessID));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// The top process of each running app named in `names` (its tree holds all its helpers).
fn roots(names: &[String]) -> Vec<(String, u32)> {
    let want: HashSet<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    let procs = processes();
    let mut out: Vec<(String, u32)> = procs
        .iter()
        .filter(|(_, (name, parent))| want.contains(name) && !procs.get(parent).map(|(pn, _)| want.contains(pn)).unwrap_or(false))
        .map(|(pid, (name, _))| (name.clone(), *pid))
        .collect();
    out.sort();
    out
}

fn plan(mode: &AudioMode, per_app: bool) -> (Vec<Source>, String) {
    match mode {
        AudioMode::All => (vec![Source::System], "all PC audio".into()),
        AudioMode::ExceptDiscord => {
            if !per_app {
                return (vec![Source::System], "all PC audio (this Windows version can't leave Discord out)".into());
            }
            let discord: Vec<String> = DISCORD.iter().map(|s| s.to_string()).collect();
            match roots(&discord).first() {
                Some((_, pid)) => (vec![Source::Process { pid: *pid, exclude: true }], "everything except Discord".into()),
                None => (vec![Source::System], "all PC audio (Discord isn't running)".into()),
            }
        }
        AudioMode::Apps(names) => {
            if !per_app {
                return (vec![Source::System], "all PC audio (this Windows version can't pick apps)".into());
            }
            let found = roots(names);
            if found.is_empty() {
                return (vec![Source::Silence], format!("only {} (not running yet)", names.join(", ")));
            }
            let mut seen = HashSet::new();
            let list: Vec<String> = found.iter().filter(|(n, _)| seen.insert(n.clone())).map(|(n, _)| n.clone()).collect();
            (found.into_iter().map(|(_, pid)| Source::Process { pid, exclude: false }).collect(), format!("only {}", list.join(", ")))
        }
    }
}

// ---------- capture ----------

fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
        nBlockAlign: CHANNELS * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Option<HANDLE>,
    buf: Vec<f32>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(e) = self.event.take() {
                let _ = CloseHandle(e);
            }
        }
    }
}

fn open(source: &Source) -> Result<Option<Capture>> {
    unsafe {
        let (client, event) = match source {
            Source::Silence => return Ok(None),
            Source::System => {
                let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).context("default output device")?;
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("IAudioClient")?;
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                        2_000_000,
                        0,
                        &format(),
                        None,
                    )
                    .context("IAudioClient::Initialize (loopback)")?;
                (client, None)
            }
            Source::Process { pid, exclude } => {
                let client = activate_process_loopback(*pid, *exclude)?;
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                        2_000_000,
                        0,
                        &format(),
                        None,
                    )
                    .context("IAudioClient::Initialize (process loopback)")?;
                // Event mode is required for process loopback; we still poll, the event just
                // has to exist.
                let event = CreateEventW(None, false, false, None).context("CreateEvent")?;
                client.SetEventHandle(event).context("SetEventHandle")?;
                (client, Some(event))
            }
        };
        let capture: IAudioCaptureClient = client.GetService().context("IAudioCaptureClient")?;
        client.Start()?;
        Ok(Some(Capture { client, capture, event, buf: Vec::with_capacity(MAX_BUFFER) }))
    }
}

fn run(control: &Control, tx: &SyncSender<AudioChunk>, mode: &AudioMode, per_app: bool, sources: &[Source]) -> Result<()> {
    let mut caps: Vec<Capture> = Vec::new();
    for s in sources {
        if let Some(c) = open(s)? {
            caps.push(c);
        }
    }
    let start = Instant::now();
    let mut emitted: u64 = 0;
    let mut last_replan = Instant::now();
    while !control.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
        for c in caps.iter_mut() {
            drain(c)?;
        }
        // Emit on a steady 10 ms clock, mixing whatever each source has (silence if nothing).
        let due = start.elapsed().as_millis() as u64 / 10;
        while emitted < due {
            let mut mix = vec![0f32; CHUNK];
            for c in caps.iter_mut() {
                let n = c.buf.len().min(CHUNK);
                for (m, s) in mix.iter_mut().zip(c.buf.drain(..n)) {
                    *m += s;
                }
            }
            let samples: Vec<i16> = mix.into_iter().map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect();
            emitted += 1;
            control.stats.audio_chunks.fetch_add(1, Ordering::Relaxed);
            match tx.try_send(AudioChunk { samples, sample_rate: RATE, channels: CHANNELS }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        // Apps start and quit: re-plan every few seconds and restart capture if it changed.
        if !matches!(mode, AudioMode::All) && last_replan.elapsed() > Duration::from_secs(3) {
            last_replan = Instant::now();
            if plan(mode, per_app).0.as_slice() != sources {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn drain(c: &mut Capture) -> Result<()> {
    unsafe {
        loop {
            let packet = c.capture.GetNextPacketSize()?;
            if packet == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            c.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let n = frames as usize * CHANNELS as usize;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                c.buf.extend(std::iter::repeat_n(0.0, n));
            } else {
                c.buf.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
            }
            c.capture.ReleaseBuffer(frames)?;
        }
        if c.buf.len() > MAX_BUFFER {
            let extra = c.buf.len() - MAX_BUFFER / 2;
            c.buf.drain(..extra - extra % CHANNELS as usize);
        }
    }
    Ok(())
}

// ---------- process loopback activation ----------

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Done(Arc<(Mutex<bool>, Condvar)>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        Ok(())
    }
}

fn activate_process_loopback(pid: u32, exclude: bool) -> Result<IAudioClient> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ..Default::default() };
        params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: pid,
            ProcessLoopbackMode: if exclude {
                PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
            } else {
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
            },
        };
        // PROPVARIANT's Drop calls PropVariantClear, which would CoTaskMemFree our stack-based
        // blob (heap corruption, 0xc0000374). The whole thing must never be dropped.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let handler: IActivateAudioInterfaceCompletionHandler = Done(signal.clone()).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop), &handler)
            .context("ActivateAudioInterfaceAsync")?;
        {
            let (lock, cv) = &*signal;
            let mut done = lock.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !*done {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(anyhow!("timed out activating per-app audio capture"));
                }
                done = cv.wait_timeout(done, left).unwrap().0;
            }
        }
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk).context("GetActivateResult")?;
        hr.ok().context("per-app audio capture was refused")?;
        unk.ok_or_else(|| anyhow!("per-app audio capture returned nothing"))?.cast::<IAudioClient>().context("IAudioClient")
    }
}

/// Process loopback exists on Windows 10 build 20348+ and Windows 11. Probe it once by
/// excluding our own process (harmless) and see whether Windows accepts it.
fn process_loopback_works() -> bool {
    match activate_process_loopback(std::process::id(), true) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("per-app audio not available: {e:#}");
            false
        }
    }
}
