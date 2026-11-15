// migration.rs — Day 5 後半 v1 config.json → v2 SQLite 移行
//
// 申し送り §2 / 最小スコープ A 採用 (判断 2026-05-21)
//
// v1 config.json (`%APPDATA%/VoiceInputMvp/config.json`) を読み取り:
//   - API キー (openAiApiKey / groqApiKey) → crypto::save_secrets で
//     config.secrets.encrypted.json に DPAPI 暗号化保存
//   - hotkey (startKey/stopKey/holdToTalkKey) / モデル名 (transcribeModel/language)
//     → settings_kv テーブルに upsert
//   - dictionary (kv マップ) → dictionary テーブルに upsert
//   - fillers (配列) → fillers テーブルに置換
// v1 config.json は **削除しない** (ロールバック可能性のため)。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config_db::{ConfigDb, ConfigDbError};
use crate::crypto::{save_secrets, CryptoError};

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Config DB error: {0}")]
    ConfigDb(#[from] ConfigDbError),
    #[error("Crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

/// v1 config.json の最小スコープスキーマ。
/// 未知フィールドは serde が無視する (#[serde(default)] + Option で耐性確保)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct V1Config {
    #[serde(rename = "openAiApiKey")]
    pub openai_api_key: Option<String>,
    #[serde(rename = "groqApiKey")]
    pub groq_api_key: Option<String>,

    #[serde(rename = "startKey")]
    pub start_key: Option<String>,
    #[serde(rename = "stopKey")]
    pub stop_key: Option<String>,
    #[serde(rename = "holdToTalkKey")]
    pub hold_to_talk_key: Option<String>,

    #[serde(rename = "transcribeModel")]
    pub transcribe_model: Option<String>,
    pub language: Option<String>,

    #[serde(default)]
    pub dictionary: HashMap<String, String>,
    #[serde(default)]
    pub fillers: Vec<String>,
}

/// v1 config.json をパース。
pub fn parse_v1_config(path: &Path) -> Result<V1Config, MigrationError> {
    let raw = fs::read_to_string(path)?;
    let cfg: V1Config = serde_json::from_str(&raw)?;
    Ok(cfg)
}

/// v1 config.json の `openAiApiKey` / `groqApiKey` フィールドを空文字列で上書きする。
/// 他のフィールドは serde_json::Value 経由で原型のまま保持する。
/// (Codex P1 #3: DPAPI クリーンアップ仕様準拠・精読ノート §3.6)
pub fn scrub_v1_secrets(v1_config_path: &Path) -> Result<(), MigrationError> {
    let raw = fs::read_to_string(v1_config_path)?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(map) = value.as_object_mut() {
        if map.contains_key("openAiApiKey") {
            map.insert(
                "openAiApiKey".to_string(),
                serde_json::Value::String(String::new()),
            );
        }
        if map.contains_key("groqApiKey") {
            map.insert(
                "groqApiKey".to_string(),
                serde_json::Value::String(String::new()),
            );
        }
    }
    let pretty = serde_json::to_string_pretty(&value)?;
    fs::write(v1_config_path, pretty)?;
    Ok(())
}

/// 移行レポート: 何件何種を移行したかを呼び出し側に返す。
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    /// 移行された項目の総数 (kv 1 件 + dict 1 件 + filler 1 件 = 3 等)
    pub items_count: u32,
    /// 移行された種別名 (例: "openai_api_key" / "start_key" / "dictionary[12]" / "fillers[7]")
    pub keys_migrated: Vec<String>,
    /// 値が None/空でスキップされた種別名
    pub skipped: Vec<String>,
}

/// v1 → v2 移行の本体。
/// v1 config.json が存在しない、または **既に移行済み**の場合は Ok(None) を返す (移行不要)。
/// 存在しかつ未移行の場合のみ Ok(Some(report)) を返す。
///
/// 既移行判定は v2 SQLite の `migration_log` テーブルに `v1_config_json` レコードが
/// 1 件以上あるかで行う (Codex P1 #2 対策・one-time marker)。
pub fn migrate_v1_to_v2(
    v1_config_path: &Path,
    v2_db_path: &Path,
    v2_secrets_path: &Path,
) -> Result<Option<MigrationReport>, MigrationError> {
    if !v1_config_path.exists() {
        return Ok(None);
    }

    // 既に移行済かをチェック (Codex P1 #2: 毎起動 v1 で上書きを防ぐ)
    // SQLite はここでだけ open する。skip 判定後すぐ閉じても問題ない。
    {
        let db_check = ConfigDb::open(v2_db_path)?;
        if db_check.latest_migration_log()?.is_some() {
            return Ok(None);
        }
    }

    let v1 = parse_v1_config(v1_config_path)?;
    let db = ConfigDb::open(v2_db_path)?;

    let mut report = MigrationReport::default();

    // ---- API キー (DPAPI 暗号化) ----
    let has_groq = v1.groq_api_key.as_deref().filter(|s| !s.is_empty()).is_some();
    let has_openai = v1
        .openai_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .is_some();
    if has_groq || has_openai {
        save_secrets(
            v2_secrets_path,
            v1.groq_api_key.as_deref().filter(|s| !s.is_empty()),
            v1.openai_api_key.as_deref().filter(|s| !s.is_empty()),
            // v1 に gateway JWT は存在しない (feature/gateway-login で追加)
            None,
        )?;
        // Codex P1 #3: 暗号化成功後に v1 config の平文 API キーフィールドを空欄化する
        // (精読ノート §3.6 / Encrypt-ApiKey.ps1 L100-121 と同じ動作)
        scrub_v1_secrets(v1_config_path)?;

        if has_groq {
            report.items_count += 1;
            report.keys_migrated.push("groq_api_key".to_string());
        } else {
            report.skipped.push("groq_api_key".to_string());
        }
        if has_openai {
            report.items_count += 1;
            report.keys_migrated.push("openai_api_key".to_string());
        } else {
            report.skipped.push("openai_api_key".to_string());
        }
    } else {
        report.skipped.push("groq_api_key".to_string());
        report.skipped.push("openai_api_key".to_string());
    }

    // ---- hotkey / モデル名 (settings_kv) ----
    for (key, value_opt) in [
        ("start_key", v1.start_key.as_deref()),
        ("stop_key", v1.stop_key.as_deref()),
        ("hold_to_talk_key", v1.hold_to_talk_key.as_deref()),
        ("transcribe_model", v1.transcribe_model.as_deref()),
        ("language", v1.language.as_deref()),
    ] {
        match value_opt.filter(|s| !s.is_empty()) {
            Some(v) => {
                db.set_kv(key, v)?;
                report.items_count += 1;
                report.keys_migrated.push(key.to_string());
            }
            None => {
                report.skipped.push(key.to_string());
            }
        }
    }

    // ---- dictionary ----
    if !v1.dictionary.is_empty() {
        db.upsert_dictionary(&v1.dictionary)?;
        report.items_count += v1.dictionary.len() as u32;
        report
            .keys_migrated
            .push(format!("dictionary[{}]", v1.dictionary.len()));
    } else {
        report.skipped.push("dictionary".to_string());
    }

    // ---- fillers ----
    if !v1.fillers.is_empty() {
        db.replace_fillers(&v1.fillers)?;
        report.items_count += v1.fillers.len() as u32;
        report
            .keys_migrated
            .push(format!("fillers[{}]", v1.fillers.len()));
    } else {
        report.skipped.push("fillers".to_string());
    }

    // ---- ログ ----
    db.insert_migration_log("v1_config_json", report.items_count)?;

    Ok(Some(report))
}

/// Day 25 Phase 2-G (SSOT v3.3): v0.1.6 → v0.1.7 マイグレーション。
///
/// v0.1.7 で ASR 補正レイヤー (`alias_db`) を完全削除したため、
/// 既存ユーザー (パイロット利用者) の `%APPDATA%\RushVoice\alias.db`
/// + WAL/SHM ファイルを起動時に自動削除する。
///
/// 動作:
///   - alias.db / alias.db-wal / alias.db-shm が存在すれば削除 (best-effort)
///   - 既に削除済 / 元から存在しない場合は no-op
///   - 削除失敗 (権限・ロック等) は warn ログのみ・アプリ起動は継続
///
/// 戻り値: 削除に成功したファイル数 (0 = 何もしなかった = 既に v0.1.7 環境)
pub fn cleanup_alias_db_v0_1_7(alias_db_path: &Path) -> u32 {
    let mut removed: u32 = 0;
    let candidates = [
        alias_db_path.to_path_buf(),
        alias_db_path.with_extension("db-wal"),
        alias_db_path.with_extension("db-shm"),
    ];
    for path in candidates {
        if path.exists() {
            match fs::remove_file(&path) {
                Ok(_) => {
                    removed += 1;
                }
                Err(e) => {
                    // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)
                    log::warn!(
                        target: "rushvoice.migration",
                        "alias.db cleanup failed (will retry next launch): path={} error={}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }
    removed
}

/// `%APPDATA%/VoiceInputMvp/config.json` のデフォルトパス (v1)。
pub fn default_v1_config_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(app) => PathBuf::from(app)
            .join("VoiceInputMvp")
            .join("config.json"),
        Err(_) => PathBuf::from("VoiceInputMvp").join("config.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_dir(name: &str) -> PathBuf {
        let p = env::temp_dir().join(format!("rushvoice_migration_test_{}", name));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn write_v1(dir: &Path, json: &str) -> PathBuf {
        let p = dir.join("config.json");
        fs::write(&p, json).expect("write v1");
        p
    }

    #[test]
    fn parse_v1_full_minimal_scope() {
        let dir = tmp_dir("parse_full");
        let p = write_v1(
            &dir,
            r#"{
              "openAiApiKey": "sk-test",
              "groqApiKey": "gsk_test",
              "startKey": "RightCtrl",
              "stopKey": "RightCtrl",
              "holdToTalkKey": "RightCtrl",
              "transcribeModel": "gpt-4o-mini-transcribe",
              "language": "ja",
              "dictionary": {"あ": "A", "い": "I"},
              "fillers": ["えー", "あの"],
              "unknownField": "should be ignored"
            }"#,
        );
        let cfg = parse_v1_config(&p).expect("parse");
        assert_eq!(cfg.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.groq_api_key.as_deref(), Some("gsk_test"));
        assert_eq!(cfg.start_key.as_deref(), Some("RightCtrl"));
        assert_eq!(cfg.dictionary.len(), 2);
        assert_eq!(cfg.fillers.len(), 2);
    }

    #[test]
    fn parse_v1_missing_fields_yields_none() {
        let dir = tmp_dir("parse_missing");
        let p = write_v1(&dir, r#"{}"#);
        let cfg = parse_v1_config(&p).expect("parse");
        assert!(cfg.openai_api_key.is_none());
        assert!(cfg.dictionary.is_empty());
        assert!(cfg.fillers.is_empty());
    }

    #[test]
    fn parse_v1_invalid_json_errors() {
        let dir = tmp_dir("parse_invalid");
        let p = write_v1(&dir, r#"not json at all"#);
        let err = parse_v1_config(&p).unwrap_err();
        assert!(matches!(err, MigrationError::Json(_)));
    }

    #[test]
    fn migrate_returns_none_when_v1_absent() {
        let dir = tmp_dir("absent");
        let v1 = dir.join("nonexistent.json");
        let db = dir.join("config.db");
        let secrets = dir.join("config.secrets.encrypted.json");
        let result = migrate_v1_to_v2(&v1, &db, &secrets).expect("migrate");
        assert!(result.is_none());
    }

    #[test]
    fn migrate_full_v1_to_v2() {
        let dir = tmp_dir("full");
        let v1 = write_v1(
            &dir,
            r#"{
              "openAiApiKey": "sk-test_openai",
              "groqApiKey": "gsk_test_groq",
              "startKey": "RightCtrl",
              "transcribeModel": "gpt-4o-mini-transcribe",
              "language": "ja",
              "dictionary": {"アクア": "Aqua", "タイプレス": "Typeless"},
              "fillers": ["えー", "あの", "うーん"]
            }"#,
        );
        let db_path = dir.join("config.db");
        let secrets_path = dir.join("config.secrets.encrypted.json");

        let report = migrate_v1_to_v2(&v1, &db_path, &secrets_path)
            .expect("migrate")
            .expect("Some report");

        // items_count = api 2 + kv 3 + dict 2 + fillers 3 = 10
        assert_eq!(report.items_count, 10);
        assert!(report.keys_migrated.contains(&"groq_api_key".to_string()));
        assert!(report.keys_migrated.contains(&"openai_api_key".to_string()));
        assert!(report.keys_migrated.contains(&"start_key".to_string()));

        // SQLite 側の値検証
        let db = ConfigDb::open(&db_path).expect("open db");
        assert_eq!(db.get_kv("start_key").unwrap().as_deref(), Some("RightCtrl"));
        assert_eq!(db.get_kv("language").unwrap().as_deref(), Some("ja"));
        assert_eq!(db.select_dictionary().unwrap().len(), 2);
        assert_eq!(db.select_fillers().unwrap().len(), 3);
        let (src, _ts, cnt) = db.latest_migration_log().unwrap().expect("log row");
        assert_eq!(src, "v1_config_json");
        assert_eq!(cnt, 10);

        // secrets 側の値検証
        let bundle = crate::crypto::load_secrets(&secrets_path)
            .expect("load secrets")
            .expect("Some bundle");
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_test_groq"));
        assert_eq!(
            bundle.openai_api_key.as_deref(),
            Some("sk-test_openai")
        );
    }

    #[test]
    fn migrate_partial_v1_skips_missing() {
        let dir = tmp_dir("partial");
        // hotkey と dictionary のみ
        let v1 = write_v1(
            &dir,
            r#"{
              "startKey": "RightCtrl",
              "dictionary": {"あ": "A"}
            }"#,
        );
        let db_path = dir.join("config.db");
        let secrets_path = dir.join("config.secrets.encrypted.json");

        let report = migrate_v1_to_v2(&v1, &db_path, &secrets_path)
            .expect("migrate")
            .expect("Some");

        // items_count = kv 1 + dict 1 = 2
        assert_eq!(report.items_count, 2);
        assert!(report.skipped.contains(&"groq_api_key".to_string()));
        assert!(report.skipped.contains(&"openai_api_key".to_string()));
        assert!(report.skipped.contains(&"language".to_string()));
        assert!(report.skipped.contains(&"fillers".to_string()));
        // secrets.encrypted.json は作らない (両キーとも空)
        assert!(!secrets_path.exists());
    }

    #[test]
    fn migrate_scrubs_v1_secrets_after_encryption() {
        let dir = tmp_dir("scrub");
        let v1 = write_v1(
            &dir,
            r#"{
              "openAiApiKey": "sk-secret_xyz",
              "groqApiKey": "gsk_secret_abc",
              "startKey": "RightCtrl",
              "unknownField": "preserved"
            }"#,
        );
        let db_path = dir.join("config.db");
        let secrets_path = dir.join("config.secrets.encrypted.json");

        migrate_v1_to_v2(&v1, &db_path, &secrets_path)
            .expect("migrate")
            .expect("Some");

        // v1 ファイルの API キーフィールドは空欄になっているはず
        let raw = fs::read_to_string(&v1).expect("read v1");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        assert_eq!(v["openAiApiKey"], "");
        assert_eq!(v["groqApiKey"], "");
        // 他フィールドは保持されている
        assert_eq!(v["startKey"], "RightCtrl");
        assert_eq!(v["unknownField"], "preserved");

        // secrets.encrypted.json には元のキーが暗号化済みで入っている
        let bundle = crate::crypto::load_secrets(&secrets_path)
            .expect("load")
            .expect("Some");
        assert_eq!(bundle.openai_api_key.as_deref(), Some("sk-secret_xyz"));
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_secret_abc"));
    }

    #[test]
    fn migrate_already_migrated_returns_none_on_second_call() {
        let dir = tmp_dir("already");
        let v1 = write_v1(
            &dir,
            r#"{
              "startKey": "RightCtrl",
              "language": "ja"
            }"#,
        );
        let db_path = dir.join("config.db");
        let secrets_path = dir.join("config.secrets.encrypted.json");

        // 初回: 移行成功
        let first = migrate_v1_to_v2(&v1, &db_path, &secrets_path)
            .expect("first")
            .expect("Some");
        assert!(first.items_count > 0);

        // v1 値を改ざんしてから再度呼んでも、v2 を上書きしないことを確認
        fs::write(
            &v1,
            r#"{"startKey": "STALE_VALUE", "language": "STALE"}"#,
        )
        .expect("rewrite v1");

        let second = migrate_v1_to_v2(&v1, &db_path, &secrets_path).expect("second");
        assert!(second.is_none(), "二回目は migration_log により None");

        // SQLite の値は初回のもののまま
        let db = ConfigDb::open(&db_path).expect("open");
        assert_eq!(
            db.get_kv("start_key").unwrap().as_deref(),
            Some("RightCtrl")
        );
        assert_eq!(db.get_kv("language").unwrap().as_deref(), Some("ja"));
    }

    #[test]
    fn scrub_v1_secrets_preserves_unknown_fields() {
        let dir = tmp_dir("scrub_preserve");
        let v1 = write_v1(
            &dir,
            r#"{
              "openAiApiKey": "sk-x",
              "groqApiKey": "gsk_x",
              "appProfiles": {"chrome": "casual"},
              "screenContextExcludeProcesses": ["1password", "keepass"]
            }"#,
        );
        scrub_v1_secrets(&v1).expect("scrub");
        let raw = fs::read_to_string(&v1).expect("read");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        assert_eq!(v["openAiApiKey"], "");
        assert_eq!(v["groqApiKey"], "");
        assert_eq!(v["appProfiles"]["chrome"], "casual");
        assert_eq!(v["screenContextExcludeProcesses"][0], "1password");
        assert_eq!(v["screenContextExcludeProcesses"][1], "keepass");
    }
}
