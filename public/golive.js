// Browser streaming (the fallback to the Windows app): screen capture, encoder choice,
// advanced settings, the "find best settings" tester, and live stats with advice.
// Publishes into the room the page is already connected to.
import * as LK from '/vendor/livekit-client.esm.mjs';
import { api, livekitUrl, store, toast } from '/common.js';
import { copyDiag, envDiag, senderDiag } from '/diag.js';

const $ = (id) => document.getElementById(id);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- encoder choice ----
// Software H.264 (OpenH264) with two layers performed far worse than VP8 in testing, so H.264
// is only the default when the browser reports a hardware encoder for it.
// Add ?codec=vp8 (or vp9/av1/h264) to the page address to force a codec for testing.
const enc = { codec: 'vp8', hw: false };
const encReady = (async () => {
  const forced = new URLSearchParams(location.search).get('codec');
  const caps = (window.RTCRtpSender?.getCapabilities?.('video')?.codecs || []).map((c) => c.mimeType.toLowerCase());
  const probe = async (mime) => {
    try {
      const r = await navigator.mediaCapabilities.encodingInfo({
        type: 'webrtc', video: { contentType: mime, width: 1920, height: 1080, bitrate: 8_000_000, framerate: 60 },
      });
      return r.supported ? r.powerEfficient : null;
    } catch { return null; }
  };
  if (forced) { enc.codec = forced; enc.hw = Boolean(await probe(`video/${forced.toUpperCase()}`)); return; }
  const h264hw = caps.includes('video/h264') && Boolean(await probe('video/H264'));
  if (h264hw) { enc.codec = 'h264'; enc.hw = true; } else { enc.codec = 'vp8'; enc.hw = Boolean(await probe('video/VP8')); }
})();

let mode = store.get('sr.mode', 'motion');

// Encoding profiles. One extra 360p layer lets small tiles and slow viewers get a light copy.
function profile() {
  if (mode === 'detail') {
    return {
      capture: { width: 1920, height: 1080, frameRate: 30 },
      encoding: { maxBitrate: 5_000_000, maxFramerate: 30 },
      layers: [new LK.VideoPreset(640, 360, 400_000, 15)],
      degradation: 'maintain-resolution',
      fps: 30,
    };
  }
  return {
    capture: { width: 1920, height: 1080, frameRate: 60 },
    encoding: { maxBitrate: 8_000_000, maxFramerate: 60 },
    layers: [new LK.VideoPreset(640, 360, 600_000, 30)],
    // 'balanced': with H.264, 'maintain-framerate' made Chrome shrink to 270p at the slightest dip.
    degradation: 'balanced',
    fps: 60,
  };
}

// Advanced overrides (saved per browser), applied on top of the mode's profile.
const adv = Object.assign({ res: 'auto', fps: 'auto', layers: 'auto', codec: 'auto', degrade: 'auto', bitrate: 'auto' }, store.get('sr.adv', {}));
const RES = { 1440: [2560, 1440], 1080: [1920, 1080], 720: [1280, 720] };
function finalProfile(o = adv) {
  const p = profile();
  if (o.fps !== 'auto') { p.fps = Number(o.fps); p.capture.frameRate = p.fps; p.encoding.maxFramerate = p.fps; }
  if (RES[o.res]) [p.capture.width, p.capture.height] = RES[o.res];
  p.native = o.res === 'native';
  if (o.bitrate !== 'auto') p.encoding.maxBitrate = Number(o.bitrate) * 1_000_000;
  p.simulcast = o.layers !== 'off';
  if (!p.simulcast) p.layers = [];
  p.codec = o.codec !== 'auto' ? o.codec : enc.codec;
  if (o.degrade !== 'auto') p.degradation = o.degrade;
  return p;
}
const videoPublishOptions = (p) => ({
  source: LK.Track.Source.ScreenShare,
  simulcast: p.simulcast,
  screenShareEncoding: p.encoding,
  screenShareSimulcastLayers: p.layers,
  degradationPreference: p.degradation,
  videoCodec: p.codec,
});

// Ask the browser for the screen. Must run straight from a click.
function capture() {
  const p = finalProfile();
  return LK.createLocalScreenTracks({
    // Native: no size constraint (0x0 tells LiveKit "uncapped"; leaving it out means 1080p30).
    ...(p.native ? { video: { frameRate: { ideal: p.fps } } } : {}),
    audio: true,
    systemAudio: 'include',
    suppressLocalAudioPlayback: false,
    selfBrowserSurface: 'exclude',
    surfaceSwitching: 'include',
    resolution: p.native ? { width: 0, height: 0, frameRate: p.fps } : p.capture,
    contentHint: mode === 'motion' ? 'motion' : 'detail',
  });
}

const ADVICE = {
  capture: 'Your screen capture is slow. Share your <b>Entire screen</b> instead of a window, set the game to <b>borderless windowed</b>, and make sure "Use graphics acceleration when available" is on in your browser settings. For games, the Streamroom Live app is much smoother.',
  cpu: 'Your computer can\'t encode fast enough. Close heavy apps or tabs, or use the Streamroom Live app, which encodes on your graphics card.',
  bandwidth: 'Your internet upload is too slow for this quality, so frames are being dropped. Pause big downloads or uploads, or use a wired connection.',
  tcp: 'Your connection to the server is using the slower TCP fallback, which causes stutter. UDP port 7882 is being blocked by your network or the server firewall.',
};

/**
 * Wires up the Go Live dialog and live bar on the room page.
 * onLocalVideo(track | null) shows/hides your own "You're live" tile.
 * onLiveChange(bool) updates the top bar.
 */
export function setupBrowserStreaming({ room, roomId, onLocalVideo, onLiveChange }) {
  let tracks = [];
  let statsTimer = null;
  let lastDiag = null;

  // ---- settings UI ----
  const setMode = (m) => {
    mode = m; store.set('sr.mode', m);
    document.querySelectorAll('#modeSeg button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.mode === m)));
  };
  setMode(mode);
  document.querySelectorAll('#modeSeg button').forEach((b) => { b.onclick = () => setMode(b.dataset.mode); });
  document.querySelectorAll('[data-adv]').forEach((sel) => {
    sel.value = adv[sel.dataset.adv];
    sel.onchange = () => { adv[sel.dataset.adv] = sel.value; store.set('sr.adv', adv); };
  });
  $('advReset').onclick = () => {
    Object.keys(adv).forEach((k) => { adv[k] = 'auto'; });
    store.set('sr.adv', adv);
    document.querySelectorAll('[data-adv]').forEach((sel) => { sel.value = 'auto'; });
  };
  if (Object.values(adv).some((v) => v !== 'auto')) { $('adv').open = true; $('browserBox').open = true; }

  if (!navigator.mediaDevices?.getDisplayMedia) {
    $('browserGo').disabled = true;
    $('tuneBtn').disabled = true;
    $('browserNote').textContent = window.isSecureContext
      ? 'This browser can\'t share its screen. Use Chrome or Edge on a computer, or the app.'
      : 'Screen sharing needs a secure (https) address.';
  }

  async function publish(newTracks) {
    await encReady;
    const p = finalProfile();
    for (const t of newTracks) {
      if (t.kind === 'video') {
        await room.localParticipant.publishTrack(t, videoPublishOptions(p));
        onLocalVideo(t);
        t.on(LK.TrackEvent.Ended, () => stop()); // the browser's own "Stop sharing" bar
      } else {
        await room.localParticipant.publishTrack(t, {
          source: LK.Track.Source.ScreenShareAudio,
          audioPreset: LK.AudioPresets.musicHighQualityStereo,
          dtx: false, red: false, forceStereo: true,
        });
      }
    }
    $('audioNote').textContent = newTracks.some((t) => t.kind === 'audio')
      ? '🔊 Sharing sound'
      : '🔇 No sound. To add it, use "Change" and tick the share-audio box.';
  }

  async function start() {
    $('goErr').textContent = '';
    let next;
    try {
      next = await capture(); // screen picker first, while we still have the click
    } catch (e) {
      $('goErr').textContent = e.name === 'NotAllowedError' ? 'Screen sharing was cancelled.' : e.message;
      return;
    }
    try {
      tracks = next;
      await publish(tracks);
      $('goLive').close();
      $('liveBar').classList.remove('hidden');
      onLiveChange(true);
      startStats();
      window.addEventListener('beforeunload', warnOnLeave);
    } catch (e) {
      console.error(e);
      next.forEach((t) => t.stop());
      tracks = [];
      $('goErr').textContent = `Could not start: ${e.message}`;
    }
  }

  async function stop() {
    clearInterval(statsTimer);
    window.removeEventListener('beforeunload', warnOnLeave);
    const old = tracks;
    tracks = [];
    for (const t of old) {
      t.removeAllListeners(LK.TrackEvent.Ended);
      await room.localParticipant.unpublishTrack(t, true).catch(() => t.stop());
    }
    onLocalVideo(null);
    $('liveBar').classList.add('hidden');
    $('perfNote').classList.add('hidden');
    onLiveChange(false);
  }

  async function change() {
    let next;
    try { next = await capture(); } catch { return; } // cancelled: keep the current share
    const old = tracks;
    tracks = next;
    for (const t of old) { t.removeAllListeners(LK.TrackEvent.Ended); await room.localParticipant.unpublishTrack(t, true); }
    await publish(next);
  }

  function warnOnLeave(e) { e.preventDefault(); e.returnValue = ''; }

  // ---- live stats and advice ----
  function startStats() {
    clearInterval(statsTimer);
    const strikes = { capture: 0, cpu: 0, bandwidth: 0, tcp: 0 };
    $('encInfo').textContent = 'Measuring…';
    let failures = 0;
    statsTimer = setInterval(async () => {
      const v = tracks.find((t) => t.kind === 'video');
      if (!v) return;
      const d = await senderDiag(v).catch(() => null);
      const top = d?.layers.find((l) => l.active !== false && l.fps) || d?.layers[0];
      if (!top) {
        if (++failures >= 5) $('encInfo').textContent = 'This browser isn\'t reporting stream stats.';
        return;
      }
      lastDiag = d;
      const target = finalProfile().fps;
      const capFps = d.capture.fps || 0;
      $('liveStats').textContent = `${top.height || '?'}p · ${Math.round(top.fps || 0)} fps`;
      $('encInfo').textContent = `capturing ${Math.round(capFps)} fps · ${top.codec || enc.codec.toUpperCase()} on ${top.hardware ? 'graphics card' : 'processor'}`
        + (d.transport.protocol ? ` · ${d.transport.protocol.toUpperCase()}` : '');
      const captureSlow = capFps > 0 && capFps < target * 0.6;
      strikes.capture = captureSlow ? strikes.capture + 1 : 0;
      strikes.cpu = !captureSlow && top.limitedBy === 'cpu' ? strikes.cpu + 1 : 0;
      strikes.bandwidth = !captureSlow && top.limitedBy === 'bandwidth' ? strikes.bandwidth + 1 : 0;
      strikes.tcp = d.transport.protocol === 'tcp' ? strikes.tcp + 1 : 0;
      const problem = ['capture', 'cpu', 'bandwidth', 'tcp'].find((k) => strikes[k] >= 4);
      $('perfNote').innerHTML = problem ? `⚠️ ${ADVICE[problem]}` : '';
      $('perfNote').classList.toggle('hidden', !problem);
    }, 1000);
  }

  // ---- "find best settings": test-stream each setup into a private side room ----
  function candidates() {
    const fps = mode === 'motion' ? '60' : '30';
    const h264 = (window.RTCRtpSender?.getCapabilities?.('video')?.codecs || []).some((c) => c.mimeType.toLowerCase() === 'video/h264');
    return [
      { codec: 'h264', layers: 'auto', res: '1080' },
      { codec: 'h264', layers: 'off', res: '1080' },
      { codec: 'vp8', layers: 'auto', res: '1080' },
      { codec: 'vp8', layers: 'off', res: '1080' },
      { codec: 'vp8', layers: 'off', res: '720' },
      { codec: 'h264', layers: 'off', res: '720' },
    ].filter((c) => h264 || c.codec !== 'h264').map((c) => ({ ...c, fps, degrade: 'auto', bitrate: 'auto' }));
  }
  const describe = (c) => `${c.codec === 'h264' ? 'H.264' : c.codec.toUpperCase()} · ${c.layers === 'off' ? '1 layer' : '2 layers'} · ${c.res}p${c.fps}`;
  function renderTune(results, pick) {
    const rows = results.map((r) => `<tr class="${r === pick ? 'win' : ''}"><td>${describe(r.c)}</td><td>${Math.round(r.fps)} fps</td><td>${r.height || '?'}p</td><td>${r.hw ? 'graphics card' : 'processor'}</td></tr>`).join('');
    $('tuneTable').innerHTML = `<tr><th>Setup</th><th>Sent</th><th>Size</th><th>Encoder</th></tr>${rows}`;
  }

  async function tune() {
    $('goErr').textContent = '';
    let testTracks = [];
    let testRoom = null;
    try {
      $('tuneBtn').disabled = true; $('browserGo').disabled = true;
      testTracks = await LK.createLocalScreenTracks({
        audio: false, selfBrowserSurface: 'exclude',
        resolution: { width: 1920, height: 1080, frameRate: mode === 'motion' ? 60 : 30 },
        contentHint: mode === 'motion' ? 'motion' : 'detail',
      });
      const video = testTracks.find((t) => t.kind === 'video');
      await encReady;
      $('tuneOut').classList.remove('hidden');
      $('tuneTable').innerHTML = '';
      $('tuneStatus').textContent = 'Connecting…';
      const { token, url } = await api(`/api/rooms/${encodeURIComponent(roomId)}/join`, { test: true });
      testRoom = new LK.Room({ dynacast: false });
      await testRoom.connect(livekitUrl(url), token, { autoSubscribe: false });
      const results = [];
      const list = candidates();
      for (const [i, c] of list.entries()) {
        $('tuneStatus').textContent = `Testing ${i + 1} of ${list.length}: ${describe(c)}… keep the game moving`;
        const p = finalProfile(c);
        await video.mediaStreamTrack.applyConstraints({ width: { ideal: p.capture.width }, height: { ideal: p.capture.height }, frameRate: { ideal: p.fps } }).catch(() => {});
        await testRoom.localParticipant.publishTrack(video, videoPublishOptions(p));
        await sleep(3000);
        const samples = [];
        for (let k = 0; k < 5; k++) {
          await sleep(1000);
          const d = await senderDiag(video).catch(() => null);
          const top = d?.layers[0];
          if (top) samples.push({ fps: top.fps || 0, height: top.height || 0, hw: top.hardware, cap: d.capture.fps || 0 });
        }
        await testRoom.localParticipant.unpublishTrack(video, false);
        const avg = (k) => samples.reduce((a, x) => a + x[k], 0) / Math.max(1, samples.length);
        results.push({ c, fps: avg('fps'), cap: avg('cap'), height: Math.round(avg('height')), hw: samples.some((x) => x.hw) });
        renderTune(results, null);
        await sleep(500);
      }
      const bestFps = Math.max(...results.map((r) => r.fps));
      const pick = results.filter((r) => r.fps >= bestFps * 0.9)
        .sort((a, b) => (b.height - a.height) || ((a.c.layers === 'off') - (b.c.layers === 'off')))[0];
      renderTune(results, pick);
      if (pick && bestFps > 0) {
        Object.assign(adv, { codec: pick.c.codec, layers: pick.c.layers, res: pick.c.res, fps: 'auto', degrade: 'auto', bitrate: 'auto' });
        store.set('sr.adv', adv);
        document.querySelectorAll('[data-adv]').forEach((sel) => { sel.value = adv[sel.dataset.adv]; });
        $('tuneStatus').innerHTML = `Best on this computer: <b>${describe(pick.c)}</b> at about ${Math.round(pick.fps)} fps. Saved.`
          + (pick.cap < Number(pick.c.fps) * 0.6 ? ' Your screen capture itself is slow: the app will do much better.' : '');
      } else {
        $('tuneStatus').textContent = 'Couldn\'t measure anything. Try again with the game moving.';
      }
    } catch (e) {
      console.error(e);
      $('goErr').textContent = e.name === 'NotAllowedError' ? 'Screen sharing was cancelled.' : `Test failed: ${e.message}`;
    } finally {
      testTracks.forEach((t) => t.stop());
      if (testRoom) testRoom.disconnect();
      $('tuneBtn').disabled = false; $('browserGo').disabled = false;
    }
  }

  $('browserGo').onclick = start;
  $('stopBtn').onclick = () => stop();
  $('switchBtn').onclick = change;
  $('tuneBtn').onclick = tune;
  $('diagBtn').onclick = async () => {
    const v = tracks.find((t) => t.kind === 'video');
    const d = (await senderDiag(v).catch(() => null)) || lastDiag;
    await copyDiag({ env: envDiag(), mode, advanced: adv, encoderGuess: enc, stream: d });
    toast('Diagnostics copied.');
  };

  return { isLive: () => tracks.length > 0, stop };
}
