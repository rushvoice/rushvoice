// SeminarSubtitlePage.tsx — セミナー字幕モード コントロールページ
//
// 仕様 (2026-07-04 合意):
//   - 出力言語 (現地語) は講習開始時に固定 → 実行中は変更不可
//   - 日本語段は常時表示・英語 3 段目は ON/OFF
//   - 字幕ウィンドウは別 Tauri ウィンドウ (外部モニターがあれば自動全画面)
//   - 状態表示: 監視中/待機中/エラー + 検出言語 + 処理遅延 + 直近字幕プレビュー
//   - フォント倍率は `subtitle:config` event で字幕ウィンドウへ送る (frontend 間 broadcast)

import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, emit, type UnlistenFn } from '@tauri-apps/api/event'
import { Languages, MonitorUp, MonitorX, Play, Square, Loader2, AlertCircle } from 'lucide-react'
import {
  TARGET_LANG_OPTIONS,
  detectedLangLabel,
  type SubtitleLineEvent,
  type SubtitleStateEvent,
  type SubtitleRunConfig,
} from '../types/subtitle'
import { cn } from '../lib/cn'

export function SeminarSubtitlePage() {
  const [running, setRunning] = useState(false)
  const [targetLang, setTargetLang] = useState('zh-TW')
  const [showEnglish, setShowEnglish] = useState(false)
  const [fontScale, setFontScale] = useState(1.0)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [lastLine, setLastLine] = useState<SubtitleLineEvent | null>(null)

  // 再マウント時 (タブ切替) の状態復元
  useEffect(() => {
    let mounted = true
    ;(async () => {
      try {
        const cfg = await invoke<SubtitleRunConfig | null>('subtitle_get_state')
        if (!mounted) return
        if (cfg) {
          setRunning(true)
          setTargetLang(cfg.target_lang)
          setShowEnglish(cfg.show_english)
        }
      } catch (e) {
        console.warn('[subtitle-page] state restore failed:', e)
      }
    })()
    return () => {
      mounted = false
    }
  }, [])

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen<SubtitleStateEvent>('subtitle:state', (e) => {
            if (!mounted) return
            if (e.payload.state === 'listening') {
              setRunning(true)
              setError(null)
            } else if (e.payload.state === 'idle') {
              setRunning(false)
            } else if (e.payload.state === 'error') {
              setError(e.payload.message ?? 'エラーが発生しました')
            }
          }),
        )
        unlistens.push(
          await listen<SubtitleLineEvent>('subtitle:line', (e) => {
            if (!mounted) return
            setLastLine(e.payload)
          }),
        )
      } catch (e) {
        console.warn('[subtitle-page] listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  const onStart = async () => {
    setBusy(true)
    setError(null)
    try {
      await invoke('subtitle_start', { targetLang, showEnglish })
      setRunning(true)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const onStop = async () => {
    setBusy(true)
    try {
      await invoke('subtitle_stop')
      // running=false は pipeline の idle event で確定させる (残発話処理中も表示継続)
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  const onOpenWindow = async () => {
    try {
      await invoke('subtitle_open_window')
      // 開いた直後に現在のフォント倍率を同期
      await emit('subtitle:config', { fontScale })
    } catch (e) {
      setError(String(e))
    }
  }

  const onCloseWindow = async () => {
    try {
      await invoke('subtitle_close_window')
    } catch (e) {
      setError(String(e))
    }
  }

  const onFontScale = async (v: number) => {
    setFontScale(v)
    try {
      await emit('subtitle:config', { fontScale: v })
    } catch (e) {
      console.warn('[subtitle-page] config emit failed:', e)
    }
  }

  return (
    <main className="flex-1 overflow-y-auto p-8">
      <div className="max-w-2xl mx-auto space-y-6">
        <div className="flex items-center gap-3">
          <Languages size={22} className="text-[var(--color-brand-500)]" />
          <h2 className="text-xl font-semibold">セミナー字幕</h2>
          {running && (
            <span className="text-xs bg-green-900/60 text-green-200 rounded px-2 py-1">
              ● 監視中
            </span>
          )}
        </div>
        <p className="text-sm text-neutral-400">
          海外講習向け: 話した内容を現地語＋日本語の字幕として外部モニターに表示します。
          入力言語は自動判定（誰が何語で話してもOK・受講者の質問は講師が復唱してください）。
        </p>

        {/* 出力言語 (講習開始時に固定) */}
        <section className="bg-neutral-950 border border-neutral-800 rounded-lg p-5 space-y-4">
          <h3 className="text-sm font-medium text-neutral-300">
            出力言語（開始後は変更できません）
          </h3>
          <div className="grid grid-cols-2 gap-2">
            {TARGET_LANG_OPTIONS.map((opt) => (
              <button
                key={opt.code}
                type="button"
                disabled={running || busy}
                onClick={() => setTargetLang(opt.code)}
                className={cn(
                  'text-sm rounded border px-3 py-2 text-left transition-colors',
                  targetLang === opt.code
                    ? 'border-[var(--color-brand-500)] bg-[var(--color-brand-700)]/20 text-white'
                    : 'border-neutral-800 text-neutral-400 hover:text-white hover:border-neutral-600',
                  (running || busy) && 'opacity-60 cursor-not-allowed',
                )}
              >
                {opt.label}
              </button>
            ))}
          </div>
          <label
            className={cn(
              'flex items-center gap-2 text-sm text-neutral-300',
              (running || busy || targetLang === 'en') && 'opacity-60',
            )}
          >
            <input
              type="checkbox"
              checked={showEnglish && targetLang !== 'en'}
              disabled={running || busy || targetLang === 'en'}
              onChange={(e) => setShowEnglish(e.target.checked)}
              className="accent-[var(--color-brand-500)]"
            />
            英語の 3 段目を表示する
            {targetLang === 'en' && (
              <span className="text-xs text-neutral-500">（出力言語が英語のため不要）</span>
            )}
          </label>
        </section>

        {/* 字幕ウィンドウ + 開始/停止 */}
        <section className="bg-neutral-950 border border-neutral-800 rounded-lg p-5 space-y-4">
          <div className="flex flex-wrap gap-3">
            <button
              type="button"
              onClick={onOpenWindow}
              className="flex items-center gap-2 text-sm rounded bg-neutral-800 hover:bg-neutral-700 px-4 py-2.5 transition-colors"
            >
              <MonitorUp size={16} />
              字幕ウィンドウを開く
            </button>
            <button
              type="button"
              onClick={onCloseWindow}
              className="flex items-center gap-2 text-sm rounded bg-neutral-800 hover:bg-neutral-700 px-4 py-2.5 transition-colors"
            >
              <MonitorX size={16} />
              閉じる
            </button>
            {!running ? (
              <button
                type="button"
                disabled={busy}
                onClick={onStart}
                className="flex items-center gap-2 text-sm rounded bg-green-700 hover:bg-green-600 px-5 py-2.5 font-medium transition-colors disabled:opacity-60"
              >
                {busy ? <Loader2 size={16} className="animate-spin" /> : <Play size={16} />}
                字幕を開始
              </button>
            ) : (
              <button
                type="button"
                disabled={busy}
                onClick={onStop}
                className="flex items-center gap-2 text-sm rounded bg-red-700 hover:bg-red-600 px-5 py-2.5 font-medium transition-colors disabled:opacity-60"
              >
                {busy ? <Loader2 size={16} className="animate-spin" /> : <Square size={16} />}
                停止
              </button>
            )}
          </div>
          <p className="text-xs text-neutral-500">
            外部モニター（HDMI）接続時は字幕ウィンドウが自動で外部側に全画面表示されます。
            字幕実行中はホットキーによる通常録音は無効になります。
          </p>

          {/* フォント倍率 */}
          <div className="flex items-center gap-3 text-sm text-neutral-300">
            <span className="whitespace-nowrap">文字サイズ</span>
            <input
              type="range"
              min={0.6}
              max={1.8}
              step={0.1}
              value={fontScale}
              onChange={(e) => onFontScale(Number(e.target.value))}
              className="flex-1 accent-[var(--color-brand-500)]"
            />
            <span className="w-10 text-right tabular-nums">{Math.round(fontScale * 100)}%</span>
          </div>
        </section>

        {/* 状態表示 */}
        {error && (
          <div className="flex items-start gap-2 bg-red-950/60 border border-red-900 text-red-200 text-sm rounded-lg p-4">
            <AlertCircle size={16} className="mt-0.5 shrink-0" />
            <span>{error}</span>
          </div>
        )}
        {lastLine && (
          <section className="bg-neutral-950 border border-neutral-800 rounded-lg p-5 space-y-2">
            <div className="flex items-center gap-4 text-xs text-neutral-500">
              <span>直近の字幕 #{lastLine.seq + 1}</span>
              <span>検出言語: {detectedLangLabel(lastLine.detected_lang)}</span>
              <span>処理遅延: {(lastLine.latency_ms / 1000).toFixed(1)} 秒</span>
            </div>
            {lastLine.local && <p className="text-base text-white">{lastLine.local}</p>}
            {lastLine.ja && <p className="text-sm text-neutral-300">{lastLine.ja}</p>}
            {lastLine.en && <p className="text-sm text-neutral-500">{lastLine.en}</p>}
          </section>
        )}
      </div>
    </main>
  )
}
