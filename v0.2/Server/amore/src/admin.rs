//! 管理Web用のAPIと、カメラコンテナの監視。
//!
//! - `GET    /api/v1/admin/overview`               Server・Docker・全カメラの状態
//! - `POST   /api/v1/admin/cameras`                カメラ追加（AMoRe.yamlへ保存し、コンテナを自動作成）
//! - `PATCH  /api/v1/admin/cameras/{id}`           表示名の変更
//! - `DELETE /api/v1/admin/cameras/{id}`           カメラ削除（コンテナも削除）
//! - `POST   /api/v1/admin/cameras/{id}/restart`   コンテナの再起動
//! - `GET    /api/v1/admin/cameras/{id}/logs`      コンテナのログ
//! - `POST   /api/v1/admin/reconcile`              AMoRe.yaml とコンテナを再度そろえる
//!
//! v0.2 では管理Webに認証を設けない（LAN内での利用が前提。利用者の判断による）。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{camera_statuses, ApiError, AppState, CameraStatus, RuntimeStatus, SharedState};
use crate::config::{camera_id_error, camera_name_error, Camera, Config};
use crate::docker::{self, ReconcileReport};

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// コンテナ一覧を取り直して状態を更新する。
async fn refresh_runtime(state: &AppState, report: Option<&ReconcileReport>) {
    let list = docker::list_managed().await;
    let mut rt = state.runtime.write().expect("runtime lock");
    match list {
        Ok(containers) => {
            rt.containers = containers;
            rt.docker_error = None;
        }
        Err(e) => rt.docker_error = Some(format!("{e:#}")),
    }
    if let Some(r) = report {
        rt.last_reconcile = Some(r.summary());
        rt.last_reconcile_at = Some(now_unix());
    }
}

/// AMoRe.yaml の内容にコンテナをそろえる。呼び出し側で `state.ops` をロックしておくこと。
async fn apply(state: &AppState, cfg: &Config) -> ReconcileReport {
    let report = match docker::reconcile(cfg).await {
        Ok(report) => report,
        Err(e) => {
            tracing::error!("コンテナを確認できません: {e:#}");
            ReconcileReport {
                errors: vec![format!("{e:#}")],
                ..Default::default()
            }
        }
    };
    refresh_runtime(state, Some(&report)).await;
    report
}

/// 起動時にイメージを用意してコンテナをそろえ、その後は状態を定期的に更新する。
pub async fn supervise(state: SharedState) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut prepared = false;
    let mut round: u32 = 0;
    loop {
        tick.tick().await;
        // 準備が終わるまでは30秒ごとに再試行する（Docker未接続・イメージ取得失敗に備える）。
        if !prepared && round % 6 == 0 {
            prepared = prepare(&state).await;
        } else {
            refresh_runtime(&state, None).await;
        }
        round = round.wrapping_add(1);
    }
}

async fn prepare(state: &AppState) -> bool {
    match docker::ping().await {
        Ok(version) => {
            let mut rt = state.runtime.write().expect("runtime lock");
            rt.docker_version = Some(version);
            rt.docker_error = None;
        }
        Err(e) => {
            tracing::error!("Dockerに接続できません: {e:#}");
            state.runtime.write().expect("runtime lock").docker_error = Some(format!("{e:#}"));
            return false;
        }
    }

    let image = state.store.get().media.image.clone();
    let pulled = docker::ensure_image(&image).await;
    {
        let mut rt = state.runtime.write().expect("runtime lock");
        rt.image_ready = pulled.is_ok();
        rt.image_error = pulled.as_ref().err().map(|e| format!("{e:#}"));
    }
    if let Err(e) = pulled {
        tracing::error!("イメージを取得できません: {e:#}");
        return false;
    }

    let _ops = state.ops.lock().await;
    let cfg = state.store.get();
    let report = apply(state, &cfg).await;
    report.errors.is_empty()
}

#[derive(Serialize)]
pub struct Overview {
    version: &'static str,
    public_host: String,
    sender_url: String,
    dock_url: String,
    image: String,
    max_cameras: usize,
    /// 次に追加する場合のカメラID（上限・空きポートなしなら null）。
    next_camera_id: Option<String>,
    runtime: RuntimeStatus,
    cameras: Vec<CameraStatus>,
}

/// `GET /api/v1/admin/overview`
pub async fn overview(State(state): State<SharedState>) -> Json<Overview> {
    let cfg = state.store.get();
    let cameras = camera_statuses(&state, &cfg).await;
    let runtime = state.runtime.read().expect("runtime lock").clone();
    let room = cfg.cameras.len() < cfg.limits.max_cameras && cfg.next_free_ports().is_some();
    Json(Overview {
        version: env!("CARGO_PKG_VERSION"),
        public_host: cfg.server.public_host.clone(),
        sender_url: format!("https://{}/", cfg.server.public_host),
        dock_url: format!("https://{}/obs/", cfg.server.public_host),
        image: cfg.media.image.clone(),
        max_cameras: cfg.limits.max_cameras,
        next_camera_id: room.then(|| cfg.next_camera_id()),
        runtime,
        cameras,
    })
}

fn report_json(report: &ReconcileReport) -> serde_json::Value {
    json!({ "summary": report.summary(), "errors": report.errors })
}

#[derive(Deserialize)]
pub struct AddCamera {
    /// 省略時は未使用の `camN` を割り当てる。
    #[serde(default)]
    id: Option<String>,
    name: String,
}

/// `POST /api/v1/admin/cameras`
pub async fn add_camera(
    State(state): State<SharedState>,
    Json(req): Json<AddCamera>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let _ops = state.ops.lock().await;
    let cfg = state.store.get();

    if cfg.cameras.len() >= cfg.limits.max_cameras {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("カメラは最大 {} 台までです（limits.max_cameras）", cfg.limits.max_cameras),
        ));
    }
    let id = req
        .id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.next_camera_id());
    if let Some(e) = camera_id_error(&id) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, e));
    }
    if cfg.camera(&id).is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("カメラID {id:?} は既に登録されています"),
        ));
    }
    let name = req.name.trim().to_string();
    if let Some(e) = camera_name_error(&name) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, e));
    }
    let (webrtc_udp_port, srt_port) = cfg.next_free_ports().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "割り当てられる空きポートがありません（media.webrtc_udp_ports / media.srt_ports）",
        )
    })?;

    let camera = Camera {
        id: id.clone(),
        name,
        webrtc_udp_port,
        srt_port,
    };
    let mut cameras = cfg.cameras.clone();
    cameras.push(camera.clone());
    let next = state
        .store
        .update_cameras(cameras)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    tracing::info!(camera = %id, "カメラを追加しました");

    let report = apply(&state, &next).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "camera": camera,
            "srt_url": next.srt_url(&camera),
            "container": next.container_name(&id),
            "reconcile": report_json(&report),
        })),
    ))
}

#[derive(Deserialize)]
pub struct RenameCamera {
    name: String,
}

/// `PATCH /api/v1/admin/cameras/{id}`（表示名の変更。コンテナは作り直さない）
pub async fn rename_camera(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<RenameCamera>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _ops = state.ops.lock().await;
    let cfg = state.store.get();
    let name = req.name.trim().to_string();
    if let Some(e) = camera_name_error(&name) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, e));
    }
    let mut cameras = cfg.cameras.clone();
    let camera = cameras
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| not_found(&id))?;
    camera.name = name;
    let camera = camera.clone();
    state
        .store
        .update_cameras(cameras)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    tracing::info!(camera = %id, name = %camera.name, "表示名を変更しました");
    Ok(Json(json!({ "camera": camera })))
}

/// `DELETE /api/v1/admin/cameras/{id}`
pub async fn delete_camera(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _ops = state.ops.lock().await;
    let cfg = state.store.get();
    if cfg.camera(&id).is_none() {
        return Err(not_found(&id));
    }
    let cameras: Vec<Camera> = cfg.cameras.iter().filter(|c| c.id != id).cloned().collect();
    let next = state
        .store
        .update_cameras(cameras)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    state.owners.lock().expect("owners lock").remove(&id);
    tracing::info!(camera = %id, "カメラを削除しました");

    // 登録から外れたコンテナは reconcile が削除する。
    let report = apply(&state, &next).await;
    Ok(Json(json!({ "deleted": id, "reconcile": report_json(&report) })))
}

/// `POST /api/v1/admin/cameras/{id}/restart`
pub async fn restart_camera(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _ops = state.ops.lock().await;
    let cfg = state.store.get();
    if cfg.camera(&id).is_none() {
        return Err(not_found(&id));
    }
    let name = cfg.container_name(&id);
    let result = if state.container(&name).is_some() {
        docker::restart(&name).await
    } else {
        // コンテナが無い場合は作成する。
        docker::reconcile(&cfg).await.map(|_| ())
    };
    refresh_runtime(&state, None).await;
    result.map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "restarted": name })))
}

/// `GET /api/v1/admin/cameras/{id}/logs`
pub async fn camera_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    if cfg.camera(&id).is_none() {
        return Err(not_found(&id));
    }
    let text = docker::logs(&cfg.container_name(&id), 300)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

/// `POST /api/v1/admin/reconcile`
pub async fn reconcile(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let _ops = state.ops.lock().await;
    let cfg = state.store.get();
    let report = apply(&state, &cfg).await;
    Json(report_json(&report))
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        format!("カメラID {id:?} は登録されていません"),
    )
}
