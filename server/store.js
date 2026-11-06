// People, sessions and invites in one small JSON file. Plenty for a friend group.
// Tokens are never stored, only their SHA-256 hashes.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { DATA_DIR } from './config.js';

const FILE = path.join(DATA_DIR, 'state.json');

const empty = () => ({ users: {}, sessions: {}, invites: {}, usedTickets: {} });
let state = empty();
try { state = { ...empty(), ...JSON.parse(fs.readFileSync(FILE, 'utf8')) }; } catch { /* fresh start */ }

let saveTimer = null;
export function save(now = false) {
  const write = () => {
    saveTimer = null;
    const tmp = `${FILE}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify(state, null, 1), { mode: 0o600 });
    fs.renameSync(tmp, FILE);
  };
  if (now) { clearTimeout(saveTimer); write(); return; }
  if (!saveTimer) saveTimer = setTimeout(write, 1000);
}
process.on('exit', () => { if (saveTimer) save(true); });

export const hash = (token) => crypto.createHash('sha256').update(token).digest('base64url');
export const newId = (bytes = 9) => crypto.randomBytes(bytes).toString('base64url');
export const now = () => Date.now();

// ---- users ----
export const users = () => Object.values(state.users);
export const getUser = (id) => state.users[id] || null;
export const adminUser = () => users().find((u) => u.isAdmin) || null;

export function createUser(name, { isAdmin = false } = {}) {
  const u = { id: newId(6), name, isAdmin, createdAt: now(), lastSeen: now() };
  state.users[u.id] = u;
  save();
  return u;
}

export function renameUser(id, name) {
  const u = state.users[id];
  if (!u) return null;
  u.name = name;
  save();
  return u;
}

export function deleteUser(id) {
  delete state.users[id];
  for (const [k, s] of Object.entries(state.sessions)) if (s.userId === id) delete state.sessions[k];
  for (const [k, i] of Object.entries(state.invites)) if (i.userId === id) delete state.invites[k];
  save();
}

// ---- sessions ----
export function createSession(userId, device, days) {
  const token = crypto.randomBytes(32).toString('base64url');
  state.sessions[hash(token)] = { userId, device, createdAt: now(), lastSeen: now(), expiresAt: now() + days * 864e5 };
  save();
  return token;
}

export function findSession(token, days) {
  if (!token) return null;
  const key = hash(token);
  const s = state.sessions[key];
  if (!s) return null;
  if (s.expiresAt < now() || !state.users[s.userId]) { delete state.sessions[key]; save(); return null; }
  // Sliding expiry; only write when it has moved meaningfully.
  if (now() - s.lastSeen > 60_000) {
    s.lastSeen = now();
    s.expiresAt = now() + days * 864e5;
    state.users[s.userId].lastSeen = now();
    save();
  }
  return { key, ...s };
}

export function deleteSession(key) { delete state.sessions[key]; save(); }
export const sessionsFor = (userId) => Object.entries(state.sessions)
  .filter(([, s]) => s.userId === userId && s.expiresAt > now())
  .map(([key, s]) => ({ key, ...s }));

// ---- invites ----
// One invite per friend. Works on any number of devices until it expires.
export function createInvite(name, days) {
  const token = crypto.randomBytes(18).toString('base64url');
  const inv = { id: newId(6), tokenHash: hash(token), name, createdAt: now(), expiresAt: now() + days * 864e5, userId: null, uses: 0 };
  state.invites[inv.id] = inv;
  save();
  return { invite: inv, token };
}

export function renewInvite(id, days) {
  const inv = state.invites[id];
  if (!inv) return null;
  const token = crypto.randomBytes(18).toString('base64url');
  inv.tokenHash = hash(token);
  inv.expiresAt = now() + days * 864e5;
  save();
  return { invite: inv, token };
}

export const findInvite = (token) => Object.values(state.invites).find((i) => i.tokenHash === hash(token)) || null;
export const invites = () => Object.values(state.invites);
export function deleteInvite(id) { delete state.invites[id]; save(); }

export function useInvite(inv) {
  if (!inv.userId || !state.users[inv.userId]) inv.userId = createUser(inv.name).id;
  inv.uses += 1;
  save();
  return state.users[inv.userId];
}

// ---- one-time tickets (admin links, device links) ----
export const ticketUsed = (jti) => Boolean(state.usedTickets[jti]);
export function markTicketUsed(jti, exp) {
  state.usedTickets[jti] = exp;
  for (const [k, e] of Object.entries(state.usedTickets)) if (e < now()) delete state.usedTickets[k];
  save();
}

// Hourly cleanup of expired sessions and invites.
setInterval(() => {
  let changed = false;
  for (const [k, s] of Object.entries(state.sessions)) if (s.expiresAt < now()) { delete state.sessions[k]; changed = true; }
  for (const [k, i] of Object.entries(state.invites)) if (!i.userId && i.expiresAt < now() - 30 * 864e5) { delete state.invites[k]; changed = true; }
  if (changed) save();
}, 3600_000).unref();
