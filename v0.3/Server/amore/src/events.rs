//! 運用上の出来事（カメラの追加・削除・不調・復旧・録画切替・AIの異常検知）の記録と通知。
//!
//! - 直近の出来事を保持する（管理Webの表示・AIの障害診断に使う）
//! - Loki へログとして送る（`monitoring.loki`）
//! - Grafana へ注釈（アノテーション）として送る（`monitoring.grafana`）

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::api::AppState;
use crate::monitoring;

/// 保持する件数。
const KEEP: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// 例: `camera_added` / `problem` / `recovered` / `recording` / `ai_anomaly`
    pub kind: &'static str,
    /// `info` / `warning` / `error`
    pub level: &'static str,
    pub camera: Option<String>,
    pub text: String,
    /// UNIX時刻（ミリ秒）
    pub time_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Event {
    fn new(kind: &'static str, level: &'static str, camera: Option<&str>, text: String) -> Self {
        Self {
            kind,
            level,
            camera: camera.map(str::to_string),
            text,
            time_ms: now_ms(),
        }
    }

    pub fn camera_added(camera: &str, name: &str) -> Self {
        Self::new("camera_added", "info", Some(camera), format!("カメラ {name}（{camera}）を追加しました"))
    }

    pub fn camera_deleted(camera: &str) -> Self {
        Self::new("camera_deleted", "info", Some(camera), format!("カメラ {camera} を削除しました"))
    }

    pub fn problem(camera: &str, text: &str) -> Self {
        Self::new("problem", "error", Some(camera), format!("カメラ {camera} のコンテナが不調: {text}"))
    }

    pub fn recovered(camera: &str) -> Self {
        Self::new("recovered", "info", Some(camera), format!("カメラ {camera} のコンテナが復旧しました"))
    }

    pub fn recording(camera: &str, enabled: bool) -> Self {
        let text = if enabled {
            format!("カメラ {camera} の録画を有効にしました")
        } else {
            format!("カメラ {camera} の録画を無効にしました")
        };
        Self::new("recording", "info", Some(camera), text)
    }

    pub fn ai_anomaly(camera: &str, text: &str) -> Self {
        Self::new("ai_anomaly", "warning", Some(camera), format!("カメラ {camera} の映像に異常: {text}"))
    }
}

/// 出来事を記録し、設定に応じて Loki・Grafana へ送る（送信は非同期で、失敗しても処理は続ける）。
pub fn notify(state: &AppState, event: Event) {
    {
        let mut recent = state.events.lock().expect("events lock");
        if recent.len() >= KEEP {
            recent.pop_front();
        }
        recent.push_back(event.clone());
    }
    monitoring::loki::push_event(&event);
    monitoring::grafana::annotate(state, &event);
}

/// 直近の出来事（新しい順）。
pub fn recent(state: &AppState, camera: Option<&str>, limit: usize) -> Vec<Event> {
    state
        .events
        .lock()
        .expect("events lock")
        .iter()
        .rev()
        .filter(|e| camera.is_none() || e.camera.as_deref() == camera)
        .take(limit)
        .cloned()
        .collect()
}

pub type EventLog = std::sync::Mutex<VecDeque<Event>>;
