# Streamroom

A private place for a group of friends to watch and share each other's screens, with low latency
and several streams on screen at once.

- **Lobby:** the home page lists every live room and who's streaming in it. Anyone can start a room.
- **One link per room:** everyone opens the same room page to watch, and clicks **Go Live** to share
  their screen, either with the **Streamroom Live** Windows app (smooth 60 fps, uses the graphics
  card) or straight from the browser.
- **Rooms are temporary:** the person who started a room can end it for everyone. Otherwise it ends
  on its own 5 minutes after the last person leaves.
- **Invite only:** the admin sends each friend a personal invite link. No passwords.

```
browser / Windows app ──WebRTC──▶ LiveKit (SFU) ──WebRTC──▶ everyone in the room
           │                          ▲
           └── sign-in, rooms ──▶ Streamroom server (Node)
```

## Access

| Who | How |
|-----|-----|
| **Admin (you)** | The first sign-in link is printed when the server is installed. Lost access? Run `sudo streamroom-admin-link` on the server for a new one-time link. |
| **Friends** | On the **Admin** page, type their name and click **Create invite**, then send them the link. It signs in any of their devices for 7 days, then expires. Signed-in devices stay signed in for 90 days of inactivity. |
| **New device later** | Account menu (top right) → **Sign in another device** shows a QR code and a one-time link valid for 10 minutes. No admin needed. |

The Admin page also lists everyone with their devices, lets you rename or remove people (removing
signs out all their devices and drops them from live rooms), and shows every live room with buttons
to remove a person or end the room.

The Windows app has no sign-in of its own: **Go Live → Copy app link** gives it a link that
identifies you for 12 hours.

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
static IP, the firewall rules and the DNS record (if the domain is in Route 53), installs
everything and prints your **admin sign-in link**. Running the same command again deploys the
latest code from GitHub and keeps your people and settings.

Options before the command: `REGION=us-west-2`, `PLAN_GB=1` ($7 plan) or `4`, `NAME=...`.

## Deploy on your own Linux server (Arch or Ubuntu/Debian)

1. Point a domain at the server and open ports **80, 443, 7881 TCP** and **7882 UDP**.
2. From a checkout of this repository: `sudo bash deploy/linux/setup.sh stream.example.com`

Caddy provides HTTPS automatically. Logs: `journalctl -u streamroom -u livekit -u caddy -f`

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
server/store.js      people, sessions, invites (JSON file)
server/cli.js        admin-link command
public/              lobby, room, join and admin pages
app/                 Streamroom Live (Windows app, Rust)
deploy/              Lightsail script, Linux setup, Caddy, systemd, LiveKit config
```
