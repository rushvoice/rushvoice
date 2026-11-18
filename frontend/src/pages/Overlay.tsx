// Overlay.tsx — Phase 2 C Wave 3: ドラッグ + 位置永続化 + プロファイル表示
//
// 仕様 (要望 2026-05-21 + Wave 3 追加):
//   - 画面下端中央・常時表示 (待機中は小さな pill で RushVoice 起動中を示す)
//   - 録音/文字起こし時は大きく展開して状態 + 波形 + プレビュー表示
//   - success/error 後は hide ではなく idle pill に戻る (window 自体は常駐)
//   - click-through で他作業の邪魔にならない
//   - [Wave 3] ドラッグ対応: data-tauri-drag-region で pill をドラッグ移動可能
//   - [Wave 3] 位置永続化: mouseup 後に window 位置を config_db (get_setting/set_setting) に保存
//   - [Wave 3] プロファイル表示: idle pill にアクティブプロファイル名を表示
//   - [Wave 3] クリック切替: pill クリックで次のプロファイルに順送り切替

import { useEffect, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { PhysicalPosition } from '@tauri-apps/api/dpi'
import { Mic, Loader2, CheckCircle2, AlertCircle, AlertTriangle } from 'lucide-react'
import {
  getHotkeyConfig,
  setOverlaySize,
  type HotkeyConfig,
  listProfiles,
  getActiveProfileKey,
  setActiveProfileKey,
  getOverlayPosition,
  saveOverlayPosition,
  type Profile,
} from '../lib/tauri'

type OverlayState = 'idle' | 'recording' | 'transcribing' | 'success' | 'error' | 'cancelled'

const WAVEFORM_SIZE = 50

// プロファイル順序 (固定)
// 2026-07-27 要望: 素出力 → ナチュラル → ていねい → カジュアル → メール。
// 注意: HomePage.tsx の PROFILE_KEY_ORDER と必ず同じ並びにすること
// (かつて両者が別々の並びで定義され、ホーム画面とピルで順番が食い違っていた)。
const PROFILE_ORDER = ['raw', 'natural', 'business_doc', 'casual', 'mail'] as const
type ProfileKey = (typeof PROFILE_ORDER)[number]

// フォールバック表示名マッピング
const PROFILE_LABEL_FALLBACK: Record<string, string> = {
  raw: '素出力',
  natural: 'ナチュラル',
  // 2026-07-27 要望: 「ビジネス文書」→「ていねい」に改称 (builtin_key は互換のため据置)
  business_doc: 'ていねい',
  casual: 'カジュアル',
  mail: 'メール',
}

// ホットキー日本語表記 (HomePage / SettingsPage と同じロジック)
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
  return [...mods, key].join('+')
}

// 2026-05-22 要望: state ごとに最適 window サイズで透明枠を最小化
// pill 内容 + padding に基づいて余白がほぼ出ないようにする
// 2026-05-23 Day 9 (要望・段階的): 高さ半減 + 横幅微調整
//   idle 175 (確定) / recording 135 / transcribing 105 / success 195 / error 185
// Wave 3: idle 幅を少し広げてプロファイル名を表示するため 200 に拡張
const OVERLAY_SIZE: Record<OverlayState, { w: number; h: number }> = {
  idle:         { w: 200, h: 18 },
  recording:    { w: 135, h: 22 },
  transcribing: { w: 105, h: 18 },
  success:      { w: 195, h: 22 },
  error:        { w: 185, h: 22 },
  cancelled:    { w: 145, h: 18 },  // HK-06
}

// v0.1.12 F2: マイク入力低レベル警告 (L264 予防) 用の拡張サイズ
//   recording pill にバナーチップを追加表示するため一時的にウィンドウ幅を広げる。
//   検知中のみ適用・recording 状態の通常サイズ (135×22) → 拡張サイズ (320×22)。
const OVERLAY_SIZE_RECORDING_WITH_LOW_INPUT_WARN = { w: 320, h: 22 }

// v0.1.15 小声警告: success pill に「小声かも」チップを併記する際の拡張サイズ
//   success 通常 (195×22) → チップ分を足して 290×22。
const OVERLAY_SIZE_SUCCESS_WITH_QUIET_WARN = { w: 290, h: 22 }

export function Overlay() {
  const [state, setState] = useState<OverlayState>('idle')
  const [message, setMessage] = useState<string>('')
  const [waveform, setWaveform] = useState<number[]>(() => new Array(WAVEFORM_SIZE).fill(0))
  const [seconds, setSeconds] = useState(0)
  // 要望 (2026-05-22): idle pill に現在のホットキーを常時表示 (Aqua 風 hover tooltip 代替)
  const [hotkey, setHotkey] = useState<string>('')
  // v0.1.12 F2: マイク入力低レベル警告フラグ (L264 予防)
  //   recording 状態かつ true のとき recording pill にチップを追加表示する。
  //   recorder:low-input-level-detected で true・recorder:state-changed=idle で false。
  const [lowInputLevel, setLowInputLevel] = useState(false)
  // v0.1.15 小声警告: 文字起こし完了後の success pill に小声チップを出すフラグ。
  //   recorder:quiet-voice-warning で true・次の録音開始 (state=recording) で false。
  const [quietVoice, setQuietVoice] = useState(false)
  // 2026-07-23 指摘 (Shokz 冒頭切れ): Bluetooth HFP はリンク確立まで 0.3〜0.5s
  // 音声が届かず、その間の発話は復元不可能。最初の音声フレーム (recorder:waveform) が
  // 実際に届くまで「マイク準備中…」を表示し、「録音中」に変わってから話してもらう。
  const [micReady, setMicReady] = useState(false)

  // Wave 3: アクティブプロファイル
  const [profiles, setProfiles] = useState<Profile[]>([])
  const [activeProfileKey, setActiveProfileKeyState] = useState<string | null>(null)
  const startTimeRef = useRef<number>(0)
  const tickRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const resetTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // プロファイルラベル要素 ref (クリック判定用)
  const profileLabelRef = useRef<HTMLSpanElement>(null)

  // 2026-05-22 要望: state 変化時に Tauri window をその state の最適サイズに動的変更
  // P2-3 (2026-05-22): 同サイズ遷移は省略 (idle 200x32 → transcribing 200x32 等)・
  //   React 18 StrictMode の useEffect 二重実行で同じ resize が連続呼ばれるのも防ぐ。
  //   各 resize 直後に click-through が一瞬外れる Tauri 既知挙動の影響を最小化。
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null)
  useEffect(() => {
    // v0.1.12 F2: recording + lowInputLevel のときだけ警告チップ分の拡張サイズを使う
    // v0.1.15: success + quietVoice も「小声かも」チップ分だけ拡張する
    const size =
      state === 'recording' && lowInputLevel
        ? OVERLAY_SIZE_RECORDING_WITH_LOW_INPUT_WARN
        : state === 'success' && quietVoice
          ? OVERLAY_SIZE_SUCCESS_WITH_QUIET_WARN
          : OVERLAY_SIZE[state]
    if (
      lastSizeRef.current?.w === size.w &&
      lastSizeRef.current?.h === size.h
    ) {
      return
    }
    lastSizeRef.current = size
    setOverlaySize(size.w, size.h, state === 'idle').catch((e) => {
      console.warn('setOverlaySize failed:', e)
    })
  }, [state, lowInputLevel, quietVoice])

  // Wave 3: 起動時に保存済み位置を復元
  useEffect(() => {
    ;(async () => {
      try {
        const pos = await getOverlayPosition()
        if (pos != null) {
          const win = getCurrentWindow()
          await win.setPosition(new PhysicalPosition(pos.x, pos.y))
        }
      } catch (e) {
        console.warn('overlay position restore failed:', e)
      }
    })()
  }, [])

  // Wave 3: プロファイル一覧 + アクティブキーを取得
  useEffect(() => {
    ;(async () => {
      try {
        const [profs, activeKey] = await Promise.all([
          listProfiles().catch(() => [] as Profile[]),
          getActiveProfileKey(),
        ])
        setProfiles(profs)
        setActiveProfileKeyState(activeKey)
      } catch (e) {
        console.warn('profile load failed (overlay):', e)
      }
    })()
  }, [])

  // success/error 表示後に idle pill に戻す (window 自体は常駐継続)
  const scheduleResetToIdle = (delayMs: number) => {
    if (resetTimerRef.current) clearTimeout(resetTimerRef.current)
    resetTimerRef.current = setTimeout(() => {
      setState('idle')
      setMessage('')
    }, delayMs)
  }

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []

    ;(async () => {
      try {
        const cfg = await getHotkeyConfig()
        if (mounted) setHotkey(formatHotkey(cfg))
      } catch (e) {
        console.warn('getHotkeyConfig failed (overlay):', e)
      }
      try {
        // 2026-05-22 修正: ホットキー設定変更時に Rust から emit される通知を受けて再描画
        unlistens.push(
          await listen<HotkeyConfig>('hotkey:config-changed', (e) => {
            if (!mounted) return
            try {
              setHotkey(formatHotkey(e.payload))
            } catch (err) {
              console.warn('formatHotkey failed (overlay event):', err)
            }
          }),
          await listen<string>('recorder:state-changed', (e) => {
            if (!mounted) return
            const s = e.payload as 'idle' | 'recording' | 'transcribing'
            if (s === 'recording') {
              if (resetTimerRef.current) clearTimeout(resetTimerRef.current)
              setState('recording')
              setMessage('録音中')
              // 2026-07-23: 最初の音声フレームが届くまで「マイク準備中…」(Shokz 冒頭切れ対策)
              setMicReady(false)
              setWaveform(new Array(WAVEFORM_SIZE).fill(0))
              startTimeRef.current = Date.now()
              setSeconds(0)
              // v0.1.12 F2: 録音開始時に警告フラグをリセット (session ごと独立)
              setLowInputLevel(false)
              // v0.1.15: 小声チップも次録音開始でリセット
              setQuietVoice(false)
              if (tickRef.current) clearInterval(tickRef.current)
              tickRef.current = setInterval(() => {
                setSeconds(Math.floor((Date.now() - startTimeRef.current) / 1000))
              }, 250)
            } else if (s === 'transcribing') {
              if (tickRef.current) clearInterval(tickRef.current)
              setState('transcribing')
              setMessage('文字起こし中…')
              // v0.1.12 F2: 文字起こし開始で警告は消す (録音は終わった = 役割完了)
              setLowInputLevel(false)
            } else if (s === 'idle') {
              if (tickRef.current) clearInterval(tickRef.current)
              // v0.1.12 F2: フェイルセーフでフラグも下ろす
              setLowInputLevel(false)
              // idle 通知時は success/error の auto-reset を尊重 (即 idle 戻しはしない)
            }
          }),
          // v0.1.12 F2: マイク入力低レベル警告 (L264 予防)
          // Rust 側 worker thread から session 単位 1 回だけ emit される。
          // recording 状態のときだけ表示 (transcribing 以降は無視)。
          await listen('recorder:low-input-level-detected', () => {
            if (!mounted) return
            setLowInputLevel(true)
          }),
          // v0.1.15 小声警告: 文字起こし完了直前に emit される。success pill に小声チップを出す。
          await listen('recorder:quiet-voice-warning', () => {
            if (!mounted) return
            setQuietVoice(true)
          }),
          await listen<number>('recorder:waveform', (e) => {
            if (!mounted) return
            // 2026-07-23: 波形イベント = デバイスから実音声が届いた証拠 → マイク準備完了
            setMicReady(true)
            setWaveform((prev) => {
              const next = prev.slice(1)
              next.push(e.payload)
              return next
            })
          }),
          await listen<string>('recorder:transcribed', (e) => {
            if (!mounted) return
            setState('success')
            const text = e.payload
            const preview = text.length > 30 ? text.slice(0, 30) + '…' : text
            setMessage(`完了：${preview}`)
            scheduleResetToIdle(3000)
          }),
          // 2026-10-31: 書き起こし・貼り付けの最中に押された → 終わったらすぐ次の録音を始める合図
          await listen('recorder:queued', () => {
            if (!mounted) return
            setMessage('文字起こし中… 終わったらすぐ録音します')
          }),
          // 2026-11-18: 30 秒ほど声が無く、録音を自動で止めた (押し間違い対策・speech_gate.rs)
          await listen('recorder:auto-stopped', () => {
            if (!mounted) return
            setMessage('30秒ほど声が無かったので録音を止めました')
          }),
          await listen<string>('recorder:error', (e) => {
            if (!mounted) return
            setState('error')
            setMessage(`エラー：${e.payload}`)
            scheduleResetToIdle(5000)
          }),
          // HK-06: ESC キャンセル通知
          await listen<void>('recorder:cancelled', (_e) => {
            if (!mounted) return
            if (tickRef.current) clearInterval(tickRef.current)
            setState('cancelled')
            setMessage('キャンセルしました')
            scheduleResetToIdle(1500)
          }),
          // Wave 3: アクティブプロファイル変更通知 (Phase 2 A が emit する想定)
          await listen<string>('profile:active-changed', (e) => {
            if (!mounted) return
            setActiveProfileKeyState(e.payload)
          }),
        )
      } catch (e) {
        console.warn('overlay event listen failed:', e)
      }
    })()

    return () => {
      mounted = false
      for (const u of unlistens) u()
      if (tickRef.current) clearInterval(tickRef.current)
      if (resetTimerRef.current) clearTimeout(resetTimerRef.current)
    }
  }, [])

  // Wave 3: ピルクリックで次のプロファイルに順送り切替
  async function handlePillClick() {
    if (profiles.length === 0) return

    // 現在のアクティブキーから次のキーを決定
    const currentKey = activeProfileKey ?? PROFILE_ORDER[0]
    const currentIdx = PROFILE_ORDER.indexOf(currentKey as ProfileKey)
    const nextIdx = (currentIdx + 1) % PROFILE_ORDER.length
    const nextKey = PROFILE_ORDER[nextIdx]

    // profiles に次のキーが存在するかチェック (builtin_key or skip)
    const nextProfile = profiles.find(
      (p) => p.is_builtin && p.builtin_key === nextKey,
    )
    const resolvedKey = nextProfile ? nextKey : PROFILE_ORDER[0]

    setActiveProfileKeyState(resolvedKey)
    await setActiveProfileKey(resolvedKey)
  }

  // アクティブプロファイルの表示名取得
  function getActiveProfileLabel(): string {
    if (activeProfileKey == null) return ''
    const prof = profiles.find(
      (p) => p.is_builtin && p.builtin_key === activeProfileKey,
    )
    if (prof) return prof.display_name
    return PROFILE_LABEL_FALLBACK[activeProfileKey] ?? activeProfileKey
  }

  // 2026-05-22 要望: pill を window と同サイズにして「透明な縁」を pill フチに溶け込ませる
  // - 親 div を inset-0 で window 全体に展開
  // - pill 自身を w-full h-full で window 全体を埋める
  // - rounded-full → rounded-full で window 矩形と pill 端が一致
  // - 背景の透明度を 85% → 95% にして縁とのコントラストを下げる
  //
  // Wave 3: data-tauri-drag-region を pill に付けてドラッグ移動可能にする
  // ただし idle 状態のクリック (プロファイル切替) は pointer-events-auto に戻す

  // 待機中: Aqua Voice 風 ミニマル pill + ホットキー常時表示 + プロファイル表示
  // 2026-05-23 要望: マイクの線を太く (strokeWidth 2 → 2.5) + 文字色 真っ白
  if (state === 'idle') {
    const profileLabel = getActiveProfileLabel()

    // startDragging(): OS レベルでウィンドウ移動を処理する Tauri API
    // ウィンドウ外にカーソルが出ても動作する (pointer capture では不十分)
    // クリック vs ドラッグ判定: 3px 以内の動きはクリック扱い
    // プロファイル切替: profileLabelRef の上でクリックした時のみ
    function handlePointerDown(e: React.PointerEvent<HTMLDivElement>) {
      if (e.button !== 0) return
      e.preventDefault()

      const win = getCurrentWindow()
      const startX = e.screenX
      const startY = e.screenY
      const clickedOnProfile = profileLabelRef.current?.contains(e.target as Node) ?? false
      let dragStarted = false

      const el = e.currentTarget
      el.setPointerCapture(e.pointerId)

      function onPointerMove(pe: PointerEvent) {
        if (dragStarted) return
        const dx = pe.screenX - startX
        const dy = pe.screenY - startY
        if (Math.abs(dx) + Math.abs(dy) > 3) {
          dragStarted = true
          el.removeEventListener('pointermove', onPointerMove)
          el.removeEventListener('pointerup', onPointerUp)
          // OS 任せで window を動かす。終了後に位置を保存
          win.startDragging()
            .then(() => win.outerPosition())
            .then((pos) => saveOverlayPosition(pos.x, pos.y))
            .catch(() => {})
        }
      }

      function onPointerUp(_pe: PointerEvent) {
        el.removeEventListener('pointermove', onPointerMove)
        el.removeEventListener('pointerup', onPointerUp)
        if (!dragStarted && clickedOnProfile) {
          void handlePillClick()
        }
      }

      el.addEventListener('pointermove', onPointerMove)
      el.addEventListener('pointerup', onPointerUp)
    }

    return (
      <div className="fixed inset-0 flex">
        <div
          onPointerDown={(e) => { void handlePointerDown(e) }}
          className="bg-neutral-900/50 backdrop-blur-md text-white w-full h-full rounded-full flex items-center justify-center gap-1.5 shadow-md cursor-grab active:cursor-grabbing select-none"
        >
          <Mic size={11} strokeWidth={2.5} className="text-white" />
          <span className="text-[10px] font-medium tracking-wide whitespace-nowrap overflow-hidden">
            {hotkey || 'RushVoice'}
          </span>
          {profileLabel && (
            <span
              ref={profileLabelRef}
              className="text-[9px] text-neutral-200 border-l border-neutral-600 pl-1.5 ml-0.5 whitespace-nowrap cursor-pointer"
            >
              {profileLabel}
            </span>
          )}
        </div>
      </div>
    )
  }

  // 録音中/文字起こし中/完了/エラー: window 全体を埋める pill
  // 録音中表示は「N秒」のみ (アイコンで状態は自明)
  // v0.1.12 F2: recording + lowInputLevel のとき警告チップを横並びで追加表示
  return (
    <div className="fixed inset-0 flex pointer-events-none">
      <div
        data-tauri-drag-region
        className="bg-neutral-900/50 backdrop-blur-md text-white w-full h-full rounded-full flex items-center justify-center gap-1.5 shadow-lg"
      >
        <Icon state={state} />
        <div className="flex items-center gap-1.5">
          <p
            className={
              state === 'recording' && !micReady
                ? 'text-[10px] font-medium whitespace-nowrap truncate max-w-[150px] text-amber-300'
                : 'text-[10px] font-medium whitespace-nowrap truncate max-w-[150px]'
            }
          >
            {state === 'recording'
              ? micReady
                ? `${seconds}s`
                : 'マイク準備中…'
              : message}
          </p>
          {state === 'recording' && micReady && <WaveformBars values={waveform} />}
          {state === 'recording' && lowInputLevel && (
            <span
              role="status"
              aria-live="polite"
              className="flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-yellow-400/95 text-neutral-900 text-[9px] font-semibold whitespace-nowrap"
            >
              <AlertTriangle size={10} />
              マイク音量を確認
            </span>
          )}
          {/* v0.1.15: 文字起こし完了後の小声警告チップ (success pill に併記) */}
          {state === 'success' && quietVoice && (
            <span
              role="status"
              aria-live="polite"
              className="flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-yellow-400/95 text-neutral-900 text-[9px] font-semibold whitespace-nowrap"
            >
              <AlertTriangle size={10} />
              小声かも
            </span>
          )}
        </div>
      </div>
    </div>
  )
}

function Icon({ state }: { state: OverlayState }) {
  switch (state) {
    case 'recording':
      // 要望 (2026-05-22): もう少し薄い水色 (sky-400 → sky-300)
      return <Mic size={12} className="text-sky-300 animate-pulse" />
    case 'transcribing':
      return <Loader2 size={12} className="text-amber-400 animate-spin" />
    case 'success':
      return <CheckCircle2 size={12} className="text-emerald-400" />
    case 'error':
      return <AlertCircle size={12} className="text-red-400" />
    case 'cancelled':
      // HK-06: ESC キャンセル — neutral-400 で目立たず通知
      return <AlertCircle size={12} className="text-neutral-400" />
    default:
      return null
  }
}

function WaveformBars({ values }: { values: number[] }) {
  // recording 時のみ表示・幅 70px (要望 3 回目: さらに 100→70 に短縮)
  return (
    <div className="flex items-end gap-[1px] h-3 w-[70px]">
      {values.map((rms, i) => {
        // RMS は典型的に 0.001 ~ 0.1 → sqrt scale + 最小 15% で見やすく
        const scaled = Math.min(1, Math.max(0.15, Math.sqrt(rms * 30)))
        const heightPct = scaled * 100
        return (
          <div
            key={i}
            // 要望 (2026-05-22): 波形バーも赤 → 水色に
            className="w-[2px] rounded-full bg-sky-400/80"
            style={{ height: `${heightPct}%`, transition: 'height 80ms ease-out' }}
          />
        )
      })}
    </div>
  )
}
