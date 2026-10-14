//! カメラごとのメディア処理コンテナ（MediaMTX＋FFmpeg）の設定と状態取得。
//!
//! - `AMoRe.yaml` からカメラ1台分の `mediamtx.yml` を生成する（MediaMTX v1.21系のキー名）。
//! - 変換スクリプトと各カメラの設定を共有ボリュームへ書き出す。
//! - MediaMTX 制御API（`/v3/paths/...`）から受信状態を取得する。
//!
//! 各コンテナは2つのパスを持つ。
//! - `<id>_in`：ブラウザからWebRTC(WHIP)で受信する。受信開始時に変換スクリプトを起動する。
//! - `<id>`   ：変換後（H.264・AAC）のストリーム。OBSはSRT `read:<id>` で読み取る。

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{is_secret_reference, parse_duration_secs, Camera, Config, INGEST_SUFFIX};

/// コンテナ内のMediaMTX制御APIのポート（Dockerネットワーク内のみ）。
pub const API_PORT: u16 = 9997;
/// コンテナ内のWebRTC(WHIP/WHEP) HTTPのポート（Dockerネットワーク内のみ）。
pub const WEBRTC_HTTP_PORT: u16 = 8889;
/// コンテナ内のRTSPのポート（変換スクリプトの入出力に使う。外部へ公開しない）。
pub const RTSP_PORT: u16 = 8554;

/// 変換スクリプト（バイナリに組み込み、起動時に共有ボリュームへ書き出す）。
pub const TRANSCODE_SCRIPT: &str = include_str!("../assets/transcode.sh");
/// 録画ファイルの変換スクリプト（同上）。
pub const CONVERT_SCRIPT: &str = include_str!("../assets/convert.sh");

/// MediaMTX上の受信パス名。
pub fn ingest_path(camera_id: &str) -> String {
    format!("{camera_id}{INGEST_SUFFIX}")
}

/// カメラコンテナのMediaMTX APIのURL。
pub fn api_base(cfg: &Config, camera_id: &str) -> String {
    format!("http://{}:{API_PORT}", cfg.container_name(camera_id))
}

/// カメラコンテナのWebRTC HTTPのURL。
pub fn webrtc_base(cfg: &Config, camera_id: &str) -> String {
    format!("http://{}:{WEBRTC_HTTP_PORT}", cfg.container_name(camera_id))
}

/// カメラコンテナ内での `mediamtx.yml` のパス（コンテナの起動引数）。
pub fn config_path(cfg: &Config, camera_id: &str) -> String {
    cfg.camera_shared_dir(camera_id)
        .join("mediamtx.yml")
        .to_string_lossy()
        .into_owned()
}

/// カメラ1台分の `mediamtx.yml` の内容を生成する。
pub fn render_camera_config(cfg: &Config, cam: &Camera) -> Result<String> {
    let media = &cfg.media;
    let video = &cfg.video.standard;
    let audio = &cfg.audio;
    let ingest = ingest_path(&cam.id);
    let output = cam.id.clone();

    // 引数: 入力パス 出力パス 目標kbps 上限kbps fps 標本化周波数 最大ch 音声kbps x264プリセット
    let transcode = format!(
        "sh {script} {ingest} {output} {vt} {vm} {fps} {rate} {ch} {ab} {preset}",
        script = media.shared_dir.join("transcode.sh").to_string_lossy(),
        vt = video.target_bitrate_kbps,
        vm = video.max_bitrate_kbps,
        fps = video.fps,
        rate = audio.sample_rate,
        ch = audio.max_channels,
        ab = audio.bitrate_kbps,
        preset = media.x264_preset,
    );

    let mut paths = serde_json::Map::new();
    paths.insert(
        ingest.clone(),
        json!({
            "source": "publisher",
            // 同一カメラへの再接続（切断からの復旧）で古いセッションを置き換える。
            // 別の送信画面からの重複接続は AMoRe 側で 409 として拒否する。
            "overridePublisher": true,
            "runOnAvailable": transcode,
            "runOnAvailableRestart": true,
        }),
    );
    let mut output_conf = json!({
        "source": "publisher",
        "overridePublisher": true,
        "record": cfg.camera_records(cam),
    });
    if cfg.camera_records(cam) && cfg.recording.output_format != "none" {
        // 区切りが閉じるたびに再エンコードなしで変換する。引数: ファイル 変換先 元を残すか 保存期間(分)
        let retention_min = parse_duration_secs(&cfg.recording.retention).unwrap_or(0) / 60;
        output_conf["runOnRecordSegmentComplete"] = json!(format!(
            "sh {script} $MTX_SEGMENT_PATH {fmt} {keep} {retention_min}",
            script = media.shared_dir.join("convert.sh").to_string_lossy(),
            fmt = cfg.recording.output_format,
            keep = u8::from(cfg.recording.keep_segments),
        ));
    }
    paths.insert(output.clone(), output_conf);

    // 認証情報が未解決の参照（op:// 等）のままの ICE サーバーは渡さない（参照文字列をパスワードとして使わない）。
    let ice_servers: Vec<Value> = cfg
        .ice_servers
        .iter()
        .filter(|s| !s.credential.as_deref().is_some_and(is_secret_reference))
        .flat_map(|s| {
            s.urls.iter().map(move |url| {
                json!({
                    "url": url,
                    "username": s.username.clone().unwrap_or_default(),
                    "password": s.credential.clone().unwrap_or_default(),
                })
            })
        })
        .collect();

    let doc = json!({
        "logLevel": "info",
        // UDPの受信バッファ。既定（OS既定 約208KB）ではキーフレームのバーストで取りこぼしが出る。
        "udpReadBufferSize": media.udp_read_buffer_bytes,
        "authMethod": "internal",
        "authInternalUsers": [
            // AMoReコンテナ（Dockerネットワーク）: WHIP中継による送信、WHEP中継による映像確認、APIの参照。
            {
                "user": "any", "pass": "", "ips": [media.docker_subnet],
                "permissions": [
                    { "action": "publish", "path": ingest },
                    { "action": "read", "path": ingest },
                    { "action": "api" },
                ],
            },
            // コンテナ内（変換スクリプト）: 受信パスの読み取り、出力パスへの送信、APIの参照。
            {
                "user": "any", "pass": "", "ips": ["127.0.0.1", "::1"],
                "permissions": [
                    { "action": "read", "path": ingest },
                    { "action": "publish", "path": output },
                    { "action": "api" },
                    { "action": "metrics" },
                ],
            },
            // 任意のアドレス（OBS）: 出力パスの読み取りのみ。
            {
                "user": "any", "pass": "", "ips": [],
                "permissions": [ { "action": "read", "path": output } ],
            },
        ],
        "api": true,
        "apiAddress": format!(":{API_PORT}"),
        "metrics": false,
        "playback": false,
        "rtsp": true,
        "rtspTransports": ["tcp"],
        "rtspAddress": format!(":{RTSP_PORT}"),
        "rtmp": false,
        "hls": false,
        "webrtc": true,
        "webrtcAddress": format!(":{WEBRTC_HTTP_PORT}"),
        // ホストへ同じ番号で公開するため、コンテナ内でも割り当てたポートで待ち受ける。
        "webrtcLocalUDPAddress": format!(":{}", cam.webrtc_udp_port),
        "webrtcLocalTCPAddress": "",
        // コンテナ内部のIPを候補に含めず、カメラPCから到達できるVMのIPだけを通知する。
        "webrtcIPsFromInterfaces": false,
        "webrtcAdditionalHosts": [cfg.server.public_host],
        "webrtcICEServers2": ice_servers,
        "srt": true,
        "srtAddress": format!(":{}", cam.srt_port),
        "pathDefaults": {
            // 保存先は recording.host_dir をコンテナの /recordings にマウントしたもの。
            "recordPath": format!("/recordings/{}", cfg.recording.relative_template()),
            "recordFormat": cfg.recording.segment_format,
            "recordSegmentDuration": cfg.recording.segment_duration,
            // 0 は自動削除しない（MediaMTXの 0s と同じ）。
            "recordDeleteAfter": if parse_duration_secs(&cfg.recording.retention) == Some(0) {
                "0s".to_string()
            } else {
                cfg.recording.retention.clone()
            },
        },
        "paths": paths,
    });

    let body = serde_yaml::to_string(&doc)?;
    // 表示名は含めない（名前変更でコンテナを作り直さないため）。
    Ok(format!(
        "# このファイルは AMoRe が AMoRe.yaml から自動生成する。直接編集しないこと。\n# カメラ: {}\n{body}",
        cam.id
    ))
}

/// 変換スクリプトと全カメラの `mediamtx.yml` を共有ボリュームへ書き出し、
/// 登録されていないカメラのディレクトリを削除する。
pub fn write_shared_files(cfg: &Config) -> Result<()> {
    let shared = &cfg.media.shared_dir;
    write_atomic(&shared.join("transcode.sh"), TRANSCODE_SCRIPT)?;
    write_atomic(&shared.join("convert.sh"), CONVERT_SCRIPT)?;
    for cam in &cfg.cameras {
        let text = render_camera_config(cfg, cam)?;
        write_atomic(&cfg.camera_shared_dir(&cam.id).join("mediamtx.yml"), &text)?;
    }
    let cameras_dir = shared.join("cameras");
    if let Ok(entries) = std::fs::read_dir(&cameras_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if cfg.camera(&name).is_none() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(())
}

/// 読み込み途中のファイルを読まれないよう、一時ファイルから置き換える。内容が同じなら書かない。
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("ディレクトリを作成できません: {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("書き込めません: {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("置き換えできません: {}", path.display()))?;
    Ok(())
}

/// MediaMTX APIのパス情報（必要な項目のみ）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathInfo {
    pub name: String,
    #[serde(default)]
    pub available: Option<bool>,
    /// v1.21で非推奨。古い版との互換用。
    #[serde(default)]
    pub ready: Option<bool>,
    #[serde(default)]
    pub tracks2: Option<Vec<Track>>,
    /// v1.21で非推奨。古い版との互換用。
    #[serde(default)]
    pub tracks: Option<Vec<String>>,
    #[serde(default)]
    pub inbound_bytes: Option<u64>,
    #[serde(default)]
    pub bytes_received: Option<u64>,
    #[serde(default)]
    pub readers: Vec<Reader>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub codec: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reader {
    #[serde(rename = "type")]
    pub kind: String,
}

impl PathInfo {
    pub fn is_available(&self) -> bool {
        self.available.or(self.ready).unwrap_or(false)
    }

    pub fn codecs(&self) -> Vec<String> {
        match (&self.tracks2, &self.tracks) {
            (Some(t), _) => t.iter().map(|t| t.codec.clone()).collect(),
            (None, Some(t)) => t.clone(),
            (None, None) => Vec::new(),
        }
    }

    pub fn inbound_bytes(&self) -> u64 {
        self.inbound_bytes.or(self.bytes_received).unwrap_or(0)
    }

    pub fn srt_readers(&self) -> usize {
        self.readers.iter().filter(|r| r.kind == "srtConn").count()
    }

    pub fn webrtc_readers(&self) -> usize {
        self.readers.iter().filter(|r| r.kind == "webRTCSession").count()
    }
}

#[derive(Debug, Deserialize)]
struct PathList {
    #[serde(default)]
    items: Vec<PathInfo>,
}

/// 存在するパスの一覧。設定済みでも送信元がないパスは含まれない場合がある。
pub async fn list_paths(http: &reqwest::Client, api_base: &str) -> Result<Vec<PathInfo>> {
    let url = format!("{api_base}/v3/paths/list?itemsPerPage=1000");
    let list: PathList = http
        .get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(list.items)
}

/// 指定パスが受信中か。パスが存在しない（404）場合は受信していないとみなす。
pub async fn is_available(http: &reqwest::Client, api_base: &str, path: &str) -> Result<bool> {
    let url = format!("{api_base}/v3/paths/get/{path}");
    let res = http
        .get(url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    let info: PathInfo = res.error_for_status()?.json().await?;
    Ok(info.is_available())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    fn rendered(index: usize) -> serde_yaml::Value {
        let cfg = sample();
        let text = render_camera_config(&cfg, &cfg.cameras[index]).unwrap();
        serde_yaml::from_str(&text).unwrap()
    }

    #[test]
    fn renders_paths_for_one_camera() {
        let doc = rendered(1);
        let paths = doc["paths"].as_mapping().unwrap();
        assert_eq!(paths.len(), 2);
        let ingest = &doc["paths"]["cam2_in"];
        assert_eq!(ingest["source"].as_str(), Some("publisher"));
        assert_eq!(
            ingest["runOnAvailable"].as_str(),
            Some("sh /shared/transcode.sh cam2_in cam2 6000 8000 30 48000 2 128 ultrafast")
        );
        assert_eq!(ingest["runOnAvailableRestart"].as_bool(), Some(true));
        assert_eq!(doc["paths"]["cam2"]["record"].as_bool(), Some(false));
        assert!(doc["paths"]["cam2"]["runOnRecordSegmentComplete"].is_null());
    }

    #[test]
    fn renders_recording_for_enabled_camera() {
        let mut cfg = sample();
        cfg.cameras[0].record = Some(true);
        cfg.recording.retention = "2d".into();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&render_camera_config(&cfg, &cfg.cameras[0]).unwrap()).unwrap();
        let out = &doc["paths"]["cam1"];
        assert_eq!(out["record"].as_bool(), Some(true));
        assert_eq!(
            out["runOnRecordSegmentComplete"].as_str(),
            Some("sh /shared/convert.sh $MTX_SEGMENT_PATH mp4 0 2880")
        );
        assert_eq!(
            doc["pathDefaults"]["recordPath"].as_str(),
            Some("/recordings/%path/%Y-%m-%d_%H-%M-%S-%f")
        );
        assert_eq!(doc["pathDefaults"]["recordFormat"].as_str(), Some("fmp4"));
        assert_eq!(doc["pathDefaults"]["recordDeleteAfter"].as_str(), Some("2d"));
        // 他のカメラは録画しない。
        assert!(!cfg.camera_records(&cfg.cameras[1]));
    }

    #[test]
    fn skips_ice_servers_with_unresolved_secrets() {
        let mut cfg = sample();
        cfg.ice_servers = vec![
            crate::config::IceServer {
                urls: vec!["turn:turn.example.jp:3478".into()],
                username: Some("amore".into()),
                credential: Some("op://AMoRe/TURN/password".into()),
            },
            crate::config::IceServer {
                urls: vec!["stun:stun.example.jp:3478".into()],
                username: None,
                credential: None,
            },
        ];
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&render_camera_config(&cfg, &cfg.cameras[0]).unwrap()).unwrap();
        let ice = doc["webrtcICEServers2"].as_sequence().unwrap();
        assert_eq!(ice.len(), 1);
        assert_eq!(ice[0]["url"].as_str(), Some("stun:stun.example.jp:3478"));
    }

    #[test]
    fn renders_camera_ports() {
        let doc = rendered(1);
        assert_eq!(doc["webrtcLocalUDPAddress"].as_str(), Some(":8190"));
        assert_eq!(doc["srtAddress"].as_str(), Some(":8891"));
        assert_eq!(doc["webrtcAdditionalHosts"][0].as_str(), Some("10.40.0.20"));
        assert_eq!(doc["webrtcIPsFromInterfaces"].as_bool(), Some(false));
        assert_eq!(doc["apiAddress"].as_str(), Some(":9997"));
        assert_eq!(doc["udpReadBufferSize"].as_u64(), Some(8 * 1024 * 1024));
        assert_eq!(doc["rtmp"].as_bool(), Some(false));
        assert_eq!(doc["hls"].as_bool(), Some(false));
    }

    #[test]
    fn obs_can_only_read_output_path() {
        let doc = rendered(0);
        let users = doc["authInternalUsers"].as_sequence().unwrap();
        let anyone = users
            .iter()
            .find(|u| u["ips"].as_sequence().unwrap().is_empty())
            .unwrap();
        let perms = anyone["permissions"].as_sequence().unwrap();
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0]["action"].as_str(), Some("read"));
        assert_eq!(perms[0]["path"].as_str(), Some("cam1"));
    }

    #[test]
    fn config_path_is_in_shared_volume() {
        let cfg = sample();
        assert_eq!(config_path(&cfg, "cam1"), "/shared/cameras/cam1/mediamtx.yml");
        assert_eq!(api_base(&cfg, "cam1"), "http://amore-cam-cam1:9997");
        assert_eq!(webrtc_base(&cfg, "cam2"), "http://amore-cam-cam2:8889");
    }

    #[test]
    fn path_info_prefers_new_fields() {
        let info: PathInfo = serde_json::from_str(
            r#"{"name":"cam1_in","available":true,"ready":false,
                "tracks2":[{"codec":"H264"},{"codec":"Opus"}],"tracks":["X"],
                "inboundBytes":10,"bytesReceived":5,
                "readers":[{"type":"srtConn","id":"a"},{"type":"webRTCSession","id":"b"}]}"#,
        )
        .unwrap();
        assert!(info.is_available());
        assert_eq!(info.codecs(), vec!["H264", "Opus"]);
        assert_eq!(info.inbound_bytes(), 10);
        assert_eq!(info.srt_readers(), 1);
        assert_eq!(info.webrtc_readers(), 1);
    }

    #[test]
    fn path_info_falls_back_to_old_fields() {
        let info: PathInfo = serde_json::from_str(
            r#"{"name":"cam1","ready":true,"tracks":["H264","MPEG-4 Audio"],"bytesReceived":5}"#,
        )
        .unwrap();
        assert!(info.is_available());
        assert_eq!(info.codecs(), vec!["H264", "MPEG-4 Audio"]);
        assert_eq!(info.inbound_bytes(), 5);
    }
}
