//! 秘密情報（APIキー・パスワード・トークン）の取得。
//!
//! `AMoRe.yaml` には値を直接書かず、次の参照で指定する。参照でない値はそのまま使う。
//! - `op://保管庫/アイテム/項目`（または `op://保管庫/アイテム/セクション/項目`）… 1Password Connect サーバーから取得
//! - `env:変数名` … AMoRe コンテナの環境変数
//! - `file:/パス` … AMoRe コンテナ内のファイル（末尾の改行は除く）
//!
//! 取得した値はログ・API・`check` の出力に出さない。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::{parse_duration_secs, parse_op_reference, OnePasswordConnect};

#[derive(Clone)]
pub struct SecretResolver {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    op: OnePasswordConnect,
    ttl: Duration,
    cache: Mutex<HashMap<String, (String, Instant)>>,
}

#[derive(Debug, Deserialize)]
struct IdOnly {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(default)]
    fields: Vec<Field>,
    #[serde(default)]
    sections: Vec<Section>,
}

#[derive(Debug, Deserialize)]
struct Field {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    section: Option<IdOnly>,
}

#[derive(Debug, Deserialize)]
struct Section {
    id: String,
    #[serde(default)]
    label: String,
}

impl SecretResolver {
    pub fn new(op: &OnePasswordConnect) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let ttl = Duration::from_secs(parse_duration_secs(&op.cache_ttl).unwrap_or(300));
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                op: op.clone(),
                ttl,
                cache: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// 参照を解決する。参照でない値はそのまま返す。空文字は空のまま返す。
    pub async fn resolve(&self, value: &str) -> Result<String> {
        if let Some(name) = value.strip_prefix("env:") {
            return std::env::var(name).with_context(|| format!("環境変数 {name} がありません"));
        }
        if let Some(path) = value.strip_prefix("file:") {
            let text = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("ファイル {path} を読めません"))?;
            return Ok(text.trim_end_matches(['\r', '\n']).to_string());
        }
        if !value.starts_with("op://") {
            return Ok(value.to_string());
        }

        {
            let cache = self.inner.cache.lock().await;
            if let Some((v, at)) = cache.get(value) {
                if at.elapsed() < self.inner.ttl {
                    return Ok(v.clone());
                }
            }
        }
        let resolved = self.fetch_op(value).await?;
        self.inner
            .cache
            .lock()
            .await
            .insert(value.to_string(), (resolved.clone(), Instant::now()));
        Ok(resolved)
    }

    /// 解決できない場合は警告を出して None を返す（その機能だけ止める）。
    pub async fn resolve_or_warn(&self, field: &str, value: &str) -> Option<String> {
        match self.resolve(value).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("{field} の秘密情報を取得できません: {e:#}");
                None
            }
        }
    }

    async fn fetch_op(&self, reference: &str) -> Result<String> {
        let op = &self.inner.op;
        if !op.enabled {
            bail!("op:// 参照を使うには secrets.onepassword_connect.enabled を true にしてください");
        }
        let (vault, item, section, field) =
            parse_op_reference(reference).ok_or_else(|| anyhow!("参照の形式が不正です"))?;
        let token = tokio::fs::read_to_string(&op.token_file)
            .await
            .with_context(|| format!("Connect のトークン {} を読めません", op.token_file.display()))?;
        let token = token.trim();
        let base = op.url.trim_end_matches('/');

        let get = |url: String, filter: Option<String>| {
            let mut req = self.inner.http.get(url).bearer_auth(token);
            if let Some(f) = filter {
                req = req.query(&[("filter", f)]);
            }
            req
        };

        let vaults: Vec<IdOnly> = get(format!("{base}/v1/vaults"), Some(format!("name eq \"{vault}\"")))
            .send()
            .await
            .context("1Password Connect に接続できません")?
            .error_for_status()
            .context("1Password Connect が保管庫の一覧を返しません（トークン・権限を確認）")?
            .json()
            .await?;
        let vault_id = &vaults
            .first()
            .ok_or_else(|| anyhow!("保管庫 {vault:?} が見つかりません（Connect のアクセス範囲を確認）"))?
            .id;

        let items: Vec<IdOnly> = get(
            format!("{base}/v1/vaults/{vault_id}/items"),
            Some(format!("title eq \"{item}\"")),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
        let item_id = &items
            .first()
            .ok_or_else(|| anyhow!("アイテム {item:?} が保管庫 {vault:?} にありません"))?
            .id;

        let detail: Item = get(format!("{base}/v1/vaults/{vault_id}/items/{item_id}"), None)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        pick_field(&detail, section.as_deref(), &field)
            .ok_or_else(|| anyhow!("アイテム {item:?} に項目 {field:?} がありません"))
    }
}

/// アイテムの項目から値を取り出す（ラベルまたはIDで一致。セクション指定があればその中から）。
fn pick_field(item: &Item, section: Option<&str>, field: &str) -> Option<String> {
    let section_id = match section {
        Some(s) => Some(
            item.sections
                .iter()
                .find(|x| x.label == s || x.id == s)
                .map(|x| x.id.clone())?,
        ),
        None => None,
    };
    item.fields
        .iter()
        .filter(|f| f.label == field || f.id == field)
        .find(|f| match &section_id {
            Some(sid) => f.section.as_ref().is_some_and(|s| &s.id == sid),
            None => true,
        })
        .and_then(|f| f.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        serde_json::from_str(
            r#"{"id":"i1","title":"Loki",
                "sections":[{"id":"s1","label":"auth"}],
                "fields":[
                  {"id":"username","label":"username","value":"amore","purpose":"USERNAME"},
                  {"id":"password","label":"password","value":"top","purpose":"PASSWORD"},
                  {"id":"f3","label":"password","value":"in-section","section":{"id":"s1"}}
                ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn picks_fields_by_label_and_section() {
        let item = item();
        assert_eq!(pick_field(&item, None, "username").as_deref(), Some("amore"));
        assert_eq!(pick_field(&item, None, "password").as_deref(), Some("top"));
        assert_eq!(pick_field(&item, Some("auth"), "password").as_deref(), Some("in-section"));
        assert_eq!(pick_field(&item, None, "missing"), None);
        assert_eq!(pick_field(&item, Some("nope"), "password"), None);
    }

    #[tokio::test]
    async fn resolves_env_file_and_literals() {
        let resolver = SecretResolver::new(&OnePasswordConnect::default()).unwrap();
        assert_eq!(resolver.resolve("plain").await.unwrap(), "plain");
        std::env::set_var("AMORE_TEST_SECRET", "from-env");
        assert_eq!(resolver.resolve("env:AMORE_TEST_SECRET").await.unwrap(), "from-env");
        let path = std::env::temp_dir().join("amore-secret-test.txt");
        std::fs::write(&path, "from-file\n").unwrap();
        assert_eq!(
            resolver.resolve(&format!("file:{}", path.display())).await.unwrap(),
            "from-file"
        );
        // Connect が無効なら op:// は解決しない。
        assert!(resolver.resolve("op://AMoRe/Item/field").await.is_err());
    }
}
