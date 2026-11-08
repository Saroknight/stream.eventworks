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

## Where the rest of the notes are

Folder notes load when Claude opens files there. `app/CLAUDE.md` has the app's capture design,
build commands and lessons: read it before planning app work. `public/CLAUDE.md` has the site
page behaviour, `server/CLAUDE.md` the auth model, `tests/CLAUDE.md` the e2e notes. Release history is in `CHANGELOG.md`, open items in `BACKLOG.md`.

## Build, run, test

### Site
Scripts are in `package.json`; `npm run dev` (LiveKit dev server + the site) needs
`livekit-server` on PATH. e2e setup and test traps are in `tests/CLAUDE.md`.

## CI and releasing

| Workflow | Runs when | Does |
|---|---|---|
| `site.yml` | any push not under `app/` and not `*.md`; PRs | e2e test; `relay` job only by hand |
| `app-windows.yml` | `app/**` except Linux-only paths | NSIS installer + CLI; publishes `app-vX.Y.Z` |
| `app-linux.yml` | `app/**` except Windows-only paths | fmt + clippy (`-D warnings`), unit tests, app, CLI, pacman package (built in an `archlinux:base-devel` container); publishes `linux-vX.Y.Z` |

- **Versions are per platform:** `app/release/windows` and `app/release/linux`. CI writes that
  number into `Cargo.toml` before building (the committed `Cargo.toml` version is only for local
  builds). **To release:** bump the platform's file, add an entry at the top of `CHANGELOG.md`,
  and push to `main`. A build is published only
  if its tag doesn't exist yet.
- **Nothing ships without a bump.** A merged client change stays unreleased (installed apps
  report "latest version") until the platform's file is bumped. Any PR that changes the client
  bumps it, or says in the description why not.
- Windows releases are `app-v*` and marked **latest** (old Windows apps and the site look at
  "latest"). Linux releases are `linux-v*` and never marked latest.
- Each updater only looks at its own platform's tags (`TAG` in `update.rs`); the site's
  `appinfo.js` picks the newest `app-v*` release with the installer, and the newest Linux package
  for `/api/app/download/linux`.
- The Windows build is much slower than Linux; unit tests, `cargo fmt --check` and clippy run only on
  Linux, so clippy never sees `#[cfg(windows)]` code.
- Tag pushes and GitHub's GraphQL API aren't available from some Claude environments; this
  release flow (version file + push) was chosen so no tag push is needed. `gh api` (REST) works
  for runs and releases.

## Deploy

**Site changes only go live after a deploy**; app releases don't need one (except changes to
`appinfo.js`/download routes). Steps, server paths and logs are in the `deploy` skill.

## Lessons learned (don't relearn these)

- **Pin LiveKit's `node_ip`.** LiveKit learned a stale public IPv4 via STUN and friends without
  IPv6 couldn't connect. `setup.sh` takes the public IP and writes `node_ip`; the Health panel
  checks the advertised address matches the domain (`LIVEKIT_NODE_IP`).
- **Port 443 relay is parked** (opt-in `RELAY_443=on`). It needs Caddy with the layer4 module
  (SNI split between the site and LiveKit TURN, `servers 127.0.0.1:8443` must match exactly,
  proxy_protocol, `allow_restricted_peer_cidrs` for private addresses). It caused more problems
  than it solved and was never proven in CI.
- **Browser hand-off to the app:** `beforeunload` fires on `streamroom://` navigation (see Page
  leave in `public/CLAUDE.md`). The app's single-instance callback must handle links from argv itself.

## Open items / backlog

Open items are in `BACKLOG.md`; none are started unless the owner picks them.
