//! Grafana への注釈（アノテーション）送信（`monitoring.grafana`）。
//! カメラの追加・削除・不調・復旧・録画切替・AIの異常検知を、ダッシュボードのグラフ上に印として残す。

use serde_json::json;

use crate::api::AppState;
use crate::events::Event;

/// 送信内容（`POST /api/annotations` の本文）。
pub fn body(event: &Event, base_tags: &[String]) -> serde_json::Value {
    let mut tags: Vec<String> = base_tags.to_vec();
    tags.push(event.kind.to_string());
    if let Some(camera) = &event.camera {
        tags.push(camera.clone());
    }
    json!({
        "time": event.time_ms,
        "tags": tags,
        "text": event.text,
    })
}

/// 注釈を非同期で送る（失敗しても処理は続ける）。
pub fn annotate(state: &AppState, event: &Event) {
    let cfg = state.store.get();
    let grafana = cfg.monitoring.grafana.clone();
    if !grafana.enabled {
        return;
    }
    let http = state.http.clone();
    let secrets = state.secrets.clone();
    let payload = body(event, &grafana.tags);
    tokio::spawn(async move {
        let Some(token) = secrets
            .resolve_or_warn("monitoring.grafana.api_token", &grafana.api_token)
            .await
        else {
            return;
        };
        let result = http
            .post(format!("{}/api/annotations", grafana.url.trim_end_matches('/')))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        if let Err(e) = result {
            tracing::warn!("Grafana へ注釈を送れません: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_annotation() {
        let event = Event::problem("cam3", "停止しています");
        let b = body(&event, &["amore".to_string()]);
        assert_eq!(b["tags"], json!(["amore", "problem", "cam3"]));
        assert!(b["text"].as_str().unwrap().contains("cam3"));
        assert!(b["time"].as_u64().unwrap() > 0);
    }
}
