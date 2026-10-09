# AMoRe要件定義書

## 正式名称
Aketi MoIP on Rust engine

## Mission
- 可能な限り既存OSSを活用し構築を行う
- OBSでの動作/編集を最終的に行えるようにする
- 低遅延/高画質な映像伝送を行う
- 自作の部分はRastで作成を行う
- Serverの設定は[AMoRe.yaml]で管理を行う
    - Server側での自動録画保存先
    - 接続上限
    - 動画品質
    - 他

## 使用技術
- [RUST](https://rust-lang.org/ja/)
- [SRT](https://www.paltek.co.jp/solution/video/srt/index.html)
- [Docker](https://www.docker.com/ja-jp/)
- [OBS](https://obsproject.com/ja)
- [Zabbix](https://www.zabbix.com/jp)
- [Grafana](https://grafana.com/ja/)
- [Prometheus](https://prometheus.io/)
- [Loki](https://grafana.com/ja/oss/loki/)

## 想定動作環境
### カメラPC
- [Chromium](https://www.chromium.org/chromium-projects/) Webブラウザ
- USBWebカメラ
### Server
- LinuxServer[Rocky/Ubuntuを作者はテスト予定]
- すべての設定は[AMoRe.yaml]で管理を行う
- Rastで可能な限り作成
### 編集PC
- OBS
    - 可能な限りプラグインで実装


## 構成案
````mermaid
flowchart TB

subgraph カメラPC_A
  AA[Webブラウザ]
  AB[Zabbix Agent]
  AC[Prometheus Agent]
  AD[DLoki Agent]
  end
AA--SRT-->AZ
AB-->CC
AC-->CE
AD-->CD

subgraph カメラPC_B
  BA[監視は同様]
  BB[Webブラウザ]
  end
BB--SRT-->BZ

subgraph Server
  AZ[コンテナ]-->CA
  BZ[コンテナ]-->CA
  CA[Docker]-->CB[AMoRe]
  CC[Zabbix Server]-->CF
  CD[Grafana Server]-->CF
  CE[Prometheus Server]-->CF
  CF[Shumoku]
  end
CB-->DA

subgraph 編集PC
  DA[OBS]
  end


DA-->D[配信プラットフォーム]
````
