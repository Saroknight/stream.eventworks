// Detailed WebRTC stats for troubleshooting. Used by the "Copy diagnostics" buttons.

// Connection info shared by sender and receiver reports: UDP vs TCP, direct vs relayed, round trip.
function transportOf(report) {
  let pair = null;
  report.forEach((st) => {
    if (st.type === 'candidate-pair' && (st.nominated || st.selected) && st.state === 'succeeded') pair = st;
  });
  if (!pair) return {};
  const local = report.get(pair.localCandidateId) || {};
  const remote = report.get(pair.remoteCandidateId) || {};
  return {
    protocol: local.protocol,               // "udp" is best; "tcp" means UDP 7882 is blocked somewhere
    localType: local.candidateType,         // host / srflx / relay
    remotePort: remote.port,
    rttMs: pair.currentRoundTripTime != null ? Math.round(pair.currentRoundTripTime * 1000) : undefined,
    availableOutKbps: pair.availableOutgoingBitrate ? Math.round(pair.availableOutgoingBitrate / 1000) : undefined,
    availableInKbps: pair.availableIncomingBitrate ? Math.round(pair.availableIncomingBitrate / 1000) : undefined,
  };
}

const codecOf = (report, st) => (report.get(st.codecId)?.mimeType || '').replace('video/', '');

export async function senderDiag(track) {
  const report = await track?.sender?.getStats();
  if (!report) return null;
  const out = { transport: transportOf(report), capture: {}, layers: [] };
  report.forEach((st) => {
    if (st.type === 'media-source' && st.kind === 'video') {
      out.capture = { width: st.width, height: st.height, fps: st.framesPerSecond };
    }
    if (st.type === 'outbound-rtp' && st.kind === 'video') {
      out.layers.push({
        rid: st.rid, active: st.active, codec: codecOf(report, st),
        encoder: st.encoderImplementation, hardware: st.powerEfficientEncoder,
        width: st.frameWidth, height: st.frameHeight, fps: st.framesPerSecond,
        limitedBy: st.qualityLimitationReason, limitSeconds: st.qualityLimitationDurations,
        targetKbps: st.targetBitrate ? Math.round(st.targetBitrate / 1000) : undefined,
        retransmits: st.retransmittedPacketsSent, nacks: st.nackCount, plis: st.pliCount,
      });
    }
    if (st.type === 'remote-inbound-rtp' && st.kind === 'video') {
      out.lossToServer = Math.round((st.fractionLost || 0) * 1000) / 10 + '%';
    }
  });
  out.layers.sort((a, b) => (b.height || 0) - (a.height || 0));
  return out;
}

export async function receiverDiag(track) {
  const report = await track?.receiver?.getStats();
  if (!report) return null;
  const out = { transport: transportOf(report) };
  report.forEach((st) => {
    if (st.type === 'inbound-rtp' && st.kind === 'video') {
      Object.assign(out, {
        codec: codecOf(report, st), decoder: st.decoderImplementation, hardware: st.powerEfficientDecoder,
        width: st.frameWidth, height: st.frameHeight, fps: st.framesPerSecond,
        framesReceived: st.framesReceived, framesDecoded: st.framesDecoded, framesDropped: st.framesDropped,
        freezes: st.freezeCount, freezeSeconds: st.totalFreezesDuration,
        packetsLost: st.packetsLost, packetsReceived: st.packetsReceived,
        jitterMs: st.jitter != null ? Math.round(st.jitter * 1000) : undefined,
        avgJitterBufferMs: st.jitterBufferEmittedCount
          ? Math.round((st.jitterBufferDelay / st.jitterBufferEmittedCount) * 1000) : undefined,
        nacks: st.nackCount, plis: st.pliCount,
      });
    }
  });
  return out;
}

export function envDiag() {
  return {
    page: location.pathname,
    browser: navigator.userAgentData?.brands?.map((b) => `${b.brand} ${b.version}`).join(', ') || navigator.userAgent,
    platform: navigator.userAgentData?.platform || navigator.platform,
    cores: navigator.hardwareConcurrency,
    screen: `${screen.width}x${screen.height} @${window.devicePixelRatio}x`,
    time: new Date().toISOString(),
  };
}

export async function copyDiag(obj) {
  const text = JSON.stringify(obj, null, 1);
  try { await navigator.clipboard.writeText(text); return true; } catch { prompt('Copy these diagnostics:', text); return false; }
}
