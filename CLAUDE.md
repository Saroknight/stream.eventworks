# Streamroom: notes for Claude Code

Read this first. It's the working knowledge for this repo: what it is, how the pieces fit, how
to build, test, release and deploy, the decisions already made, and the traps already found.
`README.md` (site, deploy) and `app/README.md` (the native app) are the user-facing docs; this
file is for whoever is changing the code. Keep it current when something here stops being true.

## Working agreements

- **Two people commit to this repo** (Saroknight, the owner, and Studdder). Before any change:
  `git pull`, read `git log` for what's new, and check open pull requests
  (`gh pr list`). Never build on stale code or redo someone else's work.
- **Pushing straight to `main` is fine.** No PR needed for our own changes. Say what you're
  pushing before you push.
- **Pushes cost CI time.** Each app platform only builds when its files change (see Releasing).
  Don't push half-finished app work that triggers a slow Windows build for nothing, and don't
  commit stray files (test pid files, logs, screenshots) inside `app/`: anything under `app/`
  that isn't excluded counts as shared code and builds both platforms.
- **Never ask for or paste secrets** (AWS keys, GitHub tokens, LiveKit secrets). Deploys run in
  AWS CloudShell; GitHub access is through `gh`/the user's own login.
- **Present options neutrally.** When there's a choice, lay out the options with trade-offs and a
  recommendation if asked; don't push one.
- **No code signing.** The Windows installer is unsigned on purpose (SmartScreen "Run anyway"
  is documented on the download page). The Linux package is installed with pacman.
- **User-facing text is plain English.** Errors in the site and app say what happened and what
  to do, not error codes. Keep that style.
- **When stopped, stop.** If the owner says to stop and wait, do exactly that.

## What it is

A private, invite-only site for a group of friends to watch each other's screens (mostly games)
with low latency, several streams at once. Live at **https://stream.eventworks.click**.

```
browser / Streamroom Live app ──WebRTC──▶ LiveKit (SFU) ──WebRTC──▶ everyone in the room
           │                                   ▲
           └── sign-in, rooms, tokens ──▶ Streamroom server (Node/Express)
```

Two halves:

1. **The site** (`server/`, `public/`): Node 20+/Express 5, no build step, no database (a JSON
   file). LiveKit does all media. Deployed to one AWS Lightsail box behind Caddy.
2. **Streamroom Live** (`app/`): a native Rust + Tauri 2 app for Windows and Arch Linux that
   captures the screen and sound, hardware-encodes H.264 and publishes pre-encoded frames to
   LiveKit with the LiveKit Rust SDK. This is the main way to go live; browser screen sharing
   is the fallback.

## Repo map

```
server/index.js      Express app, page routes, /api mounts
server/config.js     env settings (PORT, PUBLIC_URL, LIVEKIT_*, AUTH_SECRET, DATA_DIR, ROOM_EMPTY_SECONDS)
server/auth.js       sessions (cookie or app Bearer key), invites, device links, admin link, rate limit
server/tickets.js    signed short-lived HMAC tickets (pair codes, etc.)
server/store.js      people, sessions, invites, health events (JSON file, tokens stored hashed)
server/rooms.js      rooms (live in LiveKit only), LiveKit tokens, app pairing + app API
server/admin.js      admin API (invites, people, devices, rooms)
server/health.js     admin Health panel data, client problem reports
server/appinfo.js    latest app releases from GitHub (cached), /api/app/download[/linux]
server/cli.js        `npm run admin-link`
public/              pages: index (lobby), room, join, admin, download; room.js is the big one
tests/e2e.mjs        end-to-end test (real LiveKit + Chromium via Playwright)
tests/relay.mjs      port 443 relay test (parked feature)
deploy/aws/lightsail.sh   one-command Lightsail deploy from CloudShell (re-run = update)
deploy/linux/setup.sh     installer/updater for Arch or Ubuntu/Debian (Caddy, systemd, LiveKit)
deploy/linux/*            Caddyfiles, livekit.yaml template, systemd units
app/                 Streamroom Live (see below)
.github/workflows/   site.yml (e2e), app-windows.yml, app-linux.yml
```

### `app/` (Cargo workspace)

| Crate | What |
|---|---|
| `core/` | Server API client (`api.rs`: pair, me, rooms, create/end room, app token, sign out, report), app link parsing (`parse_launch`), `Settings`, `Session` (connect to LiveKit, publish video + audio), `capture_test`, logging |
| `wincap/` | Capture + encode. Windows: `capture.rs` (DXGI duplication), `convert.rs`/`d3d.rs` (GPU NV12), `encoder.rs` (Media Foundation H.264), `audio.rs` (WASAPI, per-app process loopback), `pipeline.rs`, `cursor.rs`. Linux: `src/linux/` (`portal.rs`, `video.rs`, `audio.rs`, `launch.rs` (shared GStreamer pipeline setup, shutdown, bus errors), `mod.rs`) |
| `desktop/` | The Tauri app: `main.rs` (commands, deep links, single instance, tray, account), `account.rs` (key storage: DPAPI on Windows, a 0600 file on Linux), `update.rs` (self-updater), `ui/` (plain HTML/JS/CSS window) |
| `cli/` | `streamroom-live-cli`, same engine from a terminal (troubleshooting, CI e2e, `--capture-test`) |
| `packaging/arch/` | PKGBUILD + .desktop file (registers `x-scheme-handler/streamroom`) |
| `release/` | `windows` and `linux`: each platform's version number (see Releasing) |

## Auth model (server)

- **Admin:** bootstrap link from `npm run admin-link` (or `sudo streamroom-admin-link` on the
  server). Admin can invite, rename, remove people, sign out devices, end rooms.
- **Friends:** personal invite link (7 days). Redeeming it signs that device in (90-day sliding
  cookie session). "Sign in another device" gives a QR/one-time link (10 min).
- **CSRF:** cookie is SameSite=Lax and every state-changing API call must be JSON
  (`jsonOnly`), so forms from other sites can't call it.
- **Rate limit:** small per-IP limiter on sign-in endpoints (`rateLimit` in `auth.js`).
- **The app:** Go Live → "Open Streamroom Live" opens
  `streamroom://open?server=…&room=…&pair=<code>`. The pair code is a single-use HMAC ticket
  (10 minutes). The app exchanges it once (`POST /api/app/pair`) for its own **app session
  key**: a 90-day session sent as `Authorization: Bearer`, flagged `viaApp`, **never allowed
  on admin routes** (`requireAdmin` rejects it), revocable per device on the Admin page.
  Stored as `account.dat` in the app's config folder: DPAPI-encrypted on Windows, a plain file
  readable only by the user (0600) on Linux (see backlog).
- **LiveKit tokens** are minted per join (`rooms.js`, TTL 8h). Rooms are created only by the
  server (`auto_create: false` in LiveKit), so ended rooms stay ended.
- Legacy routes still exist and are candidates for removal: `POST /api/token` and the old
  `/live/:id#k=KEY` app links (`/rooms/:id/app-link`, `Launch::Legacy` in `core`).

## The site, notable behaviour

- **Go Live dialog** (`room.html`/`room.js`): the app is the main option on Windows and Linux
  (`appOS`); browser streaming is a `<details>` dropdown (`#browserBox`) with resolution, fps,
  layers, codec, bitrate and "Find the best settings".
- **Connection errors** are classified in `room.js` (`classify()`, uses `reasonName`; the ICE
  regex is `\bice\b` because "service" matched before) and shown in plain words; failures are
  reported to `POST /api/health/report` and appear in the admin Health panel.
- **Page leave:** LiveKit's `disconnectOnPageLeave` is off and a `pagehide` handler is used,
  because navigating to `streamroom://` fired `beforeunload` and dropped the room connection.
- **Phone layout:** stacked streams in portrait, full screen in landscape.
- **Download page** (`/download`): Windows/Linux switch; the Linux command is built from
  `location.origin` + `/api/app/download/linux`.
- `localStorage['sr.forceRelay']` forces TURN-only (testing the parked relay).

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
  capture failing 3 times in a row shares all PC audio for 30 s, then tries per-app again.
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
  under `%LOCALAPPDATA%\click.eventworks.streamroom\logs\`.

## Build, run, test

### Site
```
npm install
npm run dev          # LiveKit dev server + the site (needs livekit-server on PATH)
npm run admin-link   # admin sign-in link
npm run test:e2e     # end-to-end: real LiveKit + Chromium (Playwright)
```
e2e notes: needs `livekit-server` on PATH or `LIVEKIT_BIN`; Chromium via
`npx playwright install chromium` or `CHROME_PATH`. `E2E_CLI=<path to streamroom-live-cli>` plus
`E2E_CLI_ARGS="--height 720 --no-audio"` adds the step where the native engine streams into a
room (on Linux it needs an X display). Clear `HTTPS_PROXY`/`HTTP_PROXY` for local runs behind a
proxy. Test traps already solved: a closed `<dialog>` never counts as visible (wait on `open`);
the external-protocol prompt wedges a tab (do that step in its own tab); Chrome needs
`--use-fake-ui-for-media-stream` to offer host ICE candidates.

### App
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

## CI and releasing

| Workflow | Runs when | Does |
|---|---|---|
| `site.yml` | any push not under `app/` and not `*.md`; PRs | e2e test; `relay` job only by hand |
| `app-windows.yml` | `app/**` except Linux-only paths | NSIS installer + CLI; publishes `app-vX.Y.Z` |
| `app-linux.yml` | `app/**` except Windows-only paths | fmt + clippy (`-D warnings`), unit tests, app, CLI, pacman package (built in an `archlinux:base-devel` container); publishes `linux-vX.Y.Z` |

- **Versions are per platform:** `app/release/windows` and `app/release/linux`. CI writes that
  number into `Cargo.toml` before building (the committed `Cargo.toml` version is only for local
  builds). **To release:** bump the platform's file and push to `main`. A build is published only
  if its tag doesn't exist yet.
- **Nothing ships without a bump.** A merged client change stays unreleased (installed apps
  report "latest version") until the platform's file is bumped. Any PR that changes the client
  bumps it, or says in the description why not.
- Windows releases are `app-v*` and marked **latest** (old Windows apps and the site look at
  "latest"). Linux releases are `linux-v*` and never marked latest.
- Each updater only looks at its own platform's tags (`TAG` in `update.rs`); the site's
  `appinfo.js` picks the newest `app-v*` release with the installer, and the newest Linux package
  for `/api/app/download/linux`.
- Windows-only paths (Linux workflow ignores them): `wincap/src/{audio,capture,convert,cursor,d3d,
  encoder,pipeline}.rs`, `desktop/icons/icon.ico`, `release/windows`. (`desktop/src/account.rs`
  is shared: Linux stores the key there too.)
  Linux-only paths (Windows workflow ignores them): `wincap/src/linux/**`, `packaging/**`,
  `release/linux`. `app/README.md` builds neither. Everything else in `app/` builds both.
- The Windows build is much slower than Linux; unit tests, `cargo fmt --check` and clippy run only on
  Linux, so clippy never sees `#[cfg(windows)]` code. Style is `app/rustfmt.toml` (wide lines, matching
  the code). `wincap` and `core` deny `unwrap()` outside tests (clippy).
- Tag pushes and GitHub's GraphQL API aren't available from some Claude environments; this
  release flow (version file + push) was chosen so no tag push is needed. `gh api` (REST) works
  for runs and releases.

## Deploy

From AWS CloudShell (re-running updates in place and keeps people and settings):
```
rm -rf stream.eventworks && git clone -q --depth 1 https://github.com/Saroknight/stream.eventworks \
  && DOMAIN=stream.eventworks.click bash stream.eventworks/deploy/aws/lightsail.sh
```
Add `REGION`/`NAME` if they were set originally. `RELAY_443=on` turns on the parked relay.
The script creates or reuses the instance + static IP, opens ports 80, 443, 7881/tcp,
7882/udp (and 3478/udp), sets Route 53 DNS if the zone is there, then runs
`deploy/linux/setup.sh <domain> <public ip>` on the box.

On the server: code `/opt/streamroom`, settings `/etc/streamroom.env`, data
`/var/lib/streamroom/state.json`, LiveKit `/etc/livekit/livekit.yaml`; logs
`journalctl -u streamroom -u livekit -u caddy -f`. **Site changes only go live after a deploy**;
app releases don't need one (except changes to `appinfo.js`/download routes).

## Lessons learned (don't relearn these)

- **Pin LiveKit's `node_ip`.** LiveKit learned a stale public IPv4 via STUN and friends without
  IPv6 couldn't connect. `setup.sh` takes the public IP and writes `node_ip`; the Health panel
  checks the advertised address matches the domain (`LIVEKIT_NODE_IP`).
- **Port 443 relay is parked** (opt-in `RELAY_443=on`). It needs Caddy with the layer4 module
  (SNI split between the site and LiveKit TURN, `servers 127.0.0.1:8443` must match exactly,
  proxy_protocol, `allow_restricted_peer_cidrs` for private addresses). It caused more problems
  than it solved and was never proven in CI.
- **PipeWire ≥1.6 + WirePlumber 0.5: set `media.type` on `pipewiresrc`.** Without it the stream
  registers as `Stream/Input/Unknown`, WirePlumber only links matching media types, and every
  target fails with "target not found" (screen and per-app sound). Found by Studdder (PR #1).
- **Screens have variable frame rates (0/1).** `videorate max-rate` lets everything through
  (a 165 Hz monitor sent 165 fps and starved the bitrate). A fixed `framerate=N/1` caps filter
  after it breaks negotiation on GNOME (0.4.6 regression). The fix is `FrameGate` at the encoder
  input; never drop *encoded* H.264 frames (later frames depend on them). `videorate` was removed
  in 0.4.9 as dead weight.
- **`ximagesrc` defaults to 25 fps** unless the caps after it set a framerate (X11 only; the
  fixed rate there is safe, unlike the GNOME portal stream). Fixed in 0.4.9.
- **KDE can report pixel aspect 2147483647:1**, which overflows scalers: force square pixels
  with `capssetter` and spell out the output width and height.
- **GNOME's portal doesn't report the PipeWire serial;** look it up on the portal connection.
- **A failed GStreamer pipeline must be set to Null,** or it keeps threads and PipeWire streams.
- **`set_state(Playing)` failing hides the reason:** read the Error message off the bus.
- **Browser hand-off to the app:** `beforeunload` fires on `streamroom://` navigation (see Page
  leave above). The app's single-instance callback must handle links from argv itself.
- **Windows build features:** `wincap` needs `Win32_Security` (for `CreateEventW`).
- **Updater safety:** never updates mid-stream; always verifies SHA256SUMS first.

## Open items / backlog

Not started unless the owner picks them:
- Security pass items (from the earlier review): pin GitHub Actions to commit SHAs and sign
  updates; shorter LiveKit token TTL than 8h; remove the legacy `/token` and `#k=` app links;
  CSP and HSTS headers on the site; a CSP for the app window; decide on invite-link lifetime and
  reuse; scheduled Lightsail snapshots for backups; on Linux, store the app key encrypted
  (Secret Service/libsecret); today `account.dat` is a plain file with 0600 permissions.
- Windows per-app sound (`activate_process_loopback` in `wincap/src/audio.rs`): on the 5 s
  timeout the function returns while the activation may still be pending, freeing the stack
  `params` its PROPVARIANT blob points at (use-after-free if Windows reads it later). Fix: heap
  allocate `params`/`PROPVARIANT` owned by the `Done` completion handler (Windows holds it until
  `ActivateCompleted`), narrow the whole-function `unsafe` block to the two calls with `SAFETY`
  comments, move the condvar wait into a safe helper (removes the last two `unwrap()`s that fail
  Windows-target clippy). Own branch; needs testing on Windows.
- Port 443 relay: parked; revisit only if someone's network actually blocks the video ports.
- Linux: confirm 0.4.7+ streams cleanly at the set fps on the tester's GNOME machine, including
  per-app sound. No Flatpak/other distros planned; Steam Deck and OBS integration explicitly out
  of scope.
- `appinfo.js` changes and the Linux download command need a Lightsail redeploy to go live.

## History (short)

1. Site: invite-only lobby, rooms in LiveKit, browser streaming with tuning, admin page.
2. Windows app (Rust/Tauri): GPU capture/encode, pre-encoded publish, tray, deep links, updater.
3. Fixed friends not connecting (stale LiveKit IP → `node_ip`).
4. Improvements: the app does everything after one sign-in, plain-English errors, admin Health
   panel, CI e2e, per-app sound (default: everything except Discord), phone layout, app-first
   Go Live. Port 443 relay built, then parked.
5. Linux app for Arch/CachyOS (portal + PipeWire + GStreamer, pacman package, self-updates).
6. Split Windows and Linux into separate builds and releases (Linux 0.4.3+).
7. GNOME fixes: media type (0.4.5, Studdder), frame cap (0.4.6, regressed; fixed 0.4.7 by
   Studdder with FrameGate), audio pipeline leak (0.4.7).
8. Linux 0.4.8: quality pass (fmt/clippy in CI, shared GStreamer pipeline helper, encoder enum,
   real frame counters, 0600 `account.dat`, updater off the async runtime) and "Check for
   updates" in the ⋯ menu.
9. Linux 0.4.9: X11 streams at the chosen fps (was stuck at 25), `pw-dump` timeout, per-app sound
   fallback is temporary instead of permanent, `videorate` removed.
