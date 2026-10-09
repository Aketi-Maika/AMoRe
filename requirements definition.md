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

## 目的・利用場面
- 複数のカメラを使用し、簡易的にMoIPを行う必要がある場合
- 小規模イベントにおいて1人で複数台のカメラ映像を切り替えて映像配信を行う必要がある場合

## 開発範囲・優先順位
1. フルHD品質でカメラ(最大4台)→Server→OBSを接続する
1. 監視体制の導入
1. 4Kに対応
1. 最大カメラ数を100台に増加

## 映像伝送の経路
- 音声も入力を行う
- Serverはカメラごとの映像を受信し、それぞれ独立したストリームとしてOBSへ提供する。映像の合成・切り替えはOBSで行う
- Serverは受信映像をOBSで扱える形式へ変換する。映像のコーデック・設定が適合する場合は、映像を再エンコードせずに中継する
- SRT接続はServerをListener、OBSをCallerとし、OBSからServerへ接続する。
- 初期版ではOBS標準の「メディアソース」で受信する。OBSプラグインは、接続先の選択や状態表示など、操作を簡略化するために実装する
- OBSから配信プラットフォームへの送信方式は、配信先の仕様に従う。
````mermaid
flowchart LR
A[USBカメラ]--USB-->B
B[Chromiumブラウザ]--WebRTC-->C
C[Server]--SRT/AAC-->D
D[OBS]
````

## ブラウザからの送信方式
- カメラPCは、HTTPSで公開したAMoReの送信画面にアクセスして映像を送信する。映像送信のための専用アプリのインストールは不要とする。
- 利用者がカメラを選択し、ブラウザの利用許可を与えた後、getUserMedia()で映像を取得する。Media Capture仕様
- 取得した映像は、RTCPeerConnectionを使用してServerへWebRTCで送信する。接続確立に必要な情報はHTTPSのAPIで交換する。WebRTC仕様
- 映像コーデックはH.264を優先する。利用環境で使用できない場合はVP8で送信し、ServerでH.264へ変換する。
- NATなどにより直接接続できない環境では、STUN／TURNを利用して接続する。
- 送信画面には、映像プレビュー、送信開始・停止、接続状態、実際の解像度・FPS・ビットレートを表示する。
- 切断時は自動再接続を行い、カメラの権限不足や未接続など、自動復旧できない原因は画面に表示する。
- ブラウザ側の処理はJavaScriptで実装し、Server側の接続・設定管理などの自作部分はRustで実装する

## 性能目標
初期版は、次の条件を基準とします。
|項目	|暫定目標|
|---|---|
|標準映像品質	|1920×1080、30fps|
|映像ビットレート	|1カメラあたり目標6Mbps、設定上限8Mbps|
|同時接続	|カメラ4台、編集PC1台で全4映像を同時受信|
|LANでの映像遅延	|カメラ撮影からOBSプレビュー表示まで、95パーセンタイルで500ms以下|
|WANでの映像遅延	|同じ測定区間で、95パーセンタイルで1,000ms以下|
|初回表示	|送信開始とOBS接続の両方が成立してから5秒以内|
|切断からの復旧	|ネットワーク復旧後10秒以内に映像表示を再開|
|連続稼働	|4台同時接続で8時間、プロセスの異常終了がないこと|
|帯域不足時	|ビットレートを下げ、必要に応じて1280×720・30fpsへ移行し、画面に品質低下を表示|

## 使用技術
- [RUST](https://rust-lang.org/ja/)
- [SRT](https://www.paltek.co.jp/solution/video/srt/index.html)
- [Docker](https://www.docker.com/ja-jp/)
- [OBS](https://obsproject.com/ja)
- [Zabbix](https://www.zabbix.com/jp)
- [Grafana](https://grafana.com/ja/)
- [Prometheus](https://prometheus.io/)
- [Loki](https://grafana.com/ja/oss/loki/)
- [WebRTC](https://imageflux.sakura.ad.jp/column/webrtc/?gad_source=1&gad_campaignid=24051150930&gclid=CjwKCAjw_pzWBhAkEiwAwDCi1XUJJzi0OO7wd0sfIlrUjS9kjmw7kzsMncnDSZgNAE5avQmRE5ZL9RoC9YUQAvD_BwE)

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
