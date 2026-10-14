// AMoRe v0.3 - WHIP クライアント
// SDP の offer を Server（AMoRe）へ送り、answer を受け取って WebRTC 接続を確立する。
// ICE 候補は収集完了まで待ってから offer に含める（Trickle ICE は使わない）。

export const CLIENT_HEADER = "X-AMoRe-Client";

export class WhipError extends Error {
  /**
   * @param {string} message 画面に表示する説明
   * @param {{status?: number, retryable?: boolean}} [opts]
   */
  constructor(message, { status = 0, retryable = true } = {}) {
    super(message);
    this.name = "WhipError";
    this.status = status;
    this.retryable = retryable;
  }
}

function waitIceGathering(pc, timeoutMs) {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      pc.removeEventListener("icegatheringstatechange", onChange);
      resolve();
    };
    const onChange = () => {
      if (pc.iceGatheringState === "complete") done();
    };
    // 収集が終わらない環境（到達できないSTUN等）でも、集まった候補で続行する。
    const timer = setTimeout(done, timeoutMs);
    pc.addEventListener("icegatheringstatechange", onChange);
  });
}

async function errorMessage(res) {
  const text = await res.text().catch(() => "");
  try {
    const json = JSON.parse(text);
    if (json && typeof json.error === "string") return json.error;
  } catch {
    // JSON 以外（MediaMTX の応答など）はそのまま使う。
  }
  return text.trim() || `HTTP ${res.status}`;
}

export class WhipClient {
  /**
   * @param {string} endpoint 例: /api/v1/cameras/cam1/whip
   * @param {string} clientId 送信画面ごとの識別子（同じ画面からの再接続を判別する）
   */
  constructor(endpoint, clientId) {
    this.endpoint = endpoint;
    this.clientId = clientId;
    this.resource = null;
  }

  /** @param {RTCPeerConnection} pc */
  async publish(pc, { iceTimeoutMs = 2000 } = {}) {
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await waitIceGathering(pc, iceTimeoutMs);

    let res;
    try {
      res = await fetch(this.endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/sdp", [CLIENT_HEADER]: this.clientId },
        body: pc.localDescription.sdp,
      });
    } catch (e) {
      throw new WhipError(`Server に接続できません（${e.message}）`);
    }

    if (res.status !== 201) {
      const message = await errorMessage(res);
      // 404: 未登録のカメラID、400/415: 要求の不備 → 再試行しても回復しない。
      const retryable = !(res.status === 400 || res.status === 404 || res.status === 415);
      throw new WhipError(`送信を開始できません: ${message}`, { status: res.status, retryable });
    }

    const location = res.headers.get("Location");
    this.resource = location ? new URL(location, new URL(this.endpoint, window.location.href)).href : null;
    const answer = await res.text();
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
  }

  /** WHIP セッションを終了する。ページを閉じる場合も送信されるよう keepalive を使う。 */
  async stop() {
    const resource = this.resource;
    this.resource = null;
    if (!resource) return;
    try {
      await fetch(resource, {
        method: "DELETE",
        headers: { [CLIENT_HEADER]: this.clientId },
        keepalive: true,
      });
    } catch {
      // Server 側のセッションはタイムアウトで破棄されるため無視する。
    }
  }
}
