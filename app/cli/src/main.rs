//! Console version of Streamroom Live, for troubleshooting and testing.
//!
//!   streamroom-live-cli "<app link>" [--height 1080] [--fps 60] [--bitrate 8] [--monitor 0]
//!                       [--no-audio] [--no-cursor] [--test-pattern] [--list-monitors]

use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use streamroom_core::api::parse_launch;
use streamroom_core::{log, Api, Launch, Session, Settings, Source};
use tracing::{error, info};

const HELP: &str = "Streamroom Live (console)

Usage: streamroom-live-cli [APP_LINK] [options]

Options:
  --height N         Output height: 1080 (default), 720, 1440, or 0 for native
  --fps N            Frame rate (default 60)
  --bitrate MBPS     Video bitrate in Mbps (default 8)
  --monitor N        Monitor to capture, 0 = primary (default)
  --no-audio         Don't share PC audio
  --no-cursor        Don't show the mouse cursor
  --list-monitors    Show monitors and exit
  --capture-test     Capture and encode the screen for 5 seconds, no server needed (troubleshooting)
  --test-pattern     Send a moving test pattern instead of the screen (testing only)
";

fn prompt(question: &str) -> Result<String> {
    print!("{question}: ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn main() {
    log::init(None);
    streamroom_core::init();
    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(anyhow::Error::from).and_then(|rt| rt.block_on(run()))
    });
    let failed = match result {
        Ok(Ok(())) => false,
        Ok(Err(e)) => {
            eprintln!("\nError: {e:#}");
            error!("Error: {e:#}");
            true
        }
        Err(_) => true,
    };
    log::flush();
    if failed {
        eprintln!("\nLog: {}", log::path().display());
        let _ = prompt("Press Enter to close");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut settings = Settings::default();
    let mut link_text = None;
    let mut source = Source::Screen;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().with_context(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "--height" => settings.height = value("--height")?.parse().context("--height")?,
            "--fps" => settings.fps = value("--fps")?.parse().context("--fps")?,
            "--bitrate" => settings.bitrate_mbps = value("--bitrate")?.parse().context("--bitrate")?,
            "--monitor" => settings.monitor = value("--monitor")?.parse().context("--monitor")?,
            "--no-audio" => settings.audio = false,
            "--no-cursor" => settings.cursor = false,
            "--test-pattern" => source = Source::TestPattern,
            "--name" => {
                value("--name")?; // accepted for compatibility; the server uses your account name
            }
            "--capture-test" => {
                println!("Streamroom Live {} capture test: pick a screen if your desktop asks.", env!("CARGO_PKG_VERSION"));
                let s = settings.clone();
                let report = tokio::task::spawn_blocking(move || streamroom_core::capture_test(&s, 5)).await??;
                println!("{report}\n\nCapture works.");
                return Ok(());
            }
            "--list-monitors" => {
                for m in streamroom_core::list_monitors() {
                    println!("{}: {} {}x{}{} on {}", m.index, m.name, m.width, m.height, if m.primary { " (primary)" } else { "" }, m.gpu);
                }
                return Ok(());
            }
            s if s.starts_with("--") => bail!("unknown option {s}\n\n{HELP}"),
            s => link_text = Some(s.to_string()),
        }
    }
    println!("Streamroom Live {} (console)", env!("CARGO_PKG_VERSION"));
    let link_text = match link_text {
        Some(l) => l,
        None => prompt("Paste a link for the app (Go Live → Copy a link for the app)")?,
    };
    let launch = parse_launch(&link_text)?;
    print!("Connecting to {} ... ", launch.server());
    std::io::stdout().flush()?;
    // New links carry a single-use sign-in code: sign in for this run only, sign out at the end.
    let mut signed_in: Option<Api> = None;
    let (session, info) = match launch {
        Launch::Legacy(link) => Session::start(&link, &settings, source).await?,
        Launch::Open { server, room, pair } => {
            let code = pair.ok_or_else(|| anyhow::anyhow!("that link has no sign-in code. Use Go Live → Copy a link for the app."))?;
            let room = room.ok_or_else(|| anyhow::anyhow!("that link doesn't name a room"))?;
            let account = Api::new(&server, None)?.pair(&code, None, "console").await?;
            let api = Api::for_account(&account)?;
            let joined = api.app_token(&room).await?;
            signed_in = Some(api);
            Session::start_joined(&server, joined, &settings, source).await?
        }
    };
    println!("live in \"{}\"", info.room_title);
    println!("GPU:      {}", info.video.adapter);
    println!(
        "Capture:  {} {}x{} -> {}x{} @ {} fps",
        info.video.capture_method, info.video.source_width, info.video.source_height, info.video.width, info.video.height, info.video.fps
    );
    println!("Encoder:  {} (H.264, {:.1} Mbps)", info.video.encoder, info.bitrate_mbps);
    if !info.video.unsupported_settings.is_empty() {
        println!("          (encoder ignored: {})", info.video.unsupported_settings.join(", "));
    }
    println!("Audio:    {}\n\nLIVE. Press Ctrl+C to stop.\n", info.audio);

    let mut ticks = 0u64;
    let reason = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break "stopped".to_string(),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        if let Some(r) = session.ended() {
            break r;
        }
        let s = session.stats();
        print!(
            "\rcapture {:5.1} fps | sent {:5.1} fps | {:5.2} Mbps | encode {:4.1} ms | keyframes {} | dropped {}   ",
            s.capture_fps, s.sent_fps, s.mbps, s.encode_ms, s.keyframes, s.dropped
        );
        let _ = std::io::stdout().flush();
        ticks += 1;
        if ticks.is_multiple_of(10) {
            info!("stats: {s:?}");
        }
    };
    println!("\n\n{reason} Shutting down...");
    info!("{reason}");
    session.stop().await;
    if let Some(api) = signed_in {
        let _ = api.signout().await;
    }
    Ok(())
}
