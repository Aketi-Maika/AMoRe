//! `AMoRe.yaml` の読み込み・検証・更新。
//!
//! Server側の設定はすべて yaml で管理する（要件定義書 Mission）。
//! 長くなるため、`AMoRe.yaml` の `include` に書いたファイル（`cameras.yaml`・`cluster.yaml` など）へ
//! セクションごとに分けられる。各ファイルのトップレベルのキー（セクション）を1つの設定にまとめ、
//! 同じセクションが2つのファイルにあればエラーにする。1つの `AMoRe.yaml` に全部書いても読める。
//! 未知のキーは誤記とみなしてエラーにする。
//! 管理Web・Server 間の同期によるカメラの変更は、`cameras:` を含むファイルの `cameras:` と
//! `cameras_revision:` の行だけを書き換え、それ以外の記述（コメントを含む）は保持する。

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// 開発段階1（v0.1〜v0.6）の同時送信カメラ数の上限。
pub const STAGE1_MAX_CAMERAS: usize = 4;

/// MediaMTX上でWebRTC受信に使うパス名の接尾辞（`cam1` → `cam1_in`）。
pub const INGEST_SUFFIX: &str = "_in";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// セクションごとに分けた設定ファイル（`AMoRe.yaml` からの相対パス）。`AMoRe.yaml` にだけ書ける。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    pub server: ServerConfig,
    pub media: MediaConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub cameras: Vec<Camera>,
    /// カメラ一覧の改訂番号（変更した時刻のミリ秒）。Server 間の同期で新しい方を採用する。AMoRe が書き換える。
    #[serde(default)]
    pub cameras_revision: u64,
    #[serde(default)]
    pub video: VideoConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub srt: SrtConfig,
    #[serde(default)]
    pub quality_control: QualityControl,
    #[serde(default)]
    pub reconnect: Reconnect,
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
    #[serde(default)]
    pub recording: Recording,
    #[serde(default)]
    pub secrets: Secrets,
    #[serde(default)]
    pub monitoring: Monitoring,
    #[serde(default)]
    pub ai: Ai,
    #[serde(default)]
    pub cluster: Cluster,
    #[serde(default)]
    pub access: Access,
    #[serde(default)]
    pub kubernetes: Kubernetes,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// HTTPS待受アドレス（コンテナ内）。
    pub listen: SocketAddr,
    /// カメラPC・編集PCから見たServerのIPアドレスまたはホスト名。
    pub public_host: String,
    pub tls: TlsConfig,
    /// 送信画面（Camera/www）の配置先。
    pub camera_dir: PathBuf,
    /// OBSドック（OBS/dock）の配置先。
    pub obs_dir: PathBuf,
    /// 管理Web（Server/admin）の配置先。
    pub admin_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// カメラごとのメディア処理コンテナ（MediaMTX＋FFmpeg）の設定。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    /// カメラのコンテナを動かす仕組み（`docker` / `kubernetes`）。
    #[serde(default)]
    pub runtime: Runtime,
    /// カメラコンテナのイメージ（バージョンを固定する）。
    pub image: String,
    /// コンテナ名の接頭辞（`amore-cam-` → `amore-cam-cam1`）。
    pub container_prefix: String,
    /// AMoRe とカメラコンテナを接続するDockerネットワーク（事前に作成する）。
    pub docker_network: String,
    /// 上記ネットワークのサブネット。AMoReからのWHIP/WHEP中継・API利用を許可する。
    pub docker_subnet: String,
    /// 設定ファイル・変換スクリプトを共有するDockerボリューム名。
    pub shared_volume: String,
    /// 共有ボリュームのマウント先（AMoRe・カメラコンテナ共通）。
    pub shared_dir: PathBuf,
    /// v0.2まで録画に使っていたDockerボリューム名。v0.3で廃止（`recording.host_dir` を使う）。
    /// 既存の設定ファイルをそのまま読めるよう受け付けるが、使わない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recordings_volume: Option<String>,
    /// カメラごとに割り当てるWebRTC(ICE)のUDPポート範囲。
    pub webrtc_udp_ports: PortRange,
    /// カメラごとに割り当てるSRTのUDPポート範囲。
    pub srt_ports: PortRange,
    /// VP8等をH.264へ変換する場合のlibx264プリセット。
    #[serde(default = "default_x264_preset")]
    pub x264_preset: String,
    /// カメラコンテナのUDPソケットの受信バッファ（バイト）。キーフレームのバーストによる取りこぼしを防ぐ。
    /// ホストの `net.core.rmem_max` 以上にしても上限で切り詰められる。
    #[serde(default = "default_udp_read_buffer_bytes")]
    pub udp_read_buffer_bytes: u32,
}

/// カメラのコンテナを動かす仕組み。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// Docker（Compose）。AMoRe が Docker ソケットでカメラのコンテナを作る。
    #[default]
    Docker,
    /// Kubernetes。AMoRe の Pod が k8s API でカメラの Pod（Deployment）を作る。
    Kubernetes,
}

/// Kubernetes で動かす場合の設定（`kubernetes.yaml`。`media.runtime: kubernetes` の場合だけ使う）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kubernetes {
    /// 空なら AMoRe の Pod の namespace。
    pub namespace: String,
    /// 拠点を表すノードのラベル（値は `cluster.site`。冗長構成でなければ `default`）。
    pub site_label: String,
    /// 録画の保存先の PVC（空なら録画しない）。
    pub recordings_claim: String,
    /// 拠点の接続先IP（`server.public_host`）を割り当てる MetalLB の IPAddressPool。
    pub metallb_pool: String,
    pub camera_resources: CameraResources,
    /// クラスターの全ノード（keepalived の unicast の相手）。
    pub nodes: Vec<KubeNode>,
}

impl Default for Kubernetes {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            site_label: "amore.site".to_string(),
            recordings_claim: String::new(),
            metallb_pool: "amore".to_string(),
            camera_resources: CameraResources::default(),
            nodes: Vec::new(),
        }
    }
}

/// カメラの Pod の requests / limits（空なら指定しない）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraResources {
    pub cpu_request: String,
    pub memory_request: String,
    pub cpu_limit: String,
    pub memory_limit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KubeNode {
    /// ノード名（`kubectl get nodes`）。
    pub name: String,
    /// ノードの実IP（VLAN40）。
    pub ip: String,
    /// ノードの拠点（`cluster.peers[].name`）。
    pub site: String,
}

fn default_x264_preset() -> String {
    "ultrafast".to_string()
}

fn default_udp_read_buffer_bytes() -> u32 {
    8 * 1024 * 1024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }

    pub fn size(&self) -> usize {
        if self.last < self.first {
            0
        } else {
            usize::from(self.last - self.first) + 1
        }
    }

    fn overlaps(&self, other: &PortRange) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_cameras: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_cameras: STAGE1_MAX_CAMERAS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    /// SRTのStream ID（`read:<id>`）・コンテナ名にも使う識別子。
    pub id: String,
    pub name: String,
    /// このカメラのコンテナが使うWebRTC(ICE)のUDPポート。
    pub webrtc_udp_port: u16,
    /// このカメラのコンテナが使うSRTのUDPポート（OBSの接続先）。
    pub srt_port: u16,
    /// 録画の有無。省略時は `recording.enabled` に従う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    /// 送信先の冗長化。省略時は `cluster.default_redundancy` に従う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<Redundancy>,
}

/// カメラの送信先の冗長化。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Redundancy {
    /// 共通IPだけへ送る。Server の切替時は同じIPへ自動で張り直す（数秒途切れる）。
    #[default]
    Single,
    /// 共通IPに加え、共通IPを持っていない Server の実IPへも同時に送る（途切れない。上り帯域は2倍）。
    Dual,
}

impl Redundancy {
    pub fn as_str(self) -> &'static str {
        match self {
            Redundancy::Single => "single",
            Redundancy::Dual => "dual",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub target_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum VideoCodec {
    H264,
    VP8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VideoConfig {
    /// 標準映像品質（1080p30）。
    pub standard: VideoProfile,
    /// 帯域不足時の品質（720p30）。
    pub fallback: VideoProfile,
    /// ブラウザで優先するコーデックの順序。
    pub codec_preference: Vec<VideoCodec>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            standard: VideoProfile {
                width: 1920,
                height: 1080,
                fps: 30,
                target_bitrate_kbps: 6000,
                max_bitrate_kbps: 8000,
            },
            fallback: VideoProfile {
                width: 1280,
                height: 720,
                fps: 30,
                target_bitrate_kbps: 3000,
                max_bitrate_kbps: 4000,
            },
            codec_preference: vec![VideoCodec::H264, VideoCodec::VP8],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    /// ブラウザで音声を送信する初期値。
    pub enabled: bool,
    pub sample_rate: u32,
    pub max_channels: u8,
    pub bitrate_kbps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 48000,
            max_channels: 2,
            bitrate_kbps: 128,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SrtConfig {
    /// OBS(Caller)が指定するSRT遅延。LAN 120ms / WAN 200ms（暫定）。
    pub latency_ms: u32,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self { latency_ms: 120 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QualityControl {
    pub enabled: bool,
    pub stats_interval_ms: u32,
    pub downgrade_below_kbps: u32,
    pub downgrade_after_sec: u32,
    pub upgrade_above_kbps: u32,
    pub upgrade_after_sec: u32,
    pub relapse_window_sec: u32,
    pub hold_after_relapse_sec: u32,
}

impl Default for QualityControl {
    fn default() -> Self {
        Self {
            enabled: true,
            stats_interval_ms: 1000,
            downgrade_below_kbps: 4500,
            downgrade_after_sec: 5,
            upgrade_above_kbps: 8000,
            upgrade_after_sec: 15,
            relapse_window_sec: 15,
            hold_after_relapse_sec: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reconnect {
    /// 再試行間隔（秒）。末尾の値を以降の上限として繰り返す。
    pub backoff_sec: Vec<u32>,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self {
            backoff_sec: vec![1, 2, 4],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// 録画。各カメラのコンテナ（MediaMTX）が OBS向けの出力（H.264・AAC）を区切りごとにファイルへ保存し、
/// 区切りが閉じるたびに `output_format` へ再エンコードなしで変換する。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recording {
    /// 全カメラの既定。カメラごとに `cameras[].record` で上書きできる。
    pub enabled: bool,
    /// 保存先（Serverホスト上の絶対パス。NASのマウント先も可）。カメラコンテナに `/recordings` としてマウントする。
    #[serde(default = "default_recording_host_dir")]
    pub host_dir: PathBuf,
    /// SELinuxのラベル付け（`:z`）を行うか。NFS等ラベル付けできない保存先では false。
    #[serde(default = "default_true")]
    pub selinux_relabel: bool,
    /// 保存ファイル名（`/recordings` からの相対。`%path` はカメラID）。v0.2の `path` も受け付ける。
    #[serde(default = "default_path_template", alias = "path")]
    pub path_template: String,
    /// 録画中の形式（`fmp4` / `mpegts`）。停電・異常終了でも途中まで再生できる形式で書く。v0.2の `format` も受け付ける。
    #[serde(default = "default_segment_format", alias = "format")]
    pub segment_format: String,
    /// 1ファイルの長さ。
    #[serde(default = "default_segment_duration")]
    pub segment_duration: String,
    /// 区切りごとの変換先（`none` / `mp4` / `mkv` / `mov`）。再エンコードはしない。
    #[serde(default = "default_output_format")]
    pub output_format: String,
    /// 変換後に録画中の形式のファイルを残すか。
    #[serde(default)]
    pub keep_segments: bool,
    /// 保存期間（例: `7d`・`12h`。`0` で自動削除しない）。v0.2の `delete_after` も受け付ける。
    #[serde(default = "default_retention", alias = "delete_after")]
    pub retention: String,
}

fn default_true() -> bool {
    true
}
fn default_recording_host_dir() -> PathBuf {
    PathBuf::from("/srv/amore/recordings")
}
fn default_path_template() -> String {
    "%path/%Y-%m-%d_%H-%M-%S-%f".to_string()
}
fn default_segment_format() -> String {
    "fmp4".to_string()
}
fn default_segment_duration() -> String {
    "1h".to_string()
}
fn default_output_format() -> String {
    "mp4".to_string()
}
fn default_retention() -> String {
    "7d".to_string()
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            enabled: false,
            host_dir: default_recording_host_dir(),
            selinux_relabel: true,
            path_template: default_path_template(),
            segment_format: default_segment_format(),
            segment_duration: default_segment_duration(),
            output_format: default_output_format(),
            keep_segments: false,
            retention: default_retention(),
        }
    }
}

impl Recording {
    /// v0.2 の設定で `path` に `/recordings/` から始まるパスを書いていた場合も受け付ける。
    pub fn relative_template(&self) -> &str {
        self.path_template
            .strip_prefix("/recordings/")
            .unwrap_or(&self.path_template)
    }
}

/// 秘密情報の取得先。yamlには `op://保管庫/アイテム/項目`・`env:変数名`・`file:/パス` で参照を書く。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub onepassword_connect: OnePasswordConnect,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OnePasswordConnect {
    pub enabled: bool,
    /// Connect API サーバーのURL（Dockerネットワーク内）。
    pub url: String,
    /// Connect のアクセストークンを書いたファイル（AMoReコンテナ内のパス。Git管理外）。
    pub token_file: PathBuf,
    /// 取得した値を再利用する時間。
    pub cache_ttl: String,
}

impl Default for OnePasswordConnect {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://op-connect-api:8080".to_string(),
            token_file: PathBuf::from("/config/op-connect-token"),
            cache_ttl: "5m".to_string(),
        }
    }
}

/// 既存の監視基盤への送信。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Monitoring {
    #[serde(default)]
    pub prometheus: PrometheusConfig,
    #[serde(default)]
    pub loki: LokiConfig,
    #[serde(default)]
    pub zabbix: ZabbixConfig,
    #[serde(default)]
    pub grafana: GrafanaConfig,
    /// 送信画面から届く映像の統計（解像度・fps・ビットレート・損失・RTT）。
    #[serde(default)]
    pub quality: QualityStatsConfig,
    /// カメラPC → OBS の遅延計測（映像の時刻コードを OBS ドックが読む）。
    #[serde(default)]
    pub latency: LatencyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QualityStatsConfig {
    /// 送信画面が映像の統計を Server へ送り、メトリクスとして公開する。
    pub enabled: bool,
    /// 送信画面が統計を送る間隔（秒）。
    pub interval_sec: u32,
}

impl Default for QualityStatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_sec: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LatencyConfig {
    /// 送信画面が映像の下端に時刻コードを描き、OBS ドックが読み取って遅延を測る。
    /// 時刻コードは放送にも映るため、OBS のシーンで下端をクロップする。
    pub enabled: bool,
    /// OBS ドックが測る間隔（秒）。
    pub interval_sec: u32,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_sec: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrometheusConfig {
    /// `GET <path>` でメトリクスを公開する（Prometheus がスクレイプし、Grafana で表示する）。
    pub enabled: bool,
    pub path: String,
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/metrics".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LokiConfig {
    pub enabled: bool,
    /// 例: `http://10.20.0.20:3100`
    pub url: String,
    /// マルチテナント構成の場合のテナント（`X-Scope-OrgID`）。
    #[serde(default)]
    pub tenant: String,
    /// Basic認証（使わない場合は空）。password は秘密参照で指定する。
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// すべてのログに付けるラベル。
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// カメラのコンテナ（MediaMTX・FFmpeg）のログも送るか。
    #[serde(default = "default_true")]
    pub container_logs: bool,
}

impl Default for LokiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            tenant: String::new(),
            username: String::new(),
            password: String::new(),
            labels: Default::default(),
            container_logs: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZabbixConfig {
    pub enabled: bool,
    /// Zabbix Server（または Proxy）のアドレス。
    pub server: String,
    pub port: u16,
    /// Zabbix に登録したホスト名（テンプレート「AMoRe」をリンクする）。
    pub host: String,
    pub interval_sec: u32,
}

impl Default for ZabbixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: String::new(),
            port: 10051,
            host: "amore01".to_string(),
            interval_sec: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrafanaConfig {
    /// カメラの追加・削除・不調・復旧・録画切替・AIの異常検知を注釈（アノテーション）として記録する。
    pub enabled: bool,
    /// 例: `http://10.20.0.20:3000`
    pub url: String,
    /// サービスアカウントトークン（秘密参照で指定する）。
    pub api_token: String,
    /// 注釈に付けるタグ。
    #[serde(default = "default_grafana_tags")]
    pub tags: Vec<String>,
}

fn default_grafana_tags() -> Vec<String> {
    vec!["amore".to_string()]
}

impl Default for GrafanaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            api_token: String::new(),
            tags: default_grafana_tags(),
        }
    }
}

/// AI API 連携。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ai {
    pub enabled: bool,
    pub provider: AiProvider,
    #[serde(default)]
    pub anthropic: AnthropicConfig,
    #[serde(default)]
    pub openai_compatible: OpenAiCompatibleConfig,
    #[serde(default)]
    pub diagnosis: AiDiagnosis,
    #[serde(default)]
    pub vision: AiVision,
    #[serde(default)]
    pub director: AiDirector,
    #[serde(default = "default_ai_timeout")]
    pub timeout_sec: u32,
}

fn default_ai_timeout() -> u32 {
    120
}

impl Default for Ai {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: AiProvider::Anthropic,
            anthropic: AnthropicConfig::default(),
            openai_compatible: OpenAiCompatibleConfig::default(),
            diagnosis: AiDiagnosis::default(),
            vision: AiVision::default(),
            director: AiDirector::default(),
            timeout_sec: default_ai_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    #[default]
    Anthropic,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnthropicConfig {
    pub base_url: String,
    /// APIキー（秘密参照で指定する）。
    pub api_key: String,
    /// 障害診断・配信操作支援に使うモデル。
    pub model: String,
    /// 映像解析に使うモデル。
    pub vision_model: String,
    /// 拒否（refusal）時にサーバー側で別モデルへ切り替える。
    #[serde(default = "default_true")]
    pub fallbacks: bool,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "claude-opus-5-5".to_string(),
            vision_model: "claude-opus-5-5".to_string(),
            fallbacks: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiCompatibleConfig {
    /// 例: `http://10.80.0.50:8000/v1`（`/chat/completions` を付けない）
    pub base_url: String,
    /// APIキー（不要なサーバーでは空。秘密参照で指定する）。
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    /// 画像入力に対応したモデル。空なら `model` を使う。
    #[serde(default)]
    pub vision_model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiDiagnosis {
    pub enabled: bool,
    /// Claude の思考の深さ（low / medium / high / xhigh / max）。OpenAI互換では使わない。
    pub effort: String,
    /// 送るログの行数。
    pub log_lines: u32,
}

impl Default for AiDiagnosis {
    fn default() -> Self {
        Self {
            enabled: true,
            effort: "high".to_string(),
            log_lines: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiVision {
    /// 受信中のカメラの静止画を定期的にAIへ送り、内容と異常を判定する（画像が外部へ送られる）。
    pub enabled: bool,
    pub interval_sec: u32,
    /// 送る静止画の幅（px）。
    pub image_width: u32,
    pub effort: String,
}

impl Default for AiVision {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_sec: 120,
            image_width: 640,
            effort: "low".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiDirector {
    /// 映像解析の結果から「今使うと良いカメラ」を提案する（OBSは操作しない）。vision の有効化が必要。
    pub enabled: bool,
    pub interval_sec: u32,
    pub effort: String,
}

impl Default for AiDirector {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_sec: 30,
            effort: "low".to_string(),
        }
    }
}

/// 拠点ごとの Server による冗長化（`cluster.yaml`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cluster {
    pub enabled: bool,
    /// この Server の名前（`peers[].name` のどれか）。
    pub site: String,
    /// Server の一覧（実IP）。全 Server で同じ内容にする。
    pub peers: Vec<Peer>,
    pub keepalived: Keepalived,
    /// Server 同士の認証と入室トークンの署名に使う共通の秘密（秘密参照で指定する）。
    pub shared_secret: String,
    /// 他の Server の状態を確認する間隔（秒）。
    pub heartbeat_sec: u32,
    /// 応答がこの時間（秒）なければ停止とみなす。
    pub down_after_sec: u32,
    /// `cameras[].redundancy` を省略したカメラの方式。
    pub default_redundancy: Redundancy,
}

impl Default for Cluster {
    fn default() -> Self {
        Self {
            enabled: false,
            site: String::new(),
            peers: Vec::new(),
            keepalived: Keepalived::default(),
            shared_secret: String::new(),
            heartbeat_sec: 2,
            down_after_sec: 6,
            default_redundancy: Redundancy::Single,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub name: String,
    /// この Server の実IPのURL（例: `https://10.40.0.21`）。
    pub url: String,
    /// 共通IPを受け持つ優先度（1～254。大きいほど優先）。
    pub priority: u8,
}

impl Peer {
    /// URL のホスト部分（`https://10.40.0.21:443/` → `10.40.0.21`）。
    pub fn host(&self) -> &str {
        url_host(&self.url).unwrap_or("")
    }
}

/// 共通IP（VRRP）。AMoRe がこの設定から keepalived の設定ファイルを生成する。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keepalived {
    /// false の場合は keepalived を使わない（共通IPはルーター等で用意する）。
    pub enabled: bool,
    /// 共通IPを載せる VM の NIC（VLAN40 側）。
    pub interface: String,
    /// カメラ用の共通IP（CIDR）。カメラPCは `https://<このアドレス>/` を開く。
    pub camera_vip: String,
    /// OBS 用の共通IP（CIDR）。OBS の SRT URL にこのアドレスを使う。
    pub obs_vip: String,
    /// VRRP の識別番号（同じ VLAN の他の VRRP と重ねない）。
    pub camera_vrid: u8,
    pub obs_vrid: u8,
    /// VRRP の通知間隔（秒）。
    pub advert_int: u32,
    /// 優先度の高い Server が復旧しても共通IPを取り戻さない（余計な切替をしない）。
    pub nopreempt: bool,
}

impl Default for Keepalived {
    fn default() -> Self {
        Self {
            enabled: true,
            interface: "ens18".to_string(),
            camera_vip: "149.40.2.1/24".to_string(),
            obs_vip: "10.40.0.25/16".to_string(),
            camera_vrid: 51,
            obs_vrid: 52,
            advert_int: 1,
            nopreempt: true,
        }
    }
}

/// 画面ごとの入室パスワード（`access.yaml`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Access {
    /// 管理画面（/admin/）とカメラ・コンテナの操作。
    pub admin: ScreenAccess,
    /// カメラ画面（送信画面 /）と送信（WHIP）。
    pub camera: ScreenAccess,
    /// モニター画面（/admin/monitor.html）と映像確認（WHEP）。
    pub monitor: ScreenAccess,
    /// OBSドック（/obs/）と状態の取得。
    pub dock: ScreenAccess,
    /// 入室後、再入力が不要な時間。
    pub session_ttl: String,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            admin: ScreenAccess::default(),
            camera: ScreenAccess::default(),
            monitor: ScreenAccess::default(),
            dock: ScreenAccess::default(),
            session_ttl: "12h".to_string(),
        }
    }
}

impl Access {
    /// 入室の範囲ごとの設定（範囲名, 設定）。
    pub fn scopes(&self) -> [(&'static str, &ScreenAccess); 4] {
        [
            ("admin", &self.admin),
            ("camera", &self.camera),
            ("monitor", &self.monitor),
            ("dock", &self.dock),
        ]
    }

    pub fn scope(&self, name: &str) -> Option<&ScreenAccess> {
        self.scopes()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, a)| a)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenAccess {
    pub password_required: bool,
    /// パスワード（秘密参照で指定する）。
    pub password: String,
}

/// `https://host[:port][/]` のホスト部分。パスやユーザー情報を含むURLは受け付けない。
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains('/') || rest.contains('@') {
        return None;
    }
    let host = match rest.rsplit_once(':') {
        Some((h, port)) if port.parse::<u16>().is_ok() => h,
        Some(_) => return None,
        None => rest,
    };
    (!host.is_empty()).then_some(host)
}

/// `149.40.2.1/24` 形式（IPv4 の CIDR）を分解する。
pub fn parse_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = s.trim().split_once('/')?;
    let addr: Ipv4Addr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    (1..=32).contains(&prefix).then_some((addr, prefix))
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// `30s`・`5m`・`1h`・`7d`・`0` 形式の期間を秒に変換する。
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s == "0" {
        return Some(0);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let mul = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(mul)
}

/// 値が秘密情報の参照（`op://`・`env:`・`file:`）か。
pub fn is_secret_reference(value: &str) -> bool {
    value.starts_with("op://") || value.starts_with("env:") || value.starts_with("file:")
}

/// 設定を読み込んだファイルと、各セクションの出どころ。
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    /// 読み込んだ順（先頭が `AMoRe.yaml`）。
    pub files: Vec<PathBuf>,
    /// （セクション名, そのセクションを書いたファイル）。
    pub sections: Vec<(String, PathBuf)>,
}

impl ConfigSources {
    pub fn file_of(&self, section: &str) -> Option<&Path> {
        self.sections
            .iter()
            .find(|(name, _)| name == section)
            .map(|(_, p)| p.as_path())
    }
}

fn read_mapping(path: &Path, text: &str) -> Result<serde_yaml::Mapping> {
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))?;
    match value {
        serde_yaml::Value::Null => Ok(serde_yaml::Mapping::new()),
        serde_yaml::Value::Mapping(m) => Ok(m),
        _ => bail!(
            "{} の最上位は「セクション名: 内容」の形式にしてください",
            path.display()
        ),
    }
}

/// `main` と、その `include` に書いたファイルを読み、セクション単位でまとめる。
/// `read` はファイルの読み込み（テストでは差し替える）。
pub fn merge_files(
    main: &Path,
    read: &dyn Fn(&Path) -> Result<String>,
) -> Result<(serde_yaml::Value, ConfigSources)> {
    let mut merged = read_mapping(main, &read(main)?)?;
    let mut sources = ConfigSources {
        files: vec![main.to_path_buf()],
        sections: Vec::new(),
    };
    for key in merged.keys() {
        let name = key
            .as_str()
            .with_context(|| format!("{} のセクション名は文字列にしてください", main.display()))?;
        sources
            .sections
            .push((name.to_string(), main.to_path_buf()));
    }
    let includes: Vec<String> = match merged.get("include") {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(v) => serde_yaml::from_value(v.clone()).context(
            "include はファイル名の一覧（例: [cameras.yaml, cluster.yaml]）で指定してください",
        )?,
    };
    let base = main.parent().unwrap_or(Path::new("."));
    for inc in includes {
        anyhow::ensure!(!inc.trim().is_empty(), "include に空のファイル名があります");
        let path = base.join(inc.trim());
        anyhow::ensure!(
            !sources.files.contains(&path),
            "include に {} が2回書かれています",
            path.display()
        );
        let text = read(&path)
            .with_context(|| format!("include のファイルを読み込めません: {}", path.display()))?;
        for (key, value) in read_mapping(&path, &text)? {
            let name = key
                .as_str()
                .with_context(|| {
                    format!("{} のセクション名は文字列にしてください", path.display())
                })?
                .to_string();
            if name == "include" {
                bail!(
                    "{}: include は {} にだけ書けます（入れ子にはできません）",
                    path.display(),
                    main.display()
                );
            }
            if let Some(prev) = sources.file_of(&name) {
                bail!(
                    "セクション {name} が {} と {} の両方に書かれています（どちらか一方にしてください）",
                    prev.display(),
                    path.display()
                );
            }
            sources.sections.push((name, path.clone()));
            merged.insert(key, value);
        }
        sources.files.push(path);
    }
    if let (Some(cams), Some(rev)) = (
        sources.file_of("cameras"),
        sources.file_of("cameras_revision"),
    ) {
        anyhow::ensure!(
            cams == rev,
            "cameras_revision は cameras と同じファイル（{}）に書いてください",
            cams.display()
        );
    }
    Ok((serde_yaml::Value::Mapping(merged), sources))
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("設定ファイルを読み込めません: {}", path.display()))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::load_with_sources(path)?.0)
    }

    /// 読み込み・検証し、各セクションの出どころも返す。
    pub fn load_with_sources(path: &Path) -> Result<(Self, ConfigSources)> {
        let (value, sources) = merge_files(path, &read_file)?;
        let config = Self::from_value(value).with_context(|| {
            format!(
                "設定の形式が不正です（{}）",
                sources
                    .files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("・")
            )
        })?;
        config.validate()?;
        Ok((config, sources))
    }

    /// 1つの yaml 文字列から読む（テスト用。実行時は `merge_files` で include をまとめてから `from_value` で読む）。
    #[cfg(test)]
    pub fn from_yaml(text: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(text)?)
    }

    pub fn from_value(value: serde_yaml::Value) -> Result<Self> {
        Ok(serde_yaml::from_value(value)?)
    }

    /// カメラPCが接続する先（冗長構成ではカメラ用の共通IP）。
    pub fn camera_host(&self) -> String {
        match self.cluster_vip(&self.cluster.keepalived.camera_vip) {
            Some(ip) => ip.to_string(),
            None => self.server.public_host.clone(),
        }
    }

    /// OBS が接続する先（冗長構成では OBS 用の共通IP）。
    pub fn obs_host(&self) -> String {
        match self.cluster_vip(&self.cluster.keepalived.obs_vip) {
            Some(ip) => ip.to_string(),
            None => self.server.public_host.clone(),
        }
    }

    fn cluster_vip(&self, cidr: &str) -> Option<Ipv4Addr> {
        if !self.cluster.enabled {
            return None;
        }
        parse_cidr(cidr).map(|(ip, _)| ip)
    }

    /// この Server（`cluster.site`）。
    pub fn self_peer(&self) -> Option<&Peer> {
        self.cluster
            .peers
            .iter()
            .find(|p| p.name == self.cluster.site)
    }

    /// この Server 以外の Server。
    pub fn other_peers(&self) -> impl Iterator<Item = &Peer> {
        self.cluster
            .peers
            .iter()
            .filter(move |p| p.name != self.cluster.site)
    }

    pub fn camera_redundancy(&self, cam: &Camera) -> Redundancy {
        if !self.cluster.enabled {
            return Redundancy::Single;
        }
        cam.redundancy.unwrap_or(self.cluster.default_redundancy)
    }

    pub fn camera(&self, id: &str) -> Option<&Camera> {
        self.cameras.iter().find(|c| c.id == id)
    }

    /// カメラのメディア処理コンテナ名（Dockerネットワーク内のホスト名を兼ねる）。
    /// Kubernetes では全拠点が同じ namespace に入るため、冗長構成では拠点名を入れる
    /// （`amore-cam-hq-cam1`）。同じ名前の Service があるため、AMoRe からは名前で接続できる。
    pub fn container_name(&self, camera_id: &str) -> String {
        if self.media.runtime == Runtime::Kubernetes && self.cluster.enabled {
            format!(
                "{}{}-{}",
                self.media.container_prefix, self.cluster.site, camera_id
            )
        } else {
            format!("{}{}", self.media.container_prefix, camera_id)
        }
    }

    /// Kubernetes での拠点名（ノードのラベル・リソース名に使う）。
    pub fn kube_site(&self) -> &str {
        if self.cluster.enabled {
            &self.cluster.site
        } else {
            "default"
        }
    }

    /// カメラコンテナ用の設定を置く共有ボリューム内のディレクトリ。
    pub fn camera_shared_dir(&self, camera_id: &str) -> PathBuf {
        self.media.shared_dir.join("cameras").join(camera_id)
    }

    /// OBSのメディアソースに入力するSRT URL。ffmpegのlatencyはマイクロ秒で指定する。
    /// 冗長構成では OBS 用の共通IP（どの Server が受け持っても同じURL）。
    pub fn srt_url(&self, camera: &Camera) -> String {
        self.srt_url_at(&self.obs_host(), camera)
    }

    /// 指定した Server（実IP等）のSRT URL。Server 間の中継の取り込み元に使う。
    pub fn srt_url_at(&self, host: &str, camera: &Camera) -> String {
        format!(
            "srt://{}:{}?streamid=read:{}&latency={}",
            host,
            camera.srt_port,
            camera.id,
            u64::from(self.srt.latency_ms) * 1000
        )
    }

    /// 未使用の最小番号で `camN` を提案する。
    pub fn next_camera_id(&self) -> String {
        (1..)
            .map(|n| format!("cam{n}"))
            .find(|id| self.camera(id).is_none())
            .expect("無限列から必ず見つかる")
    }

    /// 未使用の (WebRTC UDPポート, SRTポート) を各範囲の小さい順に割り当てる。
    pub fn next_free_ports(&self) -> Option<(u16, u16)> {
        let webrtc = first_free(self.media.webrtc_udp_ports, |p| {
            self.cameras.iter().any(|c| c.webrtc_udp_port == p)
        })?;
        let srt = first_free(self.media.srt_ports, |p| {
            self.cameras.iter().any(|c| c.srt_port == p)
        })?;
        Some((webrtc, srt))
    }

    /// 全項目を検証し、問題をまとめて返す。
    pub fn validate(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        if self.version != 1 {
            errors.push(format!(
                "version は 1 を指定してください（現在: {}）",
                self.version
            ));
        }

        let host = &self.server.public_host;
        if host.is_empty() || host.contains("://") || host.contains('/') || host.contains(':') {
            errors.push(format!(
                "server.public_host はスキーム・ポートなしのIPv4アドレスかホスト名にしてください（現在: {host:?}）"
            ));
        }

        let media = &self.media;
        if !is_valid_docker_name(&media.container_prefix) {
            errors.push(
                "media.container_prefix は英数字・'-'・'_'・'.'で指定してください".to_string(),
            );
        }
        for (name, range) in [
            ("media.webrtc_udp_ports", media.webrtc_udp_ports),
            ("media.srt_ports", media.srt_ports),
        ] {
            if range.first == 0 || range.last < range.first {
                errors.push(format!("{name} は 1 以上で first ≦ last にしてください"));
            } else if range.size() < self.limits.max_cameras {
                errors.push(format!(
                    "{name} のポート数 {} が limits.max_cameras {} より少なくなっています",
                    range.size(),
                    self.limits.max_cameras
                ));
            }
        }
        if media.webrtc_udp_ports.overlaps(&media.srt_ports) {
            errors.push(
                "media.webrtc_udp_ports と media.srt_ports の範囲が重なっています（どちらもUDP）"
                    .to_string(),
            );
        }
        // 0 は OS の既定値を使う（MediaMTX の udpReadBufferSize: 0 と同じ）。
        let buffer = media.udp_read_buffer_bytes;
        if buffer != 0 && !(65_536..=67_108_864).contains(&buffer) {
            errors.push(
                "media.udp_read_buffer_bytes は 0（OS既定）または 65536～67108864 にしてください"
                    .to_string(),
            );
        }

        if self.limits.max_cameras == 0 || self.limits.max_cameras > STAGE1_MAX_CAMERAS {
            errors.push(format!(
                "limits.max_cameras は 1～{STAGE1_MAX_CAMERAS} にしてください（開発段階1の対応範囲）"
            ));
        }
        if self.cameras.len() > self.limits.max_cameras {
            errors.push(format!(
                "cameras の登録数 {} が limits.max_cameras {} を超えています",
                self.cameras.len(),
                self.limits.max_cameras
            ));
        }
        let mut ids = HashSet::new();
        let mut webrtc_ports = HashSet::new();
        let mut srt_ports = HashSet::new();
        for cam in &self.cameras {
            errors.extend(camera_id_error(&cam.id));
            if !ids.insert(cam.id.as_str()) {
                errors.push(format!("カメラID {:?} が重複しています", cam.id));
            }
            errors
                .extend(camera_name_error(&cam.name).map(|e| format!("カメラ {:?}: {e}", cam.id)));
            if !media.webrtc_udp_ports.contains(cam.webrtc_udp_port) {
                errors.push(format!(
                    "カメラ {:?} の webrtc_udp_port {} が media.webrtc_udp_ports の範囲外です",
                    cam.id, cam.webrtc_udp_port
                ));
            }
            if !media.srt_ports.contains(cam.srt_port) {
                errors.push(format!(
                    "カメラ {:?} の srt_port {} が media.srt_ports の範囲外です",
                    cam.id, cam.srt_port
                ));
            }
            if !webrtc_ports.insert(cam.webrtc_udp_port) {
                errors.push(format!(
                    "webrtc_udp_port {} が重複しています",
                    cam.webrtc_udp_port
                ));
            }
            if !srt_ports.insert(cam.srt_port) {
                errors.push(format!("srt_port {} が重複しています", cam.srt_port));
            }
        }

        validate_profile("video.standard", &self.video.standard, &mut errors);
        validate_profile("video.fallback", &self.video.fallback, &mut errors);
        if self.video.fallback.height > self.video.standard.height
            || self.video.fallback.width > self.video.standard.width
        {
            errors.push("video.fallback は video.standard 以下の解像度にしてください".to_string());
        }
        if self.video.codec_preference.is_empty() {
            errors.push("video.codec_preference を1つ以上指定してください".to_string());
        }
        let unique_codecs: HashSet<_> = self.video.codec_preference.iter().collect();
        if unique_codecs.len() != self.video.codec_preference.len() {
            errors.push("video.codec_preference に重複があります".to_string());
        }

        if ![44100, 48000].contains(&self.audio.sample_rate) {
            errors.push("audio.sample_rate は 48000 または 44100 にしてください".to_string());
        }
        if !(1..=2).contains(&self.audio.max_channels) {
            errors.push("audio.max_channels は 1 または 2 にしてください".to_string());
        }
        if !(32..=320).contains(&self.audio.bitrate_kbps) {
            errors.push("audio.bitrate_kbps は 32～320 にしてください".to_string());
        }

        if !(20..=8000).contains(&self.srt.latency_ms) {
            errors.push("srt.latency_ms は 20～8000 にしてください".to_string());
        }

        let qc = &self.quality_control;
        if qc.stats_interval_ms < 200 {
            errors.push("quality_control.stats_interval_ms は 200 以上にしてください".to_string());
        }
        if qc.downgrade_below_kbps >= qc.upgrade_above_kbps {
            errors.push(
                "quality_control.downgrade_below_kbps は upgrade_above_kbps より小さくしてください"
                    .to_string(),
            );
        }

        if self.reconnect.backoff_sec.is_empty() || self.reconnect.backoff_sec.contains(&0) {
            errors.push("reconnect.backoff_sec は1以上の値を1つ以上指定してください".to_string());
        }

        for (i, ice) in self.ice_servers.iter().enumerate() {
            if ice.urls.is_empty() {
                errors.push(format!("ice_servers[{i}].urls を指定してください"));
            }
            for url in &ice.urls {
                if !(url.starts_with("stun:")
                    || url.starts_with("turn:")
                    || url.starts_with("turns:"))
                {
                    errors.push(format!(
                        "ice_servers[{i}] の {url:?} は stun: / turn: / turns: で始めてください"
                    ));
                }
            }
        }

        self.validate_recording(&mut errors);
        self.validate_integrations(&mut errors);
        self.validate_cluster(&mut errors);
        self.validate_access(&mut errors);
        self.validate_kubernetes(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "設定（AMoRe.yaml・include のファイル）の検証に失敗しました:\n  - {}",
                errors.join("\n  - ")
            )
        }
    }
}

impl Config {
    /// このカメラを録画するか（カメラ個別の指定 > 全体の既定）。
    pub fn camera_records(&self, cam: &Camera) -> bool {
        cam.record.unwrap_or(self.recording.enabled)
    }

    /// yamlに書かれた秘密情報の参照（項目名, 値）。`check` での確認に使う。
    pub fn secret_fields(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, ice) in self.ice_servers.iter().enumerate() {
            if let Some(c) = &ice.credential {
                out.push((format!("ice_servers[{i}].credential"), c.clone()));
            }
        }
        let m = &self.monitoring;
        if m.loki.enabled && !m.loki.password.is_empty() {
            out.push(("monitoring.loki.password".into(), m.loki.password.clone()));
        }
        if m.grafana.enabled {
            out.push((
                "monitoring.grafana.api_token".into(),
                m.grafana.api_token.clone(),
            ));
        }
        if self.ai.enabled {
            match self.ai.provider {
                AiProvider::Anthropic => out.push((
                    "ai.anthropic.api_key".into(),
                    self.ai.anthropic.api_key.clone(),
                )),
                AiProvider::OpenaiCompatible if !self.ai.openai_compatible.api_key.is_empty() => {
                    out.push((
                        "ai.openai_compatible.api_key".into(),
                        self.ai.openai_compatible.api_key.clone(),
                    ))
                }
                AiProvider::OpenaiCompatible => {}
            }
        }
        if self.cluster.enabled {
            out.push((
                "cluster.shared_secret".into(),
                self.cluster.shared_secret.clone(),
            ));
        }
        for (scope, access) in self.access.scopes() {
            if access.password_required {
                out.push((format!("access.{scope}.password"), access.password.clone()));
            }
        }
        out
    }

    fn validate_cluster(&self, errors: &mut Vec<String>) {
        let c = &self.cluster;
        if !c.enabled {
            return;
        }
        if c.peers.is_empty() {
            errors.push("cluster.peers に Server を1台以上書いてください".to_string());
        }
        let mut names = HashSet::new();
        let mut priorities = HashSet::new();
        let mut hosts = HashSet::new();
        for (i, p) in c.peers.iter().enumerate() {
            if camera_id_error(&p.name).is_some() {
                errors.push(format!(
                    "cluster.peers[{i}].name {:?} は英小文字・数字・'-'・'_'の1～32文字にしてください",
                    p.name
                ));
            }
            if !names.insert(p.name.as_str()) {
                errors.push(format!(
                    "cluster.peers の name {:?} が重複しています",
                    p.name
                ));
            }
            if url_host(&p.url).is_none() {
                errors.push(format!(
                    "cluster.peers[{i}].url は https://<実IP> の形式にしてください（現在: {:?}）",
                    p.url
                ));
            } else if !hosts.insert(p.host()) {
                errors.push(format!("cluster.peers の url {:?} が重複しています", p.url));
            }
            if !(1..=254).contains(&p.priority) {
                errors.push(format!(
                    "cluster.peers[{i}].priority は 1～254 にしてください"
                ));
            } else if !priorities.insert(p.priority) {
                errors.push(format!(
                    "cluster.peers の priority {} が重複しています（Server ごとに変えてください）",
                    p.priority
                ));
            }
        }
        match self.self_peer() {
            None => errors.push(format!(
                "cluster.site {:?} が cluster.peers の name にありません",
                c.site
            )),
            Some(me) if url_host(&me.url).is_some() && me.host() != self.server.public_host => {
                errors.push(format!(
                    "cluster.peers の {:?} の url（{}）と server.public_host（{}）を同じアドレスにしてください",
                    me.name,
                    me.host(),
                    self.server.public_host
                ))
            }
            Some(_) => {}
        }
        if c.shared_secret.is_empty() {
            errors.push("cluster.shared_secret を指定してください（op:// 等の秘密参照を推奨。全 Server で同じ値）".to_string());
        }
        if c.heartbeat_sec == 0 || c.heartbeat_sec > 60 {
            errors.push("cluster.heartbeat_sec は 1～60 にしてください".to_string());
        }
        if c.down_after_sec <= c.heartbeat_sec {
            errors
                .push("cluster.down_after_sec は heartbeat_sec より大きくしてください".to_string());
        }

        let k = &c.keepalived;
        let camera_vip = parse_cidr(&k.camera_vip);
        let obs_vip = parse_cidr(&k.obs_vip);
        if camera_vip.is_none() {
            errors.push(format!(
                "cluster.keepalived.camera_vip は 149.40.2.1/24 のような IPv4 の CIDR で指定してください（現在: {:?}）",
                k.camera_vip
            ));
        }
        if obs_vip.is_none() {
            errors.push(format!(
                "cluster.keepalived.obs_vip は 10.40.0.25/16 のような IPv4 の CIDR で指定してください（現在: {:?}）",
                k.obs_vip
            ));
        }
        if let (Some((a, _)), Some((b, _))) = (camera_vip, obs_vip) {
            if a == b {
                errors.push(
                    "cluster.keepalived の camera_vip と obs_vip は別のアドレスにしてください"
                        .to_string(),
                );
            }
            if c.peers
                .iter()
                .any(|p| p.host() == a.to_string() || p.host() == b.to_string())
            {
                errors.push("cluster.keepalived の共通IPを Server の実IP（peers の url）と重ねないでください".to_string());
            }
        }
        if k.enabled {
            if k.interface.is_empty()
                || !k
                    .interface
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
            {
                errors.push("cluster.keepalived.interface に共通IPを載せる NIC 名（例: ens18）を指定してください".to_string());
            }
            if k.camera_vrid == 0 || k.obs_vrid == 0 || k.camera_vrid == k.obs_vrid {
                errors.push("cluster.keepalived の camera_vrid と obs_vrid は 1～255 の別々の値にしてください".to_string());
            }
            if !(1..=255).contains(&k.advert_int) {
                errors.push("cluster.keepalived.advert_int は 1～255 にしてください".to_string());
            }
        }
    }

    fn validate_kubernetes(&self, errors: &mut Vec<String>) {
        if self.media.runtime != Runtime::Kubernetes {
            return;
        }
        let k = &self.kubernetes;
        // リソース名（DNS-1035：英小文字・数字・'-'、63文字以内）に使うため。
        for cam in &self.cameras {
            if cam.id.contains('_') {
                errors.push(format!(
                    "カメラID {:?}: media.runtime: kubernetes では '_' を使えません（'-' を使ってください）",
                    cam.id
                ));
            }
            if self.container_name(&cam.id).len() + "-site".len() > 63 {
                errors.push(format!(
                    "カメラID {:?} が長すぎます（Kubernetes の名前は63文字まで）",
                    cam.id
                ));
            }
        }
        if !self
            .media
            .container_prefix
            .starts_with(|c: char| c.is_ascii_lowercase())
            || self.media.container_prefix.contains(['_', '.'])
            || self
                .media
                .container_prefix
                .chars()
                .any(|c| c.is_ascii_uppercase())
        {
            errors.push("media.container_prefix は Kubernetes では英小文字で始まる英小文字・数字・'-' にしてください".to_string());
        }
        if self.cluster.enabled && self.cluster.site.contains('_') {
            errors.push("cluster.site: Kubernetes では '_' を使えません".to_string());
        }
        let records = self.recording.enabled || self.cameras.iter().any(|c| self.camera_records(c));
        if records && k.recordings_claim.is_empty() {
            errors.push(
                "録画するには kubernetes.recordings_claim に保存先の PVC を指定してください"
                    .to_string(),
            );
        }
        if k.site_label.is_empty() {
            errors.push("kubernetes.site_label を指定してください（例: amore.site）".to_string());
        }
        if self.cluster.enabled && self.cluster.keepalived.enabled {
            if k.nodes.is_empty() {
                errors.push("kubernetes.nodes にクラスターの全ノード（name・ip・site）を書いてください（keepalived の相手）".to_string());
            }
            let mut names = HashSet::new();
            for (i, n) in k.nodes.iter().enumerate() {
                if n.ip.parse::<Ipv4Addr>().is_err() {
                    errors.push(format!(
                        "kubernetes.nodes[{i}].ip は IPv4 アドレスにしてください"
                    ));
                }
                if !self.cluster.peers.iter().any(|p| p.name == n.site) {
                    errors.push(format!(
                        "kubernetes.nodes[{i}].site {:?} が cluster.peers の name にありません",
                        n.site
                    ));
                }
                if !names.insert(n.name.as_str()) {
                    errors.push(format!(
                        "kubernetes.nodes の name {:?} が重複しています",
                        n.name
                    ));
                }
                if self.cluster.peers.iter().any(|p| p.host() == n.ip) {
                    errors.push(format!(
                        "kubernetes.nodes[{i}].ip がいずれかの Server の接続先IP（cluster.peers の url）と同じです。Kubernetes では接続先IPは MetalLB が割り当てるため、ノードには別の IP を使ってください"
                    ));
                }
            }
        }
    }

    fn validate_access(&self, errors: &mut Vec<String>) {
        for (scope, access) in self.access.scopes() {
            if access.password_required && access.password.is_empty() {
                errors.push(format!(
                    "access.{scope}.password_required が true ですが password が空です（秘密参照を推奨）"
                ));
            }
        }
        if parse_duration_secs(&self.access.session_ttl).is_none_or(|s| s < 60) {
            errors.push("access.session_ttl は 60s 以上（例: 12h）にしてください".to_string());
        }
    }

    fn validate_recording(&self, errors: &mut Vec<String>) {
        let r = &self.recording;
        if !r.host_dir.is_absolute() {
            errors.push(
                "recording.host_dir は絶対パスで指定してください（例: /srv/amore/recordings）"
                    .to_string(),
            );
        }
        if !r.relative_template().contains("%path") {
            errors
                .push("recording.path_template には %path（カメラID）を含めてください".to_string());
        }
        if r.relative_template().starts_with('/') || r.relative_template().contains("..") {
            errors.push("recording.path_template は保存先からの相対パスにしてください".to_string());
        }
        if !["fmp4", "mpegts"].contains(&r.segment_format.as_str()) {
            errors
                .push("recording.segment_format は fmp4 または mpegts にしてください".to_string());
        }
        if !["none", "mp4", "mkv", "mov"].contains(&r.output_format.as_str()) {
            errors.push(
                "recording.output_format は none・mp4・mkv・mov のいずれかにしてください"
                    .to_string(),
            );
        }
        if parse_duration_secs(&r.segment_duration).is_none_or(|s| s < 60) {
            errors.push(
                "recording.segment_duration は 60s 以上（例: 30m・1h）にしてください".to_string(),
            );
        }
        if parse_duration_secs(&r.retention).is_none() {
            errors.push("recording.retention は 7d・12h・0 のように指定してください".to_string());
        }
    }

    fn validate_integrations(&self, errors: &mut Vec<String>) {
        let op = &self.secrets.onepassword_connect;
        if op.enabled {
            if !op.url.starts_with("http://") && !op.url.starts_with("https://") {
                errors.push(
                    "secrets.onepassword_connect.url は http(s):// で指定してください".to_string(),
                );
            }
            if parse_duration_secs(&op.cache_ttl).is_none() {
                errors.push(
                    "secrets.onepassword_connect.cache_ttl は 5m のように指定してください"
                        .to_string(),
                );
            }
        }
        for (field, value) in self.secret_fields() {
            if value.starts_with("op://") {
                if !op.enabled {
                    errors.push(format!(
                        "{field} が op:// 参照ですが secrets.onepassword_connect.enabled が false です"
                    ));
                }
                if parse_op_reference(&value).is_none() {
                    errors.push(format!(
                        "{field} の参照は op://保管庫/アイテム/項目 の形式にしてください"
                    ));
                }
            }
        }

        let m = &self.monitoring;
        if m.prometheus.enabled
            && (!m.prometheus.path.starts_with('/')
                || m.prometheus.path.starts_with("/api/")
                || m.prometheus.path.starts_with("/admin")
                || m.prometheus.path.starts_with("/obs"))
        {
            errors.push("monitoring.prometheus.path は / で始まり、/api・/admin・/obs と重ならないパスにしてください".to_string());
        }
        if m.loki.enabled && !m.loki.url.starts_with("http") {
            errors.push("monitoring.loki.url を http(s):// で指定してください".to_string());
        }
        if m.zabbix.enabled {
            if m.zabbix.server.trim().is_empty() || m.zabbix.host.trim().is_empty() {
                errors.push("monitoring.zabbix.server と host を指定してください".to_string());
            }
            if m.zabbix.interval_sec < 10 {
                errors.push("monitoring.zabbix.interval_sec は 10 以上にしてください".to_string());
            }
        }
        if m.grafana.enabled
            && (!m.grafana.url.starts_with("http") || m.grafana.api_token.is_empty())
        {
            errors.push(
                "monitoring.grafana.url（http(s)://）と api_token を指定してください".to_string(),
            );
        }
        if m.quality.enabled && !(1..=60).contains(&m.quality.interval_sec) {
            errors.push("monitoring.quality.interval_sec は 1～60 にしてください".to_string());
        }
        if m.latency.enabled && !(1..=60).contains(&m.latency.interval_sec) {
            errors.push("monitoring.latency.interval_sec は 1～60 にしてください".to_string());
        }

        let ai = &self.ai;
        if ai.enabled {
            match ai.provider {
                AiProvider::Anthropic => {
                    if ai.anthropic.api_key.is_empty() {
                        errors.push(
                            "ai.anthropic.api_key を指定してください（op:// 参照を推奨）"
                                .to_string(),
                        );
                    }
                    if ai.anthropic.model.is_empty() || ai.anthropic.vision_model.is_empty() {
                        errors.push(
                            "ai.anthropic.model と vision_model を指定してください".to_string(),
                        );
                    }
                }
                AiProvider::OpenaiCompatible => {
                    let o = &ai.openai_compatible;
                    if !o.base_url.starts_with("http") || o.model.is_empty() {
                        errors.push("ai.openai_compatible.base_url（http(s)://…/v1）と model を指定してください".to_string());
                    }
                }
            }
            for (name, effort) in [
                ("ai.diagnosis.effort", &ai.diagnosis.effort),
                ("ai.vision.effort", &ai.vision.effort),
                ("ai.director.effort", &ai.director.effort),
            ] {
                if !EFFORTS.contains(&effort.as_str()) {
                    errors.push(format!(
                        "{name} は {} のいずれかにしてください",
                        EFFORTS.join("・")
                    ));
                }
            }
            if ai.vision.enabled
                && (ai.vision.interval_sec < 10 || !(160..=1920).contains(&ai.vision.image_width))
            {
                errors.push(
                    "ai.vision.interval_sec は 10 以上、image_width は 160～1920 にしてください"
                        .to_string(),
                );
            }
            if ai.director.enabled && !ai.vision.enabled {
                errors.push("ai.director を使うには ai.vision.enabled を true にしてください（映像解析の結果から提案します）".to_string());
            }
            if ai.director.enabled && ai.director.interval_sec < 10 {
                errors.push("ai.director.interval_sec は 10 以上にしてください".to_string());
            }
            if ai.timeout_sec < 10 {
                errors.push("ai.timeout_sec は 10 以上にしてください".to_string());
            }
        }
    }
}

/// `op://保管庫/アイテム/項目` または `op://保管庫/アイテム/セクション/項目` を分解する。
pub fn parse_op_reference(value: &str) -> Option<(String, String, Option<String>, String)> {
    let rest = value.strip_prefix("op://")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    match parts.as_slice() {
        [vault, item, field] => {
            Some((vault.to_string(), item.to_string(), None, field.to_string()))
        }
        [vault, item, section, field] => Some((
            vault.to_string(),
            item.to_string(),
            Some(section.to_string()),
            field.to_string(),
        )),
        _ => None,
    }
}

fn first_free(range: PortRange, used: impl Fn(u16) -> bool) -> Option<u16> {
    (range.first..=range.last).find(|p| !used(*p))
}

fn validate_profile(name: &str, p: &VideoProfile, errors: &mut Vec<String>) {
    if p.width == 0 || p.height == 0 || p.fps == 0 || p.fps > 60 {
        errors.push(format!(
            "{name} の width/height は1以上、fps は1～60にしてください"
        ));
    }
    if p.target_bitrate_kbps == 0 || p.target_bitrate_kbps > p.max_bitrate_kbps {
        errors.push(format!(
            "{name}.target_bitrate_kbps は1以上かつ max_bitrate_kbps 以下にしてください"
        ));
    }
}

/// カメラIDの形式を検査する。問題があれば説明を返す。
pub fn camera_id_error(id: &str) -> Option<String> {
    let bytes = id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_');
    if !valid {
        return Some(format!(
            "カメラID {id:?} は英小文字・数字・'-'・'_'の1～32文字で、英小文字か数字で始めてください"
        ));
    }
    if id.ends_with(INGEST_SUFFIX) {
        return Some(format!(
            "カメラID {id:?} の末尾 {INGEST_SUFFIX:?} は内部用に予約されています"
        ));
    }
    None
}

/// 表示名を検査する。問題があれば説明を返す。
pub fn camera_name_error(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        Some("表示名を入力してください".to_string())
    } else if name.chars().count() > 40 {
        Some("表示名は40文字以内にしてください".to_string())
    } else if name.chars().any(char::is_control) {
        Some("表示名に制御文字は使えません".to_string())
    } else {
        None
    }
}

fn is_valid_docker_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// YAMLの `cameras:` ブロックだけを書き換える。他の行（コメントを含む）は保持する。
pub fn replace_cameras_block(text: &str, cameras: &[Camera]) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut block = String::new();
    if cameras.is_empty() {
        block.push_str("cameras: []");
        block.push_str(newline);
    } else {
        block.push_str("cameras:");
        block.push_str(newline);
        for cam in cameras {
            // JSONの文字列表現はYAMLの二重引用符スカラーとして有効。数字だけのIDも文字列のまま保たれる。
            let quote = |s: &str| serde_json::to_string(s).expect("文字列のJSON化");
            let mut lines = vec![
                format!("  - id: {}", quote(&cam.id)),
                format!("    name: {}", quote(cam.name.trim())),
                format!("    webrtc_udp_port: {}", cam.webrtc_udp_port),
                format!("    srt_port: {}", cam.srt_port),
            ];
            if let Some(record) = cam.record {
                lines.push(format!("    record: {record}"));
            }
            if let Some(redundancy) = cam.redundancy {
                lines.push(format!("    redundancy: {}", redundancy.as_str()));
            }
            for line in lines {
                block.push_str(&line);
                block.push_str(newline);
            }
        }
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(start) = lines.iter().position(|l| l.starts_with("cameras:")) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(newline);
        out.push_str(&block);
        return out;
    };
    // ブロックの終わり = 次の行頭が字下げ・'-'・空行でない行（次のトップレベルのキーやコメント）。
    let mut end = start + 1;
    while end < lines.len() {
        let line = lines[end].trim_end_matches(['\r', '\n']);
        if line.is_empty()
            || line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with('-')
        {
            end += 1;
        } else {
            break;
        }
    }
    // 末尾の空行は次のブロックとの区切りとして残す。
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    let mut out = String::with_capacity(text.len() + block.len());
    lines[..start].iter().for_each(|l| out.push_str(l));
    out.push_str(&block);
    lines[end..].iter().for_each(|l| out.push_str(l));
    out
}

/// トップレベルの `key: 値` の1行を書き換える。無ければ末尾に加える。
pub fn replace_top_level_scalar(text: &str, key: &str, value: &str) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let prefix = format!("{key}:");
    let mut out = String::with_capacity(text.len() + 32);
    let mut found = false;
    for line in text.split_inclusive('\n') {
        if !found && line.starts_with(&prefix) {
            found = true;
            out.push_str(&format!("{key}: {value}"));
            out.push_str(if line.ends_with('\n') { newline } else { "" });
        } else {
            out.push_str(line);
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(&format!("{key}: {value}{newline}"));
    }
    out
}

/// `cameras:` を含むファイルの本文から、カメラ一覧と改訂番号だけを読む。
fn parse_cameras_file(path: &Path, text: &str) -> Result<(Vec<Camera>, u64)> {
    let map = read_mapping(path, text)?;
    let cameras = match map.get("cameras") {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(v) => serde_yaml::from_value(v.clone())?,
    };
    let revision = match map.get("cameras_revision") {
        None => 0,
        Some(v) => serde_yaml::from_value(v.clone())?,
    };
    Ok((cameras, revision))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 稼働中の設定と、カメラ一覧（`cameras:` を含むファイル）の書き換えを管理する。
pub struct ConfigStore {
    /// `cameras:` を書き換えるファイル（分割時は `cameras.yaml`）。
    cameras_file: PathBuf,
    sources: ConfigSources,
    current: RwLock<Arc<Config>>,
}

impl ConfigStore {
    pub fn load(path: &Path) -> Result<Self> {
        let (config, sources) = Config::load_with_sources(path)?;
        let cameras_file = sources.file_of("cameras").unwrap_or(path).to_path_buf();
        Ok(Self {
            cameras_file,
            sources,
            current: RwLock::new(Arc::new(config)),
        })
    }

    /// 現在の設定（スナップショット）。
    pub fn get(&self) -> Arc<Config> {
        self.current.read().expect("config lock").clone()
    }

    pub fn sources(&self) -> &ConfigSources {
        &self.sources
    }

    pub fn cameras_file(&self) -> &Path {
        &self.cameras_file
    }

    /// この Server でカメラ一覧を変更して保存する（改訂番号を進める）。
    /// カメラ以外の項目はファイルを手動で変更していても起動時の値のまま（再起動で反映）。
    pub fn update_cameras(&self, cameras: Vec<Camera>) -> Result<Arc<Config>> {
        let revision = now_millis().max(self.get().cameras_revision + 1);
        self.write_cameras(cameras, revision)
    }

    /// 他の Server から受けたカメラ一覧を、改訂番号が新しい場合だけ反映する。反映したら Some。
    pub fn apply_remote_cameras(
        &self,
        cameras: Vec<Camera>,
        revision: u64,
    ) -> Result<Option<Arc<Config>>> {
        if revision <= self.get().cameras_revision {
            return Ok(None);
        }
        // 内容が同じでも改訂番号はそろえる（次回以降の比較のため）。
        self.write_cameras(cameras, revision).map(Some)
    }

    fn write_cameras(&self, cameras: Vec<Camera>, revision: u64) -> Result<Arc<Config>> {
        let mut next = (*self.get()).clone();
        next.cameras = cameras;
        next.cameras_revision = revision;
        next.validate()?;

        let path = &self.cameras_file;
        let text = read_file(path)?;
        let new_text = replace_cameras_block(&text, &next.cameras);
        let new_text =
            replace_top_level_scalar(&new_text, "cameras_revision", &revision.to_string());
        let (cameras, rev) = parse_cameras_file(path, &new_text)
            .with_context(|| format!("書き換え後の {} を解析できません", path.display()))?;
        anyhow::ensure!(
            cameras == next.cameras && rev == revision,
            "{} の cameras を正しく書き換えられませんでした（cameras: の記述を確認してください）",
            path.display()
        );
        write_preserving_owner(path, &new_text)?;

        let next = Arc::new(next);
        *self.current.write().expect("config lock") = next.clone();
        Ok(next)
    }
}

/// 一時ファイルへ書いてから置き換える。直前の内容は `.bak` に残し、所有者を引き継ぐ。
fn write_preserving_owner(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("yaml.tmp");
    let backup = path.with_extension("yaml.bak");
    std::fs::write(&tmp, text).with_context(|| format!("書き込めません: {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{chown, MetadataExt, PermissionsExt};
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = chown(&tmp, Some(meta.uid()), Some(meta.gid()));
            let _ = std::fs::set_permissions(
                &tmp,
                std::fs::Permissions::from_mode(meta.mode() & 0o777),
            );
        }
    }
    let _ = std::fs::copy(path, &backup);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("置き換えできません: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 同梱の設定例（Server/config.example）。ファイル名と本文。
    pub(crate) const EXAMPLE_FILES: [(&str, &str); 9] = [
        (
            "AMoRe.yaml",
            include_str!("../../config.example/AMoRe.yaml"),
        ),
        (
            "cameras.yaml",
            include_str!("../../config.example/cameras.yaml"),
        ),
        (
            "recording.yaml",
            include_str!("../../config.example/recording.yaml"),
        ),
        (
            "secrets.yaml",
            include_str!("../../config.example/secrets.yaml"),
        ),
        (
            "access.yaml",
            include_str!("../../config.example/access.yaml"),
        ),
        (
            "cluster.yaml",
            include_str!("../../config.example/cluster.yaml"),
        ),
        (
            "monitoring.yaml",
            include_str!("../../config.example/monitoring.yaml"),
        ),
        ("ai.yaml", include_str!("../../config.example/ai.yaml")),
        (
            "kubernetes.yaml",
            include_str!("../../config.example/kubernetes.yaml"),
        ),
    ];

    /// 設定例を1つのファイルにまとめたもの（セクションは重ならないため連結すればよい）。
    pub(crate) const EXAMPLE: &str = concat!(
        include_str!("../../config.example/AMoRe.yaml"),
        include_str!("../../config.example/cameras.yaml"),
        include_str!("../../config.example/recording.yaml"),
        include_str!("../../config.example/secrets.yaml"),
        include_str!("../../config.example/access.yaml"),
        include_str!("../../config.example/cluster.yaml"),
        include_str!("../../config.example/monitoring.yaml"),
        include_str!("../../config.example/ai.yaml"),
        include_str!("../../config.example/kubernetes.yaml"),
    );

    const CAMERAS_EXAMPLE: &str = include_str!("../../config.example/cameras.yaml");

    pub(crate) fn cam(id: &str, webrtc: u16, srt: u16) -> Camera {
        Camera {
            id: id.into(),
            name: format!("カメラ {id}"),
            webrtc_udp_port: webrtc,
            srt_port: srt,
            record: None,
            redundancy: None,
        }
    }

    /// 同梱の設定例にカメラを2台加えたものをテストの基準にする。
    pub(crate) fn sample() -> Config {
        let mut cfg = Config::from_yaml(EXAMPLE).expect("同梱 config.example の読み込み");
        cfg.cameras = vec![cam("cam1", 8189, 8890), cam("cam2", 8190, 8891)];
        cfg
    }

    /// 冗長構成（2台・共通IPは既定値）を有効にした基準。
    pub(crate) fn cluster_sample() -> Config {
        let mut cfg = sample();
        cfg.cluster.enabled = true;
        cfg.cluster.shared_secret = "env:AMORE_CLUSTER_SECRET".into();
        cfg
    }

    /// Kubernetes で冗長構成（設定例のノード3台）にした基準。
    pub(crate) fn kube_sample() -> Config {
        let mut cfg = cluster_sample();
        cfg.media.runtime = Runtime::Kubernetes;
        cfg.media.docker_subnet = "10.42.0.0/16".into();
        cfg.kubernetes.recordings_claim = "amore-hq-recordings".into();
        cfg
    }

    #[test]
    fn validates_kubernetes() {
        let cfg = kube_sample();
        cfg.validate().unwrap();
        assert_eq!(cfg.container_name("cam1"), "amore-cam-hq-cam1");
        assert_eq!(cfg.kube_site(), "hq");
        assert_eq!(cfg.kubernetes.nodes.len(), 3);
        assert_eq!(
            sample().container_name("cam1"),
            "amore-cam-cam1",
            "docker では従来どおり"
        );

        let mut cfg = kube_sample();
        cfg.cameras.push(cam("cam_3", 8191, 8892));
        cfg.kubernetes.nodes[0].ip = "10.40.0.21".into();
        cfg.kubernetes.nodes[1].site = "osaka".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("\"cam_3\": media.runtime: kubernetes では '_'"),
            "{err}"
        );
        assert!(err.contains("MetalLB が割り当てる"), "{err}");
        assert!(err.contains("\"osaka\""), "{err}");

        let mut cfg = kube_sample();
        cfg.cluster.enabled = false;
        assert_eq!(cfg.kube_site(), "default");
        assert_eq!(cfg.container_name("cam1"), "amore-cam-cam1");
        cfg.validate().unwrap();
    }

    /// 設定例のファイルをメモリ上で読む（`/cfg/<名前>`）。
    fn read_example<'a>(extra: &'a [(&'a str, &'a str)]) -> impl Fn(&Path) -> Result<String> + 'a {
        move |path: &Path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            extra
                .iter()
                .chain(EXAMPLE_FILES.iter())
                .find(|(n, _)| *n == name)
                .map(|(_, t)| t.to_string())
                .with_context(|| format!("ない: {}", path.display()))
        }
    }

    #[test]
    fn bundled_example_is_valid() {
        let cfg = Config::from_yaml(EXAMPLE).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.cameras.is_empty());
        sample().validate().unwrap();
        cluster_sample().validate().unwrap();
    }

    #[test]
    fn merges_included_files_by_section() {
        let main = Path::new("/cfg/AMoRe.yaml");
        let (value, sources) = merge_files(main, &read_example(&[])).unwrap();
        let cfg = Config::from_value(value).unwrap();
        cfg.validate().unwrap();
        assert_eq!(sources.files.len(), 9);
        assert_eq!(
            sources.file_of("cameras"),
            Some(Path::new("/cfg/cameras.yaml"))
        );
        assert_eq!(
            sources.file_of("cameras_revision"),
            Some(Path::new("/cfg/cameras.yaml"))
        );
        assert_eq!(
            sources.file_of("cluster"),
            Some(Path::new("/cfg/cluster.yaml"))
        );
        assert_eq!(sources.file_of("server"), Some(main));
        // 分割しても1ファイルにまとめても同じ設定になる。
        let single = Config::from_yaml(EXAMPLE).unwrap();
        assert_eq!(
            serde_yaml::to_string(&cfg).unwrap(),
            serde_yaml::to_string(&single).unwrap()
        );
        assert_eq!(cfg.cluster.keepalived.camera_vip, "149.40.2.1/24");
        assert_eq!(cfg.cluster.keepalived.obs_vip, "10.40.0.25/16");
    }

    #[test]
    fn rejects_duplicate_and_nested_sections() {
        let main = Path::new("/cfg/AMoRe.yaml");
        let dup = [("cluster.yaml", "cluster: {}\nrecording: {}\n")];
        let err = merge_files(main, &read_example(&dup))
            .unwrap_err()
            .to_string();
        assert!(err.contains("セクション recording"), "{err}");
        assert!(
            err.contains("recording.yaml") && err.contains("cluster.yaml"),
            "{err}"
        );

        let nested = [("ai.yaml", "include: [x.yaml]\n")];
        let err = merge_files(main, &read_example(&nested))
            .unwrap_err()
            .to_string();
        assert!(err.contains("入れ子"), "{err}");

        let missing = [("AMoRe.yaml", "version: 1\ninclude: [nothing.yaml]\n")];
        let err = format!(
            "{:#}",
            merge_files(main, &read_example(&missing)).unwrap_err()
        );
        assert!(err.contains("nothing.yaml"), "{err}");

        // include を書かない1ファイルの設定も読める（v0.3 形式）。
        let mut whole: serde_yaml::Value = serde_yaml::from_str(EXAMPLE).unwrap();
        whole.as_mapping_mut().unwrap().remove("include");
        let whole = serde_yaml::to_string(&whole).unwrap();
        let single = [("AMoRe.yaml", whole.as_str())];
        let (value, sources) = merge_files(main, &read_example(&single)).unwrap();
        assert_eq!(sources.file_of("cameras"), Some(main));
        Config::from_value(value).unwrap().validate().unwrap();
    }

    #[test]
    fn store_rewrites_only_the_cameras_file() {
        let dir = std::env::temp_dir().join(format!("amore-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in EXAMPLE_FILES {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let store = ConfigStore::load(&dir.join("AMoRe.yaml")).unwrap();
        assert_eq!(store.cameras_file(), dir.join("cameras.yaml"));

        let next = store.update_cameras(vec![cam("cam1", 8189, 8890)]).unwrap();
        assert!(next.cameras_revision > 0);
        for (name, text) in EXAMPLE_FILES {
            let now = std::fs::read_to_string(dir.join(name)).unwrap();
            if name == "cameras.yaml" {
                assert!(now.contains("  - id: \"cam1\""), "{now}");
                assert!(now.contains(&format!("cameras_revision: {}", next.cameras_revision)));
                assert!(now.starts_with("# cameras.yaml"), "コメントを保持する");
            } else {
                assert_eq!(now, text, "{name} は変更しない");
            }
        }
        // 読み直しても同じ。
        let again = ConfigStore::load(&dir.join("AMoRe.yaml")).unwrap();
        assert_eq!(again.get().cameras, next.cameras);
        assert_eq!(again.get().cameras_revision, next.cameras_revision);

        // 他の Server から古い一覧が届いても反映しない。新しければ反映する。
        let old = next.cameras_revision - 1;
        assert!(again.apply_remote_cameras(vec![], old).unwrap().is_none());
        let newer = next.cameras_revision + 10;
        let applied = again
            .apply_remote_cameras(vec![cam("cam2", 8190, 8891)], newer)
            .unwrap()
            .expect("反映する");
        assert_eq!(applied.cameras_revision, newer);
        assert_eq!(applied.cameras[0].id, "cam2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_top_level_scalar() {
        assert_eq!(
            replace_top_level_scalar("a: 1\ncameras_revision: 5\nb: 2\n", "cameras_revision", "9"),
            "a: 1\ncameras_revision: 9\nb: 2\n"
        );
        assert_eq!(
            replace_top_level_scalar("a: 1", "cameras_revision", "9"),
            "a: 1\ncameras_revision: 9\n"
        );
        assert_eq!(
            replace_top_level_scalar("x:\r\n  cameras_revision: 1\r\n", "cameras_revision", "2"),
            "x:\r\n  cameras_revision: 1\r\ncameras_revision: 2\r\n",
            "字下げされた同名のキーは書き換えない"
        );
    }

    #[test]
    fn parses_cidr_and_peer_urls() {
        assert_eq!(
            parse_cidr("149.40.2.1/24"),
            Some(("149.40.2.1".parse().unwrap(), 24))
        );
        assert_eq!(parse_cidr("10.40.0.25/16").map(|c| c.1), Some(16));
        assert!(parse_cidr("149.40.2.1").is_none());
        assert!(parse_cidr("149.40.2.1/33").is_none());
        assert!(parse_cidr("149.40.2/24").is_none());
        assert_eq!(url_host("https://10.40.0.21"), Some("10.40.0.21"));
        assert_eq!(url_host("https://10.40.0.21:443/"), Some("10.40.0.21"));
        assert_eq!(url_host("http://10.40.0.21"), None);
        assert_eq!(url_host("https://10.40.0.21/admin"), None);
        assert_eq!(url_host("https://user@10.40.0.21"), None);
    }

    #[test]
    fn cluster_hosts_and_validation() {
        let cfg = sample();
        assert_eq!(cfg.camera_host(), "10.40.0.20", "冗長構成でなければ実IP");
        assert_eq!(cfg.obs_host(), "10.40.0.20");

        let mut cfg = cluster_sample();
        assert_eq!(cfg.camera_host(), "149.40.2.1");
        assert_eq!(cfg.obs_host(), "10.40.0.25");
        assert_eq!(
            cfg.srt_url(&cfg.cameras[0]),
            "srt://10.40.0.25:8890?streamid=read:cam1&latency=120000"
        );
        assert_eq!(cfg.self_peer().unwrap().name, "hq");
        assert_eq!(
            cfg.other_peers()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["site1"]
        );
        assert_eq!(cfg.camera_redundancy(&cfg.cameras[0]), Redundancy::Single);
        cfg.cameras[0].redundancy = Some(Redundancy::Dual);
        assert_eq!(cfg.camera_redundancy(&cfg.cameras[0]), Redundancy::Dual);

        cfg.cluster.site = "site2".into();
        cfg.cluster.keepalived.camera_vip = "149.40.2.1".into();
        cfg.cluster.keepalived.camera_vrid = 52;
        cfg.cluster.peers[1].priority = 100;
        cfg.cluster.shared_secret.clear();
        cfg.cluster.down_after_sec = 2;
        let err = cfg.validate().unwrap_err().to_string();
        for needle in [
            "cluster.site \"site2\"",
            "cluster.keepalived.camera_vip",
            "camera_vrid と obs_vrid",
            "priority 100 が重複",
            "cluster.shared_secret",
            "down_after_sec",
        ] {
            assert!(err.contains(needle), "{needle}: {err}");
        }

        let mut cfg = cluster_sample();
        cfg.server.public_host = "10.40.0.99".into();
        cfg.cluster.keepalived.obs_vip = "10.40.0.21/16".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("server.public_host（10.40.0.99）"), "{err}");
        assert!(err.contains("実IP（peers の url）と重ねない"), "{err}");
    }

    #[test]
    fn validates_access() {
        let mut cfg = sample();
        cfg.access.admin.password_required = true;
        cfg.access.session_ttl = "30s".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("access.admin.password_required"), "{err}");
        assert!(err.contains("access.session_ttl"), "{err}");
        cfg.access.admin.password = "op://AMoRe/AMoRe admin/password".into();
        cfg.access.session_ttl = "8h".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("access.admin.password が op:// 参照"), "{err}");
        cfg.secrets.onepassword_connect.enabled = true;
        cfg.validate().unwrap();
        assert!(cfg
            .secret_fields()
            .iter()
            .any(|(f, _)| f == "access.admin.password"));
        assert!(!cfg
            .secret_fields()
            .iter()
            .any(|(f, _)| f == "access.monitor.password"));
    }

    #[test]
    fn persists_redundancy() {
        let mut c = cam("cam1", 8189, 8890);
        c.redundancy = Some(Redundancy::Dual);
        let text = replace_cameras_block(CAMERAS_EXAMPLE, std::slice::from_ref(&c));
        assert!(text.contains("    redundancy: dual\n"));
        let (cameras, rev) = parse_cameras_file(Path::new("cameras.yaml"), &text).unwrap();
        assert_eq!(cameras, vec![c]);
        assert_eq!(rev, 0);
    }

    #[test]
    fn rejects_too_many_cameras() {
        let mut cfg = sample();
        cfg.cameras.push(cam("cam3", 8191, 8892));
        cfg.cameras.push(cam("cam4", 8192, 8893));
        cfg.validate().unwrap();
        cfg.cameras.push(cam("cam5", 8193, 8894));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("limits.max_cameras"), "{err}");
    }

    #[test]
    fn rejects_duplicates_and_invalid_ids() {
        let mut cfg = sample();
        cfg.cameras[1].id = "cam1".into();
        cfg.cameras[1].srt_port = 8890;
        cfg.cameras.push(cam("Cam/3", 8191, 8892));
        cfg.cameras.push(cam("cam4_in", 8192, 8893));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("カメラID \"cam1\" が重複"), "{err}");
        assert!(err.contains("srt_port 8890 が重複"), "{err}");
        assert!(err.contains("\"Cam/3\""), "{err}");
        assert!(err.contains("予約"), "{err}");
    }

    #[test]
    fn rejects_ports_out_of_range() {
        let mut cfg = sample();
        cfg.cameras[0].srt_port = 9000;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("srt_port 9000 が media.srt_ports の範囲外"),
            "{err}"
        );
    }

    #[test]
    fn validates_udp_read_buffer() {
        let mut cfg = sample();
        assert_eq!(cfg.media.udp_read_buffer_bytes, 8 * 1024 * 1024);
        cfg.media.udp_read_buffer_bytes = 0;
        cfg.validate().unwrap();
        cfg.media.udp_read_buffer_bytes = 1000;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("media.udp_read_buffer_bytes"), "{err}");
    }

    #[test]
    fn rejects_target_above_max_bitrate() {
        let mut cfg = sample();
        cfg.video.standard.target_bitrate_kbps = 9000;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("video.standard.target_bitrate_kbps"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        let mut value: serde_yaml::Value = serde_yaml::from_str(EXAMPLE).unwrap();
        value["srt"]
            .as_mapping_mut()
            .unwrap()
            .insert("latncy_ms".into(), 1.into());
        let text = serde_yaml::to_string(&value).unwrap();
        assert!(Config::from_yaml(&text).is_err());
    }

    #[test]
    fn allocates_next_id_and_ports() {
        let mut cfg = sample();
        assert_eq!(cfg.next_camera_id(), "cam3");
        assert_eq!(cfg.next_free_ports(), Some((8191, 8892)));
        cfg.cameras.remove(0);
        assert_eq!(cfg.next_camera_id(), "cam1");
        assert_eq!(cfg.next_free_ports(), Some((8189, 8890)));
    }

    #[test]
    fn srt_url_uses_camera_port_and_microseconds() {
        let cfg = sample();
        assert_eq!(
            cfg.srt_url(&cfg.cameras[1]),
            "srt://10.40.0.20:8891?streamid=read:cam2&latency=120000"
        );
    }

    #[test]
    fn replaces_cameras_block_and_keeps_comments() {
        let cameras = vec![cam("cam1", 8189, 8890), cam("123", 8190, 8891)];
        let text = replace_cameras_block(CAMERAS_EXAMPLE, &cameras);
        let (parsed, _) = parse_cameras_file(Path::new("cameras.yaml"), &text).unwrap();
        assert_eq!(parsed, cameras);
        // カメラ以外の記述・コメントは変わらない。
        assert!(text.contains("# cameras.yaml - カメラの一覧（AMoRe v0.6）"));
        assert!(text.contains("cameras_revision: 0"));
        assert_eq!(
            text.lines().count(),
            CAMERAS_EXAMPLE.lines().count() + 8,
            "cameras: [] の1行が9行に置き換わる"
        );
        // 空に戻すと元の内容に一致する。
        assert_eq!(replace_cameras_block(&text, &[]), CAMERAS_EXAMPLE);
        // 1ファイルの設定でも cameras だけが替わる。
        let whole = replace_cameras_block(EXAMPLE, &cameras);
        assert_eq!(Config::from_yaml(&whole).unwrap().cameras, cameras);
        assert!(whole.contains("video:"));
    }

    #[test]
    fn replaces_block_with_crlf_and_appends_when_missing() {
        let crlf = "version: 1\r\ncameras:\r\n  - id: a\r\n    name: x\r\n\r\nvideo: {}\r\n";
        let out = replace_cameras_block(crlf, &[cam("cam1", 1, 2)]);
        assert!(out.starts_with("version: 1\r\ncameras:\r\n  - id: \"cam1\"\r\n"));
        assert!(out.ends_with("    srt_port: 2\r\n\r\nvideo: {}\r\n"));

        let out = replace_cameras_block("version: 1\n", &[]);
        assert_eq!(out, "version: 1\n\ncameras: []\n");
    }

    #[test]
    fn persists_record_flag() {
        let mut c = cam("cam1", 8189, 8890);
        c.record = Some(true);
        let text = replace_cameras_block(EXAMPLE, std::slice::from_ref(&c));
        assert!(text.contains("    record: true\n"));
        let cfg = Config::from_yaml(&text).unwrap();
        assert_eq!(cfg.cameras, vec![c]);
        assert!(cfg.camera_records(&cfg.cameras[0]));
    }

    #[test]
    fn parses_durations_and_op_references() {
        assert_eq!(parse_duration_secs("7d"), Some(604_800));
        assert_eq!(parse_duration_secs("30m"), Some(1800));
        assert_eq!(parse_duration_secs("0"), Some(0));
        assert_eq!(parse_duration_secs("1w"), None);
        assert_eq!(parse_duration_secs("h"), None);
        assert_eq!(
            parse_op_reference("op://AMoRe/Claude API/credential"),
            Some((
                "AMoRe".into(),
                "Claude API".into(),
                None,
                "credential".into()
            ))
        );
        assert_eq!(
            parse_op_reference("op://AMoRe/Loki/auth/password").map(|r| r.2),
            Some(Some("auth".into()))
        );
        assert!(parse_op_reference("op://AMoRe//x").is_none());
        assert!(parse_op_reference("op://AMoRe").is_none());
    }

    #[test]
    fn accepts_v02_recording_keys() {
        let mut value: serde_yaml::Value = serde_yaml::from_str(EXAMPLE).unwrap();
        let v02: serde_yaml::Value = serde_yaml::from_str(
            "enabled: false\npath: /recordings/%path/%Y-%m-%d_%H-%M-%S-%f\nformat: mpegts\nsegment_duration: 1h\ndelete_after: 1d\n",
        )
        .unwrap();
        value["recording"] = v02;
        value["media"]
            .as_mapping_mut()
            .unwrap()
            .insert("recordings_volume".into(), "amore-recordings".into());
        let cfg =
            Config::from_yaml(&serde_yaml::to_string(&value).unwrap()).expect("v0.2形式の設定");
        assert_eq!(cfg.recording.segment_format, "mpegts");
        assert_eq!(cfg.recording.retention, "1d");
        assert_eq!(
            cfg.recording.relative_template(),
            "%path/%Y-%m-%d_%H-%M-%S-%f"
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn validates_integrations() {
        let mut cfg = sample();
        cfg.ai.enabled = true;
        cfg.ai.anthropic.api_key = "op://AMoRe/Claude/credential".into();
        cfg.ai.director.enabled = true;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("secrets.onepassword_connect.enabled が false"),
            "{err}"
        );
        assert!(err.contains("ai.vision.enabled"), "{err}");
        cfg.secrets.onepassword_connect.enabled = true;
        cfg.ai.vision.enabled = true;
        cfg.validate().unwrap();
        cfg.recording.host_dir = "recordings".into();
        cfg.recording.output_format = "avi".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("recording.host_dir"), "{err}");
        assert!(err.contains("recording.output_format"), "{err}");
    }

    #[test]
    fn validates_quality_and_latency_monitoring() {
        let mut cfg = sample();
        assert!(cfg.monitoring.quality.enabled, "設定例では有効");
        assert!(cfg.monitoring.latency.enabled, "設定例では有効");
        cfg.monitoring.quality.interval_sec = 0;
        cfg.monitoring.latency.interval_sec = 61;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("monitoring.quality.interval_sec"), "{err}");
        assert!(err.contains("monitoring.latency.interval_sec"), "{err}");
        // 無効にすれば間隔は問わない。
        cfg.monitoring.quality.enabled = false;
        cfg.monitoring.latency.enabled = false;
        cfg.validate().unwrap();
    }
}
