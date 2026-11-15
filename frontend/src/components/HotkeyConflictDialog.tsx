/* HotkeyConflictDialog.tsx — v0.1.10 機能 D: ホットキー競合検出ダイアログ
 *
 * SSOT: 内部設計メモ
 *
 * 動作:
 *   - 起動時 1 回のみ表示 (state でフラグ管理し、同セッション内で再表示しない)
 *   - listen 'hotkey:conflict-detected' → ダイアログ表示 (起動時 setup hook で emit される)
 *   - listen 'hotkey:conflict-cleared'  → 何もしない (内部 state にもダイアログにも影響なし)
 *
 * ボタン:
 *   - 「設定画面を開く」: onJumpToSettings() を呼ぶ → App.tsx で setPage('settings')
 *   - 「あとで」: ダイアログを閉じるのみ。設定画面の赤バッジは残り続ける。
 *
 * 既存パターン: ImportConfirmDialog.tsx と同じ Radix Dialog 構成。
 */

import { useEffect, useRef, useState } from 'react'
import * as Dialog from '@radix-ui/react-dialog'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { AlertTriangle, Settings } from 'lucide-react'
import { getHotkeyConflictState, type ConflictState } from '../lib/tauri'

interface Props {
  onJumpToSettings: () => void
}

type ConflictPayload = Extract<ConflictState, { kind: 'Conflict' }>

export function HotkeyConflictDialog({ onJumpToSettings }: Props) {
  const [open, setOpen] = useState(false)
  const [payload, setPayload] = useState<ConflictPayload | null>(null)
  // 1 セッション 1 回のみ表示する。「あとで」で閉じた後は同セッション中に再 emit されても表示しない。
  // useState ではなく useRef を使う:
  //   listen の closure が初回 mount 時の値で固定されるのを避け、
  //   かつ初期 fetch / listen の両経路で確実に 1 度きり表示にするため。
  const shownOnceRef = useRef(false)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    const triggerOnce = (p: ConflictPayload) => {
      if (!mounted) return
      if (shownOnceRef.current) return
      shownOnceRef.current = true
      setPayload(p)
      setOpen(true)
    }

    ;(async () => {
      // Codex P2 対応 (2026-05-30): Rust setup hook の emit がこの listener mount より前に
      // 起きる race を回避する。永続化された状態を mount 直後に読み出して
      // 既に conflict なら同じ UX でダイアログを表示する。
      // listen 自体は念のため subscribe し続け、設定変更時の二重保証として残す。
      try {
        const cs = await getHotkeyConflictState()
        if (cs.kind === 'Conflict') {
          triggerOnce(cs)
        }
      } catch (err) {
        console.warn('HotkeyConflictDialog initial fetch failed:', err)
      }

      try {
        unlistens.push(
          await listen<ConflictPayload>('hotkey:conflict-detected', (e) => {
            // ガード: kind が 'Conflict' でない場合 (None が誤って来た場合) は無視
            if (!e.payload || e.payload.kind !== 'Conflict') return
            triggerOnce(e.payload)
          }),
        )
      } catch (err) {
        console.warn('HotkeyConflictDialog listen failed:', err)
      }
    })()

    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  function formatHotkey(p: ConflictPayload): string {
    const parts: string[] = []
    for (const m of p.modifiers) {
      // 'ControlLeft' / 'Control' を 'Ctrl' に丸める表示用
      const lower = m.toLowerCase()
      if (lower.startsWith('control')) parts.push('Ctrl')
      else if (lower.startsWith('shift')) parts.push('Shift')
      else if (lower.startsWith('alt')) parts.push('Alt')
      else if (lower.startsWith('meta')) parts.push('Win')
      else parts.push(m)
    }
    // code を簡略表記
    let code = p.code
    if (code.startsWith('Key')) code = code.slice(3)
    else if (code.startsWith('Digit')) code = code.slice(5)
    else if (code === 'NonConvert') code = '無変換'
    else if (code === 'Convert') code = '変換'
    parts.push(code)
    return parts.join(' + ')
  }

  function handleJump() {
    setOpen(false)
    onJumpToSettings()
  }

  function handleLater() {
    setOpen(false)
  }

  return (
    <Dialog.Root open={open} onOpenChange={(o) => !o && handleLater()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60" />
        <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[480px] max-h-[85vh] bg-neutral-950 border border-red-500/50 rounded-lg p-5 shadow-2xl overflow-y-auto">
          <Dialog.Title className="text-sm font-semibold text-white flex items-center gap-2">
            <AlertTriangle size={16} className="text-red-500" />
            <span>ホットキーが他のアプリと競合しています</span>
          </Dialog.Title>
          <Dialog.Description className="mt-3 text-xs text-neutral-400 leading-relaxed">
            現在のホットキー
            {payload && (
              <span className="mx-1 px-1.5 py-0.5 bg-red-500/15 border border-red-500/40 text-red-200 rounded font-mono">
                {formatHotkey(payload)}
              </span>
            )}
            は他のアプリ・OS によって既に使用されているため、録音操作が正常に行えない可能性があります。
            設定画面から別のキー組合せに変更してください。
          </Dialog.Description>

          <div className="mt-5 flex gap-2 justify-end">
            <button
              type="button"
              onClick={handleLater}
              className="px-3 py-1.5 text-xs text-neutral-300 bg-neutral-800 hover:bg-neutral-700 rounded-md"
            >
              あとで
            </button>
            <button
              type="button"
              onClick={handleJump}
              className="px-3 py-1.5 text-xs text-white bg-red-600 hover:bg-red-500 rounded-md flex items-center gap-1.5"
            >
              <Settings size={12} />
              <span>設定画面を開く</span>
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}
