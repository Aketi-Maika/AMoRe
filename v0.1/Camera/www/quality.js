// AMoRe v0.1 - 帯域不足時の品質調整
// WebRTC の推定利用可能帯域（availableOutgoingBitrate）から 1080p ⇔ 720p を切り替える。
// 閾値・待機時間は AMoRe.yaml の quality_control（要件定義書 v2 の暫定値）に従う。

/** 品質レベル: standard = 1080p（標準）, fallback = 720p（帯域不足時） */
export class QualityController {
  /**
   * @param {object} qc AMoRe.yaml の quality_control
   * @param {(level: string) => Promise<void>} apply 品質を送信設定へ反映する関数
   * @param {() => number} [now] 現在時刻（ミリ秒）
   */
  constructor(qc, apply, now = () => performance.now()) {
    this.qc = qc;
    this.apply = apply;
    this.now = now;
    this.mode = "auto";
    this.level = "standard";
    this.unsupported = false;
    this.lastError = null;
    this.reset();
  }

  reset() {
    this.belowSince = null;
    this.aboveSince = null;
    this.lastUpgradeAt = null;
    this.holdUntil = 0;
  }

  /** 手動操作。auto 以外は自動切替を止めて固定する。 */
  async setMode(mode) {
    this.mode = mode;
    this.belowSince = null;
    this.aboveSince = null;
    if (mode !== "auto") await this.switchTo(mode);
  }

  /**
   * 統計の取得ごとに呼ぶ。
   * @param {number|null} videoAvailableKbps 映像に使える推定帯域。取得できない場合は null。
   */
  async update(videoAvailableKbps) {
    if (this.mode !== "auto" || !this.qc.enabled) return;
    if (videoAvailableKbps == null || !Number.isFinite(videoAvailableKbps)) {
      this.unsupported = true;
      return;
    }
    this.unsupported = false;
    const t = this.now();
    const qc = this.qc;

    if (this.level === "standard") {
      if (videoAvailableKbps < qc.downgrade_below_kbps) {
        this.belowSince ??= t;
        if (t - this.belowSince >= qc.downgrade_after_sec * 1000) {
          // 復帰直後に再び低下した場合は、しばらく復帰を保留して切替の繰り返しを防ぐ。
          if (this.lastUpgradeAt != null && t - this.lastUpgradeAt <= qc.relapse_window_sec * 1000) {
            this.holdUntil = t + qc.hold_after_relapse_sec * 1000;
          }
          await this.switchTo("fallback");
        }
      } else {
        this.belowSince = null;
      }
    } else if (videoAvailableKbps >= qc.upgrade_above_kbps && t >= this.holdUntil) {
      this.aboveSince ??= t;
      if (t - this.aboveSince >= qc.upgrade_after_sec * 1000) {
        this.lastUpgradeAt = t;
        await this.switchTo("standard");
      }
    } else {
      this.aboveSince = null;
    }
  }

  async switchTo(level) {
    this.belowSince = null;
    this.aboveSince = null;
    try {
      await this.apply(level);
      this.level = level === "standard_max" ? "standard" : level;
      this.lastError = null;
    } catch (e) {
      this.lastError = `解像度・ビットレートの変更に失敗しました（${e.message}）`;
    }
  }

  /** 復帰保留の残り秒数 */
  holdRemainingSec() {
    return Math.max(0, Math.ceil((this.holdUntil - this.now()) / 1000));
  }
}
