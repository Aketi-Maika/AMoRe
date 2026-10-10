// AMoRe v0.2 - 送信画面
// getUserMedia() でカメラ・マイクを取得し、RTCPeerConnection（WHIP）で Server へ送信する。

import { WhipClient, WhipError } from "./whip.js";
import { QualityController } from "./quality.js";

/** ICEが disconnected のまま回復しない場合に再接続するまでの待ち時間 */
const DISCONNECT_GRACE_MS = 10000;
/** パケット損失率がこの値（%）を超えたら警告表示する */
const LOSS_WARN_PERCENT = 2;

const $ = (id) => document.getElementById(id);
const ui = {
  connState: $("connState"),
  preview: $("preview"),
  placeholder: $("previewPlaceholder"),
  qualityBanner: $("qualityBanner"),
  message: $("message"),
  videoOnlyBtn: $("videoOnlyBtn"),
  cameraId: $("cameraId"),
  videoDevice: $("videoDevice"),
  audioDevice: $("audioDevice"),
  audioEnabled: $("audioEnabled"),
  audioProcessing: $("audioProcessing"),
  qualityMode: $("qualityMode"),
  previewBtn: $("previewBtn"),
  startBtn: $("startBtn"),
  stopBtn: $("stopBtn"),
  version: $("version"),
  st: {
    codec: $("stCodec"),
    resolution: $("stResolution"),
    fps: $("stFps"),
    bitrate: $("stBitrate"),
    loss: $("stLoss"),
    available: $("stAvailable"),
    rtt: $("stRtt"),
    route: $("stRoute"),
    quality: $("stQuality"),
    audio: $("stAudio"),
  },
};

const state = {
  config: null,
  clientId: null,
  stream: null,
  pc: null,
  whip: null,
  videoSender: null,
  quality: null,
  running: false,
  attempt: 0,
  reconnectTimer: null,
  disconnectTimer: null,
  statsTimer: null,
  prevStats: null,
  lowFpsCount: 0,
};

/** 自動では回復しない原因（権限不足・カメラ未接続など）。再接続しない。 */
class FatalError extends Error {}
/** 音声だけ取得できなかった場合。映像のみで送信するかを利用者が選ぶ。 */
class AudioError extends Error {}

// ---------- 保存（このブラウザだけの利便機能。使えなくても動作する） ----------

const store = {
  get(key) {
    try { return localStorage.getItem(`amore.${key}`); } catch { return null; }
  },
  set(key, value) {
    try { localStorage.setItem(`amore.${key}`, value); } catch { /* 無視 */ }
  },
};

function getClientId() {
  // 同じタブの再読み込みでは同じIDを使い、直前のセッションを置き換えられるようにする。
  try {
    let id = sessionStorage.getItem("amore.clientId");
    if (!id) {
      id = crypto.randomUUID();
      sessionStorage.setItem("amore.clientId", id);
    }
    return id;
  } catch {
    return crypto.randomUUID();
  }
}

// ---------- 画面表示 ----------

const STATE_LABEL = {
  idle: "待機中",
  preview: "プレビュー中",
  connecting: "接続中",
  live: "送信中",
  reconnecting: "再接続待ち",
  error: "エラー",
};

function setConnState(kind, label = STATE_LABEL[kind]) {
  ui.connState.className = `badge ${kind === "preview" ? "idle" : kind}`;
  ui.connState.textContent = label;
}

function showMessage(text, { info = false } = {}) {
  ui.message.textContent = text;
  ui.message.classList.toggle("info", info);
  ui.message.hidden = !text;
}

function clearMessage() {
  showMessage("");
  ui.videoOnlyBtn.hidden = true;
}

function setControls() {
  const running = state.running;
  ui.startBtn.disabled = running;
  ui.stopBtn.disabled = !running && !state.stream;
  ui.previewBtn.disabled = running || !!state.stream;
  // 送信中は送信先・音声の有無を変更しない（停止してから変更する）。
  ui.cameraId.disabled = running;
  ui.audioEnabled.disabled = running;
  ui.audioProcessing.disabled = running;
  ui.audioDevice.disabled = running || !ui.audioEnabled.checked;
}

function resetStats() {
  for (const el of Object.values(ui.st)) {
    el.textContent = "-";
    el.classList.remove("warn");
  }
  ui.qualityBanner.hidden = true;
}

function fmtKbps(kbps) {
  if (kbps == null || !Number.isFinite(kbps)) return "-";
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(2)} Mbps` : `${Math.round(kbps)} kbps`;
}

// ---------- 設定・デバイス ----------

async function loadConfig() {
  const res = await fetch("/api/v1/config", { cache: "no-store" });
  if (!res.ok) throw new Error(`設定を取得できません（HTTP ${res.status}）`);
  return res.json();
}

function fillSelect(select, items, saved) {
  const current = select.value || saved;
  select.replaceChildren(
    ...items.map(({ value, label }) => {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = label;
      return opt;
    }),
  );
  if (items.some((i) => i.value === current)) select.value = current;
}

async function refreshDevices() {
  const devices = await navigator.mediaDevices.enumerateDevices();
  const cams = devices.filter((d) => d.kind === "videoinput");
  const mics = devices.filter((d) => d.kind === "audioinput");
  // 利用許可の前はラベルが空になるため番号で表示する。
  fillSelect(
    ui.videoDevice,
    cams.map((d, i) => ({ value: d.deviceId, label: d.label || `カメラ ${i + 1}` })),
    store.get("videoDevice"),
  );
  fillSelect(
    ui.audioDevice,
    mics.map((d, i) => ({ value: d.deviceId, label: d.label || `マイク ${i + 1}` })),
    store.get("audioDevice"),
  );
  if (cams.length === 0) {
    showMessage("カメラが見つかりません。USBカメラの接続とOSのカメラ利用許可を確認してください。");
  }
}

function videoConstraints() {
  const v = state.config.video.standard;
  const deviceId = ui.videoDevice.value;
  return {
    deviceId: deviceId ? { exact: deviceId } : undefined,
    width: { ideal: v.width },
    height: { ideal: v.height },
    frameRate: { ideal: v.fps, max: v.fps },
  };
}

function audioConstraints() {
  const deviceId = ui.audioDevice.value;
  const processing = ui.audioProcessing.checked;
  return {
    deviceId: deviceId ? { exact: deviceId } : undefined,
    channelCount: { ideal: state.config.audio.max_channels },
    sampleRate: { ideal: state.config.audio.sample_rate },
    echoCancellation: processing,
    noiseSuppression: processing,
    autoGainControl: processing,
  };
}

function describeMediaError(err, device) {
  switch (err?.name) {
    case "NotAllowedError":
    case "SecurityError":
      return `${device}の利用が許可されていません。アドレスバーのサイト情報（鍵アイコン）から${device}を「許可」にし、OSの${device}利用許可も確認してください。`;
    case "NotFoundError":
      return `${device}が見つかりません。接続を確認してください。`;
    case "NotReadableError":
    case "AbortError":
      return `${device}を開けません。他のアプリ（会議アプリ等）が使用中でないか確認してください。`;
    case "OverconstrainedError":
      return `選択した${device}が要求条件を満たしません。別の${device}を選択してください。`;
    case "TypeError":
      return "この画面は HTTPS で開く必要があります。";
    default:
      return `${device}を取得できません（${err?.name ?? ""} ${err?.message ?? err}）`;
  }
}

function stopTracks(stream) {
  stream?.getTracks().forEach((t) => t.stop());
}

/** カメラ・マイクを取得してプレビューする。 */
async function acquireMedia() {
  const wantAudio = ui.audioEnabled.checked;
  stopTracks(state.stream);
  state.stream = null;

  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      video: videoConstraints(),
      audio: wantAudio ? audioConstraints() : false,
    });
  } catch (err) {
    if (!wantAudio) throw new FatalError(describeMediaError(err, "カメラ"));
    // 映像だけで取得できるかを確かめ、原因がカメラかマイクかを切り分ける。
    let videoOnly;
    try {
      videoOnly = await navigator.mediaDevices.getUserMedia({ video: videoConstraints() });
    } catch (videoErr) {
      throw new FatalError(describeMediaError(videoErr, "カメラ"));
    }
    stopTracks(videoOnly);
    throw new AudioError(describeMediaError(err, "マイク"));
  }

  state.stream = stream;
  ui.preview.srcObject = stream;
  ui.placeholder.hidden = true;

  const [videoTrack] = stream.getVideoTracks();
  videoTrack.contentHint = "motion";
  videoTrack.addEventListener("ended", () => {
    fatal("カメラが切断されました。接続を確認してから送信を再開してください。");
  });
  const [audioTrack] = stream.getAudioTracks();
  audioTrack?.addEventListener("ended", () => {
    fatal("マイクが切断されました。接続を確認してから送信を再開してください。");
  });

  // 利用許可の後はデバイス名を取得できる。
  await refreshDevices();
  store.set("videoDevice", ui.videoDevice.value);
  if (wantAudio) store.set("audioDevice", ui.audioDevice.value);

  const s = videoTrack.getSettings();
  ui.st.resolution.textContent = `${s.width}×${s.height}（カメラ出力）`;
  ui.st.audio.textContent = audioTrack
    ? `${audioTrack.label || "マイク"}（${audioTrack.getSettings().channelCount ?? "?"}ch）`
    : "送信しない";
  if (s.height < state.config.video.standard.height) {
    showMessage(
      `カメラの出力が ${s.width}×${s.height} です。${state.config.video.standard.height}p に対応したカメラ・設定を確認してください。`,
      { info: true },
    );
  }
}

// ---------- WebRTC ----------

function applyCodecPreferences(transceiver) {
  if (typeof RTCRtpSender.getCapabilities !== "function" || !transceiver.setCodecPreferences) return;
  const caps = RTCRtpSender.getCapabilities("video");
  if (!caps) return;
  const byMime = (mime) => caps.codecs.filter((c) => c.mimeType.toLowerCase() === mime);
  const preferred = [];
  for (const codec of state.config.video.codec_preference) {
    const list = byMime(`video/${codec.toLowerCase()}`);
    if (codec === "H264") {
      // packetization-mode=1 を優先する（Server・OBSでの扱いが一般的な形式）。
      list.sort((a, b) => {
        const pa = /packetization-mode=1/.test(a.sdpFmtpLine ?? "") ? 0 : 1;
        const pb = /packetization-mode=1/.test(b.sdpFmtpLine ?? "") ? 0 : 1;
        return pa - pb;
      });
    }
    preferred.push(...list);
  }
  // 再送・冗長化用のコーデックは残す。
  const extras = caps.codecs.filter((c) => /video\/(rtx|red|ulpfec|flexfec)/i.test(c.mimeType));
  if (preferred.length > 0) transceiver.setCodecPreferences([...preferred, ...extras]);
}

/** 品質レベルを送信設定（解像度の縮小率・上限ビットレート）へ反映する。 */
async function applyQuality(level) {
  const sender = state.videoSender;
  if (!sender) return;
  const video = state.config.video;
  const profile = level === "fallback" ? video.fallback : video.standard;
  const kbps = level === "standard_max" ? profile.max_bitrate_kbps : profile.target_bitrate_kbps;
  const sourceHeight = state.stream?.getVideoTracks()[0]?.getSettings().height ?? profile.height;

  const params = sender.getParameters();
  if (!params.encodings || params.encodings.length === 0) params.encodings = [{}];
  const enc = params.encodings[0];
  enc.scaleResolutionDownBy = Math.max(1, sourceHeight / profile.height);
  enc.maxBitrate = kbps * 1000;
  enc.maxFramerate = profile.fps;
  await sender.setParameters(params);
}

function createPeerConnection() {
  const iceServers = (state.config.ice_servers ?? []).map((s) => ({
    urls: s.urls,
    username: s.username,
    credential: s.credential,
  }));
  const pc = new RTCPeerConnection({ iceServers, bundlePolicy: "max-bundle" });
  const stream = state.stream;
  const [videoTrack] = stream.getVideoTracks();
  const [audioTrack] = stream.getAudioTracks();
  const v = state.config.video.standard;

  const videoTx = pc.addTransceiver(videoTrack, {
    direction: "sendonly",
    streams: [stream],
    // WebRTC には目標ビットレートの指定がないため、上限を目標値に合わせる。
    sendEncodings: [{ maxBitrate: v.target_bitrate_kbps * 1000, maxFramerate: v.fps }],
  });
  applyCodecPreferences(videoTx);
  state.videoSender = videoTx.sender;

  if (audioTrack) {
    pc.addTransceiver(audioTrack, { direction: "sendonly", streams: [stream] });
  }

  pc.addEventListener("connectionstatechange", () => onConnectionState(pc));
  return pc;
}

function onConnectionState(pc) {
  if (pc !== state.pc) return;
  clearTimeout(state.disconnectTimer);
  switch (pc.connectionState) {
    case "connected":
      state.attempt = 0;
      setConnState("live");
      clearMessage();
      break;
    case "disconnected":
      // パケット損失による一時的な切断はICEが自然に回復することが多い。すぐに作り直すと
      // Server側の変換処理も再起動してOBSの映像が長く途切れるため、しばらく待ってから再接続する。
      setConnState("reconnecting", "通信が不安定");
      state.disconnectTimer = setTimeout(() => {
        if (pc === state.pc && pc.connectionState === "disconnected") {
          handleFailure(new WhipError("通信が途切れたまま回復しません"));
        }
      }, DISCONNECT_GRACE_MS);
      break;
    case "failed":
      handleFailure(new WhipError("接続が切断されました"));
      break;
    default:
      break;
  }
}

async function connect() {
  state.reconnectTimer = null;
  if (!state.running) return;
  setConnState("connecting");
  const cameraId = ui.cameraId.value;
  try {
    if (!state.stream) await acquireMedia();
    state.pc = createPeerConnection();
    state.whip = new WhipClient(`/api/v1/cameras/${encodeURIComponent(cameraId)}/whip`, state.clientId);
    await state.whip.publish(state.pc, { iceTimeoutMs: 2000 });
    state.quality = new QualityController(state.config.quality_control, applyQuality);
    await state.quality.setMode(ui.qualityMode.value);
    startStats();
  } catch (err) {
    handleFailure(err);
  }
}

function handleFailure(err) {
  // 停止済み、または再接続を予約済み（接続失敗と状態変化の通知が重なった場合）は何もしない。
  if (!state.running || state.reconnectTimer) return;
  if (err instanceof AudioError) {
    teardownPeer();
    state.running = false;
    setConnState("error");
    showMessage(`${err.message}\n映像のみで送信する場合は下のボタンを押してください。`);
    ui.videoOnlyBtn.hidden = false;
    setControls();
    return;
  }
  if (err instanceof FatalError || (err instanceof WhipError && !err.retryable)) {
    fatal(err.message);
    return;
  }
  teardownPeer();
  const backoff = state.config.reconnect.backoff_sec;
  const delay = backoff[Math.min(state.attempt, backoff.length - 1)];
  state.attempt += 1;
  setConnState("reconnecting", `再接続待ち（${delay}秒）`);
  showMessage(`${err.message}\n${delay}秒後に再接続します（${state.attempt}回目）。`, { info: true });
  state.reconnectTimer = setTimeout(connect, delay * 1000);
}

function teardownPeer() {
  stopStats();
  clearTimeout(state.disconnectTimer);
  state.whip?.stop();
  state.whip = null;
  if (state.pc) {
    const pc = state.pc;
    state.pc = null;
    pc.close();
  }
  state.videoSender = null;
  state.quality = null;
  state.prevStats = null;
  state.lowFpsCount = 0;
}

/** 自動では回復できない原因で停止し、理由を表示する。 */
function fatal(message) {
  state.running = false;
  clearTimeout(state.reconnectTimer);
  teardownPeer();
  stopTracks(state.stream);
  state.stream = null;
  ui.preview.srcObject = null;
  ui.placeholder.hidden = false;
  setConnState("error");
  showMessage(message);
  setControls();
}

// ---------- 統計 ----------

function startStats() {
  stopStats();
  state.statsTimer = setInterval(updateStats, state.config.quality_control.stats_interval_ms);
}

function stopStats() {
  clearInterval(state.statsTimer);
  state.statsTimer = null;
}

async function updateStats() {
  const pc = state.pc;
  if (!pc) return;
  const report = await pc.getStats().catch(() => null);
  if (!report || pc !== state.pc) return;

  let video;
  let audio;
  let remoteVideo;
  let transport;
  const pairs = new Map();
  const locals = new Map();
  const codecs = new Map();
  report.forEach((s) => {
    if (s.type === "outbound-rtp" && s.kind === "video") video = s;
    else if (s.type === "outbound-rtp" && s.kind === "audio") audio = s;
    else if (s.type === "remote-inbound-rtp" && s.kind === "video") remoteVideo = s;
    else if (s.type === "transport") transport = s;
    else if (s.type === "candidate-pair") pairs.set(s.id, s);
    else if (s.type === "local-candidate") locals.set(s.id, s);
    else if (s.type === "codec") codecs.set(s.id, s);
  });
  const pair =
    (transport?.selectedCandidatePairId && pairs.get(transport.selectedCandidatePairId)) ||
    [...pairs.values()].find((p) => p.nominated && p.state === "succeeded");

  const prev = state.prevStats;
  const rate = (cur, old) =>
    cur && old && cur.timestamp > old.timestamp
      ? ((cur.bytesSent - old.bytesSent) * 8) / (cur.timestamp - old.timestamp) // kbps（ms単位のため）
      : null;
  const videoKbps = rate(video, prev?.video);
  const audioKbps = rate(audio, prev?.audio);
  state.prevStats = { video, audio };

  const cfgVideo = state.config.video;
  const st = ui.st;
  if (video) {
    const codec = codecs.get(video.codecId)?.mimeType?.replace(/^video\//i, "") ?? "-";
    st.codec.textContent = codec + (/vp8/i.test(codec) ? "（ServerでH.264へ変換）" : "");
    st.resolution.textContent = video.frameWidth ? `${video.frameWidth}×${video.frameHeight}` : "-";
    const fps = video.framesPerSecond;
    st.fps.textContent = fps != null ? `${Math.round(fps)} fps` : "-";
    // 30fps を維持できない状態が続いたら表示する。
    const targetFps = state.quality?.level === "fallback" ? cfgVideo.fallback.fps : cfgVideo.standard.fps;
    state.lowFpsCount = fps != null && fps < targetFps * 0.9 ? state.lowFpsCount + 1 : 0;
    const lowFps = state.lowFpsCount >= 5;
    st.fps.classList.toggle("warn", lowFps);
    if (lowFps) {
      const reason = { cpu: "CPU負荷", bandwidth: "帯域不足", other: "その他" }[video.qualityLimitationReason];
      st.fps.textContent += `（${targetFps}fps 未達${reason ? `: ${reason}` : ""}）`;
    }
  }
  st.bitrate.textContent = fmtKbps(videoKbps);
  renderLoss(remoteVideo, video);

  const availableKbps = pair?.availableOutgoingBitrate != null ? pair.availableOutgoingBitrate / 1000 : null;
  st.available.textContent = availableKbps != null ? fmtKbps(availableKbps) : "取得できません";
  st.rtt.textContent = pair?.currentRoundTripTime != null ? `${Math.round(pair.currentRoundTripTime * 1000)} ms` : "-";
  const local = pair ? locals.get(pair.localCandidateId) : null;
  st.route.textContent = local ? (local.candidateType === "relay" ? "TURN経由" : "直接接続") : "-";
  if (audio) st.audio.textContent = `送信中 ${fmtKbps(audioKbps)}`;

  // 品質調整（映像に使える帯域 = 推定帯域 − 音声の送信量）
  const quality = state.quality;
  if (!quality) return;
  const videoAvailable = availableKbps != null ? availableKbps - (audioKbps ?? 0) : null;
  await quality.update(videoAvailable);
  renderQuality(quality, videoAvailable);
}

/**
 * 映像のパケット損失を表示する。Server（受信側）からの受信報告（RTCP）に基づく。
 * 損失が続く場合は経路（Wi-Fi・ルーター折り返し）やServerの受信バッファを確認する。
 */
function renderLoss(remoteVideo, video) {
  const el = ui.st.loss;
  if (!remoteVideo || remoteVideo.fractionLost == null) {
    el.textContent = "-";
    el.classList.remove("warn");
    return;
  }
  const percent = remoteVideo.fractionLost * 100;
  const parts = [`累計 ${remoteVideo.packetsLost ?? 0}`];
  if (video?.nackCount != null) parts.push(`再送要求 ${video.nackCount}`);
  if (video?.retransmittedPacketsSent != null) parts.push(`再送 ${video.retransmittedPacketsSent}`);
  el.textContent = `${percent.toFixed(1)}%（${parts.join("／")}）`;
  el.classList.toggle("warn", percent > LOSS_WARN_PERCENT);
}

function renderQuality(quality, videoAvailable) {
  const cfg = state.config;
  const fallback = quality.level === "fallback";
  const profile = fallback ? cfg.video.fallback : cfg.video.standard;
  let text = `${profile.height}p${profile.fps}`;
  if (quality.mode !== "auto") text += "（手動固定）";
  else if (!cfg.quality_control.enabled) text += "（自動切替無効）";
  else if (quality.unsupported) text += "（推定帯域を取得できないため自動切替不可。手動で変更してください）";
  else if (quality.holdRemainingSec() > 0) text += `（復帰保留 残り${quality.holdRemainingSec()}秒）`;
  ui.st.quality.textContent = text;
  ui.st.quality.classList.toggle("warn", fallback || quality.unsupported);

  const notes = [];
  if (fallback && quality.mode === "auto") {
    notes.push(`帯域不足のため ${cfg.video.fallback.height}p に品質を下げて送信しています。`);
  }
  if (fallback && videoAvailable != null && videoAvailable < cfg.video.fallback.target_bitrate_kbps) {
    notes.push(`${cfg.video.fallback.height}p でも帯域が不足しています（推定 ${fmtKbps(videoAvailable)}）。`);
  }
  if (quality.lastError) notes.push(quality.lastError);
  ui.qualityBanner.textContent = notes.join("\n");
  ui.qualityBanner.hidden = notes.length === 0;
}

// ---------- 操作 ----------

async function startPreview() {
  clearMessage();
  try {
    await acquireMedia();
    setConnState("preview");
  } catch (err) {
    if (err instanceof AudioError) {
      setConnState("error");
      showMessage(`${err.message}\n映像のみで送信する場合は下のボタンを押してください。`);
      ui.videoOnlyBtn.hidden = false;
    } else {
      fatal(err.message);
    }
  }
  setControls();
}

async function startSending() {
  if (!ui.cameraId.value) {
    showMessage("カメラIDを選択してください。");
    return;
  }
  clearMessage();
  resetStats();
  store.set("cameraId", ui.cameraId.value);
  state.running = true;
  state.attempt = 0;
  setControls();
  await connect();
  setControls();
}

function stopAll() {
  state.running = false;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = null;
  teardownPeer();
  stopTracks(state.stream);
  state.stream = null;
  ui.preview.srcObject = null;
  ui.placeholder.hidden = false;
  setConnState("idle");
  clearMessage();
  resetStats();
  setControls();
}

async function onDeviceChange() {
  store.set("videoDevice", ui.videoDevice.value);
  store.set("audioDevice", ui.audioDevice.value);
  if (!state.stream) return;
  // 送信中・プレビュー中はデバイスを取り直し、送信中のトラックを差し替える。
  try {
    await acquireMedia();
    if (state.pc) {
      for (const tx of state.pc.getTransceivers()) {
        const track = state.stream.getTracks().find((t) => t.kind === tx.receiver.track.kind);
        if (track) await tx.sender.replaceTrack(track);
      }
      if (state.quality) await applyQuality(state.quality.level);
    }
  } catch (err) {
    if (err instanceof AudioError) fatal(`${err.message}\n音声を無効にするか、別のマイクを選択してください。`);
    else fatal(err.message);
  }
}

async function init() {
  state.clientId = getClientId();
  if (!window.isSecureContext || !navigator.mediaDevices?.getUserMedia) {
    setConnState("error");
    showMessage("この画面は HTTPS で開いてください（カメラ・マイクは安全な接続でのみ利用できます）。");
    ui.startBtn.disabled = true;
    ui.previewBtn.disabled = true;
    return;
  }

  try {
    state.config = await loadConfig();
  } catch (err) {
    setConnState("error");
    showMessage(`${err.message}\nServer の稼働状態を確認してください。`);
    ui.startBtn.disabled = true;
    ui.previewBtn.disabled = true;
    return;
  }
  ui.version.textContent = `v${state.config.version}`;

  if (state.config.cameras.length === 0) {
    showMessage("カメラが登録されていません。管理Web（/admin/）でカメラを追加してから再読み込みしてください。");
  }
  // 管理Webの「送信画面」リンク（?camera=<ID>）で開いた場合はそのカメラIDを選ぶ。
  const requested = new URLSearchParams(window.location.search).get("camera");
  fillSelect(
    ui.cameraId,
    state.config.cameras.map((c) => ({ value: c.id, label: `${c.name}（${c.id}）` })),
    requested ?? store.get("cameraId"),
  );
  const savedAudio = store.get("audioEnabled");
  ui.audioEnabled.checked = savedAudio == null ? state.config.audio.enabled : savedAudio === "1";
  const savedProcessing = store.get("audioProcessing");
  if (savedProcessing != null) ui.audioProcessing.checked = savedProcessing === "1";

  await refreshDevices().catch(() => {});
  navigator.mediaDevices.addEventListener("devicechange", () => refreshDevices().catch(() => {}));

  ui.previewBtn.addEventListener("click", startPreview);
  ui.startBtn.addEventListener("click", startSending);
  ui.stopBtn.addEventListener("click", stopAll);
  ui.videoOnlyBtn.addEventListener("click", () => {
    ui.audioEnabled.checked = false;
    store.set("audioEnabled", "0");
    startSending();
  });
  ui.videoDevice.addEventListener("change", onDeviceChange);
  ui.audioDevice.addEventListener("change", onDeviceChange);
  ui.audioEnabled.addEventListener("change", async () => {
    store.set("audioEnabled", ui.audioEnabled.checked ? "1" : "0");
    setControls();
    if (state.stream) await startPreview();
  });
  ui.audioProcessing.addEventListener("change", async () => {
    store.set("audioProcessing", ui.audioProcessing.checked ? "1" : "0");
    if (state.stream) await startPreview();
  });
  ui.qualityMode.addEventListener("change", async () => {
    if (state.quality) {
      await state.quality.setMode(ui.qualityMode.value);
      if (state.quality.lastError) showMessage(state.quality.lastError);
    }
  });
  window.addEventListener("pagehide", () => state.whip?.stop());

  setConnState("idle");
  setControls();
}

init();
