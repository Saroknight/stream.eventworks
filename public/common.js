// Shared helpers for all pages.

export class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

export async function api(path, body) {
  const res = await fetch(path, body !== undefined ? {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  } : undefined);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new ApiError(data.error || `Request failed (${res.status})`, res.status);
  return data;
}

// The signed-in person, or null.
export async function me() {
  try { return (await api('/api/me')).user; } catch (e) { if (e.status === 401) return null; throw e; }
}

// LiveKit URL: what the server says, otherwise derived from this page's address.
// Local (http): LiveKit dev server on port 7880 of the same machine.
// Production (https): same domain, Caddy forwards /rtc to LiveKit.
export function livekitUrl(fromServer) {
  if (fromServer) return fromServer;
  if (location.protocol === 'https:') return `wss://${location.host}`;
  return `ws://${location.hostname}:7880`;
}

export const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* storage blocked */ } },
};

export function toast(msg, ms = 2400) {
  const el = document.createElement('div');
  el.className = 'toast';
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(() => el.remove(), ms);
}

export async function copy(text, label = 'Link copied') {
  try { await navigator.clipboard.writeText(text); toast(label); return true; }
  catch { prompt('Copy this:', text); return false; }
}

// Tiny element builder: h('div.card', {onclick}, 'text', child)
export function h(tag, attrs = {}, ...children) {
  const [name, ...classes] = tag.split('.');
  const el = document.createElement(name || 'div');
  if (classes.length) el.className = classes.join(' ');
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith('on')) el[k] = v;
    else if (k === 'html') el.innerHTML = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined && c !== false) el.append(c);
  return el;
}

export function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

// Two-click confirm without browser dialogs: first click arms, second click (within 4 s) acts.
export function confirmButton(btn, armedText, action) {
  let armed = null;
  const original = btn.textContent;
  btn.onclick = async (e) => {
    e.stopPropagation();
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

// ---- account menu (name, sign in another device, admin, sign out) ----
export function accountMenu(user) {
  const menu = h('div.menu.hidden',
    { role: 'menu' },
    h('div.menu-head', {}, h('b', {}, user.name), user.isAdmin ? h('span.badge', {}, 'admin') : null),
    h('button.menu-item', { onclick: () => { close(); showDeviceLink(); } }, '📱 Sign in another device'),
    h('a.menu-item', { href: '/download' }, '🎮 Get the Windows app'),
    user.isAdmin ? h('a.menu-item', { href: '/admin' }, '⚙️ Admin') : null,
    h('button.menu-item', {
      onclick: async () => { await api('/api/auth/logout', {}); location.href = '/'; },
    }, 'Sign out'),
  );
  const btn = h('button.avatar', { title: user.name, 'aria-haspopup': 'menu', onclick: (e) => { e.stopPropagation(); menu.classList.toggle('hidden'); } },
    user.name.trim().charAt(0).toUpperCase() || '?');
  const close = () => menu.classList.add('hidden');
  document.addEventListener('click', (e) => { if (!menu.contains(e.target)) close(); });
  return h('div.account', {}, btn, menu);
}

export async function showDeviceLink() {
  const dlg = h('dialog.sheet.small', {},
    h('h2', {}, 'Sign in another device'),
    h('p.hint', {}, 'Scan this with your phone, or open the link on your other computer.'),
    h('div.qr', {}, 'Making a link…'),
  );
  document.body.append(dlg);
  dlg.addEventListener('close', () => dlg.remove());
  dlg.showModal();
  try {
    const { url, svg, minutes } = await api('/api/auth/device-link', {});
    dlg.querySelector('.qr').innerHTML = svg;
    dlg.append(
      h('div.linkbox', {}, h('input', { type: 'text', readonly: true, value: url }), h('button.btn', { onclick: () => copy(url) }, 'Copy')),
      h('p.hint', {}, `Works once, for ${minutes} minutes. Don't share it: it signs in as you.`),
    );
  } catch (e) {
    dlg.querySelector('.qr').textContent = e.message;
  }
  dlg.append(h('div.sheet-actions', {}, h('button.btn', { onclick: () => dlg.close() }, 'Done')));
}

export const roomIdFromPath = () => decodeURIComponent(location.pathname.split('/').filter(Boolean)[1] || '');
