// Rooms live only in LiveKit: we create them with our details in the room metadata, list them
// for the lobby, and delete them to end them. LiveKit closes a room on its own once it has been
// empty for ROOM_EMPTY_SECONDS. Nothing to store, and a server restart loses nothing.
import crypto from 'node:crypto';
import express from 'express';
import { AccessToken, RoomServiceClient, TrackSource } from 'livekit-server-sdk';
import {
  APP_TICKET_HOURS, LIVEKIT_API_KEY, LIVEKIT_API_SECRET, LIVEKIT_API_URL, LIVEKIT_PUBLIC_URL, ROOM_EMPTY_SECONDS, SESSION_DAYS,
} from './config.js';
import { cleanName, rateLimit, requireUser } from './auth.js';
import * as store from './store.js';
import { makeTicket, readTicket } from './tickets.js';

export const lk = new RoomServiceClient(LIVEKIT_API_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET);

const roomId = () => crypto.randomBytes(5).toString('base64url');
const parse = (s) => { try { return JSON.parse(s || '{}'); } catch { return {}; } };
const ours = (room) => parse(room.metadata).app === 'streamroom';

// Participant identity: "<userId>.<kind>.<random>", kind = web (browser) | app (Windows app).
export const identityFor = (userId, kind) => `${userId}.${kind}.${crypto.randomBytes(3).toString('hex')}`;
export const userIdOf = (identity) => String(identity).split('.')[0];
const kindOf = (identity) => String(identity).split('.')[1] || 'web';

async function getRoom(id) {
  if (typeof id !== 'string' || !/^[\w-]{4,20}$/.test(id)) return null;
  const [room] = await lk.listRooms([id]).catch(() => []);
  return room && ours(room) ? room : null;
}

function describe(room, participants, me) {
  const meta = parse(room.metadata);
  const creator = store.getUser(meta.creatorId);
  const people = participants.map((p) => {
    const live = p.tracks.some((t) => t.source === TrackSource.SCREEN_SHARE);
    return { identity: p.identity, userId: userIdOf(p.identity), name: p.name, kind: kindOf(p.identity), live };
  });
  const users = new Set(people.map((p) => p.userId));
  return {
    id: room.name,
    title: meta.title,
    createdAt: meta.createdAt,
    creator: { id: meta.creatorId, name: creator?.name || 'someone' },
    isCreator: me ? meta.creatorId === me.id : false,
    canEnd: me ? meta.creatorId === me.id || me.isAdmin : false,
    live: people.filter((p) => p.live).map(({ userId, name, kind }) => ({ userId, name, kind })),
    here: users.size,
    people,
  };
}

// Lobby listing, cached briefly so many open lobbies don't hammer LiveKit.
let cache = { at: 0, rooms: [] };
export async function listRooms() {
  if (Date.now() - cache.at < 1500) return cache.rooms;
  const rooms = (await lk.listRooms()).filter(ours);
  const detailed = await Promise.all(rooms.map(async (r) => ({ room: r, participants: await lk.listParticipants(r.name).catch(() => []) })));
  cache = { at: Date.now(), rooms: detailed };
  return detailed;
}
const invalidate = () => { cache.at = 0; };

export async function endRoom(id) {
  await lk.deleteRoom(id);
  invalidate();
}

// Removes every connection of a user from every room (used when revoking someone).
export async function kickUser(userId) {
  for (const { room, participants } of await listRooms()) {
    for (const p of participants) {
      if (userIdOf(p.identity) === userId) await lk.removeParticipant(room.name, p.identity).catch(() => {});
    }
  }
  invalidate();
}

export async function removeParticipant(roomName, identity) {
  await lk.removeParticipant(roomName, identity);
  invalidate();
}

async function livekitToken(user, room, kind) {
  const at = new AccessToken(LIVEKIT_API_KEY, LIVEKIT_API_SECRET, {
    identity: identityFor(user.id, kind),
    name: user.name,
    ttl: '8h',
  });
  at.addGrant({
    room,
    roomJoin: true,
    canPublish: true,
    canSubscribe: kind === 'web',
    canPublishData: false,
    canUpdateOwnMetadata: false,
    canPublishSources: [TrackSource.SCREEN_SHARE, TrackSource.SCREEN_SHARE_AUDIO],
  });
  return at.toJwt();
}

export const router = express.Router();
router.use('/rooms', requireUser);

router.get('/rooms', async (req, res, next) => {
  try {
    const rooms = await listRooms();
    res.json({
      rooms: rooms.map(({ room, participants }) => {
        const { people, ...r } = describe(room, participants, req.user);
        return r;
      }).sort((a, b) => b.createdAt - a.createdAt),
    });
  } catch (e) { next(e); }
});

router.post('/rooms', async (req, res, next) => {
  try {
    const title = cleanName(req.body?.title) || `${req.user.name}'s room`;
    const id = roomId();
    await lk.createRoom({
      name: id,
      emptyTimeout: ROOM_EMPTY_SECONDS,
      departureTimeout: ROOM_EMPTY_SECONDS,
      metadata: JSON.stringify({ app: 'streamroom', title, creatorId: req.user.id, createdAt: Date.now() }),
    });
    invalidate();
    res.json({ id, title });
  } catch (e) { next(e); }
});

router.get('/rooms/:id', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    const participants = await lk.listParticipants(room.name).catch(() => []);
    const { people, ...r } = describe(room, participants, req.user);
    res.json(r);
  } catch (e) { next(e); }
});

// Browser connection: watch everyone, and go live from the browser if wanted.
// test: a private side room for the "find best settings" check.
router.post('/rooms/:id/join', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    let name = room.name;
    if (req.body?.test === true) {
      name = `${room.name}--test-${req.user.id}`;
      await lk.createRoom({ name, emptyTimeout: 30, departureTimeout: 10 });
    }
    res.json({ token: await livekitToken(req.user, name, 'web'), url: LIVEKIT_PUBLIC_URL, title: parse(room.metadata).title });
  } catch (e) { next(e); }
});

router.post('/rooms/:id/end', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.json({ ok: true });
    const meta = parse(room.metadata);
    if (meta.creatorId !== req.user.id && !req.user.isAdmin) {
      return res.status(403).json({ error: 'Only the person who started the room can end it.' });
    }
    await endRoom(room.name);
    res.json({ ok: true });
  } catch (e) { next(e); }
});

// Link for the Windows app. The app takes ".../live/<room>#k=<key>"; the key is a signed ticket
// that says who you are, so the app needs no sign-in of its own.
router.post('/rooms/:id/app-link', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    const ticket = makeTicket({ p: 'app', uid: req.user.id, room: room.name }, APP_TICKET_HOURS * 3600_000);
    res.json({ url: `${req.protocol}://${req.get('host')}/live/${room.name}#k=${ticket}` });
  } catch (e) { next(e); }
});

// ---- the Windows app ----
// "Open the app" launches streamroom://open?server=…&room=…&pair=…. The pair code is single-use
// and lasts 10 minutes: the app trades it once for its own key (a revocable 90-day session,
// listed with your devices on the admin page). An app that already has a key for you keeps it.
const PAIR_MINUTES = 10;
const siteBase = (req) => `${req.protocol}://${req.get('host')}`;

router.post('/rooms/:id/app-open', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    const pair = makeTicket({ p: 'pair', uid: req.user.id }, PAIR_MINUTES * 60_000);
    const q = new URLSearchParams({ server: siteBase(req), room: room.name, pair });
    res.json({ url: `streamroom://open?${q}` });
  } catch (e) { next(e); }
});

// Pairing doesn't need a sign-in: the code itself proves who you are.
router.post('/app/pair', rateLimit(20, 10 * 60_000), (req, res) => {
  const t = readTicket(req.body?.code, 'pair');
  if (!t || store.ticketUsed(t.jti)) {
    return res.status(403).json({ error: 'That sign-in code has expired. In a room, click Go Live, then Open the app again.' });
  }
  const user = store.getUser(t.uid);
  if (!user) return res.status(403).json({ error: 'Your account was removed.' });
  store.markTicketUsed(t.jti, t.exp);
  const existing = store.findSession(String(req.body?.deviceKey || ''), SESSION_DAYS);
  let deviceKey;
  if (existing?.app && existing.userId === user.id) {
    deviceKey = req.body.deviceKey;
  } else {
    if (existing?.app) store.deleteSession(existing.key); // this PC was signed in as someone else
    const pc = cleanName(req.body?.deviceName) || 'PC';
    deviceKey = store.createSession(user.id, `Windows app · ${pc}`, SESSION_DAYS, { app: true });
  }
  res.json({ deviceKey, user: { id: user.id, name: user.name, isAdmin: user.isAdmin } });
});

router.get('/app/me', requireUser, (req, res) => {
  res.json({ user: { id: req.user.id, name: req.user.name, isAdmin: req.user.isAdmin }, app: req.viaApp });
});

router.post('/app/signout', requireUser, (req, res) => {
  if (req.viaApp) store.deleteSession(req.session.key);
  res.json({ ok: true });
});

// The app's pass into a room: it publishes only, never downloads anyone's stream.
router.post('/rooms/:id/app-token', async (req, res, next) => {
  try {
    const room = await getRoom(req.params.id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    res.json({ token: await livekitToken(req.user, room.name, 'app'), url: LIVEKIT_PUBLIC_URL, title: parse(room.metadata).title });
  } catch (e) { next(e); }
});

// Called by the Windows app with the ticket from its link.
router.post('/token', async (req, res, next) => {
  try {
    const { roomId: id, streamKey } = req.body || {};
    const t = readTicket(streamKey, 'app');
    if (!t || t.room !== id) {
      return res.status(403).json({ error: 'This app link has expired. Copy a new one from the room page.' });
    }
    const user = store.getUser(t.uid);
    if (!user) return res.status(403).json({ error: 'Your account was removed.' });
    const room = await getRoom(id);
    if (!room) return res.status(404).json({ error: 'This room has ended.' });
    res.json({ token: await livekitToken(user, room.name, 'app'), url: LIVEKIT_PUBLIC_URL, title: parse(room.metadata).title });
  } catch (e) { next(e); }
});

// Admin view of everything live.
export async function adminRooms() {
  invalidate();
  return (await listRooms()).map(({ room, participants }) => describe(room, participants, null));
}

// For the health panel: is LiveKit answering, and how busy is it?
export async function liveSummary() {
  invalidate();
  const rooms = await listRooms();
  let people = 0;
  let live = 0;
  for (const { participants } of rooms) {
    people += participants.length;
    live += participants.filter((p) => p.tracks.some((t) => t.source === TrackSource.SCREEN_SHARE)).length;
  }
  return { rooms: rooms.length, people, live };
}
