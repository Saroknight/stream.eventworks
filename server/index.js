// Streamroom server: creates rooms, hands out streamer/viewer links and LiveKit tokens,
// and serves the web pages. Media itself flows through LiveKit, never through this server.
import express from 'express';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { AccessToken, TrackSource } from 'livekit-server-sdk';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..');

// ---- config (all via environment, see .env.example) ----
loadDotEnv(path.join(ROOT, '.env'));
const PORT = Number(process.env.PORT || 3000);
const HOST = process.env.HOST || '0.0.0.0';
const LIVEKIT_API_KEY = process.env.LIVEKIT_API_KEY || 'devkey';
const LIVEKIT_API_SECRET = process.env.LIVEKIT_API_SECRET || 'secret';
// URL the *browser* uses to reach LiveKit. Unset = work it out from the page address
// (http://host:3000 -> ws://host:7880 locally; https://domain -> wss://domain behind Caddy).
const LIVEKIT_PUBLIC_URL = process.env.LIVEKIT_PUBLIC_URL || '';
// Optional password required to create rooms. Strongly recommended on a public server.
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD || '';
const DATA_DIR = process.env.DATA_DIR || path.join(ROOT, 'data');

if (LIVEKIT_API_SECRET === 'secret') {
  console.warn('[streamroom] Using LiveKit dev credentials. Fine locally, never on a public server.');
}

// ---- tiny JSON-file room store ----
fs.mkdirSync(DATA_DIR, { recursive: true });
const ROOMS_FILE = path.join(DATA_DIR, 'rooms.json');
let rooms = {};
try { rooms = JSON.parse(fs.readFileSync(ROOMS_FILE, 'utf8')); } catch { rooms = {}; }
function saveRooms() {
  const tmp = ROOMS_FILE + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(rooms, null, 2));
  fs.renameSync(tmp, ROOMS_FILE);
}

const slug = (bytes) => crypto.randomBytes(bytes).toString('base64url');
const safeEqual = (a, b) => {
  const ba = Buffer.from(String(a)), bb = Buffer.from(String(b));
  return ba.length === bb.length && crypto.timingSafeEqual(ba, bb);
};
const cleanName = (n) => String(n || '').replace(/[\u0000-\u001f]/g, '').trim().slice(0, 32);

// ---- app ----
const app = express();
app.disable('x-powered-by');
app.use(express.json({ limit: '10kb' }));

app.get('/api/config', (_req, res) => {
  res.json({ needsAdminPassword: Boolean(ADMIN_PASSWORD) });
});

// Create a room. Returns the secret streamer key; viewers only need the room id.
app.post('/api/rooms', (req, res) => {
  if (ADMIN_PASSWORD && !safeEqual(req.body?.adminPassword || '', ADMIN_PASSWORD)) {
    return res.status(401).json({ error: 'Wrong admin password.' });
  }
  const title = cleanName(req.body?.title) || 'Stream room';
  const id = slug(6);
  rooms[id] = { title, streamKey: slug(12), createdAt: new Date().toISOString() };
  saveRooms();
  res.json({ roomId: id, streamKey: rooms[id].streamKey, title });
});

// Public room info (never exposes the stream key).
app.get('/api/rooms/:id', (req, res) => {
  const room = rooms[req.params.id];
  if (!room) return res.status(404).json({ error: 'Room not found.' });
  res.json({ roomId: req.params.id, title: room.title });
});

// Rotate the streamer key (invalidates old streamer links). Requires the current key.
app.post('/api/rooms/:id/rotate', (req, res) => {
  const room = rooms[req.params.id];
  if (!room) return res.status(404).json({ error: 'Room not found.' });
  if (!safeEqual(req.body?.streamKey || '', room.streamKey)) return res.status(403).json({ error: 'Invalid key.' });
  room.streamKey = slug(12);
  saveRooms();
  res.json({ streamKey: room.streamKey });
});

// Issue a LiveKit token. role = "streamer" (needs streamKey) or "viewer".
app.post('/api/token', async (req, res) => {
  try {
    const { roomId, role, streamKey } = req.body || {};
    // test: the "Find best settings" tuner publishes into a private side room nobody watches.
    const test = role === 'streamer' && req.body?.test === true;
    const room = rooms[roomId];
    if (!room) return res.status(404).json({ error: 'Room not found.' });
    const name = cleanName(req.body?.name) || (role === 'streamer' ? 'Streamer' : 'Viewer');

    let grant;
    if (role === 'streamer') {
      if (!safeEqual(streamKey || '', room.streamKey)) {
        return res.status(403).json({ error: 'This streamer link is no longer valid. Ask the host for a new one.' });
      }
      grant = {
        room: test ? `${roomId}--settings-test` : roomId, roomJoin: true,
        canPublish: true, canSubscribe: false, canPublishData: false,
        canPublishSources: [TrackSource.SCREEN_SHARE, TrackSource.SCREEN_SHARE_AUDIO],
      };
    } else {
      grant = { room: roomId, roomJoin: true, canPublish: false, canSubscribe: true, canPublishData: false };
    }

    const at = new AccessToken(LIVEKIT_API_KEY, LIVEKIT_API_SECRET, {
      identity: `${role === 'streamer' ? 's' : 'v'}_${slug(6)}`,
      name,
      ttl: '12h',
      metadata: JSON.stringify({ role: role === 'streamer' ? 'streamer' : 'viewer' }),
    });
    at.addGrant(grant);
    res.json({ token: await at.toJwt(), url: LIVEKIT_PUBLIC_URL, title: room.title });
  } catch (err) {
    console.error(err);
    res.status(500).json({ error: 'Could not create a token.' });
  }
});

// Static files: pages + the LiveKit browser SDK served locally (no CDN dependency).
app.use('/vendor', express.static(path.join(ROOT, 'node_modules/livekit-client/dist'), { maxAge: '7d' }));
app.use(express.static(path.join(ROOT, 'public'), { extensions: ['html'] }));
app.get('/live/:id', (_req, res) => res.sendFile(path.join(ROOT, 'public/live.html')));
app.get('/watch/:id', (_req, res) => res.sendFile(path.join(ROOT, 'public/watch.html')));

app.listen(PORT, HOST, () => {
  console.log(`[streamroom] http://localhost:${PORT}  (LiveKit: ${LIVEKIT_PUBLIC_URL || 'auto'})`);
});

function loadDotEnv(file) {
  try {
    for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
      const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$/);
      if (m && !(m[1] in process.env)) process.env[m[1]] = m[2].replace(/^["']|["']$/g, '');
    }
  } catch { /* no .env file, that's fine */ }
}
