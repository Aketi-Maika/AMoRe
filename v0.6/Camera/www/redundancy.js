// AMoRe v0.6 - 予備の送信（冗長構成の dual）
// 共通IP（この画面を開いたアドレス）への送信に加え、共通IPを持っていない別の Server の実IPへも同時に送る。
// どちらかの Server が止まっても、もう一方は送り続けるため、全 Server に映像が残り途切れない。
// 送り先は Server の /api/v1/config（cluster.backup）で決まり、定期的に取り直して変われば張り直す。

import { WhipClient, WhipError } from "./whip.js";

/** 送り先を確認し直す間隔 */
const REFRESH_MS = 10000;

export class BackupSender {
  /**
   * @param {object} opts
   * @param {() => {pc: RTCPeerConnection, videoSender: RTCRtpSender}} opts.createPeer 送信用の接続を作る（映像・音声のトラック付き）
   * @param {() => Promise<object>} opts.loadConfig /api/v1/config を取り直す
   * @param {(sender: RTCRtpSender) => Promise<void>} opts.applyQuality 主の送信と同じ品質にそろえる
   * @param {(h: object) => object} opts.headers 入室トークンを加える
   * @param {string} opts.clientId
   * @param {number[]} opts.backoff 再接続の間隔（秒）
   * @param {(status: {kind: string, text: string}) => void} opts.onStatus 表示の更新
   */
  constructor(opts) {
    this.opts = opts;
    this.cameraId = null;
    this.target = null;
    this.pc = null;
    this.whip = null;
    this.videoSender = null;
    this.attempt = 0;
    this.running = false;
    this.retryTimer = null;
    this.refreshTimer = null;
    this.connecting = false;
  }

  /**
   * @param {string} cameraId
   * @param {{name: string, url: string} | null} target 最初の送り先（config.cluster.backup）
   */
  start(cameraId, target) {
    this.stop();
    this.running = true;
    this.cameraId = cameraId;
    this.target = target;
    this.attempt = 0;
    this.refreshTimer = setInterval(() => this.refresh(), REFRESH_MS);
    this.connect();
  }

  stop() {
    this.running = false;
    clearTimeout(this.retryTimer);
    clearInterval(this.refreshTimer);
    this.retryTimer = null;
    this.refreshTimer = null;
    this.teardown();
    this.target = null;
  }

  /** 推定利用可能帯域（kbps）。未接続なら null。 */
  async availableKbps() {
    const pc = this.pc;
    if (!pc || pc.connectionState !== "connected") return null;
    const report = await pc.getStats().catch(() => null);
    if (!report) return null;
    let transport;
    const pairs = new Map();
    report.forEach((s) => {
      if (s.type === "transport") transport = s;
      else if (s.type === "candidate-pair") pairs.set(s.id, s);
    });
    const pair =
      (transport?.selectedCandidatePairId && pairs.get(transport.selectedCandidatePairId)) ||
      [...pairs.values()].find((p) => p.nominated && p.state === "succeeded");
    return pair?.availableOutgoingBitrate != null ? pair.availableOutgoingBitrate / 1000 : null;
  }

  /** 主の送信の品質を変えたときに呼ぶ。 */
  async applyQuality() {
    if (this.videoSender) await this.opts.applyQuality(this.videoSender).catch(() => {});
  }

  /** 送信中のトラックを差し替える（カメラ・マイクの変更）。 */
  async replaceTracks(stream) {
    if (!this.pc) return;
    for (const tx of this.pc.getTransceivers()) {
      const track = stream.getTracks().find((t) => t.kind === tx.receiver.track.kind);
      if (track) await tx.sender.replaceTrack(track);
    }
  }

  status(kind, text) {
    this.opts.onStatus({ kind, text });
  }

  async refresh() {
    if (!this.running || this.connecting) return;
    let next;
    try {
      const cfg = await this.opts.loadConfig();
      next = cfg.cluster?.backup ?? null;
    } catch {
      return; // 主の Server に届かない間は今の送り先のまま
    }
    if ((next?.url ?? null) === (this.target?.url ?? null)) {
      if (!next && !this.pc) this.status("none", "送り先なし（稼働中の別の Server がありません）");
      return;
    }
    // 共通IPが移った等で送り先が変わった。張り直す。
    this.target = next;
    this.attempt = 0;
    clearTimeout(this.retryTimer);
    this.retryTimer = null;
    this.teardown();
    this.connect();
  }

  async connect() {
    this.retryTimer = null;
    if (!this.running) return;
    if (!this.target) {
      this.status("none", "送り先なし（稼働中の別の Server がありません）");
      return;
    }
    const target = this.target;
    this.connecting = true;
    this.status("connecting", `${target.name} へ接続中`);
    try {
      const { pc, videoSender } = this.opts.createPeer();
      this.pc = pc;
      this.videoSender = videoSender;
      pc.addEventListener("connectionstatechange", () => this.onState(pc, target));
      const endpoint = `${target.url}/api/v1/cameras/${encodeURIComponent(this.cameraId)}/whip`;
      this.whip = new WhipClient(endpoint, this.opts.clientId, { headers: this.opts.headers });
      await this.whip.publish(pc, { iceTimeoutMs: 2000 });
      await this.applyQuality();
    } catch (err) {
      this.failed(target, err);
    } finally {
      this.connecting = false;
    }
  }

  onState(pc, target) {
    if (pc !== this.pc) return;
    if (pc.connectionState === "connected") {
      this.attempt = 0;
      this.status("live", `${target.name} へ送信中`);
    } else if (pc.connectionState === "failed") {
      this.failed(target, new WhipError("接続が切断されました"));
    }
  }

  failed(target, err) {
    if (!this.running || this.retryTimer || target !== this.target) return;
    this.teardown();
    const backoff = this.opts.backoff;
    const delay = backoff[Math.min(this.attempt, backoff.length - 1)];
    this.attempt += 1;
    // 入室切れ等の回復しない原因でも、主の送信は続けるため予備だけ止めずに間隔をあけて試す。
    const wait = err instanceof WhipError && !err.retryable ? Math.max(delay, 30) : delay;
    this.status("reconnecting", `${target.name}: ${err.message}（${wait}秒後に再接続）`);
    this.retryTimer = setTimeout(() => this.connect(), wait * 1000);
  }

  teardown() {
    this.whip?.stop();
    this.whip = null;
    if (this.pc) {
      const pc = this.pc;
      this.pc = null;
      pc.close();
    }
    this.videoSender = null;
  }
}
