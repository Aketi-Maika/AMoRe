// AMoRe v0.6 - OBS カスタムブラウザドック
// Server の /api/v1/status を定期取得し、カメラごとの受信状態と SRT URL を表示する。
// （OBSプラグインの実装までの代替。映像の受信は OBS 標準のメディアソースで行う。）
// 冗長構成では、OBS 用の共通IPを受け持つ Server と、各カメラを直接受けている Server・中継の状況も表示する。
// v0.6：OBS のメディアソースの映像から、カメラPC → OBS の遅延を測って表示し、Server へ送る（latency.js）。

import { auth } from "/login.js";
import { LatencyProbe } from "/obs/latency.js";

const POLL_MS = 2000;

const $ = (id) => document.getElementById(id);
const list = $("cameras");
const template = $("cameraTemplate");
const items = new Map();
const probe = new LatencyProbe(renderLatency);

/** 遅延の表示（カメラごと・計測の状態）。 */
function renderLatency() {
  for (const [id, el] of items) {
    const r = probe.results.get(id);
    const out = el.querySelector(".latency");
    out.hidden = !r;
    if (!r) continue;
    out.textContent = r.ms != null ? `遅延 ${Math.round(r.ms)} ms（カメラPC → OBS）` : `遅延 - （${r.error}）`;
    out.classList.toggle("warn", r.ms == null);
  }
  const status = $("latencyStatus");
  status.textContent = probe.status.text;
  status.className = probe.status.ok ? "ok" : "warn";
}

function initLatencySettings() {
  const s = probe.settings;
  $("obsUrl").value = s.url;
  $("obsPassword").value = s.password;
  $("obsName").value = s.name;
  $("latencyEnabled").checked = s.enabled;
  $("latencySave").addEventListener("click", () => {
    probe.configure({
      url: $("obsUrl").value.trim() || "ws://127.0.0.1:4455",
      password: $("obsPassword").value,
      name: $("obsName").value.trim() || "編集PC",
      enabled: $("latencyEnabled").checked,
    });
  });
}

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
  if (cam.health?.problem) {
    // カメラ専用コンテナの不調（再起動の繰り返し・停止など）。AMoRe が自動で復旧を試みる。
    return { cls: "error", state: "不調", detail: `${cam.health.problem}（${cam.health.recovery ?? "自動で確認・復旧しています"}）` };
  }
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
  if (cam.relay_from && cam.output_ready) {
    // 冗長構成：このカメラは別の Server が直接受けており、この Server は中継で同じ映像を出している。
    return {
      cls: "live",
      state: `中継中・OBS接続 ${cam.srt_readers}`,
      detail: `Server ${cam.relay_from} から中継（${codecNames(cam.output_codecs)}）`,
    };
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
    // AI の推奨カメラ・映像の異常（ai が有効な場合）
    const v = cam.ai_vision;
    const ai = el.querySelector(".ai");
    const recommended = status.ai_director?.camera_id === cam.id;
    ai.textContent = [recommended ? `★ AIの推奨: ${status.ai_director.reason}` : "", v?.anomaly ? `⚠ ${v.anomalies.join(" / ")}` : ""].filter(Boolean).join("　");
    ai.hidden = !ai.textContent;
    el.classList.toggle("recommended", recommended);
    el.querySelector(".rec").hidden = !cam.recording;
    const input = el.querySelector(".url");
    if (document.activeElement !== input) input.value = cam.srt_url;
  }
  $("version").textContent = `v${status.version}`;
  // OBS 向けの映像を出しているカメラだけを測る。
  probe.update(
    status.latency_interval_sec ?? null,
    status.cameras.filter((c) => c.output_ready).map((c) => c.id),
  );
  renderLatency();
}

async function poll() {
  const server = $("server");
  const error = $("error");
  try {
    const res = await auth.fetch("/api/v1/status", { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const status = await res.json();
    render(status);
    if (!status.docker_error) {
      const c = status.cluster;
      const down = c?.servers.filter((s) => !s.up).map((s) => s.name) ?? [];
      server.textContent = c
        ? `Server ${c.site} 接続中${down.length ? `（停止: ${down.join("・")}）` : ""}`
        : "Server 接続中";
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

await auth.ensure("dock");
initLatencySettings();
poll();
setInterval(poll, POLL_MS);
