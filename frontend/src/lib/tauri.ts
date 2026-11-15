/* tauri.ts — Phase 1 Day 6 Stage 1 Tauri invoke ラッパ
 *
 * 仕様: 内部設計メモ
 * 各関数は src-tauri/src/commands.rs の #[tauri::command] と 1:1 対応。
 *
 * 型付き wrapper にしておくことで、引数 / 戻り値の手書きミスを TS が捕まえる。
 */

import { invoke } from '@tauri-apps/api/core'
import type { AppRule } from '../types/app_rule'
import type { GlossaryEntry, OverlapResult } from '../types/glossary'
import type { Profile } from '../types/profile'

export type { AppRule } from '../types/app_rule'
export type { GlossaryEntry, OverlapResult } from '../types/glossary'
export type { Profile, EmojiDensity } from '../types/profile'

export type Provider = 'groq' | 'openai'

export interface ApiKeyTestResult {
  ok: boolean
  status: number | null
  message: string
}

// ----- settings_kv -----

export async function getSetting(key: string): Promise<string | null> {
  return await invoke<string | null>('get_setting', { key })
}

export async function setSetting(key: string, value: string): Promise<void> {
  await invoke('set_setting', { key, value })
}

// ----- API キー (DPAPI 暗号化) -----

export async function saveApiKey(provider: Provider, key: string): Promise<void> {
  await invoke('save_api_key', { provider, key })
}

export async function hasApiKey(provider: Provider): Promise<boolean> {
  return await invoke<boolean>('has_api_key', { provider })
}

export async function deleteApiKey(provider: Provider): Promise<void> {
  await invoke('delete_api_key', { provider })
}

// ----- API ping -----

export async function testApiKey(
  provider: Provider,
  key: string,
): Promise<ApiKeyTestResult> {
  return await invoke<ApiKeyTestResult>('test_api_key', { provider, key })
}

/// 保存済みキーで API ping (入力欄が空でもテストできる UX 補助)
export async function testStoredApiKey(
  provider: Provider,
): Promise<ApiKeyTestResult> {
  return await invoke<ApiKeyTestResult>('test_stored_api_key', { provider })
}

// ----- 録音セッション (Stage 2-A) -----

export type RecorderState = 'idle' | 'recording' | 'transcribing'

export async function startRecording(): Promise<void> {
  await invoke('start_recording')
}

/// 録音停止 → リサンプル → Groq 文字起こし → text を返す。
/// Groq キー未設定 / 文字起こし失敗時は Err。
export async function stopRecording(): Promise<string> {
  return await invoke<string>('stop_recording')
}

export async function getRecordingState(): Promise<RecorderState> {
  return await invoke<RecorderState>('get_recording_state')
}

// ----- ホットキー (Stage 2-C-1) -----

export type HotkeyMode = 'toggle' | 'pushtotalk'

export interface HotkeyConfig {
  code: string
  modifiers: string[]
  mode: HotkeyMode
}

export async function getHotkeyConfig(): Promise<HotkeyConfig> {
  return await invoke<HotkeyConfig>('get_hotkey_config')
}

export async function setHotkeyConfig(config: HotkeyConfig): Promise<void> {
  await invoke('set_hotkey_config', { config })
}

// ----- v0.1.10 機能 D: ホットキー競合検出 -----
//
// SSOT: 内部設計メモ
// Rust 側 src-tauri/src/hotkey_conflict.rs::ConflictState と 1:1 対応。
// serde tag="kind" + PascalCase で JSON 化されているため
// TS 側でも { kind: 'None' } | { kind: 'Conflict', ... } の discriminated union とする。
//
// イベント:
//   - 'hotkey:conflict-detected' (payload: ConflictState の Conflict variant)
//   - 'hotkey:conflict-cleared'  (payload: HotkeyConfig)
// は HotkeyConflictDialog.tsx と SettingsPage.tsx の HotkeySection が直接 listen する。

export type ConflictState =
  | { kind: 'None' }
  | {
      kind: 'Conflict'
      code: string
      modifiers: string[]
      detected_at: number
    }

/// 現在のホットキー競合状態を取得 (未保存なら { kind: 'None' })。
export async function getHotkeyConflictState(): Promise<ConflictState> {
  return await invoke<ConflictState>('get_hotkey_conflict_state')
}

// ----- Overlay window 動的サイズ (2026-05-22 要望・透明枠最小化) -----

export async function setOverlaySize(width: number, height: number, isIdle: boolean = false): Promise<void> {
  await invoke('set_overlay_size', { width, height, isIdle })
}

// ----- Autostart (Phase 1 Day 9 AUT-01・2026-05-23 / v0.1.12 F3 改訂) -----
//
// 実体: HKCU\Software\Microsoft\Windows\CurrentVersion\Run\RushVoice
//       (tauri.conf.json::productName 由来・実機検証済 2026-05-23。
//        2026-07-07 productName を "rushvoice-hello" → "RushVoice" にリネーム)
// 起動時引数: --minimized (AUT-01 P1-2 で追加・自動起動時はメイン window を初期 hide)
//
// v0.1.12 F3 (A 案) 改訂:
// - enable/disable は Rust 側ラッパ (`commands::enable_autostart` / `commands::disable_autostart`)
//   を経由する。理由 = plugin 呼び出しに加えて
//   `autostart_default_applied_v017_1` フラグ操作をセットで実施する必要があるため
//   (NSIS Uninstall 復元 vs ユーザー OFF 意図の両立)。
// - is_enabled は副作用なしのため plugin を直接呼ぶ (従来通り)。
//   tauri-plugin-autostart 2.x は frontend 側の TS SDK を入れなくても
//   `plugin:autostart|<cmd>` invoke 経由で呼べる (最小依存方針)。

export async function isAutostartEnabled(): Promise<boolean> {
  return await invoke<boolean>('plugin:autostart|is_enabled')
}

export async function enableAutostart(): Promise<void> {
  // v0.1.12 F3: Rust ラッパ経由 (plugin enable + flag set)
  await invoke('enable_autostart')
}

export async function disableAutostart(): Promise<void> {
  // v0.1.12 F3: Rust ラッパ経由 (plugin disable + flag delete = ユーザー OFF 意図尊重)
  await invoke('disable_autostart')
}

// ----- Phase 2 D: 履歴 (HistoryDb) -----
//
// 仕様: SSOT v2.10 §8.3.2 / 設計ドラフト phase2_d_history_sqlite_design_2026-05-22.md
// Rust 側 src-tauri/src/history_db.rs::HistoryItem と 1:1 対応。
// snake_case のフィールド名は Rust struct 由来 (serde はデフォルトでそのまま JSON 化)。
//
// 表示優先順位 (HistoryPage で利用):
//   edited_text (Phase 3 編集学習) > refined_text (LLM 整形後) > raw_text (Whisper 生)
// Phase 2 D 時点では raw_text == refined_text・edited_text は null。

export interface HistoryItem {
  /** SQLite AUTOINCREMENT rowid。INSERT 前は null。 */
  id: number | null
  /** RFC3339 / ISO8601 UTC */
  created_at: string
  /** 'groq' | 'openai' | 'hedged' */
  provider: string
  /** 'whisper-large-v3-turbo' 等 */
  model: string
  /** Phase 2 後半で利用 (dictation/command 等)。現状 null */
  mode: string | null
  /** Phase 3 AT-01 で前面アプリ名を入れる。現状 null */
  app_name: string | null
  /** Whisper 生テキスト */
  raw_text: string
  /** LLM 整形後 (Phase 3)。Phase 2 D は raw_text と同一 */
  refined_text: string
  /** ユーザー編集後 (Phase 3 編集学習) */
  edited_text: string | null
  /** JSON Patch 等の差分 (Phase 3) */
  edit_diff_json: string | null
  edit_count: number
  /** halu Pass で no_speech_prob 等から自動推定された低信頼フラグ */
  confidence_drop_flag: boolean
  duration_ms: number
  transcribe_ms: number
  audio_bytes: number
  avg_logprob: number | null
  no_speech_prob: number | null
}

/// 新しい順 (created_at DESC, id DESC) で limit/offset ページング取得。
export async function listHistory(
  limit: number,
  offset: number,
): Promise<HistoryItem[]> {
  return await invoke<HistoryItem[]>('list_history', { limit, offset })
}

/// 指定 ID 1 件取得。存在しなければ null。
export async function getHistory(id: number): Promise<HistoryItem | null> {
  return await invoke<HistoryItem | null>('get_history', { id })
}

/// Phase 3 編集学習 (Phase 2 D では UI 未接続)。
/// editDiffJson は空文字で渡すと Rust 側で None 扱い。
export async function updateHistoryEdit(
  id: number,
  editedText: string,
  editDiffJson: string = '',
): Promise<void> {
  await invoke('update_history_edit', { id, editedText, editDiffJson })
}

/// 1 件削除。存在しない id でも OK (楽観的更新と整合)。
export async function deleteHistoryItem(id: number): Promise<void> {
  await invoke('delete_history_item', { id })
}

/// 全件削除 (Settings「履歴を全削除」用)。
export async function clearHistory(): Promise<void> {
  await invoke('clear_history')
}

/// 総件数 (Settings 容量管理用)。
export async function historyCount(): Promise<number> {
  return await invoke<number>('history_count')
}

// ----- Phase 2 C Wave 1: 辞書 (GlossaryDb) -----
//
// 仕様: SSOT v2.10 §8.3.1 DT-01 / DT-02
// Rust 側 src-tauri/src/glossary_db.rs::GlossaryEntry と 1:1 対応。
// GlossaryEntry 型は冒頭で再 export 済。

/// 全件取得 (created_at ASC 安定順)。
export async function listGlossary(): Promise<GlossaryEntry[]> {
  return await invoke<GlossaryEntry[]>('list_glossary')
}

/// 語彙ヒント枠 (Whisper へ実際に送られる語) の使用状況。
/// 数値は Rust 側の build_glossary_phrase と同じ順序・同じ上限で算出される。
export interface GlossaryHintUsage {
  used_words: number
  max_words: number
  used_chars: number
  max_chars: number
  dropped_words: number
}

export async function glossaryHintUsage(): Promise<GlossaryHintUsage> {
  return await invoke<GlossaryHintUsage>('glossary_hint_usage')
}

/// entry.id == null なら INSERT・number なら UPDATE。最終 rowid を返す。
export async function upsertGlossaryEntry(entry: GlossaryEntry): Promise<number> {
  return await invoke<number>('upsert_glossary_entry', { entry })
}

/// 1 件削除。存在しない id でも OK (楽観的更新と整合)。
export async function deleteGlossaryEntry(id: number): Promise<void> {
  await invoke('delete_glossary_entry', { id })
}

/// 同 pattern の既存エントリを検索。excludeId を渡すと「自分自身」を除外。
export async function findGlossaryDuplicate(
  pattern: string,
  excludeId: number | null = null,
): Promise<GlossaryEntry | null> {
  return await invoke<GlossaryEntry | null>('find_glossary_duplicate', {
    pattern,
    excludeId,
  })
}

/// Step 2.5: 「/」区切り pattern の sub-pattern 単位重複検出。
/// 1 つのルールに複数誤変換を集約する UX 用 (要望)。
/// 例: 「たかなし/たかな」を新規追加・既存「たかなし」がある → [{entry, ["たかなし"]}] を返す。
export async function findGlossaryOverlapping(
  patternQuery: string,
  excludeId: number | null = null,
): Promise<OverlapResult[]> {
  return await invoke<OverlapResult[]>('find_glossary_overlapping', {
    patternQuery,
    excludeId,
  })
}

// ----- v0.1.10 機能 E: 辞書 CSV インポート/エクスポート -----
//
// SSOT: 内部設計メモ
// Rust 側 commands::ImportPreview / ImportResult と 1:1 対応。

/** CSV インポート時の重複処理モード */
export type ImportMode = 'Overwrite' | 'Skip' | 'ReplaceAll'

/** glossary_import_csv_preview の戻り値 */
export interface ImportPreview {
  total_in_csv: number
  /** (csv_entry, existing_entry) のペア */
  duplicates: [GlossaryEntry, GlossaryEntry][]
  new_entries: GlossaryEntry[]
  error_count: number
}

/** glossary_import_csv_commit の戻り値 */
export interface ImportResult {
  added: number
  updated: number
  skipped: number
  errors: number
}

/// 辞書 CSV エクスポート。savePath は plugin-dialog::save で得た絶対パス。戻り値は出力件数。
export async function glossaryExportCsv(savePath: string): Promise<number> {
  return await invoke<number>('glossary_export_csv', { savePath })
}

/// 辞書 CSV インポート プレビュー。openPath は plugin-dialog::open で得た絶対パス。
/// DB は変更されない (commit で初めて変更)。
export async function glossaryImportCsvPreview(openPath: string): Promise<ImportPreview> {
  return await invoke<ImportPreview>('glossary_import_csv_preview', { openPath })
}

/// 辞書 CSV インポート コミット。preview の結果 (new + duplicates の csv 側) を mode と共に渡す。
/// error_count は preview の値をそのまま引き継いで送る (戻り値の errors に反映される)。
export async function glossaryImportCsvCommit(
  entries: GlossaryEntry[],
  mode: ImportMode,
  errorCount: number,
): Promise<ImportResult> {
  return await invoke<ImportResult>('glossary_import_csv_commit', {
    entries,
    mode,
    errorCount,
  })
}

// ----- Phase 2 C Wave 1 Step 3: アプリ別ルール (AppRulesDb) AT-06 -----
//
// 仕様: SSOT v2.10 §8.3.1 AT-06
// Rust 側 src-tauri/src/app_rules_db.rs::AppRule と 1:1 対応。
// v1 互換: 「CellEndEdit 全件再構築保存」を replaceAllAppRules で表現。
//   AppsPage.tsx は onBlur で「現在の全行」を送る。

/// 全件取得 (created_at ASC 安定順)。
export async function listAppRules(): Promise<AppRule[]> {
  return await invoke<AppRule[]>('list_app_rules')
}

/// 単発 upsert (将来 Wave 2 で個別行編集 UI 用)。Wave 1 では replaceAllAppRules が主。
/// rule.id == null なら INSERT・number なら UPDATE。最終 rowid を返す。
export async function upsertAppRule(rule: AppRule): Promise<number> {
  return await invoke<number>('upsert_app_rule', { rule })
}

/// 1 件削除。存在しない id でも OK (楽観的更新と整合)。
export async function deleteAppRule(id: number): Promise<void> {
  await invoke('delete_app_rule', { id })
}

/// 全件再構築保存 (v1 saveAllRows 互換)。AppsPage onBlur で「現在の全行」を送る。
/// 完全空行は Rust 側で除外される。返り値は新 id 付きの全行 (saving 後 setState 用)。
export async function replaceAllAppRules(rules: AppRule[]): Promise<AppRule[]> {
  return await invoke<AppRule[]>('replace_all_app_rules', { rules })
}

// ----- Phase 2 C Wave 2: プロファイル (ProfileDb) PT-01〜PT-10 -----
//
// 仕様: SSOT v2.10 §8.3.1 PT-01〜PT-10
// Rust 側 src-tauri/src/profile_db.rs::Profile と 1:1 対応。
// ビルトイン 3 種 (salon / accounting / meeting) は初回 open 時に投入される。
//
// 制約:
//   - ビルトインは削除不可 (Rust 側で拒否・UI でも防ぐ)
//   - llm_postprocess_prompt は最大 1200 文字 (Rust 側バリデート)
//   - emoji_density は 'none'|'few'|'normal'|'many' のいずれか

/// 全件取得 (ビルトイン優先・その後 created_at ASC)。
export async function listProfiles(): Promise<Profile[]> {
  return await invoke<Profile[]>('list_profiles')
}

/// id 指定で 1 件取得 (現状 ProfilePage 未使用・将来用)。
export async function getProfile(id: number): Promise<Profile | null> {
  return await invoke<Profile | null>('get_profile', { id })
}

/// PT-09 保存ボタン。profile.id == null なら INSERT・number なら UPDATE。
/// 1200 文字超等のバリデーション失敗時は文字列 Err。
export async function upsertProfile(profile: Profile): Promise<number> {
  return await invoke<number>('upsert_profile', { profile })
}

/// 削除。ビルトイン削除は Rust 側で拒否される (Err 文字列)。
export async function deleteProfile(id: number): Promise<void> {
  await invoke('delete_profile', { id })
}

/// PT-10「デフォルトに戻す」。ビルトインのみ呼出可能 (user 作成プロファイルは Err)。
export async function resetProfileToDefault(id: number): Promise<void> {
  await invoke('reset_profile_to_default', { id })
}

// ----- Phase 2 A: アクティブプロファイル (in-memory State + config_db 永続化) -----
//
// アクティブプロファイルは録音完了後の LLM 後処理に使用される。
// デフォルト: "raw" (後処理なし)。変更は config_db にも永続化されるため再起動後も保持される。
// overlay クリック切替 (Wave 3) でも同コマンドを利用する。

/// 現在のアクティブプロファイルキーを取得。
export async function getActiveProfileKey(): Promise<string | null> {
  try {
    return await invoke<string>('get_active_profile_key')
  } catch {
    return null
  }
}

/// アクティブプロファイルキーをセット (in-memory + config_db 永続化)。
export async function setActiveProfileKey(key: string): Promise<void> {
  try {
    await invoke('set_active_profile_key', { key })
  } catch {
    // 保存失敗は無視
  }
}

// ----- Wave 3: overlay window 位置保存 -----
//
// overlay ピルのドラッグ後位置を config_db に保存する。
// get_setting / set_setting の既存コマンドを流用する (Rust 追加実装不要)。
//
// キー: "overlay_pos_x" / "overlay_pos_y" (物理ピクセル)

export async function getOverlayPosition(): Promise<{ x: number; y: number } | null> {
  try {
    const [xs, ys] = await Promise.all([
      invoke<string | null>('get_setting', { key: 'overlay_pos_x' }),
      invoke<string | null>('get_setting', { key: 'overlay_pos_y' }),
    ])
    if (xs == null || ys == null) return null
    const x = parseInt(xs, 10)
    const y = parseInt(ys, 10)
    if (isNaN(x) || isNaN(y)) return null
    return { x, y }
  } catch {
    return null
  }
}

export async function saveOverlayPosition(x: number, y: number): Promise<void> {
  try {
    await Promise.all([
      invoke('set_setting', { key: 'overlay_pos_x', value: String(Math.round(x)) }),
      invoke('set_setting', { key: 'overlay_pos_y', value: String(Math.round(y)) }),
    ])
  } catch {
    // 保存失敗は無視 (次回起動時にデフォルト位置になるだけ)
  }
}

// ----- v0.1.10 機能 A: 混雑通知バナー設定 -----
//
// SSOT: 内部設計メモ
// Rust 側 commands::CongestionConfigDto と 1:1 対応。
// バナーイベント (transcribe:congestion-started / -cleared) は
// CongestionBanner.tsx で直接 listen するためここでは bridge を提供しない。

export interface CongestionConfig {
  /** 混雑バナー発動閾値 (ms)・1000-30000 範囲・default 5000 */
  threshold_ms: number
  /** hedged プロバイダー時もバナーを表示するか (default false = hedged 抑止) */
  show_in_hedged: boolean
}

/// 現在の設定値を取得 (未設定なら default = 5000ms / hedged 抑止)。
export async function getCongestionConfig(): Promise<CongestionConfig> {
  return await invoke<CongestionConfig>('get_congestion_config')
}

/// 設定値を保存。範囲外 (< 1000 or > 30000) は Rust 側でクランプされる。
export async function setCongestionConfig(cfg: CongestionConfig): Promise<void> {
  await invoke('set_congestion_config', { cfg })
}

// ----- Gateway ログイン (feature/gateway-login) -----
//
// 設計 SSOT: 内部設計メモ §A-2
// Rust 側 src-tauri/src/gateway_auth.rs と 1:1 対応。
//
// イベント:
//   - 'gateway:auth-changed' (payload: GatewayStatus)
//       ログイン成功 / ログアウト時に Rust 側から emit される (L145: invoke + emit 二重通知)。
//       SettingsPage の AccountSection が listen する。
//   - 'gateway:entitlement-error' (payload: { status: number, message: string })
//       ゲートウェイ経由の転写で 403/429 を受けたとき。
//       GatewayEntitlementBanner.tsx が listen する (L337: alert 禁止・インラインバナー)。

export interface GatewayStatus {
  /** logged_out=JWT 未保存 / active=有効 / expired=JWT 失効 (再ログイン要) / error=一時エラー */
  state: 'logged_out' | 'active' | 'expired' | 'error'
  email: string | null
  /** 'trial' 等 */
  plan: string | null
  /** 'active' 等 (entitlement.status) */
  status: string | null
  /** RFC3339 (プラン有効期限) */
  current_period_end: string | null
  message: string | null
}

export interface GatewayEntitlementError {
  status: number
  /// 'login_expired' = ログイン切れ (再ログインで直る・購入は不要) / 'entitlement' = 契約・上限
  reason?: 'login_expired' | 'entitlement'
  message: string
}

/// メールにワンタイムコード (6 桁・10 分有効) を送付する。
export async function gatewayRequestCode(email: string): Promise<void> {
  await invoke('gateway_request_code', { email })
}

/// コードを検証してログイン。成功時: JWT 保存 + 転写経路が gateway に自動切替。
export async function gatewayVerifyCode(
  email: string,
  code: string,
): Promise<GatewayStatus> {
  return await invoke<GatewayStatus>('gateway_verify_code', { email, code })
}

/// 保存済 JWT で現在の認証状態を取得 (JWT 未保存なら logged_out)。
export async function gatewayStatus(): Promise<GatewayStatus> {
  return await invoke<GatewayStatus>('gateway_status')
}

/// ログアウト: JWT 削除 + 転写経路が byok に戻る。
export async function gatewayLogout(): Promise<void> {
  await invoke('gateway_logout')
}

/** アプリから購入できるプラン (ゲートウェイの `plan` と 1:1)。 */
export type PurchasablePlan = 'standard' | 'pro'

/** 購入導線の表示用メタ (価格は 2026-07-27 決定)。 */
export const PLAN_CATALOG: { plan: PurchasablePlan; label: string; price: string }[] = [
  { plan: 'standard', label: 'スタンダード', price: '¥980/月' },
  { plan: 'pro', label: 'プロ', price: '¥1,980/月' },
]

/// Stripe Checkout を既定ブラウザで開く。戻り値は開いた URL (手動コピー用)。
/// 要ログイン (JWT は Rust 側に留まり frontend へは渡らない)。
export async function gatewayOpenCheckout(plan: PurchasablePlan): Promise<string> {
  return await invoke<string>('gateway_open_checkout', { plan })
}

/** 上位プラン限定機能 (議事録 / セミナー字幕) の利用可否。 */
export interface ProFeatureAccess {
  allowed: boolean
  /** 判定に使ったプラン (未ログインなら null) */
  plan: string | null
  /** false = BYOK 運用 (未ログイン) */
  logged_in: boolean
}

/// プラン別ゲートの判定 (config_db の写しで即時に返る・ネットワーク待ちなし)。
export async function gatewayProFeatureAccess(): Promise<ProFeatureAccess> {
  return await invoke<ProFeatureAccess>('gateway_pro_feature_access')
}

// Day 25 Phase 2-G: ASR 補正レイヤー (alias_db / EditDiff / applyAliasFromEditDiff) は
// SSOT v3.3 に基づき完全削除済 (副作用検証で実用不可と判定・Day 24 commit 47d77d3 撤回)。
// v0.1.7 では glossary_db (用語集) のみで運用する。

// ----- マイク選択 GUI (2026-07-23 要望) -----

export interface InputDeviceList {
  /** 入力デバイス名一覧 (cpal 列挙) */
  devices: string[]
  /** Windows 既定入力デバイス名 (取得失敗時 null) */
  default_name: string | null
}

/// 入力デバイス一覧と既定デバイス名を取得。選択値は settings_kv "input_device_name" に保存。
export async function listInputDevices(): Promise<InputDeviceList> {
  return await invoke<InputDeviceList>('list_input_devices')
}

// ----- マイク常時スタンバイ (2026-07-23 要望・Shokz冒頭切れ根本対策) -----

/// 常時スタンバイの現在値 (settings_kv "mic_standby")。
export async function getMicStandby(): Promise<boolean> {
  return await invoke<boolean>('get_mic_standby')
}

/// 常時スタンバイを設定し即時適用 (スタンバイストリームの張り直し)。
export async function setMicStandby(enabled: boolean): Promise<void> {
  await invoke('set_mic_standby', { enabled })
}

/// マイク選択変更後に呼ぶ: スタンバイONなら新デバイスで張り直す。
export async function applyMicStandby(): Promise<void> {
  await invoke('apply_mic_standby')
}
