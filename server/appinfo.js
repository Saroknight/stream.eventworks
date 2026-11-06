// The Windows app's downloads live on GitHub Releases. This only looks up the latest version
// (cached, so the download page doesn't hit GitHub's rate limit) and points at the installer.
import express from 'express';

export const APP_RELEASES_REPO = process.env.APP_RELEASES_REPO || 'Saroknight/stream.eventworks';
const SETUP = 'Streamroom-Live-Setup.exe';
const CACHE_MS = 10 * 60_000;

let cache = { at: 0, value: null };

async function latest() {
  if (cache.value && Date.now() - cache.at < CACHE_MS) return cache.value;
  try {
    const r = await fetch(`https://api.github.com/repos/${APP_RELEASES_REPO}/releases/latest`, {
      headers: { Accept: 'application/vnd.github+json', 'User-Agent': 'streamroom-server' },
      signal: AbortSignal.timeout(5000),
    });
    if (r.status === 404) {
      cache = { at: Date.now(), value: { version: null } };
    } else if (r.ok) {
      const rel = await r.json();
      const asset = (rel.assets || []).find((a) => a.name === SETUP);
      cache = {
        at: Date.now(),
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
// A stable link for the button; GitHub's own /latest/download/ link does the rest.
router.get('/app/download', (_req, res) => {
  res.redirect(`https://github.com/${APP_RELEASES_REPO}/releases/latest/download/${SETUP}`);
});
