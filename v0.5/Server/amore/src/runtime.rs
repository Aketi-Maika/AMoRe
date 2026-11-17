//! カメラのコンテナを動かす仕組み（`media.runtime`）の切り替え口。
//!
//! - `docker`：AMoRe が Docker ソケットでカメラのコンテナを作る（docker.rs。v0.4 までと同じ）
//! - `kubernetes`：AMoRe の Pod が k8s API でカメラの Pod（Deployment）を作る（k8s.rs）
//!
//! 管理Web・不調検知・AI・冗長構成はこの窓口だけを使い、どちらで動いているかを意識しない。

use anyhow::Result;

use crate::config::{Camera, Config, Runtime};
use crate::docker::{self, ContainerInfo, ReconcileReport};
use crate::k8s;

fn is_kube(cfg: &Config) -> bool {
    cfg.media.runtime == Runtime::Kubernetes
}

/// 接続先（Docker / Kubernetes API）の版。
pub async fn ping(cfg: &Config) -> Result<String> {
    if is_kube(cfg) {
        k8s::ping(cfg).await
    } else {
        docker::ping().await
    }
}

/// カメラのイメージを用意する（Kubernetes では各ノードの kubelet が取得するため何もしない）。
pub async fn ensure_image(cfg: &Config) -> Result<()> {
    if is_kube(cfg) {
        Ok(())
    } else {
        docker::ensure_image(&cfg.media.image).await
    }
}

/// 管理しているカメラのコンテナ（Pod）の状態。
pub async fn list_managed(cfg: &Config) -> Result<Vec<ContainerInfo>> {
    if is_kube(cfg) {
        k8s::list_managed(cfg).await
    } else {
        docker::list_managed().await
    }
}

/// カメラ一覧にコンテナ（Pod）をそろえる。
pub async fn reconcile(cfg: &Config) -> Result<ReconcileReport> {
    if is_kube(cfg) {
        k8s::reconcile(cfg).await
    } else {
        docker::reconcile(cfg).await
    }
}

/// 作り直す（不調時・手動の再起動）。
pub async fn recreate(cfg: &Config, cam: &Camera) -> Result<()> {
    if is_kube(cfg) {
        return k8s::recreate(cfg, cam).await;
    }
    let name = cfg.container_name(&cam.id);
    docker::remove(&name).await?;
    docker::create(cfg, cam).await
}

/// 再起動する（Kubernetes では Pod を消して作り直させる）。
pub async fn restart(cfg: &Config, name: &str) -> Result<()> {
    if is_kube(cfg) {
        k8s::restart(cfg, name).await
    } else {
        docker::restart(name).await
    }
}

/// ログ（直近 `tail` 行）。
pub async fn logs(cfg: &Config, name: &str, tail: u32) -> Result<String> {
    if is_kube(cfg) {
        k8s::logs(cfg, name, tail).await
    } else {
        docker::logs(name, tail).await
    }
}

/// Docker ネットワークへ接続し直す（Kubernetes では Service 経由のため不要）。
pub async fn network_connect(cfg: &Config, network: &str, name: &str) -> Result<()> {
    if is_kube(cfg) {
        Ok(())
    } else {
        docker::network_connect(network, name).await
    }
}

/// 自動復旧をあきらめたコンテナを止める。
pub async fn stop_and_hold(cfg: &Config, name: &str) -> Result<()> {
    if is_kube(cfg) {
        k8s::stop_and_hold(cfg, name).await
    } else {
        docker::stop_and_hold(name).await
    }
}

/// 静止画（JPEG）。
pub async fn snapshot_jpeg(cfg: &Config, name: &str, path: &str, width: u32) -> Result<Vec<u8>> {
    if is_kube(cfg) {
        k8s::snapshot_jpeg(name, path, width).await
    } else {
        docker::snapshot_jpeg(name, path, width).await
    }
}

/// `cameras.yaml` を保存先へ残す（Kubernetes では ConfigMap。Docker ではファイルのため何もしない）。
pub async fn save_cameras(cfg: &Config, cameras_file: &std::path::Path) {
    if !is_kube(cfg) {
        return;
    }
    let result = async {
        let text = std::fs::read_to_string(cameras_file)?;
        k8s::save_cameras(cfg, &text).await
    }
    .await;
    if let Err(e) = result {
        tracing::error!(
            "cameras.yaml を ConfigMap に保存できません（Pod を作り直すと失われます）: {e:#}"
        );
    }
}

/// Docker 用のコンテナのログを Loki へ送る機能を使えるか（Kubernetes ではログ収集の仕組みに任せる）。
pub fn follows_container_logs(cfg: &Config) -> bool {
    !is_kube(cfg)
}
