# Changelog

Newest first. Windows releases are tagged `app-vX.Y.Z`, Linux releases `linux-vX.Y.Z`.

- Linux 0.4.15: fixed going live a second time without restarting the app (stuck on
   Connecting, then "no screen was chosen in time"; the screen-share portal connection died
   with the first stream's runtime). While connecting, the window says the desktop may ask
   which screen to share.
- Linux 0.4.14: *Save a copy on this computer*: the stream (as sent, with sound) saved to
   `~/Videos/Streamroom` as MKV, on/off in Settings and from the live view without restarting
   the stream, failures shown in the live view; CLI `--record`. Windows gets the shared code
   (setting hidden there), no Windows bump.
- Linux 0.4.13: keyframe requests after dropped frames at most once a second, `log`-crate
   warnings (LiveKit, WebRTC, Tauri) in the log file, the unused capture error list removed,
   DMA-BUF use documented (it was already on with VA-API). Windows still has the log bridge and
   error-list change unreleased (no Windows bump).
- Windows 0.4.5 / Linux 0.4.12: logging through `tracing` with a non-blocking file writer (was
   opening the file for every line), typed sound setting `AudioChoice` (same saved values),
   Linux asks for a keyframe when a frame is dropped, Linux per-app sound fallback shares
   silence instead of all PC audio.
- Linux 0.4.11: real capture-to-encoded time (was always 0 ms), even output width on X11,
   typed error for the PipeWire node lookup, `video.rs` split into `encoder.rs`/`gate.rs`/`portal.rs`.
- Windows 0.4.4 / Linux 0.4.10: unwrap lint for every crate, typed "video stopped" reason
   (`Control::report_stopped`), Linux device names from the kernel host name, dropped errors
   logged.
- Linux 0.4.9: X11 streams at the chosen fps (was stuck at 25), `pw-dump` timeout, per-app sound
   fallback is temporary instead of permanent, `videorate` removed.
- Linux 0.4.8: quality pass (fmt/clippy in CI, shared GStreamer pipeline helper, encoder enum,
   real frame counters, 0600 `account.dat`, updater off the async runtime) and "Check for
   updates" in the ⋯ menu.
- GNOME fixes: media type (0.4.5, Studdder), frame cap (0.4.6, regressed; fixed 0.4.7 by
   Studdder with FrameGate), audio pipeline leak (0.4.7).
- Split Windows and Linux into separate builds and releases (Linux 0.4.3+).
- Linux app for Arch/CachyOS (portal + PipeWire + GStreamer, pacman package, self-updates).
- Improvements: the app does everything after one sign-in, plain-English errors, admin Health
   panel, CI e2e, per-app sound (default: everything except Discord), phone layout, app-first
   Go Live. Port 443 relay built, then parked.
- Fixed friends not connecting (stale LiveKit IP → `node_ip`).
- Windows app (Rust/Tauri): GPU capture/encode, pre-encoded publish, tray, deep links, updater.
- Site: invite-only lobby, rooms in LiveKit, browser streaming with tuning, admin page.
