//! カメラごとのメディア処理コンテナの管理（1カメラ＝1コンテナ）。
//!
//! AMoRe コンテナにマウントしたDockerソケットを `docker` コマンドで操作する。
//! `AMoRe.yaml` のカメラ一覧を正として、コンテナの作成・再作成・削除をそろえる（reconcile）。
//! 作成したコンテナにはラベル `amore.managed=true` を付け、それ以外のコンテナには触れない。

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::process::Command;

use crate::config::{Camera, Config};
use crate::mediamtx;

const LABEL_MANAGED: &str = "amore.managed";
const LABEL_CAMERA: &str = "amore.camera";
const LABEL_HASH: &str = "amore.config-hash";

/// 管理対象コンテナの状態。
#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    pub name: String,
    pub camera: String,
    /// created / running / restarting / exited / paused / dead
    pub state: String,
    /// 例: `Up 3 minutes`
    pub status: String,
    #[serde(skip)]
    pub config_hash: String,
}

/// reconcile で行った操作。
#[derive(Debug, Default, Serialize)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub recreated: Vec<String>,
    pub started: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

impl ReconcileReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (label, list) in [
            ("作成", &self.created),
            ("再作成", &self.recreated),
            ("起動", &self.started),
            ("削除", &self.removed),
        ] {
            if !list.is_empty() {
                parts.push(format!("{label}: {}", list.join(", ")));
            }
        }
        if !self.errors.is_empty() {
            parts.push(format!("エラー: {}", self.errors.join(" / ")));
        }
        if parts.is_empty() {
            "変更なし".to_string()
        } else {
            parts.join("、")
        }
    }
}

async fn docker<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args: Vec<std::ffi::OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let sub = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out = Command::new("docker")
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await
        .context("docker コマンドを実行できません（Dockerソケットのマウントを確認してください）")?;
    if !out.status.success() {
        bail!(
            "docker {sub} が失敗しました: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Dockerに接続できるか。
pub async fn ping() -> Result<String> {
    let version = docker(["version", "--format", "{{.Server.Version}}"]).await?;
    Ok(version.trim().to_string())
}

/// イメージが無ければ取得する（初回は時間がかかる）。
pub async fn ensure_image(image: &str) -> Result<()> {
    if docker(["image", "inspect", "--format", "{{.Id}}", image])
        .await
        .is_ok()
    {
        return Ok(());
    }
    tracing::info!("イメージを取得しています: {image}");
    docker(["pull", "--quiet", image]).await?;
    tracing::info!("イメージを取得しました: {image}");
    Ok(())
}

/// 管理対象（ラベル付き）のコンテナ一覧。
pub async fn list_managed() -> Result<Vec<ContainerInfo>> {
    let format = format!(
        "{{{{.Names}}}}\t{{{{.State}}}}\t{{{{.Status}}}}\t{{{{.Label \"{LABEL_CAMERA}\"}}}}\t{{{{.Label \"{LABEL_HASH}\"}}}}"
    );
    let filter = format!("label={LABEL_MANAGED}=true");
    let out = docker([
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        filter.as_str(),
        "--format",
        format.as_str(),
    ])
    .await?;
    Ok(parse_ps(&out))
}

fn parse_ps(out: &str) -> Vec<ContainerInfo> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some(ContainerInfo {
                name: cols.next()?.to_string(),
                state: cols.next()?.to_string(),
                status: cols.next()?.to_string(),
                camera: cols.next().unwrap_or_default().to_string(),
                config_hash: cols.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// コンテナの設定を表すハッシュ。変わった場合はコンテナを作り直す。
pub fn spec_hash(cfg: &Config, cam: &Camera) -> Result<String> {
    let rendered = mediamtx::render_camera_config(cfg, cam)?;
    let args = run_args(cfg, cam, "").join("\u{1f}");
    // バージョン間で値が変わらないよう、自前のFNV-1a（64bit）を使う。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in rendered
        .as_bytes()
        .iter()
        .chain(mediamtx::TRANSCODE_SCRIPT.as_bytes())
        .chain(args.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

/// `docker run` の引数。
fn run_args(cfg: &Config, cam: &Camera, hash: &str) -> Vec<String> {
    let media = &cfg.media;
    let shared_dir = media.shared_dir.to_string_lossy();
    vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        cfg.container_name(&cam.id),
        "--label".into(),
        format!("{LABEL_MANAGED}=true"),
        "--label".into(),
        format!("{LABEL_CAMERA}={}", cam.id),
        "--label".into(),
        format!("{LABEL_HASH}={hash}"),
        "--restart".into(),
        "unless-stopped".into(),
        "--network".into(),
        media.docker_network.clone(),
        "--publish".into(),
        format!("{0}:{0}/udp", cam.webrtc_udp_port),
        "--publish".into(),
        format!("{0}:{0}/udp", cam.srt_port),
        "--volume".into(),
        format!("{}:{shared_dir}:ro", media.shared_volume),
        "--volume".into(),
        format!("{}:/recordings", media.recordings_volume),
        "--log-opt".into(),
        "max-size=10m".into(),
        "--log-opt".into(),
        "max-file=3".into(),
        media.image.clone(),
        mediamtx::config_path(cfg, &cam.id),
    ]
}

/// カメラのコンテナを作成して起動する。
pub async fn create(cfg: &Config, cam: &Camera) -> Result<()> {
    let hash = spec_hash(cfg, cam)?;
    docker(run_args(cfg, cam, &hash)).await?;
    tracing::info!("コンテナを作成しました: {}", cfg.container_name(&cam.id));
    Ok(())
}

/// コンテナを停止・削除する（存在しなければ何もしない）。
pub async fn remove(name: &str) -> Result<()> {
    match docker(["rm", "--force", name]).await {
        Ok(_) => {
            tracing::info!("コンテナを削除しました: {name}");
            Ok(())
        }
        Err(e) if e.to_string().contains("No such container") => Ok(()),
        Err(e) => Err(e),
    }
}

pub async fn restart(name: &str) -> Result<()> {
    docker(["restart", "--time", "5", name]).await?;
    tracing::info!("コンテナを再起動しました: {name}");
    Ok(())
}

pub async fn start(name: &str) -> Result<()> {
    docker(["start", name]).await?;
    Ok(())
}

/// コンテナのログ（標準出力・標準エラーを時刻順に結合）。
pub async fn logs(name: &str, tail: u32) -> Result<String> {
    let tail = tail.to_string();
    let out = Command::new("docker")
        .args(["logs", "--timestamps", "--tail", tail.as_str(), name])
        .kill_on_drop(true)
        .output()
        .await
        .context("docker コマンドを実行できません")?;
    if !out.status.success() {
        bail!("docker logs が失敗しました: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .map(str::to_string)
        .collect();
    // 行頭のRFC3339時刻で並べ替える。
    lines.sort();
    Ok(lines.join("\n"))
}

/// `AMoRe.yaml` のカメラ一覧とコンテナをそろえる。
pub async fn reconcile(cfg: &Config) -> Result<ReconcileReport> {
    mediamtx::write_shared_files(cfg)?;
    let existing: HashMap<String, ContainerInfo> = list_managed()
        .await?
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();
    let mut report = ReconcileReport::default();

    for cam in &cfg.cameras {
        let name = cfg.container_name(&cam.id);
        let result = async {
            let hash = spec_hash(cfg, cam)?;
            match existing.get(&name) {
                None => {
                    create(cfg, cam).await?;
                    report.created.push(name.clone());
                }
                Some(c) if c.config_hash != hash => {
                    remove(&name).await?;
                    create(cfg, cam).await?;
                    report.recreated.push(name.clone());
                }
                Some(c) if c.state != "running" && c.state != "restarting" => {
                    start(&name).await?;
                    report.started.push(name.clone());
                }
                Some(_) => {}
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!("{name}: {e:#}");
            report.errors.push(format!("{name}: {e:#}"));
        }
    }

    for (name, info) in &existing {
        let registered = cfg.cameras.iter().any(|c| cfg.container_name(&c.id) == *name);
        if !registered {
            match remove(name).await {
                Ok(()) => report.removed.push(format!("{name}（{}）", info.camera)),
                Err(e) => report.errors.push(format!("{name}: {e:#}")),
            }
        }
    }

    tracing::info!("コンテナを確認しました: {}", report.summary());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    #[test]
    fn parses_docker_ps_output() {
        let out = "amore-cam-cam1\trunning\tUp 3 minutes\tcam1\tabc\n\
                   amore-cam-cam2\texited\tExited (1) 5 seconds ago\tcam2\t\n\n";
        let list = parse_ps(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "amore-cam-cam1");
        assert_eq!(list[0].state, "running");
        assert_eq!(list[0].config_hash, "abc");
        assert_eq!(list[1].camera, "cam2");
        assert_eq!(list[1].config_hash, "");
    }

    #[test]
    fn run_args_publish_camera_ports() {
        let cfg = sample();
        let args = run_args(&cfg, &cfg.cameras[1], "h");
        let joined = args.join(" ");
        assert!(joined.contains("--name amore-cam-cam2"), "{joined}");
        assert!(joined.contains("--publish 8190:8190/udp"), "{joined}");
        assert!(joined.contains("--publish 8891:8891/udp"), "{joined}");
        assert!(joined.contains("--network amore_net"), "{joined}");
        assert!(joined.contains("--volume amore-shared:/shared:ro"), "{joined}");
        assert!(joined.contains("--label amore.config-hash=h"), "{joined}");
        assert!(joined.ends_with(
            "bluenviron/mediamtx:1.21.1-ffmpeg /shared/cameras/cam2/mediamtx.yml"
        ));
    }

    #[test]
    fn hash_changes_with_ports_but_not_name() {
        let cfg = sample();
        let base = spec_hash(&cfg, &cfg.cameras[0]).unwrap();
        let mut renamed = cfg.cameras[0].clone();
        renamed.name = "別名".into();
        // 表示名は mediamtx.yml のコメントに含まれるため、名前変更では作り直さないよう比較対象から外す。
        assert_eq!(spec_hash(&cfg, &renamed).unwrap(), base);
        let mut moved = cfg.cameras[0].clone();
        moved.srt_port = 8893;
        assert_ne!(spec_hash(&cfg, &moved).unwrap(), base);
    }
}
