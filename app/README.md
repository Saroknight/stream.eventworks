# Streamroom Live (Windows)

Streams your screen into a Streamroom room using your graphics card, for much smoother
frame rates than browser screen sharing.

## Install

**Linux (Arch, CachyOS and other Arch-based distros):**
```
curl -LO https://github.com/Saroknight/stream.eventworks/releases/latest/download/streamroom-live-x86_64.pkg.tar.zst && sudo pacman -U streamroom-live-x86_64.pkg.tar.zst
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
single-use sign-in code. The app trades it for its own key (stored encrypted with Windows DPAPI,
revocable from the admin page) and goes live.

**After that** the app works on its own: it lists the rooms, **starts a room and goes live**,
**goes live** in any room, opens a room in the browser to **watch**, **ends** rooms you started
(or any room, for the admin), and **signs out** from the ⋯ menu. While live it shows the room,
how many others are in it, frame rates, bitrate and a Stop button. Closing the window keeps you
live (the app stays in the tray); right-click the tray icon to stop.

**Sound** (Settings): *Everything except Discord* (default, so viewers don't hear your voice chat
echoed back), *Only certain apps* (tick them from the apps playing sound now), *All PC audio*, or
*No sound*. Per-app capture needs Windows 10 build 20348+ or Windows 11; older Windows shares all
PC audio and says so. Installs from before this setting keep sharing all PC audio until changed.

Settings (quality, frame rate, bitrate, screen, sound, mouse cursor) are saved and can be
applied mid-stream. **Copy diagnostics** at the bottom copies everything useful for a bug report.

The app checks GitHub Releases for updates and shows **Update now** when one is out (never
mid-stream). Updates are verified against the release's `SHA256SUMS` before they run.

## Layout

| Folder | What |
|---|---|
| `wincap/` | Capture and hardware encoding: Windows (DXGI, Media Foundation, WASAPI) and Linux (`src/linux/`: portal/PipeWire or X11, GStreamer, PipeWire sound) |
| `packaging/arch/` | PKGBUILD and .desktop file for the Arch package (built by CI) |
| `core/` | Sign-in and the server API (`api.rs`), links, and publishing the stream |
| `desktop/` | The Tauri app: window (`ui/`), tray, deep links, updater, installer config |
| `cli/` | `streamroom-live-cli.exe`, the same engine from a terminal (troubleshooting) |

## Command-line tool

```
streamroom-live-cli.exe "https://stream.example.com/live/ROOM#k=KEY" [options]
```

| Option | Meaning |
|---|---|
| `--height 1080` | Output height: 1080 (default), 720, 1440, or 0 for native |
| `--fps 60` | Frame rate |
| `--bitrate 8` | Video bitrate in Mbps |
| `--monitor 0` | Which monitor (0 = primary) |
| `--no-audio` | Don't share PC audio |
| `--no-cursor` | Don't draw the mouse cursor |
| `--list-monitors` | List monitors and exit |
| `--test-pattern` | Send a test pattern instead of the screen (troubleshooting) |

## Releasing

Bump `version` in `app/Cargo.toml` and push to `main`. The **Windows app** workflow builds the
installer and, because that version has no release yet, publishes it as `app-vX.Y.Z`. Installed
apps pick it up within a few hours (or at their next start). If publishing fails, any later push
that touches `app/` tries again, as long as that version has no release yet. Pushes that don't change the
version just build, with the installer attached to the workflow run.

## How it works

- **Capture:** DXGI Desktop Duplication (works on all Windows 10/11 versions, keeps up with
  the monitor's refresh rate). The image stays on the GPU.
- **Convert:** D3D11 video processor scales and converts to NV12 on the GPU.
- **Encode:** Media Foundation hardware H.264 encoder (NVIDIA NVENC, AMD AMF or Intel
  QuickSync), low-latency mode, no B-frames, keyframes when a viewer needs one.
- **Publish:** the LiveKit Rust SDK sends the already-encoded frames, no re-encoding.
- **Audio:** WASAPI loopback of the default output device.

## Build

Windows builds run on GitHub Actions (`.github/workflows/app.yml`); download the
`streamroom-live-windows` artifact from the workflow run. Locally:

```
cargo build --release
```

`--features testsrc` adds the software test pattern.
