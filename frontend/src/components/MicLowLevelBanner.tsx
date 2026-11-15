// MicLowLevelBanner.tsx — v0.1.12 F2: マイク入力低レベル警告バナー (L264 予防)
//
// 仕様 SSOT: 内部設計メモ
//
// 背景 (L264):
//   開発機で「貼り付かない / ハルシ判定」の真因が Windows 入力音量 8 だった事案。
//   半日格闘した教訓を予防するため、録音開始から N 秒間マイク RMS が継続して
//   閾値未満なら overlay 上に警告バナーを 1 度だけ表示する。
//
// 動作:
//   - listen 'recorder:low-input-level-detected' → 表示 (Rust 側で session 単位 1 回 emit)
//   - listen 'recorder:transcribed' → 非表示 (テキスト確定で自動消去)
//   - listen 'recorder:error' → 非表示 (フェイルセーフ)
//   - listen 'recorder:cancelled' → 非表示 (ESC キャンセル)
//   - listen 'recorder:state-changed' (idle) → 非表示 (フェイルセーフ)
//   - listen 'transcribe:congestion-started' → 抑止 (機能 A 優先・バナーは 1 つだけ画面に出す)
//   - listen 'transcribe:congestion-cleared' → 抑止解除 (再表示しないため意味は薄いが対称性のため購読)
//
// 機能 A 混雑バナーは info 青色 (amber-500)、本バナーは warning 黄色 (yellow-400) で差別化。
// 同一セッション内で 1 回のみ表示する制御は Rust 側 (already_emitted) で担保。

import { useEffect, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { AlertTriangle } from 'lucide-react'

export function MicLowLevelBanner() {
  const [visible, setVisible] = useState(false)
  const [suppressedByCongestion, setSuppressedByCongestion] = useState(false)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    ;(async () => {
      try {
        unlistens.push(
          await listen('recorder:low-input-level-detected', () => {
            if (!mounted) return
            // 機能 A 混雑バナー表示中なら抑止 (バナーは 1 つだけ)
            if (suppressedByCongestion) return
            setVisible(true)
          }),
          // 抑止条件: テキスト確定
          await listen('recorder:transcribed', () => {
            if (!mounted) return
            setVisible(false)
          }),
          // 抑止条件: 録音エラー / halu Drop による recorder:error
          await listen('recorder:error', () => {
            if (!mounted) return
            setVisible(false)
          }),
          // 抑止条件: HK-06 ESC キャンセル
          await listen('recorder:cancelled', () => {
            if (!mounted) return
            setVisible(false)
          }),
          // 抑止条件: 録音終了 (transcribing / idle どちらに遷移しても役割完了)
          //   recording → transcribing で stop が起きるため transcribing でも消去する。
          //   recording → recording の連続変化は無視 (banner 維持)。
          await listen<string>('recorder:state-changed', (e) => {
            if (!mounted) return
            if (e.payload === 'transcribing' || e.payload === 'idle') {
              setVisible(false)
            }
          }),
          // 機能 A 混雑バナー優先 (バナーは 1 つだけ画面に出す)
          await listen('transcribe:congestion-started', () => {
            if (!mounted) return
            setSuppressedByCongestion(true)
            setVisible(false)
          }),
          await listen('transcribe:congestion-cleared', () => {
            if (!mounted) return
            setSuppressedByCongestion(false)
          }),
        )
      } catch (err) {
        console.warn('MicLowLevelBanner listen failed:', err)
      }
    })()

    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
    // suppressedByCongestion は state なので effect の依存に含めず、listener 内で
    // 関数クロージャ越しに参照する。複数 emit の間で state が変わっても、
    // 次の emit 時点の最新値を使うために setVisible を直接読まず closure 経由でも OK。
    // (React 18 で再 effect 走るより listener 維持が重要)
  }, [suppressedByCongestion])

  if (!visible) return null

  return (
    <div
      role="status"
      aria-live="polite"
      className="flex items-center gap-2 px-4 py-2 rounded-md bg-yellow-400/90 text-neutral-900 shadow-lg text-sm font-medium animate-in fade-in slide-in-from-top-2"
    >
      <AlertTriangle size={14} />
      <span>マイク入力が低めです。音量を確認してください</span>
    </div>
  )
}
