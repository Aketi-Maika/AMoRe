// AMoRe v0.6 - obs-websocket（v5。OBS 28 以降に標準搭載）の最小限のクライアント
// OBS ドックが、メディアソースの映像を取り込んで遅延計測の時刻コードを読むために使う。
// OBS の「ツール → WebSocket サーバー設定」で「WebSocket サーバーを有効にする」を ON にしておく。

const OP = { Hello: 0, Identify: 1, Identified: 2, Request: 6, RequestResponse: 7 };
const REQUEST_TIMEOUT_MS = 5000;

async function sha256Base64(text) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return btoa(String.fromCharCode(...new Uint8Array(digest)));
}

export class ObsError extends Error {}

export class ObsClient {
  /**
   * @param {string} url 例: ws://127.0.0.1:4455
   * @param {string} password WebSocket サーバーのパスワード（認証を使わない場合は空）
   */
  constructor(url, password) {
    this.url = url;
    this.password = password;
    this.ws = null;
    this.ready = false;
    this.nextId = 1;
    this.pending = new Map();
  }

  /** 接続して認証する。 */
  connect() {
    return new Promise((resolve, reject) => {
      let settled = false;
      const fail = (message) => {
        if (settled) return;
        settled = true;
        this.close();
        reject(new ObsError(message));
      };
      let ws;
      try {
        ws = new WebSocket(this.url, "obswebsocket.json");
      } catch (e) {
        fail(`OBS の WebSocket サーバー（${this.url}）に接続できません: ${e.message}`);
        return;
      }
      this.ws = ws;
      const timer = setTimeout(() => fail("OBS の WebSocket サーバーが応答しません"), REQUEST_TIMEOUT_MS);
      ws.addEventListener("message", async (ev) => {
        let msg;
        try {
          msg = JSON.parse(ev.data);
        } catch {
          return;
        }
        if (msg.op === OP.Hello) {
          const identify = { rpcVersion: 1, eventSubscriptions: 0 };
          const auth = msg.d?.authentication;
          if (auth) {
            if (!this.password) {
              fail("OBS の WebSocket サーバーにパスワードが設定されています。ドックの「遅延計測」にパスワードを入力してください");
              return;
            }
            const secret = await sha256Base64(this.password + auth.salt);
            identify.authentication = await sha256Base64(secret + auth.challenge);
          }
          ws.send(JSON.stringify({ op: OP.Identify, d: identify }));
        } else if (msg.op === OP.Identified) {
          clearTimeout(timer);
          this.ready = true;
          settled = true;
          resolve();
        } else if (msg.op === OP.RequestResponse) {
          const p = this.pending.get(msg.d.requestId);
          if (!p) return;
          this.pending.delete(msg.d.requestId);
          clearTimeout(p.timer);
          if (msg.d.requestStatus?.result) p.resolve(msg.d.responseData ?? {});
          else p.reject(new ObsError(msg.d.requestStatus?.comment ?? `OBS の要求 ${msg.d.requestType} に失敗しました`));
        }
      });
      ws.addEventListener("close", (ev) => {
        clearTimeout(timer);
        this.ready = false;
        for (const p of this.pending.values()) {
          clearTimeout(p.timer);
          p.reject(new ObsError("OBS との接続が切れました"));
        }
        this.pending.clear();
        // 4009 = 認証の失敗（obs-websocket の WebSocketCloseCode.AuthenticationFailed）
        fail(ev.code === 4009 ? "OBS の WebSocket サーバーのパスワードが違います" : `OBS の WebSocket サーバー（${this.url}）に接続できません。OBS の「ツール → WebSocket サーバー設定」を確認してください`);
      });
    });
  }

  /** 要求を送り、応答（responseData）を返す。 */
  request(requestType, requestData = {}) {
    if (!this.ready) return Promise.reject(new ObsError("OBS に接続していません"));
    const requestId = String(this.nextId++);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        reject(new ObsError(`OBS の要求 ${requestType} が時間内に終わりません`));
      }, REQUEST_TIMEOUT_MS);
      this.pending.set(requestId, { resolve, reject, timer });
      this.ws.send(JSON.stringify({ op: OP.Request, d: { requestType, requestId, requestData } }));
    });
  }

  close() {
    this.ready = false;
    try {
      this.ws?.close();
    } catch {
      /* 無視 */
    }
    this.ws = null;
  }
}
