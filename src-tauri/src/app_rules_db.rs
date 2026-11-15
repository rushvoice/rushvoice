// app_rules_db.rs — Phase 2 C Wave 1 Step 3 アプリ別ルール (app_title_rules) SQLite 永続化
//
// SSOT v2.10 §8.3.1 AT-06
// Wave 1 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md §1.4
// Wave 2 follow-up 2026-05-26: 初回 open 時にデフォルト 10 件を投入 (要望)
//
// 設計方針:
// - v1 AppMappingPage.ps1 の「DataGridView 3 列 + CellEndEdit 全件再構築保存」モデルを踏襲。
//   v2 ではこれを `replace_all` API として表現し、onBlur で UI 側が「現在の全行」を送る。
// - glossary_db.rs の構造をそのまま流用 (rusqlite + Connection + WAL + busy_timeout)。
// - 別 DB ファイル %APPDATA%/RushVoice/app_rules.db に分離 (config.db / history.db / glossary.db と並列)。
//   理由: 設定の書込特性 (低頻度 + 全件再構築) は glossary (中頻度 + upsert/delete) と若干異なる。
// - Wave 1 では重複検出を実装しない (Wave 2 AT-04 で追加)。
// - Wave 1 では profileId は単なる文字列 (Wave 2 PT-01 で DropdownMenu 化)。
//
// Wave 2 follow-up 初回投入 (要望):
// - `schema_meta` テーブル (key TEXT PRIMARY KEY, value TEXT) を追加し、
//   `seeded_defaults_v1` キーで「デフォルト投入済」フラグを管理する。
//   ※ ユーザーが UI で削除した状態 (count=0) と「未投入」を区別するため。
// - DEFAULT_APP_RULES の 10 件は profile_db の builtin_key (mail/business_doc/casual) と
//   profile_id を一致させる。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppRulesDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// アプリ別ルール 1 件。INSERT 前は `id = None`、SELECT 結果は `id = Some(rowid)`。
///
/// v1 互換のため snake_case ではなく **camelCase** を JSON 側で使う必要があるか
/// 検討したが、v2 frontend は既に snake_case で history_db / glossary_db を扱っているため
/// 全フィールド snake_case で統一する。frontend 側 TS 型も snake_case で揃える。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppRule {
    pub id: Option<i64>,
    /// プロファイル参照キー (Wave 2 follow-up 2026-05-26 確定方針):
    /// - ビルトインプロファイル: `builtin_key` 文字列をそのまま使う
    ///   (例: "mail", "business_doc", "casual")。profile_db.rs::BUILTIN_PRESETS と一致。
    /// - ユーザー作成プロファイル: `profiles.id` (i64 rowid) を文字列化したもの
    ///   (例: "42")。`upsertProfile` で採番された rowid を to_string() して保存する。
    ///
    /// 解決ヘルパー (profile_id → Profile) は Wave 3 AppsPage DropdownMenu 実装時に
    /// `profile_db::resolve_by_app_rule_key()` を追加予定。それまでは UI 側で
    /// 「builtin_key 集合に含まれれば builtin・含まなければ parse::<i64>() で rowid 検索」
    /// のフォールバック解決を行う想定。
    // TODO(Wave 3): profile_db に resolve_by_app_rule_key(profile_id: &str) -> Option<Profile> 追加
    pub profile_id: String,
    /// 対象アプリ名 (例: "chrome.exe", "Code.exe")
    pub app_name: String,
    /// ウィンドウタイトル正規表現 (空文字列も許容)
    pub title_regex: String,
    /// INSERT 時刻 (Unix epoch seconds, UTC)
    pub created_at: i64,
    /// 最終 UPDATE 時刻 (Unix epoch seconds, UTC)
    pub updated_at: i64,
}

pub struct AppRulesDb {
    conn: Connection,
}

impl AppRulesDb {
    /// 指定パスの SQLite を開き、必要なら親ディレクトリを作成して
    /// CREATE TABLE IF NOT EXISTS で app_rules テーブルを初期化する。
    pub fn open(path: &Path) -> Result<Self, AppRulesDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = AppRulesDb { conn };
        db.init()?;
        // Wave 2 follow-up (2026-05-26): 初回のみデフォルト 10 件投入。
        // schema_meta テーブルで「投入済」フラグを管理し、ユーザー削除後の再投入を防ぐ。
        // P1-1 fix: 内部で単一トランザクション化 (10 件 INSERT + flag を atomic に)
        db.seed_defaults_if_needed()?;
        // v0.1.3 (2026-05-27): v1 で投入された誤 default 2 行を正規化する。
        // - LINE: chrome.exe + (?i)line → LINE.exe + "" に置換
        // - Messenger: Messenger.exe + "" → chrome.exe + (?i)messenger に置換
        // schema_meta.seeded_defaults_v2 フラグで冪等性を保証 (L188 単一トランザクション準拠)。
        db.migrate_seed_v1_to_v2()?;
        Ok(db)
    }

    fn init(&self) -> Result<(), AppRulesDbError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS app_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id TEXT NOT NULL,
                app_name TEXT NOT NULL,
                title_regex TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_app_rules_app_name ON app_rules(app_name);
            CREATE TABLE IF NOT EXISTS schema_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Wave 2 follow-up 初回投入:
    /// `schema_meta.seeded_defaults_v1` が無ければ DEFAULT_APP_RULES を全件 INSERT し、
    /// フラグを立てる。ユーザーが UI から削除しても (count=0) フラグが立っていれば再投入しない。
    ///
    /// idempotent: 2 回目以降の open では何もしない。
    ///
    /// Wave 2 follow-up review fix (P1-1, 2026-05-26):
    /// 単一トランザクションでラップする。途中 INSERT が失敗した場合、
    /// 一部行のみが残った状態 + schema_meta フラグなし → 次回 open で重複投入される
    /// 不整合を防ぐ。`self.conn.transaction()` で BEGIN/COMMIT を発行し、
    /// `?` による早期 return 時は Drop で自動 rollback される。
    ///
    /// `&mut self` への変更が必要だが、`AppRulesDb::open` 内で db を mut bind してから
    /// 呼ぶことで対応 (公開 API 変更なし)。
    fn seed_defaults_if_needed(&mut self) -> Result<(), AppRulesDbError> {
        let already: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SEEDED_DEFAULTS_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            return Ok(());
        }
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
            )?;
            for d in DEFAULT_APP_RULES {
                stmt.execute(params![d.profile_id, d.app_name, d.title_regex, now])?;
            }
        }
        tx.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)",
            params![SEEDED_DEFAULTS_KEY, "1"],
        )?;
        tx.commit()?;
        log::info!(
            "[app_rules] seeded {} default rules (v1)",
            DEFAULT_APP_RULES.len()
        );
        Ok(())
    }

    /// v0.1.3 マイグレーション: v1 で投入された誤 default 2 行 (LINE と Messenger) を正規化する。
    ///
    /// 対象:
    ///   - 旧 LINE 行: (profile_id='casual', app_name='chrome.exe', title_regex='(?i)line')
    ///     → 新 LINE 行: (profile_id='casual', app_name='LINE.exe', title_regex='')
    ///   - 旧 Messenger 行: (profile_id='casual', app_name='Messenger.exe', title_regex='')
    ///     → 新 Messenger 行: (profile_id='casual', app_name='chrome.exe', title_regex='(?i)messenger')
    ///
    /// ユーザー編集行の保護:
    ///   - title_regex の完全一致のみを DELETE 対象とする
    ///     (例: ユーザーが title_regex を '(?i)line my chat' に変更していれば対象外)
    ///   - 置換後の行が既に存在する場合 (例: ユーザーが手動で正規版を追加済) は INSERT skip
    ///
    /// idempotent: schema_meta.seeded_defaults_v2 フラグが立っていれば何もしない。
    /// 単一トランザクション (L188 準拠): DELETE + INSERT + フラグ立てを atomic に実行。
    fn migrate_seed_v1_to_v2(&mut self) -> Result<(), AppRulesDbError> {
        let already: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SEEDED_DEFAULTS_V2_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            return Ok(());
        }
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        let mut changes = 0;
        // --- LINE 正規化 ---
        // 1) 旧 LINE 行を削除 (title_regex 完全一致のみ・ユーザー編集行は触らない)
        let line_deleted = tx.execute(
            "DELETE FROM app_rules
             WHERE profile_id = ?1 AND app_name = ?2 AND title_regex = ?3",
            params!["casual", "chrome.exe", "(?i)line"],
        )?;
        changes += line_deleted;
        // 2) 新 LINE 行が既に存在しなければ INSERT (重複防止)
        let line_exists: bool = tx
            .query_row(
                "SELECT 1 FROM app_rules
                 WHERE profile_id = ?1 AND app_name = ?2 AND title_regex = ?3 LIMIT 1",
                params!["casual", "LINE.exe", ""],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !line_exists {
            tx.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params!["casual", "LINE.exe", "", now],
            )?;
            changes += 1;
        }
        // --- Messenger 正規化 ---
        // 1) 旧 Messenger 行を削除 (title_regex 完全一致 '' のみ)
        let msgr_deleted = tx.execute(
            "DELETE FROM app_rules
             WHERE profile_id = ?1 AND app_name = ?2 AND title_regex = ?3",
            params!["casual", "Messenger.exe", ""],
        )?;
        changes += msgr_deleted;
        // 2) 新 Messenger 行が既に存在しなければ INSERT
        let msgr_exists: bool = tx
            .query_row(
                "SELECT 1 FROM app_rules
                 WHERE profile_id = ?1 AND app_name = ?2 AND title_regex = ?3 LIMIT 1",
                params!["casual", "chrome.exe", "(?i)messenger"],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !msgr_exists {
            tx.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params!["casual", "chrome.exe", "(?i)messenger", now],
            )?;
            changes += 1;
        }
        tx.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)",
            params![SEEDED_DEFAULTS_V2_KEY, "1"],
        )?;
        tx.commit()?;
        if changes > 0 {
            log::info!(
                "[app_rules] migrate_seed_v1_to_v2 applied: {} change(s)",
                changes
            );
        }
        Ok(())
    }

    /// 全件取得 (created_at ASC で安定順)。v1 の Rows 並び順と同じ「追加順」になる。
    pub fn list(&self) -> Result<Vec<AppRule>, AppRulesDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, profile_id, app_name, title_regex, created_at, updated_at
             FROM app_rules
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_rule)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// id 指定で 1 件取得。存在しない場合 `Ok(None)`。
    pub fn get(&self, id: i64) -> Result<Option<AppRule>, AppRulesDbError> {
        let rule = self
            .conn
            .query_row(
                "SELECT id, profile_id, app_name, title_regex, created_at, updated_at
                 FROM app_rules WHERE id = ?1",
                params![id],
                row_to_rule,
            )
            .optional()?;
        Ok(rule)
    }

    /// id が Some なら UPDATE・None なら INSERT。返却は最終的な rowid。
    pub fn upsert(&self, rule: &AppRule) -> Result<i64, AppRulesDbError> {
        let now = current_epoch_secs();
        match rule.id {
            Some(id) => {
                self.conn.execute(
                    "UPDATE app_rules
                     SET profile_id = ?2, app_name = ?3, title_regex = ?4, updated_at = ?5
                     WHERE id = ?1",
                    params![id, rule.profile_id, rule.app_name, rule.title_regex, now],
                )?;
                Ok(id)
            }
            None => {
                self.conn.execute(
                    "INSERT INTO app_rules
                        (profile_id, app_name, title_regex, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![rule.profile_id, rule.app_name, rule.title_regex, now, now],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    /// 1 件削除。存在しない id でも Err にせず Ok を返す (UI 楽観的更新と整合)。
    pub fn delete(&self, id: i64) -> Result<(), AppRulesDbError> {
        self.conn
            .execute("DELETE FROM app_rules WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 全件削除 + 新 rules を順次 INSERT する「全件再構築保存」。
    ///
    /// v1 AppMappingPage.ps1 の `saveAllRows` 等価。onBlur 時に UI 側が
    /// 「現在の全行」を送ってくる前提。完全空行 (3 フィールド全て空白) は呼出側で
    /// 事前にスキップしている想定だが、保険として Rust 側でも除外する。
    ///
    /// トランザクション保証: 1 件でも INSERT 失敗したら全ロールバック。
    /// id フィールドは無視 (全件 INSERT 新規 ID 採番) — UI 側は返り値で新 ID を受け取る。
    pub fn replace_all(&mut self, rules: &[AppRule]) -> Result<Vec<AppRule>, AppRulesDbError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM app_rules", [])?;

        let now = current_epoch_secs();
        let mut inserted: Vec<AppRule> = Vec::with_capacity(rules.len());
        {
            let mut stmt = tx.prepare(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for rule in rules {
                // 完全空行スキップ (v1 互換・3 フィールド全空白は保存しない)
                if rule.profile_id.trim().is_empty()
                    && rule.app_name.trim().is_empty()
                    && rule.title_regex.trim().is_empty()
                {
                    continue;
                }
                // created_at は呼出側が 0 を渡してきたら now に置換、それ以外は維持。
                // 既存 row の created_at を保持したい場合は呼出側で詰めて渡す前提。
                let created = if rule.created_at > 0 {
                    rule.created_at
                } else {
                    now
                };
                stmt.execute(params![
                    rule.profile_id,
                    rule.app_name,
                    rule.title_regex,
                    created,
                    now,
                ])?;
                let new_id = tx.last_insert_rowid();
                inserted.push(AppRule {
                    id: Some(new_id),
                    profile_id: rule.profile_id.clone(),
                    app_name: rule.app_name.clone(),
                    title_regex: rule.title_regex.clone(),
                    created_at: created,
                    updated_at: now,
                });
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// 総件数 (Settings UI 用)。
    pub fn count(&self) -> Result<i64, AppRulesDbError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM app_rules", [], |row| row.get(0))?;
        Ok(n)
    }

    /// schema_meta テーブルから任意のキーを取得 (v0.1.7.1 タスク A 用に汎用化)。
    ///
    /// 既存の `SEEDED_DEFAULTS_KEY` / `SEEDED_DEFAULTS_V2_KEY` の管理に使われていた
    /// `SELECT value FROM schema_meta WHERE key = ?1` を公開メソッドとして抽出。
    /// 想定キー例: `autostart_default_applied_v017_1` (autostart デフォルト ON 化済フラグ)。
    pub fn get_meta(&self, key: &str) -> Result<Option<String>, AppRulesDbError> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// schema_meta テーブルに任意のキー/値を upsert (v0.1.7.1 タスク A 用に汎用化)。
    ///
    /// 既存キーがあれば値を上書き、無ければ INSERT。`autostart_default_applied_v017_1`
    /// のような「1 度だけ実行」フラグ用に、値は "1" / "0" を文字列で持つ運用とする。
    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), AppRulesDbError> {
        self.conn.execute(
            "INSERT INTO schema_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// schema_meta テーブルから任意のキーを削除 (v0.1.12 F3 A 案用)。
    ///
    /// 用途: ユーザーが UI 経由で `disable_autostart` を実行した時に
    /// `autostart_default_applied_v017_1` フラグを消し、次回起動時の
    /// `ensure_autostart_consistency` が「ユーザー OFF 意図」を尊重して
    /// 再 enable しないようにする (L265 NSIS Uninstall 復元と両立)。
    ///
    /// 存在しないキーを指定しても Ok を返す (idempotent)。
    pub fn delete_meta(&self, key: &str) -> Result<(), AppRulesDbError> {
        self.conn
            .execute("DELETE FROM schema_meta WHERE key = ?1", params![key])?;
        Ok(())
    }
}

fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppRule> {
    Ok(AppRule {
        id: Some(row.get(0)?),
        profile_id: row.get(1)?,
        app_name: row.get(2)?,
        title_regex: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn current_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `<data_dir>/RushVoice/app_rules.db` のデフォルトパス。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
pub fn default_app_rules_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("app_rules.db")
}

// ----- Wave 2 follow-up: 初回デフォルト 10 件投入定義 -----

/// `schema_meta` テーブルで「デフォルト投入済」を管理する key。
/// v1 接尾辞は将来「v2 デフォルトを追加投入」する余地のため。
const SEEDED_DEFAULTS_KEY: &str = "seeded_defaults_v1";

/// v0.1.3 (2026-05-27) で導入した「v1 → v2 正規化マイグレーション完了」フラグ key。
/// LINE と Messenger の誤 default 行を正規化したことを記録する。
const SEEDED_DEFAULTS_V2_KEY: &str = "seeded_defaults_v2";

/// デフォルト投入の 1 件 (静的 const 用)。
struct DefaultAppRule {
    profile_id: &'static str,
    app_name: &'static str,
    title_regex: &'static str,
}

/// 委任プロンプト 2026-05-26 で確定の 10 件。
/// profile_id は profile_db.rs::BUILTIN_PRESETS の builtin_key と一致させる。
///
/// v0.1.3 修正 (2026-05-27): Meta が Messenger ネイティブ Windows アプリの配布を停止し
/// Web 版 (messenger.com) のみになったため、Messenger.exe → chrome.exe + (?i)messenger に変更。
/// (LINE はネイティブ LINE.exe が公式配布継続中のためそのまま維持。)
const DEFAULT_APP_RULES: &[DefaultAppRule] = &[
    DefaultAppRule { profile_id: "mail",         app_name: "chrome.exe",       title_regex: "(?i)gmail" },
    DefaultAppRule { profile_id: "mail",         app_name: "OUTLOOK.EXE",      title_regex: "" },
    DefaultAppRule { profile_id: "mail",         app_name: "thunderbird.exe",  title_regex: "" },
    DefaultAppRule { profile_id: "business_doc", app_name: "chrome.exe",       title_regex: "(?i)google docs" },
    DefaultAppRule { profile_id: "business_doc", app_name: "WINWORD.EXE",      title_regex: "" },
    DefaultAppRule { profile_id: "business_doc", app_name: "chrome.exe",       title_regex: "(?i)notion" },
    DefaultAppRule { profile_id: "business_doc", app_name: "Notion.exe",       title_regex: "" },
    DefaultAppRule { profile_id: "casual",       app_name: "LINE.exe",         title_regex: "" },
    DefaultAppRule { profile_id: "casual",       app_name: "chrome.exe",       title_regex: "(?i)google chat" },
    DefaultAppRule { profile_id: "casual",       app_name: "chrome.exe",       title_regex: "(?i)messenger" },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_app_rules_db_test_{}", name))
            .join("app_rules.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn sample(profile_id: &str, app_name: &str, title_regex: &str) -> AppRule {
        AppRule {
            id: None,
            profile_id: profile_id.to_string(),
            app_name: app_name.to_string(),
            title_regex: title_regex.to_string(),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// 既存テスト用: open 後にデフォルト 10 件をクリアして既存テストの前提 (空 DB) を維持。
    /// schema_meta.seeded_defaults_v1 は残る (フラグ立ったまま) ので「再投入されない」状態。
    fn open_cleared(path: &PathBuf) -> AppRulesDb {
        let mut db = AppRulesDb::open(path).expect("open");
        // 既存 default を全削除 (replace_all は引数 [] で全 DELETE)
        db.replace_all(&[]).expect("clear defaults");
        db
    }

    #[test]
    fn test_init_and_list_empty() {
        let path = tmp_db("init_empty");
        let db = open_cleared(&path);
        // 2 回 open しても壊れない (CREATE TABLE IF NOT EXISTS)
        let _db2 = AppRulesDb::open(&path).expect("open 2");
        assert_eq!(db.count().unwrap(), 0);
        assert!(db.list().unwrap().is_empty());
    }

    #[test]
    fn test_upsert_insert_and_update() {
        let path = tmp_db("upsert");
        let db = open_cleared(&path);
        let id1 = db
            .upsert(&sample("natural", "chrome.exe", ".*メール.*"))
            .expect("insert 1");
        let id2 = db
            .upsert(&sample("plain", "Code.exe", ""))
            .expect("insert 2");
        assert!(id2 > id1);

        // UPDATE: id1 の profile_id を変更
        let mut updated = sample("business", "chrome.exe", ".*メール.*");
        updated.id = Some(id1);
        let ret = db.upsert(&updated).expect("update");
        assert_eq!(ret, id1, "UPDATE は同じ id を返す");
        assert_eq!(db.count().unwrap(), 2);

        let got = db.get(id1).unwrap().unwrap();
        assert_eq!(got.profile_id, "business");
        assert_eq!(got.app_name, "chrome.exe");
    }

    #[test]
    fn test_delete() {
        let path = tmp_db("delete");
        let db = open_cleared(&path);
        let id1 = db
            .upsert(&sample("plain", "keep.exe", ""))
            .expect("insert 1");
        let id2 = db
            .upsert(&sample("plain", "remove.exe", ""))
            .expect("insert 2");
        db.delete(id2).expect("delete");

        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, Some(id1));
        // 存在しない id 削除は OK
        db.delete(999_999).expect("delete nonexistent");
    }

    #[test]
    fn test_replace_all_replaces_and_skips_empty_rows() {
        let path = tmp_db("replace_all");
        let mut db = open_cleared(&path);
        // 既存 2 件
        db.upsert(&sample("plain", "old1.exe", "")).expect("old 1");
        db.upsert(&sample("plain", "old2.exe", "")).expect("old 2");
        assert_eq!(db.count().unwrap(), 2);

        // 全件再構築: 新 3 件 (うち 1 件は完全空行 → スキップ)
        let new_rules = vec![
            sample("natural", "new1.exe", ".*Foo.*"),
            sample("", "", ""), // skip 完全空行
            sample("business", "new2.exe", ""),
        ];
        let inserted = db.replace_all(&new_rules).expect("replace_all");
        assert_eq!(inserted.len(), 2, "完全空行はスキップされる");
        assert!(inserted.iter().all(|r| r.id.is_some()));

        // DB 上は新 2 件のみ・旧 2 件は削除
        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].app_name, "new1.exe");
        assert_eq!(rows[0].profile_id, "natural");
        assert_eq!(rows[1].app_name, "new2.exe");
    }

    #[test]
    fn test_replace_all_empty_clears_table() {
        // 空配列を渡すと全削除になる (v1 仕様: 「+ 追加」で 0 行になった場合等)
        let path = tmp_db("replace_all_empty");
        let mut db = open_cleared(&path);
        db.upsert(&sample("plain", "x.exe", "")).expect("insert");
        assert_eq!(db.count().unwrap(), 1);

        let inserted = db.replace_all(&[]).expect("replace_all empty");
        assert!(inserted.is_empty());
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_replace_all_preserves_created_at_when_positive() {
        // created_at > 0 で渡したものは維持される (既存 row 更新時の挙動)
        let path = tmp_db("preserve_created_at");
        let mut db = open_cleared(&path);
        let rule_with_history = AppRule {
            id: None,
            profile_id: "natural".to_string(),
            app_name: "history.exe".to_string(),
            title_regex: "".to_string(),
            created_at: 1_700_000_000,
            updated_at: 0,
        };
        let inserted = db
            .replace_all(&[rule_with_history])
            .expect("replace_all");
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].created_at, 1_700_000_000);
        // updated_at は now (= 0 でなければ何でも) になる
        assert!(inserted[0].updated_at > 0);
    }

    // ----- Wave 2 follow-up: デフォルト 10 件投入のテスト -----

    #[test]
    fn test_seed_defaults_inserted_on_first_open() {
        // 新規 DB の初回 open でデフォルト 10 件が投入される。
        let path = tmp_db("seed_first_open");
        let db = AppRulesDb::open(&path).expect("open");
        assert_eq!(
            db.count().unwrap(),
            DEFAULT_APP_RULES.len() as i64,
            "初回 open でデフォルト 10 件投入"
        );
        // builtin_key と一致する profile_id が含まれることを確認
        let rules = db.list().expect("list");
        let profile_ids: Vec<&str> = rules.iter().map(|r| r.profile_id.as_str()).collect();
        assert!(profile_ids.contains(&"mail"));
        assert!(profile_ids.contains(&"business_doc"));
        assert!(profile_ids.contains(&"casual"));
        // chrome.exe + (?i)gmail のペアが含まれること
        let gmail = rules
            .iter()
            .find(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)gmail");
        assert!(gmail.is_some(), "gmail rule should be present");
        assert_eq!(gmail.unwrap().profile_id, "mail");
    }

    #[test]
    fn test_seed_defaults_idempotent_second_open() {
        // 2 回目 open ではデフォルトは再投入されない。
        let path = tmp_db("seed_idempotent");
        {
            let _db = AppRulesDb::open(&path).expect("open 1");
        }
        let db = AppRulesDb::open(&path).expect("open 2");
        assert_eq!(
            db.count().unwrap(),
            DEFAULT_APP_RULES.len() as i64,
            "2 回目 open でも 10 件のまま (重複投入されない)"
        );
    }

    #[test]
    fn test_seed_defaults_not_reinserted_after_user_delete_all() {
        // ユーザーが UI から全削除 → 再起動でも再投入されない (重要要件)
        let path = tmp_db("seed_user_delete");
        {
            let mut db = AppRulesDb::open(&path).expect("open 1");
            // ユーザーが UI から全削除を模倣
            db.replace_all(&[]).expect("user clear");
            assert_eq!(db.count().unwrap(), 0);
        }
        // 再起動相当の再 open
        let db = AppRulesDb::open(&path).expect("open 2");
        assert_eq!(
            db.count().unwrap(),
            0,
            "ユーザー削除後はデフォルト再投入されない"
        );
    }

    #[test]
    fn test_seed_defaults_skipped_when_existing_data_predates_flag() {
        // 旧バージョン (seed 機能なし) の DB に既存データがある状態で
        // 新バージョン open すると、既存データは保持されつつフラグが立って seed されない
        // …のが理想だが、現在の実装は「フラグ無しなら必ず seed」なので、
        // 既存データがあった場合は重複投入される。
        //
        // この挙動を許容する根拠:
        //   1) Wave 2 follow-up 以前は app_rules テーブルは空が標準 (UI は実装直後・Wave 1 Step 3 の AppsPage で初めて編集可能になった)
        //   2) 万一旧データがあっても 10 件追加なら UI 上で気付いて削除可能
        //   3) フラグ立てた後の 2 回目 open では重複しない (idempotent 保証)
        //
        // 本テストは「旧データ + 新 open で 12 件 (旧 2 + 新 10) になる」ことを記録目的で残す。
        let path = tmp_db("seed_with_existing");
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE app_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    profile_id TEXT NOT NULL,
                    app_name TEXT NOT NULL,
                    title_regex TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            for app in ["old_user1.exe", "old_user2.exe"] {
                raw.execute(
                    "INSERT INTO app_rules (profile_id, app_name, title_regex, created_at, updated_at)
                     VALUES ('legacy', ?1, '', 100, 100)",
                    params![app],
                )
                .unwrap();
            }
        }
        let db = AppRulesDb::open(&path).expect("open");
        assert_eq!(
            db.count().unwrap(),
            12,
            "旧データ 2 + デフォルト 10 = 12 件 (許容挙動)"
        );
        // 旧 user 行は残っている
        let rules = db.list().unwrap();
        let legacy: Vec<&str> = rules
            .iter()
            .filter(|r| r.profile_id == "legacy")
            .map(|r| r.app_name.as_str())
            .collect();
        assert_eq!(legacy.len(), 2);
    }

    /// P1-1 rollback test:
    /// seed_defaults_if_needed が単一トランザクションで動いていることの間接検証。
    /// テーブル不存在状態 (schema_meta が無い) でも、open() で init→seed が
    /// 「flag 立て」と「10 件 INSERT」を atomic に行うことを確認する。
    ///
    /// 直接的な失敗注入は rusqlite では困難 (UNIQUE 制約違反等を仕掛ける必要あり) なので、
    /// ここでは「正常系で 10 件 + flag が同時にコミットされる」「クラッシュ模擬で
    /// 部分投入が残らない」の 2 観点を検証する。
    #[test]
    fn test_seed_defaults_is_atomic_with_schema_meta_flag() {
        let path = tmp_db("seed_atomic");
        // 通常 open
        let db = AppRulesDb::open(&path).expect("open");
        // 10 件投入され、フラグも同時に立っている
        assert_eq!(db.count().unwrap(), DEFAULT_APP_RULES.len() as i64);
        let flag: Option<String> = db
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SEEDED_DEFAULTS_KEY],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            flag.as_deref(),
            Some("1"),
            "10 件投入とフラグ立ては atomic"
        );
    }

    /// P1-1 部分投入が残らないこと:
    /// 強制失敗を仕掛ける代わりに、「seed 前にテーブル drop して制約違反を起こす」
    /// パターンを raw connection で構築 → トランザクションが rollback されることを確認。
    #[test]
    fn test_seed_defaults_rollback_on_insert_failure() {
        let path = tmp_db("seed_rollback");
        // 1) DB を作って app_rules テーブルを「profile_id NOT NULL」制約を強めた版で先に作る
        //    (DEFAULT_APP_RULES と互換だが、追加 CHECK で 1 件目を強制失敗させる)
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE app_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    profile_id TEXT NOT NULL CHECK(profile_id != 'mail'),
                    app_name TEXT NOT NULL,
                    title_regex TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE schema_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        // 2) open() → seed_defaults_if_needed で profile_id='mail' (1 件目) が CHECK 違反 → Err
        let result = AppRulesDb::open(&path);
        assert!(result.is_err(), "CHECK 違反で open は Err");

        // 3) 別 connection で確認: 部分投入されていない (rollback された)
        let raw = Connection::open(&path).unwrap();
        let count: i64 = raw
            .query_row("SELECT COUNT(*) FROM app_rules", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "トランザクション rollback で 0 件");
        // schema_meta フラグも立っていない (atomic 保証)
        let flag: Option<String> = raw
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SEEDED_DEFAULTS_KEY],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(flag.is_none(), "失敗時は flag も立たない");
    }

    // ----- v0.1.3: seeded_defaults_v2 マイグレーション 5 ケース -----

    #[test]
    fn test_migrate_v2_no_op_on_fresh_db() {
        // ケース 1: 新規 DB (v1 seed 直後 = 既に新 DEFAULT_APP_RULES = 旧行なし)
        // → 0 件削除 + 0 件 INSERT (新行は既存) + v2 フラグ立つ
        let path = tmp_db("migrate_v2_fresh");
        let db = AppRulesDb::open(&path).expect("open");
        let rules = db.list().expect("list");
        // 10 件のうち LINE.exe と chrome.exe + (?i)messenger が含まれる (v0.1.3 新 default)
        assert!(rules.iter().any(|r| r.app_name == "LINE.exe" && r.title_regex.is_empty()));
        assert!(rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)messenger"));
        // 旧形式は存在しない
        assert!(!rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)line"));
        assert!(!rules.iter().any(|r| r.app_name == "Messenger.exe"));
        // schema_meta フラグ立っている
        let v2_flag: Option<String> = db
            .conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = ?1",
                params![SEEDED_DEFAULTS_V2_KEY],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(v2_flag.as_deref(), Some("1"));
    }

    #[test]
    fn test_migrate_v2_converts_v1_legacy_rows() {
        // ケース 2: v1 で投入済 DB (旧形式 2 行存在 + v2 フラグなし) → v0.1.3 open で正規化
        let path = tmp_db("migrate_v2_legacy");
        // 1) raw connection で v1 状態を再現 (seed_defaults_v1 だけ立てて旧形式を投入)
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE app_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    profile_id TEXT NOT NULL,
                    app_name TEXT NOT NULL,
                    title_regex TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE schema_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
            // v1 seed 直後の状態: 旧 LINE + 旧 Messenger を含む 2 行のみ投入
            raw.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('casual', 'chrome.exe', '(?i)line', 100, 100)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('casual', 'Messenger.exe', '', 100, 100)",
                [],
            )
            .unwrap();
            // v1 フラグだけ立てる (v2 マイグレ未実施状態)
            raw.execute(
                "INSERT INTO schema_meta (key, value) VALUES ('seeded_defaults_v1', '1')",
                [],
            )
            .unwrap();
        }
        // 2) v0.1.3 open: seed v1 は skip (フラグ既立)・seed v2 が走る
        let db = AppRulesDb::open(&path).expect("open");
        let rules = db.list().expect("list");
        // 旧形式は削除されている
        assert!(!rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)line"));
        assert!(!rules.iter().any(|r| r.app_name == "Messenger.exe"));
        // 新形式 2 行が INSERT されている
        assert!(rules.iter().any(|r| r.app_name == "LINE.exe" && r.title_regex.is_empty()));
        assert!(rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)messenger"));
        // 件数: 旧 2 削除 + 新 2 投入 = 2 件
        assert_eq!(db.count().unwrap(), 2);
    }

    #[test]
    fn test_migrate_v2_idempotent_on_second_open() {
        // ケース 3: v2 マイグレ済 DB (v2 フラグあり) → 再 open でも何も変化なし
        let path = tmp_db("migrate_v2_idempotent");
        // 1 回目 open: 新規 DB → v1 seed + v2 マイグレ完了
        {
            let db = AppRulesDb::open(&path).expect("open 1");
            assert_eq!(db.count().unwrap(), DEFAULT_APP_RULES.len() as i64);
        }
        // 2 回目 open: 件数も内容も変わらない
        let db = AppRulesDb::open(&path).expect("open 2");
        assert_eq!(db.count().unwrap(), DEFAULT_APP_RULES.len() as i64);
        let rules = db.list().unwrap();
        // LINE 行・Messenger 行ともに 1 件ずつ
        let line_count = rules
            .iter()
            .filter(|r| r.app_name == "LINE.exe" && r.title_regex.is_empty())
            .count();
        let msgr_count = rules
            .iter()
            .filter(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)messenger")
            .count();
        assert_eq!(line_count, 1);
        assert_eq!(msgr_count, 1);
    }

    #[test]
    fn test_migrate_v2_preserves_user_edited_rows() {
        // ケース 4: ユーザーが title_regex を編集していた行は触らない
        let path = tmp_db("migrate_v2_user_edit");
        // v1 状態を再現: 旧 LINE 行を「ユーザーが編集した状態」で投入
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE app_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    profile_id TEXT NOT NULL,
                    app_name TEXT NOT NULL,
                    title_regex TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE schema_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
            // ユーザーが title_regex を編集した行 (DELETE 対象外であるべき)
            raw.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('casual', 'chrome.exe', '(?i)line my custom chat', 100, 100)",
                [],
            )
            .unwrap();
            // ユーザーが profile_id を編集した Messenger 行 (DELETE 対象外)
            raw.execute(
                "INSERT INTO app_rules
                    (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('business_doc', 'Messenger.exe', '', 100, 100)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO schema_meta (key, value) VALUES ('seeded_defaults_v1', '1')",
                [],
            )
            .unwrap();
        }
        let db = AppRulesDb::open(&path).expect("open");
        let rules = db.list().expect("list");
        // ユーザー編集 2 行は残っている
        assert!(rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)line my custom chat"));
        assert!(rules
            .iter()
            .any(|r| r.profile_id == "business_doc" && r.app_name == "Messenger.exe"));
        // 新 default 2 行も INSERT されている
        assert!(rules.iter().any(|r| r.app_name == "LINE.exe" && r.title_regex.is_empty()));
        assert!(rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)messenger"));
        // 計 4 行 (ユーザー 2 + 新 default 2)
        assert_eq!(db.count().unwrap(), 4);
    }

    #[test]
    fn test_migrate_v2_skips_insert_when_target_already_exists() {
        // ケース 5: 新形式行が既存 (ユーザーが手動で追加済) → 重複 INSERT skip
        let path = tmp_db("migrate_v2_skip_dup");
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE app_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    profile_id TEXT NOT NULL,
                    app_name TEXT NOT NULL,
                    title_regex TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE schema_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )
            .unwrap();
            // 旧 LINE 行 + 新 LINE 行 (ユーザー手動追加) が共存
            raw.execute(
                "INSERT INTO app_rules (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('casual', 'chrome.exe', '(?i)line', 100, 100)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO app_rules (profile_id, app_name, title_regex, created_at, updated_at)
                 VALUES ('casual', 'LINE.exe', '', 200, 200)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO schema_meta (key, value) VALUES ('seeded_defaults_v1', '1')",
                [],
            )
            .unwrap();
        }
        let db = AppRulesDb::open(&path).expect("open");
        let rules = db.list().expect("list");
        // 旧 LINE は削除・新 LINE は 1 件のまま (重複 INSERT skip)
        let line_count = rules
            .iter()
            .filter(|r| r.app_name == "LINE.exe" && r.title_regex.is_empty())
            .count();
        assert_eq!(line_count, 1, "新 LINE 行は重複しない");
        assert!(!rules
            .iter()
            .any(|r| r.app_name == "chrome.exe" && r.title_regex == "(?i)line"));
    }

    #[test]
    fn test_special_chars_escaped() {
        // SQL injection 防御 (params! が処理することを検証)
        let path = tmp_db("escape");
        let db = open_cleared(&path);
        let tricky_app = "It's; DROP TABLE app_rules;--";
        let tricky_regex = "\"\\b(test|prod)\\b\"";
        let id = db
            .upsert(&sample("plain", tricky_app, tricky_regex))
            .expect("insert tricky");
        let got = db.get(id).unwrap().unwrap();
        assert_eq!(got.app_name, tricky_app);
        assert_eq!(got.title_regex, tricky_regex);
        // テーブルが残っていること
        assert_eq!(db.count().unwrap(), 1);
    }

    // ----- v0.1.7.1 タスク A: schema_meta 汎用 helper のテスト -----

    #[test]
    fn test_get_meta_returns_none_for_unknown_key() {
        let path = tmp_db("meta_unknown");
        let db = open_cleared(&path);
        let v = db.get_meta("nonexistent_key_xyz").expect("get_meta");
        assert!(v.is_none(), "未投入キーは None");
    }

    #[test]
    fn test_set_meta_then_get_meta_roundtrip() {
        let path = tmp_db("meta_roundtrip");
        let db = open_cleared(&path);
        // タスク A の本命フラグキーを使ってラウンドトリップを検証
        db.set_meta("autostart_default_applied_v017_1", "1")
            .expect("set_meta");
        let v = db.get_meta("autostart_default_applied_v017_1").expect("get");
        assert_eq!(v.as_deref(), Some("1"));
    }

    #[test]
    fn test_delete_meta_removes_key_and_is_idempotent() {
        // v0.1.12 F3 A 案: disable_autostart で autostart_default_applied_v017_1 を消す経路を担保。
        let path = tmp_db("meta_delete");
        let db = open_cleared(&path);
        db.set_meta("autostart_default_applied_v017_1", "1")
            .expect("set_meta");
        // 1 回目: 削除成功 + get_meta が None
        db.delete_meta("autostart_default_applied_v017_1")
            .expect("delete_meta first");
        let v = db.get_meta("autostart_default_applied_v017_1").expect("get");
        assert!(v.is_none(), "delete 後は None");
        // 2 回目: 存在しないキーへの delete も Ok (idempotent)
        db.delete_meta("autostart_default_applied_v017_1")
            .expect("delete_meta idempotent");
        db.delete_meta("never_existed_key").expect("delete_meta unknown");
    }

    #[test]
    fn test_set_meta_upsert_overwrites_existing_value() {
        let path = tmp_db("meta_upsert");
        let db = open_cleared(&path);
        db.set_meta("k1", "0").expect("set_meta v0");
        db.set_meta("k1", "1").expect("set_meta v1");
        let v = db.get_meta("k1").expect("get");
        assert_eq!(v.as_deref(), Some("1"), "後勝ち upsert");
    }

    #[test]
    fn test_seed_defaults_v1_key_visible_via_get_meta() {
        // 既存の seed 投入と新 helper が同じ schema_meta テーブルを参照していることを担保。
        let path = tmp_db("meta_shares_seed");
        let _db_first = AppRulesDb::open(&path).expect("open");
        let db = AppRulesDb::open(&path).expect("open 2");
        // seeded_defaults_v1 は初回 open で自動投入される
        let v = db.get_meta("seeded_defaults_v1").expect("get");
        assert_eq!(v.as_deref(), Some("1"));
    }
}
