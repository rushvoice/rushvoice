// history_db.rs — Phase 2 D 履歴 SQLite 永続化 (Phase A: 基盤層)
//
// SSOT v2.10 §8.3.2 / 設計ドラフト phase2_d_history_sqlite_design_2026-05-22.md
//
// Q1-Q5 確定事項 (2026-05-22 Day 11 判断・2026-05-28 Q2 件数ベース化):
// - Q1: DB 分離 (config.db とは別の history.db)
// - Q2: デフォルト保持 直近 50 件 (delete_keep_recent)
//       ※ 経緯: 90 日 (Day 11) → 7 日 (v0.1.7.1 初版) → 直近 50 件 (v0.1.7.1 改訂)
//       1 日の録音件数にバラつきがあるため、件数で管理する方が利用者の体感に合う。
// - Q3: 平文保存許容 (DPAPI 対象外)
// - Q4: Phase 2 開始時の D 先頭着手
// - Q5: Phase 3 用カラム (edited_text / edit_diff_json / edit_count / confidence_drop_flag) 先行追加
//
// 保存先: %APPDATA%/RushVoice/history.db
// config_db.rs (Day 5 後半 100% カバーパターン) を踏襲。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HistoryDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// 履歴 1 レコード。INSERT 前は `id = None`、SELECT 結果は `id = Some(rowid)`。
///
/// Phase 2 D で全カラム実装。Phase 3 編集学習で `edited_text` / `edit_diff_json` /
/// `edit_count` を本格利用するが、カラムは Phase 2 で先行追加するため
/// Phase 3 で migration 不要 (Q5)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: Option<i64>,
    pub created_at: String, // RFC3339 / ISO8601 UTC
    pub provider: String,   // 'groq' | 'openai' | 'groq_solo' (v0.1.9 hedged 勝者の実プロバイダー)
    pub model: String,      // 'whisper-large-v3-turbo' 等
    pub mode: Option<String>,
    pub app_name: Option<String>,
    pub raw_text: String,
    pub refined_text: String,
    pub edited_text: Option<String>,
    pub edit_diff_json: Option<String>,
    pub edit_count: i32,
    pub confidence_drop_flag: bool,
    pub duration_ms: i64,
    pub transcribe_ms: i64,
    pub audio_bytes: i64,
    pub avg_logprob: Option<f64>,
    pub no_speech_prob: Option<f64>,
    // v0.1.9 hedged 計測カラム (idempotent migration で追加)
    /// primary 単独の所要 ms (hedged 経由のときに記録・solo 経路は None)
    #[serde(default)]
    pub primary_ms: Option<i64>,
    /// fallback が dispatch されたか (hedged 経由のときのみ Some・solo 経路は None)
    #[serde(default)]
    pub fallback_triggered: Option<bool>,
    /// hedged 勝者 ("primary" / "fallback" / "solo"・solo 経路は None)
    #[serde(default)]
    pub hedged_winner: Option<String>,
}

pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    /// 指定パスの SQLite を開き、必要なら親ディレクトリを作成して
    /// CREATE TABLE IF NOT EXISTS で history テーブル + 3 インデックスを初期化する。
    ///
    /// WAL モード + busy_timeout 5 秒 (SSOT §8.3.2.9 R1/R3 対策):
    /// - history は高頻度 INSERT (発話毎)・config.db (rare reads) と分離してロック競合回避
    /// - HistoryPage 読込中の INSERT 衝突は busy_timeout で吸収
    pub fn open(path: &Path) -> Result<Self, HistoryDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL + busy_timeout: SSOT §8.3.2.9 R1/R3 (高頻度 INSERT 競合対策)
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = HistoryDb { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<(), HistoryDbError> {
        // SSOT v2.10 §8.3.2.2 確定スキーマと完全一致 (v0.1.9 migration は init 末尾で追加)
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                mode TEXT,
                app_name TEXT,
                raw_text TEXT NOT NULL,
                refined_text TEXT NOT NULL,
                edited_text TEXT,
                edit_diff_json TEXT,
                edit_count INTEGER NOT NULL DEFAULT 0,
                confidence_drop_flag INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL,
                transcribe_ms INTEGER NOT NULL,
                audio_bytes INTEGER NOT NULL,
                avg_logprob REAL,
                no_speech_prob REAL
            );
            CREATE INDEX IF NOT EXISTS idx_history_created_at ON history(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_history_edit_count ON history(edit_count) WHERE edit_count > 0;
            CREATE INDEX IF NOT EXISTS idx_history_app ON history(app_name) WHERE app_name IS NOT NULL;
            "#,
        )?;
        self.migrate_v019_hedged_columns()?;
        Ok(())
    }

    /// v0.1.9 migration: hedged 計測カラム 3 件を idempotent に追加。
    ///
    /// PRAGMA table_info で既存列を取得 → 存在しない列のみ ALTER TABLE ADD COLUMN。
    /// L182 (DB 保持) 準拠で既存データは破壊しない。
    fn migrate_v019_hedged_columns(&self) -> Result<(), HistoryDbError> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(history)")?;
        let existing: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        if !existing.contains("primary_ms") {
            self.conn
                .execute("ALTER TABLE history ADD COLUMN primary_ms INTEGER", [])?;
            log::info!("[history_db] migration v0.1.9: added column primary_ms");
        }
        if !existing.contains("fallback_triggered") {
            self.conn
                .execute("ALTER TABLE history ADD COLUMN fallback_triggered INTEGER", [])?;
            log::info!("[history_db] migration v0.1.9: added column fallback_triggered");
        }
        if !existing.contains("hedged_winner") {
            self.conn
                .execute("ALTER TABLE history ADD COLUMN hedged_winner TEXT", [])?;
            log::info!("[history_db] migration v0.1.9: added column hedged_winner");
        }
        Ok(())
    }

    /// 履歴 1 件追加し、生成された rowid を返す。`item.id` は無視される (常に AUTOINCREMENT)。
    pub fn insert(&self, item: &HistoryItem) -> Result<i64, HistoryDbError> {
        self.conn.execute(
            "INSERT INTO history (
                created_at, provider, model, mode, app_name,
                raw_text, refined_text, edited_text, edit_diff_json,
                edit_count, confidence_drop_flag,
                duration_ms, transcribe_ms, audio_bytes,
                avg_logprob, no_speech_prob,
                primary_ms, fallback_triggered, hedged_winner
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                item.created_at,
                item.provider,
                item.model,
                item.mode,
                item.app_name,
                item.raw_text,
                item.refined_text,
                item.edited_text,
                item.edit_diff_json,
                item.edit_count,
                item.confidence_drop_flag as i32,
                item.duration_ms,
                item.transcribe_ms,
                item.audio_bytes,
                item.avg_logprob,
                item.no_speech_prob,
                item.primary_ms,
                item.fallback_triggered.map(|b| b as i32),
                item.hedged_winner,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 新しい順 (created_at DESC, id DESC) で limit/offset ページング取得。
    ///
    /// 同一 created_at 内の安定順序のため id DESC を tiebreaker に併用 (テスト容易性のため)。
    pub fn list(&self, limit: i64, offset: i64) -> Result<Vec<HistoryItem>, HistoryDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, provider, model, mode, app_name,
                    raw_text, refined_text, edited_text, edit_diff_json,
                    edit_count, confidence_drop_flag,
                    duration_ms, transcribe_ms, audio_bytes,
                    avg_logprob, no_speech_prob,
                    primary_ms, fallback_triggered, hedged_winner
             FROM history
             ORDER BY created_at DESC, id DESC
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![limit, offset], row_to_item)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 指定 ID の 1 件取得。存在しない場合 `Ok(None)`。
    pub fn get(&self, id: i64) -> Result<Option<HistoryItem>, HistoryDbError> {
        let item = self
            .conn
            .query_row(
                "SELECT id, created_at, provider, model, mode, app_name,
                        raw_text, refined_text, edited_text, edit_diff_json,
                        edit_count, confidence_drop_flag,
                        duration_ms, transcribe_ms, audio_bytes,
                        avg_logprob, no_speech_prob,
                        primary_ms, fallback_triggered, hedged_winner
                 FROM history WHERE id = ?1",
                params![id],
                row_to_item,
            )
            .optional()?;
        Ok(item)
    }

    /// Phase 3 編集学習で使用。`edited_text` 更新 + `edit_count` インクリメント。
    /// `diff_json` は省略可 (Phase 2 では未使用)。
    pub fn update_edit(
        &self,
        id: i64,
        edited_text: &str,
        diff_json: Option<&str>,
    ) -> Result<(), HistoryDbError> {
        self.conn.execute(
            "UPDATE history
             SET edited_text = ?2,
                 edit_diff_json = ?3,
                 edit_count = edit_count + 1
             WHERE id = ?1",
            params![id, edited_text, diff_json],
        )?;
        Ok(())
    }

    /// 指定 ID を削除。存在しない ID でもエラーにせず Ok を返す (UI 楽観的更新と整合)。
    pub fn delete(&self, id: i64) -> Result<(), HistoryDbError> {
        self.conn
            .execute("DELETE FROM history WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 全件削除 (Settings「履歴を全削除」UI 用)。
    pub fn clear(&self) -> Result<(), HistoryDbError> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }

    /// 直近 `n` 件のみ残し、それ以外 (id 昇順で古いもの) を削除。削除件数を返す。
    ///
    /// 起動時 cleanup で `delete_keep_recent(HISTORY_RETENTION_COUNT)` を Phase B の `lib.rs` setup
    /// から呼ぶ想定 (Q2)。v0.1.7.1 改訂 (2026-05-28) で `HISTORY_RETENTION_COUNT = 50`。
    ///
    /// 順序の根拠: history テーブルは AUTOINCREMENT id で、id は INSERT 順に単調増加するため
    /// id 降順 = 新しい順となる (created_at の文字列比較ではなく id を採用する方が、
    /// 同秒内 INSERT や時計ずれの影響を受けず確実)。
    ///
    /// 実装: `DELETE FROM history WHERE id NOT IN (SELECT id FROM history ORDER BY id DESC LIMIT N)`
    ///
    /// n=0 を渡した場合は全件削除と等価になる (DELETE が 0 件以外をすべて削除)。
    pub fn delete_keep_recent(&self, n: i64) -> Result<u64, HistoryDbError> {
        let affected = self.conn.execute(
            "DELETE FROM history WHERE id NOT IN (SELECT id FROM history ORDER BY id DESC LIMIT ?1)",
            params![n],
        )?;
        Ok(affected as u64)
    }

    /// 履歴総件数 (Settings 容量管理 UI 用)。
    pub fn count(&self) -> Result<i64, HistoryDbError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        Ok(n)
    }
}

/// SQLite 行 → HistoryItem 変換 (list / get 共用)。
fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryItem> {
    let confidence_drop_flag_int: i32 = row.get(11)?;
    let fallback_triggered_int: Option<i32> = row.get(18)?;
    Ok(HistoryItem {
        id: Some(row.get(0)?),
        created_at: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        mode: row.get(4)?,
        app_name: row.get(5)?,
        raw_text: row.get(6)?,
        refined_text: row.get(7)?,
        edited_text: row.get(8)?,
        edit_diff_json: row.get(9)?,
        edit_count: row.get(10)?,
        confidence_drop_flag: confidence_drop_flag_int != 0,
        duration_ms: row.get(12)?,
        transcribe_ms: row.get(13)?,
        audio_bytes: row.get(14)?,
        avg_logprob: row.get(15)?,
        no_speech_prob: row.get(16)?,
        primary_ms: row.get(17)?,
        fallback_triggered: fallback_triggered_int.map(|i| i != 0),
        hedged_winner: row.get(19)?,
    })
}

/// `<data_dir>/RushVoice/history.db` のデフォルトパス (Q1)。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
pub fn default_history_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("history.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_history_db_test_{}", name))
            .join("history.db");
        let _ = std::fs::remove_file(&p);
        // WAL/SHM 残骸も掃除 (テスト再実行時のロック回避)
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    /// テスト用サンプル: 必須カラムだけ埋めた最小 HistoryItem。
    fn sample_item(raw: &str) -> HistoryItem {
        HistoryItem {
            id: None,
            created_at: "2026-05-22T10:00:00Z".to_string(),
            provider: "groq".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            mode: None,
            app_name: None,
            raw_text: raw.to_string(),
            refined_text: raw.to_string(),
            edited_text: None,
            edit_diff_json: None,
            edit_count: 0,
            confidence_drop_flag: false,
            duration_ms: 3000,
            transcribe_ms: 500,
            audio_bytes: 96000,
            avg_logprob: Some(-0.15),
            no_speech_prob: Some(0.0),
            primary_ms: None,
            fallback_triggered: None,
            hedged_winner: None,
        }
    }

    #[test]
    fn test_init_creates_table() {
        let path = tmp_db("init");
        let _db = HistoryDb::open(&path).expect("open 1");
        // 2 回目の open でも壊れない (CREATE TABLE IF NOT EXISTS)
        let _db2 = HistoryDb::open(&path).expect("open 2");
    }

    #[test]
    fn test_insert_and_list() {
        let path = tmp_db("insert_list");
        let db = HistoryDb::open(&path).expect("open");
        let id1 = db.insert(&sample_item("こんにちは")).expect("insert 1");
        let id2 = db.insert(&sample_item("テストです")).expect("insert 2");
        assert!(id2 > id1, "AUTOINCREMENT で id が増えること");

        let rows = db.list(10, 0).expect("list");
        assert_eq!(rows.len(), 2);
        // 同一 created_at の場合 id DESC tiebreaker により新しい id が先頭
        assert_eq!(rows[0].id, Some(id2));
        assert_eq!(rows[0].raw_text, "テストです");
        assert_eq!(rows[1].id, Some(id1));
        assert_eq!(rows[1].raw_text, "こんにちは");
    }

    #[test]
    fn test_get_by_id() {
        let path = tmp_db("get");
        let db = HistoryDb::open(&path).expect("open");
        let id = db.insert(&sample_item("get テスト")).expect("insert");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.id, Some(id));
        assert_eq!(got.raw_text, "get テスト");
        // 全カラム往復確認 (型変換の正しさ)
        assert_eq!(got.provider, "groq");
        assert_eq!(got.model, "whisper-large-v3-turbo");
        assert_eq!(got.duration_ms, 3000);
        assert_eq!(got.transcribe_ms, 500);
        assert_eq!(got.audio_bytes, 96000);
        assert_eq!(got.avg_logprob, Some(-0.15));
        assert_eq!(got.no_speech_prob, Some(0.0));
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let path = tmp_db("get_none");
        let db = HistoryDb::open(&path).expect("open");
        assert!(db.get(999_999).unwrap().is_none());
    }

    #[test]
    fn test_delete() {
        let path = tmp_db("delete");
        let db = HistoryDb::open(&path).expect("open");
        let id1 = db.insert(&sample_item("残す")).expect("insert 1");
        let id2 = db.insert(&sample_item("消す")).expect("insert 2");
        db.delete(id2).expect("delete");
        let rows = db.list(10, 0).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, Some(id1));
    }

    #[test]
    fn test_delete_nonexistent_ok() {
        // UI 楽観的更新との整合: 存在しない ID 削除でもエラーにしない
        let path = tmp_db("delete_none");
        let db = HistoryDb::open(&path).expect("open");
        db.delete(999_999).expect("delete nonexistent");
    }

    #[test]
    fn test_clear() {
        let path = tmp_db("clear");
        let db = HistoryDb::open(&path).expect("open");
        for i in 0..5 {
            db.insert(&sample_item(&format!("item {}", i)))
                .expect("insert");
        }
        assert_eq!(db.count().unwrap(), 5);
        db.clear().expect("clear");
        assert_eq!(db.count().unwrap(), 0);
        assert!(db.list(10, 0).unwrap().is_empty());
    }

    #[test]
    fn test_count() {
        let path = tmp_db("count");
        let db = HistoryDb::open(&path).expect("open");
        assert_eq!(db.count().unwrap(), 0);
        for i in 0..3 {
            db.insert(&sample_item(&format!("c{}", i)))
                .expect("insert");
        }
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn test_list_empty() {
        let path = tmp_db("list_empty");
        let db = HistoryDb::open(&path).expect("open");
        let rows = db.list(50, 0).expect("list empty");
        assert!(rows.is_empty());
    }

    #[test]
    fn test_list_pagination() {
        let path = tmp_db("paginate");
        let db = HistoryDb::open(&path).expect("open");
        for i in 0..7 {
            db.insert(&sample_item(&format!("p{}", i)))
                .expect("insert");
        }
        // limit=3, offset=0 → 上位 3 件
        let page1 = db.list(3, 0).expect("page1");
        assert_eq!(page1.len(), 3);
        // limit=3, offset=3 → 次 3 件
        let page2 = db.list(3, 3).expect("page2");
        assert_eq!(page2.len(), 3);
        // limit=3, offset=6 → 残 1 件
        let page3 = db.list(3, 6).expect("page3");
        assert_eq!(page3.len(), 1);
        // page1 / page2 / page3 で id 重複なし
        let mut all_ids: Vec<Option<i64>> = page1.iter().map(|i| i.id).collect();
        all_ids.extend(page2.iter().map(|i| i.id));
        all_ids.extend(page3.iter().map(|i| i.id));
        let unique: std::collections::HashSet<_> = all_ids.iter().collect();
        assert_eq!(unique.len(), 7, "全 ID がページ間で重複しないこと");
    }

    #[test]
    fn test_cleanup_keeps_most_recent_50_records() {
        // v0.1.7.1 改訂 (2026-05-28): 件数ベース保持 (直近 50 件) の根幹テスト。
        // 51 件挿入 → delete_keep_recent(50) → 50 件残ること + 最古 1 件が削除されること。
        // id は AUTOINCREMENT で挿入順に単調増加するため、id=1 が最古・id=51 が最新。
        let path = tmp_db("cleanup_keep50");
        let db = HistoryDb::open(&path).expect("open");

        // 51 件挿入 (raw_text に index を埋め込んで識別可能にする)
        for i in 1..=51 {
            let mut item = sample_item(&format!("record-{:03}", i));
            // created_at は単調増加させる (実運用では INSERT 順 = 時系列順)
            item.created_at = chrono::Utc::now().to_rfc3339();
            db.insert(&item).expect("insert");
        }
        assert_eq!(db.count().unwrap(), 51);

        let deleted = db.delete_keep_recent(50).expect("cleanup");
        assert_eq!(deleted, 1, "51 件中、最古 1 件のみ削除されること");
        assert_eq!(db.count().unwrap(), 50);

        // 最古 (record-001 = id=1) が削除され、record-002..051 が残ること
        let rows = db.list(100, 0).expect("list");
        assert_eq!(rows.len(), 50);
        let texts: Vec<String> = rows.iter().map(|r| r.raw_text.clone()).collect();
        assert!(!texts.contains(&"record-001".to_string()), "最古 record-001 は削除されているはず");
        assert!(texts.contains(&"record-002".to_string()), "record-002 は残っているはず");
        assert!(texts.contains(&"record-051".to_string()), "最新 record-051 は残っているはず");
    }

    #[test]
    fn test_cleanup_keep_recent_zero_when_under_limit() {
        // 件数が上限未満なら 0 件削除 (10 件 ≤ 50 件)。
        let path = tmp_db("cleanup_under");
        let db = HistoryDb::open(&path).expect("open");
        for i in 0..10 {
            db.insert(&sample_item(&format!("u-{}", i))).expect("insert");
        }
        let deleted = db.delete_keep_recent(50).expect("cleanup");
        assert_eq!(deleted, 0);
        assert_eq!(db.count().unwrap(), 10);
    }

    #[test]
    fn test_cleanup_keep_recent_boundary_exact_n() {
        // ちょうど上限と同数なら 0 件削除 (50 件 == 50 件 の境界)。
        let path = tmp_db("cleanup_boundary");
        let db = HistoryDb::open(&path).expect("open");
        for i in 0..50 {
            db.insert(&sample_item(&format!("b-{}", i))).expect("insert");
        }
        let deleted = db.delete_keep_recent(50).expect("cleanup");
        assert_eq!(deleted, 0, "ちょうど 50 件なら削除なし");
        assert_eq!(db.count().unwrap(), 50);
    }

    #[test]
    fn test_phase3_columns_default_values() {
        // Q5 Phase 3 用カラム先行追加: insert 時 edited_text=None, edit_count=0,
        // confidence_drop_flag=false が正しく往復すること
        let path = tmp_db("phase3_defaults");
        let db = HistoryDb::open(&path).expect("open");
        let id = db.insert(&sample_item("Phase 3 default")).expect("insert");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.edited_text, None);
        assert_eq!(got.edit_diff_json, None);
        assert_eq!(got.edit_count, 0);
        assert!(!got.confidence_drop_flag);
    }

    #[test]
    fn test_update_edit_increments_count() {
        // Phase 3 編集学習の基盤動作: update_edit で edit_count がインクリメント
        let path = tmp_db("update_edit");
        let db = HistoryDb::open(&path).expect("open");
        let id = db.insert(&sample_item("原文")).expect("insert");

        db.update_edit(id, "編集1", None).expect("edit 1");
        let after1 = db.get(id).unwrap().unwrap();
        assert_eq!(after1.edited_text.as_deref(), Some("編集1"));
        assert_eq!(after1.edit_count, 1);
        assert_eq!(after1.edit_diff_json, None);

        db.update_edit(id, "編集2", Some(r#"{"op":"replace"}"#))
            .expect("edit 2");
        let after2 = db.get(id).unwrap().unwrap();
        assert_eq!(after2.edited_text.as_deref(), Some("編集2"));
        assert_eq!(after2.edit_count, 2);
        assert_eq!(after2.edit_diff_json.as_deref(), Some(r#"{"op":"replace"}"#));
    }

    #[test]
    fn test_special_chars_escaped() {
        // SSOT §8.3.2.9 R4: シングル/ダブルクォート等の SQL エスケープを params! が処理すること
        let path = tmp_db("escape");
        let db = HistoryDb::open(&path).expect("open");
        let tricky = "It's a \"test\" with 'quotes' and ;DROP TABLE history;--";
        let id = db.insert(&sample_item(tricky)).expect("insert tricky");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.raw_text, tricky);
        // テーブルは残っていること
        assert_eq!(db.count().unwrap(), 1);
    }

    #[test]
    fn test_all_optional_fields_roundtrip() {
        // mode / app_name / avg_logprob / no_speech_prob の Some/None 両方が正しく往復
        let path = tmp_db("optional");
        let db = HistoryDb::open(&path).expect("open");
        let mut item = sample_item("optional");
        item.mode = Some("dictation".to_string());
        item.app_name = Some("notepad.exe".to_string());
        item.avg_logprob = Some(-0.5);
        item.no_speech_prob = Some(0.01);
        item.confidence_drop_flag = true;
        let id = db.insert(&item).expect("insert");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.mode.as_deref(), Some("dictation"));
        assert_eq!(got.app_name.as_deref(), Some("notepad.exe"));
        assert_eq!(got.avg_logprob, Some(-0.5));
        assert_eq!(got.no_speech_prob, Some(0.01));
        assert!(got.confidence_drop_flag);
    }

    // ---- v0.1.9 hedged 計測カラム ----

    #[test]
    fn test_v019_hedged_columns_roundtrip() {
        let path = tmp_db("v019_hedged_roundtrip");
        let db = HistoryDb::open(&path).expect("open");
        let mut item = sample_item("hedged-test");
        item.provider = "openai".to_string();
        item.primary_ms = Some(5200);
        item.fallback_triggered = Some(true);
        item.hedged_winner = Some("fallback".to_string());
        let id = db.insert(&item).expect("insert hedged");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.provider, "openai");
        assert_eq!(got.primary_ms, Some(5200));
        assert_eq!(got.fallback_triggered, Some(true));
        assert_eq!(got.hedged_winner.as_deref(), Some("fallback"));
    }

    #[test]
    fn test_v019_hedged_columns_null_for_solo() {
        // solo 経路 (Groq 単独・hedged 経由せず) は v0.1.9 列がすべて NULL
        let path = tmp_db("v019_hedged_solo");
        let db = HistoryDb::open(&path).expect("open");
        let item = sample_item("solo-test"); // 既定で primary_ms=None / fallback_triggered=None / hedged_winner=None
        let id = db.insert(&item).expect("insert solo");
        let got = db.get(id).expect("get").expect("Some");
        assert_eq!(got.primary_ms, None);
        assert_eq!(got.fallback_triggered, None);
        assert_eq!(got.hedged_winner, None);
    }

    #[test]
    fn test_v019_migration_idempotent() {
        // 同一 DB に open() を 2 回呼んでも ALTER は idempotent (PRAGMA table_info で重複防止)。
        // 1 度目 = ALTER 実行・2 度目 = skip → エラー無し。
        let path = tmp_db("v019_migration_idempotent");
        {
            let _db1 = HistoryDb::open(&path).expect("open 1");
        }
        {
            let _db2 = HistoryDb::open(&path).expect("open 2 (should skip ALTER)");
        }
        let db3 = HistoryDb::open(&path).expect("open 3");
        // INSERT が 3 新カラム込みで成功すること
        let mut item = sample_item("migration-test");
        item.primary_ms = Some(100);
        item.fallback_triggered = Some(false);
        item.hedged_winner = Some("primary".to_string());
        db3.insert(&item).expect("insert after re-open");
    }

    #[test]
    fn test_default_history_db_path_uses_appdata() {
        // APPDATA 環境変数経由の正規パス生成 (config_db.rs と同等の挙動)
        let path = default_history_db_path();
        // 末尾は history.db
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("history.db")
        );
        // 親ディレクトリは RushVoice
        assert_eq!(
            path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
            Some("RushVoice")
        );
    }
}
