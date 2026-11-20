// model_catalog.rs — 整文・翻訳のモデル候補を、アプリを更新せずに差し替える
//
// 2026-11-21: 提供元 (Groq など) がモデルを廃止すると、アプリに組み込んだ候補が使えなくなる
// (2026-11-16 に Groq の llama-3.3-70b-versatile が無くなった)。公開リポジトリの `models.json`
// を読み、候補の一覧を差し替えられるようにした (会長要望「アップデートできるようにしておきたい」)。
//
// 優先順: 利用者が設定で書いたモデル名 > 公開の一覧 (models.json) > アプリに組み込んだ既定値。
//   - 一覧は起動時と、候補をすべて試しても「モデルが無い」で終わったときに取りに行く (1 日 1 回まで。
//     「無い」のときは 10 分空ければもう一度)。
//   - 取れた一覧は settings_kv に控え、次の起動ではネットを待たずに使う。
//   - 取れない・形が違う・値が怪しいときは使わず、控えか既定値のまま動く (アプリは止めない)。
//   - 取りに行かない設定 (settings_kv "model_catalog_auto_update" = "0") も用意する。
//
// 一覧に書けるのはモデル名だけ。接続先 URL・キーはここでは変えない (送り先を外から変えさせない)。

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

/// 公開の一覧の置き場所 (公開リポジトリの main ブランチ)。
pub const CATALOG_URL: &str = "https://raw.githubusercontent.com/rushvoice/rushvoice/main/models.json";

/// settings_kv のキー
pub const CATALOG_CACHE_KEY: &str = "model_catalog_cache";
pub const CATALOG_FETCHED_AT_KEY: &str = "model_catalog_fetched_at";
pub const CATALOG_AUTO_UPDATE_KEY: &str = "model_catalog_auto_update";

/// 普段の取り直しの間隔と、「モデルが無い」で急ぐときの最短間隔
const REFRESH_INTERVAL_SECS: i64 = 24 * 60 * 60;
const URGENT_MIN_INTERVAL_SECS: i64 = 10 * 60;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// 一覧の大きさの上限 (これを超えるものは怪しいので使わない)
const MAX_BYTES: usize = 64 * 1024;
const MAX_MODELS_PER_PROVIDER: usize = 10;
const MAX_MODEL_NAME_LEN: usize = 100;

/// 一覧の形式の版。上がったら古いアプリは読まない (既定値のまま動く)。
const SUPPORTED_SCHEMA: u32 = 1;

#[derive(Debug, Deserialize)]
struct RawCatalog {
    schema: u32,
    #[serde(default)]
    chat: HashMap<String, Vec<String>>,
}

/// 検証済みの一覧。接続先の名前 ("groq" / "openai") → 前から順に試すモデル名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    pub chat: HashMap<String, Vec<String>>,
}

impl ModelCatalog {
    pub fn chat_models(&self, provider: &str) -> Option<&[String]> {
        self.chat.get(provider).map(|v| v.as_slice()).filter(|v| !v.is_empty())
    }
}

/// モデル名として受け付ける文字だけか (英数字と . _ - : / のみ)。
fn is_valid_model_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_MODEL_NAME_LEN
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
}

/// 一覧の JSON を読んで確かめる (純粋関数)。形が違えば None。
/// 対象は "groq" と "openai" だけ (カスタム接続先は利用者がモデル名を書くので触らない)。
pub fn parse_catalog(json: &str) -> Option<ModelCatalog> {
    if json.len() > MAX_BYTES {
        return None;
    }
    let raw: RawCatalog = serde_json::from_str(json).ok()?;
    if raw.schema != SUPPORTED_SCHEMA {
        return None;
    }
    let mut chat = HashMap::new();
    for provider in ["groq", "openai"] {
        let Some(list) = raw.chat.get(provider) else { continue };
        let mut out: Vec<String> = Vec::new();
        for m in list {
            let m = m.trim();
            if !is_valid_model_name(m) {
                return None; // 1 つでも怪しい値があれば一覧ごと使わない
            }
            if !out.iter().any(|x| x == m) {
                out.push(m.to_string());
            }
        }
        if out.len() > MAX_MODELS_PER_PROVIDER {
            return None;
        }
        if !out.is_empty() {
            chat.insert(provider.to_string(), out);
        }
    }
    if chat.is_empty() {
        return None;
    }
    Some(ModelCatalog { chat })
}

/// 取りに行く時期か (純粋関数)。`urgent` は「モデルが無い」で終わったとき。
pub fn should_refresh(auto_update: &str, fetched_at: Option<i64>, now: i64, urgent: bool) -> bool {
    if auto_update.trim() == "0" {
        return false;
    }
    let Some(t) = fetched_at else { return true };
    let elapsed = now - t;
    if elapsed < 0 {
        return true; // 時計が戻った
    }
    if urgent {
        elapsed >= URGENT_MIN_INTERVAL_SECS
    } else {
        elapsed >= REFRESH_INTERVAL_SECS
    }
}

// ===========================================================================
// 設定 (settings_kv) とネットを使う薄い関数
// ===========================================================================

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn open_config() -> Option<crate::config_db::ConfigDb> {
    crate::config_db::ConfigDb::open(&crate::config_db::default_config_db_path()).ok()
}

fn kv(db: &crate::config_db::ConfigDb, key: &str) -> String {
    db.get_kv(key).ok().flatten().unwrap_or_default()
}

/// 控えてある一覧を読む (無い・壊れていれば None)。
pub fn load_cached_catalog() -> Option<ModelCatalog> {
    let db = open_config()?;
    parse_catalog(&kv(&db, CATALOG_CACHE_KEY))
}

async fn fetch_catalog_text() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(CATALOG_URL).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("too large".into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
}

/// 必要なら公開の一覧を取り直して控える。失敗しても何もしない (best-effort)。
pub async fn refresh_if_due(urgent: bool) {
    let Some(db) = open_config() else { return };
    let fetched_at = kv(&db, CATALOG_FETCHED_AT_KEY).trim().parse::<i64>().ok();
    let now = now_secs();
    if !should_refresh(&kv(&db, CATALOG_AUTO_UPDATE_KEY), fetched_at, now, urgent) {
        return;
    }
    // 失敗しても試した時刻は残す (取れないときに毎回ネットを待たせない)
    let _ = db.set_kv(CATALOG_FETCHED_AT_KEY, &now.to_string());
    match fetch_catalog_text().await {
        Ok(text) => match parse_catalog(&text) {
            Some(cat) => {
                let _ = db.set_kv(CATALOG_CACHE_KEY, &text);
                log::info!(
                    "[model_catalog] updated: groq={:?} openai={:?}",
                    cat.chat_models("groq"),
                    cat.chat_models("openai")
                );
            }
            None => log::warn!("[model_catalog] fetched list rejected (bad format); keeping current"),
        },
        Err(e) => log::warn!("[model_catalog] fetch failed: {}; keeping current", e),
    }
}

/// 起動時・「モデルが無い」のときに、裏で取り直す。
pub fn spawn_refresh(urgent: bool) {
    tauri::async_runtime::spawn(async move {
        refresh_if_due(urgent).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
        "schema": 1,
        "updated": "2026-11-21",
        "chat": {
            "groq": ["openai/gpt-oss-120b", "qwen/qwen3.8-27b", "openai/gpt-oss-20b"],
            "openai": ["gpt-4o-mini", "gpt-4.1-mini"]
        }
    }"#;

    #[test]
    fn parses_good_catalog() {
        let c = parse_catalog(GOOD).expect("ok");
        assert_eq!(c.chat_models("groq").unwrap()[0], "openai/gpt-oss-120b");
        assert_eq!(c.chat_models("groq").unwrap().len(), 3);
        assert_eq!(c.chat_models("openai").unwrap(), ["gpt-4o-mini", "gpt-4.1-mini"]);
        assert!(c.chat_models("custom").is_none());
    }

    #[test]
    fn repo_models_json_is_valid() {
        // リポジトリ直下の models.json (公開の一覧の元) が、このアプリで読める形であること
        let text = include_str!("../../models.json");
        let c = parse_catalog(text).expect("models.json must parse");
        assert!(c.chat_models("groq").is_some());
        assert!(c.chat_models("openai").is_some());
    }

    #[test]
    fn rejects_bad_catalogs() {
        assert!(parse_catalog("").is_none());
        assert!(parse_catalog("not json").is_none());
        // 形式の版が違う
        assert!(parse_catalog(r#"{"schema":2,"chat":{"groq":["a"]}}"#).is_none());
        // 中身が無い
        assert!(parse_catalog(r#"{"schema":1,"chat":{}}"#).is_none());
        assert!(parse_catalog(r#"{"schema":1,"chat":{"groq":[]}}"#).is_none());
        // 怪しい文字 (空白・URL・制御文字)
        assert!(parse_catalog(r#"{"schema":1,"chat":{"groq":["a b"]}}"#).is_none());
        assert!(parse_catalog(r#"{"schema":1,"chat":{"groq":["https://evil.example/x?y=1"]}}"#).is_none());
        assert!(parse_catalog(r#"{"schema":1,"chat":{"groq":["a\nb"]}}"#).is_none());
        // 多すぎる
        let many: Vec<String> = (0..11).map(|i| format!("\"m{}\"", i)).collect();
        let j = format!(r#"{{"schema":1,"chat":{{"groq":[{}]}}}}"#, many.join(","));
        assert!(parse_catalog(&j).is_none());
        // 長すぎる名前
        let long = "a".repeat(101);
        assert!(parse_catalog(&format!(r#"{{"schema":1,"chat":{{"groq":["{}"]}}}}"#, long)).is_none());
    }

    #[test]
    fn ignores_unknown_providers_and_dedups() {
        let c = parse_catalog(r#"{"schema":1,"chat":{"groq":["a"," a ","b"],"custom":["x"]}}"#).unwrap();
        assert_eq!(c.chat_models("groq").unwrap(), ["a", "b"]);
        assert!(c.chat_models("custom").is_none());
    }

    #[test]
    fn refresh_timing() {
        let now = 1_000_000;
        assert!(should_refresh("", None, now, false));
        assert!(!should_refresh("0", None, now, true)); // 取りに行かない設定
        assert!(!should_refresh("", Some(now - 3600), now, false));
        assert!(should_refresh("", Some(now - 25 * 3600), now, false));
        assert!(!should_refresh("", Some(now - 60), now, true)); // 急ぎでも 10 分は空ける
        assert!(should_refresh("", Some(now - 11 * 60), now, true));
        assert!(should_refresh("", Some(now + 100), now, false)); // 時計が戻った
    }
}
