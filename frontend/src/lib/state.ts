/* RushVoice v2 12 状態定義（v2.6 §4.1 ベース）
 * Day 2: 見た目スケッチ用・実機能なし
 * Phase 2 で URL クエリ切替は削除予定
 */

export type RecordingState =
  | 'idle'
  | 'recording'
  | 'transcribing'
  | 'thinking-slow'
  | 'recording-longhold'
  | 'network-slow'
  | 'network-fail-groq'
  | 'network-fail-both'
  | 'network-recovering'
  | 'success'
  | 'empty'
  | 'hallucination'

export interface StateConfig {
  key: RecordingState
  label: string
  description: string
  tone: 'neutral' | 'active' | 'warning' | 'error' | 'success'
}

export const STATE_CONFIGS: StateConfig[] = [
  { key: 'idle', label: '待機中', description: 'F1 キーを押して録音を開始', tone: 'neutral' },
  { key: 'recording', label: '録音中', description: 'マイク入力を受信しています', tone: 'active' },
  { key: 'transcribing', label: '文字起こし中', description: 'Groq に送信中…', tone: 'active' },
  { key: 'thinking-slow', label: '思考中…', description: '通常より時間がかかっています', tone: 'warning' },
  { key: 'recording-longhold', label: '録音時間が長くなっています', description: '20 秒経過・続けますか？', tone: 'warning' },
  { key: 'network-slow', label: '通信が遅延しています', description: 'リトライ中…', tone: 'warning' },
  { key: 'network-fail-groq', label: 'Groq 応答なし', description: 'OpenAI に切替えました', tone: 'warning' },
  { key: 'network-fail-both', label: '通信失敗', description: '両プロバイダーとも応答なし', tone: 'error' },
  { key: 'network-recovering', label: '接続復旧中', description: 'もうしばらくお待ちください', tone: 'warning' },
  { key: 'success', label: '完了', description: 'クリップボードへコピー済み', tone: 'success' },
  { key: 'empty', label: '音声が検出されませんでした', description: '無音または VAD で破棄', tone: 'neutral' },
  { key: 'hallucination', label: 'ハルシ検知・破棄', description: '「ご視聴ありがとうございました」等を除去', tone: 'neutral' },
]

export function getStateConfig(key: RecordingState): StateConfig {
  return STATE_CONFIGS.find((c) => c.key === key) ?? STATE_CONFIGS[0]
}

export function parseStateFromUrl(): RecordingState {
  if (typeof window === 'undefined') return 'idle'
  const params = new URLSearchParams(window.location.search)
  const state = params.get('state') as RecordingState | null
  if (!state) return 'idle'
  return STATE_CONFIGS.find((c) => c.key === state) ? state : 'idle'
}
