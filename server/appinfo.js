// The Windows app's downloads live on GitHub Releases. This only looks up the latest version
// (cached, so the download page doesn't hit GitHub's rate limit) and points at the installer.
import express from 'express';

export const APP_RELEASES_REPO = process.env.APP_RELEASES_REPO || 'Saroknight/stream.eventworks';
const SETUP = 'Streamroom-Live-Setup.exe';
const LINUX_PKG = 'streamroom-live-x86_64.pkg.tar.zst';
const CACHE_MS = 10 * 60_000;

// Windows releases are tagged app-v<version>, Linux ones linux-v<version>.
let cache = { at: 0, value: null, linux: null };

async function latest() {
  if (cache.value && Date.now() - cache.at < CACHE_MS) return cache.value;
  try {
    const r = await fetch(`https://api.github.com/repos/${APP_RELEASES_REPO}/releases?per_page=10`, {
      headers: { Accept: 'application/vnd.github+json', 'User-Agent': 'streamroom-server' },
      signal: AbortSignal.timeout(5000),
    });
    if (r.status === 404) {
      cache = { at: Date.now(), value: { version: null } };
    } else if (r.ok) {
      const list = (await r.json()).filter((x) => !x.draft && !x.prerelease);
      const has = (name) => (x) => (x.assets || []).some((a) => a.name === name);
      const rel = list.find((x) => /^app-v/.test(x.tag_name) && has(SETUP)(x)) || {};
      const asset = (rel.assets || []).find((a) => a.name === SETUP);
      const lrel = list.find((x) => /^(linux|app)-v/.test(x.tag_name) && has(LINUX_PKG)(x));
      cache = {
        at: Date.now(),
        linux: lrel ? lrel.assets.find((a) => a.name === LINUX_PKG).browser_download_url : null,
        value: {
          version: asset ? String(rel.tag_name || '').replace(/^app-v|^v/, '') : null,
          published: rel.published_at || null,
          size: asset?.size || null,
          url: asset?.browser_download_url || null,
        },
      };
    }
  } catch { /* GitHub unreachable: keep whatever we had */ }
  return cache.value || { version: null };
}

export const router = express.Router();
router.get('/app/latest', async (_req, res) => {
  const v = await latest();
  res.json({ version: v.version, published: v.published || null, size: v.size || null });
});
// A stable link for the button: the installer from the newest release that has one.
router.get('/app/download', async (_req, res) => {
  const v = await latest();
  res.redirect(v.url || `https://github.com/${APP_RELEASES_REPO}/releases/latest/download/${SETUP}`);
});
// The Linux package, from the newest Linux release (used by the download page's command).
router.get('/app/download/linux', async (_req, res) => {
  await latest();
  if (!cache.linux) return res.status(404).type('text').send('No Linux release yet.\n');
  res.redirect(cache.linux);
});
