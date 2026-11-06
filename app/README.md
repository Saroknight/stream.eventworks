# Streamroom Live (Windows)

Streams your screen into a Streamroom room using your graphics card, for much smoother
frame rates than browser screen sharing.

## Use it

1. On the Go Live page, copy your **streamer link** (the one containing `/live/`).
2. Run `streamroom-live.exe` and paste the link when asked, or pass it directly:

   ```
   streamroom-live.exe "https://stream.example.com/live/ROOM#k=KEY"
   ```

3. Type your name. You're live. A status line shows capture and sent frame rates,
   bitrate and encoding time. Press Ctrl+C to stop.

Windows SmartScreen may warn that the app is unrecognized (it isn't code-signed):
click **More info → Run anyway**.

## Options

| Option | Meaning |
|---|---|
| `--height 1080` | Output height: 1080 (default), 720, 1440, or 0 for native |
| `--fps 60` | Frame rate |
| `--bitrate 8` | Video bitrate in Mbps |
| `--monitor 0` | Which monitor (0 = primary) |
| `--no-audio` | Don't share PC audio |
| `--name NAME` | Name shown to viewers |
| `--test-pattern` | Send a test pattern instead of the screen (troubleshooting) |

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
