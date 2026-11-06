// Port 443 fallback test (CI): the site is installed with deploy/linux/setup.sh, and the
// browser is blocked from LiveKit's normal video ports, so video can only arrive through the
// TURN relay on port 443. Usage: node tests/relay.mjs <https base> <admin link>
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright';

const [BASE, ADMIN] = process.argv.slice(2);
const SHOTS = process.env.E2E_SHOTS || 'test-results';
fs.mkdirSync(SHOTS, { recursive: true });

const wait = async (fn, what, ms) => {
  const end = Date.now() + ms;
  for (;;) {
    try { if (await fn()) return; } catch { /* not yet */ }
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await new Promise((r) => setTimeout(r, 500));
  }
};

const browser = await chromium.launch({
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', '--auto-select-desktop-capture-source=Entire screen'],
});
const errors = [];
async function user() {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  // Relay only (the site's troubleshooting switch), on top of the blocked ports.
  await ctx.addInitScript(() => { try { localStorage.setItem('sr.forceRelay', 'true'); } catch {} });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') console.log('  [console]', m.text()); });
  return page;
}

let ok = false;
const a = await user();
const f = await user();
try {
  await a.goto(ADMIN);
  await a.waitForSelector('#go:not(.hidden)');
  await a.fill('#name', 'Martin');
  await a.click('#go');
  await a.waitForSelector('#lobby:not(.hidden)');
  console.log('✓ admin signed in over https');

  const inv = await a.evaluate(() => fetch('/api/admin/invites', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"name":"Alex"}' }).then((r) => r.json()));
  await f.goto(inv.url);
  await f.waitForSelector('#go:not(.hidden)');
  await f.click('#go');
  await f.waitForSelector('#lobby:not(.hidden)');
  await f.fill('#roomTitle', 'Relay test');
  await f.click('#startBtn');
  await f.waitForURL('**/r/*');
  await f.waitForSelector('#liveBtn:not([disabled])', { timeout: 60000 });
  console.log('✓ joined a room with direct video ports blocked');

  await f.click('#liveBtn');
  await f.waitForSelector('#goLive[open]');
  if (!(await f.$eval('#browserBox', (d) => d.open))) await f.click('#browserBox summary');
  await f.click('#browserGo');
  await f.waitForSelector('#liveBar:not(.hidden)', { timeout: 60000 });
  console.log('✓ streaming from the browser');

  await a.goto(f.url());
  await a.waitForSelector('.tile video', { timeout: 60000 });
  await wait(() => a.$eval('.tile video', (v) => v.videoWidth > 0), 'video frames through the relay', 60000);
  console.log('✓ viewer receives video through the port 443 relay');
  await a.screenshot({ path: path.join(SHOTS, 'relay-viewer.png') });

  const health = await a.evaluate(() => fetch('/api/admin/health').then((r) => r.json()));
  console.log(`  health: relay ${health.relay.status} (${health.relay.text}); video address ${health.media.status}`);
  if (health.relay.status !== 'ok') throw new Error('health panel does not report the relay as on');
  if (errors.length) throw new Error(`script errors: ${errors.join('; ')}`);
  ok = true;
} catch (e) {
  console.log(`✗ ${e.message}`);
  await a.screenshot({ path: path.join(SHOTS, 'relay-admin-failed.png') }).catch(() => {});
  await f.screenshot({ path: path.join(SHOTS, 'relay-friend-failed.png') }).catch(() => {});
} finally {
  await browser.close();
}
process.exit(ok ? 0 : 1);
