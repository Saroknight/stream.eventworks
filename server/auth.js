// Sign-in: invite links (one per friend, any device, 7 days), "sign in another device" links,
// and the admin bootstrap link printed by the CLI. Signed-in devices get a 90-day cookie.
import express from 'express';
import {
  ADMIN_LINK_HOURS, DEVICE_LINK_MINUTES, SESSION_DAYS,
} from './config.js';
import * as store from './store.js';
import { makeTicket, readTicket } from './tickets.js';

const COOKIE = 'sr_session';

export const cleanName = (n) => String(n || '').replace(/[\u0000-\u001f<>]/g, '').trim().slice(0, 32);

function parseCookies(header = '') {
  const out = {};
  for (const part of header.split(';')) {
    const i = part.indexOf('=');
    if (i > 0) out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

function setSessionCookie(req, res, token) {
  const secure = req.secure ? '; Secure' : '';
  res.setHeader('Set-Cookie',
    `${COOKIE}=${token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=${SESSION_DAYS * 86400}${secure}`);
}
const clearSessionCookie = (res) => res.setHeader('Set-Cookie', `${COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`);

function deviceLabel(ua = '') {
  const os = /Windows/.test(ua) ? 'Windows' : /iPhone|iPad/.test(ua) ? 'iPhone/iPad' : /Android/.test(ua) ? 'Android'
    : /Mac OS/.test(ua) ? 'Mac' : /Linux/.test(ua) ? 'Linux' : 'Device';
  const br = /Edg\//.test(ua) ? 'Edge' : /OPR\/|Opera/.test(ua) ? 'Opera' : /Firefox\//.test(ua) ? 'Firefox'
    : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'browser';
  return `${os} · ${br}`;
}

// ---- tiny per-IP rate limiter for sign-in endpoints ----
const hits = new Map();
export function rateLimit(max, windowMs) {
  return (req, res, next) => {
    const key = `${req.ip}|${req.path}`;
    const t = Date.now();
    const h = (hits.get(key) || []).filter((x) => x > t - windowMs);
    h.push(t);
    hits.set(key, h);
    if (h.length > max) return res.status(429).json({ error: 'Too many attempts. Wait a few minutes and try again.' });
    next();
  };
}
setInterval(() => { const t = Date.now(); for (const [k, h] of hits) if (h.every((x) => x < t - 3600_000)) hits.delete(k); }, 600_000).unref();

// ---- middleware ----
export function loadUser(req, _res, next) {
  const token = parseCookies(req.headers.cookie)[COOKIE];
  const s = store.findSession(token, SESSION_DAYS);
  req.session = s;
  req.user = s ? store.getUser(s.userId) : null;
  next();
}

export function requireUser(req, res, next) {
  if (!req.user) return res.status(401).json({ error: 'Please sign in with your invite link.' });
  next();
}

export function requireAdmin(req, res, next) {
  if (!req.user?.isAdmin) return res.status(403).json({ error: 'Admins only.' });
  next();
}

// Cross-site request protection: browsers can't send JSON to another site without permission,
// and the cookie is SameSite=Lax. So every state-changing API call must be JSON.
export function jsonOnly(req, res, next) {
  if (req.method !== 'GET' && req.method !== 'HEAD' && !req.is('application/json')) {
    return res.status(415).json({ error: 'Expected JSON.' });
  }
  next();
}

// ---- links ----
export const adminLinkTicket = () => makeTicket({ p: 'admin' }, ADMIN_LINK_HOURS * 3600_000);
const deviceTicket = (userId) => makeTicket({ p: 'device', uid: userId }, DEVICE_LINK_MINUTES * 60_000);

// What a join link is, without using it up. Tickets contain a dot; invite tokens don't.
function inspect(token) {
  if (typeof token !== 'string' || !token) return { error: 'This link is incomplete.' };
  if (token.includes('.')) {
    const admin = readTicket(token, 'admin');
    if (admin) {
      if (store.ticketUsed(admin.jti)) return { error: 'This admin link was already used. Print a new one on the server.' };
      const a = store.adminUser();
      return { kind: 'admin', ticket: admin, name: a?.name || '', needsName: !a };
    }
    const dev = readTicket(token, 'device');
    if (dev) {
      if (store.ticketUsed(dev.jti)) return { error: 'This sign-in link was already used. Make a new one from your other device.' };
      const u = store.getUser(dev.uid);
      if (!u) return { error: 'This account no longer exists.' };
      return { kind: 'device', ticket: dev, name: u.name, user: u };
    }
    return { error: 'This link has expired. Ask for a new one.' };
  }
  const inv = store.findInvite(token);
  if (!inv) return { error: 'This invite link is not valid. Ask for a new one.' };
  if (inv.expiresAt < Date.now()) return { error: 'This invite link has expired. Ask for a new one.' };
  return { kind: 'invite', invite: inv, name: inv.userId ? store.getUser(inv.userId)?.name || inv.name : inv.name };
}

export const router = express.Router();
const limited = rateLimit(30, 10 * 60_000);

router.get('/me', (req, res) => {
  if (!req.user) return res.status(401).json({ error: 'Not signed in.' });
  const { id, name, isAdmin } = req.user;
  res.json({ user: { id, name, isAdmin } });
});

router.get('/auth/peek', limited, (req, res) => {
  const r = inspect(req.query.t);
  if (r.error) return res.status(400).json({ error: r.error });
  res.json({ kind: r.kind, name: r.name, needsName: Boolean(r.needsName), signedInAs: req.user?.name || null });
});

router.post('/auth/redeem', limited, (req, res) => {
  const r = inspect(req.body?.token);
  if (r.error) return res.status(400).json({ error: r.error });
  let user;
  if (r.kind === 'invite') {
    user = store.useInvite(r.invite);
  } else if (r.kind === 'device') {
    store.markTicketUsed(r.ticket.jti, r.ticket.exp);
    user = r.user;
  } else {
    store.markTicketUsed(r.ticket.jti, r.ticket.exp);
    user = store.adminUser();
    if (!user) {
      const name = cleanName(req.body?.name);
      if (!name) return res.status(400).json({ error: 'Type your name.' });
      user = store.createUser(name, { isAdmin: true });
    }
  }
  // A device signing in as someone else replaces its old session.
  if (req.session) store.deleteSession(req.session.key);
  setSessionCookie(req, res, store.createSession(user.id, deviceLabel(req.headers['user-agent']), SESSION_DAYS));
  res.json({ user: { id: user.id, name: user.name, isAdmin: user.isAdmin } });
});

router.post('/auth/logout', (req, res) => {
  if (req.session) store.deleteSession(req.session.key);
  clearSessionCookie(res);
  res.json({ ok: true });
});

router.post('/auth/device-link', requireUser, async (req, res) => {
  const ticket = deviceTicket(req.user.id);
  const url = `${req.protocol}://${req.get('host')}/join/${ticket}`;
  const { default: QRCode } = await import('qrcode');
  const svg = await QRCode.toString(url, { type: 'svg', margin: 1, color: { dark: '#000000', light: '#ffffff' } });
  res.json({ url, svg, minutes: DEVICE_LINK_MINUTES });
});
