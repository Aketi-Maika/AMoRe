//! 既存の監視基盤への送信（`AMoRe.yaml` の `monitoring`）。
//!
//! - `metrics`：Prometheus 形式のメトリクス公開（Grafana で表示）
//! - `loki`：AMoRe とカメラコンテナのログ・出来事の送信
//! - `zabbix`：Zabbix sender プロトコルでの値の送信（trapper アイテム・カメラの自動検出）
//! - `grafana`：出来事を注釈（アノテーション）として記録

pub mod grafana;
pub mod loki;
pub mod metrics;
pub mod zabbix;
