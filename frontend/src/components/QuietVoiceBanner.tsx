// QuietVoiceBanner.tsx — v0.1.15 小声警告バナー (L121: 小声で Groq が自信満々に誤字生成)
//
// 仕様 SSOT: 内部設計メモ (v0.1.15 plan)
//
// 背景:
//   小声で喋ると Groq Whisper が自信満々に誤字を生成して貼り付く問題 (L121)。
//   録音セッションの平均 RMS を baseline (普段の声水準・ローリング基準で自動学習) と
//   比較し、相対的に小さければ「声が小さめ」警告を出す (warning-only・貼付は通常実行)。
//
// F2 (MicLowLevelBanner) との違い:
//   - F2 は「録音中」に発火し transcribed/idle で消える。
//   - 本バナーは「文字起こし完了後 (paste 後)」に emit されるため、recorder:transcribed の
//     直前に来る。transcribed で消すと即消えてしまうので、自動消去タイマー方式にする。
//
// 動作:
//   - listen 'recorder:quiet-voice-warning' → 表示 + 6 秒で自動消去
//   - listen 'recorder:state-changed' (recording) → 次の録音開始で即消去 (古い警告を残さない)
//
// 色は F2 と同じ warning 黄色 (yellow-400)。

import { useEffect, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { AlertTriangle } from 'lucide-react'

const AUTO_HIDE_MS = 6000

export function QuietVoiceBanner() {
  const [visible, setVisible] = useState(false)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    const clearTimer = () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current)
        timerRef.current = null
      }
    }

    ;(async () => {
      try {
        unlistens.push(
          // 小声判定 (文字起こし完了後に Rust 側から emit)
          await listen('recorder:quiet-voice-warning', () => {
            if (!mounted) return
            setVisible(true)
            clearTimer()
            timerRef.current = setTimeout(() => {
              if (mounted) setVisible(false)
            }, AUTO_HIDE_MS)
          }),
          // 次の録音が始まったら古い警告を即消去
          await listen<string>('recorder:state-changed', (e) => {
            if (!mounted) return
            if (e.payload === 'recording') {
              clearTimer()
              setVisible(false)
            }
          }),
        )
      } catch (err) {
        console.warn('QuietVoiceBanner listen failed:', err)
      }
    })()

    return () => {
      mounted = false
      clearTimer()
      for (const u of unlistens) u()
    }
  }, [])

  if (!visible) return null

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex items-center gap-2 px-4 py-2 rounded-md bg-yellow-400/90 text-neutral-900 shadow-lg text-sm font-medium animate-in fade-in slide-in-from-top-2"
    >
      <AlertTriangle size={14} />
      <span>声が小さめです・誤変換が増えることがあります（履歴で確認できます）</span>
    </div>
  )
}
