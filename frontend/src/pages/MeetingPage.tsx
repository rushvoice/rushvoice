/* MeetingPage.tsx — v0.1.16 機能 L (議事録モード) Day 4: 録音 UI
 *
 * SSOT: 内部設計メモ §機能L
 * 委任 prompt: 内部設計メモ §2.2
 *
 * 仕様:
 * - タイトル入力 + 録音開始/停止ボタン + 経過時間 mm:ss + 波形 + エクスポート 3 種
 * - 状態は backend `meeting:state-changed` event で受信 (Idle/Recording/Transcribing/Completed/Error)
 * - 波形は `recorder:waveform` event で RMS 受信 (Overlay.tsx:261 と同パターン)
 * - 議事録 Recording 中はホットキー処理 backend 側で skip → frontend は注意テキストのみ
 * - dest_dir = MVP Windows ハードコード (CEO 事前確定 6・v0.1.17 で改善)
 * - Toast UI は使わず alert で済ませる (MVP・既存パターン無し)
 */

import { useState, useEffect, useRef } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { Mic, Square, Loader2, FileDown, AlertCircle, CheckCircle2 } from 'lucide-react'
import type {
  MeetingState,
  MeetingStateEvent,
  MeetingProgressEvent,
  MeetingRepeatFilteredEvent,
  ExportFormat,
} from '../types/meeting'
import { cn } from '../lib/cn'


/** 波形バーの本数 (Overlay と同等の見た目を保つ) */
const WAVEFORM_BARS = 60

function formatElapsed(ms: number): string {
  const totalSec = Math.floor(ms / 1000)
  const mm = String(Math.floor(totalSec / 60)).padStart(2, '0')
  const ss = String(totalSec % 60).padStart(2, '0')
  return `${mm}:${ss}`
}

export function MeetingPage() {
  const [state, setState] = useState<MeetingState>('idle')
  const [meetingId, setMeetingId] = useState<string | null>(null)
  const [title, setTitle] = useState('')
  const [elapsedMs, setElapsedMs] = useState(0)
  const [chunkProgress, setChunkProgress] = useState<number[]>([])
  const [error, setError] = useState<string | null>(null)
  const [waveform, setWaveform] = useState<number[]>(() =>
    Array.from({ length: WAVEFORM_BARS }, () => 0),
  )
  const [busy, setBusy] = useState(false)
  // v0.1.16 精度向上 ブロック1: エクスポート成功をインライン表示する (alert は Tauri WebView で無反応のため廃止)
  const [exportMsg, setExportMsg] = useState<string | null>(null)
  // Day 41 [[L370]] 解消: 議事録 export dir を起動時に backend から取得 (MeetingHistoryPage.tsx と同パターン)。
  // onExport / 「保存先」表示の両方で backend 値 (`exportDir`) を使う。
  const [exportDir, setExportDir] = useState<string | null>(null)
  // 保存先が未取得のときは backend に問い合わせ直し、取れた値を state にも残す
  const refetchExportDir = async (): Promise<string> => {
    const dir = await invoke<string>('meeting_get_export_dir')
    setExportDir(dir)
    return dir
  }
  // Day 39 リピート除去フィルタ: 累積発火件数 (chunk と dropped の合計)
  //   backend filter_repeat_segments(N=5) で 5 連続以上の同一テキスト圧縮時のみ emit。
  //   通常録音では発火しない (無音区間で Whisper ハルシ連発時のみ)。同セッション内で複数 chunk
  //   から emit された場合は合計値を保持し、idle 戻り (onReset) で null に戻す。
  const [repeatFilterStat, setRepeatFilterStat] = useState<{
    firedChunks: number
    totalDropped: number
  } | null>(null)
  const startRef = useRef<number | null>(null)
  // v0.1.16 Day 37 修正: 現行セッション id を ref で保持 (listen クロージャから最新値を参照)。
  // 古いセッションの state-changed emit が新セッションの状態を上書きするのを防ぐ照合用。
  const meetingIdRef = useRef<string | null>(null)

  // event 購読
  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen<MeetingStateEvent>('meeting:state-changed', (e) => {
            if (!mounted) return
            const evId = e.payload.id ?? null
            // v0.1.16 Day 37 修正: 古いセッションのイベントを無視する。
            // 1 回目の遅延した completed/error emit が 2 回目の recording 状態を上書きすると
            // 停止ボタン (state==='recording' で動作) が効かなくなるため、現行 id と不一致の
            // イベントは破棄する (meetingIdRef が未設定の開始直後は受け入れる)。
            if (evId && meetingIdRef.current && evId !== meetingIdRef.current) return
            setState(e.payload.state)
            if (evId) {
              setMeetingId(evId)
              meetingIdRef.current = evId
            }
            if (e.payload.state === 'idle' || e.payload.state === 'error') {
              startRef.current = null
            }
            // backend が error を emit したのに error メッセージが未設定なら
            // フォールバック文言を入れる (error UI のリセット導線を必ず出すため・Codex P2)
            if (e.payload.state === 'error') {
              setError((prev) => prev ?? '録音は停止しましたが転写を開始できませんでした。もう一度お試しください。')
            }
          }),
          await listen<MeetingProgressEvent>('meeting:progress', (e) => {
            if (!mounted) return
            if (e.payload.phase === 'parsed') {
              setChunkProgress((p) =>
                p.includes(e.payload.chunk_index) ? p : [...p, e.payload.chunk_index],
              )
            }
          }),
          // Overlay.tsx:261 と同パターン (議事録録音中も recorder:waveform は議事録 worker から emit される想定)
          await listen<number>('recorder:waveform', (e) => {
            if (!mounted) return
            setWaveform((prev) => {
              const next = prev.slice(1)
              next.push(e.payload)
              return next
            })
          }),
          // Day 39 リピート除去フィルタ event: dropped>0 の chunk のみ emit される。
          // 同セッション内で複数 chunk から emit された場合は累積 (上書きせず加算) する。
          await listen<MeetingRepeatFilteredEvent>('meeting:repeat-filtered', (e) => {
            if (!mounted) return
            setRepeatFilterStat((prev) => {
              if (!prev) {
                return { firedChunks: 1, totalDropped: e.payload.dropped }
              }
              return {
                firedChunks: prev.firedChunks + 1,
                totalDropped: prev.totalDropped + e.payload.dropped,
              }
            })
          }),
        )
      } catch (e) {
        console.warn('[meeting] event listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  // Day 41 [[L370]] 解消: 議事録 export dir を起動時に backend から取得する。
  // MeetingHistoryPage.tsx と同パターン。取得失敗時は exportDir=null のまま (フォールバック動作)。
  useEffect(() => {
    let mounted = true
    ;(async () => {
      try {
        const dir = await invoke<string>('meeting_get_export_dir')
        if (mounted) setExportDir(dir)
      } catch (e) {
        console.warn('[meeting] meeting_get_export_dir failed:', e)
      }
    })()
    return () => {
      mounted = false
    }
  }, [])

  // 経過時間タイマー (Recording 中のみ)
  useEffect(() => {
    if (state !== 'recording') {
      setElapsedMs(0)
      return
    }
    startRef.current = Date.now()
    const timer = setInterval(() => {
      if (startRef.current) setElapsedMs(Date.now() - startRef.current)
    }, 500)
    return () => clearInterval(timer)
  }, [state])

  const onStart = async () => {
    if (busy) return
    setBusy(true)
    setError(null)
    setChunkProgress([])
    try {
      const id = await invoke<string>('meeting_start', {
        title: title.trim() ? title.trim() : null,
      })
      // v0.1.16 Day 37 修正: ref を先に更新 (古いセッションの emit 照合用) し、
      // 楽観的に recording へ遷移する。backend の recording emit を待たずに状態を確定させ、
      // 2 回目以降 (reset 後) も停止ボタンが確実に効くようにする。
      meetingIdRef.current = id
      setMeetingId(id)
      setState('recording')
    } catch (e) {
      const msg = String(e)
      setError(msg)
      setState('error')
      console.warn('[meeting] meeting_start failed:', e)
    } finally {
      setBusy(false)
    }
  }

  const onStop = async () => {
    // v0.1.16 Day 37 修正1: 停止ボタンが効かないバグ対策。
    // 旧実装は `if (!meetingId || busy) return` で meetingId 取りこぼし / busy 中の押下を
    // 無視していたため、録音が止まらず発話が議事録に残り続けた。
    // recording 中なら meetingId / busy に関わらず必ず停止を実行する。
    // backend は holder ベース停止 (id 不問) なので id 欠落でも録音は確実に止まる。
    console.log(
      '[meeting] onStop fired meetingId=',
      meetingId,
      'busy=',
      busy,
      'state=',
      state,
    )
    if (state !== 'recording') return
    // 押下直後に楽観的 UI 更新 (即フィードバック)。確定状態は backend emit で上書きされる。
    setState('transcribing')
    setBusy(true)
    try {
      await invoke('meeting_stop', { id: meetingId ?? '' })
      // state は backend emit (transcribing → completed/error) で更新される
    } catch (e) {
      const msg = String(e)
      setError(msg)
      setState('error')
      console.warn('[meeting] meeting_stop failed:', e)
    } finally {
      setBusy(false)
    }
  }

  const onExport = async (format: ExportFormat) => {
    if (!meetingId) return
    setBusy(true)
    setExportMsg(null)
    try {
      // Day 41 [[L370]]: backend 単一情報源化。exportDir (backend 取得値) を優先採用し、
      // 未取得のときはその場で backend に問い合わせる (端末固有のパスを持たない)。
      const path = await invoke<string>('meeting_export', {
        id: meetingId,
        format,
        destDir: exportDir ?? (await refetchExportDir()),
      })
      // v0.1.16 精度向上 ブロック1: alert は Tauri WebView で無反応のため画面内インライン表示に置換
      setExportMsg(`保存しました: ${path}`)
    } catch (e) {
      const msg = String(e)
      setError(msg)
      setExportMsg(null)
      console.warn('[meeting] meeting_export failed:', e)
    } finally {
      setBusy(false)
    }
  }

  const onReset = () => {
    setError(null)
    setExportMsg(null)
    setMeetingId(null)
    meetingIdRef.current = null
    setState('idle')
    setChunkProgress([])
    setElapsedMs(0)
    setRepeatFilterStat(null)
  }

  // === 状態別 UI ===
  const Icon = () => {
    if (state === 'recording')
      return <Square size={48} className="text-white" fill="currentColor" />
    if (state === 'transcribing')
      return <Loader2 size={48} className="text-white animate-spin" />
    if (state === 'completed') return <CheckCircle2 size={48} className="text-white" />
    if (state === 'error') return <AlertCircle size={48} className="text-white" />
    return <Mic size={48} className="text-white" />
  }

  const buttonColor =
    state === 'recording'
      ? 'bg-red-500 shadow-red-700/30'
      : state === 'transcribing'
        ? 'bg-amber-500 shadow-amber-700/30'
        : state === 'completed'
          ? 'bg-emerald-600 shadow-emerald-800/30'
          : state === 'error'
            ? 'bg-rose-600 shadow-rose-800/30'
            : 'bg-[var(--color-brand-500)] shadow-[var(--color-brand-700)]/30'

  const handleMainClick = () => {
    if (state === 'idle') onStart()
    else if (state === 'recording') onStop()
    else if (state === 'completed' || state === 'error') onReset()
  }

  const mainDisabled = state === 'transcribing' || busy

  return (
    <div className="flex-1 flex flex-col items-center bg-neutral-900 text-white relative overflow-y-auto">
      <div className="w-full max-w-2xl px-8 py-10 flex flex-col items-center">
        <h2 className="text-xl font-semibold text-neutral-100 mb-1">議事録モード</h2>
        <p className="text-xs text-neutral-500 mb-6">
          長時間録音 → Whisper 一括転写 (5 分チャンク・最大 60 分)
        </p>

        {/* タイトル入力 */}
        <div className="w-full mb-6">
          <label htmlFor="meeting-title" className="block text-sm text-neutral-400 mb-1">
            タイトル (任意)
          </label>
          <input
            id="meeting-title"
            type="text"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            disabled={state !== 'idle'}
            placeholder="例: 5月31日定例会議"
            maxLength={200}
            className="w-full bg-neutral-800 border border-neutral-700 rounded px-3 py-2 text-sm text-neutral-200 focus:outline-none focus:border-neutral-500 disabled:opacity-50 disabled:cursor-not-allowed"
          />
        </div>

        {/* メイン録音ボタン
            v0.1.16 Day 38 真因対策: 録音中の波形 emit (60 bars × 高頻度 setState) で
            button 周辺が連続再 render され、React の onClick handler が reconciliation
            中に drop されていた事案 (開発機の実機検証で 10+回押して 1 回しか fire しない)。
            対策: onPointerDown (mouse down 時に即 fire・click より早い・React の click
            イベント drop 問題を回避) を追加し、onClick は冗長保険として残す。 */}
        <button
          type="button"
          onClick={handleMainClick}
          onPointerDown={(e) => {
            // 左ボタンのみ反応 (右クリック/middle click を除外)
            if (e.button !== 0) return
            // recording 中の停止のみ pointerdown 経路で発火させる (開始時の二重発火を防ぐ)
            if (state === 'recording' && !mainDisabled) {
              handleMainClick()
            }
          }}
          disabled={mainDisabled}
          className={cn(
            'w-32 h-32 rounded-full transition-colors flex items-center justify-center shadow-2xl focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-white/30 disabled:opacity-70 disabled:cursor-not-allowed',
            buttonColor,
          )}
          aria-label={
            state === 'recording'
              ? '録音停止'
              : state === 'idle'
                ? '議事録録音開始'
                : state === 'completed' || state === 'error'
                  ? 'リセット'
                  : '転写中'
          }
        >
          <Icon />
        </button>

        {/* 経過時間 (Recording 中) */}
        {state === 'recording' && (
          <div className="mt-4 text-3xl font-mono text-red-400 tabular-nums">
            {formatElapsed(elapsedMs)}
          </div>
        )}

        {/* 波形 (Recording 中のみ表示) */}
        {state === 'recording' && (
          <div className="mt-4 w-full h-16 flex items-end justify-center gap-[2px] px-4">
            {waveform.map((rms, i) => {
              const h = Math.min(100, Math.max(2, rms * 800))
              return (
                <div
                  key={i}
                  className="w-1 bg-red-400/80 rounded-t"
                  style={{ height: `${h}%` }}
                />
              )
            })}
          </div>
        )}

        {/* Transcribing 中: チャンク進捗 */}
        {state === 'transcribing' && (
          <div className="mt-6 w-full">
            <p className="text-sm text-amber-400 text-center mb-2">
              文字起こし中… ({chunkProgress.length} チャンク完了)
            </p>
            <div className="h-2 bg-neutral-800 rounded overflow-hidden">
              <div
                className="h-full bg-amber-500 transition-all"
                style={{
                  width: `${Math.min(100, chunkProgress.length * 15)}%`,
                }}
              />
            </div>
          </div>
        )}

        {/* Day 39 リピート除去フィルタ警告: transcribing/completed/error 中のみ表示
            (idle/recording 中は非表示・onReset で消去)。通常録音 UX への影響ゼロ。 */}
        {repeatFilterStat && state !== 'idle' && state !== 'recording' && (
          <div className="mt-4 w-full">
            <div className="rounded border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-amber-200 text-xs">
              <p className="flex items-start gap-2">
                <span aria-hidden>⚠️</span>
                <span>
                  リピート検出: 合計 {repeatFilterStat.totalDropped} 回連続同一テキストを圧縮しました
                  (発火 chunk: {repeatFilterStat.firedChunks} 件)
                </span>
              </p>
              <p className="mt-1 text-[11px] text-amber-300/70">
                無音区間で Whisper が同じハルシ句を繰り返し出力した可能性があります。マイク音量が極端に低い場合に発生しやすいです。
              </p>
            </div>
          </div>
        )}

        {/* Completed: エクスポート 3 ボタン */}
        {state === 'completed' && (
          <div className="mt-6 w-full">
            <p className="text-sm text-emerald-400 text-center mb-3">
              文字起こし完了。形式を選んでエクスポートしてください。
            </p>
            <div className="flex justify-center gap-3">
              {(['txt', 'md', 'srt'] as ExportFormat[]).map((f) => (
                <button
                  key={f}
                  type="button"
                  onClick={() => onExport(f)}
                  disabled={busy}
                  className="flex items-center gap-2 bg-neutral-800 hover:bg-neutral-700 border border-neutral-700 rounded px-4 py-2 text-sm text-neutral-200 disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  <FileDown size={16} />
                  <span>{f.toUpperCase()}</span>
                </button>
              ))}
            </div>
            {/* v0.1.16 精度向上 ブロック1: SRT 用語補足 */}
            <p className="text-[11px] text-neutral-500 text-center mt-2">
              TXT / MD = 議事録本文・SRT = 字幕用（動画字幕ファイル）
            </p>
            <p className="text-xs text-neutral-500 text-center mt-1">
              保存先: {exportDir ?? '確認中…'}
            </p>

            {/* v0.1.16 精度向上 ブロック1: エクスポート成功インライン表示 (alert 廃止) */}
            {exportMsg && (
              <p className="mt-3 text-sm text-emerald-400 text-center break-all">
                {exportMsg}
              </p>
            )}

            {/* v0.1.16 精度向上 ブロック1: 明示的なリセットボタン (緑チェックの二重導線) */}
            <div className="flex justify-center mt-5">
              <button
                type="button"
                onClick={onReset}
                disabled={busy}
                className="bg-neutral-800 hover:bg-neutral-700 border border-neutral-700 rounded px-4 py-2 text-sm text-neutral-200 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                新しい議事録を録音
              </button>
            </div>
          </div>
        )}

        {/* Error: メッセージ + リセット */}
        {state === 'error' && error && (
          <div className="mt-6 w-full">
            <p className="text-sm text-rose-400 text-center mb-3 break-all">{error}</p>
            <div className="flex justify-center">
              <button
                type="button"
                onClick={onReset}
                className="bg-neutral-800 hover:bg-neutral-700 border border-neutral-700 rounded px-4 py-2 text-sm text-neutral-200"
              >
                もう一度
              </button>
            </div>
          </div>
        )}

        {/* 注意書き: 議事録 Recording 中はホットキー無効 */}
        {state === 'recording' && (
          <p className="mt-6 text-xs text-amber-500/70 text-center">
            ※ 議事録録音中は通常録音のホットキーが無効になります
          </p>
        )}

        {state === 'idle' && (
          <p className="mt-6 text-xs text-neutral-500 text-center">
            録音は 5 分ごとにディスクへ書き出されます (最大 60 分・RAM 常駐なし)
          </p>
        )}
      </div>
    </div>
  )
}
