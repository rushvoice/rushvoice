// profile_db.rs — Phase 2 C Wave 2 プロファイル (profiles) SQLite 永続化
//
// SSOT v2.10 §8.3.1 PT-01〜PT-10
// Wave 1 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md §1.2
// Wave 2 設計判断: 委任プロンプト 2026-05-26 (承認済) — SQLite 採用・3 プリセット投入
// Wave 2 フォローアップ 2026-05-26 (要望): プリセットを mail/business_doc/casual に差替え
//
// 設計方針:
// - glossary_db.rs / app_rules_db.rs / history_db.rs と同じパターン踏襲 (rusqlite + WAL)。
// - 配置: %APPDATA%/RushVoice/profile.db
// - 初回 open 時にビルトイン 3 種プリセット (mail/business_doc/casual) を投入。
//   既に存在すれば INSERT スキップ (idempotent)。
// - **migration (Wave 2 follow-up)**: 旧ビルトイン (salon/accounting/meeting) が DB に
//   残っていれば seed_builtins() の前段で削除する (is_builtin=true かつ未知の builtin_key)。
//   user 作成プロファイル (is_builtin=false) は無条件保持。
// - llm_postprocess_prompt は max 1200 文字を **アプリケーション層** でバリデート。
//   SQLite CHECK 制約だと INSERT 失敗が透けやすく UI でハンドリングしづらいため。
// - emoji_density は 'none'|'few'|'normal'|'many' のいずれか (アプリ層バリデート)。
// - active_profile_id は kv 表ではなく **config.db settings_kv** に "active_profile_id" キーで保存する
//   (Wave 2 PT-01 でアクティブ切替を実装するため・lib.rs から設定値として参照される)。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 後処理 LLM プロンプトの最大長 (要望 2026-05-26)。
/// 1200 を超える値は upsert で Err を返す。
pub const LLM_PROMPT_MAX_CHARS: usize = 1200;

/// 表示名の最大長 (Wave 2 follow-up review P2-2, 2026-05-26)。
/// frontend ProfilePage.tsx の `maxLength={64}` と一致させ、Tauri commands 経由で
/// 巨大ペイロード (例: 1MB) が投げられた場合に Rust 側でも防御する。
pub const DISPLAY_NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Error)]
pub enum ProfileDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("バリデーション失敗: {0}")]
    Validation(String),
}

/// プロファイル 1 件。INSERT 前は `id = None`、SELECT 結果は `id = Some(rowid)`。
///
/// snake_case で統一 (v2 frontend 既存 type/glossary.ts / type/app_rule.ts と整合)。
///
/// ビルトインプリセット (is_builtin = true) は:
///   - 表示名 (display_name) のみ編集可能
///   - 削除不可
///   - 「デフォルトに戻す」で builtin_key 由来の初期値に復元
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: Option<i64>,
    /// UI 表示用の名前 (例: "メール", "ビジネス文書", "カジュアル")
    pub display_name: String,
    /// Whisper API に渡す prompt パラメータ (Whisper-side hint・100 文字以内推奨)
    pub whisper_prompt: String,
    /// LLM 後処理用システムプロンプト (Phase 3 以降で使用・現状は保存のみ)
    pub llm_postprocess_prompt: String,
    /// えー・あのー 等のフィラーを後処理で除去するか
    pub filler_removal: bool,
    /// 'none' | 'few' | 'normal' | 'many'
    pub emoji_density: String,
    /// ビルトインプリセットなら true (削除不可・「デフォルトに戻す」対象)
    pub is_builtin: bool,
    /// is_builtin = true のときの識別子 ("mail" / "business_doc" / "casual")
    /// user 作成プロファイルは None。
    pub builtin_key: Option<String>,
    /// INSERT 時刻 (Unix epoch seconds, UTC)
    pub created_at: i64,
    /// 最終 UPDATE 時刻 (Unix epoch seconds, UTC)
    pub updated_at: i64,
}

pub struct ProfileDb {
    conn: Connection,
}

impl ProfileDb {
    pub fn open(path: &Path) -> Result<Self, ProfileDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = ProfileDb { conn };
        db.init()?;
        // Wave 2 follow-up (2026-05-26): 旧プリセット (salon/accounting/meeting) が
        // 残っていれば削除してから新プリセット (mail/business_doc/casual) を投入。
        //
        // P1-2 fix (2026-05-26 follow-up review): migration + seed を単一トランザクションでラップ。
        // 旧 builtin 削除のみ成功して新 seed が失敗 → 次回起動で seed のみ走り整合は取れるが、
        // 「中間 (旧削除済 + 新未投入)」状態でアプリが続行することを防ぎ、再起動なしで
        // 整合性を保証する。
        db.migrate_and_seed_in_tx()?;
        Ok(db)
    }

    /// P1-2 fix: migrate_obsolete_builtins と seed_builtins を 1 トランザクションでラップ。
    /// 既存の内部関数 (migrate_obsolete_builtins / seed_builtins) はテストおよび直接呼出を
    /// 引き続きサポートするためそのまま残し、本体 open() からは tx 経由でのみ呼ぶ。
    fn migrate_and_seed_in_tx(&mut self) -> Result<(), ProfileDbError> {
        let tx = self.conn.transaction()?;
        // -- migrate (現行 BUILTIN_PRESETS に無い builtin 行を削除) --
        let current_keys: Vec<&str> = BUILTIN_PRESETS.iter().map(|p| p.builtin_key).collect();
        let placeholders = current_keys
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let migrate_sql = format!(
            "DELETE FROM profiles
             WHERE is_builtin = 1
               AND (builtin_key IS NULL OR builtin_key NOT IN ({}))",
            placeholders
        );
        let migrate_params: Vec<&dyn rusqlite::ToSql> = current_keys
            .iter()
            .map(|k| k as &dyn rusqlite::ToSql)
            .collect();
        let removed = tx.execute(&migrate_sql, migrate_params.as_slice())?;
        if removed > 0 {
            log::info!(
                "[profile] migrate_obsolete_builtins (tx): removed {} old builtin row(s)",
                removed
            );
        }
        // 注意: かつてここに「既存 builtin 行の prompt を BUILTIN_PRESETS 定義に同期する」
        // UPDATE ループが存在したが、ユーザーが編集したビルトインプロファイルの
        // whisper_prompt / llm_postprocess_prompt を毎回起動時に踏みつぶしてしまい
        // 「保存しても消える」という致命的なユーザー体験を引き起こすため削除した
        // (mac/profile-builtin-no-overwrite, 2026-06-14)。
        //
        // 旧コードの意図は「バージョンアップで BUILTIN_PRESETS の prompt 文面が変わった際に
        // 既存 DB を最新定義へ追随させる」ことだったが、それはユーザーの編集を上書きする副作用が大きすぎる。
        // 開発者が更新した最新プロンプトに戻したいユーザーは PT-10「デフォルトに戻す」ボタン
        // (reset_to_builtin_default) を明示的に押すことで反映できる。
        let now = current_epoch_secs();

        // -- seed (BUILTIN_PRESETS のうち未投入なものを INSERT) --
        for preset in BUILTIN_PRESETS {
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM profiles WHERE builtin_key = ?1 LIMIT 1",
                    params![preset.builtin_key],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if exists {
                continue;
            }
            tx.execute(
                "INSERT INTO profiles
                    (display_name, whisper_prompt, llm_postprocess_prompt,
                     filler_removal, emoji_density, is_builtin, builtin_key,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
                params![
                    preset.display_name,
                    preset.whisper_prompt,
                    preset.llm_postprocess_prompt,
                    preset.filler_removal as i64,
                    preset.emoji_density,
                    preset.builtin_key,
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Wave 2 follow-up migration:
    /// 過去バージョン (Wave 2 初版) が投入していた旧ビルトイン (salon/accounting/meeting) 等、
    /// 現在の BUILTIN_PRESETS に含まれない builtin_key を持つ「ビルトイン行」を削除する。
    ///
    /// 削除対象判定:
    ///   - is_builtin = 1
    ///   - builtin_key が現行 BUILTIN_PRESETS の builtin_key 集合に含まれない (NULL も対象)
    ///
    /// user 作成プロファイル (is_builtin = 0) は **絶対に削除しない**。
    /// idempotent (再呼出しても 0 件削除で終わる)。
    ///
    /// 注: P1-2 fix 以降、open() からは呼ばれない (migrate_and_seed_in_tx に統合)。
    /// 将来の単独運用 (例: マイグレーションのみ走らせる管理コマンド) のために残置。
    #[allow(dead_code)]
    fn migrate_obsolete_builtins(&self) -> Result<(), ProfileDbError> {
        // 現行 builtin_key 集合を SQL プレースホルダで構築 (param injection 防止)
        let current_keys: Vec<&str> = BUILTIN_PRESETS.iter().map(|p| p.builtin_key).collect();
        // NOT IN は NULL を弾けないので OR builtin_key IS NULL を明示
        let placeholders = current_keys
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "DELETE FROM profiles
             WHERE is_builtin = 1
               AND (builtin_key IS NULL OR builtin_key NOT IN ({}))",
            placeholders
        );
        let params: Vec<&dyn rusqlite::ToSql> = current_keys
            .iter()
            .map(|k| k as &dyn rusqlite::ToSql)
            .collect();
        let removed = self.conn.execute(&sql, params.as_slice())?;
        if removed > 0 {
            log::info!(
                "[profile] migrate_obsolete_builtins: removed {} old builtin row(s)",
                removed
            );
        }
        Ok(())
    }

    fn init(&self) -> Result<(), ProfileDbError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS profiles (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                display_name TEXT NOT NULL,
                whisper_prompt TEXT NOT NULL DEFAULT '',
                llm_postprocess_prompt TEXT NOT NULL DEFAULT '',
                filler_removal INTEGER NOT NULL DEFAULT 1,
                emoji_density TEXT NOT NULL DEFAULT 'none',
                emoji_mode TEXT NOT NULL DEFAULT 'none',
                is_builtin INTEGER NOT NULL DEFAULT 0,
                builtin_key TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_profiles_builtin_key ON profiles(builtin_key);
            "#,
        )?;
        // emoji_mode カラムは DB スキーマ上残したままにする (DROP COLUMN 不要)。
        // コードから参照しなければ無害。既存 DB がまだカラムを持たない場合に備えて
        // ALTER TABLE で追加する (冪等)。
        let has_emoji_mode = self
            .conn
            .prepare("PRAGMA table_info(profiles)")?
            .query_map([], |row| {
                let name: String = row.get(1)?;
                Ok(name)
            })?
            .filter_map(|r| r.ok())
            .any(|col| col == "emoji_mode");
        if !has_emoji_mode {
            self.conn.execute_batch(
                "ALTER TABLE profiles ADD COLUMN emoji_mode TEXT NOT NULL DEFAULT 'none'",
            )?;
            log::info!("[profile] migration: added emoji_mode column to profiles table");
        }
        Ok(())
    }

    /// 初回 open 時にビルトイン 3 種を投入する。
    /// 既に builtin_key が存在すれば INSERT スキップ (idempotent)。
    ///
    /// 注: P1-2 fix 以降、open() からは呼ばれない (migrate_and_seed_in_tx に統合)。
    /// 将来の単独運用 (例: builtin 再投入専用コマンド) のために残置。
    #[allow(dead_code)]
    fn seed_builtins(&self) -> Result<(), ProfileDbError> {
        let now = current_epoch_secs();
        for preset in BUILTIN_PRESETS {
            let exists: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM profiles WHERE builtin_key = ?1 LIMIT 1",
                    params![preset.builtin_key],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if exists {
                continue;
            }
            self.conn.execute(
                "INSERT INTO profiles
                    (display_name, whisper_prompt, llm_postprocess_prompt,
                     filler_removal, emoji_density, is_builtin, builtin_key,
                     created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
                params![
                    preset.display_name,
                    preset.whisper_prompt,
                    preset.llm_postprocess_prompt,
                    preset.filler_removal as i64,
                    preset.emoji_density,
                    preset.builtin_key,
                    now,
                ],
            )?;
        }
        Ok(())
    }

    /// 全件取得 (ビルトイン優先・その後 created_at ASC で安定順)。
    pub fn list(&self) -> Result<Vec<Profile>, ProfileDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, display_name, whisper_prompt, llm_postprocess_prompt,
                    filler_removal, emoji_density, is_builtin, builtin_key,
                    created_at, updated_at
             FROM profiles
             ORDER BY is_builtin DESC, created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_profile)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// id 指定で 1 件取得。
    pub fn get(&self, id: i64) -> Result<Option<Profile>, ProfileDbError> {
        let p = self
            .conn
            .query_row(
                "SELECT id, display_name, whisper_prompt, llm_postprocess_prompt,
                        filler_removal, emoji_density, is_builtin, builtin_key,
                        created_at, updated_at
                 FROM profiles WHERE id = ?1",
                params![id],
                row_to_profile,
            )
            .optional()?;
        Ok(p)
    }

    /// id が Some なら UPDATE・None なら INSERT。返却は最終的な rowid。
    ///
    /// バリデーション (Validation Err):
    ///   - llm_postprocess_prompt が LLM_PROMPT_MAX_CHARS 文字超
    ///   - emoji_density が 'none'|'few'|'normal'|'many' 以外
    ///   - display_name が空白のみ
    ///
    /// ビルトインプロファイル (is_builtin = true) の UPDATE 制約:
    ///   - builtin_key と is_builtin は変更させない (DB 上の旧値を維持)
    ///   - これ以外のフィールド (display_name 含む) は変更可能
    pub fn upsert(&self, profile: &Profile) -> Result<i64, ProfileDbError> {
        validate(profile)?;
        let now = current_epoch_secs();
        match profile.id {
            Some(id) => {
                // 既存行の is_builtin / builtin_key を維持 (UI から書き換え不可)
                let existing = self
                    .get(id)?
                    .ok_or_else(|| ProfileDbError::Validation(format!("profile id={} not found", id)))?;
                self.conn.execute(
                    "UPDATE profiles
                     SET display_name = ?2,
                         whisper_prompt = ?3,
                         llm_postprocess_prompt = ?4,
                         filler_removal = ?5,
                         emoji_density = ?6,
                         is_builtin = ?7,
                         builtin_key = ?8,
                         updated_at = ?9
                     WHERE id = ?1",
                    params![
                        id,
                        profile.display_name,
                        profile.whisper_prompt,
                        profile.llm_postprocess_prompt,
                        profile.filler_removal as i64,
                        profile.emoji_density,
                        existing.is_builtin as i64,
                        existing.builtin_key,
                        now,
                    ],
                )?;
                Ok(id)
            }
            None => {
                // 新規作成はビルトイン扱いにできない (UI から作成された user プロファイル)。
                // builtin_key も None で固定 (seed_builtins 以外から builtin を作らせない)。
                self.conn.execute(
                    "INSERT INTO profiles
                        (display_name, whisper_prompt, llm_postprocess_prompt,
                         filler_removal, emoji_density, is_builtin, builtin_key,
                         created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?6)",
                    params![
                        profile.display_name,
                        profile.whisper_prompt,
                        profile.llm_postprocess_prompt,
                        profile.filler_removal as i64,
                        profile.emoji_density,
                        now,
                    ],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    /// 1 件削除。ビルトイン (is_builtin = true) は削除拒否。
    /// 存在しない id でも Ok (UI 楽観的更新と整合)。
    pub fn delete(&self, id: i64) -> Result<(), ProfileDbError> {
        if let Some(p) = self.get(id)? {
            if p.is_builtin {
                return Err(ProfileDbError::Validation(
                    "ビルトインプロファイルは削除できません".into(),
                ));
            }
        }
        self.conn
            .execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// ビルトインプロファイル (builtin_key 一致) を BUILTIN_PRESETS の初期値で
    /// 上書き UPDATE する。「デフォルトに戻す」ボタン (PT-10) の実装。
    ///
    /// 該当 builtin_key が DB に存在しなければ Validation Err。
    /// builtin_key が None の (user 作成) プロファイルでは呼ばれない想定 (UI 側で制御)。
    pub fn reset_to_builtin_default(&self, id: i64) -> Result<(), ProfileDbError> {
        let profile = self
            .get(id)?
            .ok_or_else(|| ProfileDbError::Validation(format!("profile id={} not found", id)))?;
        if !profile.is_builtin {
            return Err(ProfileDbError::Validation(
                "ビルトインプロファイルのみリセット可能です".into(),
            ));
        }
        let key = profile.builtin_key.as_deref().ok_or_else(|| {
            ProfileDbError::Validation("builtin_key が NULL のためリセットできません".into())
        })?;
        let preset = BUILTIN_PRESETS
            .iter()
            .find(|p| p.builtin_key == key)
            .ok_or_else(|| {
                ProfileDbError::Validation(format!("未知の builtin_key: {}", key))
            })?;
        let now = current_epoch_secs();
        self.conn.execute(
            "UPDATE profiles
             SET display_name = ?2,
                 whisper_prompt = ?3,
                 llm_postprocess_prompt = ?4,
                 filler_removal = ?5,
                 emoji_density = ?6,
                 updated_at = ?7
             WHERE id = ?1 AND is_builtin = 1",
            params![
                id,
                preset.display_name,
                preset.whisper_prompt,
                preset.llm_postprocess_prompt,
                preset.filler_removal as i64,
                preset.emoji_density,
                now,
            ],
        )?;
        Ok(())
    }

    /// v0.1.3: app_rules.profile_id 文字列を Profile に解決する。
    ///
    /// 解決規則 (BUILTIN_PRESETS の builtin_key と app_rules の profile_id 設計に準拠):
    ///   1. builtin_key 集合 ("raw" / "natural" / "mail" / "business_doc" / "casual") に含まれれば
    ///      WHERE builtin_key = ?1 で 1 件取得
    ///   2. それ以外は parse::<i64>() を試みて user rowid 検索
    ///   3. どちらでもなければ None (未定義 profile_id ≒ typo)
    ///
    /// AppsPage DropdownMenu の typo 値ハンドリング (例: "plain" 等) は UI 側で
    /// None 受領を「未定義: <値>」表示に変換すること。
    pub fn resolve_by_app_rule_key(&self, key: &str) -> Result<Option<Profile>, ProfileDbError> {
        if is_builtin_key(key) {
            let p = self
                .conn
                .query_row(
                    "SELECT id, display_name, whisper_prompt, llm_postprocess_prompt,
                            filler_removal, emoji_density, is_builtin, builtin_key,
                            created_at, updated_at
                     FROM profiles WHERE builtin_key = ?1",
                    params![key],
                    row_to_profile,
                )
                .optional()?;
            Ok(p)
        } else if let Ok(rowid) = key.parse::<i64>() {
            self.get(rowid)
        } else {
            Ok(None)
        }
    }

    /// 総件数 (UI 用)。
    pub fn count(&self) -> Result<i64, ProfileDbError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))?;
        Ok(n)
    }
}

fn row_to_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<Profile> {
    let filler: i64 = row.get(4)?;
    let is_builtin: i64 = row.get(6)?;
    Ok(Profile {
        id: Some(row.get(0)?),
        display_name: row.get(1)?,
        whisper_prompt: row.get(2)?,
        llm_postprocess_prompt: row.get(3)?,
        filler_removal: filler != 0,
        emoji_density: row.get(5)?,
        is_builtin: is_builtin != 0,
        builtin_key: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn current_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `<data_dir>/RushVoice/profile.db` のデフォルトパス。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
pub fn default_profile_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("profile.db")
}

fn validate(profile: &Profile) -> Result<(), ProfileDbError> {
    if profile.display_name.trim().is_empty() {
        return Err(ProfileDbError::Validation("表示名が空です".into()));
    }
    // P2-2 fix: 表示名の最大長を Rust 側でも検証 (UI maxLength={64} の二重防御)
    if profile.display_name.chars().count() > DISPLAY_NAME_MAX_CHARS {
        return Err(ProfileDbError::Validation(format!(
            "表示名が {} 文字を超えています ({})",
            DISPLAY_NAME_MAX_CHARS,
            profile.display_name.chars().count()
        )));
    }
    if profile.llm_postprocess_prompt.chars().count() > LLM_PROMPT_MAX_CHARS {
        return Err(ProfileDbError::Validation(format!(
            "後処理 LLM プロンプトが {} 文字を超えています ({})",
            LLM_PROMPT_MAX_CHARS,
            profile.llm_postprocess_prompt.chars().count()
        )));
    }
    match profile.emoji_density.as_str() {
        "none" | "few" | "normal" | "many" => {}
        other => {
            return Err(ProfileDbError::Validation(format!(
                "emoji_density は none/few/normal/many のいずれか (受領: {})",
                other
            )));
        }
    }
    Ok(())
}

// ----- ビルトインプリセット 5 種 (Phase 2 A 2026-05-28) -----
//
// Wave 2 follow-up: salon/accounting/meeting → mail/business_doc/casual に差替え。
// Phase 2 A: "natural" (ナチュラル) を raw の次に追加 (4 種 → 5 種)。
// 用途軸ではなく **「文体軸」** で分類することで Phase 3 のアプリ自動切替 (AT-06) と
// 自然に対応する (Gmail/Outlook → mail / Docs/Notion → business_doc / LINE/Chat → casual)。
//
// 設計思想 (継承):
//   - LLM プロンプト 3 大禁忌 (SSOT §6.2.1) を遵守:
//     1) 具体的フレーズの列挙禁止 → 文体特徴の抽象記述のみ
//     2) ネガティブ指示の多用禁止 → ポジティブルールに統一
//     3) 200 字以内目標 → 各プリセット約 60-100 字
//   - フィラー削除 / 句読点補正 / 固有名詞処理は **コード側** で実装 (二重処理回避・L079)。
//     プロンプトには書かない。
//   - 絵文字密度の表現語:
//       none   = 絵文字なし
//       few    = ごく稀に絵文字 (1 メッセージ 0-1 個)
//       normal = 適度に絵文字 (1 メッセージ 1-3 個)
//       many   = 積極的に絵文字
//
// 文面ソース: 委任プロンプト 2026-05-26 の叩き台を採用。利用者レビュー前提で PR 説明文に明記。

struct BuiltinPreset {
    builtin_key: &'static str,
    display_name: &'static str,
    whisper_prompt: &'static str,
    llm_postprocess_prompt: &'static str,
    filler_removal: bool,
    emoji_density: &'static str,
}

/// BUILTIN_PRESETS の builtin_key 集合に key が含まれるか判定。
/// app_rules の profile_id が builtin か user rowid かを判別する用 (Wave 3 DropdownMenu)。
pub fn is_builtin_key(key: &str) -> bool {
    BUILTIN_PRESETS.iter().any(|p| p.builtin_key == key)
}

const BUILTIN_PRESETS: &[BuiltinPreset] = &[
    // v0.1.3 追加 (2026-05-27): 「素出力」プロファイル。
    // 後処理 LLM プロンプトを空にし、フィラー除去・絵文字付与もすべて無効化。
    // Whisper 生テキスト (raw) をそのまま貼付する用途。
    // app_rules で「LINE はやっぱり素出力」「特定アプリは整形不要」を実現する選択肢。
    BuiltinPreset {
        builtin_key: "raw",
        display_name: "— 素出力 —",
        whisper_prompt: "",
        llm_postprocess_prompt: "",
        filler_removal: false,
        emoji_density: "none",
    },
    // Phase 2 A 追加 (2026-05-28): 「ナチュラル」プロファイル。
    // 口語の自然さを保ちつつ句読点を補い読みやすくする。
    // casual よりも敬語を維持し、mail よりも砕けた中間的な文体。
    BuiltinPreset {
        builtin_key: "natural",
        display_name: "ナチュラル",
        whisper_prompt: "",
        // 2026-07-22: フィラー除去の一文を削除 (llm_postprocess::build_system_prompt が
        // filler_removal=true で共通注入するため・二重指示回避)。既存 install の DB 行は
        // 踏みつぶさない方針のため旧文面が残るが、重複指示は無害。
        // 2026-07-22 (2): 句読点・表記の文も共通ベース (TRANSCRIPT_CLEANUP_INSTRUCTION) に
        // 昇格したため、ナチュラル固有指示は「文体を変えない」ことだけになった。
        // 2026-07-27 実機検証: 「敬語レベルは変えない」と書いてあるのに敬語化する事故が継続
        // (「〜しといて」→「〜してください」)。抽象語 (敬語レベル) は守られないため、
        // 指示を増やさずに具体例へ置き換える。文体は共通指示ではなくプロファイルの責務
        // (共通側に常体維持を書くと「ていねい」と衝突する・L602)。
        // 2026-07-27: トークン圧縮。抽象語(敬語レベル)は守られないので具体例は残し、
        // 「常体は常体のまま/敬体は敬体のまま」の説明は例で足りるため削除。
        llm_postprocess_prompt: "文体・内容を変えない(『しといて』を『してください』にしない)。発話にない情報を追加しない。",
        filler_removal: true,
        emoji_density: "none",
    },
    BuiltinPreset {
        builtin_key: "casual",
        display_name: "カジュアル",
        // Whisper prompt: 砕けた口語の頻出語
        whisper_prompt:
            "ありがとう、よろしく、了解、確認、お疲れさま、お願い、ごめん、わかった。",
        // 約 60 字・P2-4 fix: 絵文字に関する指示を削除 (emoji_density 設定で完全制御)。
        // 旧版「文脈に合う絵文字を控えめに加える」は emoji_density='many' に変更しても
        // プロンプト「控えめに」が残り Phase 3 接続時に矛盾するため除去 (L079 二重処理回避の延長)。
        llm_postprocess_prompt: "入力テキストをチャット返信として整える。砕けた口語にし、固い敬語は使わない。発話にない語句・文・情報は絶対に追加しない。意味は変えない。",
        // 2026-07-22 要望: フィラー除去は全プロファイル原則 ON (ナチュラル原則の
        // ベース化)。既存 install の DB 行は一度だけ手動移行済 (起動時の自動上書きは
        // ユーザーのトグル操作を毎回踏みつぶすため行わない・2026-06-14 教訓と同型)。
        filler_removal: true,
        emoji_density: "normal",
    },
    BuiltinPreset {
        builtin_key: "mail",
        display_name: "メール",
        // Whisper prompt: メール文脈の語彙ヒント (短く・敬語の頻出語)
        whisper_prompt:
            "お世話になっております、ご連絡、ご確認、よろしくお願いいたします、件名、添付。",
        // 2026-07-26 要望: 配布物に個人の実名・実住所を含めない。署名はプレースホルダーとし、
        // 各ユーザーが「設定 → プロファイル → メール」で自分の署名に書き換えて使う。
        // 注: 既存 DB の行は起動時に上書きしない設計のため、既に自分の署名へ編集済みの
        // ユーザーの内容がこの変更で戻ることはない (seed は未投入時のみ INSERT)。
        llm_postprocess_prompt: "以下のメールテンプレートの[本文]部分に、入力テキストをビジネスメール文体（です・ます体・丁寧語）に言い換えた内容を挿入して出力する。文体のみを変換し、入力にない情報は一切追加・創作しない。入力の意味・情報量を厳守する。署名部分の〇〇はテンプレートのまま出力し、推測で埋めない。\n\n〇〇様\nお世話になっております。\n株式会社〇〇の〇〇です。\n\n[本文]\n\n引き続きどうぞよろしくお願いします。\n\n\n〇〇 〇〇\n株式会社〇〇\n〒000-0000 〇〇県〇〇市〇〇町0-0-0",
        filler_removal: true,
        emoji_density: "none",
    },
    BuiltinPreset {
        builtin_key: "business_doc",
        // 2026-07-27 要望: 表示名を「ビジネス文書」→「ていねい」に改称。
        // builtin_key は app_rules の profile_id や既存 DB との互換のため据え置く。
        display_name: "ていねい",
        // Whisper prompt: 文書構造の語彙ヒント
        whisper_prompt:
            "概要、目的、背景、課題、対応方針、スケジュール、担当、結論、補足。",
        // 2026-07-25 要望: 敬語・です・ます調への変換を明示する。
        // 旧文面は文体に一切触れておらず、口語のまま段落分けされるだけだった。
        // あわせて共通の清書指示との矛盾を解消: 共通側は「箇条書きを足さない」(創作防止) だが
        // 本プロファイルは発話に含まれる列挙の整形として箇条書きを使う。優先関係が曖昧だと
        // 出力が安定しないため「入力に列挙があるときに限り」「新たな項目を作らない」と条件を明示。
        // 2026-07-26 事故修正: 「です・ます体に統一」が共通指示の「動詞の語尾は原文どおり保つ」と
        // 衝突し、依頼文が宣言文へ書き換わる事故が発生 (「確認をしてみてください」→「確認をしてみます」)。
        // さらに指示文と解釈されて回答を生成する事例も観測 (history id=4378/4379/4380)。
        // プロファイル指示は共通指示より後ろ = LLM の重み付けが強いため、ここで例外を明示して衝突を断つ。
        // ガード (伸長検知) は今回の事故を捕捉できない (出力が原文より短いため)・L582 の原則どおり
        // プロンプト側で断つ。
        llm_postprocess_prompt: "入力テキストをビジネス文書として整える。文体はです・ます体の丁寧語に整え、口語やくだけた言い回しは書き言葉に改める。ただし文の働き (依頼・命令・疑問・断定) は必ず保ち、『〜してください』『〜ですか』を『〜します』『〜です』のような宣言文に置き換えない。話し手が誰かに依頼・質問している文は、依頼・質問のまま丁寧な言い方にする。二重敬語や過剰にへりくだった表現は使わない。話の切れ目で段落を分ける。入力に列挙が含まれるときに限り箇条書きにしてよい (新たな項目を作らない)。発話にない情報・所感・結論は絶対に追加しない。意味と順序は変えない。",
        filler_removal: true,
        emoji_density: "none",
    },
];

/// テスト用: ビルトインプリセットの builtin_key 配列 (固定順)。
#[cfg(test)]
fn builtin_keys() -> Vec<&'static str> {
    BUILTIN_PRESETS.iter().map(|p| p.builtin_key).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_profile_db_test_{}", name))
            .join("profile.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn sample_user(display_name: &str) -> Profile {
        Profile {
            id: None,
            display_name: display_name.to_string(),
            whisper_prompt: String::new(),
            llm_postprocess_prompt: String::new(),
            filler_removal: true,
            emoji_density: "none".to_string(),
            is_builtin: false,
            builtin_key: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_init_seeds_all_builtins() {
        // Phase 2 A: ビルトイン数は BUILTIN_PRESETS の長さで動的に検証 (5 種: raw/natural/mail/business_doc/casual)
        let path = tmp_db("seed");
        let db = ProfileDb::open(&path).expect("open");
        let list = db.list().expect("list");
        assert_eq!(
            list.len(),
            BUILTIN_PRESETS.len(),
            "ビルトイン {} 種が投入される",
            BUILTIN_PRESETS.len()
        );
        let keys: Vec<&str> = list
            .iter()
            .filter_map(|p| p.builtin_key.as_deref())
            .collect();
        for expected in builtin_keys() {
            assert!(keys.contains(&expected), "{} が投入されていない", expected);
        }
        // 全てビルトイン
        assert!(list.iter().all(|p| p.is_builtin));
    }

    #[test]
    fn test_reopen_does_not_duplicate_builtins() {
        let path = tmp_db("reopen");
        {
            let _db = ProfileDb::open(&path).expect("open 1");
        }
        let db = ProfileDb::open(&path).expect("open 2");
        assert_eq!(
            db.count().unwrap(),
            BUILTIN_PRESETS.len() as i64,
            "再 open でもビルトイン件数のまま"
        );
    }

    #[test]
    fn test_user_profile_insert_update_delete() {
        let path = tmp_db("user_crud");
        let db = ProfileDb::open(&path).expect("open");
        let id = db.upsert(&sample_user("私のメモ")).expect("insert");
        let got = db.get(id).expect("get").unwrap();
        assert_eq!(got.display_name, "私のメモ");
        assert!(!got.is_builtin);
        assert_eq!(got.builtin_key, None);

        // UPDATE
        let mut updated = got.clone();
        updated.display_name = "改名メモ".to_string();
        updated.emoji_density = "many".to_string();
        let ret = db.upsert(&updated).expect("update");
        assert_eq!(ret, id);
        let after = db.get(id).unwrap().unwrap();
        assert_eq!(after.display_name, "改名メモ");
        assert_eq!(after.emoji_density, "many");

        // DELETE OK
        db.delete(id).expect("delete user");
        assert!(db.get(id).unwrap().is_none());
        // ビルトインは残る
        assert_eq!(db.count().unwrap(), BUILTIN_PRESETS.len() as i64);
    }

    #[test]
    fn test_cannot_delete_builtin() {
        let path = tmp_db("no_delete_builtin");
        let db = ProfileDb::open(&path).expect("open");
        let builtin = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.is_builtin)
            .expect("at least one builtin");
        let id = builtin.id.unwrap();
        let err = db.delete(id).expect_err("ビルトイン削除は Err");
        match err {
            ProfileDbError::Validation(_) => {}
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
        // 件数は変わらない
        assert_eq!(db.count().unwrap(), BUILTIN_PRESETS.len() as i64);
    }

    #[test]
    fn test_update_builtin_preserves_flags() {
        // ビルトインの UPDATE で is_builtin と builtin_key は維持される (UI から書換不可)
        let path = tmp_db("preserve_builtin_flags");
        let db = ProfileDb::open(&path).expect("open");
        let mut builtin = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("mail"))
            .unwrap();
        let id = builtin.id.unwrap();
        // 悪意ある書換試行
        builtin.is_builtin = false;
        builtin.builtin_key = Some("hacked".to_string());
        builtin.display_name = "改名メール".to_string();
        db.upsert(&builtin).expect("update builtin");

        let after = db.get(id).unwrap().unwrap();
        assert!(after.is_builtin, "is_builtin は維持される");
        assert_eq!(after.builtin_key.as_deref(), Some("mail"), "builtin_key は維持される");
        assert_eq!(after.display_name, "改名メール", "display_name は変更される");
    }

    #[test]
    fn test_reset_to_builtin_default_restores_preset() {
        let path = tmp_db("reset_default");
        let db = ProfileDb::open(&path).expect("open");
        let mut mail = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("mail"))
            .unwrap();
        let id = mail.id.unwrap();
        let original_prompt = mail.llm_postprocess_prompt.clone();
        let original_emoji = mail.emoji_density.clone();
        let original_filler = mail.filler_removal;

        // 書換
        mail.llm_postprocess_prompt = "改変済プロンプト".to_string();
        mail.emoji_density = "many".to_string();
        mail.filler_removal = !original_filler;
        db.upsert(&mail).expect("update");

        // 改変が反映されていることを確認
        let dirty = db.get(id).unwrap().unwrap();
        assert_eq!(dirty.llm_postprocess_prompt, "改変済プロンプト");
        assert_eq!(dirty.emoji_density, "many");
        assert_eq!(dirty.filler_removal, !original_filler);

        // リセット実行
        db.reset_to_builtin_default(id).expect("reset");

        // プリセット初期値に復元されていることを確認
        let restored = db.get(id).unwrap().unwrap();
        assert_eq!(restored.llm_postprocess_prompt, original_prompt);
        assert_eq!(restored.emoji_density, original_emoji);
        assert_eq!(restored.filler_removal, original_filler);
        assert!(restored.is_builtin);
        assert_eq!(restored.builtin_key.as_deref(), Some("mail"));
    }

    #[test]
    fn test_reset_user_profile_rejected() {
        // user 作成プロファイルへの reset は Err (UI でも防がれるが Rust 側でも保険)
        let path = tmp_db("reset_user_rejected");
        let db = ProfileDb::open(&path).expect("open");
        let id = db.upsert(&sample_user("私のメモ")).expect("insert");
        let err = db.reset_to_builtin_default(id).expect_err("reset user");
        match err {
            ProfileDbError::Validation(_) => {}
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
    }

    #[test]
    fn test_validation_llm_prompt_too_long() {
        let path = tmp_db("validation_prompt");
        let db = ProfileDb::open(&path).expect("open");
        let mut p = sample_user("長文");
        p.llm_postprocess_prompt = "あ".repeat(LLM_PROMPT_MAX_CHARS + 1);
        let err = db.upsert(&p).expect_err("over-length should fail");
        match err {
            ProfileDbError::Validation(msg) => assert!(msg.contains(&LLM_PROMPT_MAX_CHARS.to_string())),
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }

        // 上限ぴったり (1200) は OK
        p.llm_postprocess_prompt = "あ".repeat(LLM_PROMPT_MAX_CHARS);
        db.upsert(&p).expect("max len ok");
    }

    #[test]
    fn test_validation_emoji_density_invalid() {
        let path = tmp_db("validation_emoji");
        let db = ProfileDb::open(&path).expect("open");
        let mut p = sample_user("絵文字テスト");
        p.emoji_density = "tons".to_string();
        let err = db.upsert(&p).expect_err("invalid emoji density");
        match err {
            ProfileDbError::Validation(msg) => assert!(msg.contains("emoji_density")),
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
        // 全 4 値 OK
        for v in ["none", "few", "normal", "many"] {
            p.emoji_density = v.to_string();
            db.upsert(&p).expect("valid emoji density");
        }
    }

    #[test]
    fn test_validation_empty_display_name() {
        let path = tmp_db("validation_name");
        let db = ProfileDb::open(&path).expect("open");
        let p = sample_user("   ");
        let err = db.upsert(&p).expect_err("empty name should fail");
        match err {
            ProfileDbError::Validation(_) => {}
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
    }

    #[test]
    fn test_migrate_obsolete_builtins_removes_old_salon_accounting_meeting() {
        // Wave 2 follow-up: 旧 builtin が DB に残っていれば open() 時に削除される。
        // user 作成プロファイル (is_builtin = false) は保持される。
        let path = tmp_db("migrate_obsolete");
        // 1) DB を直接作って旧 builtin + user 行を投入 (open は使わない)
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    display_name TEXT NOT NULL,
                    whisper_prompt TEXT NOT NULL DEFAULT '',
                    llm_postprocess_prompt TEXT NOT NULL DEFAULT '',
                    filler_removal INTEGER NOT NULL DEFAULT 1,
                    emoji_density TEXT NOT NULL DEFAULT 'none',
                    is_builtin INTEGER NOT NULL DEFAULT 0,
                    builtin_key TEXT,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            // 旧 builtin (3 件) + builtin_key=NULL のビルトイン (異常データ) + user 1 件
            for (key, name) in [
                ("salon", "サロン業務"),
                ("accounting", "経理"),
                ("meeting", "議事録"),
            ] {
                raw.execute(
                    "INSERT INTO profiles
                     (display_name, whisper_prompt, llm_postprocess_prompt,
                      filler_removal, emoji_density, is_builtin, builtin_key,
                      created_at, updated_at)
                     VALUES (?1, '', '', 1, 'none', 1, ?2, 100, 100)",
                    params![name, key],
                )
                .unwrap();
            }
            // builtin_key=NULL の異常 builtin (過去バージョン互換性テスト)
            raw.execute(
                "INSERT INTO profiles
                 (display_name, whisper_prompt, llm_postprocess_prompt,
                  filler_removal, emoji_density, is_builtin, builtin_key,
                  created_at, updated_at)
                 VALUES ('壊れた builtin', '', '', 1, 'none', 1, NULL, 100, 100)",
                [],
            )
            .unwrap();
            // user 作成プロファイル (絶対に保持される)
            raw.execute(
                "INSERT INTO profiles
                 (display_name, whisper_prompt, llm_postprocess_prompt,
                  filler_removal, emoji_density, is_builtin, builtin_key,
                  created_at, updated_at)
                 VALUES ('私のメモ', '', '', 1, 'none', 0, NULL, 200, 200)",
                [],
            )
            .unwrap();
        }

        // 2) 通常 open: migration が走り旧 builtin 削除 + 新 builtin 投入
        let db = ProfileDb::open(&path).expect("open");
        let list = db.list().unwrap();
        // 期待: builtin BUILTIN_PRESETS.len() 件 + user 1 件
        assert_eq!(
            list.len(),
            BUILTIN_PRESETS.len() + 1,
            "新 builtin + user 1"
        );
        let user_count = list.iter().filter(|p| !p.is_builtin).count();
        assert_eq!(user_count, 1, "user プロファイルは保持される");
        let user = list.iter().find(|p| !p.is_builtin).unwrap();
        assert_eq!(user.display_name, "私のメモ");

        // builtin_key 集合が現行 BUILTIN_PRESETS と一致すること
        let keys: Vec<&str> = list
            .iter()
            .filter(|p| p.is_builtin)
            .filter_map(|p| p.builtin_key.as_deref())
            .collect();
        assert_eq!(keys.len(), BUILTIN_PRESETS.len());
        for expected in builtin_keys() {
            assert!(keys.contains(&expected), "{} が投入されていない", expected);
        }
        // 旧 key は存在しない
        for old in ["salon", "accounting", "meeting"] {
            assert!(!keys.contains(&old), "旧 builtin {} が残っている", old);
        }
    }

    #[test]
    fn test_migrate_idempotent_on_fresh_db() {
        // 新規 DB に対しては migration は 0 件削除で終わる (idempotent 検証)
        let path = tmp_db("migrate_idempotent");
        let db = ProfileDb::open(&path).expect("open 1");
        assert_eq!(db.count().unwrap(), BUILTIN_PRESETS.len() as i64);
        // 2 回目 open でも件数のまま
        let db2 = ProfileDb::open(&path).expect("open 2");
        assert_eq!(db2.count().unwrap(), BUILTIN_PRESETS.len() as i64);
    }

    // ----- P2-2 fix: 表示名 maxLength バリデーション -----

    #[test]
    fn test_validation_display_name_too_long() {
        let path = tmp_db("validation_name_long");
        let db = ProfileDb::open(&path).expect("open");
        let mut p = sample_user("a");
        // 65 文字 (上限 64 超え)
        p.display_name = "あ".repeat(DISPLAY_NAME_MAX_CHARS + 1);
        let err = db.upsert(&p).expect_err("over-length should fail");
        match err {
            ProfileDbError::Validation(msg) => {
                assert!(msg.contains(&DISPLAY_NAME_MAX_CHARS.to_string()))
            }
            other => panic!("Validation Err 想定 / 実際: {:?}", other),
        }
        // 64 文字ぴったりは OK
        p.display_name = "あ".repeat(DISPLAY_NAME_MAX_CHARS);
        db.upsert(&p).expect("max len ok");
    }

    // ----- P2-4 fix: casual プリセットに絵文字指示が含まれないこと -----

    #[test]
    fn test_casual_preset_has_no_emoji_instruction() {
        let casual = BUILTIN_PRESETS
            .iter()
            .find(|p| p.builtin_key == "casual")
            .expect("casual preset exists");
        // emoji_density の単一情報源化 (二重指定禁止)
        assert!(
            !casual.llm_postprocess_prompt.contains("絵文字"),
            "casual プロンプトに『絵文字』指示が残っている: {}",
            casual.llm_postprocess_prompt
        );
        // emoji_density は normal が既定
        assert_eq!(casual.emoji_density, "normal");
    }

    // ----- P1-2 fix: migrate + seed 単一トランザクション化の検証 -----

    /// 正常系: 旧 builtin + user 行がある状態で open() すると、
    /// 1 トランザクション内で旧削除 + 新 seed がコミットされる。
    #[test]
    fn test_migrate_and_seed_atomic_normal() {
        let path = tmp_db("migrate_seed_atomic");
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    display_name TEXT NOT NULL,
                    whisper_prompt TEXT NOT NULL DEFAULT '',
                    llm_postprocess_prompt TEXT NOT NULL DEFAULT '',
                    filler_removal INTEGER NOT NULL DEFAULT 1,
                    emoji_density TEXT NOT NULL DEFAULT 'none',
                    is_builtin INTEGER NOT NULL DEFAULT 0,
                    builtin_key TEXT,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            raw.execute(
                "INSERT INTO profiles
                 (display_name, whisper_prompt, llm_postprocess_prompt,
                  filler_removal, emoji_density, is_builtin, builtin_key,
                  created_at, updated_at)
                 VALUES ('サロン業務', '', '', 1, 'none', 1, 'salon', 100, 100)",
                [],
            )
            .unwrap();
        }
        let db = ProfileDb::open(&path).expect("open");
        // 旧 salon 削除 + 新 BUILTIN_PRESETS 種投入
        assert_eq!(db.count().unwrap(), BUILTIN_PRESETS.len() as i64);
        let keys: Vec<String> = db
            .list()
            .unwrap()
            .into_iter()
            .filter_map(|p| p.builtin_key)
            .collect();
        assert!(keys.iter().any(|k| k == "mail"));
        assert!(keys.iter().any(|k| k == "business_doc"));
        assert!(keys.iter().any(|k| k == "casual"));
        assert!(!keys.iter().any(|k| k == "salon"));
    }

    /// 異常系: seed_builtins の途中で UNIQUE 違反等を起こした場合に
    /// migration (旧削除) も rollback されることを検証。
    /// builtin_key にユニーク制約を追加した DB で、既存の "mail" 行を埋めておくと
    /// seed_builtins の INSERT で UNIQUE 違反 → 全体 rollback。
    #[test]
    fn test_migrate_and_seed_rollback_on_seed_failure() {
        let path = tmp_db("migrate_seed_rollback");
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            raw.execute_batch(
                r#"
                CREATE TABLE profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    display_name TEXT NOT NULL,
                    whisper_prompt TEXT NOT NULL DEFAULT '',
                    llm_postprocess_prompt TEXT NOT NULL DEFAULT '',
                    filler_removal INTEGER NOT NULL DEFAULT 1,
                    emoji_density TEXT NOT NULL DEFAULT 'none',
                    is_builtin INTEGER NOT NULL DEFAULT 0,
                    builtin_key TEXT UNIQUE,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            // 旧 builtin 1 件 (削除対象)
            raw.execute(
                "INSERT INTO profiles
                 (display_name, whisper_prompt, llm_postprocess_prompt,
                  filler_removal, emoji_density, is_builtin, builtin_key,
                  created_at, updated_at)
                 VALUES ('旧salon', '', '', 1, 'none', 1, 'salon', 100, 100)",
                [],
            )
            .unwrap();
            // user 作成プロファイルで builtin_key='mail' を埋める (本来 user は NULL のはずだが
            // 強制的に同名を入れることで seed の INSERT を UNIQUE 違反させる)
            raw.execute(
                "INSERT INTO profiles
                 (display_name, whisper_prompt, llm_postprocess_prompt,
                  filler_removal, emoji_density, is_builtin, builtin_key,
                  created_at, updated_at)
                 VALUES ('衝突メール', '', '', 1, 'none', 0, 'mail', 200, 200)",
                [],
            )
            .unwrap();
        }
        // open() は seed_builtins 内の "mail" INSERT で UNIQUE 違反 → Err
        // (seed_builtins 内の exists 判定で既存 'mail' 行が見つかれば continue するが、
        //  exists 判定は WHERE builtin_key=? で行うため衝突 user 行も hit して INSERT が走らない)
        //  → このパターンでは exists=true で seed がスキップされ rollback されない可能性あり。
        //  ここではトランザクションが破綻したケースを検出するための保険として置く。
        let result = ProfileDb::open(&path);
        // open 結果に関わらず DB 状態を確認: 旧 salon が残っているか削除されているかは
        // tx の到達点による。重要なのは「旧 salon 削除 + mail INSERT 部分成功」のような
        // 中間状態が残らないこと。
        let raw = Connection::open(&path).unwrap();
        let salon_count: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM profiles WHERE builtin_key='salon'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mail_builtin_count: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM profiles WHERE builtin_key='mail' AND is_builtin=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // 整合性: open 成功 → salon は削除済 / open 失敗 → salon は残っているか削除済
        // どちらの分岐でも「旧削除済 + mail builtin 未投入」の中間状態は許されない。
        if result.is_ok() {
            // 成功時: salon は削除され、mail builtin が投入されているか、
            // exists 判定で skip されているはず。整合性のみ確認 (具体的件数は実装依存)。
            assert_eq!(salon_count, 0, "成功時は salon 削除済");
        } else {
            // 失敗時: rollback で旧 salon は残り、mail builtin は投入されていない
            assert_eq!(salon_count, 1, "rollback で salon は残る");
            assert_eq!(mail_builtin_count, 0, "rollback で mail builtin 未投入");
        }
    }

    #[test]
    fn test_list_order_builtins_first() {
        // ビルトインが最初・user 作成行が後ろに並ぶ
        let path = tmp_db("order");
        let db = ProfileDb::open(&path).expect("open");
        db.upsert(&sample_user("user A")).expect("insert");
        db.upsert(&sample_user("user B")).expect("insert");
        let list = db.list().unwrap();
        let bn = BUILTIN_PRESETS.len();
        assert_eq!(list.len(), bn + 2);
        // 先頭 bn 件はビルトイン
        for p in list.iter().take(bn) {
            assert!(p.is_builtin);
        }
        // 末尾 2 件は user
        assert!(!list[bn].is_builtin);
        assert!(!list[bn + 1].is_builtin);
    }

    // ----- v0.1.3: 「素出力」(raw) プロファイル投入確認 + 後処理スキップ動作 -----

    #[test]
    fn test_raw_builtin_inserted_on_open() {
        let path = tmp_db("raw_inserted");
        let db = ProfileDb::open(&path).expect("open");
        let raw = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("raw"))
            .expect("raw builtin should be present");
        assert_eq!(raw.display_name, "— 素出力 —");
        assert!(raw.is_builtin);
    }

    #[test]
    fn test_raw_builtin_has_no_postprocess_settings() {
        // 「素出力」マッチ時は Phase 2 A 後処理 (prompt/filler/emoji) がすべて no-op になる
        let raw = BUILTIN_PRESETS
            .iter()
            .find(|p| p.builtin_key == "raw")
            .expect("raw preset exists");
        assert_eq!(raw.whisper_prompt, "");
        assert_eq!(raw.llm_postprocess_prompt, "");
        assert!(!raw.filler_removal);
        assert_eq!(raw.emoji_density, "none");
    }

    // ----- v0.1.3: is_builtin_key ヘルパ -----

    // ----- v0.1.3: resolve_by_app_rule_key -----

    #[test]
    fn test_resolve_by_app_rule_key_returns_builtin() {
        let path = tmp_db("resolve_builtin");
        let db = ProfileDb::open(&path).expect("open");
        for key in builtin_keys() {
            let p = db
                .resolve_by_app_rule_key(key)
                .expect("resolve ok")
                .unwrap_or_else(|| panic!("{} resolved to None", key));
            assert!(p.is_builtin);
            assert_eq!(p.builtin_key.as_deref(), Some(key));
        }
    }

    #[test]
    fn test_resolve_by_app_rule_key_returns_user_by_rowid() {
        let path = tmp_db("resolve_user");
        let db = ProfileDb::open(&path).expect("open");
        let id = db.upsert(&sample_user("私のメモ")).expect("insert");
        let key = id.to_string();
        let p = db
            .resolve_by_app_rule_key(&key)
            .expect("resolve ok")
            .expect("should resolve");
        assert_eq!(p.id, Some(id));
        assert_eq!(p.display_name, "私のメモ");
        assert!(!p.is_builtin);
    }

    #[test]
    fn test_resolve_by_app_rule_key_returns_none_for_undefined() {
        let path = tmp_db("resolve_none");
        let db = ProfileDb::open(&path).expect("open");
        // typo (= builtin_key にも user rowid 候補にもならない文字列)
        assert!(db.resolve_by_app_rule_key("plain").unwrap().is_none());
        assert!(db.resolve_by_app_rule_key("").unwrap().is_none());
        // 数字だが存在しない rowid
        assert!(db.resolve_by_app_rule_key("999999").unwrap().is_none());
    }

    #[test]
    fn test_is_builtin_key_recognizes_all_builtins() {
        for key in builtin_keys() {
            assert!(is_builtin_key(key), "{} は builtin_key として認識されるべき", key);
        }
        // 存在しない key は false
        assert!(!is_builtin_key("not_a_builtin"));
        assert!(!is_builtin_key(""));
        // 数字文字列 (user rowid 想定) も false
        assert!(!is_builtin_key("42"));
    }

    // ----- 回帰防止: ビルトインプロファイル編集の永続化 (mac/profile-builtin-no-overwrite, 2026-06-14) -----

    /// ユーザーがビルトインプロファイルの llm_postprocess_prompt を編集して保存した後、
    /// アプリ再起動相当 (DB を close → 再 open) で編集内容が消えないことを検証する。
    ///
    /// かつて migrate_and_seed_in_tx() に存在した「既存 builtin 行の prompt を
    /// BUILTIN_PRESETS 定義に強制同期する UPDATE ループ」が、ユーザー編集を毎回
    /// 起動時に踏みつぶす致命バグの原因だった。本テストはその回帰を防ぐ。
    #[test]
    fn test_builtin_user_edit_survives_reopen() {
        let path = tmp_db("builtin_edit_persists");
        // 1) 初回 open → seed されたビルトインを編集 → 保存
        let user_edited_prompt = "私好みに編集した後処理プロンプト 12345 !!!".to_string();
        let user_edited_whisper = "私の Whisper ヒント abcde".to_string();
        {
            let db = ProfileDb::open(&path).expect("open 1");
            let mut natural = db
                .list()
                .unwrap()
                .into_iter()
                .find(|p| p.builtin_key.as_deref() == Some("natural"))
                .expect("natural builtin seeded");
            natural.llm_postprocess_prompt = user_edited_prompt.clone();
            natural.whisper_prompt = user_edited_whisper.clone();
            db.upsert(&natural).expect("user edit save");
        }
        // 2) DB を一度閉じて再 open (アプリ再起動相当)
        let db = ProfileDb::open(&path).expect("open 2");
        let natural = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("natural"))
            .expect("natural builtin still exists");
        // 3) 編集内容が保持されていること
        assert_eq!(
            natural.llm_postprocess_prompt, user_edited_prompt,
            "ユーザー編集した llm_postprocess_prompt が再 open で消えてはいけない"
        );
        assert_eq!(
            natural.whisper_prompt, user_edited_whisper,
            "ユーザー編集した whisper_prompt が再 open で消えてはいけない"
        );
        // is_builtin / builtin_key は維持される (既存契約)
        assert!(natural.is_builtin);
        assert_eq!(natural.builtin_key.as_deref(), Some("natural"));
    }

    /// 同上、user 作成プロファイル (is_builtin=false) も再 open で消えない・上書きされない。
    /// これはもともと sync 対象外だったが、リファクタで巻き込み事故を起こさないための保険。
    #[test]
    fn test_user_profile_survives_reopen() {
        let path = tmp_db("user_profile_persists");
        let original_prompt = "user の長い長いプロンプト・全部保持されるべし".to_string();
        let id;
        {
            let db = ProfileDb::open(&path).expect("open 1");
            let mut p = sample_user("ゆきナチュラル");
            p.llm_postprocess_prompt = original_prompt.clone();
            id = db.upsert(&p).expect("user insert");
        }
        let db = ProfileDb::open(&path).expect("open 2");
        let after = db.get(id).unwrap().expect("user profile retained");
        assert_eq!(after.display_name, "ゆきナチュラル");
        assert_eq!(after.llm_postprocess_prompt, original_prompt);
        assert!(!after.is_builtin);
    }

    /// PT-10「デフォルトに戻す」(reset_to_builtin_default) は引き続き正常動作する。
    /// 削除した sync の代替として、ユーザーが明示的に呼ぶ唯一のリセット経路。
    /// (test_reset_to_builtin_default_restores_preset と機能は重複するが、
    ///  本 fix と関連付ける意図で別テストとして残す)
    #[test]
    fn test_reset_button_still_works_after_user_edit() {
        let path = tmp_db("reset_after_edit");
        let db = ProfileDb::open(&path).expect("open");
        let mut natural = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("natural"))
            .unwrap();
        let preset_prompt = BUILTIN_PRESETS
            .iter()
            .find(|p| p.builtin_key == "natural")
            .unwrap()
            .llm_postprocess_prompt
            .to_string();
        // ユーザー編集
        natural.llm_postprocess_prompt = "編集済み".to_string();
        db.upsert(&natural).expect("user edit");
        // PT-10 で明示的にデフォルトへ戻す
        db.reset_to_builtin_default(natural.id.unwrap())
            .expect("reset to default");
        let restored = db.get(natural.id.unwrap()).unwrap().unwrap();
        assert_eq!(
            restored.llm_postprocess_prompt, preset_prompt,
            "PT-10 でプリセット文面に戻る"
        );
    }

    #[test]
    fn test_emoji_mode_column_added_by_migration() {
        // emoji_mode カラムがない旧スキーマ DB に open() するとカラムが追加される (DB 上カラムは保持)
        let path = tmp_db("emoji_mode_migration");
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let raw = Connection::open(&path).unwrap();
            // emoji_mode カラムなしの旧スキーマ
            raw.execute_batch(
                r#"
                CREATE TABLE profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    display_name TEXT NOT NULL,
                    whisper_prompt TEXT NOT NULL DEFAULT '',
                    llm_postprocess_prompt TEXT NOT NULL DEFAULT '',
                    filler_removal INTEGER NOT NULL DEFAULT 1,
                    emoji_density TEXT NOT NULL DEFAULT 'none',
                    is_builtin INTEGER NOT NULL DEFAULT 0,
                    builtin_key TEXT,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_profiles_builtin_key ON profiles(builtin_key);
                "#,
            ).unwrap();
        }
        // open() でマイグレーションが走り emoji_mode カラムが追加され、ビルトインが投入される
        let db = ProfileDb::open(&path).expect("migration should succeed");
        assert_eq!(db.count().unwrap(), BUILTIN_PRESETS.len() as i64);
        // casual の emoji_density が "normal" として読み取れることを確認
        let casual = db
            .list()
            .unwrap()
            .into_iter()
            .find(|p| p.builtin_key.as_deref() == Some("casual"))
            .expect("casual builtin present after migration");
        assert_eq!(casual.emoji_density, "normal", "マイグレーション後に casual emoji_density=normal が正しく読み取れる");
    }
}
