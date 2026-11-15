/* profile.ts — Phase 2 C Wave 2 プロファイル型定義
 *
 * SSOT v2.10 §8.3.1 PT-01〜PT-10
 * Rust 側 src-tauri/src/profile_db.rs::Profile と 1:1 対応。
 * snake_case のフィールド名は Rust struct 由来 (serde はデフォルトでそのまま JSON 化)。
 */

export type EmojiDensity = 'none' | 'few' | 'normal' | 'many'

export interface Profile {
  /** SQLite AUTOINCREMENT rowid。INSERT 前は null。 */
  id: number | null
  /** UI 表示用の名前 */
  display_name: string
  /** Whisper API に渡す prompt パラメータ (50-100 文字推奨) */
  whisper_prompt: string
  /** LLM 後処理用システムプロンプト (最大 1200 文字) */
  llm_postprocess_prompt: string
  /** フィラー (えー・あのー) を除去するか */
  filler_removal: boolean
  /** 絵文字密度 ('none'|'few'|'normal'|'many')。LLM 後処理の絵文字指示に接続済み。 */
  emoji_density: EmojiDensity
  /** ビルトインプリセットなら true (削除不可・「デフォルトに戻す」対象) */
  is_builtin: boolean
  /** ビルトインの識別子 ('raw' | 'natural' | 'mail' | 'business_doc' | 'casual')・user 作成なら null */
  builtin_key: string | null
  /** Unix epoch seconds, UTC */
  created_at: number
  /** Unix epoch seconds, UTC */
  updated_at: number
}

/** LLM プロンプト最大文字数 (Rust profile_db.rs::LLM_PROMPT_MAX_CHARS と同期) */
export const LLM_PROMPT_MAX_CHARS = 1200

/** 新規追加用の空プロファイル生成。
 *  is_builtin / builtin_key は Rust 側 INSERT で強制的に false / NULL に固定される。
 */
export function emptyProfile(): Profile {
  return {
    id: null,
    display_name: '',
    whisper_prompt: '',
    llm_postprocess_prompt: '',
    filler_removal: true,
    emoji_density: 'none',
    is_builtin: false,
    builtin_key: null,
    created_at: 0,
    updated_at: 0,
  }
}

/** 絵文字密度の日本語ラベル (PT-08 RadioGroup 表示用) */
export const EMOJI_DENSITY_LABELS: Record<EmojiDensity, string> = {
  none: 'なし',
  few: '少なめ',
  normal: 'ふつう',
  many: '多め',
}

/** ビルトイン display_name のフォールバック (UI 説明用) */
export function builtinKeyToHint(key: string | null): string | null {
  switch (key) {
    case 'raw':
      return '後処理なし・Whisper 生テキストをそのまま貼付'
    case 'natural':
      return '口語の自然さを保ちつつ句読点補完'
    case 'mail':
      return 'メール本文向け・敬語整形'
    case 'business_doc':
      return '丁寧語・敬体に整える (依頼文は依頼のまま)'
    case 'casual':
      return 'チャット返信向け・砕けた口語'
    default:
      return null
  }
}
