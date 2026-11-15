// Day 39 (2026-06-03) Mac 致命バグ修正: APPDATA 環境変数 (Windows 専用) 直接参照を全廃する。
//
// 背景:
//   v0.1.16 以前は全 default_*_path() が `std::env::var("APPDATA")` を直接参照しており、
//   Mac 実機では Err(_) フォールバックの相対パス "RushVoice/xxx.db" に落ちて
//   .app バンドル内の作業ディレクトリで mkdir / open に失敗していた。
//   Phase 5 Step C-1/C-2/C-3 で clipboard / keyboard_hook / crypto は cfg 抽象化されたが、
//   DB / chunk_dir パス系 8 箇所が抜けていた (Day 39 で発見)。
//
// 方針:
//   `dirs::data_dir()` で OS 標準のユーザーデータ格納ディレクトリを取得し、
//   その下に "RushVoice" サブディレクトリを付けて返す共通関数を提供する。
//   Windows: %APPDATA%\RushVoice (= FOLDERID_RoamingAppData) — 既存ユーザーパス不変
//   macOS:   ~/Library/Application Support/RushVoice
//   Linux:   ~/.local/share/RushVoice (将来用)
//
// 互換性:
//   Windows での dirs::data_dir() は %APPDATA% と完全一致する (FOLDERID_RoamingAppData は
//   APPDATA 環境変数と同じ実体を指すため)。既存ユーザーの DB ファイルパスは変わらない。
//
// ---------------------------------------------------------------------------
// 🔴 2026-08-16 段階2 Phase 1-1: 無料版 RushVoice Air との関係を明文化した。
//
//   設計書: docs/phase2_paths_design_2026-08-16.md
//
//   これまで有料版側には「相手のフォルダを触らない」防御が 1 つも無かった。
//   Air 側 (rushvoice-lite/src-tauri/src/paths.rs) には定数 + 実行時ガード + テスト 2 本の
//   三重の守りがあるのに、有料版側は無防備という非対称な状態だった。
//   共通化 (crates/core) で移行機構を共有した瞬間に事故になるため、先に対称化する。
//
//   判断 (2026-08-16): **無料版 Air から有料版への引き継ぎは「する」。片道のみ。**
//     - 引き継ぐのは *履歴と単語登録だけ* (history.db / glossary.db)
//     - Air 側は読むだけ。消さない・書き換えない
//     - 既にファイルがあれば上書きしない (有料版を使い込んだ後に Air を入れても潰れない)
//     - 逆方向 (有料 → Air) は行わない。Air 側の paths.rs が有料版を forbidden にしている
//
//   config.db と config.secrets.encrypted.json は **意図的に引き継がない**。
//   両製品で config_db.rs のスキーマが食い違っており (2026-08-16 実測で 56 行差)、
//   Air の config を有料版が開くと壊れうるため。API キーの再登録 1 回で済む方を選ぶ。
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

/// 現行のデータ格納ディレクトリ名。
const APP_DIR: &str = "RushVoice";

/// 引き継ぎ元 (読み取り専用)。**新しい順**に並べる。
///
/// Air は自分の旧世代 (VoiceHarness / RushVoiceLite) を `RushVoiceAir` へ集約済みなので、
/// 有料版はその 1 つだけを見ればよい。Air の祖先まで手を伸ばさないこと
/// (`FORBIDDEN_APP_DIRS` で明示的に禁止している)。
const LEGACY_APP_DIRS: &[&str] = &["RushVoiceAir"];

/// 🔴 **読みも書きもしないディレクトリ名。**
///
/// Air の旧世代。Air 自身が `RushVoiceAir` へまとめる責任を持っているので、
/// 有料版がここへ直接手を伸ばすと二重取り込み・世代の取り違えが起きる。
const FORBIDDEN_APP_DIRS: &[&str] = &["VoiceHarness", "RushVoiceLite"];

/// 引き継ぐ SQLite DB。`-wal` / `-shm` のサイドカーも一緒に運ぶ。
///
/// 判断で「履歴と単語登録」に絞っている (ファイル先頭のコメント参照)。
/// `config.db` / `profile.db` / `app_rules.db` / `meeting_logs.db` / `alias.db` は
/// 有料版固有か、スキーマが食い違うため対象外。
const MIGRATE_DBS: &[&str] = &["history.db", "glossary.db"];

/// SQLite の WAL モードで本体 `.db` と対になるサイドカー。
///
/// **これを一緒にコピーしないと履歴が消える。** 2026-08-11 に開発機の実データで
/// 発覚した事故 (history.db が 4 KB なのに history.db-wal が 1.5 MB) と同型。
/// WAL は「全接続が閉じた時」に本体へ書き戻される (checkpoint) ため、
/// 旧アプリが起動したままだと本体はほぼ空のままになる。
///
/// コピー順は必ず「本体 → -wal → -shm」。
const SQLITE_SIDECARS: &[&str] = &["-wal", "-shm"];

/// RushVoice のデータ格納ディレクトリを返す。
///
/// - Windows: `%APPDATA%\RushVoice`
/// - macOS:   `~/Library/Application Support/RushVoice`
/// - Linux:   `~/.local/share/RushVoice`
/// - フォールバック: 相対パス `RushVoice`
///
/// 各 DB の `default_*_path()` はこの戻り値に `.db` ファイル名を join する。
pub fn app_data_dir() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join(APP_DIR))
        .unwrap_or_else(|| PathBuf::from(APP_DIR))
}

/// `app_data_dir()` の旧名。既存の呼び出し 9 箇所のために残している。
///
/// crates/core への切り出し (段階2 Phase 1-3) で呼び出し側を `app_data_dir()` へ
/// 揃えたら削除する。
pub fn rushvoice_data_dir() -> PathBuf {
    app_data_dir()
}

/// 引き継ぎ元ディレクトリを**新しい順に**返す。探索にのみ使う。
pub fn legacy_data_dirs() -> Vec<PathBuf> {
    LEGACY_APP_DIRS
        .iter()
        .map(|name| {
            dirs::data_dir()
                .map(|d| d.join(name))
                .unwrap_or_else(|| PathBuf::from(name))
        })
        .collect()
}

/// 無料版 Air から履歴・単語登録を引き継ぐ (片道・コピー・上書きしない)。
///
/// 戻り値: 実際にコピーしたものの一覧。`"RushVoiceAir/history.db"` のように
/// **どこから来たか**を含める (障害調査でどれが採用されたか分かるように)。
pub fn migrate_legacy_data_dir() -> Vec<String> {
    let sources: Vec<(&str, PathBuf)> = LEGACY_APP_DIRS
        .iter()
        .copied()
        .zip(legacy_data_dirs())
        .collect();
    migrate_all(&sources, &app_data_dir())
}

/// `migrate_legacy_data_dir` の本体。テストから任意のパスで呼べるよう分離する。
///
/// `sources` は **新しい順**。先に置かれたファイルが勝つ (`migrate_dir` が
/// 既存ファイルを上書きしないため)。
fn migrate_all(sources: &[(&str, PathBuf)], current: &Path) -> Vec<String> {
    let mut copied = Vec::new();

    for (name, legacy) in sources {
        // 🔴 禁止ディレクトリは読まない。定数を編集した時に実行時にも気付けるようにする。
        if FORBIDDEN_APP_DIRS.contains(name) {
            log::error!(
                "[migrate] 禁止ディレクトリ {name} を引き継ぎ元にしようとしました。飛ばします。"
            );
            continue;
        }
        for file in migrate_dir(legacy, current) {
            copied.push(format!("{name}/{file}"));
        }
    }

    copied
}

/// 1 ディレクトリ分の引き継ぎ。
fn migrate_dir(legacy: &Path, current: &Path) -> Vec<String> {
    let mut copied = Vec::new();

    if !legacy.is_dir() || legacy == current {
        return copied;
    }

    // SQLite: 本体が未移行の時だけ、本体 + サイドカーをまとめて運ぶ。
    // 判定を本体だけで行うのは、本体と WAL がちぐはぐな世代になるのを防ぐため。
    for db in MIGRATE_DBS {
        if current.join(db).exists() {
            continue;
        }
        if !legacy.join(db).is_file() {
            continue;
        }
        // 順序が重要: 本体 → -wal → -shm (SQLITE_SIDECARS のコメント参照)
        if copy_one(legacy, current, db, &mut copied) {
            for suffix in SQLITE_SIDECARS {
                let name = format!("{db}{suffix}");
                copy_one(legacy, current, &name, &mut copied);
            }
        }
    }

    copied
}

/// 1 ファイルをコピーする。成功したら `copied` に名前を積んで true を返す。
///
/// 失敗しても panic せず false を返すだけにしている。引き継ぎに失敗してアプリが
/// 起動しなくなる方が、履歴が一部欠けるより実害が大きいため。
fn copy_one(legacy: &Path, current: &Path, name: &str, copied: &mut Vec<String>) -> bool {
    let src = legacy.join(name);
    let dst = current.join(name);
    if !src.is_file() {
        return false;
    }
    if let Some(parent) = dst.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => {
            copied.push(name.to_string());
            true
        }
        Err(e) => {
            log::warn!("無料版からの引き継ぎに失敗しました ({name}): {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_dir_ends_with_rushvoice() {
        let p = app_data_dir();
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("RushVoice"),
            "末尾コンポーネントは 'RushVoice' であるべき: {:?}",
            p
        );
    }

    /// 旧名は新名と完全に同じものを返すこと (呼び出し 9 箇所の互換)。
    #[test]
    fn legacy_alias_matches_new_name() {
        assert_eq!(rushvoice_data_dir(), app_data_dir());
    }

    #[test]
    fn app_data_dir_is_absolute_on_real_os() {
        // dirs::data_dir() が Some を返す通常環境では絶対パスのはず。
        // CI 等で None になる場合は相対パスにフォールバックするので保証しない。
        if dirs::data_dir().is_some() {
            assert!(
                app_data_dir().is_absolute(),
                "通常環境では絶対パスであるべき: {:?}",
                app_data_dir()
            );
        }
    }

    /// 🔴 自分の保管先を引き継ぎ元や禁止先に混ぜないこと。混ざると自分自身を食う。
    #[test]
    fn app_dir_is_not_a_source_or_forbidden() {
        assert!(!LEGACY_APP_DIRS.contains(&APP_DIR));
        assert!(!FORBIDDEN_APP_DIRS.contains(&APP_DIR));
    }

    /// 🔴 引き継ぎ元と禁止先が重ならないこと (どちらの意図か曖昧な定数を作らない)。
    #[test]
    fn sources_and_forbidden_do_not_overlap() {
        for name in LEGACY_APP_DIRS {
            assert!(
                !FORBIDDEN_APP_DIRS.contains(name),
                "{name} が引き継ぎ元と禁止先の両方に入っている"
            );
        }
    }

    /// 🔴 Air の祖先へ直接手を伸ばさないこと。
    ///
    /// Air 自身が VoiceHarness / RushVoiceLite を RushVoiceAir へまとめる責任を持つ。
    /// 有料版が直接読むと二重取り込みと世代の取り違えが起きる。
    #[test]
    fn air_ancestors_are_forbidden() {
        assert!(FORBIDDEN_APP_DIRS.contains(&"VoiceHarness"));
        assert!(FORBIDDEN_APP_DIRS.contains(&"RushVoiceLite"));
        assert_eq!(LEGACY_APP_DIRS, &["RushVoiceAir"]);
    }

    /// 引き継ぐのは履歴と単語登録だけ。設定と暗号化シークレットは運ばない。
    ///
    /// config_db.rs のスキーマが両製品で食い違うため (2026-08-16 実測)。
    #[test]
    fn only_history_and_glossary_are_migrated() {
        assert_eq!(MIGRATE_DBS, &["history.db", "glossary.db"]);
        assert!(!MIGRATE_DBS.contains(&"config.db"));
        assert!(!MIGRATE_DBS.contains(&"profile.db"));
    }

    /// Air のディレクトリからコピーされ、Air 側は無改変で残ること。
    #[test]
    fn migrate_copies_from_air_and_keeps_source() {
        let base = std::env::temp_dir().join("rv_paid_migrate_basic");
        let _ = std::fs::remove_dir_all(&base);
        let air = base.join("RushVoiceAir");
        let current = base.join("RushVoice");
        std::fs::create_dir_all(&air).unwrap();
        std::fs::write(air.join("history.db"), b"air-history").unwrap();
        std::fs::write(air.join("glossary.db"), b"air-glossary").unwrap();
        // 対象外のものは運ばない
        std::fs::write(air.join("config.db"), b"air-config").unwrap();
        std::fs::write(air.join("meta.db"), b"air-meta").unwrap();

        let copied = migrate_dir(&air, &current);

        assert!(copied.contains(&"history.db".to_string()));
        assert!(copied.contains(&"glossary.db".to_string()));
        assert!(!copied.contains(&"config.db".to_string()), "設定は運ばない");
        assert!(!copied.contains(&"meta.db".to_string()), "Air 固有は運ばない");
        assert!(!current.join("config.db").exists());
        // Air 側は無改変で残る (無料版へ戻せるように)
        assert_eq!(std::fs::read(air.join("history.db")).unwrap(), b"air-history");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// SQLite の `-wal` / `-shm` も一緒に運ばれること。
    ///
    /// これが無いと履歴が消える (2026-08-11 実データで発覚)。回帰したら即失敗させる。
    #[test]
    fn migrate_carries_sqlite_wal_and_shm() {
        let base = std::env::temp_dir().join("rv_paid_migrate_wal");
        let _ = std::fs::remove_dir_all(&base);
        let air = base.join("RushVoiceAir");
        let current = base.join("RushVoice");
        std::fs::create_dir_all(&air).unwrap();
        // 実データと同じ形: 本体はほぼ空で、中身は WAL 側にある
        std::fs::write(air.join("history.db"), b"header-only").unwrap();
        std::fs::write(air.join("history.db-wal"), b"the-actual-history").unwrap();
        std::fs::write(air.join("history.db-shm"), b"shm").unwrap();

        let copied = migrate_dir(&air, &current);

        assert!(
            copied.contains(&"history.db-wal".to_string()),
            "-wal を運ばないと履歴が失われる: {:?}",
            copied
        );
        assert!(copied.contains(&"history.db-shm".to_string()));
        assert_eq!(
            std::fs::read(current.join("history.db-wal")).unwrap(),
            b"the-actual-history"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 🔴 有料版を使い込んだ後に Air を入れても、有料版のデータが潰れないこと。
    #[test]
    fn migrate_does_not_overwrite_existing_paid_data() {
        let base = std::env::temp_dir().join("rv_paid_migrate_no_overwrite");
        let _ = std::fs::remove_dir_all(&base);
        let air = base.join("RushVoiceAir");
        let current = base.join("RushVoice");
        std::fs::create_dir_all(&air).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(air.join("history.db"), b"air").unwrap();
        std::fs::write(current.join("history.db"), b"paid-in-use").unwrap();

        let copied = migrate_dir(&air, &current);

        assert!(copied.is_empty(), "既存の有料版データは対象外: {:?}", copied);
        assert_eq!(
            std::fs::read(current.join("history.db")).unwrap(),
            b"paid-in-use"
        );
        // 本体が既存なら WAL だけ後から上書きしないこと (世代のちぐはぐを防ぐ)
        std::fs::write(air.join("history.db-wal"), b"air-wal").unwrap();
        assert!(migrate_dir(&air, &current).is_empty());
        assert!(!current.join("history.db-wal").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 🔴 禁止ディレクトリが引き継ぎ元に紛れ込んでも、読まずに飛ばすこと。
    ///
    /// `air_ancestors_are_forbidden` が定数を守る一段目、これが実行時の二段目。
    #[test]
    fn migrate_skips_forbidden_dir() {
        let base = std::env::temp_dir().join("rv_paid_migrate_forbidden");
        let _ = std::fs::remove_dir_all(&base);
        let forbidden = base.join("VoiceHarness");
        let current = base.join("RushVoice");
        std::fs::create_dir_all(&forbidden).unwrap();
        std::fs::write(forbidden.join("history.db"), b"air-ancestor").unwrap();

        let sources = [("VoiceHarness", forbidden.clone())];
        let copied = migrate_all(&sources, &current);

        assert!(copied.is_empty(), "禁止先から吸い上げてはいけない: {:?}", copied);
        assert!(!current.join("history.db").exists());
        // 相手側は無改変
        assert_eq!(
            std::fs::read(forbidden.join("history.db")).unwrap(),
            b"air-ancestor"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Air を使っていない人 (新規インストール) では何も起きないこと。
    #[test]
    fn migrate_noop_without_air() {
        let base = std::env::temp_dir().join("rv_paid_migrate_fresh");
        let _ = std::fs::remove_dir_all(&base);
        let air = base.join("RushVoiceAir");
        let current = base.join("RushVoice");

        let copied = migrate_dir(&air, &current);

        assert!(copied.is_empty());
        assert!(!current.exists());
    }
}
