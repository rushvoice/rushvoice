// HomePage.tsx — Phase 1 Day 6 Stage 2-C-2: シンプル化版
//
// 設計変更 (UX 要望):
//   - リアルタイム状態表示は全部 overlay に集約
//   - Home は履歴閲覧 + 「ホットキー使えない人向け裏口」の手動録音ボタンのみ
//   - toast 通知も overlay と重複するため削除
//   - 状態テキストや「最新の文字起こし」ボックスは削除

import { useEffect, useState } from 'react'
import { Mic, Square, Loader2, Copy, Pencil, BookOpen } from 'lucide-react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { toast } from 'sonner'
import {
  startRecording,
  stopRecording,
  getRecordingState,
  getHotkeyConfig,
  listProfiles,
  getActiveProfileKey,
  setActiveProfileKey,
  listHistory,
  updateHistoryEdit,
  upsertGlossaryEntry,
  type RecorderState,
  type HotkeyConfig,
  type Profile,
} from '../lib/tauri'
import { emptyGlossaryEntry } from '../types/glossary'
import { cn } from '../lib/cn'
import { CongestionBanner } from '../components/CongestionBanner'
import { MicLowLevelBanner } from '../components/MicLowLevelBanner'
import { QuietVoiceBanner } from '../components/QuietVoiceBanner'
import { GatewayEntitlementBanner } from '../components/GatewayEntitlementBanner'

// 設定 hotkey を「右Ctrl + 右Shift + 無変換」のような日本語表記に
const MOD_ORDER = [
  'Control', 'ControlLeft', 'ControlRight',
  'Shift', 'ShiftLeft', 'ShiftRight',
  'Alt', 'AltLeft', 'AltRight',
  'Meta', 'MetaLeft', 'MetaRight',
] as const
const MOD_LABEL_JA: Record<string, string> = {
  Control: 'Ctrl', ControlLeft: '左Ctrl', ControlRight: '右Ctrl',
  Shift: 'Shift', ShiftLeft: '左Shift', ShiftRight: '右Shift',
  Alt: 'Alt', AltLeft: '左Alt', AltRight: '右Alt',
  Meta: 'Win', MetaLeft: '左Win', MetaRight: '右Win',
}
const KEY_LABEL_JA: Record<string, string> = {
  NonConvert: '無変換', Convert: '変換', KanaMode: 'カナ',
  Lang1: 'かな', Lang2: '英数',
  Space: 'Space', Enter: 'Enter', Escape: 'Esc', Tab: 'Tab',
  Backspace: 'BS', Delete: 'Del', CapsLock: 'CapsLock',
  ControlLeft: '左Ctrl', ControlRight: '右Ctrl',
  ShiftLeft: '左Shift', ShiftRight: '右Shift',
  AltLeft: '左Alt', AltRight: '右Alt',
  MetaLeft: '左Win', MetaRight: '右Win',
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

// Phase 2 A: プロファイル表示順 (builtin_key の順序)
// 2026-07-27 要望: 素出力 → ナチュラル → ていねい → カジュアル → メール。
// 注意: Overlay.tsx の PROFILE_ORDER と必ず同じ並びにすること
// (かつて両者が別々の並びで定義され、ホーム画面とピルで順番が食い違っていた)。
const PROFILE_KEY_ORDER = ['raw', 'natural', 'business_doc', 'casual', 'mail']

function sortProfiles(profiles: Profile[]): Profile[] {
  return [...profiles].sort((a, b) => {
    const ai = PROFILE_KEY_ORDER.indexOf(a.builtin_key ?? '')
    const bi = PROFILE_KEY_ORDER.indexOf(b.builtin_key ?? '')
    // ビルトインは order に従い、ユーザー作成 (-1) は末尾
    const aPos = ai === -1 ? PROFILE_KEY_ORDER.length : ai
    const bPos = bi === -1 ? PROFILE_KEY_ORDER.length : bi
    return aPos - bPos
  })
}

export function HomePage() {
  const [state, setState] = useState<RecorderState>('idle')
  // 要望 (2026-05-21): 最新文字起こしを Home でも確認できるよう復活
  // (Overlay は 3 秒で消えるため、全文を後から見たい時のための表示)
  const [latest, setLatest] = useState<string>('')
  // 要望 (2026-05-22): ホットキー説明文を hardcode せず DB から読込
  const [hotkeyDisplay, setHotkeyDisplay] = useState<string>('Ctrl + Shift + 無変換')
  // Phase 2 A: プロファイル選択
  const [profiles, setProfiles] = useState<Profile[]>([])
  const [activeKey, setActiveKey] = useState<string>('raw')
  // 2026-07-23 要望: ホームで誤字修正 (最新文字起こしの編集 + 辞書クイック登録)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const [quickWrong, setQuickWrong] = useState('')
  const [quickRight, setQuickRight] = useState('')

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        const s = await getRecordingState()
        if (mounted) setState(s)
      } catch (e) {
        console.warn('getRecordingState failed:', e)
      }
      try {
        const cfg = await getHotkeyConfig()
        if (mounted) setHotkeyDisplay(formatHotkey(cfg))
      } catch (e) {
        console.warn('getHotkeyConfig failed:', e)
      }
      // Phase 2 A: プロファイル一覧とアクティブキーを取得
      try {
        const [ps, ak] = await Promise.all([listProfiles(), getActiveProfileKey()])
        if (mounted) {
          setProfiles(sortProfiles(ps))
          setActiveKey(ak ?? 'raw')
        }
      } catch (e) {
        console.warn('listProfiles/getActiveProfileKey failed:', e)
      }
      try {
        unlistens.push(
          await listen<string>('recorder:state-changed', (e) => {
            if (!mounted) return
            setState(e.payload as RecorderState)
          }),
          await listen<string>('recorder:transcribed', (e) => {
            if (!mounted) return
            setLatest(e.payload)
          }),
          // 2026-05-22 修正: ホットキー設定変更時に Rust から emit される通知を受けて再描画
          await listen<HotkeyConfig>('hotkey:config-changed', (e) => {
            if (!mounted) return
            try {
              setHotkeyDisplay(formatHotkey(e.payload))
            } catch (err) {
              console.warn('formatHotkey failed (home event):', err)
            }
          }),
          // v0.1.3 防御第 2 層: ハルシ検出時の貼付抑止通知
          await listen<string>('recorder:halu-suppressed', (e) => {
            if (!mounted) return
            toast.warning(e.payload, { duration: 6000 })
          }),
          // ピル側でプロファイルが切り替わった時に Home の選択表示を同期
          await listen<string>('profile:active-changed', (e) => {
            if (!mounted) return
            setActiveKey(e.payload)
          }),
        )
      } catch (e) {
        console.warn('event listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  async function handleClick() {
    if (state === 'transcribing') return
    try {
      if (state === 'idle') {
        await startRecording()
      } else if (state === 'recording') {
        await stopRecording()
      }
    } catch (e) {
      console.warn('record action failed:', e)
    }
  }

  // 2026-07-22 要望: Home の最新文字起こしをワンクリックでコピー
  async function handleCopyLatest() {
    if (!latest) return
    try {
      await navigator.clipboard.writeText(latest)
      toast.success('クリップボードにコピーしました')
    } catch (e) {
      toast.error(`コピー失敗: ${e}`)
    }
  }

  // 2026-07-23 要望: 誤字の修正をホーム (先頭タブ) でもできるように
  function handleStartEdit() {
    setDraft(latest)
    setEditing(true)
  }

  async function handleSaveEdit() {
    const text = draft.trim()
    if (!text) {
      toast.error('本文が空です')
      return
    }
    try {
      // 最新の履歴行に編集を保存 (履歴タブの編集と同じ update_history_edit)
      const [newest] = await listHistory(1, 0)
      if (newest?.id != null) {
        await updateHistoryEdit(newest.id, text)
      }
      await navigator.clipboard.writeText(text)
      setLatest(text)
      setEditing(false)
      toast.success('修正を保存してクリップボードにコピーしました')
    } catch (e) {
      toast.error(`保存失敗: ${e}`)
    }
  }

  // 誤字→正しい表記 をその場で辞書登録 (次回から自動置換される)
  async function handleQuickRegister() {
    const wrong = quickWrong.trim()
    const right = quickRight.trim()
    if (!wrong || !right) {
      toast.error('「誤」と「正」を両方入力してください')
      return
    }
    if (wrong === right) {
      toast.error('誤と正が同じです')
      return
    }
    try {
      await upsertGlossaryEntry({
        ...emptyGlossaryEntry(),
        pattern: wrong,
        replacement: right,
        note: 'ホーム画面から登録',
      })
      setQuickWrong('')
      setQuickRight('')
      toast.success(`辞書に登録しました: ${wrong} → ${right}（次回の録音から適用）`)
    } catch (e) {
      toast.error(`辞書登録失敗: ${e}`)
    }
  }

  // Phase 2 A: プロファイル変更ハンドラ
  async function handleProfileChange(key: string) {
    setActiveKey(key)
    try {
      await setActiveProfileKey(key)
    } catch (e) {
      console.warn('setActiveProfileKey failed:', e)
      toast.error('プロファイルの保存に失敗しました')
    }
  }

  const Icon = () => {
    if (state === 'recording') return <Square size={48} className="text-white" fill="currentColor" />
    if (state === 'transcribing') return <Loader2 size={48} className="text-white animate-spin" />
    return <Mic size={48} className="text-white" />
  }

  const color =
    state === 'recording'
      ? 'bg-red-500 shadow-red-700/30'
      : state === 'transcribing'
        ? 'bg-amber-500 shadow-amber-700/30'
        : 'bg-[var(--color-brand-500)] shadow-[var(--color-brand-700)]/30'

  return (
    <div className="flex-1 flex flex-col items-center justify-center bg-neutral-900 text-white relative">
      {/* Phase 2 A: プロファイル選択ドロップダウン */}
      {profiles.length > 0 && (
        <div className="mb-5 flex items-center gap-2">
          <label htmlFor="profile-select" className="text-sm text-neutral-400">
            プロファイル
          </label>
          <select
            id="profile-select"
            value={activeKey}
            onChange={(e) => handleProfileChange(e.target.value)}
            disabled={state !== 'idle'}
            className="bg-neutral-800 border border-neutral-700 rounded px-2 py-1 text-sm text-neutral-200 focus:outline-none focus:border-neutral-500 disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {profiles.map((p) => (
              <option key={p.builtin_key ?? String(p.id)} value={p.builtin_key ?? String(p.id)}>
                {p.display_name}
              </option>
            ))}
          </select>
        </div>
      )}
      <button
        type="button"
        onClick={handleClick}
        disabled={state === 'transcribing'}
        className={cn(
          'w-32 h-32 rounded-full transition-colors flex items-center justify-center shadow-2xl focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-white/30 disabled:opacity-70 disabled:cursor-not-allowed',
          color,
        )}
        aria-label={state === 'recording' ? '録音停止' : '録音開始'}
      >
        <Icon />
      </button>

      {/* v0.1.10 機能 A: Groq 混雑時の安心バナー (threshold_ms 経過後に表示) */}
      {/* v0.1.12 F2: マイク入力低レベル警告バナー (L264 予防・録音開始 N 秒後に表示) */}
      <div className="mt-5 min-h-10 flex flex-col items-center justify-center gap-2">
        <div className="flex items-center justify-center gap-2">
          <CongestionBanner />
          <MicLowLevelBanner />
          {/* v0.1.15: 小声警告 (文字起こし完了後に表示・自動消去) */}
          <QuietVoiceBanner />
        </div>
        {/* Gateway ログイン (A-2-4): 403/429 インラインバナー (L337: alert 禁止) */}
        <GatewayEntitlementBanner />
      </div>

      <div className="mt-2 text-center max-w-md">
        <p className="text-base text-neutral-300">
          通常は <kbd className="px-2 py-0.5 bg-neutral-800 rounded text-sm">{hotkeyDisplay}</kbd> でどこからでも録音
        </p>
        <p className="text-xs text-neutral-500 mt-2">
          状態・波形は画面下端のオーバーレイに表示
        </p>
      </div>

      <div className="mt-8 px-6 py-4 bg-neutral-950 border border-neutral-800 rounded-lg w-[520px] min-h-[120px]">
        <div className="flex items-center justify-between mb-2">
          <p className="text-xs text-neutral-500">最新の文字起こし</p>
          {latest && !editing && (
            <div className="flex items-center gap-1">
              <button
                type="button"
                onClick={handleStartEdit}
                className="flex items-center gap-1 px-2 py-1 text-xs text-neutral-400 hover:text-neutral-100 hover:bg-neutral-800 rounded transition-colors"
                aria-label="最新の文字起こしを修正"
              >
                <Pencil size={13} />
                修正
              </button>
              <button
                type="button"
                onClick={handleCopyLatest}
                className="flex items-center gap-1 px-2 py-1 text-xs text-neutral-400 hover:text-neutral-100 hover:bg-neutral-800 rounded transition-colors"
                aria-label="最新の文字起こしをコピー"
              >
                <Copy size={13} />
                コピー
              </button>
            </div>
          )}
        </div>
        {editing ? (
          <div className="space-y-2">
            <textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              rows={4}
              autoFocus
              className="w-full px-3 py-2 bg-neutral-900 border border-neutral-700 rounded-md text-sm text-neutral-100 leading-relaxed focus:outline-none focus:border-neutral-500 resize-y"
            />
            <div className="flex items-center justify-end gap-2">
              <button
                type="button"
                onClick={() => setEditing(false)}
                className="px-3 py-1.5 text-xs text-neutral-400 hover:text-neutral-100 hover:bg-neutral-800 rounded transition-colors"
              >
                キャンセル
              </button>
              <button
                type="button"
                onClick={() => void handleSaveEdit()}
                className="px-3 py-1.5 text-xs bg-[var(--color-brand-500)] hover:bg-[var(--color-brand-700)] text-white rounded transition-colors"
              >
                保存してコピー
              </button>
            </div>
          </div>
        ) : latest ? (
          <p className="text-neutral-200 whitespace-pre-wrap leading-relaxed">{latest}</p>
        ) : (
          <p className="text-neutral-700 italic">（まだ録音されていません）</p>
        )}

        {/* 2026-07-23 要望: 誤字→正 をその場で辞書登録 (次回から自動置換) */}
        <div className="mt-3 pt-3 border-t border-neutral-800">
          <div className="flex items-center gap-1.5 mb-1.5">
            <BookOpen size={12} className="text-neutral-500" />
            <p className="text-xs text-neutral-500">誤変換を辞書に登録（次回から自動で直ります）</p>
          </div>
          <div className="flex items-center gap-2">
            <input
              type="text"
              value={quickWrong}
              onChange={(e) => setQuickWrong(e.target.value)}
              placeholder="誤（例: 記憲）"
              className="flex-1 min-w-0 px-2 py-1.5 bg-neutral-900 border border-neutral-800 rounded text-sm focus:outline-none focus:border-neutral-600"
            />
            <span className="text-neutral-500 text-sm">→</span>
            <input
              type="text"
              value={quickRight}
              onChange={(e) => setQuickRight(e.target.value)}
              placeholder="正（例: 規定）"
              onKeyDown={(e) => {
                if (e.key === 'Enter') void handleQuickRegister()
              }}
              className="flex-1 min-w-0 px-2 py-1.5 bg-neutral-900 border border-neutral-800 rounded text-sm focus:outline-none focus:border-neutral-600"
            />
            <button
              type="button"
              onClick={() => void handleQuickRegister()}
              className="px-3 py-1.5 text-xs bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded transition-colors shrink-0"
            >
              登録
            </button>
          </div>
        </div>
      </div>

      <p className="absolute bottom-3 left-3 text-[10px] text-amber-500/70 font-mono">
        Stage 2-C-2: WH_KEYBOARD_LL + Overlay window / hedged / clipboard は Stage 2-B
      </p>
    </div>
  )
}
