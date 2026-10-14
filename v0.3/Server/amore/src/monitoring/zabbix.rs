//! Zabbix への値の送信（`monitoring.zabbix`）。
//!
//! Zabbix sender プロトコル（`ZBXD\x01`＋データ長＋JSON）で、trapper アイテムへ定期的に送る。
//! カメラはローレベルディスカバリ（`amore.camera.discovery`）で自動登録される。
//! Zabbix 側には `Server/monitoring/zabbix_template_amore.yaml` を取り込み、ホストにリンクする。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::api::{camera_statuses, CameraStatus, SharedState};

/// 送信データを作る。
pub fn build_request(
    host: &str,
    docker_up: bool,
    cameras: &[CameraStatus],
    recommended: Option<&str>,
) -> Value {
    let discovery: Vec<Value> = cameras
        .iter()
        .map(|c| json!({ "{#CAMERA}": c.id, "{#NAME}": c.name }))
        .collect();
    let mut data = vec![
        item(host, "amore.camera.discovery", Value::String(Value::Array(discovery).to_string())),
        item(host, "amore.docker_ok", u8::from(docker_up).into()),
        item(host, "amore.cameras", cameras.len().into()),
        item(host, "amore.ai.recommended", recommended.unwrap_or("").into()),
    ];
    for c in cameras {
        let key = |name: &str| format!("amore.camera.{name}[{}]", c.id);
        let running = c.container.as_ref().is_some_and(|x| x.state == "running");
        let anomaly = c
            .ai_vision
            .as_ref()
            .filter(|v| v.anomaly)
            .map(|v| v.anomalies.join(" / "))
            .unwrap_or_default();
        data.extend([
            item(host, &key("receiving"), u8::from(c.receiving).into()),
            item(host, &key("output"), u8::from(c.output_ready).into()),
            item(host, &key("obs_readers"), c.srt_readers.into()),
            item(host, &key("inbound_bytes"), c.inbound_bytes.into()),
            item(host, &key("container_running"), u8::from(running).into()),
            item(
                host,
                &key("restart_count"),
                c.container.as_ref().map_or(0, |x| x.restart_count).into(),
            ),
            item(host, &key("problem"), c.health.problem.clone().unwrap_or_default().into()),
            item(host, &key("recording"), u8::from(c.recording).into()),
            item(host, &key("ai_anomaly"), anomaly.into()),
        ]);
    }
    json!({ "request": "sender data", "data": data })
}

fn item(host: &str, key: &str, value: Value) -> Value {
    // Zabbix は値を文字列として受け取る。
    let value = match value {
        Value::String(s) => s,
        other => other.to_string(),
    };
    json!({ "host": host, "key": key, "value": value })
}

/// Zabbix sender プロトコルのパケット（ヘッダー `ZBXD` + 0x01 + データ長(u32 LE) + 予約(u32) + JSON）。
pub fn packet(json: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(13 + json.len());
    p.extend_from_slice(b"ZBXD\x01");
    p.extend_from_slice(&(json.len() as u32).to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(json);
    p
}

/// 送信して応答（例: `processed: 10; failed: 0; total: 10; …`）を返す。
pub async fn send(server: &str, port: u16, request: &Value) -> Result<String> {
    let body = serde_json::to_vec(request)?;
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((server, port)))
        .await
        .context("Zabbix Server への接続がタイムアウトしました")?
        .with_context(|| format!("Zabbix Server {server}:{port} に接続できません"))?;
    stream.write_all(&packet(&body)).await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .context("Zabbix Server の応答がタイムアウトしました")??;
    if response.len() < 13 || &response[..4] != b"ZBXD" {
        bail!("Zabbix Server の応答が不正です");
    }
    let json: Value = serde_json::from_slice(&response[13..]).context("応答を解析できません")?;
    if json["response"] != "success" {
        bail!("Zabbix Server が受け付けませんでした: {json}");
    }
    Ok(json["info"].as_str().unwrap_or("").to_string())
}

/// `monitoring.zabbix.enabled` のとき、定期的に送信する。
pub fn start(state: SharedState) {
    tokio::spawn(async move {
        loop {
            let cfg = state.store.get();
            let z = cfg.monitoring.zabbix.clone();
            tokio::time::sleep(Duration::from_secs(u64::from(z.interval_sec))).await;
            let cameras = camera_statuses(&state, &cfg).await;
            let docker_up = state.runtime.read().expect("runtime lock").docker_error.is_none();
            let recommended = state.ai.director_camera();
            let request = build_request(&z.host, docker_up, &cameras, recommended.as_deref());
            match send(&z.server, z.port, &request).await {
                Ok(info) if info.contains("failed: 0") => {
                    tracing::debug!("Zabbix へ送信しました: {info}");
                }
                Ok(info) => tracing::warn!(
                    "Zabbix が一部の値を受け付けませんでした（テンプレートのリンク・ホスト名 {} を確認）: {info}",
                    z.host
                ),
                Err(e) => tracing::warn!("Zabbix へ送信できません: {e:#}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_packet_header() {
        let p = packet(b"{}");
        assert_eq!(&p[..5], b"ZBXD\x01");
        assert_eq!(u32::from_le_bytes(p[5..9].try_into().unwrap()), 2);
        assert_eq!(&p[9..13], &[0, 0, 0, 0]);
        assert_eq!(&p[13..], b"{}");
    }

    #[test]
    fn builds_request_with_discovery() {
        let req = build_request("amore01", true, &[], Some("cam1"));
        assert_eq!(req["request"], "sender data");
        let data = req["data"].as_array().unwrap();
        assert_eq!(data[0]["key"], "amore.camera.discovery");
        assert_eq!(data[0]["value"], "[]");
        assert_eq!(data[1]["value"], "1");
        assert_eq!(data[3]["value"], "cam1");
        assert!(data.iter().all(|d| d["host"] == "amore01" && d["value"].is_string()));
    }
}
