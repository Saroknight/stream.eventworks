// Admin API: people, invites, rooms. Only the admin account can use it.
import express from 'express';
import { INVITE_DAYS } from './config.js';
import { cleanName, requireAdmin, requireUser } from './auth.js';
import { adminRooms, endRoom, kickUser, removeParticipant } from './rooms.js';
import * as store from './store.js';

export const router = express.Router();
router.use('/admin', requireUser, requireAdmin);

const inviteUrl = (req, token) => `${req.protocol}://${req.get('host')}/join/${token}`;

router.get('/admin/state', async (req, res, next) => {
  try {
    const people = store.users().map((u) => {
      const sessions = store.sessionsFor(u.id);
      const inv = store.invites().find((i) => i.userId === u.id);
      return {
        id: u.id, name: u.name, isAdmin: u.isAdmin, createdAt: u.createdAt,
        lastSeen: Math.max(u.lastSeen || 0, ...sessions.map((s) => s.lastSeen)),
        devices: sessions.map((s) => ({ id: s.key.slice(0, 12), device: s.device, app: Boolean(s.app), lastSeen: s.lastSeen })),
        invite: inv ? { id: inv.id, expiresAt: inv.expiresAt } : null,
      };
    }).sort((a, b) => b.lastSeen - a.lastSeen);
    const pending = store.invites().filter((i) => !i.userId)
      .map((i) => ({ id: i.id, name: i.name, createdAt: i.createdAt, expiresAt: i.expiresAt }))
      .sort((a, b) => b.createdAt - a.createdAt);
    let rooms = [];
    let roomsError = null;
    try { rooms = await adminRooms(); } catch (e) { roomsError = `Media server unreachable: ${e.message}`; }
    res.json({ people, pending, rooms, roomsError, me: req.user.id });
  } catch (e) { next(e); }
});

router.post('/admin/invites', (req, res) => {
  const name = cleanName(req.body?.name);
  if (!name) return res.status(400).json({ error: 'Type the person\'s name.' });
  const { invite, token } = store.createInvite(name, INVITE_DAYS);
  res.json({ id: invite.id, name, url: inviteUrl(req, token), expiresAt: invite.expiresAt });
});

// New link for an invite (pending, or an existing person's, to sign in more devices). Old link stops working.
router.post('/admin/invites/:id/renew', (req, res) => {
  const r = store.renewInvite(req.params.id, INVITE_DAYS);
  if (!r) return res.status(404).json({ error: 'Invite not found.' });
  res.json({ id: r.invite.id, url: inviteUrl(req, r.token), expiresAt: r.invite.expiresAt });
});

// Same, for a person who has no invite record (e.g. the admin): makes one tied to them.
router.post('/admin/users/:id/link', (req, res) => {
  const u = store.getUser(req.params.id);
  if (!u) return res.status(404).json({ error: 'Person not found.' });
  const existing = store.invites().find((i) => i.userId === u.id);
  const r = existing ? store.renewInvite(existing.id, INVITE_DAYS) : (() => {
    const c = store.createInvite(u.name, INVITE_DAYS);
    c.invite.userId = u.id;
    store.save();
    return c;
  })();
  res.json({ url: inviteUrl(req, r.token), expiresAt: r.invite.expiresAt });
});

router.post('/admin/invites/:id/cancel', (req, res) => {
  store.deleteInvite(req.params.id);
  res.json({ ok: true });
});

router.post('/admin/users/:id/devices/:sid/signout', (req, res) => {
  if (!store.deleteSessionById(req.params.id, req.params.sid)) return res.status(404).json({ error: 'That device is already signed out.' });
  res.json({ ok: true });
});

router.post('/admin/users/:id/rename', (req, res) => {
  const name = cleanName(req.body?.name);
  if (!name) return res.status(400).json({ error: 'Type a name.' });
  if (!store.renameUser(req.params.id, name)) return res.status(404).json({ error: 'Person not found.' });
  res.json({ ok: true });
});

router.post('/admin/users/:id/revoke', async (req, res, next) => {
  try {
    const u = store.getUser(req.params.id);
    if (!u) return res.status(404).json({ error: 'Person not found.' });
    if (u.isAdmin) return res.status(400).json({ error: 'You can\'t remove the admin.' });
    store.deleteUser(u.id);
    await kickUser(u.id).catch(() => {});
    res.json({ ok: true });
  } catch (e) { next(e); }
});

router.post('/admin/rooms/:id/end', async (req, res, next) => {
  try { await endRoom(req.params.id); res.json({ ok: true }); } catch (e) { next(e); }
});

router.post('/admin/rooms/:id/remove', async (req, res, next) => {
  try {
    await removeParticipant(req.params.id, String(req.body?.identity || ''));
    res.json({ ok: true });
  } catch (e) { next(e); }
});
