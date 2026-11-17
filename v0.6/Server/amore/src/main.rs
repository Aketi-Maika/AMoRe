//! AMoRe (Aketi MoIP on Rust engine) v0.6 - 接続・設定管理サーバー
//!
//! 映像・音声の受信・変換・SRT送信・録画は、カメラごとのコンテナ（MediaMTX＋FFmpeg）が担当し、
//! AMoRe は次を担当する。
//! - 設定（`AMoRe.yaml` と include のファイル）の検証・更新と、カメラごとの `mediamtx.yml` の生成
//! - カメラの追加・削除に合わせたコンテナの自動作成・削除（1カメラ＝1コンテナ）と、不調の検知・自動復旧
//! - 送信画面・OBSドック・管理Web・モニター画面のHTTPS配信
//! - WHIP（送信）・WHEP（映像確認）の中継と状態API
//! - 秘密情報の取得（1Password Connect）、監視基盤への送信（Prometheus・Loki・Zabbix・Grafana）、AI API 連携
//! - 画面ごとの入室パスワード（access.yaml）
//! - 拠点ごとの Server による冗長化（cluster.yaml）：共通IP（keepalived）の設定、カメラ一覧の同期、Server 間の映像の中継
//! - カメラのコンテナを Docker（docker.rs）または Kubernetes（k8s.rs。AMoRe の Pod が k8s API でカメラの Pod を作る）で動かす
//! - 映像品質（送信画面の統計）とカメラPC → OBS の遅延（映像の時刻コードを OBS ドックが読む）の集計（telemetry.rs）

mod admin;
mod ai;
mod api;
mod auth;
mod cluster;
mod config;
mod docker;
mod events;
mod health;
mod k8s;
mod keepalived;
mod mediamtx;
mod monitoring;
mod runtime;
mod secrets;
mod telemetry;
mod whip_proxy;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, Subcommand};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::{Config, ConfigStore};
use crate::secrets::SecretResolver;

#[derive(Parser)]
#[command(name = "amore", version, about = "AMoRe v0.6 接続・設定管理サーバー")]
struct Cli {
    /// AMoRe.yaml のパス
    #[arg(short, long, global = true, default_value = "/config/AMoRe.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// HTTPSサーバーを起動し、カメラごとのコンテナを管理する
    Serve,
    /// 設定（AMoRe.yaml と include のファイル）を検証する
    Check,
    /// カメラごとに生成される mediamtx.yml を標準出力へ表示する
    RenderMediamtx,
    /// cluster.yaml から生成される keepalived の設定を標準出力へ表示する
    RenderKeepalived,
    /// media.runtime: kubernetes で AMoRe が適用するマニフェストを標準出力へ表示する
    RenderKubernetes,
    /// カメラ一覧（cameras.yaml）にコンテナをそろえる（1回だけ実行）
    Reconcile,
    /// コンテナのヘルスチェック（HTTPS待受を確認）
    Healthcheck,
}

fn main() -> ExitCode {
    // Loki への送信レイヤーは、monitoring.loki が有効で送信を開始したときだけ働く。
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .with(monitoring::loki::LokiLayer)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve => serve(&cli.config),
        Command::Check => check(&cli.config),
        Command::RenderMediamtx => render(&cli.config),
        Command::RenderKeepalived => render_keepalived(&cli.config),
        Command::RenderKubernetes => render_kubernetes(&cli.config),
        Command::Reconcile => reconcile_once(&cli.config),
        Command::Healthcheck => healthcheck(&cli.config),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("エラー: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn check(path: &Path) -> Result<()> {
    let (cfg, sources) = Config::load_with_sources(path)?;
    println!(
        "OK: {}（カメラ {} 台 / 上限 {} 台）",
        path.display(),
        cfg.cameras.len(),
        cfg.limits.max_cameras
    );
    println!("設定ファイル:");
    for file in &sources.files {
        let sections: Vec<&str> = sources
            .sections
            .iter()
            .filter(|(name, p)| p == file && name != "include")
            .map(|(name, _)| name.as_str())
            .collect();
        println!("  {}: {}", file.display(), sections.join(", "));
    }
    for cam in &cfg.cameras {
        println!(
            "  {} {} -> コンテナ {} / {}{}",
            cam.id,
            cam.name,
            cfg.container_name(&cam.id),
            cfg.srt_url(cam),
            if cfg.camera_records(cam) {
                "（録画）"
            } else {
                ""
            }
        );
    }
    println!(
        "録画: 保存先 {} / {} で記録し {} へ変換 / 保存期間 {}",
        cfg.recording.host_dir.display(),
        cfg.recording.segment_format,
        cfg.recording.output_format,
        cfg.recording.retention
    );
    if cfg.cluster.enabled {
        let k = &cfg.cluster.keepalived;
        println!(
            "冗長構成: この Server {} / カメラ用の共通IP {} / OBS 用の共通IP {} / keepalived {}",
            cfg.cluster.site,
            k.camera_vip,
            k.obs_vip,
            on_off(k.enabled)
        );
        for p in &cfg.cluster.peers {
            let me = if p.name == cfg.cluster.site {
                " ← この Server"
            } else {
                ""
            };
            println!("  {} {}（priority {}）{me}", p.name, p.url, p.priority);
        }
    } else {
        println!("冗長構成: 無効（cluster.enabled: false）");
    }
    match cfg.media.runtime {
        config::Runtime::Docker => println!("実行方式: docker（Docker ソケットでカメラのコンテナを作る）"),
        config::Runtime::Kubernetes => println!(
            "実行方式: kubernetes（拠点 {} / ノードのラベル {}={} / 録画の PVC {} / keepalived のノード {} 台）",
            cfg.kube_site(),
            cfg.kubernetes.site_label,
            cfg.kube_site(),
            if cfg.kubernetes.recordings_claim.is_empty() { "なし" } else { cfg.kubernetes.recordings_claim.as_str() },
            cfg.kubernetes.nodes.len()
        ),
    }
    let required: Vec<&str> = cfg
        .access
        .scopes()
        .into_iter()
        .filter(|(_, a)| a.password_required)
        .map(|(name, _)| name)
        .collect();
    println!(
        "入室パスワード: {}",
        if required.is_empty() {
            "なし".to_string()
        } else {
            required.join(", ")
        }
    );

    // 秘密情報の参照を実際に取得できるか確認する（値は表示しない）。
    let fields = cfg.secret_fields();
    if !fields.is_empty() {
        println!("秘密情報:");
        let resolver = SecretResolver::new(&cfg.secrets.onepassword_connect)?;
        let runtime = tokio::runtime::Runtime::new()?;
        let mut failed = 0;
        for (field, value) in fields {
            let kind = if config::is_secret_reference(&value) {
                "参照"
            } else {
                "直接記載"
            };
            match runtime.block_on(resolver.resolve(&value)) {
                Ok(v) if v.is_empty() => {
                    failed += 1;
                    println!("  NG {field}（{kind}）: 値が空です");
                }
                Ok(_) => println!("  OK {field}（{kind}・解決済み）"),
                Err(e) => {
                    failed += 1;
                    println!("  NG {field}（{kind}）: {e:#}");
                }
            }
        }
        anyhow::ensure!(failed == 0, "取得できない秘密情報があります（{failed}件）");
    }
    Ok(())
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "有効"
    } else {
        "無効"
    }
}

fn render(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    for cam in &cfg.cameras {
        println!("# ===== {} =====", mediamtx::config_path(&cfg, &cam.id));
        print!("{}", mediamtx::render_camera_config(&cfg, cam)?);
    }
    Ok(())
}

fn render_keepalived(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    anyhow::ensure!(
        keepalived::enabled(&cfg),
        "cluster.enabled と cluster.keepalived.enabled が true の場合だけ生成します"
    );
    if cfg.media.runtime == config::Runtime::Kubernetes {
        for node in &cfg.kubernetes.nodes {
            println!(
                "# ===== ConfigMap amore-keepalived / {}.conf =====",
                node.name
            );
            print!("{}", keepalived::render_for_node(&cfg, node)?);
        }
        return Ok(());
    }
    println!("# ===== {} =====", keepalived::conf_path(&cfg).display());
    print!("{}", keepalived::render(&cfg)?);
    Ok(())
}

fn render_kubernetes(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    anyhow::ensure!(
        cfg.media.runtime == config::Runtime::Kubernetes,
        "media.runtime: kubernetes の場合だけ生成します"
    );
    let mut docs = k8s::amore_services(&cfg);
    for cam in &cfg.cameras {
        docs.push(k8s::camera_configmap(&cfg, cam)?);
        docs.push(k8s::camera_deployment(
            &cfg,
            cam,
            &k8s::spec_hash(&cfg, cam)?,
        )?);
        docs.push(k8s::camera_service(&cfg, cam));
        docs.push(k8s::camera_site_service(&cfg, cam));
        docs.extend(k8s::camera_vip_services(&cfg, cam));
    }
    if keepalived::enabled(&cfg) {
        docs.push(k8s::keepalived_configmap(&cfg)?);
    }
    for doc in docs {
        println!("---");
        print!("{}", serde_yaml::to_string(&doc)?);
    }
    Ok(())
}

fn reconcile_once(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(runtime::reconcile(&cfg))?;
    println!("{}", report.summary());
    anyhow::ensure!(
        report.errors.is_empty(),
        "一部のコンテナを処理できませんでした"
    );
    Ok(())
}

fn healthcheck(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.server.listen.port()));
    TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .with_context(|| format!("{addr} で待ち受けていません"))?;
    Ok(())
}

fn serve(path: &Path) -> Result<()> {
    let store = ConfigStore::load(path)?;
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("TLS暗号プロバイダーを初期化できません"))?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(store))
}

async fn run(store: ConfigStore) -> Result<()> {
    let cfg = store.get();
    let tls = RustlsConfig::from_pem_file(&cfg.server.tls.cert, &cfg.server.tls.key)
        .await
        .with_context(|| {
            format!(
                "TLS証明書を読み込めません（cert: {} / key: {}）。scripts/gen-cert.sh で作成してください",
                cfg.server.tls.cert.display(),
                cfg.server.tls.key.display()
            )
        })?;

    let listen = cfg.server.listen;
    tracing::info!(
        "AMoRe v{} を https://{listen} で起動します",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!(
        "管理Web: https://{}/admin/ （カメラ {} 台 / 上限 {} 台）",
        cfg.server.public_host,
        cfg.cameras.len(),
        cfg.limits.max_cameras
    );

    tracing::info!(
        "録画: {}（保存先 {}）/ Loki: {} / Zabbix: {} / Grafana: {} / AI: {}",
        if cfg.recording.enabled {
            "全カメラ"
        } else {
            "カメラごとの設定"
        },
        cfg.recording.host_dir.display(),
        on_off(cfg.monitoring.loki.enabled),
        on_off(cfg.monitoring.zabbix.enabled),
        on_off(cfg.monitoring.grafana.enabled),
        on_off(cfg.ai.enabled),
    );

    let state = Arc::new(AppState::new(store)?);
    if cfg.monitoring.loki.enabled {
        monitoring::loki::start(state.clone());
    }
    if cfg.monitoring.zabbix.enabled {
        monitoring::zabbix::start(state.clone());
    }
    if cfg.ai.enabled {
        ai::start(state.clone());
    }
    if cfg.cluster.enabled {
        tracing::info!(
            "冗長構成: この Server {} / カメラ用の共通IP {} / OBS 用の共通IP {}",
            cfg.cluster.site,
            cfg.camera_host(),
            cfg.obs_host()
        );
        cluster::start(state.clone());
    }
    // イメージの取得・コンテナの作成は時間がかかるため、HTTPSの待受と並行して行う。
    tokio::spawn(admin::supervise(state.clone()));

    axum_server::bind_rustls(listen, tls)
        // 入室の総当たり対策で接続元のアドレスを使う。
        .serve(api::router(state).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("HTTPSサーバーが停止しました")
}
