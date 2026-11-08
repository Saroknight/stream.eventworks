# Streamroom Live (Windows and Linux)

Streams your screen into a Streamroom room using your graphics card, for much smoother
frame rates than browser screen sharing. Windows 10/11, and Arch Linux and Arch-based distros
(CachyOS, EndeavourOS, Manjaro) on Wayland or X11.

## Install

**Linux (Arch, CachyOS and other Arch-based distros):**
```
curl -L -o streamroom-live-x86_64.pkg.tar.zst https://stream.eventworks.click/api/app/download/linux && sudo pacman -U streamroom-live-x86_64.pkg.tar.zst
```
pacman pulls in GStreamer (VA-API for AMD/Intel, x264 as a fallback), PipeWire and the screen-share
portal. On Wayland the desktop asks once which screen or window to share; the choice is remembered
(Settings → Screen → *Ask every time* to pick again). Updates install through pacman (password
prompt). NVIDIA: also install `gst-plugins-bad` for NVENC.

**Windows:**

Download **Streamroom-Live-Setup.exe** from the site's `/download` page (or this repo's
latest GitHub Release) and run it. It installs per-user, so no admin prompt. Windows SmartScreen
may warn that the app is unrecognized (it isn't code-signed): click **More info → Run anyway**.

## Use it

**First time:** in a room on the site, click **Go Live**, then **Open Streamroom Live**. The
browser launches the app through the `streamroom://` link the installer registers, with a
single-use sign-in code. The app trades it for its own key (revocable from the admin page; on
Windows stored encrypted with DPAPI) and goes live. On Linux the pacman package registers the
`streamroom://` link with the desktop.

**After that** the app works on its own: it lists the rooms, **starts a room and goes live**,
**goes live** in any room, opens a room in the browser to **watch**, **ends** rooms you started
(or any room, for the admin), and **signs out** from the ⋯ menu. While live it shows the room,
how many others are in it, frame rates, bitrate and a Stop button. Closing the window keeps you
live (the app stays in the tray); right-click the tray icon to stop.

**Sound** (Settings): *Everything except Discord* (default, so viewers don't hear your voice chat
echoed back), *Only certain apps* (tick them from the apps playing sound now), *All PC audio*, or
*No sound*. Per-app capture needs Windows 10 build 20348+ or Windows 11; older Windows shares all
PC audio and says so. On Linux it uses PipeWire (apps are matched by the program name PipeWire
reports). Installs from before this setting keep sharing all PC audio until changed.

**Save a copy (Linux):** Settings → *Save a copy on this computer* also saves what you stream to
`~/Videos/Streamroom` as an MKV file (H.264 video as streamed, Opus sound), one file per go-live,
named by date and time. While live, *Save a copy* / *Stop saving* turns it on or off without
interrupting the stream. If saving fails (disk full, say) the live view says why and the stream
carries on.

**Screen on Linux (Wayland):** the desktop's own dialog picks the screen or window the first time;
the app remembers it. Settings → Screen → *Ask every time* shows the dialog on every go-live.

Settings (quality, frame rate, bitrate, screen, sound, mouse cursor) are saved and can be
applied mid-stream. **Copy diagnostics** at the bottom copies everything useful for a bug report.

The app checks GitHub Releases for updates and shows **Update now** when one is out (never
mid-stream). Updates are verified against the release's `SHA256SUMS` before they run. On Linux
the update installs through pacman, so the desktop asks for your password.

**Logs:** Linux `~/.local/share/click.eventworks.streamroom/logs/streamroom-live.log`; Windows
under `%LOCALAPPDATA%\click.eventworks.streamroom\logs\`.

## Troubleshooting (Linux)

- **Check capture without a server:** `streamroom-live-cli --capture-test` opens the screen
  dialog, captures and encodes for 5 seconds, and prints the capture method, encoder and frame
  count.
- **"couldn't start video with any encoder":** the message lists each encoder and way of finding
  the screen that was tried, and what PipeWire showed on the screen-share connection. Send it
  along with the log.
- **No hardware encoder:** AMD/Intel need `gst-plugin-va` (pulled in by the package); NVIDIA needs
  `gst-plugins-bad` for NVENC. Without them it falls back to x264 on the CPU.
- **Blurry at high refresh rates:** the app caps frames at the set rate (60 by default); if fast
  scenes are still soft, raise the bitrate in Settings.
- After a system update that touched PipeWire or the desktop, log out and back in before testing.

## Layout

| Folder | What |
|---|---|
| `wincap/` | Capture and hardware encoding: Windows (DXGI, Media Foundation, WASAPI) and Linux (`src/linux/`: portal/PipeWire or X11, GStreamer, PipeWire sound) |
| `core/` | Sign-in and the server API (`api.rs`), links, publishing the stream, capture test |
| `desktop/` | The Tauri app: window (`ui/`), tray, deep links, updater, installer config |
| `cli/` | `streamroom-live-cli`, the same engine from a terminal (troubleshooting) |
| `packaging/arch/` | PKGBUILD and .desktop file for the Arch package (built by CI) |
| `release/` | Each platform's version number (see Releasing) |

## Command-line tool

```
streamroom-live-cli "streamroom://open?server=...&room=...&pair=..." [options]
streamroom-live-cli --capture-test
```

The link is the one the site's Go Live dialog opens the app with (it signs in for that run only).
Without a link it asks for one.

| Option | Meaning |
|---|---|
| `--height 1080` | Output height: 1080 (default), 720, 1440, or 0 for native |
| `--fps 60` | Frame rate |
| `--bitrate 8` | Video bitrate in Mbps |
| `--monitor 0` | Which monitor (0 = primary) |
| `--no-audio` | Don't share PC audio |
| `--no-cursor` | Don't draw the mouse cursor |
| `--record` | Also save a copy in `~/Videos/Streamroom` (Linux) |
| `--list-monitors` | List monitors and exit |
| `--test-pattern` | Send a test pattern instead of the screen (troubleshooting) |
| `--capture-test` | Capture and encode the screen for 5 seconds, no server needed |

## Releasing

Windows and Linux are built and released separately, each with its own version:

| | Version file | Workflow | Release tag |
|---|---|---|---|
| Windows | `app/release/windows` | `.github/workflows/app-windows.yml` | `app-vX.Y.Z` (marked latest) |
| Linux | `app/release/linux` | `.github/workflows/app-linux.yml` | `linux-vX.Y.Z` |

Each workflow only runs when its platform's files or the shared code change (a change under
`wincap/src/linux/` doesn't build Windows; a change to Windows capture doesn't build Linux; `core`,
`desktop` and the UI build both). To release a platform, bump its version file and push to
`main`: the build is published if that version has no release yet. Otherwise the build is just
attached to the workflow run. Installed apps only look at their own platform's releases. The
`version` in `app/Cargo.toml` is only for local builds; CI replaces it with the platform's.

## How it works

**Windows**
- **Capture:** DXGI Desktop Duplication (works on all Windows 10/11 versions, keeps up with
  the monitor's refresh rate). The image stays on the GPU.
- **Convert:** D3D11 video processor scales and converts to NV12 on the GPU.
- **Encode:** Media Foundation hardware H.264 encoder (NVIDIA NVENC, AMD AMF or Intel
  QuickSync), low-latency mode, no B-frames, keyframes when a viewer needs one.
- **Audio:** WASAPI loopback; per-app sound with process loopback (include or exclude apps).

**Linux**
- **Capture:** on Wayland, the desktop's screen-share portal hands over a PipeWire stream
  (`pipewiresrc`, with `media.type=Video` set so WirePlumber links it); on X11, `ximagesrc`.
- **Encode:** GStreamer, first that works of VA-API (`vah264enc`, `vaapih264enc`), NVENC
  (`nvh264enc`), `x264enc`, `openh264enc`. Frames over the set rate are dropped before the
  encoder (screens can send 144-165 fps).
- **Audio:** PipeWire: each chosen app's stream mixed together, or the default output's monitor
  for all PC audio.

**Both:** the LiveKit Rust SDK sends the already-encoded H.264 frames, no re-encoding.

## Build

Builds run on GitHub Actions (`app-windows.yml`, `app-linux.yml`); download the
`streamroom-live-windows` or `streamroom-live-linux` artifact from the workflow run. Locally:

```
cargo build -p streamroom-cli --features testsrc   # console tool (+ test pattern)
cargo test -p streamroom-core -p streamroom-desktop -p wincap
cd desktop && npx @tauri-apps/cli@2 build          # the app (installer on Windows)
```

`--features testsrc` adds the software test pattern. Linux build dependencies are listed in
`.github/workflows/app-linux.yml`.
