# Streamroom

Low-latency, multi-stream screen sharing for a group of friends.

- **Streamers** open a link, type their name, click **Go Live**, and pick a screen or window. Nothing to install.
- **Viewers** open another link and see every live stream at once: **Grid**, **Spotlight** (one big, others in a strip) or **PiP** (one full, others floating), with per-stream volume and fullscreen.
- Media runs over WebRTC through [LiveKit](https://livekit.io), typically a few hundred milliseconds behind real time. Each viewer only downloads the quality their tile size needs, so watching several streams at once stays light.

```
streamer browser ──WebRTC──▶ LiveKit (SFU) ──WebRTC──▶ viewer browsers
        │                         ▲
        └── /api/token ──▶ Streamroom app (rooms, links, tokens, pages)
```

## Run it locally

You need **Node.js 20+** and the **LiveKit server** binary.

Install LiveKit:

| System  | Command |
|---------|---------|
| Arch / Linux | `curl -sSL https://get.livekit.io \| bash` |
| macOS   | `brew install livekit` |
| Windows | Download `livekit_<version>_windows_amd64.zip` from the [LiveKit releases](https://github.com/livekit/livekit/releases) and put `livekit-server.exe` on your PATH |

Then, in this folder:

```bash
npm install
npm run dev
```

Open **http://localhost:3000**, create a room, and open the streamer link in one tab and the viewer link in another. Use Chrome or Edge.

`npm run dev` starts LiveKit in dev mode (keys `devkey` / `secret`) and the app together. Ctrl+C stops both.

**Testing with friends locally:** viewers on your network can watch at `http://<your-ip>:3000/watch/...`. Browsers only allow *screen sharing* on `localhost` or `https`, though, so streamers on other machines need the server setup below.

## Deploy on Amazon Lightsail (from AWS CloudShell)

No keys to copy around: CloudShell runs as your signed-in AWS user.

1. In the AWS console, open **CloudShell** (the `>_` icon in the top bar).
2. **Actions → Upload file** and upload `streamroom.zip`.
3. Run (replace the domain with yours, or leave `DOMAIN=` off for a free `<ip>.sslip.io` address):

   ```bash
   unzip -oq streamroom.zip && DOMAIN=stream.example.com bash streamroom/deploy/aws/lightsail.sh
   ```

The script creates a Lightsail instance (Ubuntu, 2 GB plan by default, $12/month with 3 TB of transfer), attaches a free static IP, opens the ports, creates the DNS record if the domain is in Route 53 (otherwise it shows you the record to add and waits), copies the app over and installs it. It prints the **admin password** at the end.

Optional settings go before the command: `REGION=us-west-2`, `PLAN_GB=1` (the $7 plan) or `PLAN_GB=4`, `NAME=...`.

**Updating:** upload the new zip and run the same command. It reuses the server and keeps your keys, password and rooms.

**Removing everything:** delete the instance and the static IP (`streamroom` and `streamroom-ip`) in the Lightsail console, and the DNS record in Route 53.

Your AWS user needs permission to use Lightsail (and Route 53, for the automatic DNS record).

## Deploy on your own Linux server (Arch or Ubuntu/Debian)

1. Point a domain at the server (DNS A record), for example `stream.example.com`.
2. Copy this folder to the server and run:

   ```bash
   sudo bash deploy/linux/setup.sh stream.example.com
   ```

   The script installs Node, Caddy and LiveKit, generates keys and an **admin password** (printed at the end, stored in `/etc/streamroom.env`), sets up HTTPS automatically through Caddy, and starts everything as systemd services.
3. Make sure these ports are open, including on your router or hosting provider's firewall:

   | Port | Why |
   |------|-----|
   | 80, 443 TCP | Web app, HTTPS certificates, LiveKit signaling |
   | 7881 TCP | Media fallback when UDP is blocked |
   | 7882 UDP | Media (main path, lowest latency) |

A home server also needs those ports forwarded on your router, and your home upload speed limits how many people can watch.

Then visit `https://stream.example.com`. Creating rooms asks for the admin password; streamer and viewer links work without it.

**Updating:** copy the new code over and run the same `setup.sh` command again. It keeps your keys, password and rooms.

**Logs:** `journalctl -u streamroom -u livekit -u caddy -f`

**What goes where:**

| Path | Contents |
|------|----------|
| `/opt/streamroom` | App code |
| `/etc/streamroom.env` | App settings and secrets |
| `/etc/livekit/livekit.yaml` | LiveKit config |
| `/etc/caddy/conf.d/streamroom.caddy` | HTTPS site config |
| `/var/lib/streamroom/rooms.json` | Rooms |

## How links work

- **Streamer link:** `/live/<room>#k=<key>`. The key sits after `#`, so it never shows up in server logs. Anyone holding it can go live in that room.
- **Viewer link:** `/watch/<room>`. Watch-only, cannot publish anything.
- If a streamer link leaks, rotate its key: `POST /api/rooms/<room>/rotate` with `{"streamKey": "<current key>"}` returns a new key, and the old link stops working.

## Tuning

Encoding profiles live in `public/live.html` (`profile()`):

- **Game or video:** 1080p, up to 60 fps, 8 Mbps. Keeps frame rate when bandwidth drops.
- **Text or code:** 1080p, 30 fps, 5 Mbps. Keeps sharpness when bandwidth drops.

Streams use **VP8** by default, or **H.264** when the browser reports a hardware (graphics card) H.264 encoder. Software H.264 with two layers dropped most frames in testing. Add `?codec=vp8` (or `vp9`, `av1`, `h264`) to a streamer link, before the `#`, to force a codec for testing.

Each stream sends two layers: full quality plus a 360p copy. Viewers automatically get the layer that fits their tile, and a layer nobody is watching pauses to save the streamer's upload.

**Find the best settings for this computer** (Go Live page) captures the screen once and test-streams it into a private side room with six setups, a few seconds each, then saves the one that sent the most frames. Run it with the game moving.

**Advanced settings** on the Go Live page (saved per browser) let a streamer override resolution (including native, no scaling), frame rate, quality layers, codec, what to sacrifice when struggling, and max bitrate. Turning quality layers off lets Chrome and Edge use the graphics card's H.264 encoder, at the cost of every viewer receiving the full-quality stream.

While live, the Go Live page shows capture fps, sent fps and which encoder is in use. It warns the streamer if the screen capture, their computer, their upload or a blocked UDP port is the bottleneck. **Copy diagnostics** on the Go Live page and **Stats** on the viewer page copy detailed numbers to the clipboard for troubleshooting.

**Bandwidth rough guide:** a streamer in game mode uploads up to about 9 Mbps. Server egress is roughly *viewers × streams on screen × layer bitrate*. For example, 10 viewers in grid view of 4 streams at 720p (about 2.5 Mbps each) is about 100 Mbps.

## Troubleshooting

- **Black stream from a fullscreen game:** switch the game to borderless windowed.
- **No game audio:** in Chrome on Windows, share the *entire screen* and tick "Share system audio". On macOS, system audio sharing depends on the Chrome and macOS versions; tab audio always works.
- **Stuck on "Connecting…" in production:** check 7882/udp and 7881/tcp are open (Lightsail: instance → Networking tab), and that `livekit` is running (`systemctl status livekit`).
- **Viewers on strict corporate or school networks can't connect:** they need a TURN relay. LiveKit has one built in. See the `turn:` section in the LiveKit config docs, add it to `/etc/livekit/livekit.yaml`, then `systemctl restart livekit`.

## Project layout

```
server/index.js        rooms, tokens, static files
public/index.html      create a room
public/live.html       Go Live page (streamers)
public/watch.html      multi-stream viewer
public/app.css, common.js
scripts/dev.js         npm run dev (LiveKit + app)
deploy/linux/          setup.sh, systemd units, Caddyfile, livekit.yaml
deploy/aws/            lightsail.sh (CloudShell installer)
```
