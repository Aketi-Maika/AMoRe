// AMoRe v0.6 - カメラPC → OBS の遅延計測（OBS ドック）
//
// OBS のメディアソースのうち、入力が AMoRe の SRT URL（streamid=read:<カメラID>）のものを探し、
// obs-websocket の GetSourceScreenshot で今の映像を取り込む。送信画面が映像の下端に描いた時刻コードを読み、
// 「取り込んだ時刻 − 時刻コードの時刻」を遅延として Server（POST /api/v1/latency）へ送る。
// 両方の時刻は Server の時計に補正している（timecode.js の ServerClock）。

import { ServerClock, decodeTimecode, latencyMs } from "/timecode.js";
import { ObsClient } from "/obs/obsws.js";
import { auth } from "/login.js";

/** メディアソースとカメラの対応を取り直す間隔 */
const MAP_REFRESH_MS = 30000;
/** 取り込む映像の幅（px）。時刻コードの1マスが 20px になる。 */
const SHOT_WIDTH = 960;

const DEFAULTS = { url: "ws://127.0.0.1:4455", password: "", name: "編集PC", enabled: true };

// 設定はこの OBS（ブラウザドック）だけに保存する。使えなくても既定値で動く。
function loadSettings() {
  try {
    const saved = JSON.parse(localStorage.getItem("amore.latency") ?? "{}");
    return { ...DEFAULTS, ...saved };
  } catch {
    return { ...DEFAULTS };
  }
}

function saveSettings(settings) {
  try {
    localStorage.setItem("amore.latency", JSON.stringify(settings));
  } catch {
    /* 無視 */
  }
}

/** 取り込んだ画像（data URL）の時刻コードを読む。 */
async function decodeShot(dataUrl) {
  const blob = await (await fetch(dataUrl)).blob();
  const bitmap = await createImageBitmap(blob);
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  ctx.drawImage(bitmap, 0, 0);
  bitmap.close();
  return decodeTimecode(ctx.getImageData(0, 0, canvas.width, canvas.height));
}

export class LatencyProbe {
  /** @param {() => void} onUpdate 結果・状態が変わったときに呼ぶ */
  constructor(onUpdate) {
    this.onUpdate = onUpdate;
    this.settings = loadSettings();
    this.clock = new ServerClock();
    this.client = null;
    /** カメラID → OBS の入力（メディアソース）名 */
    this.inputs = new Map();
    this.mappedAt = 0;
    /** カメラID → { ms } または { error } */
    this.results = new Map();
    this.status = { ok: false, text: "確認中…" };
    this.cameras = [];
    this.intervalSec = null;
    this.timer = null;
    this.busy = false;
  }

  /**
   * Server の状態（/api/v1/status）を受け取る。
   * @param {number|null} intervalSec 計測の間隔（monitoring.latency が無効なら null）
   * @param {string[]} cameras OBS 向けの映像を出しているカメラID
   */
  update(intervalSec, cameras) {
    this.cameras = cameras;
    for (const id of this.results.keys()) {
      if (!cameras.includes(id)) this.results.delete(id);
    }
    if (intervalSec === this.intervalSec) return;
    this.intervalSec = intervalSec;
    clearInterval(this.timer);
    this.timer = null;
    if (!intervalSec) {
      this.clock.stop();
      this.disconnect();
      this.setStatus(false, "Server の設定で無効（monitoring.latency）");
      return;
    }
    this.clock.start();
    this.timer = setInterval(() => this.run(), intervalSec * 1000);
    this.run();
  }

  /** ドックの設定欄から呼ぶ。 */
  configure(settings) {
    this.settings = { ...this.settings, ...settings };
    saveSettings(this.settings);
    this.disconnect();
    this.results.clear();
    this.setStatus(false, this.settings.enabled ? "接続し直しています…" : "計測しない設定");
    this.run();
  }

  setStatus(ok, text) {
    this.status = { ok, text };
    this.onUpdate();
  }

  disconnect() {
    this.client?.close();
    this.client = null;
    this.inputs.clear();
    this.mappedAt = 0;
  }

  async ensureConnected() {
    if (this.client?.ready) return;
    this.disconnect();
    const client = new ObsClient(this.settings.url, this.settings.password);
    await client.connect();
    this.client = client;
  }

  /** メディアソースのうち、入力が AMoRe の SRT URL のものをカメラIDと対応付ける。 */
  async mapInputs() {
    const { inputs = [] } = await this.client.request("GetInputList");
    const map = new Map();
    for (const input of inputs) {
      if (!String(input.inputKind ?? "").startsWith("ffmpeg_source")) continue;
      const { inputSettings } = await this.client.request("GetInputSettings", { inputName: input.inputName });
      const m = /[?&]streamid=read:([^&]+)/.exec(inputSettings?.input ?? "");
      if (!m) continue;
      const id = decodeURIComponent(m[1]);
      if (!map.has(id)) map.set(id, input.inputName);
    }
    this.inputs = map;
    this.mappedAt = Date.now();
  }

  async measure(cameraId) {
    const inputName = this.inputs.get(cameraId);
    if (!inputName) {
      return { error: "OBS にこのカメラのメディアソースがありません" };
    }
    // OBS はこの要求を受けた後のフレームを取り込むため、送る直前の時刻を表示の時刻とみなす。
    const shownAt = this.clock.now();
    const shot = await this.client.request("GetSourceScreenshot", {
      sourceName: inputName,
      imageFormat: "jpg",
      imageWidth: SHOT_WIDTH,
      imageCompressionQuality: 90,
    });
    const code = await decodeShot(shot.imageData);
    if (code == null) {
      return { error: "時刻コードが読めません（送信画面を v0.6 の Chrome / Edge で開いているか確認）" };
    }
    return { ms: latencyMs(shownAt, code), inputName };
  }

  async run() {
    if (this.busy || !this.intervalSec) return;
    if (!this.settings.enabled) {
      this.setStatus(false, "計測しない設定");
      return;
    }
    if (!this.clock.synced) {
      this.setStatus(false, "Server の時計を確認中…");
      return;
    }
    this.busy = true;
    try {
      await this.ensureConnected();
      if (Date.now() - this.mappedAt > MAP_REFRESH_MS) await this.mapInputs();
      const samples = [];
      for (const id of this.cameras) {
        let result;
        try {
          result = await this.measure(id);
        } catch (e) {
          result = { error: e.message };
        }
        this.results.set(id, result);
        if (result.ms != null) samples.push({ camera: id, latency_ms: result.ms });
      }
      if (samples.length > 0) {
        await auth.fetch("/api/v1/latency", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ obs: this.settings.name, samples }),
        });
      }
      const u = this.clock.uncertaintyMs();
      this.setStatus(true, `計測中（${this.intervalSec}秒ごと・時計の補正 ±${Math.ceil(u)} ms）`);
    } catch (e) {
      this.disconnect();
      this.setStatus(false, e.message);
    } finally {
      this.busy = false;
    }
  }
}
