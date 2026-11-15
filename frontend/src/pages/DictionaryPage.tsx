/* DictionaryPage.tsx — Phase 2 C Wave 3: DT-01 + DT-02 + DT-05 本実装
 *
 * 仕様: SSOT v2.10 §8.3.1
 *   - DT-01: 表 2 列表示 (pattern / replacement)
 *   - DT-02: 内部列 (context / note / is_regex) を編集フォームで保持・テーブル非表示
 *   - DT-05 (Wave 3): インライン編集 + onBlur 自動保存
 * 設計変更 (Wave 3):
 *   - 追加フォームをテーブル上部に固定 (辞書が増えても常にアクセス可能)
 *   - DT-05: テーブル行の pattern/replacement 列を直接クリックして編集できる
 *             フォーカスが外れた時 (onBlur) に自動保存
 *             既存の「行クリック → 下部フォームで編集」は廃止・インライン編集に統一
 *
 * 既存パターン踏襲: HistoryPage.tsx の sonner + Tailwind + lucide-react 構成。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Loader2, Plus, Search, Trash2, Upload, Download } from 'lucide-react'
import * as Dialog from '@radix-ui/react-dialog'
import { toast } from 'sonner'
import { open as openDialog, save as saveDialog } from '@tauri-apps/plugin-dialog'
import {
  deleteGlossaryEntry,
  findGlossaryOverlapping,
  glossaryExportCsv,
  glossaryHintUsage,
  glossaryImportCsvCommit,
  glossaryImportCsvPreview,
  listGlossary,
  upsertGlossaryEntry,
  type GlossaryHintUsage,
  type ImportMode,
  type ImportPreview,
} from '../lib/tauri'
import {
  emptyGlossaryEntry,
  type GlossaryEntry,
  type OverlapResult,
} from '../types/glossary'
import { ImportConfirmDialog } from '../components/ImportConfirmDialog'

type SortKey = 'pattern' | 'replacement'
type SortDir = 'asc' | 'desc'

export function DictionaryPage() {
  const [items, setItems] = useState<GlossaryEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [search, setSearch] = useState('')
  const [sortKey, setSortKey] = useState<SortKey>('pattern')
  const [sortDir, setSortDir] = useState<SortDir>('asc')

  /** 追加フォームの表示/非表示 */
  const [showAddForm, setShowAddForm] = useState(false)
  const [addDraft, setAddDraft] = useState<GlossaryEntry>(emptyGlossaryEntry())
  const [addSaving, setAddSaving] = useState(false)

  /** 重複検出後の上書き確認 Dialog 対象 */
  const [overwriteCandidate, setOverwriteCandidate] = useState<OverlapResult | null>(null)
  const [overwriteDraft, setOverwriteDraft] = useState<GlossaryEntry>(emptyGlossaryEntry())

  /** 削除確認 Dialog 対象 */
  const [confirmingDelete, setConfirmingDelete] = useState<GlossaryEntry | null>(null)
  const [lastUpdatedAt, setLastUpdatedAt] = useState<Date | null>(null)

  /** DT-05: インライン編集中の行 id → フィールド別の値 (未保存ドラフト) */
  const [inlineDrafts, setInlineDrafts] = useState<Map<number, { pattern: string; replacement: string }>>(new Map())

  /** DT-05: blur 保存タイマー (onBlur → 100ms 後に保存) */
  const blurTimerRef = useRef<Map<number, number>>(new Map())

  /** 機能 E: CSV インポートプレビュー結果 (重複確認 Dialog の駆動データ) */
  const [importPreview, setImportPreview] = useState<ImportPreview | null>(null)
  /** 機能 E: エクスポート / インポート 進行中フラグ (ボタン disable + spinner) */
  const [csvBusy, setCsvBusy] = useState(false)

  /** 2026-08-08: Whisper へ実際に送られる語彙ヒント枠の使用状況 (残量表示用) */
  const [hintUsage, setHintUsage] = useState<GlossaryHintUsage | null>(null)

  const refetch = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const rows = await listGlossary()
      setItems(rows)
      setLastUpdatedAt(new Date())
      // 枠の使用状況は表示のためだけのもの。失敗しても辞書自体の読込は成功扱いにする。
      try {
        setHintUsage(await glossaryHintUsage())
      } catch {
        setHintUsage(null)
      }
    } catch (e) {
      const msg = `${e}`
      setError(msg)
      toast.error(`辞書の読込に失敗: ${msg}`)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void (async () => {
      await refetch()
    })()
  }, [refetch])

  // フィルタ + ソート (client side)
  const visibleItems = useMemo(() => {
    const q = search.trim().toLowerCase()
    const filtered = q
      ? items.filter(
          (e) =>
            e.pattern.toLowerCase().includes(q) ||
            e.replacement.toLowerCase().includes(q),
        )
      : items.slice()
    const dir = sortDir === 'asc' ? 1 : -1
    filtered.sort((a, b) => {
      const av = a[sortKey]
      const bv = b[sortKey]
      if (av < bv) return -1 * dir
      if (av > bv) return 1 * dir
      return 0
    })
    return filtered
  }, [items, search, sortKey, sortDir])

  function handleHeaderClick(key: SortKey) {
    if (sortKey === key) {
      setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
    } else {
      setSortKey(key)
      setSortDir('asc')
    }
  }

  // ===== 追加フォーム =====

  function handleStartAdd() {
    setShowAddForm(true)
    setAddDraft(emptyGlossaryEntry())
  }

  function handleCancelAdd() {
    setShowAddForm(false)
    setAddDraft(emptyGlossaryEntry())
    setOverwriteCandidate(null)
  }

  async function handleAddSubmit() {
    const pattern = addDraft.pattern.trim()
    const replacement = addDraft.replacement.trim()
    if (!pattern) {
      toast.error('誤変換 (pattern) を入力してください')
      return
    }
    if (!replacement) {
      toast.error('正しい変換 (replacement) を入力してください')
      return
    }
    const normalized: GlossaryEntry = {
      ...addDraft,
      pattern,
      replacement,
      context: addDraft.context?.trim() ? addDraft.context.trim() : null,
      note: addDraft.note?.trim() ? addDraft.note.trim() : null,
    }
    // 重複検出
    try {
      const overlaps = await findGlossaryOverlapping(pattern, null)
      if (overlaps.length > 0) {
        const first = overlaps[0]
        setOverwriteCandidate(first)
        setOverwriteDraft({ ...normalized, id: first.entry.id })
        return
      }
    } catch (e) {
      toast.error(`重複検出失敗: ${e}`)
      return
    }
    await commitAdd(normalized)
  }

  async function commitAdd(entry: GlossaryEntry) {
    setAddSaving(true)
    try {
      await upsertGlossaryEntry(entry)
      const wasNew = entry.id == null
      toast.success(wasNew ? '追加しました' : '更新しました (上書き)')
      // 連続追加 UX: フォームクリアして追加モード継続
      setAddDraft(emptyGlossaryEntry())
      setOverwriteCandidate(null)
      await refetch()
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setAddSaving(false)
    }
  }

  // ===== DT-05: インライン編集 =====

  /** インラインセル変更 */
  function handleInlineChange(
    id: number,
    field: 'pattern' | 'replacement',
    value: string,
  ) {
    setInlineDrafts((cur) => {
      const next = new Map(cur)
      const prev = next.get(id) ?? getItemValues(id)
      next.set(id, { ...prev, [field]: value })
      return next
    })
  }

  function getItemValues(id: number): { pattern: string; replacement: string } {
    const item = items.find((x) => x.id === id)
    return { pattern: item?.pattern ?? '', replacement: item?.replacement ?? '' }
  }

  /** インラインセルのフォーカス開始: draft 初期化 */
  function handleInlineFocus(id: number) {
    // タイマーをキャンセル (同一行内のセル移動で保存が走るのを防ぐ)
    const timer = blurTimerRef.current.get(id)
    if (timer != null) {
      window.clearTimeout(timer)
      blurTimerRef.current.delete(id)
    }
    // draft がまだなければ現在の items 値で初期化
    if (!inlineDrafts.has(id)) {
      setInlineDrafts((cur) => {
        const next = new Map(cur)
        next.set(id, getItemValues(id))
        return next
      })
    }
  }

  /** インラインセルの blur: 100ms 後に保存 (同行別セルへの移動ならキャンセル) */
  function handleInlineBlur(id: number) {
    const timer = window.setTimeout(() => {
      blurTimerRef.current.delete(id)
      void commitInlineSave(id)
    }, 100)
    blurTimerRef.current.set(id, timer)
  }

  async function commitInlineSave(id: number) {
    const draft = inlineDrafts.get(id)
    if (draft == null) return
    const item = items.find((x) => x.id === id)
    if (item == null) return

    const pattern = draft.pattern.trim()
    const replacement = draft.replacement.trim()

    // 変更なしならスキップ
    if (pattern === item.pattern && replacement === item.replacement) {
      setInlineDrafts((cur) => {
        const next = new Map(cur)
        next.delete(id)
        return next
      })
      return
    }
    if (!pattern) {
      toast.error('誤変換 (pattern) を入力してください')
      return
    }
    if (!replacement) {
      toast.error('正しい変換 (replacement) を入力してください')
      return
    }
    const updated: GlossaryEntry = { ...item, pattern, replacement }
    try {
      await upsertGlossaryEntry(updated)
      toast.success('更新しました')
      setInlineDrafts((cur) => {
        const next = new Map(cur)
        next.delete(id)
        return next
      })
      await refetch()
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    }
  }

  /** インラインセルの現在表示値 (編集中ならドラフト・それ以外は items の値) */
  function inlineValue(id: number, field: 'pattern' | 'replacement'): string {
    const draft = inlineDrafts.get(id)
    if (draft != null) return draft[field]
    const item = items.find((x) => x.id === id)
    return item?.[field] ?? ''
  }

  // ===== 機能 E: CSV エクスポート / インポート =====

  /** YYYYMMDD 形式の default ファイル名生成 */
  function defaultExportFilename(): string {
    const d = new Date()
    const y = d.getFullYear()
    const m = String(d.getMonth() + 1).padStart(2, '0')
    const day = String(d.getDate()).padStart(2, '0')
    return `rushvoice_glossary_${y}${m}${day}.csv`
  }

  async function handleExportCsv() {
    if (csvBusy) return
    try {
      const path = await saveDialog({
        title: '辞書を CSV に書き出し',
        defaultPath: defaultExportFilename(),
        filters: [{ name: 'CSV', extensions: ['csv'] }],
      })
      if (!path) return // ユーザーキャンセル
      setCsvBusy(true)
      const count = await glossaryExportCsv(path as string)
      toast.success(`CSV にエクスポートしました (${count} 件)`)
    } catch (e) {
      toast.error(`エクスポート失敗: ${e}`)
    } finally {
      setCsvBusy(false)
    }
  }

  async function handleImportCsv() {
    if (csvBusy) return
    try {
      const picked = await openDialog({
        title: '辞書 CSV を取り込む',
        multiple: false,
        filters: [{ name: 'CSV', extensions: ['csv'] }],
      })
      if (!picked) return // ユーザーキャンセル
      const path = typeof picked === 'string' ? picked : picked[0]
      if (!path) return
      setCsvBusy(true)
      const preview = await glossaryImportCsvPreview(path)
      // 重複がなければ即 commit (Overwrite mode = 新規 INSERT のみ実行されることになる)
      if (preview.duplicates.length === 0) {
        const result = await glossaryImportCsvCommit(
          preview.new_entries,
          'Overwrite',
          preview.error_count,
        )
        await refetch()
        toast.success(
          `インポート完了: 追加 ${result.added} / 上書き ${result.updated} / スキップ ${result.skipped} / エラー ${result.errors}`,
        )
      } else {
        // 重複あり → 確認ダイアログ表示
        setImportPreview(preview)
      }
    } catch (e) {
      toast.error(`インポート失敗: ${e}`)
    } finally {
      setCsvBusy(false)
    }
  }

  /** ImportConfirmDialog からのモード選択を受けて確定処理 */
  async function handleImportConfirm(mode: ImportMode) {
    if (!importPreview) return
    setCsvBusy(true)
    try {
      const entriesToSend =
        mode === 'ReplaceAll'
          ? // 全置換は CSV 全件 (new + duplicates の csv 側) を bulk_insert
            [
              ...importPreview.new_entries,
              ...importPreview.duplicates.map(([csv]) => csv),
            ]
          : // Overwrite / Skip は CSV 全件を Rust 側で振り分け処理
            [
              ...importPreview.new_entries,
              ...importPreview.duplicates.map(([csv]) => csv),
            ]
      const result = await glossaryImportCsvCommit(
        entriesToSend,
        mode,
        importPreview.error_count,
      )
      setImportPreview(null)
      await refetch()
      toast.success(
        `インポート完了: 追加 ${result.added} / 上書き ${result.updated} / スキップ ${result.skipped} / エラー ${result.errors}`,
      )
    } catch (e) {
      toast.error(`コミット失敗: ${e}`)
    } finally {
      setCsvBusy(false)
    }
  }

  // ===== 削除 =====

  async function handleDeleteConfirmed() {
    const target = confirmingDelete
    if (target == null || target.id == null) {
      setConfirmingDelete(null)
      return
    }
    const id = target.id
    const prev = items
    setItems((cur) => cur.filter((x) => x.id !== id))
    setConfirmingDelete(null)
    try {
      await deleteGlossaryEntry(id)
      toast.success('削除しました')
      setLastUpdatedAt(new Date())
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
      setItems(prev)
      void refetch()
    }
  }

  return (
    <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
      <header className="mb-6">
        <h2 className="text-xl font-semibold">辞書</h2>
        <p className="text-sm text-neutral-500 mt-1">
          誤変換 → 正しい変換 の登録/編集 · {visibleItems.length} / {items.length} 件
          {lastUpdatedAt && (
            <span className="ml-3 text-neutral-600">
              最終更新: {lastUpdatedAt.toLocaleTimeString('ja-JP')}
            </span>
          )}
        </p>

        {/*
          2026-08-08: Whisper へ実際に送られる語彙ヒント枠の残量。
          辞書は「認識そのものを誘導するヒント枠 (上限あり)」と「転写後の置換 (全件)」の
          2 階建てで、枠に入らなかった語は従来 warn ログにしか出ておらず、
          ユーザーからは「登録したのに効かない」ように見えていた (指摘)。
        */}
        {hintUsage && (
          <div className="mt-3 max-w-4xl px-3 py-2 rounded-lg border border-neutral-700 bg-neutral-800/50 text-xs text-neutral-400">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
              <span className="text-neutral-300 font-medium">音声認識ヒント枠</span>
              <span>
                語数 {hintUsage.used_words} / {hintUsage.max_words}
              </span>
              <span>
                文字数 {hintUsage.used_chars} / {hintUsage.max_chars}
              </span>
              {hintUsage.dropped_words > 0 && (
                <span className="text-amber-400">枠外 {hintUsage.dropped_words} 語</span>
              )}
            </div>
            <p className="mt-1 leading-relaxed">
              {hintUsage.dropped_words > 0 ? (
                <>
                  枠に入った語だけが音声認識そのものへのヒントとして送られます。
                  枠外の {hintUsage.dropped_words} 語も
                  <span className="text-neutral-300">転写後の置換では全件そのまま効きます</span>
                  。置換では直せない語 (同音の実在語など) が枠に優先して入ります。
                </>
              ) : (
                <>登録済みの語はすべて音声認識へのヒントとして送られています。</>
              )}
            </p>
          </div>
        )}
      </header>

      {/* エラー状態 */}
      {error && !loading && (
        <div className="max-w-4xl mb-4 px-4 py-3 bg-red-950/50 border border-red-900/50 rounded-lg text-sm text-red-300">
          辞書の読込に失敗しました: {error}
          <button
            type="button"
            onClick={() => void refetch()}
            className="ml-3 px-2 py-1 bg-red-900/50 hover:bg-red-900 text-xs rounded"
          >
            再試行
          </button>
        </div>
      )}

      {/* 追加フォーム (テーブル上部に固定・スクロールしても常にアクセス可) */}
      {showAddForm && (
        <div className="max-w-4xl mb-4 p-5 bg-neutral-950 border border-neutral-700 rounded-lg">
          <h3 className="text-sm font-semibold text-white mb-4">新規追加</h3>
          <div className="space-y-3">
            <Field label="誤変換 (pattern)" required>
              <input
                type="text"
                value={addDraft.pattern}
                onChange={(e) => setAddDraft({ ...addDraft, pattern: e.target.value })}
                placeholder="例: たかなし/たかなさ/たかなら"
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded text-sm text-white placeholder:text-neutral-600 focus:border-neutral-600 outline-none"
                autoFocus
              />
              <p className="mt-1 text-xs text-neutral-500">
                複数の誤変換を / で区切って 1 つのルールに集約できます
              </p>
            </Field>
            <Field label="正しい変換 (replacement)" required>
              <input
                type="text"
                value={addDraft.replacement}
                onChange={(e) => setAddDraft({ ...addDraft, replacement: e.target.value })}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') void handleAddSubmit()
                }}
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded text-sm text-white focus:border-neutral-600 outline-none"
              />
            </Field>
            <Field label="文脈 (context · 任意)">
              <input
                type="text"
                value={addDraft.context ?? ''}
                onChange={(e) =>
                  setAddDraft({
                    ...addDraft,
                    context: e.target.value === '' ? null : e.target.value,
                  })
                }
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded text-sm text-white focus:border-neutral-600 outline-none"
              />
            </Field>
            <Field label="メモ (note · 任意)">
              <textarea
                value={addDraft.note ?? ''}
                onChange={(e) =>
                  setAddDraft({
                    ...addDraft,
                    note: e.target.value === '' ? null : e.target.value,
                  })
                }
                rows={2}
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded text-sm text-white focus:border-neutral-600 outline-none resize-none"
              />
            </Field>
            <label className="flex items-center gap-2 text-xs text-neutral-400">
              <input
                type="checkbox"
                checked={addDraft.is_regex}
                onChange={(e) => setAddDraft({ ...addDraft, is_regex: e.target.checked })}
              />
              <span>正規表現として扱う (Phase 3 で適用)</span>
            </label>
          </div>
          <div className="mt-5 flex items-center gap-3">
            <button
              type="button"
              onClick={() => void handleAddSubmit()}
              disabled={addSaving}
              className="px-4 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] disabled:opacity-50 text-white text-sm rounded-md transition-colors flex items-center gap-2"
            >
              {addSaving && <Loader2 size={12} className="animate-spin" />}
              <span>追加する</span>
            </button>
            <button
              type="button"
              onClick={handleCancelAdd}
              disabled={addSaving}
              className="px-4 py-2 bg-neutral-800 hover:bg-neutral-700 disabled:opacity-50 text-neutral-300 text-sm rounded-md transition-colors"
            >
              キャンセル
            </button>
          </div>
        </div>
      )}

      {/* 検索 + 追加ボタン */}
      <div className="max-w-4xl mb-3 flex items-center gap-3">
        <div className="flex-1 relative">
          <Search
            size={14}
            className="absolute left-3 top-1/2 -translate-y-1/2 text-neutral-600"
          />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="検索 (誤変換 / 正しい変換)"
            className="w-full pl-9 pr-3 py-2 bg-neutral-950 border border-neutral-800 rounded-md text-sm text-white placeholder:text-neutral-600 focus:border-neutral-600 outline-none"
          />
        </div>
        {!showAddForm && (
          <button
            type="button"
            onClick={handleStartAdd}
            className="flex items-center gap-2 px-4 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-sm rounded-md transition-colors"
          >
            <Plus size={14} />
            <span>追加</span>
          </button>
        )}
        <button
          type="button"
          onClick={() => void handleImportCsv()}
          disabled={csvBusy}
          className="flex items-center gap-2 px-3 py-2 bg-neutral-800 hover:bg-neutral-700 disabled:opacity-50 text-neutral-200 text-sm rounded-md transition-colors"
          title="CSV から辞書を取り込み"
        >
          {csvBusy ? <Loader2 size={14} className="animate-spin" /> : <Upload size={14} />}
          <span>CSV 取込</span>
        </button>
        <button
          type="button"
          onClick={() => void handleExportCsv()}
          disabled={csvBusy || items.length === 0}
          className="flex items-center gap-2 px-3 py-2 bg-neutral-800 hover:bg-neutral-700 disabled:opacity-50 text-neutral-200 text-sm rounded-md transition-colors"
          title="辞書を CSV に書き出し"
        >
          {csvBusy ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
          <span>CSV 書出</span>
        </button>
      </div>

      {/* テーブル (DT-05: 各セルをインライン編集可能) */}
      <div className="max-w-4xl border border-neutral-800 rounded-lg overflow-hidden bg-neutral-950">
        {loading && items.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-neutral-500 flex items-center justify-center gap-2">
            <Loader2 size={14} className="animate-spin" />
            <span>読み込み中…</span>
          </div>
        ) : visibleItems.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-neutral-500">
            {items.length === 0
              ? '辞書エントリがまだありません。「+ 追加」から登録してください。'
              : '検索条件に一致するエントリがありません。'}
          </div>
        ) : (
          <table className="w-full text-sm border-collapse">
            <thead className="bg-neutral-900 text-neutral-400">
              <tr>
                <SortableHeader
                  label="誤変換"
                  active={sortKey === 'pattern'}
                  dir={sortDir}
                  onClick={() => handleHeaderClick('pattern')}
                />
                <SortableHeader
                  label="正しい変換"
                  active={sortKey === 'replacement'}
                  dir={sortDir}
                  onClick={() => handleHeaderClick('replacement')}
                />
                <th className="w-16 text-left px-4 py-2 border-b border-neutral-800"></th>
              </tr>
            </thead>
            <tbody>
              {visibleItems.map((entry) => {
                const id = entry.id!
                const isEditingThisRow = inlineDrafts.has(id)
                return (
                  <tr
                    key={id ?? `${entry.pattern}-${entry.created_at}`}
                    className={
                      'border-b border-neutral-800 transition-colors ' +
                      (isEditingThisRow ? 'bg-neutral-900/70' : 'hover:bg-neutral-900/40')
                    }
                  >
                    {/* pattern 列: インライン編集 */}
                    <td className="px-2 py-1.5">
                      <input
                        type="text"
                        value={inlineValue(id, 'pattern')}
                        onChange={(e) => handleInlineChange(id, 'pattern', e.target.value)}
                        onFocus={() => handleInlineFocus(id)}
                        onBlur={() => handleInlineBlur(id)}
                        className="w-full bg-transparent border border-transparent hover:border-neutral-800 focus:border-neutral-600 rounded px-2 py-1 text-sm text-neutral-100 outline-none"
                      />
                    </td>
                    {/* replacement 列: インライン編集 */}
                    <td className="px-2 py-1.5">
                      <input
                        type="text"
                        value={inlineValue(id, 'replacement')}
                        onChange={(e) => handleInlineChange(id, 'replacement', e.target.value)}
                        onFocus={() => handleInlineFocus(id)}
                        onBlur={() => handleInlineBlur(id)}
                        className="w-full bg-transparent border border-transparent hover:border-neutral-800 focus:border-neutral-600 rounded px-2 py-1 text-sm text-neutral-100 outline-none"
                      />
                    </td>
                    <td className="px-2 py-2 text-right">
                      <button
                        type="button"
                        onClick={(ev) => {
                          ev.stopPropagation()
                          setConfirmingDelete(entry)
                        }}
                        className="p-1.5 hover:bg-red-950/50 rounded text-neutral-500 hover:text-red-400"
                        aria-label="削除"
                      >
                        <Trash2 size={14} />
                      </button>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        )}
      </div>
      <p className="max-w-4xl mt-3 text-xs text-neutral-600">
        ・各セルを直接クリックして編集できます。テーブル外をクリックすると自動保存されます (DT-05)
      </p>

      {/* 削除確認 Dialog */}
      <Dialog.Root
        open={confirmingDelete != null}
        onOpenChange={(open) => !open && setConfirmingDelete(null)}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[420px] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl">
            <Dialog.Title className="text-sm font-semibold text-white">
              辞書エントリを削除しますか?
            </Dialog.Title>
            <Dialog.Description className="mt-2 text-xs text-neutral-400">
              {confirmingDelete && (
                <>
                  「<span className="text-neutral-200">{confirmingDelete.pattern}</span>」 →
                  「<span className="text-neutral-200">{confirmingDelete.replacement}</span>」
                  を削除します。この操作は取り消せません。
                </>
              )}
            </Dialog.Description>
            <div className="mt-5 flex justify-end gap-2">
              <Dialog.Close asChild>
                <button
                  type="button"
                  className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded"
                >
                  キャンセル
                </button>
              </Dialog.Close>
              <button
                type="button"
                onClick={() => void handleDeleteConfirmed()}
                className="px-3 py-1.5 bg-red-900/70 hover:bg-red-900 text-red-100 text-xs rounded"
              >
                削除
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>

      {/* 機能 E: CSV インポート 重複確認 Dialog */}
      <ImportConfirmDialog
        open={importPreview != null}
        duplicates={importPreview?.duplicates ?? []}
        newCount={importPreview?.new_entries.length ?? 0}
        errorCount={importPreview?.error_count ?? 0}
        onConfirm={(mode) => void handleImportConfirm(mode)}
        onCancel={() => setImportPreview(null)}
      />

      {/* 重複上書き確認 Dialog */}
      <Dialog.Root
        open={overwriteCandidate != null}
        onOpenChange={(open) => !open && setOverwriteCandidate(null)}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[460px] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl">
            <Dialog.Title className="text-sm font-semibold text-white">
              既存ルールと誤変換が重複しています
            </Dialog.Title>
            <Dialog.Description className="mt-2 text-xs text-neutral-400">
              {overwriteCandidate && (
                <>
                  以下の誤変換が既存ルールと重複しています:
                  <ul className="mt-2 ml-3 list-disc text-neutral-300">
                    {overwriteCandidate.overlapping_sub_patterns.map((sp) => (
                      <li key={sp}>
                        <span className="text-neutral-100">{sp}</span>
                        <span className="text-neutral-500">
                          {' '}
                          (現 pattern:{' '}
                          <span className="text-neutral-300">
                            {overwriteCandidate.entry.pattern}
                          </span>
                          ・現 replacement:{' '}
                          <span className="text-neutral-300">
                            {overwriteCandidate.entry.replacement}
                          </span>
                          )
                        </span>
                      </li>
                    ))}
                  </ul>
                  <p className="mt-3 text-neutral-500">
                    新しい内容で上書きしますか?
                  </p>
                </>
              )}
            </Dialog.Description>
            <div className="mt-5 flex justify-end gap-2">
              <Dialog.Close asChild>
                <button
                  type="button"
                  className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded"
                >
                  キャンセル
                </button>
              </Dialog.Close>
              <button
                type="button"
                onClick={() => {
                  setOverwriteCandidate(null)
                  void commitAdd(overwriteDraft)
                }}
                className="px-3 py-1.5 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded"
              >
                上書きする
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}

function Field({
  label,
  required,
  children,
}: {
  label: string
  required?: boolean
  children: React.ReactNode
}) {
  return (
    <div>
      <label className="block text-xs text-neutral-400 mb-1">
        {label}
        {required && <span className="text-red-400 ml-1">*</span>}
      </label>
      {children}
    </div>
  )
}

function SortableHeader({
  label,
  active,
  dir,
  onClick,
}: {
  label: string
  active: boolean
  dir: SortDir
  onClick: () => void
}) {
  return (
    <th
      onClick={onClick}
      className="text-left px-4 py-2 font-medium border-b border-neutral-800 cursor-pointer select-none hover:text-white"
    >
      <span>{label}</span>
      {active && <span className="ml-1 text-neutral-500">{dir === 'asc' ? '▲' : '▼'}</span>}
    </th>
  )
}
