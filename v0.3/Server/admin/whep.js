// AMoRe v0.3 - 映像確認（WHEP）プレーヤー
// Server（AMoRe）経由でカメラのコンテナから WebRTC で映像・音声を受信し、<video> に表示する。
// 管理Web（admin.js）とモニター画面（monitor.js）で共通に使う。

function waitIceGathering(pc, timeoutMs) {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      pc.removeEventListener("icegatheringstatechange", onChange);
      resolve();
    };
    const onChange = () => { if (pc.iceGatheringState === "complete") done(); };
    const timer = setTimeout(done, timeoutMs);
    pc.addEventListener("icegatheringstatechange", onChange);
  });
}

/** 送信されていないため表示できない（再試行で回復しうる） */
export class NotPublishingError extends Error {}

export class WhepPlayer {
  /**
   * @param {string} cameraId
   * @param {HTMLVideoElement} video
   * @param {{ iceServers?: RTCIceServer[], onState?: (state: string, message?: string) => void }} [opts]
   *   state: "connecting" | "playing" | "failed" | "stopped"
   */
  constructor(cameraId, video, { iceServers = [], onState = () => {} } = {}) {
    this.cameraId = cameraId;
    this.video = video;
    this.iceServers = iceServers;
    this.onState = onState;
    this.pc = null;
    this.resource = null;
  }

  get active() {
    return this.pc !== null;
  }

  async start() {
    this.stop({ silent: true });
    this.onState("connecting", "接続中…");
    const pc = new RTCPeerConnection({ iceServers: this.iceServers, bundlePolicy: "max-bundle" });
    this.pc = pc;
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
    const stream = new MediaStream();
    pc.addEventListener("track", (ev) => {
      stream.addTrack(ev.track);
      this.video.srcObject = stream;
      this.video.play().catch(() => {});
    });
    pc.addEventListener("connectionstatechange", () => {
      if (pc !== this.pc) return;
      if (pc.connectionState === "connected") this.onState("playing", "表示中");
      if (pc.connectionState === "failed") {
        this.stop({ silent: true });
        this.onState("failed", "接続が切断されました（WebRTCのUDPポートに到達できるか確認してください）");
      }
    });

    try {
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      await waitIceGathering(pc, 2000);
      const res = await fetch(`/api/v1/admin/cameras/${encodeURIComponent(this.cameraId)}/whep`, {
        method: "POST",
        headers: { "Content-Type": "application/sdp" },
        body: pc.localDescription.sdp,
      });
      if (res.status !== 201) {
        const text = await res.text().catch(() => "");
        if (res.status === 404 || /no stream|not available|no one is publishing/i.test(text)) {
          throw new NotPublishingError("このカメラは送信されていないため表示できません");
        }
        throw new Error(`映像確認を開始できません（HTTP ${res.status} ${text.slice(0, 200)}）`);
      }
      const location = res.headers.get("Location");
      this.resource = location ? new URL(location, window.location.href).href : null;
      await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
    } catch (e) {
      this.stop({ silent: true });
      this.onState("failed", e.message);
      throw e;
    }
  }

  stop({ silent = false } = {}) {
    if (this.resource) {
      fetch(this.resource, { method: "DELETE", keepalive: true }).catch(() => {});
      this.resource = null;
    }
    if (this.pc) {
      const pc = this.pc;
      this.pc = null;
      pc.close();
    }
    this.video.srcObject = null;
    if (!silent) this.onState("stopped", "");
  }
}
