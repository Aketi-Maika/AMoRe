//! HTTPS API と静的ファイル配信。
//!
//! 公開（送信画面・OBSドック用）
//! - `GET  /healthz`                                   稼働確認
//! - `GET  /api/v1/config`                             送信画面向けの設定（AMoRe.yaml由来）
//! - `GET  /api/v1/status`                             カメラごとの受信・出力・コンテナ状態とSRT URL
//! - `POST /api/v1/cameras/{id}/whip`                  WHIP（whip_proxy.rs）
//!
//! 管理Web用（admin.rs）
//! - `/api/v1/admin/...`                               カメラの追加・削除・名前変更、コンテナ操作、映像確認(WHEP)
//!
//! 静的ファイル
//! - `/admin/...` 管理Web（Server/admin）、`/obs/...` OBSドック（OBS/dock）、`/...` 送信画面（Camera/www）

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
use crate::config::{
    AudioConfig, Camera, Config, ConfigStore, IceServer, QualityControl, Reconnect, SrtConfig,
    VideoConfig,
};
use crate::docker::ContainerInfo;
use crate::mediamtx::{self, PathInfo};
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
}

impl AppState {
    pub fn new(store: ConfigStore) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            store,
            http,
            owners: Mutex::new(HashMap::new()),
            runtime: RwLock::new(RuntimeStatus::default()),
            ops: tokio::sync::Mutex::new(()),
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

    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/config", get(client_config))
        .route("/api/v1/status", get(status))
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
        .route("/api/v1/admin/cameras/{id}/logs", get(admin::camera_logs))
        .route("/api/v1/admin/cameras/{id}/whep", post(whip_proxy::whep_create))
        .route(
            "/api/v1/admin/cameras/{id}/whep/{session}",
            patch(whip_proxy::whep_update).delete(whip_proxy::whep_delete),
        )
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
    ice_servers: &'a [IceServer],
}

async fn client_config(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    Json(ClientConfig {
        version: env!("CARGO_PKG_VERSION"),
        cameras: &cfg.cameras,
        video: &cfg.video,
        audio: &cfg.audio,
        srt: &cfg.srt,
        quality_control: &cfg.quality_control,
        reconnect: &cfg.reconnect,
        ice_servers: &cfg.ice_servers,
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
    /// 管理Webで映像確認中の接続数。
    pub preview_viewers: usize,
}

/// 全カメラの状態を、各カメラのコンテナのMediaMTX APIから取得する。
pub async fn camera_statuses(state: &AppState, cfg: &Config) -> Vec<CameraStatus> {
    let mut result = Vec::with_capacity(cfg.cameras.len());
    for cam in &cfg.cameras {
        let container = state.container(&cfg.container_name(&cam.id));
        let running = container.as_ref().is_some_and(|c| c.state == "running");
        let (paths, media_error) = if !running {
            (Vec::new(), Some("コンテナが起動していません".to_string()))
        } else {
            match mediamtx::list_paths(&state.http, &mediamtx::api_base(cfg, &cam.id)).await {
                Ok(paths) => (paths, None),
                Err(e) => (Vec::new(), Some(format!("MediaMTXに接続できません: {e:#}"))),
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
            media_error,
            receiving,
            output_ready: output.is_some_and(|p| p.is_available()),
            reencoding: receiving && !input_codecs.iter().any(|c| c == "H264"),
            input_codecs,
            output_codecs: output.map(|p| p.codecs()).unwrap_or_default(),
            inbound_bytes: input.map(|p| p.inbound_bytes()).unwrap_or(0),
            srt_readers: output.map(|p| p.srt_readers()).unwrap_or(0),
            preview_viewers: input.map(|p| p.webrtc_readers()).unwrap_or(0),
        });
    }
    result
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    docker_error: Option<String>,
    cameras: Vec<CameraStatus>,
}

async fn status(State(state): State<SharedState>) -> Response {
    let cfg = state.store.get();
    let cameras = camera_statuses(&state, &cfg).await;
    let docker_error = state.runtime.read().expect("runtime lock").docker_error.clone();
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        docker_error,
        cameras,
    })
    .into_response()
}
