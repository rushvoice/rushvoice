// commands.rs — Phase 1 Day 6 Stage 1: Settings UI 接続用 Tauri command 6 個
//
// 仕様: 内部設計メモ
// - get_setting / set_setting        ... settings_kv (config_db)
// - save_api_key / has_api_key / delete_api_key ... DPAPI 暗号化 (crypto)
// - test_api_key / test_endpoint_url ... 接続先の /models に軽い ping (5 秒 timeout)
//
// エラーは String にフラット化して invoke で扱いやすくする (Tauri 2.x 標準)。

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

// Day 25 Phase 2-G (SSOT v3.3): alias_db imports は ASR 補正レイヤー撤廃と共に削除
use crate::app_rules_db::{AppRule, AppRulesDb};
use crate::config_db::{default_config_db_path, ConfigDb};
use crate::crypto::{default_secrets_path, load_secrets, save_secrets};
use crate::glossary_csv::{export_to_writer, parse_csv};
use crate::glossary_db::{GlossaryDb, GlossaryEntry, OverlapResult};
use crate::history_db::{HistoryDb, HistoryItem};
use crate::hotkey::{code_to_vk, parse_modifier_side, HotkeyConfig, HOTKEY_CONFIG_KEY};
use crate::hotkey_conflict::{probe_conflict, ConflictState};
use crate::llm_postprocess::apply_llm_postprocess;
use crate::profile_db::{Profile, ProfileDb};
use crate::recorder::{
    start_session, stop_session_and_transcribe, CongestionConfig, MicLowLevelConfig,
    RecorderHolder, RecorderState, TranscribeOutcome,
};
use crate::{ActiveProfileKey, HookHolder};

/// Phase 2 A: アクティブプロファイルのキー定数 (config_db KV)
const ACTIVE_PROFILE_KEY_KV: &str = "active_profile_key";

/// キーの保存先。カスタム接続先は文字起こし用と整文用で別々に持つ
/// (例: 文字起こしは手元の whisper.cpp、整文は Groq、のような組み合わせを許す)。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Groq,
    Openai,
    CustomTranscribe,
    CustomLlm,
}

impl Provider {
    fn slot(self, b: &mut crate::crypto::SecretsBundle) -> &mut Option<String> {
        match self {
            Provider::Groq => &mut b.groq_api_key,
            Provider::Openai => &mut b.openai_api_key,
            Provider::CustomTranscribe => &mut b.custom_transcribe_api_key,
            Provider::CustomLlm => &mut b.custom_llm_api_key,
        }
    }

    /// 接続テストに使うベース URL。カスタムは保存済みの設定から読む。
    fn base_url(self) -> String {
        match self {
            Provider::Groq => crate::endpoints::GROQ_BASE_URL.to_string(),
            Provider::Openai => crate::endpoints::OPENAI_BASE_URL.to_string(),
            Provider::CustomTranscribe | Provider::CustomLlm => {
                let key = if self == Provider::CustomTranscribe {
                    crate::endpoints::CUSTOM_TRANSCRIBE_BASE_URL_KEY
                } else {
                    crate::endpoints::CUSTOM_LLM_BASE_URL_KEY
                };
                ConfigDb::open(&default_config_db_path())
                    .ok()
                    .and_then(|db| db.get_kv(key).ok().flatten())
                    .unwrap_or_default()
            }
        }
    }
}

pub type ApiKeyTestResult = crate::endpoints::EndpointTestResult;

fn load_bundle() -> Result<crate::crypto::SecretsBundle, String> {
    Ok(load_secrets(&default_secrets_path())
        .map_err(|e| e.to_string())?
        .unwrap_or_default())
}

// ----- settings_kv -----

#[tauri::command]
pub fn get_setting(key: String) -> Result<Option<String>, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    db.get_kv(&key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_setting(key: String, value: String) -> Result<(), String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    db.set_kv(&key, &value).map_err(|e| e.to_string())
}

// ----- マイク選択 GUI (2026-07-23 要望) -----

/// 設定画面のマイク選択ドロップダウン用。入力デバイス名一覧と Windows 既定デバイス名を返す。
/// 選択値の保存/読込は既存の get_setting/set_setting (key = "input_device_name") を使う。
#[derive(serde::Serialize)]
pub struct InputDeviceList {
    pub devices: Vec<String>,
    pub default_name: Option<String>,
}

#[tauri::command]
pub fn list_input_devices() -> InputDeviceList {
    let (devices, default_name) = crate::audio::list_input_device_names();
    InputDeviceList {
        devices,
        default_name,
    }
}

// ----- マイク常時スタンバイ (2026-07-23 要望・Shokz 冒頭切れの根本対策) -----

/// settings_kv キー。"1" = 常時スタンバイ ON。
pub const MIC_STANDBY_KV_KEY: &str = "mic_standby";

/// kv とマイク選択の現状に合わせてスタンバイストリームを張り直す (冪等)。
/// 呼ぶ場面: 起動時 / トグル変更時 / マイク選択変更時。
pub(crate) fn apply_mic_standby_state(app: &AppHandle) {
    let state = match app.try_state::<Mutex<Option<crate::audio::MicStandby>>>() {
        Some(s) => s,
        None => {
            log::warn!("[mic_standby] state not managed; skip");
            return;
        }
    };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("[mic_standby] lock poisoned: {}; skip", e);
            return;
        }
    };
    if let Some(old) = guard.take() {
        old.stop();
    }
    let enabled = ConfigDb::open(&default_config_db_path())
        .ok()
        .and_then(|db| db.get_kv(MIC_STANDBY_KV_KEY).ok().flatten())
        .map(|v| v == "1")
        .unwrap_or(false);
    if enabled {
        let preferred = crate::recorder::preferred_input_device();
        log::info!("[mic_standby] starting standby stream (preferred={:?})", preferred);
        *guard = Some(crate::audio::MicStandby::start(preferred));
    } else {
        log::info!("[mic_standby] standby disabled");
    }
}

#[tauri::command]
pub fn get_mic_standby() -> Result<bool, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    Ok(db
        .get_kv(MIC_STANDBY_KV_KEY)
        .map_err(|e| e.to_string())?
        .map(|v| v == "1")
        .unwrap_or(false))
}

#[tauri::command]
pub fn set_mic_standby(app: AppHandle, enabled: bool) -> Result<(), String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    db.set_kv(MIC_STANDBY_KV_KEY, if enabled { "1" } else { "0" })
        .map_err(|e| e.to_string())?;
    apply_mic_standby_state(&app);
    Ok(())
}

/// マイク選択変更後にフロントから呼ぶ: スタンバイ ON なら新デバイスで張り直す。
#[tauri::command]
pub fn apply_mic_standby(app: AppHandle) -> Result<(), String> {
    apply_mic_standby_state(&app);
    Ok(())
}

// ----- API キー (DPAPI 暗号化) -----

/// 既存ファイルを読み出し、指定 provider のみ key で上書きして再保存する。
/// 他のキーは復号 → 再暗号化されるが、平文は invoke 境界を越えず Rust 内で完結する。
#[tauri::command]
pub fn save_api_key(provider: Provider, key: String) -> Result<(), String> {
    let mut bundle = load_bundle()?;
    let key = key.trim().to_string();
    *provider.slot(&mut bundle) = if key.is_empty() { None } else { Some(key) };
    save_secrets(&default_secrets_path(), &bundle).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn has_api_key(provider: Provider) -> Result<bool, String> {
    let mut bundle = load_bundle()?;
    Ok(provider.slot(&mut bundle).is_some())
}

#[tauri::command]
pub fn delete_api_key(provider: Provider) -> Result<(), String> {
    let mut bundle = load_bundle()?;
    *provider.slot(&mut bundle) = None;
    save_secrets(&default_secrets_path(), &bundle).map_err(|e| e.to_string())
}

// ----- 接続テスト ({base}/models に軽く GET) -----

#[tauri::command]
pub async fn test_api_key(provider: Provider, key: String) -> Result<ApiKeyTestResult, String> {
    Ok(crate::endpoints::test_endpoint(&provider.base_url(), &key).await)
}

/// 保存済み API キーで試す。入力欄が空でもテストできるようにする UX 補助。
/// カスタム接続先はキーが無くてもよい (ローカルサーバー)。
#[tauri::command]
pub async fn test_stored_api_key(provider: Provider) -> Result<ApiKeyTestResult, String> {
    let mut bundle = load_bundle()?;
    let key = provider.slot(&mut bundle).clone();
    let key = match (provider, key) {
        (_, Some(k)) => k,
        (Provider::CustomTranscribe | Provider::CustomLlm, None) => String::new(),
        (_, None) => return Err("保存済みキーがありません".to_string()),
    };
    Ok(crate::endpoints::test_endpoint(&provider.base_url(), &key).await)
}

/// カスタム接続先を、画面に入力中の URL で試す (保存前でも試せるように)。
/// key が空なら保存済みのキー (無ければキー無し) を使う。
#[tauri::command]
pub async fn test_endpoint_url(
    provider: Provider,
    base_url: String,
    key: Option<String>,
) -> Result<ApiKeyTestResult, String> {
    let key = match key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
        Some(k) => k,
        None => {
            let mut bundle = load_bundle()?;
            provider.slot(&mut bundle).clone().unwrap_or_default()
        }
    };
    Ok(crate::endpoints::test_endpoint(&base_url, &key).await)
}

/// 旧版の有料アカウント (ゲートウェイ) 終了の案内を出すか。
#[tauri::command]
pub fn get_gateway_retired_notice() -> Result<bool, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    Ok(db
        .get_kv(crate::endpoints::GATEWAY_RETIRED_NOTICE_KEY)
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some("1"))
}

/// 案内を閉じる (以後出さない)。
#[tauri::command]
pub fn dismiss_gateway_retired_notice() -> Result<(), String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    db.set_kv(crate::endpoints::GATEWAY_RETIRED_NOTICE_KEY, "0")
        .map_err(|e| e.to_string())
}

// ----- 録音セッション (Stage 2-A・Stage 2-C-2 で波形 emit 追加) -----

#[tauri::command]
pub fn start_recording(
    app: AppHandle,
    holder: State<'_, RecorderHolder>,
) -> Result<(), String> {
    log::info!("[cmd] start_recording invoked");
    // Stage 2-C-2: AppHandle を渡すと録音中に recorder:waveform イベントが発火される
    // v0.1.12 F2: マイク入力低レベル警告設定を config_db から読み込んで worker に渡す
    let mic_low_level = load_mic_low_level_config_or_default();
    match start_session(&holder, Some(app.clone()), mic_low_level) {
        Ok(_) => log::info!("[cmd] start_session OK"),
        Err(e) => {
            log::error!("[cmd] start_session failed: {}", e);
            // overlay にエラー表示 (toast 削除した分の代替フィードバック)
            if let Some(overlay) = app.get_webview_window("overlay") {
                let _ = overlay.show();
            }
            let _ = app.emit("recorder:error", e.clone());
            return Err(e);
        }
    }
    if let Some(overlay) = app.get_webview_window("overlay") {
        match overlay.show() {
            Ok(_) => log::info!("[cmd] overlay.show OK"),
            Err(e) => log::error!("[cmd] overlay.show failed: {:?}", e),
        }
    } else {
        log::error!("[cmd] overlay window not found (get_webview_window returned None)");
    }
    let _ = app.emit("recorder:state-changed", "recording");
    Ok(())
}

#[tauri::command]
pub async fn stop_recording(
    app: AppHandle,
    holder: State<'_, RecorderHolder>,
) -> Result<String, String> {
    // 接続先 (Groq / OpenAI / カスタム) を先に決める。未設定なら録音を書き起こせないので止める。
    let setup = crate::endpoints::load_transcribe_setup()?;

    // v0.1.3 patch2 (C): GlossaryDb から固有名詞を fetch して Whisper prompt 末尾に注入。
    // best-effort: 取得失敗 / state 未管理は空 Vec で続行 (従来通り固定 prompt 動作)。
    // v0.1.3 patch3 (C-fix): glossary_words の中身は replacement 列に変更済 (commands.rs::collect_glossary_words)
    let glossary_words = collect_glossary_words(&app);
    // v0.1.3 patch3 (C-add): 後処理 pattern→replacement 置換用に entries も別途取得
    let glossary_entries = collect_glossary_entries(&app);
    log::info!(
        "[stop_recording] glossary_words count={} (first 5: {:?}) entries count={}",
        glossary_words.len(),
        glossary_words.iter().take(5).collect::<Vec<_>>(),
        glossary_entries.len()
    );

    let _ = app.emit("recorder:state-changed", "transcribing");
    // Day 25 Phase 2-G (SSOT v3.3): multi pipeline 経路削除に伴い legacy 直接呼出に簡素化
    // v0.1.9 (Day 28): hedged 本番配線。provider 設定 + OpenAI key を読み込んで stop_session_and_transcribe に渡す
    // v0.1.10 機能 A: 混雑バナー設定を config_db から読み込んで recorder に渡す
    let congestion = load_congestion_config_or_default();
    match stop_session_and_transcribe(
        &holder,
        setup,
        glossary_words,
        glossary_entries,
        Some(app.clone()),
        congestion,
    ).await {
        Ok(outcome) => {
            // 2026-07-25: LLM 後処理後テキスト (履歴 refined_text 用)。後処理を通らない経路では None。
            let mut refined_for_history: Option<String> = None;
            // v0.1.3 防御第 2 層: outcome.text が空 + confidence_drop_flag=1 のとき clipboard 貼付を抑止。
            // UI には警告イベント recorder:halu-suppressed を emit してトースト表示させる。
            if outcome.text.is_empty() && outcome.confidence_drop_flag {
                log::warn!("[paste] suppressed: confidence_drop_flag=1, raw_text retained in history");
                let _ = app.emit(
                    "recorder:halu-suppressed",
                    "⚠️ ハルシネーション検出のため貼付を中止しました (履歴から手動コピー可)",
                );
            } else if !outcome.text.is_empty() {
                // Phase 2 A: アクティブプロファイルで LLM 後処理
                let final_text = apply_profile_postprocess(&app, &outcome.text).await;
                // L058: clipboard コピー → SendInput(Ctrl+V) でアクティブアプリにペースト (PA-02)
                if let Err(e) = crate::clipboard::paste_or_type_text(&final_text) {
                    log::warn!("[paste] failed: {}", e);
                }
                // 2026-07-25: 後処理後テキストを履歴の refined_text に残す (診断用)
                refined_for_history = Some(final_text.clone());
                // v0.1.15 小声警告: 貼付は実行した上で、小声なら警告 emit + baseline 学習更新。
                // v0.1.16 機能 L: 通常録音経路なので RecorderMode::Normal を渡す。
                handle_quiet_voice_warning(
                    &app,
                    outcome.session_rms_mean,
                    crate::recorder::RecorderMode::Normal,
                );
            }
            // Phase 2 D: 履歴 INSERT (best-effort・失敗しても録音フロー継続)
            // 設計ドラフト §6 / SSOT §8.3.2.5: halu Pass + clipboard 貼付直後 (PA-02 復元前)
            insert_history_best_effort(&app, &outcome, refined_for_history.as_deref());
            // 履歴表示や HomePage の「最新の文字起こし」には raw_text_for_history を使う
            // (text が空でも UI で確認できるようにするため)
            let display_text = outcome.raw_text_for_history.clone();
            let _ = app.emit("recorder:state-changed", "idle");
            let _ = app.emit("recorder:transcribed", display_text.clone());
            Ok(display_text)
        }
        Err(e) => {
            let _ = app.emit("recorder:state-changed", "idle");
            let _ = app.emit("recorder:error", e.clone());
            Err(e)
        }
    }
}

/// v0.1.3 patch3 (C-fix): GlossaryDb State から **replacement 列**を全件取得する。
///
/// patch2 までは pattern 列を渡していたが、それは方向ミスだった:
///   pattern   = Whisper が誤変換しがちな表記 (例: "松江区", "松葉", "春氏")
///   replacement = 利用者が本来意図した固有名詞   (例: "マツエク", "マツパ", "ハルシ")
/// Whisper prompt には「正しい固有名詞リスト」を渡すべきなので replacement を採用する。
///
/// 後処理での pattern → replacement 置換は `collect_glossary_entries` + recorder.rs の
/// `apply_glossary_replacements` で別途行う (C-add)。
///
/// best-effort: GlossaryDb state 未管理・lock 失敗・list 失敗いずれも警告ログのみで空 Vec を返す。
/// 取得結果は recorder.rs::build_dynamic_whisper_prompt に渡され、Whisper prompt 末尾に
/// 「固有名詞: word1, word2, ...」として注入される。
///
/// 単語数上限の最終的なバジェット制御は recorder.rs::build_glossary_phrase
/// (WHISPER_PROMPT_GLOSSARY_MAX_CHARS = 180 文字) で行うため、ここでは件数制限しない。
///
/// L526 (2026-07-23): 戻り値は **新規登録優先 (created_at 降順)**。180 字上限で
/// 詰め切れない分は古い語から圏外にする (字幕翻訳プロンプト側 Day2 と同方針)。
pub(crate) fn collect_glossary_words(app: &AppHandle) -> Vec<String> {
    let state = match app.try_state::<Mutex<GlossaryDb>>() {
        Some(s) => s,
        None => {
            log::warn!("[glossary] GlossaryDb state not managed; whisper prompt will be unchanged");
            return Vec::new();
        }
    };
    let db = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("[glossary] lock poisoned: {}; whisper prompt will be unchanged", e);
            return Vec::new();
        }
    };
    match db.list() {
        Ok(entries) => {
            let pairs: Vec<(String, String)> = entries
                .into_iter()
                // L526: list() は created_at ASC → 新規優先に反転 (180 字上限対策)
                .rev()
                // v0.1.3 patch3 (C-fix): pattern → replacement に変更 (方向反転)
                .map(|e| (e.pattern, e.replacement))
                .collect();
            // 2026-07-25: 置換で直せる語より「候補提示しか手のない語」を先に枠へ詰める
            crate::recorder::prioritize_glossary_words(&pairs)
        }
        Err(e) => {
            log::warn!("[glossary] list failed: {:?}; whisper prompt will be unchanged", e);
            Vec::new()
        }
    }
}

/// 2026-07-27: 後処理 LLM へ渡す「取り違えられやすい実在語」を辞書から集める。
///
/// 対象は `pattern == replacement` の登録語 (例「品川→品川」「店舗→店舗」)。
/// これは置換では直せない衝突語として登録される慣習で、
/// `prioritize_glossary_words` が Whisper prompt 用に優先する集合と同じもの。
///
/// `pattern != replacement` の置換ルールは後処理の置換で確実に直るため渡さない
/// (毎回送る固定トークンを増やさない)。
///
/// L526: 新規登録優先 (created_at 降順) で返す。上限超過分の切り捨ては
/// `build_confusable_hint` 側で WARN と共に行う。
pub(crate) fn collect_confusable_words(app: &AppHandle) -> Vec<String> {
    let state = match app.try_state::<Mutex<GlossaryDb>>() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let db = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("[glossary] lock poisoned: {}; confusable hint will be empty", e);
            return Vec::new();
        }
    };
    match db.list() {
        Ok(entries) => entries
            .into_iter()
            .rev() // list() は created_at ASC → 新規優先に反転 (L526)
            .filter(|e| !e.replacement.trim().is_empty() && e.pattern.trim() == e.replacement.trim())
            .map(|e| e.replacement)
            .collect(),
        Err(e) => {
            log::warn!("[glossary] list failed: {:?}; confusable hint will be empty", e);
            Vec::new()
        }
    }
}

/// v0.1.3 patch3 (C-add): GlossaryDb State から (pattern, replacement, is_regex) を全件取得する。
///
/// `collect_glossary_words` (C-fix で replacement 列に変更) と独立して、後処理の
/// pattern → replacement 置換のために pattern / replacement / is_regex を 1 タプルで返す。
///
/// best-effort: いずれの失敗ケースでも空 Vec を返し、置換処理はスキップ (text は無加工で通る)。
/// 空 pattern (= ヒットしようがない) は除外する。replacement は空でも許可
/// (= 「pattern を空文字列に置換 = 削除」用途)。
pub(crate) fn collect_glossary_entries(app: &AppHandle) -> Vec<(String, String, bool)> {
    let state = match app.try_state::<Mutex<GlossaryDb>>() {
        Some(s) => s,
        None => {
            log::warn!("[glossary] GlossaryDb state not managed; post-processing replacements skipped");
            return Vec::new();
        }
    };
    let db = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("[glossary] lock poisoned: {}; post-processing replacements skipped", e);
            return Vec::new();
        }
    };
    match db.list() {
        Ok(entries) => entries
            .into_iter()
            .filter(|e| !e.pattern.trim().is_empty())
            .map(|e| (e.pattern, e.replacement, e.is_regex))
            .collect(),
        Err(e) => {
            log::warn!("[glossary] list failed: {:?}; post-processing replacements skipped", e);
            Vec::new()
        }
    }
}

/// Phase 2 D: TranscribeOutcome から HistoryItem を組み立て、HistoryDb State に INSERT。
///
/// best-effort: State 未管理・lock 失敗・INSERT 失敗いずれも warn ログのみで録音フロー継続。
/// `app_name` は AT-01 未実装のため None。`mode` も Phase 2 D 時点では None。
/// commands.rs / lib.rs の transcribe 完了経路 2 箇所から共通利用するための pub(crate) ヘルパー。
///
/// 2026-07-25 報告 (診断不能): 従来は raw_text / refined_text の両方に
/// `raw_text_for_history` (= LLM 後処理**前**) を入れており、後処理の結果が
/// どこにも残っていなかった。そのため L583 の切り分け手順 (raw と refined を
/// 比較して Whisper 段か LLM 段かを判定する) が実行できなかった。
/// `refined` に後処理後テキストを渡すこと。None のときは従来通り前段テキストを入れる
/// (ハルシ抑止で貼付しなかった経路など、後処理を通っていない場合)。
pub(crate) fn insert_history_best_effort(
    app: &AppHandle,
    outcome: &TranscribeOutcome,
    refined: Option<&str>,
) {
    let state = match app.try_state::<Mutex<HistoryDb>>() {
        Some(s) => s,
        None => {
            log::warn!("[history] HistoryDb state not managed; skip insert");
            return;
        }
    };
    let db = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("[history] HistoryDb lock poisoned: {}; skip insert", e);
            return;
        }
    };
    let item = HistoryItem {
        id: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        provider: outcome.provider.clone(),
        model: outcome.model.clone(),
        mode: None,
        app_name: None,
        // v0.1.3: outcome.text は confidence_drop_flag=1 時に空文字列化される (貼付抑止のため)。
        //         raw_text には LLM 後処理**前** (Whisper + 辞書置換後) を残す。
        raw_text: outcome.raw_text_for_history.clone(),
        // 2026-07-25: 後処理**後**の最終テキスト。両者の差分が LLM 段の診断材料になる。
        refined_text: refined
            .map(|s| s.to_string())
            .unwrap_or_else(|| outcome.raw_text_for_history.clone()),
        edited_text: None,
        edit_diff_json: None,
        edit_count: 0,
        confidence_drop_flag: outcome.confidence_drop_flag,
        duration_ms: outcome.duration_ms,
        transcribe_ms: outcome.transcribe_ms,
        audio_bytes: outcome.audio_bytes,
        avg_logprob: outcome.avg_logprob,
        no_speech_prob: outcome.no_speech_prob,
        // v0.1.9: hedged 計測カラム (solo 経路は全て None)
        primary_ms: outcome.primary_ms,
        fallback_triggered: outcome.fallback_triggered,
        hedged_winner: outcome.hedged_winner.clone(),
    };
    match db.insert(&item) {
        Ok(id) => {
            log::info!("[history] inserted id={} transcribe_ms={}", id, outcome.transcribe_ms);
            // Task #7: HistoryPage 自動更新用に event 発火 (best-effort)。
            // payload は新規 INSERT id (デバッグ用)。フロントは payload を見ずに refetch するだけ。
            if let Err(e) = app.emit("history:updated", id) {
                log::warn!("[history] emit history:updated failed: {:?}", e);
            }
        }
        Err(e) => log::warn!("[history] insert failed: {:?}", e),
    }
}

#[tauri::command]
pub fn get_recording_state(holder: State<'_, RecorderHolder>) -> Result<RecorderState, String> {
    Ok(holder.current_state())
}

// ----- Stage 2-C-1: ホットキー設定 -----

/// 現在のホットキー設定を返す。未保存ならデフォルト (Ctrl+Shift+NonConvert/Toggle)。
#[tauri::command]
pub fn get_hotkey_config() -> Result<HotkeyConfig, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    match db.get_kv(HOTKEY_CONFIG_KEY).map_err(|e| e.to_string())? {
        Some(json) => HotkeyConfig::from_json(&json).map_err(|e| e.to_string()),
        None => Ok(HotkeyConfig::default()),
    }
}

/// 新しいホットキー設定で再登録する。
///
/// 手順:
///   1. 新 config を Shortcut に変換 (Code/Modifiers パース失敗ならここで Err)
///   2. 旧 Shortcut を unregister (失敗は warn して続行)
///   3. 新 Shortcut を register
///      失敗時は旧 Shortcut を re-register でロールバック → 元の状態を保持
///   4. DB に JSON 保存・CurrentHotkey 更新
///
/// 設定変更中にホットキーが失われる時間は数 ms (unregister → register) 程度。
/// Stage 2-C-2: KeyboardHook 経由でホットキー設定を更新する。
///
/// 手順:
///   1. validate: code (code_to_vk) と modifiers (parse_modifier_side) がすべて parse 可能か
///   2. 旧 config を DB から読み出し (ロールバック用)
///   3. KeyboardHook::update_config 呼出
///   4. DB に新 JSON 保存
///      失敗時は旧 config を update_config で戻し、ロールバック完了させる
#[tauri::command]
pub fn set_hotkey_config(
    app: AppHandle,
    hook: State<'_, HookHolder>,
    config: HotkeyConfig,
) -> Result<(), String> {
    // 1. validate (WH_KEYBOARD_LL hook 側で使う code_to_vk + parse_modifier_side が成立するか)
    if code_to_vk(&config.code).is_none() {
        return Err(format!("無効なキーコード: {}", config.code));
    }
    for m in &config.modifiers {
        if parse_modifier_side(m).is_none() {
            return Err(format!("無効な修飾キー: {}", m));
        }
    }

    let json = config.to_json().map_err(|e| e.to_string())?;

    // v0.1.10 機能 D: 保存前にホットキー競合検査 (Q3 確定: 設定変更時にも検査)
    //
    // probe_conflict は tauri-plugin-global-shortcut で「試験 register → 即 unregister」を行う。
    //
    // ⚠️ 永続化ポリシー (Codex P2 対策 2026-05-30):
    //   設定変更時に「保存試行した候補 hotkey が衝突」した場合は、
    //   現在 active な hotkey (旧値) は変更されない (hook も DB も触らない) ため、
    //   ConflictState を「候補値」で永続化するのは不適切 (UI と active hotkey が乖離する)。
    //   そのため設定変更経路では DB persistence + event emit を一切行わず、
    //   即 Err 文字列を返して frontend の toast で transient エラーとして扱う。
    //   設定画面の赤バッジ / 起動時ダイアログは「active な hotkey の状態」のみを反映する。
    //
    // 一方、成功時の ConflictState::None クリアは hook 更新 + DB save 完了後にまとめて
    // 実行する (途中失敗で「解消済」と嘘の状態を永続化しないため・Codex 2 回目 P2 対策)。
    let db_for_probe = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    if let Err(reason) = probe_conflict(&app, &config) {
        log::info!(
            "[hotkey-conflict] save rejected (candidate conflict): code={} mods={:?} reason={}",
            config.code, config.modifiers, reason
        );
        return Err(format!(
            "ホットキーが他のアプリ / OS と競合しています ({})。別の組合せに変更してください。",
            reason
        ));
    }

    // 2. 旧 config (DB) を退避 (ロールバック用)
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    let old_json = db.get_kv(HOTKEY_CONFIG_KEY).map_err(|e| e.to_string())?;

    // 3. KeyboardHook を新 config に更新
    let guard = hook
        .lock()
        .map_err(|e| format!("hook lock poisoned: {}", e))?;
    let hook_ref = guard
        .as_ref()
        .ok_or_else(|| "KeyboardHook not initialized".to_string())?;
    hook_ref
        .update_config(config.clone())
        .map_err(|e| format!("update_config failed: {}", e))?;

    // 4. DB 保存。失敗時は旧 config を hook に戻す
    if let Err(e) = db.set_kv(HOTKEY_CONFIG_KEY, &json) {
        if let Some(old) = old_json {
            if let Ok(old_config) = HotkeyConfig::from_json(&old) {
                match hook_ref.update_config(old_config) {
                    Ok(_) => log::info!(
                        "[hotkey] rollback (DB set_kv): hook restored to previous config"
                    ),
                    Err(rb) => log::error!("[hotkey] rollback failed: {}", rb),
                }
            }
        }
        return Err(format!("DB set_kv failed: {}", e));
    }

    log::info!(
        "[hotkey] updated: code={} modifiers={:?} mode={:?}",
        config.code, config.modifiers, config.mode
    );

    // v0.1.10 機能 D (Codex P2 対策): hook 更新 + DB 保存が両方成功した今のタイミングで
    // ConflictState::None を永続化し、conflict-cleared を emit する。
    // 旧状態が conflict だった場合のみ emit (冪等性: 連続変更で event 連発を防ぐ)。
    {
        let prev_json = db_for_probe
            .get_hotkey_conflict_state_json()
            .ok()
            .flatten();
        let was_conflict = prev_json
            .as_deref()
            .and_then(|s| ConflictState::from_json(s).ok())
            .map(|s| s.is_conflict())
            .unwrap_or(false);

        if let Ok(cleared_json) = ConflictState::None.to_json() {
            if let Err(e) = db_for_probe.set_hotkey_conflict_state_json(&cleared_json) {
                log::warn!(
                    "[hotkey-conflict] failed to persist cleared state (non-fatal): {}",
                    e
                );
            }
        }
        if was_conflict {
            if let Err(e) = app.emit("hotkey:conflict-cleared", &config) {
                log::warn!("[hotkey-conflict] emit conflict-cleared failed: {}", e);
            }
        }
    }

    // フロントエンド (Overlay / Home) にホットキー変更を通知 (UI 即時反映)
    // P2-4: emit 失敗は ConfigDB 更新は成功している状態なので Err は返さず警告ログのみ
    //       (pill 表示が古いままだが、機能としては動作する)
    if let Err(e) = app.emit("hotkey:config-changed", &config) {
        log::warn!("[hotkey] emit hotkey:config-changed failed: {}", e);
    }

    Ok(())
}

// ----- v0.1.10 機能 D: ホットキー競合状態 -----

/// 現在の競合状態を取得する。
///
/// 起動時 setup hook で probe_conflict が config_db に保存した値を返す。
/// 未保存 / パース失敗時は `ConflictState::None` を返す (UI 側で安全側に倒す)。
///
/// SettingsPage の HotkeySection が mount 時に呼び、赤バッジ表示の初期状態を決定する。
#[tauri::command]
pub fn get_hotkey_conflict_state() -> Result<ConflictState, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    let json = db
        .get_hotkey_conflict_state_json()
        .map_err(|e| e.to_string())?;
    match json {
        Some(s) => ConflictState::from_json(&s).map_err(|e| e.to_string()),
        None => Ok(ConflictState::None),
    }
}

// ----- Phase 2 D: 履歴 (HistoryDb) Tauri command -----

/// HistoryPage.tsx 一覧表示用 (ページング・新しい順)。
/// limit/offset は usize で受け、内部 SQL は i64 に変換。
#[tauri::command]
pub fn list_history(
    limit: usize,
    offset: usize,
    state: State<'_, Mutex<HistoryDb>>,
) -> Result<Vec<HistoryItem>, String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    db.list(limit as i64, offset as i64)
        .map_err(|e| e.to_string())
}

/// HistoryPage.tsx 詳細表示用 (将来)。存在しない id は Ok(None)。
#[tauri::command]
pub fn get_history(
    id: i64,
    state: State<'_, Mutex<HistoryDb>>,
) -> Result<Option<HistoryItem>, String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    db.get(id).map_err(|e| e.to_string())
}

/// Phase 3 編集学習で使用 (Phase 2 では UI 未接続)。
/// `edit_diff_json` を空文字 "" で受けたら None として保存し、edit_diff_json カラム NULL を維持する。
#[tauri::command]
pub fn update_history_edit(
    id: i64,
    edited_text: String,
    edit_diff_json: String,
    state: State<'_, Mutex<HistoryDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    let diff = if edit_diff_json.is_empty() {
        None
    } else {
        Some(edit_diff_json.as_str())
    };
    db.update_edit(id, &edited_text, diff)
        .map_err(|e| e.to_string())
}

/// HistoryPage.tsx 1 件削除用。存在しない id でも Ok (UI 楽観的更新と整合)。
#[tauri::command]
pub fn delete_history_item(
    id: i64,
    state: State<'_, Mutex<HistoryDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    db.delete(id).map_err(|e| e.to_string())
}

/// SettingsPage「履歴を全削除」UI 用。全件 DELETE。
#[tauri::command]
pub fn clear_history(state: State<'_, Mutex<HistoryDb>>) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    db.clear().map_err(|e| e.to_string())
}

/// SettingsPage 容量管理 UI 用の総件数取得。
/// 戻り値は u64 (件数は常に非負・i64 マイナスは想定しない)。
#[tauri::command]
pub fn history_count(state: State<'_, Mutex<HistoryDb>>) -> Result<u64, String> {
    let db = state.lock().map_err(|e| format!("history lock poisoned: {}", e))?;
    db.count().map(|n| n as u64).map_err(|e| e.to_string())
}

// ----- Phase 2 C Wave 1: 辞書 (GlossaryDb) Tauri command -----
//
// SSOT v2.10 §8.3.1 DT-01 / DT-02
// 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md

/// DictionaryPage.tsx 一覧表示用。全件取得 (created_at ASC)。
/// Wave 1 想定件数 100-500 のため LIMIT なし。1,000 件超で要再評価 (設計判断書 §3.4)。
#[tauri::command]
pub fn list_glossary(
    state: State<'_, Mutex<GlossaryDb>>,
) -> Result<Vec<GlossaryEntry>, String> {
    let db = state.lock().map_err(|e| format!("glossary lock poisoned: {}", e))?;
    db.list().map_err(|e| e.to_string())
}

/// DictionaryPage.tsx の語彙ヒント残量表示用 (2026-08-08)。
///
/// 辞書に登録した語のうち、実際に Whisper へ送られるのは上限内の先頭だけで、
/// 残りは後段の置換でしか効かない。従来これは warn ログにしか出ておらず、
/// ユーザーからは「登録したのに効いていない」状態が見えなかった (指摘)。
///
/// 順序・上限は `build_glossary_phrase` と共有しているため、
/// ここで返す数字と実際に送られる内容は必ず一致する。
#[tauri::command]
pub fn glossary_hint_usage(app: AppHandle) -> crate::recorder::GlossaryHintUsage {
    let words = collect_glossary_words(&app);
    crate::recorder::glossary_hint_usage(&words)
}

/// DictionaryPage.tsx 「+ 追加」/「✓ 更新」用。
/// entry.id == None なら INSERT・Some なら UPDATE。最終的な rowid を返す。
#[tauri::command]
pub fn upsert_glossary_entry(
    entry: GlossaryEntry,
    state: State<'_, Mutex<GlossaryDb>>,
) -> Result<i64, String> {
    let db = state.lock().map_err(|e| format!("glossary lock poisoned: {}", e))?;
    db.upsert(&entry).map_err(|e| e.to_string())
}

/// DictionaryPage.tsx 「- 削除」用。存在しない id でも Ok (UI 楽観的更新と整合)。
#[tauri::command]
pub fn delete_glossary_entry(
    id: i64,
    state: State<'_, Mutex<GlossaryDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("glossary lock poisoned: {}", e))?;
    db.delete(id).map_err(|e| e.to_string())
}

/// 重複 pattern 検出 (✓ 更新時の事前チェック)。
/// exclude_id を渡すと UPDATE 対象自身を除外して検索。
#[tauri::command]
pub fn find_glossary_duplicate(
    pattern: String,
    exclude_id: Option<i64>,
    state: State<'_, Mutex<GlossaryDb>>,
) -> Result<Option<GlossaryEntry>, String> {
    let db = state.lock().map_err(|e| format!("glossary lock poisoned: {}", e))?;
    db.find_duplicate(&pattern, exclude_id).map_err(|e| e.to_string())
}

/// Step 2.5: 「/」区切り pattern の sub-pattern 単位重複検出。
/// 1 つのルールに複数誤変換を集約する UX 用 (要望)。
/// find_glossary_duplicate は完全一致専用で残置 (後方互換)。
#[tauri::command]
pub fn find_glossary_overlapping(
    pattern_query: String,
    exclude_id: Option<i64>,
    state: State<'_, Mutex<GlossaryDb>>,
) -> Result<Vec<OverlapResult>, String> {
    let db = state.lock().map_err(|e| format!("glossary lock poisoned: {}", e))?;
    db.find_overlapping(&pattern_query, exclude_id)
        .map_err(|e| e.to_string())
}

// ----- Phase 2 C Wave 1 Step 3: アプリ別ルール (AppRulesDb) AT-06 Tauri command -----
//
// SSOT v2.10 §8.3.1 AT-06
// 設計判断: tasks/phase2c_wave1_design_decisions_2026-05-24.md §1.4
// v1 互換: v1 AppMappingPage.ps1 の「CellEndEdit 全件再構築保存」モデルを
//   `replace_all_app_rules` で表現。frontend onBlur で「現在の全行」を送る。

/// AppsPage.tsx 一覧表示用。全件取得 (created_at ASC)。
#[tauri::command]
pub fn list_app_rules(
    state: State<'_, Mutex<AppRulesDb>>,
) -> Result<Vec<AppRule>, String> {
    let db = state.lock().map_err(|e| format!("app_rules lock poisoned: {}", e))?;
    db.list().map_err(|e| e.to_string())
}

/// 単発 upsert (将来 Wave 2 で個別行編集 UI 用)。Wave 1 では replace_all_app_rules が主。
#[tauri::command]
pub fn upsert_app_rule(
    rule: AppRule,
    state: State<'_, Mutex<AppRulesDb>>,
) -> Result<i64, String> {
    let db = state.lock().map_err(|e| format!("app_rules lock poisoned: {}", e))?;
    db.upsert(&rule).map_err(|e| e.to_string())
}

/// 1 件削除。存在しない id でも Ok (UI 楽観的更新と整合)。
#[tauri::command]
pub fn delete_app_rule(
    id: i64,
    state: State<'_, Mutex<AppRulesDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("app_rules lock poisoned: {}", e))?;
    db.delete(id).map_err(|e| e.to_string())
}

/// 全件再構築保存 (v1 saveAllRows 互換)。frontend onBlur で「現在の全行」を送る。
/// 完全空行は Rust 側で除外。返り値は新 id 付きの全行。
#[tauri::command]
pub fn replace_all_app_rules(
    rules: Vec<AppRule>,
    state: State<'_, Mutex<AppRulesDb>>,
) -> Result<Vec<AppRule>, String> {
    let mut db = state.lock().map_err(|e| format!("app_rules lock poisoned: {}", e))?;
    db.replace_all(&rules).map_err(|e| e.to_string())
}

// ----- Phase 2 C Wave 2: プロファイル (ProfileDb) PT-01〜PT-10 Tauri command -----
//
// SSOT v2.10 §8.3.1 PT-01〜PT-10
// 設計判断: 委任プロンプト 2026-05-26 (承認済) — SQLite 採用・3 プリセット投入
//
// ビルトインプリセット (salon/accounting/meeting) は profile_db.rs の seed_builtins() で
// 初回 open 時に投入される。
//
// 制約:
//   - ビルトインは削除拒否 (Rust 側でも保険・UI でも防ぐ)
//   - llm_postprocess_prompt max 1200 文字 (Rust 側バリデート・UI 側でも文字数表示)
//   - emoji_density は 'none'|'few'|'normal'|'many' のいずれか (Rust 側バリデート)

/// ProfilePage.tsx 一覧表示用 (ビルトイン優先・その後 created_at ASC)。
#[tauri::command]
pub fn list_profiles(state: State<'_, Mutex<ProfileDb>>) -> Result<Vec<Profile>, String> {
    let db = state.lock().map_err(|e| format!("profile lock poisoned: {}", e))?;
    db.list().map_err(|e| e.to_string())
}

/// id 指定で 1 件取得 (将来の詳細画面用・現状 ProfilePage では未使用)。
#[tauri::command]
pub fn get_profile(
    id: i64,
    state: State<'_, Mutex<ProfileDb>>,
) -> Result<Option<Profile>, String> {
    let db = state.lock().map_err(|e| format!("profile lock poisoned: {}", e))?;
    db.get(id).map_err(|e| e.to_string())
}

/// PT-09 保存ボタンから呼ばれる upsert。
/// profile.id == None なら INSERT・Some なら UPDATE。
/// バリデーション失敗 (1200 文字超等) は Err(String) 文字列で返される。
#[tauri::command]
pub fn upsert_profile(
    profile: Profile,
    state: State<'_, Mutex<ProfileDb>>,
) -> Result<i64, String> {
    let db = state.lock().map_err(|e| format!("profile lock poisoned: {}", e))?;
    db.upsert(&profile).map_err(|e| e.to_string())
}

/// PT-10 削除ボタンから呼ばれる削除。
/// ビルトイン削除は Rust 側で拒否 → Err 文字列で返される。
#[tauri::command]
pub fn delete_profile(
    id: i64,
    state: State<'_, Mutex<ProfileDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("profile lock poisoned: {}", e))?;
    db.delete(id).map_err(|e| e.to_string())
}

/// PT-10「デフォルトに戻す」ボタンから呼ばれる reset。
/// builtin_key 一致のプリセット初期値で UPDATE する。
/// user 作成プロファイル (is_builtin = false) への呼出は Err。
#[tauri::command]
pub fn reset_profile_to_default(
    id: i64,
    state: State<'_, Mutex<ProfileDb>>,
) -> Result<(), String> {
    let db = state.lock().map_err(|e| format!("profile lock poisoned: {}", e))?;
    db.reset_to_builtin_default(id).map_err(|e| e.to_string())
}

/// 2026-05-22: Overlay window のサイズを state に応じて動的変更 (要望)
/// pill 周りの透明枠を最小化するため state 変化時に呼ばれる。
/// サイズ変更後に画面下端中央に再配置 + click-through 再設定。
///
/// P2-1: 位置計算は overlay::position_bottom_center ヘルパーに集約。
/// P2-2: set_position / set_ignore_cursor_events 失敗時は警告ログを残す (Err は返さない)。
#[tauri::command]
pub fn set_overlay_size(app: AppHandle, width: f64, height: f64, is_idle: bool) -> Result<(), String> {
    let overlay = app
        .get_webview_window("overlay")
        .ok_or_else(|| "overlay window not found".to_string())?;

    overlay
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|e| format!("set_size failed: {}", e))?;

    // idle 状態のみカーソルイベントを受け取る (ドラッグ移動 + プロファイル切替クリック用)。
    // 録音中・文字起こし中は click-through を維持。
    // ユーザーがドラッグした位置を保持するため position_bottom_center は呼ばない。
    let ignore = !is_idle;
    if let Err(e) = overlay.set_ignore_cursor_events(ignore) {
        log::warn!("[overlay] set_ignore_cursor_events({}) failed: {}", ignore, e);
    }

    // Day 31 (2026-05-30 報告): always_on_top は build 時のみで、Windows が他の
    // 最前面/全画面ウィンドウ出現時に topmost を解除すると pill が背面に隠れる。
    // state 変化 (録音サイクル) のたびに最前面を再表明する。位置はユーザーのドラッグ位置を
    // 保持するため変更しない (上の position_bottom_center 非呼び出し方針を踏襲)。
    if let Err(e) = overlay.set_always_on_top(true) {
        log::warn!("[overlay] set_always_on_top(true) failed: {}", e);
    }

    Ok(())
}

// ----- Phase 2 A: アクティブプロファイル in-memory State -----

/// 現在のアクティブプロファイルキーを返す。
/// デフォルトは "raw" (後処理なし)。
#[tauri::command]
pub fn get_active_profile_key(
    state: tauri::State<'_, ActiveProfileKey>,
) -> String {
    state.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// アクティブプロファイルキーを更新する (in-memory + config_db 永続化)。
///
/// config_db への保存: best-effort。保存失敗でも in-memory は更新される。
/// 次回起動時に config_db から読み出して復元 (lib.rs の manage() で実施)。
#[tauri::command]
pub fn set_active_profile_key(
    app: AppHandle,
    key: String,
    state: tauri::State<'_, ActiveProfileKey>,
) {
    {
        let mut guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
        *guard = key.clone();
    }
    // config_db に永続化 (best-effort)
    match ConfigDb::open(&default_config_db_path()) {
        Ok(db) => {
            if let Err(e) = db.set_kv(ACTIVE_PROFILE_KEY_KV, &key) {
                log::warn!("[profile] set active_profile_key to config_db failed: {}", e);
            } else {
                log::info!("[profile] active_profile_key saved: {:?}", key);
            }
        }
        Err(e) => {
            log::warn!("[profile] config_db open failed for active_profile_key: {}", e);
        }
    }
    // Overlay に変更を通知 (best-effort)
    let _ = app.emit("profile:active-changed", &key);
}

/// Phase 2 A: アクティブプロファイルで LLM 後処理を適用するヘルパー。
///
/// - アクティブプロファイルが "raw" なら即座に `text` を返す (LLM 呼び出しなし)
/// - ProfileDb から llm_postprocess_prompt を取得し、空なら `text` を返す
/// - LLM 呼び出し失敗・空レスポンスは `text` をフォールバック返却
///
/// lib.rs のホットキーイベントハンドラと stop_recording command の両経路から共通呼出。
pub(crate) async fn apply_profile_postprocess(app: &AppHandle, text: &str) -> String {
    // アクティブプロファイルキーを取得
    let active_key = match app.try_state::<ActiveProfileKey>() {
        Some(s) => s.0.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        None => {
            log::warn!("[llm_postprocess] ActiveProfileKey state not managed; skipping");
            return text.to_string();
        }
    };

    // "raw" プロファイルは後処理なし
    if active_key == "raw" {
        return text.to_string();
    }

    // ProfileDb からプロファイルを取得 (llm_postprocess_prompt + emoji_density + filler_removal)
    let (prompt, emoji_density, filler_removal) = match app.try_state::<Mutex<ProfileDb>>() {
        Some(s) => {
            let db = match s.lock() {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("[llm_postprocess] ProfileDb lock poisoned: {}; skipping", e);
                    return text.to_string();
                }
            };
            match db.resolve_by_app_rule_key(&active_key) {
                Ok(Some(profile)) => (
                    profile.llm_postprocess_prompt,
                    profile.emoji_density,
                    profile.filler_removal,
                ),
                Ok(None) => {
                    log::warn!("[llm_postprocess] profile not found for key {:?}; skipping", active_key);
                    return text.to_string();
                }
                Err(e) => {
                    log::warn!("[llm_postprocess] resolve_by_app_rule_key failed: {}; skipping", e);
                    return text.to_string();
                }
            }
        }
        None => {
            log::warn!("[llm_postprocess] ProfileDb state not managed; skipping");
            return text.to_string();
        }
    };

    // プロンプトが空なら LLM 不要
    if prompt.trim().is_empty() {
        return text.to_string();
    }

    // 整文の接続先 (Groq / OpenAI / カスタム)。未設定なら整えずにそのまま貼る (録音の流れは止めない)。
    let endpoint = match crate::endpoints::load_chat_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            log::warn!("[llm_postprocess] skipped: {}", e);
            return text.to_string();
        }
    };

    // reqwest Client を都度生成 (transcribe.rs の build_client と同じ設定)
    let client = match crate::transcribe::build_client() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[llm_postprocess] build_client failed: {}; skipping", e);
            return text.to_string();
        }
    };

    log::info!(
        "[llm_postprocess] applying profile {:?} (emoji_density={}) to text (len={})",
        active_key,
        emoji_density,
        text.len()
    );

    // 2026-07-27: 誤変換が文脈で直らない件 (③) の対策。辞書の「語彙ヒント」
    // (pattern == replacement = 置換では直せず候補提示しか手のない実在語) を
    // 後処理 LLM にも渡す。従来は Whisper prompt にしか渡っておらず、
    // 後処理 LLM には辞書情報が一切届いていなかった。
    // プロファイル指示の**後ろ**に置く = LLM の重み付けが強い位置 (L602)。
    let prompt = match crate::llm_postprocess::build_confusable_hint(&collect_confusable_words(app))
    {
        hint if hint.is_empty() => prompt,
        hint => format!("{}\n{}", prompt.trim_end(), hint),
    };

    // 伸長ガードのプロファイル別閾値 (2026-07-23): テンプレート挿入で正当に伸びるのは
    // mail 系のみ。整形系 (natural/casual/business_doc 等) は厳格閾値で創作を弾く。
    let allow_template_expansion = active_key == "mail";
    // 「ていねい」「メール」は敬体化が仕様なので、敬語化の検出対象から外す
    // (仕様どおりの動作を誤検知すると、ナチュラルでの本当の敬語化がログに埋もれる)
    let allow_style_change = allow_template_expansion || active_key == "business_doc";
    let out = apply_llm_postprocess(
        &client,
        &endpoint,
        &prompt,
        text,
        &crate::llm_postprocess::PostprocessOptions {
            emoji_density,
            filler_removal,
            allow_template_expansion,
            allow_style_change,
        },
    )
    .await;
    // 候補をすべて試してもモデルが無かった → 画面に「モデルの更新が必要」と出す (原文は貼る)
    if crate::endpoints::take_model_update_needed() {
        let _ = app.emit("llm-model-unavailable", ());
    }
    out
}

// ----- v0.1.10 機能 A: 混雑通知バナー設定 -----
//
// SSOT: v0_1_10_feature_a_delegation_2026-05-30.md
// config_db (settings_kv) の便利関数経由で読み書きする。
// recorder 側は CongestionConfig を引数で受け取る (config_db 非依存を維持)。

/// React 側から JSON で受け渡される設定。`threshold_ms` は 1000-30000ms 範囲・
/// Rust 側でクランプして保存する。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CongestionConfigDto {
    pub threshold_ms: i64,
    pub show_in_hedged: bool,
}

/// 設定読込ヘルパー。失敗時は default に静かにフォールバック (録音 UX を止めない)。
pub(crate) fn load_congestion_config_or_default() -> CongestionConfig {
    match ConfigDb::open(&default_config_db_path()) {
        Ok(db) => {
            let threshold = db
                .get_congestion_threshold_ms()
                .unwrap_or(crate::config_db::DEFAULT_CONGESTION_THRESHOLD_MS);
            let show_in_hedged = db.get_congestion_show_in_hedged().unwrap_or(false);
            CongestionConfig {
                threshold_ms: threshold.max(0) as u64,
                show_in_hedged,
            }
        }
        Err(e) => {
            log::warn!(
                "[congestion] config_db open failed: {}; using default (5000ms / hedged suppressed)",
                e
            );
            CongestionConfig::default()
        }
    }
}

// ----- v0.1.12 F2: マイク入力低レベル警告設定 -----
//
// SSOT: v0_1_12_scope_2026-05-30.md
// recorder 側に MicLowLevelConfig を引数で渡す (config_db 非依存を維持)。
// 設定 UI からの変更はスコープ外 (v0.1.13 で追加可能)。

/// 設定読込ヘルパー。失敗時は default に静かにフォールバック (録音 UX を止めない)。
pub(crate) fn load_mic_low_level_config_or_default() -> MicLowLevelConfig {
    match ConfigDb::open(&default_config_db_path()) {
        Ok(db) => {
            let threshold = db
                .get_mic_low_level_threshold()
                .unwrap_or(crate::config_db::DEFAULT_MIC_LOW_LEVEL_THRESHOLD);
            let window_ms = db
                .get_mic_low_level_window_ms()
                .unwrap_or(crate::config_db::DEFAULT_MIC_LOW_LEVEL_WINDOW_MS);
            MicLowLevelConfig {
                threshold,
                window_ms,
            }
        }
        Err(e) => {
            log::warn!(
                "[mic_low_level] config_db open failed: {}; using default (threshold=0.005 / window=3000ms)",
                e
            );
            MicLowLevelConfig::default()
        }
    }
}

/// v0.1.15 小声警告: 録音セッション平均 RMS を baseline (普段の声水準) と比較し、
/// 相対的に小さければ `recorder:quiet-voice-warning` を emit する (warning-only・貼付は別途実行済)。
/// 小声でなければ baseline を EMA 更新する (普段の声だけで baseline を育てる)。
///
/// すべて best-effort: config_db が開けない・mean<=0 なら何もしない。貼付フローは止めない。
/// ホットキー経路 (lib.rs toggle / ptt) からも呼ぶため pub(crate)。
///
/// v0.1.16 機能 L (議事録モード) Day 1 拡張:
/// 第 3 引数 `mode` で baseline 学習可否を分岐する。
///   - `RecorderMode::Normal`: 既存挙動 (小声なら警告 emit + 学習スキップ / 普段の声で学習)
///   - `RecorderMode::Meeting`: 警告 emit は行うが baseline 学習は **常にスキップ** する
///     (議事録専用経路で通常録音 baseline を汚染しないため)。
/// 純粋関数 `judge_quiet_voice` / `update_voice_baseline` のシグネチャ・本体は触らない。
/// 学習可否は `recorder::should_learn_baseline` で判定する (unit テスト対象)。
pub(crate) fn handle_quiet_voice_warning(
    app: &AppHandle,
    mean: f32,
    mode: crate::recorder::RecorderMode,
) {
    if mean <= 0.0 {
        return;
    }
    let db = match ConfigDb::open(&default_config_db_path()) {
        Ok(db) => db,
        Err(e) => {
            log::warn!("[quiet_voice] config_db open failed: {}; skip", e);
            return;
        }
    };
    let baseline = db
        .get_voice_baseline_rms()
        .unwrap_or(crate::config_db::DEFAULT_VOICE_BASELINE_RMS);
    let count = db
        .get_voice_baseline_count()
        .unwrap_or(crate::config_db::DEFAULT_VOICE_BASELINE_COUNT);
    let factor = db
        .get_voice_low_factor()
        .unwrap_or(crate::config_db::DEFAULT_VOICE_LOW_FACTOR);
    let alpha = db
        .get_voice_baseline_alpha()
        .unwrap_or(crate::config_db::DEFAULT_VOICE_BASELINE_ALPHA);
    let min_samples = db
        .get_voice_min_samples()
        .unwrap_or(crate::config_db::DEFAULT_VOICE_MIN_SAMPLES);

    let is_quiet =
        crate::recorder::judge_quiet_voice(mean, baseline, count, factor, min_samples);

    if is_quiet {
        log::info!(
            target: "rushvoice.rms",
            "quiet-voice warning: mean={:.5} baseline={:.5} factor={} mode={:?} (paste continues)",
            mean,
            baseline,
            factor,
            mode
        );
        let _ = app.emit(
            "recorder:quiet-voice-warning",
            "⚠️ 声が小さめです・誤変換が増えることがあります（履歴で確認できます）",
        );
        // 小声と判定した録音は baseline を更新しない (基準が下がる暴走を防ぐ)。
        return;
    }

    // v0.1.16 機能 L: 議事録モードでは普段の声でも学習しない。
    if !crate::recorder::should_learn_baseline(is_quiet, mode) {
        log::info!(
            target: "rushvoice.rms",
            "[meeting] skip baseline learning: mean={:.5} mode={:?}",
            mean,
            mode
        );
        return;
    }

    // 普段の声 (または コールドスタート中) は baseline を学習更新。
    let new_baseline = crate::recorder::update_voice_baseline(baseline, mean, alpha, count);
    if let Err(e) = db.set_voice_baseline_rms(new_baseline) {
        log::warn!("[quiet_voice] set_voice_baseline_rms failed: {}", e);
    }
    if let Err(e) = db.set_voice_baseline_count(count.saturating_add(1)) {
        log::warn!("[quiet_voice] set_voice_baseline_count failed: {}", e);
    }
}

/// SettingsPage 読込用。default は config_db 内の定数に従う。
#[tauri::command]
pub fn get_congestion_config() -> Result<CongestionConfigDto, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    let threshold = db
        .get_congestion_threshold_ms()
        .map_err(|e| e.to_string())?;
    let show_in_hedged = db
        .get_congestion_show_in_hedged()
        .map_err(|e| e.to_string())?;
    Ok(CongestionConfigDto {
        threshold_ms: threshold,
        show_in_hedged,
    })
}

/// SettingsPage 保存用。範囲外値は config_db 側でクランプされる。
#[tauri::command]
pub fn set_congestion_config(cfg: CongestionConfigDto) -> Result<(), String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    db.set_congestion_threshold_ms(cfg.threshold_ms)
        .map_err(|e| e.to_string())?;
    db.set_congestion_show_in_hedged(cfg.show_in_hedged)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ----- v0.1.10 機能 E: 辞書 CSV インポート/エクスポート -----
//
// SSOT: v0_1_10_feature_e_delegation_2026-05-30.md
// 列定義: id, pattern, replacement, context, note, is_regex, created_at, updated_at
// CSV: UTF-8 BOM + CRLF + RFC 4180
// import 時 id は無視・重複判定 key = pattern
//
// frontend (DictionaryPage.tsx) 側で @tauri-apps/plugin-dialog の save/open で
// パス文字列を取得し、それをこの 3 つの command に渡す。Rust 側で std::fs::File
// 経由で読み書きする。

/// インポートモード (CSV 重複時の挙動)。
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum ImportMode {
    /// 既存 entry を CSV 内容で UPDATE (id 保持)
    Overwrite,
    /// 既存 entry はそのまま・CSV 側は無視
    Skip,
    /// DB の全エントリ削除 → CSV 全件 INSERT
    ReplaceAll,
}

/// preview API の返り値。frontend は duplicates 件数を見て確認ダイアログを出す。
#[derive(Debug, Clone, Serialize)]
pub struct ImportPreview {
    /// CSV の有効行数 (= new_entries + duplicates)
    pub total_in_csv: usize,
    /// 既存 DB と pattern が衝突した entries: (csv_entry, existing_entry) のペア
    pub duplicates: Vec<(GlossaryEntry, GlossaryEntry)>,
    /// 既存 DB と衝突しない新規 entries
    pub new_entries: Vec<GlossaryEntry>,
    /// パース失敗で skip された行数
    pub error_count: usize,
}

/// commit API の返り値。
#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    /// 新規 INSERT 件数
    pub added: usize,
    /// UPDATE 件数 (Overwrite mode の重複)
    pub updated: usize,
    /// スキップ件数 (Skip mode の重複)
    pub skipped: usize,
    /// パース失敗で skip された行数 (= preview.error_count を引き継ぐ)
    pub errors: usize,
}

/// 辞書 CSV エクスポート。`save_path` は frontend (plugin-dialog::save) で得た絶対パス。
///
/// std::fs::File::create() で上書き作成し、glossary_csv::export_to_writer に渡す。
/// 戻り値はエクスポート件数。
#[tauri::command]
pub fn glossary_export_csv(
    save_path: String,
    state: tauri::State<'_, std::sync::Mutex<GlossaryDb>>,
) -> Result<usize, String> {
    let db = state
        .lock()
        .map_err(|e| format!("glossary lock poisoned: {}", e))?;
    let file = std::fs::File::create(&save_path)
        .map_err(|e| format!("ファイル作成失敗 {:?}: {}", save_path, e))?;
    let writer = std::io::BufWriter::new(file);
    let n = export_to_writer(&db, writer).map_err(|e| format!("CSV 書き出し失敗: {}", e))?;
    log::info!("[glossary_csv] exported {} entries to {:?}", n, save_path);
    Ok(n)
}

/// 辞書 CSV インポート プレビュー。`open_path` は frontend (plugin-dialog::open) で得た絶対パス。
///
/// CSV をパース → 既存 DB と pattern で照合 → duplicates / new_entries に振り分けて返す。
/// この command は DB を変更しない (commit で初めて変更)。
#[tauri::command]
pub fn glossary_import_csv_preview(
    open_path: String,
    state: tauri::State<'_, std::sync::Mutex<GlossaryDb>>,
) -> Result<ImportPreview, String> {
    let db = state
        .lock()
        .map_err(|e| format!("glossary lock poisoned: {}", e))?;
    let file = std::fs::File::open(&open_path)
        .map_err(|e| format!("ファイル読込失敗 {:?}: {}", open_path, e))?;
    let reader = std::io::BufReader::new(file);
    let (entries, error_count) =
        parse_csv(reader).map_err(|e| format!("CSV パース失敗: {}", e))?;

    let mut duplicates: Vec<(GlossaryEntry, GlossaryEntry)> = Vec::new();
    let mut new_entries: Vec<GlossaryEntry> = Vec::new();
    let total_in_csv = entries.len();
    for csv_entry in entries.into_iter() {
        match db
            .find_by_pattern(&csv_entry.pattern)
            .map_err(|e| format!("find_by_pattern failed: {}", e))?
        {
            Some(existing) => duplicates.push((csv_entry, existing)),
            None => new_entries.push(csv_entry),
        }
    }

    log::info!(
        "[glossary_csv] preview {:?}: total={} new={} dup={} errors={}",
        open_path,
        total_in_csv,
        new_entries.len(),
        duplicates.len(),
        error_count
    );
    Ok(ImportPreview {
        total_in_csv,
        duplicates,
        new_entries,
        error_count,
    })
}

/// 辞書 CSV インポート コミット。preview で取得した entries を mode に従って適用。
///
/// frontend は preview 結果を一度ユーザーに見せ、確定後に
///   - new_entries + duplicates (CSV 側) を flat に並べて
///   - mode (Overwrite / Skip / ReplaceAll) と共に
/// この commit を呼ぶ。
///
/// 動作:
///   - ReplaceAll: delete_all → bulk_insert(entries)
///   - Overwrite: 各 entry について find_by_pattern → 既存あれば UPDATE (id 保持)・なければ INSERT
///   - Skip: 各 entry について find_by_pattern → 既存あれば 何もしない・なければ INSERT
#[tauri::command]
pub fn glossary_import_csv_commit(
    entries: Vec<GlossaryEntry>,
    mode: ImportMode,
    error_count: usize,
    state: tauri::State<'_, std::sync::Mutex<GlossaryDb>>,
) -> Result<ImportResult, String> {
    let mut db = state
        .lock()
        .map_err(|e| format!("glossary lock poisoned: {}", e))?;

    let (added, updated, skipped) = match mode {
        ImportMode::ReplaceAll => {
            // Codex review P2 (Day 30): DELETE → INSERT は 1 transaction で atomic 化。
            // disk full 等で「既存辞書消したまま新規も入らない」事故を防ぐ。
            let (removed, n) = db
                .delete_all_and_bulk_insert(&entries)
                .map_err(|e| format!("delete_all_and_bulk_insert failed: {}", e))?;
            log::info!(
                "[glossary_csv] ReplaceAll (atomic): removed {} existing entries, inserted {} new",
                removed, n
            );
            (n, 0usize, 0usize)
        }
        ImportMode::Overwrite => {
            // Codex review P2 #4 (Day 30): Overwrite 経路全体を 1 transaction で atomic 化。
            // - 既存 entry あり → UPDATE (id 保持・created_at 保持・updated_at = now)
            // - 既存なし → INSERT (CSV timestamp 尊重)
            // 途中失敗時は全 rollback で「一部だけ上書き」を防ぐ。
            let (a, u) = db
                .apply_import_overwrite(&entries)
                .map_err(|e| format!("apply_import_overwrite failed: {}", e))?;
            (a, u, 0usize)
        }
        ImportMode::Skip => {
            // Codex review P2 #4 (Day 30): Skip 経路も 1 transaction で atomic 化。
            let (a, s) = db
                .apply_import_skip(&entries)
                .map_err(|e| format!("apply_import_skip failed: {}", e))?;
            (a, 0usize, s)
        }
    };

    log::info!(
        "[glossary_csv] commit done: added={} updated={} skipped={} errors={}",
        added,
        updated,
        skipped,
        error_count
    );
    Ok(ImportResult {
        added,
        updated,
        skipped,
        errors: error_count,
    })
}

// Day 25 Phase 2-G (SSOT v3.3): alias_db Tauri command (cmd_list_aliases /
// cmd_insert_alias / cmd_delete_alias / cmd_apply_aliases /
// cmd_insert_alias_with_overwrite / cmd_apply_alias_from_edit_diff) と
// EditDiff / DuplicateAction / EditDiffApplyResult 型は ASR 補正レイヤー撤廃と共に削除済。

// ----- v0.1.12 F3 (A 案): autostart enable/disable ラッパ commands -----
//
// 背景:
// - 従来は frontend (lib/tauri.ts) から `plugin:autostart|enable` /
//   `plugin:autostart|disable` を直接 invoke していた。
// - F3 NSIS Uninstall 復元 (起動時 `ensure_autostart_consistency`) と
//   「ユーザー OFF 意図」尊重を両立するため、Rust 側に薄いラッパを挟む形に変更。
//
// 2 フラグ設計 (Codex P1 v0.1.12 F3 追加修正で確定):
// - `autostart_default_applied_v017_1` = v0.1.7.1 デフォルト ON 化マイグレーションが
//   過去 1 度でも走ったかを示す不変フラグ (set のみ・削除しない)。
//   `apply_autostart_default_v017_1` がこれを見て二度目の自動 ON を抑止する。
// - `autostart_user_intent` = ユーザーの意思表示 (値: "enabled" / "disabled")。
//   `ensure_autostart_consistency` がこれを見て NSIS Uninstall 復元するか判定する。
//   "enabled" のみ復元対象 = ユーザーが UI で OFF にした (= "disabled") 場合は復元しない。
//
// 動作仕様:
// - `enable_autostart`:
//     plugin `autolaunch().enable()` 成功 → user_intent="enabled" + migration flag も set
//     (初回 enable パスをスキップする保険・既に set 済みなら上書きで no-op)。
// - `disable_autostart`:
//     plugin `autolaunch().disable()` 成功 → user_intent="disabled"。
//     migration flag は触らない (= 次回起動時 apply_autostart_default は skip し続け、
//     再 enable しない)。
// - flag 操作失敗は best-effort (warn ログのみ・plugin 操作結果を優先)。
// - is_enabled は plugin 側を frontend が直接呼び続ける (副作用なし・読み取り専用)。
//
// AppRulesDb 状態の堅牢性 (Codex P2 対応):
// - `app.try_state::<Mutex<AppRulesDb>>()` を使い、State 未登録時 (app_rules.db open 失敗時)
//   は warn ログのみで plugin 操作結果を返す。autostart の OS 操作自体は app_rules.db に
//   依存しないため、AppRulesDb が無い場合でも UI トグルが panic しないようにする。

pub(crate) const AUTOSTART_MIGRATION_FLAG_KEY: &str = "autostart_default_applied_v017_1";
pub(crate) const AUTOSTART_USER_INTENT_KEY: &str = "autostart_user_intent";
pub(crate) const AUTOSTART_USER_INTENT_ENABLED: &str = "enabled";
pub(crate) const AUTOSTART_USER_INTENT_DISABLED: &str = "disabled";

fn with_app_rules_db<F>(app: &AppHandle, label: &str, f: F)
where
    F: FnOnce(&AppRulesDb),
{
    match app.try_state::<Mutex<AppRulesDb>>() {
        Some(state) => match state.lock() {
            Ok(db) => f(&db),
            Err(e) => log::warn!("[{}] AppRulesDb mutex poisoned: {:?}", label, e),
        },
        None => log::warn!(
            "[{}] AppRulesDb state not managed; skipping flag update (autostart OS op succeeded)",
            label
        ),
    }
}

#[tauri::command]
pub fn enable_autostart(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    app.autolaunch()
        .enable()
        .map_err(|e| format!("autostart enable failed: {:?}", e))?;

    with_app_rules_db(&app, "enable_autostart", |db| {
        if let Err(e) = db.set_meta(AUTOSTART_USER_INTENT_KEY, AUTOSTART_USER_INTENT_ENABLED) {
            log::warn!(
                "[enable_autostart] set_meta('{}') failed: {:?}",
                AUTOSTART_USER_INTENT_KEY,
                e
            );
        }
        // マイグレーションフラグも保険で立てる (既に立っていれば上書きで no-op)。
        // ユーザーが手動 ON した時点で apply_autostart_default の対象から外す。
        if let Err(e) = db.set_meta(AUTOSTART_MIGRATION_FLAG_KEY, "1") {
            log::warn!(
                "[enable_autostart] set_meta('{}') failed: {:?}",
                AUTOSTART_MIGRATION_FLAG_KEY,
                e
            );
        }
        log::info!(
            "[enable_autostart] enable succeeded; user_intent='{}' migration_flag set",
            AUTOSTART_USER_INTENT_ENABLED
        );
    });
    Ok(())
}

#[tauri::command]
pub fn disable_autostart(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    app.autolaunch()
        .disable()
        .map_err(|e| format!("autostart disable failed: {:?}", e))?;

    with_app_rules_db(&app, "disable_autostart", |db| {
        if let Err(e) = db.set_meta(AUTOSTART_USER_INTENT_KEY, AUTOSTART_USER_INTENT_DISABLED) {
            log::warn!(
                "[disable_autostart] set_meta('{}') failed: {:?}",
                AUTOSTART_USER_INTENT_KEY,
                e
            );
        } else {
            log::info!(
                "[disable_autostart] disable succeeded; user_intent='{}' (migration flag preserved)",
                AUTOSTART_USER_INTENT_DISABLED
            );
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // 単体テストは default_*_path() が APPDATA に依存するため tempdir 注入不可。
    // ファクト確認だけ (型整合性): Provider が serde で正しく lower-case にデシリアライズされること。
    #[test]
    fn provider_deserialize_lowercase() {
        let g: Provider = serde_json::from_str("\"groq\"").unwrap();
        matches!(g, Provider::Groq);
        let o: Provider = serde_json::from_str("\"openai\"").unwrap();
        matches!(o, Provider::Openai);
        let c: Provider = serde_json::from_str("\"custom_transcribe\"").unwrap();
        assert_eq!(c, Provider::CustomTranscribe);
        let l: Provider = serde_json::from_str("\"custom_llm\"").unwrap();
        assert_eq!(l, Provider::CustomLlm);
    }

    // Day 25 Phase 2-G (SSOT v3.3): EditDiff / DuplicateAction / EditDiffApplyResult
    // の serde テスト 3 件は ASR 補正レイヤー撤廃と共に削除済。

    // ----- Phase 2 D: history command 単体テスト -----
    //
    // Tauri State モックは複雑なため、HistoryDb の API ロジックを直接叩く形でテストする。
    // Tauri command の薄いラッパ (lock + delegate) はビルド可能性 (型整合性) で担保し、
    // 実機 E2E (Phase C) で挙動確認する。
    // 以下のテストは「commands 経由相当の操作 (insert → list → get → delete → clear)」を
    // HistoryDb の単体メソッド連結で再現する。

    fn tmp_history_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("rushvoice_cmd_history_test_{}", name))
            .join("history.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn make_item(text: &str) -> HistoryItem {
        HistoryItem {
            id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            provider: "groq".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            mode: None,
            app_name: None,
            raw_text: text.to_string(),
            refined_text: text.to_string(),
            edited_text: None,
            edit_diff_json: None,
            edit_count: 0,
            confidence_drop_flag: false,
            duration_ms: 3000,
            transcribe_ms: 500,
            audio_bytes: 96000,
            avg_logprob: Some(-0.15),
            no_speech_prob: Some(0.0),
            primary_ms: None,
            fallback_triggered: None,
            hedged_winner: None,
        }
    }

    #[test]
    fn test_list_history_command_returns_items() {
        // list_history (limit, offset) は HistoryDb::list と等価
        let path = tmp_history_path("list_cmd");
        let db = HistoryDb::open(&path).expect("open");
        db.insert(&make_item("一件目")).expect("insert 1");
        db.insert(&make_item("二件目")).expect("insert 2");

        // limit=10, offset=0 → 2 件 (新しい順)
        let rows = db.list(10, 0).expect("list");
        assert_eq!(rows.len(), 2);
        // raw_text のいずれかが入っていることを確認 (created_at が同一の場合 id DESC で順序確定)
        let texts: Vec<&str> = rows.iter().map(|r| r.raw_text.as_str()).collect();
        assert!(texts.contains(&"一件目"));
        assert!(texts.contains(&"二件目"));

        // Serialize 可能性 (Tauri 戻り値型のため必須)
        let json = serde_json::to_string(&rows).expect("serialize");
        assert!(json.contains("一件目"));
    }

    #[test]
    fn test_delete_history_item_command() {
        // delete_history_item (id) は HistoryDb::delete と等価
        let path = tmp_history_path("delete_cmd");
        let db = HistoryDb::open(&path).expect("open");
        let id1 = db.insert(&make_item("残す")).expect("insert");
        let id2 = db.insert(&make_item("消す")).expect("insert");
        db.delete(id2).expect("delete");
        let rows = db.list(10, 0).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, Some(id1));
        // 存在しない id 削除は OK (UI 楽観的更新と整合)
        db.delete(999_999).expect("delete nonexistent");
    }

    #[test]
    fn test_clear_history_command() {
        // clear_history は HistoryDb::clear と等価
        let path = tmp_history_path("clear_cmd");
        let db = HistoryDb::open(&path).expect("open");
        for i in 0..3 {
            db.insert(&make_item(&format!("c{}", i))).expect("insert");
        }
        assert_eq!(db.count().unwrap(), 3);
        db.clear().expect("clear");
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_update_history_edit_empty_diff_treated_as_none() {
        // update_history_edit は edit_diff_json="" を None として保存する。
        let path = tmp_history_path("update_edit");
        let db = HistoryDb::open(&path).expect("open");
        let id = db.insert(&make_item("原文")).expect("insert");

        // command 側ロジック (空文字 → None) を再現
        let edit_diff_json = String::new();
        let diff = if edit_diff_json.is_empty() {
            None
        } else {
            Some(edit_diff_json.as_str())
        };
        db.update_edit(id, "編集後", diff).expect("update");

        let after = db.get(id).unwrap().unwrap();
        assert_eq!(after.edited_text.as_deref(), Some("編集後"));
        assert_eq!(after.edit_count, 1);
        assert_eq!(after.edit_diff_json, None);
    }

    #[test]
    fn test_get_history_command_none_for_missing() {
        let path = tmp_history_path("get_cmd_none");
        let db = HistoryDb::open(&path).expect("open");
        assert!(db.get(999_999).unwrap().is_none());
    }
}
