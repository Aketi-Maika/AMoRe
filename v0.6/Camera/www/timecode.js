// AMoRe v0.6 - 遅延計測の時刻コード（送信画面と OBS ドックで共用。ドックは /timecode.js を読み込む）
//
// 送信画面は、送る映像の各フレームの下端（高さの 1/120）に、Server の時計で見た現在時刻を白黒の帯として描く。
// OBS ドックは、OBS のメディアソースの映像を obs-websocket で取り込み、この帯を読んで
// 「OBS に表示された時刻 − 送信画面が描いた時刻」= カメラPC → OBS の遅延を求める。
// カメラPC・編集PC の時計は Server との往復で補正する（ServerClock）ため、PC の時刻合わせは不要。
//
// 帯は横に 48 マス：開始の印 [白, 黒]・時刻（ミリ秒の下位 36bit、上位ビットから）・CRC-8・終了の印 [黒, 白]。
// 1マスが十分に大きい（1920px で 40px）ため、圧縮・縮小・1080p ⇔ 720p の切替があっても読み取れる。

/** 帯の高さ（フレームの高さに対する比）。1080p で 9px、720p で 6px。 */
export const STRIP_RATIO = 1 / 120;
/** OBS で隠すために推奨する下端のクロップ（1080p の映像で px）。 */
export const RECOMMENDED_CROP_PX = 12;

const CELLS = 48;
const DATA_BITS = 36;
const CRC_BITS = 8;
const MODULO = 2 ** DATA_BITS;

/** 現在時刻（この PC の時計、ミリ秒）。 */
function localNow() {
  return performance.timeOrigin + performance.now();
}

/** CRC-8（多項式 0x07）。値を 5 バイト（上位から）として計算する。 */
function crc8(value) {
  let crc = 0;
  for (let i = 4; i >= 0; i--) {
    crc ^= Math.floor(value / 2 ** (8 * i)) % 256;
    for (let b = 0; b < 8; b++) {
      crc = crc & 0x80 ? ((crc << 1) ^ 0x07) & 0xff : (crc << 1) & 0xff;
    }
  }
  return crc;
}

/** 時刻（ミリ秒）を 48 マスの白黒（true = 白）にする。 */
export function encodeCells(ms) {
  const value = Math.floor(ms) % MODULO;
  const cells = [true, false];
  for (let i = DATA_BITS - 1; i >= 0; i--) cells.push(Math.floor(value / 2 ** i) % 2 === 1);
  const crc = crc8(value);
  for (let i = CRC_BITS - 1; i >= 0; i--) cells.push(((crc >> i) & 1) === 1);
  cells.push(false, true);
  return cells;
}

/** 48 マスの白黒から時刻（ミリ秒の下位 36bit）を取り出す。印・CRC が合わなければ null。 */
export function decodeCells(cells) {
  if (cells.length !== CELLS) return null;
  if (!cells[0] || cells[1] || cells[CELLS - 2] || !cells[CELLS - 1]) return null;
  let value = 0;
  for (let i = 0; i < DATA_BITS; i++) value = value * 2 + (cells[2 + i] ? 1 : 0);
  let crc = 0;
  for (let i = 0; i < CRC_BITS; i++) crc = crc * 2 + (cells[2 + DATA_BITS + i] ? 1 : 0);
  return crc === crc8(value) ? value : null;
}

/** フレームの下端に時刻コードを描く（ctx はフレームと同じ大きさのキャンバス）。 */
export function drawTimecode(ctx, width, height, ms) {
  const strip = Math.max(4, Math.round(height * STRIP_RATIO));
  const top = height - strip;
  const cells = encodeCells(ms);
  for (let i = 0; i < CELLS; i++) {
    const x0 = Math.round((i * width) / CELLS);
    const x1 = Math.round(((i + 1) * width) / CELLS);
    ctx.fillStyle = cells[i] ? "#ffffff" : "#000000";
    ctx.fillRect(x0, top, x1 - x0, strip);
  }
}

/**
 * 画像（ImageData。フレーム全体を縮小したものでよい）の下端の時刻コードを読む。
 * @returns {number|null} 時刻（ミリ秒の下位 36bit）。コードが無い・読めない場合は null。
 */
export function decodeTimecode(image) {
  const { width, height, data } = image;
  const strip = height * STRIP_RATIO;
  // 帯の中央付近の行だけを使う（上の映像・圧縮のにじみを避ける）。
  const y0 = Math.min(height - 1, Math.floor(height - strip * 0.7));
  const y1 = Math.min(height - 1, Math.max(y0, Math.floor(height - strip * 0.3)));
  const luma = [];
  for (let i = 0; i < CELLS; i++) {
    const x0 = Math.floor(((i + 0.25) * width) / CELLS);
    const x1 = Math.max(x0 + 1, Math.floor(((i + 0.75) * width) / CELLS));
    let sum = 0;
    let n = 0;
    for (let y = y0; y <= y1; y++) {
      for (let x = x0; x < x1; x++) {
        const p = (y * width + x) * 4;
        sum += 0.299 * data[p] + 0.587 * data[p + 1] + 0.114 * data[p + 2];
        n++;
      }
    }
    luma.push(sum / n);
  }
  // 開始の印（白・黒）の明るさの中間を境目にする。明暗の差が小さければコードは無いとみなす。
  if (luma[0] - luma[1] < 60) return null;
  const threshold = (luma[0] + luma[1]) / 2;
  return decodeCells(luma.map((v) => v > threshold));
}

/**
 * 遅延（ミリ秒）= now − 時刻コードの時刻。時刻コードは下位 36bit だけなので、差を ±2^35 の範囲に直す。
 * @param {number} nowMs 読み取った時点の Server の時計（ミリ秒）
 * @param {number} code decodeTimecode の結果
 */
export function latencyMs(nowMs, code) {
  let diff = (Math.floor(nowMs) % MODULO) - code;
  if (diff >= MODULO / 2) diff -= MODULO;
  if (diff < -MODULO / 2) diff += MODULO;
  return diff;
}

/** Server の時計（GET /api/v1/time）との差を、往復時間が最も短い応答から求める。 */
export class ServerClock {
  constructor(url = "/api/v1/time") {
    this.url = url;
    this.offset = 0;
    this.rtt = null;
    this.synced = false;
    this.timer = null;
  }

  /** Server の時計で見た現在時刻（ミリ秒）。 */
  now() {
    return localNow() + this.offset;
  }

  /** 時計の補正の誤差の目安（ミリ秒。往復時間の半分）。 */
  uncertaintyMs() {
    return this.rtt == null ? null : this.rtt / 2;
  }

  async sync(samples = 5) {
    let best = null;
    for (let i = 0; i < samples; i++) {
      const t0 = localNow();
      let body;
      try {
        const res = await fetch(this.url, { cache: "no-store" });
        if (!res.ok) continue;
        body = await res.json();
      } catch {
        continue;
      }
      const t1 = localNow();
      if (typeof body?.now_ms !== "number") continue;
      const rtt = t1 - t0;
      if (!best || rtt < best.rtt) best = { rtt, offset: body.now_ms - (t0 + t1) / 2 };
    }
    if (best) {
      this.offset = best.offset;
      this.rtt = best.rtt;
      this.synced = true;
    }
    return this.synced;
  }

  /** 定期的に合わせ直す（PC の時計のずれ・スリープ復帰に追従する）。 */
  start(intervalMs = 30000) {
    this.stop();
    this.sync();
    this.timer = setInterval(() => this.sync(), intervalMs);
  }

  stop() {
    clearInterval(this.timer);
    this.timer = null;
  }
}

/** このブラウザで映像のフレームを加工できるか（Chrome・Edge）。 */
export function timecodeSupported() {
  return (
    typeof MediaStreamTrackProcessor === "function" &&
    typeof MediaStreamTrackGenerator === "function" &&
    typeof OffscreenCanvas === "function" &&
    typeof VideoFrame === "function"
  );
}

/**
 * カメラの映像トラックから、時刻コードを描いた映像トラックを作る。
 * 元のトラックが終わると、作ったトラックも終わる。
 * @returns {{ track: MediaStreamTrack, stop: () => void }}
 */
export function withTimecode(track, clock) {
  const processor = new MediaStreamTrackProcessor({ track });
  const generator = new MediaStreamTrackGenerator({ kind: "video" });
  generator.contentHint = track.contentHint || "motion";
  let canvas = null;
  let ctx = null;
  const transformer = new TransformStream({
    transform(frame, controller) {
      try {
        const w = frame.displayWidth;
        const h = frame.displayHeight;
        if (!canvas || canvas.width !== w || canvas.height !== h) {
          canvas = new OffscreenCanvas(w, h);
          ctx = canvas.getContext("2d", { alpha: false });
        }
        ctx.drawImage(frame, 0, 0, w, h);
        drawTimecode(ctx, w, h, clock.now());
        const init = { timestamp: frame.timestamp };
        if (frame.duration != null) init.duration = frame.duration;
        const out = new VideoFrame(canvas, init);
        frame.close();
        controller.enqueue(out);
      } catch {
        // 加工できないフレームはそのまま送る（映像を止めない）。
        controller.enqueue(frame);
      }
    },
  });
  const abort = new AbortController();
  processor.readable
    .pipeThrough(transformer, { signal: abort.signal })
    .pipeTo(generator.writable, { signal: abort.signal })
    .catch(() => {});
  return {
    track: generator,
    stop() {
      abort.abort();
      generator.stop();
    },
  };
}
