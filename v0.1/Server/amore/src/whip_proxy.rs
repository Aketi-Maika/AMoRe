//! WHIP（送信）・WHEP（映像確認）の中継。
//!
//! 送信画面は `/api/v1/cameras/{id}/whip`、管理Webは `/api/v1/admin/cameras/{id}/whep` へSDPを送り、
//! AMoRe がそのカメラのコンテナ（MediaMTX）の `/{id}_in/whip`・`/{id}_in/whep` へ中継する。
//! WHIPの中継時は次を判定する。
//! - 未登録のカメラID → 404
//! - 別の送信画面が同じカメラIDで送信中 → 409（重複接続の拒否）
//! - 同じ送信画面からの再接続 → 許可（MediaMTXの overridePublisher で旧セッションを置換）

use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::{ApiError, SharedState};
use crate::config::Config;
use crate::mediamtx::{self, ingest_path};

/// 送信画面ごとに生成するクライアントIDのヘッダー。
pub const CLIENT_HEADER: &str = "x-amore-client";

/// 新規接続の直後は受信開始前でも所有者として扱い、同時接続の競合を防ぐ。
const CLAIM_GRACE: Duration = Duration::from_secs(15);

/// MediaMTXの応答から画面へ引き継ぐヘッダー。
const RELAY_HEADERS: [&str; 4] = ["content-type", "etag", "accept-patch", "link"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 送信（カメラPC → Server）
    Whip,
    /// 映像確認（Server → 管理Web）
    Whep,
}

impl Kind {
    fn segment(self) -> &'static str {
        match self {
            Kind::Whip => "whip",
            Kind::Whep => "whep",
        }
    }

    /// 画面側から見たセッションURLの接頭辞。
    fn public_prefix(self, camera_id: &str) -> String {
        match self {
            Kind::Whip => format!("/api/v1/cameras/{camera_id}/whip"),
            Kind::Whep => format!("/api/v1/admin/cameras/{camera_id}/whep"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Owner {
    pub client: String,
    pub since: Instant,
}

fn upstream_url(cfg: &Config, camera_id: &str, kind: Kind, session: Option<&str>) -> String {
    let base = format!(
        "{}/{}/{}",
        mediamtx::webrtc_base(cfg, camera_id),
        ingest_path(camera_id),
        kind.segment()
    );
    match session {
        Some(s) => format!("{base}/{s}"),
        None => base,
    }
}

/// MediaMTXの `Location`（例: `/cam1_in/whip/<session>`）をAMoReのURLへ書き換える。
pub fn rewrite_location(kind: Kind, camera_id: &str, location: &str) -> Option<String> {
    let marker = format!("/{}/", kind.segment());
    let idx = location.rfind(&marker)?;
    let session = location[idx + marker.len()..].split(['?', '#']).next()?;
    is_valid_token(session, 128).then(|| format!("{}/{session}", kind.public_prefix(camera_id)))
}

fn is_valid_token(s: &str, max_len: usize) -> bool {
    !s.is_empty()
        && s.len() <= max_len
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn client_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CLIENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() >= 8 && is_valid_token(v, 64))
        .map(str::to_string)
}

fn bad_gateway(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        format!("カメラのメディア処理コンテナに接続できません: {e}"),
    )
}

fn ensure_camera(cfg: &Config, id: &str) -> Result<(), ApiError> {
    if cfg.camera(id).is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("カメラID {id:?} は登録されていません（管理Webで追加してください）"),
        ));
    }
    Ok(())
}

fn ensure_sdp(headers: &HeaderMap) -> Result<(), ApiError> {
    let is_sdp = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/sdp"));
    if is_sdp {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type は application/sdp にしてください",
        ))
    }
}

fn ensure_session(session: &str) -> Result<(), ApiError> {
    if is_valid_token(session, 128) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::BAD_REQUEST, "セッションIDが不正です"))
    }
}

/// 送信の可否を判定する。拒否する場合は理由を返す。
pub fn check_owner(
    owner: Option<&Owner>,
    client: Option<&str>,
    receiving: bool,
    now: Instant,
) -> Result<(), String> {
    let same_client = matches!((owner, client), (Some(o), Some(c)) if o.client == c);
    if same_client {
        return Ok(());
    }
    if receiving {
        return Err("このカメラIDは別の送信画面から送信中です".to_string());
    }
    if let Some(o) = owner {
        if now.duration_since(o.since) < CLAIM_GRACE {
            return Err("このカメラIDは別の送信画面が接続処理中です".to_string());
        }
    }
    Ok(())
}

async fn relay(res: reqwest::Response, location: Option<String>) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for name in RELAY_HEADERS {
        for value in res.headers().get_all(name) {
            headers.append(name, value.clone());
        }
    }
    if let Some(loc) = location {
        if let Ok(v) = loc.parse() {
            headers.insert(header::LOCATION, v);
        }
    }
    let body = res.bytes().await.map_err(bad_gateway)?;
    Ok((status, headers, body).into_response())
}

/// SDPのofferをカメラのコンテナへ送り、応答を中継する。
async fn forward_offer(
    state: &SharedState,
    cfg: &Config,
    id: &str,
    kind: Kind,
    body: Bytes,
) -> Result<(bool, Response), ApiError> {
    let res = state
        .http
        .post(upstream_url(cfg, id, kind, None))
        .header(header::CONTENT_TYPE.as_str(), "application/sdp")
        .body(body)
        .send()
        .await
        .map_err(bad_gateway)?;
    let location = res
        .headers()
        .get(header::LOCATION.as_str())
        .and_then(|v| v.to_str().ok())
        .and_then(|loc| rewrite_location(kind, id, loc));
    let ok = res.status().is_success();
    if !ok {
        tracing::warn!(camera = %id, status = %res.status(), "MediaMTXが{}要求を拒否しました", kind.segment());
    }
    Ok((ok, relay(res, location).await?))
}

async fn forward_session(
    state: &SharedState,
    id: &str,
    kind: Kind,
    session: &str,
    method: reqwest::Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut req = state
        .http
        .request(method, upstream_url(&state.store.get(), id, kind, Some(session)));
    for name in [header::CONTENT_TYPE, header::IF_MATCH] {
        if let Some(v) = headers.get(&name) {
            req = req.header(name.as_str(), v.clone());
        }
    }
    let res = req.body(body).send().await.map_err(bad_gateway)?;
    relay(res, None).await
}

/// `POST /api/v1/cameras/{id}/whip`（送信開始）
pub async fn whip_create(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_sdp(&headers)?;
    let client = client_id(&headers);

    let receiving = mediamtx::is_available(
        &state.http,
        &mediamtx::api_base(&cfg, &id),
        &ingest_path(&id),
    )
    .await
    .map_err(bad_gateway)?;
    {
        let owners = state.owners.lock().expect("owners lock");
        check_owner(owners.get(&id), client.as_deref(), receiving, Instant::now())
            .map_err(|msg| ApiError::new(StatusCode::CONFLICT, msg))?;
    }

    let (ok, response) = forward_offer(&state, &cfg, &id, Kind::Whip, body).await?;
    if ok {
        if let Some(c) = &client {
            state.owners.lock().expect("owners lock").insert(
                id.clone(),
                Owner {
                    client: c.clone(),
                    since: Instant::now(),
                },
            );
        }
        tracing::info!(camera = %id, client = ?client, "送信を開始しました");
    }
    Ok(response)
}

/// `PATCH /api/v1/cameras/{id}/whip/{session}`（Trickle ICE）
pub async fn whip_update(
    State(state): State<SharedState>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_session(&session)?;
    forward_session(&state, &id, Kind::Whip, &session, reqwest::Method::PATCH, &headers, body).await
}

/// `DELETE /api/v1/cameras/{id}/whip/{session}`（送信停止）
pub async fn whip_delete(
    State(state): State<SharedState>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_session(&session)?;
    let response = forward_session(
        &state,
        &id,
        Kind::Whip,
        &session,
        reqwest::Method::DELETE,
        &headers,
        Bytes::new(),
    )
    .await?;
    if let Some(c) = client_id(&headers) {
        let mut owners = state.owners.lock().expect("owners lock");
        if owners.get(&id).is_some_and(|o| o.client == c) {
            owners.remove(&id);
        }
    }
    tracing::info!(camera = %id, "送信を終了しました");
    Ok(response)
}

/// `POST /api/v1/admin/cameras/{id}/whep`（映像確認の開始）
pub async fn whep_create(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_sdp(&headers)?;
    let (_, response) = forward_offer(&state, &cfg, &id, Kind::Whep, body).await?;
    Ok(response)
}

/// `PATCH /api/v1/admin/cameras/{id}/whep/{session}`
pub async fn whep_update(
    State(state): State<SharedState>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_session(&session)?;
    forward_session(&state, &id, Kind::Whep, &session, reqwest::Method::PATCH, &headers, body).await
}

/// `DELETE /api/v1/admin/cameras/{id}/whep/{session}`（映像確認の終了）
pub async fn whep_delete(
    State(state): State<SharedState>,
    Path((id, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cfg = state.store.get();
    ensure_camera(&cfg, &id)?;
    ensure_session(&session)?;
    forward_session(
        &state,
        &id,
        Kind::Whep,
        &session,
        reqwest::Method::DELETE,
        &headers,
        Bytes::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    #[test]
    fn rewrites_mediamtx_location() {
        assert_eq!(
            rewrite_location(Kind::Whip, "cam1", "/cam1_in/whip/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0")
                .as_deref(),
            Some("/api/v1/cameras/cam1/whip/0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0")
        );
        assert_eq!(
            rewrite_location(Kind::Whep, "cam2", "http://amore-cam-cam2:8889/cam2_in/whep/abc123?x=1")
                .as_deref(),
            Some("/api/v1/admin/cameras/cam2/whep/abc123")
        );
        assert_eq!(rewrite_location(Kind::Whip, "cam1", "/cam1_in/whip/../../api"), None);
        assert_eq!(rewrite_location(Kind::Whep, "cam1", "/cam1_in/whip/abc"), None);
    }

    #[test]
    fn builds_upstream_urls_per_camera() {
        let cfg = sample();
        assert_eq!(
            upstream_url(&cfg, "cam2", Kind::Whip, None),
            "http://amore-cam-cam2:8889/cam2_in/whip"
        );
        assert_eq!(
            upstream_url(&cfg, "cam1", Kind::Whep, Some("s1")),
            "http://amore-cam-cam1:8889/cam1_in/whep/s1"
        );
    }

    #[test]
    fn owner_rules() {
        let now = Instant::now();
        let owner = Owner {
            client: "client-aaaa".into(),
            since: now,
        };
        // 同じ送信画面からの再接続は受信中でも許可する。
        assert!(check_owner(Some(&owner), Some("client-aaaa"), true, now).is_ok());
        // 別の送信画面は受信中なら拒否する。
        assert!(check_owner(Some(&owner), Some("client-bbbb"), true, now).is_err());
        // 接続処理中（猶予時間内）も拒否する。
        assert!(check_owner(Some(&owner), Some("client-bbbb"), false, now).is_err());
        // 猶予時間を過ぎて受信もしていなければ許可する。
        let later = now + CLAIM_GRACE + Duration::from_secs(1);
        assert!(check_owner(Some(&owner), Some("client-bbbb"), false, later).is_ok());
        // 所有者不明でも受信中なら拒否し、未使用なら許可する。
        assert!(check_owner(None, None, true, now).is_err());
        assert!(check_owner(None, None, false, now).is_ok());
    }
}
