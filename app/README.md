# Streamroom Live (Windows)

Streams your screen into a Streamroom room using your graphics card, for much smoother
frame rates than browser screen sharing.

## Install

Download **Streamroom-Live-Setup.exe** from the site's `/download` page (or this repo's
latest GitHub Release) and run it. It installs per-user, so no admin prompt. Windows SmartScreen
may warn that the app is unrecognized (it isn't code-signed): click **More info → Run anyway**.

## Use it

In a room, click **Go Live**, then **Open the app**. The browser hands the room to the app
through the `streamroom://` link the installer registers, and you're live under your
account's name. The window shows LIVE status, frame rates, bitrate and a Stop button.
Closing it keeps you live (the app stays in the tray); right-click the tray icon to stop.

Settings (quality, frame rate, bitrate, screen, PC audio, mouse cursor) are saved and can be
applied mid-stream. **Copy diagnostics** at the bottom copies everything useful for a bug report.

The app checks GitHub Releases for updates and shows **Update now** when one is out (never
mid-stream). Updates are verified against the release's `SHA256SUMS` before they run.

## Layout

| Folder | What |
|---|---|
| `wincap/` | Windows capture, cursor drawing, GPU conversion and hardware encoding |
| `core/` | Joining a room from an app link and publishing the stream |
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

Bump `version` in `app/Cargo.toml`, push, then push a tag `app-vX.Y.Z` with the same version.
The **Windows app** workflow builds the installer and publishes the release.

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
