//! AMoRe (Aketi MoIP on Rust engine) v0.1 - 接続・設定管理サーバー
//!
//! 映像・音声の受信・変換・SRT送信は、カメラごとのコンテナ（MediaMTX＋FFmpeg）が担当し、
//! AMoRe は次を担当する。
//! - `AMoRe.yaml` の検証・更新と、カメラごとの `mediamtx.yml` の生成
//! - カメラの追加・削除に合わせたコンテナの自動作成・削除（1カメラ＝1コンテナ）
//! - 送信画面・OBSドック・管理WebのHTTPS配信
//! - WHIP（送信）・WHEP（映像確認）の中継と状態API

mod admin;
mod api;
mod config;
mod docker;
mod mediamtx;
mod whip_proxy;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::{Config, ConfigStore};

#[derive(Parser)]
#[command(name = "amore", version, about = "AMoRe v0.1 接続・設定管理サーバー")]
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
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
            "  {} {} -> コンテナ {} / {}",
            cam.id,
            cam.name,
            cfg.container_name(&cam.id),
            cfg.srt_url(cam)
        );
    }
    Ok(())
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

    let state = Arc::new(AppState::new(store)?);
    // イメージの取得・コンテナの作成は時間がかかるため、HTTPSの待受と並行して行う。
    tokio::spawn(admin::supervise(state.clone()));

    axum_server::bind_rustls(listen, tls)
        .serve(api::router(state).into_make_service())
        .await
        .context("HTTPSサーバーが停止しました")
}
