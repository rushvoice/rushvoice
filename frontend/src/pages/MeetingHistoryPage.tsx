/* MeetingHistoryPage.tsx — v0.1.16 機能 L (議事録モード) Day 4: 履歴 UI
 *
 * SSOT: 内部設計メモ §機能L
 * 委任 prompt: 内部設計メモ §2.3
 *
 * 仕様:
 * - 1 列リスト + クリックでインライン展開 (CEO 推奨方針 2)
 * - 詳細: segments_json → MeetingSegment[] → 800ms ギャップで段落化 (frontend 側で素朴実装)
 * - 削除確認ダイアログ + 再エクスポート 3 種
 * - PAGE_SIZE = 50 (HistoryPage.tsx と同じ・追加読込なし)
 */

import { useCallback, useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { Clock, FileDown, Trash2, ChevronRight, ChevronDown, Loader2, FolderOpen, FileText } from 'lucide-react'
import { openPath } from '@tauri-apps/plugin-opener'
import type {
  MeetingLogDto,
  MeetingSegment,
  MeetingStateEvent,
  ExportFormat,
} from '../types/meeting'
import { cn } from '../lib/cn'

const PAGE_SIZE = 50
/** 段落化のギャップ閾値 (ms)。SSOT §機能L 段落化 = 0.8 秒。 */
const PARAGRAPH_GAP_MS = 800

const DATE_FMT = new Intl.DateTimeFormat('ja-JP', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
})

function formatStartAt(epochSecs: number): string {
  const d = new Date(epochSecs * 1000)
  if (isNaN(d.getTime())) return String(epochSecs)
  return DATE_FMT.format(d)
}

function formatDuration(ms: number): string {
  if (ms <= 0) return '—'
  const totalSec = Math.floor(ms / 1000)
  const mm = Math.floor(totalSec / 60)
  const ss = totalSec % 60
  if (mm === 0) return `${ss}秒`
  return `${mm}分${ss}秒`
}

function formatSegmentTime(ms: number): string {
  const totalSec = Math.floor(ms / 1000)
  const mm = String(Math.floor(totalSec / 60)).padStart(2, '0')
  const ss = String(totalSec % 60).padStart(2, '0')
  return `${mm}:${ss}`
}

/** segments を 800ms ギャップで段落化 (履歴表示用・素朴実装・backend split_into_paragraphs と挙動同等) */
function paragraphize(segs: MeetingSegment[]): string[] {
  if (segs.length === 0) return []
  const paragraphs: string[] = []
  let current = ''
  let prevEnd: number | null = null
  for (const s of segs) {
    if (prevEnd !== null && s.start_ms - prevEnd >= PARAGRAPH_GAP_MS) {
      if (current.trim()) paragraphs.push(current.trim())
      current = ''
    }
    current += s.text
    prevEnd = s.end_ms
  }
  if (current.trim()) paragraphs.push(current.trim())
  return paragraphs
}

export function MeetingHistoryPage() {
  const [items, setItems] = useState<MeetingLogDto[]>([])
  const [loading, setLoading] = useState(true)
  const [expandedId, setExpandedId] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null) // 操作中の id
  // Day 40: 議事録 export dir (フォルダボタン用) を起動時に backend から取得
  const [exportDir, setExportDir] = useState<string | null>(null)
  // 保存先が未取得のときは backend に問い合わせ直し、取れた値を state にも残す
  const refetchExportDir = async (): Promise<string> => {
    const dir = await invoke<string>('meeting_get_export_dir')
    setExportDir(dir)
    return dir
  }

  const refresh = useCallback(async () => {
    setLoading(true)
    try {
      const rows = await invoke<MeetingLogDto[]>('meeting_list', {
        limit: PAGE_SIZE,
        offset: 0,
      })
      setItems(rows)
    } catch (e) {
      console.warn('[meeting-history] meeting_list failed:', e)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  // Day 40: 議事録 export dir を起動時に backend から取得 (フォルダボタン用)
  useEffect(() => {
    let mounted = true
    ;(async () => {
      try {
        const dir = await invoke<string>('meeting_get_export_dir')
        if (mounted) setExportDir(dir)
      } catch (e) {
        console.warn('[meeting-history] meeting_get_export_dir failed:', e)
      }
    })()
    return () => {
      mounted = false
    }
  }, [])

  // 議事録の状態変化 (completed/error 時) で一覧を再取得 = 進行中録音が終わった瞬間に segments が見える
  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen<MeetingStateEvent>('meeting:state-changed', (e) => {
            if (!mounted) return
            if (e.payload.state === 'completed' || e.payload.state === 'error') {
              refresh()
            }
          }),
        )
      } catch (e) {
        console.warn('[meeting-history] event listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [refresh])

  const onToggleExpand = (id: string) => {
    setExpandedId((cur) => (cur === id ? null : id))
  }

  const onDelete = async (id: string, title: string) => {
    const label = title.trim() || '(無題)'
    if (!window.confirm(`「${label}」を削除します。元に戻せません。よろしいですか？`)) return
    setBusy(id)
    try {
      await invoke<boolean>('meeting_delete', { id })
      // 楽観的 UI 更新
      setItems((prev) => prev.filter((r) => r.id !== id))
      if (expandedId === id) setExpandedId(null)
    } catch (e) {
      console.warn('[meeting-history] meeting_delete failed:', e)
      alert(`削除失敗: ${e}`)
    } finally {
      setBusy(null)
    }
  }

  const onExport = async (id: string, format: ExportFormat) => {
    setBusy(id)
    try {
      // Day 41 [[L370]]: backend 単一情報源化。exportDir (backend 取得値) を優先採用し、
      // 未取得のときはその場で backend に問い合わせる (端末固有のパスを持たない)。
      const path = await invoke<string>('meeting_export', {
        id,
        format,
        destDir: exportDir ?? (await refetchExportDir()),
      })
      alert(`保存しました: ${path}`)
    } catch (e) {
      console.warn('[meeting-history] meeting_export failed:', e)
      alert(`エクスポート失敗: ${e}`)
    } finally {
      setBusy(null)
    }
  }

  /** Day 40: 議事録 md を OS 既定アプリで開く。未 export なら案内 alert。 */
  const onOpenMd = async (id: string) => {
    setBusy(id)
    try {
      const path = await invoke<string>('meeting_get_export_md_path', { id })
      await openPath(path)
    } catch (e) {
      const msg = String(e)
      if (msg.includes('md export がまだありません') || msg.includes('まだありません')) {
        alert('先に「MD」ボタンでエクスポートしてください。')
      } else {
        console.warn('[meeting-history] meeting_get_export_md_path / openPath failed:', e)
        alert(`md を開けませんでした: ${msg}`)
      }
    } finally {
      setBusy(null)
    }
  }

  /** Day 40: 議事録保存フォルダを OS 既定 explorer/Finder で開く。 */
  const onOpenFolder = async () => {
    if (!exportDir) {
      // 初回取得が遅延中の場合は再取得を試みる
      try {
        const dir = await invoke<string>('meeting_get_export_dir')
        setExportDir(dir)
        await openPath(dir)
      } catch (e) {
        console.warn('[meeting-history] meeting_get_export_dir failed:', e)
        alert(`フォルダを開けませんでした: ${e}`)
      }
      return
    }
    try {
      await openPath(exportDir)
    } catch (e) {
      console.warn('[meeting-history] openPath dir failed:', e)
      alert(`フォルダを開けませんでした: ${e}`)
    }
  }

  if (loading) {
    return (
      <div className="flex-1 flex items-center justify-center bg-neutral-900 text-neutral-400">
        <Loader2 size={20} className="animate-spin mr-2" />
        <span className="text-sm">読み込み中…</span>
      </div>
    )
  }

  return (
    <div className="flex-1 flex flex-col bg-neutral-900 text-white overflow-y-auto">
      <div className="w-full max-w-3xl mx-auto px-6 py-6">
        <h2 className="text-xl font-semibold text-neutral-100 mb-1">議事録履歴</h2>
        <div className="flex items-center justify-between mb-4">
          <p className="text-xs text-neutral-500">
            直近 {PAGE_SIZE} 件 (作成日時の新しい順)
          </p>
          <button
            type="button"
            onClick={onOpenFolder}
            className="flex items-center gap-1.5 bg-neutral-900 hover:bg-neutral-800 border border-neutral-700 rounded px-3 py-1.5 text-xs text-neutral-300"
            title={exportDir ?? '議事録保存フォルダ'}
          >
            <FolderOpen size={12} />
            <span>保存フォルダを開く</span>
          </button>
        </div>

        {items.length === 0 ? (
          <div className="border border-neutral-800 rounded-lg p-8 text-center text-neutral-500 text-sm">
            まだ議事録はありません
          </div>
        ) : (
          <ul className="space-y-2">
            {items.map((item) => {
              const expanded = expandedId === item.id
              const title = item.title.trim() || '(無題)'
              const empty = item.segments_json === '[]' || item.segments_json === ''
              let paragraphs: string[] = []
              let segments: MeetingSegment[] = []
              if (expanded && !empty) {
                try {
                  segments = JSON.parse(item.segments_json) as MeetingSegment[]
                  paragraphs = paragraphize(segments)
                } catch (e) {
                  console.warn('[meeting-history] segments parse failed:', e)
                }
              }
              const operating = busy === item.id
              return (
                <li
                  key={item.id}
                  className={cn(
                    'border border-neutral-800 rounded-lg bg-neutral-950 transition-colors',
                    expanded && 'border-neutral-700',
                  )}
                >
                  <button
                    type="button"
                    onClick={() => onToggleExpand(item.id)}
                    className="w-full flex items-center gap-3 px-4 py-3 text-left hover:bg-neutral-900/50"
                  >
                    {expanded ? (
                      <ChevronDown size={16} className="text-neutral-500 shrink-0" />
                    ) : (
                      <ChevronRight size={16} className="text-neutral-500 shrink-0" />
                    )}
                    <div className="flex-1 min-w-0">
                      <p className="text-sm text-neutral-200 truncate">{title}</p>
                      <p className="text-xs text-neutral-500 mt-0.5 flex items-center gap-2">
                        <Clock size={11} />
                        <span>{formatStartAt(item.start_at)}</span>
                        <span>·</span>
                        <span>{formatDuration(item.duration_ms)}</span>
                        {empty && (
                          <>
                            <span>·</span>
                            <span className="text-amber-500">未転写</span>
                          </>
                        )}
                      </p>
                    </div>
                  </button>

                  {expanded && (
                    <div className="border-t border-neutral-800 px-4 py-4 space-y-4">
                      {/* 詳細メタ */}
                      <div className="text-xs text-neutral-500 space-y-0.5">
                        <p>ID: <span className="font-mono text-neutral-600">{item.id}</span></p>
                        <p>セグメント数: {segments.length}</p>
                      </div>

                      {/* 段落表示 */}
                      {empty ? (
                        <p className="text-sm text-neutral-500 italic">
                          転写未完了 (segments が空です)
                        </p>
                      ) : paragraphs.length === 0 ? (
                        <p className="text-sm text-neutral-500 italic">
                          表示できる段落がありません
                        </p>
                      ) : (
                        <div className="space-y-3">
                          {paragraphs.map((p, i) => {
                            // 最初の segment 時刻を段落先頭に併記
                            const startMs = segments[0]?.start_ms ?? 0
                            // 段落 i 番目の先頭時刻を雑に推定: 段落区切り計算と整合させるため再走査するのは過剰
                            // → MVP は最初の段落のみ時刻表示で割愛 (Day 5 で改善検討)
                            return (
                              <p
                                key={i}
                                className="text-sm text-neutral-200 leading-relaxed whitespace-pre-wrap"
                              >
                                {i === 0 && (
                                  <span className="text-xs font-mono text-neutral-500 mr-2">
                                    [{formatSegmentTime(startMs)}]
                                  </span>
                                )}
                                {p}
                              </p>
                            )
                          })}
                        </div>
                      )}

                      {/* 操作ボタン */}
                      <div className="flex flex-wrap gap-2 pt-2 border-t border-neutral-800/50">
                        {!empty &&
                          (['txt', 'md', 'srt'] as ExportFormat[]).map((f) => (
                            <button
                              key={f}
                              type="button"
                              onClick={() => onExport(item.id, f)}
                              disabled={operating}
                              className="flex items-center gap-1.5 bg-neutral-900 hover:bg-neutral-800 border border-neutral-700 rounded px-3 py-1.5 text-xs text-neutral-200 disabled:opacity-50 disabled:cursor-not-allowed"
                            >
                              <FileDown size={12} />
                              <span>{f.toUpperCase()}</span>
                            </button>
                          ))}
                        {!empty && (
                          <button
                            type="button"
                            onClick={() => onOpenMd(item.id)}
                            disabled={operating}
                            className="flex items-center gap-1.5 bg-emerald-950/40 hover:bg-emerald-900/40 border border-emerald-900/50 rounded px-3 py-1.5 text-xs text-emerald-300 disabled:opacity-50 disabled:cursor-not-allowed"
                            title="保存済みの md ファイルを既定アプリで開く"
                          >
                            <FileText size={12} />
                            <span>md を開く</span>
                          </button>
                        )}
                        <button
                          type="button"
                          onClick={() => onDelete(item.id, item.title)}
                          disabled={operating}
                          className="flex items-center gap-1.5 bg-rose-950/40 hover:bg-rose-900/40 border border-rose-900/50 rounded px-3 py-1.5 text-xs text-rose-300 disabled:opacity-50 disabled:cursor-not-allowed ml-auto"
                        >
                          <Trash2 size={12} />
                          <span>削除</span>
                        </button>
                      </div>
                    </div>
                  )}
                </li>
              )
            })}
          </ul>
        )}
      </div>
    </div>
  )
}
