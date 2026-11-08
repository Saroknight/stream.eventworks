# The site's pages (`public/`): notes for Claude Code

Loaded when Claude works on files under `public/`. Repo-wide rules are in the root `CLAUDE.md`.

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
