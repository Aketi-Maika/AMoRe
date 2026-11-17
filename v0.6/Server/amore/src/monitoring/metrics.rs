//! Prometheus 形式のメトリクス（`GET /metrics`。パスは `monitoring.prometheus.path`）。
//! Prometheus がスクレイプし、Grafana で表示する。
//!
//! v0.6 で追加：映像品質（送信画面の統計）、接続数・OBS（SRT）の通信状態（MediaMTX）、
//! カメラPC → OBS の遅延（OBS ドックの測定）。

use std::fmt::Write as _;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::{camera_statuses, CameraStatus, SharedState};
use crate::mediamtx::{self, SrtConn, WebrtcSession};
use crate::telemetry::{LatencySummary, QualityStats};

/// カメラごとの接続（カメラのコンテナの MediaMTX から取得）。
#[derive(Debug, Default)]
pub struct CameraConnections {
    pub camera: String,
    /// OBS（SRT）の読み取りの接続
    pub srt: Vec<SrtConn>,
    /// カメラPCからの送信（WebRTC）のセッション
    pub publishers: Vec<WebrtcSession>,
}

impl CameraConnections {
    /// MediaMTX の一覧から、このカメラの OBS の接続とカメラPCからの送信だけを選ぶ。
    pub fn new(camera: &str, srt: Vec<SrtConn>, webrtc: Vec<WebrtcSession>) -> Self {
        let ingest = mediamtx::ingest_path(camera);
        Self {
            camera: camera.to_string(),
            srt: srt
                .into_iter()
                .filter(|c| c.state == "read" && c.path == camera)
                .collect(),
            publishers: webrtc
                .into_iter()
                .filter(|s| s.state == "publish" && s.path == ingest)
                .collect(),
        }
    }
}

/// ラベル値のエスケープ（`\`・`"`・改行）。
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
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
    latency: &[LatencySummary],
    connections: &[CameraConnections],
) -> String {
    let mut out = String::new();
    gauge(
        &mut out,
        "amore_info",
        "AMoRe のバージョン",
        &[(format!("version=\"{}\"", esc(version)), 1.0)],
    );
    gauge(
        &mut out,
        "amore_docker_up",
        "Docker（Kubernetes では k8s API）に接続できるか",
        &[(String::new(), b(docker_up))],
    );
    gauge(
        &mut out,
        "amore_cameras",
        "登録カメラ数",
        &[(String::new(), cameras.len() as f64)],
    );

    let label = |c: &CameraStatus| format!("camera=\"{}\",name=\"{}\"", esc(&c.id), esc(&c.name));
    let rows = |f: &dyn Fn(&CameraStatus) -> f64| -> Vec<(String, f64)> {
        cameras.iter().map(|c| (label(c), f(c))).collect()
    };
    gauge(
        &mut out,
        "amore_camera_receiving",
        "カメラPCから受信中",
        &rows(&|c| b(c.receiving)),
    );
    gauge(
        &mut out,
        "amore_camera_output_ready",
        "OBS向けストリームを提供中",
        &rows(&|c| b(c.output_ready)),
    );
    gauge(
        &mut out,
        "amore_camera_reencoding",
        "H.264へ再エンコード中",
        &rows(&|c| b(c.reencoding)),
    );
    gauge(
        &mut out,
        "amore_camera_srt_readers",
        "SRTの接続数（OBS）",
        &rows(&|c| c.srt_readers as f64),
    );
    gauge(
        &mut out,
        "amore_camera_preview_viewers",
        "映像確認・モニターの接続数",
        &rows(&|c| c.preview_viewers as f64),
    );
    gauge(
        &mut out,
        "amore_camera_inbound_bytes",
        "受信量（バイト。受信開始からの累計）",
        &rows(&|c| c.inbound_bytes as f64),
    );
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
    gauge(
        &mut out,
        "amore_camera_problem",
        "コンテナの不調",
        &rows(&|c| b(c.health.problem.is_some())),
    );
    gauge(
        &mut out,
        "amore_camera_recording",
        "録画中の設定",
        &rows(&|c| b(c.recording)),
    );
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
    render_quality(&mut out, cameras);
    render_connections(&mut out, cameras, connections);
    render_latency(&mut out, cameras, latency);
    out
}

/// 表示名（未登録なら空）。
fn name_of<'a>(cameras: &'a [CameraStatus], id: &str) -> &'a str {
    cameras
        .iter()
        .find(|c| c.id == id)
        .map_or("", |c| c.name.as_str())
}

fn camera_label(cameras: &[CameraStatus], id: &str) -> String {
    format!(
        "camera=\"{}\",name=\"{}\"",
        esc(id),
        esc(name_of(cameras, id))
    )
}

/// 映像品質（送信画面の統計。届いているカメラだけ）。
fn render_quality(out: &mut String, cameras: &[CameraStatus]) {
    let rows = |f: &dyn Fn(&QualityStats) -> Option<f64>| -> Vec<(String, f64)> {
        cameras
            .iter()
            .filter_map(|c| Some((camera_label(cameras, &c.id), f(c.quality.as_ref()?)?)))
            .collect()
    };
    gauge(
        out,
        "amore_camera_video_width",
        "送信している映像の幅（px。送信画面の統計）",
        &rows(&|q| q.width.map(f64::from)),
    );
    gauge(
        out,
        "amore_camera_video_height",
        "送信している映像の高さ（px。送信画面の統計）",
        &rows(&|q| q.height.map(f64::from)),
    );
    gauge(
        out,
        "amore_camera_video_fps",
        "送信している映像のフレームレート",
        &rows(&|q| q.fps),
    );
    gauge(
        out,
        "amore_camera_video_bitrate_kbps",
        "映像の送信ビットレート（kbps。送信画面の統計）",
        &rows(&|q| q.video_kbps),
    );
    gauge(
        out,
        "amore_camera_audio_bitrate_kbps",
        "音声の送信ビットレート（kbps）",
        &rows(&|q| q.audio_kbps),
    );
    gauge(
        out,
        "amore_camera_available_bitrate_kbps",
        "送信画面の推定利用可能帯域（kbps）",
        &rows(&|q| q.available_kbps),
    );
    gauge(
        out,
        "amore_camera_rtt_ms",
        "カメラPC ⇔ Server の往復時間（ミリ秒）",
        &rows(&|q| q.rtt_ms),
    );
    gauge(
        out,
        "amore_camera_packet_loss_percent",
        "映像のパケット損失率（%。カメラPC → Server）",
        &rows(&|q| q.loss_percent),
    );
    gauge(
        out,
        "amore_camera_packets_lost",
        "失われた映像のパケット（累計。カメラPC → Server）",
        &rows(&|q| q.packets_lost),
    );
    gauge(
        out,
        "amore_camera_nack",
        "Server からの再送要求（累計）",
        &rows(&|q| q.nack_count),
    );
    gauge(
        out,
        "amore_camera_retransmitted_packets",
        "再送したパケット（累計）",
        &rows(&|q| q.retransmitted_packets),
    );
    gauge(
        out,
        "amore_camera_jitter_ms",
        "映像のジッター（ミリ秒。カメラPC → Server）",
        &rows(&|q| q.jitter_ms),
    );
    gauge(
        out,
        "amore_camera_fallback",
        "帯域不足時の品質（720p）で送信中",
        &rows(&|q| Some(b(q.quality_level == "fallback"))),
    );
    gauge(
        out,
        "amore_camera_timecode",
        "遅延計測の時刻コードを付けて送信中",
        &rows(&|q| Some(b(q.timecode))),
    );
    // 品質が下がっている理由（cpu / bandwidth / other。none なら 0）。
    let limited: Vec<(String, f64)> = cameras
        .iter()
        .filter_map(|c| {
            let q = c.quality.as_ref()?;
            let reason = if q.quality_limitation.is_empty() {
                "none"
            } else {
                q.quality_limitation.as_str()
            };
            Some((
                format!(
                    "{},reason=\"{}\"",
                    camera_label(cameras, &c.id),
                    esc(reason)
                ),
                b(reason != "none"),
            ))
        })
        .collect();
    gauge(
        out,
        "amore_camera_quality_limited",
        "ブラウザが映像の品質を下げている（理由はラベル reason）",
        &limited,
    );
}

/// 接続数と OBS（SRT）の通信状態（MediaMTX の統計）。
fn render_connections(
    out: &mut String,
    cameras: &[CameraStatus],
    connections: &[CameraConnections],
) {
    let per_camera = |f: &dyn Fn(&CameraConnections) -> Option<f64>| -> Vec<(String, f64)> {
        connections
            .iter()
            .filter_map(|cc| Some((camera_label(cameras, &cc.camera), f(cc)?)))
            .collect()
    };
    gauge(
        out,
        "amore_camera_publishers",
        "カメラPCからの送信（WebRTC）の接続数",
        &per_camera(&|cc| Some(cc.publishers.len() as f64)),
    );
    gauge(
        out,
        "amore_camera_webrtc_packets_lost",
        "Server で受信できなかった RTP パケット（累計。MediaMTX の統計）",
        &per_camera(&|cc| {
            cc.publishers
                .iter()
                .filter_map(|s| s.rtp_packets_lost)
                .reduce(|a, b| a + b)
        }),
    );
    let per_conn = |f: &dyn Fn(&SrtConn) -> Option<f64>| -> Vec<(String, f64)> {
        let mut rows = Vec::new();
        for cc in connections {
            for s in &cc.srt {
                if let Some(v) = f(s) {
                    rows.push((
                        format!(
                            "{},remote=\"{}\"",
                            camera_label(cameras, &cc.camera),
                            esc(&s.remote_addr)
                        ),
                        v,
                    ));
                }
            }
        }
        rows
    };
    gauge(
        out,
        "amore_srt_rtt_ms",
        "OBS（SRT）との往復時間（ミリ秒。接続ごと）",
        &per_conn(&|s| s.ms_rtt),
    );
    gauge(
        out,
        "amore_srt_send_mbps",
        "OBS（SRT）への送信レート（Mbps。接続ごと）",
        &per_conn(&|s| s.mbps_send_rate),
    );
    gauge(
        out,
        "amore_srt_packets_lost",
        "OBS（SRT）へ送って失われたパケット（累計。接続ごと）",
        &per_conn(&|s| s.packets_send_loss),
    );
    gauge(
        out,
        "amore_srt_packets_retrans",
        "OBS（SRT）へ再送したパケット（累計。接続ごと）",
        &per_conn(&|s| s.packets_retrans),
    );
}

/// カメラPC → OBS の遅延（OBS ドックの測定。カメラ・OBS ごと）。
fn render_latency(out: &mut String, cameras: &[CameraStatus], latency: &[LatencySummary]) {
    let rows = |f: &dyn Fn(&LatencySummary) -> f64| -> Vec<(String, f64)> {
        latency
            .iter()
            .map(|l| {
                (
                    format!(
                        "{},obs=\"{}\"",
                        camera_label(cameras, &l.camera),
                        esc(&l.obs)
                    ),
                    f(l),
                )
            })
            .collect()
    };
    gauge(
        out,
        "amore_camera_latency_ms",
        "カメラPC → OBS の遅延（ミリ秒。最新の測定値）",
        &rows(&|l| l.latest_ms),
    );
    gauge(
        out,
        "amore_camera_latency_avg_ms",
        "カメラPC → OBS の遅延（ミリ秒。直近60秒の平均）",
        &rows(&|l| l.avg_ms),
    );
    gauge(
        out,
        "amore_camera_latency_max_ms",
        "カメラPC → OBS の遅延（ミリ秒。直近60秒の最大）",
        &rows(&|l| l.max_ms),
    );
}

pub async fn handler(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    if !cfg.monitoring.prometheus.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cameras = camera_statuses(&state, &cfg).await;
    let docker_up = state
        .runtime
        .read()
        .expect("runtime lock")
        .docker_error
        .is_none();
    let recommended = state.ai.director_camera();
    let latency: Vec<LatencySummary> = state
        .telemetry
        .latency(Instant::now())
        .into_iter()
        .filter(|l| cfg.camera(&l.camera).is_some())
        .collect();
    // 稼働中のカメラのコンテナの MediaMTX から接続の統計を取る（取れないカメラは出さない）。
    let mut connections = Vec::new();
    for c in &cameras {
        if !c.container.as_ref().is_some_and(|x| x.state == "running") {
            continue;
        }
        let api = mediamtx::api_base(&cfg, &c.id);
        let (Ok(srt), Ok(webrtc)) = (
            mediamtx::list_srt_conns(&state.http, &api).await,
            mediamtx::list_webrtc_sessions(&state.http, &api).await,
        ) else {
            continue;
        };
        connections.push(CameraConnections::new(&c.id, srt, webrtc));
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        render(
            env!("CARGO_PKG_VERSION"),
            docker_up,
            &cameras,
            recommended.as_deref(),
            &latency,
            &connections,
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let text = render("0.3.0", true, &[], None, &[], &[]);
        assert!(text.contains("# TYPE amore_docker_up gauge\namore_docker_up 1\n"));
        assert!(text.contains("amore_info{version=\"0.3.0\"} 1"));
        assert!(text.contains("amore_cameras 0"));
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn renders_connections_and_latency() {
        let conns = [CameraConnections::new(
            "cam1",
            vec![
                SrtConn {
                    remote_addr: "10.40.0.30:5000".into(),
                    state: "read".into(),
                    path: "cam1".into(),
                    ms_rtt: Some(12.0),
                    ..Default::default()
                },
                // 読み取り以外・他のカメラの接続は数えない。
                SrtConn {
                    state: "publish".into(),
                    path: "cam1".into(),
                    ..Default::default()
                },
                SrtConn {
                    state: "read".into(),
                    path: "cam2".into(),
                    ..Default::default()
                },
            ],
            vec![
                WebrtcSession {
                    state: "publish".into(),
                    path: "cam1_in".into(),
                    rtp_packets_lost: Some(4.0),
                },
                WebrtcSession {
                    state: "read".into(),
                    path: "cam1_in".into(),
                    rtp_packets_lost: None,
                },
            ],
        )];
        assert_eq!(conns[0].srt.len(), 1);
        assert_eq!(conns[0].publishers.len(), 1);
        let latency = [LatencySummary {
            camera: "cam1".into(),
            obs: "編集PC".into(),
            latest_ms: 320.0,
            avg_ms: 300.0,
            max_ms: 410.0,
        }];
        let text = render("0.6.0", true, &[], None, &latency, &conns);
        for line in [
            "amore_camera_publishers{camera=\"cam1\",name=\"\"} 1\n",
            "amore_camera_webrtc_packets_lost{camera=\"cam1\",name=\"\"} 4\n",
            "amore_srt_rtt_ms{camera=\"cam1\",name=\"\",remote=\"10.40.0.30:5000\"} 12\n",
            "amore_camera_latency_ms{camera=\"cam1\",name=\"\",obs=\"編集PC\"} 320\n",
            "amore_camera_latency_max_ms{camera=\"cam1\",name=\"\",obs=\"編集PC\"} 410\n",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        assert!(
            !text.contains("amore_srt_send_mbps{"),
            "値の無い項目は出さない"
        );
    }
}
