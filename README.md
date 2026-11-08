# Streamroom

A private place for a group of friends to watch and share each other's screens, with low latency
and several streams on screen at once.

- **Lobby:** the home page lists every live room and who's streaming in it. Anyone can start a room.
- **One link per room:** everyone opens the same room page to watch, and clicks **Go Live** to share
  their screen, with the **Streamroom Live** app for Windows or Arch Linux (smooth 60 fps on the
  graphics card, picks which apps' sound to share) or, from a dropdown, straight from the browser.
- **The app does everything:** after one "Open Streamroom Live" it stays signed in, lists rooms,
  starts and ends rooms and goes live without the site.
- **Works on phones** for watching (stacked streams in portrait, full screen in landscape).
- **Rooms are temporary:** the person who started a room can end it for everyone. Otherwise it ends
  on its own 5 minutes after the last person leaves.
- **Invite only:** the admin sends each friend a personal invite link. No passwords.

```
browser / Streamroom Live ──WebRTC──▶ LiveKit (SFU) ──WebRTC──▶ everyone in the room
           │                          ▲
           └── sign-in, rooms ──────▶ Streamroom server (Node)
```

The full picture, with a diagram and the sign-in and go-live steps, is in
[docs/architecture.md](docs/architecture.md).

## Access

| Who | How |
|-----|-----|
| **Admin (you)** | The first sign-in link is printed when the server is installed. Lost access? Run `sudo streamroom-admin-link` on the server for a new one-time link. |
| **Friends** | On the **Admin** page, type their name and click **Create invite**, then send them the link. It signs in any of their devices for 7 days, then expires. Signed-in devices stay signed in for 90 days of inactivity. |
| **New device later** | Account menu (top right) → **Sign in another device** shows a QR code and a one-time link valid for 10 minutes. No admin needed. |

The Admin page also lists everyone with their devices (including signed-in apps, each
with a sign-out button), lets you rename or remove people (removing signs out all their devices
and drops them from live rooms), and shows every live room with buttons to remove a person or end
the room.

The **Health** section at the top of the Admin page shows server load, whether the video server
answers, whether the address it hands out matches the domain, whether the port 443 fallback is on,
and every recent connection problem (who, when, which browser or app, and what failed). Room pages
report failures there automatically and tell the person what went wrong in plain words.

**App sign-in:** **Go Live → Open Streamroom Live** launches the app with a single-use code
(10 minutes). The app trades it once for its own key, a revocable 90-day session (encrypted for
that Windows user on Windows). App keys can use rooms but never the admin pages.

## The app (Streamroom Live)

Get it from the site's **/download** page. **Windows:** an installer (not code-signed, so
SmartScreen asks once: **More info → Run anyway**). **Arch Linux / CachyOS:** one pacman command
shown on the page; works on Wayland (GNOME, KDE, Hyprland) and X11, encoding on AMD/Intel
(VA-API) or NVIDIA (NVENC), with x264 as a fallback. Both update themselves. Details, the
console tool and how it works: [`app/README.md`](app/README.md).

## Run it locally

You need **Node.js 20+** and the **LiveKit server** binary
(`curl -sSL https://get.livekit.io | bash` on Linux, `brew install livekit` on macOS, or the
[Windows release](https://github.com/livekit/livekit/releases)).

```bash
npm install
npm run dev          # LiveKit (dev mode) + the app, Ctrl+C stops both
npm run admin-link   # in a second terminal: your admin sign-in link
```

Open the printed link, set your name, then invite yourself on a second browser profile to try it
as a friend. Use Chrome or Edge. Browsers only allow screen sharing on `localhost` or `https`.

## Deploy on Amazon Lightsail (from AWS CloudShell)

In the AWS console, open **CloudShell** and run:

```bash
rm -rf stream.eventworks && git clone -q --depth 1 https://github.com/Saroknight/stream.eventworks \
  && DOMAIN=stream.example.com bash stream.eventworks/deploy/aws/lightsail.sh
```

The first run creates a Lightsail instance (Ubuntu, $12/month plan with 3 TB of transfer), a
static IP, the firewall rules and the DNS record (if the domain is in Route 53), installs everything and prints your **admin sign-in link**. Running the same command again deploys the
latest code from GitHub and keeps your people and settings.

Options before the command: `REGION=us-west-2`, `PLAN_GB=1` ($7 plan) or `4`, `NAME=...`.

## Deploy on your own Linux server (Arch or Ubuntu/Debian)

1. Point a domain at the server, and open ports **80, 443, 7881 TCP** and **7882 UDP**
   (plus **3478 UDP** and a `turn.` DNS name if you turn on the optional relay below).
2. From a checkout of this repository: `sudo bash deploy/linux/setup.sh stream.example.com [server-ip]`

Caddy provides HTTPS automatically. Logs: `journalctl -u streamroom -u livekit -u caddy -f`

Site changes go live only when you re-run the deploy command (or `setup.sh`). App releases don't
need a deploy: installed apps update from GitHub Releases.

**Port 443 fallback (optional, off by default, not yet proven in production).** Some networks
(work, school, strict routers) block the normal video ports. With `RELAY_443=on` before the deploy
command, and `turn.<domain>` pointing at the server, setup turns on LiveKit's TURN relay and shares port 443
between the site and the relay, told apart by name. That needs the Caddy build with the
[layer4](https://github.com/mholt/caddy-l4) module, which setup downloads from caddyserver.com.
The new config is validated first; on any problem setup keeps the normal config and says why.
The CI job **Website: E2E tests → Relay E2E** (run by hand) checks it end to end with the video ports blocked,
and the admin Health panel's **Test** button checks it on the live server.

| Path | Contents |
|------|----------|
| `/opt/streamroom` | App code |
| `/etc/streamroom.env` | Settings and secrets |
| `/var/lib/streamroom/state.json` | People, devices, invites (tokens stored only as hashes) |
| `/etc/livekit/livekit.yaml` | LiveKit config |

Rooms aren't stored anywhere: they exist only in LiveKit while they're live.

## Browser streaming

Streaming from the browser is the fallback to the app. The **Go Live** dialog has a Game/Text
choice, and under **Browser streaming options**: resolution, frame rate, quality layers, codec,
bitrate, and **Find the best settings**, which test-streams six setups privately and keeps the
smoothest. While live, a bar shows the frame rate and encoder and warns when the capture, the
computer, the upload or a blocked UDP port is the bottleneck.

## Project layout

```
server/index.js      web server, pages
server/auth.js       invite/device/admin links, sessions
server/rooms.js      rooms (LiveKit), tokens, app links
server/admin.js      admin API
server/health.js     health panel and connection-problem reports
server/appinfo.js    latest app release for /download
server/store.js      people, sessions, invites (JSON file)
server/cli.js        admin-link command
public/              lobby, room, join, admin and download pages
tests/e2e.mjs        end-to-end test (CI on every push; npm run test:e2e)
tests/relay.mjs      port 443 fallback test (CI, run by hand, after a real setup.sh install)
app/                 Streamroom Live (Windows and Linux app, Rust + Tauri), see app/README.md
deploy/              Lightsail script, Linux setup, Caddy, systemd, LiveKit config
docs/                how it works: architecture diagram, steps, passes and ports
CLAUDE.md            notes for working on the code (architecture, CI, releasing, lessons learned)
```

## Tests and CI

| Workflow | When | What |
|---|---|---|
| **Website: E2E tests** | every push outside `app/` (not docs), and pull requests | `tests/e2e.mjs`: LiveKit + the site + real Chromium through sign-in, invites, rooms, streaming, the app hand-off, Health and admin removal |
| **Client app: Windows build** | changes to Windows or shared app code | installer + console tool; publishes `app-vX.Y.Z` when `app/release/windows` is bumped |
| **Client app: Linux build** | changes to Linux or shared app code | unit tests, app, console tool, pacman package; publishes `linux-vX.Y.Z` when `app/release/linux` is bumped |

Run the site test locally with `npm run test:e2e` (needs `livekit-server` and
`npx playwright install chromium`).
