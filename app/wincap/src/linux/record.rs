//! Saving a copy of the stream on this computer: the H.264 frames the stream already sends, and
//! its sound encoded as Opus, in an MKV file in `~/Videos/Streamroom`. No second video encode.
//!
//! The capture threads hand frames and sound here before they go to the network, so frames the
//! network side drops are still saved. Both feeds are timed from when saving started: video by
//! each frame's capture time, sound by counting samples (back in step with the clock after a
//! gap). The muxer waits for both feeds, so while no sound comes (the per-app fallback pause, the
//! sound pipeline restarting) the video thread fills in silence.
//!
//! MKV stays playable if the app dies mid-recording; ending it properly also writes the seek
//! index.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tracing::{info, warn};

use super::audio::{CHANNELS, CHUNK, RATE};
use super::launch::next_error;
use crate::{Control, Recording};

/// Silence filled in per step while no sound arrives: one chunk (`CHUNK` samples).
const SILENCE: Duration = Duration::from_millis(10);
/// Sound this far ahead of or behind where the clock says is put back in step.
const TOLERANCE: Duration = Duration::from_millis(200);
/// Sound this far behind the clock (none arriving) gets silence filled in...
const FILL_LAG: Duration = Duration::from_millis(300);
/// ...up to this far behind, so sound that comes back carries on without a jump.
const FILL_TO: Duration = Duration::from_millis(100);
/// How long ending the file may take (the muxer writes its index).
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether this session is saving a copy. Lives in [`Control`], so the capture threads find it.
#[derive(Default)]
pub enum Slot {
    #[default]
    Off,
    On(Arc<Recorder>),
    /// Couldn't start; kept so the app can say why.
    Failed {
        path: PathBuf,
        why: String,
    },
}

/// Starts saving a copy (or keeps the one already running). `with_audio`: the session shares
/// sound, so the file gets a sound track. A failure is also kept for [`recording`].
pub fn start_recording(control: &Control, with_audio: bool) -> Result<PathBuf> {
    if let Slot::On(r) = &*slot(control) {
        if r.failure().is_none() {
            return Ok(r.path.clone());
        }
    }
    let path = match folder().and_then(|dir| new_path(&dir)) {
        Ok(p) => p,
        Err(e) => {
            *slot(control) = Slot::Failed { path: PathBuf::new(), why: format!("{e:#}") };
            return Err(e);
        }
    };
    match Recorder::start(path.clone(), with_audio) {
        Ok(r) => {
            info!(path = %path.display(), with_audio, "saving a copy of the stream");
            let old = std::mem::replace(&mut *slot(control), Slot::On(Arc::new(r)));
            drop(old); // a failed earlier recorder: its pipeline is already stopped
                       // The file has to start with a keyframe.
            control.request_keyframe();
            Ok(path)
        }
        Err(e) => {
            *slot(control) = Slot::Failed { path, why: format!("{e:#}") };
            Err(e)
        }
    }
}

/// Stops saving and finishes the file (waits up to a few seconds for the muxer).
pub fn stop_recording(control: &Control) {
    let old = std::mem::take(&mut *slot(control));
    if let Slot::On(r) = old {
        r.finish();
    }
}

/// Whether a copy is being saved, for the app.
pub fn recording(control: &Control) -> Recording {
    match &*slot(control) {
        Slot::Off => Recording::Off,
        Slot::On(r) => match r.failure() {
            None => Recording::On { path: r.path.display().to_string() },
            Some(why) => Recording::Failed { path: r.path.display().to_string(), why },
        },
        Slot::Failed { path, why } => Recording::Failed { path: path.display().to_string(), why: why.clone() },
    }
}

/// An encoded frame, from the video thread. `captured`: when the screen sent it.
pub fn video(control: &Control, data: &[u8], keyframe: bool, captured: Instant) {
    if let Some(r) = current(control) {
        r.push_video(data, keyframe, captured);
    }
}

/// A chunk of sound (interleaved 16-bit, 48 kHz stereo), from the sound thread.
pub fn audio(control: &Control, samples: &[i16]) {
    if let Some(r) = current(control) {
        r.push_audio(samples);
    }
}

fn slot(control: &Control) -> std::sync::MutexGuard<'_, Slot> {
    control.recording.lock().unwrap_or_else(PoisonError::into_inner)
}

fn current(control: &Control) -> Option<Arc<Recorder>> {
    match &*slot(control) {
        Slot::On(r) => Some(r.clone()),
        Slot::Off | Slot::Failed { .. } => None,
    }
}

/// `~/Videos/Streamroom`, created if missing.
fn folder() -> Result<PathBuf> {
    let dir = gst::glib::home_dir().join("Videos").join("Streamroom");
    std::fs::create_dir_all(&dir).with_context(|| format!("couldn't create the folder {}", dir.display()))?;
    Ok(dir)
}

/// A new file named for the local date and time, e.g. `2026-11-08 20-15-03.mkv`.
fn new_path(dir: &Path) -> Result<PathBuf> {
    let stamp =
        gst::glib::DateTime::now_local().and_then(|t| t.format("%Y-%m-%d %H-%M-%S")).map_err(|e| anyhow!("couldn't read the local time: {e}"))?;
    free_path(dir, &stamp, |p| p.exists())
}

/// The first of `<stamp>.mkv`, `<stamp> (2).mkv`, ... that doesn't `exist` yet.
fn free_path(dir: &Path, stamp: &str, exists: impl Fn(&Path) -> bool) -> Result<PathBuf> {
    (1..=100)
        .map(|n| dir.join(if n == 1 { format!("{stamp}.mkv") } else { format!("{stamp} ({n}).mkv") }))
        .find(|p| !exists(p))
        .ok_or_else(|| anyhow!("too many recordings named {stamp} in {}", dir.display()))
}

/// The file being written: a pipeline fed by the capture threads.
pub struct Recorder {
    pipeline: gst::Pipeline,
    video: gst_app::AppSrc,
    /// None when the session shares no sound.
    audio: Option<gst_app::AppSrc>,
    path: PathBuf,
    start: Instant,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Video is saved from the first keyframe on.
    keyframe_seen: bool,
    last_video: Option<Duration>,
    sound: SoundClock,
    /// Why saving stopped on its own (disk full, muxer error).
    failed: Option<String>,
    /// The file is being ended: take nothing more.
    ending: bool,
}

/// Appsrc settings: timed by us, never blocks the capture thread, and if the file side stalls
/// (slow disk), drops rather than growing without limit.
const SRC: &str = "format=time is-live=false block=false max-bytes=64000000 leaky-type=downstream";

fn pipeline_text(with_audio: bool) -> String {
    let mut s = format!(
        "appsrc name=v {SRC} caps=\"video/x-h264,stream-format=byte-stream,alignment=au\" ! \
         h264parse ! queue ! matroskamux name=mux ! filesink name=file"
    );
    if with_audio {
        s.push_str(&format!(
            " appsrc name=a {SRC} caps=\"audio/x-raw,format=S16LE,rate={RATE},channels={CHANNELS},layout=interleaved\" ! \
             audioconvert ! opusenc bitrate=128000 ! queue ! mux."
        ));
    }
    s
}

fn app_src(pipeline: &gst::Pipeline, name: &str) -> Result<gst_app::AppSrc> {
    pipeline.by_name(name).and_then(|e| e.downcast::<gst_app::AppSrc>().ok()).ok_or_else(|| anyhow!("the recording pipeline has no {name} input"))
}

impl Recorder {
    fn start(path: PathBuf, with_audio: bool) -> Result<Self> {
        gst::init().context("GStreamer")?;
        let pipeline = gst::parse::launch(&pipeline_text(with_audio))
            .context("building the recording pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("the recording pipeline isn't a pipeline"))?;
        // Set here rather than in the text: no quoting trouble with spaces in the path.
        pipeline
            .by_name("file")
            .ok_or_else(|| anyhow!("the recording pipeline has no file output"))?
            .set_property("location", path.to_string_lossy().as_ref());
        let video = app_src(&pipeline, "v")?;
        let audio = if with_audio { Some(app_src(&pipeline, "a")?) } else { None };
        // From here on, Drop sets the pipeline back to Null however this ends.
        let r = Recorder { pipeline, video, audio, path, start: Instant::now(), state: Mutex::new(State::default()) };
        if r.pipeline.set_state(gst::State::Playing).is_err() {
            let why = r.pipeline.bus().and_then(|b| next_error(&b, gst::ClockTime::from_seconds(1))).map(|e| e.message);
            bail!("couldn't start writing {}: {}", r.path.display(), why.as_deref().unwrap_or("no details"));
        }
        Ok(r)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means an earlier call panicked; the state is still usable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn failure(&self) -> Option<String> {
        self.lock().failed.clone()
    }

    /// Saving stopped on its own: say why once and stop writing.
    fn fail(&self, st: &mut State, why: String) {
        warn!(path = %self.path.display(), why, "saving the copy of the stream stopped");
        st.failed = Some(why);
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            warn!(error = %e, "stopping the recording pipeline");
        }
    }

    fn push_video(&self, data: &[u8], keyframe: bool, captured: Instant) {
        let mut st = self.lock();
        if st.failed.is_some() || st.ending {
            return;
        }
        if let Some(e) = self.pipeline.bus().and_then(|b| next_error(&b, gst::ClockTime::ZERO)) {
            return self.fail(&mut st, e.message);
        }
        if !st.keyframe_seen && !keyframe {
            return;
        }
        st.keyframe_seen = true;
        let pts = video_time(st.last_video, captured.saturating_duration_since(self.start));
        st.last_video = Some(pts);
        let mut buf = gst::Buffer::from_mut_slice(data.to_vec());
        if let Some(b) = buf.get_mut() {
            let t = gst::ClockTime::try_from(pts).ok();
            b.set_pts(t);
            b.set_dts(t); // no B-frames: decode order is display order
            if !keyframe {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        if let Err(e) = self.video.push_buffer(buf) {
            return self.fail(&mut st, format!("the file stopped taking video ({e:?})"));
        }
        self.fill_silence(&mut st);
    }

    fn push_audio(&self, samples: &[i16]) {
        let mut st = self.lock();
        if st.failed.is_some() || st.ending {
            return;
        }
        let length = sound_length(samples.len());
        let Some(pts) = st.sound.place(self.start.elapsed(), length) else { return };
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.push_sound(&mut st, bytes, pts, length);
    }

    /// No sound arriving: fill in silence, so the muxer (which waits for both feeds) keeps
    /// writing video.
    fn fill_silence(&self, st: &mut State) {
        if self.audio.is_none() {
            return;
        }
        let bytes = silence_bytes();
        for pts in st.sound.fill(self.start.elapsed()) {
            self.push_sound(st, bytes.clone(), pts, SILENCE);
            if st.failed.is_some() {
                return;
            }
        }
    }

    fn push_sound(&self, st: &mut State, bytes: Vec<u8>, pts: Duration, length: Duration) {
        let Some(src) = &self.audio else { return };
        let mut buf = gst::Buffer::from_mut_slice(bytes);
        if let Some(b) = buf.get_mut() {
            b.set_pts(gst::ClockTime::try_from(pts).ok());
            b.set_duration(gst::ClockTime::try_from(length).ok());
        }
        if let Err(e) = src.push_buffer(buf) {
            self.fail(st, format!("the file stopped taking sound ({e:?})"));
        }
    }

    /// Ends the file properly (the muxer writes its index) and waits for that, a few seconds at
    /// most.
    fn finish(&self) {
        {
            let mut st = self.lock();
            if st.failed.is_some() {
                return;
            }
            st.ending = true;
        }
        for src in std::iter::once(&self.video).chain(self.audio.as_ref()) {
            if let Err(e) = src.end_of_stream() {
                warn!(error = ?e, "ending the recording");
            }
        }
        let done = self
            .pipeline
            .bus()
            .and_then(|b| b.timed_pop_filtered(gst::ClockTime::try_from(FINISH_TIMEOUT).ok(), &[gst::MessageType::Eos, gst::MessageType::Error]));
        match done.as_ref().map(|m| m.view()) {
            Some(gst::MessageView::Eos(_)) => info!(path = %self.path.display(), "saved the copy of the stream"),
            Some(gst::MessageView::Error(e)) => warn!(path = %self.path.display(), error = %e.error(), "finishing the recording"),
            _ => warn!(path = %self.path.display(), "the recording didn't finish in time; it plays but may not seek"),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            warn!(error = %e, "stopping the recording pipeline");
        }
    }
}

/// A video frame's time in the file: its capture time, nudged so it always comes after the one
/// before (the muxer refuses frames going backwards).
fn video_time(last: Option<Duration>, captured: Duration) -> Duration {
    match last {
        Some(l) if captured <= l => l + Duration::from_micros(1),
        Some(_) | None => captured,
    }
}

/// How long `samples` (interleaved, all channels) last.
fn sound_length(samples: usize) -> Duration {
    let frames = (samples / usize::from(CHANNELS)) as u64;
    Duration::from_nanos(frames * 1_000_000_000 / u64::from(RATE))
}

/// One chunk of silence, as 16-bit samples.
fn silence_bytes() -> Vec<u8> {
    vec![0; CHUNK * 2]
}

/// Times sound chunks in the file. Chunks follow on from each other (counting samples), so the
/// sound has no clicks, but are put back in step with the clock after a gap.
#[derive(Default)]
struct SoundClock {
    /// Where the next chunk goes; None until the first.
    next: Option<Duration>,
}

impl SoundClock {
    /// Where a chunk of `length` that arrived at `now` (since saving started) goes, or None to
    /// drop it because sound is running ahead of the clock.
    fn place(&mut self, now: Duration, length: Duration) -> Option<Duration> {
        let arrived = now.saturating_sub(length);
        let pts = match self.next {
            Some(next) if next > arrived + TOLERANCE => return None,
            Some(next) if arrived <= next + TOLERANCE => next,
            // The first chunk, or the first after a gap.
            Some(_) | None => arrived,
        };
        self.next = Some(pts + length);
        Some(pts)
    }

    /// Times of the silent chunks to fill in at `now`: none while sound keeps up, else enough
    /// to bring it to `FILL_TO` behind the clock.
    fn fill(&mut self, now: Duration) -> Vec<Duration> {
        let mut next = self.next.unwrap_or_default();
        if next + FILL_LAG >= now {
            return Vec::new();
        }
        let mut out = Vec::new();
        while next + FILL_TO < now {
            out.push(next);
            next += SILENCE;
        }
        self.next = Some(next);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn sound_follows_on_by_sample_count() {
        let mut c = SoundClock::default();
        assert_eq!(c.place(MS(30), MS(10)), Some(MS(20)), "the first chunk goes where it arrived");
        // Arrival jitters (two chunks at once, one late), the file times don't.
        assert_eq!(c.place(MS(35), MS(10)), Some(MS(30)));
        assert_eq!(c.place(MS(35), MS(10)), Some(MS(40)));
        assert_eq!(c.place(MS(80), MS(10)), Some(MS(50)));
    }

    #[test]
    fn sound_back_in_step_after_a_gap() {
        let mut c = SoundClock::default();
        c.place(MS(10), MS(10));
        assert_eq!(c.place(MS(1010), MS(10)), Some(MS(1000)), "a second with no sound: jump to the clock");
        assert_eq!(c.place(MS(1020), MS(10)), Some(MS(1010)));
    }

    #[test]
    fn sound_far_ahead_of_the_clock_is_dropped() {
        let mut c = SoundClock::default();
        c.place(MS(10), MS(10));
        // 30 chunks arrive at once: the ones beyond the tolerance are dropped.
        let placed = (0..30).filter_map(|_| c.place(MS(20), MS(10))).count();
        assert_eq!(placed, 21, "up to 200 ms ahead is kept");
        assert_eq!(c.place(MS(240), MS(10)), Some(MS(220)), "then it carries on from there");
    }

    #[test]
    fn silence_fills_in_only_when_sound_stops() {
        let mut c = SoundClock::default();
        c.place(MS(10), MS(10));
        assert!(c.fill(MS(250)).is_empty(), "a short wait for sound fills nothing");
        let filled = c.fill(MS(500));
        assert_eq!(filled.first(), Some(&MS(10)), "silence starts where the sound stopped");
        assert_eq!(filled.last(), Some(&MS(390)), "and runs up to 100 ms behind the clock");
        assert_eq!(filled.len(), 39);
        assert_eq!(c.place(MS(510), MS(10)), Some(MS(400)), "sound that comes back carries on without a jump");
    }

    #[test]
    fn silence_from_the_start_when_no_sound_ever_came() {
        let mut c = SoundClock::default();
        let filled = c.fill(MS(400));
        assert_eq!(filled.first(), Some(&Duration::ZERO));
        assert_eq!(filled.len(), 30);
    }

    #[test]
    fn video_times_never_go_backwards() {
        assert_eq!(video_time(None, MS(5)), MS(5));
        assert_eq!(video_time(Some(MS(5)), MS(21)), MS(21));
        assert_eq!(video_time(Some(MS(21)), MS(20)), MS(21) + Duration::from_micros(1));
    }

    #[test]
    fn sound_chunk_lengths() {
        assert_eq!(sound_length(960), MS(10), "480 stereo frames at 48 kHz");
        assert_eq!(sound_length(CHUNK), SILENCE, "a silent chunk is as long as a sound chunk");
    }

    #[test]
    fn file_names_dont_overwrite() {
        let dir = Path::new("/v");
        let taken = |p: &Path| p == Path::new("/v/2026-11-08 20-15-03.mkv") || p == Path::new("/v/2026-11-08 20-15-03 (2).mkv");
        assert_eq!(free_path(dir, "2026-11-08 20-15-03", taken).unwrap(), Path::new("/v/2026-11-08 20-15-03 (3).mkv"));
        assert_eq!(free_path(dir, "2026-11-08 20-15-04", taken).unwrap(), Path::new("/v/2026-11-08 20-15-04.mkv"));
        assert!(free_path(dir, "x", |_| true).is_err());
    }

    /// The real pipeline: x264 frames and a tone in, then the file read back. Needs x264enc
    /// and the Matroska and Opus plugins (all in the Linux CI image).
    #[test]
    fn writes_an_mkv_with_video_and_sound() {
        gst::init().unwrap();
        let dir = std::env::temp_dir().join(format!("streamroom-record-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.mkv");
        let r = Recorder::start(path.clone(), true).unwrap();

        let (frames, keyframes) = encoded_test_frames(30);
        assert!(keyframes > 0);
        for (i, (data, key)) in frames.iter().enumerate() {
            r.push_audio(&[0i16; 960]);
            r.push_audio(&[0i16; 960]);
            r.push_video(data, *key, r.start + MS(33 * i as u64));
        }
        r.finish();
        assert_eq!(r.failure(), None);
        drop(r);

        let (video, audio) = read_back(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(video >= 25, "video frames read back: {video}");
        assert!(audio > 0, "sound packets read back: {audio}");
    }

    fn encoded_test_frames(n: u32) -> (Vec<(Vec<u8>, bool)>, usize) {
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers={n} ! video/x-raw,width=320,height=240,framerate=30/1 ! x264enc key-int-max=10 bframes=0 ! \
             h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = p.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Ok(s) = sink.pull_sample() {
            let b = s.buffer().unwrap();
            out.push((b.map_readable().unwrap().to_vec(), !b.flags().contains(gst::BufferFlags::DELTA_UNIT)));
        }
        p.set_state(gst::State::Null).unwrap();
        let keys = out.iter().filter(|(_, k)| *k).count();
        (out, keys)
    }

    /// Video frames and sound packets in the file.
    fn read_back(path: &Path) -> (usize, usize) {
        let p = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! matroskademux name=d d.video_0 ! queue ! appsink name=v sync=false d.audio_0 ! queue ! appsink name=a sync=false",
            path.display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let count = |name: &str| {
            let sink = p.by_name(name).unwrap().downcast::<gst_app::AppSink>().unwrap();
            std::thread::spawn(move || std::iter::from_fn(|| sink.pull_sample().ok()).count())
        };
        let (v, a) = (count("v"), count("a"));
        let counts = (v.join().unwrap(), a.join().unwrap());
        p.set_state(gst::State::Null).unwrap();
        counts
    }
}
