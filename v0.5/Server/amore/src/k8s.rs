//! Kubernetes での実行（`media.runtime: kubernetes`）。
//!
//! AMoRe の Pod がコントローラーとして k8s API を呼び、カメラごとに次のリソースを作る（1カメラ＝1Pod）。
//! - ConfigMap `<名前>`：`mediamtx.yml`・`transcode.sh`・`convert.sh`（`media.shared_dir` にマウント。Docker の共有ボリュームと同じ配置）
//! - Deployment `<名前>`：MediaMTX＋FFmpeg（replicas 1、Recreate。テンプレートの注釈に設定ハッシュ）
//! - Service `<名前>`（ClusterIP）：AMoRe からの API・WHIP/WHEP 中継・RTSP（Docker のコンテナ名と同じ名前で接続できる）
//! - Service `<名前>-site`（LoadBalancer・MetalLB）：拠点の接続先IP（`server.public_host`）で WebRTC・SRT を公開
//! - 冗長構成では Service `amore-vip-<id>`（カメラ用の共通IP・WebRTC）・`amore-obs-<id>`（OBS 用の共通IP・SRT）：
//!   全拠点で共用。`externalIPs` ＋ `externalTrafficPolicy: Local` により、keepalived が共通IPを載せたノード上の Pod
//!   （＝受け持ち拠点の一式。カメラの Pod は同じ拠点の AMoRe と同じノードに配置する）だけへ届く。
//!
//! 外部クレートは使わず reqwest で REST を呼ぶ（in-cluster の ServiceAccount）。作成・更新は server-side apply。

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{Camera, Config};
use crate::docker::{ContainerInfo, ReconcileReport};
use crate::keepalived;
use crate::mediamtx;

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const FIELD_MANAGER: &str = "amore";

pub const LABEL_PART_OF: &str = "app.kubernetes.io/part-of";
pub const LABEL_NAME: &str = "app.kubernetes.io/name";
pub const LABEL_MANAGED: &str = "amore.managed";
pub const LABEL_CAMERA: &str = "amore.camera";
pub const LABEL_SITE: &str = "amore.site";
pub const LABEL_ROLE: &str = "amore.role";
pub const LABEL_WORKLOAD: &str = "amore.workload";
pub const LABEL_VIP: &str = "amore.vip-service";
pub const ANNOTATION_HASH: &str = "amore.config-hash";
/// MetalLB に扱わせない（共通IPは keepalived が載せる）Service の LoadBalancer クラス。
pub const KEEPALIVED_LB_CLASS: &str = "amore.io/keepalived";
/// カメラの Pod のコンテナ名。
const CONTAINER: &str = "mediamtx";

// ---------------------------------------------------------------------------
// API クライアント
// ---------------------------------------------------------------------------

struct Client {
    http: reqwest::Client,
    base: String,
    namespace: String,
}

static CLIENT: OnceLock<Client> = OnceLock::new();

fn client(cfg: &Config) -> Result<&'static Client> {
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let ca = std::fs::read(format!("{SA_DIR}/ca.crt")).with_context(|| {
        format!("{SA_DIR}/ca.crt を読めません（AMoRe が Kubernetes の Pod として動いているか、ServiceAccount を確認してください）")
    })?;
    let http = reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(
            reqwest::Certificate::from_pem(&ca).context("クラスターの CA を解析できません")?,
        )
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()?;
    let base = match (
        std::env::var("KUBERNETES_SERVICE_HOST"),
        std::env::var("KUBERNETES_SERVICE_PORT_HTTPS")
            .or_else(|_| std::env::var("KUBERNETES_SERVICE_PORT")),
    ) {
        // API サーバーの証明書は Service の IP を SAN に含む。
        (Ok(host), Ok(port)) if host.contains(':') => format!("https://[{host}]:{port}"),
        (Ok(host), Ok(port)) => format!("https://{host}:{port}"),
        _ => "https://kubernetes.default.svc".to_string(),
    };
    let namespace = if cfg.kubernetes.namespace.is_empty() {
        std::fs::read_to_string(format!("{SA_DIR}/namespace"))
            .context("Pod の namespace を読めません（kubernetes.namespace を指定してください）")?
            .trim()
            .to_string()
    } else {
        cfg.kubernetes.namespace.clone()
    };
    let _ = CLIENT.set(Client {
        http,
        base,
        namespace,
    });
    Ok(CLIENT.get().expect("設定済み"))
}

/// カメラの Service の ClusterIP（名前 → IP）。DNS（CoreDNS）が使えない間も接続できるように覚えておく。
static CLUSTER_IPS: OnceLock<std::sync::RwLock<HashMap<String, String>>> = OnceLock::new();

fn cluster_ips() -> &'static std::sync::RwLock<HashMap<String, String>> {
    CLUSTER_IPS.get_or_init(Default::default)
}

fn remember_cluster_ip(service: &Value) {
    let (Some(name), Some(ip)) = (
        service["metadata"]["name"].as_str(),
        service["spec"]["clusterIP"].as_str(),
    ) else {
        return;
    };
    if service["spec"]["type"] == "ClusterIP" && ip != "None" && !ip.is_empty() {
        cluster_ips()
            .write()
            .expect("cluster ip lock")
            .insert(name.to_string(), ip.to_string());
    }
}

/// カメラの Service の ClusterIP（Kubernetes で、かつ一度でも確認できた場合だけ）。
pub fn cluster_ip(name: &str) -> Option<String> {
    CLUSTER_IPS.get()?.read().ok()?.get(name).cloned()
}

/// ServiceAccount のトークンは期限付きで kubelet が更新するため、毎回読む。
fn token() -> Result<String> {
    Ok(std::fs::read_to_string(format!("{SA_DIR}/token"))
        .context("ServiceAccount のトークンを読めません")?
        .trim()
        .to_string())
}

#[derive(Clone, Copy)]
enum Kind {
    ConfigMap,
    Service,
    Pod,
    Deployment,
}

impl Kind {
    fn collection(self, ns: &str) -> String {
        match self {
            Kind::ConfigMap => format!("/api/v1/namespaces/{ns}/configmaps"),
            Kind::Service => format!("/api/v1/namespaces/{ns}/services"),
            Kind::Pod => format!("/api/v1/namespaces/{ns}/pods"),
            Kind::Deployment => format!("/apis/apps/v1/namespaces/{ns}/deployments"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::ConfigMap => "ConfigMap",
            Kind::Service => "Service",
            Kind::Pod => "Pod",
            Kind::Deployment => "Deployment",
        }
    }
}

impl Client {
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
        let res = req
            .bearer_auth(token()?)
            .send()
            .await
            .with_context(|| format!("Kubernetes API に接続できません（{what}）"))?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let message = body["message"].as_str().unwrap_or("").to_string();
        if status == reqwest::StatusCode::FORBIDDEN {
            bail!("{what}: 権限がありません（Container/k8s の Role・RoleBinding を確認してください）: {message}");
        }
        bail!("{what}: HTTP {status} {message}");
    }

    /// server-side apply（無ければ作成、あれば更新）。Service の場合は ClusterIP を覚える。
    async fn apply(&self, kind: Kind, manifest: &Value) -> Result<()> {
        let name = manifest["metadata"]["name"]
            .as_str()
            .context("manifest に名前がありません")?;
        let url = format!(
            "{}{}/{name}?fieldManager={FIELD_MANAGER}&force=true",
            self.base,
            kind.collection(&self.namespace)
        );
        let req = self
            .http
            .patch(url)
            .header("Content-Type", "application/apply-patch+yaml")
            .body(serde_json::to_vec(manifest)?);
        let res = self
            .send(req, &format!("{} {name} の適用", kind.label()))
            .await?;
        if matches!(kind, Kind::Service) {
            if let Ok(applied) = res.json::<Value>().await {
                remember_cluster_ip(&applied);
            }
        }
        Ok(())
    }

    /// 削除する。無ければ何もしない。
    async fn delete(&self, kind: Kind, name: &str) -> Result<()> {
        let url = format!("{}{}/{name}", self.base, kind.collection(&self.namespace));
        let res = self
            .http
            .delete(url)
            .bearer_auth(token()?)
            .json(&json!({ "propagationPolicy": "Background" }))
            .send()
            .await
            .context("Kubernetes API に接続できません")?;
        if res.status().is_success() || res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        bail!(
            "{} {name} を削除できません: {}",
            kind.label(),
            body["message"].as_str().unwrap_or("")
        )
    }

    async fn list(&self, kind: Kind, selector: &str) -> Result<Vec<Value>> {
        let url = format!("{}{}", self.base, kind.collection(&self.namespace));
        let req = self.http.get(url).query(&[("labelSelector", selector)]);
        let res = self.send(req, &format!("{} の一覧", kind.label())).await?;
        let list: Value = res.json().await?;
        Ok(list["items"].as_array().cloned().unwrap_or_default())
    }

    async fn merge_patch(&self, kind: Kind, name: &str, patch: &Value) -> Result<()> {
        let url = format!("{}{}/{name}", self.base, kind.collection(&self.namespace));
        let req = self
            .http
            .patch(url)
            .header("Content-Type", "application/merge-patch+json")
            .body(serde_json::to_vec(patch)?);
        self.send(req, &format!("{} {name} の変更", kind.label()))
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// マニフェスト（単体テストの対象）
// ---------------------------------------------------------------------------

/// カメラのリソースに付けるラベル（拠点ごと）。
pub fn camera_labels(cfg: &Config, cam: &Camera) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (LABEL_PART_OF, "amore".to_string()),
        (LABEL_NAME, "amore-camera".to_string()),
        (LABEL_MANAGED, "true".to_string()),
        (LABEL_CAMERA, cam.id.clone()),
        (LABEL_SITE, cfg.kube_site().to_string()),
        (LABEL_ROLE, "camera".to_string()),
        (LABEL_WORKLOAD, cfg.container_name(&cam.id)),
    ])
}

/// Deployment・Service が選ぶ Pod（この拠点のこのカメラ）。
fn camera_selector(cfg: &Config, cam: &Camera) -> Value {
    json!({
        LABEL_MANAGED: "true",
        LABEL_SITE: cfg.kube_site(),
        LABEL_CAMERA: cam.id,
    })
}

fn webrtc_port_name(cam: &Camera) -> String {
    format!("w{}", cam.webrtc_udp_port)
}

fn srt_port_name(cam: &Camera) -> String {
    format!("s{}", cam.srt_port)
}

pub fn camera_configmap(cfg: &Config, cam: &Camera) -> Result<Value> {
    let data: BTreeMap<&str, String> = mediamtx::camera_files(cfg, cam)?
        .into_iter()
        .map(|(key, _, text)| (key, text))
        .collect();
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": cfg.container_name(&cam.id), "labels": camera_labels(cfg, cam) },
        "data": data,
    }))
}

pub fn camera_deployment(cfg: &Config, cam: &Camera, hash: &str) -> Result<Value> {
    let name = cfg.container_name(&cam.id);
    let site = cfg.kube_site();
    let k = &cfg.kubernetes;
    let items: Vec<Value> = mediamtx::camera_files(cfg, cam)?
        .into_iter()
        .map(|(key, path, _)| json!({ "key": key, "path": path }))
        .collect();

    let mut mounts = vec![json!({
        "name": "shared",
        "mountPath": cfg.media.shared_dir.to_string_lossy(),
        "readOnly": true,
    })];
    let mut volumes = vec![json!({
        "name": "shared",
        "configMap": { "name": name, "items": items, "defaultMode": 0o555 },
    })];
    if !k.recordings_claim.is_empty() {
        mounts.push(json!({ "name": "recordings", "mountPath": "/recordings" }));
        volumes.push(json!({
            "name": "recordings",
            "persistentVolumeClaim": { "claimName": k.recordings_claim },
        }));
    }

    let r = &k.camera_resources;
    let pick = |pairs: [(&str, &String); 2]| -> serde_json::Map<String, Value> {
        pairs
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    };
    let requests = pick([("cpu", &r.cpu_request), ("memory", &r.memory_request)]);
    let limits = pick([("cpu", &r.cpu_limit), ("memory", &r.memory_limit)]);

    let mut labels = camera_labels(cfg, cam);
    labels.insert(LABEL_WORKLOAD, name.clone());
    Ok(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "labels": labels,
            "annotations": { ANNOTATION_HASH: hash },
        },
        "spec": {
            "replicas": 1,
            // UDP ポートを使うため、古い Pod を止めてから新しい Pod を作る。
            "strategy": { "type": "Recreate" },
            "selector": { "matchLabels": camera_selector(cfg, cam) },
            "template": {
                "metadata": { "labels": labels, "annotations": { ANNOTATION_HASH: hash } },
                "spec": {
                    "nodeSelector": { k.site_label.as_str(): site },
                    // 同じ拠点の AMoRe と同じノードに置く（共通IPを載せたノードで受けるため）。
                    "affinity": { "podAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": [{
                        "labelSelector": { "matchLabels": { LABEL_NAME: "amore", LABEL_SITE: site } },
                        "topologyKey": "kubernetes.io/hostname",
                    }]}},
                    "enableServiceLinks": false,
                    "terminationGracePeriodSeconds": 5,
                    // ノードが止まったら30秒で別のノードへ移す（既定の5分では長すぎる）。
                    "tolerations": node_down_tolerations(),
                    "containers": [{
                        "name": CONTAINER,
                        "image": cfg.media.image,
                        "args": [mediamtx::config_path(cfg, &cam.id)],
                        "ports": [
                            { "name": webrtc_port_name(cam), "containerPort": cam.webrtc_udp_port, "protocol": "UDP" },
                            { "name": srt_port_name(cam), "containerPort": cam.srt_port, "protocol": "UDP" },
                            { "name": "api", "containerPort": mediamtx::API_PORT, "protocol": "TCP" },
                            { "name": "webrtc-http", "containerPort": mediamtx::WEBRTC_HTTP_PORT, "protocol": "TCP" },
                            { "name": "rtsp", "containerPort": mediamtx::RTSP_PORT, "protocol": "TCP" },
                        ],
                        "volumeMounts": mounts,
                        "resources": { "requests": requests, "limits": limits },
                        "readinessProbe": {
                            "tcpSocket": { "port": mediamtx::API_PORT },
                            "periodSeconds": 5,
                        },
                    }],
                    "volumes": volumes,
                },
            },
        },
    }))
}

fn node_down_tolerations() -> Value {
    json!([
        { "key": "node.kubernetes.io/unreachable", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 30 },
        { "key": "node.kubernetes.io/not-ready", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 30 },
    ])
}

/// AMoRe からの接続用（ClusterIP。名前は Docker のコンテナ名と同じ）。
pub fn camera_service(cfg: &Config, cam: &Camera) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": cfg.container_name(&cam.id), "labels": camera_labels(cfg, cam) },
        "spec": {
            "type": "ClusterIP",
            "selector": camera_selector(cfg, cam),
            "ports": [
                { "name": "api", "port": mediamtx::API_PORT, "protocol": "TCP" },
                { "name": "webrtc-http", "port": mediamtx::WEBRTC_HTTP_PORT, "protocol": "TCP" },
                { "name": "rtsp", "port": mediamtx::RTSP_PORT, "protocol": "TCP" },
            ],
        },
    })
}

/// MetalLB の注釈（拠点の接続先IPを、この拠点の全 Service で共有する）。
fn metallb_annotations(cfg: &Config) -> Value {
    json!({
        "metallb.universe.tf/address-pool": cfg.kubernetes.metallb_pool,
        "metallb.universe.tf/loadBalancerIPs": cfg.server.public_host,
        "metallb.universe.tf/allow-shared-ip": format!("amore-{}", cfg.kube_site()),
    })
}

/// 拠点の接続先IP（MetalLB）で WebRTC・SRT を公開する。
pub fn camera_site_service(cfg: &Config, cam: &Camera) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": format!("{}-site", cfg.container_name(&cam.id)),
            "labels": camera_labels(cfg, cam),
            "annotations": metallb_annotations(cfg),
        },
        "spec": {
            "type": "LoadBalancer",
            "allocateLoadBalancerNodePorts": false,
            // 同じ IP を複数の Service で共有するため Cluster にする（MetalLB の制約）。
            "externalTrafficPolicy": "Cluster",
            "selector": camera_selector(cfg, cam),
            "ports": [
                { "name": webrtc_port_name(cam), "port": cam.webrtc_udp_port, "targetPort": cam.webrtc_udp_port, "protocol": "UDP" },
                { "name": srt_port_name(cam), "port": cam.srt_port, "targetPort": cam.srt_port, "protocol": "UDP" },
            ],
        },
    })
}

/// 共通IP（keepalived が載せる）で受ける Service。全拠点で共用し、ノード上の Pod だけへ届ける。
fn vip_service(name: &str, labels: Value, ip: &str, selector: Value, ports: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": name, "labels": labels },
        "spec": {
            "type": "LoadBalancer",
            // MetalLB に扱わせない（共通IPは keepalived が載せる）。
            "loadBalancerClass": KEEPALIVED_LB_CLASS,
            "allocateLoadBalancerNodePorts": false,
            "externalIPs": [ip],
            // 共通IPを持つノード上の Pod（＝受け持ち拠点の一式）だけへ届ける。
            "externalTrafficPolicy": "Local",
            "selector": selector,
            "ports": ports,
        },
    })
}

/// カメラの共通IP用の Service（冗長構成のみ。全拠点の AMoRe が同じ内容を適用する）。
pub fn camera_vip_services(cfg: &Config, cam: &Camera) -> Vec<Value> {
    if !cfg.cluster.enabled {
        return Vec::new();
    }
    let labels = json!({
        LABEL_PART_OF: "amore",
        LABEL_MANAGED: "true",
        LABEL_VIP: "true",
        LABEL_CAMERA: cam.id,
    });
    // 拠点を問わずこのカメラの Pod を選ぶ（届くのはノード上の Pod だけ）。
    let selector = json!({ LABEL_MANAGED: "true", LABEL_ROLE: "camera", LABEL_CAMERA: cam.id });
    vec![
        vip_service(
            &format!("amore-vip-{}", cam.id),
            labels.clone(),
            &cfg.camera_host(),
            selector.clone(),
            json!([{ "name": webrtc_port_name(cam), "port": cam.webrtc_udp_port, "targetPort": cam.webrtc_udp_port, "protocol": "UDP" }]),
        ),
        vip_service(
            &format!("amore-obs-{}", cam.id),
            labels,
            &cfg.obs_host(),
            selector,
            json!([{ "name": srt_port_name(cam), "port": cam.srt_port, "targetPort": cam.srt_port, "protocol": "UDP" }]),
        ),
    ]
}

/// AMoRe 自身の HTTPS（拠点の接続先IP、冗長構成では共通IPも）。
pub fn amore_services(cfg: &Config) -> Vec<Value> {
    let site = cfg.kube_site();
    let https = cfg.server.listen.port();
    let ports = json!([{ "name": "https", "port": 443, "targetPort": https, "protocol": "TCP" }]);
    let mut out = vec![json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": format!("amore-{site}-https"),
            "labels": { LABEL_PART_OF: "amore", LABEL_SITE: site },
            "annotations": metallb_annotations(cfg),
        },
        "spec": {
            "type": "LoadBalancer",
            "allocateLoadBalancerNodePorts": false,
            "externalTrafficPolicy": "Cluster",
            "selector": { LABEL_NAME: "amore", LABEL_SITE: site },
            "ports": ports,
        },
    })];
    if cfg.cluster.enabled {
        let labels = json!({ LABEL_PART_OF: "amore", LABEL_VIP: "true" });
        let selector = json!({ LABEL_NAME: "amore" });
        out.push(vip_service(
            "amore-vip-https",
            labels.clone(),
            &cfg.camera_host(),
            selector.clone(),
            ports.clone(),
        ));
        out.push(vip_service(
            "amore-obs-https",
            labels,
            &cfg.obs_host(),
            selector,
            ports,
        ));
    }
    out
}

/// keepalived のノードごとの設定（DaemonSet が `$NODE_NAME.conf` を読む）。
pub fn keepalived_configmap(cfg: &Config) -> Result<Value> {
    let mut data = BTreeMap::new();
    for node in &cfg.kubernetes.nodes {
        data.insert(
            format!("{}.conf", node.name),
            keepalived::render_for_node(cfg, node)?,
        );
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": "amore-keepalived", "labels": { LABEL_PART_OF: "amore" } },
        "data": data,
    }))
}

/// `cameras.yaml` を保存する ConfigMap（AMoRe の Pod の起動時に `/config` へコピーされる）。
pub fn cameras_configmap(cfg: &Config, text: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": format!("amore-{}-cameras", cfg.kube_site()),
            "labels": { LABEL_PART_OF: "amore", LABEL_SITE: cfg.kube_site() },
        },
        "data": { "cameras.yaml": text },
    })
}

/// カメラのリソースの設定を表すハッシュ（Deployment の注釈。変わったら更新する）。
pub fn spec_hash(cfg: &Config, cam: &Camera) -> Result<String> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let parts = [
        serde_json::to_vec(&camera_configmap(cfg, cam)?)?,
        serde_json::to_vec(&camera_deployment(cfg, cam, "")?)?,
        serde_json::to_vec(&camera_service(cfg, cam))?,
        serde_json::to_vec(&camera_site_service(cfg, cam))?,
        serde_json::to_vec(&camera_vip_services(cfg, cam))?,
    ];
    for byte in parts.iter().flatten() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{hash:016x}"))
}

// ---------------------------------------------------------------------------
// Pod の状態 → ContainerInfo（health.rs の判定をそのまま使う）
// ---------------------------------------------------------------------------

/// Pod の状態を Docker の `ContainerInfo` の形に写す。
pub fn pod_info(cfg: &Config, pod: &Value) -> ContainerInfo {
    let labels = &pod["metadata"]["labels"];
    let camera = labels[LABEL_CAMERA].as_str().unwrap_or("").to_string();
    let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
    let node = pod["spec"]["nodeName"].as_str().unwrap_or("");
    let status = &pod["status"];
    let cs = status["containerStatuses"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == CONTAINER))
        .cloned()
        .unwrap_or(Value::Null);
    let restart_count = cs["restartCount"].as_u64().unwrap_or(0);
    let state = &cs["state"];
    let last = &cs["lastState"]["terminated"];
    let terminated = if state["terminated"].is_object() {
        &state["terminated"]
    } else {
        last
    };
    let waiting_reason = state["waiting"]["reason"].as_str().unwrap_or("");
    let waiting_message = state["waiting"]["message"].as_str().unwrap_or("");

    let deleting = pod["metadata"]["deletionTimestamp"].is_string();
    let kind = if deleting {
        "exited"
    } else if state["running"].is_object() {
        "running"
    } else if waiting_reason == "CrashLoopBackOff" {
        "restarting"
    } else if state["terminated"].is_object() {
        "exited"
    } else {
        // Pending（スケジュール待ち・イメージ取得中・コンテナ作成中など）
        "created"
    };

    let mut error = String::new();
    if !waiting_reason.is_empty()
        && waiting_reason != "ContainerCreating"
        && waiting_reason != "CrashLoopBackOff"
    {
        error = format!("{waiting_reason}: {waiting_message}");
    }
    if error.is_empty() {
        // スケジュールできない（ノード・リソース不足・podAffinity）理由。
        if let Some(cond) = status["conditions"].as_array().and_then(|c| {
            c.iter()
                .find(|c| c["type"] == "PodScheduled" && c["status"] == "False")
        }) {
            error = format!(
                "配置できません（{}）: {}",
                cond["reason"].as_str().unwrap_or(""),
                cond["message"].as_str().unwrap_or("")
            );
        }
    }
    let started_at = state["running"]["startedAt"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let started = started_at.get(..19).unwrap_or("").replace('T', " ");
    let display = format!(
        "Pod {pod_name}{} / {} / 再起動 {restart_count}回",
        if node.is_empty() {
            String::new()
        } else {
            format!("（ノード {node}）")
        },
        if started.is_empty() {
            "未起動".to_string()
        } else {
            format!("起動 {started}（UTC）")
        },
    );
    ContainerInfo {
        name: labels[LABEL_WORKLOAD].as_str().unwrap_or("").to_string(),
        camera,
        state: kind.to_string(),
        status: display,
        restart_count,
        exit_code: terminated["exitCode"].as_i64().unwrap_or(0),
        oom_killed: terminated["reason"] == "OOMKilled",
        error,
        // k8s では Service 経由で接続するため、ネットワークの確認は常に通る扱いにする。
        networks: vec![cfg.media.docker_network.clone()],
        started_at,
        config_hash: pod["metadata"]["annotations"][ANNOTATION_HASH]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

/// 自動復旧をあきらめて replicas 0 にした Deployment（Pod が無い）の表示。
fn held_info(cfg: &Config, deployment: &Value) -> ContainerInfo {
    let labels = &deployment["metadata"]["labels"];
    ContainerInfo {
        name: deployment["metadata"]["name"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        camera: labels[LABEL_CAMERA].as_str().unwrap_or("").to_string(),
        state: "exited".to_string(),
        status: "停止中（replicas 0。自動復旧を中止したか、手動で止めています）".to_string(),
        restart_count: 0,
        exit_code: 0,
        oom_killed: false,
        error: String::new(),
        networks: vec![cfg.media.docker_network.clone()],
        started_at: String::new(),
        config_hash: deployment["metadata"]["annotations"][ANNOTATION_HASH]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

fn site_selector(cfg: &Config) -> String {
    format!("{LABEL_MANAGED}=true,{LABEL_SITE}={}", cfg.kube_site())
}

// ---------------------------------------------------------------------------
// 操作（runtime.rs から呼ぶ）
// ---------------------------------------------------------------------------

/// API サーバーの版（管理Webの表示用）。
pub async fn ping(cfg: &Config) -> Result<String> {
    let c = client(cfg)?;
    let res = c
        .send(c.http.get(format!("{}/version", c.base)), "版の取得")
        .await?;
    let v: Value = res.json().await?;
    Ok(format!("k8s {}", v["gitVersion"].as_str().unwrap_or("?")))
}

/// この拠点のカメラの Pod の状態。Pod の無い Deployment（replicas 0）も含める。
pub async fn list_managed(cfg: &Config) -> Result<Vec<ContainerInfo>> {
    let c = client(cfg)?;
    let selector = site_selector(cfg);
    let pods = c
        .list(Kind::Pod, &format!("{selector},{LABEL_ROLE}=camera"))
        .await?;
    for svc in c.list(Kind::Service, &selector).await.unwrap_or_default() {
        remember_cluster_ip(&svc);
    }
    let mut by_name: HashMap<String, ContainerInfo> = HashMap::new();
    for pod in &pods {
        let info = pod_info(cfg, pod);
        // 作り直し中は新旧の Pod が並ぶため、終了処理中でない方を優先する。
        let replace = match by_name.get(&info.name) {
            None => true,
            Some(prev) => prev.state == "exited" && info.state != "exited",
        };
        if replace {
            by_name.insert(info.name.clone(), info);
        }
    }
    for d in c.list(Kind::Deployment, &selector).await? {
        let name = d["metadata"]["name"].as_str().unwrap_or("").to_string();
        if !by_name.contains_key(&name) {
            let replicas = d["spec"]["replicas"].as_u64().unwrap_or(1);
            let info = if replicas == 0 {
                held_info(cfg, &d)
            } else {
                ContainerInfo {
                    state: "created".to_string(),
                    status: "Pod を作成しています".to_string(),
                    ..held_info(cfg, &d)
                }
            };
            by_name.insert(name, info);
        }
    }
    let mut out: Vec<ContainerInfo> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// カメラのリソースをすべて適用する（Deployment は replicas 1 に戻る）。
pub async fn apply_camera(cfg: &Config, cam: &Camera) -> Result<()> {
    let c = client(cfg)?;
    let hash = spec_hash(cfg, cam)?;
    c.apply(Kind::ConfigMap, &camera_configmap(cfg, cam)?)
        .await?;
    c.apply(Kind::Service, &camera_service(cfg, cam)).await?;
    c.apply(Kind::Service, &camera_site_service(cfg, cam))
        .await?;
    for svc in camera_vip_services(cfg, cam) {
        c.apply(Kind::Service, &svc).await?;
    }
    c.apply(Kind::Deployment, &camera_deployment(cfg, cam, &hash)?)
        .await?;
    Ok(())
}

/// 作り直す：リソースを適用し、今の Pod を消す（Deployment が新しい Pod を作る）。
pub async fn recreate(cfg: &Config, cam: &Camera) -> Result<()> {
    apply_camera(cfg, cam).await?;
    restart(cfg, &cfg.container_name(&cam.id)).await
}

/// Pod を消して作り直させる（コンテナの再起動に相当）。
pub async fn restart(cfg: &Config, name: &str) -> Result<()> {
    let c = client(cfg)?;
    for pod in c
        .list(Kind::Pod, &format!("{LABEL_WORKLOAD}={name}"))
        .await?
    {
        if let Some(pod_name) = pod["metadata"]["name"].as_str() {
            c.delete(Kind::Pod, pod_name).await?;
        }
    }
    Ok(())
}

/// この拠点のカメラのリソースを消す（共通IP用の Service は reconcile が登録外のものを消す）。
pub async fn remove(cfg: &Config, name: &str) -> Result<()> {
    let c = client(cfg)?;
    c.delete(Kind::Deployment, name).await?;
    c.delete(Kind::Service, name).await?;
    c.delete(Kind::Service, &format!("{name}-site")).await?;
    c.delete(Kind::ConfigMap, name).await?;
    Ok(())
}

/// 自動復旧をあきらめたカメラを止める（replicas 0。無限に作り直させない）。
pub async fn stop_and_hold(cfg: &Config, name: &str) -> Result<()> {
    client(cfg)?
        .merge_patch(
            Kind::Deployment,
            name,
            &json!({ "spec": { "replicas": 0 } }),
        )
        .await
}

/// カメラの Pod のログ（直近）。再起動を繰り返している場合は前回のコンテナのログも付ける。
pub async fn logs(cfg: &Config, name: &str, tail: u32) -> Result<String> {
    let c = client(cfg)?;
    let pods = c
        .list(Kind::Pod, &format!("{LABEL_WORKLOAD}={name}"))
        .await?;
    let Some(pod) = pods.iter().max_by_key(|p| {
        p["metadata"]["creationTimestamp"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }) else {
        return Ok("（Pod がありません）".to_string());
    };
    let pod_name = pod["metadata"]["name"].as_str().unwrap_or("");
    let restarts = pod_info(cfg, pod).restart_count;
    let mut out = String::new();
    let url = format!(
        "{}{}/{pod_name}/log",
        c.base,
        Kind::Pod.collection(&c.namespace)
    );
    if restarts > 0 {
        let req = c.http.get(&url).query(&[
            ("container", CONTAINER.to_string()),
            ("tailLines", (tail / 2).max(1).to_string()),
            ("timestamps", "true".to_string()),
            ("previous", "true".to_string()),
        ]);
        if let Ok(res) = c.send(req, "前回のログの取得").await {
            out.push_str(&format!(
                "----- 前回のコンテナ（再起動 {restarts}回） -----\n"
            ));
            out.push_str(&res.text().await.unwrap_or_default());
            out.push_str("----- 現在のコンテナ -----\n");
        }
    }
    let req = c.http.get(&url).query(&[
        ("container", CONTAINER.to_string()),
        ("tailLines", tail.to_string()),
        ("timestamps", "true".to_string()),
    ]);
    match c.send(req, "ログの取得").await {
        Ok(res) => out.push_str(&res.text().await.unwrap_or_default()),
        Err(e) if !out.is_empty() => out.push_str(&format!("（取得できません: {e:#}）\n")),
        Err(e) => return Err(e),
    }
    Ok(out)
}

/// この拠点のカメラ一覧にリソースをそろえる（不足を作成、設定が変わったものを更新、登録外を削除）。
pub async fn reconcile(cfg: &Config) -> Result<ReconcileReport> {
    let c = client(cfg)?;
    let mut report = ReconcileReport::default();

    for svc in amore_services(cfg) {
        if let Err(e) = c.apply(Kind::Service, &svc).await {
            report.errors.push(format!("{e:#}"));
        }
    }

    let selector = site_selector(cfg);
    let deployments: HashMap<String, Value> = c
        .list(Kind::Deployment, &selector)
        .await?
        .into_iter()
        .filter_map(|d| Some((d["metadata"]["name"].as_str()?.to_string(), d)))
        .collect();

    for cam in &cfg.cameras {
        let name = cfg.container_name(&cam.id);
        let result = async {
            let hash = spec_hash(cfg, cam)?;
            match deployments.get(&name) {
                None => {
                    apply_camera(cfg, cam).await?;
                    report.created.push(name.clone());
                }
                Some(d) if d["metadata"]["annotations"][ANNOTATION_HASH] != hash.as_str() => {
                    apply_camera(cfg, cam).await?;
                    report.recreated.push(name.clone());
                }
                Some(d) if d["spec"]["replicas"].as_u64() == Some(0) => {
                    apply_camera(cfg, cam).await?;
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

    // 登録から外れたカメラのリソースを消す（この拠点の分）。
    for (name, d) in &deployments {
        if !cfg
            .cameras
            .iter()
            .any(|cam| cfg.container_name(&cam.id) == *name)
        {
            let camera = d["metadata"]["labels"][LABEL_CAMERA].as_str().unwrap_or("");
            match remove(cfg, name).await {
                Ok(()) => report.removed.push(format!("{name}（{camera}）")),
                Err(e) => report.errors.push(format!("{name}: {e:#}")),
            }
        }
    }
    // Deployment が無いまま残った ConfigMap・Service（途中で失敗した場合）も消す。
    for kind in [Kind::Service, Kind::ConfigMap] {
        for item in c.list(kind, &selector).await.unwrap_or_default() {
            let camera = item["metadata"]["labels"][LABEL_CAMERA]
                .as_str()
                .unwrap_or("");
            let name = item["metadata"]["name"].as_str().unwrap_or("");
            if !camera.is_empty() && cfg.camera(camera).is_none() {
                let _ = c.delete(kind, name).await;
            }
        }
    }
    // 共通IP用の Service（全拠点で共用）は、どの拠点の一覧にも無いカメラの分を消す。
    for svc in c
        .list(Kind::Service, &format!("{LABEL_VIP}=true"))
        .await
        .unwrap_or_default()
    {
        let camera = svc["metadata"]["labels"][LABEL_CAMERA]
            .as_str()
            .unwrap_or("");
        let name = svc["metadata"]["name"].as_str().unwrap_or("");
        let unused = if camera.is_empty() {
            // AMoRe の共通IP用（冗長構成をやめた場合）
            !cfg.cluster.enabled
        } else {
            !cfg.cluster.enabled || cfg.camera(camera).is_none()
        };
        if unused {
            let _ = c.delete(Kind::Service, name).await;
        }
    }

    tracing::info!("カメラの Pod を確認しました: {}", report.summary());
    Ok(report)
}

/// keepalived のノードごとの設定を適用する（全拠点の AMoRe が同じ内容を適用する）。
pub async fn apply_keepalived(cfg: &Config) -> Result<()> {
    client(cfg)?
        .apply(Kind::ConfigMap, &keepalived_configmap(cfg)?)
        .await
}

/// `cameras.yaml` を ConfigMap に保存する（Pod を作り直しても残す）。
pub async fn save_cameras(cfg: &Config, text: &str) -> Result<()> {
    client(cfg)?
        .apply(Kind::ConfigMap, &cameras_configmap(cfg, text))
        .await
}

/// 静止画（JPEG）を、AMoRe の Pod 内の ffmpeg でカメラの RTSP（OBS 向けの出力）から1枚取り出す。
pub async fn snapshot_jpeg(name: &str, path: &str, width: u32) -> Result<Vec<u8>> {
    let host = cluster_ip(name).unwrap_or_else(|| name.to_string());
    let url = format!("rtsp://{host}:{}/{path}", mediamtx::RTSP_PORT);
    let scale = format!("scale={width}:-2");
    let fut = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-rtsp_transport",
            "tcp",
            "-i",
            url.as_str(),
            "-frames:v",
            "1",
            "-vf",
            scale.as_str(),
            "-q:v",
            "5",
            "-f",
            "image2",
            "-c:v",
            "mjpeg",
            "pipe:1",
        ])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .context("静止画の取得がタイムアウトしました")?
        .context("ffmpeg を実行できません（Container/Dockerfile のイメージを使っているか確認してください）")?;
    if !out.status.success() || out.stdout.len() < 100 {
        bail!(
            "静止画を取得できません: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::kube_sample;
    use crate::health;

    #[test]
    fn camera_manifests_follow_config() {
        let cfg = kube_sample();
        let cam = &cfg.cameras[0];
        let d = camera_deployment(&cfg, cam, "abc").unwrap();
        assert_eq!(d["metadata"]["name"], "amore-cam-hq-cam1");
        assert_eq!(d["spec"]["strategy"]["type"], "Recreate");
        let pod = &d["spec"]["template"];
        assert_eq!(pod["metadata"]["annotations"][ANNOTATION_HASH], "abc");
        assert_eq!(pod["metadata"]["labels"][LABEL_SITE], "hq");
        assert_eq!(pod["spec"]["nodeSelector"]["amore.site"], "hq");
        let affinity = &pod["spec"]["affinity"]["podAffinity"]
            ["requiredDuringSchedulingIgnoredDuringExecution"][0];
        assert_eq!(
            affinity["labelSelector"]["matchLabels"][LABEL_NAME],
            "amore"
        );
        assert_eq!(affinity["topologyKey"], "kubernetes.io/hostname");
        let c = &pod["spec"]["containers"][0];
        assert_eq!(c["image"], "bluenviron/mediamtx:1.21.1-ffmpeg");
        assert_eq!(c["args"][0], "/shared/cameras/cam1/mediamtx.yml");
        assert_eq!(c["ports"][0]["name"], "w8189");
        assert_eq!(c["ports"][0]["protocol"], "UDP");
        assert_eq!(c["ports"][1]["containerPort"], 8890);
        assert_eq!(c["resources"]["limits"]["memory"], "1Gi");
        assert!(c["resources"]["limits"]["cpu"].is_null(), "空は指定しない");
        let vols = pod["spec"]["volumes"].as_array().unwrap();
        assert_eq!(
            vols[0]["configMap"]["items"][0]["path"],
            "cameras/cam1/mediamtx.yml"
        );
        assert_eq!(
            vols[1]["persistentVolumeClaim"]["claimName"],
            "amore-hq-recordings"
        );
        // selector はテンプレートのラベルに含まれる。
        for (k, v) in d["spec"]["selector"]["matchLabels"].as_object().unwrap() {
            assert_eq!(&pod["metadata"]["labels"][k], v);
        }

        let cm = camera_configmap(&cfg, cam).unwrap();
        assert!(cm["data"]["mediamtx.yml"]
            .as_str()
            .unwrap()
            .contains("webrtcAdditionalHosts"));
        assert!(cm["data"]["transcode.sh"]
            .as_str()
            .unwrap()
            .starts_with("#!"));

        let svc = camera_service(&cfg, cam);
        assert_eq!(svc["metadata"]["name"], "amore-cam-hq-cam1");
        assert_eq!(svc["spec"]["ports"][0]["port"], 9997);

        let site = camera_site_service(&cfg, cam);
        assert_eq!(
            site["metadata"]["annotations"]["metallb.universe.tf/loadBalancerIPs"],
            "10.40.0.20"
        );
        assert_eq!(
            site["metadata"]["annotations"]["metallb.universe.tf/allow-shared-ip"],
            "amore-hq"
        );
        assert_eq!(site["spec"]["externalTrafficPolicy"], "Cluster");
        assert_eq!(site["spec"]["ports"][1]["port"], 8890);
    }

    #[test]
    fn vip_services_use_keepalived_ip_and_local_policy() {
        let cfg = kube_sample();
        let vips = camera_vip_services(&cfg, &cfg.cameras[1]);
        assert_eq!(vips.len(), 2);
        assert_eq!(vips[0]["metadata"]["name"], "amore-vip-cam2");
        assert_eq!(vips[0]["spec"]["externalIPs"][0], "149.40.2.1");
        assert_eq!(vips[0]["spec"]["externalTrafficPolicy"], "Local");
        assert_eq!(vips[0]["spec"]["loadBalancerClass"], KEEPALIVED_LB_CLASS);
        assert!(
            vips[0]["spec"]["selector"][LABEL_SITE].is_null(),
            "全拠点の Pod を選ぶ"
        );
        assert_eq!(vips[0]["spec"]["ports"][0]["port"], 8190);
        assert_eq!(vips[1]["metadata"]["name"], "amore-obs-cam2");
        assert_eq!(vips[1]["spec"]["externalIPs"][0], "10.40.0.25");
        assert_eq!(vips[1]["spec"]["ports"][0]["port"], 8891);

        let amore = amore_services(&cfg);
        assert_eq!(amore.len(), 3);
        assert_eq!(amore[0]["metadata"]["name"], "amore-hq-https");
        assert_eq!(amore[0]["spec"]["ports"][0]["targetPort"], 8443);
        assert_eq!(amore[1]["spec"]["externalIPs"][0], "149.40.2.1");

        let mut single = kube_sample();
        single.cluster.enabled = false;
        assert!(camera_vip_services(&single, &single.cameras[0]).is_empty());
        assert_eq!(amore_services(&single).len(), 1);
        assert_eq!(
            amore_services(&single)[0]["metadata"]["name"],
            "amore-default-https"
        );
    }

    #[test]
    fn hash_changes_with_config() {
        let cfg = kube_sample();
        let a = spec_hash(&cfg, &cfg.cameras[0]).unwrap();
        assert_eq!(a, spec_hash(&cfg, &cfg.cameras[0]).unwrap());
        let mut changed = cfg.clone();
        changed.kubernetes.camera_resources.memory_limit = "2Gi".into();
        assert_ne!(a, spec_hash(&changed, &changed.cameras[0]).unwrap());
        let mut renamed = cfg.clone();
        renamed.cameras[0].name = "別名".into();
        assert_eq!(
            a,
            spec_hash(&renamed, &renamed.cameras[0]).unwrap(),
            "表示名では作り直さない"
        );
    }

    fn pod(state: Value, last: Value, restarts: u64) -> Value {
        json!({
            "metadata": {
                "name": "amore-cam-hq-cam1-5d9c-abcde",
                "labels": { LABEL_CAMERA: "cam1", LABEL_WORKLOAD: "amore-cam-hq-cam1" },
                "annotations": { ANNOTATION_HASH: "abc" },
            },
            "spec": { "nodeName": "k8s-hq1" },
            "status": { "containerStatuses": [{
                "name": "mediamtx", "restartCount": restarts, "state": state, "lastState": last,
            }]},
        })
    }

    #[test]
    fn maps_pod_status_for_health_checks() {
        let cfg = kube_sample();
        let net = &cfg.media.docker_network;

        let running = pod_info(
            &cfg,
            &pod(
                json!({ "running": { "startedAt": "2026-10-27T01:00:01Z" } }),
                json!({}),
                0,
            ),
        );
        assert_eq!(running.name, "amore-cam-hq-cam1");
        assert_eq!(running.camera, "cam1");
        assert_eq!(running.state, "running");
        assert_eq!(running.config_hash, "abc");
        assert!(running.status.contains("ノード k8s-hq1"));
        assert_eq!(health::assess(&running, net, Some(0)), None);

        let crash = pod_info(
            &cfg,
            &pod(
                json!({ "waiting": { "reason": "CrashLoopBackOff", "message": "back-off 40s" } }),
                json!({ "terminated": { "exitCode": 1, "reason": "Error" } }),
                5,
            ),
        );
        assert_eq!(crash.state, "restarting");
        assert_eq!(crash.exit_code, 1);
        let p = health::assess(&crash, net, Some(4)).unwrap();
        assert!(
            p.contains("再起動を繰り返しています（5回、終了コード 1"),
            "{p}"
        );

        let oom = pod_info(
            &cfg,
            &pod(
                json!({ "running": {} }),
                json!({ "terminated": { "exitCode": 137, "reason": "OOMKilled" } }),
                1,
            ),
        );
        assert!(oom.oom_killed);
        assert!(health::assess(&oom, net, Some(0))
            .unwrap()
            .contains("メモリ不足"));

        let pull = pod_info(
            &cfg,
            &pod(
                json!({ "waiting": { "reason": "ImagePullBackOff", "message": "not found" } }),
                json!({}),
                0,
            ),
        );
        assert_eq!(pull.state, "created");
        assert!(health::assess(&pull, net, None)
            .unwrap()
            .contains("ImagePullBackOff"));

        let mut pending = pod(Value::Null, json!({}), 0);
        pending["status"] = json!({ "conditions": [{
            "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
            "message": "0/3 nodes are available: 3 node(s) didn't match pod affinity rules.",
        }]});
        let pending = pod_info(&cfg, &pending);
        assert_eq!(pending.state, "created");
        assert!(
            pending.error.contains("配置できません（Unschedulable）"),
            "{}",
            pending.error
        );
    }

    #[test]
    fn keepalived_configmap_has_one_file_per_node() {
        let cfg = kube_sample();
        let cm = keepalived_configmap(&cfg).unwrap();
        let data = cm["data"].as_object().unwrap();
        assert_eq!(data.len(), 3);
        assert!(data["k8s-site1.conf"]
            .as_str()
            .unwrap()
            .contains("unicast_src_ip 10.40.0.43"));
        let cams = cameras_configmap(&cfg, "cameras: []\n");
        assert_eq!(cams["metadata"]["name"], "amore-hq-cameras");
    }
}
