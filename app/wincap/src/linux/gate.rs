//! The frame-rate cap: which raw frames go on to the encoder.

/// Lets frames through at no more than the chosen rate, by their timestamps. Screens can send
/// far more (a 165 Hz monitor sends 165 fps), which starves each frame of bitrate. Runs before
/// the encoder: dropping encoded H.264 frames would break the ones that refer to them.
#[derive(Debug)]
pub struct FrameGate {
    /// Nanoseconds between frames at the chosen rate.
    interval: u64,
    /// How early a frame may be and still count as on time (timestamps jitter).
    slack: u64,
    /// When the next frame is due; `None` before the first one.
    next: Option<u64>,
}

impl FrameGate {
    pub fn new(fps: u32) -> Self {
        let interval = 1_000_000_000 / u64::from(fps.max(1));
        Self { interval, slack: interval / 4, next: None }
    }

    /// Whether the frame stamped `pts` (nanoseconds) goes through.
    pub fn admit(&mut self, pts: u64) -> bool {
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
