// Signed, expiring tickets (HMAC-SHA256): admin sign-in links, "sign in another device" links,
// and launch tickets for the Windows app. No database needed to check them.
import crypto from 'node:crypto';
import { AUTH_SECRET } from './config.js';

const sign = (data) => crypto.createHmac('sha256', AUTH_SECRET).update(data).digest('base64url');

export function makeTicket(payload, ttlMs) {
  const body = Buffer.from(JSON.stringify({
    ...payload, exp: Date.now() + ttlMs, jti: crypto.randomBytes(9).toString('base64url'),
  })).toString('base64url');
  return `${body}.${sign(body)}`;
}

// Returns the payload, or null if forged, malformed, expired or of the wrong purpose.
export function readTicket(ticket, purpose) {
  if (typeof ticket !== 'string' || ticket.length > 2000) return null;
  const [body, sig] = ticket.split('.');
  if (!body || !sig) return null;
  const expected = sign(body);
  if (sig.length !== expected.length || !crypto.timingSafeEqual(Buffer.from(sig), Buffer.from(expected))) return null;
  let p;
  try { p = JSON.parse(Buffer.from(body, 'base64url').toString()); } catch { return null; }
  if (p.p !== purpose || typeof p.exp !== 'number' || p.exp < Date.now()) return null;
  return p;
}
