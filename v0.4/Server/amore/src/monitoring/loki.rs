//! Loki へのログ送信（`monitoring.loki`）。
//!
//! 次の3種類を `POST /loki/api/v1/push` へ1秒ごとにまとめて送る。
//! - AMoRe 自身のログ（`source="amore"`。tracing のレイヤーで取得）
//! - カメラのコンテナのログ（`source="camera"`。MediaMTX・FFmpeg。`container_logs: true` の場合）
//! - 出来事（`source="event"`。カメラの追加・不調・復旧・AIの異常検知など）
//!
//! 送信に失敗しても AMoRe の動作は止めない（ログが欠けるだけ）。

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::api::{AppState, SharedState};
use crate::events::Event;

/// 1行のログ。
#[derive(Debug, Clone)]
pub struct Line {
    pub ts_ns: u128,
    pub labels: BTreeMap<String, String>,
    pub text: String,
}

static SENDER: OnceLock<mpsc::UnboundedSender<Line>> = OnceLock::new();
/// 転送中のカメラコンテナのログ（コンテナ名 → タスク）。
static FOLLOWERS: OnceLock<Mutex<HashMap<String, JoinHandle<()>>>> = OnceLock::new();

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn send(labels: &[(&str, &str)], text: String) {
    if let Some(tx) = SENDER.get() {
        let labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let _ = tx.send(Line {
            ts_ns: now_ns(),
            labels,
            text,
        });
    }
}

/// 出来事を送る（Loki が無効なら何もしない）。
pub fn push_event(event: &Event) {
    let camera = event.camera.clone().unwrap_or_default();
    send(
        &[
            ("source", "event"),
            ("level", event.level),
            ("kind", event.kind),
            ("camera", camera.as_str()),
        ],
        event.text.clone(),
    );
}

/// tracing のイベントを Loki へ送るレイヤー。AMoRe 自身（target が `amore`）のログだけを対象にする
/// （HTTP クライアント等のログを送ると、送信処理のログが再び送られて増え続けるため）。
pub struct LokiLayer;

#[derive(Default)]
struct Visitor {
    message: String,
    camera: String,
    fields: Vec<String>,
}

impl Visit for Visitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let text = format!("{value:?}");
        match field.name() {
            "message" => self.message = text,
            "camera" => self.camera = text.trim_matches('"').to_string(),
            name => self.fields.push(format!("{name}={text}")),
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "camera" => self.camera = value.to_string(),
            name => self.fields.push(format!("{name}={value}")),
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for LokiLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if SENDER.get().is_none() {
            return;
        }
        let meta = event.metadata();
        if !meta.target().starts_with("amore") {
            return;
        }
        let mut v = Visitor::default();
        event.record(&mut v);
        let mut text = v.message;
        if !v.fields.is_empty() {
            text.push(' ');
            text.push_str(&v.fields.join(" "));
        }
        let level = meta.level().to_string().to_lowercase();
        send(
            &[("source", "amore"), ("level", level.as_str()), ("camera", v.camera.as_str())],
            text,
        );
    }
}

/// 送信データ（Loki の push API の JSON）を作る。ラベルの組ごとにストリームへまとめる。
pub fn build_payload(lines: &[Line], base: &BTreeMap<String, String>) -> serde_json::Value {
    let mut streams: BTreeMap<BTreeMap<String, String>, Vec<serde_json::Value>> = BTreeMap::new();
    for line in lines {
        let mut labels = base.clone();
        labels.insert("app".to_string(), "amore".to_string());
        for (k, v) in &line.labels {
            // 空のラベルは送らない（Loki では空と未指定が同じ扱い）。
            if !v.is_empty() {
                labels.insert(k.clone(), v.clone());
            }
        }
        streams
            .entry(labels)
            .or_default()
            .push(json!([line.ts_ns.to_string(), line.text]));
    }
    json!({
        "streams": streams
            .into_iter()
            .map(|(stream, values)| json!({ "stream": stream, "values": values }))
            .collect::<Vec<_>>()
    })
}

/// Loki への送信を開始する（`monitoring.loki.enabled` のときだけ呼ぶ）。
pub fn start(state: SharedState) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Line>();
    if SENDER.set(tx).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut buffer: Vec<Line> = Vec::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(line) = rx.recv() => {
                    buffer.push(line);
                    if buffer.len() < 1000 {
                        continue;
                    }
                }
                _ = tick.tick() => {}
            }
            if buffer.is_empty() {
                continue;
            }
            let lines = std::mem::take(&mut buffer);
            if let Err(e) = push(&state, &lines).await {
                // tracing を使うと自分自身へ送られるため、標準エラーへ出す。
                eprintln!("[amore] Loki へ送信できません（{} 行を破棄）: {e:#}", lines.len());
            }
        }
    });
}

async fn push(state: &AppState, lines: &[Line]) -> anyhow::Result<()> {
    let cfg = state.store.get();
    let loki = &cfg.monitoring.loki;
    let payload = build_payload(lines, &loki.labels);
    let mut req = state
        .http
        .post(format!("{}/loki/api/v1/push", loki.url.trim_end_matches('/')))
        .json(&payload);
    if !loki.tenant.is_empty() {
        req = req.header("X-Scope-OrgID", loki.tenant.as_str());
    }
    if !loki.username.is_empty() {
        let password = state.secrets.resolve(&loki.password).await?;
        req = req.basic_auth(&loki.username, Some(password));
    }
    req.send().await?.error_for_status()?;
    Ok(())
}

/// 稼働中のカメラコンテナのログ転送をそろえる（定期的に呼ぶ）。
pub fn sync_container_logs(state: &Arc<AppState>) {
    let cfg = state.store.get();
    if SENDER.get().is_none() || !cfg.monitoring.loki.container_logs {
        return;
    }
    let running: Vec<(String, String)> = state
        .runtime
        .read()
        .expect("runtime lock")
        .containers
        .iter()
        .filter(|c| c.state == "running")
        .map(|c| (c.name.clone(), c.camera.clone()))
        .collect();
    let followers = FOLLOWERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut followers = followers.lock().expect("followers lock");
    followers.retain(|name, task| {
        let keep = running.iter().any(|(n, _)| n == name) && !task.is_finished();
        if !keep {
            task.abort();
        }
        keep
    });
    for (name, camera) in running {
        followers
            .entry(name.clone())
            .or_insert_with(|| tokio::spawn(follow(name, camera)));
    }
}

/// `docker logs --follow` で1コンテナのログを読み続けて送る。
async fn follow(name: String, camera: String) {
    let child = Command::new("docker")
        .args(["logs", "--follow", "--since", "1s", name.as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        return;
    };
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return;
    };
    tokio::join!(
        read_lines(stdout, "stdout", &camera),
        read_lines(stderr, "stderr", &camera),
    );
    let _ = child.wait().await;
}

async fn read_lines<R: tokio::io::AsyncRead + Unpin>(pipe: R, stream: &'static str, camera: &str) {
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // MediaMTX のログは " INF " / " WAR " / " ERR " を含む。FFmpeg のエラーは "error" を含むことが多い。
        let level = if line.contains(" ERR ") || line.contains("error") {
            "error"
        } else if line.contains(" WAR ") {
            "warning"
        } else {
            "info"
        };
        send(
            &[("source", "camera"), ("camera", camera), ("stream", stream), ("level", level)],
            line,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_lines_into_streams() {
        let mut base = BTreeMap::new();
        base.insert("site".to_string(), "hq".to_string());
        let line = |camera: &str, text: &str| Line {
            ts_ns: 1_700_000_000_000_000_000,
            labels: [("source", "camera"), ("camera", camera), ("level", "info")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            text: text.to_string(),
        };
        let payload = build_payload(&[line("cam1", "a"), line("cam2", "b"), line("cam1", "c")], &base);
        let streams = payload["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["stream"]["camera"], "cam1");
        assert_eq!(streams[0]["stream"]["app"], "amore");
        assert_eq!(streams[0]["stream"]["site"], "hq");
        assert_eq!(streams[0]["values"][1][1], "c");
        assert_eq!(streams[0]["values"][0][0], "1700000000000000000");
    }
}
