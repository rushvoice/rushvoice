// config_db.rs — Day 5 後半 SQLite 設定ストア (v2)
//
// 申し送り §2 / Sprint Plan §3.2 Day 11
// %APPDATA%/RushVoice/config.db に保存。
// API キーは含まない (DPAPI 暗号化済の config.secrets.encrypted.json で別管理)。
//
// 最小スコープ (Phase 1):
// - settings_kv: hotkey / transcribeModel / language 等の単純 KV
// - dictionary: 用語置換マップ
// - fillers: フィラー語リスト
// - migration_log: v1 → v2 移行履歴

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

// ---- v0.1.10 機能 A 定数 ----
//
// SSOT: v0_1_10_feature_a_delegation_2026-05-30.md
// バナー発動閾値の default = 5000ms (確定)・許容範囲 1000-30000ms
pub const CONGESTION_THRESHOLD_KEY: &str = "congestion_threshold_ms";
pub const CONGESTION_SHOW_IN_HEDGED_KEY: &str = "congestion_show_in_hedged";
pub const DEFAULT_CONGESTION_THRESHOLD_MS: i64 = 5000;
pub const CONGESTION_THRESHOLD_MIN_MS: i64 = 1000;
pub const CONGESTION_THRESHOLD_MAX_MS: i64 = 30000;

// ---- v0.1.10 機能 D 定数 ----
//
// SSOT: v0_1_10_feature_d_delegation_2026-05-30.md
// ホットキー競合検出状態 (ConflictState JSON 文字列・default = "None" 相当)
pub const HOTKEY_CONFLICT_STATE_KEY: &str = "hotkey_conflict_state";

// ---- v0.1.12 F2 定数 (L264 予防: マイク入力低レベル警告) ----
//
// SSOT: v0_1_12_scope_2026-05-30.md
// 録音開始から `mic_low_level_window_ms` の間、RMS が継続して
// `mic_low_level_threshold` 未満なら警告バナーを 1 度だけ発火する。
//   - mic_low_level_threshold: f32 (default 0.005 ≒ -46dB)
//   - mic_low_level_window_ms: u64 (default 3000ms)
// 値は文字列で settings_kv に保存。parse 失敗時はデフォルトに best-effort フォールバック。
pub const MIC_LOW_LEVEL_THRESHOLD_KEY: &str = "mic_low_level_threshold";
pub const MIC_LOW_LEVEL_WINDOW_MS_KEY: &str = "mic_low_level_window_ms";
pub const DEFAULT_MIC_LOW_LEVEL_THRESHOLD: f32 = 0.005;
pub const DEFAULT_MIC_LOW_LEVEL_WINDOW_MS: u64 = 3000;

// ---- v0.1.15 小声警告 定数 (L121: 小声で Groq が自信満々に誤字生成) ----
//
// SSOT: 内部設計メモ (v0.1.15 plan)
// 録音セッションの平均 RMS を baseline (普段の声水準) と比較し、相対的に小さければ警告。
// 固定値はマイク差で破綻するため (L122/L166)、baseline を EMA で自動学習する。
//   - voice_baseline_rms:   f32 (普段の声の平均 RMS・EMA で更新・default 0.0=未 seed)
//   - voice_baseline_count: u32 (学習サンプル数・コールドスタート判定用・default 0)
//   - voice_low_factor:     f32 (baseline×factor 未満で小声判定・default 0.45)
//   - voice_baseline_alpha: f32 (EMA 重み・default 0.2)
//   - voice_min_samples:    u32 (判定開始に必要な最小サンプル数・default 5)
pub const VOICE_BASELINE_RMS_KEY: &str = "voice_baseline_rms";
pub const VOICE_BASELINE_COUNT_KEY: &str = "voice_baseline_count";
pub const VOICE_LOW_FACTOR_KEY: &str = "voice_low_factor";
pub const VOICE_BASELINE_ALPHA_KEY: &str = "voice_baseline_alpha";
pub const VOICE_MIN_SAMPLES_KEY: &str = "voice_min_samples";
pub const DEFAULT_VOICE_BASELINE_RMS: f32 = 0.0;
pub const DEFAULT_VOICE_BASELINE_COUNT: u32 = 0;
pub const DEFAULT_VOICE_LOW_FACTOR: f32 = 0.45;
pub const DEFAULT_VOICE_BASELINE_ALPHA: f32 = 0.2;
pub const DEFAULT_VOICE_MIN_SAMPLES: u32 = 5;

#[derive(Debug, Error)]
pub enum ConfigDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct ConfigDb {
    conn: Connection,
}

impl ConfigDb {
    /// 指定パスの SQLite を開き、必要なら親ディレクトリを作成して
    /// CREATE TABLE IF NOT EXISTS で 4 テーブル初期化する。
    pub fn open(path: &Path) -> Result<Self, ConfigDbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        let db = ConfigDb { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<(), ConfigDbError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings_kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS dictionary (
                term TEXT PRIMARY KEY,
                replacement TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS fillers (
                word TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS migration_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source TEXT NOT NULL,
                migrated_at TEXT NOT NULL,
                items_count INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    // ---- settings_kv ----

    pub fn get_kv(&self, key: &str) -> Result<Option<String>, ConfigDbError> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM settings_kv WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO settings_kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- v0.1.10 機能 A: 混雑時通知バナー設定 (settings_kv ベース) ----
    //
    // SSOT: v0_1_10_feature_a_delegation_2026-05-30.md
    // KV キー: `congestion_threshold_ms` (i64・default 5000)
    //          `congestion_show_in_hedged` (bool・default false)
    //
    // 設計判断: recorder.rs を config_db に依存させない方針 (既存設計) を維持するため、
    // 設定読み込みは commands.rs::stop_recording で行い、関数引数で recorder に渡す。
    // ここでは config_db の便利関数として getter/setter を提供する。

    /// 混雑バナー発動閾値 (ms)。未設定なら 5000ms。
    pub fn get_congestion_threshold_ms(&self) -> Result<i64, ConfigDbError> {
        match self.get_kv(CONGESTION_THRESHOLD_KEY)? {
            Some(s) => Ok(s.parse::<i64>().unwrap_or(DEFAULT_CONGESTION_THRESHOLD_MS)),
            None => Ok(DEFAULT_CONGESTION_THRESHOLD_MS),
        }
    }

    /// 混雑バナー発動閾値 (ms) を保存。
    /// 範囲外 (< 1000 or > 30000) はクランプして保存する (UI 側でも制限するが二重防御)。
    pub fn set_congestion_threshold_ms(&self, ms: i64) -> Result<(), ConfigDbError> {
        let clamped = ms.clamp(
            CONGESTION_THRESHOLD_MIN_MS,
            CONGESTION_THRESHOLD_MAX_MS,
        );
        self.set_kv(CONGESTION_THRESHOLD_KEY, &clamped.to_string())
    }

    /// hedged mode でもバナーを表示するか (default false = hedged では抑止)。
    pub fn get_congestion_show_in_hedged(&self) -> Result<bool, ConfigDbError> {
        match self.get_kv(CONGESTION_SHOW_IN_HEDGED_KEY)? {
            Some(s) => Ok(s == "1" || s.eq_ignore_ascii_case("true")),
            None => Ok(false),
        }
    }

    /// hedged mode 時のバナー表示可否を保存。
    pub fn set_congestion_show_in_hedged(&self, show: bool) -> Result<(), ConfigDbError> {
        self.set_kv(CONGESTION_SHOW_IN_HEDGED_KEY, if show { "1" } else { "0" })
    }

    // ---- v0.1.10 機能 D: ホットキー競合検出状態 (settings_kv ベース) ----
    //
    // SSOT: v0_1_10_feature_d_delegation_2026-05-30.md
    // 保存値は ConflictState の JSON 文字列。未設定 / パース失敗時は "None" 扱いで返す。
    // 実体の ConflictState 型は src/hotkey_conflict.rs にあるが、config_db 単体での
    // テスト容易性のためここでは生の JSON 文字列のみを保存する。

    /// ConflictState を JSON 文字列で取得 (未設定なら None)。
    pub fn get_hotkey_conflict_state_json(&self) -> Result<Option<String>, ConfigDbError> {
        self.get_kv(HOTKEY_CONFLICT_STATE_KEY)
    }

    /// ConflictState を JSON 文字列で保存。
    pub fn set_hotkey_conflict_state_json(&self, json: &str) -> Result<(), ConfigDbError> {
        self.set_kv(HOTKEY_CONFLICT_STATE_KEY, json)
    }

    // ---- v0.1.12 F2: マイク入力低レベル警告 (settings_kv ベース) ----
    //
    // SSOT: v0_1_12_scope_2026-05-30.md
    // 文字列 KV として保存し、parse 失敗時は default にフォールバックする (best-effort)。

    /// マイク入力低レベル判定の RMS 閾値。未設定 / parse 失敗なら 0.005。
    pub fn get_mic_low_level_threshold(&self) -> Result<f32, ConfigDbError> {
        match self.get_kv(MIC_LOW_LEVEL_THRESHOLD_KEY)? {
            Some(s) => Ok(s.parse::<f32>().unwrap_or(DEFAULT_MIC_LOW_LEVEL_THRESHOLD)),
            None => Ok(DEFAULT_MIC_LOW_LEVEL_THRESHOLD),
        }
    }

    /// マイク入力低レベル判定の RMS 閾値を保存。
    /// 範囲外 (<= 0.0 or > 1.0) は default にクランプする (二重防御)。
    pub fn set_mic_low_level_threshold(&self, v: f32) -> Result<(), ConfigDbError> {
        let safe = if v.is_finite() && v > 0.0 && v <= 1.0 {
            v
        } else {
            DEFAULT_MIC_LOW_LEVEL_THRESHOLD
        };
        self.set_kv(MIC_LOW_LEVEL_THRESHOLD_KEY, &safe.to_string())
    }

    /// マイク入力低レベル判定の継続観測時間 (ms)。未設定 / parse 失敗なら 3000ms。
    pub fn get_mic_low_level_window_ms(&self) -> Result<u64, ConfigDbError> {
        match self.get_kv(MIC_LOW_LEVEL_WINDOW_MS_KEY)? {
            Some(s) => Ok(s.parse::<u64>().unwrap_or(DEFAULT_MIC_LOW_LEVEL_WINDOW_MS)),
            None => Ok(DEFAULT_MIC_LOW_LEVEL_WINDOW_MS),
        }
    }

    /// マイク入力低レベル判定の継続観測時間 (ms) を保存。
    /// 0 は意味を持たないため default にフォールバック。
    pub fn set_mic_low_level_window_ms(&self, ms: u64) -> Result<(), ConfigDbError> {
        let safe = if ms == 0 { DEFAULT_MIC_LOW_LEVEL_WINDOW_MS } else { ms };
        self.set_kv(MIC_LOW_LEVEL_WINDOW_MS_KEY, &safe.to_string())
    }

    // ---- v0.1.15 小声警告: ローリング基準 (baseline) 自動学習 ----
    // 文字列 KV として保存し、parse 失敗時は default にフォールバック (best-effort)。

    /// 普段の声の平均 RMS (EMA で学習)。未設定 / parse 失敗なら 0.0 (未 seed)。
    pub fn get_voice_baseline_rms(&self) -> Result<f32, ConfigDbError> {
        match self.get_kv(VOICE_BASELINE_RMS_KEY)? {
            Some(s) => Ok(s.parse::<f32>().unwrap_or(DEFAULT_VOICE_BASELINE_RMS)),
            None => Ok(DEFAULT_VOICE_BASELINE_RMS),
        }
    }

    /// 普段の声の平均 RMS (baseline) を保存。非有限 / 負値は 0.0 にクランプ。
    pub fn set_voice_baseline_rms(&self, v: f32) -> Result<(), ConfigDbError> {
        let safe = if v.is_finite() && v >= 0.0 { v } else { 0.0 };
        self.set_kv(VOICE_BASELINE_RMS_KEY, &safe.to_string())
    }

    /// baseline 学習サンプル数。未設定 / parse 失敗なら 0。
    pub fn get_voice_baseline_count(&self) -> Result<u32, ConfigDbError> {
        match self.get_kv(VOICE_BASELINE_COUNT_KEY)? {
            Some(s) => Ok(s.parse::<u32>().unwrap_or(DEFAULT_VOICE_BASELINE_COUNT)),
            None => Ok(DEFAULT_VOICE_BASELINE_COUNT),
        }
    }

    /// baseline 学習サンプル数を保存。
    pub fn set_voice_baseline_count(&self, c: u32) -> Result<(), ConfigDbError> {
        self.set_kv(VOICE_BASELINE_COUNT_KEY, &c.to_string())
    }

    /// 小声判定の係数 (baseline×factor 未満で小声)。未設定 / parse 失敗なら 0.45。
    /// 範囲外 (<= 0.0 or >= 1.0) は default にクランプ。
    pub fn get_voice_low_factor(&self) -> Result<f32, ConfigDbError> {
        match self.get_kv(VOICE_LOW_FACTOR_KEY)? {
            Some(s) => {
                let v = s.parse::<f32>().unwrap_or(DEFAULT_VOICE_LOW_FACTOR);
                if v.is_finite() && v > 0.0 && v < 1.0 {
                    Ok(v)
                } else {
                    Ok(DEFAULT_VOICE_LOW_FACTOR)
                }
            }
            None => Ok(DEFAULT_VOICE_LOW_FACTOR),
        }
    }

    /// 小声判定の係数を保存。範囲外は default にクランプ。
    pub fn set_voice_low_factor(&self, v: f32) -> Result<(), ConfigDbError> {
        let safe = if v.is_finite() && v > 0.0 && v < 1.0 {
            v
        } else {
            DEFAULT_VOICE_LOW_FACTOR
        };
        self.set_kv(VOICE_LOW_FACTOR_KEY, &safe.to_string())
    }

    /// baseline EMA の重み alpha。未設定 / parse 失敗 / 範囲外なら 0.2。
    pub fn get_voice_baseline_alpha(&self) -> Result<f32, ConfigDbError> {
        match self.get_kv(VOICE_BASELINE_ALPHA_KEY)? {
            Some(s) => {
                let v = s.parse::<f32>().unwrap_or(DEFAULT_VOICE_BASELINE_ALPHA);
                if v.is_finite() && v > 0.0 && v <= 1.0 {
                    Ok(v)
                } else {
                    Ok(DEFAULT_VOICE_BASELINE_ALPHA)
                }
            }
            None => Ok(DEFAULT_VOICE_BASELINE_ALPHA),
        }
    }

    /// baseline EMA の重み alpha を保存。範囲外は default にクランプ。
    pub fn set_voice_baseline_alpha(&self, v: f32) -> Result<(), ConfigDbError> {
        let safe = if v.is_finite() && v > 0.0 && v <= 1.0 {
            v
        } else {
            DEFAULT_VOICE_BASELINE_ALPHA
        };
        self.set_kv(VOICE_BASELINE_ALPHA_KEY, &safe.to_string())
    }

    /// 判定開始に必要な最小サンプル数。未設定 / parse 失敗なら 5。
    pub fn get_voice_min_samples(&self) -> Result<u32, ConfigDbError> {
        match self.get_kv(VOICE_MIN_SAMPLES_KEY)? {
            Some(s) => Ok(s.parse::<u32>().unwrap_or(DEFAULT_VOICE_MIN_SAMPLES)),
            None => Ok(DEFAULT_VOICE_MIN_SAMPLES),
        }
    }

    /// 判定開始に必要な最小サンプル数を保存。0 は意味を持たないため default。
    pub fn set_voice_min_samples(&self, n: u32) -> Result<(), ConfigDbError> {
        let safe = if n == 0 { DEFAULT_VOICE_MIN_SAMPLES } else { n };
        self.set_kv(VOICE_MIN_SAMPLES_KEY, &safe.to_string())
    }

    // ---- dictionary ----

    /// 既存全件を保ったまま、引数 map で 1 件ずつ upsert する。
    pub fn upsert_dictionary(&self, map: &HashMap<String, String>) -> Result<(), ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO dictionary (term, replacement) VALUES (?1, ?2)
                 ON CONFLICT(term) DO UPDATE SET replacement = excluded.replacement",
            )?;
            for (k, v) in map {
                stmt.execute(params![k, v])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn select_dictionary(&self) -> Result<HashMap<String, String>, ConfigDbError> {
        let mut stmt = self.conn.prepare("SELECT term, replacement FROM dictionary")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
        Ok(map)
    }

    // ---- fillers ----

    /// 既存を全削除して引数の word リストに置き換える (idempotent な置換)。
    pub fn replace_fillers(&self, words: &[String]) -> Result<(), ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM fillers", [])?;
        {
            // INSERT OR IGNORE で重複ワードを idempotent に扱う (Codex P2 対策)
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO fillers (word) VALUES (?1)")?;
            for w in words {
                stmt.execute(params![w])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn select_fillers(&self) -> Result<Vec<String>, ConfigDbError> {
        let mut stmt = self.conn.prepare("SELECT word FROM fillers ORDER BY word")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---- migration_log ----

    pub fn insert_migration_log(
        &self,
        source: &str,
        items_count: u32,
    ) -> Result<i64, ConfigDbError> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO migration_log (source, migrated_at, items_count)
             VALUES (?1, ?2, ?3)",
            params![source, now, items_count],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn latest_migration_log(
        &self,
    ) -> Result<Option<(String, String, u32)>, ConfigDbError> {
        let row = self
            .conn
            .query_row(
                "SELECT source, migrated_at, items_count
                 FROM migration_log
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }
}

/// `<data_dir>/RushVoice/config.db` のデフォルトパス。
/// Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
/// Windows: %APPDATA%\RushVoice\config.db (既存ユーザーパス不変)
/// macOS:   ~/Library/Application Support/RushVoice/config.db
pub fn default_config_db_path() -> PathBuf {
    crate::paths::rushvoice_data_dir().join("config.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_config_db_test_{}", name))
            .join("config.db");
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn open_creates_tables_idempotently() {
        let path = tmp_db("open");
        let _db = ConfigDb::open(&path).expect("open 1");
        // 2 回目の open でも壊れない
        let _db2 = ConfigDb::open(&path).expect("open 2");
    }

    #[test]
    fn kv_set_get_roundtrip() {
        let path = tmp_db("kv");
        let db = ConfigDb::open(&path).expect("open");
        assert_eq!(db.get_kv("start_key").unwrap(), None);
        db.set_kv("start_key", "RightCtrl").unwrap();
        assert_eq!(db.get_kv("start_key").unwrap().as_deref(), Some("RightCtrl"));
        // upsert
        db.set_kv("start_key", "LeftAlt").unwrap();
        assert_eq!(db.get_kv("start_key").unwrap().as_deref(), Some("LeftAlt"));
    }

    #[test]
    fn dictionary_upsert_select() {
        let path = tmp_db("dict");
        let db = ConfigDb::open(&path).expect("open");
        let mut m = HashMap::new();
        m.insert("アクアボイス".to_string(), "Aqua Voice".to_string());
        m.insert("タイプレス".to_string(), "Typeless".to_string());
        db.upsert_dictionary(&m).unwrap();

        let got = db.select_dictionary().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("アクアボイス").unwrap(), "Aqua Voice");

        // 上書き
        let mut m2 = HashMap::new();
        m2.insert("アクアボイス".to_string(), "AquaVoice2".to_string());
        db.upsert_dictionary(&m2).unwrap();
        let got2 = db.select_dictionary().unwrap();
        assert_eq!(got2.len(), 2);
        assert_eq!(got2.get("アクアボイス").unwrap(), "AquaVoice2");
    }

    #[test]
    fn fillers_replace_all() {
        let path = tmp_db("fillers");
        let db = ConfigDb::open(&path).expect("open");
        db.replace_fillers(&[
            "えー".to_string(),
            "あの".to_string(),
            "うーん".to_string(),
        ])
        .unwrap();
        let got = db.select_fillers().unwrap();
        assert_eq!(got.len(), 3);
        // ORDER BY word なのでソート済
        assert_eq!(got, vec!["あの", "うーん", "えー"]);

        // 置換 (DELETE + INSERT)
        db.replace_fillers(&["まあ".to_string()]).unwrap();
        let got2 = db.select_fillers().unwrap();
        assert_eq!(got2, vec!["まあ"]);
    }

    #[test]
    fn fillers_replace_tolerates_duplicates() {
        // Codex P2 回帰防止: 重複ワードでも PRIMARY KEY 違反でエラーしない
        let path = tmp_db("fillers_dup");
        let db = ConfigDb::open(&path).expect("open");
        db.replace_fillers(&[
            "えー".to_string(),
            "えー".to_string(),
            "あの".to_string(),
            "あの".to_string(),
        ])
        .expect("replace tolerates dupes");
        let got = db.select_fillers().unwrap();
        assert_eq!(got, vec!["あの", "えー"]);
    }

    // ----- v0.1.10 機能 A: 混雑通知バナー設定 -----

    #[test]
    fn congestion_threshold_defaults_to_5000ms_when_unset() {
        let path = tmp_db("congestion_default");
        let db = ConfigDb::open(&path).expect("open");
        assert_eq!(
            db.get_congestion_threshold_ms().unwrap(),
            DEFAULT_CONGESTION_THRESHOLD_MS
        );
        assert!(!db.get_congestion_show_in_hedged().unwrap());
    }

    #[test]
    fn congestion_threshold_roundtrip_and_clamp() {
        let path = tmp_db("congestion_roundtrip");
        let db = ConfigDb::open(&path).expect("open");
        // 正常値
        db.set_congestion_threshold_ms(3000).unwrap();
        assert_eq!(db.get_congestion_threshold_ms().unwrap(), 3000);
        // 上限超過 → クランプ
        db.set_congestion_threshold_ms(99_999).unwrap();
        assert_eq!(
            db.get_congestion_threshold_ms().unwrap(),
            CONGESTION_THRESHOLD_MAX_MS
        );
        // 下限未満 → クランプ
        db.set_congestion_threshold_ms(100).unwrap();
        assert_eq!(
            db.get_congestion_threshold_ms().unwrap(),
            CONGESTION_THRESHOLD_MIN_MS
        );
    }

    #[test]
    fn congestion_show_in_hedged_roundtrip() {
        let path = tmp_db("congestion_hedged");
        let db = ConfigDb::open(&path).expect("open");
        db.set_congestion_show_in_hedged(true).unwrap();
        assert!(db.get_congestion_show_in_hedged().unwrap());
        db.set_congestion_show_in_hedged(false).unwrap();
        assert!(!db.get_congestion_show_in_hedged().unwrap());
    }

    // ----- v0.1.10 機能 D: ホットキー競合状態 -----

    #[test]
    fn hotkey_conflict_state_defaults_to_none_when_unset() {
        let path = tmp_db("hotkey_conflict_default");
        let db = ConfigDb::open(&path).expect("open");
        assert_eq!(db.get_hotkey_conflict_state_json().unwrap(), None);
    }

    #[test]
    fn hotkey_conflict_state_roundtrip() {
        let path = tmp_db("hotkey_conflict_roundtrip");
        let db = ConfigDb::open(&path).expect("open");
        let json =
            r#"{"kind":"Conflict","code":"KeyM","modifiers":["Control","Shift"],"detected_at":1717000000}"#;
        db.set_hotkey_conflict_state_json(json).unwrap();
        assert_eq!(
            db.get_hotkey_conflict_state_json().unwrap().as_deref(),
            Some(json)
        );
        // 上書きで None 状態に戻す
        db.set_hotkey_conflict_state_json(r#"{"kind":"None"}"#)
            .unwrap();
        assert_eq!(
            db.get_hotkey_conflict_state_json().unwrap().as_deref(),
            Some(r#"{"kind":"None"}"#)
        );
    }

    // ----- v0.1.12 F2: マイク入力低レベル警告 -----

    #[test]
    fn mic_low_level_defaults_when_unset() {
        let path = tmp_db("mic_low_default");
        let db = ConfigDb::open(&path).expect("open");
        assert!(
            (db.get_mic_low_level_threshold().unwrap() - DEFAULT_MIC_LOW_LEVEL_THRESHOLD).abs()
                < f32::EPSILON
        );
        assert_eq!(
            db.get_mic_low_level_window_ms().unwrap(),
            DEFAULT_MIC_LOW_LEVEL_WINDOW_MS
        );
    }

    #[test]
    fn mic_low_level_threshold_roundtrip_and_clamp() {
        let path = tmp_db("mic_low_threshold");
        let db = ConfigDb::open(&path).expect("open");
        db.set_mic_low_level_threshold(0.01).unwrap();
        assert!((db.get_mic_low_level_threshold().unwrap() - 0.01).abs() < 1e-6);
        // 範囲外 → default
        db.set_mic_low_level_threshold(-0.5).unwrap();
        assert!(
            (db.get_mic_low_level_threshold().unwrap() - DEFAULT_MIC_LOW_LEVEL_THRESHOLD).abs()
                < f32::EPSILON
        );
        db.set_mic_low_level_threshold(2.0).unwrap();
        assert!(
            (db.get_mic_low_level_threshold().unwrap() - DEFAULT_MIC_LOW_LEVEL_THRESHOLD).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn mic_low_level_window_ms_roundtrip_and_zero_guard() {
        let path = tmp_db("mic_low_window");
        let db = ConfigDb::open(&path).expect("open");
        db.set_mic_low_level_window_ms(1500).unwrap();
        assert_eq!(db.get_mic_low_level_window_ms().unwrap(), 1500);
        // 0 → default
        db.set_mic_low_level_window_ms(0).unwrap();
        assert_eq!(
            db.get_mic_low_level_window_ms().unwrap(),
            DEFAULT_MIC_LOW_LEVEL_WINDOW_MS
        );
    }

    #[test]
    fn mic_low_level_threshold_parse_failure_falls_back_to_default() {
        let path = tmp_db("mic_low_parse_fail");
        let db = ConfigDb::open(&path).expect("open");
        // 直接 settings_kv に不正値を入れて parse 失敗を再現
        db.set_kv(MIC_LOW_LEVEL_THRESHOLD_KEY, "notanumber").unwrap();
        assert!(
            (db.get_mic_low_level_threshold().unwrap() - DEFAULT_MIC_LOW_LEVEL_THRESHOLD).abs()
                < f32::EPSILON
        );
        db.set_kv(MIC_LOW_LEVEL_WINDOW_MS_KEY, "ms3000").unwrap();
        assert_eq!(
            db.get_mic_low_level_window_ms().unwrap(),
            DEFAULT_MIC_LOW_LEVEL_WINDOW_MS
        );
    }

    #[test]
    fn migration_log_insert_and_latest() {
        let path = tmp_db("log");
        let db = ConfigDb::open(&path).expect("open");
        assert!(db.latest_migration_log().unwrap().is_none());

        let id1 = db.insert_migration_log("v1_config_json", 5).unwrap();
        let id2 = db.insert_migration_log("v1_config_json", 7).unwrap();
        assert!(id2 > id1);

        let (src, _ts, cnt) = db.latest_migration_log().unwrap().expect("Some");
        assert_eq!(src, "v1_config_json");
        assert_eq!(cnt, 7);
    }
}
