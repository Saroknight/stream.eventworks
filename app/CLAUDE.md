# Streamroom Live (`app/`): notes for Claude Code

Loaded when Claude works on files under `app/`. Repo-wide rules (working agreements, CI and
releasing, deploy) are in the root `CLAUDE.md`.

## Streamroom Live: how capture works

### Windows
DXGI Desktop Duplication → D3D11 video processor (scale + NV12 on GPU) → Media Foundation
hardware H.264 (NVENC/AMF/QuickSync), low latency, no B-frames, keyframes on request → LiveKit
Rust SDK publishes the encoded frames. Sound: WASAPI loopback; per-app uses
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (include or exclude a process tree), which needs
Windows 10 build 20348+ / Windows 11, else it falls back to all PC audio and says so.
Sound modes: everything except Discord (default), only certain apps, all PC audio, none.

### Linux (Arch and derivatives; tested on CachyOS with GNOME and KDE, AMD GPU)
- **Screen:** Wayland → xdg-desktop-portal ScreenCast via `ashpd` 0.14 (restore token saved at
  `$XDG_STATE_HOME/streamroom-live/screencast-token`, `PersistMode::ExplicitlyRevoked`;
  Settings → Screen → "Ask every time" = monitor index 1). The portal thread keeps the session
  open until stop. X11 (no `WAYLAND_DISPLAY`) → `ximagesrc ! video/x-raw,framerate=N/1`.
- **Pipeline (GStreamer 0.25 bindings):**
  `pipewiresrc fd=<portal fd> <target> stream-properties="props,media.type=Video,…" … !
  capssetter (square pixels) ! queue ! convert ! [FrameGate probe] encoder !
  h264parse ! appsink`.
- **Encoders, tried in order** until one produces a frame: `vah264enc`/`vah264lpenc` (with
  `vapostproc`), `vaapih264enc`, `nvh264enc`, `x264enc`, `openh264enc`.
- **DMA-BUF (zero-copy) is already in use** with `vah264enc`/`vah264lpenc`: caps negotiation picks
  `memory:DMABuf` because `vapostproc` accepts it (checked on GNOME + AMD: `DMA_DRM` XR24 with a
  tiling modifier; about 3x less CPU in our process than forced system memory, and the
  compositor skips a GPU readback). The other encoders go through `videoconvert`, so they get
  system memory. Check what was negotiated with `GST_DEBUG=pipewiresrc:4` ("set format"). KDE
  not yet checked; NVIDIA (`nvh264enc`) has no DMA-BUF path.
- **Finding the portal stream:** `target-object=<serial>` (serial from the portal, or looked up on
  the portal's connection with the PipeWire device provider; GNOME doesn't report it), then
  `path=<node id>`, then no target. Failures list what was tried and what's visible on the
  connection.
- **Frame-rate cap:** `FrameGate`, a buffer probe at the encoder's input that drops raw frames by
  timestamp (see Lessons).
- **Sound:** PipeWire. App streams come from `pw-dump`; each wanted app is a
  `pipewiresrc target-object=<serial>` (with `media.type=Audio`) into an `audiomixer`; "all PC
  audio" is `pulsesrc @DEFAULT_MONITOR@`. A guard sets failed pipelines back to Null (they
  leaked threads and PipeWire streams before 0.4.7). `pw-dump` is killed after 2 s. Per-app
  capture failing 3 times in a row shares no sound for 30 s (all PC audio would include
  Discord), then tries per-app again.
- **Saving a copy** (`linux/record.rs`, Linux only, `CAN_RECORD`): the stream's own H.264 frames
  plus its sound (Opus) into `matroskamux` → `~/Videos/Streamroom/<date time>.mkv`; no second
  encode. Frames are handed over in `video.rs` `send()` / `audio.rs` before the network
  `try_send`, so network drops are still saved. The recorder sits in `Control` (a slot), so
  `Session::set_recording` turns it on/off mid-stream without a restart (on asks for a
  keyframe; the file starts at one). Sound is timed by sample count, re-synced to the clock
  after gaps; `matroskamux` (an aggregator) waits for both feeds, so the video thread fills in
  silence when no sound comes (per-app fallback pause, sound pipeline restarts). Failures show
  in the live view (`Stats::recording`) and the stream goes on. A killed app leaves a playable
  file (no seek index).
- **Updater on Linux:** downloads the package, verifies SHA256SUMS, `pkexec pacman -U`, relaunches.

### App behaviour shared by both
- Tray app, single instance; a second `streamroom://` link is handed to the running instance
  (with 10-second de-duplication; links are redacted in logs).
- After pairing, the app lists rooms, creates/ends rooms, goes live, opens rooms in the browser,
  signs out. Closing the window keeps streaming; Stop is in the window and the tray.
- Settings (height, fps, bitrate, screen, sound mode/apps, cursor) are saved and can be applied
  mid-stream. "Copy diagnostics" copies a JSON snapshot (version, status, settings, monitors,
  stats, log path).
- Logs: Linux `~/.local/share/click.eventworks.streamroom/logs/streamroom-live.log`, Windows
  under `%LOCALAPPDATA%\click.eventworks.streamroom\logs\` (the CLI: `streamroom-live.log` in the
  temp folder). Code logs with `tracing`; `core::log::init` sends it to the file through a
  background writer (our crates from `info`, others from `warn`, including `log`-crate users
  like LiveKit through the `tracing-log` bridge), and `log::flush()` must run before exiting
  (the app does it on `RunEvent::Exit`, the CLI before `process::exit`) or the last lines are
  lost. Capture threads report problems with `Control::report_error`, which just logs them.
- A dropped encoded frame (the network side is behind) requests a keyframe on both platforms;
  on Linux at most once a second (`KeyframeLimit`), since keyframes are big.

## Build, run, test

```
cd app
cargo build -p streamroom-cli --features testsrc     # console tool (+ test pattern)
cargo test -p streamroom-core -p streamroom-desktop -p wincap
cargo fmt --check && cargo clippy --workspace --all-targets --features streamroom-cli/testsrc -- -D warnings
cd desktop && npx @tauri-apps/cli@2 build            # the app (Windows: NSIS installer)
streamroom-live-cli --capture-test                   # capture + encode 5 s, no server
```
Linux build deps (Arch): `webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg gstreamer
gst-plugins-{base,good,bad,ugly} gst-plugin-pipewire gst-plugin-va pipewire xdg-desktop-portal`
plus `clang pkgconf openssl`. See `app-linux.yml` for the exact list.

## Lessons learned (don't relearn these)

- **PipeWire ≥1.6 + WirePlumber 0.5: set `media.type` on `pipewiresrc`.** Without it the stream
  registers as `Stream/Input/Unknown`, WirePlumber only links matching media types, and every
  target fails with "target not found" (screen and per-app sound). Found by Studdder (PR #1).
- **Screens have variable frame rates (0/1).** `videorate max-rate` lets everything through
  (a 165 Hz monitor sent 165 fps and starved the bitrate). A fixed `framerate=N/1` caps filter
  after it breaks negotiation on GNOME (0.4.6 regression). The fix is `FrameGate` at the encoder
  input; never drop *encoded* H.264 frames (later frames depend on them). `videorate` was removed
  in 0.4.9 as dead weight.
- **Tell the encoder the capped frame rate.** Encoders split their bitrate by the caps'
  frame rate; a PipeWire screen says `0/1` with `max-framerate` ≈360 fps (GNOME), and VA-API
  budgeted for that: 16 Mbps came out as 1.5 Mbps at 55 fps (≤0.4.15). A `capssetter
  framerate=<fps>/1` right before the encoder fixes it (downstream only, so negotiation with the
  screen is unaffected). Check with `GST_DEBUG=vah264enc:5` ("frame duration is").
- **`ximagesrc` defaults to 25 fps** unless the caps after it set a framerate (X11 only; the
  fixed rate there is safe, unlike the GNOME portal stream). Fixed in 0.4.9.
- **KDE can report pixel aspect 2147483647:1**, which overflows scalers: force square pixels
  with `capssetter` and spell out the output width and height.
- **GNOME's portal doesn't report the PipeWire serial;** look it up on the portal connection.
- **Encoders shift output timestamps by 1000 hours** (so decode times never go negative). To
  compare an encoded frame's PTS with the pipeline's running time, convert it with the sample's
  segment (`to_running_time`), or every latency comes out negative.
- **x264 refuses odd widths.** On X11 the screen size is read first (ximagesrc caps in Paused)
  so the output width is spelled out and even; left to the scaler it picked 1365x720.
- **One tokio runtime for all portal work, for the life of the app.** ashpd caches one D-Bus
  connection per process and zbus drives it from the runtime it was first used on. A runtime
  per session (≤0.4.14) made the second go-live in one run hang: no dialog, then "no screen was
  chosen in time" after 3 minutes (`portal_runtime` in `portal.rs`).
- **A failed GStreamer pipeline must be set to Null,** or it keeps threads and PipeWire streams.
- **`set_state(Playing)` failing hides the reason:** read the Error message off the bus.
- **Windows build features:** `wincap` needs `Win32_Security` (for `CreateEventW`).
- **Updater safety:** never updates mid-stream; always verifies SHA256SUMS first.
