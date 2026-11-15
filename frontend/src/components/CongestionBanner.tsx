// CongestionBanner.tsx — v0.1.10 機能 A: Groq 混雑時通知バナー
//
// 仕様 SSOT: 内部設計メモ
//
// 動作:
//   - listen 'transcribe:congestion-started' → 表示 (Rust 側で threshold_ms 経過後に発火)
//   - listen 'transcribe:congestion-cleared' → 非表示 (transcribe 完了)
//   - listen 'recorder:transcribed' / 'recorder:error' → 非表示 (フェイルセーフ)
//   - listen 'recorder:state-changed' (idle) → 非表示 (フェイルセーフ)
//
// HomePage の録音ボタン直下に配置することで、ユーザーが「文字起こし中」と
// 認識している箇所と物理的に近い位置で安心メッセージを表示する。

import { useEffect, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { Loader2 } from 'lucide-react'

export function CongestionBanner() {
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    ;(async () => {
      try {
        unlistens.push(
          await listen<number>('transcribe:congestion-started', () => {
            if (!mounted) return
            setVisible(true)
          }),
          await listen('transcribe:congestion-cleared', () => {
            if (!mounted) return
            setVisible(false)
          }),
          // フェイルセーフ: cleared が来なかったケースで二重保証
          await listen('recorder:transcribed', () => {
            if (!mounted) return
            setVisible(false)
          }),
          await listen('recorder:error', () => {
            if (!mounted) return
            setVisible(false)
          }),
          await listen<string>('recorder:state-changed', (e) => {
            if (!mounted) return
            // idle に遷移したら必ずバナーを消す
            if (e.payload === 'idle') {
              setVisible(false)
            }
          }),
        )
      } catch (err) {
        console.warn('CongestionBanner listen failed:', err)
      }
    })()

    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  if (!visible) return null

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex items-center gap-2 px-4 py-2 rounded-md bg-amber-500/90 text-neutral-900 shadow-lg text-sm font-medium animate-in fade-in slide-in-from-top-2"
    >
      <Loader2 size={14} className="animate-spin" />
      <span>文字起こし中…通常より時間がかかっています</span>
    </div>
  )
}
