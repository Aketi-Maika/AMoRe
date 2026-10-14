// AMoRe v0.3 - モニター画面
// 1つの画面に全カメラの映像を並べて表示する。送信が始まったカメラは自動で表示し、途切れたら自動で再接続する。
// 映像は Server から WebRTC（WHEP）で受信する（管理Webの映像確認と同じ）。

import { WhepPlayer } from "/admin/whep.js";

const POLL_MS = 3000;
/** 接続に失敗したカメラを再試行するまでの時間 */
const RETRY_MS = 5000;

const $ = (id) => document.getElementById(id);
const grid = $("grid");
const template = $("tileTemplate");
const tiles = new Map(); // id -> { el, player, video, retryAt, status }
let iceServers = [];
let audioCamera = null;

function setState(el, text, cls) {
  const s = el.querySelector(".state");
  s.textContent = text;
  s.className = `state ${cls}`;
}

function createTile(id) {
  const el = template.content.firstElementChild.cloneNode(true);
  el.dataset.id = id;
  const video = el.querySelector("video");
  const placeholder = el.querySelector(".placeholder");
  const tile = { el, video, player: null, retryAt: 0, status: null };
  tile.player = new WhepPlayer(id, video, {
    iceServers,
    onState: (s, text) => {
      placeholder.hidden = s === "playing";
      if (s !== "playing") placeholder.textContent = text || "";
      if (s === "failed") tile.retryAt = Date.now() + RETRY_MS;
    },
  });

  // タイルを押すと拡大・元に戻す。
  el.addEventListener("click", () => {
    const focused = el.classList.toggle("focus");
    tiles.forEach((t) => { if (t.el !== el) t.el.classList.remove("focus"); });
    grid.classList.toggle("focused", focused);
  });
  // 音声は1台だけ鳴らす（複数の音声が重なるとエコーになるため）。
  el.querySelector(".audio").addEventListener("click", (ev) => {
    ev.stopPropagation();
    audioCamera = audioCamera === id ? null : id;
    tiles.forEach((t, tid) => {
      t.video.muted = tid !== audioCamera;
      const btn = t.el.querySelector(".audio");
      btn.textContent = tid === audioCamera ? "🔊" : "🔇";
      btn.classList.toggle("on", tid === audioCamera);
    });
  });
  setInterval(() => {
    el.querySelector(".res").textContent = video.videoWidth ? `${video.videoWidth}×${video.videoHeight}` : "";
  }, 1000);

  grid.append(el);
  tiles.set(id, tile);
  return tile;
}

function render(status) {
  const ids = new Set(status.cameras.map((c) => c.id));
  for (const [id, t] of tiles) {
    if (!ids.has(id)) {
      t.player.stop({ silent: true });
      t.el.remove();
      tiles.delete(id);
    }
  }
  const recommended = status.ai_director?.camera_id ?? null;
  const director = $("director");
  if (status.ai_director) {
    const cam = status.cameras.find((c) => c.id === recommended);
    director.textContent = `AIの推奨: ${cam?.name ?? recommended}（${status.ai_director.reason}）`;
    director.hidden = false;
  } else {
    director.hidden = true;
  }

  for (const cam of status.cameras) {
    const t = tiles.get(cam.id) ?? createTile(cam.id);
    const { el, player } = t;
    el.querySelector(".name").textContent = `${cam.name}（${cam.id}）`;
    el.querySelector(".rec").hidden = !cam.recording;
    el.classList.toggle("recommended", cam.id === recommended);
    el.querySelector(".recommend").hidden = cam.id !== recommended;
    el.classList.toggle("problem", !!cam.health?.problem);
    const v = cam.ai_vision;
    el.classList.toggle("anomaly", !!v?.anomaly);
    el.querySelector(".ai").textContent = v ? (v.anomaly ? `⚠ ${v.anomalies.join(" / ")}` : v.summary) : "";

    if (cam.health?.problem) {
      setState(el, "不調", "err");
    } else if (cam.output_ready) {
      setState(el, "受信中", "ok");
    } else if (cam.receiving) {
      setState(el, "準備中", "warn");
    } else {
      setState(el, "未送信", "");
    }

    if (cam.output_ready || cam.receiving) {
      if (!player.active && Date.now() >= t.retryAt) {
        player.start().catch(() => {});
      }
    } else if (player.active) {
      player.stop({ silent: true });
    }
    if (!player.active) {
      el.querySelector(".placeholder").hidden = false;
      if (!cam.receiving) {
        el.querySelector(".placeholder").textContent = cam.health?.problem
          ? `コンテナが不調です\n${cam.health.problem}`
          : "送信を待っています";
      }
    }
  }
  grid.dataset.count = String(Math.min(status.cameras.length, 4));
  $("empty").hidden = status.cameras.length > 0;
}

async function poll() {
  const server = $("server");
  try {
    const res = await fetch("/api/v1/status", { cache: "no-store" });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    render(await res.json());
    server.textContent = "Server 接続中";
    server.className = "server ok";
  } catch (e) {
    server.textContent = `Server 接続不可（${e.message}）`;
    server.className = "server err";
  }
}

$("fullscreenBtn").addEventListener("click", () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else document.documentElement.requestFullscreen().catch(() => {});
});

setInterval(() => {
  $("clock").textContent = new Date().toLocaleTimeString("ja-JP");
}, 1000);

window.addEventListener("pagehide", () => {
  tiles.forEach((t) => t.player.stop({ silent: true }));
});

async function init() {
  try {
    const cfg = await (await fetch("/api/v1/config")).json();
    iceServers = (cfg.ice_servers ?? []).map((s) => ({ urls: s.urls, username: s.username, credential: s.credential }));
  } catch {
    // LAN 内では ICE サーバーなしで表示できる。
  }
  await poll();
  setInterval(poll, POLL_MS);
}

init();
