// AMoRe v0.3 - 管理Web
// カメラの追加・削除・名前変更・録画切替、カメラごとのコンテナ操作・不調の表示、映像確認（WHEP）、AIによる障害診断を行う。

import { WhepPlayer } from "/admin/whep.js";

const POLL_MS = 3000;

const $ = (id) => document.getElementById(id);
const ui = {
  dockerBadge: $("dockerBadge"),
  senderLink: $("senderLink"),
  dockLink: $("dockLink"),
  monitorLink: $("monitorLink"),
  cameraCount: $("cameraCount"),
  reconcileInfo: $("reconcileInfo"),
  reconcileBtn: $("reconcileBtn"),
  addBtn: $("addBtn"),
  alerts: $("alerts"),
  cameras: $("cameras"),
  empty: $("empty"),
  events: $("events"),
  toast: $("toast"),
  template: $("cardTemplate"),
};

const state = {
  overview: null,
  iceServers: [],
  cards: new Map(), // id -> { el, player }
  busy: false,
};

// ---------- 共通 ----------

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    const msg = (data && data.error) || (typeof data === "string" && data) || `HTTP ${res.status}`;
    throw new Error(msg);
  }
  return data;
}

let toastTimer;
function toast(message, { error = false, ms = 5000 } = {}) {
  ui.toast.textContent = message;
  ui.toast.classList.toggle("err", error);
  ui.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { ui.toast.hidden = true; }, ms);
}

function setBusy(busy) {
  state.busy = busy;
  ui.reconcileBtn.disabled = busy;
  ui.addBtn.disabled = busy || !state.overview?.next_camera_id;
  for (const { el } of state.cards.values()) {
    el.querySelectorAll(".actions button").forEach((b) => { b.disabled = busy; });
  }
}

function reconcileMessage(result) {
  if (!result) return "";
  const r = result.reconcile ?? result;
  return r.errors?.length ? `コンテナ操作でエラー:\n${r.errors.join("\n")}` : `コンテナ: ${r.summary}`;
}

const CONTAINER_STATE = {
  running: ["稼働中", "ok"],
  restarting: ["再起動中", "warn"],
  created: ["作成済み", "warn"],
  paused: ["一時停止", "warn"],
  exited: ["停止", "err"],
  dead: ["異常終了", "err"],
};

const codecNames = (list) => list.map((c) => (c === "MPEG-4 Audio" ? "AAC" : c)).join(" / ");

function fmtTime(ms) {
  return new Date(ms).toLocaleTimeString("ja-JP");
}

/** AI の回答（Markdown）を安全に表示する（見出し・箇条書き・コード・太字だけ対応）。 */
function renderMarkdown(md, container) {
  const esc = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const inline = (s) => esc(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  const out = [];
  let list = null;
  let code = null;
  const closeList = () => { if (list) { out.push(`</${list}>`); list = null; } };
  for (const line of md.split(/\r?\n/)) {
    if (code !== null) {
      if (line.startsWith("```")) { out.push(`<pre>${esc(code.join("\n"))}</pre>`); code = null; } else { code.push(line); }
      continue;
    }
    if (line.startsWith("```")) { closeList(); code = []; continue; }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    const ul = line.match(/^\s*[-*]\s+(.*)$/);
    const ol = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (h) { closeList(); out.push(`<h${Math.min(h[1].length + 2, 6)}>${inline(h[2])}</h${Math.min(h[1].length + 2, 6)}>`); }
    else if (ul || ol) {
      const tag = ul ? "ul" : "ol";
      if (list !== tag) { closeList(); out.push(`<${tag}>`); list = tag; }
      out.push(`<li>${inline((ul || ol)[1])}</li>`);
    } else if (line.trim() === "") { closeList(); }
    else { closeList(); out.push(`<p>${inline(line)}</p>`); }
  }
  if (code !== null) out.push(`<pre>${esc(code.join("\n"))}</pre>`);
  closeList();
  container.innerHTML = out.join("");
}

// ---------- 確認ダイアログ ----------

function confirmAction(title, text, okLabel = "実行") {
  const dialog = $("confirmDialog");
  $("confirmTitle").textContent = title;
  $("confirmText").textContent = text;
  $("confirmOk").textContent = okLabel;
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue === "ok"), { once: true });
    dialog.returnValue = "";
    dialog.showModal();
  });
}

$("confirmOk").addEventListener("click", (ev) => {
  ev.preventDefault();
  $("confirmDialog").close("ok");
});

document.querySelectorAll("dialog [data-close]").forEach((btn) => {
  btn.addEventListener("click", () => btn.closest("dialog").close("cancel"));
});

// ---------- カード ----------

function createCard(id) {
  const el = ui.template.content.firstElementChild.cloneNode(true);
  el.dataset.id = id;
  const video = el.querySelector(".video");
  const msg = el.querySelector(".preview-msg");
  const previewBtn = el.querySelector(".preview-btn");
  const muteBtn = el.querySelector(".mute-btn");
  let infoTimer = null;

  const player = new WhepPlayer(id, video, {
    iceServers: state.iceServers,
    onState: (s, text) => {
      msg.textContent = text ?? "";
      msg.classList.toggle("err", s === "failed");
      const active = s === "connecting" || s === "playing";
      previewBtn.textContent = active ? "停止" : "映像確認";
      el.querySelector(".placeholder").hidden = s === "playing";
      muteBtn.hidden = s !== "playing";
      clearInterval(infoTimer);
      const info = el.querySelector(".video-info");
      info.hidden = true;
      if (s === "playing") {
        infoTimer = setInterval(() => {
          if (video.videoWidth) {
            info.textContent = `${video.videoWidth}×${video.videoHeight}`;
            info.hidden = false;
          }
        }, 1000);
      } else {
        video.muted = true;
        muteBtn.textContent = "音声をオン";
      }
    },
  });

  previewBtn.addEventListener("click", () => {
    if (player.active) player.stop();
    else player.start().catch(() => {});
  });
  muteBtn.addEventListener("click", () => {
    video.muted = !video.muted;
    muteBtn.textContent = video.muted ? "音声をオン" : "音声をオフ";
  });
  el.querySelector(".copy-btn").addEventListener("click", async (ev) => {
    const input = el.querySelector(".srt");
    const btn = ev.currentTarget;
    try {
      await navigator.clipboard.writeText(input.value);
      btn.textContent = "コピー済";
    } catch {
      input.focus();
      input.select();
      btn.textContent = "Ctrl+C";
    }
    setTimeout(() => { btn.textContent = "コピー"; }, 1500);
  });
  el.querySelector(".rename-btn").addEventListener("click", () => openRename(id));
  el.querySelector(".restart-btn").addEventListener("click", () => restartCamera(id));
  el.querySelector(".logs-btn").addEventListener("click", () => openLogs(id));
  el.querySelector(".delete-btn").addEventListener("click", () => deleteCamera(id));
  el.querySelector(".record-btn").addEventListener("click", () => toggleRecording(id));
  el.querySelector(".diagnose-btn").addEventListener("click", () => openDiagnosis(id));

  ui.cameras.append(el);
  const entry = { el, player };
  state.cards.set(id, entry);
  return entry;
}

function setDd(el, text, cls = "") {
  el.textContent = text;
  el.className = el.className.split(" ").filter((c) => !["ok", "warn", "err"].includes(c)).join(" ");
  if (cls) el.classList.add(cls);
}

function renderCard(cam, entry, ov) {
  const { el } = entry;
  el.querySelector(".name").textContent = cam.name;
  el.querySelector(".cid").textContent = `${cam.id}　/　${cam.container?.name ?? "コンテナ未作成"}`;

  const badge = el.querySelector(".state");
  let [label, cls] = cam.container ? (CONTAINER_STATE[cam.container.state] ?? [cam.container.state, "warn"]) : ["未作成", "err"];
  if (cam.health.problem && cls === "ok") [label, cls] = ["不調", "err"];
  badge.textContent = label;
  badge.className = `badge state ${cls}`;
  el.querySelector(".rec").hidden = !cam.recording;
  el.classList.toggle("has-problem", !!cam.health.problem);

  // 不調の理由・自動復旧の状況・直近ログ
  const problem = el.querySelector(".problem");
  problem.hidden = !cam.health.problem;
  if (cam.health.problem) {
    el.querySelector(".problem-text").textContent = cam.health.problem;
    el.querySelector(".problem-recovery").textContent = cam.health.recovery ?? "自動で確認・復旧しています";
    const logs = el.querySelector(".problem-logs");
    logs.hidden = cam.health.logs.length === 0;
    logs.querySelector("pre").textContent = cam.health.logs.join("\n");
  }

  const recv = el.querySelector(".recv");
  if (cam.receiving && cam.output_ready) {
    const enc = cam.reencoding ? "H.264へ変換中" : "再エンコードなし";
    setDd(recv, `受信中　${codecNames(cam.input_codecs)} → ${codecNames(cam.output_codecs)}（${enc}）`, "ok");
  } else if (cam.receiving) {
    setDd(recv, `受信中・変換準備中（${codecNames(cam.input_codecs)}）`, "warn");
  } else if (cam.media_error) {
    setDd(recv, cam.media_error, "err");
  } else {
    setDd(recv, "未送信");
  }
  setDd(el.querySelector(".obs"), `${cam.srt_readers} 接続${cam.preview_viewers ? `（映像確認・モニター ${cam.preview_viewers}）` : ""}`, cam.srt_readers ? "ok" : "");
  const v = cam.ai_vision;
  setDd(
    el.querySelector(".ai"),
    v ? `${v.anomaly ? `⚠ ${v.anomalies.join(" / ")}` : v.summary}（品質 ${v.quality}/5・${fmtTime(v.at_ms)}）` : "-",
    v?.anomaly ? "err" : "",
  );
  setDd(el.querySelector(".ports"), `WebRTC ${cam.webrtc_udp_port}/udp　SRT ${cam.srt_port}/udp`);
  setDd(el.querySelector(".cstatus"), cam.container ? cam.container.status : "未作成（「再起動」または「再適用」で作成）", cam.container ? "" : "err");

  const srt = el.querySelector(".srt");
  if (document.activeElement !== srt) srt.value = cam.srt_url;
  el.querySelector(".sender-link").href = `/?camera=${encodeURIComponent(cam.id)}`;
  el.querySelector(".record-btn").textContent = cam.recording ? "録画を止める" : "録画する";
  el.querySelector(".diagnose-btn").hidden = !ov.ai_diagnosis;
}

function renderAlerts(ov) {
  const alerts = [];
  const rt = ov.runtime;
  if (rt.docker_error) {
    alerts.push(["err", `Dockerに接続できません。カメラのコンテナを操作できません。\n${rt.docker_error}`]);
  } else if (!rt.image_ready) {
    alerts.push(rt.image_error
      ? ["err", `コンテナのイメージ ${ov.image} を取得できません。\n${rt.image_error}`]
      : ["warn", `コンテナのイメージ ${ov.image} を準備しています（初回は数分かかります）。`]);
  }
  const troubled = ov.cameras.filter((c) => c.health.problem);
  if (troubled.length) {
    alerts.push(["err", `不調のカメラがあります: ${troubled.map((c) => `${c.name}（${c.id}）`).join("、")}\n各カードに理由と直近のログを表示しています。`]);
  }
  ui.alerts.replaceChildren(...alerts.map(([cls, text]) => {
    const div = document.createElement("div");
    div.className = `alert ${cls}`;
    div.textContent = text;
    return div;
  }));
}

function renderEvents(ov) {
  const events = ov.recent_events ?? [];
  if (events.length === 0) return;
  ui.events.replaceChildren(...events.map((e) => {
    const li = document.createElement("li");
    li.className = `event ${e.level}`;
    li.textContent = `${fmtTime(e.time_ms)}　${e.text}`;
    return li;
  }));
}

function render(ov) {
  state.overview = ov;
  const rt = ov.runtime;

  if (rt.docker_error) {
    ui.dockerBadge.textContent = "Docker 接続不可";
    ui.dockerBadge.className = "badge err";
  } else if (rt.docker_version) {
    ui.dockerBadge.textContent = `Docker ${rt.docker_version}`;
    ui.dockerBadge.className = "badge ok";
  }
  ui.senderLink.href = ov.sender_url;
  ui.dockLink.href = ov.dock_url;
  ui.monitorLink.href = ov.monitor_url;
  ui.cameraCount.textContent = `カメラ ${ov.cameras.length} / ${ov.max_cameras} 台`;
  if (rt.last_reconcile_at) {
    const time = new Date(rt.last_reconcile_at * 1000).toLocaleTimeString("ja-JP");
    ui.reconcileInfo.textContent = `最終確認 ${time}：${rt.last_reconcile}　録画の保存先 ${ov.recording_dir}`;
  }
  ui.addBtn.disabled = state.busy || !ov.next_camera_id;
  ui.addBtn.title = ov.next_camera_id ? "" : "カメラ数の上限に達しています";
  renderAlerts(ov);
  renderEvents(ov);

  const ids = new Set(ov.cameras.map((c) => c.id));
  for (const [id, entry] of state.cards) {
    if (!ids.has(id)) {
      entry.player.stop();
      entry.el.remove();
      state.cards.delete(id);
    }
  }
  for (const cam of ov.cameras) {
    const entry = state.cards.get(cam.id) ?? createCard(cam.id);
    renderCard(cam, entry, ov);
  }
  ui.empty.hidden = ov.cameras.length > 0;
  setBusy(state.busy);
}

async function refresh() {
  try {
    render(await api("GET", "/api/v1/admin/overview"));
  } catch (e) {
    ui.dockerBadge.textContent = "Server 接続不可";
    ui.dockerBadge.className = "badge err";
    ui.alerts.replaceChildren(Object.assign(document.createElement("div"), {
      className: "alert err",
      textContent: `AMoRe Server に接続できません（${e.message}）。`,
    }));
  }
}

// ---------- 操作 ----------

async function runBusy(fn) {
  setBusy(true);
  try {
    await fn();
  } finally {
    setBusy(false);
    await refresh();
  }
}

const findCamera = (id) => state.overview?.cameras.find((c) => c.id === id);

ui.addBtn.addEventListener("click", () => {
  const ov = state.overview;
  $("addId").value = "";
  $("addId").placeholder = ov?.next_camera_id ?? "";
  $("addName").value = "";
  $("addError").hidden = true;
  $("addDialog").showModal();
  $("addName").focus();
});

$("addForm").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const id = $("addId").value.trim();
  const name = $("addName").value.trim();
  const submit = $("addSubmit");
  submit.disabled = true;
  submit.textContent = "コンテナ作成中…";
  try {
    await runBusy(async () => {
      const result = await api("POST", "/api/v1/admin/cameras", id ? { id, name } : { name });
      $("addDialog").close();
      const errs = result.reconcile?.errors ?? [];
      toast(`カメラ ${result.camera.name}（${result.camera.id}）を追加しました。\nOBS: ${result.srt_url}\n${reconcileMessage(result)}`, { error: errs.length > 0, ms: 9000 });
    });
  } catch (e) {
    $("addError").textContent = e.message;
    $("addError").hidden = false;
  } finally {
    submit.disabled = false;
    submit.textContent = "追加";
  }
});

let renameId = null;
function openRename(id) {
  const cam = findCamera(id);
  renameId = id;
  $("renameTarget").textContent = `カメラID: ${id}`;
  $("renameName").value = cam?.name ?? "";
  $("renameError").hidden = true;
  $("renameDialog").showModal();
  $("renameName").select();
}

$("renameForm").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  try {
    await runBusy(async () => {
      await api("PATCH", `/api/v1/admin/cameras/${encodeURIComponent(renameId)}`, { name: $("renameName").value.trim() });
      $("renameDialog").close();
      toast("表示名を変更しました。");
    });
  } catch (e) {
    $("renameError").textContent = e.message;
    $("renameError").hidden = false;
  }
});

async function toggleRecording(id) {
  const cam = findCamera(id);
  const enable = !cam?.recording;
  const ok = await confirmAction(
    enable ? "録画を開始" : "録画を停止",
    `${cam?.name ?? id}（${id}）の録画を${enable ? "有効" : "無効"}にします。\n` +
      `設定を反映するため、このカメラのコンテナを作り直します。送信中の映像とOBSの受信は数秒途切れ、自動で再接続します。\n` +
      `保存先: ${state.overview?.recording_dir ?? ""}`,
    enable ? "録画する" : "録画を止める",
  );
  if (!ok) return;
  state.cards.get(id)?.player.stop();
  try {
    await runBusy(async () => {
      const result = await api("POST", `/api/v1/admin/cameras/${encodeURIComponent(id)}/record`, { enabled: enable });
      toast(`カメラ ${id} の録画を${enable ? "有効" : "無効"}にしました。\n${reconcileMessage(result)}`, { error: (result.reconcile?.errors ?? []).length > 0 });
    });
  } catch (e) {
    toast(`録画の設定を変更できませんでした: ${e.message}`, { error: true });
  }
}

async function deleteCamera(id) {
  const cam = findCamera(id);
  const ok = await confirmAction(
    "カメラを削除",
    `${cam?.name ?? id}（${id}）を削除します。\n` +
      `コンテナ ${cam?.container?.name ?? ""} も削除され、送信中の映像とOBSの受信は停止します。\n` +
      "AMoRe.yaml からも削除されます（直前の内容は AMoRe.yaml.bak に残ります）。録画済みのファイルは削除しません。",
    "削除",
  );
  if (!ok) return;
  state.cards.get(id)?.player.stop();
  try {
    await runBusy(async () => {
      const result = await api("DELETE", `/api/v1/admin/cameras/${encodeURIComponent(id)}`);
      toast(`カメラ ${id} を削除しました。\n${reconcileMessage(result)}`, { error: (result.reconcile?.errors ?? []).length > 0 });
    });
  } catch (e) {
    toast(`削除できませんでした: ${e.message}`, { error: true });
  }
}

async function restartCamera(id) {
  const cam = findCamera(id);
  const troubled = !!cam?.health.problem || cam?.container?.state !== "running";
  const ok = await confirmAction(
    troubled ? "コンテナを作り直す" : "コンテナを再起動",
    `${cam?.name ?? id}（${id}）のコンテナを${troubled ? "作り直します（自動復旧の記録も消します）" : "再起動します"}。\n送信中の映像とOBSの受信は一時的に切断され、自動で再接続します。`,
    troubled ? "作り直す" : "再起動",
  );
  if (!ok) return;
  state.cards.get(id)?.player.stop();
  try {
    await runBusy(async () => {
      const result = await api("POST", `/api/v1/admin/cameras/${encodeURIComponent(id)}/restart`);
      toast(`カメラ ${id} のコンテナを${result.recreated ? "作り直しました" : "再起動しました"}。`);
    });
  } catch (e) {
    toast(`再起動できませんでした: ${e.message}`, { error: true });
  }
}

let logsId = null;
async function loadLogs() {
  const body = $("logsBody");
  body.textContent = "読み込み中…";
  try {
    const res = await fetch(`/api/v1/admin/cameras/${encodeURIComponent(logsId)}/logs`);
    const text = await res.text();
    if (!res.ok) {
      let msg = text;
      try { msg = JSON.parse(text).error; } catch { /* そのまま */ }
      throw new Error(msg);
    }
    body.textContent = text || "（ログはありません）";
    body.scrollTop = body.scrollHeight;
  } catch (e) {
    body.textContent = `ログを取得できません: ${e.message}`;
  }
}

function openLogs(id) {
  logsId = id;
  $("logsTitle").textContent = `ログ：${id}（直近300行）`;
  $("logsDialog").showModal();
  loadLogs();
}

$("logsReload").addEventListener("click", loadLogs);

let aiId = null;
async function runDiagnosis() {
  const body = $("aiBody");
  const run = $("aiRun");
  run.disabled = true;
  $("aiMeta").textContent = "AIに状態とログを送って診断しています（数十秒かかることがあります）…";
  body.replaceChildren();
  try {
    const d = await api("POST", `/api/v1/admin/cameras/${encodeURIComponent(aiId)}/diagnose`);
    $("aiMeta").textContent = `${new Date(d.at_ms).toLocaleString("ja-JP")}　モデル: ${d.model}　※AIの回答です。内容を確認してから操作してください。`;
    renderMarkdown(d.text, body);
  } catch (e) {
    $("aiMeta").textContent = "";
    body.textContent = `診断できませんでした: ${e.message}`;
  } finally {
    run.disabled = false;
  }
}

function openDiagnosis(id) {
  aiId = id;
  $("aiTitle").textContent = `AIによる診断：${findCamera(id)?.name ?? id}（${id}）`;
  $("aiDialog").showModal();
  runDiagnosis();
}

$("aiRun").addEventListener("click", runDiagnosis);

ui.reconcileBtn.addEventListener("click", async () => {
  try {
    await runBusy(async () => {
      const result = await api("POST", "/api/v1/admin/reconcile");
      toast(reconcileMessage(result), { error: (result.errors ?? []).length > 0 });
    });
  } catch (e) {
    toast(`再適用できませんでした: ${e.message}`, { error: true });
  }
});

window.addEventListener("pagehide", () => {
  for (const { player } of state.cards.values()) player.stop({ silent: true });
});

// ---------- 起動 ----------

async function init() {
  try {
    const cfg = await api("GET", "/api/v1/config");
    state.iceServers = (cfg.ice_servers ?? []).map((s) => ({ urls: s.urls, username: s.username, credential: s.credential }));
  } catch {
    // ICEサーバー設定が取れなくてもLAN内の映像確認はできる。
  }
  await refresh();
  setInterval(() => { if (!state.busy) refresh(); }, POLL_MS);
}

init();
