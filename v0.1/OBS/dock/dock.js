// AMoRe v0.1 - OBS カスタムブラウザドック
// Server の /api/v1/status を定期取得し、カメラごとの受信状態と SRT URL を表示する。
// （OBSプラグインの実装までの代替。映像の受信は OBS 標準のメディアソースで行う。）

const POLL_MS = 2000;

const $ = (id) => document.getElementById(id);
const list = $("cameras");
const template = $("cameraTemplate");
const items = new Map();

function itemFor(cam) {
  let el = items.get(cam.id);
  if (el) return el;
  el = template.content.firstElementChild.cloneNode(true);
  const input = el.querySelector(".url");
  el.querySelector(".copy").addEventListener("click", async (ev) => {
    const button = ev.currentTarget;
    try {
      await navigator.clipboard.writeText(input.value);
      button.textContent = "コピー済";
    } catch {
      // OBS のドックでクリップボードが使えない場合は選択状態にして手動コピーさせる。
      input.focus();
      input.select();
      button.textContent = "Ctrl+C";
    }
    setTimeout(() => { button.textContent = "コピー"; }, 1500);
  });
  list.append(el);
  items.set(cam.id, el);
  return el;
}

// MediaMTX のコーデック名を表示用に短くする。
const codecNames = (list) => list.map((c) => (c === "MPEG-4 Audio" ? "AAC" : c)).join(" / ") || "-";

function describe(cam) {
  if (cam.receiving && cam.output_ready) {
    const enc = cam.reencoding ? "H.264へ変換中" : "映像は再エンコードなし";
    return {
      cls: "live",
      state: `受信中・OBS接続 ${cam.srt_readers}`,
      detail: `${codecNames(cam.input_codecs)} → ${codecNames(cam.output_codecs)}（${enc}）`,
    };
  }
  if (cam.receiving) {
    return { cls: "partial", state: "変換準備中", detail: `受信: ${codecNames(cam.input_codecs)}` };
  }
  if (cam.media_error) {
    // カメラ専用コンテナが停止・未作成の場合など。
    return { cls: "error", state: "処理停止", detail: `${cam.media_error}（管理Webで確認してください）` };
  }
  return { cls: "", state: "未送信", detail: "カメラPCの送信画面で送信を開始してください" };
}

function render(status) {
  // 管理Webで削除されたカメラは表示から外す。
  const ids = new Set(status.cameras.map((c) => c.id));
  for (const [id, el] of items) {
    if (!ids.has(id)) {
      el.remove();
      items.delete(id);
    }
  }
  for (const cam of status.cameras) {
    const el = itemFor(cam);
    const d = describe(cam);
    el.className = `camera ${d.cls}`;
    el.querySelector(".name").textContent = `${cam.name}（${cam.id}）`;
    el.querySelector(".state").textContent = d.state;
    el.querySelector(".detail").textContent = d.detail;
    const input = el.querySelector(".url");
    if (document.activeElement !== input) input.value = cam.srt_url;
  }
  $("version").textContent = `v${status.version}`;
}

async function poll() {
  const server = $("server");
  const error = $("error");
  try {
    const res = await fetch("/api/v1/status", { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const status = await res.json();
    render(status);
    if (!status.docker_error) {
      server.textContent = "Server 接続中";
      server.className = "server ok";
      error.textContent = status.cameras.length ? "" : "カメラが登録されていません（管理Web /admin/ で追加）。";
      error.hidden = status.cameras.length > 0;
    } else {
      server.textContent = "コンテナ管理 停止";
      server.className = "server err";
      error.textContent = `Server が Docker に接続できません。管理Webで確認してください。\n${status.docker_error}`;
      error.hidden = false;
    }
  } catch (e) {
    server.textContent = "Server 接続不可";
    server.className = "server err";
    error.textContent = `AMoRe Server に接続できません（${e.message}）。`;
    error.hidden = false;
  }
  $("updated").textContent = `更新 ${new Date().toLocaleTimeString("ja-JP")}`;
}

poll();
setInterval(poll, POLL_MS);
