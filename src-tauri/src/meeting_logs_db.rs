// meeting_logs_db.rs — v0.1.16 機能 L (議事録モード) Day 1: meeting_logs SQLite 永続化
//
// SSOT: 内部設計メモ §機能L 4. データ保持
// 委任 prompt: 内部設計メモ §2.1
//
// 設計方針:
// - profile_db.rs / history_db.rs / app_rules_db.rs と同じパターン踏襲 (rusqlite + WAL)。
// - 配置: %APPDATA%/RushVoice/meeting_logs.db (history.db と同じ親ディレクトリ)。
// - migration は単一トランザクション (L188): 早期 return で自動 rollback。
// - seed は不要 (議事録は user 操作で発生・builtin プリセットなし)。
// - CRUD 5 本 (insert / get / list / delete / update_segments) のみ。
//   update_segments は Day 3 で segments_json と duration_ms をまとめて書き戻す用途。
// - 一覧は start_at DESC (新しい録音を上に表示)。
// - id は外部 (commands 層) で生成して渡す。Day 1 では UUID v4 風の 32 hex 文字列を
//   タイムスタンプ + atomic counter から組み立てる (uuid crate 依存追加を避ける)。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeetingLogsDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("バリデーション失敗: {0}")]
    Validation(String),
}

/// 議事録ログ 1 件。
///
/// snake_case で統一 (frontend type/meeting.ts と整合させる予定・Day 4 で定義)。
///
/// - `id`: 外部生成の文字列識別子 (Day 1 では 32 hex 文字列・将来 UUID v4 に置換可)。
/// - `start_at`: 録音開始時刻 (Unix epoch seconds, UTC)。一覧の DESC ソートキー。
/// - `duration_ms`: 録音長さ (ms)。Day 1 では INSERT 時 0、Day 3 で update_segments と
///   同時に確定値を書き戻す。
/// - `title`: 議事録タイトル (Day 1 では空可・Day 4 で UI 編集を実装)。
/// - `segments_json`: Vec<MeetingSegment> シリアライズ済 JSON 文字列。Day 1 では "[]"。
///   Day 3 で transcribe 結果から構築して update_segments で書き戻す。
/// - `chunk_dir`: 5 分チャンク WAV の保存先ディレクトリ (絶対パス)。Day 2 で
///   実際の書き出し処理を実装。Day 1 では空文字または `meeting_start` で生成した
///   `%APPDATA%/RushVoice/meetings/<id>/` を入れて OK。
/// - `created_at`: INSERT 時刻 (Unix epoch seconds, UTC)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingLogRow {
    pub id: String,
    pub start_at: i64,
    pub duration_ms: i64,
    pub title: String,
    pub segments_json: String,
    pub chunk_dir: String,
    pub created_at: i64,
}

pub struct MeetingLogsDb {
    conn: Connection,
}

impl MeetingLogsDb {
    /// open: ディレクトリ自動作成 + WAL モード + migration を単一トランザクションで実行。
    /// migration が途中で失敗した場合は rollback され、DB は中間状態にならない (L188)。
    pub fn open(path: &Path) -> Result<Self, MeetingLogsDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = MeetingLogsDb { conn };
        db.migrate_in_tx()?;
        Ok(db)
    }

    /// L188: migration を単一トランザクションでラップ。
    ///
    /// v1: CREATE TABLE + CREATE INDEX。
    /// v2 (v0.1.16 精度向上): LLM 後処理済の段落本文を保持する `refined_json` 列を
    ///     ALTER TABLE ADD COLUMN で追加 (既存行は既定値 '' = 「LLM 整形なし」)。
    ///     ALTER は列が既存だと Err になるため PRAGMA table_info で存在チェックしてから実行。
    ///     列追加が失敗しても tx 全体が rollback され DB は中間状態にならない (L188)。
    fn migrate_in_tx(&mut self) -> Result<(), MeetingLogsDbError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meeting_logs (
                id TEXT PRIMARY KEY,
                start_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                title TEXT NOT NULL DEFAULT '',
                segments_json TEXT NOT NULL DEFAULT '[]',
                chunk_dir TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_meeting_logs_start_at
                ON meeting_logs(start_at DESC);
            "#,
        )?;
        // v2: refined_json 列を冪等に追加 (PRAGMA で存在確認してから ALTER)。
        if !Self::column_exists(&tx, "meeting_logs", "refined_json")? {
            tx.execute_batch(
                "ALTER TABLE meeting_logs ADD COLUMN refined_json TEXT NOT NULL DEFAULT '';",
            )?;
        }
        // v3 (Day 39 履歴リンク機能): 最後に md 形式で export した絶対パスを保持する
        // `export_md_path` 列を追加。空文字 = 「未 export」(frontend 側で「開く」ボタン
        // を無効化する判定に使う)。meeting_export 完了時に format=md なら自動 update。
        if !Self::column_exists(&tx, "meeting_logs", "export_md_path")? {
            tx.execute_batch(
                "ALTER TABLE meeting_logs ADD COLUMN export_md_path TEXT NOT NULL DEFAULT '';",
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// PRAGMA table_info で指定テーブルに指定列が存在するか判定する (migration ガード用)。
    fn column_exists(
        conn: &Connection,
        table: &str,
        column: &str,
    ) -> Result<bool, MeetingLogsDbError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?; // table_info の 2 列目 = name
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 議事録ログを INSERT。id が既存と衝突した場合は SQLite UNIQUE 違反 Err。
    pub fn insert(&self, row: &MeetingLogRow) -> Result<(), MeetingLogsDbError> {
        if row.id.trim().is_empty() {
            return Err(MeetingLogsDbError::Validation("id が空です".into()));
        }
        self.conn.execute(
            "INSERT INTO meeting_logs
                (id, start_at, duration_ms, title, segments_json, chunk_dir, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id,
                row.start_at,
                row.duration_ms,
                row.title,
                row.segments_json,
                row.chunk_dir,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    /// id 指定で 1 件取得。
    pub fn get(&self, id: &str) -> Result<Option<MeetingLogRow>, MeetingLogsDbError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, start_at, duration_ms, title, segments_json, chunk_dir, created_at
                 FROM meeting_logs WHERE id = ?1",
                params![id],
                row_to_meeting_log,
            )
            .optional()?;
        Ok(row)
    }

    /// 一覧取得 (start_at DESC)。
    /// - `limit`: 0 のときは LIMIT を付けない (全件)。
    /// - `offset`: スキップ件数。
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<MeetingLogRow>, MeetingLogsDbError> {
        let mut sql = String::from(
            "SELECT id, start_at, duration_ms, title, segments_json, chunk_dir, created_at
             FROM meeting_logs
             ORDER BY start_at DESC, id DESC",
        );
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {} OFFSET {}", limit, offset));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_meeting_log)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 1 件削除。true = 削除成功 / false = 該当 id なし。
    pub fn delete(&self, id: &str) -> Result<bool, MeetingLogsDbError> {
        let n = self
            .conn
            .execute("DELETE FROM meeting_logs WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Day 3 で使う: 転写完了後に segments_json と duration_ms をまとめて更新。
    /// 該当 id がなければ何もせず Ok(()) (rusqlite::execute は 0 行更新でも Ok)。
    pub fn update_segments(
        &self,
        id: &str,
        segments_json: &str,
        duration_ms: i64,
    ) -> Result<(), MeetingLogsDbError> {
        self.conn.execute(
            "UPDATE meeting_logs
             SET segments_json = ?2, duration_ms = ?3
             WHERE id = ?1",
            params![id, segments_json, duration_ms],
        )?;
        Ok(())
    }

    /// v0.1.16 精度向上 ブロック3: LLM 後処理済の段落本文 (Vec<Paragraph> JSON) を書き戻す。
    ///
    /// `segments_json` (辞書適用済の生 segment・SRT 用) とは別列で保持し、
    /// txt/md エクスポート時に refined を優先表示する。該当 id がなければ何もせず Ok(())。
    pub fn update_refined(
        &self,
        id: &str,
        refined_json: &str,
    ) -> Result<(), MeetingLogsDbError> {
        self.conn.execute(
            "UPDATE meeting_logs SET refined_json = ?2 WHERE id = ?1",
            params![id, refined_json],
        )?;
        Ok(())
    }

    /// v0.1.16 精度向上 ブロック3: refined_json 列を取得する。
    /// 該当 id なし or 列が空文字なら `Ok(None)` (= LLM 整形版なし → 呼出側で segments から段落化)。
    pub fn get_refined(&self, id: &str) -> Result<Option<String>, MeetingLogsDbError> {
        let refined: Option<String> = self
            .conn
            .query_row(
                "SELECT refined_json FROM meeting_logs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match refined {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        })
    }

    /// Day 39 履歴リンク機能: md 形式で export した最後の絶対パスを保存する。
    /// 該当 id がなければ何もせず Ok(())。同一 id を再 export すると上書き。
    pub fn update_export_md_path(
        &self,
        id: &str,
        export_md_path: &str,
    ) -> Result<(), MeetingLogsDbError> {
        self.conn.execute(
            "UPDATE meeting_logs SET export_md_path = ?2 WHERE id = ?1",
            params![id, export_md_path],
        )?;
        Ok(())
    }

    /// Day 39 履歴リンク機能: 保存済みの md export 絶対パスを取得する。
    /// 該当 id なし or 列が空文字なら `Ok(None)` (= 未 export → frontend で「開く」無効化)。
    pub fn get_export_md_path(&self, id: &str) -> Result<Option<String>, MeetingLogsDbError> {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT export_md_path FROM meeting_logs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match path {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        })
    }

    /// Day 2 で使う: 録音停止時に duration_ms だけ更新する (segments_json は Day 3 で書き戻し)。
    /// `update_segments(id, "[]", duration_ms)` で代用も可能だが、Day 2 段階で
    /// segments_json を "[]" に上書きする副作用を避けるため専用関数を用意。
    /// 該当 id がなければ何もせず Ok(()) (rusqlite::execute は 0 行更新でも Ok)。
    pub fn update_duration_only(
        &self,
        id: &str,
        duration_ms: i64,
    ) -> Result<(), MeetingLogsDbError> {
        self.conn.execute(
            "UPDATE meeting_logs
             SET duration_ms = ?2
             WHERE id = ?1",
            params![id, duration_ms],
        )?;
        Ok(())
    }

    /// 総件数 (UI 用)。
    pub fn count(&self) -> Result<i64, MeetingLogsDbError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM meeting_logs", [], |row| row.get(0))?;
        Ok(n)
    }
}

fn row_to_meeting_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<MeetingLogRow> {
    Ok(MeetingLogRow {
        id: row.get(0)?,
        start_at: row.get(1)?,
        duration_ms: row.get(2)?,
        title: row.get(3)?,
        segments_json: row.get(4)?,
        chunk_dir: row.get(5)?,
        created_at: row.get(6)?,
    })
}

/// `<data_dir>/RushVoice/meeting_logs.db` のデフォルトパス。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
pub fn default_meeting_logs_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("meeting_logs.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_meeting_logs_db_test_{}", name))
            .join("meeting_logs.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn sample_row(id: &str, start_at: i64) -> MeetingLogRow {
        MeetingLogRow {
            id: id.to_string(),
            start_at,
            duration_ms: 0,
            title: String::new(),
            segments_json: "[]".to_string(),
            chunk_dir: String::new(),
            created_at: start_at,
        }
    }

    /// open: migration 成功 + 0 件で始まる。
    #[test]
    fn test_open_creates_empty_table() {
        let path = tmp_db("open_empty");
        let db = MeetingLogsDb::open(&path).expect("open");
        assert_eq!(db.count().unwrap(), 0, "初期状態は 0 件");
        // テーブルが存在することを SELECT で間接確認
        let list = db.list(0, 0).expect("list empty");
        assert!(list.is_empty());
    }

    /// open: 冪等 (2 回呼んでも追加副作用なし)。
    #[test]
    fn test_open_is_idempotent() {
        let path = tmp_db("open_idempotent");
        {
            let _db = MeetingLogsDb::open(&path).expect("open 1");
        }
        let db = MeetingLogsDb::open(&path).expect("open 2");
        assert_eq!(db.count().unwrap(), 0);
    }

    /// insert → get のラウンドトリップ。
    #[test]
    fn test_insert_and_get_roundtrip() {
        let path = tmp_db("insert_get");
        let db = MeetingLogsDb::open(&path).expect("open");
        let row = sample_row("abc123", 1_700_000_000);
        db.insert(&row).expect("insert");

        let got = db.get("abc123").expect("get").unwrap();
        assert_eq!(got, row, "INSERT/SELECT のラウンドトリップで一致");
        // 存在しない id は None
        assert!(db.get("not_exist").unwrap().is_none());
    }

    /// list: start_at DESC で並ぶ。
    #[test]
    fn test_list_order_start_at_desc() {
        let path = tmp_db("list_order");
        let db = MeetingLogsDb::open(&path).expect("open");
        // 投入順は古い→新しい・list は新しい→古い順で返る想定
        db.insert(&sample_row("old", 1_000)).expect("insert old");
        db.insert(&sample_row("mid", 2_000)).expect("insert mid");
        db.insert(&sample_row("new", 3_000)).expect("insert new");

        let list = db.list(0, 0).expect("list");
        let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old"], "start_at DESC で並ぶ");
    }

    /// list: limit/offset が効く。
    #[test]
    fn test_list_limit_offset() {
        let path = tmp_db("list_limit");
        let db = MeetingLogsDb::open(&path).expect("open");
        for i in 0..5 {
            db.insert(&sample_row(&format!("id_{}", i), 1_000 + i))
                .expect("insert");
        }
        // limit=2: 最新 2 件
        let page1 = db.list(2, 0).expect("page1");
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].id, "id_4");
        assert_eq!(page1[1].id, "id_3");
        // offset=2 limit=2: 3-4 件目
        let page2 = db.list(2, 2).expect("page2");
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].id, "id_2");
        assert_eq!(page2[1].id, "id_1");
    }

    /// insert → delete → get None.
    #[test]
    fn test_insert_delete_get_none() {
        let path = tmp_db("insert_delete");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("xyz", 1_700_000_000)).expect("insert");
        assert!(db.delete("xyz").unwrap(), "削除成功 = true");
        assert!(db.get("xyz").unwrap().is_none(), "削除後は get None");
        // 存在しない id の delete は false
        assert!(!db.delete("xyz").unwrap(), "再削除は false");
    }

    /// migration の rollback 検証 (異常系):
    /// 中間状態 (壊れたテーブル) で open() を呼ぶと migrate_in_tx は IF NOT EXISTS で
    /// 既存テーブルをそのまま保持する (= 何も書き換えない) ことを確認。
    /// L188 の rollback ガードは将来の DROP/ALTER を含む migration で意味を持つ。
    /// ここでは「migration が冪等で副作用 free」を間接的に検証する。
    #[test]
    fn test_migrate_in_tx_idempotent_on_existing_schema() {
        let path = tmp_db("migrate_idempotent");
        // 1) 一度 open してテーブル作成 + 1 行投入
        {
            let db = MeetingLogsDb::open(&path).expect("open 1");
            db.insert(&sample_row("seed", 12345)).expect("insert");
        }
        // 2) 2 度目 open: migration 再走するが既存行は壊れない
        let db = MeetingLogsDb::open(&path).expect("open 2");
        let got = db.get("seed").expect("get").unwrap();
        assert_eq!(got.start_at, 12345);
        assert_eq!(db.count().unwrap(), 1, "再 open でも 1 件のまま");
    }

    /// update_segments: 既存行の segments_json と duration_ms を上書きする。
    #[test]
    fn test_update_segments_overwrites() {
        let path = tmp_db("update_segments");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("rec1", 1_700_000_000))
            .expect("insert");
        db.update_segments("rec1", r#"[{"text":"こんにちは"}]"#, 3500)
            .expect("update");
        let got = db.get("rec1").expect("get").unwrap();
        assert_eq!(got.segments_json, r#"[{"text":"こんにちは"}]"#);
        assert_eq!(got.duration_ms, 3500);
        // 他フィールドは不変
        assert_eq!(got.start_at, 1_700_000_000);
        assert_eq!(got.title, "");
    }

    /// insert: 空 id は Validation Err。
    #[test]
    fn test_insert_empty_id_rejected() {
        let path = tmp_db("empty_id");
        let db = MeetingLogsDb::open(&path).expect("open");
        let row = sample_row("", 1_000);
        let err = db.insert(&row).expect_err("空 id は Err");
        match err {
            MeetingLogsDbError::Validation(_) => {}
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
    }

    /// update_duration_only: duration_ms のみ更新し segments_json は維持する。
    #[test]
    fn test_update_duration_only_preserves_segments() {
        let path = tmp_db("update_duration_only");
        let db = MeetingLogsDb::open(&path).expect("open");
        // 先に segments_json をセットしてから duration_ms 単独更新
        db.insert(&sample_row("rec_d", 1_700_000_000))
            .expect("insert");
        db.update_segments("rec_d", r#"[{"text":"前段"}]"#, 1000)
            .expect("update_segments");
        // duration_only で 1000 → 4500 に更新するが segments_json は不変
        db.update_duration_only("rec_d", 4500)
            .expect("update_duration_only");
        let got = db.get("rec_d").expect("get").unwrap();
        assert_eq!(got.duration_ms, 4500, "duration_ms 上書き");
        assert_eq!(
            got.segments_json, r#"[{"text":"前段"}]"#,
            "segments_json は不変"
        );
    }

    /// v0.1.16: refined_json 列の round-trip (update_refined → get_refined)。
    #[test]
    fn test_update_and_get_refined() {
        let path = tmp_db("refined_roundtrip");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("rec_r", 1_700_000_000)).expect("insert");
        // 初期は refined なし (空文字 → None)
        assert!(db.get_refined("rec_r").unwrap().is_none(), "初期は None");
        // 書き込み後は取得できる
        db.update_refined("rec_r", r#"[{"start_ms":0,"end_ms":1000,"text":"整形済"}]"#)
            .expect("update_refined");
        let got = db.get_refined("rec_r").expect("get_refined").unwrap();
        assert!(got.contains("整形済"));
        // 存在しない id は None
        assert!(db.get_refined("nope").unwrap().is_none());
    }

    /// v0.1.16: refined_json 列が migration v2 で追加され既存行は既定値 '' になる。
    #[test]
    fn test_refined_column_default_empty_for_existing_rows() {
        let path = tmp_db("refined_default");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("rec_d", 1_700_000_000)).expect("insert");
        // 既定値 '' は get_refined で None に正規化される
        assert!(db.get_refined("rec_d").unwrap().is_none());
        // 再 open しても列追加は冪等 (ALTER をスキップ) で壊れない
        let db2 = MeetingLogsDb::open(&path).expect("open 2");
        assert!(db2.get_refined("rec_d").unwrap().is_none());
    }

    /// insert: id 重複は SQLite UNIQUE 違反 Err。
    #[test]
    fn test_insert_duplicate_id_rejected() {
        let path = tmp_db("dup_id");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("same", 1_000)).expect("insert 1");
        let err = db
            .insert(&sample_row("same", 2_000))
            .expect_err("重複 id は Err");
        match err {
            MeetingLogsDbError::Sqlite(_) => {}
            other => panic!("Sqlite Err 想定 / 実際: {:?}", other),
        }
        // 1 件目は維持される
        let got = db.get("same").expect("get").unwrap();
        assert_eq!(got.start_at, 1_000);
    }

    /// Day 39 履歴リンク機能: export_md_path 列の round-trip。
    #[test]
    fn test_update_and_get_export_md_path() {
        let path = tmp_db("export_md_path_roundtrip");
        let db = MeetingLogsDb::open(&path).expect("open");
        db.insert(&sample_row("rec_e", 1_700_000_000))
            .expect("insert");
        // 初期は path なし (空文字 → None)
        assert!(
            db.get_export_md_path("rec_e").unwrap().is_none(),
            "初期は None"
        );
        // path を保存
        db.update_export_md_path(
            "rec_e",
            "C:\\Users\\user\\Documents\\RushVoice議事録\\meeting_20260603_120000_test.md",
        )
        .expect("update_export_md_path");
        // 取得
        let got = db
            .get_export_md_path("rec_e")
            .expect("get_export_md_path")
            .unwrap();
        assert!(got.ends_with("meeting_20260603_120000_test.md"));
        // 別 path で上書き
        db.update_export_md_path("rec_e", "/Users/user/Documents/RushVoice議事録/other.md")
            .expect("update overwrite");
        let got2 = db.get_export_md_path("rec_e").unwrap().unwrap();
        assert!(got2.ends_with("other.md"));
        // 該当 id なしは None (Err にしない)
        assert!(db.get_export_md_path("nope").unwrap().is_none());
    }

    /// Day 39: v3 migration が既存 v2 DB に対しても冪等 (再 open しても列追加 Err にならない)。
    #[test]
    fn test_export_md_path_migration_idempotent() {
        let path = tmp_db("export_md_path_idempotent");
        {
            let db = MeetingLogsDb::open(&path).expect("open 1");
            db.insert(&sample_row("rec_i", 1_700_000_000))
                .expect("insert");
            db.update_export_md_path("rec_i", "/tmp/test.md")
                .expect("update");
        }
        // 2 度目 open: migration 再走するが既存値は壊れない
        let db2 = MeetingLogsDb::open(&path).expect("open 2");
        let got = db2.get_export_md_path("rec_i").unwrap().unwrap();
        assert_eq!(got, "/tmp/test.md", "再 open で path 維持");
    }
}
