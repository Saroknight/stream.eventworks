// Room page: watch everyone's streams, go live yourself, and (for the creator) end the room.
import * as LK from '/vendor/livekit-client.esm.mjs';
import { api, confirmButton, copy, livekitUrl, me, roomIdFromPath, store, toast } from '/common.js';
import { copyDiag, envDiag, receiverDiag } from '/diag.js';
import { setupBrowserStreaming } from '/golive.js';

const $ = (id) => document.getElementById(id);
const stage = $('stage');
const roomId = roomIdFromPath();
const userIdOf = (identity) => String(identity).split('.')[0];

let user = null;
let layout = store.get('sr.layout', 'grid');
let focusId = null;
let mutedAll = store.get('sr.mutedAll', false);
const volumes = store.get('sr.volumes', {}); // by streamer name
const tiles = new Map(); // key -> tile  (key = participant identity, or 'local' for your browser stream)
let browser = null;

const room = new LK.Room({
  // Each tile only receives the quality its on-screen size needs. The 1.5x floor biases toward
  // the sharper layer on normal screens, which matters for text.
  adaptiveStream: { pixelDensity: Math.max(1.5, window.devicePixelRatio || 1) },
  dynacast: true,
  // LiveKit normally leaves on 'beforeunload', but opening the app (streamroom://…) fires that
  // too without leaving the page. Leave on 'pagehide' instead, which only fires on a real exit.
  disconnectOnPageLeave: false,
});
window.addEventListener('pagehide', () => room.disconnect());

// ---------- overlay (ended / removed / disconnected) ----------
function showOverlay(title, text, { rejoin = false } = {}) {
  $('overlayTitle').textContent = title;
  $('overlayText').textContent = text;
  $('rejoinBtn').classList.toggle('hidden', !rejoin);
  $('overlay').classList.remove('hidden');
  try { $('goLive').close(); } catch { /* not open */ }
}
$('rejoinBtn').onclick = () => location.reload();

// ---------- tiles ----------
function makeTile(key, name, { local = false, appOnly = false, participant = null } = {}) {
  let t = tiles.get(key);
  if (t) return t;
  const el = document.createElement('div');
  el.className = `tile${local || appOnly ? ' mine' : ''}`;
  el.tabIndex = 0;
  el.innerHTML = `
    ${appOnly ? '<div class="placeholder"><span class="live-badge">LIVE</span><b>You\'re live from the app</b><small>Others see your stream. It isn\'t shown here to save your bandwidth.</small></div>'
      : '<video playsinline autoplay muted></video>'}
    <div class="label"><span class="name"></span><span class="q"></span></div>
    <div class="controls">
      <label class="vol hidden" title="Volume"><span>🔊</span><input type="range" min="0" max="1" step="0.05"></label>
      <button data-act="fs" title="Fullscreen">⛶</button>
    </div>`;
  el.querySelector('.name').textContent = local || appOnly ? `You${appOnly ? ' (app)' : ''}` : name;
  if (local || appOnly) el.querySelector('.q').textContent = 'LIVE';
  t = { key, name, el, video: el.querySelector('video'), participant, local, appOnly, videoTrack: null, audioTrack: null };
  const range = el.querySelector('input');
  range.value = volumes[name] ?? 1;
  range.oninput = () => { volumes[name] = Number(range.value); store.set('sr.volumes', volumes); applyVolume(t); };
  range.onclick = (e) => e.stopPropagation();
  el.querySelector('.vol').onclick = (e) => e.stopPropagation();
  el.querySelector('[data-act=fs]').onclick = (e) => { e.stopPropagation(); fullscreen(el); };
  el.onclick = () => onTileClick(key);
  el.ondblclick = () => fullscreen(el);
  tiles.set(key, t);
  stage.appendChild(el);
  if (!focusId) focusId = key;
  relayout();
  return t;
}

function removeTile(key) {
  const t = tiles.get(key);
  if (!t) return;
  t.el.remove();
  tiles.delete(key);
  if (focusId === key) focusId = tiles.keys().next().value || null;
  relayout();
}

function applyVolume(t) {
  if (!t.participant) return;
  t.participant.setVolume(mutedAll ? 0 : Number(t.el.querySelector('input').value), LK.Track.Source.ScreenShareAudio);
}

function onTileClick(key) {
  if (layout === 'grid' && tiles.size > 1) { focusId = key; setLayout('spotlight'); return; }
  focusId = key;
  relayout();
}

function fullscreen(el) {
  if (document.fullscreenElement) document.exitFullscreen();
  else el.requestFullscreen?.();
}

// ---------- layouts ----------
function setLayout(l) { layout = l; store.set('sr.layout', l); relayout(); }

// Phones: no screen sharing from a phone browser, and in landscape the top bar gets out of the way.
const canShare = Boolean(navigator.mediaDevices?.getDisplayMedia);
const touch = matchMedia('(hover: none)').matches;
if (!canShare) {
  $('liveBtn').classList.add('hidden');
  $('emptyHint').textContent = 'Waiting for someone to go live. To stream yourself, use a computer.';
}
if (touch) $('audioGate').textContent = '🔊 Tap to turn on sound';
const phoneLandscape = matchMedia('(hover: none) and (orientation: landscape) and (max-height: 520px)');
let chromeTimer = null;
function pokeChrome() {
  document.body.classList.remove('chrome-hidden');
  clearTimeout(chromeTimer);
  if (!phoneLandscape.matches || !tiles.size) return;
  chromeTimer = setTimeout(() => {
    if (!document.querySelector('dialog[open]') && $('overlay').classList.contains('hidden')) document.body.classList.add('chrome-hidden');
  }, 3500);
}
stage.addEventListener('click', pokeChrome, true);
document.querySelector('.topbar').addEventListener('touchstart', pokeChrome, { passive: true });
phoneLandscape.addEventListener?.('change', pokeChrome);

function relayout() {
  const n = tiles.size;
  $('empty').classList.toggle('hidden', n > 0);
  pokeChrome();
  document.querySelectorAll('[data-layout]').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.layout === layout)));
  // Spotlight and PiP need 2+ streams; portrait phones always stack streams top to bottom.
  const portraitPhone = matchMedia('(max-width: 600px) and (orientation: portrait)').matches;
  const eff = n < 2 || portraitPhone ? 'grid' : layout;
  stage.className = `stage ${eff}`;
  const ordered = [...tiles.values()];
  ordered.forEach((t) => {
    t.el.classList.toggle('focus', t.key === focusId);
    t.el.style.top = '';
    t.el.style.order = t.key === focusId ? '-1' : '0';
  });
  if (eff === 'grid') stage.style.setProperty('--cols', bestColumns(n));
  else if (eff === 'spotlight') stage.style.setProperty('--side', Math.max(1, n - 1));
  else {
    let i = 0;
    for (const t of ordered) {
      if (t.key === focusId) continue;
      const h = Math.max(90, (stage.clientWidth * 0.22 * 9) / 16);
      t.el.style.top = `${22 + i * (h + 12)}px`;
      i++;
    }
  }
  updateCount();
}

// Column count that makes 16:9 tiles as large as possible.
function bestColumns(n) {
  if (n <= 1) return 1;
  const W = stage.clientWidth - 20;
  const H = stage.clientHeight - 20;
  let best = 1;
  let bestArea = 0;
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    const w = Math.min((W - (cols - 1) * 10) / cols, ((H - (rows - 1) * 10) / rows) * 16 / 9);
    if (w * w > bestArea) { bestArea = w * w; best = cols; }
  }
  return best;
}
new ResizeObserver(() => relayout()).observe(stage);

function updateCount() {
  const people = new Set([...room.remoteParticipants.values()].map((p) => userIdOf(p.identity)));
  if (user) people.add(user.id);
  $('count').textContent = `${tiles.size} live · ${people.size} here`;
}

// ---------- sound ----------
function renderMute() {
  $('muteAll').textContent = mutedAll ? '🔇' : '🔊';
  $('muteAll').title = mutedAll ? 'Unmute all (M)' : 'Mute all (M)';
  tiles.forEach(applyVolume);
}
$('muteAll').onclick = () => { mutedAll = !mutedAll; store.set('sr.mutedAll', mutedAll); renderMute(); };
$('audioGate').onclick = async () => { await room.startAudio(); $('audioGate').classList.add('hidden'); };
room.on(LK.RoomEvent.AudioPlaybackStatusChanged, () => $('audioGate').classList.toggle('hidden', room.canPlaybackAudio));

// ---------- subscriptions ----------
// We subscribe manually so we never download our own stream (e.g. from the app).
const isMine = (p) => user && userIdOf(p.identity) === user.id;
function handlePublication(pub, p) {
  if (pub.source !== LK.Track.Source.ScreenShare && pub.source !== LK.Track.Source.ScreenShareAudio) return;
  if (isMine(p)) {
    if (pub.source === LK.Track.Source.ScreenShare) {
      makeTile(p.identity, p.name, { appOnly: true });
      // The app just went live for you: the Go Live dialog's job is done.
      if ($('goLive').open) { $('goLive').close(); toast('You\'re live from the app'); }
    }
    return;
  }
  pub.setSubscribed(true);
}
room.on(LK.RoomEvent.TrackPublished, handlePublication);
room.on(LK.RoomEvent.TrackUnpublished, (pub, p) => {
  if (pub.source === LK.Track.Source.ScreenShare) removeTile(p.identity);
});
room.on(LK.RoomEvent.TrackSubscribed, (track, _pub, p) => {
  if (track.source === LK.Track.Source.ScreenShare && track.kind === 'video') {
    const t = makeTile(p.identity, p.name || 'Streamer', { participant: p });
    t.videoTrack = track;
    track.attach(t.video);
  } else if (track.source === LK.Track.Source.ScreenShareAudio) {
    const t = makeTile(p.identity, p.name || 'Streamer', { participant: p });
    t.audioTrack = track;
    const a = track.attach();
    a.style.display = 'none';
    t.el.appendChild(a);
    t.el.querySelector('.vol').classList.remove('hidden');
    applyVolume(t);
    if (!room.canPlaybackAudio) $('audioGate').classList.remove('hidden');
  }
});
room.on(LK.RoomEvent.TrackUnsubscribed, (track, _pub, p) => {
  track.detach().forEach((e) => { if (e.tagName === 'AUDIO') e.remove(); });
  const t = tiles.get(p.identity);
  if (!t) return;
  if (track.kind === 'video') removeTile(p.identity);
  else { t.audioTrack = null; t.el.querySelector('.vol').classList.add('hidden'); }
});
room.on(LK.RoomEvent.ParticipantDisconnected, (p) => { removeTile(p.identity); updateCount(); });
room.on(LK.RoomEvent.ParticipantConnected, updateCount);
room.on(LK.RoomEvent.Reconnecting, () => toast('Connection hiccup, reconnecting…'));
room.on(LK.RoomEvent.Reconnected, () => toast('Back online'));
room.on(LK.RoomEvent.Disconnected, (reason) => {
  if (!joined) return; // a failed join attempt; join() handles that
  if (reason === LK.DisconnectReason.ROOM_DELETED) {
    showOverlay('This room has ended', 'Head back to the lobby to find another one.');
  } else if (reason === LK.DisconnectReason.PARTICIPANT_REMOVED) {
    showOverlay('You were removed from this room', 'If that was a mistake, ask the admin.');
  } else if (reason !== LK.DisconnectReason.CLIENT_INITIATED) {
    // LiveKit already tried to resume and couldn't. Rejoin from scratch a few times, then explain.
    report('dropped', `disconnected: ${LK.DisconnectReason[reason] ?? reason}`);
    const tries = rejoinCount();
    if (tries < 3 && navigator.onLine !== false) {
      showOverlay('Connection lost', 'Reconnecting…');
      setTimeout(() => location.reload(), 1500 + tries * 2000);
    } else {
      showOverlay(...MESSAGES.dropped, { rejoin: true });
    }
  }
});

// ---------- connection problems: say what actually went wrong ----------
let joined = false;
const MESSAGES = {
  'network-blocked': ['Your network is blocking video',
    'You reached Streamroom, but the video connection couldn\'t get through from this network. Try again, or try another network (like your phone\'s hotspot). The admin can see what went wrong.'],
  'server-unreachable': ['Can\'t reach the video server',
    'Check that you\'re online and try again. If other people can get in, your network may be blocking it. The admin can see what went wrong.'],
  'not-allowed': ['The video server turned you away',
    'Your pass for this room was refused. Go back to the lobby and join the room again.'],
  dropped: ['Connection lost',
    'Your connection to the room dropped and didn\'t come back. Check that you\'re online, then try again.'],
  'join-failed': ['Couldn\'t join the room', 'Something went wrong while joining. Try again in a moment.'],
};
function classify(e) {
  // reasonName is the SDK's ConnectionErrorReason as text, which is stable across SDK versions.
  const why = String(e?.reasonName ?? LK.ConnectionErrorReason?.[e?.reason] ?? '');
  const msg = String(e?.message || '');
  if (why === 'NotAllowed') return 'not-allowed';
  if (/pc connection|peer ?connection|\bice\b/i.test(msg)) return 'network-blocked';
  if (['ServerUnreachable', 'WebSocket', 'Timeout', 'ServiceNotFound'].includes(why) || /signal connection/i.test(msg)) return 'server-unreachable';
  return 'join-failed';
}
function report(kind, message, detail = {}) {
  api('/api/health/report', {
    kind, room: roomId, message: String(message).slice(0, 300),
    detail: { ...detail, online: navigator.onLine, net: navigator.connection?.effectiveType || null },
  }).catch(() => {});
}
// Auto-rejoins after a drop, counted per room so a broken connection can't reload forever.
function rejoinCount() {
  const key = `sr.rejoin.${roomId}`;
  const r = store.get(key, { n: 0, at: 0 });
  const n = Date.now() - r.at < 120_000 ? r.n : 0;
  store.set(key, { n: n + 1, at: Date.now() });
  return n;
}

// ---------- your own browser stream ----------
function onLocalVideo(track) {
  if (!track) { removeTile('local'); return; }
  const t = makeTile('local', user.name, { local: true });
  t.videoTrack = track;
  track.attach(t.video);
}
function onLiveChange(live) {
  $('liveBtn').textContent = live ? '■ Stop streaming' : '● Go Live';
  $('liveBtn').classList.toggle('live', live);
}
$('liveBtn').onclick = () => {
  if (browser?.isLive()) browser.stop();
  else { $('goErr').textContent = ''; $('goLive').showModal(); }
};
$('goClose').onclick = () => $('goLive').close();
// "Open Streamroom Live" launches the app through the streamroom:// link its installer
// registers. The link names the room and carries a single-use, 10-minute sign-in code; an app
// that's already signed in as you just ignores the code.
const appOpenLink = () => api(`/api/rooms/${encodeURIComponent(roomId)}/app-open`, {}).then((r) => r.url);
$('appOpenBtn').onclick = async () => {
  $('goErr').textContent = '';
  try {
    location.href = await appOpenLink();
    $('appAfter').classList.remove('hidden');
  } catch (e) { $('goErr').textContent = e.message; }
};
$('appCopyBtn').onclick = async () => {
  try { await copy(await appOpenLink(), 'Copied. In Streamroom Live, paste it into "Have a link?"'); } catch (e) { $('goErr').textContent = e.message; }
};
// The app is Windows-only: elsewhere, lead with the browser option.
if (!/Windows/.test(navigator.userAgent)) {
  $('browserBox').open = true;
  $('appHint').textContent = 'Streamroom Live is a Windows app. On this computer, stream from the browser below.';
  $('appOpenBtn').classList.remove('primary');
}
$('goLive').addEventListener('close', () => $('appAfter').classList.add('hidden'));

// ---------- stats on tiles ----------
let showStats = false;
setInterval(async () => {
  for (const t of tiles.values()) {
    if (!t.videoTrack || t.local) continue;
    const d = await receiverDiag(t.videoTrack).catch(() => null);
    if (!d) continue;
    t.diag = d;
    let label = d.height ? `${d.height}p · ${Math.round(d.fps || 0)}` : '';
    if (showStats && d.height) {
      const lost = d.packetsReceived ? Math.round((d.packetsLost / (d.packetsLost + d.packetsReceived)) * 1000) / 10 : 0;
      label += ` fps · ${d.codec || '?'} · loss ${lost}% · ${(d.transport.protocol || '?').toUpperCase()}`
        + (d.transport.rttMs != null ? ` · ${d.transport.rttMs} ms` : '');
    }
    t.el.querySelector('.q').textContent = label;
  }
}, 1000);
$('statsBtn').onclick = async () => {
  showStats = !showStats;
  $('statsBtn').setAttribute('aria-pressed', String(showStats));
  if (!showStats) return;
  const streams = [];
  for (const t of tiles.values()) {
    if (t.local || t.appOnly) continue;
    streams.push({ streamer: t.name, ...((await receiverDiag(t.videoTrack).catch(() => null)) || t.diag || {}) });
  }
  await copyDiag({ env: envDiag(), layout, streams });
  toast('Detailed stats on. Diagnostics copied.', 3000);
};

// ---------- controls ----------
document.querySelectorAll('[data-layout]').forEach((b) => { b.onclick = () => setLayout(b.dataset.layout); });
$('fsAll').onclick = () => fullscreen(document.documentElement);
window.addEventListener('keydown', (e) => {
  if ($('goLive').open || ['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || e.metaKey || e.ctrlKey || e.altKey) return;
  const k = e.key.toLowerCase();
  if (k === 'g') setLayout('grid');
  else if (k === 's') setLayout('spotlight');
  else if (k === 'p') setLayout('pip');
  else if (k === 'm') $('muteAll').click();
  else if (k === 'f' && focusId) fullscreen(tiles.get(focusId).el);
  else if (k === 'arrowright' || k === 'arrowleft') {
    const ids = [...tiles.keys()];
    if (ids.length < 2) return;
    const i = ids.indexOf(focusId);
    focusId = ids[(i + (k === 'arrowright' ? 1 : -1) + ids.length) % ids.length];
    relayout();
  }
});

// ---------- start ----------
(async () => {
  try {
    user = await me();
    if (!user) { location.href = '/'; return; }
    const info = await api(`/api/rooms/${encodeURIComponent(roomId)}`).catch((e) => {
      if (e.status === 404) showOverlay('This room has ended', 'Head back to the lobby to find another one.');
      throw e;
    });
    $('title').textContent = info.title;
    document.title = `${info.title} · Streamroom`;
    if (info.canEnd) {
      $('endBtn').classList.remove('hidden');
      confirmButton($('endBtn'), 'End for everyone?', async () => {
        try { await api(`/api/rooms/${encodeURIComponent(roomId)}/end`, {}); } catch (e) { toast(e.message); }
      });
    }
    await join();
    for (const p of room.remoteParticipants.values()) for (const pub of p.trackPublications.values()) handlePublication(pub, p);
    browser = setupBrowserStreaming({ room, roomId, onLocalVideo, onLiveChange });
    $('liveBtn').disabled = false;
    renderMute();
    updateCount();
  } catch (e) {
    console.error(e);
    if (!$('overlay').classList.contains('hidden')) return;
    if (e.status) { showOverlay('Couldn\'t join the room', e.message, { rejoin: true }); return; }
    showOverlay(...MESSAGES[e.kind || 'join-failed'], { rejoin: true });
  }
})();

// Connect, retrying network trouble twice before giving up. Each failure is reported so the
// admin's Health panel shows who couldn't connect and why.
async function join() {
  const delays = [0, 2000, 5000];
  for (let i = 0; i < delays.length; i++) {
    if (delays[i]) {
      toast('Connection trouble, trying again…');
      await new Promise((r) => setTimeout(r, delays[i]));
    }
    try {
      const { token, url } = await api(`/api/rooms/${encodeURIComponent(roomId)}/join`, {});
      await room.connect(livekitUrl(url), token, { autoSubscribe: false });
      joined = true;
      return;
    } catch (e) {
      if (e.status) throw e; // our own API said no (room ended, signed out): not a network problem
      const kind = classify(e);
      if (kind === 'not-allowed' || i === delays.length - 1) {
        report(kind, e.message, { attempts: i + 1, reason: e.reasonName ?? e.reason ?? null });
        e.kind = kind;
        throw e;
      }
    }
  }
}
