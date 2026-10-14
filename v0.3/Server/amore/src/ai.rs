//! AI API 連携（`AMoRe.yaml` の `ai`）。
//!
//! - 障害診断：カメラのコンテナの状態・ログ・直近の出来事をAIへ渡し、原因と対処を日本語で受け取る（管理Webのボタン）
//! - 映像解析：受信中のカメラの静止画を定期的にAIへ送り、内容と異常（真っ暗・覆われている等）を判定する
//! - 配信操作支援：映像解析の結果から「今使うと良いカメラ」を提案する（OBSは操作しない）
//!
//! 接続先は Anthropic（Claude Messages API。Rust には公式SDKがないため HTTP で直接呼ぶ）か、
//! OpenAI互換の Chat Completions API（ローカルLLM等）を `ai.provider` で選ぶ。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};

use crate::api::{camera_statuses, AppState, SharedState};
use crate::config::{AiProvider, Config};
use crate::docker;
use crate::events::{self, Event};

/// Anthropic Messages API のバージョンヘッダー。
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 拒否（refusal）時にサーバー側で別モデルへ切り替える機能のベータヘッダー（`fallbacks: "default"` 用）。
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct VisionResult {
    pub at_ms: u64,
    pub model: String,
    /// 映っている内容の要約。
    pub summary: String,
    /// 主な被写体。
    pub subject: String,
    /// 映像の品質（1〜5、5が最良）。
    pub quality: u8,
    pub anomaly: bool,
    pub anomalies: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectorResult {
    pub at_ms: u64,
    pub model: String,
    pub camera_id: String,
    pub reason: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    pub at_ms: u64,
    pub model: String,
    /// Markdown の本文。
    pub text: String,
}

/// AI の結果の保持と、同時実行の制御。
#[derive(Default)]
pub struct AiState {
    vision: Mutex<HashMap<String, VisionResult>>,
    director: Mutex<Option<DirectorResult>>,
    diagnoses: Mutex<HashMap<String, Diagnosis>>,
    last_error: Mutex<Option<String>>,
    /// AI への問い合わせを1件ずつにする（費用・負荷の制御）。
    busy: tokio::sync::Mutex<()>,
}

impl AiState {
    pub fn vision(&self, camera: &str) -> Option<VisionResult> {
        self.vision.lock().expect("ai lock").get(camera).cloned()
    }

    pub fn director(&self) -> Option<DirectorResult> {
        self.director.lock().expect("ai lock").clone()
    }

    pub fn director_camera(&self) -> Option<String> {
        self.director().map(|d| d.camera_id)
    }

    pub fn diagnosis(&self, camera: &str) -> Option<Diagnosis> {
        self.diagnoses.lock().expect("ai lock").get(camera).cloned()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect("ai lock").clone()
    }

    /// 削除したカメラの結果を消す。
    pub fn forget(&self, camera: &str) {
        self.vision.lock().expect("ai lock").remove(camera);
        self.diagnoses.lock().expect("ai lock").remove(camera);
        let mut d = self.director.lock().expect("ai lock");
        if d.as_ref().is_some_and(|x| x.camera_id == camera) {
            *d = None;
        }
    }

    fn set_error(&self, e: Option<String>) {
        *self.last_error.lock().expect("ai lock") = e;
    }
}

/// AI への問い合わせ。
struct Request {
    system: String,
    text: String,
    images: Vec<Vec<u8>>,
    /// JSON で受け取る場合のスキーマ。
    schema: Option<Value>,
    effort: String,
    /// 映像解析用のモデルを使うか。
    vision: bool,
    max_tokens: u32,
}

/// effort（思考の深さ）を受け付けるモデルか。受け付けないモデルへ送ると 400 になる。
fn supports_effort(model: &str) -> bool {
    [
        "claude-fable", "claude-mythos", "claude-opus-5", "claude-opus-4-8", "claude-opus-4-7",
        "claude-opus-4-6", "claude-opus-4-5", "claude-sonnet-5", "claude-sonnet-4-6", "claude-haiku-5",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}

/// サーバー側フォールバック（`fallbacks: "default"`）を使えるモデルか。
fn supports_fallback(model: &str) -> bool {
    ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"].contains(&model)
}

/// Anthropic Messages API の要求本文。
fn anthropic_body(model: &str, req: &Request, fallbacks: bool) -> Value {
    let mut content: Vec<Value> = req
        .images
        .iter()
        .map(|img| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/jpeg",
                    "data": base64::engine::general_purpose::STANDARD.encode(img),
                }
            })
        })
        .collect();
    content.push(json!({ "type": "text", "text": req.text }));
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "system": req.system,
        "messages": [{ "role": "user", "content": content }],
    });
    let mut output_config = serde_json::Map::new();
    if supports_effort(model) {
        output_config.insert("effort".into(), json!(req.effort));
    }
    if let Some(schema) = &req.schema {
        output_config.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
    }
    if !output_config.is_empty() {
        body["output_config"] = Value::Object(output_config);
    }
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    body
}

/// Anthropic の応答から本文（text ブロック）を取り出す。拒否・途中打ち切りはエラーにする。
fn anthropic_text(msg: &Value) -> Result<String> {
    match msg["stop_reason"].as_str() {
        Some("refusal") => bail!(
            "AIが応答を拒否しました（{}）",
            msg["stop_details"]["category"].as_str().unwrap_or("理由不明")
        ),
        Some("max_tokens") => bail!("AIの応答が長さの上限で途中終了しました"),
        _ => {}
    }
    let text: Vec<&str> = msg["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        bail!("AIの応答に本文がありません");
    }
    Ok(text.join("\n"))
}

/// OpenAI互換 Chat Completions API の要求本文。
fn openai_body(model: &str, req: &Request) -> Value {
    let mut content = vec![json!({ "type": "text", "text": req.text })];
    for img in &req.images {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(img)) }
        }));
    }
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "messages": [
            { "role": "system", "content": req.system },
            { "role": "user", "content": content },
        ],
    });
    if let Some(schema) = &req.schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": "result", "schema": schema, "strict": true },
        });
    }
    body
}

/// AI へ問い合わせ、(本文, 使ったモデル) を返す。
async fn complete(state: &AppState, req: Request) -> Result<(String, String)> {
    let cfg = state.store.get();
    let ai = &cfg.ai;
    let timeout = Duration::from_secs(u64::from(ai.timeout_sec));
    match ai.provider {
        AiProvider::Anthropic => {
            let a = &ai.anthropic;
            let key = state.secrets.resolve(&a.api_key).await.context("ai.anthropic.api_key")?;
            let model = if req.vision { &a.vision_model } else { &a.model };
            let fallbacks = a.fallbacks && supports_fallback(model);
            let mut http = state
                .http
                .post(format!("{}/v1/messages", a.base_url.trim_end_matches('/')))
                .timeout(timeout)
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&anthropic_body(model, &req, fallbacks));
            if fallbacks {
                http = http.header("anthropic-beta", FALLBACK_BETA);
            }
            let res = http.send().await.context("Claude API に接続できません")?;
            let status = res.status();
            let msg: Value = res.json().await.context("Claude API の応答を解析できません")?;
            if !status.is_success() {
                bail!(
                    "Claude API がエラーを返しました（HTTP {status}）: {}",
                    msg["error"]["message"].as_str().unwrap_or("詳細なし")
                );
            }
            let used = msg["model"].as_str().unwrap_or(model).to_string();
            Ok((anthropic_text(&msg)?, used))
        }
        AiProvider::OpenaiCompatible => {
            let o = &ai.openai_compatible;
            let model = if req.vision && !o.vision_model.is_empty() { &o.vision_model } else { &o.model };
            let mut http = state
                .http
                .post(format!("{}/chat/completions", o.base_url.trim_end_matches('/')))
                .timeout(timeout)
                .json(&openai_body(model, &req));
            if !o.api_key.is_empty() {
                let key = state.secrets.resolve(&o.api_key).await.context("ai.openai_compatible.api_key")?;
                http = http.bearer_auth(key);
            }
            let res = http.send().await.context("AI API に接続できません")?;
            let status = res.status();
            let msg: Value = res.json().await.context("AI API の応答を解析できません")?;
            if !status.is_success() {
                bail!("AI API がエラーを返しました（HTTP {status}）: {}", msg["error"]["message"].as_str().unwrap_or("詳細なし"));
            }
            let text = msg["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| anyhow!("AI API の応答に本文がありません"))?;
            Ok((text.to_string(), msg["model"].as_str().unwrap_or(model).to_string()))
        }
    }
}

/// 応答本文から JSON を取り出す（前後に説明が付いた場合も `{`〜`}` を探す）。
fn extract_json(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Ok(v);
    }
    let start = text.find('{').ok_or_else(|| anyhow!("応答に JSON がありません"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("応答に JSON がありません"))?;
    serde_json::from_str(&text[start..=end]).context("応答の JSON を解析できません")
}

// ---------------------------------------------------------------- 障害診断

const DIAGNOSIS_SYSTEM: &str = "あなたは AMoRe の運用を支援するエンジニアです。AMoRe は次の構成の映像伝送システムです。\n\
- カメラPCのブラウザ（Chrome/Edge）が WebRTC（WHIP）で映像・音声を送信する\n\
- Server（Rocky Linux 9 の VM、Docker）上で、カメラ1台につき1つのコンテナ（MediaMTX＋FFmpeg、名前 amore-cam-<カメラID>）が受信し、\n\
  H.264 はそのまま、VP8 は H.264 へ、Opus は AAC へ変換して、SRT（MPEG-TS）で編集PCの OBS へ送る\n\
- 管理用の AMoRe（Rust）が Docker ソケット経由でカメラのコンテナを作成・監視し、Docker ネットワーク amore_net で接続する\n\
- 不調時は AMoRe が自動で作り直す（最大3回）\n\
渡された状態・ログ・出来事をもとに、操作者が次に何をすればよいか分かるよう、日本語の Markdown で次の順に答えてください。\n\
1. 状況の要約（2〜3行）\n2. 考えられる原因（可能性の高い順。根拠となるログの行を引用）\n3. 確認手順（Server で実行するコマンドを含める）\n4. 対処\n\
推測は推測と明記し、ログにない事実を断定しないでください。";

/// 診断用の情報（秘密情報は含めない）。
async fn diagnosis_context(state: &AppState, cfg: &Config, camera_id: &str) -> Result<String> {
    let cam = cfg.camera(camera_id).ok_or_else(|| anyhow!("カメラ {camera_id} は登録されていません"))?;
    let statuses = camera_statuses(state, cfg).await;
    let status = statuses.iter().find(|s| s.id == camera_id);
    let name = cfg.container_name(camera_id);
    let logs = docker::logs(&name, cfg.ai.diagnosis.log_lines)
        .await
        .unwrap_or_else(|e| format!("（ログを取得できません: {e:#}）"));
    let events: Vec<String> = events::recent(state, Some(camera_id), 20)
        .into_iter()
        .map(|e| format!("{} {} {}", e.time_ms, e.level, e.text))
        .collect();
    let all_containers: Vec<Value> = statuses
        .iter()
        .map(|s| json!({ "camera": s.id, "container": s.container, "problem": s.health.problem }))
        .collect();
    let context = json!({
        "camera": { "id": cam.id, "name": cam.name, "webrtc_udp_port": cam.webrtc_udp_port, "srt_port": cam.srt_port },
        "status": status,
        "all_cameras": all_containers,
        "settings": {
            "public_host": cfg.server.public_host,
            "docker_network": cfg.media.docker_network,
            "docker_subnet": cfg.media.docker_subnet,
            "image": cfg.media.image,
            "udp_read_buffer_bytes": cfg.media.udp_read_buffer_bytes,
            "video": cfg.video.standard,
            "srt_latency_ms": cfg.srt.latency_ms,
            "recording": { "enabled": cfg.camera_records(cam), "host_dir": cfg.recording.host_dir, "output_format": cfg.recording.output_format },
        },
        "docker_error": state.runtime.read().expect("runtime lock").docker_error,
        "recent_events": events,
    });
    Ok(format!(
        "カメラ {camera_id} について診断してください。\n\n## 状態（JSON）\n```json\n{}\n```\n\n## コンテナ {name} のログ（直近 {} 行）\n```\n{logs}\n```",
        serde_json::to_string_pretty(&context)?,
        cfg.ai.diagnosis.log_lines,
    ))
}

/// 障害診断を実行する（管理Webから呼ぶ）。
pub async fn diagnose(state: &AppState, camera_id: &str) -> Result<Diagnosis> {
    let cfg = state.store.get();
    if !cfg.ai.enabled || !cfg.ai.diagnosis.enabled {
        bail!("AIの障害診断は無効です（ai.enabled・ai.diagnosis.enabled）");
    }
    let text = diagnosis_context(state, &cfg, camera_id).await?;
    let _busy = state.ai.busy.lock().await;
    let (answer, model) = complete(
        state,
        Request {
            system: DIAGNOSIS_SYSTEM.to_string(),
            text,
            images: Vec::new(),
            schema: None,
            effort: cfg.ai.diagnosis.effort.clone(),
            vision: false,
            max_tokens: 16_000,
        },
    )
    .await?;
    let d = Diagnosis {
        at_ms: now_ms(),
        model,
        text: answer,
    };
    state
        .ai
        .diagnoses
        .lock()
        .expect("ai lock")
        .insert(camera_id.to_string(), d.clone());
    Ok(d)
}

// ---------------------------------------------------------------- 映像解析

const VISION_SYSTEM: &str = "あなたはライブ配信の映像を監視する担当者です。カメラの現在の静止画1枚を見て、指定の JSON で答えてください。\n\
- summary: 映っている内容（日本語、40字以内）\n- subject: 主な被写体（日本語、15字以内）\n\
- quality: 配信映像としての品質を1〜5の整数で（5が最良。ピント・明るさ・構図・水平で判断）\n\
- anomaly: 配信上の問題があれば true。例: 画面が真っ暗・真っ白、レンズが覆われている、大きく傾いている、カメラが倒れている、ひどいピンぼけ、極端な露出不足・過多、ブロックノイズ等の映像の乱れ\n\
- anomalies: 問題の具体的な内容（日本語。無ければ空の配列）\n\
被写体がいない・動きがないだけでは異常にしないでください。";

fn vision_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "subject", "quality", "anomaly", "anomalies"],
        "properties": {
            "summary": { "type": "string" },
            "subject": { "type": "string" },
            "quality": { "type": "integer" },
            "anomaly": { "type": "boolean" },
            "anomalies": { "type": "array", "items": { "type": "string" } },
        }
    })
}

fn parse_vision(v: &Value, model: String) -> Result<VisionResult> {
    Ok(VisionResult {
        at_ms: now_ms(),
        model,
        summary: v["summary"].as_str().ok_or_else(|| anyhow!("summary がありません"))?.to_string(),
        subject: v["subject"].as_str().unwrap_or("").to_string(),
        quality: v["quality"].as_u64().unwrap_or(0).clamp(0, 5) as u8,
        anomaly: v["anomaly"].as_bool().unwrap_or(false),
        anomalies: v["anomalies"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
    })
}

async fn analyze_camera(state: &AppState, cfg: &Config, camera_id: &str) -> Result<VisionResult> {
    let cam = cfg.camera(camera_id).ok_or_else(|| anyhow!("未登録のカメラ"))?;
    let jpeg = docker::snapshot_jpeg(&cfg.container_name(camera_id), camera_id, cfg.ai.vision.image_width).await?;
    let (text, model) = complete(
        state,
        Request {
            system: VISION_SYSTEM.to_string(),
            text: format!("カメラ「{}」（{}）の現在の映像です。", cam.name, cam.id),
            images: vec![jpeg],
            schema: Some(vision_schema()),
            effort: cfg.ai.vision.effort.clone(),
            vision: true,
            max_tokens: 4_000,
        },
    )
    .await?;
    parse_vision(&extract_json(&text)?, model)
}

// ---------------------------------------------------------------- 配信操作支援

const DIRECTOR_SYSTEM: &str = "あなたは小規模イベントのライブ配信で、1人で複数カメラを切り替える操作者を補佐するディレクターです。\n\
各カメラの最新の映像解析結果をもとに、今メインに映すと良いカメラを1つ選び、指定の JSON で答えてください。\n\
- camera_id: 候補のカメラIDから1つ\n- reason: 選んだ理由（日本語、30字程度）\n- confidence: 確信度（0〜1）\n\
異常のあるカメラ・送信していないカメラ・解析結果が古いカメラは避けてください。";

fn director_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["camera_id", "reason", "confidence"],
        "properties": {
            "camera_id": { "type": "string" },
            "reason": { "type": "string" },
            "confidence": { "type": "number" },
        }
    })
}

async fn direct(state: &AppState, cfg: &Config) -> Result<Option<DirectorResult>> {
    let now = now_ms();
    let candidates: Vec<Value> = cfg
        .cameras
        .iter()
        .filter_map(|c| {
            let v = state.ai.vision(&c.id)?;
            Some(json!({
                "camera_id": c.id,
                "name": c.name,
                "analyzed_seconds_ago": now.saturating_sub(v.at_ms) / 1000,
                "summary": v.summary,
                "subject": v.subject,
                "quality": v.quality,
                "anomalies": v.anomalies,
            }))
        })
        .collect();
    if candidates.len() < 2 {
        return Ok(None);
    }
    let ids: Vec<&str> = cfg.cameras.iter().map(|c| c.id.as_str()).collect();
    let (text, model) = complete(
        state,
        Request {
            system: DIRECTOR_SYSTEM.to_string(),
            text: format!(
                "候補のカメラ（JSON）:\n```json\n{}\n```",
                serde_json::to_string_pretty(&candidates)?
            ),
            images: Vec::new(),
            schema: Some(director_schema()),
            effort: cfg.ai.director.effort.clone(),
            vision: false,
            max_tokens: 4_000,
        },
    )
    .await?;
    let v = extract_json(&text)?;
    let camera_id = v["camera_id"].as_str().unwrap_or("").to_string();
    if !ids.contains(&camera_id.as_str()) {
        bail!("AIが候補にないカメラ {camera_id:?} を提案しました");
    }
    Ok(Some(DirectorResult {
        at_ms: now_ms(),
        model,
        camera_id,
        reason: v["reason"].as_str().unwrap_or("").to_string(),
        confidence: v["confidence"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0),
    }))
}

/// 映像解析・配信操作支援を定期実行する（`ai.enabled` のとき）。
pub fn start(state: SharedState) {
    tokio::spawn(async move {
        let mut next_vision: HashMap<String, Instant> = HashMap::new();
        let mut next_director = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let cfg = state.store.get();
            if !cfg.ai.enabled {
                continue;
            }
            if cfg.ai.vision.enabled {
                vision_round(&state, &cfg, &mut next_vision).await;
            }
            if cfg.ai.director.enabled && Instant::now() >= next_director {
                next_director = Instant::now() + Duration::from_secs(u64::from(cfg.ai.director.interval_sec));
                let _busy = state.ai.busy.lock().await;
                match direct(&state, &cfg).await {
                    Ok(Some(d)) => *state.ai.director.lock().expect("ai lock") = Some(d),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("AIの配信操作支援に失敗しました: {e:#}");
                        state.ai.set_error(Some(format!("配信操作支援: {e:#}")));
                    }
                }
            }
        }
    });
}

async fn vision_round(state: &Arc<AppState>, cfg: &Config, next: &mut HashMap<String, Instant>) {
    let statuses = camera_statuses(state, cfg).await;
    let interval = Duration::from_secs(u64::from(cfg.ai.vision.interval_sec));
    for s in statuses.iter().filter(|s| s.output_ready) {
        if next.get(&s.id).is_some_and(|t| Instant::now() < *t) {
            continue;
        }
        next.insert(s.id.clone(), Instant::now() + interval);
        let _busy = state.ai.busy.lock().await;
        match analyze_camera(state, cfg, &s.id).await {
            Ok(result) => {
                let was_anomaly = state.ai.vision(&s.id).is_some_and(|v| v.anomaly);
                if result.anomaly && !was_anomaly {
                    events::notify(state, Event::ai_anomaly(&s.id, &result.anomalies.join(" / ")));
                }
                state.ai.vision.lock().expect("ai lock").insert(s.id.clone(), result);
                state.ai.set_error(None);
            }
            Err(e) => {
                tracing::warn!(camera = %s.id, "AIの映像解析に失敗しました: {e:#}");
                state.ai.set_error(Some(format!("映像解析（{}）: {e:#}", s.id)));
            }
        }
    }
    // 送信していないカメラの古い結果は消す（推奨の対象から外す）。
    let receiving: Vec<&str> = statuses.iter().filter(|s| s.output_ready).map(|s| s.id.as_str()).collect();
    state
        .ai
        .vision
        .lock()
        .expect("ai lock")
        .retain(|id, _| receiving.contains(&id.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(schema: bool) -> Request {
        Request {
            system: "sys".into(),
            text: "見てください".into(),
            images: vec![vec![0xff, 0xd8, 0xff]],
            schema: schema.then(vision_schema),
            effort: "low".into(),
            vision: true,
            max_tokens: 4000,
        }
    }

    #[test]
    fn builds_anthropic_request() {
        let body = anthropic_body("claude-opus-5-5", &request(true), true);
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
        assert_eq!(body["messages"][0]["content"][0]["source"]["media_type"], "image/jpeg");
        assert_eq!(body["messages"][0]["content"][0]["source"]["data"], "/9j/");
        assert_eq!(body["messages"][0]["content"][1]["text"], "見てください");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["fallbacks"], "default");
        // effort を受け付けないモデルには送らない。
        let body = anthropic_body("claude-haiku-4-5", &request(false), false);
        assert!(body.get("output_config").is_none());
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn reads_anthropic_response() {
        let ok = json!({
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{ "type": "thinking", "thinking": "" }, { "type": "text", "text": "{\"a\":1}" }]
        });
        assert_eq!(anthropic_text(&ok).unwrap(), "{\"a\":1}");
        let refused = json!({ "stop_reason": "refusal", "stop_details": { "category": "cyber" }, "content": [] });
        assert!(anthropic_text(&refused).unwrap_err().to_string().contains("拒否"));
    }

    #[test]
    fn builds_openai_request() {
        let body = openai_body("local-vl", &request(true));
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"][1]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
        assert_eq!(body["response_format"]["type"], "json_schema");
    }

    #[test]
    fn parses_json_answers() {
        let v = extract_json("結果です: {\"summary\":\"舞台\",\"subject\":\"演者\",\"quality\":4,\"anomaly\":false,\"anomalies\":[]} 以上").unwrap();
        let r = parse_vision(&v, "m".into()).unwrap();
        assert_eq!(r.summary, "舞台");
        assert_eq!(r.quality, 4);
        assert!(!r.anomaly);
        assert!(extract_json("JSONなし").is_err());
    }

    #[test]
    fn effort_and_fallback_support() {
        assert!(supports_effort("claude-opus-5-5"));
        assert!(supports_effort("claude-haiku-5-5"));
        assert!(!supports_effort("claude-haiku-4-5"));
        assert!(supports_fallback("claude-opus-5-5"));
        assert!(!supports_fallback("claude-haiku-5-5"));
    }
}
