// glossary_db.rs — Phase 2 C Wave 1 辞書 (glossary) SQLite 永続化
//
// SSOT v2.10 §8.3.1 DT-01 / DT-02
// Wave 1 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md
//
// 設計方針:
// - DT-01 表示 2 列 (pattern / replacement) のみ Table に出すが、
//   DT-02 内部列 (context / note / is_regex) も DB スキーマ・型に保持。
//   将来 Phase 3 で辞書置換ロジックが利用する。
// - history_db.rs の Phase 2 D 実装パターン (Connection + 単一 struct + thiserror) を踏襲。
// - 別 DB ファイル %APPDATA%/RushVoice/glossary.db に分離 (config.db / history.db と並列)。
//   理由: 辞書は中頻度 INSERT/UPDATE・config (rare) / history (high freq) と書込特性が異なる。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GlossaryDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// `find_overlapping` の戻り値。1 件の既存 entry と、query との交差した
/// sub-pattern (「/」区切り単位) の一覧を保持。
///
/// Step 2.5 (要望): 「たかなし/たかなさ/たかなら → 高梨」のように 1 ルールに
/// 複数誤変換を集約する UX。新規/更新時に既存ルールと sub-pattern が重複したら
/// このリストを使って警告 + 上書き選択を行う。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlapResult {
    pub entry: GlossaryEntry,
    pub overlapping_sub_patterns: Vec<String>,
}

/// 辞書 1 エントリ。INSERT 前は `id = None`、SELECT 結果は `id = Some(rowid)`。
///
/// Wave 1 表示は pattern / replacement のみだが、Wave 1 で context / note / is_regex
/// カラムを先行追加し編集フォームでのみ表示 (DT-02)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlossaryEntry {
    pub id: Option<i64>,
    pub pattern: String,
    pub replacement: String,
    /// 適用文脈 (例: "メール", "口語") — DT-02 内部列・Wave 1 では編集フォームのみ
    pub context: Option<String>,
    /// 自由記述メモ — DT-02 内部列
    pub note: Option<String>,
    /// 正規表現として扱うか — DT-02 内部列・将来 Phase 3 L3 置換で使用 (L068)
    pub is_regex: bool,
    /// INSERT 時刻 (Unix epoch seconds, UTC)
    pub created_at: i64,
    /// 最終 UPDATE 時刻 (Unix epoch seconds, UTC)
    pub updated_at: i64,
}

pub struct GlossaryDb {
    conn: Connection,
}

impl GlossaryDb {
    /// 指定パスの SQLite を開き、必要なら親ディレクトリを作成して
    /// CREATE TABLE IF NOT EXISTS で glossary テーブル + index を初期化する。
    pub fn open(path: &Path) -> Result<Self, GlossaryDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL + busy_timeout: history_db.rs と同じ恒久対策
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = GlossaryDb { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<(), GlossaryDbError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS glossary (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pattern TEXT NOT NULL,
                replacement TEXT NOT NULL,
                context TEXT,
                note TEXT,
                is_regex INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_glossary_pattern ON glossary(pattern);
            "#,
        )?;
        Ok(())
    }

    /// 全件取得 (created_at ASC で安定順)。Wave 1 は件数想定 100-500 のため LIMIT なし。
    /// 将来 1,000 件超で仮想スクロール検討 (設計判断書 §3.4)。
    pub fn list(&self) -> Result<Vec<GlossaryEntry>, GlossaryDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pattern, replacement, context, note, is_regex, created_at, updated_at
             FROM glossary
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// id 指定で 1 件取得。存在しない場合 `Ok(None)`。
    pub fn get(&self, id: i64) -> Result<Option<GlossaryEntry>, GlossaryDbError> {
        let entry = self
            .conn
            .query_row(
                "SELECT id, pattern, replacement, context, note, is_regex, created_at, updated_at
                 FROM glossary WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()?;
        Ok(entry)
    }

    /// id が Some なら UPDATE・None なら INSERT。返却は最終的な rowid。
    ///
    /// created_at は INSERT 時のみ書き込み・UPDATE では維持。
    /// updated_at は INSERT/UPDATE 両方で `now` に上書き。
    pub fn upsert(&self, entry: &GlossaryEntry) -> Result<i64, GlossaryDbError> {
        let now = current_epoch_secs();
        match entry.id {
            Some(id) => {
                self.conn.execute(
                    "UPDATE glossary
                     SET pattern = ?2, replacement = ?3, context = ?4, note = ?5,
                         is_regex = ?6, updated_at = ?7
                     WHERE id = ?1",
                    params![
                        id,
                        entry.pattern,
                        entry.replacement,
                        entry.context,
                        entry.note,
                        entry.is_regex as i32,
                        now,
                    ],
                )?;
                Ok(id)
            }
            None => {
                self.conn.execute(
                    "INSERT INTO glossary
                        (pattern, replacement, context, note, is_regex, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        entry.pattern,
                        entry.replacement,
                        entry.context,
                        entry.note,
                        entry.is_regex as i32,
                        now,
                        now,
                    ],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    /// 1 件削除。存在しない id でも Err にせず Ok を返す (UI 楽観的更新と整合)。
    pub fn delete(&self, id: i64) -> Result<(), GlossaryDbError> {
        self.conn
            .execute("DELETE FROM glossary WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 同 pattern の既存エントリを 1 件検索。
    /// `exclude_id` を渡すと「自分自身」を除外できる (UPDATE 時の重複検出に使用)。
    pub fn find_duplicate(
        &self,
        pattern: &str,
        exclude_id: Option<i64>,
    ) -> Result<Option<GlossaryEntry>, GlossaryDbError> {
        let entry = match exclude_id {
            Some(eid) => self
                .conn
                .query_row(
                    "SELECT id, pattern, replacement, context, note, is_regex, created_at, updated_at
                     FROM glossary WHERE pattern = ?1 AND id <> ?2 LIMIT 1",
                    params![pattern, eid],
                    row_to_entry,
                )
                .optional()?,
            None => self
                .conn
                .query_row(
                    "SELECT id, pattern, replacement, context, note, is_regex, created_at, updated_at
                     FROM glossary WHERE pattern = ?1 LIMIT 1",
                    params![pattern],
                    row_to_entry,
                )
                .optional()?,
        };
        Ok(entry)
    }

    /// 「/」区切り pattern を sub-pattern 単位で重複検出する。
    ///
    /// Step 2.5 (要望): 1 つのルールに複数の誤変換を集約する UX 対応。
    /// 例: 既存「たかなし/たかな」+ 新規「たかな/のだ/たかなし」 →
    ///     既存 entry を返し overlapping = ["たかなし", "たかな"]。
    ///
    /// - `pattern_query` を `/` で split し trim + 空除外して sub_patterns_query を得る
    /// - DB 全 entries を取得 (exclude_id があれば除外)
    /// - 各 entry の pattern を同様に split → sub_patterns_existing
    /// - sub_patterns_query と sub_patterns_existing の交差を計算
    /// - 交差が 1 件以上なら OverlapResult として push
    ///
    /// `find_duplicate` は完全一致専用で残置 (後方互換)。
    pub fn find_overlapping(
        &self,
        pattern_query: &str,
        exclude_id: Option<i64>,
    ) -> Result<Vec<OverlapResult>, GlossaryDbError> {
        let sub_patterns_query: Vec<String> = split_sub_patterns(pattern_query);
        if sub_patterns_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut out: Vec<OverlapResult> = Vec::new();
        for entry in self.list()? {
            if let (Some(eid), Some(xid)) = (entry.id, exclude_id) {
                if eid == xid {
                    continue;
                }
            }
            let sub_existing = split_sub_patterns(&entry.pattern);
            let overlapping: Vec<String> = sub_patterns_query
                .iter()
                .filter(|q| sub_existing.iter().any(|e| e == *q))
                .cloned()
                .collect();
            if !overlapping.is_empty() {
                out.push(OverlapResult {
                    entry,
                    overlapping_sub_patterns: overlapping,
                });
            }
        }
        Ok(out)
    }

    /// 総件数 (将来 Settings UI 用)。
    pub fn count(&self) -> Result<i64, GlossaryDbError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM glossary", [], |row| row.get(0))?;
        Ok(n)
    }

    // ----- v0.1.10 機能 E: CSV インポート/エクスポート用追加メソッド -----
    //
    // SSOT: v0_1_10_feature_e_delegation_2026-05-30.md
    // find_by_pattern : 完全一致検索 (CSV import 重複判定用・大文字小文字区別)
    // delete_all      : 「全置換」モード用
    // bulk_insert     : 一括 INSERT (transaction 化)

    /// 同 pattern (完全一致・大文字小文字区別) の既存 entry を取得。
    /// CSV import 時の重複判定 (key = pattern) に使用する。
    /// `find_duplicate(pattern, None)` と等価だが、用途が明確なため別名で公開。
    pub fn find_by_pattern(
        &self,
        pattern: &str,
    ) -> Result<Option<GlossaryEntry>, GlossaryDbError> {
        self.find_duplicate(pattern, None)
    }

    /// 全 entry を削除し、削除件数を返す。CSV import「全置換」モード用。
    /// SQLite の DELETE は AUTOINCREMENT 連番をリセットしないため、新規 INSERT 時の
    /// id は元の最大値以降から再採番される (sqlite_sequence をリセットしない方針)。
    pub fn delete_all(&self) -> Result<usize, GlossaryDbError> {
        let n = self.conn.execute("DELETE FROM glossary", [])?;
        Ok(n)
    }

    /// 一括 INSERT を transaction 内で実行。
    ///
    /// - entries[i].id は無視 (CSV import 時は再採番のため)
    /// - created_at / updated_at は entry に値があればそのまま採用 (export → import の round-trip 用)・
    ///   ただし 0 以下なら現在時刻を採用 (新規追加でテスト用空 entry を受けたケース)
    /// - 全件成功時のみコミット・途中失敗で rollback
    /// - 戻り値は INSERT 成功件数 (= entries.len() on success)
    pub fn bulk_insert(
        &mut self,
        entries: &[GlossaryEntry],
    ) -> Result<usize, GlossaryDbError> {
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO glossary
                    (pattern, replacement, context, note, is_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entry in entries {
                let created_at = if entry.created_at > 0 { entry.created_at } else { now };
                let updated_at = if entry.updated_at > 0 { entry.updated_at } else { now };
                stmt.execute(params![
                    entry.pattern,
                    entry.replacement,
                    entry.context,
                    entry.note,
                    entry.is_regex as i32,
                    created_at,
                    updated_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(entries.len())
    }

    /// Codex review P2 #4 (Day 30): Overwrite mode の atomic 化。
    ///
    /// CSV import の Overwrite 戦略を 1 transaction で実行:
    ///   - 各 entry について同 pattern の既存 entry を SELECT
    ///   - 既存あり → UPDATE (id・created_at 保持・updated_at = now)
    ///   - 既存なし → INSERT (CSV の created_at/updated_at を保持・0 以下なら now)
    ///
    /// 戻り値: (added 件数, updated 件数)。
    /// 途中失敗時は全 rollback で「一部だけ上書きされた」状態を防ぐ。
    pub fn apply_import_overwrite(
        &mut self,
        entries: &[GlossaryEntry],
    ) -> Result<(usize, usize), GlossaryDbError> {
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        let mut added = 0usize;
        let mut updated = 0usize;
        {
            let mut find_stmt = tx.prepare(
                "SELECT id, created_at FROM glossary WHERE pattern = ?1 LIMIT 1",
            )?;
            let mut update_stmt = tx.prepare(
                "UPDATE glossary
                 SET replacement = ?2, context = ?3, note = ?4, is_regex = ?5, updated_at = ?6
                 WHERE id = ?1",
            )?;
            let mut insert_stmt = tx.prepare(
                "INSERT INTO glossary
                    (pattern, replacement, context, note, is_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entry in entries {
                let existing: Option<(i64, i64)> = find_stmt
                    .query_row(params![entry.pattern], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?;
                match existing {
                    Some((existing_id, _existing_created_at)) => {
                        update_stmt.execute(params![
                            existing_id,
                            entry.replacement,
                            entry.context,
                            entry.note,
                            entry.is_regex as i32,
                            now,
                        ])?;
                        updated += 1;
                    }
                    None => {
                        let created_at = if entry.created_at > 0 { entry.created_at } else { now };
                        let updated_at = if entry.updated_at > 0 { entry.updated_at } else { now };
                        insert_stmt.execute(params![
                            entry.pattern,
                            entry.replacement,
                            entry.context,
                            entry.note,
                            entry.is_regex as i32,
                            created_at,
                            updated_at,
                        ])?;
                        added += 1;
                    }
                }
            }
        }
        tx.commit()?;
        Ok((added, updated))
    }

    /// Codex review P2 #4 (Day 30): Skip mode の atomic 化。
    ///
    /// CSV import の Skip 戦略を 1 transaction で実行:
    ///   - 各 entry について同 pattern の既存 entry を SELECT
    ///   - 既存あり → 何もしない (skip 件数++)
    ///   - 既存なし → INSERT
    ///
    /// 戻り値: (added 件数, skipped 件数)。
    pub fn apply_import_skip(
        &mut self,
        entries: &[GlossaryEntry],
    ) -> Result<(usize, usize), GlossaryDbError> {
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        let mut added = 0usize;
        let mut skipped = 0usize;
        {
            let mut find_stmt =
                tx.prepare("SELECT 1 FROM glossary WHERE pattern = ?1 LIMIT 1")?;
            let mut insert_stmt = tx.prepare(
                "INSERT INTO glossary
                    (pattern, replacement, context, note, is_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entry in entries {
                let exists: bool = find_stmt
                    .query_row(params![entry.pattern], |_row| Ok(()))
                    .optional()?
                    .is_some();
                if exists {
                    skipped += 1;
                    continue;
                }
                let created_at = if entry.created_at > 0 { entry.created_at } else { now };
                let updated_at = if entry.updated_at > 0 { entry.updated_at } else { now };
                insert_stmt.execute(params![
                    entry.pattern,
                    entry.replacement,
                    entry.context,
                    entry.note,
                    entry.is_regex as i32,
                    created_at,
                    updated_at,
                ])?;
                added += 1;
            }
        }
        tx.commit()?;
        Ok((added, skipped))
    }

    /// Codex review P2 (Day 30): ReplaceAll の atomic 化。
    /// DELETE → INSERT を 1 transaction で実行し、INSERT 途中失敗時に DELETE もロールバック。
    /// SQLite I/O エラー・disk full 等で「既存辞書を消したまま新規も入らない」事故を防ぐ。
    /// 戻り値は (削除件数, 挿入件数)。
    pub fn delete_all_and_bulk_insert(
        &mut self,
        entries: &[GlossaryEntry],
    ) -> Result<(usize, usize), GlossaryDbError> {
        let now = current_epoch_secs();
        let tx = self.conn.transaction()?;
        let removed = tx.execute("DELETE FROM glossary", [])?;
        let inserted = entries.len();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO glossary
                    (pattern, replacement, context, note, is_regex, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entry in entries {
                let created_at = if entry.created_at > 0 { entry.created_at } else { now };
                let updated_at = if entry.updated_at > 0 { entry.updated_at } else { now };
                stmt.execute(params![
                    entry.pattern,
                    entry.replacement,
                    entry.context,
                    entry.note,
                    entry.is_regex as i32,
                    created_at,
                    updated_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok((removed, inserted))
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<GlossaryEntry> {
    let is_regex_int: i32 = row.get(5)?;
    Ok(GlossaryEntry {
        id: Some(row.get(0)?),
        pattern: row.get(1)?,
        replacement: row.get(2)?,
        context: row.get(3)?,
        note: row.get(4)?,
        is_regex: is_regex_int != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// pattern を「/」で split し trim + 空除外した sub-pattern 配列を返す。
/// Step 2.5: find_overlapping の共通ロジック。
fn split_sub_patterns(pattern: &str) -> Vec<String> {
    pattern
        .split('/')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn current_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `<data_dir>/RushVoice/glossary.db` のデフォルトパス。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
pub fn default_glossary_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("glossary.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_glossary_db_test_{}", name))
            .join("glossary.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn sample(pattern: &str, replacement: &str) -> GlossaryEntry {
        GlossaryEntry {
            id: None,
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            context: None,
            note: None,
            is_regex: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_init_and_list_empty() {
        let path = tmp_db("init_empty");
        let db = GlossaryDb::open(&path).expect("open 1");
        // 2 回 open しても壊れない (CREATE TABLE IF NOT EXISTS)
        let _db2 = GlossaryDb::open(&path).expect("open 2");
        assert_eq!(db.count().unwrap(), 0);
        assert!(db.list().unwrap().is_empty());
    }

    #[test]
    fn test_insert_and_list() {
        let path = tmp_db("insert_list");
        let db = GlossaryDb::open(&path).expect("open");
        let id1 = db.upsert(&sample("ますえく", "マツエク")).expect("insert 1");
        let id2 = db.upsert(&sample("いー松", "イータ")).expect("insert 2");
        assert!(id2 > id1);

        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 2);
        // created_at ASC + id ASC で 1 つ目が先頭
        assert_eq!(rows[0].id, Some(id1));
        assert_eq!(rows[0].pattern, "ますえく");
        assert_eq!(rows[0].replacement, "マツエク");
        assert_eq!(rows[1].id, Some(id2));
    }

    #[test]
    fn test_update_overwrites_same_id() {
        let path = tmp_db("update");
        let db = GlossaryDb::open(&path).expect("open");
        let id = db.upsert(&sample("foo", "bar")).expect("insert");
        // 同 id を指定して UPDATE
        let mut updated = sample("foo", "BAZ");
        updated.id = Some(id);
        updated.context = Some("メール".to_string());
        updated.is_regex = true;
        let returned = db.upsert(&updated).expect("update");
        assert_eq!(returned, id, "UPDATE は同じ id を返す");
        // 件数は増えない
        assert_eq!(db.count().unwrap(), 1);
        // 内容反映確認
        let got = db.get(id).unwrap().unwrap();
        assert_eq!(got.replacement, "BAZ");
        assert_eq!(got.context.as_deref(), Some("メール"));
        assert!(got.is_regex);
    }

    #[test]
    fn test_delete() {
        let path = tmp_db("delete");
        let db = GlossaryDb::open(&path).expect("open");
        let id1 = db.upsert(&sample("残す", "残す!")).expect("insert 1");
        let id2 = db.upsert(&sample("消す", "消す!")).expect("insert 2");
        db.delete(id2).expect("delete");
        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, Some(id1));
        // 存在しない id 削除は OK (楽観的更新と整合)
        db.delete(999_999).expect("delete nonexistent");
    }

    #[test]
    fn test_find_duplicate_with_and_without_exclude() {
        let path = tmp_db("dup");
        let db = GlossaryDb::open(&path).expect("open");
        let id1 = db.upsert(&sample("dup_key", "v1")).expect("insert 1");
        let id2 = db.upsert(&sample("dup_key", "v2")).expect("insert 2");

        // exclude なし → 1 件目 (id ASC LIMIT 1) を検出
        let found = db.find_duplicate("dup_key", None).expect("find");
        assert!(found.is_some());
        let f = found.unwrap();
        assert_eq!(f.id, Some(id1));

        // exclude=id1 で id2 のみ検出
        let found2 = db.find_duplicate("dup_key", Some(id1)).expect("find exclude");
        assert_eq!(found2.unwrap().id, Some(id2));

        // exclude=id1 かつ pattern 一致なし
        let none = db.find_duplicate("nonexistent", Some(id1)).expect("none");
        assert!(none.is_none());

        // 自分自身 (id2) も exclude すれば検出 0 件
        let _ = db.delete(id1).unwrap();
        let none2 = db.find_duplicate("dup_key", Some(id2)).expect("none2");
        assert!(none2.is_none());
    }

    #[test]
    fn test_find_overlapping_single_match() {
        // Step 2.5: 既存「たかなし」 + 新規「たかなし/たかな」 → 「たかなし」検出
        let path = tmp_db("overlap_single");
        let db = GlossaryDb::open(&path).expect("open");
        let id1 = db.upsert(&sample("たかなし", "高梨")).expect("insert 1");

        let results = db
            .find_overlapping("たかなし/たかな", None)
            .expect("find_overlapping");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.id, Some(id1));
        assert_eq!(results[0].overlapping_sub_patterns, vec!["たかなし".to_string()]);
    }

    #[test]
    fn test_find_overlapping_multi_match() {
        // 既存「たかなし/たかな」 + 新規「たかな/のだ/たかなし」 → 両方検出
        let path = tmp_db("overlap_multi");
        let db = GlossaryDb::open(&path).expect("open");
        let id1 = db
            .upsert(&sample("たかなし/たかな", "高梨"))
            .expect("insert 1");

        let results = db
            .find_overlapping("たかな/のだ/たかなし", None)
            .expect("find_overlapping");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.id, Some(id1));
        // query 側の出現順で交差を返す
        assert_eq!(
            results[0].overlapping_sub_patterns,
            vec!["たかな".to_string(), "たかなし".to_string()]
        );
    }

    #[test]
    fn test_find_overlapping_no_match() {
        // 重複なしのとき空配列
        let path = tmp_db("overlap_none");
        let db = GlossaryDb::open(&path).expect("open");
        let _ = db.upsert(&sample("ますえく", "マツエク")).expect("insert");

        let results = db
            .find_overlapping("たかなし/たかな", None)
            .expect("find_overlapping");
        assert!(results.is_empty());

        // exclude_id で自分自身を除外したケースも空
        let id2 = db.upsert(&sample("foo/bar", "FB")).expect("insert 2");
        let results2 = db
            .find_overlapping("foo/bar", Some(id2))
            .expect("find_overlapping exclude self");
        assert!(results2.is_empty());
    }

    // ----- v0.1.10 機能 E: find_by_pattern / delete_all / bulk_insert -----

    #[test]
    fn test_find_by_pattern_exact_match() {
        let path = tmp_db("find_by_pattern");
        let db = GlossaryDb::open(&path).expect("open");
        let id = db.upsert(&sample("ますえく", "マツエク")).expect("insert");

        // 完全一致でヒット
        let got = db.find_by_pattern("ますえく").expect("find");
        assert!(got.is_some());
        assert_eq!(got.unwrap().id, Some(id));

        // 部分一致しない (CSV import の重複判定は厳密一致のみ)
        let none = db.find_by_pattern("ますえ").expect("partial");
        assert!(none.is_none());

        // 大文字小文字区別
        let _ = db.upsert(&sample("FOO", "foo-rep")).expect("upper");
        let upper = db.find_by_pattern("FOO").expect("upper found");
        assert!(upper.is_some());
        let lower = db.find_by_pattern("foo").expect("lower not found");
        assert!(lower.is_none());
    }

    #[test]
    fn test_delete_all_removes_everything() {
        let path = tmp_db("delete_all");
        let db = GlossaryDb::open(&path).expect("open");
        for i in 0..5 {
            db.upsert(&sample(&format!("p{}", i), &format!("r{}", i)))
                .expect("insert");
        }
        assert_eq!(db.count().unwrap(), 5);

        let removed = db.delete_all().expect("delete_all");
        assert_eq!(removed, 5);
        assert_eq!(db.count().unwrap(), 0);
        assert!(db.list().unwrap().is_empty());

        // 空 DB に対する delete_all は 0 件・エラーにならない
        let again = db.delete_all().expect("delete_all empty");
        assert_eq!(again, 0);
    }

    #[test]
    fn test_bulk_insert_basic() {
        let path = tmp_db("bulk_insert");
        let mut db = GlossaryDb::open(&path).expect("open");
        let entries = vec![
            sample("a", "A"),
            sample("b", "B"),
            sample("c", "C"),
        ];
        let n = db.bulk_insert(&entries).expect("bulk_insert");
        assert_eq!(n, 3);
        assert_eq!(db.count().unwrap(), 3);
        // 順序 (created_at ASC, id ASC) で取得
        let rows = db.list().expect("list");
        let patterns: Vec<&str> = rows.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_bulk_insert_preserves_timestamps_when_positive() {
        // export → import round-trip: created_at / updated_at に既存値があれば保持する
        let path = tmp_db("bulk_insert_ts");
        let mut db = GlossaryDb::open(&path).expect("open");
        let mut e = sample("x", "X");
        e.created_at = 1_700_000_000;
        e.updated_at = 1_700_000_100;
        db.bulk_insert(&[e]).expect("bulk_insert");
        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_at, 1_700_000_000);
        assert_eq!(rows[0].updated_at, 1_700_000_100);
    }

    #[test]
    fn test_apply_import_overwrite_basic() {
        let path = tmp_db("import_overwrite");
        let mut db = GlossaryDb::open(&path).expect("open");
        let _ = db.upsert(&sample("existing", "OLD")).expect("seed");

        let mut new_entry = sample("brandnew", "NEW");
        new_entry.created_at = 1_700_000_000;
        new_entry.updated_at = 1_700_000_100;
        let entries = vec![
            // 既存と pattern 一致 → UPDATE
            sample("existing", "UPDATED_VALUE"),
            // 新規 → INSERT (timestamp 保持)
            new_entry,
        ];
        let (added, updated) = db.apply_import_overwrite(&entries).expect("overwrite");
        assert_eq!(added, 1);
        assert_eq!(updated, 1);
        assert_eq!(db.count().unwrap(), 2);

        // 既存 entry が UPDATE されている (id 保持・replacement 変更)
        let ex = db.find_by_pattern("existing").unwrap().unwrap();
        assert_eq!(ex.replacement, "UPDATED_VALUE");

        // 新規 entry の timestamp が保持されている
        let nw = db.find_by_pattern("brandnew").unwrap().unwrap();
        assert_eq!(nw.created_at, 1_700_000_000);
        assert_eq!(nw.updated_at, 1_700_000_100);
    }

    #[test]
    fn test_apply_import_skip_basic() {
        let path = tmp_db("import_skip");
        let mut db = GlossaryDb::open(&path).expect("open");
        let _ = db.upsert(&sample("existing", "KEEP_ME")).expect("seed");

        let entries = vec![
            sample("existing", "WILL_BE_SKIPPED"),
            sample("brandnew", "INSERTED"),
        ];
        let (added, skipped) = db.apply_import_skip(&entries).expect("skip");
        assert_eq!(added, 1);
        assert_eq!(skipped, 1);

        // 既存はそのまま
        let ex = db.find_by_pattern("existing").unwrap().unwrap();
        assert_eq!(ex.replacement, "KEEP_ME");
        // 新規は入っている
        let nw = db.find_by_pattern("brandnew").unwrap().unwrap();
        assert_eq!(nw.replacement, "INSERTED");
    }

    #[test]
    fn test_delete_all_and_bulk_insert_atomic() {
        let path = tmp_db("delete_all_atomic");
        let mut db = GlossaryDb::open(&path).expect("open");
        let _ = db.upsert(&sample("old1", "OLD1")).expect("insert old");
        let _ = db.upsert(&sample("old2", "OLD2")).expect("insert old");

        let new_entries = vec![sample("new1", "NEW1"), sample("new2", "NEW2"), sample("new3", "NEW3")];
        let (removed, inserted) = db
            .delete_all_and_bulk_insert(&new_entries)
            .expect("atomic replace");
        assert_eq!(removed, 2);
        assert_eq!(inserted, 3);

        let rows = db.list().expect("list");
        let patterns: Vec<&str> = rows.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(patterns, vec!["new1", "new2", "new3"]);
        // 古い entries は完全消滅
        assert!(db.find_by_pattern("old1").unwrap().is_none());
        assert!(db.find_by_pattern("old2").unwrap().is_none());
    }

    #[test]
    fn test_delete_all_and_bulk_insert_empty_entries_clears_db() {
        // 空 entries で呼ぶと既存削除のみ実行・件数は (n, 0)
        let path = tmp_db("delete_all_atomic_empty");
        let mut db = GlossaryDb::open(&path).expect("open");
        let _ = db.upsert(&sample("x", "X")).expect("insert");
        let (removed, inserted) = db.delete_all_and_bulk_insert(&[]).expect("atomic empty");
        assert_eq!(removed, 1);
        assert_eq!(inserted, 0);
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_bulk_insert_empty_is_ok() {
        let path = tmp_db("bulk_insert_empty");
        let mut db = GlossaryDb::open(&path).expect("open");
        let n = db.bulk_insert(&[]).expect("empty bulk_insert");
        assert_eq!(n, 0);
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_special_chars_escaped() {
        // SQL injection 防御 (params! が処理することを検証)
        let path = tmp_db("escape");
        let db = GlossaryDb::open(&path).expect("open");
        let tricky_pattern = "It's a \"test\"; DROP TABLE glossary;--";
        let tricky_replacement = "'malicious' \"value\"";
        let id = db
            .upsert(&sample(tricky_pattern, tricky_replacement))
            .expect("insert tricky");
        let got = db.get(id).unwrap().unwrap();
        assert_eq!(got.pattern, tricky_pattern);
        assert_eq!(got.replacement, tricky_replacement);
        // テーブルが残っていること
        assert_eq!(db.count().unwrap(), 1);
    }
}
