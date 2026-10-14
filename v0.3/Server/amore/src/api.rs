//! HTTPS API と静的ファイル配信。
//!
//! 公開（送信画面・OBSドック・モニター用）
//! - `GET  /healthz`                                   稼働確認
//! - `GET  /api/v1/config`                             送信画面向けの設定（AMoRe.yaml由来）
//! - `GET  /api/v1/status`                             カメラごとの受信・出力・コンテナ状態・不調・録画・AI結果とSRT URL
//! - `GET  /api/v1/ai`                                 AIの映像解析・推奨カメラ
//! - `POST /api/v1/cameras/{id}/whip`                  WHIP（whip_proxy.rs）
//! - `GET  /metrics`                                   Prometheus 形式のメトリクス（monitoring.prometheus）
//!
//! 管理Web用（admin.rs）
//! - `/api/v1/admin/...`                               カメラの追加・削除・名前変更・録画切替、コンテナ操作、映像確認(WHEP)、AI診断
//!
//! 静的ファイル
//! - `/admin/...` 管理Web・モニター（Server/admin）、`/obs/...` OBSドック（OBS/dock）、`/...` 送信画面（Camera/www）

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::admin;
use crate::ai::{AiState, DirectorResult, VisionResult};
use crate::config::{
    is_secret_reference, AudioConfig, Camera, Config, ConfigStore, IceServer, QualityControl,
    Reconnect, SrtConfig, VideoConfig,
};
use crate::docker::ContainerInfo;
use crate::events::EventLog;
use crate::health::{CamHealth, HealthView};
use crate::mediamtx::{self, PathInfo};
use crate::monitoring;
use crate::secrets::SecretResolver;
use crate::whip_proxy;

pub struct AppState {
    pub store: ConfigStore,
    pub http: reqwest::Client,
    /// カメラIDごとの送信元（送信画面のクライアントID）。重複接続の判定に使う。
    pub owners: Mutex<HashMap<String, whip_proxy::Owner>>,
    /// Docker・コンテナの状態（定期的に更新する）。
    pub runtime: RwLock<RuntimeStatus>,
    /// カメラの追加・削除・コンテナ操作を1つずつ実行するためのロック。
    pub ops: tokio::sync::Mutex<()>,
    /// カメラごとの不調・自動復旧の記録。
    pub health: Mutex<HashMap<String, CamHealth>>,
    /// 直近の出来事。
    pub events: EventLog,
    pub secrets: SecretResolver,
    pub ai: AiState,
}

impl AppState {
    pub fn new(store: ConfigStore) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let secrets = SecretResolver::new(&store.get().secrets.onepassword_connect)?;
        Ok(Self {
            store,
            http,
            owners: Mutex::new(HashMap::new()),
            runtime: RwLock::new(RuntimeStatus::default()),
            ops: tokio::sync::Mutex::new(()),
            health: Mutex::new(HashMap::new()),
            events: Default::default(),
            secrets,
            ai: AiState::default(),
        })
    }

    pub fn container(&self, name: &str) -> Option<ContainerInfo> {
        self.runtime
            .read()
            .expect("runtime lock")
            .containers
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    pub fn health_view(&self, camera_id: &str) -> HealthView {
        self.health
            .lock()
            .expect("health lock")
            .get(camera_id)
            .cloned()
            .unwrap_or_default()
            .view()
    }
}

pub type SharedState = Arc<AppState>;

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeStatus {
    pub docker_version: Option<String>,
    /// Dockerに接続できない場合の理由。
    pub docker_error: Option<String>,
    pub image_ready: bool,
    pub image_error: Option<String>,
    pub containers: Vec<ContainerInfo>,
    /// 直近のコンテナ確認（reconcile）の結果。
    pub last_reconcile: Option<String>,
    /// 直近のコンテナ確認の時刻（UNIX秒）。
    pub last_reconcile_at: Option<u64>,
}

/// APIエラー。本文は `{"error": "..."}`。
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

pub fn router(state: SharedState) -> Router {
    let cfg = state.store.get();
    let camera_dir = cfg.server.camera_dir.clone();
    let obs_dir = cfg.server.obs_dir.clone();
    let admin_dir = cfg.server.admin_dir.clone();

    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/config", get(client_config))
        .route("/api/v1/status", get(status))
        .route("/api/v1/ai", get(ai_status))
        .route("/api/v1/cameras/{id}/whip", post(whip_proxy::whip_create))
        .route(
            "/api/v1/cameras/{id}/whip/{session}",
            patch(whip_proxy::whip_update).delete(whip_proxy::whip_delete),
        )
        .route("/api/v1/admin/overview", get(admin::overview))
        .route("/api/v1/admin/reconcile", post(admin::reconcile))
        .route("/api/v1/admin/cameras", post(admin::add_camera))
        .route(
            "/api/v1/admin/cameras/{id}",
            patch(admin::rename_camera).delete(admin::delete_camera),
        )
        .route("/api/v1/admin/cameras/{id}/restart", post(admin::restart_camera))
        .route("/api/v1/admin/cameras/{id}/record", post(admin::set_recording))
        .route("/api/v1/admin/cameras/{id}/diagnose", post(admin::diagnose_camera))
        .route("/api/v1/admin/cameras/{id}/logs", get(admin::camera_logs))
        .route("/api/v1/admin/cameras/{id}/whep", post(whip_proxy::whep_create))
        .route(
            "/api/v1/admin/cameras/{id}/whep/{session}",
            patch(whip_proxy::whep_update).delete(whip_proxy::whep_delete),
        );
    if cfg.monitoring.prometheus.enabled {
        router = router.route(&cfg.monitoring.prometheus.path, get(monitoring::metrics::handler));
    }
    router
        .nest_service("/admin", ServeDir::new(admin_dir))
        .nest_service("/obs", ServeDir::new(obs_dir))
        .fallback_service(ServeDir::new(camera_dir))
        // 画面の更新が確実に反映されるよう、キャッシュは都度検証させる。
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// 送信画面が使う設定。サーバー内部の値（証明書・内部URL等）は含めない。
#[derive(Serialize)]
struct ClientConfig<'a> {
    version: &'static str,
    cameras: &'a [Camera],
    video: &'a VideoConfig,
    audio: &'a AudioConfig,
    srt: &'a SrtConfig,
    quality_control: &'a QualityControl,
    reconnect: &'a Reconnect,
    ice_servers: Vec<IceServer>,
}

/// ブラウザへ渡す ICE サーバー。TURN の認証情報が参照なら解決し、解決できなければ除く。
async fn browser_ice_servers(state: &AppState, cfg: &Config) -> Vec<IceServer> {
    let mut out = Vec::new();
    for (i, ice) in cfg.ice_servers.iter().enumerate() {
        let mut ice = ice.clone();
        if let Some(c) = ice.credential.as_deref().filter(|c| is_secret_reference(c)) {
            match state
                .secrets
                .resolve_or_warn(&format!("ice_servers[{i}].credential"), c)
                .await
            {
                Some(v) => ice.credential = Some(v),
                None => continue,
            }
        }
        out.push(ice);
    }
    out
}

async fn client_config(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    let ice_servers = browser_ice_servers(&state, &cfg).await;
    Json(ClientConfig {
        version: env!("CARGO_PKG_VERSION"),
        cameras: &cfg.cameras,
        video: &cfg.video,
        audio: &cfg.audio,
        srt: &cfg.srt,
        quality_control: &cfg.quality_control,
        reconnect: &cfg.reconnect,
        ice_servers,
    })
    .into_response()
}

#[derive(Debug, Serialize)]
pub struct CameraStatus {
    pub id: String,
    pub name: String,
    pub webrtc_udp_port: u16,
    pub srt_port: u16,
    pub srt_url: String,
    /// カメラのメディア処理コンテナ（未作成なら null）。
    pub container: Option<ContainerInfo>,
    /// コンテナの不調と自動復旧の状況。
    pub health: HealthView,
    /// コンテナのMediaMTXから状態を取得できない場合の理由。
    pub media_error: Option<String>,
    /// カメラPCからWebRTCで受信中。
    pub receiving: bool,
    /// OBS向けストリーム（H.264・AAC）を提供中。
    pub output_ready: bool,
    pub input_codecs: Vec<String>,
    pub output_codecs: Vec<String>,
    /// 受信中のコーデックがH.264以外なら再エンコードしている。
    pub reencoding: bool,
    pub inbound_bytes: u64,
    /// SRTで読み取り中の接続数（OBS）。
    pub srt_readers: usize,
    /// 管理Webの映像確認・モニターの接続数。
    pub preview_viewers: usize,
    /// 録画する設定か。
    pub recording: bool,
    /// AIの映像解析の最新結果。
    pub ai_vision: Option<VisionResult>,
}

/// MediaMTX へ接続できない理由を、利用者に分かる言葉にする。
fn media_error_text(cfg: &Config, err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.contains("dns error") || text.contains("failed to lookup address") {
        format!(
            "コンテナに名前で接続できません。コンテナが再起動を繰り返しているか、ネットワーク {} から外れています（自動で確認・復旧します）",
            cfg.media.docker_network
        )
    } else if text.contains("Connection refused") || text.contains("connection refused") {
        "コンテナは起動していますが、メディア処理（MediaMTX）がまだ応答しません".to_string()
    } else {
        format!("MediaMTXに接続できません: {text}")
    }
}

/// 全カメラの状態を、各カメラのコンテナのMediaMTX APIから取得する。
pub async fn camera_statuses(state: &AppState, cfg: &Config) -> Vec<CameraStatus> {
    let mut result = Vec::with_capacity(cfg.cameras.len());
    for cam in &cfg.cameras {
        let container = state.container(&cfg.container_name(&cam.id));
        let health = state.health_view(&cam.id);
        let running = container.as_ref().is_some_and(|c| c.state == "running");
        let (paths, media_error) = if !running {
            let reason = health
                .problem
                .clone()
                .unwrap_or_else(|| match &container {
                    None => "コンテナがありません（「再起動」または「再適用」で作成します）".to_string(),
                    Some(_) => "コンテナが起動していません".to_string(),
                });
            (Vec::new(), Some(reason))
        } else {
            match mediamtx::list_paths(&state.http, &mediamtx::api_base(cfg, &cam.id)).await {
                Ok(paths) => (paths, None),
                Err(e) => (
                    Vec::new(),
                    Some(health.problem.clone().unwrap_or_else(|| media_error_text(cfg, &e))),
                ),
            }
        };
        let find = |name: &str| -> Option<&PathInfo> { paths.iter().find(|p| p.name == name) };
        let input = find(&mediamtx::ingest_path(&cam.id));
        let output = find(&cam.id);
        let receiving = input.is_some_and(|p| p.is_available());
        let input_codecs = input.map(|p| p.codecs()).unwrap_or_default();

        result.push(CameraStatus {
            id: cam.id.clone(),
            name: cam.name.clone(),
            webrtc_udp_port: cam.webrtc_udp_port,
            srt_port: cam.srt_port,
            srt_url: cfg.srt_url(cam),
            container,
            health,
            media_error,
            receiving,
            output_ready: output.is_some_and(|p| p.is_available()),
            reencoding: receiving && !input_codecs.iter().any(|c| c == "H264"),
            input_codecs,
            output_codecs: output.map(|p| p.codecs()).unwrap_or_default(),
            inbound_bytes: input.map(|p| p.inbound_bytes()).unwrap_or(0),
            srt_readers: output.map(|p| p.srt_readers()).unwrap_or(0),
            preview_viewers: input.map(|p| p.webrtc_readers()).unwrap_or(0),
            recording: cfg.camera_records(cam),
            ai_vision: state.ai.vision(&cam.id),
        });
    }
    result
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    docker_error: Option<String>,
    ai_director: Option<DirectorResult>,
    cameras: Vec<CameraStatus>,
}

async fn status(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    let cameras = camera_statuses(&state, &cfg).await;
    let docker_error = state.runtime.read().expect("runtime lock").docker_error.clone();
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        docker_error,
        ai_director: state.ai.director(),
        cameras,
    })
    .into_response()
}

#[derive(Serialize)]
struct AiStatusResponse {
    enabled: bool,
    provider: crate::config::AiProvider,
    diagnosis: bool,
    vision: bool,
    director: bool,
    director_result: Option<DirectorResult>,
    vision_results: HashMap<String, VisionResult>,
    last_error: Option<String>,
}

async fn ai_status(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    let ai = &cfg.ai;
    let vision_results = cfg
        .cameras
        .iter()
        .filter_map(|c| state.ai.vision(&c.id).map(|v| (c.id.clone(), v)))
        .collect();
    Json(AiStatusResponse {
        enabled: ai.enabled,
        provider: ai.provider,
        diagnosis: ai.enabled && ai.diagnosis.enabled,
        vision: ai.enabled && ai.vision.enabled,
        director: ai.enabled && ai.director.enabled,
        director_result: state.ai.director(),
        vision_results,
        last_error: state.ai.last_error(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    #[test]
    fn explains_dns_errors() {
        let cfg = sample();
        let err = anyhow::anyhow!(
            "error sending request: client error (Connect): dns error: failed to lookup address information: Name or service not known"
        );
        let text = media_error_text(&cfg, &err);
        assert!(text.contains("再起動を繰り返しているか、ネットワーク amore_net から外れています"), "{text}");
    }
}
