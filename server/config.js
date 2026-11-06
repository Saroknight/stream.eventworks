// Settings, all from the environment (or a .env file next to package.json).
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function loadDotEnv(file) {
  try {
    for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
      const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$/);
      if (m && !(m[1] in process.env)) process.env[m[1]] = m[2].replace(/^["']|["']$/g, '');
    }
  } catch { /* no .env file */ }
}
loadDotEnv(path.join(ROOT, '.env'));

export const PORT = Number(process.env.PORT || 3000);
export const HOST = process.env.HOST || '0.0.0.0';
export const DATA_DIR = process.env.DATA_DIR || path.join(ROOT, 'data');
fs.mkdirSync(DATA_DIR, { recursive: true });

export const LIVEKIT_API_KEY = process.env.LIVEKIT_API_KEY || 'devkey';
export const LIVEKIT_API_SECRET = process.env.LIVEKIT_API_SECRET || 'secret';
// Where *this server* reaches LiveKit's API (same machine).
export const LIVEKIT_API_URL = process.env.LIVEKIT_API_URL || 'http://127.0.0.1:7880';
// Where *browsers* reach LiveKit. Empty = derive from the page address.
export const LIVEKIT_PUBLIC_URL = process.env.LIVEKIT_PUBLIC_URL || '';

// Public address of the site, used for links printed by the CLI. Pages use their own address.
export const PUBLIC_URL = (process.env.PUBLIC_URL || `http://localhost:${PORT}`).replace(/\/+$/, '');

// Secret for signing links and tickets. Production sets AUTH_SECRET; locally we make one.
function authSecret() {
  if (process.env.AUTH_SECRET) return process.env.AUTH_SECRET;
  const file = path.join(DATA_DIR, 'auth-secret');
  try { return fs.readFileSync(file, 'utf8').trim(); } catch { /* create below */ }
  const s = crypto.randomBytes(32).toString('base64url');
  fs.writeFileSync(file, s, { mode: 0o600 });
  return s;
}
export const AUTH_SECRET = authSecret();

// Room rules
export const ROOM_EMPTY_SECONDS = Number(process.env.ROOM_EMPTY_SECONDS || 300); // end after 5 min empty

// Session and link lifetimes
export const SESSION_DAYS = 90;
export const INVITE_DAYS = 7;
export const DEVICE_LINK_MINUTES = 10;
export const ADMIN_LINK_HOURS = 24;
export const APP_TICKET_HOURS = 12;

if (LIVEKIT_API_SECRET === 'secret') {
  console.warn('[streamroom] Using LiveKit dev credentials. Fine locally, never on a public server.');
}
