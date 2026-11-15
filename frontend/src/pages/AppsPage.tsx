/* AppsPage.tsx — Phase 2 C Wave 3: AT-06 + AT-04 本実装
 *
 * 仕様: SSOT v2.10 §8.3.1 AT-06 / v1 AppMappingPage.ps1 互換
 * 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md §1.4
 *
 * 機能:
 *   - 3 列表 (プロファイル ID / アプリ名 / タイトル正規表現) インライン編集
 *   - 「+ 追加」ボタンで空行追加 (即 DB 保存はしない・onBlur で保存)
 *   - 「🗑️」ボタンで行削除確認 Dialog → 削除
 *   - onBlur (input/td focusout) で replaceAllAppRules 全件再構築保存
 *   - 完全空行は保存時にスキップ (Rust 側でも保険的にスキップ)
 *   - ステータス表示「最終保存: HH:mm:ss」または「保存エラー: ...」
 *   - AT-04 (Wave 3): 重複検出 (app_name + title_regex の組合せ重複でエラートースト)
 *
 * 既存パターン踏襲: DictionaryPage.tsx の sonner + Tailwind + lucide-react 構成。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Check, ChevronDown, Loader2, Plus, Trash2 } from 'lucide-react'
import * as Dialog from '@radix-ui/react-dialog'
import * as DropdownMenu from '@radix-ui/react-dropdown-menu'
import { toast } from 'sonner'
import {
  deleteAppRule,
  listAppRules,
  listProfiles,
  replaceAllAppRules,
  type Profile,
} from '../lib/tauri'
import { emptyAppRule, isEmptyRule, type AppRule } from '../types/app_rule'

/** クライアント側「未保存ローカル行」識別用の擬似キー。
 *  id が null の新規行を React key として一意化するためインクリメントしていく。 */
let localKeyCounter = 1

interface DraftRow extends AppRule {
  /** React key 専用の擬似ローカルキー (新規行が複数あっても識別できるよう常時付与) */
  _localKey: number
}

function withLocalKey(rule: AppRule): DraftRow {
  return { ...rule, _localKey: localKeyCounter++ }
}

/** v0.1.3: app_rules.profile_id 文字列 → Profile 解決
 *
 *  resolve_by_app_rule_key (Rust) と同じ規則をフロント側で再現する:
 *    1. builtin_key 集合に含まれれば builtin Profile を返す
 *    2. それ以外は parseInt して user rowid 検索
 *    3. どちらでもなければ null (= 未定義 / typo)
 */
function resolveProfile(profileId: string, profiles: Profile[]): Profile | null {
  if (profileId === '') return null
  const builtin = profiles.find(
    (p) => p.is_builtin && p.builtin_key === profileId,
  )
  if (builtin) return builtin
  const rowid = Number.parseInt(profileId, 10)
  if (!Number.isNaN(rowid)) {
    const user = profiles.find((p) => p.id === rowid)
    if (user) return user
  }
  return null
}

/** profile.id / builtin_key から app_rules.profile_id 文字列へ */
function profileToKey(p: Profile): string {
  if (p.is_builtin && p.builtin_key) return p.builtin_key
  if (p.id != null) return String(p.id)
  return ''
}

export function AppsPage() {
  const [rows, setRows] = useState<DraftRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [lastSavedAt, setLastSavedAt] = useState<Date | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)
  /** v0.1.3: DropdownMenu 用の全プロファイル (builtin 優先 → user 順) */
  const [profiles, setProfiles] = useState<Profile[]>([])
  /** 削除確認 Dialog 対象 (DB 保存済 id 持ち) */
  const [confirmingDelete, setConfirmingDelete] = useState<DraftRow | null>(null)

  /** onBlur のタイミング検出用 — Table 外にフォーカスが移ったら保存。
   *  setTimeout で 1 tick 遅らせて Table 内の別 input への focus 移動を保存対象外にする。 */
  const blurTimeoutRef = useRef<number | null>(null)

  const refetch = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      // v0.1.3: profiles も並行取得 (DropdownMenu 用)
      const [fetched, fetchedProfiles] = await Promise.all([
        listAppRules(),
        listProfiles().catch((e) => {
          console.warn('[apps] listProfiles failed (DropdownMenu fallback to typo display):', e)
          return [] as Profile[]
        }),
      ])
      setRows(fetched.map(withLocalKey))
      setProfiles(fetchedProfiles)
      setLastSavedAt(new Date())
      setSaveError(null)
    } catch (e) {
      const msg = `${e}`
      setError(msg)
      toast.error(`アプリ別ルールの読込に失敗: ${msg}`)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void (async () => {
      await refetch()
    })()
  }, [refetch])

  /** AT-04 (Wave 3): 重複検出
   *  app_name + title_regex の組合せが同一の行が 2 件以上あれば重複と判定。
   *  大文字小文字は無視 (app_name のみ)。
   *  重複があれば toast.error を表示して true を返す。なければ false。
   */
  function checkDuplicateRules(currentRows: DraftRow[]): boolean {
    const nonEmpty = currentRows.filter((r) => !isEmptyRule(r))
    const seen = new Map<string, number>() // key → _localKey
    for (const row of nonEmpty) {
      const key = `${row.app_name.trim().toLowerCase()}|${row.title_regex.trim()}`
      if (seen.has(key)) {
        toast.error('同じアプリルールが既に存在します')
        return true
      }
      seen.set(key, row._localKey)
    }
    return false
  }

  /** 全件再構築保存。空配列でも呼べる (Rust 側で全削除になる)。
   *
   *  バグ B 対策 (2026-05-25): 旧実装は `setRows(inserted.map(withLocalKey))` で state を
   *  丸ごと置換していたが、これだと saveAll の await 中にユーザーが追加した行
   *  (空行や入力途中の値) が消失する race があった (Codex PR #14 P2 残課題)。
   *
   *  新実装は _localKey ベースの順序非依存 merge 戦略:
   *    - payload 生成時に payloadKeys[i] = cur 非空行 i の _localKey を記録
   *    - inserted 取得後、insertedByLocalKey Map を構築 (_localKey → AppRule)
   *    - 空行: id == null (in-flight 新規空行) → 維持 / id != null (空化された既存) → 除去
   *    - 非空行: insertedByLocalKey に該当があれば id/created_at/updated_at のみ反映、
   *             profile_id/app_name/title_regex は cur (= in-flight 編集) を維持
   *    - insertedByLocalKey に該当なしの非空行 = saveAll 後 in-flight 追加行 → そのまま維持
   *
   *  これにより以下のレースをすべて解消:
   *    1. handleAddRow が saveAll 中に走っても新空行は消えない
   *    2. in-flight 中の編集が stale な inserted で上書きされない
   *    3. in-flight 中の空化で payload と inserted の順序がずれても id 誤付与が起きない
   *    4. 既存保存済を空にして blur した場合は state からも除去され DB と整合
   */
  const saveAll = useCallback(async (currentRows: DraftRow[]) => {
    // AT-04: 重複検出 (保存前にクライアント側でブロック)
    if (checkDuplicateRules(currentRows)) return

    setSaving(true)
    setSaveError(null)
    try {
      // 完全空行はクライアント側で事前除外 (Rust 側でも保険的にスキップ)
      //   payload の i 番目がどの cur._localKey に対応するかを payloadKeys に記録する
      //   (Codex P2: in-flight 中に空化された行があると順序が崩れて id ずれを起こす対策)。
      const payloadKeys: number[] = []
      const payload: AppRule[] = []
      currentRows.forEach((r) => {
        if (!isEmptyRule(r)) {
          payloadKeys.push(r._localKey)
          const { _localKey: _k, ...rest } = r
          payload.push(rest)
        }
      })
      const inserted = await replaceAllAppRules(payload)
      // _localKey → inserted 行の Map を構築 (順序非依存 join 用)
      const insertedByLocalKey = new Map<number, AppRule>()
      payloadKeys.forEach((lk, i) => {
        const ins = inserted[i]
        if (ins != null) insertedByLocalKey.set(lk, ins)
      })
      // merge 戦略:
      //   - 空行: id == null → state 維持 / id != null → state からも除去 (DB と整合)
      //   - 非空行: insertedByLocalKey に該当があれば id/created_at/updated_at を反映。
      //            ユーザー編集フィールドは cur を維持 (in-flight 編集保護)。
      //            該当なし = saveAll 後に追加された in-flight 行 → そのまま維持。
      setRows((cur) =>
        cur.flatMap<DraftRow>((row) => {
          if (isEmptyRule(row)) {
            return row.id == null ? [row] : []
          }
          const ins = insertedByLocalKey.get(row._localKey)
          if (ins == null) return [row]
          return [
            {
              // 編集フィールドは cur を維持
              profile_id: row.profile_id,
              app_name: row.app_name,
              title_regex: row.title_regex,
              // DB メタデータは inserted を反映
              id: ins.id,
              created_at: ins.created_at,
              updated_at: ins.updated_at,
              _localKey: row._localKey,
            },
          ]
        }),
      )
      setLastSavedAt(new Date())
    } catch (e) {
      const msg = `${e}`
      setSaveError(msg)
      toast.error(`保存失敗: ${msg}`)
    } finally {
      setSaving(false)
    }
  }, [])

  // 最新 rows を ref で保持 (unmount フラッシュ用)
  const rowsRef = useRef<DraftRow[]>(rows)
  rowsRef.current = rows

  // unmount 時に保留中の保存をフラッシュ (Codex P2 対応・v1 SaveBeforeClose 互換)
  // 保留タイマがあれば即座に saveAll を呼んで未保存編集を取りこぼさない。
  // ※ React unmount は同期実行されるため saveAll Promise は fire-and-forget。
  //   実際の DB 書込は Tauri バックエンドで継続するため、画面を離れた後も保存は完了する。
  useEffect(() => {
    return () => {
      if (blurTimeoutRef.current != null) {
        window.clearTimeout(blurTimeoutRef.current)
        blurTimeoutRef.current = null
        // 保留中編集をフラッシュ (fire-and-forget)
        void saveAll(rowsRef.current)
      }
    }
  }, [saveAll])

  /** 削除確認 Dialog open 中は blur 保存を抑止する ref。
   *  Codex P1: trash button → Dialog open で focus 抜けが起きる際の race を回避。
   *  ref にしているのは state 更新タイミングと blur 発火タイミングがほぼ同時のため
   *  (closure cap 経由だと古い値を見る恐れがある)。 */
  const suppressBlurSaveRef = useRef(false)

  /** Table 外にフォーカスが移った時の保存トリガ。
   *  setTimeout(100) で Table 内別セルへの focus 遷移なら即キャンセル。
   *  Codex P1: 削除確認 Dialog open 中は保存をスキップ (race 回避)。 */
  function handleTableBlur() {
    if (blurTimeoutRef.current != null) {
      window.clearTimeout(blurTimeoutRef.current)
    }
    // 現在の rows を snapshot キャプチャ
    blurTimeoutRef.current = window.setTimeout(() => {
      blurTimeoutRef.current = null
      if (suppressBlurSaveRef.current) {
        // Dialog open 中は保存スキップ
        return
      }
      // 状態スナップショットを functional updater 内で取り出して保存
      setRows((current) => {
        void saveAll(current)
        return current
      })
    }, 100)
  }

  function handleTableFocus() {
    // 別セルにフォーカスが移ったら保存タイマをキャンセル
    if (blurTimeoutRef.current != null) {
      window.clearTimeout(blurTimeoutRef.current)
      blurTimeoutRef.current = null
    }
  }

  function handleCellChange(
    localKey: number,
    field: 'profile_id' | 'app_name' | 'title_regex',
    value: string,
  ) {
    setRows((cur) =>
      cur.map((r) => (r._localKey === localKey ? { ...r, [field]: value } : r)),
    )
  }

  function handleAddRow() {
    // バグ A 対策 (2026-05-25): 末尾が既に完全空行なら追加しない (連続クリック空行無制限増殖防止)。
    const last = rowsRef.current[rowsRef.current.length - 1]
    if (last && isEmptyRule(last)) {
      toast.info('空の行が既にあります')
      return
    }
    // バグ B 対策 (2026-05-25):
    //   旧実装は「保留 blurTimer があれば await saveAll → 完了後に空行 push」だったが、
    //   await 中に発生した onChange (= 入力途中の値) が saveAll 内 setRows(inserted) で
    //   上書きされ消失する race があった。
    //   入力途中の値は既に state に保持されているため、handleAddRow は純粋に state 操作のみ
    //   行えば十分 (純 setRows + 空行追加)。
    //
    //   saveAll は merge 戦略 (上記 saveAll コメント参照) で in-flight 編集と空行を保護するため、
    //   保留 blurTimer はそのまま維持してよい:
    //     - 100ms 後に blur save 発火 → 既存編集 (= 他行) が永続化される (Codex P2 永続化保証)
    //     - 空行と in-flight 編集は merge で cur に残る (旧 Codex P2 #5 退行も発生しない)
    setRows((cur) => [...cur, withLocalKey(emptyAppRule())])
  }

  async function handleDeleteConfirmed() {
    const target = confirmingDelete
    if (target == null) {
      setConfirmingDelete(null)
      suppressBlurSaveRef.current = false
      return
    }
    setConfirmingDelete(null)
    // Codex P2: Radix onOpenChange は controlled prop の null セット時には
    //   発火しない場合がある。明示的に抑止フラグを解除する。
    suppressBlurSaveRef.current = false
    // 未保存ローカル行 (id == null) はクライアント削除のみで終わり
    if (target.id == null) {
      setRows((cur) => cur.filter((r) => r._localKey !== target._localKey))
      return
    }
    // DB 保存済: 楽観的更新 → delete → 失敗時 refetch
    const id = target.id
    const prev = rows
    setRows((cur) => cur.filter((r) => r._localKey !== target._localKey))
    try {
      await deleteAppRule(id)
      toast.success('削除しました')
      setLastSavedAt(new Date())
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
      setRows(prev)
      void refetch()
    }
  }

  return (
    <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
      <header className="mb-6">
        <h2 className="text-xl font-semibold">アプリ別ルール</h2>
        <p className="text-sm text-neutral-500 mt-1">
          アプリに応じて自動でプロファイルを切り替えます · {rows.length} 件
          {lastSavedAt && !saveError && (
            <span className="ml-3 text-neutral-600">
              最終保存: {lastSavedAt.toLocaleTimeString('ja-JP')}
            </span>
          )}
          {saving && (
            <span className="ml-3 text-neutral-600 inline-flex items-center gap-1">
              <Loader2 size={11} className="animate-spin" />
              保存中…
            </span>
          )}
        </p>
      </header>

      {/* エラー (読込失敗) */}
      {error && !loading && (
        <div className="max-w-4xl mb-4 px-4 py-3 bg-red-950/50 border border-red-900/50 rounded-lg text-sm text-red-300">
          アプリ別ルールの読込に失敗しました: {error}
          <button
            type="button"
            onClick={() => void refetch()}
            className="ml-3 px-2 py-1 bg-red-900/50 hover:bg-red-900 text-xs rounded"
          >
            再試行
          </button>
        </div>
      )}

      {/* エラー (保存失敗) */}
      {saveError && (
        <div className="max-w-4xl mb-4 px-4 py-3 bg-red-950/50 border border-red-900/50 rounded-lg text-sm text-red-300">
          保存エラー: {saveError}
        </div>
      )}

      {/* ツールバー (追加ボタン) */}
      <div className="max-w-4xl mb-3 flex items-center justify-end">
        <button
          type="button"
          onClick={handleAddRow}
          className="flex items-center gap-2 px-4 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-sm rounded-md transition-colors"
        >
          <Plus size={14} />
          <span>追加</span>
        </button>
      </div>

      {/* テーブル */}
      <div
        className="max-w-4xl border border-neutral-800 rounded-lg overflow-hidden bg-neutral-950"
        onBlur={handleTableBlur}
        onFocus={handleTableFocus}
      >
        {loading && rows.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-neutral-500 flex items-center justify-center gap-2">
            <Loader2 size={14} className="animate-spin" />
            <span>読み込み中…</span>
          </div>
        ) : rows.length === 0 ? (
          <div className="px-4 py-12 text-center text-sm text-neutral-500">
            アプリ別ルールがまだありません。「+ 追加」から登録してください。
          </div>
        ) : (
          <table className="w-full text-sm border-collapse table-fixed">
            <thead className="bg-neutral-900 text-neutral-400">
              <tr>
                <th className="w-[28%] text-left px-3 py-2 font-medium border-b border-neutral-800">
                  プロファイル ID
                </th>
                <th className="w-[28%] text-left px-3 py-2 font-medium border-b border-neutral-800">
                  アプリ名
                </th>
                <th className="w-[36%] text-left px-3 py-2 font-medium border-b border-neutral-800">
                  タイトル正規表現
                </th>
                <th className="w-[8%] text-left px-2 py-2 border-b border-neutral-800"></th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row._localKey}
                  className="border-b border-neutral-800 hover:bg-neutral-900/40 transition-colors"
                >
                  <td className="px-3 py-1.5">
                    <ProfilePickerCell
                      value={row.profile_id}
                      profiles={profiles}
                      onChange={(newKey) => {
                        handleCellChange(row._localKey, 'profile_id', newKey)
                        // DropdownMenu 選択時は blur イベントが発火しないため
                        // 明示的に saveAll を予約 (テーブル外 click と同じタイマー方式)
                        if (blurTimeoutRef.current != null) {
                          window.clearTimeout(blurTimeoutRef.current)
                        }
                        blurTimeoutRef.current = window.setTimeout(() => {
                          blurTimeoutRef.current = null
                          if (suppressBlurSaveRef.current) return
                          setRows((current) => {
                            void saveAll(current)
                            return current
                          })
                        }, 100)
                      }}
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      type="text"
                      value={row.app_name}
                      onChange={(e) =>
                        handleCellChange(row._localKey, 'app_name', e.target.value)
                      }
                      placeholder="chrome.exe"
                      className="w-full bg-transparent border border-transparent hover:border-neutral-800 focus:border-neutral-600 rounded px-2 py-1 text-sm text-neutral-100 outline-none placeholder:text-neutral-700"
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      type="text"
                      value={row.title_regex}
                      onChange={(e) =>
                        handleCellChange(row._localKey, 'title_regex', e.target.value)
                      }
                      placeholder=".*メール.*"
                      className="w-full bg-transparent border border-transparent hover:border-neutral-800 focus:border-neutral-600 rounded px-2 py-1 text-sm text-neutral-100 outline-none placeholder:text-neutral-700 font-mono"
                    />
                  </td>
                  <td className="px-2 py-1.5 text-right">
                    <button
                      type="button"
                      onClick={() => {
                        // Codex P1/P2: dialog open 前の処理 (2 段階)
                        //   1) 保留 blur タイマがあれば「他行の編集」を flush 保存
                        //      (trash クリックで未保存編集を失わないため)
                        //   2) 抑止フラグ ON にしてその後の blur save 競合を防ぐ
                        if (blurTimeoutRef.current != null) {
                          window.clearTimeout(blurTimeoutRef.current)
                          blurTimeoutRef.current = null
                          // 削除対象 row を除いた現在 rows を保存 (delete と save が重複しないように
                          // 削除前 snapshot に target を含めない形で flush)
                          const snapshot = rowsRef.current.filter(
                            (r) => r._localKey !== row._localKey,
                          )
                          // fire-and-forget で OK (handleDeleteConfirmed が単発 deleteAppRule を発行する想定)
                          void saveAll(snapshot)
                        }
                        suppressBlurSaveRef.current = true
                        setConfirmingDelete(row)
                      }}
                      className="p-1.5 hover:bg-red-950/50 rounded text-neutral-500 hover:text-red-400"
                      aria-label="削除"
                    >
                      <Trash2 size={14} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <p className="max-w-4xl mt-3 text-xs text-neutral-600">
        ・編集後にテーブル外をクリックすると自動保存されます (v1 互換)
        <br />
        ・完全空行 (3 列とも空白) は保存時に自動で削除されます
        <br />
        ・タイトル正規表現は空欄可 (空欄ならアプリ名のみで一致)
      </p>

      {/* 削除確認 Dialog */}
      <Dialog.Root
        open={confirmingDelete != null}
        onOpenChange={(open) => {
          if (!open) {
            setConfirmingDelete(null)
            // Codex P1: Dialog 閉じたら blur save 抑止を解除
            suppressBlurSaveRef.current = false
          }
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className="fixed inset-0 bg-black/60" />
          <Dialog.Content className="fixed top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[420px] bg-neutral-950 border border-neutral-800 rounded-lg p-5 shadow-2xl">
            <Dialog.Title className="text-sm font-semibold text-white">
              アプリ別ルールを削除しますか?
            </Dialog.Title>
            <Dialog.Description className="mt-2 text-xs text-neutral-400">
              {confirmingDelete && (
                <>
                  プロファイル「
                  <span className="text-neutral-200">
                    {confirmingDelete.profile_id || '(空)'}
                  </span>
                  」/ アプリ「
                  <span className="text-neutral-200">
                    {confirmingDelete.app_name || '(空)'}
                  </span>
                  」 のルールを削除します。この操作は取り消せません。
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
    </div>
  )
}

/** v0.1.3: profile 列セル用 DropdownMenu。
 *
 *  - 選択肢: 全プロファイル (builtin → user 順)
 *  - 「素出力」(raw) は display_name 「— 素出力 —」がそのまま表示される
 *  - typo 値 (未定義 profile_id) は赤背景で「⚠️ 未定義: <値>」表示
 *  - 空 ("") の場合は灰色で「未選択」表示
 */
function ProfilePickerCell({
  value,
  profiles,
  onChange,
}: {
  value: string
  profiles: Profile[]
  onChange: (newKey: string) => void
}) {
  const resolved = useMemo(() => resolveProfile(value, profiles), [value, profiles])

  // 表示テキストとスタイル決定
  let label: string
  let labelCls: string
  if (value === '') {
    label = '未選択'
    labelCls = 'text-neutral-600 italic'
  } else if (resolved) {
    label = resolved.display_name
    labelCls = 'text-neutral-100'
  } else {
    label = `⚠️ 未定義: ${value}`
    labelCls = 'text-red-300 bg-red-950/30 rounded px-1'
  }

  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          className="w-full flex items-center justify-between gap-1 bg-transparent border border-transparent hover:border-neutral-800 focus:border-neutral-600 rounded px-2 py-1 text-sm outline-none text-left"
        >
          <span className={`truncate ${labelCls}`}>{label}</span>
          <ChevronDown size={12} className="text-neutral-500 shrink-0" />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="start"
          sideOffset={4}
          className="min-w-[220px] bg-neutral-950 border border-neutral-800 rounded-md shadow-xl p-1 z-50"
        >
          {profiles.length === 0 ? (
            <div className="px-3 py-2 text-xs text-neutral-600">
              プロファイル未読込
            </div>
          ) : (
            profiles.map((p) => {
              const key = profileToKey(p)
              const selected = key === value && resolved != null
              return (
                <DropdownMenu.Item
                  key={`${p.is_builtin ? 'b' : 'u'}-${p.id ?? p.builtin_key ?? key}`}
                  onSelect={() => onChange(key)}
                  className="flex items-center gap-2 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800 focus:bg-neutral-800 outline-none rounded cursor-pointer"
                >
                  {selected ? (
                    <Check size={12} className="text-emerald-400 shrink-0" />
                  ) : (
                    <span className="w-3 shrink-0" />
                  )}
                  <span className="flex-1 truncate">{p.display_name}</span>
                  {p.is_builtin && (
                    <span className="text-[10px] text-neutral-500 shrink-0">builtin</span>
                  )}
                </DropdownMenu.Item>
              )
            })
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  )
}
