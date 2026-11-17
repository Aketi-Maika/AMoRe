// AMoRe v0.6 - 入室（画面ごとのパスワード）
// access.yaml でパスワードが必要な画面は、開いたときに入力欄を出し、入室トークンを API に付けて送る。
// トークンはタブを閉じると消える（sessionStorage）。admin で入室していれば他の画面でもそのまま使える。
// 送信画面・管理Web・モニター・OBSドックで共通に使う（Server は /login.js として配信する）。

const KEY = (scope) => `amore.token.${scope}`;
const LABEL = {
  admin: "管理画面",
  camera: "送信画面",
  monitor: "モニター",
  dock: "OBSドック",
};

const session = {
  get(key) {
    try { return sessionStorage.getItem(key); } catch { return null; }
  },
  set(key, value) {
    try { sessionStorage.setItem(key, value); } catch { /* 保存できなくても今の画面では使える */ }
  },
  remove(key) {
    try { sessionStorage.removeItem(key); } catch { /* 無視 */ }
  },
};

/** トークンの期限（UNIX秒）。形式は `範囲.期限.署名`。 */
function expiresAt(token) {
  const exp = Number(token?.split(".")[1]);
  return Number.isFinite(exp) ? exp : 0;
}

function injectStyle() {
  if (document.getElementById("amore-login-style")) return;
  const style = document.createElement("style");
  style.id = "amore-login-style";
  style.textContent = `
    .amore-login { position: fixed; inset: 0; z-index: 1000; display: flex; align-items: center; justify-content: center;
      padding: 16px; background: rgba(0, 0, 0, 0.55); font-family: system-ui, sans-serif; }
    .amore-login[hidden] { display: none !important; }
    .amore-login form { width: min(360px, 100%); padding: 20px; border-radius: 10px; background: #fff; color: #1f2328;
      box-shadow: 0 10px 30px rgba(0, 0, 0, 0.35); display: grid; gap: 12px; }
    .amore-login h2 { margin: 0; font-size: 1.1rem; }
    .amore-login p { margin: 0; font-size: 0.9rem; color: #59636e; }
    .amore-login input { font: inherit; padding: 8px 10px; border: 1px solid #c8ced6; border-radius: 6px; }
    .amore-login button { font: inherit; padding: 8px 12px; border: 0; border-radius: 6px; background: #1f6feb; color: #fff; cursor: pointer; }
    .amore-login button:disabled { opacity: 0.6; cursor: default; }
    .amore-login .amore-login-error { color: #cf222e; white-space: pre-wrap; }
    @media (prefers-color-scheme: dark) {
      .amore-login form { background: #161b22; color: #e6edf3; }
      .amore-login p { color: #9198a1; }
      .amore-login input { background: #0d1117; color: #e6edf3; border-color: #3d444d; }
      .amore-login .amore-login-error { color: #ff7b72; }
    }
  `;
  document.head.append(style);
}

/** パスワードの入力欄を出し、入室できたらトークンを返す。 */
function promptLogin(scope, message) {
  injectStyle();
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "amore-login";
    overlay.innerHTML = `
      <form>
        <h2></h2>
        <p class="amore-login-note"></p>
        <input type="password" autocomplete="current-password" required aria-label="パスワード" placeholder="パスワード">
        <p class="amore-login-error" role="alert" hidden></p>
        <button type="submit">入室</button>
      </form>`;
    overlay.querySelector("h2").textContent = `AMoRe ${LABEL[scope] ?? scope}`;
    overlay.querySelector(".amore-login-note").textContent =
      message ?? "この画面に入るにはパスワードが必要です。";
    const form = overlay.querySelector("form");
    const input = overlay.querySelector("input");
    const error = overlay.querySelector(".amore-login-error");
    const button = overlay.querySelector("button");
    form.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      button.disabled = true;
      error.hidden = true;
      try {
        const res = await fetch("/api/v1/login", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ scope, password: input.value }),
        });
        const data = await res.json().catch(() => ({}));
        if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
        session.set(KEY(scope), data.token);
        overlay.remove();
        resolve(data.token);
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        input.select();
      } finally {
        button.disabled = false;
      }
    });
    document.body.append(overlay);
    input.focus();
  });
}

class Auth {
  constructor() {
    /** この画面の範囲 */
    this.scope = null;
    /** 使うトークン（入室不要なら null） */
    this.token = null;
    this.required = {};
  }

  /**
   * 画面を開いたときに呼ぶ。パスワードが必要なら入室させる。
   * @param {string} scope admin / camera / monitor / dock
   */
  async ensure(scope) {
    this.scope = scope;
    let info;
    try {
      const res = await fetch("/api/v1/access", { cache: "no-store" });
      info = res.ok ? await res.json() : null;
    } catch {
      info = null;
    }
    // 入室の仕組みがない（古い Server）場合はそのまま使う。
    if (!info) return;
    this.required = info.required ?? {};
    if (!this.required[scope]) return;
    // この範囲か admin のトークンが残っていて期限内なら使う（Server で確かめる）。
    const now = Date.now() / 1000;
    for (const s of [scope, "admin"]) {
      const token = session.get(KEY(s));
      if (token && expiresAt(token) > now + 30 && (await this.#valid(token))) {
        this.token = token;
        return;
      }
      if (token) session.remove(KEY(s));
    }
    this.token = await promptLogin(scope);
  }

  async #valid(token) {
    try {
      const res = await fetch("/api/v1/access", {
        cache: "no-store",
        headers: { Authorization: `Bearer ${token}` },
      });
      return res.ok && (await res.json()).current != null;
    } catch {
      return false;
    }
  }

  /** API に付けるヘッダー。 */
  headers(extra = {}) {
    return this.token ? { ...extra, Authorization: `Bearer ${this.token}` } : { ...extra };
  }

  /** トークンを付けて送る。期限切れ等で 401 になったら入室し直して1回だけ送り直す。 */
  async fetch(url, opts = {}) {
    const send = () => fetch(url, { ...opts, headers: this.headers(opts.headers ?? {}) });
    const res = await send();
    if (res.status !== 401 || !this.scope || !this.required[this.scope]) return res;
    session.remove(KEY(this.scope));
    this.token = await promptLogin(this.scope, "入室の期限が切れました。もう一度パスワードを入力してください。");
    return send();
  }
}

/** 画面ごとに1つ。 */
export const auth = new Auth();
