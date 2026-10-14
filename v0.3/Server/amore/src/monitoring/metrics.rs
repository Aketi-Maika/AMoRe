//! Prometheus 形式のメトリクス（`GET /metrics`。パスは `monitoring.prometheus.path`）。
//! Prometheus がスクレイプし、Grafana で表示する。

use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::{camera_statuses, CameraStatus, SharedState};

/// ラベル値のエスケープ（`\`・`"`・改行）。
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn gauge(out: &mut String, name: &str, help: &str, rows: &[(String, f64)]) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    for (labels, value) in rows {
        if labels.is_empty() {
            let _ = writeln!(out, "{name} {value}");
        } else {
            let _ = writeln!(out, "{name}{{{labels}}} {value}");
        }
    }
}

fn b(v: bool) -> f64 {
    if v {
        1.0
    } else {
        0.0
    }
}

/// メトリクス本文を作る（テストしやすいよう状態から分離）。
pub fn render(
    version: &str,
    docker_up: bool,
    cameras: &[CameraStatus],
    recommended: Option<&str>,
) -> String {
    let mut out = String::new();
    gauge(&mut out, "amore_info", "AMoRe のバージョン", &[(format!("version=\"{}\"", esc(version)), 1.0)]);
    gauge(&mut out, "amore_docker_up", "Docker に接続できるか", &[(String::new(), b(docker_up))]);
    gauge(&mut out, "amore_cameras", "登録カメラ数", &[(String::new(), cameras.len() as f64)]);

    let label = |c: &CameraStatus| format!("camera=\"{}\",name=\"{}\"", esc(&c.id), esc(&c.name));
    let rows = |f: &dyn Fn(&CameraStatus) -> f64| -> Vec<(String, f64)> {
        cameras.iter().map(|c| (label(c), f(c))).collect()
    };
    gauge(&mut out, "amore_camera_receiving", "カメラPCから受信中", &rows(&|c| b(c.receiving)));
    gauge(&mut out, "amore_camera_output_ready", "OBS向けストリームを提供中", &rows(&|c| b(c.output_ready)));
    gauge(&mut out, "amore_camera_reencoding", "H.264へ再エンコード中", &rows(&|c| b(c.reencoding)));
    gauge(&mut out, "amore_camera_srt_readers", "SRTの接続数（OBS）", &rows(&|c| c.srt_readers as f64));
    gauge(&mut out, "amore_camera_preview_viewers", "映像確認・モニターの接続数", &rows(&|c| c.preview_viewers as f64));
    gauge(&mut out, "amore_camera_inbound_bytes", "受信量（バイト。受信開始からの累計）", &rows(&|c| c.inbound_bytes as f64));
    gauge(
        &mut out,
        "amore_camera_container_running",
        "カメラのコンテナが稼働中",
        &rows(&|c| b(c.container.as_ref().is_some_and(|x| x.state == "running"))),
    );
    gauge(
        &mut out,
        "amore_camera_container_restarts",
        "カメラのコンテナの再起動回数",
        &rows(&|c| c.container.as_ref().map_or(0.0, |x| x.restart_count as f64)),
    );
    gauge(&mut out, "amore_camera_problem", "コンテナの不調", &rows(&|c| b(c.health.problem.is_some())));
    gauge(&mut out, "amore_camera_recording", "録画中の設定", &rows(&|c| b(c.recording)));
    gauge(
        &mut out,
        "amore_camera_ai_anomaly",
        "AI映像解析で異常を検知",
        &rows(&|c| b(c.ai_vision.as_ref().is_some_and(|v| v.anomaly))),
    );
    gauge(
        &mut out,
        "amore_camera_ai_recommended",
        "AIが推奨するカメラ",
        &rows(&|c| b(recommended == Some(c.id.as_str()))),
    );
    out
}

pub async fn handler(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    if !cfg.monitoring.prometheus.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cameras = camera_statuses(&state, &cfg).await;
    let docker_up = state.runtime.read().expect("runtime lock").docker_error.is_none();
    let recommended = state.ai.director_camera();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        render(env!("CARGO_PKG_VERSION"), docker_up, &cameras, recommended.as_deref()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let text = render("0.3.0", true, &[], None);
        assert!(text.contains("# TYPE amore_docker_up gauge\namore_docker_up 1\n"));
        assert!(text.contains("amore_info{version=\"0.3.0\"} 1"));
        assert!(text.contains("amore_cameras 0"));
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
