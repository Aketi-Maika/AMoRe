//! カメラごとのメディア処理コンテナの管理（1カメラ＝1コンテナ）。
//!
//! AMoRe コンテナにマウントしたDockerソケットを `docker` コマンドで操作する。
//! `AMoRe.yaml` のカメラ一覧を正として、コンテナの作成・再作成・削除をそろえる（reconcile）。
//! 作成したコンテナにはラベル `amore.managed=true` を付け、それ以外のコンテナには触れない。

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::{Camera, Config};
use crate::mediamtx;

const LABEL_MANAGED: &str = "amore.managed";
const LABEL_CAMERA: &str = "amore.camera";
const LABEL_HASH: &str = "amore.config-hash";

/// 管理対象コンテナの状態（`docker inspect` から取得）。
#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    pub name: String,
    pub camera: String,
    /// created / running / restarting / exited / paused / dead
    pub state: String,
    /// 画面表示用の説明（例: `起動 2026-10-14 10:00:01 / 再起動 0回`）
    pub status: String,
    /// Docker が自動で再起動した回数。
    pub restart_count: u64,
    pub exit_code: i64,
    pub oom_killed: bool,
    /// Docker が記録した起動エラー（ポート使用中など）。
    pub error: String,
    /// 接続中の Docker ネットワーク。
    pub networks: Vec<String>,
    pub started_at: String,
    #[serde(skip)]
    pub config_hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspect {
    name: String,
    #[serde(default)]
    restart_count: u64,
    state: InspectState,
    config: InspectConfig,
    network_settings: InspectNetworkSettings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectState {
    status: String,
    #[serde(default)]
    exit_code: i64,
    #[serde(default, rename = "OOMKilled")]
    oom_killed: bool,
    #[serde(default)]
    error: String,
    #[serde(default)]
    started_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectNetworkSettings {
    #[serde(default)]
    networks: Option<HashMap<String, serde_json::Value>>,
}

impl From<Inspect> for ContainerInfo {
    fn from(i: Inspect) -> Self {
        let labels = i.config.labels.unwrap_or_default();
        let mut networks: Vec<String> = i.network_settings.networks.unwrap_or_default().into_keys().collect();
        networks.sort();
        // 例: 2026-10-14T01:00:01.123456789Z → 2026-10-14 01:00:01
        let started = i.state.started_at.get(..19).unwrap_or("").replace('T', " ");
        let status = if started.is_empty() || started.starts_with("0001") {
            format!("未起動 / 再起動 {}回", i.restart_count)
        } else {
            format!("起動 {started}（UTC） / 再起動 {}回", i.restart_count)
        };
        ContainerInfo {
            name: i.name.trim_start_matches('/').to_string(),
            camera: labels.get(LABEL_CAMERA).cloned().unwrap_or_default(),
            state: i.state.status,
            status,
            restart_count: i.restart_count,
            exit_code: i.state.exit_code,
            oom_killed: i.state.oom_killed,
            error: i.state.error,
            networks,
            started_at: i.state.started_at,
            config_hash: labels.get(LABEL_HASH).cloned().unwrap_or_default(),
        }
    }
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

/// 管理対象（ラベル付き）のコンテナ一覧。再起動回数・終了コード・接続ネットワークまで取得する。
pub async fn list_managed() -> Result<Vec<ContainerInfo>> {
    let filter = format!("label={LABEL_MANAGED}=true");
    let ids = docker(["ps", "--all", "--quiet", "--no-trunc", "--filter", filter.as_str()]).await?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["inspect"];
    args.extend(ids.iter().copied());
    match docker(args).await {
        Ok(json) => parse_inspect(&json),
        Err(_) => {
            // 一覧取得と inspect の間に削除されたコンテナがあると全体が失敗するため、1つずつ取り直す。
            let mut all = Vec::new();
            for id in ids {
                if let Ok(json) = docker(["inspect", id]).await {
                    all.extend(parse_inspect(&json)?);
                }
            }
            Ok(all)
        }
    }
}

fn parse_inspect(json: &str) -> Result<Vec<ContainerInfo>> {
    let list: Vec<Inspect> =
        serde_json::from_str(json).context("docker inspect の出力を解析できません")?;
    Ok(list.into_iter().map(ContainerInfo::from).collect())
}

/// コンテナを Docker ネットワークへ接続する（接続が外れた場合の自動修復）。
pub async fn network_connect(network: &str, name: &str) -> Result<()> {
    docker(["network", "connect", network, name]).await?;
    tracing::warn!("コンテナ {name} をネットワーク {network} へ接続し直しました");
    Ok(())
}

/// 自動復旧をあきらめたコンテナを止め、Docker の自動再起動も止める（無限に再起動させない）。
pub async fn stop_and_hold(name: &str) -> Result<()> {
    docker(["update", "--restart", "no", name]).await?;
    docker(["stop", "--time", "5", name]).await?;
    Ok(())
}

/// 指定コンテナの静止画（JPEG）を、コンテナ内の FFmpeg で OBS 向け出力から1枚取り出す。
pub async fn snapshot_jpeg(container: &str, path: &str, width: u32) -> Result<Vec<u8>> {
    let url = format!("rtsp://127.0.0.1:{}/{path}", mediamtx::RTSP_PORT);
    let scale = format!("scale={width}:-2");
    let fut = Command::new("docker")
        .args([
            "exec", container, "ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin",
            "-rtsp_transport", "tcp", "-i", url.as_str(), "-frames:v", "1", "-vf", scale.as_str(),
            "-q:v", "5", "-f", "image2", "-c:v", "mjpeg", "pipe:1",
        ])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), fut)
        .await
        .context("静止画の取得がタイムアウトしました")?
        .context("docker exec を実行できません")?;
    if !out.status.success() || out.stdout.len() < 100 {
        bail!(
            "静止画を取得できません: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
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
        .chain(mediamtx::CONVERT_SCRIPT.as_bytes())
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
    // 録画の保存先（ホスト上のディレクトリ）。SELinux 有効時は共有ラベル（:z）を付ける。
    let recordings = format!(
        "{}:/recordings{}",
        cfg.recording.host_dir.to_string_lossy(),
        if cfg.recording.selinux_relabel { ":z" } else { "" }
    );
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
        recordings,
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
    // 自動復旧をあきらめて自動再起動を止めたコンテナも、手動で起動するときは元の再起動方針へ戻す。
    docker(["update", "--restart", "unless-stopped", name]).await?;
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
    fn parses_docker_inspect_output() {
        let json = r#"[
          {"Name":"/amore-cam-cam1","RestartCount":0,
           "State":{"Status":"running","ExitCode":0,"OOMKilled":false,"Error":"","StartedAt":"2026-10-14T01:00:01.123456789Z"},
           "Config":{"Labels":{"amore.camera":"cam1","amore.config-hash":"abc","amore.managed":"true"}},
           "NetworkSettings":{"Networks":{"amore_net":{"IPAddress":"172.30.40.3"}}}},
          {"Name":"/amore-cam-cam3","RestartCount":7,
           "State":{"Status":"restarting","ExitCode":1,"OOMKilled":false,"Error":"","StartedAt":"0001-01-01T00:00:00Z"},
           "Config":{"Labels":{"amore.camera":"cam3"}},
           "NetworkSettings":{"Networks":{}}}
        ]"#;
        let list = parse_inspect(json).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "amore-cam-cam1");
        assert_eq!(list[0].state, "running");
        assert_eq!(list[0].config_hash, "abc");
        assert_eq!(list[0].networks, vec!["amore_net"]);
        assert_eq!(list[0].status, "起動 2026-10-14 01:00:01（UTC） / 再起動 0回");
        assert_eq!(list[1].camera, "cam3");
        assert_eq!(list[1].restart_count, 7);
        assert_eq!(list[1].exit_code, 1);
        assert!(list[1].networks.is_empty());
        assert_eq!(list[1].status, "未起動 / 再起動 7回");
    }

    #[test]
    fn run_args_mount_recording_dir() {
        let mut cfg = sample();
        cfg.recording.host_dir = "/mnt/nas/amore".into();
        let joined = run_args(&cfg, &cfg.cameras[0], "h").join(" ");
        assert!(joined.contains("--volume /mnt/nas/amore:/recordings:z"), "{joined}");
        cfg.recording.selinux_relabel = false;
        let joined = run_args(&cfg, &cfg.cameras[0], "h").join(" ");
        assert!(joined.contains("--volume /mnt/nas/amore:/recordings "), "{joined}");
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
