// Streamroom Live window. Talks to the Rust side through Tauri commands and events.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let snap = null;      // last full state from Rust
let rooms = [];       // the lobby, refreshed while signed in
let liveSince = 0;
let lastStats = null;
let lowFpsSeconds = 0;
let recording = { state: 'off' }; // saving a copy of the stream (Linux)

function toast(msg, ms = 2600) {
  $('toast').textContent = msg;
  $('toast').classList.remove('hidden');
  clearTimeout(toast.t);
  toast.t = setTimeout(() => $('toast').classList.add('hidden'), ms);
}
const call = async (cmd, args) => {
  try { return await invoke(cmd, args); } catch (e) { toast(String(e), 5000); throw e; }
};

// Small element helper: h('div.card', {onclick}, children...)
function h(tag, props = {}, ...kids) {
  const [name, ...cls] = tag.split('.');
  const el = document.createElement(name);
  if (cls.length) el.className = cls.join(' ');
  for (const [k, v] of Object.entries(props)) {
    if (k.startsWith('on')) el[k] = v; else if (v != null) el.setAttribute(k, v);
  }
  for (const k of kids.flat()) if (k != null) el.append(k);
  return el;
}

// A button that asks once more before doing something drastic.
function confirmButton(btn, armedText, action) {
  const original = btn.textContent;
  let armed = null;
  btn.onclick = async () => {
    if (!armed) {
      btn.textContent = armedText;
      btn.classList.add('armed');
      armed = setTimeout(() => { armed = null; btn.textContent = original; btn.classList.remove('armed'); }, 4000);
      return;
    }
    clearTimeout(armed); armed = null;
    btn.disabled = true;
    try { await action(); } finally { btn.disabled = false; btn.textContent = original; btn.classList.remove('armed'); }
  };
}

function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  return `${Math.floor(s / 3600)} h ago`;
}

// ---------- which view ----------
function showState(name) {
  document.querySelectorAll('.state').forEach((s) => s.classList.toggle('hidden', s.dataset.state !== name));
}
function hostOf(url) { try { return new URL(url).host; } catch { return url; } }

function render() {
  const st = snap.status;
  const signedIn = Boolean(snap.account);
  $('whoLine').textContent = signedIn ? `${snap.account.name} · ${snap.account.host}` : `version ${snap.version}`;
  // The menu is always there (Check for updates); the account items only when signed in.
  $('openSiteBtn').classList.toggle('hidden', !signedIn);
  $('signOutBtn').classList.toggle('hidden', !signedIn);
  $('startTitle').placeholder = signedIn ? `${snap.account.name}'s room` : '';
  // The paste box is the fallback for signing in; keep it out of the way once signed in.
  $('pasteBox').classList.toggle('hidden', signedIn && st.kind !== 'error');

  if (st.kind === 'idle') {
    showState(signedIn ? 'idle' : 'signin');
  } else if (st.kind === 'connecting') {
    showState('connecting');
    $('connectingTo').textContent = `Joining the room on ${hostOf(st.server)}`;
  } else if (st.kind === 'live') {
    showState('live');
    liveSince = st.since;
    $('roomTitle').textContent = st.info.room_title;
    const v = st.info.video;
    const scaled = v.width !== v.source_width ? ` (from ${v.source_width}×${v.source_height})` : '';
    $('details').textContent = `${v.width}×${v.height}${scaled} · ${v.fps} fps · ${v.encoder} · ${st.info.bitrate_mbps} Mbps · audio ${st.info.audio}`;
    lowFpsSeconds = 0;
    recording = st.info.recording;
    renderRecording();
    renderLiveRoom();
  } else {
    showState('ended');
    const isError = st.kind === 'error';
    $('endedTitle').textContent = isError ? 'Couldn\'t go live' : 'Stream ended';
    $('endedText').textContent = st.message;
    $('retryBtn').classList.toggle('hidden', !isError || !snap.room);
  }
  $('applyRow').classList.toggle('hidden', !(st.kind === 'live' && $('applyRow').dataset.pending === '1'));
  renderUpdate();
}

// ---------- rooms ----------
function roomCard(r) {
  const liveNames = r.live.map((p) => p.name);
  const goBtn = h('button.btn.sm.primary', { onclick: () => call('go_live', { room: r.id }) }, 'Go live here');
  const watchBtn = h('button.btn.sm.ghost', { onclick: () => call('open_room', { room: r.id }) }, 'Watch');
  const actions = h('div.card-actions', {}, goBtn, watchBtn);
  if (r.canEnd) {
    const endBtn = h('button.btn.sm.ghost.danger', {}, 'End');
    confirmButton(endBtn, 'End for all?', async () => { await call('end_room', { room: r.id }); refreshRooms(); });
    actions.append(endBtn);
  }
  return h('div.room-card', {},
    h('div.room-top', {},
      h('b.room-title', {}, r.title),
      liveNames.length ? h('span.live-pill', {}, 'LIVE') : null),
    h('div.muted.small', {}, `${r.isCreator ? 'Your room' : `Started by ${r.creator.name}`} · ${ago(r.createdAt)} · ${r.here} here`),
    liveNames.length ? h('div.small.live-names', {}, `Streaming: ${liveNames.join(', ')}`) : null,
    actions);
}

function renderRooms() {
  $('roomsNote').textContent = rooms.length ? `${rooms.length} open` : '';
  $('rooms').replaceChildren(...(rooms.length ? rooms.map(roomCard)
    : [h('p.muted.small.empty', {}, 'No rooms right now. Start one above, and your friends will see it in their lobby.')]));
}

function renderLiveRoom() {
  const r = rooms.find((x) => x.id === snap.room);
  $('watchers').textContent = r ? `${Math.max(0, r.here - 1)} other${r.here - 1 === 1 ? '' : 's'} in the room` : '';
  $('watchBtn').classList.toggle('hidden', !snap.room);
  $('endRoomBtn').classList.toggle('hidden', !r?.canEnd);
}

let roomsBusy = false;
async function refreshRooms() {
  if (!snap?.account || roomsBusy) return;
  roomsBusy = true;
  try {
    rooms = await invoke('list_rooms');
    renderRooms();
    if (snap.status.kind === 'live') renderLiveRoom();
  } catch (e) {
    $('rooms').replaceChildren(h('p.muted.small.empty', {}, String(e)));
  } finally { roomsBusy = false; }
}
setInterval(() => { if (!document.hidden) refreshRooms(); }, 4000);

$('startForm').onsubmit = async (e) => {
  e.preventDefault();
  $('startBtn').disabled = true;
  try { await call('create_room', { title: $('startTitle').value }); $('startTitle').value = ''; } catch { /* toasted */ }
  $('startBtn').disabled = false;
};

// ---------- live ----------
function renderStats(s) {
  lastStats = s;
  $('sSent').textContent = Math.round(s.sent_fps);
  $('sCap').textContent = Math.round(s.capture_fps);
  $('sMbps').textContent = s.mbps.toFixed(1);
  $('sEnc').textContent = s.encode_ms ? s.encode_ms.toFixed(1) : '–';
  lowFpsSeconds = s.capture_fps < 10 ? lowFpsSeconds + 1 : 0;
  $('stillHint').classList.toggle('hidden', lowFpsSeconds < 4);
  recording = s.recording;
  renderRecording();
}

// ---------- saving a copy ----------
const REC_TEXT = {
  on: (r) => `Saving a copy: ${r.path}`,
  failed: (r) => `Saving stopped: ${r.why}`,
  off: () => 'Not saving a copy',
};
const REC_BUTTON = { on: 'Stop saving', failed: 'Try again', off: 'Save a copy' };
function renderRecording() {
  $('recordRow').classList.toggle('hidden', !snap.can_record);
  $('recordBox').checked = Boolean(snap.settings.record);
  $('recRow').classList.toggle('hidden', !snap.can_record);
  $('recRow').classList.toggle('failed', recording.state === 'failed');
  $('recText').textContent = REC_TEXT[recording.state](recording);
  $('recBtn').textContent = REC_BUTTON[recording.state];
}
async function setRecording(on) {
  $('recBtn').disabled = true;
  $('recordBox').disabled = true;
  try {
    const r = await call('set_recording', { on });
    snap.settings.record = on;
    if (snap.status.kind === 'live') recording = r;
  } catch { /* toasted */ }
  $('recBtn').disabled = false;
  $('recordBox').disabled = false;
  renderRecording();
}
$('recBtn').onclick = () => setRecording(recording.state !== 'on');
$('recordBox').onchange = () => setRecording($('recordBox').checked);

setInterval(() => {
  if (!liveSince || snap?.status?.kind !== 'live') return;
  const s = Math.max(0, Math.floor((Date.now() - liveSince) / 1000));
  const hr = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, '0');
  $('timer').textContent = hr ? `${hr}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
}, 500);

$('stopBtn').onclick = async () => {
  $('stopBtn').disabled = true;
  try { await invoke('stop'); } finally { $('stopBtn').disabled = false; }
};
$('watchBtn').onclick = () => call('open_room', { room: snap.room });
confirmButton($('endRoomBtn'), 'Sure? Ends it for everyone', () => call('end_room', { room: snap.room }));

// ---------- settings ----------
function renderSettings() {
  const sel = $('monitorSel');
  sel.replaceChildren(...(snap.monitors.length ? snap.monitors : [{ index: 0, name: 'Primary screen', width: 0, height: 0, primary: true }])
    .map((m) => h('option', { value: m.index }, `${m.name}${m.width ? ` · ${m.width}×${m.height}` : ''}${m.primary ? ' (main)' : ''}`)));
  document.querySelectorAll('[data-s]').forEach((el) => {
    const v = snap.settings[el.dataset.s];
    if (el.type === 'checkbox') el.checked = Boolean(v);
    else el.value = String(v);
  });
  renderAudio();
  renderRecording();
}

// ---------- sound ----------
const AUDIO_HINTS = {
  except_discord: 'Viewers hear your game and videos, but not your Discord calls (so no echo).',
  apps: 'Only the apps you tick below. They\'re picked up when they start.',
  all: 'Everything your PC plays, including Discord.',
  off: 'Viewers won\'t hear anything from your PC.',
};
function audioChoice() { return snap.settings.audio ? snap.settings.audio_mode || 'except_discord' : 'off'; }
function renderAudio() {
  const choice = audioChoice();
  $('audioSel').value = choice;
  $('audioHint').textContent = AUDIO_HINTS[choice];
  $('appsBox').classList.toggle('hidden', choice !== 'apps');
  if (choice === 'apps') renderApps();
}
let playing = [];
async function refreshApps() {
  try { playing = await invoke('audio_apps'); } catch { playing = []; }
  renderApps();
}
function renderApps() {
  const chosen = new Set(snap.settings.audio_apps || []);
  const names = [...new Set([...playing, ...chosen])].sort();
  $('appsList').replaceChildren(...(names.length ? names.map((n) => {
    const box = h('input', { type: 'checkbox' });
    box.checked = chosen.has(n);
    box.onchange = () => {
      const set = new Set(snap.settings.audio_apps || []);
      if (box.checked) set.add(n); else set.delete(n);
      snap.settings.audio_apps = [...set];
      saveSettings();
    };
    return h('label.toggle', {}, box, ` ${n.replace(/\.exe$/i, '')}`, playing.includes(n) ? null : h('span.muted.small', {}, ' (not playing now)'));
  }) : [h('p.muted.small', {}, 'Nothing is playing sound right now. Start your game, then click Refresh.')]));
}
$('audioSel').onchange = () => {
  const v = $('audioSel').value;
  snap.settings.audio = v !== 'off';
  if (v !== 'off') snap.settings.audio_mode = v;
  renderAudio();
  if (v === 'apps') refreshApps();
  saveSettings();
};
$('appsRefresh').onclick = refreshApps;

async function saveSettings() {
  const s = { ...snap.settings };
  document.querySelectorAll('[data-s]').forEach((el) => {
    s[el.dataset.s] = el.type === 'checkbox' ? el.checked : Number(el.value);
  });
  try {
    await invoke('save_settings', { settings: s });
    snap.settings = s;
    if (snap.status.kind === 'live') { $('applyRow').dataset.pending = '1'; $('applyRow').classList.remove('hidden'); }
  } catch (e) { toast(String(e)); }
}
document.querySelectorAll('[data-s]').forEach((el) => el.addEventListener('change', saveSettings));
$('applyBtn').onclick = async () => {
  $('applyRow').dataset.pending = '';
  $('applyRow').classList.add('hidden');
  await call('restart').catch(() => {});
};

// ---------- updates ----------
function renderUpdate() {
  const u = snap?.update;
  $('update').classList.toggle('hidden', !u);
  if (!u) return;
  const busy = ['live', 'connecting'].includes(snap.status.kind);
  $('updateText').textContent = busy ? `Version ${u.version} is ready. Update after you stop streaming.` : `Version ${u.version} is ready.`;
  $('updateBtn').disabled = busy;
}
$('updateBtn').onclick = async () => {
  $('updateBtn').disabled = true;
  $('updateBtn').textContent = 'Downloading…';
  try {
    await invoke('install_update'); // the app closes and the new version starts
  } catch (e) {
    toast(String(e), 5000);
    $('updateBtn').disabled = false;
    $('updateBtn').textContent = 'Update now';
  }
};

// ---------- account menu ----------
$('menuBtn').onclick = (e) => { e.stopPropagation(); $('menu').classList.toggle('hidden'); };
document.addEventListener('click', () => $('menu').classList.add('hidden'));
$('openSiteBtn').onclick = () => call('open_site');
$('checkUpdateBtn').onclick = async () => {
  const btn = $('checkUpdateBtn');
  if (btn.disabled) return;
  btn.disabled = true;
  toast('Checking for updates…', 10000);
  try {
    const u = await call('check_update'); // a failure is shown by call()
    toast(u ? `Version ${u.version} is ready. Update from the banner above.` : `You have the latest version (${snap.version}).`);
  } catch {
    // already shown
  } finally {
    btn.disabled = false;
  }
};
confirmButton($('signOutBtn'), 'Click again to sign out', async () => { await call('sign_out'); rooms = []; renderRooms(); });

// ---------- other actions ----------
$('okBtn').onclick = () => invoke('dismiss');
$('retryBtn').onclick = () => call('restart').catch(() => {});
$('pasteForm').onsubmit = async (e) => {
  e.preventDefault();
  const link = $('pasteInput').value.trim();
  if (!link) return;
  try { await call('start_link', { link }); $('pasteInput').value = ''; } catch { /* toasted */ }
};
$('logBtn').onclick = () => invoke('open_logs');
$('diagBtn').onclick = async () => {
  const d = {
    version: snap.version, status: snap.status, signedIn: Boolean(snap.account), server: snap.account?.server,
    room: snap.room, settings: snap.settings, monitors: snap.monitors, stats: lastStats, log: snap.log_path, time: new Date().toISOString(),
  };
  try {
    await navigator.clipboard.writeText(JSON.stringify(d, null, 1));
    toast('Diagnostics copied');
  } catch { toast('Couldn\'t copy. The log has the same details.'); }
};

// ---------- start ----------
(async () => {
  snap = await invoke('get_state');
  renderSettings();
  render();
  refreshRooms();
  await listen('status', async (e) => {
    snap.status = e.payload;
    const s = await invoke('get_state'); // picks up the room we're live in
    snap.room = s.room;
    render();
    refreshRooms();
  });
  await listen('account', (e) => { snap.account = e.payload; render(); refreshRooms(); });
  await listen('stats', (e) => renderStats(e.payload));
  await listen('update', (e) => { snap.update = e.payload; renderUpdate(); });
  // Monitors can change (plugged in / out): refresh when the window gets focus.
  window.addEventListener('focus', async () => {
    const s = await invoke('get_state');
    snap.monitors = s.monitors;
    if (document.activeElement?.tagName !== 'SELECT') renderSettings();
    refreshRooms();
  });
})();
