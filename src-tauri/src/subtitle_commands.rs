// subtitle_commands.rs — セミナー字幕モード Tauri commands + リアルタイムパイプライン
//
// フロー:
//   subtitle_start
//     → recorder::start_subtitle_session (発話区切り worker・mpsc 送信側)
//     → tokio::spawn(run_subtitle_pipeline) (mpsc 受信側)
//        発話 WAV → Groq Whisper (language 自動判定) → Groq LLM 翻訳 (JSON)
//        → subtitle:line emit (字幕ウィンドウ + コントロールページが購読)
//   subtitle_stop
//     → worker 停止 → tx drop → パイプライン自然終了 → subtitle:state=idle emit + 一時 WAV 掃除
//
// 設計方針:
//   - 発話は**直列処理** (表示順序保証・講習の発話リズムなら backlog は実用上問題なし)
//   - 転写/翻訳の個別失敗は当該発話のフォールバック表示に留め、セッションは止めない
//   - 議事録 (meeting_commands) のコードは不変・字幕専用に分離

use std::path::PathBuf;
use std::sync::Mutex;


use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio_util::sync::CancellationToken;

/// 翻訳 LLM モデル (llm_postprocess.rs と同じ既存採用モデルを流用)。
// 宛先・キー・モデルは endpoints.rs が決める (文字起こし = verbose_json 必須の接続先・
// 翻訳 = 整文と同じ接続先)。
/// 転写リトライ (字幕は低遅延優先: 初回 + 1 リトライのみ・バックオフ 500ms)。
const SUBTITLE_TRANSCRIBE_RETRIES: u32 = 1;

/// 字幕ウィンドウの window label (capabilities/default.json の windows 配列と一致必須・L136)。
const SUBTITLE_WINDOW_LABEL: &str = "subtitle";

const EVENT_LINE: &str = "subtitle:line";
const EVENT_STATE: &str = "subtitle:state";

/// 実行中セッションの設定 (start 時に固定・確定仕様「出力言語は講習開始時に固定」)。
#[derive(Debug, Clone, Serialize)]
pub struct SubtitleRunConfig {
    pub target_lang: String,
    pub show_english: bool,
}

/// セッション設定 State (frontend の再表示・状態問い合わせ用)。
#[derive(Default)]
pub struct SubtitleConfigState(pub Mutex<Option<SubtitleRunConfig>>);

#[derive(Debug, Clone, Serialize)]
struct SubtitleStateEvent {
    state: &'static str, // "listening" | "idle" | "error"
    message: Option<String>,
}

fn emit_state(app: &AppHandle, state: &'static str, message: Option<String>) {
    let _ = app.emit(EVENT_STATE, SubtitleStateEvent { state, message });
}

/// 字幕セッション開始。出力言語・英語 3 段目はここで固定する。
#[tauri::command]
pub async fn subtitle_start(
    app: AppHandle,
    target_lang: String,
    show_english: bool,
) -> Result<(), String> {
    // 診断ログ (Day 1 スモーク): invoke 到達と各ステップ通過を必ず記録する (L286/Day38 切り分け方式)
    log::info!(
        "[subtitle][dbg] subtitle_start INVOKED: target={} show_english={}",
        target_lang,
        show_english
    );
    if !crate::subtitle::SUPPORTED_TARGET_LANGS.contains(&target_lang.as_str()) {
        return Err(format!("未対応の出力言語: {}", target_lang));
    }

    // 二重起動・他モード併用ガード
    let holder: State<crate::recorder::SubtitleHolder> = app
        .try_state::<crate::recorder::SubtitleHolder>()
        .ok_or_else(|| "SubtitleHolder state not managed".to_string())?;
    if holder.is_recording() {
        return Err("既に字幕モード実行中です".into());
    }
    if let Some(meeting) = app.try_state::<crate::recorder::MeetingHolder>() {
        if meeting.is_recording() {
            return Err("議事録録音中は字幕モードを開始できません".into());
        }
    }

    log::info!("[subtitle][dbg] guards passed");

    // 接続先 (議事録と同じ: 言語判定に verbose_json が要る)
    let transcribe_ep = crate::endpoints::load_segment_transcribe_endpoint()?;
    // 翻訳の接続先。未設定なら翻訳せず原文のまま出す (decide_rows のフォールバック)
    let chat_ep = match crate::endpoints::load_chat_endpoint() {
        Ok(ep) => Some(ep),
        Err(e) => {
            log::warn!("[subtitle] translation disabled: {}", e);
            None
        }
    };
    log::info!(
        "[subtitle][dbg] endpoints: transcribe={} chat={:?}",
        transcribe_ep.label,
        chat_ep.as_ref().map(|c| c.label)
    );

    // 辞書 replacement → 翻訳プロンプトに固有名詞ヒント注入 (L209 対応)
    // 新しい登録を先頭に (reverse): 上限切り捨て (翻訳30語/400字・Whisper180字) を
    // 古い登録側に寄せる。Day2 事案: 登録順だと新規の講習用語 6 語がほぼ全て圏外だった。
    let glossary_entries = crate::commands::collect_glossary_entries(&app);
    let glossary_replacements: Vec<String> = {
        let mut v: Vec<String> =
            glossary_entries.iter().map(|(_, rep, _)| rep.clone()).collect();
        v.reverse();
        v
    };
    let (system_prompt, truncated) = crate::subtitle::build_translation_prompt(
        &target_lang,
        show_english,
        &glossary_replacements,
    );
    if truncated > 0 {
        // 無言の切り捨て禁止 (L286)
        log::info!(
            "[subtitle] translation prompt glossary hint truncated: {} terms dropped",
            truncated
        );
    }

    // 認識精度対策 (2026-07-05 Day1 検品: 「エクステ→エプステ/いくつで」誤認識 3 回中 2 回):
    // 通常録音と同じ辞書入り Whisper prompt を用意する。ただし常時使うと言語自動判定が
    // 日本語に偏るため、パイプライン側で「直前の発話が日本語だったときだけ」適用する (適応方式)。
    // L526 (2026-07-23): collect_glossary_words が新規優先 (降順) を返すようになったため
    // ここでの reverse() は削除 (二重反転で古い順に戻ってしまうバグ防止)。
    let whisper_words = crate::commands::collect_glossary_words(&app);
    // Whisper ヒント 180 字上限の切り捨てを可視化 (L286: 無言の切り捨て禁止)
    let hint_phrase = crate::recorder::build_glossary_phrase(&whisper_words);
    let hint_included =
        if hint_phrase.is_empty() { 0 } else { hint_phrase.split(", ").count() };
    let unique_count = whisper_words
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<std::collections::HashSet<_>>()
        .len();
    if hint_included < unique_count {
        log::warn!(
            "[subtitle] whisper prompt glossary hint truncated: {}/{} terms included (180-char cap)",
            hint_included,
            unique_count
        );
    }
    let whisper_prompt_ja = crate::recorder::build_dynamic_whisper_prompt(&whisper_words);

    // 発話 WAV 一時ディレクトリ (セッション毎に分離・終了時に削除)
    let session_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let chunk_dir = crate::paths::rushvoice_data_dir()
        .join("subtitle_tmp")
        .join(format!("s{}", session_id));
    std::fs::create_dir_all(&chunk_dir)
        .map_err(|e| format!("字幕一時ディレクトリ作成失敗 {:?}: {}", chunk_dir, e))?;

    log::info!("[subtitle][dbg] chunk_dir ready: {:?}", chunk_dir);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(usize, PathBuf)>();
    crate::recorder::start_subtitle_session(&holder, chunk_dir.clone(), tx)?;
    log::info!("[subtitle][dbg] worker started");

    // 設定を State に保存 (frontend 状態問い合わせ用)
    if let Some(cfg_state) = app.try_state::<SubtitleConfigState>() {
        if let Ok(mut g) = cfg_state.0.lock() {
            *g = Some(SubtitleRunConfig {
                target_lang: target_lang.clone(),
                show_english,
            });
        }
    }

    emit_state(&app, "listening", None);
    // 要望 (2026-07-05): 開始時、字幕ウィンドウが無ければ自動で開き、必ず最前面に出す。
    // (async command 内なので window 生成は安全・wry#583 は同期 command のみの問題)
    if let Err(e) = open_or_focus_subtitle_window(&app) {
        // ウィンドウ表示失敗でも字幕セッション自体は動いているため継続 (ログのみ)
        log::warn!("[subtitle] 字幕ウィンドウ自動オープン失敗: {}", e);
    }
    log::info!(
        "[subtitle] start: target={} show_english={} glossary_hints={}",
        target_lang,
        show_english,
        glossary_replacements.len()
    );

    let app_bg = app.clone();
    tokio::spawn(async move {
        run_subtitle_pipeline(
            app_bg,
            rx,
            transcribe_ep,
            chat_ep,
            system_prompt,
            whisper_prompt_ja,
            target_lang,
            show_english,
            chunk_dir,
        )
        .await;
    });
    Ok(())
}

/// 字幕セッション停止。worker 停止 → tx drop → パイプラインが残処理後に idle emit する。
#[tauri::command]
pub async fn subtitle_stop(app: AppHandle) -> Result<(), String> {
    let holder: State<crate::recorder::SubtitleHolder> = app
        .try_state::<crate::recorder::SubtitleHolder>()
        .ok_or_else(|| "SubtitleHolder state not managed".to_string())?;
    crate::recorder::stop_subtitle_session(&holder)?;
    if let Some(cfg_state) = app.try_state::<SubtitleConfigState>() {
        if let Ok(mut g) = cfg_state.0.lock() {
            *g = None;
        }
    }
    log::info!("[subtitle] stop requested (pipeline will drain and emit idle)");
    Ok(())
}

/// 実行中かどうか + 実行中設定を返す (frontend 再マウント時の状態復元用)。
#[tauri::command]
pub fn subtitle_get_state(app: AppHandle) -> Result<Option<SubtitleRunConfig>, String> {
    let holder: State<crate::recorder::SubtitleHolder> = app
        .try_state::<crate::recorder::SubtitleHolder>()
        .ok_or_else(|| "SubtitleHolder state not managed".to_string())?;
    if !holder.is_recording() {
        return Ok(None);
    }
    let cfg = app
        .try_state::<SubtitleConfigState>()
        .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()));
    Ok(cfg)
}

/// 字幕ウィンドウを開く。既存なら show + focus。
/// 外部モニターがあればそちらへ移動して全画面表示 (講習の HDMI 外部モニター用途)。
///
/// ⚠️ 必ず async にすること: Tauri 2 では同期コマンド内の WebviewWindowBuilder::build() が
/// Windows でメインスレッドをデッドロックさせる既知問題がある (wry#583・公式 docs 明記)。
/// 2026-07-05 Day1 スモークで「開く→開始が永遠にぐるぐる」の真因だった。
#[tauri::command]
pub async fn subtitle_open_window(app: AppHandle) -> Result<(), String> {
    open_or_focus_subtitle_window(&app)
}

/// 字幕ウィンドウを開く実体 (無ければ作成・あれば最前面化)。
/// ⚠️ 同期 command から直接呼ばないこと (wry#583 デッドロック)。呼出元は async command のみ。
fn open_or_focus_subtitle_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SUBTITLE_WINDOW_LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let window = WebviewWindowBuilder::new(
        app,
        SUBTITLE_WINDOW_LABEL,
        WebviewUrl::App(PathBuf::from("index.html#/subtitle")),
    )
    .title("RushVoice セミナー字幕")
    // 要望 (2026-07-05): 最初から縦長め (履歴スクロール閲覧のため)
    .inner_size(1100.0, 760.0)
    .resizable(true)
    .build()
    .map_err(|e| format!("字幕ウィンドウ作成失敗: {}", e))?;

    // 外部モニター検出: primary と位置が異なる先頭モニターへ全画面配置。
    // 単一モニター時は通常ウィンドウのまま (開発機での開発・検証用)。
    let monitors = window.available_monitors().unwrap_or_default();
    if monitors.len() >= 2 {
        let primary_pos = window
            .primary_monitor()
            .ok()
            .flatten()
            .map(|m| *m.position());
        let target = monitors
            .iter()
            .find(|m| primary_pos.map(|p| *m.position() != p).unwrap_or(true));
        if let Some(m) = target {
            let pos = *m.position();
            if let Err(e) = window.set_position(tauri::PhysicalPosition::new(pos.x, pos.y)) {
                log::warn!("[subtitle] 外部モニターへの移動失敗: {}", e);
            } else if let Err(e) = window.set_fullscreen(true) {
                log::warn!("[subtitle] 全画面化失敗: {}", e);
            } else {
                log::info!("[subtitle] 字幕ウィンドウを外部モニターに全画面配置 ({}, {})", pos.x, pos.y);
            }
        }
    }
    Ok(())
}

/// 字幕ウィンドウを閉じる (存在しなければ何もしない)。
/// (open と同じく async: 同期コマンドの window 操作は Windows デッドロックの恐れ・wry#583)
#[tauri::command]
pub async fn subtitle_close_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SUBTITLE_WINDOW_LABEL) {
        w.close().map_err(|e| format!("字幕ウィンドウ close 失敗: {}", e))?;
    }
    Ok(())
}

/// 1 発話 WAV を転写する (language 自動判定・低遅延優先の短いリトライ)。
/// `prompt` は適応方式: 直前発話が日本語なら辞書入り prompt、それ以外は空 (言語判定への偏り防止)。
async fn transcribe_utterance(
    client: &reqwest::Client,
    wav_path: &PathBuf,
    idx: usize,
    endpoint: &crate::endpoints::TranscribeEndpoint,
    prompt: &str,
) -> Result<String, String> {
    let audio = tokio::fs::read(wav_path)
        .await
        .map_err(|e| format!("WAV 読込失敗 {:?}: {}", wav_path, e))?;
    let mut last_err = String::new();
    for attempt in 0..=SUBTITLE_TRANSCRIBE_RETRIES {
        let req = crate::transcribe::TranscribeRequest {
            api_url: endpoint.url.clone(),
            api_key: endpoint.api_key.clone(),
            model: endpoint.model.clone(),
            // 適応方式: 直前発話が日本語のときのみ辞書入り prompt (呼出側で決定)。
            // 常時付与しないのは Whisper の言語自動判定が日本語に偏るのを防ぐため。
            prompt: prompt.to_string(),
            // language=None → Whisper 自動判定 (verbose_json "language" で検出結果取得)
            language: None,
            audio: audio.clone(),
            file_name: format!("utterance_{:03}.wav", idx),
            mime_type: "audio/wav".to_string(),
            // 字幕は verbose_json 前提 (トップレベル "language" で言語検出)
            verbose_json: true,
        };
        match crate::transcribe::transcribe(client, req, CancellationToken::new()).await {
            Ok(resp) => return Ok(resp.body),
            Err(e) => {
                last_err = format!("{:?}", e);
                if attempt < SUBTITLE_TRANSCRIBE_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }
    Err(format!("utterance {} transcribe failed: {}", idx, last_err))
}

/// Chat Completions (整文と同じ接続先) で翻訳 JSON を取得する。失敗時 None (呼出側で原文フォールバック)。
///
/// llm_postprocess::apply_llm_postprocess は日本語強制サフィックスを付与するため流用不可。
/// 字幕翻訳専用に response_format=json_object で JSON のみ返させる。
async fn translate_chat(
    client: &reqwest::Client,
    endpoint: Option<&crate::endpoints::ChatEndpoint>,
    system_prompt: &str,
    context: &str,
    text: &str,
) -> Option<String> {
    let endpoint = endpoint?;
    // 直前発話を語義判定用の文脈として添える (文が途中分断された場合の誤訳/誤補正防止)。
    // 文脈自体は訳させない (出力 JSON に含めない指示)。
    let user_content = if context.is_empty() {
        text.to_string()
    } else {
        format!(
            "(Context — the previous utterance, for word-sense disambiguation ONLY. \
             Do NOT translate it and do NOT include it in the output JSON: 「{}」)\n\n\
             Translate this utterance:\n{}",
            context, text
        )
    };
    // モデル候補 (前から順に試す・整文と同じ)
    let candidates = crate::endpoints::usable_candidates(client, endpoint).await;
    let mut idx = 0usize;
    let first = candidates.first().cloned().unwrap_or_else(|| endpoint.model.clone());
    let mut req = serde_json::json!({
        "model": first,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_content},
        ],
        "max_tokens": crate::endpoints::with_reasoning_headroom(&first, 1024),
        "temperature": 0.2,
        "response_format": {"type": "json_object"},
    });
    if let Some(effort) = crate::endpoints::reasoning_effort_for(&first) {
        req["reasoning_effort"] = serde_json::json!(effort);
    }
    // JSON モード (response_format) を受け付けないローカルサーバーがあるため、
    // 400 のときだけ外して 1 回だけ送り直す (プロンプト側でも JSON だけ返すよう指示済み)。
    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let resp = match crate::endpoints::with_auth(client.post(&endpoint.url), &endpoint.api_key)
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!("[subtitle] translate request failed: {}", e);
                return None;
            }
        };
        let st = resp.status().as_u16();
        if st == 400 || st == 404 || st == 429 {
            let body = resp.text().await.unwrap_or_default();
            let unavailable = crate::endpoints::is_model_unavailable(st, &body);
            if (st == 429 || unavailable) && idx + 1 < candidates.len() {
                if unavailable {
                    crate::endpoints::mark_model_unavailable(endpoint, &candidates[idx]);
                }
                idx += 1;
                let next = candidates[idx].clone();
                log::warn!("[subtitle] translate HTTP {} on {}; switching to {}", st, candidates[idx - 1], next);
                req["max_tokens"] = serde_json::json!(crate::endpoints::with_reasoning_headroom(&next, 1024));
                match crate::endpoints::reasoning_effort_for(&next) {
                    Some(e) => req["reasoning_effort"] = serde_json::json!(e),
                    None => {
                        if let Some(obj) = req.as_object_mut() {
                            obj.remove("reasoning_effort");
                        }
                    }
                }
                req["model"] = serde_json::json!(next);
                attempt = 0;
                continue;
            }
            if st == 400 && !unavailable && attempt == 1 {
                log::info!("[subtitle] translate HTTP 400 from {}; retrying without response_format", endpoint.label);
                if let Some(obj) = req.as_object_mut() {
                    obj.remove("response_format");
                }
                continue;
            }
            if unavailable {
                crate::endpoints::set_model_update_needed(true);
            }
            log::warn!("[subtitle] translate HTTP {} from {}: {}", st, endpoint.label, body.chars().take(300).collect::<String>());
            return None;
        }
        break resp;
    };
    if !resp.status().is_success() {
        log::warn!("[subtitle] translate HTTP {} from {}", resp.status(), endpoint.label);
        return None;
    }
    let body = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let content = v
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?
        .to_string();
    if content.trim().is_empty() {
        None
    } else {
        Some(content)
    }
}

/// 字幕パイプライン本体: 発話 WAV を順番に 転写 → 翻訳 → emit する (直列・表示順序保証)。
#[allow(clippy::too_many_arguments)]
async fn run_subtitle_pipeline(
    app: AppHandle,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(usize, PathBuf)>,
    transcribe_ep: crate::endpoints::TranscribeEndpoint,
    chat_ep: Option<crate::endpoints::ChatEndpoint>,
    system_prompt: String,
    whisper_prompt_ja: String,
    target_lang: String,
    show_english: bool,
    chunk_dir: PathBuf,
) {
    let client = match crate::transcribe::build_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("[subtitle] HTTP client build failed: {:?}", e);
            emit_state(&app, "error", Some(format!("HTTP クライアント初期化失敗: {}", e)));
            return;
        }
    };

    // 適応 prompt: 直前発話の検出言語が日本語のときだけ辞書入り prompt を使う。
    // 初回発話と非日本語直後は空 prompt (言語自動判定を汚さない)。
    let mut prev_lang_ja = false;
    // 直前発話テキスト (末尾 120 字): 文が途中で分断された場合の文脈継続用。
    // Whisper prompt 末尾 + 翻訳 LLM の文脈として渡す (「湿度→角度」分断事案対策)。
    let mut prev_text = String::new();

    while let Some((idx, wav_path)) = rx.recv().await {
        let t0 = std::time::Instant::now();
        let whisper_prompt = {
            let mut p = String::new();
            if prev_lang_ja {
                p.push_str(&whisper_prompt_ja);
            }
            if !prev_text.is_empty() {
                p.push_str(&prev_text);
            }
            p
        };
        let whisper_prompt = whisper_prompt.as_str();

        // 1. 転写 (language 自動判定)
        let body = match transcribe_utterance(&client, &wav_path, idx, &transcribe_ep, whisper_prompt).await {
            Ok(b) => b,
            Err(e) => {
                log::error!("[subtitle] {} (skip)", e);
                emit_state(&app, "error", Some("転写に失敗しました (回線を確認してください)".into()));
                let _ = tokio::fs::remove_file(&wav_path).await;
                continue;
            }
        };
        let (text, raw_lang) = match crate::subtitle::parse_transcription(&body) {
            Ok(t) => t,
            Err(e) => {
                log::error!("[subtitle] utterance {} parse failed: {} (skip)", idx, e);
                let _ = tokio::fs::remove_file(&wav_path).await;
                continue;
            }
        };
        let _ = tokio::fs::remove_file(&wav_path).await; // 一時 WAV は逐次削除 (社外秘・音声を残さない)
        // prompt 漏洩防御 (通常録音/議事録と同じ 2 経路・prompt を付けた発話のみ実質作用)
        let text = crate::prompt_sanitize::strip_prompt_leak_head_fragment(&text);
        let text = crate::prompt_sanitize::sanitize_prompt_leak(whisper_prompt, &text);
        if text.is_empty() {
            continue; // 発話なしチャンク (正常系)
        }
        // L121 対策: 環境音チャンクに対する Whisper 幻聴 (1 文字程度の出力) は表示せず破棄。
        // Day1 スモーク実測: 無発話 12 秒チャンクが「検出=en・1 文字」を量産した。
        if text.chars().count() <= 1 {
            log::info!(
                "[subtitle] line {} skipped: too short (len={} likely noise hallucination L121)",
                idx,
                text.chars().count()
            );
            continue;
        }
        // 遠い声フィルタ (Day2): 隣席の声・環境音由来の低信頼転写は崩れた字幕になるため
        // 表示せず破棄。閾値は実測決定 (subtitle::FAR_VOICE_AVG_LOGPROB_THRESHOLD 参照)。
        // prev_text/prev_lang_ja 更新前に抜けるので、後続発話の文脈も汚さない。
        let conf = crate::subtitle::parse_confidence(&body);
        if crate::subtitle::is_far_voice(conf.0) {
            log::info!(
                "[subtitle] line {} skipped: far-voice filtered (min_avg_logprob={:?} threshold={})",
                idx,
                conf.0,
                crate::subtitle::FAR_VOICE_AVG_LOGPROB_THRESHOLD
            );
            continue;
        }
        let detected =
            raw_lang.as_deref().and_then(crate::subtitle::normalize_detected_language);
        prev_lang_ja = detected == Some("ja");

        // 翻訳 LLM に渡す文脈 (この発話の直前テキスト)。更新は翻訳呼び出しの後。
        let context_for_llm = prev_text.clone();
        // 次発話用に末尾 120 字を保持 (長文でも prompt を肥大させない)
        prev_text = {
            let chars: Vec<char> = text.chars().collect();
            let start = chars.len().saturating_sub(120);
            chars[start..].iter().collect()
        };

        // 2. 翻訳 (失敗は decide_rows の原文フォールバックに委ねる)
        let translated = translate_chat(
            &client,
            chat_ep.as_ref(),
            &system_prompt,
            &context_for_llm,
            &text,
        )
        .await
            .and_then(|raw| crate::subtitle::parse_translation_json(&raw));

        // 3. 表示行確定 + emit
        let rows = crate::subtitle::decide_rows(
            detected,
            &target_lang,
            show_english,
            &text,
            translated.as_ref(),
        );
        let latency_ms = t0.elapsed().as_millis() as u64;
        log::info!(
            "[subtitle] line {}: detected={:?} latency_ms={} original_len={}",
            idx,
            detected,
            latency_ms,
            text.chars().count()
        );
        // ⚠️ QA 一時ログ (2026-07-05 Day1): 翻訳品質検品のため発話内容をローカルログに残す。
        // 本番リリース前に必ず削除すること (発話内容 = 社外秘・ログに残さない方針)。
        // conf = (min avg_logprob, max no_speech_prob): 遠い声フィルタの閾値検証用。
        log::info!(
            "[subtitle][qa] #{} conf={:?} original=「{}」 local=「{}」 ja=「{}」 en={:?}",
            idx,
            conf,
            text,
            rows.local,
            rows.ja,
            rows.en
        );
        let _ = app.emit(
            EVENT_LINE,
            serde_json::json!({
                "seq": idx,
                "original": text,
                "detected_lang": detected,
                "local": rows.local,
                "ja": rows.ja,
                "en": rows.en,
                "latency_ms": latency_ms,
            }),
        );
        emit_state(&app, "listening", None);
    }

    // worker 終了 (tx drop) → 残発話を処理し終えたらここに到達
    emit_state(&app, "idle", None);
    if let Err(e) = tokio::fs::remove_dir_all(&chunk_dir).await {
        log::warn!("[subtitle] 一時ディレクトリ削除失敗 {:?}: {} (次回起動時に残骸)", chunk_dir, e);
    }
    log::info!("[subtitle] pipeline finished");
}
