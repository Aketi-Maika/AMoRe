//! 拠点ごとの Server による冗長化（`cluster.yaml`）。
//!
//! - 状態交換：`heartbeat_sec` ごとに他の Server の `GET /api/v1/cluster/state` を取得する。
//!   中身は共通IPの受け持ち、カメラ一覧（改訂番号つき）、カメラごとの直接受信・出力・不調・中継の状況。
//! - カメラ一覧の同期：改訂番号が新しい一覧を採用し、コンテナをそろえる。この Server で変更した場合は
//!   `POST /api/v1/cluster/cameras` で他の Server へすぐに配る（届かなくても次の状態交換で取り込まれる）。
//! - 映像の中継：この Server が直接受けていないカメラは、直接受けている Server の OBS 向け出力（SRT）を
//!   自分の出力パス（`<id>`）へ常に取り込む（MediaMTX の取り込み元を API で切り替える。再エンコードなし）。
//!   これにより、OBS 用の共通IPがどの Server へ移っても、移った先はすでに全カメラの映像を持っている。
//!
//! Server 同士の API は `X-AMoRe-Cluster: <site>.<UNIX秒>.<署名>` で認証する（署名の鍵は `cluster.shared_secret`
//! から作る。秘密そのものは送らない）。時刻のずれが大きいと失敗するため、各 Server の時刻を NTP で合わせる。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::admin;
use crate::api::{camera_statuses, ApiError, AppState, CameraStatus, SharedState};
use crate::auth::derive_key;
use crate::config::{Camera, Config, Peer};
use crate::events::{self, Event};
use crate::health;
use crate::keepalived::{self, VipState};
use crate::mediamtx;

/// Server 同士の API の認証ヘッダー。
pub const HEADER: &str = "x-amore-cluster";
/// 署名の時刻として受け付けるずれ（秒）。
const CLOCK_SKEW_SEC: u64 = 30;
/// keepalived の設定ファイルを確認し直す間隔。
const KEEPALIVED_EVERY: Duration = Duration::from_secs(60);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// カメラ1台の、ある Server での状況。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraState {
    pub id: String,
    /// カメラPCから直接受けている（WebRTC）。
    pub direct: bool,
    /// OBS 向け出力を提供中（直接受信の変換、または中継）。
    pub output: bool,
    /// 中継の取り込み元の Server。
    pub relay_from: Option<String>,
    pub problem: Option<String>,
    pub recording: bool,
    /// OBS（SRT）の接続数。
    pub obs_readers: usize,
}

/// ある Server の状態（`GET /api/v1/cluster/state`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerState {
    pub site: String,
    pub version: String,
    pub vip: VipState,
    pub cameras_revision: u64,
    pub cameras: Vec<Camera>,
    pub statuses: Vec<CameraState>,
    pub time: u64,
}

impl PeerState {
    pub fn camera(&self, id: &str) -> Option<&CameraState> {
        self.statuses.iter().find(|s| s.id == id)
    }
}

/// 他の Server の見え方。
#[derive(Debug, Clone, Serialize)]
pub struct PeerView {
    pub name: String,
    pub url: String,
    pub priority: u8,
    /// 稼働中とみなしているか（`down_after_sec` 以内に応答があった）。
    pub up: bool,
    /// 最後に応答があった時刻（UNIX秒）。
    pub last_seen: Option<u64>,
    pub error: Option<String>,
    #[serde(skip)]
    pub last_ok: Option<Instant>,
    #[serde(skip)]
    pub state: Option<PeerState>,
}

impl PeerView {
    fn new(peer: &Peer) -> Self {
        Self {
            name: peer.name.clone(),
            url: peer.url.trim_end_matches('/').to_string(),
            priority: peer.priority,
            up: false,
            last_seen: None,
            error: None,
            last_ok: None,
            state: None,
        }
    }

    pub fn holds_camera_vip(&self) -> bool {
        self.up && self.state.as_ref().is_some_and(|s| s.vip.camera)
    }

    pub fn holds_obs_vip(&self) -> bool {
        self.up && self.state.as_ref().is_some_and(|s| s.vip.obs)
    }

    fn receives(&self, camera_id: &str) -> bool {
        self.up
            && self
                .state
                .as_ref()
                .and_then(|s| s.camera(camera_id))
                .is_some_and(|c| c.direct)
    }
}

/// 冗長構成の実行時の状態。
#[derive(Default)]
pub struct ClusterRuntime {
    /// この Server の状態（他の Server へ返す）。
    local: RwLock<Option<PeerState>>,
    peers: RwLock<Vec<PeerView>>,
    vip: RwLock<VipState>,
    /// カメラID → 中継の取り込み元の Server。
    relays: Mutex<HashMap<String, String>>,
    client: OnceLock<reqwest::Client>,
}

impl ClusterRuntime {
    pub fn peers(&self) -> Vec<PeerView> {
        self.peers.read().expect("cluster lock").clone()
    }

    pub fn vip(&self) -> VipState {
        self.vip.read().expect("cluster lock").clone()
    }

    pub fn relay_from(&self, camera_id: &str) -> Option<String> {
        self.relays.lock().expect("cluster lock").get(camera_id).cloned()
    }

    /// カメラを直接受けている他の Server。
    pub fn direct_peers(&self, camera_id: &str) -> Vec<String> {
        self.peers
            .read()
            .expect("cluster lock")
            .iter()
            .filter(|p| p.receives(camera_id))
            .map(|p| p.name.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 判断（単体テストの対象）
// ---------------------------------------------------------------------------

/// 中継の取り込み元を選ぶ。自分が直接受けていれば中継しない。
/// 今の取り込み元がまだ使えるならそのまま（余計な切替をしない）。なければ優先度の高い Server。
pub fn choose_relay<'a>(self_direct: bool, current: Option<&str>, candidates: &[(&'a str, u8)]) -> Option<&'a str> {
    if self_direct {
        return None;
    }
    if let Some(cur) = current {
        if let Some((name, _)) = candidates.iter().find(|(n, _)| *n == cur) {
            return Some(*name);
        }
    }
    candidates.iter().max_by_key(|(_, p)| *p).map(|(n, _)| *n)
}

/// dual のカメラの2本目の送り先。応答した Server（`self_site`）以外で稼働中のもののうち、
/// カメラ用の共通IPを持っていない Server を優先し、その中で優先度が最も高いもの。
pub fn choose_backup<'a>(self_site: &str, peers: &'a [PeerView]) -> Option<&'a PeerView> {
    peers
        .iter()
        .filter(|p| p.name != self_site && p.up)
        .max_by_key(|p| (!p.holds_camera_vip(), p.priority))
}

/// カメラ一覧を取り込むべき Server（稼働中で、改訂番号が自分より新しく最も新しいもの）。
pub fn newer_cameras<'a>(local_revision: u64, peers: &'a [PeerView]) -> Option<&'a PeerView> {
    peers
        .iter()
        .filter(|p| p.up)
        .filter(|p| p.state.as_ref().is_some_and(|s| s.cameras_revision > local_revision))
        .max_by_key(|p| p.state.as_ref().map_or(0, |s| s.cameras_revision))
}

type HmacSha256 = Hmac<Sha256>;

fn signature(key: &[u8], site: &str, ts: u64, method: &str, path: &str) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC は任意長の鍵を受け付ける");
    let ts = ts.to_string();
    for part in [site, ts.as_str(), method, path] {
        mac.update(part.as_bytes());
        mac.update(&[0]);
    }
    mac
}

/// Server 同士の API の認証ヘッダーの値。
pub fn sign(key: &[u8], site: &str, ts: u64, method: &str, path: &str) -> String {
    let sig = signature(key, site, ts, method, path).finalize().into_bytes();
    format!("{site}.{ts}.{}", URL_SAFE_NO_PAD.encode(sig))
}

/// 認証ヘッダーを確かめ、送ってきた Server の名前を返す。
pub fn verify(
    key: &[u8],
    value: &str,
    method: &str,
    path: &str,
    now: u64,
    sites: &[&str],
) -> Result<String, &'static str> {
    let mut parts = value.splitn(3, '.');
    let (Some(site), Some(ts), Some(sig)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("認証ヘッダーの形式が不正です");
    };
    if !sites.contains(&site) {
        return Err("cluster.peers にない Server です");
    }
    let ts: u64 = ts.parse().map_err(|_| "認証ヘッダーの時刻が不正です")?;
    if ts.abs_diff(now) > CLOCK_SKEW_SEC {
        return Err("Server の時刻がずれています（NTP で合わせてください）");
    }
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| "署名が不正です")?;
    signature(key, site, ts, method, path)
        .verify_slice(&sig)
        .map_err(|_| "署名が一致しません（cluster.shared_secret を全 Server で同じにしてください）")?;
    Ok(site.to_string())
}

// ---------------------------------------------------------------------------
// Server 同士の API
// ---------------------------------------------------------------------------

async fn cluster_key(state: &AppState, cfg: &Config) -> Result<Vec<u8>> {
    let secret = state
        .secrets
        .resolve(&cfg.cluster.shared_secret)
        .await
        .context("cluster.shared_secret を取得できません")?;
    anyhow::ensure!(!secret.is_empty(), "cluster.shared_secret が空です");
    Ok(derive_key(&secret, "cluster"))
}

async fn authenticate(state: &AppState, cfg: &Config, headers: &HeaderMap, method: &Method, path: &str) -> Result<String, ApiError> {
    if !cfg.cluster.enabled {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "冗長構成（cluster.enabled）が無効です"));
    }
    let value = headers
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Server 間の認証ヘッダーがありません"))?;
    let key = cluster_key(state, cfg)
        .await
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let sites: Vec<&str> = cfg.other_peers().map(|p| p.name.as_str()).collect();
    verify(&key, value, method.as_str(), path, now_unix(), &sites).map_err(|e| {
        tracing::warn!("Server 間の認証に失敗しました: {e}");
        ApiError::new(StatusCode::UNAUTHORIZED, e)
    })
}

/// `GET /api/v1/cluster/state`
pub async fn state_handler(State(state): State<SharedState>, headers: HeaderMap) -> Result<Json<PeerState>, ApiError> {
    let cfg = state.store.get();
    authenticate(&state, &cfg, &headers, &Method::GET, "/api/v1/cluster/state").await?;
    let local = state.cluster.local.read().expect("cluster lock").clone();
    let local = match local {
        Some(s) => s,
        None => local_state(&state, &cfg, &camera_statuses(&state, &cfg).await),
    };
    Ok(Json(local))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CamerasPush {
    pub revision: u64,
    pub cameras: Vec<Camera>,
}

/// `POST /api/v1/cluster/cameras`（他の Server で変更されたカメラ一覧）
pub async fn cameras_handler(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(push): Json<CamerasPush>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = state.store.get();
    let site = authenticate(&state, &cfg, &headers, &Method::POST, "/api/v1/cluster/cameras").await?;
    let applied = apply_remote(&state, &site, push.cameras, push.revision)
        .await
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(serde_json::json!({
        "applied": applied,
        "revision": state.store.get().cameras_revision,
    })))
}

/// 他の Server のカメラ一覧を、改訂番号が新しければ反映してコンテナをそろえる。
async fn apply_remote(state: &AppState, site: &str, cameras: Vec<Camera>, revision: u64) -> Result<bool> {
    let _ops = state.ops.lock().await;
    let before = state.store.get();
    let Some(after) = state.store.apply_remote_cameras(cameras, revision)? else {
        return Ok(false);
    };
    for cam in &before.cameras {
        if after.camera(&cam.id).is_none() {
            state.owners.lock().expect("owners lock").remove(&cam.id);
            state.cluster.relays.lock().expect("cluster lock").remove(&cam.id);
            health::reset(state, &cam.id);
            state.ai.forget(&cam.id);
        }
    }
    for cam in &after.cameras {
        if before.camera(&cam.id).is_none() {
            health::reset(state, &cam.id);
        }
    }
    if before.cameras != after.cameras {
        tracing::info!(from = %site, revision, "他の Server からカメラ一覧を受け取りました");
        events::notify(state, Event::cameras_synced(site));
        let report = admin::apply(state).await;
        if !report.errors.is_empty() {
            tracing::warn!("一部のコンテナを準備できませんでした: {}", report.summary());
        }
    }
    Ok(true)
}

fn client(state: &AppState) -> Option<&reqwest::Client> {
    state.cluster.client.get()
}

async fn signed(state: &AppState, cfg: &Config, method: Method, url: &str, path: &str) -> Result<reqwest::RequestBuilder> {
    let http = client(state).context("Server 間の通信を準備できていません")?;
    let key = cluster_key(state, cfg).await?;
    let value = sign(&key, &cfg.cluster.site, now_unix(), method.as_str(), path);
    Ok(http.request(method, format!("{url}{path}")).header(HEADER, value))
}

/// この Server で変更したカメラ一覧を他の Server へ配る（届かなかった Server は次の状態交換で取り込む）。
pub fn broadcast_cameras(state: SharedState) {
    let cfg = state.store.get();
    if !cfg.cluster.enabled {
        return;
    }
    let push = CamerasPush {
        revision: cfg.cameras_revision,
        cameras: cfg.cameras.clone(),
    };
    for peer in cfg.other_peers().cloned().collect::<Vec<_>>() {
        let state = state.clone();
        let cfg = cfg.clone();
        let body = serde_json::to_value(&push).unwrap_or_default();
        tokio::spawn(async move {
            let path = "/api/v1/cluster/cameras";
            let url = peer.url.trim_end_matches('/').to_string();
            let result = async {
                signed(&state, &cfg, Method::POST, &url, path)
                    .await?
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
                anyhow::Ok(())
            }
            .await;
            match result {
                Ok(()) => tracing::info!(to = %peer.name, "カメラ一覧を配りました"),
                Err(e) => tracing::warn!(to = %peer.name, "カメラ一覧を配れません（次の状態交換で同期します）: {e:#}"),
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 定期処理
// ---------------------------------------------------------------------------

fn local_state(state: &AppState, cfg: &Config, statuses: &[CameraStatus]) -> PeerState {
    PeerState {
        site: cfg.cluster.site.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        vip: state.cluster.vip(),
        cameras_revision: cfg.cameras_revision,
        cameras: cfg.cameras.clone(),
        statuses: statuses
            .iter()
            .map(|s| CameraState {
                id: s.id.clone(),
                direct: s.receiving,
                output: s.output_ready,
                relay_from: s.relay_from.clone(),
                problem: s.health.problem.clone().or_else(|| s.media_error.clone()),
                recording: s.recording,
                obs_readers: s.srt_readers,
            })
            .collect(),
        time: now_unix(),
    }
}

/// Server 間の通信に使うクライアント。全 Server で共用する証明書（server.tls.cert）だけを信頼する。
fn build_client(cfg: &Config) -> Result<reqwest::Client> {
    let pem = std::fs::read(&cfg.server.tls.cert)
        .with_context(|| format!("証明書を読み込めません: {}", cfg.server.tls.cert.display()))?;
    let cert = reqwest::Certificate::from_pem(&pem).context("証明書を解析できません")?;
    Ok(reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(cert)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()?)
}

/// 冗長構成の処理を始める（keepalived の設定・状態交換・同期・中継）。
pub fn start(state: SharedState) {
    let cfg = state.store.get();
    if !cfg.cluster.enabled {
        return;
    }
    match build_client(&cfg) {
        Ok(c) => {
            let _ = state.cluster.client.set(c);
        }
        Err(e) => tracing::error!("Server 間の通信を準備できません: {e:#}"),
    }
    *state.cluster.peers.write().expect("cluster lock") = cfg.other_peers().map(PeerView::new).collect();
    tokio::spawn(run(state));
}

async fn run(state: SharedState) {
    let cfg = state.store.get();
    let period = Duration::from_secs(u64::from(cfg.cluster.heartbeat_sec.max(1)));
    let down_after = Duration::from_secs(u64::from(cfg.cluster.down_after_sec));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut keepalived_at: Option<Instant> = None;
    loop {
        tick.tick().await;
        let cfg = state.store.get();

        if keepalived_at.is_none_or(|t| t.elapsed() >= KEEPALIVED_EVERY) {
            if let Err(e) = keepalived::write_and_reload(&cfg).await {
                tracing::error!("keepalived の設定を書き出せません: {e:#}");
            }
            keepalived_at = Some(Instant::now());
        }
        update_vip(&state, &cfg).await;

        let statuses = camera_statuses(&state, &cfg).await;
        *state.cluster.local.write().expect("cluster lock") = Some(local_state(&state, &cfg, &statuses));

        poll_peers(&state, &cfg, period, down_after).await;

        let peers = state.cluster.peers();
        if let Some(src) = newer_cameras(cfg.cameras_revision, &peers) {
            let remote = src.state.clone().unwrap_or_default();
            if let Err(e) = apply_remote(&state, &src.name, remote.cameras, remote.cameras_revision).await {
                tracing::error!(from = %src.name, "カメラ一覧を反映できません: {e:#}");
                events::notify(&state, Event::sync_failed(&src.name, &format!("{e:#}")));
            }
        }

        let cfg = state.store.get();
        apply_relays(&state, &cfg, &statuses, &peers).await;
    }
}

async fn update_vip(state: &AppState, cfg: &Config) {
    let next = keepalived::vip_state(cfg).await;
    let prev = std::mem::replace(&mut *state.cluster.vip.write().expect("cluster lock"), next.clone());
    if !next.managed {
        return;
    }
    if prev.camera != next.camera {
        tracing::warn!(state = %next.camera_state, "カメラ用の共通IPの受け持ちが変わりました");
        events::notify(state, Event::vip("カメラ用", next.camera));
    }
    if prev.obs != next.obs {
        tracing::warn!(state = %next.obs_state, "OBS 用の共通IPの受け持ちが変わりました");
        events::notify(state, Event::vip("OBS 用", next.obs));
    }
}

async fn poll_peers(state: &SharedState, cfg: &Config, period: Duration, down_after: Duration) {
    let path = "/api/v1/cluster/state";
    let mut set = tokio::task::JoinSet::new();
    for peer in state.cluster.peers() {
        let state = state.clone();
        let cfg = cfg.clone();
        set.spawn(async move {
            let result = async {
                let res = signed(&state, &cfg, Method::GET, &peer.url, path)
                    .await?
                    .timeout(period.max(Duration::from_secs(2)))
                    .send()
                    .await?;
                let status = res.status();
                if !status.is_success() {
                    let text = res.text().await.unwrap_or_default();
                    anyhow::bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
                }
                Ok(res.json::<PeerState>().await?)
            }
            .await;
            (peer.name, result)
        });
    }
    let mut results = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((name, result)) = joined {
            results.insert(name, result);
        }
    }

    let mut notices = Vec::new();
    {
        let mut peers = state.cluster.peers.write().expect("cluster lock");
        let now = Instant::now();
        for peer in peers.iter_mut() {
            let was_up = peer.up;
            match results.remove(&peer.name) {
                Some(Ok(s)) => {
                    peer.last_ok = Some(now);
                    peer.last_seen = Some(now_unix());
                    peer.error = None;
                    peer.state = Some(s);
                }
                Some(Err(e)) => peer.error = Some(explain(&e)),
                None => {}
            }
            peer.up = peer.last_ok.is_some_and(|t| now.duration_since(t) < down_after);
            if !peer.up {
                // 停止中の Server の古い状態では判断しない。
                peer.state = None;
            }
            if was_up != peer.up {
                notices.push(if peer.up {
                    Event::peer_up(&peer.name)
                } else {
                    Event::peer_down(&peer.name, peer.error.as_deref().unwrap_or("応答がありません"))
                });
            }
        }
    }
    for e in notices {
        tracing::warn!("{}", e.text);
        events::notify(state, e);
    }
}

/// 接続できない理由を分かる言葉にする。
fn explain(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.contains("certificate") || text.contains("UnknownIssuer") || text.contains("NotValidForName") {
        format!("証明書を検証できません（全 Server で同じ証明書を使い、全 Server の実IPを含めてください）: {text}")
    } else {
        text
    }
}

async fn apply_relays(state: &AppState, cfg: &Config, statuses: &[CameraStatus], peers: &[PeerView]) {
    for cam in &cfg.cameras {
        let running = state
            .container(&cfg.container_name(&cam.id))
            .is_some_and(|c| c.state == "running");
        if !running {
            continue;
        }
        let direct = statuses.iter().any(|s| s.id == cam.id && s.receiving);
        let candidates: Vec<(&str, u8)> = peers
            .iter()
            .filter(|p| p.receives(&cam.id))
            .map(|p| (p.name.as_str(), p.priority))
            .collect();
        let current = state.cluster.relay_from(&cam.id);
        let choice = choose_relay(direct, current.as_deref(), &candidates);
        let desired = match choice.and_then(|name| peers.iter().find(|p| p.name == name)) {
            Some(peer) => mediamtx::relay_source(crate::config::url_host(&peer.url).unwrap_or_default(), cam),
            None => "publisher".to_string(),
        };

        let api = mediamtx::api_base(cfg, &cam.id);
        let actual = match mediamtx::output_source(&state.http, &api, &cam.id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(camera = %cam.id, "取り込み元を確認できません: {e:#}");
                continue;
            }
        };
        if actual != desired {
            match mediamtx::set_output_source(&state.http, &api, &cam.id, &desired).await {
                Ok(()) => {
                    tracing::info!(camera = %cam.id, source = %desired, "出力の取り込み元を切り替えました");
                    events::notify(state, Event::relay(&cam.id, choice));
                }
                Err(e) => {
                    tracing::warn!(camera = %cam.id, "出力の取り込み元を切り替えられません: {e:#}");
                    continue;
                }
            }
        }
        let mut relays = state.cluster.relays.lock().expect("cluster lock");
        match choice {
            Some(name) => relays.insert(cam.id.clone(), name.to_string()),
            None => relays.remove(&cam.id),
        };
    }
}

// ---------------------------------------------------------------------------
// 画面向けの要約
// ---------------------------------------------------------------------------

/// 画面（管理Web・モニター・OBSドック・送信画面）向けの冗長構成の状態。
#[derive(Debug, Clone, Serialize)]
pub struct ClusterSummary {
    /// この画面に応答した Server。
    pub site: String,
    pub camera_host: String,
    pub obs_host: String,
    /// keepalived を AMoRe が設定しているか。
    pub keepalived: bool,
    pub servers: Vec<ServerSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerSummary {
    pub name: String,
    pub url: String,
    pub priority: u8,
    pub up: bool,
    /// この画面に応答した Server か。
    #[serde(rename = "self")]
    pub is_self: bool,
    /// 共通IPを受け持っているか（keepalived を使わない場合は null）。
    pub camera_vip: Option<bool>,
    pub obs_vip: Option<bool>,
    pub last_seen: Option<u64>,
    pub error: Option<String>,
    pub version: Option<String>,
}

pub fn summary(state: &AppState, cfg: &Config) -> Option<ClusterSummary> {
    if !cfg.cluster.enabled {
        return None;
    }
    let vip = state.cluster.vip();
    let peers = state.cluster.peers();
    let managed = keepalived::enabled(cfg);
    let mut servers = Vec::new();
    for p in &cfg.cluster.peers {
        if p.name == cfg.cluster.site {
            servers.push(ServerSummary {
                name: p.name.clone(),
                url: p.url.trim_end_matches('/').to_string(),
                priority: p.priority,
                up: true,
                is_self: true,
                camera_vip: managed.then_some(vip.camera),
                obs_vip: managed.then_some(vip.obs),
                last_seen: Some(now_unix()),
                error: None,
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            });
        } else if let Some(v) = peers.iter().find(|v| v.name == p.name) {
            servers.push(ServerSummary {
                name: v.name.clone(),
                url: v.url.clone(),
                priority: v.priority,
                up: v.up,
                is_self: false,
                camera_vip: managed.then(|| v.holds_camera_vip()),
                obs_vip: managed.then(|| v.holds_obs_vip()),
                last_seen: v.last_seen,
                error: v.error.clone(),
                version: v.state.as_ref().map(|s| s.version.clone()),
            });
        }
    }
    Some(ClusterSummary {
        site: cfg.cluster.site.clone(),
        camera_host: cfg.camera_host(),
        obs_host: cfg.obs_host(),
        keepalived: managed,
        servers,
    })
}

/// 送信画面向け：dual のカメラの2本目の送り先。
#[derive(Debug, Clone, Serialize)]
pub struct Backup {
    pub name: String,
    pub url: String,
}

pub fn backup(state: &AppState, cfg: &Config) -> Option<Backup> {
    if !cfg.cluster.enabled {
        return None;
    }
    let peers = state.cluster.peers();
    choose_backup(&cfg.cluster.site, &peers).map(|p| Backup {
        name: p.name.clone(),
        url: p.url.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(name: &str, priority: u8, up: bool, camera_vip: bool, direct: &[&str], revision: u64) -> PeerView {
        PeerView {
            name: name.into(),
            url: format!("https://{name}"),
            priority,
            up,
            last_seen: None,
            error: None,
            last_ok: None,
            state: Some(PeerState {
                site: name.into(),
                vip: VipState {
                    managed: true,
                    camera: camera_vip,
                    obs: camera_vip,
                    ..Default::default()
                },
                cameras_revision: revision,
                statuses: direct
                    .iter()
                    .map(|id| CameraState {
                        id: id.to_string(),
                        direct: true,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn relays_only_when_not_receiving_directly() {
        let candidates = [("hq", 100), ("site2", 30)];
        assert_eq!(choose_relay(true, None, &candidates), None, "直接受けていれば中継しない");
        assert_eq!(choose_relay(false, None, &candidates), Some("hq"), "優先度の高い Server");
        assert_eq!(choose_relay(false, Some("site2"), &candidates), Some("site2"), "今の取り込み元を保つ");
        assert_eq!(choose_relay(false, Some("gone"), &candidates), Some("hq"));
        assert_eq!(choose_relay(false, Some("hq"), &[]), None, "どこも受けていなければ中継しない");
    }

    #[test]
    fn picks_backup_server_for_dual() {
        let peers = [
            view("hq", 100, true, true, &[], 0),
            view("site1", 50, true, false, &[], 0),
            view("site2", 70, false, false, &[], 0),
        ];
        // 応答した Server（hq）以外で、共通IPを持たず稼働中の Server。
        assert_eq!(choose_backup("hq", &peers).unwrap().name, "site1");
        // site1 が応答した場合（hq が共通IPを持つ）は hq を選ぶしかない。
        assert_eq!(choose_backup("site1", &peers).unwrap().name, "hq");
        let peers = [
            view("site1", 50, true, false, &[], 0),
            view("site2", 70, true, false, &[], 0),
        ];
        assert_eq!(choose_backup("hq", &peers).unwrap().name, "site2", "優先度の高い方");
        let peers = [view("site1", 50, false, false, &[], 0)];
        assert!(choose_backup("hq", &peers).is_none(), "停止中の Server には送らない");
    }

    #[test]
    fn takes_newest_camera_list_from_running_peers() {
        let peers = [
            view("site1", 50, true, false, &[], 20),
            view("site2", 70, true, false, &[], 30),
            view("site3", 90, false, false, &[], 99),
        ];
        assert_eq!(newer_cameras(10, &peers).unwrap().name, "site2");
        assert!(newer_cameras(30, &peers).is_none());
    }

    #[test]
    fn peer_views_report_direct_reception() {
        let p = view("site1", 50, true, false, &["cam1"], 0);
        assert!(p.receives("cam1"));
        assert!(!p.receives("cam2"));
        let down = view("site1", 50, false, true, &["cam1"], 0);
        assert!(!down.receives("cam1"), "停止中の Server は受けていないとみなす");
        assert!(!down.holds_camera_vip());
    }

    #[test]
    fn signs_and_verifies_cluster_requests() {
        let key = derive_key("secret", "cluster");
        let value = sign(&key, "hq", 1_000, "GET", "/api/v1/cluster/state");
        let sites = ["hq", "site1"];
        assert_eq!(verify(&key, &value, "GET", "/api/v1/cluster/state", 1_010, &sites).as_deref(), Ok("hq"));
        assert!(verify(&key, &value, "GET", "/api/v1/cluster/state", 1_100, &sites).is_err(), "時刻のずれ");
        assert!(verify(&key, &value, "POST", "/api/v1/cluster/state", 1_000, &sites).is_err(), "別のメソッド");
        assert!(verify(&key, &value, "GET", "/api/v1/cluster/cameras", 1_000, &sites).is_err(), "別のパス");
        assert!(verify(&key, &value, "GET", "/api/v1/cluster/state", 1_000, &["site1"]).is_err(), "知らない Server");
        assert!(verify(&derive_key("other", "cluster"), &value, "GET", "/api/v1/cluster/state", 1_000, &sites).is_err());
        assert!(verify(&key, "garbage", "GET", "/", 0, &sites).is_err());
    }
}
