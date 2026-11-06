// Shared helpers for all pages.
export async function api(path, body) {
  const res = await fetch(path, body ? {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  } : undefined);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `Request failed (${res.status})`);
  return data;
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

export function toast(msg, ms = 2200) {
  const el = document.createElement('div');
  el.className = 'toast';
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(() => el.remove(), ms);
}

export async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast('Link copied'); }
  catch { prompt('Copy this link:', text); }
}

export const roomIdFromPath = () => decodeURIComponent(location.pathname.split('/').filter(Boolean)[1] || '');
