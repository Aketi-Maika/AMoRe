//! AMoRe (Aketi MoIP on Rust engine) v0.3 - 接続・設定管理サーバー
//!
//! 映像・音声の受信・変換・SRT送信・録画は、カメラごとのコンテナ（MediaMTX＋FFmpeg）が担当し、
//! AMoRe は次を担当する。
//! - `AMoRe.yaml` の検証・更新と、カメラごとの `mediamtx.yml` の生成
//! - カメラの追加・削除に合わせたコンテナの自動作成・削除（1カメラ＝1コンテナ）と、不調の検知・自動復旧
//! - 送信画面・OBSドック・管理Web・モニター画面のHTTPS配信
//! - WHIP（送信）・WHEP（映像確認）の中継と状態API
//! - 秘密情報の取得（1Password Connect）、監視基盤への送信（Prometheus・Loki・Zabbix・Grafana）、AI API 連携

mod admin;
mod ai;
mod api;
mod config;
mod docker;
mod events;
mod health;
mod mediamtx;
mod monitoring;
mod secrets;
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
#[command(name = "amore", version, about = "AMoRe v0.3 接続・設定管理サーバー")]
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
    /// AMoRe.yaml を検証する
    Check,
    /// カメラごとに生成される mediamtx.yml を標準出力へ表示する
    RenderMediamtx,
    /// AMoRe.yaml のカメラ一覧にコンテナをそろえる（1回だけ実行）
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
    let cfg = Config::load(path)?;
    println!(
        "OK: {}（カメラ {} 台 / 上限 {} 台）",
        path.display(),
        cfg.cameras.len(),
        cfg.limits.max_cameras
    );
    for cam in &cfg.cameras {
        println!(
            "  {} {} -> コンテナ {} / {}{}",
            cam.id,
            cam.name,
            cfg.container_name(&cam.id),
            cfg.srt_url(cam),
            if cfg.camera_records(cam) { "（録画）" } else { "" }
        );
    }
    println!(
        "録画: 保存先 {} / {} で記録し {} へ変換 / 保存期間 {}",
        cfg.recording.host_dir.display(),
        cfg.recording.segment_format,
        cfg.recording.output_format,
        cfg.recording.retention
    );

    // 秘密情報の参照を実際に取得できるか確認する（値は表示しない）。
    let fields = cfg.secret_fields();
    if !fields.is_empty() {
        println!("秘密情報:");
        let resolver = SecretResolver::new(&cfg.secrets.onepassword_connect)?;
        let runtime = tokio::runtime::Runtime::new()?;
        let mut failed = 0;
        for (field, value) in fields {
            let kind = if config::is_secret_reference(&value) { "参照" } else { "直接記載" };
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

fn reconcile_once(path: &Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let report = runtime.block_on(docker::reconcile(&cfg))?;
    println!("{}", report.summary());
    anyhow::ensure!(report.errors.is_empty(), "一部のコンテナを処理できませんでした");
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
    tracing::info!("AMoRe v{} を https://{listen} で起動します", env!("CARGO_PKG_VERSION"));
    tracing::info!(
        "管理Web: https://{}/admin/ （カメラ {} 台 / 上限 {} 台）",
        cfg.server.public_host,
        cfg.cameras.len(),
        cfg.limits.max_cameras
    );

    tracing::info!(
        "録画: {}（保存先 {}）/ Loki: {} / Zabbix: {} / Grafana: {} / AI: {}",
        if cfg.recording.enabled { "全カメラ" } else { "カメラごとの設定" },
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
    // イメージの取得・コンテナの作成は時間がかかるため、HTTPSの待受と並行して行う。
    tokio::spawn(admin::supervise(state.clone()));

    axum_server::bind_rustls(listen, tls)
        .serve(api::router(state).into_make_service())
        .await
        .context("HTTPSサーバーが停止しました")
}
