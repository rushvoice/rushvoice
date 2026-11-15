// meeting_commands.rs — v0.1.16 機能 L (議事録モード) Day 1: Tauri commands 雛形
//
// SSOT: 内部設計メモ §機能L
// 委任 prompt: 内部設計メモ §2.4
//
// Day 1 スコープ:
// - 6 commands のシグネチャ + 結線 (DB CRUD + state emit のみ)
// - 録音 / 転写 / チャンク書き出し / 整形は Day 2-3 で実装 (本ファイルでは未着手)
// - 未実装の処理は `Err("not_yet_implemented_in_day1")` を返す (panic させない・L283 派生)
// - `meeting:state-changed` emit を雛形でも配線 (L202 準拠)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::meeting_logs_db::{MeetingLogRow, MeetingLogsDb};

/// 5 分チャンク秒数 (Day 2 の compute_chunk_threshold_samples と同値・SSOT 確定)。
const CHUNK_SECS: u32 = 300;
/// 最大並列 transcribe 数 (SSOT §L-R2)。
const MAX_PARALLEL: usize = 3;
/// チャンク単位の transcribe 最大リトライ数 (指数バックオフ 3 回 = 計 4 トライ)。
const MAX_RETRIES: u32 = 3;
/// 段落化のギャップ閾値 (ms)。SSOT §機能L 段落化 = 0.8 秒。
const PARAGRAPH_GAP_MS: u64 = 800;
/// 段落化の最大字数 (チャンク文字単位)。SSOT §機能L 段落化 = 200 字。
const PARAGRAPH_MAX_CHARS: usize = 200;

/// 議事録セッションの状態 (frontend 表示用)。
///
/// Day 1 では meeting_start / meeting_stop の擬似遷移にのみ使う:
///   Idle → Recording → Transcribing → Completed (or Error)
/// Day 2-3 で実録音と転写を実装した時に正しい遷移経路を組む。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MeetingState {
    Idle,
    Recording,
    Transcribing,
    Completed,
    Error,
}

/// state-changed emit ペイロード。
/// `id` は最後に start_at で生成した session id (進行中なら Some、Idle なら None)。
#[derive(Debug, Clone, Serialize)]
pub struct MeetingStateEvent {
    pub state: MeetingState,
    pub id: Option<String>,
}

/// frontend に返す議事録 1 件 (DB 内部表現 MeetingLogRow と同形)。
/// 現状は単純な type alias 風だが、将来 segments_json をパース済の Vec<MeetingSegment> に
/// 展開する DTO 化を行いやすくするため別 struct で公開する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingLogDto {
    pub id: String,
    pub start_at: i64,
    pub duration_ms: i64,
    pub title: String,
    pub segments_json: String,
    pub chunk_dir: String,
    pub created_at: i64,
}

impl From<MeetingLogRow> for MeetingLogDto {
    fn from(r: MeetingLogRow) -> Self {
        MeetingLogDto {
            id: r.id,
            start_at: r.start_at,
            duration_ms: r.duration_ms,
            title: r.title,
            segments_json: r.segments_json,
            chunk_dir: r.chunk_dir,
            created_at: r.created_at,
        }
    }
}

/// `meeting:state-changed` イベント名 (frontend 購読側と一致させる)。
const EVENT_STATE_CHANGED: &str = "meeting:state-changed";

/// Day 1 では UUID v4 ライブラリ依存を増やさず、独自に「時刻 ns + プロセス内 atomic
/// counter」から 32 hex 文字列を組み立てる。
/// 衝突可能性: 同一プロセス内で同時刻 (ns) かつ counter 一致でのみ衝突 → 実用上ゼロ。
/// 将来 uuid crate を導入したら置き換える (DB スキーマは TEXT のため互換)。
fn generate_meeting_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 16 byte = 32 hex 字。前半: ns、後半: counter。
    format!("{:016x}{:016x}", ns, c)
}

fn current_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 議事録録音を開始する。
///
/// Day 2: chunk_dir を**実 mkdir** + `start_meeting_session` を呼んで実際の audio
/// worker を起動する。DB に新規行 INSERT (segments=[] / duration_ms=0 / chunk_dir=絶対パス)
/// → `meeting:state-changed = Recording` emit。返却値は新 session id。
/// 5 分チャンクの transcribe / segments 書き戻しは Day 3 で実装。
#[tauri::command]
pub async fn meeting_start(
    app: AppHandle,
    title: Option<String>,
) -> Result<String, String> {
    let id = generate_meeting_id();
    let now = current_epoch_secs();

    // Day 2: chunk_dir の実 mkdir。失敗時は早期 return (DB 行も作らない)。
    // Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
    // Windows: %APPDATA%\RushVoice\meetings\<id> (既存パス不変)
    // macOS:   ~/Library/Application Support/RushVoice/meetings/<id>
    let chunk_dir: std::path::PathBuf = crate::paths::rushvoice_data_dir()
        .join("meetings")
        .join(&id);
    std::fs::create_dir_all(&chunk_dir)
        .map_err(|e| format!("chunk_dir 作成失敗 {:?}: {}", chunk_dir, e))?;

    let row = MeetingLogRow {
        id: id.clone(),
        start_at: now,
        duration_ms: 0,
        title: title.unwrap_or_default(),
        segments_json: "[]".to_string(),
        chunk_dir: chunk_dir.to_string_lossy().to_string(),
        created_at: now,
    };

    // DB insert
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        db.insert(&row).map_err(|e| e.to_string())?;
    }

    // Day 2: 議事録 worker 起動 (start_meeting_session)
    let holder: State<crate::recorder::MeetingHolder> = app
        .try_state::<crate::recorder::MeetingHolder>()
        .ok_or_else(|| "MeetingHolder state not managed".to_string())?;
    // Day 2 は default() で OK (Day 4 で config_db 読込検討・CEO 事前確定)
    let mic_low_level = crate::recorder::MicLowLevelConfig::default();
    crate::recorder::start_meeting_session(
        &holder,
        Some(app.clone()),
        chunk_dir,
        mic_low_level,
    )?;

    let _ = app.emit(
        EVENT_STATE_CHANGED,
        MeetingStateEvent {
            state: MeetingState::Recording,
            id: Some(id.clone()),
        },
    );
    log::info!("[meeting][dbg] emit Recording: id={}", id);
    log::info!(
        "[meeting] start: id={} (Day 2 actual worker started)",
        id
    );
    Ok(id)
}

/// 議事録録音を停止する。
///
/// Day 2: `stop_meeting_session` で worker を停止し残バッファを finalize、
///        `MeetingStopOutcome` から `duration_ms` を DB に書き戻す。
/// Day 3: `Transcribing` emit 後に background transcribe タスクを `tokio::spawn` で起動。
///        本 command 自体は即 return し UI 反応性を確保する。
///        background タスクが完了時に `Completed` (または `Error`) を emit する。
#[tauri::command]
pub async fn meeting_stop(app: AppHandle, id: String) -> Result<(), String> {
    // v0.1.16 Day 38 デバッグ: frontend からの invoke 受信を必ず記録する。
    // 連打時に backend invoke が何回受信されたか確定するための診断ログ。
    log::info!("[meeting][dbg] meeting_stop INVOKED: id_in={:?}", id);
    // v0.1.16 Day 37 修正1: 停止ボタンが効かないバグ対策。
    // **停止 (stop_meeting_session) を最優先で必ず実行**する。id が空でも holder ベースで
    // 録音は確実に止まる (id 不問)。id 空ガードを停止前に置いていた旧実装では、frontend が
    // meetingId を取りこぼすと録音が止まらず発話が議事録に残り続けた。
    let holder: State<crate::recorder::MeetingHolder> = app
        .try_state::<crate::recorder::MeetingHolder>()
        .ok_or_else(|| "MeetingHolder state not managed".to_string())?;
    let outcome = crate::recorder::stop_meeting_session(&holder)?;

    // 停止は成功済。id が空の場合は DB 更新・転写を skip して停止だけ成功で返す
    // (録音は holder で確実に止まっている)。
    // ただし frontend は押下時に楽観的に Transcribing 表示にしているため、ここで
    // `Error` state を emit して UI を復帰させる (emit しないと transcribing のまま固まる・Codex P2)。
    // id が無い = DB 行に紐付けられず転写・エクスポート不可のため Error 表示が妥当 (録音停止自体は成功)。
    if id.trim().is_empty() {
        log::warn!(
            "[meeting] stop: id is empty; recording stopped via holder (DB update / transcribe skipped). chunks={} duration_ms={}",
            outcome.chunk_paths.len(),
            outcome.total_duration_ms
        );
        let _ = app.emit(
            EVENT_STATE_CHANGED,
            MeetingStateEvent {
                state: MeetingState::Error,
                id: None,
            },
        );
        return Ok(());
    }

    // v0.1.16 機能 L Day 4: 小声警告 emit (議事録モードでも警告は出すが baseline 学習は行わない)。
    // 第 3 引数 RecorderMode::Meeting により handle_quiet_voice_warning 内で
    // should_learn_baseline(_, Meeting) = false となり、warning emit のみ実行される。
    // 既存純粋関数 judge_quiet_voice / update_voice_baseline は不変 (C4 ガード遵守)。
    crate::commands::handle_quiet_voice_warning(
        &app,
        outcome.rms_mean,
        crate::recorder::RecorderMode::Meeting,
    );

    // DB の duration_ms 単独更新 (segments_json は background transcribe 完了後に書き戻し)
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        db.update_duration_only(&id, outcome.total_duration_ms as i64)
            .map_err(|e| e.to_string())?;
    }

    // 状態遷移: Recording → Transcribing emit
    let _ = app.emit(
        EVENT_STATE_CHANGED,
        MeetingStateEvent {
            state: MeetingState::Transcribing,
            id: Some(id.clone()),
        },
    );
    log::info!("[meeting][dbg] emit Transcribing: id={}", id);

    // 接続先 (区間の時刻が要るので verbose_json を返せる接続先: endpoints.rs)
    let endpoint = match crate::endpoints::load_segment_transcribe_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            // key が無ければ Transcribing で止めず、即 Error emit して return Ok する
            // (録音自体は成功し DB 行と WAV は残るため、後でエクスポート/手動転写可能)。
            log::error!("[meeting] 文字起こしの接続先が未設定: {}", e);
            let _ = app.emit(
                EVENT_STATE_CHANGED,
                MeetingStateEvent {
                    state: MeetingState::Error,
                    id: Some(id.clone()),
                },
            );
            return Ok(());
        }
    };

    // Day 3 / v0.1.16 精度向上 ブロック2: background transcribe spawn (即 return で UI 反応性確保)
    //
    // ブロック2: 議事録モードでも辞書 (glossary) を適用する。
    //   - Whisper prompt に固有名詞 (replacement 列) を載せ転写段階の精度を上げる
    //     (通常録音 commands.rs:215 collect_glossary_words と同じパターン)
    //   - 転写後の segment text に pattern→replacement 置換を後処理で適用 (run_background_transcribe 内)
    log::info!(
        "[meeting] transcribe endpoint={} ({})",
        endpoint.label,
        crate::endpoints::host_for_log(&endpoint.url)
    );
    let api_url = endpoint.url.clone();
    let api_key = endpoint.api_key.clone();
    let model = endpoint.model.clone();
    // 通常録音と同じ helper を再利用 (pub(crate))。固有名詞を Whisper prompt 末尾に注入。
    let glossary_words = crate::commands::collect_glossary_words(&app);
    let glossary_entries = crate::commands::collect_glossary_entries(&app);
    let prompt = crate::recorder::build_dynamic_whisper_prompt(&glossary_words);

    // v0.1.16 Day 37 修正2: 議事録 LLM 後処理プロンプトを辞書 replacement 込みで動的構築。
    // replacement 側 (正しい表記) を LLM ヒントに反映 (各社の辞書がそのまま固有名詞精度に効く)。
    let glossary_replacements: Vec<String> =
        glossary_entries.iter().map(|(_, rep, _)| rep.clone()).collect();
    let (llm_prompt, llm_hint_truncated) =
        crate::meeting::build_meeting_llm_prompt(&glossary_replacements);
    if llm_hint_truncated > 0 {
        // 無言の切り捨て禁止 (L286 ログ統一): 上限超過で捨てた件数を残す。
        log::info!(
            "[meeting] LLM prompt glossary hint truncated: {} terms dropped (over word/char limit)",
            llm_hint_truncated
        );
    }
    log::info!(
        "[meeting] glossary loaded: words={} entries={} llm_prompt_len={}",
        glossary_words.len(),
        glossary_entries.len(),
        llm_prompt.chars().count()
    );
    let app_for_bg = app.clone();
    let id_for_bg = id.clone();
    let chunks = outcome.chunk_paths.clone();
    let dur = outcome.total_duration_ms;
    tokio::spawn(async move {
        run_background_transcribe(
            app_for_bg,
            id_for_bg,
            chunks,
            dur,
            api_url,
            api_key,
            model,
            prompt,
            glossary_entries,
            llm_prompt,
        )
        .await;
    });

    log::info!(
        "[meeting] stop: id={} chunks={} duration_ms={} (background transcribe spawned)",
        id,
        outcome.chunk_paths.len(),
        outcome.total_duration_ms
    );
    Ok(())
}

/// 1 チャンクを transcribe して verbose_json body を返す (指数バックオフ付きリトライ)。
///
/// MAX_RETRIES = 3 → 計 4 トライ (初回 + リトライ 3 回)。
/// バックオフは 1s / 2s / 4s (1000 << attempt)。
async fn transcribe_chunk_with_retry(
    client: &reqwest::Client,
    wav_path: PathBuf,
    chunk_index: usize,
    api_url: String,
    api_key: String,
    model: String,
    prompt: String,
) -> Result<String, String> {
    let mut last_err = String::new();
    for attempt in 0..=MAX_RETRIES {
        let audio = match tokio::fs::read(&wav_path).await {
            Ok(b) => b,
            Err(e) => return Err(format!("WAV 読込失敗 {:?}: {}", wav_path, e)),
        };
        let req = crate::transcribe::TranscribeRequest {
            api_url: api_url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
            prompt: prompt.clone(),
            language: Some(crate::transcribe::DEFAULT_LANGUAGE.to_string()),
            audio,
            file_name: format!("chunk_{:03}.wav", chunk_index),
            mime_type: "audio/wav".to_string(),
            // 区間の時刻を使うため verbose_json 必須 (接続先は endpoints::resolve_segment_transcribe_endpoint)
            verbose_json: true,
        };
        let cancel = CancellationToken::new();
        match crate::transcribe::transcribe(client, req, cancel).await {
            Ok(resp) => {
                log::info!(
                    "[meeting] chunk {} transcribed: bytes={} elapsed_ms={}",
                    chunk_index,
                    resp.bytes_sent,
                    resp.elapsed_ms
                );
                return Ok(resp.body);
            }
            Err(e) => {
                last_err = format!("{:?}", e);
                if attempt < MAX_RETRIES {
                    let backoff_ms = 1000u64 << attempt;
                    log::warn!(
                        "[meeting] chunk {} retry {}/{} after {}ms: {}",
                        chunk_index,
                        attempt + 1,
                        MAX_RETRIES,
                        backoff_ms,
                        last_err
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    Err(format!(
        "chunk {} transcribe failed after {} retries: {}",
        chunk_index, MAX_RETRIES, last_err
    ))
}

/// background transcribe タスク本体。
///
/// 1. チャンクパス配列を最大 3 並列で transcribe (Semaphore で制御)
/// 2. 各 verbose_json body を parse_segments → apply_global_offset
/// 3. 全 segments を chunk_index 昇順で連結
/// 4. DB update_segments(id, segments_json, duration_ms)
/// 5. meeting:state-changed = Completed emit
///
/// 失敗時:
/// - 個別チャンク失敗 → 空 segments で続行 + log::error (全失敗してもプロセスは続行)
/// - 全体失敗 (HTTP client build / DB 書込失敗) → meeting:state-changed = Error emit + 中断
#[allow(clippy::too_many_arguments)]
async fn run_background_transcribe(
    app: AppHandle,
    meeting_id: String,
    chunk_paths: Vec<PathBuf>,
    total_duration_ms: u64,
    api_url: String,
    api_key: String,
    model: String,
    prompt: String,
    glossary_entries: Vec<(String, String, bool)>,
    llm_prompt: String,
) {
    let client = match crate::transcribe::build_client() {
        Ok(c) => c,
        Err(e) => {
            log::error!("[meeting] HTTP client build failed: {:?}", e);
            let _ = app.emit(
                EVENT_STATE_CHANGED,
                MeetingStateEvent {
                    state: MeetingState::Error,
                    id: Some(meeting_id),
                },
            );
            return;
        }
    };
    let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL));
    let mut tasks = Vec::with_capacity(chunk_paths.len());

    for (i, path) in chunk_paths.iter().cloned().enumerate() {
        let sem = semaphore.clone();
        let client_c = client.clone();
        let api_url_c = api_url.clone();
        let api_key_c = api_key.clone();
        let model_c = model.clone();
        let prompt_c = prompt.clone();
        let app_c = app.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let _ = app_c.emit(
                "meeting:progress",
                serde_json::json!({"chunk_index": i, "phase": "transcribing"}),
            );
            transcribe_chunk_with_retry(
                &client_c, path, i, api_url_c, api_key_c, model_c, prompt_c,
            )
            .await
        }));
    }

    // チャンク順に segments を収集 (chunk_index 昇順は spawn 順 = enumerate 順で保証)
    let mut all_segments: Vec<crate::meeting::MeetingSegment> = Vec::new();
    for (i, task) in tasks.into_iter().enumerate() {
        match task.await {
            Ok(Ok(body)) => match crate::meeting::parse_segments(&body, i) {
                Ok(segs) => {
                    let offsetted = crate::meeting::apply_global_offset(segs, i, CHUNK_SECS);
                    // Day 39 (2026-06-04) L340 リピート除去フィルタ配線。
                    // Whisper が無音/極小入力に対し同一フレーズを大量にループ生成する事案
                    // (Day 33-38 で観測: mean=0.005 以下で 200 回ループ等) の保険。
                    // チャンク内で同一テキストが N=5 以上連続したら 1 件に圧縮し、
                    // 圧縮件数を meeting:repeat-filtered event で frontend に通知する。
                    const REPEAT_THRESHOLD: usize = 5;
                    let (filtered, dropped) =
                        crate::meeting::filter_repeat_segments(&offsetted, REPEAT_THRESHOLD);
                    if dropped > 0 {
                        log::warn!(
                            "[meeting] chunk {} repeat-filter triggered: dropped {} duplicate segments (threshold={})",
                            i,
                            dropped,
                            REPEAT_THRESHOLD
                        );
                        let _ = app.emit(
                            "meeting:repeat-filtered",
                            serde_json::json!({
                                "chunk_index": i,
                                "dropped": dropped,
                                "threshold": REPEAT_THRESHOLD,
                            }),
                        );
                    }
                    all_segments.extend(filtered);
                    let _ = app.emit(
                        "meeting:progress",
                        serde_json::json!({"chunk_index": i, "phase": "parsed"}),
                    );
                }
                Err(e) => {
                    log::error!(
                        "[meeting] chunk {} parse_segments failed: {} (skip)",
                        i,
                        e
                    );
                }
            },
            Ok(Err(e)) => {
                log::error!("[meeting] chunk {} transcribe failed: {} (skip)", i, e);
            }
            Err(e) => {
                log::error!("[meeting] chunk {} join failed: {:?} (skip)", i, e);
            }
        }
    }

    // ---- v0.1.16 精度向上 ブロック2: prompt 漏洩 sanitize + 辞書適用 ----
    // 議事録 transcribe は従来 sanitize を通していなかったため、ここで段落化前の各 segment text に
    // 既存 sanitize (head fragment strip + prompt suffix strip) を適用してから辞書置換する。
    // 既存純粋関数を呼ぶだけ (本体不変・L294)。時刻 (start_ms/end_ms) は変更しない。
    let mut glossary_affected = 0usize;
    for seg in all_segments.iter_mut() {
        // prompt 漏洩防御 (通常録音 recorder.rs と同じ 2 経路)
        let cleaned = crate::prompt_sanitize::strip_prompt_leak_head_fragment(&seg.text);
        let cleaned = crate::prompt_sanitize::sanitize_prompt_leak(&prompt, &cleaned);
        // 辞書置換 (pattern → replacement)
        let (refined, applied) =
            crate::recorder::apply_glossary_replacements(&cleaned, &glossary_entries);
        if applied > 0 || refined != seg.text {
            glossary_affected += 1;
        }
        seg.text = refined;
    }
    log::info!(
        "[meeting] glossary applied: {} segments affected (entries={})",
        glossary_affected,
        glossary_entries.len()
    );

    // ---- 辞書適用済の生 segment を**先に**永続化 (Codex P2 / 録音データ保護) ----
    // LLM 後処理は best-effort かつ低速 (段落数 × API レイテンシ)。その await 前に segments_json を
    // 確定保存しておくことで、LLM 中にアプリ終了・ハングしても転写結果は失われず
    // txt/md/srt エクスポート可能 (refined が無くても segments から段落化フォールバック)。
    let segments_json = match serde_json::to_string(&all_segments) {
        Ok(s) => s,
        Err(e) => {
            log::error!("[meeting] segments JSON serialize failed: {}", e);
            let _ = app.emit(
                EVENT_STATE_CHANGED,
                MeetingStateEvent {
                    state: MeetingState::Error,
                    id: Some(meeting_id),
                },
            );
            return;
        }
    };
    {
        let state: tauri::State<Mutex<MeetingLogsDb>> =
            match app.try_state::<Mutex<MeetingLogsDb>>() {
                Some(s) => s,
                None => {
                    log::error!("[meeting] MeetingLogsDb state not managed");
                    let _ = app.emit(
                        EVENT_STATE_CHANGED,
                        MeetingStateEvent {
                            state: MeetingState::Error,
                            id: Some(meeting_id),
                        },
                    );
                    return;
                }
            };
        let db = match state.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!("[meeting] DB mutex poisoned: {}", e);
                let _ = app.emit(
                    EVENT_STATE_CHANGED,
                    MeetingStateEvent {
                        state: MeetingState::Error,
                        id: Some(meeting_id),
                    },
                );
                return;
            }
        };
        if let Err(e) =
            db.update_segments(&meeting_id, &segments_json, total_duration_ms as i64)
        {
            log::error!("[meeting] update_segments failed: {}", e);
            let _ = app.emit(
                EVENT_STATE_CHANGED,
                MeetingStateEvent {
                    state: MeetingState::Error,
                    id: Some(meeting_id),
                },
            );
            return;
        }
    }

    // ---- v0.1.16 精度向上 ブロック3: 段落単位 LLM 後処理 (フォールバック厳守) ----
    // 辞書適用済 all_segments を段落化 → 各段落を議事録専用プロンプトで LLM 整形。
    // 失敗・空・API キー欠落時は段落単位で辞書版にフォールバック (merge_refined_paragraphs)。
    // 全体が失敗しても segments は上で永続化済のため Completed に必ず到達する (録音データは消えない)。
    let _ = app.emit(
        "meeting:progress",
        serde_json::json!({"chunk_index": 0, "phase": "postprocess"}),
    );
    let base_paragraphs = crate::meeting::group_segments_into_paragraphs(
        &all_segments,
        PARAGRAPH_GAP_MS,
        PARAGRAPH_MAX_CHARS,
    );
    let refined_paragraphs = run_meeting_llm_postprocess(
        &client,
        &base_paragraphs,
        &llm_prompt,
    )
    .await;
    let refined_json = match serde_json::to_string(&refined_paragraphs) {
        Ok(s) => s,
        Err(e) => {
            // refined のシリアライズ失敗は致命ではない (segments から段落化フォールバック可能)。
            log::error!("[meeting] refined JSON serialize failed: {} (export will use segments)", e);
            String::new()
        }
    };

    // refined_json を別 DB 書き込みで保存 (失敗してもログ警告のみで Completed 継続)。
    if !refined_json.is_empty() {
        if let Some(state) = app.try_state::<Mutex<MeetingLogsDb>>() {
            match state.lock() {
                Ok(db) => {
                    if let Err(e) = db.update_refined(&meeting_id, &refined_json) {
                        log::warn!(
                            "[meeting] update_refined failed: {} (export will fall back to segments)",
                            e
                        );
                    }
                }
                Err(e) => log::warn!(
                    "[meeting] DB mutex poisoned at update_refined: {} (export will fall back)",
                    e
                ),
            }
        }
    }

    log::info!(
        "[meeting] background transcribe completed: id={} segments={} duration_ms={}",
        meeting_id,
        all_segments.len(),
        total_duration_ms
    );
    log::info!("[meeting][dbg] emit Completed: id={}", meeting_id);
    let _ = app.emit(
        EVENT_STATE_CHANGED,
        MeetingStateEvent {
            state: MeetingState::Completed,
            id: Some(meeting_id),
        },
    );
}

/// 各段落に LLM 後処理間隔を空けるための待機時間 (ms)。
/// Groq 分速トークン制限 (6,000/分) 回避のため段落を直列処理 + 小休止する。
const MEETING_LLM_INTER_PARAGRAPH_MS: u64 = 250;

/// v0.1.16 精度向上 ブロック3: 段落配列を議事録専用プロンプトで LLM 整形する。
///
/// **直列処理** (分速トークン制限回避)。各段落について `apply_llm_postprocess` を呼び、
/// 失敗・空・API キー欠落いずれの場合も `apply_llm_postprocess` 自体が**元テキストを返す**
/// (llm_postprocess.rs:62-73 のフォールバック契約) ため、戻り値が原文と同じなら
/// `None` 扱いにして `merge_refined_paragraphs` で辞書版にフォールバックさせる。
///
/// 段落が 0 件なら即空配列。**この関数は段落を 1 つも落とさない** (merge_refined_paragraphs
/// が originals.len() を保証)。これが M5「全体失敗でも Completed 到達」の核。
async fn run_meeting_llm_postprocess(
    client: &reqwest::Client,
    base_paragraphs: &[crate::meeting::Paragraph],
    llm_prompt: &str,
) -> Vec<crate::meeting::Paragraph> {
    // 整文の接続先が未設定なら LLM を一切呼ばず辞書版をそのまま使う (録音は守る)。
    let endpoint = match crate::endpoints::load_chat_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            log::warn!("[meeting] LLM postprocess skipped: {} (using dictionary version)", e);
            return base_paragraphs.to_vec();
        }
    };
    if base_paragraphs.is_empty() {
        return Vec::new();
    }

    let mut refined: Vec<Option<String>> = Vec::with_capacity(base_paragraphs.len());
    let total = base_paragraphs.len();
    for (i, p) in base_paragraphs.iter().enumerate() {
        // emoji_density="none" 固定 (議事録に絵文字不要)。
        // 専用プロンプト = 方針 + 固有名詞ヒント + 辞書登録済用語 (Day 37 build_meeting_llm_prompt)。
        // filler_removal=false: 議事録専用プロンプト側に既にフィラー整形指示が
        // あるため二重指示を避ける (言い直し解決の共通指示のみ効かせる)。
        let out = crate::llm_postprocess::apply_llm_postprocess(
            client,
            &endpoint,
            llm_prompt,
            &p.text,
            &crate::llm_postprocess::PostprocessOptions {
                emoji_density: "none".to_string(),
                filler_removal: false,
                // 議事録整形は同長〜短縮が原則 → 厳格閾値 (2026-07-23)
                allow_template_expansion: false,
                // 議事録は話し言葉を書き言葉へ整えるのが仕事 = 文体変換は仕様なので
                // 敬語化の検出対象から外す (2026-07-27)
                allow_style_change: true,
            },
        )
        .await;
        // apply_llm_postprocess は失敗時 p.text をそのまま返すため、
        // 「原文と異なる = LLM が実際に整形した」場合のみ採用する。原文一致は None でフォールバック。
        if out != p.text && !out.trim().is_empty() {
            refined.push(Some(out));
        } else {
            refined.push(None);
        }
        // 最終段落以外は分速制限回避のため小休止。
        if i + 1 < total {
            tokio::time::sleep(std::time::Duration::from_millis(
                MEETING_LLM_INTER_PARAGRAPH_MS,
            ))
            .await;
        }
    }
    let applied = refined.iter().filter(|r| r.is_some()).count();
    log::info!(
        "[meeting] LLM postprocess: {}/{} paragraphs refined (rest fell back to dictionary)",
        applied,
        total
    );
    // 段落を 1 つも落とさずマージ (時刻継承・フォールバック)。
    crate::meeting::merge_refined_paragraphs(base_paragraphs, &refined)
}

/// 議事録一覧を取得する。
/// `limit` 未指定時は 50 件、`offset` 未指定時は 0。
#[tauri::command]
pub fn meeting_list(
    app: AppHandle,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<MeetingLogDto>, String> {
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
    let rows = db
        .list(limit.unwrap_or(50), offset.unwrap_or(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(MeetingLogDto::from).collect())
}

/// 議事録 1 件取得。該当 id がなければ Ok(None)。
#[tauri::command]
pub fn meeting_get(app: AppHandle, id: String) -> Result<Option<MeetingLogDto>, String> {
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
    let row = db.get(&id).map_err(|e| e.to_string())?;
    Ok(row.map(MeetingLogDto::from))
}

/// 議事録 1 件削除。返り値: true = 削除した / false = 該当なし。
///
/// Day 2: DB 削除に加え chunk_dir 配下の WAV ファイル一式を `remove_dir_all` で削除する。
/// chunk_dir が存在しない場合はスキップ。削除に失敗してもログ警告だけ出して
/// DB 削除は成功扱いで返す (孤立ファイルを残しても DB との整合は壊さない方針)。
#[tauri::command]
pub fn meeting_delete(app: AppHandle, id: String) -> Result<bool, String> {
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;

    // 先に chunk_dir を取得 (DB 削除後では参照できないため)。
    let row = {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        db.get(&id).map_err(|e| e.to_string())?
    };

    let deleted = {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        db.delete(&id).map_err(|e| e.to_string())?
    };

    // DB 削除成功時のみ chunk_dir を remove_dir_all (失敗してもログ警告で続行)
    if deleted {
        if let Some(r) = row {
            if !r.chunk_dir.is_empty() {
                let path = std::path::PathBuf::from(&r.chunk_dir);
                if path.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        log::warn!(
                            "[meeting] chunk_dir 削除失敗 {:?}: {} (DB 行は削除済)",
                            path,
                            e
                        );
                    } else {
                        log::info!("[meeting] chunk_dir 削除 OK {:?}", path);
                    }
                }
            }
        }
    }
    Ok(deleted)
}

/// 議事録を指定形式 (txt / md / srt) でエクスポートする。
///
/// Day 3 本実装:
/// 1. DB から row 取得 (segments_json が "[]" / 空なら転写未完了で Err)
/// 2. format に応じて整形 (txt/md は paragraph 単位 / srt は segment 単位)
/// 3. dest_dir に create_dir_all して `meeting_<YYYYMMDD_HHmmss>_<title>.<ext>` を書き出し
/// 4. 書き出したフル絶対パスを文字列で返す
#[tauri::command]
pub async fn meeting_export(
    app: AppHandle,
    id: String,
    format: String,
    dest_dir: String,
) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err("id is empty".into());
    }
    let format_l = format.to_lowercase();
    if !matches!(format_l.as_str(), "txt" | "md" | "srt") {
        return Err(format!("未対応形式: {}", format));
    }

    // DB から取得
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    let row = {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        db.get(&id).map_err(|e| e.to_string())?
    }
    .ok_or_else(|| format!("id={} が存在しません", id))?;

    if row.segments_json == "[]" || row.segments_json.is_empty() {
        return Err("転写未完了 (segments 空)".to_string());
    }

    // segments デシリアライズ
    let segments: Vec<crate::meeting::MeetingSegment> =
        serde_json::from_str(&row.segments_json).map_err(|e| format!("segments parse: {}", e))?;

    // 出力先 dir 作成 (冪等)
    let dest = std::path::PathBuf::from(&dest_dir);
    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("dest_dir 作成失敗 {:?}: {}", dest, e))?;

    // ファイル名: meeting_YYYYMMDD_HHmmss_<title>.<ext>
    let datetime = format_filename_datetime(row.start_at);
    let title_safe = sanitize_title(&row.title);
    let filename = format!("meeting_{}_{}.{}", datetime, title_safe, format_l);
    let out_path = dest.join(&filename);

    // v0.1.16 精度向上 ブロック3: txt/md は LLM 整形済段落 (refined_json) を優先。
    // refined が無い (旧録音・LLM 失敗) 場合は従来どおり segments を段落化してフォールバック。
    // srt は字幕仕様慣習どおり常に segment 単位 (辞書適用済 segments)。
    let refined_paragraphs: Option<Vec<crate::meeting::Paragraph>> = {
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        match db.get_refined(&id).map_err(|e| e.to_string())? {
            Some(json) => serde_json::from_str::<Vec<crate::meeting::Paragraph>>(&json).ok(),
            None => None,
        }
    };
    let resolve_paragraphs = || -> Vec<crate::meeting::Paragraph> {
        match &refined_paragraphs {
            Some(p) if !p.is_empty() => p.clone(),
            _ => crate::meeting::split_into_paragraphs(
                &segments,
                PARAGRAPH_GAP_MS,
                PARAGRAPH_MAX_CHARS,
            ),
        }
    };

    // 整形
    let content = match format_l.as_str() {
        "txt" => crate::meeting::format_txt(&resolve_paragraphs()),
        "md" => crate::meeting::format_md(&resolve_paragraphs(), &row.title),
        "srt" => crate::meeting::format_srt(&segments),
        _ => unreachable!(),
    };

    std::fs::write(&out_path, content)
        .map_err(|e| format!("書出失敗 {:?}: {}", out_path, e))?;

    // Day 39 履歴リンク機能: md 形式 export 完了時は DB に絶対パスを保存し、
    // frontend 履歴アイテムの「md を開く」ボタンで再オープン可能にする。
    // txt/srt は履歴リンクの対象外 (仕様 = 既定 md)。失敗してもエクスポート自体は成功扱い。
    if format_l == "md" {
        let abs_path = out_path.to_string_lossy().to_string();
        let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
        if let Err(e) = db.update_export_md_path(&id, &abs_path) {
            log::warn!(
                "[meeting] update_export_md_path failed for id={}: {} (export 自体は成功)",
                id, e
            );
        }
    }

    log::info!("[meeting] exported {:?} ({})", out_path, format_l);
    Ok(out_path.to_string_lossy().to_string())
}

/// Day 39 履歴リンク機能: 保存済みの md export 絶対パスを取得する Tauri command。
/// 該当 id がない or 未 export なら Err。frontend は Err を受け取ったら
/// 「先に md エクスポートしてください」表示にフォールバックする想定。
#[tauri::command]
pub async fn meeting_get_export_md_path(
    app: AppHandle,
    id: String,
) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err("id is empty".into());
    }
    let state: State<Mutex<MeetingLogsDb>> = app
        .try_state::<Mutex<MeetingLogsDb>>()
        .ok_or_else(|| "MeetingLogsDb state not managed".to_string())?;
    let db = state.lock().map_err(|e| format!("mutex poisoned: {}", e))?;
    let path = db
        .get_export_md_path(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("id={} の md export がまだありません", id))?;
    Ok(path)
}

/// Day 39 履歴リンク機能: 既定の議事録 export ディレクトリ絶対パスを返す Tauri command。
/// frontend「フォルダを開く」ボタンで explorer.exe / Finder を起動する用途。
/// 未存在でも作成して返す (mkdir 失敗時は Err)。
#[tauri::command]
pub async fn meeting_get_export_dir() -> Result<String, String> {
    // 既定 export 先 = ~/Documents/RushVoice議事録/
    // Windows: %USERPROFILE%\Documents\RushVoice議事録
    // macOS:   ~/Documents/RushVoice議事録
    let dir = dirs::document_dir()
        .ok_or_else(|| "Documents ディレクトリが取得できません".to_string())?
        .join("RushVoice議事録");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("dir 作成失敗 {:?}: {}", dir, e))?;
    Ok(dir.to_string_lossy().to_string())
}

/// epoch secs → "YYYYMMDD_HHmmss" (UTC)。
/// chrono 0.4 が既存依存 (Cargo.toml で確認済) のため流用。
fn format_filename_datetime(epoch_secs: i64) -> String {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = DateTime::from_timestamp(epoch_secs, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
    dt.format("%Y%m%d_%H%M%S").to_string()
}

/// タイトルからファイル名に使えない文字を除去 (Windows 禁則: < > : " / \ | ? *)。
/// 空タイトルは "untitled"。
fn sanitize_title(title: &str) -> String {
    let t = title.trim();
    if t.is_empty() {
        return "untitled".to_string();
    }
    t.chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// generate_meeting_id: 32 hex 文字列を返す。
    #[test]
    fn test_generate_meeting_id_format() {
        let id = generate_meeting_id();
        assert_eq!(id.len(), 32, "32 hex 字: {}", id);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "hex 字のみ: {}",
            id
        );
    }

    /// generate_meeting_id: 連続呼出で衝突しない (counter で差別化される)。
    #[test]
    fn test_generate_meeting_id_unique() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = generate_meeting_id();
            assert!(ids.insert(id), "1000 件中重複あり");
        }
    }

    /// MeetingState の serde 出力が lowercase。
    #[test]
    fn test_meeting_state_serde_lowercase() {
        let s = serde_json::to_string(&MeetingState::Recording).unwrap();
        assert_eq!(s, "\"recording\"");
        let s = serde_json::to_string(&MeetingState::Completed).unwrap();
        assert_eq!(s, "\"completed\"");
    }

    // ---------------- v0.1.16 Day 2: chunk_dir mkdir / remove_dir_all 挙動 ----------------

    /// std::fs::create_dir_all は冪等で chunk_dir の概念を満たす
    /// (meeting_start 内の chunk_dir 構築ロジックの sanity 確認)。
    #[test]
    fn test_meeting_start_chunk_dir_concept() {
        let tmp = std::env::temp_dir().join("rushvoice_meeting_dir_test_d2");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create_dir_all");
        assert!(tmp.exists() && tmp.is_dir());
        // 2 回呼んでもエラーにならない (冪等)
        std::fs::create_dir_all(&tmp).expect("create_dir_all idempotent");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// meeting_delete の remove_dir_all 挙動: 存在する dir は削除、
    /// 存在しない dir は exists() ガードでスキップ (1 回目成功・2 回目スキップ)。
    #[test]
    fn test_meeting_delete_remove_dir_all_idempotent() {
        let tmp = std::env::temp_dir().join("rushvoice_meeting_delete_test_d2");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create_dir_all");
        std::fs::write(tmp.join("chunk_000.wav"), b"dummy").expect("write chunk");
        // 1 回目: 存在 → 削除成功
        assert!(tmp.exists());
        std::fs::remove_dir_all(&tmp).expect("remove 1");
        assert!(!tmp.exists());
        // 2 回目: 存在しない → meeting_delete は exists() ガードでスキップする想定
        // (生 remove_dir_all は Err になるため、commands 側の exists() ガードが必要であることを示す)
        let exists_after = tmp.exists();
        assert!(!exists_after);
    }

    // ---------------- v0.1.16 Day 3: ファイル名整形 / sanitize_title ----------------

    #[test]
    fn test_format_filename_datetime_zero_epoch() {
        let s = format_filename_datetime(0);
        assert_eq!(s, "19700101_000000");
    }

    #[test]
    fn test_format_filename_datetime_known_value() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let s = format_filename_datetime(1_704_067_200);
        assert_eq!(s, "20240101_000000");
    }

    #[test]
    fn test_sanitize_title_strips_windows_forbidden_chars() {
        let s = sanitize_title("foo<bar>:\"/\\|?*baz");
        assert_eq!(s, "foobarbaz");
    }

    #[test]
    fn test_sanitize_title_empty_falls_back_to_untitled() {
        assert_eq!(sanitize_title(""), "untitled");
        assert_eq!(sanitize_title("   "), "untitled");
    }

    #[test]
    fn test_sanitize_title_keeps_japanese_and_safe_chars() {
        let s = sanitize_title("会議 5月31日_A-1");
        assert_eq!(s, "会議 5月31日_A-1");
    }

    /// MeetingLogRow から DTO 変換でフィールドが保持される。
    #[test]
    fn test_dto_from_row_roundtrip() {
        let row = MeetingLogRow {
            id: "abc".to_string(),
            start_at: 100,
            duration_ms: 200,
            title: "T".to_string(),
            segments_json: "[]".to_string(),
            chunk_dir: "/tmp".to_string(),
            created_at: 100,
        };
        let dto = MeetingLogDto::from(row.clone());
        assert_eq!(dto.id, row.id);
        assert_eq!(dto.start_at, row.start_at);
        assert_eq!(dto.duration_ms, row.duration_ms);
        assert_eq!(dto.title, row.title);
        assert_eq!(dto.segments_json, row.segments_json);
        assert_eq!(dto.chunk_dir, row.chunk_dir);
        assert_eq!(dto.created_at, row.created_at);
    }
}
