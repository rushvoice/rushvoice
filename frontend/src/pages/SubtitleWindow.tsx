// SubtitleWindow.tsx — セミナー字幕表示ウィンドウ (#/subtitle・外部モニター全画面用)
//
// 仕様 (2026-07-04 合意):
//   - 黒背景・現地語 (特大) + 日本語 (大) + 英語 3 段目 (opt・やや小)
//   - 直前の発話を薄く上に残す (読み取り猶予)
//   - 表示設定 (フォント倍率) はコントロールページから `subtitle:config` event で受信
//   - backend `subtitle:line` を購読して確定字幕のみ表示 (途中経過ストリーミングなし・Step 1)

import { useEffect, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type {
  SubtitleLineEvent,
  SubtitleStateEvent,
  SubtitleDisplayConfig,
} from '../types/subtitle'

/** 基準フォントサイズ px (fontScale=1.0 のとき)。全画面 1080p を想定した初期値 */
const BASE_LOCAL_PX = 64
const BASE_JA_PX = 44
const BASE_EN_PX = 28

/** 保持する発話履歴数 (要望 2026-07-05: スクロールで遡れるように) */
const MAX_LINES = 50

/** 新しい行からの距離 (0=最新) に応じた不透明度。それより古い行は末尾値で固定 */
const DEPTH_OPACITY = ['opacity-100', 'opacity-60', 'opacity-40', 'opacity-30']

export function SubtitleWindow() {
  // 直近 MAX_LINES 件を縦に蓄積表示 (要望 2026-07-05: 2件→4件・新しいほど大きく)
  const [lines, setLines] = useState<SubtitleLineEvent[]>([])
  const [fontScale, setFontScale] = useState(1.0)
  const [sessionState, setSessionState] = useState<'listening' | 'idle' | 'error'>('idle')
  // スクロール制御: 最下部付近にいる間だけ新着で自動スクロール (遡り閲覧中は動かさない)
  const scrollRef = useRef<HTMLDivElement>(null)
  const stickToBottom = useRef(true)

  useEffect(() => {
    if (stickToBottom.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [lines])

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen<SubtitleLineEvent>('subtitle:line', (e) => {
            if (!mounted) return
            setLines((prev) => [...prev.slice(-(MAX_LINES - 1)), e.payload])
          }),
        )
        unlistens.push(
          await listen<SubtitleStateEvent>('subtitle:state', (e) => {
            if (!mounted) return
            setSessionState(e.payload.state)
          }),
        )
        unlistens.push(
          await listen<SubtitleDisplayConfig>('subtitle:config', (e) => {
            if (!mounted) return
            if (typeof e.payload.fontScale === 'number' && e.payload.fontScale > 0) {
              setFontScale(e.payload.fontScale)
            }
          }),
        )
      } catch (err) {
        console.warn('[subtitle-window] listen failed:', err)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  // depth = 最新行からの距離 (0=最新・大きく表示 / 1〜3=過去行・縮小+減光)
  const renderRows = (line: SubtitleLineEvent, depth: number) => {
    const scale = depth === 0 ? 1 : 0.55
    return (
      <div key={line.seq} className={DEPTH_OPACITY[Math.min(depth, DEPTH_OPACITY.length - 1)]}>
        {line.local && (
          <p
            className="font-bold text-white leading-tight break-words"
            style={{ fontSize: BASE_LOCAL_PX * scale * fontScale }}
          >
            {line.local}
          </p>
        )}
        {line.ja && (
          <p
            className="text-neutral-200 leading-tight break-words mt-2"
            style={{ fontSize: BASE_JA_PX * scale * fontScale }}
          >
            {line.ja}
          </p>
        )}
        {line.en && (
          <p
            className="text-neutral-400 leading-tight break-words mt-2"
            style={{ fontSize: BASE_EN_PX * scale * fontScale }}
          >
            {line.en}
          </p>
        )}
      </div>
    )
  }

  return (
    <div className="h-screen w-screen bg-black flex flex-col overflow-hidden select-none cursor-default">
      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget
          stickToBottom.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 60
        }}
        className="flex-1 overflow-y-auto px-12 pt-6 pb-12"
      >
        <div className="min-h-full flex flex-col justify-end space-y-6">
          {lines.length === 0 && (
            <div className="flex-1 flex items-center justify-center">
              <p className="text-neutral-600 text-2xl">
                {sessionState === 'listening' ? '…' : 'RushVoice セミナー字幕'}
              </p>
            </div>
          )}
          {lines.map((line, i) => renderRows(line, lines.length - 1 - i))}
        </div>
      </div>
      {/* 状態インジケータ (右下・控えめ) */}
      <div className="absolute bottom-3 right-4 flex items-center gap-2 text-xs text-neutral-700">
        <span
          className={
            sessionState === 'listening'
              ? 'inline-block w-2 h-2 rounded-full bg-green-500'
              : sessionState === 'error'
                ? 'inline-block w-2 h-2 rounded-full bg-red-500'
                : 'inline-block w-2 h-2 rounded-full bg-neutral-700'
          }
        />
      </div>
    </div>
  )
}
