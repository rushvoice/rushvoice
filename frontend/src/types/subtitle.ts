// subtitle.ts — セミナー字幕モード (feature/seminar-subtitle) 型定義
// backend subtitle_commands.rs の emit payload と一致させる (snake_case)

/** backend `subtitle:line` event payload (1 発話分の確定字幕) */
export interface SubtitleLineEvent {
  seq: number
  original: string
  /** 正規化済み検出言語コード: ja/en/zh/id/ms/th。未知は null */
  detected_lang: string | null
  /** 現地語段 (講習開始時に固定した出力言語) */
  local: string
  /** 日本語段 */
  ja: string
  /** 英語 3 段目 (設定 OFF / target=en のとき null) */
  en: string | null
  /** 発話 WAV 受信から emit までの処理遅延 ms */
  latency_ms: number
}

/** backend `subtitle:state` event payload */
export interface SubtitleStateEvent {
  state: 'listening' | 'idle' | 'error'
  message: string | null
}

/** backend `subtitle_get_state` 戻り値 (実行中でなければ null) */
export interface SubtitleRunConfig {
  target_lang: string
  show_english: boolean
}

/** frontend 間 (コントロールページ → 字幕ウィンドウ) の表示設定 event `subtitle:config` */
export interface SubtitleDisplayConfig {
  fontScale: number
}

/** 出力言語の選択肢 (backend subtitle.rs SUPPORTED_TARGET_LANGS と一致) */
export const TARGET_LANG_OPTIONS: { code: string; label: string }[] = [
  { code: 'zh-TW', label: '繁体字中国語（台湾）' },
  { code: 'id', label: 'インドネシア語' },
  { code: 'ms', label: 'マレー語' },
  { code: 'th', label: 'タイ語' },
  { code: 'en', label: '英語' },
]

/** 検出言語コード → 日本語ラベル */
export function detectedLangLabel(code: string | null): string {
  switch (code) {
    case 'ja': return '日本語'
    case 'en': return '英語'
    case 'zh': return '中国語'
    case 'id': return 'インドネシア語'
    case 'ms': return 'マレー語'
    case 'th': return 'タイ語'
    default: return '不明'
  }
}
