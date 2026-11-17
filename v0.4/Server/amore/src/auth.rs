//! 画面ごとの入室パスワード（`access.yaml`）。
//!
//! - `GET  /api/v1/access`  どの画面でパスワードが必要か（と、送られたトークンが有効か）
//! - `POST /api/v1/login`   `{scope, password}` が正しければ期限付きの入室トークンを返す
//!
//! 入室トークンは `範囲.期限.署名`（HMAC-SHA256）。署名の鍵は冗長構成では `cluster.shared_secret` から作るため、
//! どの Server で入室したトークンも全 Server で使える（共通IPが移った後・dual の送信先でも入り直し不要）。
//! 冗長構成でなければ起動ごとに作る鍵を使う（AMoRe を再起動すると入り直しになる）。
//! パスワードを変えると、それまでのトークンは使えなくなる（署名にパスワードを含めるため）。
//!
//! 画面（HTML・JS・CSS）自体には秘密を含めないため配信は制限せず、データ・映像・操作の API を守る。
//! API ごとの範囲は [`allowed_scopes`] を参照。admin で入室すれば全ての API を使える。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::api::{ApiError, AppState, SharedState};
use crate::config::{parse_duration_secs, Access, Config};

type HmacSha256 = Hmac<Sha256>;

/// 入室の範囲。
pub const SCOPES: [&str; 4] = ["admin", "camera", "monitor", "dock"];

/// 連続して失敗した場合に受け付けを止める回数・時間（総当たり対策）。
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);
const LOCK_FOR: Duration = Duration::from_secs(300);
/// 失敗時に待たせる時間。
const FAILURE_DELAY: Duration = Duration::from_secs(1);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mac(key: &[u8], parts: &[&[u8]]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC は任意長の鍵を受け付ける");
    for p in parts {
        m.update(p);
        // 区切りを入れ、連結の仕方で同じ入力にならないようにする。
        m.update(&[0]);
    }
    m
}

fn hmac_bytes(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    mac(key, parts).finalize().into_bytes().to_vec()
}

/// 共通の秘密から用途ごとの鍵を作る。
pub fn derive_key(secret: &str, purpose: &str) -> Vec<u8> {
    hmac_bytes(secret.as_bytes(), &[b"amore-v0.4", purpose.as_bytes()])
}

/// 冗長構成でない場合の鍵（起動ごとに作る）。
fn random_key() -> Vec<u8> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return buf.to_vec();
    }
    // /dev/urandom が無い環境（開発用）。
    let seed = format!("{:?}{}{:?}", SystemTime::now(), std::process::id(), Instant::now());
    hmac_bytes(seed.as_bytes(), &[b"amore-local-key"])
}

/// パスワードを定数時間で比較する（長さの違いも比較時間に出さない）。
pub fn password_matches(input: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let key = b"amore-password-compare";
    let want = hmac_bytes(key, &[expected.as_bytes()]);
    mac(key, &[input.as_bytes()]).verify_slice(&want).is_ok()
}

/// 入室トークンを作る。`password` は範囲のパスワード（変更で古いトークンを無効にする）。
pub fn issue_token(key: &[u8], scope: &str, password: &str, expires_at: u64) -> String {
    let exp = expires_at.to_string();
    let sig = hmac_bytes(key, &[scope.as_bytes(), exp.as_bytes(), password.as_bytes()]);
    format!("{scope}.{exp}.{}", URL_SAFE_NO_PAD.encode(sig))
}

/// トークンの範囲と期限を取り出す（署名は確かめない）。
pub fn token_claims(token: &str) -> Option<(&str, u64)> {
    let mut parts = token.splitn(3, '.');
    let scope = parts.next()?;
    let exp = parts.next()?.parse().ok()?;
    parts.next()?;
    SCOPES.contains(&scope).then_some((scope, exp))
}

/// トークンの署名と期限を確かめる。
pub fn verify_token(key: &[u8], token: &str, password: &str, now: u64) -> bool {
    let Some((scope, exp)) = token_claims(token) else {
        return false;
    };
    if exp <= now {
        return false;
    }
    let Some(sig) = token.rsplit('.').next().and_then(|s| URL_SAFE_NO_PAD.decode(s).ok()) else {
        return false;
    };
    mac(key, &[scope.as_bytes(), exp.to_string().as_bytes(), password.as_bytes()])
        .verify_slice(&sig)
        .is_ok()
}

/// この API を使う画面（admin 以外）。None はパスワードの対象外。
fn endpoint_users(path: &str) -> Option<&'static [&'static str]> {
    if path == "/api/v1/login" || path == "/api/v1/access" || path.starts_with("/api/v1/cluster/") {
        return None;
    }
    if path.starts_with("/api/v1/admin/cameras/") && path.contains("/whep") {
        return Some(&["monitor"]);
    }
    if path.starts_with("/api/v1/admin/") {
        return Some(&[]);
    }
    match path {
        "/api/v1/status" | "/api/v1/ai" => Some(&["monitor", "dock"]),
        // 送信画面のほか、管理Web・モニターも ICE サーバーの取得に使う。
        "/api/v1/config" => Some(&["camera", "monitor"]),
        _ if path.starts_with("/api/v1/cameras/") => Some(&["camera"]),
        _ => None,
    }
}

/// この API に必要な入室範囲。None なら誰でも使える。
/// 使う画面のどれかがパスワード不要なら誰でも使える（その画面が動かなくならないように）。
pub fn allowed_scopes(access: &Access, path: &str) -> Option<Vec<&'static str>> {
    let users = endpoint_users(path)?;
    let open = if users.is_empty() {
        !access.admin.password_required
    } else {
        users
            .iter()
            .any(|s| access.scope(s).is_some_and(|a| !a.password_required))
    };
    if open {
        return None;
    }
    let mut scopes = vec!["admin"];
    scopes.extend(users.iter().copied());
    Some(scopes)
}

/// 入室の状態（鍵・失敗の記録）。
pub struct AuthState {
    local_key: Vec<u8>,
    failures: std::sync::Mutex<HashMap<IpAddr, Failures>>,
}

#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    first: Instant,
    locked_until: Option<Instant>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            local_key: random_key(),
            failures: Default::default(),
        }
    }
}

impl AuthState {
    /// 受け付けを止めている接続元なら残り時間。
    fn locked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let map = self.failures.lock().expect("auth lock");
        let until = map.get(&ip)?.locked_until?;
        (until > now).then(|| until - now)
    }

    fn record_failure(&self, ip: IpAddr, now: Instant) -> bool {
        let mut map = self.failures.lock().expect("auth lock");
        // 古い記録は消す（記録が増え続けないように）。
        map.retain(|_, f| now.duration_since(f.first) < FAILURE_WINDOW || f.locked_until.is_some_and(|u| u > now));
        let f = map.entry(ip).or_insert(Failures {
            count: 0,
            first: now,
            locked_until: None,
        });
        if now.duration_since(f.first) >= FAILURE_WINDOW || f.locked_until.is_some_and(|u| u <= now) {
            *f = Failures {
                count: 0,
                first: now,
                locked_until: None,
            };
        }
        f.count += 1;
        if f.count >= MAX_FAILURES {
            f.locked_until = Some(now + LOCK_FOR);
            return true;
        }
        false
    }

    fn clear(&self, ip: IpAddr) {
        self.failures.lock().expect("auth lock").remove(&ip);
    }
}

/// 入室トークンの署名鍵。冗長構成では `cluster.shared_secret` から作る。
pub async fn token_key(state: &AppState, cfg: &Config) -> Result<Vec<u8>> {
    if !cfg.cluster.enabled {
        return Ok(state.auth.local_key.clone());
    }
    let secret = state
        .secrets
        .resolve(&cfg.cluster.shared_secret)
        .await
        .context("cluster.shared_secret を取得できません")?;
    anyhow::ensure!(!secret.is_empty(), "cluster.shared_secret が空です");
    Ok(derive_key(&secret, "access-token"))
}

/// 範囲のパスワード（秘密参照を解決したもの）。
async fn scope_password(state: &AppState, cfg: &Config, scope: &str) -> Result<String> {
    let access = cfg.access.scope(scope).context("範囲が不正です")?;
    let value = state
        .secrets
        .resolve(&access.password)
        .await
        .with_context(|| format!("access.{scope}.password を取得できません"))?;
    Ok(value)
}

/// `Authorization: Bearer <トークン>` が、許可された範囲のどれかで有効か。
async fn bearer_allows(state: &AppState, cfg: &Config, headers: &HeaderMap, allowed: &[&str]) -> bool {
    let Some(token) = bearer(headers) else {
        return false;
    };
    let Some((scope, _)) = token_claims(token) else {
        return false;
    };
    if !allowed.contains(&scope) {
        return false;
    }
    let (Ok(key), Ok(password)) = (token_key(state, cfg).await, scope_password(state, cfg, scope).await) else {
        return false;
    };
    verify_token(&key, token, &password, now_unix())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.len() <= 512)
}

/// API の入室確認（axum のミドルウェア）。
pub async fn guard(State(state): State<SharedState>, request: Request, next: Next) -> Response {
    let cfg = state.store.get();
    let path = request.uri().path().to_string();
    // CORS の事前確認（OPTIONS）は認証情報を付けずに来るため通す。
    if *request.method() == axum::http::Method::OPTIONS {
        return next.run(request).await;
    }
    let Some(allowed) = allowed_scopes(&cfg.access, &path) else {
        return next.run(request).await;
    };
    if bearer_allows(&state, &cfg, request.headers(), &allowed).await {
        return next.run(request).await;
    }
    let body = Json(serde_json::json!({
        "error": "この操作には入室（パスワード）が必要です",
        "login": allowed,
    }));
    (StatusCode::UNAUTHORIZED, body).into_response()
}

#[derive(Serialize)]
pub struct AccessInfo {
    /// 範囲ごとにパスワードが必要か。
    required: HashMap<&'static str, bool>,
    session_ttl_sec: u64,
    /// 送られたトークンが有効なら、その範囲。
    current: Option<String>,
}

/// `GET /api/v1/access`
pub async fn access_info(State(state): State<SharedState>, headers: HeaderMap) -> Json<AccessInfo> {
    let cfg = state.store.get();
    let required = cfg
        .access
        .scopes()
        .into_iter()
        .map(|(name, a)| (name, a.password_required))
        .collect();
    let mut current = None;
    if let Some((scope, _)) = bearer(&headers).and_then(token_claims) {
        if bearer_allows(&state, &cfg, &headers, &[scope]).await {
            current = Some(scope.to_string());
        }
    }
    Json(AccessInfo {
        required,
        session_ttl_sec: parse_duration_secs(&cfg.access.session_ttl).unwrap_or(43_200),
        current,
    })
}

#[derive(Deserialize)]
pub struct LoginRequest {
    scope: String,
    password: String,
}

/// `POST /api/v1/login`
pub async fn login(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = state.store.get();
    let ip = peer.ip();
    let now = Instant::now();
    if let Some(wait) = state.auth.locked(ip, now) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "パスワードの誤りが続いたため、{}秒後まで受け付けません",
                wait.as_secs().max(1)
            ),
        ));
    }
    let Some(access) = cfg.access.scope(&req.scope) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "範囲が不正です"));
    };
    if !access.password_required {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{} はパスワード不要の設定です", req.scope),
        ));
    }
    let (key, password) = match (
        token_key(&state, &cfg).await,
        scope_password(&state, &cfg, &req.scope).await,
    ) {
        (Ok(k), Ok(p)) if !p.is_empty() => (k, p),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("入室を確認できません: {e:#}");
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "パスワードを確認できません（Server の秘密情報の設定を確認してください）",
            ));
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "パスワードが設定されていません（access.yaml を確認してください）",
            ))
        }
    };
    if !password_matches(&req.password, &password) {
        let locked = state.auth.record_failure(ip, now);
        tracing::warn!(scope = %req.scope, from = %ip, "入室のパスワードが違います");
        tokio::time::sleep(FAILURE_DELAY).await;
        let message = if locked {
            format!("パスワードが違います。誤りが続いたため {} 分間受け付けません", LOCK_FOR.as_secs() / 60)
        } else {
            "パスワードが違います".to_string()
        };
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, message));
    }
    state.auth.clear(ip);
    let ttl = parse_duration_secs(&cfg.access.session_ttl).unwrap_or(43_200);
    let expires_at = now_unix() + ttl;
    tracing::info!(scope = %req.scope, from = %ip, "入室しました");
    Ok(Json(serde_json::json!({
        "scope": req.scope,
        "token": issue_token(&key, &req.scope, &password, expires_at),
        "expires_at": expires_at,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_signed_scoped_and_expire() {
        let key = derive_key("shared", "access-token");
        let token = issue_token(&key, "monitor", "pw", 1_000);
        assert_eq!(token_claims(&token), Some(("monitor", 1_000)));
        assert!(verify_token(&key, &token, "pw", 999));
        assert!(!verify_token(&key, &token, "pw", 1_000), "期限切れ");
        assert!(!verify_token(&key, &token, "changed", 999), "パスワード変更で無効");
        assert!(!verify_token(&derive_key("other", "access-token"), &token, "pw", 999), "別の鍵");
        // 範囲を書き換えると署名が合わない。
        let forged = token.replacen("monitor", "admin", 1);
        assert!(!verify_token(&key, &forged, "pw", 999));
        let extended = token.replacen("1000", "9999", 1);
        assert!(!verify_token(&key, &extended, "pw", 999));
        assert!(token_claims("root.1.x").is_none());
        assert!(!verify_token(&key, "garbage", "pw", 0));
        // 冗長構成では同じ共通の秘密から同じ鍵になる（どの Server でも有効）。
        assert_eq!(derive_key("shared", "access-token"), key);
        assert_ne!(derive_key("shared", "cluster"), key);
    }

    #[test]
    fn compares_passwords() {
        assert!(password_matches("correct horse", "correct horse"));
        assert!(!password_matches("correct hors", "correct horse"));
        assert!(!password_matches("", ""), "空のパスワードでは入室させない");
    }

    #[test]
    fn decides_scopes_per_endpoint() {
        let mut access = Access::default();
        for p in ["/api/v1/config", "/api/v1/status", "/api/v1/admin/overview", "/api/v1/cameras/cam1/whip"] {
            assert_eq!(allowed_scopes(&access, p), None, "既定は全て OFF: {p}");
        }
        access.admin.password_required = true;
        access.monitor.password_required = true;
        assert_eq!(allowed_scopes(&access, "/api/v1/admin/overview"), Some(vec!["admin"]));
        assert_eq!(
            allowed_scopes(&access, "/api/v1/admin/cameras/cam1/whep"),
            Some(vec!["admin", "monitor"])
        );
        assert_eq!(
            allowed_scopes(&access, "/api/v1/admin/cameras/cam1/whep/abc"),
            Some(vec!["admin", "monitor"])
        );
        // ドックがパスワード不要なので状態は誰でも取得できる。
        assert_eq!(allowed_scopes(&access, "/api/v1/status"), None);
        // カメラ画面がパスワード不要なので設定は誰でも取得できる。
        assert_eq!(allowed_scopes(&access, "/api/v1/config"), None);
        assert_eq!(allowed_scopes(&access, "/api/v1/cameras/cam1/whip"), None);

        access.dock.password_required = true;
        access.camera.password_required = true;
        assert_eq!(
            allowed_scopes(&access, "/api/v1/status"),
            Some(vec!["admin", "monitor", "dock"])
        );
        assert_eq!(
            allowed_scopes(&access, "/api/v1/config"),
            Some(vec!["admin", "camera", "monitor"])
        );
        assert_eq!(
            allowed_scopes(&access, "/api/v1/cameras/cam1/whip/s1"),
            Some(vec!["admin", "camera"])
        );
        for p in ["/healthz", "/metrics", "/api/v1/login", "/api/v1/access", "/api/v1/cluster/state", "/index.html"] {
            assert_eq!(allowed_scopes(&access, p), None, "{p}");
        }
    }

    #[test]
    fn locks_out_after_repeated_failures() {
        let auth = AuthState::default();
        let ip: IpAddr = "10.40.0.50".parse().unwrap();
        let t = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!auth.record_failure(ip, t));
        }
        assert!(auth.locked(ip, t).is_none());
        assert!(auth.record_failure(ip, t));
        assert!(auth.locked(ip, t).is_some());
        assert!(auth.locked(ip, t + LOCK_FOR).is_none(), "時間が過ぎれば解除");
        assert!(auth.locked("10.40.0.51".parse().unwrap(), t).is_none(), "他の接続元は影響しない");
        auth.clear(ip);
        assert!(auth.locked(ip, t).is_none());
    }
}
