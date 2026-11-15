/* ProfilePage.tsx — Phase 2 C Wave 2 PT-01〜PT-10 本実装
 *
 * 仕様: SSOT v2.10 §8.3.1 PT-01〜PT-10
 * 設計判断: 委任プロンプト 2026-05-26 (承認済) — SQLite 採用・3 プリセット投入
 *
 * 機能:
 *   - PT-01 プロファイル一覧 (Radix DropdownMenu) + 「+ 新規追加」(Wave 2 follow-up)
 *   - PT-02 表示名編集 (Input)
 *   - PT-03 Whisper 送信プロンプト (Textarea)
 *   - PT-04 後処理 LLM プロンプト (Textarea + 1200 文字バリデーション)
 *   - PT-05 フィラー除去 (Radix Checkbox)
 *   - PT-08 絵文字選択 (Radix RadioGroup)
 *   - PT-09 保存ボタン (Button + upsertProfile)
 *   - PT-10 デフォルトに戻す (Button + 確認 Dialog)
 *
 * 編集モデル:
 *   - 選択プロファイル (selectedId) の編集中スナップショット (draft) を state で持つ。
 *   - 「保存」ボタンで draft を upsertProfile に送信。
 *   - 別プロファイルに切り替える時、draft 未保存変更があれば破棄確認 (window.confirm)。
 *   - ビルトインプロファイル: 表示名・各フィールド編集可能・「デフォルトに戻す」で初期化。
 *   - user 作成プロファイル: 全フィールド編集 + 「削除」ボタン表示。
 *
 * Wave 2 follow-up 2026-05-26 (要望で追加実装):
 *   - 「+ 新規追加」を PT-01 DropdownMenu 最下部に追加 (Separator + MenuItem)。
 *     クリックで Radix Dialog 表示・名前入力 (空 / 重複バリデーション) → upsertProfile → 新規選択。
 *   - ビルトインプリセットは mail/business_doc/casual に差替済 (Rust 側 migration)。
 *
 * 未対応:
 *   - active_profile_id (どれを録音時に使うか) の永続化は Phase 3 で追加 (現状は draft selection のみ)。
 */

import { useCallback, useEffect, useMemo, useState } from 'react'
import { Check, ChevronDown, Loader2, Plus, RotateCcw, Save, Trash2, User } from 'lucide-react'
import * as DropdownMenu from '@radix-ui/react-dropdown-menu'
import * as Checkbox from '@radix-ui/react-checkbox'
import * as RadioGroup from '@radix-ui/react-radio-group'
import * as Dialog from '@radix-ui/react-dialog'
import { toast } from 'sonner'
import {
  listProfiles,
  upsertProfile,
  deleteProfile,
  resetProfileToDefault,
} from '../lib/tauri'
import {
  EMOJI_DENSITY_LABELS,
  LLM_PROMPT_MAX_CHARS,
  builtinKeyToHint,
  emptyProfile,
  type EmojiDensity,
  type Profile,
} from '../types/profile'
import { cn } from '../lib/cn'

type ConfirmKind = 'reset' | 'delete' | null

/** P2-1 fix: プロファイル名の比較用正規化 (重複検出用)。
 * NFKC で全角/半角を統一・trim で前後空白除去・toLowerCase で大小文字統一。
 * 例: "Mail" / "mail" / "ＭＡＩＬ" の表記揺れ・前後空白の有無による重複を同一視。
 *
 * 限界 (Codex P3 review 2026-05-26):
 *   ハイフン "-" (U+002D) と長音記号 "ー" (U+30FC) は NFKC では別文字のまま残るため、
 *   「メ-ル」と「メール」「メ‐ル」(U+2010) 等の表記揺れは別物として扱われる。
 *   ハイフン/ダッシュ系の包括的正規化が必要な場合は Wave 3 以降で追加検討。
 */
function normalizeProfileName(s: string): string {
  return s.normalize('NFKC').trim().toLowerCase()
}

export function ProfilePage() {
  const [profiles, setProfiles] = useState<Profile[]>([])
  const [loading, setLoading] = useState(true)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [draft, setDraft] = useState<Profile | null>(null)
  const [saving, setSaving] = useState(false)
  const [confirmKind, setConfirmKind] = useState<ConfirmKind>(null)

  // Wave 2 follow-up: 新規プロファイル追加 Dialog
  const [createOpen, setCreateOpen] = useState(false)
  const [createName, setCreateName] = useState('')
  const [creating, setCreating] = useState(false)

  const refetch = useCallback(async (preferredId?: number | null) => {
    setLoading(true)
    setLoadError(null)
    try {
      const fetched = await listProfiles()
      setProfiles(fetched)
      // 選択 ID 解決: preferred → 旧 selected → 先頭
      let nextId: number | null = null
      const prefer = preferredId ?? selectedId
      if (prefer != null && fetched.some((p) => p.id === prefer)) {
        nextId = prefer
      } else if (fetched.length > 0 && fetched[0]?.id != null) {
        nextId = fetched[0].id
      }
      setSelectedId(nextId)
      const sel = fetched.find((p) => p.id === nextId) ?? null
      setDraft(sel ? { ...sel } : null)
    } catch (e) {
      const msg = `${e}`
      setLoadError(msg)
      toast.error(`プロファイル読込失敗: ${msg}`)
    } finally {
      setLoading(false)
    }
    // refetch は selectedId の最新値を見たくないので意図的に依存配列から外す
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    void refetch()
  }, [refetch])

  /** 現在の draft が DB と差分があるか (PT-09 「保存」ボタン活性化判定) */
  const dirty = useMemo(() => {
    if (!draft) return false
    const original = profiles.find((p) => p.id === draft.id)
    if (!original) return draft.id == null // 新規はそのまま dirty
    return (
      original.display_name !== draft.display_name ||
      original.whisper_prompt !== draft.whisper_prompt ||
      original.llm_postprocess_prompt !== draft.llm_postprocess_prompt ||
      original.filler_removal !== draft.filler_removal ||
      original.emoji_density !== draft.emoji_density
    )
  }, [draft, profiles])

  const llmPromptLen = useMemo(
    () => (draft ? Array.from(draft.llm_postprocess_prompt).length : 0),
    [draft],
  )
  const llmPromptOverflow = llmPromptLen > LLM_PROMPT_MAX_CHARS

  /** PT-01 別プロファイル選択。未保存変更があれば確認。 */
  function handleSelect(p: Profile) {
    if (p.id === selectedId) return
    if (dirty) {
      // 「破棄確認」は window.confirm で簡易対応 (Wave 2 スコープ内・Dialog 追加実装は Wave 3 で再検討)
      const ok = window.confirm('未保存の変更があります。破棄して切り替えますか?')
      if (!ok) return
    }
    setSelectedId(p.id ?? null)
    setDraft({ ...p })
  }

  /** Wave 2 follow-up: 新規プロファイル追加 Dialog を開く (DropdownMenu「+ 新規追加」から)
   *  Codex P2 (2026-05-26): 未保存変更があれば確認する
   *  (確認なしで Dialog OK → upsertProfile → refetch(newId) すると現在の draft 編集が
   *   別プロファイルへの選択切替で完全に失われる)
   *
   *  P2-3 fix (2026-05-26 follow-up review): 旧文言「破棄して新規追加に進みますか?」は
   *  実際には draft を破棄しないため誤り (Dialog キャンセルすれば draft 編集に戻れる)。
   *  事実に即して「先に保存しますか?」型の文言に変更。
   */
  function handleOpenCreate() {
    if (dirty) {
      const ok = window.confirm(
        '現在のプロファイルに未保存の変更があります。\n先に保存してから新規追加することをお勧めします。\nこのまま新規追加 Dialog を開きますか? (キャンセルで戻れます)',
      )
      if (!ok) return
    }
    setCreateName('')
    setCreateOpen(true)
  }

  /** Dialog OK ボタン: バリデーション → upsertProfile (INSERT) → 新規 ID を選択状態に */
  async function handleCreateConfirmed() {
    const name = createName.trim()
    if (name === '') {
      toast.error('プロファイル名を入力してください')
      return
    }
    // P2-1 fix: 重複チェックを NFKC 正規化 + trim + lowerCase で行う。
    // 「メール」と「メ-ル」(全角/半角ハイフン)、「Mail」と「mail」、
    // 全角/半角英数 等の表記揺れによる重複作成を防ぐ。
    if (profiles.some((p) => normalizeProfileName(p.display_name) === normalizeProfileName(name))) {
      toast.error(`「${name}」は既に存在します (表記揺れ含む)`)
      return
    }
    setCreating(true)
    try {
      const fresh = { ...emptyProfile(), display_name: name }
      const newId = await upsertProfile(fresh)
      setCreateOpen(false)
      toast.success(`プロファイル「${name}」を作成しました`)
      await refetch(newId)
    } catch (e) {
      toast.error(`作成失敗: ${e}`)
    } finally {
      setCreating(false)
    }
  }

  /** PT-09 保存。upsertProfile → refetch (Rust 側 updated_at 反映) */
  async function handleSave() {
    if (!draft || !dirty || saving) return
    if (llmPromptOverflow) {
      toast.error(`後処理 LLM プロンプトが ${LLM_PROMPT_MAX_CHARS} 文字を超えています`)
      return
    }
    if (draft.display_name.trim() === '') {
      toast.error('表示名を入力してください')
      return
    }
    setSaving(true)
    try {
      const newId = await upsertProfile(draft)
      toast.success('プロファイルを保存しました')
      await refetch(newId)
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setSaving(false)
    }
  }

  /** PT-10「デフォルトに戻す」(ビルトインのみ) */
  async function handleResetConfirmed() {
    if (!draft || draft.id == null) {
      setConfirmKind(null)
      return
    }
    const id = draft.id
    setConfirmKind(null)
    setSaving(true)
    try {
      await resetProfileToDefault(id)
      toast.success('デフォルトに戻しました')
      await refetch(id)
    } catch (e) {
      toast.error(`リセット失敗: ${e}`)
    } finally {
      setSaving(false)
    }
  }

  /** user 作成プロファイル削除 */
  async function handleDeleteConfirmed() {
    if (!draft || draft.id == null) {
      setConfirmKind(null)
      return
    }
    const id = draft.id
    setConfirmKind(null)
    setSaving(true)
    try {
      await deleteProfile(id)
      toast.success('プロファイルを削除しました')
      await refetch(null)
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
    } finally {
      setSaving(false)
    }
  }

  // 編集ハンドラ群
  function updateDraft<K extends keyof Profile>(field: K, value: Profile[K]) {
    setDraft((cur) => (cur ? { ...cur, [field]: value } : cur))
  }

  if (loading && profiles.length === 0) {
    return (
      <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
        <header className="mb-6">
          <h2 className="text-xl font-semibold">プロファイル</h2>
        </header>
        <div className="px-4 py-12 text-center text-sm text-neutral-500 flex items-center justify-center gap-2">
          <Loader2 size={14} className="animate-spin" />
          <span>読み込み中…</span>
        </div>
      </div>
    )
  }

  if (loadError) {
    return (
      <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
        <header className="mb-6">
          <h2 className="text-xl font-semibold">プロファイル</h2>
        </header>
        <div className="max-w-3xl px-4 py-3 bg-red-950/50 border border-red-900/50 rounded-lg text-sm text-red-300">
          プロファイル読込に失敗しました: {loadError}
          <button
            type="button"
            onClick={() => void refetch()}
            className="ml-3 px-2 py-1 bg-red-900/50 hover:bg-red-900 text-xs rounded"
          >
            再試行
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
      <header className="mb-6">
        <h2 className="text-xl font-semibold">プロファイル</h2>
        <p className="text-sm text-neutral-500 mt-1">
          用途別の整形設定を切り替えます · {profiles.length} 件
        </p>
      </header>

      <section className="space-y-6 max-w-3xl">
        {/* PT-01 プロファイル選択 (DropdownMenu) */}
        <div className="space-y-2 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
          <label className="block text-xs text-neutral-400">プロファイル選択</label>
          <ProfileDropdown
            profiles={profiles}
            selectedId={selectedId}
            onSelect={handleSelect}
            onCreateNew={handleOpenCreate}
          />
          {draft?.is_builtin && (
            <p className="text-xs text-neutral-500 pt-1">
              ビルトインプリセット
              {draft.builtin_key && builtinKeyToHint(draft.builtin_key)
                ? ` · ${builtinKeyToHint(draft.builtin_key)}`
                : ''}
            </p>
          )}
        </div>

        {draft && (
          <>
            {/* PT-02 表示名 */}
            <div className="space-y-2 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
              <label
                htmlFor="profile-display-name"
                className="block text-xs text-neutral-400"
              >
                表示名
              </label>
              <input
                id="profile-display-name"
                type="text"
                value={draft.display_name}
                onChange={(e) => updateDraft('display_name', e.target.value)}
                placeholder="プロファイル名"
                disabled={saving}
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none disabled:opacity-60"
              />
            </div>

            {/* PT-03 Whisper 送信プロンプト */}
            <div className="space-y-2 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
              <label
                htmlFor="profile-whisper-prompt"
                className="block text-xs text-neutral-400"
              >
                Whisper 送信プロンプト
              </label>
              <p className="text-xs text-neutral-500">
                Whisper API に固有名詞のヒントを渡します (50-100 文字推奨)
              </p>
              <textarea
                id="profile-whisper-prompt"
                value={draft.whisper_prompt}
                onChange={(e) => updateDraft('whisper_prompt', e.target.value)}
                placeholder="LashRush、店舗名、スタッフ名 など…"
                rows={3}
                disabled={saving}
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none disabled:opacity-60 font-mono resize-y"
              />
            </div>

            {/* PT-04 後処理 LLM プロンプト + 1200 文字バリデーション */}
            <div className="space-y-2 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
              <div className="flex items-center justify-between">
                <label
                  htmlFor="profile-llm-prompt"
                  className="block text-xs text-neutral-400"
                >
                  後処理 LLM プロンプト
                </label>
                <span
                  className={cn(
                    'text-xs',
                    llmPromptOverflow ? 'text-red-400' : 'text-neutral-500',
                  )}
                >
                  {llmPromptLen} / {LLM_PROMPT_MAX_CHARS}
                </span>
              </div>
              <p className="text-xs text-neutral-500">
                文字起こし結果を LLM で整形する際の system prompt
                (Phase 3 で接続予定・現状は保存のみ)
              </p>
              <textarea
                id="profile-llm-prompt"
                value={draft.llm_postprocess_prompt}
                onChange={(e) => updateDraft('llm_postprocess_prompt', e.target.value)}
                placeholder="あなたは...の日本語整形 AI です。..."
                rows={6}
                disabled={saving}
                className={cn(
                  'w-full px-3 py-2 bg-neutral-900 border rounded-md text-sm focus:outline-none disabled:opacity-60 resize-y',
                  llmPromptOverflow
                    ? 'border-red-500 focus:border-red-400'
                    : 'border-neutral-800 focus:border-[var(--color-brand-500)]',
                )}
              />
              {llmPromptOverflow && (
                <p className="text-xs text-red-400">
                  上限を {llmPromptLen - LLM_PROMPT_MAX_CHARS} 文字超えています
                </p>
              )}
            </div>

            {/* PT-05 フィラー除去 + PT-08 絵文字密度 + 絵文字モード (Phase 2 A) */}
            <div className="space-y-5 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
              <FillerRemovalCheckbox
                checked={draft.filler_removal}
                disabled={saving}
                onChange={(v) => updateDraft('filler_removal', v)}
              />
              <EmojiDensityRadioGroup
                value={draft.emoji_density as EmojiDensity}
                disabled={saving}
                onChange={(v) => updateDraft('emoji_density', v)}
              />
            </div>

            {/* PT-09 保存 + PT-10 デフォルトに戻す + ユーザー作成プロファイル削除 */}
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                onClick={() => void handleSave()}
                disabled={!dirty || saving || llmPromptOverflow}
                className="px-4 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
              >
                {saving ? (
                  <Loader2 size={12} className="animate-spin" />
                ) : (
                  <Save size={12} />
                )}
                <span>保存</span>
              </button>
              {draft.is_builtin && (
                <button
                  type="button"
                  onClick={() => setConfirmKind('reset')}
                  disabled={saving}
                  className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
                  title="ビルトインプリセットの初期値に戻します"
                >
                  <RotateCcw size={12} />
                  <span>デフォルトに戻す</span>
                </button>
              )}
              {!draft.is_builtin && draft.id != null && (
                <button
                  type="button"
                  onClick={() => setConfirmKind('delete')}
                  disabled={saving}
                  className="ml-auto px-3 py-2 bg-neutral-800 hover:bg-red-900 text-neutral-400 hover:text-red-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
                  aria-label="このプロファイルを削除"
                >
                  <Trash2 size={12} />
                  <span>このプロファイルを削除</span>
                </button>
              )}
            </div>
          </>
        )}
      </section>

      {/* Wave 2 follow-up: 新規プロファイル追加 Dialog */}
      <Dialog.Root
        open={createOpen}
        onOpenChange={(open) => {
          if (!creating) setCreateOpen(open)
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[420px] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl">
            <Dialog.Title className="text-sm font-semibold text-white">
              新規プロファイル
            </Dialog.Title>
            <Dialog.Description className="mt-2 text-xs text-neutral-400">
              プロファイル名を入力してください。作成後に各フィールドを編集できます。
            </Dialog.Description>
            <form
              className="mt-4 space-y-3"
              onSubmit={(e) => {
                e.preventDefault()
                void handleCreateConfirmed()
              }}
            >
              <input
                type="text"
                value={createName}
                onChange={(e) => setCreateName(e.target.value)}
                placeholder="例: 顧客対応、研修メモ"
                disabled={creating}
                autoFocus
                maxLength={64}
                className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none disabled:opacity-60"
              />
              <div className="flex justify-end gap-2">
                <Dialog.Close asChild>
                  <button
                    type="button"
                    disabled={creating}
                    className="px-3 py-1.5 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded disabled:opacity-50"
                  >
                    キャンセル
                  </button>
                </Dialog.Close>
                <button
                  type="submit"
                  disabled={creating || createName.trim() === ''}
                  className="px-3 py-1.5 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded flex items-center gap-1.5 disabled:opacity-50"
                >
                  {creating ? (
                    <Loader2 size={12} className="animate-spin" />
                  ) : (
                    <Plus size={12} />
                  )}
                  <span>作成</span>
                </button>
              </div>
            </form>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>

      {/* 確認 Dialog: reset / delete 共用 */}
      <Dialog.Root
        open={confirmKind !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmKind(null)
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[420px] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl">
            <Dialog.Title className="text-sm font-semibold text-white">
              {confirmKind === 'reset'
                ? 'プロファイルをデフォルトに戻しますか?'
                : 'プロファイルを削除しますか?'}
            </Dialog.Title>
            <Dialog.Description className="mt-2 text-xs text-neutral-400">
              {confirmKind === 'reset' && draft && (
                <>
                  プロファイル「
                  <span className="text-neutral-200">{draft.display_name}</span>
                  」を初期値に戻します。表示名・プロンプト・各種設定が
                  プリセットの初期値に上書きされます。
                </>
              )}
              {confirmKind === 'delete' && draft && (
                <>
                  プロファイル「
                  <span className="text-neutral-200">{draft.display_name}</span>
                  」を削除します。この操作は取り消せません。
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
                onClick={() =>
                  confirmKind === 'reset'
                    ? void handleResetConfirmed()
                    : void handleDeleteConfirmed()
                }
                className={cn(
                  'px-3 py-1.5 text-xs rounded',
                  confirmKind === 'delete'
                    ? 'bg-red-900/70 hover:bg-red-900 text-red-100'
                    : 'bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white',
                )}
              >
                {confirmKind === 'reset' ? 'リセット' : '削除'}
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  )
}

// ----- PT-01 DropdownMenu (Radix UI) -----

interface ProfileDropdownProps {
  profiles: Profile[]
  selectedId: number | null
  onSelect: (p: Profile) => void
  /** Wave 2 follow-up: 「+ 新規追加」MenuItem クリックで親の Dialog を開く */
  onCreateNew: () => void
}

function ProfileDropdown({
  profiles,
  selectedId,
  onSelect,
  onCreateNew,
}: ProfileDropdownProps) {
  const selected = profiles.find((p) => p.id === selectedId) ?? null

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          className="w-full flex items-center justify-between gap-2 px-3 py-2 bg-neutral-900 border border-neutral-800 hover:border-neutral-700 rounded-md text-sm text-neutral-100 focus:outline-none focus:border-[var(--color-brand-500)]"
          aria-label="プロファイルを選択"
        >
          <span className="flex items-center gap-2">
            <User size={14} className="text-neutral-500" />
            <span>{selected ? selected.display_name : '未選択'}</span>
            {selected?.is_builtin && (
              <span className="text-[10px] text-neutral-500 bg-neutral-800 px-1.5 py-0.5 rounded">
                ビルトイン
              </span>
            )}
          </span>
          <ChevronDown size={14} className="text-neutral-500" />
        </button>
      </DropdownMenu.Trigger>

      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="start"
          sideOffset={4}
          className="min-w-[var(--radix-dropdown-menu-trigger-width)] max-h-[60vh] overflow-y-auto bg-neutral-950 border border-neutral-800 rounded-md shadow-2xl py-1 z-50"
        >
          {profiles.length === 0 && (
            <div className="px-3 py-2 text-xs text-neutral-500">プロファイルがありません</div>
          )}
          {profiles.map((p) => {
            const active = p.id === selectedId
            return (
              <DropdownMenu.Item
                key={p.id ?? `tmp-${p.display_name}`}
                onSelect={() => onSelect(p)}
                className={cn(
                  'flex items-center justify-between gap-2 px-3 py-2 text-sm cursor-pointer outline-none',
                  active
                    ? 'bg-[var(--color-brand-700)]/30 text-white'
                    : 'text-neutral-200 hover:bg-neutral-900',
                )}
              >
                <span className="flex items-center gap-2">
                  <span>{p.display_name}</span>
                  {p.is_builtin && (
                    <span className="text-[10px] text-neutral-500 bg-neutral-800 px-1.5 py-0.5 rounded">
                      ビルトイン
                    </span>
                  )}
                </span>
                {active && <Check size={12} className="text-[var(--color-brand-500)]" />}
              </DropdownMenu.Item>
            )
          })}

          {/* Wave 2 follow-up: 「+ 新規追加」MenuItem */}
          <DropdownMenu.Separator className="my-1 h-px bg-neutral-800" />
          <DropdownMenu.Item
            onSelect={() => onCreateNew()}
            className="flex items-center gap-2 px-3 py-2 text-sm text-[var(--color-brand-500)] hover:bg-neutral-900 cursor-pointer outline-none"
          >
            <Plus size={14} />
            <span>新規追加</span>
          </DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  )
}

// ----- PT-05 Filler Removal (Radix Checkbox) -----

interface FillerRemovalCheckboxProps {
  checked: boolean
  disabled: boolean
  onChange: (v: boolean) => void
}

function FillerRemovalCheckbox({ checked, disabled, onChange }: FillerRemovalCheckboxProps) {
  return (
    <div className="space-y-1">
      <label className="flex items-center gap-2 cursor-pointer select-none">
        <Checkbox.Root
          checked={checked}
          onCheckedChange={(v) => onChange(v === true)}
          disabled={disabled}
          className={cn(
            'w-4 h-4 rounded border flex items-center justify-center transition-colors',
            checked
              ? 'bg-[var(--color-brand-500)] border-[var(--color-brand-500)]'
              : 'bg-neutral-900 border-neutral-700 hover:border-neutral-600',
            disabled && 'opacity-50',
          )}
        >
          <Checkbox.Indicator>
            <Check size={12} className="text-white" />
          </Checkbox.Indicator>
        </Checkbox.Root>
        <span className="text-sm text-neutral-100">フィラー (えー・あのー) を除去する</span>
      </label>
      <p className="text-xs text-neutral-500 ml-6">
        後処理層でフィラー語を自動削除します (Phase 3 で接続予定)
      </p>
    </div>
  )
}

// ----- PT-08 Emoji Density (Radix RadioGroup) -----

interface EmojiDensityRadioGroupProps {
  value: EmojiDensity
  disabled: boolean
  onChange: (v: EmojiDensity) => void
}

function EmojiDensityRadioGroup({ value, disabled, onChange }: EmojiDensityRadioGroupProps) {
  const options: EmojiDensity[] = ['none', 'few', 'normal', 'many']
  return (
    <div className="space-y-2">
      <div className="text-sm text-neutral-100">絵文字の頻度</div>
      <RadioGroup.Root
        value={value}
        onValueChange={(v) => onChange(v as EmojiDensity)}
        disabled={disabled}
        className="flex flex-wrap gap-2"
      >
        {options.map((opt) => {
          const active = value === opt
          return (
            <label
              key={opt}
              className={cn(
                'flex items-center gap-2 px-3 py-1.5 rounded-md border cursor-pointer select-none transition-colors',
                active
                  ? 'bg-[var(--color-brand-700)]/30 border-[var(--color-brand-500)] text-white'
                  : 'bg-neutral-900 border-neutral-800 text-neutral-300 hover:border-neutral-700',
                disabled && 'opacity-50 cursor-not-allowed',
              )}
            >
              <RadioGroup.Item
                value={opt}
                disabled={disabled}
                className={cn(
                  'w-3.5 h-3.5 rounded-full border flex items-center justify-center',
                  active
                    ? 'border-[var(--color-brand-500)]'
                    : 'border-neutral-600',
                )}
              >
                <RadioGroup.Indicator className="w-1.5 h-1.5 rounded-full bg-[var(--color-brand-500)]" />
              </RadioGroup.Item>
              <span className="text-xs">{EMOJI_DENSITY_LABELS[opt]}</span>
            </label>
          )
        })}
      </RadioGroup.Root>
      <p className="text-xs text-neutral-500">
        LLM 後処理で挿入する絵文字の量 (Phase 3 で接続予定)
      </p>
    </div>
  )
}

