/* glossary.ts — Phase 2 C Wave 1 辞書エントリ型定義
 *
 * SSOT v2.10 §8.3.1 DT-01 / DT-02
 * Rust 側 src-tauri/src/glossary_db.rs::GlossaryEntry と 1:1 対応。
 * snake_case のフィールド名は Rust struct 由来 (serde はデフォルトでそのまま JSON 化)。
 *
 * DT-01 表示列: pattern / replacement
 * DT-02 内部列: context / note / is_regex (Wave 1 では編集フォームのみ・テーブル表示せず保持)
 */

export interface GlossaryEntry {
  /** SQLite AUTOINCREMENT rowid。INSERT 前は null。 */
  id: number | null
  pattern: string
  replacement: string
  /** 適用文脈 (DT-02 内部列) */
  context: string | null
  /** 自由記述メモ (DT-02 内部列) */
  note: string | null
  /** 正規表現として扱うか (DT-02 内部列・Phase 3 L3 置換で利用) */
  is_regex: boolean
  /** Unix epoch seconds, UTC */
  created_at: number
  /** Unix epoch seconds, UTC */
  updated_at: number
}

/** find_glossary_overlapping の戻り値 (Rust src/glossary_db.rs::OverlapResult 1:1)。
 *  Step 2.5: 「/」区切り sub-pattern 単位の重複検出結果。 */
export interface OverlapResult {
  entry: GlossaryEntry
  /** query 側の出現順で重複した sub-pattern 一覧 */
  overlapping_sub_patterns: string[]
}

/** 新規追加用の空エントリ生成。created_at / updated_at は Rust 側で上書きされる */
export function emptyGlossaryEntry(): GlossaryEntry {
  return {
    id: null,
    pattern: '',
    replacement: '',
    context: null,
    note: null,
    is_regex: false,
    created_at: 0,
    updated_at: 0,
  }
}
