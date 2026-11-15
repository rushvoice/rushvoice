import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { AlertTriangle, Lock, Save, Zap, Upload, Download, Trash2, Check, Keyboard, Pencil, Power, Database, Eye, EyeOff, Mic } from 'lucide-react'
import * as Checkbox from '@radix-ui/react-checkbox'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { toast } from 'sonner'
import {
  getSetting,
  setSetting,
  listInputDevices,
  getMicStandby,
  setMicStandby,
  applyMicStandby,
  saveApiKey,
  hasApiKey,
  deleteApiKey,
  testApiKey,
  testStoredApiKey,
  testEndpointUrl,
  TRANSCRIBE_PROVIDER_KEY,
  CUSTOM_TRANSCRIBE_BASE_URL_KEY,
  CUSTOM_TRANSCRIBE_MODEL_KEY,
  LLM_PROVIDER_KEY,
  CUSTOM_LLM_BASE_URL_KEY,
  CUSTOM_LLM_MODEL_KEY,
  GROQ_LLM_MODEL_KEY,
  OPENAI_LLM_MODEL_KEY,
  getHotkeyConfig,
  setHotkeyConfig,
  getHotkeyConflictState,
  isAutostartEnabled,
  enableAutostart,
  disableAutostart,
  historyCount,
  clearHistory,
  getCongestionConfig,
  setCongestionConfig,
  type Provider,
  type HotkeyConfig,
  type HotkeyMode,
  type CongestionConfig,
} from '../lib/tauri'
import { GatewayRetiredNotice } from '../components/GatewayRetiredNotice'
import { cn } from '../lib/cn'

export function SettingsPage() {
  return (
    <div className="flex-1 bg-neutral-900 text-white p-8 overflow-y-auto">
      <header className="mb-6">
        <h2 className="text-xl font-semibold">設定</h2>
        <p className="text-sm text-neutral-500 mt-1">
          ホットキー・認識設定・接続先と API キー
        </p>
      </header>

      <section className="space-y-6 max-w-2xl">
        {/* 2026-11-15: 旧版の有料アカウント終了の案内 (旧版でログインしていた人にだけ出る) */}
        <GatewayRetiredNotice />

        {/* ホットキー (頻繁に変更するため最上位・要望 2026-05-21) */}
        <HotkeySection />

        {/* Day 9 AUT-01: Windows スタートアップ自動起動 */}
        <AutostartSection />

        {/* Phase 2 D: 履歴管理 */}
        <HistorySection />

        {/* 認識設定 */}
        <div className="space-y-4 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
          <h3 className="text-sm font-medium text-neutral-200">認識設定</h3>

          <div>
            <label className="block text-xs text-neutral-400 mb-1.5">言語</label>
            <select
              className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm disabled:opacity-60"
              disabled
            >
              <option>日本語 (ja) - 固定</option>
            </select>
          </div>
        </div>

        {/* マイク選択 (2026-07-23 要望: Shokz併用時にGUIで即切替) */}
        <MicSection />

        {/* v0.1.10 機能 A: 通知バナー (混雑時の安心バナー閾値) */}
        <CongestionSection />

        {/* 接続先と API キー (一度設定したら触らないので下に・要望 2026-05-21)
            2026-11-15: 有料版のログインを廃止し、自分のキー・OpenAI 互換の任意の接続先 (ローカル LLM 可) にした */}
        <EndpointSection />

        {/* 辞書 (Stage 2 以降) */}
        <div className="space-y-3 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
          <h3 className="text-sm font-medium text-neutral-200">辞書ファイル</h3>
          <p className="text-xs text-neutral-500">
            CSV 形式（pattern, replacement, is_regex）でインポート/エクスポート
          </p>
          <p className="text-xs text-amber-400/70">
            ※ Stage 2 で実装予定（現在 UI モックのみ）
          </p>
          <div className="flex gap-2">
            <button
              type="button"
              disabled
              className="flex items-center gap-2 px-3 py-2 bg-neutral-800 text-neutral-300 text-xs rounded-md disabled:opacity-50"
            >
              <Upload size={12} />
              <span>CSV インポート</span>
            </button>
            <button
              type="button"
              disabled
              className="flex items-center gap-2 px-3 py-2 bg-neutral-800 text-neutral-300 text-xs rounded-md disabled:opacity-50"
            >
              <Download size={12} />
              <span>CSV エクスポート</span>
            </button>
          </div>
        </div>
      </section>

      <footer className="mt-12 pt-6 border-t border-neutral-800 text-xs text-neutral-600 max-w-2xl">
        <p>RushVoice v{__APP_VERSION__}</p>
        <p className="mt-1">
          © 2026 Hollywood Eyelash
        </p>
      </footer>
    </div>
  )
}

interface ApiKeyRowProps {
  label: string
  placeholder: string
  value: string
  onChange: (v: string) => void
  stored: boolean
  busy: boolean
  onSave: () => void
  onTest: () => void
  onDelete: () => void
  /** カスタム接続先 (ローカルサーバー) はキー無しでも接続テストできる */
  allowTestWithoutKey?: boolean
}

// ----- Stage 2-C-1: ホットキー設定セクション -----

const MOD_ORDER = [
  'Control', 'ControlLeft', 'ControlRight',
  'Shift', 'ShiftLeft', 'ShiftRight',
  'Alt', 'AltLeft', 'AltRight',
  'Meta', 'MetaLeft', 'MetaRight',
] as const
const MOD_LABEL_JA: Record<string, string> = {
  // 総称 (左右どちらでも反応) — mac/hotkey-cmd-either-side 2026-06-15 推奨デフォルト
  Control: 'Ctrl',
  Shift: 'Shift',
  Alt: 'Alt',
  Meta: 'Cmd/Win',
  // 左右指定 (従来・後方互換)
  ControlLeft: '左Ctrl',
  ControlRight: '右Ctrl',
  ShiftLeft: '左Shift',
  ShiftRight: '右Shift',
  AltLeft: '左Alt',
  AltRight: '右Alt',
  MetaLeft: '左Cmd',
  MetaRight: '右Cmd',
}
const KEY_LABEL_JA: Record<string, string> = {
  NonConvert: '無変換',
  Convert: '変換',
  KanaMode: 'カナ',
  Lang1: 'かな',
  Lang2: '英数',
  Space: 'Space',
  Enter: 'Enter',
  Escape: 'Esc',
  Tab: 'Tab',
  Backspace: 'BS',
  Delete: 'Del',
  CapsLock: 'CapsLock',
  // 修飾キー単独 (メインキーとして) 表示
  ControlLeft: '左Ctrl',
  ControlRight: '右Ctrl',
  ShiftLeft: '左Shift',
  ShiftRight: '右Shift',
  AltLeft: '左Alt',
  AltRight: '右Alt',
  MetaLeft: '左Win',
  MetaRight: '右Win',
  // mac/hotkey-cmd-either-side (2026-06-15): 修飾キー単独・左右どちらでもマッチ。
  // マウスのカスタムキー (Logitech MX Master 等) は左右指定不能なため、こちらが既定。
  Control: 'Ctrl (左右どちらでも)',
  Shift: 'Shift (左右どちらでも)',
  Alt: 'Alt (左右どちらでも)',
  Meta: 'Win/Cmd (左右どちらでも)',
}

function formatHotkey(c: HotkeyConfig): string {
  const sorted = [...c.modifiers].sort(
    (a, b) =>
      MOD_ORDER.indexOf(a as (typeof MOD_ORDER)[number]) -
      MOD_ORDER.indexOf(b as (typeof MOD_ORDER)[number]),
  )
  const mods = sorted.map((m) => MOD_LABEL_JA[m] ?? m)
  const key =
    KEY_LABEL_JA[c.code] ??
    c.code.replace(/^Key/, '').replace(/^Digit/, '').replace(/^Arrow/, '')
  return [...mods, key].join(' + ')
}

type Side = 'Left' | 'Right'
type HeldMods = {
  control?: Side
  shift?: Side
  alt?: Side
  meta?: Side
}

/// 修飾キー e.code から (kind, side) を判定
function parseModifierCode(code: string): { kind: keyof HeldMods; side: Side } | null {
  if (code === 'ControlLeft') return { kind: 'control', side: 'Left' }
  if (code === 'ControlRight') return { kind: 'control', side: 'Right' }
  if (code === 'ShiftLeft') return { kind: 'shift', side: 'Left' }
  if (code === 'ShiftRight') return { kind: 'shift', side: 'Right' }
  if (code === 'AltLeft') return { kind: 'alt', side: 'Left' }
  if (code === 'AltRight') return { kind: 'alt', side: 'Right' }
  if (code === 'MetaLeft') return { kind: 'meta', side: 'Left' }
  if (code === 'MetaRight') return { kind: 'meta', side: 'Right' }
  return null
}

function hotkeyEqual(a: HotkeyConfig, b: HotkeyConfig): boolean {
  if (a.code !== b.code) return false
  if (a.mode !== b.mode) return false
  const sa = [...a.modifiers].sort()
  const sb = [...b.modifiers].sort()
  if (sa.length !== sb.length) return false
  return sa.every((v, i) => v === sb[i])
}

function HotkeySection() {
  const [stored, setStored] = useState<HotkeyConfig | null>(null)
  const [draft, setDraft] = useState<HotkeyConfig | null>(null)
  const [capturing, setCapturing] = useState(false)
  const [saving, setSaving] = useState(false)
  // v0.1.10 機能 D: ホットキー競合状態 (赤バッジ表示制御)
  const [conflict, setConflict] = useState(false)
  // Codex P2 #1 (2026-05-21): キャプチャ開始時に div にフォーカスを移動して
  // onKeyDown が確実に発火するようにする (「変更」ボタンに焦点が残るのを防ぐ)。
  const captureRef = useRef<HTMLDivElement>(null)
  // 修飾キーの左右情報を保持 (要望 2026-05-21: 右Ctrl/右Alt/右Shift 対応)
  // keyup や capture 終了で reset
  const heldModsRef = useRef<HeldMods>({})

  useEffect(() => {
    (async () => {
      try {
        const c = await getHotkeyConfig()
        setStored(c)
        setDraft(c)
      } catch (e) {
        toast.error(`ホットキー読込失敗: ${e}`)
      }
      // v0.1.10 機能 D: 初期表示時に保存済みの競合状態を反映
      try {
        const cs = await getHotkeyConflictState()
        setConflict(cs.kind === 'Conflict')
      } catch (e) {
        console.warn('getHotkeyConflictState failed:', e)
      }
    })()
  }, [])

  // v0.1.10 機能 D: 競合状態変化イベントを listen して赤バッジを即時更新
  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen('hotkey:conflict-detected', () => {
            if (mounted) setConflict(true)
          }),
          await listen('hotkey:conflict-cleared', () => {
            if (mounted) setConflict(false)
          }),
        )
      } catch (err) {
        console.warn('HotkeySection conflict listen failed:', err)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  // capturing = true になった次の render で div にフォーカス
  useEffect(() => {
    if (capturing) {
      captureRef.current?.focus()
    }
  }, [capturing])

  function handleKeyCapture(e: ReactKeyboardEvent<HTMLDivElement>) {
    if (!capturing || !draft) return
    e.preventDefault()
    e.stopPropagation()

    // 修飾キー単独押下: 左右を ref に保持してこのキーでは確定しない
    const modInfo = parseModifierCode(e.code)
    if (modInfo) {
      heldModsRef.current[modInfo.kind] = modInfo.side
      return
    }

    // メインキー押下: コンビネーション確定。
    //
    // mac/hotkey-cmd-either-side (2026-06-15):
    // 修飾キー側もデフォルトで「左右どちらでも」(総称 'Control'/'Shift'/'Alt'/'Meta')
    // を保存するよう変更。理由は handleKeyUp と同じ:
    //   - マウスのカスタムボタン (Logi Options+ で "⌘Cmd + F12" 等) は左右指定不能
    //   - 物理キーボードでも、左右をユーザーが意識して使い分けるケースは稀
    //   - 厳格な左右指定が必要なら `ControlLeft` 等を手動で設定可 (API は維持)
    const mods: string[] = []
    if (e.ctrlKey) mods.push('Control')
    if (e.shiftKey) mods.push('Shift')
    if (e.altKey) mods.push('Alt')
    if (e.metaKey) mods.push('Meta')

    setDraft({ code: e.code, modifiers: mods, mode: draft.mode })
    setCapturing(false)
    heldModsRef.current = {} // 次回キャプチャ用に reset
  }

  // 要望 2026-05-21: 修飾キー単独 (右Ctrl 単体等) をホットキーとして使う
  // → 修飾キーを離した瞬間に「他のキー一切押されていない」なら単独確定
  //
  // mac/hotkey-cmd-either-side (2026-06-15): デフォルトで「左右どちらでも反応」する
  // 総称コード ('Meta', 'Control' 等) を保存する。理由:
  //   - マウスのカスタムキー割当 (Logitech MX Master 等) は Cmd を送る際に左右を区別できず、
  //     かつ macOS は受け取り側で左右を識別できない (CGEventFlagCommand は両方共通) ため、
  //     左/右指定だと「片側だけ反応・片側だけ無反応」になる。
  //   - 物理キーボードでホットキーを使うユーザーも、わざわざ「右だけ」「左だけ」と
  //     使い分けるケースは稀。両側受け付ける方が UX として自然。
  //   - 左/右厳格な指定が必要なら 'MetaLeft' / 'MetaRight' を手動で設定可 (現状の API は維持)。
  function handleKeyUp(e: ReactKeyboardEvent<HTMLDivElement>) {
    if (!capturing || !draft) return
    const modInfo = parseModifierCode(e.code)
    if (!modInfo) return
    // 他の修飾キーやメインキーが押されていなければ単独確定
    if (e.ctrlKey || e.shiftKey || e.altKey || e.metaKey) return
    // kind: 'control' | 'shift' | 'alt' | 'meta' → 'Control' | 'Shift' | 'Alt' | 'Meta'
    const code = modInfo.kind.charAt(0).toUpperCase() + modInfo.kind.slice(1)
    setDraft({ code, modifiers: [], mode: draft.mode })
    setCapturing(false)
    heldModsRef.current = {}
  }

  function handleModeChange(mode: HotkeyMode) {
    if (!draft) return
    setDraft({ ...draft, mode })
  }

  async function handleSave() {
    if (!draft) return
    setSaving(true)
    try {
      await setHotkeyConfig(draft)
      setStored(draft)
      toast.success(`ホットキーを ${formatHotkey(draft)} に変更しました`)
    } catch (e) {
      toast.error(`ホットキー保存失敗: ${e}`)
    } finally {
      setSaving(false)
    }
  }

  function handleRevert() {
    if (stored) setDraft(stored)
    setCapturing(false)
  }

  const dirty = !!stored && !!draft && !hotkeyEqual(stored, draft)

  return (
    <div
      className={cn(
        'space-y-4 p-5 bg-neutral-950 border rounded-lg',
        conflict ? 'border-red-500' : 'border-neutral-800',
      )}
    >
      <div className="flex items-center gap-2">
        <Keyboard size={14} className="text-neutral-400" />
        <h3 className="text-sm font-medium text-neutral-200">グローバルホットキー</h3>
        {conflict && (
          <span
            role="status"
            className="ml-auto inline-flex items-center gap-1 px-2 py-0.5 text-[10px] font-medium text-red-100 bg-red-500/20 border border-red-500/50 rounded"
          >
            <AlertTriangle size={10} />
            <span>衝突中</span>
          </span>
        )}
      </div>
      <p className="text-xs text-neutral-500">
        アプリ非アクティブ時もキーボードショートカットで録音操作できます。デフォルトは Ctrl + Shift + 無変換。
      </p>
      {conflict && (
        <p className="text-xs text-red-300/90 bg-red-500/5 border border-red-500/30 rounded px-2.5 py-1.5">
          現在のホットキーは他のアプリ / OS と競合しています。別の組合せに変更してください。
        </p>
      )}

      <div>
        <label className="block text-xs text-neutral-400 mb-1.5">キー</label>
        <div className="flex gap-2 items-stretch">
          <div
            ref={captureRef}
            role="button"
            tabIndex={0}
            onKeyDown={handleKeyCapture}
            onKeyUp={handleKeyUp}
            onBlur={() => setCapturing(false)}
            className={cn(
              'flex-1 px-3 py-2 bg-neutral-900 border rounded-md text-sm font-mono select-none',
              capturing
                ? 'border-amber-500 outline-none ring-1 ring-amber-500/30'
                : 'border-neutral-800',
            )}
          >
            {capturing ? (
              <span className="text-amber-400 italic">キーを押してください…（修飾キーは押下中の状態で）</span>
            ) : draft ? (
              <span className="text-neutral-100">{formatHotkey(draft)}</span>
            ) : (
              <span className="text-neutral-600 italic">読込中…</span>
            )}
          </div>
          <button
            type="button"
            onClick={() => setCapturing((c) => !c)}
            disabled={saving || !draft}
            className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
          >
            <Pencil size={12} />
            <span>{capturing ? 'キャンセル' : '変更'}</span>
          </button>
        </div>
      </div>

      <div>
        <label className="block text-xs text-neutral-400 mb-1.5">モード</label>
        <div className="space-y-1.5">
          <label className="flex items-center gap-2 text-sm text-neutral-200 cursor-pointer">
            <input
              type="radio"
              name="hotkey-mode"
              value="toggle"
              checked={draft?.mode === 'toggle'}
              onChange={() => handleModeChange('toggle')}
              disabled={saving || !draft}
              className="accent-[var(--color-brand-500)]"
            />
            <span>トグル（押すたびに録音 ON / OFF）</span>
          </label>
          <label className="flex items-center gap-2 text-sm text-neutral-200 cursor-pointer">
            <input
              type="radio"
              name="hotkey-mode"
              value="pushtotalk"
              checked={draft?.mode === 'pushtotalk'}
              onChange={() => handleModeChange('pushtotalk')}
              disabled={saving || !draft}
              className="accent-[var(--color-brand-500)]"
            />
            <span>押下中のみ録音（Push-to-Talk）</span>
          </label>
        </div>
      </div>

      <div className="flex gap-2">
        <button
          type="button"
          onClick={handleSave}
          disabled={!dirty || saving}
          className="px-4 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
        >
          <Save size={12} />
          <span>ホットキーを保存</span>
        </button>
        <button
          type="button"
          onClick={handleRevert}
          disabled={!dirty || saving}
          className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded-md disabled:opacity-50"
          title={dirty ? '保存前の値に戻す' : '変更がないため無効'}
        >
          元に戻す
        </button>
      </div>
    </div>
  )
}

// ----- Day 9 AUT-01: Windows スタートアップ自動起動 -----
//
// HKCU\Software\Microsoft\Windows\CurrentVersion\Run\rushvoice に登録/解除する。
// tauri-plugin-autostart 2.x (auto-launch crate) が裏で扱う。
// 初回起動時は OFF (判断 2026-05-23) — トグル ON で登録。
//
// 注意: enable() / disable() 中に何度か呼ばれても idempotent なので連打 OK。

function AutostartSection() {
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    (async () => {
      try {
        const e = await isAutostartEnabled()
        setEnabled(e)
      } catch (e) {
        toast.error(`自動起動状態の取得に失敗: ${e}`)
        setEnabled(false)
      }
    })()
  }, [])

  async function handleToggle() {
    if (enabled === null || busy) return
    setBusy(true)
    const next = !enabled
    try {
      if (next) {
        await enableAutostart()
        toast.success('Windows 起動時に自動起動するよう登録しました')
      } else {
        await disableAutostart()
        toast.success('自動起動の登録を解除しました')
      }
      setEnabled(next)
    } catch (e) {
      toast.error(`${next ? '登録' : '解除'}失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
      <div className="flex items-center gap-2">
        <Power size={14} className="text-neutral-400" />
        <h3 className="text-sm font-medium text-neutral-200">起動オプション</h3>
      </div>
      <p className="text-xs text-neutral-500">
        PC 起動と同時に RushVoice を立ち上げ、ホットキーが即座に使える状態にします。
      </p>

      <label className="flex items-center justify-between gap-3 cursor-pointer">
        <div>
          <div className="text-sm text-neutral-100">Windows 起動時に自動起動</div>
          <div className="text-xs text-neutral-500 mt-0.5">
            {enabled === null
              ? '読込中…'
              : enabled
                ? '登録済み（次回 PC 起動時から有効）'
                : '未登録（手動で起動が必要）'}
          </div>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={enabled === true}
          onClick={handleToggle}
          disabled={enabled === null || busy}
          className={cn(
            'relative inline-flex h-6 w-11 items-center rounded-full transition-colors disabled:opacity-50',
            enabled ? 'bg-[var(--color-brand-500)]' : 'bg-neutral-700',
          )}
        >
          <span
            className={cn(
              'inline-block h-4 w-4 transform rounded-full bg-white transition-transform',
              enabled ? 'translate-x-6' : 'translate-x-1',
            )}
          />
        </button>
      </label>
    </div>
  )
}

// ----- Phase 2 D: 履歴管理セクション -----
//
// 仕様: SSOT v2.10 §8.3.2.7 / 設計ドラフト phase2_d_history_sqlite_design_2026-05-22.md §7
// - 件数表示 (history_count)
// - 「履歴を全削除」ボタン (clear_history + confirm)
// - 保持件数 直近 50 件 (固定・読み取り専用 / v0.1.7.1 改訂で件数ベース化)
//
// Q1-Q5 確定事項:
// - Q2 デフォルト保持 直近 50 件 (delete_keep_recent・lib.rs HISTORY_RETENTION_COUNT 定数)
//   起動時 cleanup は Rust lib.rs setup で実行
//   経緯: 90 日 (Day 11) → 7 日 (v0.1.7.1 初版) → 直近 50 件 (v0.1.7.1 改訂・件数ベース)
// - Q3 平文保存許容 (DPAPI 対象外・本セクションでは触れない)

function HistorySection() {
  const [count, setCount] = useState<number | null>(null)
  const [busy, setBusy] = useState(false)

  async function loadCount() {
    try {
      const n = await historyCount()
      setCount(n)
    } catch (e) {
      // 件数取得失敗は致命ではないので toast 通知のみ・null 維持
      toast.error(`履歴件数の取得に失敗: ${e}`)
      setCount(null)
    }
  }

  // 既存 AutostartSection と同じパターンで async IIFE wrap (react-hooks/set-state-in-effect 回避)
  useEffect(() => {
    void (async () => {
      await loadCount()
    })()
  }, [])

  async function handleClear() {
    if (busy) return
    if (!confirm('履歴を全件削除します。この操作は取り消せません。よろしいですか？')) {
      return
    }
    setBusy(true)
    try {
      await clearHistory()
      toast.success('履歴を全件削除しました')
      await loadCount()
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
      <div className="flex items-center gap-2">
        <Database size={14} className="text-neutral-400" />
        <h3 className="text-sm font-medium text-neutral-200">履歴管理</h3>
      </div>
      <p className="text-xs text-neutral-500">
        文字起こし結果はローカルの SQLite に保存され、履歴ページから検索・削除できます。
      </p>

      <div className="flex items-center justify-between gap-3">
        <div>
          <div className="text-sm text-neutral-100">保存件数</div>
          <div className="text-xs text-neutral-500 mt-0.5">
            {count === null ? '読込中…' : `${count.toLocaleString()} 件`}
          </div>
        </div>
        <button
          type="button"
          onClick={handleClear}
          disabled={busy || count === null || count === 0}
          className="px-3 py-2 bg-neutral-800 hover:bg-red-900 text-neutral-300 hover:text-red-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50 disabled:hover:bg-neutral-800 disabled:hover:text-neutral-300"
          aria-label="履歴を全削除"
        >
          <Trash2 size={12} />
          <span>全件削除</span>
        </button>
      </div>

      <p className="text-xs text-neutral-600 pt-2 border-t border-neutral-900">
        ※ 履歴は直近 50 件のみ保持されます（起動時に 51 件目以降の古い履歴を自動削除）。
      </p>
    </div>
  )
}

function ApiKeyRow({
  label,
  placeholder,
  value,
  onChange,
  stored,
  busy,
  onSave,
  onTest,
  onDelete,
  allowTestWithoutKey = false,
}: ApiKeyRowProps) {
  // Phase 2 C Wave 2 AD-02: キー表示切替 Checkbox (Radix UI + Input type=password ↔ text)
  // 入力欄が空のときは「保存済み (●●●...)」の placeholder のみ表示なので切替不要
  // → 入力中だけ「表示」を有効化。Checkbox は row 内 row 下部に配置。
  const [revealed, setRevealed] = useState(false)
  return (
    <div>
      <div className="flex items-center justify-between mb-1.5">
        <label className="block text-xs text-neutral-400">{label}</label>
        {stored && (
          <span className="flex items-center gap-1 text-[10px] text-emerald-400">
            <Check size={10} />
            <span>保存済み</span>
          </span>
        )}
      </div>
      <div className="flex gap-2">
        <input
          type={revealed ? 'text' : 'password'}
          placeholder={placeholder}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="flex-1 px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none disabled:opacity-60 font-mono"
          disabled={busy}
        />
        <button
          type="button"
          onClick={onTest}
          disabled={busy || (!value.trim() && !stored && !allowTestWithoutKey)}
          className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
          title={
            value.trim()
              ? '入力欄のキーで接続テスト'
              : stored
                ? '保存済みキーで接続テスト'
                : allowTestWithoutKey
                  ? 'キー無しで接続テスト'
                  : 'キーを入力してください'
          }
        >
          <Zap size={12} />
          <span>テスト</span>
        </button>
        <button
          type="button"
          onClick={onSave}
          disabled={busy || !value.trim()}
          className="px-3 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
        >
          <Save size={12} />
          <span>保存</span>
        </button>
        {stored && (
          <button
            type="button"
            onClick={onDelete}
            disabled={busy}
            className="px-3 py-2 bg-neutral-800 hover:bg-red-900 text-neutral-400 hover:text-red-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
            aria-label={`${label} を削除`}
          >
            <Trash2 size={12} />
          </button>
        )}
      </div>
      {/* AD-02 キー表示切替 (Radix Checkbox + Input type=password ↔ text) */}
      {/* 入力欄が空のとき (= 保存済 placeholder のみ) は切替不可 */}
      <label
        className={cn(
          'mt-2 flex items-center gap-2 select-none w-fit',
          value.length === 0
            ? 'cursor-not-allowed opacity-50'
            : 'cursor-pointer',
        )}
      >
        <Checkbox.Root
          checked={revealed}
          onCheckedChange={(v) => setRevealed(v === true)}
          disabled={busy || value.length === 0}
          className={cn(
            'w-3.5 h-3.5 rounded border flex items-center justify-center transition-colors',
            revealed
              ? 'bg-[var(--color-brand-500)] border-[var(--color-brand-500)]'
              : 'bg-neutral-900 border-neutral-700 hover:border-neutral-600',
          )}
          aria-label="キーを表示"
        >
          <Checkbox.Indicator>
            <Check size={10} className="text-white" />
          </Checkbox.Indicator>
        </Checkbox.Root>
        <span className="text-[11px] text-neutral-500 inline-flex items-center gap-1">
          {revealed ? <Eye size={10} /> : <EyeOff size={10} />}
          <span>{revealed ? 'キーを表示中' : 'キーを表示'}</span>
        </span>
      </label>
    </div>
  )
}

// ----- v0.1.10 機能 A: 通知バナー設定セクション -----
//
// SSOT: 内部設計メモ
//
// - 閾値 (1000〜30000 ms・default 5000ms)
// - hedged mode 時の表示有無 (default 抑止)
//   - hedged は primary 遅延時に fallback に race するためバナー不要 (確定)
//   - 「常時表示」希望のユーザーのみ有効化

// ----- マイク選択セクション (2026-07-23 要望) -----
// 隣で会議中など Shokz ヘッドセットへ即切替したい場面用。Windows のサウンド設定を
// 開かずに GUI で入力デバイスを選ぶ。空文字 = Windows 既定デバイス自動追従 (従来挙動)。
// 反映は「次の録音開始」から (録音中ストリームは device に bind 済のため・AU-02 と同方針)。

const INPUT_DEVICE_KEY = 'input_device_name'

function MicSection() {
  const [devices, setDevices] = useState<string[]>([])
  const [defaultName, setDefaultName] = useState<string | null>(null)
  const [selected, setSelected] = useState<string>('')
  const [loaded, setLoaded] = useState(false)
  const [busy, setBusy] = useState(false)
  // 常時スタンバイ (2026-07-23: Bluetooth 冒頭切れの根本対策)
  const [standby, setStandby] = useState(false)

  const refresh = async () => {
    try {
      const [list, stored, sb] = await Promise.all([
        listInputDevices(),
        getSetting(INPUT_DEVICE_KEY),
        getMicStandby(),
      ])
      setDevices(list.devices)
      setDefaultName(list.default_name)
      setSelected(stored ?? '')
      setStandby(sb)
    } catch (e) {
      console.error('listInputDevices failed', e)
    } finally {
      setLoaded(true)
    }
  }

  useEffect(() => {
    void refresh()
    // 2026-07-23 指摘: 起動後に Bluetooth ヘッドセット (Shokz) を繋いでも一覧に
    // 出なかった → ウィンドウがフォーカスされる度に自動で再列挙する。
    const onFocus = () => void refresh()
    window.addEventListener('focus', onFocus)
    return () => window.removeEventListener('focus', onFocus)
  }, [])

  const handleChange = async (value: string) => {
    setBusy(true)
    try {
      await setSetting(INPUT_DEVICE_KEY, value)
      setSelected(value)
      // スタンバイ ON なら新デバイスで張り直す
      await applyMicStandby()
      const label = value === '' ? 'Windows 既定デバイス' : value
      toast.success(`マイクを「${label}」に変更しました（次の録音から反映）`)
    } catch (e) {
      toast.error(`マイク設定の保存に失敗しました: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  const handleStandbyToggle = async (enabled: boolean) => {
    setBusy(true)
    try {
      await setMicStandby(enabled)
      setStandby(enabled)
      toast.success(
        enabled
          ? '常時スタンバイを ON にしました（冒頭切れ対策・即時有効）'
          : '常時スタンバイを OFF にしました',
      )
    } catch (e) {
      toast.error(`常時スタンバイの切替に失敗しました: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  // 保存済みデバイスが現在の一覧に無い場合 (電源OFF・取り外し) も選択肢に出して明示する
  const missingSelected = selected !== '' && !devices.includes(selected)

  return (
    <div className="space-y-4 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Mic size={14} className="text-neutral-400" />
          <h3 className="text-sm font-medium text-neutral-200">使用マイク</h3>
        </div>
        <button
          type="button"
          className="text-xs text-neutral-500 hover:text-neutral-300"
          onClick={() => void refresh()}
          disabled={busy}
        >
          ↻ 一覧を更新
        </button>
      </div>

      {/* いま実際に使われるマイクをひと目で分かるよう明示 (2026-07-23 指摘: わかりにくい) */}
      <p className="text-sm text-neutral-300">
        録音に使うマイク:{' '}
        <span className="text-emerald-400">
          {selected === ''
            ? `${defaultName ?? '(不明)'} （Windows 既定）`
            : missingSelected
              ? `${defaultName ?? '(不明)'} （選択中の「${selected}」が未接続のため既定を使用）`
              : selected}
        </span>
      </p>

      <div>
        <select
          className="w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm disabled:opacity-60"
          value={selected}
          onChange={(e) => void handleChange(e.target.value)}
          onMouseDown={() => {
            // ドロップダウンを開いた瞬間にも再列挙 (後から繋いだ Bluetooth 機器対策)
            if (!busy) void refresh()
          }}
          disabled={!loaded || busy}
        >
          <option value="">
            Windows の設定に従う（自動）{defaultName ? ` — 現在: ${defaultName}` : ''}
          </option>
          {devices.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
          {missingSelected && (
            <option value={selected}>{selected}（未接続）</option>
          )}
        </select>
        <p className="text-xs text-neutral-500 mt-1.5">
          ヘッドセット（Shokz など）に切り替えたい時はここで選ぶだけで OK。次の録音から反映されます。
          選んだマイクが未接続の時は自動で Windows 既定マイクを使うので録音は止まりません。
          接続したばかりの機器が出てこない時は「↻ 一覧を更新」を押してください。
        </p>
      </div>

      {/* 常時スタンバイ (2026-07-23: Bluetooth 冒頭切れの根本対策) */}
      <div className="pt-3 border-t border-neutral-800">
        <label className="flex items-start gap-2.5 cursor-pointer select-none">
          <input
            type="checkbox"
            checked={standby}
            onChange={(e) => void handleStandbyToggle(e.target.checked)}
            disabled={!loaded || busy}
            className="mt-0.5 accent-[var(--color-brand-500)]"
          />
          <span>
            <span className="text-sm text-neutral-200">
              マイクを常時スタンバイする（Bluetooth の冒頭切れ対策）
            </span>
            <span className="block text-xs text-neutral-500 mt-0.5">
              録音していない間もマイクとの接続を維持し、話し始めの数語が切れるのを防ぎます。
              Shokz などの Bluetooth ヘッドセット利用時に ON 推奨。
              音声はどこにも保存・送信されません（接続維持のみ）。
              ヘッドセットの電池消費が少し増える点と、スタンバイ中は通話用モードになるため
              音楽再生の音質が下がる点にご注意ください。
            </span>
          </span>
        </label>
      </div>
    </div>
  )
}

const CONGESTION_THRESHOLD_MIN = 1000
const CONGESTION_THRESHOLD_MAX = 30000
const CONGESTION_THRESHOLD_DEFAULT = 5000

function CongestionSection() {
  const [thresholdMs, setThresholdMs] = useState<number>(CONGESTION_THRESHOLD_DEFAULT)
  const [showInHedged, setShowInHedged] = useState<boolean>(false)
  const [loaded, setLoaded] = useState(false)
  const [saving, setSaving] = useState(false)
  // 編集中の文字列 (空入力時の挙動のため number と分離)
  const [thresholdInput, setThresholdInput] = useState<string>(
    String(CONGESTION_THRESHOLD_DEFAULT),
  )

  useEffect(() => {
    ;(async () => {
      try {
        const cfg = await getCongestionConfig()
        setThresholdMs(cfg.threshold_ms)
        setThresholdInput(String(cfg.threshold_ms))
        setShowInHedged(cfg.show_in_hedged)
      } catch (e) {
        console.warn('getCongestionConfig failed:', e)
        toast.error(`通知バナー設定の読み込みに失敗: ${e}`)
      } finally {
        setLoaded(true)
      }
    })()
  }, [])

  async function persist(next: CongestionConfig) {
    setSaving(true)
    try {
      await setCongestionConfig(next)
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setSaving(false)
    }
  }

  function handleThresholdCommit() {
    const parsed = parseInt(thresholdInput, 10)
    if (Number.isNaN(parsed)) {
      // 不正値 → 元の値に戻す
      setThresholdInput(String(thresholdMs))
      return
    }
    const clamped = Math.max(
      CONGESTION_THRESHOLD_MIN,
      Math.min(CONGESTION_THRESHOLD_MAX, parsed),
    )
    if (clamped !== parsed) {
      toast.warning(
        `閾値は ${CONGESTION_THRESHOLD_MIN}〜${CONGESTION_THRESHOLD_MAX}ms の範囲に調整しました`,
      )
    }
    setThresholdMs(clamped)
    setThresholdInput(String(clamped))
    void persist({ threshold_ms: clamped, show_in_hedged: showInHedged })
  }

  async function handleHedgedToggle(next: boolean) {
    setShowInHedged(next)
    await persist({ threshold_ms: thresholdMs, show_in_hedged: next })
  }

  const busy = saving || !loaded

  return (
    <div className="space-y-4 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
      <h3 className="text-sm font-medium text-neutral-200">通知バナー</h3>
      <p className="text-xs text-neutral-500 -mt-2">
        文字起こしに時間がかかったときの安心メッセージ
      </p>

      <div>
        <label
          htmlFor="congestion-threshold"
          className="block text-xs text-neutral-400 mb-1.5"
        >
          発動までの時間 (ms)
        </label>
        <input
          id="congestion-threshold"
          type="number"
          min={CONGESTION_THRESHOLD_MIN}
          max={CONGESTION_THRESHOLD_MAX}
          step={500}
          value={thresholdInput}
          onChange={(e) => setThresholdInput(e.target.value)}
          onBlur={handleThresholdCommit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault()
              ;(e.target as HTMLInputElement).blur()
            }
          }}
          disabled={busy}
          className="w-32 px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm disabled:opacity-60"
        />
        <p className="text-[11px] text-neutral-600 mt-1">
          {CONGESTION_THRESHOLD_MIN}〜{CONGESTION_THRESHOLD_MAX}ms (default 5000ms)
        </p>
      </div>

      <label className="flex items-center gap-2 cursor-pointer">
        <Checkbox.Root
          checked={showInHedged}
          onCheckedChange={(v) => handleHedgedToggle(v === true)}
          disabled={busy}
          className={cn(
            'w-4 h-4 rounded border flex items-center justify-center transition-colors',
            showInHedged
              ? 'bg-[var(--color-brand-500)] border-[var(--color-brand-500)]'
              : 'bg-neutral-900 border-neutral-700 hover:border-neutral-600',
          )}
          aria-label="hedged mode 時にも表示する"
        >
          <Checkbox.Indicator>
            <Check size={11} className="text-white" />
          </Checkbox.Indicator>
        </Checkbox.Root>
        <span className="text-xs text-neutral-300">
          hedged モードでも表示する
          <span className="text-[11px] text-neutral-500 ml-2">
            (default OFF: hedged は OpenAI に自動切替するため抑止)
          </span>
        </span>
      </label>
    </div>
  )
}

// ----- 接続先と API キー (2026-11-15) -----
//
// 文字起こし (/audio/transcriptions) と 整文・字幕翻訳・議事録整形 (/chat/completions) の
// それぞれに接続先を選ぶ。Groq / OpenAI は自分のキー、カスタムは OpenAI 互換の任意のサーバー
// (Ollama・LM Studio・whisper.cpp server など手元のローカル LLM を含む。キーは任意)。
// 解決ロジックの正は Rust 側 src-tauri/src/endpoints.rs。

const PROVIDER_LABEL: Record<Provider, string> = {
  groq: 'Groq',
  openai: 'OpenAI',
  custom_transcribe: '文字起こしのカスタム接続先',
  custom_llm: '整文のカスタム接続先',
}

function useStoredKey(p: Provider) {
  const [stored, setStored] = useState(false)
  useEffect(() => {
    hasApiKey(p).then(setStored).catch((e) => console.warn('hasApiKey failed:', e))
  }, [p])
  return [stored, setStored] as const
}

function EndpointSection() {
  const [transcribeProvider, setTranscribeProvider] = useState('hedged')
  const [llmProvider, setLlmProvider] = useState('groq')
  const [loaded, setLoaded] = useState(false)

  useEffect(() => {
    ;(async () => {
      try {
        const [t, l] = await Promise.all([
          getSetting(TRANSCRIBE_PROVIDER_KEY),
          getSetting(LLM_PROVIDER_KEY),
        ])
        if (t) setTranscribeProvider(t)
        if (l) setLlmProvider(l)
      } catch (e) {
        toast.error(`設定読み込み失敗: ${e}`)
      } finally {
        setLoaded(true)
      }
    })()
  }, [])

  async function changeSetting(key: string, value: string, set: (v: string) => void) {
    set(value)
    try {
      await setSetting(key, value)
      toast.success('接続先を保存しました')
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    }
  }

  const selectClass =
    'w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm disabled:opacity-60'

  return (
    <div className="space-y-5 p-5 bg-neutral-950 border border-neutral-800 rounded-lg">
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-neutral-200">接続先と API キー</h3>
        <div className="flex items-center gap-1.5 text-xs text-emerald-400/80">
          <Lock size={12} />
          <span>キーは暗号化して保存</span>
        </div>
      </div>
      <p className="text-xs text-neutral-500 leading-relaxed">
        自分の Groq / OpenAI の API キー (Groq は無料枠あり) か、OpenAI 互換の接続先
        (Ollama・LM Studio・whisper.cpp server など手元のローカル LLM も可) で動きます。
        文字起こしと整文は別々の接続先にできます。
      </p>

      {/* API キー */}
      <div className="space-y-4">
        <ProviderKeyRow provider="groq" placeholderNew="gsk_..." />
        <ProviderKeyRow provider="openai" placeholderNew="sk-..." />
      </div>

      {/* 文字起こし */}
      <div className="space-y-3 pt-4 border-t border-neutral-800">
        <label className="block text-xs text-neutral-400">文字起こしの接続先</label>
        <select
          className={selectClass}
          value={transcribeProvider}
          onChange={(e) => changeSetting(TRANSCRIBE_PROVIDER_KEY, e.target.value, setTranscribeProvider)}
          disabled={!loaded}
        >
          <option value="hedged">おすすめ: Groq (遅いときは OpenAI も並走・キーがある方だけでも可)</option>
          <option value="groq">Groq だけ</option>
          <option value="openai">OpenAI だけ</option>
          <option value="custom">カスタム (OpenAI 互換・ローカル可)</option>
        </select>
        {transcribeProvider === 'custom' && (
          <CustomEndpointFields
            provider="custom_transcribe"
            baseUrlKey={CUSTOM_TRANSCRIBE_BASE_URL_KEY}
            modelKey={CUSTOM_TRANSCRIBE_MODEL_KEY}
            urlPlaceholder="http://127.0.0.1:8080/v1"
            modelPlaceholder="whisper-1 (空でも可)"
            hint="例: whisper.cpp server → http://127.0.0.1:8080/v1 ／ faster-whisper 系の OpenAI 互換サーバー → http://localhost:8000/v1 (モデル名はサーバーの指定どおり)。議事録・字幕では verbose_json に対応したサーバーが必要です。"
          />
        )}
      </div>

      {/* 整文・翻訳 */}
      <div className="space-y-3 pt-4 border-t border-neutral-800">
        <label className="block text-xs text-neutral-400">整文・字幕翻訳・議事録整形の接続先</label>
        <select
          className={selectClass}
          value={llmProvider}
          onChange={(e) => changeSetting(LLM_PROVIDER_KEY, e.target.value, setLlmProvider)}
          disabled={!loaded}
        >
          <option value="groq">Groq (gpt-oss-120b)</option>
          <option value="openai">OpenAI (gpt-4o-mini)</option>
          <option value="custom">カスタム (OpenAI 互換・ローカル可)</option>
        </select>
        {llmProvider === 'groq' && (
          <ModelOverrideField
            settingKey={GROQ_LLM_MODEL_KEY}
            defaultModel="openai/gpt-oss-120b"
            hint="空なら既定 (openai/gpt-oss-120b・日次の上限に当たったら openai/gpt-oss-20b)。Groq がモデルを入れ替えたときは、Groq のモデル一覧にある名前を入れてください。"
          />
        )}
        {llmProvider === 'openai' && (
          <ModelOverrideField
            settingKey={OPENAI_LLM_MODEL_KEY}
            defaultModel="gpt-4o-mini"
            hint="空なら既定 (gpt-4o-mini)。"
          />
        )}
        {llmProvider === 'custom' && (
          <CustomEndpointFields
            provider="custom_llm"
            baseUrlKey={CUSTOM_LLM_BASE_URL_KEY}
            modelKey={CUSTOM_LLM_MODEL_KEY}
            urlPlaceholder="http://localhost:11434/v1"
            modelPlaceholder="qwen2.5:7b"
            hint="例: Ollama → http://localhost:11434/v1 (モデル名は ollama list の名前) ／ LM Studio → http://localhost:1234/v1。整文の指示を守れる程度の大きさのモデルをおすすめします。つながらないときは整えずにそのまま貼り付けます。"
          />
        )}
      </div>
    </div>
  )
}

/** Groq / OpenAI の整文モデル名の上書き。提供元がモデルを廃止したときにアプリの更新を待たずに直せるように。 */
function ModelOverrideField({
  settingKey,
  defaultModel,
  hint,
}: {
  settingKey: string
  defaultModel: string
  hint: string
}) {
  const [value, setValue] = useState('')
  const [saved, setSaved] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    getSetting(settingKey)
      .then((v) => {
        setValue(v ?? '')
        setSaved(v ?? '')
      })
      .catch((e) => console.warn('model override load failed:', e))
  }, [settingKey])

  const dirty = value.trim() !== saved

  async function save() {
    setBusy(true)
    try {
      await setSetting(settingKey, value.trim())
      setSaved(value.trim())
      toast.success(value.trim() ? `整文のモデルを ${value.trim()} にしました` : '整文のモデルを既定に戻しました')
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-1.5">
      <label className="block text-xs text-neutral-400">モデル名 (空なら既定)</label>
      <div className="flex gap-2">
        <input
          type="text"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={defaultModel}
          spellCheck={false}
          className="flex-1 px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none font-mono"
        />
        <button
          type="button"
          onClick={save}
          disabled={busy || !dirty}
          className="px-3 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
        >
          <Save size={12} />
          <span>{dirty ? '保存' : '保存済み'}</span>
        </button>
      </div>
      <p className="text-[11px] text-neutral-500 leading-relaxed">{hint}</p>
    </div>
  )
}

function ProviderKeyRow({ provider, placeholderNew }: { provider: Provider; placeholderNew: string }) {
  const [input, setInput] = useState('')
  const [stored, setStored] = useStoredKey(provider)
  const [busy, setBusy] = useState(false)
  const name = PROVIDER_LABEL[provider]

  async function save() {
    if (!input.trim()) return
    setBusy(true)
    try {
      await saveApiKey(provider, input.trim())
      toast.success(`${name} の API キーを暗号化保存しました`)
      setInput('')
      setStored(true)
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    if (!confirm(`${name} の API キーを削除しますか？`)) return
    setBusy(true)
    try {
      await deleteApiKey(provider)
      toast.success(`${name} の API キーを削除しました`)
      setStored(false)
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  async function test() {
    setBusy(true)
    try {
      const r = input.trim() ? await testApiKey(provider, input.trim()) : await testStoredApiKey(provider)
      if (r.ok) toast.success(`接続成功 (HTTP ${r.status})`)
      else toast.error(`接続失敗: ${r.message}`)
    } catch (e) {
      toast.error(`テスト失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <ApiKeyRow
      label={`${name} API キー`}
      placeholder={stored ? '●●●●●●●●●●●● (保存済み)' : placeholderNew}
      value={input}
      onChange={setInput}
      stored={stored}
      busy={busy}
      onSave={save}
      onTest={test}
      onDelete={remove}
    />
  )
}

interface CustomEndpointFieldsProps {
  provider: Provider
  baseUrlKey: string
  modelKey: string
  urlPlaceholder: string
  modelPlaceholder: string
  hint: string
}

function CustomEndpointFields({
  provider,
  baseUrlKey,
  modelKey,
  urlPlaceholder,
  modelPlaceholder,
  hint,
}: CustomEndpointFieldsProps) {
  const [baseUrl, setBaseUrl] = useState('')
  const [model, setModel] = useState('')
  const [savedUrl, setSavedUrl] = useState('')
  const [savedModel, setSavedModel] = useState('')
  const [keyInput, setKeyInput] = useState('')
  const [keyStored, setKeyStored] = useStoredKey(provider)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    ;(async () => {
      try {
        const [u, m] = await Promise.all([getSetting(baseUrlKey), getSetting(modelKey)])
        setBaseUrl(u ?? '')
        setSavedUrl(u ?? '')
        setModel(m ?? '')
        setSavedModel(m ?? '')
      } catch (e) {
        console.warn('custom endpoint load failed:', e)
      }
    })()
  }, [baseUrlKey, modelKey])

  const urlValid = /^https?:\/\/[^/\s]+/.test(baseUrl.trim())
  const dirty = baseUrl.trim() !== savedUrl || model.trim() !== savedModel

  async function saveFields() {
    if (!urlValid) {
      toast.error('URL は http:// か https:// で始めてください')
      return
    }
    setBusy(true)
    try {
      await setSetting(baseUrlKey, baseUrl.trim())
      await setSetting(modelKey, model.trim())
      setSavedUrl(baseUrl.trim())
      setSavedModel(model.trim())
      toast.success('カスタム接続先を保存しました')
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  async function test() {
    if (!urlValid) {
      toast.error('URL は http:// か https:// で始めてください')
      return
    }
    setBusy(true)
    try {
      const r = await testEndpointUrl(provider, baseUrl.trim(), keyInput.trim() || null)
      if (r.ok) toast.success(r.status ? `${r.message} (HTTP ${r.status})` : r.message)
      else toast.error(`接続失敗: ${r.message}`)
    } catch (e) {
      toast.error(`テスト失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  async function saveKey() {
    if (!keyInput.trim()) return
    setBusy(true)
    try {
      await saveApiKey(provider, keyInput.trim())
      toast.success('キーを暗号化保存しました')
      setKeyInput('')
      setKeyStored(true)
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  async function removeKey() {
    if (!confirm('このカスタム接続先のキーを削除しますか？')) return
    setBusy(true)
    try {
      await deleteApiKey(provider)
      setKeyStored(false)
      toast.success('キーを削除しました')
    } catch (e) {
      toast.error(`削除失敗: ${e}`)
    } finally {
      setBusy(false)
    }
  }

  const inputClass =
    'w-full px-3 py-2 bg-neutral-900 border border-neutral-800 rounded-md text-sm focus:border-[var(--color-brand-500)] focus:outline-none disabled:opacity-60 font-mono'

  return (
    <div className="space-y-3 p-4 bg-neutral-900/60 border border-neutral-800 rounded-md">
      <div>
        <label className="block text-xs text-neutral-400 mb-1.5">ベース URL (/v1 まで)</label>
        <input
          type="text"
          inputMode="url"
          spellCheck={false}
          placeholder={urlPlaceholder}
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          className={inputClass}
          disabled={busy}
        />
        {baseUrl.trim() !== '' && !urlValid && (
          <p className="mt-1 text-[11px] text-amber-400">http:// か https:// で始めてください</p>
        )}
      </div>
      <div>
        <label className="block text-xs text-neutral-400 mb-1.5">モデル名</label>
        <input
          type="text"
          spellCheck={false}
          placeholder={modelPlaceholder}
          value={model}
          onChange={(e) => setModel(e.target.value)}
          className={inputClass}
          disabled={busy}
        />
      </div>
      <div className="flex gap-2">
        <button
          type="button"
          onClick={test}
          disabled={busy || !urlValid}
          className="px-3 py-2 bg-neutral-800 hover:bg-neutral-700 text-neutral-300 text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
        >
          <Zap size={12} />
          <span>接続テスト</span>
        </button>
        <button
          type="button"
          onClick={saveFields}
          disabled={busy || !urlValid || !dirty}
          className="px-3 py-2 bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white text-xs rounded-md flex items-center gap-1.5 disabled:opacity-50"
        >
          <Save size={12} />
          <span>{dirty ? '保存' : '保存済み'}</span>
        </button>
      </div>
      <ApiKeyRow
        label="キー (任意・ローカルのサーバーなら不要)"
        placeholder={keyStored ? '●●●●●●●●●●●● (保存済み)' : '空のままで可'}
        value={keyInput}
        onChange={setKeyInput}
        stored={keyStored}
        busy={busy}
        onSave={saveKey}
        onTest={test}
        onDelete={removeKey}
        allowTestWithoutKey
      />
      <p className="text-[11px] text-neutral-500 leading-relaxed">{hint}</p>
    </div>
  )
}
