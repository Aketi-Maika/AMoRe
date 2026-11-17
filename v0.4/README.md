# AMoRe v0.4

Aketi MoIP on Rust engine — 開発段階1（初期版）。要件は [`requirements definition_v2.md`](../requirements%20definition_v2.md) を参照。

カメラPCのブラウザからWebRTCで送った映像・音声を、ServerがカメラごとのSRTストリーム（MPEG-TS / H.264・AAC）に変換し、編集PCのOBSで最大4台同時に受信する。
Serverの管理Webでカメラを追加すると、そのカメラ専用のメディア処理コンテナ（MediaMTX＋FFmpeg）が自動で作成される（**1カメラ＝1コンテナ**）。

## v0.3 からの変更点

| 項目 | 内容 |
| --- | --- |
| 拠点ごとの Server による冗長化 | 拠点ごとに AMoRe Server を置き（拠点間VPNで VLAN40 を L2 延伸）、どれかが止まっても送受信を続ける。**カメラPCは全 Server 共通の1つの IP**（keepalived / VRRP。既定 `149.40.2.1/24`、`cluster.yaml` で変更可）へ送り、Server が切り替わると同じ IP へ自動で張り直す。`dual` にしたカメラは共通IPに加えて別の Server の実IPへも同時に送り、途切れない。OBS は OBS 用の共通IP（既定 `10.40.0.25`）へ固定の URL で接続する |
| カメラ情報の共有 | カメラの一覧（改訂番号つき）は全 Server で同期し、どの Server の管理Webで変更してもよい。状態は Server 間で常に交換する。各 Server は自分が直接受けていないカメラの映像を、直接受けている Server から常に中継で取り込む（SRT・再エンコードなし）ため、全 Server が全カメラの映像を持つ |
| 画面ごとの入室パスワード | 管理画面・カメラ画面（送信画面）・モニター・OBSドックごとに、パスワードの要否を `access.yaml` で ON/OFF（既定は OFF）。期限付きの入室トークン（HMAC 署名）で API を守る。総当たり対策つき |
| 設定ファイルの分割 | `AMoRe.yaml` の `include` に書いたファイル（`cameras.yaml`・`recording.yaml`・`secrets.yaml`・`access.yaml`・`cluster.yaml`・`monitoring.yaml`・`ai.yaml`）へセクションごとに分ける。同じセクションが2つのファイルにあればエラー。管理Web・同期は `cameras.yaml` だけを書き換える。1ファイルの v0.3 形式もそのまま読める |
| 不具合修正 | AMoRe の HTTP クライアントに TLS が組み込まれておらず、AI 連携（`https://api.anthropic.com`）や `https://` の Loki・Grafana へ接続できなかった |

v0.3 で稼働中の Server を更新する手順は、Server手順書の「[v0.3 から v0.4 への更新](Server/AMoRe-Server_構築手順書.md#v03-から-v04-への更新)」を参照（登録済みカメラ・ポート・証明書は引き継ぐため、1台で使う間は OBS とカメラPCの設定変更は不要）。

```mermaid
flowchart LR
    subgraph HQ[本拠点 VLAN40]
        S1[AMoRe Server hq<br/>10.40.0.20]
        OBS[編集PC OBS]
    end
    subgraph SITE[遠隔拠点 VLAN40（拠点間VPNで L2 延伸）]
        S2[AMoRe Server site1<br/>10.40.0.21]
        CAM[カメラPC<br/>Chrome/Edge 送信画面]
    end
    CVIP((カメラ用 共通IP<br/>149.40.2.1)) --- S1
    CVIP -.->|障害時に移る| S2
    OVIP((OBS用 共通IP<br/>10.40.0.25)) --- S1
    OVIP -.->|障害時に移る| S2
    CAM -->|WebRTC・WHIP| CVIP
    CAM -.->|dual のカメラだけ| S2
    S1 <-->|一覧の同期・状態交換<br/>映像の中継 SRT| S2
    OBS -->|SRT URL固定| OVIP
```

1台だけで使う場合（`cluster.enabled: false`）の構成は v0.3 と同じ：

```mermaid
flowchart LR
    ADMIN[管理PC<br/>管理Web・モニター] -->|カメラ追加・削除・録画<br/>映像確認・AI診断| AMORE
    CAM[カメラPC<br/>Chrome/Edge 送信画面] -->|WebRTC<br/>WHIP| CONT
    subgraph SERVER[Server 10.40.0.20]
        AMORE[AMoRe（Rust）<br/>設定管理・コンテナ管理<br/>不調検知・自動復旧・入室] -->|自動作成| CONT[amore-cam-camN<br/>MediaMTX＋FFmpeg<br/>カメラごと]
        CONT -->|録画| REC[(recording.host_dir)]
        OPC[1Password Connect<br/>任意] -.->|秘密情報| AMORE
    end
    CONT -->|SRT カメラごとのポート<br/>MPEG-TS / H.264・AAC| OBS[編集PC OBS<br/>メディアソース×4]
    OBS --> PLATFORM[配信プラットフォーム]
    AMORE -.->|ログ・値・注釈| MON[既存の監視基盤<br/>Prometheus・Loki・Zabbix・Grafana]
    AMORE -.->|診断・映像解析| AI[AI API<br/>Claude / OpenAI互換]
```

## 構成

| フォルダ | 内容 | 構築手順書 |
| --- | --- | --- |
| [`Server/`](Server/) | AMoRe本体（Rust）、管理Web・モニター、Docker Compose、設定例（`config.example/`）、keepalived（`keepalived/`）、監視用テンプレート（`monitoring/`） | [AMoRe-Server_構築手順書.md](Server/AMoRe-Server_構築手順書.md) |
| [`Camera/`](Camera/) | ブラウザ送信画面（JavaScript）。Serverが `https://<Server>/` で配信 | [AMoRe-Camera_構築手順書.md](Camera/AMoRe-Camera_構築手順書.md) |
| [`OBS/`](OBS/) | シーンコレクション、状態表示ドック。ドックはServerが `https://<Server>/obs/` で配信 | [AMoRe-OBS_構築手順書.md](OBS/AMoRe-OBS_構築手順書.md) |
| [`VPN/`](VPN/) | 遠隔拠点（IX2215）へ VLAN40 を L2 延伸する SoftEther VPN Server（EtherIP over IPsec）、IX2215 設定例 | [AMoRe-VPN_構築手順書.md](VPN/AMoRe-VPN_構築手順書.md) |

構築順序：**Server（管理Webでカメラ追加まで）→ Camera → OBS**（遠隔拠点のカメラPCを使う場合は Camera の前に VPN）。録画・連携・入室パスワードは Server 手順書の手順8〜13で必要なものだけ設定する。冗長構成は VPN を構築してから Server 手順書の手順14で行う。

## ネットワーク

| 機器 | IP / VLAN | 備考 |
| --- | --- | --- |
| CoreServer（Proxmox） | `10.40.0.10/16` / VLAN40 | Server VMのホスト |
| AMoRe Server VM `amore01` | `10.40.0.20/16` / VLAN40 | Rocky Linux 9 + Docker |
| AMoRe Server VM `amore02` | `10.40.0.21/16` / VLAN40（L2延伸） | 任意（冗長構成。遠隔拠点） |
| カメラ用の共通IP | `149.40.2.1/24`（VLAN40 上。x530L に `149.40.2.254/24 secondary`） | 任意（冗長構成。`cluster.yaml` で変更可）。**プライベートアドレスではない**ため社外へ漏らさない |
| OBS 用の共通IP | `10.40.0.25/16` / VLAN40 | 任意（冗長構成。`cluster.yaml` で変更可） |
| 編集PC（OBS） | `10.30.0.10/16` / VLAN30 | x530LでVLAN間ルーティング |
| SoftEther VPN Server VM `sevpn01` | `10.40.0.30/16` / VLAN40 | 任意（遠隔拠点を使う場合）。UDP 500/4500 を本拠点ルーターから転送 |
| 遠隔拠点 IX2215 `BVI1` | `10.40.0.31/16` / VLAN40（L2延伸） | 任意（遠隔拠点を使う場合） |
| 監視基盤 | 例: `10.20.0.20` / VLAN20 | 任意（既存の Loki・Zabbix・Grafana・Prometheus） |

| ポート | 用途 |
| --- | --- |
| 443/tcp | 送信画面・OBSドック・管理Web・モニター・API・`/metrics`・Server 間の状態交換 |
| 8189-8192/udp | WebRTC（カメラPC→Server、管理Webの映像確認・モニター）。カメラごとに1つ |
| 8890-8893/udp | SRT（Server→OBS、Server 間の中継）。カメラごとに1つ |
| VRRP（IP プロトコル 112） | 共通IP（Server 同士。冗長構成のみ） |

## API（AMoRe）

| メソッド・パス | 内容 |
| --- | --- |
| `GET /healthz` | 稼働確認（keepalived もこれで Server の稼働を確かめる） |
| `GET /api/v1/access` | 画面ごとにパスワードが必要か、送ったトークンが有効か |
| `POST /api/v1/login` | 入室（`{"scope": "admin", "password": "…"}` → 入室トークン） |
| `GET /api/v1/config` | 送信画面向け設定（カメラ一覧・映像/音声・品質制御・再接続・ICE、冗長構成ではカメラごとの single/dual と予備の送り先） |
| `GET /api/v1/status` | カメラごとの受信・出力・コンテナ状態・不調・録画・中継・直接受けている Server・AI解析、OBS接続数、SRT URL、冗長構成の状態 |
| `GET /api/v1/ai` | AIの映像解析・推奨カメラ・直近のエラー |
| `GET /metrics` | Prometheus 形式のメトリクス（`monitoring.prometheus`） |
| `POST /api/v1/cameras/{id}/whip` | WHIP（SDP offer → answer）。未登録ID=404、他の画面が送信中=409 |
| `PATCH` / `DELETE /api/v1/cameras/{id}/whip/{session}` | WHIPセッションの更新・終了 |
| `GET /api/v1/admin/overview` | 管理Web：Docker・イメージ・全カメラの状態・冗長構成・設定ファイルの構成・最近の出来事 |
| `POST /api/v1/admin/cameras` | 管理Web：カメラ追加（`{"name": "...", "id": "任意"}`）。`cameras.yaml` へ保存しコンテナを作成（冗長構成では全 Server へ配る） |
| `PATCH` / `DELETE /api/v1/admin/cameras/{id}` | 管理Web：表示名の変更 / カメラとコンテナの削除 |
| `POST /api/v1/admin/cameras/{id}/restart` | 管理Web：コンテナの再起動（不調時は作り直し） |
| `POST /api/v1/admin/cameras/{id}/record` | 管理Web：録画の有効・無効（`{"enabled": true}`） |
| `POST /api/v1/admin/cameras/{id}/redundancy` | 管理Web：冗長化の方式（`{"mode": "dual"}`。冗長構成のみ） |
| `POST /api/v1/admin/cameras/{id}/diagnose` | 管理Web：AIによる障害診断 |
| `GET /api/v1/admin/cameras/{id}/logs` | 管理Web：コンテナのログ |
| `POST /api/v1/admin/cameras/{id}/whep` | 管理Web・モニター：映像確認（WHEP。中継中のカメラは映像のみ） |
| `POST /api/v1/admin/reconcile` | 管理Web：カメラ一覧とコンテナをそろえ直す |
| `GET /api/v1/cluster/state` / `POST /api/v1/cluster/cameras` | Server 同士：状態交換・カメラ一覧の配布（`X-AMoRe-Cluster` の署名で認証） |

入室パスワードを ON にした範囲の API は `Authorization: Bearer <入室トークン>` が必要（無い・期限切れは `401`）。

## v0.4 の範囲と制限

- カメラ最大4台、1080p30（帯域不足時720p30）、H.264優先・VP8はServerでH.264へ変換、音声はOpus→AAC-LC。
- 入室パスワードは画面の利用者を絞るためのもの。AMoRe は引き続き Docker ソケットを使うため、管理画面のパスワードは強固にし、Server（443/tcp）へ到達できる範囲も LAN（VPN 内）に限る。インターネット公開は想定しない。
- 冗長構成で single のカメラは、Server の切替時に数秒途切れる（共通IPの移動 約3秒＋WebRTC の張り直し＋OBS の再接続）。途切れを避けたいカメラは dual にする（カメラPCの上り帯域・負荷が2倍）。
- Server 間で全カメラの映像を常に中継するため、拠点間VPNに1台あたり約 6〜8Mbps が流れる。VPN 自体の冗長化は範囲外（VPN が切れると拠点ごとに独立して動く）。
- 中継中のカメラの映像確認（WHEP）は映像のみ（OBS 向けの AAC はブラウザで再生できない）。
- カメラの一覧は改訂番号の新しい方を採用する。複数の Server で同時に変更した場合は後の変更が残る（変更は1か所で行う）。
- 全 Server で同じ証明書・`cluster.shared_secret`・`access.yaml` を使う。Server 同士の認証は時刻入りの署名のため、全 Server の時刻を NTP で合わせる。
- HTTPSは自己署名証明書（各PCで信頼登録が必要）。
- 録画の切替はそのカメラのコンテナを作り直すため、送信・受信が数秒途切れる。録画・AI・監視は Server ごとに動く。
- AI 連携は外部の API へログ・静止画を送る（既定で無効）。AI の診断・提案は参考情報であり、OBS は自動で操作しない。
- **v0.4 の Rust コードはまだコンパイル・実機試験をしていない**（開発環境に Rust がないため）。Server 手順書の `docker compose build` が初回のコンパイルになる。ビルドエラーが出た場合はエラー全文を記録して開発側へ連絡する。

## 開発

```bash
cd v0.4/Server/amore
cargo fmt --check
cargo clippy --all-targets
cargo test
mkdir -p /tmp/amore-config && cp ../config.example/*.yaml /tmp/amore-config/
cargo run -- --config /tmp/amore-config/AMoRe.yaml check
cargo run -- --config /tmp/amore-config/AMoRe.yaml render-mediamtx
cargo run -- --config /tmp/amore-config/AMoRe.yaml render-keepalived   # cluster.yaml の enabled: true が必要
```
