# The server (`server/`): notes for Claude Code

Loaded when Claude works on files under `server/`. Repo-wide rules are in the root `CLAUDE.md`.

## Auth model

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
  readable only by the user (0600) on Linux (see `BACKLOG.md`).
- **LiveKit tokens** are minted per join (`rooms.js`, TTL 8h). Rooms are created only by the
  server (`auto_create: false` in LiveKit), so ended rooms stay ended.
- Legacy routes still exist and are candidates for removal: `POST /api/token` and the old
  `/live/:id#k=KEY` app links (`/rooms/:id/app-link`, `Launch::Legacy` in `core`).
