// End-to-end test: starts LiveKit and the site with empty data, then drives real browsers
// through the main flows. Runs in CI on every push, and locally with `npm run test:e2e`.
//
// Needs: livekit-server on PATH (or LIVEKIT_BIN), Playwright with Chromium
// (`npx playwright install chromium`), or CHROME_PATH pointing at a Chrome binary.
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT || 3300);
const BASE = `http://localhost:${PORT}`;
const DATA = fs.mkdtempSync(path.join(os.tmpdir(), 'streamroom-e2e-'));
const SHOTS = process.env.E2E_SHOTS || path.join(ROOT, 'test-results');
fs.mkdirSync(SHOTS, { recursive: true });

// Browsers only offer WebRTC candidates on real interfaces, so LiveKit must advertise one.
const nodeIp = process.env.E2E_NODE_IP
  || Object.values(os.networkInterfaces()).flat().find((i) => i && i.family === 'IPv4' && !i.internal)?.address
  || '127.0.0.1';

const procs = [];
function start(name, cmd, args, env = {}) {
  const p = spawn(cmd, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  const log = fs.createWriteStream(path.join(SHOTS, `${name}.log`));
  p.stdout.pipe(log); p.stderr.pipe(log);
  p.on('exit', (code) => { if (!stopping) console.log(`[${name}] exited early (${code})`); });
  procs.push(p);
  return p;
}
let stopping = false;
function stopAll() { stopping = true; procs.forEach((p) => p.kill()); }

async function waitFor(fn, what, ms = 20000) {
  const end = Date.now() + ms;
  for (;;) {
    try { if (await fn()) return; } catch { /* not yet */ }
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

let failures = 0;
let passes = 0;
async function step(name, fn) {
  const t = Date.now();
  try {
    await fn();
    passes++;
    console.log(`  ✓ ${name} (${Date.now() - t} ms)`);
  } catch (e) {
    failures++;
    console.log(`  ✗ ${name}\n      ${String(e.message || e).split('\n').join('\n      ')}`);
    throw e;
  }
}
const expect = (cond, msg) => { if (!cond) throw new Error(msg); };

// Proxies would swallow localhost WebSockets.
const noProxy = Object.fromEntries(Object.keys(process.env).filter((k) => /proxy/i.test(k)).map((k) => [k, '']));

async function main() {
  console.log(`LiveKit media address: ${nodeIp}; data: ${DATA}`);
  start('livekit', process.env.LIVEKIT_BIN || 'livekit-server', ['--dev', '--bind', '0.0.0.0', '--node-ip', nodeIp]);
  start('server', process.execPath, [path.join(ROOT, 'server/index.js')], {
    PORT: String(PORT), DATA_DIR: DATA, PUBLIC_URL: BASE, APP_RELEASES_REPO: 'Saroknight/stream.eventworks', ...noProxy,
  });
  await waitFor(async () => (await fetch(`${BASE}/api/me`)).status === 401 || true, 'site');
  await waitFor(async () => (await fetch('http://127.0.0.1:7880')).ok, 'LiveKit');

  const adminLink = execFileSync(process.execPath, [path.join(ROOT, 'server/cli.js'), 'admin-link', BASE],
    { env: { ...process.env, DATA_DIR: DATA } }).toString().match(/https?:\/\/\S+\/join\/\S+/)[0];

  const browser = await chromium.launch({
    executablePath: process.env.CHROME_PATH || undefined,
    args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream',
      '--auto-select-desktop-capture-source=Entire screen', '--no-proxy-server'],
  });
  const pageErrors = [];
  async function newUser(w = 1280, h = 800, opts = {}) {
    const ctx = await browser.newContext({ viewport: { width: w, height: h }, ...opts });
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: BASE });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => pageErrors.push(String(e)));
    return { ctx, page };
  }

  try {
    const anon = await newUser();
    await step('anonymous visitor sees the invite-only page', async () => {
      await anon.page.goto(BASE);
      await anon.page.waitForSelector('#welcome:not(.hidden)');
    });

    const admin = await newUser();
    const a = admin.page;
    await step('admin signs in with the admin link', async () => {
      await a.goto(adminLink);
      await a.waitForSelector('#go:not(.hidden)');
      await a.fill('#name', 'Martin');
      await a.click('#go');
      await a.waitForSelector('#lobby:not(.hidden)');
    });

    let invite;
    await step('admin creates an invite', async () => {
      await a.goto(`${BASE}/admin`);
      await a.fill('#inviteName', 'Alex');
      await a.click('#inviteForm button');
      await a.waitForSelector('#newLink:not(.hidden)');
      invite = await a.inputValue('#newLinkUrl');
      expect(invite.includes('/join/'), `unexpected invite link ${invite}`);
    });

    const friend = await newUser();
    const f = friend.page;
    await step('friend signs in with the invite', async () => {
      await f.goto(invite);
      await f.waitForSelector('#go:not(.hidden)');
      await f.click('#go');
      await f.waitForSelector('#lobby:not(.hidden)');
    });

    let roomUrl;
    let appLaunch;
    await step('friend starts a room', async () => {
      await f.fill('#roomTitle', 'Test night');
      await f.click('#startBtn');
      await f.waitForURL('**/r/*');
      roomUrl = f.url();
      await f.waitForSelector('#liveBtn:not([disabled])', { timeout: 20000 });
    });

    await step('room shows in the admin lobby', async () => {
      await a.goto(BASE);
      await a.waitForSelector('.room-card', { timeout: 10000 });
      expect((await a.innerText('.room-card')).includes('Test night'), 'room card missing title');
    });

    await step('Go Live hands the room to the app (streamroom:// link)', async () => {
      // Its own tab: headless Chrome leaves the "open app?" prompt pending, which wedges the tab.
      const p = await friend.ctx.newPage();
      p.on('pageerror', (e) => pageErrors.push(String(e)));
      await p.goto(roomUrl);
      await p.waitForSelector('#liveBtn:not([disabled])', { timeout: 20000 });
      await p.click('#liveBtn');
      await p.waitForSelector('#goLive[open]');
      const opened = [];
      p.on('request', (r) => { if (r.url().startsWith('streamroom:')) opened.push(r.url()); });
      await p.click('#appOpenBtn');
      await waitFor(() => opened.length > 0, 'streamroom:// navigation', 8000);
      const deep = new URL(opened[0]);
      expect(deep.protocol === 'streamroom:' && deep.searchParams.get('room') && deep.searchParams.get('pair'), `bad deep link ${opened[0]}`);
      appLaunch = opened[0];
      // Opening the app must not drop the page from the room.
      await p.waitForTimeout(1500);
      expect(await p.isEnabled('#liveBtn'), 'room page disconnected after opening the app');
      await p.screenshot({ path: path.join(SHOTS, 'golive.png') });
      await p.close({ runBeforeUnload: false }).catch(() => {});
    });

    await step('the app signs in once with the code and can do everything with its key', async () => {
      const deep = new URL(appLaunch);
      const server = deep.searchParams.get('server');
      const post = (url, body, key) => fetch(`${server}${url}`, {
        method: 'POST', headers: { 'Content-Type': 'application/json', ...(key ? { Authorization: `Bearer ${key}` } : {}) }, body: JSON.stringify(body || {}),
      });
      const get = (url, key) => fetch(`${server}${url}`, { headers: { Authorization: `Bearer ${key}` } });
      const pair = await post('/api/app/pair', { code: deep.searchParams.get('pair'), deviceName: 'TEST-PC' });
      expect(pair.ok, `pair ${pair.status}`);
      const { deviceKey, user } = await pair.json();
      expect(user.name === 'Alex' && deviceKey, 'pair reply');
      const again = await post('/api/app/pair', { code: deep.searchParams.get('pair'), deviceName: 'TEST-PC' });
      expect(again.status === 403, 'pair code worked twice');
      const rooms = await (await get('/api/rooms', deviceKey)).json();
      expect(rooms.rooms.some((r) => r.id === deep.searchParams.get('room')), 'app cannot see the room');
      const tok = await post(`/api/rooms/${deep.searchParams.get('room')}/app-token`, {}, deviceKey);
      expect(tok.ok && (await tok.json()).token, `app token ${tok.status}`);
      const made = await post('/api/rooms', { title: 'From the app' }, deviceKey);
      const madeRoom = await made.json();
      expect(made.ok && madeRoom.id, 'app could not start a room');
      const ended = await post(`/api/rooms/${madeRoom.id}/end`, {}, deviceKey);
      expect(ended.ok, 'app could not end its room');
      // The app's key never opens admin pages, even for the admin.
      const adminTry = await get('/api/admin/state', deviceKey);
      expect(adminTry.status === 401 || adminTry.status === 403, `admin via app key: ${adminTry.status}`);
      // Shows on the admin page as a device, and signing out kills the key.
      await a.goto(`${BASE}/admin`);
      await a.waitForSelector('#people tr');
      expect((await a.innerText('#people')).includes('Windows app · TEST-PC'), 'app device not listed');
      await post('/api/app/signout', {}, deviceKey);
      expect((await get('/api/app/me', deviceKey)).status === 401, 'key still works after sign-out');
    });

    await step('friend streams from the browser and the admin sees it', async () => {
      await f.click('#liveBtn');
      await f.waitForSelector('#goLive[open]');
      if (!(await f.$eval('#browserBox', (d) => d.open))) await f.click('#browserBox summary');
      await f.click('#browserGo');
      await f.waitForSelector('#liveBar:not(.hidden)', { timeout: 20000 }).catch(async (e) => {
        await f.screenshot({ path: path.join(SHOTS, 'browser-golive-failed.png') });
        throw new Error(`${e.message.split('\n')[0]}; dialog says: ${await f.innerText('#goErr').catch(() => '?')}`);
      });
      await a.goto(roomUrl);
      await a.waitForSelector('.tile video', { timeout: 20000 });
      await waitFor(() => a.$eval('.tile video', (v) => v.videoWidth > 0), 'video frames', 20000);
      await a.screenshot({ path: path.join(SHOTS, 'room-viewer.png') });
    });

    // Optional: the real streaming engine (Linux build of the app's engine, test pattern).
    if (process.env.E2E_CLI) {
      await step('the app engine signs in with the link and streams; viewers see it', async () => {
        await f.click('#liveBtn'); // stop the browser stream so the app's is the only one
        await f.waitForSelector('#liveBar', { state: 'hidden' });
        const { url } = await f.evaluate((id) => fetch(`/api/rooms/${id}/app-open`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' }).then((r) => r.json()), roomUrl.split('/').pop());
        const cli = start('app-engine', process.env.E2E_CLI, [url, '--test-pattern', '--height', '720'], noProxy);
        try {
          await waitFor(() => a.$$eval('.tile video', (vs) => vs.some((v) => v.videoHeight === 720)), '720p video from the app', 30000);
          // The streamer's own page shows the placeholder, not a second copy of the video.
          await waitFor(() => f.$eval('.tile.mine', (t) => t.innerText.includes('You (app)')), 'You (app) tile', 10000);
          await a.screenshot({ path: path.join(SHOTS, 'room-app-stream.png') });
        } finally { cli.kill('SIGINT'); }
        await waitFor(() => a.$$eval('.tile video', (vs) => vs.length === 0), 'app stream gone after stop', 15000);
        // Restart the browser stream for the following steps.
        await f.click('#liveBtn');
        await f.waitForSelector('#goLive[open]');
        await f.click('#browserGo');
        await f.waitForSelector('#liveBar:not(.hidden)', { timeout: 20000 });
      });
    }

    await step('download page and app info API', async () => {
      const r = await fetch(`${BASE}/api/app/latest`);
      expect(r.ok, `/api/app/latest ${r.status}`);
      await anon.page.goto(`${BASE}/download`);
      await anon.page.waitForSelector('#dlBtn');
      const dl = await fetch(`${BASE}/api/app/download`, { redirect: 'manual' });
      expect(dl.status === 302, `/api/app/download ${dl.status}`);
    });

    await step('a blocked video connection gets a clear message and shows in Health', async () => {
      const p = await friend.ctx.newPage();
      // Kill LiveKit's WebSocket, as a hostile network would.
      await p.routeWebSocket(/\/rtc/, (ws) => ws.close());
      await p.goto(roomUrl);
      await p.waitForSelector('#overlay:not(.hidden)', { timeout: 30000 });
      const title = await p.innerText('#overlayTitle');
      const reported = await a.evaluate(() => fetch('/api/admin/health').then((r) => r.json()).then((j) => j.events.slice(0, 3).map((e) => `${e.kind}: ${e.message} ${JSON.stringify(e.detail)}`)));
      expect(/can't reach the video server/i.test(title), `unexpected message: ${title}; reported: ${reported.join(' | ')}`);
      await p.screenshot({ path: path.join(SHOTS, 'blocked.png') });
      await p.close();
      const ah = await admin.ctx.newPage();
      await ah.goto(`${BASE}/admin`);
      await ah.waitForSelector('.health-card');
      await ah.click('#healthEventsBox summary');
      await ah.waitForSelector('#healthEvents tr', { timeout: 5000 });
      const row = await ah.innerText('#healthEvents tr');
      expect(row.includes('Alex') && /video server/i.test(row), `health row: ${row}`);
      await ah.screenshot({ path: path.join(SHOTS, 'admin-health.png'), fullPage: true });
      await ah.close();
    });

    await step('ending the room closes it for viewers', async () => {
      await f.bringToFront();
      await f.click('#liveBtn'); // stop browser stream
      await f.waitForSelector('#endBtn:not(.hidden)');
      await f.click('#endBtn');
      await f.click('#endBtn'); // confirm
      await a.waitForSelector('#overlay:not(.hidden)', { timeout: 10000 });
      expect((await a.innerText('#overlayTitle')).includes('ended'), 'viewer did not see the room end');
    });

    await step('admin removes the friend, who is signed out', async () => {
      await a.goto(`${BASE}/admin`);
      const btn = a.locator('#people tr', { hasText: 'Alex' }).locator('button.danger');
      await btn.click();
      await btn.click();
      await waitFor(async () => (await f.evaluate(() => fetch('/api/me').then((r) => r.status))) === 401, 'friend signed out', 8000);
    });

    await step('no script errors on any page', async () => {
      expect(pageErrors.length === 0, pageErrors.join('\n'));
    });
  } finally {
    await browser.close();
  }
}

main()
  .catch(() => { failures = Math.max(failures, 1); })
  .finally(() => {
    stopAll();
    console.log(`\n${passes} passed, ${failures} failed. Logs and screenshots: ${SHOTS}`);
    process.exit(failures ? 1 : 0);
  });
