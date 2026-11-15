/* HistoryPage.tsx — Phase 2 D: HistoryDb 実機接続 + 編集学習 (Edit & Learn)
 *
 * 仕様: SSOT v2.10 §8.3.2.7 / 設計ドラフト phase2_d_history_sqlite_design_2026-05-22.md §7
 *
 * - 起動時に list_history(PAGE_SIZE, 0) で全件取得 (PAGE_SIZE = HISTORY_RETENTION_COUNT = 50)
 * - 履歴は backend 側で「直近 50 件」のみ保持されるため、1 ページで全件表示する
 *   (v0.1.7.1 改訂で件数ベース保持に変更・追加読込 UI は撤去済)
 * - 表示優先: edited_text > refined_text > raw_text
 * - 削除: 確認なしで delete_history_item + 楽観的 UI 更新
 * - confidence_drop_flag: 行に警告アイコン表示
 * - 編集学習: ✏️ → インライン textarea → 保存 → diff検出 → 辞書追加提案バナー
 */

import { useCallback, useEffect, useState } from 'react'
import { AlertTriangle, Check, Copy, Loader2, Pencil, Trash2, X } from 'lucide-react'
// Day 25 Phase 2-G: alias 重複 3 択 Dialog 削除に伴い @radix-ui/react-dialog 依存も除去
import { toast } from 'sonner'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import {
  deleteHistoryItem,
  listHistory,
  updateHistoryEdit,
  upsertGlossaryEntry,
  type GlossaryEntry,
  type HistoryItem,
} from '../lib/tauri'

const PAGE_SIZE = 50

/** 表示用テキスト: 編集後 > 整形後 > 生 の優先順位 */
function displayText(item: HistoryItem): string {
  return item.edited_text ?? item.refined_text ?? item.raw_text
}

/** Whisper 出力テキスト: 編集学習の diff 基準 (refined_text > raw_text) */
function whisperText(item: HistoryItem): string {
  return item.refined_text ?? item.raw_text
}

/** ISO8601 datetime を日本語ローカル表示 */
const DATE_FMT = new Intl.DateTimeFormat('ja-JP', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
})
function formatCreatedAt(iso: string): string {
  const d = new Date(iso)
  if (isNaN(d.getTime())) return iso
  return DATE_FMT.format(d)
}

/**
 * v0.1.7.1 タスク C (2026-05-28): 文字起こし所要時間の警告判定。
 *
 * Groq API の負荷スパイク (queue 待ち・cold start) が発生すると開発機で
 * 「文字起こし開始から結果が返るまで体感 5-10 秒」になる事案があり、
 * ユーザーが UI 上で自己判定できるよう履歴行に併記 + 異常時のみ赤系で警告する。
 *
 * 警告条件 (どちらかに該当):
 *   - 文字起こし時間 (transcribe_ms) >= 5000ms (絶対値の遅さ)
 *   - 比率 transcribe_ms / duration_ms >= 1.0 (録音長と同等以上に処理がかかった = 体感遅延)
 *
 * duration_ms <= 0 (旧データ・欠損) の場合は絶対値だけで判定する。
 */
const TRANSCRIBE_SLOW_ABS_MS = 5000
const TRANSCRIBE_SLOW_RATIO = 1.0
function isTranscribeSlow(durationMs: number, transcribeMs: number): boolean {
  if (transcribeMs <= 0) return false
  if (transcribeMs >= TRANSCRIBE_SLOW_ABS_MS) return true
  if (durationMs > 0 && transcribeMs / durationMs >= TRANSCRIBE_SLOW_RATIO) return true
  return false
}

/**
 * 2 テキスト間の diff から辞書追加候補を 1 件抽出する。
 *
 * アルゴリズム: 最長共通接頭 / 接尾を取り、残った "変化領域" を返す。
 * 単一箇所の変更のみ対象。フィルタ条件:
 *   - pattern: 2〜10 文字
 *   - replacement: 1〜10 文字
 *   - どちらもスペースなし
 */
function computeDiffSuggestion(
  original: string,
  edited: string,
): { pattern: string; replacement: string } | null {
  if (original === edited) return null
  let prefix = 0
  while (
    prefix < original.length &&
    prefix < edited.length &&
    original[prefix] === edited[prefix]
  ) {
    prefix++
  }
  let suffix = 0
  while (
    suffix < original.length - prefix &&
    suffix < edited.length - prefix &&
    original[original.length - 1 - suffix] === edited[edited.length - 1 - suffix]
  ) {
    suffix++
  }
  const pattern = original.slice(prefix, suffix > 0 ? original.length - suffix : undefined)
  const replacement = edited.slice(prefix, suffix > 0 ? edited.length - suffix : undefined)
  if (!pattern || !replacement || pattern === replacement) return null
  if (pattern.length < 2 || pattern.length > 10) return null
  if (replacement.length < 1 || replacement.length > 10) return null
  if (/[\s　]/.test(pattern) || /[\s　]/.test(replacement)) return null
  return { pattern, replacement }
}

interface GlossaryProposal {
  pattern: string
  replacement: string
  adding: boolean
}

// Day 25 Phase 2-G: AliasDuplicateDialog interface は alias_db レイヤー削除と共に撤廃。

export function HistoryPage() {
  const [items, setItems] = useState<HistoryItem[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [editState, setEditState] = useState<{ id: number; text: string } | null>(null)
  const [editSaving, setEditSaving] = useState(false)
  const [glossaryProposal, setGlossaryProposal] = useState<GlossaryProposal | null>(null)
  // Day 25 Phase 2-G: alias 重複ダイアログ state は削除済 (alias_db レイヤーごと撤廃)
  // v0.1.7.1 改訂: 件数ベース保持 (50 件) に変更したため offset/hasMore/loadingMore は撤去

  const refetch = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const rows = await listHistory(PAGE_SIZE, 0)
      setItems(rows)
    } catch (e) {
      const msg = `${e}`
      setError(msg)
      toast.error(`履歴の読込に失敗: ${msg}`)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void (async () => {
      await refetch()
    })()
  }, [refetch])

  useEffect(() => {
    let unlisten: UnlistenFn | undefined
    let cancelled = false
    void (async () => {
      try {
        const fn = await listen<number>('history:updated', () => {
          void refetch()
        })
        if (cancelled) {
          fn()
        } else {
          unlisten = fn
        }
      } catch (e) {
        console.warn('[history] listen("history:updated") failed:', e)
      }
    })()
    return () => {
      cancelled = true
      if (unlisten) unlisten()
    }
  }, [refetch])

  // v0.1.7.1 改訂 (2026-05-28): 件数ベース保持 (50 件) のため追加読込ロジックは撤去。
  // history.db は backend cleanup により常に直近 50 件のみ保持される。

  async function handleDelete(item: HistoryItem) {
    if (item.id == null) return
    const id = item.id
    const prev = items
    setItems((cur) => cur.filter((x) => x.id !== id))
    try {
      await deleteHistoryItem(id)
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
      setItems(prev)
      refetch()
    }
  }

  async function handleCopy(item: HistoryItem) {
    try {
      await navigator.clipboard.writeText(displayText(item))
      toast.success('クリップボードにコピーしました')
    } catch (e) {
      toast.error(`コピー失敗: ${e}`)
    }
  }

  function handleEditStart(item: HistoryItem) {
    if (item.id == null) return
    setEditState({ id: item.id, text: displayText(item) })
    setGlossaryProposal(null)
  }

  function handleEditCancel() {
    setEditState(null)
  }

  async function handleEditSave(item: HistoryItem) {
    if (!editState || item.id == null) return
    const originalWhisper = whisperText(item)
    const editedText = editState.text.trim()
    if (!editedText) {
      toast.error('テキストが空です')
      return
    }
    setEditSaving(true)
    try {
      await updateHistoryEdit(item.id, editedText, '')
      setItems((prev) =>
        prev.map((x) => (x.id === item.id ? { ...x, edited_text: editedText } : x)),
      )
      setEditState(null)
      toast.success('編集を保存しました')
      const suggestion = computeDiffSuggestion(originalWhisper, editedText)
      if (suggestion) {
        setGlossaryProposal({ ...suggestion, adding: false })
      }
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setEditSaving(false)
    }
  }

  /**
   * Day 25 Phase 2-G: 「辞書に追加」緑バナーのハンドラ (v0.1.6 相当に復帰)。
   *
   * SSOT v3.3 §2 に基づき alias_db (ASR 補正レイヤー) 連動は完全削除し、
   * glossary_db への upsert のみ実施する。
   */
  async function handleAddToGlossary() {
    if (!glossaryProposal) return
    setGlossaryProposal((p) => (p ? { ...p, adding: true } : null))
    const proposal = glossaryProposal
    try {
      const entry: GlossaryEntry = {
        id: null,
        pattern: proposal.pattern,
        replacement: proposal.replacement,
        context: null,
        note: '履歴編集から自動学習',
        is_regex: false,
        created_at: 0,
        updated_at: 0,
      }
      await upsertGlossaryEntry(entry)
      toast.success(
        `辞書に追加しました: 「${proposal.pattern}」→「${proposal.replacement}」`,
      )
      setGlossaryProposal(null)
    } catch (e) {
      toast.error(`辞書追加失敗: ${e}`)
      setGlossaryProposal((p) => (p ? { ...p, adding: false } : null))
    }
  }

  return (
    <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
      <header className="mb-6">
        <h2 className="text-xl font-semibold">履歴</h2>
        <p className="text-sm text-neutral-500 mt-1">
          直近 50 件の文字起こし · {items.length} 件表示
        </p>
      </header>

      {/* 辞書追加提案バナー */}
      {glossaryProposal && (
        <div className="max-w-3xl mb-4 px-4 py-3 bg-emerald-950/50 border border-emerald-900/50 rounded-lg flex items-center gap-3">
          <div className="flex-1 min-w-0 text-sm">
            <span className="text-emerald-400 font-medium">辞書に追加しますか？</span>
            <span className="ml-2 text-neutral-300">
              <span className="font-mono text-red-300">{glossaryProposal.pattern}</span>
              <span className="text-neutral-500 mx-1.5">→</span>
              <span className="font-mono text-emerald-300">{glossaryProposal.replacement}</span>
            </span>
          </div>
          <div className="flex gap-2 shrink-0">
            <button
              type="button"
              onClick={handleAddToGlossary}
              disabled={glossaryProposal.adding}
              className="px-3 py-1 bg-emerald-800 hover:bg-emerald-700 text-xs rounded text-white disabled:opacity-50 flex items-center gap-1"
            >
              {glossaryProposal.adding ? (
                <Loader2 size={11} className="animate-spin" />
              ) : (
                <Check size={11} />
              )}
              追加
            </button>
            <button
              type="button"
              onClick={() => setGlossaryProposal(null)}
              className="px-3 py-1 bg-neutral-800 hover:bg-neutral-700 text-xs rounded text-neutral-400 flex items-center gap-1"
            >
              <X size={11} />
              スキップ
            </button>
          </div>
        </div>
      )}

      {/* Day 25 Phase 2-G: alias 補正辞書の重複 3 択ダイアログは alias_db 撤廃と共に削除 */}

      {/* エラー状態 */}
      {error && !loading && (
        <div className="max-w-3xl mb-4 px-4 py-3 bg-red-950/50 border border-red-900/50 rounded-lg text-sm text-red-300">
          履歴の読込に失敗しました: {error}
          <button
            type="button"
            onClick={refetch}
            className="ml-3 px-2 py-1 bg-red-900/50 hover:bg-red-900 text-xs rounded"
          >
            再試行
          </button>
        </div>
      )}

      {/* 初回ロード中 */}
      {loading && items.length === 0 && (
        <div className="max-w-3xl flex items-center gap-2 text-sm text-neutral-500">
          <Loader2 size={14} className="animate-spin" />
          <span>読み込み中…</span>
        </div>
      )}

      {/* 空状態 */}
      {!loading && !error && items.length === 0 && (
        <div className="max-w-3xl px-4 py-12 text-center text-sm text-neutral-500 border border-dashed border-neutral-800 rounded-lg">
          履歴がまだありません。録音すると自動的にここに記録されます。
        </div>
      )}

      {/* 一覧 */}
      {items.length > 0 && (
        <ul className="space-y-2 max-w-3xl">
          {items.map((item) => {
            const text = displayText(item)
            const isEdited = item.edited_text != null
            const isEditing = editState?.id === item.id
            return (
              <li
                key={item.id ?? `${item.created_at}-${item.raw_text}`}
                className="group flex items-start gap-3 px-4 py-3 bg-neutral-950 border border-neutral-800 rounded-lg hover:border-neutral-700 transition-colors"
              >
                {item.confidence_drop_flag && !isEditing && (
                  <AlertTriangle
                    size={14}
                    className="mt-1 text-amber-400 shrink-0"
                    aria-label="低信頼度の認識結果"
                  />
                )}
                <div className="flex-1 min-w-0">
                  {isEditing ? (
                    <>
                      <textarea
                        className="w-full bg-neutral-800 text-sm text-neutral-100 rounded px-2 py-1.5 resize-none border border-neutral-600 focus:border-emerald-500 focus:outline-none leading-relaxed"
                        rows={Math.max(2, Math.ceil(editState.text.length / 45))}
                        value={editState.text}
                        onChange={(e) =>
                          setEditState((s) => (s ? { ...s, text: e.target.value } : null))
                        }
                        onKeyDown={(e) => {
                          if (e.key === 'Escape') handleEditCancel()
                        }}
                        autoFocus
                      />
                      <div className="mt-1.5 flex gap-2">
                        <button
                          type="button"
                          onClick={() => handleEditSave(item)}
                          disabled={editSaving}
                          className="px-3 py-1 bg-emerald-800 hover:bg-emerald-700 text-xs rounded text-white disabled:opacity-50 flex items-center gap-1"
                        >
                          {editSaving ? (
                            <Loader2 size={11} className="animate-spin" />
                          ) : (
                            <Check size={11} />
                          )}
                          保存
                        </button>
                        <button
                          type="button"
                          onClick={handleEditCancel}
                          className="px-3 py-1 bg-neutral-800 hover:bg-neutral-700 text-xs rounded text-neutral-400 flex items-center gap-1"
                        >
                          <X size={11} />
                          キャンセル
                        </button>
                      </div>
                    </>
                  ) : (
                    <>
                      <p className="text-sm text-neutral-100 leading-relaxed whitespace-pre-wrap break-words">
                        {text}
                      </p>
                      <div className="mt-2 flex items-center gap-3 text-xs text-neutral-500 flex-wrap">
                        <span>{formatCreatedAt(item.created_at)}</span>
                        {item.duration_ms > 0 && (
                          <>
                            <span>·</span>
                            <span>録音 {(item.duration_ms / 1000).toFixed(1)}s</span>
                          </>
                        )}
                        {item.transcribe_ms > 0 && (() => {
                          // v0.1.7.1 タスク C: Groq 負荷スパイクの自己判定用に「処理」時間併記。
                          // 警告閾値 (5s 以上 or 比率 1.0 以上) で赤系表示。
                          const slow = isTranscribeSlow(item.duration_ms, item.transcribe_ms)
                          return (
                            <>
                              <span>·</span>
                              <span
                                className={slow ? 'text-red-400' : undefined}
                                title={
                                  slow
                                    ? `Groq 文字起こしが遅延しました (>${TRANSCRIBE_SLOW_ABS_MS / 1000}s または録音長比 >${TRANSCRIBE_SLOW_RATIO})`
                                    : undefined
                                }
                              >
                                処理 {(item.transcribe_ms / 1000).toFixed(1)}s
                              </span>
                            </>
                          )
                        })()}
                        <span>·</span>
                        <span className="font-mono uppercase text-[10px] text-neutral-600">
                          {item.provider}
                        </span>
                        {isEdited && (
                          <>
                            <span>·</span>
                            <span className="text-emerald-500/70">編集済</span>
                          </>
                        )}
                      </div>
                    </>
                  )}
                </div>
                {!isEditing && (
                  <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
                    <button
                      type="button"
                      onClick={() => handleEditStart(item)}
                      className="p-1.5 hover:bg-neutral-800 rounded text-neutral-400 hover:text-white"
                      aria-label="編集"
                    >
                      <Pencil size={14} />
                    </button>
                    <button
                      type="button"
                      onClick={() => handleCopy(item)}
                      className="p-1.5 hover:bg-neutral-800 rounded text-neutral-400 hover:text-white"
                      aria-label="コピー"
                    >
                      <Copy size={14} />
                    </button>
                    <button
                      type="button"
                      onClick={() => handleDelete(item)}
                      className="p-1.5 hover:bg-neutral-800 rounded text-neutral-400 hover:text-red-400"
                      aria-label="削除"
                    >
                      <Trash2 size={14} />
                    </button>
                  </div>
                )}
              </li>
            )
          })}
        </ul>
      )}

      {/* v0.1.7.1 改訂: 件数ベース保持 (50 件) のため「もっと読み込む」ボタンは撤去 */}
    </div>
  )
}
