/* ImportConfirmDialog.tsx — v0.1.10 機能 E: CSV インポート重複確認ダイアログ
 *
 * SSOT: v0_1_10_feature_e_delegation_2026-05-30.md
 *
 * 表示内容:
 *   - 「重複 N 件あり: どう扱いますか?」
 *   - 重複一覧プレビュー (上位 5 件 + 「他 N 件」)
 *   - 3 ボタン: 「上書き (Overwrite)」「スキップ (Skip)」「全置換 (ReplaceAll)」
 *
 * 既存パターン: DictionaryPage.tsx の重複上書き Dialog と同じ Radix Dialog 構成。
 */

import * as Dialog from '@radix-ui/react-dialog'
import { AlertTriangle } from 'lucide-react'
import type { GlossaryEntry, ImportMode } from '../lib/tauri'

interface Props {
  open: boolean
  /** (csv_entry, existing_entry) のペア配列 */
  duplicates: [GlossaryEntry, GlossaryEntry][]
  /** 新規追加件数 (上書き/スキップ どちらでも追加される) */
  newCount: number
  /** パースエラー件数 (情報表示のみ) */
  errorCount: number
  /** モード選択 (Overwrite / Skip / ReplaceAll) */
  onConfirm: (mode: ImportMode) => void
  /** キャンセル */
  onCancel: () => void
}

const PREVIEW_LIMIT = 5

export function ImportConfirmDialog({
  open,
  duplicates,
  newCount,
  errorCount,
  onConfirm,
  onCancel,
}: Props) {
  const preview = duplicates.slice(0, PREVIEW_LIMIT)
  const restCount = Math.max(0, duplicates.length - PREVIEW_LIMIT)

  return (
    <Dialog.Root open={open} onOpenChange={(o) => !o && onCancel()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 bg-black/60" />
        <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[560px] max-h-[85vh] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl overflow-y-auto">
          <Dialog.Title className="text-sm font-semibold text-white flex items-center gap-2">
            <AlertTriangle size={16} className="text-yellow-500" />
            <span>CSV インポート重複確認</span>
          </Dialog.Title>
          <Dialog.Description className="mt-3 text-xs text-neutral-400 leading-relaxed">
            CSV 内に既存辞書と pattern (誤変換) が重複する行が
            <span className="text-yellow-300 mx-1">{duplicates.length}</span>
            件あります。重複の処理方法を選んでください。
            {newCount > 0 && (
              <>
                <br />
                新規追加 (重複なし) は{' '}
                <span className="text-emerald-300">{newCount}</span> 件です。
              </>
            )}
            {errorCount > 0 && (
              <>
                <br />
                パース失敗でスキップされる不正行は{' '}
                <span className="text-red-300">{errorCount}</span> 件です。
              </>
            )}
          </Dialog.Description>

          {preview.length > 0 && (
            <div className="mt-4 border border-neutral-800 rounded bg-neutral-900 max-h-[260px] overflow-y-auto">
              <table className="w-full text-xs">
                <thead className="bg-neutral-900 sticky top-0">
                  <tr className="text-neutral-500 border-b border-neutral-800">
                    <th className="text-left px-3 py-1.5 font-medium">pattern</th>
                    <th className="text-left px-3 py-1.5 font-medium">既存 → CSV</th>
                  </tr>
                </thead>
                <tbody>
                  {preview.map(([csvEntry, existing], i) => (
                    <tr key={`${csvEntry.pattern}-${i}`} className="border-b border-neutral-800/60">
                      <td className="px-3 py-1.5 text-neutral-200 font-mono">{csvEntry.pattern}</td>
                      <td className="px-3 py-1.5 text-neutral-300">
                        <span className="text-neutral-500">{existing.replacement}</span>
                        <span className="mx-2 text-neutral-600">→</span>
                        <span className="text-neutral-100">{csvEntry.replacement}</span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {restCount > 0 && (
                <div className="px-3 py-2 text-xs text-neutral-500 border-t border-neutral-800 bg-neutral-950">
                  ...他 {restCount} 件
                </div>
              )}
            </div>
          )}

          <div className="mt-5 space-y-2 text-xs text-neutral-400">
            <p>
              <span className="text-neutral-200 font-medium">上書き:</span>{' '}
              重複行は CSV 内容で UPDATE・新規はそのまま INSERT
            </p>
            <p>
              <span className="text-neutral-200 font-medium">スキップ:</span>{' '}
              重複行は無視・新規のみ INSERT
            </p>
            <p>
              <span className="text-neutral-200 font-medium">全置換:</span>{' '}
              既存辞書を全削除して CSV 全件で置き換え (取り消し不可)
            </p>
          </div>

          <div className="mt-5 flex justify-end gap-2 flex-wrap">
            <Dialog.Close asChild>
              <button
                type="button"
                onClick={onCancel}
                className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded"
              >
                キャンセル
              </button>
            </Dialog.Close>
            <button
              type="button"
              onClick={() => onConfirm('Skip')}
              className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 text-xs rounded"
            >
              スキップ
            </button>
            <button
              type="button"
              onClick={() => onConfirm('Overwrite')}
              className="px-3 py-1.5 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded"
            >
              上書き
            </button>
            <button
              type="button"
              onClick={() => onConfirm('ReplaceAll')}
              className="px-3 py-1.5 bg-red-900/70 hover:bg-red-900 text-red-100 text-xs rounded"
            >
              全置換
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  )
}
