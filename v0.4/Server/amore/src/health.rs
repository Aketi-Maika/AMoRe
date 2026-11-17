//! カメラコンテナの不調検知と自動復旧。
//!
//! v0.2 の実機で「3台目のカメラコンテナが起動できない」（管理Webに `dns error` と表示）事象があった。
//! Docker の内部DNSは、同じネットワークに接続中の**稼働中**コンテナしか名前を引けないため、
//! コンテナが再起動を繰り返している、またはネットワークから外れていると、この表示になる。
//! 原因によらず状況が分かり、自動で直るよう、次を行う。
//! - 状態（再起動回数・終了コード・メモリ不足・起動エラー・接続ネットワーク）から不調を判定し、理由と直近ログを画面に出す
//! - ネットワークから外れていれば接続し直す
//! - それ以外の不調は作り直しを最大 [`MAX_RECREATES`] 回まで試し、だめなら止めて警告する（無限に再起動させない）

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::api::AppState;
use crate::config::Config;
use crate::docker::{self, ContainerInfo};
use crate::events::{self, Event};

/// 自動で作り直す上限回数。
pub const MAX_RECREATES: u32 = 3;
/// 不調がこの時間続いたら作り直す（一時的な再起動を待つ）。
const RECREATE_AFTER: Duration = Duration::from_secs(20);
/// 正常がこの時間続いたら作り直し回数を戻す。
const RESET_AFTER: Duration = Duration::from_secs(300);
/// 不調時に直近ログを取り直す間隔。
const LOGS_EVERY: Duration = Duration::from_secs(30);

/// カメラごとの不調の記録。
#[derive(Debug, Clone, Default)]
pub struct CamHealth {
    pub problem: Option<String>,
    pub problem_since: Option<Instant>,
    pub healthy_since: Option<Instant>,
    pub last_restart_count: Option<u64>,
    pub recreates: u32,
    /// 自動復旧をあきらめて止めた。
    pub given_up: bool,
    pub network_repaired: bool,
    pub logs: Vec<String>,
    pub logs_at: Option<Instant>,
}

/// 画面に出す不調の情報。
#[derive(Debug, Clone, Serialize)]
pub struct HealthView {
    pub problem: Option<String>,
    /// 自動復旧の状況（例: `自動で作り直しました（1/3回）`）。
    pub recovery: Option<String>,
    /// 不調時のコンテナの直近ログ。
    pub logs: Vec<String>,
}

impl CamHealth {
    pub fn view(&self) -> HealthView {
        let recovery = if self.given_up {
            Some(format!(
                "自動復旧を停止しました（{MAX_RECREATES}回作り直しても直りませんでした）。ログを確認し、原因を取り除いてから「再起動」を押してください"
            ))
        } else if self.recreates > 0 {
            Some(format!("自動で作り直しました（{}/{MAX_RECREATES}回）", self.recreates))
        } else if self.network_repaired {
            Some("ネットワークへ接続し直しました".to_string())
        } else {
            None
        };
        HealthView {
            problem: self.problem.clone(),
            recovery,
            logs: if self.problem.is_some() { self.logs.clone() } else { Vec::new() },
        }
    }
}

/// コンテナの状態から不調を判定する。`prev_restart_count` は前回確認時の再起動回数。
pub fn assess(info: &ContainerInfo, network: &str, prev_restart_count: Option<u64>) -> Option<String> {
    let detail = || {
        let mut d = format!("終了コード {}", info.exit_code);
        if info.oom_killed {
            d.push_str("・メモリ不足で強制終了");
        }
        if !info.error.is_empty() {
            d.push_str(&format!("・{}", info.error));
        }
        d
    };
    match info.state.as_str() {
        "restarting" => {
            return Some(format!(
                "起動直後に終了し、再起動を繰り返しています（{}回、{}）",
                info.restart_count,
                detail()
            ))
        }
        "exited" | "dead" => return Some(format!("停止しています（{}）", detail())),
        "created" => {
            return Some(if info.error.is_empty() {
                "作成されましたが起動していません".to_string()
            } else {
                format!("起動できません: {}", info.error)
            })
        }
        "paused" => return Some("一時停止しています".to_string()),
        _ => {}
    }
    if prev_restart_count.is_some_and(|prev| info.restart_count > prev) {
        return Some(format!(
            "起動直後に終了し、再起動を繰り返しています（{}回、{}）",
            info.restart_count,
            detail()
        ));
    }
    if !info.networks.iter().any(|n| n == network) {
        return Some(format!(
            "ネットワーク {network} に接続されていません（AMoReから名前で接続できません）"
        ));
    }
    None
}

/// 一定間隔で呼ぶ。状態を判定し、必要なら自動復旧する。
pub async fn check(state: &AppState) {
    let cfg = state.store.get();
    let containers: HashMap<String, ContainerInfo> = state
        .runtime
        .read()
        .expect("runtime lock")
        .containers
        .iter()
        .map(|c| (c.camera.clone(), c.clone()))
        .collect();
    let now = Instant::now();

    for cam in &cfg.cameras {
        let name = cfg.container_name(&cam.id);
        let info = containers.get(&cam.id);
        let (previous, prev_restart) = {
            let health = state.health.lock().expect("health lock");
            let h = health.get(&cam.id).cloned().unwrap_or_default();
            (h.problem.clone(), h.last_restart_count)
        };
        // コンテナが無い場合は reconcile（追加・再適用・再起動）で作られるため、ここでは扱わない。
        let problem = info.and_then(|i| assess(i, &cfg.media.docker_network, prev_restart));

        // 状態が変わったときだけ通知する。
        match (&previous, &problem) {
            (None, Some(p)) => {
                tracing::warn!(camera = %cam.id, "カメラのコンテナが不調です: {p}");
                events::notify(state, Event::problem(&cam.id, p));
            }
            (Some(_), None) => {
                tracing::info!(camera = %cam.id, "カメラのコンテナが復旧しました");
                events::notify(state, Event::recovered(&cam.id));
            }
            _ => {}
        }

        let need_logs = {
            let mut health = state.health.lock().expect("health lock");
            let h = health.entry(cam.id.clone()).or_default();
            h.last_restart_count = info.map(|i| i.restart_count);
            if problem.is_some() {
                if h.problem.is_none() {
                    h.problem_since = Some(now);
                }
                h.healthy_since = None;
            } else {
                h.problem_since = None;
                h.healthy_since.get_or_insert(now);
                if h.healthy_since.is_some_and(|t| now.duration_since(t) >= RESET_AFTER) {
                    h.recreates = 0;
                    h.network_repaired = false;
                }
            }
            h.problem = problem.clone();
            problem.is_some() && h.logs_at.is_none_or(|t| now.duration_since(t) >= LOGS_EVERY)
        };
        if need_logs {
            let logs = docker::logs(&name, 20)
                .await
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_else(|e| vec![format!("ログを取得できません: {e:#}")]);
            let mut health = state.health.lock().expect("health lock");
            let h = health.entry(cam.id.clone()).or_default();
            h.logs = logs;
            h.logs_at = Some(now);
        }

        if let (Some(p), Some(info)) = (&problem, info) {
            recover(state, &cfg, &cam.id, &name, p, info, now).await;
        }
    }
}

async fn recover(
    state: &AppState,
    cfg: &Config,
    camera_id: &str,
    name: &str,
    problem: &str,
    info: &ContainerInfo,
    now: Instant,
) {
    let network = &cfg.media.docker_network;
    let (given_up, recreates, since, network_repaired) = {
        let health = state.health.lock().expect("health lock");
        let h = health.get(camera_id).cloned().unwrap_or_default();
        (h.given_up, h.recreates, h.problem_since, h.network_repaired)
    };
    if given_up {
        return;
    }

    // 稼働中でネットワークだけ外れている場合は接続し直す（1回だけ。直らなければ作り直しへ）。
    if info.state == "running" && !info.networks.iter().any(|n| n == network) && !network_repaired {
        let result = docker::network_connect(network, name).await;
        let mut health = state.health.lock().expect("health lock");
        let h = health.entry(camera_id.to_string()).or_default();
        h.network_repaired = true;
        if let Err(e) = result {
            tracing::error!(camera = %camera_id, "ネットワークへ接続できません: {e:#}");
        }
        return;
    }

    if since.is_none_or(|t| now.duration_since(t) < RECREATE_AFTER) {
        return;
    }
    // 追加・削除などの操作中は待つ。
    let Ok(_ops) = state.ops.try_lock() else {
        return;
    };

    if recreates >= MAX_RECREATES {
        tracing::error!(camera = %camera_id, "自動復旧を停止します: {problem}");
        if let Err(e) = docker::stop_and_hold(name).await {
            tracing::error!(camera = %camera_id, "コンテナを停止できません: {e:#}");
        }
        state
            .health
            .lock()
            .expect("health lock")
            .entry(camera_id.to_string())
            .or_default()
            .given_up = true;
        events::notify(
            state,
            Event::problem(
                camera_id,
                &format!("自動復旧を停止しました（{MAX_RECREATES}回作り直しても直りません）: {problem}"),
            ),
        );
        return;
    }

    // reconcile と同じ設定（秘密参照を解決したもの）で作る。違うと次の reconcile で再度作り直される。
    let cfg = crate::admin::container_config(state).await;
    let Some(cam) = cfg.camera(camera_id) else {
        return;
    };
    tracing::warn!(camera = %camera_id, "コンテナを作り直します（{}回目）: {problem}", recreates + 1);
    let result = async {
        docker::remove(name).await?;
        docker::create(&cfg, cam).await
    }
    .await;
    {
        let mut health = state.health.lock().expect("health lock");
        let h = health.entry(camera_id.to_string()).or_default();
        h.recreates += 1;
        h.problem_since = Some(now);
        h.last_restart_count = None;
    }
    if let Err(e) = result {
        tracing::error!(camera = %camera_id, "コンテナを作り直せません: {e:#}");
    }
}

/// 手動の再起動・再適用・削除で、自動復旧の記録を消す。
pub fn reset(state: &AppState, camera_id: &str) {
    state.health.lock().expect("health lock").remove(camera_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: &str, restarts: u64, networks: &[&str]) -> ContainerInfo {
        ContainerInfo {
            name: "amore-cam-cam3".into(),
            camera: "cam3".into(),
            state: state.into(),
            status: String::new(),
            restart_count: restarts,
            exit_code: 1,
            oom_killed: false,
            error: String::new(),
            networks: networks.iter().map(|s| s.to_string()).collect(),
            started_at: String::new(),
            config_hash: String::new(),
        }
    }

    #[test]
    fn detects_problems() {
        let net = "amore_net";
        assert_eq!(assess(&info("running", 0, &[net]), net, Some(0)), None);
        assert!(assess(&info("restarting", 4, &[]), net, None)
            .unwrap()
            .contains("再起動を繰り返しています（4回、終了コード 1）"));
        // 稼働中に見えても、前回より再起動回数が増えていれば不調。
        assert!(assess(&info("running", 5, &[net]), net, Some(4))
            .unwrap()
            .contains("再起動を繰り返しています"));
        assert!(assess(&info("running", 0, &["bridge"]), net, Some(0))
            .unwrap()
            .contains("ネットワーク amore_net に接続されていません"));
        assert!(assess(&info("exited", 0, &[net]), net, None).unwrap().contains("停止しています"));
        let mut created = info("created", 0, &[net]);
        created.error = "port is already allocated".into();
        assert_eq!(
            assess(&created, net, None).as_deref(),
            Some("起動できません: port is already allocated")
        );
    }

    #[test]
    fn view_describes_recovery() {
        let mut h = CamHealth {
            problem: Some("停止しています".into()),
            recreates: 2,
            logs: vec!["error".into()],
            ..Default::default()
        };
        assert_eq!(h.view().recovery.as_deref(), Some("自動で作り直しました（2/3回）"));
        assert_eq!(h.view().logs, vec!["error"]);
        h.given_up = true;
        assert!(h.view().recovery.unwrap().contains("自動復旧を停止しました"));
        h.problem = None;
        assert!(h.view().logs.is_empty());
    }
}
