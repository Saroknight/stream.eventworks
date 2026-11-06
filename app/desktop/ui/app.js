// Streamroom Live window. Talks to the Rust side through Tauri commands and events.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let snap = null;      // last full state from Rust
let liveSince = 0;
let lastStats = null;
let lowFpsSeconds = 0;

function toast(msg, ms = 2400) {
  $('toast').textContent = msg;
  $('toast').classList.remove('hidden');
  clearTimeout(toast.t);
  toast.t = setTimeout(() => $('toast').classList.add('hidden'), ms);
}

function showState(name) {
  document.querySelectorAll('.state').forEach((s) => s.classList.toggle('hidden', s.dataset.state !== name));
}

function hostOf(url) { try { return new URL(url).host; } catch { return url; } }

function renderStatus(status) {
  const kind = status.kind;
  if (kind === 'idle') showState('idle');
  else if (kind === 'connecting') {
    showState('connecting');
    $('connectingTo').textContent = `Joining your room on ${hostOf(status.server)}`;
  } else if (kind === 'live') {
    showState('live');
    const { info } = status;
    liveSince = status.since;
    $('roomTitle').textContent = info.room_title;
    const v = info.video;
    const scaled = v.width !== v.source_width ? ` (from ${v.source_width}×${v.source_height})` : '';
    $('details').textContent = `${v.width}×${v.height}${scaled} · ${v.fps} fps · ${v.encoder} · ${info.bitrate_mbps} Mbps · `
      + `audio ${info.audio}`;
    lowFpsSeconds = 0;
  } else {
    showState('ended');
    const isError = kind === 'error';
    $('endedTitle').textContent = isError ? 'Couldn\'t go live' : 'Stream ended';
    $('endedText').textContent = status.message;
    $('retryBtn').classList.toggle('hidden', !isError);
  }
  $('applyRow').classList.add('hidden');
  renderUpdate();
}

function renderStats(s) {
  lastStats = s;
  $('sSent').textContent = Math.round(s.sent_fps);
  $('sCap').textContent = Math.round(s.capture_fps);
  $('sMbps').textContent = s.mbps.toFixed(1);
  $('sEnc').textContent = s.encode_ms ? s.encode_ms.toFixed(1) : '–';
  lowFpsSeconds = s.capture_fps < 10 ? lowFpsSeconds + 1 : 0;
  $('stillHint').classList.toggle('hidden', lowFpsSeconds < 4);
}

setInterval(() => {
  if (!liveSince || snap?.status?.kind !== 'live') return;
  const s = Math.max(0, Math.floor((Date.now() - liveSince) / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, '0');
  $('timer').textContent = h ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
}, 500);

// ---------- settings ----------
function renderSettings() {
  const sel = $('monitorSel');
  sel.replaceChildren(...(snap.monitors.length ? snap.monitors : [{ index: 0, name: 'Primary screen', width: 0, height: 0, primary: true }])
    .map((m) => {
      const o = document.createElement('option');
      o.value = m.index;
      o.textContent = `${m.name}${m.width ? ` · ${m.width}×${m.height}` : ''}${m.primary ? ' (main)' : ''}`;
      return o;
    }));
  document.querySelectorAll('[data-s]').forEach((el) => {
    const v = snap.settings[el.dataset.s];
    if (el.type === 'checkbox') el.checked = Boolean(v);
    else el.value = String(v);
  });
}

async function saveSettings() {
  const s = { ...snap.settings };
  document.querySelectorAll('[data-s]').forEach((el) => {
    s[el.dataset.s] = el.type === 'checkbox' ? el.checked : Number(el.value);
  });
  try {
    await invoke('save_settings', { settings: s });
    snap.settings = s;
    if (snap.status.kind === 'live') $('applyRow').classList.remove('hidden');
  } catch (e) { toast(String(e)); }
}
document.querySelectorAll('[data-s]').forEach((el) => el.addEventListener('change', saveSettings));
$('applyBtn').onclick = async () => {
  $('applyRow').classList.add('hidden');
  try { await invoke('restart'); } catch (e) { toast(String(e)); }
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

// ---------- actions ----------
$('stopBtn').onclick = async () => {
  $('stopBtn').disabled = true;
  try { await invoke('stop'); } finally { $('stopBtn').disabled = false; }
};
$('okBtn').onclick = () => invoke('dismiss');
$('retryBtn').onclick = async () => { try { await invoke('restart'); } catch (e) { toast(String(e)); } };
$('pasteForm').onsubmit = async (e) => {
  e.preventDefault();
  const link = $('pasteInput').value.trim();
  if (!link) return;
  try {
    await invoke('start_link', { link });
    $('pasteInput').value = '';
  } catch (err) { toast(String(err), 5000); }
};
$('logBtn').onclick = () => invoke('open_logs');
$('diagBtn').onclick = async () => {
  const d = { version: snap.version, status: snap.status, settings: snap.settings, monitors: snap.monitors, stats: lastStats, log: snap.log_path, time: new Date().toISOString() };
  try {
    await navigator.clipboard.writeText(JSON.stringify(d, null, 1));
    toast('Diagnostics copied');
  } catch { toast('Couldn\'t copy. The log has the same details.'); }
};

// ---------- start ----------
(async () => {
  snap = await invoke('get_state');
  $('version').textContent = `version ${snap.version}`;
  renderSettings();
  renderStatus(snap.status);
  await listen('status', (e) => { snap.status = e.payload; renderStatus(e.payload); });
  await listen('stats', (e) => renderStats(e.payload));
  await listen('update', (e) => { snap.update = e.payload; renderUpdate(); });
  // Monitors can change (plugged in / out): refresh when the window gets focus.
  window.addEventListener('focus', async () => {
    const s = await invoke('get_state');
    snap.monitors = s.monitors;
    if (document.activeElement?.tagName !== 'SELECT') renderSettings();
  });
})();
