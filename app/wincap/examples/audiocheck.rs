//! Sound capture check: runs one capture mode for 3 seconds and reports how loud a 440 Hz and
//! an 880 Hz tone are in what was captured. Usage: audiocheck all|except-discord|apps:<name>
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn goertzel(samples: &[f32], freq: f32, rate: f32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * freq / rate;
    let (mut s1, mut s2) = (0f32, 0f32);
    for &x in samples {
        let s = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    ((s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt()) / samples.len() as f32
}

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let mode = match arg.as_str() {
        "all" => wincap::AudioMode::All,
        "except-discord" => wincap::AudioMode::ExceptDiscord,
        a if a.starts_with("apps:") => wincap::AudioMode::Apps(vec![a[5..].to_string()]),
        _ => anyhow::bail!("unknown mode"),
    };
    println!("apps playing: {:?}", wincap::audio_apps());
    let control = Arc::new(wincap::Control::default());
    let (tx, rx) = mpsc::sync_channel(200);
    let (_h, desc) = wincap::start_audio(control.clone(), tx, mode)?;
    let mut left = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
            if t.elapsed() > Duration::from_millis(1000) {
                left.extend(c.samples.chunks(2).map(|f| f[0] as f32 / 32768.0));
            }
        }
    }
    control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    println!(
        "mode: {desc}; samples: {}; 440 Hz: {:.4}; 880 Hz: {:.4}; errors: {:?}",
        left.len(),
        goertzel(&left, 440.0, 48000.0),
        goertzel(&left, 880.0, 48000.0),
        control.take_errors()
    );
    Ok(())
}
