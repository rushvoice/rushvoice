// types/meeting.ts — v0.1.16 機能 L (議事録モード) Day 4: frontend 型定義
//
// SSOT: 内部設計メモ §機能L
// 委任 prompt: 内部設計メモ §2.1
//
// backend serde 整合:
//   - MeetingState: meeting_commands.rs #[serde(rename_all = "lowercase")] と一致
//   - MeetingLogDto: meeting_commands.rs `MeetingLogDto` フィールド完全一致 (snake_case)
//   - MeetingSegment: meeting.rs `MeetingSegment` フィールド完全一致 (snake_case)
//   - MeetingStateEvent.id: backend は Option<String> → TS では string | null

/** 議事録セッション状態 (backend `MeetingState` と完全一致・lowercase serde) */
export type MeetingState = 'idle' | 'recording' | 'transcribing' | 'completed' | 'error'

/** `meeting:state-changed` event payload */
export interface MeetingStateEvent {
  state: MeetingState
  id: string | null
}

/** 議事録 1 件 DTO (backend `MeetingLogDto` と完全一致) */
export interface MeetingLogDto {
  id: string
  start_at: number      // epoch secs (i64)
  duration_ms: number   // i64
  title: string
  segments_json: string
  chunk_dir: string
  created_at: number    // epoch secs (i64)
}

/** 1 segment (backend `MeetingSegment` と完全一致) */
export interface MeetingSegment {
  chunk_index: number
  start_ms: number
  end_ms: number
  text: string
}

/** `meeting:progress` event payload (backend 側 serde_json::json! と一致) */
export interface MeetingProgressEvent {
  chunk_index: number
  phase: 'chunk_finalized' | 'transcribing' | 'parsed'
}

/** エクスポート形式 (backend `meeting_export` の format 引数と一致) */
export type ExportFormat = 'txt' | 'md' | 'srt'

/** Day 39 リピート除去フィルタ event (`meeting:repeat-filtered`)
 *  backend: meeting_commands.rs:478 で 1 chunk あたり dropped>0 のときのみ emit
 *  backend 純粋関数: meeting.rs filter_repeat_segments(N=5)
 */
export interface MeetingRepeatFilteredEvent {
  chunk_index: number
  dropped: number
  threshold: number
}
