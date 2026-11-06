// Health: browsers and the app report connection problems here, and the admin page shows them
// next to the server's own vital signs. Built so a "my friend can't connect" mystery can be
// solved from the admin page instead of from server logs.
import dns from 'node:dns/promises';
import os from 'node:os';
import express from 'express';
import { LIVEKIT_PUBLIC_URL, PUBLIC_URL } from './config.js';
import { rateLimit, requireAdmin, requireUser } from './auth.js';
import { liveSummary } from './rooms.js';
import * as store from './store.js';

const NODE_IP = process.env.LIVEKIT_NODE_IP || '';

// What each kind means, in words the admin page can show as-is.
export const KINDS = {
  'network-blocked': 'Reached the server, but their network blocked the video connection.',
  'server-unreachable': 'Couldn\'t reach the video server at all (server down, or the network blocks it).',
  'not-allowed': 'The video server refused their sign-in for the room.',
  dropped: 'Was watching, then lost the connection and couldn\'t get it back.',
  'join-failed': 'Couldn\'t join the room.',
  'app-error': 'The Windows app hit an error.',
};

export function browserName(ua = '') {
  const b = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Firefox\//.test(ua) ? 'Firefox'
    : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : /streamroom-live/.test(ua) ? 'App' : 'Other';
  const o = /Windows/.test(ua) ? 'Windows' : /iPhone|iPad/.test(ua) ? 'iOS' : /Android/.test(ua) ? 'Android'
    : /Mac OS/.test(ua) ? 'Mac' : /Linux/.test(ua) ? 'Linux' : '';
  return o ? `${b} on ${o}` : b;
}

// Enough of the address to tell networks apart, not enough to locate anyone.
export function maskIp(ip = '') {
  const v4 = ip.replace(/^::ffff:/, '');
  if (/^\d+\.\d+\.\d+\.\d+$/.test(v4)) return v4.replace(/\.\d+$/, '.x');
  return ip.split(':').slice(0, 3).join(':') + ':…';
}

const clip = (v, n) => String(v ?? '').slice(0, n);

export function record(req, user, body, client) {
  const kind = Object.hasOwn(KINDS, body?.kind) ? body.kind : 'join-failed';
  let detail = null;
  try {
    const d = body?.detail && typeof body.detail === 'object' ? JSON.stringify(body.detail) : '';
    if (d && d.length <= 2000) detail = JSON.parse(d);
  } catch { /* drop it */ }
  store.addEvent({
    kind,
    client,
    userId: user?.id || null,
    userName: user?.name || null,
    room: clip(body?.room, 40) || null,
    message: clip(body?.message, 300),
    browser: client === 'app' ? `App ${clip(body?.version, 16)}`.trim() : browserName(req.get('user-agent')),
    ip: maskIp(req.ip),
    detail,
  });
}

export const router = express.Router();

router.post('/health/report', requireUser, rateLimit(30, 10 * 60_000), (req, res) => {
  record(req, req.user, req.body, req.viaApp ? 'app' : 'web');
  res.json({ ok: true });
});

async function mediaAddressCheck() {
  let host = '';
  try { host = new URL(LIVEKIT_PUBLIC_URL || PUBLIC_URL).hostname; } catch { /* none */ }
  if (!host || host === 'localhost' || /^[\d.]+$/.test(host)) {
    return { status: 'skip', text: 'Running locally: nothing to check.' };
  }
  let resolved = [];
  try { resolved = await dns.resolve4(host); } catch (e) { return { status: 'warn', text: `Couldn't look up ${host}: ${e.code || e.message}` }; }
  if (!NODE_IP) {
    return { status: 'warn', text: `The video server finds its own address at startup. ${host} points at ${resolved.join(', ')}. Re-deploy to pin it.` };
  }
  if (resolved.includes(NODE_IP)) {
    return { status: 'ok', text: `Viewers are sent to ${NODE_IP}, which is where ${host} points.` };
  }
  return { status: 'bad', text: `Viewers are sent to ${NODE_IP}, but ${host} points at ${resolved.join(', ')}. Re-deploy to fix.` };
}

router.get('/admin/health', requireUser, requireAdmin, async (_req, res, next) => {
  try {
    const t = Date.now();
    let video;
    try {
      const s = await liveSummary();
      video = { status: 'ok', ms: Date.now() - t, rooms: s.rooms, people: s.people, live: s.live };
    } catch (e) {
      video = { status: 'bad', error: e.message };
    }
    const mem = { total: os.totalmem(), free: os.freemem() };
    const since = Date.now() - 24 * 3600_000;
    const evts = store.events();
    res.json({
      server: {
        uptime: os.uptime(), appUptime: process.uptime(), load: os.loadavg(), cpus: os.cpus().length,
        memUsed: mem.total - mem.free, memTotal: mem.total, node: process.version,
      },
      video,
      media: await mediaAddressCheck(),
      events: evts.slice(0, 100).map((e) => ({ ...e, explain: KINDS[e.kind] })),
      failures24h: evts.filter((e) => e.at > since).length,
    });
  } catch (e) { next(e); }
});

router.post('/admin/health/clear', requireUser, requireAdmin, (_req, res) => {
  store.clearEvents();
  res.json({ ok: true });
});
