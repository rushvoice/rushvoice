/* app_rule.ts — Phase 2 C Wave 1 Step 3 アプリ別ルール型定義
 *
 * SSOT v2.10 §8.3.1 AT-06
 * Rust 側 src-tauri/src/app_rules_db.rs::AppRule と 1:1 対応。
 * snake_case のフィールド名は Rust struct 由来 (serde はデフォルトでそのまま JSON 化)。
 *
 * Wave 1 では profileId は文字列入力のみ (Wave 2 PT-01 で DropdownMenu 化)。
 * 重複検出は Wave 2 AT-04 で追加 (Wave 1 では実装しない)。
 */

export interface AppRule {
  /** SQLite AUTOINCREMENT rowid。INSERT 前は null。 */
  id: number | null
  /** プロファイル ID (Wave 1 では文字列入力) */
  profile_id: string
  /** 対象アプリ名 (例: "chrome.exe", "Code.exe") */
  app_name: string
  /** ウィンドウタイトル正規表現 (空文字列可) */
  title_regex: string
  /** Unix epoch seconds, UTC */
  created_at: number
  /** Unix epoch seconds, UTC */
  updated_at: number
}

/** 新規追加用の空ルール生成。created_at / updated_at は Rust 側で上書きされる。 */
export function emptyAppRule(): AppRule {
  return {
    id: null,
    profile_id: '',
    app_name: '',
    title_regex: '',
    created_at: 0,
    updated_at: 0,
  }
}

/** 完全空行判定 (3 フィールド全空白なら true)。replace_all 送信前のクライアント側チェックに使用。 */
export function isEmptyRule(rule: AppRule): boolean {
  return (
    rule.profile_id.trim() === '' &&
    rule.app_name.trim() === '' &&
    rule.title_regex.trim() === ''
  )
}
