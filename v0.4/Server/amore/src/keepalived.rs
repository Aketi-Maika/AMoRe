//! 共通IP（keepalived / VRRP）の設定ファイルの生成と、受け持ち状態の取得。
//!
//! `cluster.keepalived` と `cluster.peers` から keepalived の設定（カメラ用・OBS 用の2つの VRRP インスタンス）を
//! 生成し、共有ボリュームの `keepalived/keepalived.conf` へ書き出す。keepalived はホストのネットワークを使う
//! 別のコンテナ（compose.yaml の `keepalived`）で動き、設定が変わると AMoRe が `SIGHUP` で読み直させる。
//! 共通IPを受け持っているかは、keepalived の notify スクリプトが共有ボリュームへ書く状態ファイルで知る。

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{parse_cidr, Config};
use crate::docker;
use crate::mediamtx::write_atomic;

/// keepalived のコンテナ名（compose.yaml の container_name と同じ）。
pub const CONTAINER: &str = "amore-keepalived";
/// VRRP インスタンス名。状態ファイルの名前にも使う。
pub const CAMERA_INSTANCE: &str = "AMORE_CAMERA";
pub const OBS_INSTANCE: &str = "AMORE_OBS";
/// keepalived のイメージ内のスクリプト（Server/keepalived/）。
const CHECK_SCRIPT: &str = "/usr/local/bin/check_amore.sh";
const NOTIFY_SCRIPT: &str = "/usr/local/bin/notify_state.sh";

pub fn dir(cfg: &Config) -> PathBuf {
    cfg.media.shared_dir.join("keepalived")
}

pub fn conf_path(cfg: &Config) -> PathBuf {
    dir(cfg).join("keepalived.conf")
}

fn state_path(cfg: &Config, instance: &str) -> PathBuf {
    dir(cfg).join("state").join(instance)
}

/// keepalived を使う設定か。
pub fn enabled(cfg: &Config) -> bool {
    cfg.cluster.enabled && cfg.cluster.keepalived.enabled
}

/// keepalived の設定ファイルの内容。
pub fn render(cfg: &Config) -> Result<String> {
    let c = &cfg.cluster;
    let k = &c.keepalived;
    let me = cfg
        .self_peer()
        .with_context(|| format!("cluster.site {:?} が cluster.peers にありません", c.site))?;
    let peers: Vec<&str> = cfg.other_peers().map(|p| p.host()).collect();

    let mut out = String::new();
    out.push_str("# このファイルは AMoRe が cluster.yaml から自動生成する。直接編集しないこと。\n");
    out.push_str(&format!("# Server: {}（{}） priority {}\n\n", me.name, me.host(), me.priority));
    out.push_str(&format!(
        "global_defs {{\n    router_id amore_{}\n    script_user root\n    enable_script_security\n    # 共通IPを受け持った Server は定期的に gratuitous ARP を送り、スイッチの学習を保つ。\n    vrrp_garp_master_refresh 60\n}}\n\n",
        me.name.replace('-', "_")
    ));
    // AMoRe（https://127.0.0.1/healthz）が応答しない Server は共通IPを手放す。
    out.push_str(&format!(
        "vrrp_script chk_amore {{\n    script \"{CHECK_SCRIPT}\"\n    interval 2\n    timeout 3\n    fall 2\n    rise 2\n}}\n\n"
    ));
    for (instance, vrid, vip) in [
        (CAMERA_INSTANCE, k.camera_vrid, &k.camera_vip),
        (OBS_INSTANCE, k.obs_vrid, &k.obs_vip),
    ] {
        let (addr, prefix) =
            parse_cidr(vip).with_context(|| format!("共通IP {vip:?} を解釈できません"))?;
        out.push_str(&format!("vrrp_instance {instance} {{\n"));
        // nopreempt は全 Server を BACKUP で始める必要がある。
        out.push_str("    state BACKUP\n");
        out.push_str(&format!("    interface {}\n", k.interface));
        out.push_str(&format!("    virtual_router_id {vrid}\n"));
        out.push_str(&format!("    priority {}\n", me.priority));
        out.push_str(&format!("    advert_int {}\n", k.advert_int));
        if k.nopreempt {
            out.push_str("    nopreempt\n");
        }
        // 拠点間VPNでもマルチキャストに頼らないよう、Server 同士は unicast で通知する。
        out.push_str(&format!("    unicast_src_ip {}\n", me.host()));
        out.push_str("    unicast_peer {\n");
        for p in &peers {
            out.push_str(&format!("        {p}\n"));
        }
        out.push_str("    }\n");
        out.push_str(&format!(
            "    virtual_ipaddress {{\n        {addr}/{prefix} dev {}\n    }}\n",
            k.interface
        ));
        out.push_str("    track_script {\n        chk_amore\n    }\n");
        out.push_str(&format!("    notify \"{NOTIFY_SCRIPT}\"\n"));
        out.push_str("}\n\n");
    }
    // カメラ用と OBS 用の共通IPは同じ Server が受け持つ（片方だけ移ることがないようにする）。
    out.push_str(&format!(
        "vrrp_sync_group AMORE {{\n    group {{\n        {CAMERA_INSTANCE}\n        {OBS_INSTANCE}\n    }}\n}}\n"
    ));
    Ok(out)
}

/// 設定ファイルを書き出し、内容が変わっていれば keepalived に読み直させる。
pub async fn write_and_reload(cfg: &Config) -> Result<()> {
    if !enabled(cfg) {
        return Ok(());
    }
    let text = render(cfg)?;
    let path = conf_path(cfg);
    let changed = std::fs::read_to_string(&path).map_or(true, |current| current != text);
    std::fs::create_dir_all(dir(cfg).join("state"))
        .with_context(|| format!("ディレクトリを作成できません: {}", dir(cfg).display()))?;
    write_atomic(&path, &text)?;
    if changed {
        // keepalived がまだ起動していない場合は、起動時にこの設定を読む。
        match docker::signal(CONTAINER, "HUP").await {
            Ok(()) => tracing::info!("keepalived の設定を更新しました"),
            Err(e) => tracing::warn!("keepalived に設定を読み直させられません（起動時に読みます）: {e:#}"),
        }
    }
    Ok(())
}

/// 共通IPの受け持ち状態（keepalived の状態ファイル）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VipState {
    /// keepalived を使う設定か。false の場合は受け持ちを判定できない。
    pub managed: bool,
    /// カメラ用の共通IPを受け持っているか。
    pub camera: bool,
    /// OBS 用の共通IPを受け持っているか。
    pub obs: bool,
    /// keepalived の状態（MASTER / BACKUP / FAULT / STOP。不明は空）。
    pub camera_state: String,
    pub obs_state: String,
}

/// 状態ファイルを読む。keepalived のコンテナが止まっていれば受け持っていないとみなす。
pub async fn vip_state(cfg: &Config) -> VipState {
    if !enabled(cfg) {
        return VipState::default();
    }
    let read = |instance: &str| {
        std::fs::read_to_string(state_path(cfg, instance))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let running = docker::is_running(CONTAINER).await.unwrap_or(false);
    let camera_state = if running { read(CAMERA_INSTANCE) } else { "STOP".to_string() };
    let obs_state = if running { read(OBS_INSTANCE) } else { "STOP".to_string() };
    VipState {
        managed: true,
        camera: camera_state == "MASTER",
        obs: obs_state == "MASTER",
        camera_state,
        obs_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::cluster_sample;

    #[test]
    fn renders_vrrp_instances_from_yaml() {
        let cfg = cluster_sample();
        let text = render(&cfg).unwrap();
        assert!(text.contains("router_id amore_hq"));
        assert!(text.contains("vrrp_instance AMORE_CAMERA {"));
        assert!(text.contains("vrrp_instance AMORE_OBS {"));
        assert!(text.contains("virtual_router_id 51"));
        assert!(text.contains("virtual_router_id 52"));
        assert!(text.contains("        149.40.2.1/24 dev ens18"), "既定のカメラ用共通IP");
        assert!(text.contains("        10.40.0.25/16 dev ens18"));
        assert!(text.contains("priority 100"));
        assert!(text.contains("unicast_src_ip 10.40.0.20"));
        assert!(text.contains("unicast_peer {\n        10.40.0.21\n    }"));
        assert!(text.contains("nopreempt"));
        assert!(text.contains("state BACKUP"));
        assert!(!text.contains("10.40.0.20\n    }"), "自分は unicast_peer に含めない");
    }

    #[test]
    fn camera_vip_follows_yaml() {
        let mut cfg = cluster_sample();
        cfg.cluster.site = "site1".into();
        cfg.server.public_host = "10.40.0.21".into();
        cfg.cluster.keepalived.camera_vip = "10.40.2.1/24".into();
        cfg.cluster.keepalived.interface = "eth1".into();
        cfg.cluster.keepalived.nopreempt = false;
        cfg.validate().unwrap();
        let text = render(&cfg).unwrap();
        assert!(text.contains("        10.40.2.1/24 dev eth1"));
        assert!(text.contains("priority 50"));
        assert!(text.contains("unicast_src_ip 10.40.0.21"));
        assert!(text.contains("unicast_peer {\n        10.40.0.20\n    }"));
        assert!(!text.contains("nopreempt"));
    }

    #[test]
    fn disabled_without_cluster() {
        let cfg = crate::config::tests::sample();
        assert!(!enabled(&cfg));
        let mut cfg = cluster_sample();
        cfg.cluster.keepalived.enabled = false;
        assert!(!enabled(&cfg));
    }
}
