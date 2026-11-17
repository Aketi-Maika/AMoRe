//! 映像品質・遅延の集計（v0.6。`monitoring.yaml` の `quality`・`latency`）。
//!
//! - `GET  /api/v1/time`                 Server の時計（送信画面・OBS ドックが自分の時計との差を補正する）
//! - `POST /api/v1/cameras/{id}/stats`   送信画面からの映像の統計（解像度・fps・ビットレート・損失・RTT）
//! - `POST /api/v1/latency`              OBS ドックが測ったカメラPC → OBS の遅延
//!
//! 受け取った値はメモリにだけ保持し、`/metrics`（Prometheus → Grafana）と状態API・OBS ドックで使う。
//! 古くなった値（送信停止・OBS 終了）は表示・メトリクスから外す。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, SharedState};

/// 送信画面の統計を有効とみなす時間。
const QUALITY_FRESH: Duration = Duration::from_secs(15);
/// 遅延の測定値を有効とみなす時間。
const LATENCY_FRESH: Duration = Duration::from_secs(30);
/// 遅延の平均・最大を求める期間。
const LATENCY_WINDOW: Duration = Duration::from_secs(60);
/// 受け付ける遅延の範囲（ミリ秒）。外れる値は時刻コードの読み違いとみなす。
/// 時計の補正の誤差で少し負になることがあるため、-1000 ms までは 0 として受け付ける。
const LATENCY_MIN_MS: f64 = -1000.0;
const LATENCY_MAX_MS: f64 = 60_000.0;
/// 文字列の項目の長さの上限（メトリクスのラベルに使うため）。
const TEXT_MAX: usize = 40;

/// 送信画面から届く映像の統計（送信画面の WebRTC の getStats から作る。取得できない項目は null）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityStats {
    /// 例: H264 / VP8
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_kbps: Option<f64>,
    pub audio_kbps: Option<f64>,
    /// 推定利用可能帯域（kbps）
    pub available_kbps: Option<f64>,
    /// カメラPC ⇔ Server の往復時間（ミリ秒）
    pub rtt_ms: Option<f64>,
    /// 映像のパケット損失率（%。Server からの受信報告による）
    pub loss_percent: Option<f64>,
    /// 失われた映像のパケット（累計）
    pub packets_lost: Option<f64>,
    /// 再送要求（累計）
    pub nack_count: Option<f64>,
    /// 再送したパケット（累計）
    pub retransmitted_packets: Option<f64>,
    /// ジッター（ミリ秒）
    pub jitter_ms: Option<f64>,
    /// standard（1080p）/ fallback（720p）
    pub quality_level: String,
    /// 品質低下の理由（none / cpu / bandwidth / other）
    pub quality_limitation: String,
    /// 遅延計測の時刻コードを付けて送っているか。
    pub timecode: bool,
}

fn clip(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(TEXT_MAX)
        .collect()
}

impl QualityStats {
    /// メトリクスのラベルに使う文字列を整える。
    fn sanitized(mut self) -> Self {
        self.codec = clip(&self.codec);
        self.quality_level = clip(&self.quality_level);
        self.quality_limitation = clip(&self.quality_limitation);
        self
    }
}

/// カメラ・OBS ごとの遅延の要約。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencySummary {
    pub camera: String,
    /// 測った OBS（OBS ドックで設定した名前）
    pub obs: String,
    /// 最新の測定値（ミリ秒）
    pub latest_ms: f64,
    /// 直近 60 秒の平均・最大（ミリ秒）
    pub avg_ms: f64,
    pub max_ms: f64,
}

/// （カメラID, OBS の名前）→ 直近の遅延の測定値（測った時刻, ミリ秒）。
type LatencyLog = HashMap<(String, String), VecDeque<(Instant, f64)>>;

/// 受け取った統計・遅延（メモリ上）。
#[derive(Default)]
pub struct TelemetryState {
    quality: Mutex<HashMap<String, (Instant, QualityStats)>>,
    latency: Mutex<LatencyLog>,
}

impl TelemetryState {
    pub fn record_quality(&self, camera: &str, stats: QualityStats, now: Instant) {
        self.quality
            .lock()
            .expect("telemetry lock")
            .insert(camera.to_string(), (now, stats.sanitized()));
    }

    /// 送信画面の最新の統計（古ければ None）。
    pub fn quality(&self, camera: &str, now: Instant) -> Option<QualityStats> {
        let map = self.quality.lock().expect("telemetry lock");
        let (at, stats) = map.get(camera)?;
        (now.duration_since(*at) <= QUALITY_FRESH).then(|| stats.clone())
    }

    pub fn record_latency(&self, camera: &str, obs: &str, ms: f64, now: Instant) {
        let mut map = self.latency.lock().expect("telemetry lock");
        let samples = map
            .entry((camera.to_string(), obs.to_string()))
            .or_default();
        samples.push_back((now, ms));
        while samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > LATENCY_WINDOW)
        {
            samples.pop_front();
        }
    }

    /// カメラ・OBS ごとの遅延（カメラ・OBS の名前順）。最新の測定が古いものは除き、記録からも消す。
    pub fn latency(&self, now: Instant) -> Vec<LatencySummary> {
        let mut map = self.latency.lock().expect("telemetry lock");
        map.retain(|_, samples| {
            samples
                .back()
                .is_some_and(|(t, _)| now.duration_since(*t) <= LATENCY_FRESH)
        });
        let mut out: Vec<LatencySummary> = map
            .iter()
            .filter_map(|((camera, obs), samples)| {
                let (_, latest) = samples.back()?;
                let recent: Vec<f64> = samples
                    .iter()
                    .filter(|(t, _)| now.duration_since(*t) <= LATENCY_WINDOW)
                    .map(|(_, v)| *v)
                    .collect();
                let max = recent.iter().copied().fold(*latest, f64::max);
                let avg = if recent.is_empty() {
                    *latest
                } else {
                    recent.iter().sum::<f64>() / recent.len() as f64
                };
                Some(LatencySummary {
                    camera: camera.clone(),
                    obs: obs.clone(),
                    latest_ms: *latest,
                    avg_ms: avg,
                    max_ms: max,
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.camera, &a.obs).cmp(&(&b.camera, &b.obs)));
        out
    }

    /// カメラの最新の遅延（複数の OBS で測っている場合は最大）。
    pub fn latest_latency(&self, camera: &str, now: Instant) -> Option<f64> {
        self.latency(now)
            .iter()
            .filter(|l| l.camera == camera)
            .map(|l| l.latest_ms)
            .reduce(f64::max)
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// OBS の名前（メトリクスのラベル）。空なら `obs`。
fn obs_label(name: &str) -> String {
    let name = clip(name.trim());
    if name.is_empty() {
        "obs".to_string()
    } else {
        name
    }
}

/// 受け付ける遅延の値（範囲外は None。わずかに負の値は 0 にする）。
fn accept_latency(ms: f64) -> Option<f64> {
    (LATENCY_MIN_MS..=LATENCY_MAX_MS)
        .contains(&ms)
        .then_some(ms.max(0.0))
}

#[derive(Serialize)]
pub struct ServerTime {
    /// UNIX 時刻（ミリ秒。小数を含む）
    now_ms: f64,
}

/// `GET /api/v1/time`
pub async fn time_handler() -> Json<ServerTime> {
    Json(ServerTime { now_ms: now_ms() })
}

/// `POST /api/v1/cameras/{id}/stats`（送信画面）
pub async fn camera_stats_handler(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(stats): Json<QualityStats>,
) -> Result<StatusCode, ApiError> {
    let cfg = state.store.get();
    if !cfg.monitoring.quality.enabled {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "映像の統計の受け付けは無効です（monitoring.quality）",
        ));
    }
    if cfg.camera(&id).is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("カメラ {id} は登録されていません"),
        ));
    }
    state.telemetry.record_quality(&id, stats, Instant::now());
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct LatencyReport {
    /// 測った OBS の名前（OBS ドックの設定）
    #[serde(default)]
    obs: String,
    samples: Vec<LatencySample>,
}

#[derive(Debug, Deserialize)]
pub struct LatencySample {
    camera: String,
    latency_ms: f64,
}

/// `POST /api/v1/latency`（OBS ドック）
pub async fn latency_handler(
    State(state): State<SharedState>,
    Json(report): Json<LatencyReport>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = state.store.get();
    if !cfg.monitoring.latency.enabled {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "遅延計測は無効です（monitoring.latency）",
        ));
    }
    let obs = obs_label(&report.obs);
    let now = Instant::now();
    let mut accepted = 0;
    for s in &report.samples {
        if cfg.camera(&s.camera).is_none() {
            continue;
        }
        if let Some(ms) = accept_latency(s.latency_ms) {
            state.telemetry.record_latency(&s.camera, &obs, ms, now);
            accepted += 1;
        }
    }
    Ok(Json(serde_json::json!({ "accepted": accepted })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_latest_quality_until_stale() {
        let t = TelemetryState::default();
        let now = Instant::now();
        let stats = QualityStats {
            codec: "H264\n".into(),
            height: Some(1080),
            fps: Some(29.9),
            ..Default::default()
        };
        t.record_quality("cam1", stats, now);
        let got = t.quality("cam1", now).unwrap();
        assert_eq!(got.codec, "H264", "制御文字は除く");
        assert_eq!(got.height, Some(1080));
        assert!(t.quality("cam2", now).is_none());
        assert!(
            t.quality("cam1", now + QUALITY_FRESH + Duration::from_secs(1))
                .is_none(),
            "古い統計は使わない"
        );
    }

    #[test]
    fn summarizes_latency_per_camera_and_obs() {
        let t = TelemetryState::default();
        let t0 = Instant::now();
        t.record_latency("cam1", "編集PC", 300.0, t0);
        t.record_latency("cam1", "編集PC", 500.0, t0 + Duration::from_secs(2));
        t.record_latency("cam1", "サブ", 250.0, t0 + Duration::from_secs(2));
        t.record_latency("cam2", "編集PC", 100.0, t0);
        let now = t0 + Duration::from_secs(3);
        let list = t.latency(now);
        assert_eq!(list.len(), 3);
        let main = list
            .iter()
            .find(|l| l.camera == "cam1" && l.obs == "編集PC")
            .unwrap();
        assert_eq!(main.latest_ms, 500.0);
        assert_eq!(main.avg_ms, 400.0);
        assert_eq!(main.max_ms, 500.0);
        assert_eq!(t.latest_latency("cam1", now), Some(500.0), "OBS の中で最大");
        assert_eq!(t.latest_latency("cam3", now), None);

        // 最新の測定が古くなったものは除く（cam2 は t0 のみ）。
        let later = t0 + LATENCY_FRESH + Duration::from_secs(1);
        let list = t.latency(later);
        assert!(list.iter().all(|l| l.camera == "cam1"), "{list:?}");
    }

    #[test]
    fn rejects_implausible_latency() {
        assert_eq!(accept_latency(320.0), Some(320.0));
        assert_eq!(accept_latency(-20.0), Some(0.0), "時計の補正の誤差");
        assert_eq!(accept_latency(-5000.0), None);
        assert_eq!(accept_latency(120_000.0), None);
        assert_eq!(obs_label("  "), "obs");
        assert_eq!(obs_label(" 編集PC "), "編集PC");
    }
}
