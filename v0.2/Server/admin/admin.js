// AMoRe v0.2 - 管理Web
// カメラの追加・削除・名前変更、カメラごとのコンテナ操作、映像確認（WHEP）を行う。

const POLL_MS = 3000;

const $ = (id) => document.getElementById(id);
const ui = {
  dockerBadge: $("dockerBadge"),
  senderLink: $("senderLink"),
  dockLink: $("dockLink"),
  cameraCount: $("cameraCount"),
  reconcileInfo: $("reconcileInfo"),
  reconcileBtn: $("reconcileBtn"),
  addBtn: $("addBtn"),
  alerts: $("alerts"),
  cameras: $("cameras"),
  empty: $("empty"),
  toast: $("toast"),
  template: $("cardTemplate"),
};

const state = {
  overview: null,
  iceServers: [],
  cards: new Map(), // id -> { el, preview }
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

// ---------- 映像確認（WHEP） ----------

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

class Preview {
  constructor(cameraId, card) {
    this.cameraId = cameraId;
    this.card = card;
    this.pc = null;
    this.resource = null;
    this.infoTimer = null;
  }

  get video() { return this.card.querySelector(".video"); }

  setMessage(text, error = false) {
    const el = this.card.querySelector(".preview-msg");
    el.textContent = text;
    el.classList.toggle("err", error);
  }

  get active() { return this.pc !== null; }

  async start() {
    const btn = this.card.querySelector(".preview-btn");
    btn.textContent = "停止";
    this.setMessage("接続中…");
    const pc = new RTCPeerConnection({ iceServers: state.iceServers, bundlePolicy: "max-bundle" });
    this.pc = pc;
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
    const stream = new MediaStream();
    pc.addEventListener("track", (ev) => {
      stream.addTrack(ev.track);
      this.video.srcObject = stream;
      this.video.play().catch(() => {});
      this.card.querySelector(".placeholder").hidden = true;
      if (ev.track.kind === "audio") this.card.querySelector(".mute-btn").hidden = false;
    });
    pc.addEventListener("connectionstatechange", () => {
      if (pc !== this.pc) return;
      if (pc.connectionState === "connected") this.setMessage("表示中");
      if (pc.connectionState === "failed") {
        this.stop();
        this.setMessage("接続が切断されました（WebRTCのUDPポートに到達できるか確認してください）", true);
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
        throw new Error(res.status === 404 || /no stream|not available|no one is publishing/i.test(text)
          ? "このカメラは送信されていないため表示できません"
          : `映像確認を開始できません（HTTP ${res.status} ${text.slice(0, 200)}）`);
      }
      const location = res.headers.get("Location");
      this.resource = location ? new URL(location, window.location.href).href : null;
      await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
      this.infoTimer = setInterval(() => this.updateInfo(), 1000);
    } catch (e) {
      this.stop();
      this.setMessage(e.message, true);
    }
  }

  updateInfo() {
    const info = this.card.querySelector(".video-info");
    const v = this.video;
    if (v.videoWidth) {
      info.textContent = `${v.videoWidth}×${v.videoHeight}`;
      info.hidden = false;
    }
  }

  stop() {
    clearInterval(this.infoTimer);
    if (this.resource) {
      fetch(this.resource, { method: "DELETE", keepalive: true }).catch(() => {});
      this.resource = null;
    }
    if (this.pc) {
      const pc = this.pc;
      this.pc = null;
      pc.close();
    }
    const v = this.video;
    v.srcObject = null;
    v.muted = true;
    this.card.querySelector(".placeholder").hidden = false;
    this.card.querySelector(".video-info").hidden = true;
    const mute = this.card.querySelector(".mute-btn");
    mute.hidden = true;
    mute.textContent = "音声をオン";
    this.card.querySelector(".preview-btn").textContent = "映像確認";
    this.setMessage("");
  }

  toggleMute() {
    const v = this.video;
    v.muted = !v.muted;
    this.card.querySelector(".mute-btn").textContent = v.muted ? "音声をオン" : "音声をオフ";
  }
}

// ---------- カード ----------

function createCard(id) {
  const el = ui.template.content.firstElementChild.cloneNode(true);
  el.dataset.id = id;
  const preview = new Preview(id, el);

  el.querySelector(".preview-btn").addEventListener("click", () => {
    if (preview.active) preview.stop();
    else preview.start();
  });
  el.querySelector(".mute-btn").addEventListener("click", () => preview.toggleMute());
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

  ui.cameras.append(el);
  const entry = { el, preview };
  state.cards.set(id, entry);
  return entry;
}

function setDd(el, text, cls = "") {
  el.textContent = text;
  el.className = el.className.split(" ").filter((c) => !["ok", "warn", "err"].includes(c)).join(" ");
  if (cls) el.classList.add(cls);
}

function renderCard(cam, entry) {
  const { el } = entry;
  el.querySelector(".name").textContent = cam.name;
  el.querySelector(".cid").textContent = `${cam.id}　/　${cam.container?.name ?? "コンテナ未作成"}`;

  const badge = el.querySelector(".state");
  const [label, cls] = cam.container ? (CONTAINER_STATE[cam.container.state] ?? [cam.container.state, "warn"]) : ["未作成", "err"];
  badge.textContent = label;
  badge.className = `badge state ${cls}`;

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
  setDd(el.querySelector(".obs"), `${cam.srt_readers} 接続${cam.preview_viewers ? `（映像確認 ${cam.preview_viewers}）` : ""}`, cam.srt_readers ? "ok" : "");
  setDd(el.querySelector(".ports"), `WebRTC ${cam.webrtc_udp_port}/udp　SRT ${cam.srt_port}/udp`);
  setDd(el.querySelector(".cstatus"), cam.container ? cam.container.status : "未作成（「再起動」または「再適用」で作成）", cam.container ? "" : "err");

  const srt = el.querySelector(".srt");
  if (document.activeElement !== srt) srt.value = cam.srt_url;
  el.querySelector(".sender-link").href = `/?camera=${encodeURIComponent(cam.id)}`;
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
  ui.alerts.replaceChildren(...alerts.map(([cls, text]) => {
    const div = document.createElement("div");
    div.className = `alert ${cls}`;
    div.textContent = text;
    return div;
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
  ui.cameraCount.textContent = `カメラ ${ov.cameras.length} / ${ov.max_cameras} 台`;
  if (rt.last_reconcile_at) {
    const time = new Date(rt.last_reconcile_at * 1000).toLocaleTimeString("ja-JP");
    ui.reconcileInfo.textContent = `最終確認 ${time}：${rt.last_reconcile}`;
  }
  ui.addBtn.disabled = state.busy || !ov.next_camera_id;
  ui.addBtn.title = ov.next_camera_id ? "" : "カメラ数の上限に達しています";
  renderAlerts(ov);

  const ids = new Set(ov.cameras.map((c) => c.id));
  for (const [id, entry] of state.cards) {
    if (!ids.has(id)) {
      entry.preview.stop();
      entry.el.remove();
      state.cards.delete(id);
    }
  }
  for (const cam of ov.cameras) {
    const entry = state.cards.get(cam.id) ?? createCard(cam.id);
    renderCard(cam, entry);
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
  const cam = state.overview?.cameras.find((c) => c.id === id);
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

async function deleteCamera(id) {
  const cam = state.overview?.cameras.find((c) => c.id === id);
  const ok = await confirmAction(
    "カメラを削除",
    `${cam?.name ?? id}（${id}）を削除します。\n` +
      `コンテナ ${cam?.container?.name ?? ""} も削除され、送信中の映像とOBSの受信は停止します。\n` +
      "AMoRe.yaml からも削除されます（直前の内容は AMoRe.yaml.bak に残ります）。",
    "削除",
  );
  if (!ok) return;
  state.cards.get(id)?.preview.stop();
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
  const cam = state.overview?.cameras.find((c) => c.id === id);
  const ok = await confirmAction(
    "コンテナを再起動",
    `${cam?.name ?? id}（${id}）のコンテナを再起動します。\n送信中の映像とOBSの受信は一時的に切断され、自動で再接続します。`,
    "再起動",
  );
  if (!ok) return;
  state.cards.get(id)?.preview.stop();
  try {
    await runBusy(async () => {
      await api("POST", `/api/v1/admin/cameras/${encodeURIComponent(id)}/restart`);
      toast(`カメラ ${id} のコンテナを再起動しました。`);
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
  for (const { preview } of state.cards.values()) preview.stop();
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
