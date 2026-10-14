//! `AMoRe.yaml` の読み込み・検証・更新。
//!
//! Server側の設定はすべてこのファイルで管理する（要件定義書 Mission）。
//! 未知のキーは誤記とみなしてエラーにする。
//! 管理Webからのカメラ追加・削除・名前変更は `cameras:` の部分だけを書き換え、
//! それ以外の記述（コメントを含む）は保持する。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// 開発段階1（v0.1〜v0.3）の同時送信カメラ数の上限。
pub const STAGE1_MAX_CAMERAS: usize = 4;

/// MediaMTX上でWebRTC受信に使うパス名の接尾辞（`cam1` → `cam1_in`）。
pub const INGEST_SUFFIX: &str = "_in";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub server: ServerConfig,
    pub media: MediaConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub cameras: Vec<Camera>,
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

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("設定ファイルを読み込めません: {}", path.display()))?;
        let config = Self::from_yaml(&text)
            .with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_yaml(text: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(text)?)
    }

    pub fn camera(&self, id: &str) -> Option<&Camera> {
        self.cameras.iter().find(|c| c.id == id)
    }

    /// カメラのメディア処理コンテナ名（Dockerネットワーク内のホスト名を兼ねる）。
    pub fn container_name(&self, camera_id: &str) -> String {
        format!("{}{}", self.media.container_prefix, camera_id)
    }

    /// カメラコンテナ用の設定を置く共有ボリューム内のディレクトリ。
    pub fn camera_shared_dir(&self, camera_id: &str) -> PathBuf {
        self.media.shared_dir.join("cameras").join(camera_id)
    }

    /// OBSのメディアソースに入力するSRT URL。ffmpegのlatencyはマイクロ秒で指定する。
    pub fn srt_url(&self, camera: &Camera) -> String {
        format!(
            "srt://{}:{}?streamid=read:{}&latency={}",
            self.server.public_host,
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
        let srt = first_free(self.media.srt_ports, |p| self.cameras.iter().any(|c| c.srt_port == p))?;
        Some((webrtc, srt))
    }

    /// 全項目を検証し、問題をまとめて返す。
    pub fn validate(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        if self.version != 1 {
            errors.push(format!("version は 1 を指定してください（現在: {}）", self.version));
        }

        let host = &self.server.public_host;
        if host.is_empty() || host.contains("://") || host.contains('/') || host.contains(':') {
            errors.push(format!(
                "server.public_host はスキーム・ポートなしのIPv4アドレスかホスト名にしてください（現在: {host:?}）"
            ));
        }

        let media = &self.media;
        if !is_valid_docker_name(&media.container_prefix) {
            errors.push("media.container_prefix は英数字・'-'・'_'・'.'で指定してください".to_string());
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
            errors.push("media.webrtc_udp_ports と media.srt_ports の範囲が重なっています（どちらもUDP）".to_string());
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
            errors.extend(camera_name_error(&cam.name).map(|e| format!("カメラ {:?}: {e}", cam.id)));
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
                errors.push(format!("webrtc_udp_port {} が重複しています", cam.webrtc_udp_port));
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
                if !(url.starts_with("stun:") || url.starts_with("turn:") || url.starts_with("turns:"))
                {
                    errors.push(format!(
                        "ice_servers[{i}] の {url:?} は stun: / turn: / turns: で始めてください"
                    ));
                }
            }
        }

        self.validate_recording(&mut errors);
        self.validate_integrations(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            bail!("AMoRe.yaml の検証に失敗しました:\n  - {}", errors.join("\n  - "))
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
            out.push(("monitoring.grafana.api_token".into(), m.grafana.api_token.clone()));
        }
        if self.ai.enabled {
            match self.ai.provider {
                AiProvider::Anthropic => {
                    out.push(("ai.anthropic.api_key".into(), self.ai.anthropic.api_key.clone()))
                }
                AiProvider::OpenaiCompatible if !self.ai.openai_compatible.api_key.is_empty() => out
                    .push((
                        "ai.openai_compatible.api_key".into(),
                        self.ai.openai_compatible.api_key.clone(),
                    )),
                AiProvider::OpenaiCompatible => {}
            }
        }
        out
    }

    fn validate_recording(&self, errors: &mut Vec<String>) {
        let r = &self.recording;
        if !r.host_dir.is_absolute() {
            errors.push("recording.host_dir は絶対パスで指定してください（例: /srv/amore/recordings）".to_string());
        }
        if !r.relative_template().contains("%path") {
            errors.push("recording.path_template には %path（カメラID）を含めてください".to_string());
        }
        if r.relative_template().starts_with('/') || r.relative_template().contains("..") {
            errors.push("recording.path_template は保存先からの相対パスにしてください".to_string());
        }
        if !["fmp4", "mpegts"].contains(&r.segment_format.as_str()) {
            errors.push("recording.segment_format は fmp4 または mpegts にしてください".to_string());
        }
        if !["none", "mp4", "mkv", "mov"].contains(&r.output_format.as_str()) {
            errors.push("recording.output_format は none・mp4・mkv・mov のいずれかにしてください".to_string());
        }
        if parse_duration_secs(&r.segment_duration).is_none_or(|s| s < 60) {
            errors.push("recording.segment_duration は 60s 以上（例: 30m・1h）にしてください".to_string());
        }
        if parse_duration_secs(&r.retention).is_none() {
            errors.push("recording.retention は 7d・12h・0 のように指定してください".to_string());
        }
    }

    fn validate_integrations(&self, errors: &mut Vec<String>) {
        let op = &self.secrets.onepassword_connect;
        if op.enabled {
            if !op.url.starts_with("http://") && !op.url.starts_with("https://") {
                errors.push("secrets.onepassword_connect.url は http(s):// で指定してください".to_string());
            }
            if parse_duration_secs(&op.cache_ttl).is_none() {
                errors.push("secrets.onepassword_connect.cache_ttl は 5m のように指定してください".to_string());
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
            && (!m.prometheus.path.starts_with('/') || m.prometheus.path.starts_with("/api/")
                || m.prometheus.path.starts_with("/admin") || m.prometheus.path.starts_with("/obs"))
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
        if m.grafana.enabled && (!m.grafana.url.starts_with("http") || m.grafana.api_token.is_empty()) {
            errors.push("monitoring.grafana.url（http(s)://）と api_token を指定してください".to_string());
        }

        let ai = &self.ai;
        if ai.enabled {
            match ai.provider {
                AiProvider::Anthropic => {
                    if ai.anthropic.api_key.is_empty() {
                        errors.push("ai.anthropic.api_key を指定してください（op:// 参照を推奨）".to_string());
                    }
                    if ai.anthropic.model.is_empty() || ai.anthropic.vision_model.is_empty() {
                        errors.push("ai.anthropic.model と vision_model を指定してください".to_string());
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
                    errors.push(format!("{name} は {} のいずれかにしてください", EFFORTS.join("・")));
                }
            }
            if ai.vision.enabled && (ai.vision.interval_sec < 10 || !(160..=1920).contains(&ai.vision.image_width)) {
                errors.push("ai.vision.interval_sec は 10 以上、image_width は 160～1920 にしてください".to_string());
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
        [vault, item, field] => Some((vault.to_string(), item.to_string(), None, field.to_string())),
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
        errors.push(format!("{name} の width/height は1以上、fps は1～60にしてください"));
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
        return Some(format!("カメラID {id:?} の末尾 {INGEST_SUFFIX:?} は内部用に予約されています"));
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
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
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

/// 稼働中の設定と `AMoRe.yaml` の書き換えを管理する。
pub struct ConfigStore {
    path: PathBuf,
    current: RwLock<Arc<Config>>,
}

impl ConfigStore {
    pub fn load(path: &Path) -> Result<Self> {
        let config = Config::load(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            current: RwLock::new(Arc::new(config)),
        })
    }

    /// 現在の設定（スナップショット）。
    pub fn get(&self) -> Arc<Config> {
        self.current.read().expect("config lock").clone()
    }

    /// カメラ一覧を変更して `AMoRe.yaml` へ保存する。
    /// カメラ以外の項目はファイルを手動で変更していても起動時の値のまま（再起動で反映）。
    pub fn update_cameras(&self, cameras: Vec<Camera>) -> Result<Arc<Config>> {
        let mut next = (*self.get()).clone();
        next.cameras = cameras;
        next.validate()?;

        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("設定ファイルを読み込めません: {}", self.path.display()))?;
        let new_text = replace_cameras_block(&text, &next.cameras);
        let reparsed = Config::from_yaml(&new_text).context("書き換え後の AMoRe.yaml を解析できません")?;
        anyhow::ensure!(
            reparsed.cameras == next.cameras,
            "AMoRe.yaml の cameras を正しく書き換えられませんでした（cameras: の記述を確認してください）"
        );
        write_preserving_owner(&self.path, &new_text)?;

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
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(meta.mode() & 0o777));
        }
    }
    let _ = std::fs::copy(path, &backup);
    std::fs::rename(&tmp, path).with_context(|| format!("置き換えできません: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../AMoRe.example.yaml");

    pub(crate) fn cam(id: &str, webrtc: u16, srt: u16) -> Camera {
        Camera {
            id: id.into(),
            name: format!("カメラ {id}"),
            webrtc_udp_port: webrtc,
            srt_port: srt,
            record: None,
        }
    }

    /// 同梱の設定例にカメラを2台加えたものをテストの基準にする。
    pub(crate) fn sample() -> Config {
        let mut cfg = Config::from_yaml(EXAMPLE).expect("同梱AMoRe.example.yamlの読み込み");
        cfg.cameras = vec![cam("cam1", 8189, 8890), cam("cam2", 8190, 8891)];
        cfg
    }

    #[test]
    fn bundled_example_is_valid() {
        let cfg = Config::from_yaml(EXAMPLE).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.cameras.is_empty());
        sample().validate().unwrap();
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
        assert!(err.contains("srt_port 9000 が media.srt_ports の範囲外"), "{err}");
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
        let text = replace_cameras_block(EXAMPLE, &cameras);
        let cfg = Config::from_yaml(&text).unwrap();
        assert_eq!(cfg.cameras, cameras);
        // カメラ以外の記述・コメントは変わらない。
        assert!(text.contains("# AMoRe.yaml - AMoRe v0.3 Server設定"));
        assert!(text.contains("video:"));
        assert_eq!(
            text.lines().count(),
            EXAMPLE.lines().count() + 8,
            "cameras: [] の1行が9行に置き換わる"
        );
        // 空に戻すと元の内容に一致する。
        assert_eq!(replace_cameras_block(&text, &[]), EXAMPLE);
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
            Some(("AMoRe".into(), "Claude API".into(), None, "credential".into()))
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
        let cfg = Config::from_yaml(&serde_yaml::to_string(&value).unwrap()).expect("v0.2形式の設定");
        assert_eq!(cfg.recording.segment_format, "mpegts");
        assert_eq!(cfg.recording.retention, "1d");
        assert_eq!(cfg.recording.relative_template(), "%path/%Y-%m-%d_%H-%M-%S-%f");
        cfg.validate().unwrap();
    }

    #[test]
    fn validates_integrations() {
        let mut cfg = sample();
        cfg.ai.enabled = true;
        cfg.ai.anthropic.api_key = "op://AMoRe/Claude/credential".into();
        cfg.ai.director.enabled = true;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("secrets.onepassword_connect.enabled が false"), "{err}");
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
}
