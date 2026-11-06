// Streamroom server: sign-in, rooms (via LiveKit), admin, and the web pages.
// Video never flows through here; it goes browser/app <-> LiveKit.
import path from 'node:path';
import express from 'express';
import { HOST, LIVEKIT_PUBLIC_URL, PORT, ROOT } from './config.js';
import * as auth from './auth.js';
import * as rooms from './rooms.js';
import * as admin from './admin.js';

const app = express();
app.disable('x-powered-by');
// Behind Caddy on the same machine: trust its X-Forwarded-* headers (https, client IP).
app.set('trust proxy', 'loopback');

app.use((_req, res, next) => {
  res.setHeader('X-Content-Type-Options', 'nosniff');
  res.setHeader('Referrer-Policy', 'same-origin'); // keeps join links out of Referer headers
  res.setHeader('X-Frame-Options', 'DENY');
  next();
});

app.use('/api', express.json({ limit: '10kb' }), auth.jsonOnly, auth.loadUser);
app.use('/api', auth.router, rooms.router, admin.router);
app.use('/api', (_req, res) => res.status(404).json({ error: 'Not found.' }));
// eslint-disable-next-line no-unused-vars
app.use('/api', (err, _req, res, _next) => {
  console.error(err);
  res.status(500).json({ error: 'Something went wrong on the server.' });
});

// Pages and static files. The LiveKit browser SDK is served locally (no CDN).
const page = (name) => (_req, res) => res.sendFile(path.join(ROOT, 'public', name));
app.use('/vendor', express.static(path.join(ROOT, 'node_modules/livekit-client/dist'), { maxAge: '7d' }));
app.get('/', page('index.html'));
app.get('/join/:token', page('join.html'));
app.get('/r/:id', page('room.html'));
app.get('/admin', page('admin.html'));
// Old links: /live/<room> is what the Windows app takes; opened in a browser, go to the room.
app.get(['/live/:id', '/watch/:id'], (req, res) => res.redirect(`/r/${encodeURIComponent(req.params.id)}`));
app.use(express.static(path.join(ROOT, 'public'), { index: false }));

app.listen(PORT, HOST, () => {
  console.log(`[streamroom] http://localhost:${PORT}  (LiveKit for browsers: ${LIVEKIT_PUBLIC_URL || 'auto'})`);
});
