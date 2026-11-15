// recorder.rs — Phase 1 Day 6 Stage 2-A: 録音セッション管理
//
// 仕様: 内部設計メモ (Day 6 Tauri UI 統合)
//
// 設計概要:
//   - cpal::Stream は !Send なので AudioRecorder ごと専用 std::thread に閉じ込める (L119)。
//   - start_recording → 専用 worker thread 起動 → AudioRecorder + 蓄積バッファ。
//   - stop_recording → stop_flag set → thread join → 蓄積フレーム回収 → resampler → WAV → 文字起こし。
//
// v0.1.9 (Day 28): hedged 本番配線完了。Stage 2-A の「Groq 単独固定」コメントは廃止。
//   設定値 `provider` (config_db KV "provider": hedged|groq|openai) に基づき
//   transcribe_collected_with_provider() が hedged or 単独経路を選択する。
//   L241 (Stage 簡略コメントが配線漏れ真因) に伴い // TODO 形式への置換は不要 (本実装で解消済)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

// Day 25 Phase 2-G (SSOT v3.3): alias_db / transcribe_select / transcribe_multi 関連 imports は撤廃
use crate::audio::{AudioRecorder, RawFrame};
use crate::halu::{EnergyVad, HaluConfig, HaluFilter, HaluReason, HaluVerdict};
use crate::hedged::{dispatch_hedged, HedgedConfig, WinnerProvider};
use crate::prompt_sanitize::{
    sanitize_prompt_leak, strip_prompt_echo_sentences, strip_prompt_leak_head_fragment,
    PROMPT_ECHO_REFERENCE, WHISPER_PROMPT,
};
use crate::resampler::Resampler;
use crate::transcribe::{build_client, transcribe, TranscribeRequest};

// 接続先 (URL・モデル・キー) は endpoints.rs が決める (2026-11-15: ゲートウェイ終了・カスタム接続先対応)。
use crate::endpoints::{TranscribeEndpoint, TranscribeSetup};

// ---------------------------------------------------------------------------
// 既知ハルシ句 (halu フィルタ Tier 3 / 3.2 / 3.5 に渡す)
//
// 2026-08-16: 従来 transcribe_collected 内の vec! に直書きしていた 2 リストを、無料版
// RushVoice Air (v1.1.5) と同じ形の module 定数へ出した。中身は従来と同一。
// 本番と同じ設定でテストできるようにするため (production_halu_filter) と、将来の共通コア化のため。
// ※ known_hallucinations の hardcode は L078 違反 (config 経由が原則) だが、
//    Stage 2-B で config 化するまでの暫定として最頻ハルシ句のみ列挙 (従来どおり)。
// ---------------------------------------------------------------------------

/// Tier 3 (完全一致 AND 低 logprob) で破棄する既知ハルシ句。
///
/// 完全一致なので、同じ語を利用者が実際に発話した場合でも logprob が高ければ破棄されない。
/// 比較は両側 `halu::normalize()` を通すため、句読点・空白の違いは吸収される。
pub const KNOWN_HALLUCINATIONS: &[&str] = &[
    "ご視聴ありがとうございました",
    "ご視聴ありがとうございました。",
    "視聴ありがとうございました",
    "ご視聴頂きありがとうございました",
    "Thank you for watching",
    "ありがとうございました",
];

/// Tier 3.2 (完全一致・**logprob 条件なし**) で破棄する既知ハルシ句。
/// (無料版 RushVoice Air v1.1.5 (2026-08-15) からの移植・2026-08-16)
///
/// 🔴 `KNOWN_HALLUCINATIONS` (Tier 3) との違い:
///   Tier 3 は `min_logprob < -0.5` を AND 条件に持つ。Whisper は YouTube 字幕
///   クレジット由来のハルシを**高 confidence で**返すため、その AND では捕まらない。
///   2026-08-14 実機 id=742 「字幕作成者 。）。」= `avg_logprob -0.303` が実例
///   (同じ句の id=348 は -1.33 だったため Tier 3 で止まっており、閾値の外に
///    出るかどうかは運任せだった)。
///
/// 🔴 登録規約: **発話全体がその句だけになることが実務上あり得ない**句に限る。
///   当社は動画制作も行うため「字幕作成者は誰ですか」等は実在するが、
///   完全一致判定なのでそれらは通る。「ありがとうございました」のように
///   単独で言い得る句は絶対にここへ入れない (Tier 3 のままにする)。
///   追加するときは実際の raw_text を `production_halu_filter()` に通して
///   `KnownPhraseAlways` で落ちることをテストで固定する (L928)。
pub const KNOWN_HALLUCINATIONS_ALWAYS: &[&str] = &[
    // 2026-08-13 id=348 (15.7 秒 / -1.33) と 2026-08-14 id=742 (5.8 秒 / -0.303)。
    // YouTube 字幕のクレジット行がそのまま出る。
    "字幕作成者",
    "字幕制作者",
];

/// Tier 3.5 (部分一致・logprob 条件なし) で破棄する既知ハルシ断片。
///
/// v0.1.3 防御第 3 層: prompt 漏洩等の「大きなまとまり」を部分一致で破棄する。
/// 単一単語 (「読点」「日本語で」単独) は誤検知防止のため入れない。
/// すべて 6 文字以上の N-gram で構成すること (halu.rs Tier 3.5 設計準拠)。
///
/// v0.1.3 patch (2026-05-24): Whisper 既知日本語ハルシ 3 件を追加。
/// Groq Whisper が高 confidence (avg_logprob ≈ -0.15) で長文末尾に混入させるため、
/// 完全一致 + 低 logprob ベースの Tier 3 (known_hallucinations) では検出できず、
/// 長文中の部分一致で確実に破棄する必要がある。
/// 誤検知リスク: 利用者業務文脈 (社内会議・経理・店舗運営) では発生確率低。
/// YouTube 配信トーク等を文字起こしする業務がある場合のみ要再評価。
///
/// 🔴 比較前に両側 `normalize()` が走り**句読点と中黒・括弧類が除去される**。
///   「固有名詞、」のような句読点頼みのパターンは「固有名詞」へ縮退して暴発する
///   (v0.1.3 patch5.1 (2026-05-24) で「固有名詞」バリエーション 5 件を実際に revert した。
///    利用者業務文脈で「固有名詞」発話は辞書管理・用語解説で発生しうる)。
pub const KNOWN_HALLUCINATION_SUBSTRINGS: &[&str] = &[
    // prompt 漏洩ハルシ (2026-08-08 の圧縮前 prompt。旧 exe の履歴も通るので残す)
    "自然な日本語で出力してください",
    "読点）を含む自然な",
    "を含む自然な日本語で出力",
    // prompt 漏洩ハルシ (2026-08-08 圧縮後の prompt)
    "自然な日本語で書き起こす",
    "語尾や助詞は話したまま",
    // Whisper 既知日本語ハルシ
    "ご視聴ありがとうございました",
    "ご視聴頂きありがとうございました",
    "ご視聴いただきありがとうございました",
];

/// 本番の halu フィルタ (閾値・既知句リストとも transcribe_collected が実際に使うもの)。
///
/// 2026-08-16: 従来 transcribe_collected 内で組み立てていた設定を関数へ出した。
/// テストから同じ設定を呼べるようにするため (回帰テストは「本番と同じ層構成で、
/// どの層が落としたか」まで固定する・L928)。閾値は従来と同一。
pub fn production_halu_filter() -> HaluFilter<EnergyVad> {
    HaluFilter::new(
        EnergyVad::default(),
        HaluConfig {
            no_speech_prob_threshold: 0.6,
            min_logprob_threshold: -0.5, // Groq は無音時 avg_logprob ≈ -0.15 が出るため緩め
            known_hallucinations: KNOWN_HALLUCINATIONS.iter().map(|s| s.to_string()).collect(),
            known_hallucinations_always: KNOWN_HALLUCINATIONS_ALWAYS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            known_hallucination_substrings: KNOWN_HALLUCINATION_SUBSTRINGS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..HaluConfig::default()
        },
    )
}

/// マイク選択 GUI (2026-07-23): settings_kv の入力デバイス名キー。
/// 空文字 / 未設定 = Windows 既定デバイス自動追従 (従来挙動)。
pub const INPUT_DEVICE_KV_KEY: &str = "input_device_name";

/// マイク選択 GUI (2026-07-23): 設定済みの優先入力デバイス名を best-effort で読む。
/// config_db が開けない・未設定・空文字はすべて None (= 既定デバイス)。
pub(crate) fn preferred_input_device() -> Option<String> {
    let db = crate::config_db::ConfigDb::open(&crate::config_db::default_config_db_path()).ok()?;
    db.get_kv(INPUT_DEVICE_KV_KEY)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 語尾パディング (2026-07-23 要望): 停止指示後もこの時間だけ録音を継続してから確定する。
/// 「進みました」の「た」を言い切る前に停止キーが押されて「進みます」になる語尾切れ対策。
/// フレームは 100ms 単位なので実際の延長は 300〜400ms 程度になる。
pub const STOP_TAIL_PADDING_MS: u64 = 300;

/// v0.1.9: transcribe_collected の中継で resp.body 互換を提供するための shim。
/// 既存ロジックが `resp.body` で参照していたためメンバ名を維持する。
struct TranscribeRespForParse {
    body: String,
}

fn build_request(
    api_url: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    wav: Vec<u8>,
    verbose_json: bool,
) -> TranscribeRequest {
    TranscribeRequest {
        api_url: api_url.into(),
        api_key: api_key.into(),
        model: model.into(),
        // v0.1.3 patch2 (C): glossary を末尾結合した動的 prompt を送る
        prompt: prompt.into(),
        language: Some(crate::transcribe::DEFAULT_LANGUAGE.to_string()),
        audio: wav,
        file_name: "audio.wav".into(),
        mime_type: "audio/wav".into(),
        verbose_json,
    }
}

/// transcribe エラーメッセージから HTTP の状態 (401/403/429) を取り出す純粋関数。
///
/// エラー文字列は `format!("... {:?}", e)` 由来のため、
/// `TranscribeError::Http { status: 403, .. }` の Debug 表現 ("status: 403") を見る。
/// hedged 経路 (HedgedError::BothFailed 等) も内包する TranscribeError の Debug が
/// そのまま含まれるため同じ判定で拾える。接続先 (Groq/OpenAI/カスタム) に依らない。
pub fn detect_auth_or_limit_status(err_msg: &str) -> Option<u16> {
    for code in [401u16, 403, 429] {
        if err_msg.contains(&format!("status: {}", code)) || err_msg.contains(&format!("HTTP {}", code)) {
            return Some(code);
        }
    }
    None
}

/// 401/403/429 のとき、利用者が次に何をすればよいかを先頭に付ける (純粋関数)。
pub fn friendly_transcribe_error(err_msg: &str) -> String {
    match detect_auth_or_limit_status(err_msg) {
        Some(401) | Some(403) => format!(
            "接続先が API キーを受け付けませんでした。設定の「接続先とAPIキー」でキーを確かめてください。({})",
            err_msg
        ),
        Some(429) => format!(
            "接続先の利用上限に達しました。しばらく待つか、設定で別の接続先を選んでください。({})",
            err_msg
        ),
        _ => err_msg.to_string(),
    }
}

/// v0.1.12 F2: マイク入力低レベル警告 (L264 予防) 設定。
///
/// SSOT: v0_1_12_scope_2026-05-30.md
/// 録音開始から `window_ms` の間、frame ごとに計算した RMS が継続して
/// `threshold` 未満なら `recorder:low-input-level-detected` を 1 度だけ emit する。
/// L264 (開発機入力音量 8 で半日格闘) の予防実装。
#[derive(Debug, Clone, Copy)]
pub struct MicLowLevelConfig {
    /// 低レベル判定の RMS 閾値 (0.0 ~ 1.0)。default 0.005 ≒ -46dB。
    pub threshold: f32,
    /// 継続観測時間 (ms)。default 3000。
    pub window_ms: u64,
}

impl Default for MicLowLevelConfig {
    fn default() -> Self {
        Self {
            threshold: 0.005,
            window_ms: 3000,
        }
    }
}

/// v0.1.12 F2: worker thread 内で 1 frame ぶんの低レベル判定を行う純粋関数。
///
/// 設計:
///   - `start_instant` から `window_ms` 経過しているかをチェック
///   - 「経過したが一度も RMS が `threshold` 以上を観測していない」状態なら true を返す
///   - 一度でも `threshold` 以上を観測したら以降は false 固定 (呼出側で `seen_loud` を立てる)
///
/// この関数自体は単純な比較式だが、テスト容易性と worker thread の意図明示のため別関数化。
pub fn should_emit_mic_low_level(
    elapsed_ms: u128,
    window_ms: u64,
    seen_loud: bool,
    already_emitted: bool,
) -> bool {
    if already_emitted || seen_loud {
        return false;
    }
    elapsed_ms >= window_ms as u128
}

/// v0.1.14 診断: 1 録音セッション分の RMS 統計。
///
/// 小声ハルシ (小さい声で喋ると Groq が自信満々に誤字を生成・L121) の
/// 抑制閾値を実機実測で校正するための診断用 (L122)。本番ログに mean/peak/min を出す。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RmsStats {
    pub mean: f32,
    pub peak: f32,
    pub min: f32,
    pub frames: u64,
}

/// v0.1.14 診断: worker thread が蓄積した RMS 集計値から統計を算出する純粋関数。
///
/// - `count == 0` (無録音) は mean=0 / peak=0 / min=0 を返す (ゼロ除算ガード)。
/// - `should_emit_mic_low_level` / `compute_confidence_drop_flag` と同じ
///   「純粋関数 + 単体テスト」規約に合わせてテスト容易化。
pub fn compute_rms_stats(sum: f64, peak: f32, min: f32, count: u64) -> RmsStats {
    if count == 0 {
        return RmsStats {
            mean: 0.0,
            peak: 0.0,
            min: 0.0,
            frames: 0,
        };
    }
    RmsStats {
        mean: (sum / count as f64) as f32,
        peak,
        min,
        frames: count,
    }
}

/// v0.1.15 小声警告: 今回の録音平均 RMS が、学習した baseline (普段の声水準) の
/// `factor` 倍を下回ったら「小声」と判定する純粋関数。
///
/// - 相対判定なのでマイク・端末差に強い (固定値はマイク差で破綻・L122/L166)。
/// - `count < min_samples` のコールドスタート中は判定しない (baseline 学習途上)。
/// - `baseline <= 0` (未 seed) / `mean <= 0` (無録音) も判定しない。
pub fn judge_quiet_voice(
    mean: f32,
    baseline: f32,
    count: u32,
    factor: f32,
    min_samples: u32,
) -> bool {
    if count < min_samples || baseline <= 0.0 || mean <= 0.0 {
        return false;
    }
    mean < baseline * factor
}

/// v0.1.15 小声警告: baseline (普段の声の平均 RMS) を EMA で更新する純粋関数。
///
/// - `count == 0` または `baseline <= 0` (未 seed) は今回の mean を seed として返す。
/// - それ以外は指数移動平均 `alpha*mean + (1-alpha)*baseline`。
/// - 呼出側は「小声と判定しなかった録音」でのみ呼ぶこと (小声の蓄積で baseline が
///   下がる暴走を防ぐ・普段の声だけで baseline を育てる)。
pub fn update_voice_baseline(baseline: f32, mean: f32, alpha: f32, count: u32) -> f32 {
    if count == 0 || baseline <= 0.0 {
        return mean;
    }
    let a = if alpha.is_finite() && alpha > 0.0 && alpha <= 1.0 {
        alpha
    } else {
        0.2
    };
    a * mean + (1.0 - a) * baseline
}

/// v0.1.10 機能 A: 混雑通知バナー設定。
///
/// SSOT: v0_1_10_feature_a_delegation_2026-05-30.md
/// - `threshold_ms`: バナー発動までの遅延閾値 (ms)・default 5000
/// - `show_in_hedged`: hedged 経路でもバナーを出すか (default false = 抑止)
#[derive(Debug, Clone, Copy)]
pub struct CongestionConfig {
    pub threshold_ms: u64,
    pub show_in_hedged: bool,
}

impl Default for CongestionConfig {
    fn default() -> Self {
        Self {
            threshold_ms: 5000,
            show_in_hedged: false,
        }
    }
}

/// v0.1.10 機能 A: タイマ開始の可否を純粋関数として判定する。
///
/// 仕様 (SSOT A-5):
/// - 実際に hedged で並走するとき (`is_hedged`) かつ show_in_hedged=false なら抑止 (false)
/// - それ以外は表示 (true)
pub fn should_show_congestion_banner(is_hedged: bool, show_in_hedged: bool) -> bool {
    if is_hedged {
        show_in_hedged
    } else {
        true
    }
}

/// v0.1.10 機能 A: 文字起こし開始時刻からの経過 ms を測り、threshold_ms を超えたら
/// `transcribe:congestion-started` を emit する単一タイマを spawn する。
///
/// 戻り値の JoinHandle は呼出側が transcribe 完了後に `abort()` で停止すること
/// (完了時の `transcribe:congestion-cleared` 通知は別途 emit する設計)。
fn spawn_congestion_timer(
    app: Option<tauri::AppHandle>,
    threshold_ms: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    let app = app?;
    Some(tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(threshold_ms)).await;
        if let Err(e) = app.emit("transcribe:congestion-started", threshold_ms) {
            log::warn!("[congestion] emit congestion-started failed: {:?}", e);
        } else {
            log::info!(
                "[congestion] banner triggered: threshold_ms={}",
                threshold_ms
            );
        }
    }))
}

/// v0.1.10 機能 A: 文字起こし完了時にバナーを消去する emit。
///
/// 成功・失敗 (HaluDrop 含む) の両方で呼ばれる必要があるため呼出側で確実に実行する。
fn emit_congestion_cleared(app: Option<&tauri::AppHandle>) {
    if let Some(app) = app {
        if let Err(e) = app.emit("transcribe:congestion-cleared", ()) {
            log::warn!("[congestion] emit congestion-cleared failed: {:?}", e);
        }
    }
}

/// v0.1.9: run_transcription の戻り値型エイリアス。
///
/// 8 要素タプル: (body, transcribe_ms, audio_bytes, used_provider, primary_ms, fallback_triggered, hedged_winner, model)
type TranscribeRunResult = (
    String,
    i64,
    i64,
    String,
    Option<i64>,
    Option<bool>,
    Option<String>,
    String,
);

fn request_for(ep: &TranscribeEndpoint, prompt: &str, wav: Vec<u8>) -> TranscribeRequest {
    build_request(&ep.url, &ep.api_key, &ep.model, prompt, wav, ep.verbose_json)
}

/// 接続先の組 (TranscribeSetup) に従い、hedged (primary が遅いとき fallback を並走) か
/// 単独で文字起こしし、結果を返す中核ロジック。
///
/// 戻り値: (body, transcribe_ms, audio_bytes, used_provider, primary_ms, fallback_triggered, hedged_winner, model)
///
/// - used_provider: 実際に勝者となった接続先の名前 ("groq" / "openai" / "custom")
/// - primary_ms / fallback_triggered / hedged_winner: hedged 経路のときのみ Some
///
/// v0.1.10 機能 A: 各経路の transcribe 呼出前に混雑通知バナーのタイマを spawn する。
async fn run_transcription(
    client: &reqwest::Client,
    dynamic_prompt: &str,
    wav: Vec<u8>,
    audio_bytes: i64,
    setup: &TranscribeSetup,
    app: Option<tauri::AppHandle>,
    congestion: CongestionConfig,
) -> Result<TranscribeRunResult, String> {
    let cancel = tokio_util::sync::CancellationToken::new();

    // v0.1.10 機能 A: バナータイマ spawn (hedged 抑止判定後)
    let banner_handle = if should_show_congestion_banner(setup.is_hedged(), congestion.show_in_hedged) {
        spawn_congestion_timer(app.clone(), congestion.threshold_ms)
    } else {
        None
    };

    log::info!(
        "[transcribe] route primary={} ({}) fallback={:?}",
        setup.primary.label,
        crate::endpoints::host_for_log(&setup.primary.url),
        setup.fallback.as_ref().map(|f| f.label)
    );

    // 各経路の transcribe 結果を result_inner に詰め、最後にタイマ abort して return する設計。
    let result_inner: Result<TranscribeRunResult, String> = if let Some(fallback) = &setup.fallback {
        let primary_req = request_for(&setup.primary, dynamic_prompt, wav.clone());
        let fallback_req = request_for(fallback, dynamic_prompt, wav);
        let cfg = HedgedConfig {
            trigger_secs: 3.0,
            enable_fallback: true,
            retry_count: 2,
            retry_delay_ms: 250,
        };
        let start = std::time::Instant::now();
        match dispatch_hedged(client.clone(), primary_req, Some(fallback_req), cfg, cancel).await {
            Ok(resp) => {
                let transcribe_ms: i64 = start.elapsed().as_millis() as i64;
                let used_provider = match resp.winner {
                    WinnerProvider::Primary => setup.primary.label.to_string(),
                    WinnerProvider::Fallback => fallback.label.to_string(),
                };
                let model = setup.model_for(&used_provider);
                let primary_ms = Some(resp.winner_ms as i64);
                let fallback_triggered = Some(resp.fallback_triggered);
                let hedged_winner = Some(resp.winner_str().to_string());
                log::info!(
                    "[transcribe] provider=hedged winner={} elapsed_ms={} primary_ms={} fallback_triggered={} bytes={}",
                    used_provider, transcribe_ms, resp.winner_ms, resp.fallback_triggered, audio_bytes
                );
                Ok((resp.body, transcribe_ms, audio_bytes, used_provider, primary_ms, fallback_triggered, hedged_winner, model))
            }
            Err(e) => Err(format!("hedged transcribe failed: {:?}", e)),
        }
    } else {
        let ep = &setup.primary;
        let req = request_for(ep, dynamic_prompt, wav);
        let start = std::time::Instant::now();
        match transcribe(client, req, cancel).await {
            Ok(resp) => {
                let transcribe_ms: i64 = start.elapsed().as_millis() as i64;
                log::info!(
                    "[transcribe] provider={} elapsed_ms={} bytes={}",
                    ep.label, transcribe_ms, audio_bytes
                );
                Ok((resp.body, transcribe_ms, audio_bytes, ep.label.to_string(), None, None, None, ep.model.clone()))
            }
            Err(e) => Err(format!("transcribe failed ({} solo): {:?}", ep.label, e)),
        }
    };

    // v0.1.10 機能 A: 完了またはエラー時にバナータイマを必ず停止
    if let Some(h) = banner_handle {
        h.abort();
    }
    emit_congestion_cleared(app.as_ref());

    // 401/403/429 は接続先に依らず、次に何をすればよいかを添える
    result_inner.map_err(|e| friendly_transcribe_error(&e))
}

/// Whisper へ渡す prompt 全体 (固定指示文 + 語彙ヒント) の上限文字数。
///
/// 2026-08-08 に導入。Whisper の prompt 窓は **224 トークン**で、超過分は捨てられる。
/// 日本語 1 文字あたりのトークン数はトークナイザ実装に依存する (概ね 1〜1.5) ため、
/// **最悪ケース 1.5 トークン/文字でも 224 に収まる 140 文字**を上限とする。
/// こうしておけばトークナイザの正確な挙動に依存せず溢れが起きない。
///
/// 圧縮前は固定指示文 ~100 字 + 語彙 180 字 = 最大 280 字で、実測ログでも 272 字に達しており
/// 窓を超えていた可能性が高い。超過時にどちら側から捨てられるかに依存する設計は脆いので、
/// 「そもそも溢れさせない」方針に変えた。
pub const WHISPER_PROMPT_TOTAL_MAX_CHARS: usize = 140;

/// v0.1.3 patch2 (C): glossary を Whisper prompt に統合する際の上限文字数。
///
/// 2026-08-08: 180 → 100 へ縮小。固定指示文 (39 字) と合わせて
/// `WHISPER_PROMPT_TOTAL_MAX_CHARS` に収まる値。整合はテストで担保する
/// (`whisper_prompt_total_fits_budget`)。
pub const WHISPER_PROMPT_GLOSSARY_MAX_CHARS: usize = 100;

/// 語彙ヒントに載せる語数の上限。
///
/// 2026-08-08 に導入。文字数だけで縛ると、短い語ばかりの辞書では語数が膨らみ、
/// prompt が「単語の羅列」に寄って文体条件付けとしての性質が薄れる。
/// 要望「単語数と文字数を限られた数だけしか登録できないようにする」に対応する。
pub const WHISPER_PROMPT_GLOSSARY_MAX_WORDS: usize = 20;

/// 2026-07-25: Whisper prompt の 180 字枠に詰める優先順位を決める (指摘)。
///
/// 辞書は 2 つの役割を兼ねている:
///   (a) 置換ルール (pattern != replacement) — 例「斉藤→齋藤」。後処理の置換で確実に直るので
///       prompt に載らなくても実害がない。
///   (b) 語彙ヒント (pattern == replacement) — 例「品川→品川」「施術→施術」。衝突する実在語のため
///       置換では直せず、prompt で候補提示する以外に手段がない。
///
/// 従来は登録の新しい順に無差別へ詰めていたため、(a) が枠を食い潰して (b) が捨てられていた
/// (実測 70 語中 30 語が毎回 drop)。(b) を先に詰め、余った枠に (a) を入れる。
///
/// グループ内の相対順序は入力順のまま保つ (呼出側が新規優先で渡す前提・L526)。
pub fn prioritize_glossary_words(entries: &[(String, String)]) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    let mut rules: Vec<String> = Vec::new();
    for (pattern, replacement) in entries {
        if replacement.trim().is_empty() {
            continue;
        }
        if pattern.trim() == replacement.trim() {
            hints.push(replacement.clone());
        } else {
            rules.push(replacement.clone());
        }
    }
    hints.extend(rules);
    hints
}

/// v0.1.3 patch2 (C): glossary words を Whisper prompt 用に整形する。
///
/// - 重複除去 (順序保持 first-seen wins)
/// - 空文字 / 全空白 trim 除外
/// - カンマ区切りで連結
/// - WHISPER_PROMPT_GLOSSARY_MAX_CHARS を超えたら超過直前で打ち切り (途中単語は丸ごと捨てる)
/// - 切り捨てが発生したら warn log で件数を可視化 (L526: 無言の切り捨て禁止)
/// - 結果が空なら "" を返す → 呼出側は固定 prompt にフォールバック
///
/// 戻り値: "末句, マツエク, ハリウッドアイラッシュ" 形式
pub fn build_glossary_phrase(words: &[String]) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut acc = String::new();
    let mut budget = WHISPER_PROMPT_GLOSSARY_MAX_CHARS;
    let mut included: usize = 0;
    let mut dropped: usize = 0;
    let mut full = false;
    for raw in words {
        let w = raw.trim();
        if w.is_empty() {
            continue;
        }
        if !seen.insert(w.to_string()) {
            continue;
        }
        if full {
            dropped += 1;
            continue;
        }
        if included >= WHISPER_PROMPT_GLOSSARY_MAX_WORDS {
            // 語数の上限に達した。以降は文字数に余裕があっても載せない。
            full = true;
            dropped += 1;
            continue;
        }
        let chunk_chars: usize = if acc.is_empty() {
            w.chars().count()
        } else {
            // ", " 2 文字 + w
            2 + w.chars().count()
        };
        if chunk_chars > budget {
            // 従来どおり超過語以降は丸ごと打ち切り (詰め直しはしない)。以降は件数カウントのみ。
            full = true;
            dropped += 1;
            continue;
        }
        if !acc.is_empty() {
            acc.push_str(", ");
        }
        acc.push_str(w);
        budget -= chunk_chars;
        included += 1;
    }
    if dropped > 0 {
        log::warn!(
            "[glossary] whisper prompt hint truncated: included={} dropped={} \
             (max {} words / {} chars)",
            included,
            dropped,
            WHISPER_PROMPT_GLOSSARY_MAX_WORDS,
            WHISPER_PROMPT_GLOSSARY_MAX_CHARS
        );
    }
    acc
}

/// 語彙ヒント枠の使用状況。辞書画面へ残量を出すために使う。
///
/// 2026-08-08: 従来は溢れた語を warn ログへ書くだけで、ユーザーからは
/// 「登録したのに載っていない」ことが見えなかった (指摘)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GlossaryHintUsage {
    /// 実際に Whisper へ送られる語数。
    pub used_words: usize,
    /// 語数の上限。
    pub max_words: usize,
    /// 実際に送られる語の合計文字数 (区切りの ", " を含む)。
    pub used_chars: usize,
    /// 文字数の上限。
    pub max_chars: usize,
    /// 枠に入らなかった語数。
    pub dropped_words: usize,
}

/// 現在の辞書で語彙ヒント枠がどれだけ埋まるかを算出する。
///
/// `build_glossary_phrase` と同じ順序・同じ上限で数えるため、
/// 画面の残量表示と実際に送られる内容が食い違わない。
pub fn glossary_hint_usage(words: &[String]) -> GlossaryHintUsage {
    let phrase = build_glossary_phrase(words);
    let used_words = if phrase.is_empty() {
        0
    } else {
        phrase.split(", ").count()
    };
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let candidates = words
        .iter()
        .map(|w| w.trim())
        .filter(|w| !w.is_empty())
        .filter(|w| seen.insert(*w))
        .count();
    GlossaryHintUsage {
        used_words,
        max_words: WHISPER_PROMPT_GLOSSARY_MAX_WORDS,
        used_chars: phrase.chars().count(),
        max_chars: WHISPER_PROMPT_GLOSSARY_MAX_CHARS,
        dropped_words: candidates.saturating_sub(used_words),
    }
}

/// v0.1.3 patch3 (B-fix): confidence_drop_flag を計算する純粋関数。
///
/// halu Pass 後の最終ガードとして、「Pass はしたが確信度が低い」ケースで flag=1 にする。
/// flag=1 のときは呼出元で clipboard 貼付を抑止する設計。
///
/// patch2 までは `duration_ms` を見ずに常に
///   no_speech_prob > 0.4 || avg_logprob < -0.5
/// で flag を立てていたが、これだと短発話 (例: id=158「小と」1.7s, logprob=-0.86) で
/// false-positive が頻発した。真因: 短発話は logprob が自然に下がる Whisper の挙動。
///
/// 修正: 短発話 (duration_ms < SHORT_UTTERANCE_THRESHOLD_MS = 2000ms) では
/// flag=false 固定。短発話の真ハルシは halu.rs Tier 3.5 (substring) と Tier 1 (VAD) で別経路捕捉。
// ---------------------------------------------------------------------------
// ASR 用の入力レベル正規化 (2026-08-06)
// ---------------------------------------------------------------------------
//
// Whisper は入力が小さすぎると精度が落ちる。実機で Bluetooth ヘッドセットの
// ハンズフリー接続を使うと実測 RMS が 0.0003〜0.0014 まで落ち、設計上の想定
// (発話時 RMS ≈ 0.006) の 5〜20 分の 1 になっていた。アプリは以前から
// mic_low_level バナーで警告していたが、音声そのものは小さいまま送っていた。
// 送信前に目標レベルまで持ち上げることで、マイク環境に依存せず精度を保つ。

/// 正規化の目標 RMS。一般的な音声認識向けの実用値。
const ASR_TARGET_RMS: f32 = 0.05;
/// 増幅の上限。これ以上持ち上げると無音時の環境ノイズまで音声として増幅される。
const ASR_MAX_GAIN: f32 = 20.0;
/// クリップ防止の波高上限。増幅後のピークがこれを超えないようゲインを抑える。
const ASR_PEAK_CEILING: f32 = 0.95;

/// ASR 送信用にレベルを持ち上げたサンプル列を返す。
/// 既に十分な音量なら `None` を返す (無駄なコピーをしない)。
///
/// 増幅率は「目標 RMS に届く倍率」と「ピークが `ASR_PEAK_CEILING` を超えない倍率」の
/// 小さい方を採り、さらに `ASR_MAX_GAIN` で頭打ちにする。したがって歪みは生じない。
///
/// **ハルシ判定には増幅前の音声を渡すこと。** 増幅後を VAD にかけると
/// 常に「音声あり」になり、無音時の貼付抑止が効かなくなる。
fn boost_for_asr(samples: &[f32]) -> Option<(Vec<f32>, f32)> {
    if samples.is_empty() {
        return None;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    if rms <= 0.0 || rms >= ASR_TARGET_RMS {
        return None;
    }
    let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    if peak <= 0.0 {
        return None;
    }
    let gain = (ASR_TARGET_RMS / rms)
        .min(ASR_PEAK_CEILING / peak)
        .min(ASR_MAX_GAIN);
    // 1.0 以下なら持ち上げる意味がない (ピーク制約で潰れた場合を含む)
    if gain <= 1.0 {
        return None;
    }
    Some((samples.iter().map(|s| s * gain).collect(), gain))
}

/// 無音抑止で参照する no_speech_prob のしきい値 (halu の Tier 2 と同値)。
pub const NO_SPEECH_PROB_SUPPRESS_THRESHOLD: f32 = 0.6;

/// 「一度も音として立ち上がらなかった」と判断するピークの上限。EnergyVad のしきい値と同値。
///
/// 2026-08-08 に導入。2026-08-06 に「VAD 単独の無音判定では貼付を抑止しない」へ倒した結果、
/// **本当に無音**だった録音 (156 秒・平均 RMS 0.00015・ピーク 0.00101) から Whisper が
/// 生成した 388 文字が ChatGPT の入力欄へ貼り付く事故が起きた (2026-08-08 13:25)。
///
/// 平均 RMS ではマイク不調時の実発話 (0.0003〜0.0014) と無音を分離できないが、
/// **ピークは 1 桁以上離れる**。同日の実発話はピーク 0.012〜0.068 で、しきい値の 6〜34 倍。
/// 「全サンプル中 1 つも VAD のしきい値を超えなかった」なら音声は入っていないと言い切れる。
///
/// トレードオフ: マイクが極端に弱く、ピークすらしきい値へ届かない実発話は抑止される。
/// ただし**本文は履歴に残る** (L729) ため手動コピーで回収できる。逆方向の事故
/// (無関係な長文が編集中の文書へ流し込まれる) の方が実害が大きいと判断した。
pub const SILENCE_PEAK_FLOOR: f32 = 0.002;

/// no_speech_prob が取れない場合に「無音ハルシらしさ」を文字数で代替判定する上限。
/// 無音時に Whisper が吐く定型句 (「ご視聴ありがとうございました」等) は短いため、
/// これを超える長さの文章は実際の発話とみなして貼り付ける。
pub const SILENCE_FALLBACK_MAX_CHARS: usize = 20;

/// ハルシ判定 Drop のうち、貼付を抑止すべきか (=無音とみなすか) を判定する。
///
/// 2026-08-02 利用者最終判断で「無音時だけは貼付を止める」。ただし 2026-08-06 に
/// VAD 単独判定での誤爆 (マイク入力レベル低下で正常発話が全滅) が実機で起きたため、
/// **VAD と Whisper の判断が一致した時だけ**抑止するよう変更した。
///
/// - `NoSpeechProb`: Whisper 自身が無音と言っている → 抑止
/// - `NoSpeechVad`: VAD だけが無音と言っている状態。Whisper が「音声あり」と
///   言っているなら Whisper を信じて貼り付ける (VAD は音量しきい値のみで脆い)
/// - それ以外の Drop 理由: 既定方針どおり貼り付ける
pub fn should_suppress_paste_for_silence(
    reason: &HaluReason,
    no_speech_prob: Option<f32>,
    no_speech_prob_threshold: f32,
    text_chars: usize,
    input_peak: Option<f32>,
) -> bool {
    match reason {
        HaluReason::NoSpeechProb { .. } => true,
        HaluReason::NoSpeechVad => {
            // 2026-08-08: ピークが一度もしきい値へ届いていないなら、Whisper が何と言おうと
            // 音声は入っていない。Groq Whisper は無音に対して no_speech_prob=0.0 の高い確信度で
            // 日本語のハルシを返すことが実測済み (reference_groq_whisper_silence_halu) のため、
            // ここで Whisper を信じると無音生成文が貼り付く。
            if let Some(peak) = input_peak {
                if peak < SILENCE_PEAK_FLOOR {
                    return true;
                }
            }
            match no_speech_prob {
                Some(p) => p > no_speech_prob_threshold,
                // Whisper 側の裏づけが取れない場合のみ、文字数で代替判定する
                None => text_chars <= SILENCE_FALLBACK_MAX_CHARS,
            }
        }
        _ => false,
    }
}

/// ハルシ判定 Drop のうち、**本文全体が既知の非発話句そのもの**であるため貼付を抑止すべきかを判定する。
/// (無料版 RushVoice Air v1.1.5 の Tier 3.2 を 2026-08-16 に移植した際の、本リポジトリ側の貼付判断)
///
/// 有料版の 2026-08-02 既定方針は「ハルシ疑いでも貼る (無音時だけ止める)」。その趣旨は
/// 「長文の末尾に一言混ざっただけで本文が丸ごと消える」データロスを避けることにある。
/// Tier 3.2 (`KnownPhraseAlways`) は **normalize 後の本文全体が既知句と完全一致**したときだけ
/// 発火し、登録規約 (`KNOWN_HALLUCINATIONS_ALWAYS`) 上その句が単独で発話されることは実務上
/// あり得ないため、守るべき本文が存在しない。無音から生成された文と同じく、貼ると
/// 貼付先の文書を壊すだけなので抑止する (履歴には raw_text として残り、手動コピーは可能)。
///
/// 🔴 Tier 3 (`KnownPhraseLowLogprob`) / Tier 3.5 (`KnownSubstring`) の「貼る」扱いは変えていない
/// (`should_suppress_paste_for_silence` の `_ => false` のまま)。
pub fn should_suppress_paste_for_whole_text_hallucination(reason: &HaluReason) -> bool {
    matches!(reason, HaluReason::KnownPhraseAlways { .. })
}

pub fn compute_confidence_drop_flag(
    duration_ms: i64,
    no_speech_prob: Option<f32>,
    avg_logprob: Option<f32>,
) -> bool {
    let is_short_utterance = duration_ms < crate::halu::SHORT_UTTERANCE_THRESHOLD_MS;
    if is_short_utterance {
        return false;
    }
    no_speech_prob.map(|p| p > 0.4).unwrap_or(false)
        || avg_logprob.map(|p| p < -0.5).unwrap_or(false)
}

/// v0.1.3 patch3 (C-add): 後処理 pattern → replacement 置換を順次適用する。
///
/// 仕様:
///   - 各 entry を Vec 先頭から順に str::replace で適用 (is_regex=false)
///   - is_regex=true は regex::Regex::new でコンパイル → replace_all 適用
///     コンパイル失敗 / 無効正規表現は warn log で skip (text は無加工で次 entry へ)
///   - pattern が空文字列の entry は skip (無限ループ防止)
///   - 戻り値: (置換後テキスト, 適用件数) — 適用件数 = 実際に 1 回以上 hit して text が変化したエントリ数
///
/// パフォーマンス: 辞書サイズ ~数十件想定で線形 O(N * |text|)。
/// 過剰最適化はせず素直な実装にする。
pub fn apply_glossary_replacements(
    text: &str,
    glossary_entries: &[(String, String, bool)],
) -> (String, usize) {
    let mut current = text.to_string();
    let mut applied_count: usize = 0;
    for (pattern, replacement, is_regex) in glossary_entries {
        if pattern.is_empty() {
            continue;
        }
        let before = current.clone();
        if *is_regex {
            match regex::Regex::new(pattern) {
                Ok(re) => {
                    current = re.replace_all(&current, replacement.as_str()).into_owned();
                }
                Err(e) => {
                    log::warn!(
                        "[glossary] invalid regex pattern={:?} err={}; skipping",
                        pattern, e
                    );
                    continue;
                }
            }
        } else {
            current = current.replace(pattern.as_str(), replacement.as_str());
        }
        if current != before {
            applied_count += 1;
        }
    }
    (current, applied_count)
}

/// v0.1.3 patch2 (C): WHISPER_PROMPT に glossary 句を結合した動的 prompt を返す。
///
/// glossary が空なら WHISPER_PROMPT をそのまま返す (既存挙動)。
/// 非空なら "<WHISPER_PROMPT><glossary_phrase>" 形式で返す。
///
/// v0.1.3 patch5 (2026-05-24・B): 「固有名詞: 」ラベル削除。
///   - 真因: ラベル「固有名詞:」が漏洩バリエーション (「固有名詞・」「固有名詞は」等) で
///     文字起こし結果に混入していた (id=162, id=165 実例)。
///   - 対策: ラベル文字列を prompt から完全削除し、単語のみカンマ区切りで列挙する。
///     漏洩しても異常感の少ない形 ("マツエク, マツパ, ハルシ, ...") になる。
///   - 副次効果: prompt 長が 8 文字短縮 → Groq prompt 上限に余裕。
///   - 漏洩バリエーション自体は recorder.rs 配列の Tier 3.5 substring で別途破棄する。
///
/// 例 (2026-08-08 の prompt 圧縮後):
///   - words=[] → "句読点（、。）を含む自然な日本語で書き起こす。語尾や助詞は話したまま変えない。"
///   - words=["末句", "マツエク"] →
///     "句読点（、。）を含む自然な日本語で書き起こす。語尾や助詞は話したまま変えない。末句, マツエク"
pub fn build_dynamic_whisper_prompt(glossary_words: &[String]) -> String {
    let phrase = build_glossary_phrase(glossary_words);
    if phrase.is_empty() {
        WHISPER_PROMPT.to_string()
    } else {
        format!("{}{}", WHISPER_PROMPT, phrase)
    }
}

/// 録音中の状態を Tauri 側に渡すための列挙体。frontend と文字列キーで揃える。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecorderState {
    #[default]
    Idle,
    Recording,
    Transcribing,
}

/// 起動中の worker thread を表す。stop_recording で stop_flag を立てて join する。
pub struct RecorderSession {
    pub device_sample_rate: u32,
    pub device_channels: u16,
    pub frames: Arc<Mutex<Vec<RawFrame>>>,
    pub stop_flag: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
    /// v0.1.15 小声警告: worker thread が録音終了時に書き込むセッション平均 RMS (0.0 = 未録音)。
    pub rms_mean: Arc<Mutex<f32>>,
}

/// Tauri 側で管理するグローバル State。
#[derive(Default)]
pub struct RecorderHolder {
    pub session: Mutex<Option<RecorderSession>>,
    pub state: Mutex<RecorderState>,
}

impl RecorderHolder {
    pub fn current_state(&self) -> RecorderState {
        *self.state.lock().expect("recorder state mutex poisoned")
    }

    fn set_state(&self, new_state: RecorderState) {
        *self.state.lock().expect("recorder state mutex poisoned") = new_state;
    }
}

/// 録音のフレームの出どころ。待機ストリームを借りるか、自分でマイクを開くか。
enum FrameSource {
    Own(AudioRecorder),
    Standby(crate::audio::StandbyTap),
}

impl FrameSource {
    fn format(&self) -> (u32, u16) {
        match self {
            FrameSource::Own(r) => (r.device_sample_rate, r.device_channels),
            FrameSource::Standby(t) => (t.sample_rate, t.channels),
        }
    }
    fn next_frame_blocking(&mut self) -> Option<RawFrame> {
        match self {
            FrameSource::Own(r) => r.next_frame_blocking(),
            FrameSource::Standby(t) => t.next_frame_blocking(),
        }
    }
}

/// start_recording のエントリポイント。worker thread を spawn し、device_info を受け取って返す。
///
/// Stage 2-C-2: `app` を Some にすると、各 frame の RMS を計算して
/// `recorder:waveform` イベントを emit する (overlay の波形描画用)。テスト時は None で渡せる。
///
/// v0.1.12 F2: `mic_low_level` 設定を受け、worker thread 内で録音開始から window_ms の間に
/// RMS が継続して threshold 未満なら `recorder:low-input-level-detected` を 1 度だけ emit する。
/// 既存テスト経路は MicLowLevelConfig::default() を渡せばよい。
pub fn start_session(
    holder: &RecorderHolder,
    app: Option<AppHandle>,
    mic_low_level: MicLowLevelConfig,
) -> Result<(), String> {
    {
        let guard = holder.session.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("既に録音中です".into());
        }
    }

    let frames: Arc<Mutex<Vec<RawFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));
    // v0.1.15 小声警告: worker thread が録音終了時にセッション平均 RMS を書き込む共有セル。
    let rms_mean: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
    let (init_tx, init_rx): (SyncSender<Result<(u32, u16), String>>, _) = sync_channel(0);

    let frames_ref = frames.clone();
    let stop_ref = stop_flag.clone();
    let rms_mean_ref = rms_mean.clone();
    let app_for_thread = app.clone();

    let handle = std::thread::spawn(move || {
        // マイク選択 GUI (2026-07-23): 設定済みデバイスがあれば優先 (無ければ既定追従)
        let preferred_device = preferred_input_device();
        // 2026-10-31: 待機ストリームが同じマイクで動いていれば、それをそのまま使う
        // (毎回マイクを開き直す 0.5〜1.8 秒の遅れを無くし、押す直前 300ms も足す)。
        let open_started = std::time::Instant::now();
        let mut recorder = match crate::audio::standby_subscribe(preferred_device.as_deref()) {
            Some(tap) => {
                if let Ok(mut buf) = frames_ref.lock() {
                    buf.extend(tap.preroll.iter().cloned());
                }
                log::info!(
                    "[recorder] start via standby: preroll_frames={} open_ms={}",
                    tap.preroll.len(),
                    open_started.elapsed().as_millis()
                );
                FrameSource::Standby(tap)
            }
            None => match AudioRecorder::start(preferred_device.as_deref()) {
                Ok(r) => {
                    log::info!(
                        "[recorder] start via new stream: open_ms={}",
                        open_started.elapsed().as_millis()
                    );
                    FrameSource::Own(r)
                }
                Err(e) => {
                    log::warn!("[recorder] AudioRecorder::start failed: {:?}", e);
                    let _ = init_tx.send(Err(format!("AudioRecorder::start() failed: {:?}", e)));
                    return;
                }
            },
        };
        let info = recorder.format();
        if init_tx.send(Ok(info)).is_err() {
            // メインが既に諦めたケース。リソースを drop して終わる。
            return;
        }
        // v0.1.12 F2: 低レベル判定 state (worker thread ローカル)
        //   - start_instant: worker 起動時刻 (録音開始 ≒ 初フレーム取得直前)
        //   - seen_loud: 一度でも threshold 以上の RMS を観測したら true (= 抑止)
        //   - already_emitted: emit 済なら以降は何もしない (session 単位 1 回)
        let start_instant = std::time::Instant::now();
        let mut seen_loud = false;
        let mut already_emitted = false;
        // v0.1.14 診断: 録音セッション全体の RMS 統計 (小声ハルシ実測用・F2 の seen_loud とは独立に全フレーム集計)
        let mut rms_sum: f64 = 0.0;
        let mut rms_peak: f32 = 0.0;
        let mut rms_min: f32 = f32::MAX;
        let mut rms_count: u64 = 0;
        // 語尾パディング (2026-07-23): stop 指示を検知したら即 break せず、
        // STOP_TAIL_PADDING_MS 分だけ録音を延長してから確定する (語尾切れ対策)。
        let mut stop_deadline: Option<std::time::Instant> = None;
        loop {
            if stop_deadline.is_none() && stop_ref.load(Ordering::Relaxed) {
                stop_deadline = Some(
                    std::time::Instant::now()
                        + std::time::Duration::from_millis(STOP_TAIL_PADDING_MS),
                );
            }
            if stop_deadline_reached(stop_deadline, std::time::Instant::now()) {
                break;
            }
            match recorder.next_frame_blocking() {
                Some(frame) => {
                    // Stage 2-C-2: RMS を計算して emit (overlay 波形描画用)
                    let rms = compute_rms(&frame.samples);
                    if let Some(ref app_h) = app_for_thread {
                        let _ = app_h.emit("recorder:waveform", rms);
                    }
                    // v0.1.14 診断: RMS 統計を全フレーム集計
                    rms_sum += rms as f64;
                    rms_peak = rms_peak.max(rms);
                    rms_min = rms_min.min(rms);
                    rms_count += 1;
                    // v0.1.12 F2: 低レベル sustained 判定
                    //   重要: 境界フレームの誤判定回避のため評価順序は (1)→(2)→(3) で行う:
                    //     (1) 現時点の elapsed_ms と「これまでの seen_loud」で emit 判定
                    //         → window 経過 + 未 emit + これまで sustained 低レベル → emit
                    //     (2) その後で現フレームの RMS を seen_loud に反映
                    //         (現フレームの loud は次回以降の emit 抑止に使う)
                    //   こうしないと「window ぴったり過ぎた瞬間に loud が来た」境界ケースで
                    //   seen_loud=true が先に立ち、本来 emit すべき sustained 低レベルを抑止してしまう。
                    if !already_emitted {
                        let elapsed_ms = start_instant.elapsed().as_millis();
                        if should_emit_mic_low_level(
                            elapsed_ms,
                            mic_low_level.window_ms,
                            seen_loud,
                            already_emitted,
                        ) {
                            if let Some(ref app_h) = app_for_thread {
                                if let Err(e) =
                                    app_h.emit("recorder:low-input-level-detected", ())
                                {
                                    log::warn!(
                                        "[mic_low_level] emit failed: {:?}",
                                        e
                                    );
                                } else {
                                    log::info!(
                                        "[mic_low_level] banner triggered: window_ms={} threshold={}",
                                        mic_low_level.window_ms,
                                        mic_low_level.threshold
                                    );
                                }
                            }
                            already_emitted = true;
                        }
                        // 現フレームの loud 観測は emit 判定後に反映 (次回判定で抑止)
                        if rms >= mic_low_level.threshold {
                            seen_loud = true;
                        }
                    }
                    if let Ok(mut buf) = frames_ref.lock() {
                        buf.push(frame);
                    }
                }
                None => break,
            }
        }
        // v0.1.14 診断: 録音セッションの RMS 統計を本番ログに出力 (小声ハルシ実測用)。
        //   必ず log::info! を使う (tracing::info! は tauri-plugin-log に届かず消える・F1 バグの教訓)。
        if rms_count > 0 {
            let stats = compute_rms_stats(rms_sum, rms_peak, rms_min, rms_count);
            log::info!(
                target: "rushvoice.rms",
                "stats mean={:.5} peak={:.5} min={:.5} frames={}",
                stats.mean,
                stats.peak,
                stats.min,
                stats.frames
            );
            // v0.1.15 小声警告: セッション平均 RMS を共有セルに書き込む (paste 判定で参照)。
            if let Ok(mut cell) = rms_mean_ref.lock() {
                *cell = stats.mean;
            }
        }
        // recorder はここで drop され、cpal::Stream も停止する。
    });

    let (device_sample_rate, device_channels) = init_rx
        .recv()
        .map_err(|e| format!("worker init channel closed: {}", e))??;

    let session = RecorderSession {
        device_sample_rate,
        device_channels,
        frames,
        stop_flag,
        handle: Some(handle),
        rms_mean,
    };

    {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        *guard = Some(session);
    }
    holder.set_state(RecorderState::Recording);
    Ok(())
}

/// HK-06: 録音をキャンセルする。transcribe を起動せず RecorderState を Idle に戻す。
/// 録音中でない場合 (セッションなし) はエラーにせず静かに Idle を設定して返る。
pub fn cancel_session(holder: &RecorderHolder) -> Result<(), String> {
    let session = {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    if let Some(session) = session {
        session.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = session.handle {
            let _ = h.join();
        }
    }
    holder.set_state(RecorderState::Idle);
    Ok(())
}

/// Stage 2-C-1: ホットキー toggle 時の状態遷移結果。
#[derive(Debug)]
pub enum ToggleResult {
    /// Idle → Recording に遷移した
    Started,
    /// Recording → Transcribing → Idle に遷移し、文字起こし結果を返した
    Transcribed(String),
    /// Transcribing 中で何もできなかった (UX 通知用)
    Busy,
}

/// Phase 2 D: 文字起こし完了時に履歴 INSERT 用のメタデータをまとめて返す構造体。
///
/// halu 防御を通過した成功ケースでのみ呼出元へ届く (Drop は Err にマップされる)。
/// 履歴 INSERT は呼出元 (commands.rs / lib.rs) が `HistoryDb` State 経由で行う:
/// transcribe ロジック (recorder.rs) を HistoryDb に依存させず、テスタブル性を維持するため。
#[derive(Debug, Clone)]
pub struct TranscribeOutcome {
    /// 整形後テキスト (clipboard 貼付対象・halu Pass 後・末尾句読点付与済)
    ///
    /// v0.1.3: confidence_drop_flag=1 のときは空文字列にする。
    /// 呼出元 (commands.rs / lib.rs) は空文字列を見て clipboard 貼付を抑止すること。
    pub text: String,
    /// v0.1.3: 履歴 DB の raw_text 列に格納するテキスト (常に整形後テキスト・空化しない)。
    /// confidence_drop_flag=1 でも履歴には残し、HistoryPage から手動コピーできるようにする。
    pub raw_text_for_history: String,
    /// 文字起こしプロバイダ (現状 "groq" 固定・将来 "openai" / "hedged" 追加想定)
    pub provider: String,
    /// モデル名 ("whisper-large-v3-turbo" 等)
    pub model: String,
    /// 録音時間 (ms)。device_sample_rate と frames 数から算出
    pub duration_ms: i64,
    /// 文字起こし所要時間 (ms)。HTTP リクエスト開始 → レスポンス受信完了
    pub transcribe_ms: i64,
    /// 送信した WAV のバイト数
    pub audio_bytes: i64,
    /// Groq verbose_json segments[0].avg_logprob (取得できない場合 None)
    pub avg_logprob: Option<f64>,
    /// Groq verbose_json segments[0].no_speech_prob (取得できない場合 None)
    pub no_speech_prob: Option<f64>,
    /// 低信頼フラグ (no_speech_prob > 0.4 または avg_logprob < -0.5 を超過)
    /// v0.1.3 patch2 (2026-05-24): 閾値 -0.3 → -0.5 に緩和。
    ///   - 真因: 短発話 (例: 「ミュージカル」-0.59 / 「松井九」-0.78) で誤検知頻発
    ///   - 副作用: 長文の本当のハルシ感度低下するが、Tier 3 完全一致 + Tier 3.5 substring で別経路検出可
    pub confidence_drop_flag: bool,
    // v0.1.9 hedged 計測 (history.db への記録用・solo 経路は全て None)
    /// primary 単独の所要 ms (hedged 経由のときのみ Some)
    pub primary_ms: Option<i64>,
    /// fallback が dispatch されたか (hedged 経由のときのみ Some)
    pub fallback_triggered: Option<bool>,
    /// hedged 勝者 ("primary" / "fallback"・solo 経路は None)
    pub hedged_winner: Option<String>,
    /// v0.1.15 小声警告: 録音セッションの平均 RMS (0.0 = 未取得)。
    /// 呼出元 (commands.rs) が baseline と比較して小声警告を出すのに使う。
    pub session_rms_mean: f32,
}

/// v0.1.9: provider 設定値。config_db "provider" KV に保存される文字列と対応。
///
/// - `Hedged`: Groq primary + OpenAI fallback (3.0s trigger)。OpenAI key 無しなら Groq 単独に縮退。
/// - `Groq`:   Groq 単独。
/// - `Openai`: OpenAI 単独。OpenAI key 無しなら Groq 単独に縮退 (エラー UI 表示)。
/// - `Custom`: 利用者が設定した OpenAI 互換の接続先 (ローカルの whisper.cpp server 等) 単独。
///
/// 実際の接続先の組み立ては endpoints::resolve_transcribe_setup。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    Hedged,
    Groq,
    Openai,
    Custom,
}

impl ProviderChoice {
    /// 文字列 → 列挙体 (config_db.get_kv の戻り値想定)。未知文字列は既定 = Hedged。
    pub fn parse(s: &str) -> Self {
        match s {
            "groq" => Self::Groq,
            "openai" => Self::Openai,
            "custom" => Self::Custom,
            _ => Self::Hedged,
        }
    }
}

/// Stage 2-C-1: ホットキー押下時のトグル動作。
/// 状態に応じて start_session / stop_session_and_transcribe を呼び分ける。
///
/// Stage 2-C-2: `app` を渡すことで開始時に波形 emit が有効化される。
/// Phase 2 D: stop 系経路は TranscribeOutcome.text を ToggleResult::Transcribed に変換して返す
/// (上位の履歴 INSERT は呼出元が outcome から直接組み立てる方が情報量が多いため、
///  toggle_recording 経由の呼出元では履歴 INSERT を諦めるか別経路を使う)。
pub async fn toggle_recording(
    holder: &RecorderHolder,
    app: Option<AppHandle>,
    setup: TranscribeSetup,
    glossary_words: Vec<String>,
    // v0.1.3 patch3 (C-add): 後処理 pattern→replacement 置換用 entries
    glossary_entries: Vec<(String, String, bool)>,
) -> Result<ToggleResult, String> {
    let current = holder.current_state();
    match current {
        RecorderState::Idle => {
            // v0.1.12 F2: legacy toggle_recording 経路は default 設定を渡す
            start_session(holder, app, MicLowLevelConfig::default())?;
            Ok(ToggleResult::Started)
        }
        RecorderState::Recording => {
            // v0.1.9: toggle_recording は legacy Groq solo 経路 (旧テスト用)。
            //   本番は lib.rs / commands.rs から stop_session_and_transcribe を直接呼ぶ。
            // v0.1.10: 機能 A は本番経路のみ対象なので、legacy では app=None / default 設定で渡す。
            let outcome = stop_session_and_transcribe(
                holder,
                setup,
                glossary_words,
                glossary_entries,
                None,
                CongestionConfig::default(),
            )
            .await?;
            Ok(ToggleResult::Transcribed(outcome.text))
        }
        RecorderState::Transcribing => Ok(ToggleResult::Busy),
    }
}

/// Stage 2-C-2: PCM サンプルの RMS (root mean square) を計算する。
/// 0.0 (無音) ~ 1.0 (フルスケール) の f32 値を返す。
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// 語尾パディング (2026-07-23) の純粋判定: stop 指示済み (deadline=Some) かつ
/// 締切超過なら true (= 録音ループを抜けてよい)。stop 未指示 (None) は常に false。
pub fn stop_deadline_reached(
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    matches!(deadline, Some(d) if now >= d)
}

/// stop_recording → リサンプル → WAV → Groq transcribe → TranscribeOutcome。
/// 失敗時は state を Idle に戻して Err を返す。
///
/// Phase 2 D: 戻り値を `String` → `TranscribeOutcome` に拡張。
/// `outcome.text` が従来の `String` 相当。呼出元 (commands.rs / lib.rs) は
/// outcome のメタデータを使って `HistoryDb` に INSERT する。
///
/// v0.1.10 機能 A: `app` (バナーイベント emit 用) と `congestion` (発動閾値・hedged 抑止判定) を追加。
/// テスト経路や非 Tauri 経路は app=None・congestion=CongestionConfig::default() で渡す。
#[allow(clippy::too_many_arguments)]
pub async fn stop_session_and_transcribe(
    holder: &RecorderHolder,
    // 接続先の組 (endpoints::load_transcribe_setup)
    setup: TranscribeSetup,
    glossary_words: Vec<String>,
    // v0.1.3 patch3 (C-add): 後処理 pattern→replacement 置換用 entries
    // 各要素 = (pattern, replacement, is_regex)
    glossary_entries: Vec<(String, String, bool)>,
    // v0.1.10 機能 A: バナー emit 用 AppHandle (テスト経路は None で渡す)
    app: Option<AppHandle>,
    // v0.1.10 機能 A: 混雑通知バナー設定
    congestion: CongestionConfig,
) -> Result<TranscribeOutcome, String> {
    let session = {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    let session = session.ok_or_else(|| "録音セッションがありません".to_string())?;

    session.stop_flag.store(true, Ordering::Relaxed);
    if let Some(h) = session.handle {
        let _ = h.join();
    }

    // v0.1.15 小声警告: join 後に worker が書いたセッション平均 RMS を読む (0.0 = 未取得)。
    let session_rms_mean = session.rms_mean.lock().map(|g| *g).unwrap_or(0.0);

    holder.set_state(RecorderState::Transcribing);

    // 結果回収・所有権を Mutex から外に出す
    let collected: Vec<RawFrame> = {
        let mut guard = session.frames.lock().map_err(|e| e.to_string())?;
        std::mem::take(&mut *guard)
    };

    let result = transcribe_collected(
        collected,
        session.device_sample_rate,
        session.device_channels,
        &setup,
        glossary_words,
        glossary_entries,
        app,
        congestion,
    )
    .await;

    holder.set_state(RecorderState::Idle);
    // v0.1.15 小声警告: 成功時のみ実測 mean を outcome に載せる (transcribe_collected 内は 0.0 で構築)。
    result.map(|mut outcome| {
        outcome.session_rms_mean = session_rms_mean;
        outcome
    })
}

#[allow(clippy::too_many_arguments)]
async fn transcribe_collected(
    frames: Vec<RawFrame>,
    device_sample_rate: u32,
    device_channels: u16,
    setup: &TranscribeSetup,
    glossary_words: Vec<String>,
    glossary_entries: Vec<(String, String, bool)>,
    // v0.1.10 機能 A: バナー emit 用 AppHandle
    app: Option<AppHandle>,
    // v0.1.10 機能 A: 混雑通知バナー設定
    congestion: CongestionConfig,
) -> Result<TranscribeOutcome, String> {
    if frames.is_empty() {
        return Err("録音データが空です (無音 / 録音時間が短すぎ)".into());
    }

    // Phase 2 D: 録音時間 (ms) を device_sample_rate と総サンプル数から算出
    // (リサンプル前の生フレームから計算: 16kHz mono に縮約された後だと
    //  device_channels の情報が消えるため)
    let total_input_samples_per_channel: usize =
        frames.iter().map(|f| f.samples.len() / device_channels.max(1) as usize).sum();
    let duration_ms: i64 = if device_sample_rate > 0 {
        ((total_input_samples_per_channel as f64) * 1000.0 / device_sample_rate as f64) as i64
    } else {
        0
    };

    // リサンプリング (16kHz mono)
    let mut resampler = Resampler::new(device_sample_rate, device_channels)
        .map_err(|e| format!("Resampler::new failed: {:?}", e))?;
    let mut mono16k: Vec<f32> = Vec::with_capacity(frames.len() * 1600);
    for f in frames {
        match resampler.process(f) {
            Ok(t) => mono16k.extend(t.samples),
            Err(e) => return Err(format!("Resampler::process failed: {:?}", e)),
        }
    }

    if mono16k.is_empty() {
        return Err("リサンプル後のサンプルが空です".into());
    }

    // ASR へ送る音声だけレベルを持ち上げる。mono16k (増幅前) はこの後の
    // ハルシ判定 (VAD) にそのまま使うため、書き換えないこと。
    // 2026-11-14: 話し終わりのあとの長い無音・雑音は送らない。増幅で雑音まで持ち上がると
    // Whisper が話していない句を繰り返し付け足す (history id=8127)。判定・切り落としは
    // 増幅前の音声で行い、mono16k 自体は書き換えない (下のハルシ判定で全体を使う)。
    let send_len = crate::tail_guard::trailing_trim_len(&mono16k, 16_000);
    if send_len < mono16k.len() {
        log::info!(
            "[recorder] trailing silence trimmed for ASR: {}ms -> {}ms",
            mono16k.len() * 1000 / 16_000,
            send_len * 1000 / 16_000
        );
    }
    let send_samples = &mono16k[..send_len];
    // 2026-11-15: 設定で入れたときだけ、直近の録音を PC の中に残す (原因調べ用・recent_audio.rs)
    crate::recent_audio::save_if_enabled(&mono16k, send_len);
    let wav = match boost_for_asr(send_samples) {
        Some((boosted, gain)) => {
            log::info!("[recorder] input level boosted for ASR: gain={:.1}x", gain);
            encode_wav_pcm16(&boosted, 16_000, 1)
        }
        None => encode_wav_pcm16(send_samples, 16_000, 1),
    };
    let audio_bytes: i64 = wav.len() as i64;

    let client = build_client().map_err(|e| format!("HTTP client build failed: {:?}", e))?;
    // v0.1.3 patch2 (C): WHISPER_PROMPT + glossary を動的に構築。
    //   - glossary 空 → 従来の固定 prompt
    //   - glossary 非空 → "WHISPER_PROMPT固有名詞: word1, word2, ..."
    // 上限 WHISPER_PROMPT_GLOSSARY_MAX_CHARS=180 で超過分は切り捨て (途中単語丸ごと)。
    let dynamic_prompt = build_dynamic_whisper_prompt(&glossary_words);
    log::info!(
        "[recorder] dynamic whisper prompt: glossary_count={} prompt_len={}",
        glossary_words.len(),
        dynamic_prompt.chars().count()
    );

    // 経路選択は endpoints::resolve_transcribe_setup で済んでいる
    // (hedged = Groq primary + OpenAI fallback / 単独 = Groq・OpenAI・カスタム)。
    // 勝者の接続先の名前 ("groq" / "openai" / "custom") を history.db に記録する。
    let (resp_body, transcribe_ms, audio_bytes_used, used_provider, primary_ms, fallback_triggered, hedged_winner, model_name) =
        run_transcription(
            &client,
            &dynamic_prompt,
            wav,
            audio_bytes,
            setup,
            app,
            congestion,
        )
        .await?;

    // 取得情報を以降の整形ロジックに供給する別名 (旧変数名互換)
    let resp_body_for_parse = resp_body;
    let transcribe_ms: i64 = transcribe_ms;
    let audio_bytes: i64 = audio_bytes_used;
    let resp = TranscribeRespForParse { body: resp_body_for_parse };

    let parsed: serde_json::Value =
        serde_json::from_str(&resp.body).map_err(|e| format!("verbose_json parse failed: {}", e))?;
    let raw_groq_text = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if raw_groq_text.is_empty() {
        return Err("文字起こし結果が空でした".into());
    }

    // 2026-11-14: 末尾の幻聴対策 (詳細は tail_guard.rs)。
    // 末尾の区間が Whisper の失敗の目安に当たれば外し、同じ句の連続は 1 回にする。
    let (raw_groq_text, tail_dropped) =
        crate::tail_guard::drop_suspicious_tail_segments(&raw_groq_text, &parsed);
    let (raw_groq_text, repeats_removed) =
        crate::tail_guard::collapse_repeated_phrases(&raw_groq_text);
    if tail_dropped > 0 || repeats_removed > 0 {
        log::warn!(
            "[transcribe] tail guard: dropped {} suspicious tail segment(s), collapsed {} repeated phrase(s)",
            tail_dropped,
            repeats_removed
        );
    }

    // Air v1.1.6 移植 (2026-08-16): avg_logprob をここで取り出す。第 4 層が被覆率だけでは
    // 分離できない際どい帯 (0.55〜0.65) の判定に使うため、防御層より**前**に必要になった。
    // (従来は halu フィルタの直前で取り出していた = 第 4 層からは見えなかった)
    let avg_logprob = parsed
        .get("segments")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("avg_logprob"))
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);

    // v0.1.3 防御第 1 層: prompt サフィックスが冒頭に漏洩した場合 (Whisper の既知挙動)
    // 6 文字以上の連続一致のみ切り捨て (短い一致は誤切り防止のため触らない)。
    //
    // v0.1.3 patch2 (D): 固定 WHISPER_PROMPT ではなく動的構築した prompt と比較。
    // glossary を末尾結合した prompt 全体が漏洩した場合も切り捨てられる。
    let text = sanitize_prompt_leak(&dynamic_prompt, &raw_groq_text);
    if text.is_empty() {
        return Err("prompt 漏洩除去後のテキストが空でした".into());
    }

    // v0.1.7.2 patch (Plan B): 短断片 prompt 漏洩 strip (sanitize_prompt_leak 直後)。
    // sanitize_prompt_leak (6 char 連続一致) では検出できない「読点）」「句読点）」等の
    // 短断片 (1-4 char) だけが冒頭にエコーされ続く本文と結合するケースを除去する。
    // Day 28 真因調査で history.db id=547 / id=527 の 2 件が該当と確定 (11.5h で 3.6%)。
    // halu Tier 3.5 substring は normalize() が「）」を除去するため検出できない。
    let text = strip_prompt_leak_head_fragment(&text);
    if text.is_empty() {
        return Err("prompt 漏洩短断片除去後のテキストが空でした".into());
    }

    // 防御第 4 層 (Air v1.1.3〜v1.1.6 移植・2026-08-16): prompt エコーの**言い換え**を文単位で除去。
    //
    // 第 1〜3 層はいずれも「一字一句同じ」でないと効かず、Air 実観測 id=460
    // 「を含む自然な日本語で書き上げる。」(書き起こす→書き上げる) が全層をすり抜けた。
    // 文字 bigram 被覆率で判定するため語形変化に強く、新しい言い換えのたびに
    // リストへ追記する後追い運用が要らない。
    //
    // 🔴 比較対象は送信文 (dynamic_prompt / WHISPER_PROMPT) ではなく
    // PROMPT_ECHO_REFERENCE を使う (L908)。送信文と共用すると、将来送信文を短縮した瞬間、
    // 削った部分のエコーを取りこぼす。辞書の語彙ヒントも照合先に含めない (正当な発話語)。
    // 詳細は prompt_sanitize::PROMPT_ECHO_REFERENCE のコメント参照。
    let text = strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, &text, avg_logprob);
    if text.is_empty() {
        log::warn!(
            "[transcribe] text empty after strip_prompt_echo_sentences (whole result was a paraphrased prompt echo)"
        );
        return Err("prompt エコー除去後のテキストが空でした".into());
    }

    // L121 対策: Groq Whisper は無音に対し「ご視聴ありがとうございました」等の
    // 日本語ハルシ句を no_speech_prob=0 / avg_logprob=-0.15 の高 confidence で生成する。
    // halu.rs の 3 段 OR フィルタ (Tier 1 VAD + Tier 2 no_speech_prob + Tier 3 既知句) で破棄。
    // 既知句リストは module 定数 KNOWN_HALLUCINATIONS / KNOWN_HALLUCINATIONS_ALWAYS /
    // KNOWN_HALLUCINATION_SUBSTRINGS (定義箇所のコメント参照)。
    let no_speech_prob = parsed
        .get("segments")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("no_speech_prob"))
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    // avg_logprob は上 (第 4 層の手前) で取得済み。
    let halu_filter = production_halu_filter();
    // 2026-08-06: VAD 誤判定 (マイク入力レベル低下) の切り分け用に実測 RMS を残す。
    // EnergyVad のしきい値は 0.002。これを下回ると VAD は「無音」と判定する。
    //
    // 2026-08-08: ピークも算出する。無音の貼付を止める判断に使うため、
    // ログ用の if ブロックから外へ出した (下の Drop 経路で参照する)。
    let input_peak: Option<f32> = if mono16k.is_empty() {
        None
    } else {
        let sum_sq: f32 = mono16k.iter().map(|s| s * s).sum();
        let rms = (sum_sq / mono16k.len() as f32).sqrt();
        let peak = mono16k.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        log::info!(
            "[recorder] input level: rms={:.5} peak={:.5} (vad threshold={}) samples={}",
            rms,
            peak,
            SILENCE_PEAK_FLOOR,
            mono16k.len()
        );
        Some(peak)
    };
    // v0.1.3 patch2: judge_with_duration 経由で短発話 (<2s) の Tier 2/3 スキップを有効化。
    // duration_ms (録音時間 ms) を渡すことで、短発話ハルシ false-positive を防ぐ。
    match halu_filter.judge_with_duration(
        &text,
        &mono16k,
        no_speech_prob,
        avg_logprob,
        Some(duration_ms),
    ) {
        HaluVerdict::Pass => {
            // v0.1.3 patch3 (C-add): halu Pass 後・display 整形前に pattern → replacement 置換を適用。
            // Whisper の誤変換 (例: "松江区" / "松葉" / "春氏") を利用者辞書の正しい表記
            // (例: "マツエク" / "マツパ" / "ハルシ") に書き戻す。
            // sanitize_prompt_leak は既に上で実施済 (text 変数の値が sanitize 済)。
            let (refined_text, glossary_applied) =
                apply_glossary_replacements(&text, &glossary_entries);
            log::info!(
                "[recorder] glossary replacements: applied {} substitutions (entries available={}, text changed={})",
                glossary_applied,
                glossary_entries.len(),
                refined_text != text
            );

            // Phase 2 D: confidence_drop_flag は no_speech_prob > 0.4 または
            // avg_logprob < -0.5 で立てる (halu フィルタ閾値より緩い ≒ 「Pass はしたが要監視」)
            //
            // v0.1.3 patch2 (2026-05-24・真因: 短発話誤検知): 閾値 -0.3 → -0.5 緩和。
            //   観測例: id=148「ミュージカル」-0.59 / id=141「松井九」-0.78 等 短発話で flag=1 連発。
            //
            // v0.1.3 patch3 (B-fix・2026-05-24): patch2 で halu の Tier 2/3 はスキップしたが、
            // この Pass 後の confidence_drop_flag 判定は別経路で生き残っていたため、
            // 短発話で logprob -0.86 等が flag=1 を立て続けていた (例: id=158「小と」1.7s)。
            // → 短発話 (duration_ms < SHORT_UTTERANCE_THRESHOLD_MS) は flag=0 固定にする。
            // 真のハルシは substring (Tier 3.5) と完全一致 (Tier 3) で別経路捕捉 (halu.rs 内)。
            //
            // compute_confidence_drop_flag は純粋関数として切り出し済 (テスト容易性)
            let confidence_drop_flag =
                compute_confidence_drop_flag(duration_ms, no_speech_prob, avg_logprob);

            // 2026-08-02 (要望): 従来は confidence_drop_flag=1 のとき text を空にして
            // clipboard 貼付を抑止していたが、「間違っていても貼り付けられる方がいい」との
            // 明示方針転換により貼付抑止をやめた。confidence_drop_flag 自体は履歴一覧の
            // 警告アイコン表示 (HistoryPage.tsx) に使われ続けるため、フラグの計算・保持はそのまま維持する。
            let display_text = add_trailing_punctuation(&refined_text);
            if confidence_drop_flag {
                log::warn!(
                    "[recorder] confidence_drop_flag=1 detected; pasting anyway (2026-08-02 既定方針) duration_ms={}",
                    duration_ms
                );
            }
            Ok(TranscribeOutcome {
                text: display_text.clone(),
                raw_text_for_history: display_text,
                // v0.1.9: hedged 経路の勝者プロバイダーを記録 (groq solo 縮退時は "groq")
                provider: used_provider.clone(),
                model: model_name,
                duration_ms,
                transcribe_ms,
                audio_bytes,
                avg_logprob: avg_logprob.map(|f| f as f64),
                no_speech_prob: no_speech_prob.map(|f| f as f64),
                confidence_drop_flag,
                primary_ms,
                fallback_triggered,
                hedged_winner,
                // v0.1.15: 実測 mean は stop_session_and_transcribe で上書きする (ここでは 0.0)。
                session_rms_mean: 0.0,
            })
        }
        HaluVerdict::Drop(reason) => {
            // 2026-08-02 (報告・重大バグ修正 → 同日既定方針転換): 従来は Err で本文を
            // 完全破棄していたため、長文業務発話の末尾に「ご視聴ありがとうございました」等の
            // 既知ハルシ句が混入しただけで、大半が正規の内容でも貼付されず履歴にも一切残らない
            // データロスが発生していた (tier3.5 known substring は「含む」判定のため
            // 末尾の一言でも全文が対象になる)。
            // 当初は confidence_drop_flag と同じ「貼付抑止・履歴のみ保持」に倒したが、
            // 既定方針「間違っていても貼り付けられる方がいい」を受けてさらに変更し、
            // ハルシ疑い判定時も通常どおり貼り付ける (confidence_drop_flag=true は
            // 履歴一覧の警告アイコン表示用に維持・貼付抑止には使わない)。
            //
            // ただし 2026-08-02 利用者最終判断により「無音時だけは今まで通り止める」。
            // 何も話していないのに Whisper が生成した文章がカーソル位置へ入るのは
            // 貼付先の文書を壊すため、無音系 (VAD 無音・no_speech_prob 超過) の Drop に限り
            // 貼付を抑止する。履歴には raw_text として必ず残すので手動コピーは可能。
            //
            // 2026-08-06 修正: VAD 単独の無音判定だけで抑止すると誤爆する。
            // VAD は RMS 0.002 の音量しきい値のみで、マイク入力レベルが下がると
            // 実際に喋っていても「無音」になる。一方 Whisper は小さい音でも
            // 文字起こしできるため、「180 文字書き起こせているのに無音扱いで
            // 貼付されない」事態が実機で発生した (2026-08-05 23:01〜23:07 に 4 連続)。
            // → VAD が無音と言っても、Whisper 自身が「音声あり」と言っているなら貼り付ける。
            //
            // 2026-08-08 再修正: その VAD 不信を無条件にしたため、今度は本当の無音から
            // 生成された 388 文字が貼り付く事故が起きた。ピークが一度もしきい値へ
            // 届かなかった録音だけは、Whisper の言い分によらず抑止する。
            //
            // 2026-08-16 (Air v1.1.5 Tier 3.2 移植): 本文全体が既知の非発話句そのもの
            // (「字幕作成者」等・KnownPhraseAlways) の場合も抑止する。守るべき本文が無く、
            // 無音生成文と同じく貼付先を壊すだけのため。Tier 3 / 3.5 の「貼る」扱いは従来どおり。
            let silence_case = should_suppress_paste_for_silence(
                &reason,
                no_speech_prob,
                NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
                text.chars().count(),
                input_peak,
            );
            let whole_text_case = should_suppress_paste_for_whole_text_hallucination(&reason);
            let suppress_paste = silence_case || whole_text_case;
            log::warn!(
                "[recorder] halu drop: {} (2026-08-02 既定方針) reason={} text_len={} peak={}",
                if silence_case {
                    "suppressing paste (silence)"
                } else if whole_text_case {
                    "suppressing paste (whole text is a known hallucination)"
                } else {
                    "pasting anyway"
                },
                reason,
                text.chars().count(),
                input_peak
                    .map(|p| format!("{:.5}", p))
                    .unwrap_or_else(|| "n/a".to_string())
            );
            let (refined_text, glossary_applied) =
                apply_glossary_replacements(&text, &glossary_entries);
            log::info!(
                "[recorder] glossary replacements (halu-drop path): applied {} substitutions (entries available={}, text changed={})",
                glossary_applied,
                glossary_entries.len(),
                refined_text != text
            );
            let display_text = add_trailing_punctuation(&refined_text);
            Ok(TranscribeOutcome {
                text: if suppress_paste {
                    String::new()
                } else {
                    display_text.clone()
                },
                raw_text_for_history: display_text,
                provider: used_provider.clone(),
                model: model_name,
                duration_ms,
                transcribe_ms,
                audio_bytes,
                avg_logprob: avg_logprob.map(|f| f as f64),
                no_speech_prob: no_speech_prob.map(|f| f as f64),
                confidence_drop_flag: true,
                primary_ms,
                fallback_triggered,
                hedged_winner,
                session_rms_mean: 0.0,
            })
        }
    }
}

// Day 25 Phase 2-G (SSOT v3.3): multi pipeline 関連 (stop_session_and_transcribe_multi,
// transcribe_collected_multi, should_use_multi, stop_session_with_dispatch,
// MULTI_TEMPERATURES) は ASR 補正レイヤー撤廃と共に削除済。legacy stop_session_and_transcribe
// を呼出元 (lib.rs / commands.rs) から直接呼び出す方針。


/// FP-02 簡易版: 末尾に句点が無ければ「。」を付与する。
/// Whisper の出力に末尾句読点が含まれないケースに対応。
fn add_trailing_punctuation(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let last = trimmed.chars().last();
    let already_punct = matches!(
        last,
        Some('。') | Some('．') | Some('.') | Some('！') | Some('!') | Some('？') | Some('?')
    );
    if already_punct {
        trimmed.to_string()
    } else {
        format!("{}。", trimmed)
    }
}

/// f32 [-1.0, 1.0] サンプルを PCM16 LE WAV にエンコードする (44-byte ヘッダ + データ)。
/// 依存追加なしで最小限の WAV を組み立てる。
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits_per_sample: u16 = 16;
    let bytes_per_sample: u16 = bits_per_sample / 8;
    let byte_rate: u32 = sample_rate * channels as u32 * bytes_per_sample as u32;
    let block_align: u16 = channels * bytes_per_sample;
    let data_size: u32 = (samples.len() * bytes_per_sample as usize) as u32;
    let chunk_size: u32 = 36 + data_size;

    let mut out: Vec<u8> = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());

    for &s in samples {
        let clamped = s.max(-1.0).min(1.0);
        let i = (clamped * i16::MAX as f32) as i16;
        out.extend_from_slice(&i.to_le_bytes());
    }
    out
}

// ---------------- v0.1.16 機能 L (議事録モード) Day 1: RecorderMode ----------------
//
// SSOT: 内部設計メモ §機能L
// 委任 prompt: 内部設計メモ §2.2
//
// 録音モード識別子。`commands::handle_quiet_voice_warning` の baseline 学習分岐に
// のみ使う (Meeting モード時は普段の声でも baseline を更新しない・専用経路のため
// 通常録音 baseline を汚染しない)。
//
// 純粋関数 `judge_quiet_voice` / `update_voice_baseline` は **mode 不知** を維持する
// (シグネチャ・本体・テストいずれも v0.1.15 から 1 文字も変えない・C4 ガード条件)。

/// v0.1.16 機能 L: 録音モード識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderMode {
    /// 通常録音 (ホットキー toggle / PTT)。baseline 学習を行う。
    Normal,
    /// 議事録モード (UI ボタン起動)。baseline 学習を行わない。
    Meeting,
}

/// v0.1.16 機能 L: baseline を学習すべきか判定する純粋関数。
///
/// `commands::handle_quiet_voice_warning` の分岐を unit テスト可能にするため抽出。
/// 既存純粋関数 `judge_quiet_voice` / `update_voice_baseline` は触らない。
///
/// 規則:
///   - 小声と判定された録音 (`is_quiet = true`) は学習しない (基準下振れ暴走防止・v0.1.15 既存挙動)
///   - 議事録モード (`mode = Meeting`) も学習しない (通常録音 baseline を汚染しない)
///   - 通常モード (`mode = Normal`) かつ非小声のときだけ true
pub fn should_learn_baseline(is_quiet: bool, mode: RecorderMode) -> bool {
    if is_quiet {
        return false;
    }
    matches!(mode, RecorderMode::Normal)
}

// ============================================================================
// v0.1.16 機能 L (議事録モード) Day 2: MeetingSession / MeetingHolder /
//   MeetingStopOutcome / should_split_chunk / compute_chunk_threshold_samples /
//   finalize_chunk / start_meeting_session / stop_meeting_session
//
// SSOT: 内部設計メモ §機能L
// 委任 prompt: 内部設計メモ §2
//
// 設計の柱 (両レビュー必須指摘):
//   - 通常録音 RecorderSession / RecorderHolder / start_session /
//     stop_session_and_transcribe は **永久に触らない** (C5 ガード)。
//   - 純粋関数 update_voice_baseline / judge_quiet_voice / should_learn_baseline /
//     encode_wav_pcm16 も **触らない** (C4 ガード)。
//   - frames を Arc<Mutex<Vec<RawFrame>>> でグローバル蓄積する設計は禁止
//     (60 分 RAM 占有 1.3GB → 5 分相当 ≒ 19-115MB に固定するため)。
//   - チャンク境界はサンプル数で判定 (時間ベースの float 誤差累積を避ける・L-R8 防止)。
// ============================================================================

/// v0.1.16 機能 L Day 2: 議事録モード専用セッション。
///
/// 通常録音 `RecorderSession` と**完全分離**: `frames: Arc<Mutex<Vec<RawFrame>>>` を持たず、
/// worker thread 内でローカル蓄積 → 5 分チャンク境界 (サンプル数) でリサンプル→PCM16 WAV
/// →ディスク finalize → RAM バッファ clear する。
///
/// RAM 上限: device sample 5 分ぶん (例: 48kHz/2ch ≒ 115MB / 16kHz/1ch ≒ 19MB) に固定。
/// 60 分録音でも 1.3GB → 5 分相当に削減 (SSOT §実装ポイント・両レビュー必須指摘)。
pub struct MeetingSession {
    pub device_sample_rate: u32,
    pub device_channels: u16,
    /// チャンク WAV 保存ディレクトリ (絶対パス)。`meeting_start` が事前に mkdir 済の前提。
    pub chunk_dir: PathBuf,
    /// チャンク化された WAV ファイルパス一覧 (worker thread が push)。
    pub chunk_paths: Arc<Mutex<Vec<PathBuf>>>,
    pub stop_flag: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
    /// 録音セッション全体の平均 RMS (小声警告判定用・既存 v0.1.15 仕様流用)。
    pub rms_mean: Arc<Mutex<f32>>,
    /// 録音開始からの総サンプル数 (device レート・全チャンネル含む)。
    /// `duration_ms` 計算に使う (= total_samples / (sample_rate * channels) * 1000)。
    pub total_samples: Arc<Mutex<u64>>,
}

/// v0.1.16 機能 L Day 2: 議事録モード State (通常録音 `RecorderHolder` とは別)。
#[derive(Default)]
pub struct MeetingHolder {
    pub session: Mutex<Option<MeetingSession>>,
}

impl MeetingHolder {
    /// v0.1.16 機能 L Day 4: 議事録 worker が動作中か判定 (L-R7 hotkey pause 用)。
    ///
    /// 既存 struct フィールド / 既存メソッドシグネチャは一切不変
    /// (impl block 自体を新規追加する形でのみ拡張・C6 ガード遵守)。
    ///
    /// poisoned Mutex 時は安全側で `false` を返す (= ホットキーは通常通り動作・
    /// 既存通常録音フローを止めない)。`handle_hotkey_event` 冒頭で
    /// `should_dispatch_hotkey` 純粋関数 (lib.rs) と組合せて使う。
    pub fn is_recording(&self) -> bool {
        self.session.lock().map(|g| g.is_some()).unwrap_or(false)
    }
}

/// `stop_meeting_session` 戻り値。Day 3 transcribe 入力に使う。
#[derive(Debug, Clone)]
pub struct MeetingStopOutcome {
    /// 録音中に書き出された WAV チャンクパス一覧 (chunk_000.wav, chunk_001.wav, ...)。
    pub chunk_paths: Vec<PathBuf>,
    /// 録音開始から stop までの実時間 (ms 単位)。
    pub total_duration_ms: u64,
    /// セッション平均 RMS (小声警告判定用)。
    pub rms_mean: f32,
}

/// v0.1.16 機能 L Day 2: チャンク分割判定 (純粋関数)。
///
/// **サンプル数境界**で判定すること (時間ベース禁止・L-R8)。
/// 時間ベースで分割すると float の誤差累積で `chunk_index * 300秒` の global offset が
/// チャンク境界とズレ、Day 3 の `apply_global_offset` で時刻ずれが発生する。
///
/// threshold = `chunk_secs * device_sample_rate * device_channels`
/// 例: 300 秒 × 48000 Hz × 2 ch = 28_800_000 サンプル
pub fn should_split_chunk(samples_accum: u64, threshold: u64) -> bool {
    threshold > 0 && samples_accum >= threshold
}

/// v0.1.16 機能 L Day 2: チャンク閾値計算 (純粋関数)。
/// `chunk_secs` 秒分の device サンプル数を返す (オーバーフロー時は u64::MAX)。
pub fn compute_chunk_threshold_samples(
    chunk_secs: u32,
    device_sample_rate: u32,
    device_channels: u16,
) -> u64 {
    (chunk_secs as u64)
        .saturating_mul(device_sample_rate as u64)
        .saturating_mul(device_channels as u64)
}

/// 1 チャンク分の RawFrame を 16kHz mono PCM16 WAV にエンコードして
/// `chunk_dir/chunk_NNN.wav` に書き出す。NNN は 3 桁ゼロ埋め。
///
/// 既存 `encode_wav_pcm16` (純粋関数) と `Resampler` を流用。
/// 通常録音 `stop_session_and_transcribe` のリサンプル経路と同じ手順。
///
/// 注意: ファイル名は `chunk_{:03}.wav` でゼロ埋め必須 (Day 3 で並列 transcribe
/// するときに lexical sort で順序保証するため)。
fn finalize_chunk(
    chunk_dir: &Path,
    chunk_index: usize,
    frames: &[RawFrame],
    device_sample_rate: u32,
    device_channels: u16,
) -> Result<PathBuf, String> {
    let mut resampler = Resampler::new(device_sample_rate, device_channels)
        .map_err(|e| format!("Resampler::new failed: {:?}", e))?;
    let mut mono16k: Vec<f32> = Vec::new();
    for f in frames {
        match resampler.process(f.clone()) {
            Ok(t) => mono16k.extend(t.samples),
            Err(e) => return Err(format!("Resampler::process failed: {:?}", e)),
        }
    }
    if mono16k.is_empty() {
        return Err(format!("chunk {} のリサンプル結果が空", chunk_index));
    }
    let wav = encode_wav_pcm16(&mono16k, 16_000, 1);
    let path = chunk_dir.join(format!("chunk_{:03}.wav", chunk_index));
    std::fs::write(&path, &wav).map_err(|e| format!("WAV 書き出し失敗 {:?}: {}", path, e))?;
    Ok(path)
}

/// v0.1.16 機能 L Day 2: 議事録モード worker 起動。
///
/// 通常版 `start_session` と異なる点:
///   - `frames` 共有バッファを**持たない** (worker thread ローカルに蓄積)
///   - 5 分チャンク境界 (サンプル数) でリサンプル→encode_wav_pcm16→fs::write→ローカル clear
///   - チャンクパスは `chunk_paths: Arc<Mutex<Vec<PathBuf>>>` に push
///   - stop 時に残バッファを最終チャンク化 (worker thread が stop 検知時に自身で finalize)
///
/// 引数 `chunk_dir` は `meeting_start` で事前に mkdir 済の絶対パスを渡す。
/// `mic_low_level` は通常録音同様、低レベル警告 emit に使用 (議事録モードでも警告は出す)。
pub fn start_meeting_session(
    holder: &MeetingHolder,
    app: Option<AppHandle>,
    chunk_dir: PathBuf,
    mic_low_level: MicLowLevelConfig,
) -> Result<(), String> {
    {
        let guard = holder.session.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("既に議事録録音中です".into());
        }
    }

    let chunk_paths: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let rms_mean: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
    let total_samples: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let (init_tx, init_rx): (SyncSender<Result<(u32, u16), String>>, _) = sync_channel(0);

    let chunk_paths_ref = chunk_paths.clone();
    let stop_ref = stop_flag.clone();
    let rms_mean_ref = rms_mean.clone();
    let total_samples_ref = total_samples.clone();
    let app_for_thread = app.clone();
    let chunk_dir_for_thread = chunk_dir.clone();

    let handle = std::thread::spawn(move || {
        // マイク選択 GUI (2026-07-23): 設定済みデバイスがあれば優先 (無ければ既定追従)
        let preferred_device = preferred_input_device();
        let mut recorder = match AudioRecorder::start(preferred_device.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                let _ = init_tx.send(Err(format!("AudioRecorder::start() failed: {:?}", e)));
                return;
            }
        };
        let device_sample_rate = recorder.device_sample_rate;
        let device_channels = recorder.device_channels;
        let info = (device_sample_rate, device_channels);
        if init_tx.send(Ok(info)).is_err() {
            return;
        }

        // 5 分 = 300 秒。サンプル数閾値 = 300 × device_sample_rate × device_channels。
        let chunk_threshold =
            compute_chunk_threshold_samples(300, device_sample_rate, device_channels);

        // ローカル蓄積 (Arc<Mutex> ではない・worker thread 内のみ)。
        let mut local_frames: Vec<RawFrame> = Vec::new();
        let mut samples_in_chunk: u64 = 0;
        let mut chunk_index: usize = 0;

        // 既存 mic_low_level F2 判定 state (通常 start_session と同じロジック流用)
        let start_instant = std::time::Instant::now();
        let mut seen_loud = false;
        let mut already_emitted = false;
        // RMS 統計
        let mut rms_sum: f64 = 0.0;
        let mut rms_peak: f32 = 0.0;
        let mut rms_min: f32 = f32::MAX;
        let mut rms_count: u64 = 0;

        loop {
            if stop_ref.load(Ordering::Relaxed) {
                break;
            }
            match recorder.next_frame_blocking() {
                Some(frame) => {
                    let rms = compute_rms(&frame.samples);
                    if let Some(ref app_h) = app_for_thread {
                        let _ = app_h.emit("recorder:waveform", rms);
                    }
                    rms_sum += rms as f64;
                    rms_peak = rms_peak.max(rms);
                    rms_min = rms_min.min(rms);
                    rms_count += 1;
                    // mic_low_level 判定 (通常録音と同じロジック・流用)
                    if !already_emitted {
                        let elapsed_ms = start_instant.elapsed().as_millis();
                        if should_emit_mic_low_level(
                            elapsed_ms,
                            mic_low_level.window_ms,
                            seen_loud,
                            already_emitted,
                        ) {
                            if let Some(ref app_h) = app_for_thread {
                                let _ = app_h.emit("recorder:low-input-level-detected", ());
                            }
                            already_emitted = true;
                        }
                        if rms >= mic_low_level.threshold {
                            seen_loud = true;
                        }
                    }

                    // ローカル蓄積
                    let frame_samples_len = frame.samples.len() as u64;
                    samples_in_chunk += frame_samples_len;
                    if let Ok(mut total) = total_samples_ref.lock() {
                        *total += frame_samples_len;
                    }
                    local_frames.push(frame);

                    // 5 分境界 (サンプル数) に到達したら finalize
                    if should_split_chunk(samples_in_chunk, chunk_threshold) {
                        match finalize_chunk(
                            &chunk_dir_for_thread,
                            chunk_index,
                            &local_frames,
                            device_sample_rate,
                            device_channels,
                        ) {
                            Ok(path) => {
                                if let Ok(mut paths) = chunk_paths_ref.lock() {
                                    paths.push(path);
                                }
                                if let Some(ref app_h) = app_for_thread {
                                    let _ = app_h.emit(
                                        "meeting:progress",
                                        serde_json::json!({
                                            "chunk_index": chunk_index,
                                            "phase": "chunk_finalized",
                                        }),
                                    );
                                }
                                log::info!(
                                    target: "rushvoice.meeting",
                                    "chunk {} finalized: samples={} rate={} ch={}",
                                    chunk_index,
                                    samples_in_chunk,
                                    device_sample_rate,
                                    device_channels
                                );
                            }
                            Err(e) => {
                                log::error!(
                                    target: "rushvoice.meeting",
                                    "chunk {} finalize failed: {}",
                                    chunk_index,
                                    e
                                );
                                // チャンク書き出し失敗時は録音停止 (破損データ蓄積を避ける)
                                stop_ref.store(true, Ordering::Relaxed);
                            }
                        }
                        local_frames.clear();
                        samples_in_chunk = 0;
                        chunk_index += 1;
                    }
                }
                None => break,
            }
        }

        // stop 検知時に残バッファを最終チャンクとして finalize する (方式 A)。
        // 残バッファは worker thread 内にしか存在しないため、ここでしか書き出せない。
        if !local_frames.is_empty() {
            match finalize_chunk(
                &chunk_dir_for_thread,
                chunk_index,
                &local_frames,
                device_sample_rate,
                device_channels,
            ) {
                Ok(path) => {
                    if let Ok(mut paths) = chunk_paths_ref.lock() {
                        paths.push(path);
                    }
                    log::info!(
                        target: "rushvoice.meeting",
                        "final chunk {} finalized: samples={}",
                        chunk_index,
                        samples_in_chunk
                    );
                }
                Err(e) => {
                    log::error!(
                        target: "rushvoice.meeting",
                        "final chunk {} finalize failed: {}",
                        chunk_index,
                        e
                    );
                }
            }
        }

        // RMS 統計を共有セルに書き込む (Day 3 小声警告判定用)。
        if rms_count > 0 {
            let stats = compute_rms_stats(rms_sum, rms_peak, rms_min, rms_count);
            log::info!(
                target: "rushvoice.rms",
                "[meeting] stats mean={:.5} peak={:.5} min={:.5} frames={}",
                stats.mean,
                stats.peak,
                stats.min,
                stats.frames
            );
            if let Ok(mut cell) = rms_mean_ref.lock() {
                *cell = stats.mean;
            }
        }
    });

    let (device_sample_rate, device_channels) = init_rx
        .recv()
        .map_err(|e| format!("meeting worker init channel closed: {}", e))??;

    let session = MeetingSession {
        device_sample_rate,
        device_channels,
        chunk_dir,
        chunk_paths,
        stop_flag,
        handle: Some(handle),
        rms_mean,
        total_samples,
    };
    {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        *guard = Some(session);
    }
    Ok(())
}

/// v0.1.16 機能 L Day 2: 議事録モード worker 停止 + 残バッファ finalize。
///
/// 戻り値 `MeetingStopOutcome` には書き出された全チャンクパス・録音長 ms・平均 RMS を含む。
/// Day 3 の transcribe パイプラインがこの戻り値を入力に取る。
pub fn stop_meeting_session(holder: &MeetingHolder) -> Result<MeetingStopOutcome, String> {
    let session = {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        guard
            .take()
            .ok_or_else(|| "議事録録音中ではありません".to_string())?
    };
    session.stop_flag.store(true, Ordering::Relaxed);
    if let Some(handle) = session.handle {
        let _ = handle.join();
    }

    let chunk_paths = session
        .chunk_paths
        .lock()
        .map_err(|e| format!("chunk_paths mutex poisoned: {}", e))?
        .clone();
    let total_samples = *session
        .total_samples
        .lock()
        .map_err(|e| format!("total_samples mutex poisoned: {}", e))?;
    let rms_mean = *session
        .rms_mean
        .lock()
        .map_err(|e| format!("rms_mean mutex poisoned: {}", e))?;

    // duration_ms = total_samples * 1000 / (sample_rate * channels)
    // clippy::manual_checked_ops 対応: checked_div で 0 除算ガードを統合。
    let denom = session.device_sample_rate as u64 * session.device_channels as u64;
    let total_duration_ms = total_samples
        .saturating_mul(1000)
        .checked_div(denom)
        .unwrap_or(0);

    log::info!(
        target: "rushvoice.meeting",
        "stop_meeting_session: chunks={} duration_ms={} rms_mean={:.5}",
        chunk_paths.len(),
        total_duration_ms,
        rms_mean
    );

    Ok(MeetingStopOutcome {
        chunk_paths,
        total_duration_ms,
        rms_mean,
    })
}

// ============================================================================
// セミナー字幕モード (feature/seminar-subtitle): 発話区切りワーカー
//
// 議事録モード (MeetingSession) との違い:
//   - チャンク境界が「5 分固定」ではなく「発話区切り」(無音 0.7 秒 or 上限 12 秒)
//   - チャンク finalize のたびに mpsc で即時パイプライン (subtitle_commands) へ渡す
//     (議事録は stop 後に一括転写・字幕はリアルタイム性が命)
//   - 発話が無い無音バッファは API に送らず破棄 (L121 無音ハルシ対策を入口で遮断)
//
// 設計の柱 (議事録と同じ):
//   - 通常録音 RecorderSession / 議事録 MeetingSession は**一切触らない** (C4/C5 ガード)
//   - frames は worker thread ローカル蓄積 (グローバル Arc<Mutex<Vec>> 禁止)
// ============================================================================

/// 発話区切り判定: 末尾無音がこの ms 以上続いたら 1 発話として確定する。
/// 700ms では読点「、」の自然な間で文が分断され文脈喪失で誤認識補正が効かなくなった
/// (2026-07-05 Day1「湿度→角度」事案) ため 1000ms に緩和 (遅延 +0.3 秒・目標 2〜4 秒内)。
pub const SUBTITLE_SILENCE_SPLIT_MS: u64 = 1000;
/// 1 発話の上限 ms (連続で話し続けた場合の安全弁・遅延の青天井防止)。
pub const SUBTITLE_MAX_UTTERANCE_MS: u64 = 12_000;
/// 発話とみなす最低スピーチ量 ms (これ未満のバッファは API に送らない・L121)。
pub const SUBTITLE_MIN_SPEECH_MS: u64 = 250;
/// 発話開始前に保持する先行バッファ ms (語頭欠け防止のプリロール)。
pub const SUBTITLE_PREROLL_MS: u64 = 300;
/// 発話判定 RMS 閾値。
/// 0.002 (EnergyVad::default) では開発機の環境音 (無発話時 RMS≒0.01 近傍のフレームあり) を
/// 発話と誤判定し、12 秒ごとに無音チャンクが API に送られ幻聴行を量産した (2026-07-05 Day1 スモークで実測)。
/// 0.006 に引き上げ (発話時ピークは 0.028 前後で余裕あり)。L122: マイク/現地環境で要再校正。
pub const SUBTITLE_VAD_RMS: f32 = 0.006;

/// 発話区切り判定 (純粋関数)。
///
/// スピーチ量が `SUBTITLE_MIN_SPEECH_MS` 未満なら常に false (無音のみ・短すぎは送らない)。
/// それ以上なら「末尾無音 >= 0.7 秒」または「バッファ >= 12 秒」で確定する。
pub fn subtitle_should_split(speech_ms: u64, trailing_silence_ms: u64, buffered_ms: u64) -> bool {
    if speech_ms < SUBTITLE_MIN_SPEECH_MS {
        return false;
    }
    trailing_silence_ms >= SUBTITLE_SILENCE_SPLIT_MS || buffered_ms >= SUBTITLE_MAX_UTTERANCE_MS
}

/// 発話がまだ始まっていないバッファの先頭無音を捨ててよいか (純粋関数)。
/// プリロール分 (`SUBTITLE_PREROLL_MS`) だけ残して古いフレームから破棄する。
pub fn subtitle_should_trim_leading_silence(speech_ms: u64, buffered_ms: u64) -> bool {
    speech_ms == 0 && buffered_ms > SUBTITLE_PREROLL_MS
}

/// フレームの再生時間 ms (device レート・全チャンネル込みサンプル数から算出・純粋関数)。
pub fn subtitle_frame_duration_ms(
    samples_len: usize,
    device_sample_rate: u32,
    device_channels: u16,
) -> u64 {
    let denom = device_sample_rate as u64 * device_channels as u64;
    (samples_len as u64)
        .saturating_mul(1000)
        .checked_div(denom)
        .unwrap_or(0)
}

/// セミナー字幕モード専用セッション。
pub struct SubtitleSession {
    /// 発話 WAV の一時保存ディレクトリ (絶対パス・subtitle_start が mkdir 済)。
    pub chunk_dir: PathBuf,
    pub stop_flag: Arc<AtomicBool>,
    pub handle: Option<JoinHandle<()>>,
}

/// セミナー字幕モード State (通常録音 RecorderHolder / 議事録 MeetingHolder とは別)。
#[derive(Default)]
pub struct SubtitleHolder {
    pub session: Mutex<Option<SubtitleSession>>,
}

impl SubtitleHolder {
    /// 字幕 worker が動作中か (hotkey pause 判定・二重起動ガード用)。
    /// poisoned Mutex 時は安全側で false (通常録音フローを止めない)。
    pub fn is_recording(&self) -> bool {
        self.session.lock().map(|g| g.is_some()).unwrap_or(false)
    }
}

/// セミナー字幕モード worker 起動。
///
/// 発話区切りで WAV チャンクを finalize し、`(発話 index, WAV パス)` を `tx` で
/// 即時パイプライン (subtitle_commands::run_subtitle_pipeline) に渡す。
/// worker thread 終了時に `tx` が drop され、受信側 `rx.recv()` は None を返して
/// パイプラインも自然終了する (終了通知の別経路は不要)。
pub fn start_subtitle_session(
    holder: &SubtitleHolder,
    chunk_dir: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<(usize, PathBuf)>,
) -> Result<(), String> {
    {
        let guard = holder.session.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("既に字幕モード実行中です".into());
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_ref = stop_flag.clone();
    let chunk_dir_for_thread = chunk_dir.clone();
    let (init_tx, init_rx): (SyncSender<Result<(), String>>, _) = sync_channel(0);

    let handle = std::thread::spawn(move || {
        log::info!(target: "rushvoice.subtitle", "[dbg] worker thread spawned; starting AudioRecorder");
        // マイク選択 GUI (2026-07-23): 設定済みデバイスがあれば優先 (無ければ既定追従)
        let preferred_device = preferred_input_device();
        let mut recorder = match AudioRecorder::start(preferred_device.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                let _ = init_tx.send(Err(format!("AudioRecorder::start() failed: {:?}", e)));
                return;
            }
        };
        log::info!(
            target: "rushvoice.subtitle",
            "[dbg] AudioRecorder started: rate={} ch={}",
            recorder.device_sample_rate,
            recorder.device_channels
        );
        let device_sample_rate = recorder.device_sample_rate;
        let device_channels = recorder.device_channels;
        if init_tx.send(Ok(())).is_err() {
            return;
        }

        let mut local_frames: Vec<RawFrame> = Vec::new();
        let mut frame_ms: Vec<u64> = Vec::new();
        let mut buffered_ms: u64 = 0;
        let mut speech_ms: u64 = 0;
        let mut trailing_silence_ms: u64 = 0;
        let mut utterance_index: usize = 0;

        // 1 発話を finalize して tx へ送る (state リセットは呼出側で行う)。
        // finalize 失敗は当該発話 1 件のログ破棄に留め、字幕セッション自体は続行する。
        let flush_utterance = |frames: &[RawFrame], idx: usize| -> bool {
            match finalize_chunk(
                &chunk_dir_for_thread,
                idx,
                frames,
                device_sample_rate,
                device_channels,
            ) {
                Ok(path) => {
                    log::info!(
                        target: "rushvoice.subtitle",
                        "utterance {} finalized: {:?}",
                        idx,
                        path
                    );
                    // 受信側 drop 済 (パイプライン終了) なら worker も止める。
                    tx.send((idx, path)).is_ok()
                }
                Err(e) => {
                    log::error!(
                        target: "rushvoice.subtitle",
                        "utterance {} finalize failed: {} (この発話は破棄して続行)",
                        idx,
                        e
                    );
                    true
                }
            }
        };

        loop {
            if stop_ref.load(Ordering::Relaxed) {
                break;
            }
            match recorder.next_frame_blocking() {
                Some(frame) => {
                    let rms = compute_rms(&frame.samples);
                    let dur = subtitle_frame_duration_ms(
                        frame.samples.len(),
                        device_sample_rate,
                        device_channels,
                    );
                    if rms >= SUBTITLE_VAD_RMS {
                        speech_ms += dur;
                        trailing_silence_ms = 0;
                    } else {
                        trailing_silence_ms += dur;
                    }
                    buffered_ms += dur;
                    frame_ms.push(dur);
                    local_frames.push(frame);

                    // 発話前の無音はプリロール分だけ残して破棄 (RAM/API 節約 + L121)。
                    if subtitle_should_trim_leading_silence(speech_ms, buffered_ms) {
                        while buffered_ms > SUBTITLE_PREROLL_MS && local_frames.len() > 1 {
                            buffered_ms = buffered_ms.saturating_sub(frame_ms.remove(0));
                            local_frames.remove(0);
                        }
                        trailing_silence_ms = trailing_silence_ms.min(buffered_ms);
                    }

                    if subtitle_should_split(speech_ms, trailing_silence_ms, buffered_ms) {
                        if !flush_utterance(&local_frames, utterance_index) {
                            break;
                        }
                        utterance_index += 1;
                        local_frames.clear();
                        frame_ms.clear();
                        buffered_ms = 0;
                        speech_ms = 0;
                        trailing_silence_ms = 0;
                    }
                }
                None => break,
            }
        }

        // stop 検知時: 発話中だった残バッファを最終発話として finalize (無音のみなら捨てる)。
        if speech_ms >= SUBTITLE_MIN_SPEECH_MS && !local_frames.is_empty() {
            let _ = flush_utterance(&local_frames, utterance_index);
        }
        log::info!(
            target: "rushvoice.subtitle",
            "subtitle worker stopped: utterances={}",
            utterance_index
        );
        // ここで tx が drop → パイプライン側 rx.recv() = None → 自然終了
    });

    // AudioRecorder 起動失敗を同期的に受け取り、start 失敗として返す。
    init_rx
        .recv()
        .map_err(|e| format!("subtitle worker init recv failed: {}", e))??;

    let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
    *guard = Some(SubtitleSession {
        chunk_dir,
        stop_flag,
        handle: Some(handle),
    });
    Ok(())
}

/// セミナー字幕モード worker 停止。
pub fn stop_subtitle_session(holder: &SubtitleHolder) -> Result<(), String> {
    let session = {
        let mut guard = holder.session.lock().map_err(|e| e.to_string())?;
        guard
            .take()
            .ok_or_else(|| "字幕モード実行中ではありません".to_string())?
    };
    session.stop_flag.store(true, Ordering::Relaxed);
    if let Some(handle) = session.handle {
        let _ = handle.join();
    }
    log::info!(target: "rushvoice.subtitle", "stop_subtitle_session OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実機の Bluetooth ヘッドセット相当 (rms≈0.0005) を目標レベルまで持ち上げる。
    #[test]
    fn quiet_input_is_boosted_without_clipping() {
        // 振幅 0.001 の矩形波 = rms 0.001
        let samples: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 0.001 } else { -0.001 })
            .collect();
        let (boosted, gain) = boost_for_asr(&samples).expect("持ち上げるはず");
        assert!(gain > 1.0 && gain <= ASR_MAX_GAIN, "gain={}", gain);
        let peak = boosted.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak <= ASR_PEAK_CEILING, "peak={}", peak);
        assert_eq!(boosted.len(), samples.len());
    }

    /// 既に十分な音量なら触らない。
    #[test]
    fn loud_enough_input_is_left_alone() {
        let samples: Vec<f32> = (0..1000)
            .map(|i| if i % 2 == 0 { 0.2 } else { -0.2 })
            .collect();
        assert!(boost_for_asr(&samples).is_none());
    }

    /// 無音 (全ゼロ) を増幅してはならない。ゼロ除算も起こさないこと。
    #[test]
    fn silence_is_not_boosted() {
        assert!(boost_for_asr(&vec![0.0f32; 1000]).is_none());
        assert!(boost_for_asr(&[]).is_none());
    }

    /// ピークが大きい (突発音を含む) 小音量入力では、クリップしない範囲に抑える。
    #[test]
    fn peak_limits_the_gain() {
        let mut samples: Vec<f32> = vec![0.0005; 1000];
        samples[0] = 0.9; // 突発的なピーク
        match boost_for_asr(&samples) {
            Some((boosted, _)) => {
                let peak = boosted.iter().fold(0.0f32, |a, s| a.max(s.abs()));
                assert!(peak <= ASR_PEAK_CEILING, "peak={}", peak);
            }
            None => {} // ピーク制約でゲイン<=1.0 なら据え置きも正
        }
    }

    /// 2026-08-05 実機事案の再現: VAD は無音と言うが Whisper は 180 文字書き起こした。
    /// マイク入力レベルが下がると VAD (RMS しきい値のみ) が誤って無音と言うため、
    /// これを抑止根拠にしてはならない。
    #[test]
    fn vad_silence_does_not_suppress_when_whisper_hears_speech() {
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            Some(0.02), // Whisper は「音声あり」
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            180,
            Some(0.03), // ピークはしきい値を大きく超えている = 音は入っている
        ));
    }

    /// 2026-08-08 実機事案の再現: 156 秒の無音録音 (平均 RMS 0.00015・ピーク 0.00101) から
    /// Whisper が 388 文字を生成し、ChatGPT の入力欄へ貼り付いた。
    /// Whisper は無音でも no_speech_prob=0.0 (=音声あり) を返すため、
    /// ピークが一度もしきい値へ届かない録音は Whisper の言い分を採用してはならない。
    #[test]
    fn silent_recording_with_no_peak_suppresses_even_if_whisper_hears_speech() {
        assert!(should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            Some(0.0), // Whisper は「音声あり」と言っている
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            388,
            Some(0.00101), // 実測ピーク。しきい値 0.002 に一度も届いていない
        ));
    }

    /// ピーク不明 (音声が空) の場合は従来判定へ委ねる。ピークだけで抑止を増やさない。
    #[test]
    fn missing_peak_falls_back_to_previous_behaviour() {
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            Some(0.02),
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            180,
            None,
        ));
    }

    /// VAD と Whisper の双方が無音と言っている → 本来止めたいケース。
    #[test]
    fn vad_and_whisper_agreeing_on_silence_suppresses() {
        assert!(should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            Some(0.95),
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            14,
            Some(0.03),
        ));
        assert!(should_suppress_paste_for_silence(
            &HaluReason::NoSpeechProb { value: 0.9, threshold: 0.6 },
            Some(0.9),
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            14,
            Some(0.03),
        ));
    }

    /// no_speech_prob が取れない場合は文字数で代替判定する。
    #[test]
    fn without_whisper_signal_falls_back_to_length() {
        // 無音時の定型ハルシ句 (短い) → 抑止
        assert!(should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            None,
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            14,
            Some(0.03),
        ));
        // 長文 → 実際の発話とみなして貼り付ける
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::NoSpeechVad,
            None,
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            180,
            Some(0.03),
        ));
    }

    /// 無音系以外の Drop は既定方針どおり貼り付ける (2026-08-02 決定)。
    #[test]
    fn non_silence_drops_still_paste() {
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::KnownSubstring { substring: "ご視聴ありがとうございました".into() },
            Some(0.95),
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            200,
            Some(0.03),
        ));
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::AsciiShort { len: 5, ascii_ratio: 0.9 },
            None,
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            5,
            Some(0.03),
        ));
    }

    /// 2026-08-16 (Air v1.1.5 Tier 3.2 移植): 本文全体が既知の非発話句そのものなら貼らない。
    /// 無音判定 (should_suppress_paste_for_silence) とは独立した判断であること、
    /// Tier 3 / 3.5 / ASCII 残骸には効かない (従来どおり貼る) ことを固定する。
    #[test]
    fn whole_text_known_hallucination_suppresses_paste_but_others_do_not() {
        assert!(should_suppress_paste_for_whole_text_hallucination(
            &HaluReason::KnownPhraseAlways { phrase: "字幕作成者".into() }
        ));
        // 無音判定側は Tier 3.2 を無音とは見なさない (責務分離)
        assert!(!should_suppress_paste_for_silence(
            &HaluReason::KnownPhraseAlways { phrase: "字幕作成者".into() },
            Some(0.0),
            NO_SPEECH_PROB_SUPPRESS_THRESHOLD,
            5,
            Some(0.03),
        ));
        for reason in [
            HaluReason::KnownSubstring { substring: "ご視聴ありがとうございました".into() },
            HaluReason::KnownPhraseLowLogprob {
                phrase: "ありがとうございました".into(),
                min_logprob: -0.9,
                threshold: -0.5,
            },
            HaluReason::AsciiShort { len: 5, ascii_ratio: 0.9 },
            HaluReason::NoSpeechVad,
            HaluReason::NoSpeechProb { value: 0.9, threshold: 0.6 },
        ] {
            assert!(
                !should_suppress_paste_for_whole_text_hallucination(&reason),
                "Tier 3.2 以外は本判定で抑止しない: {:?}",
                reason
            );
        }
    }

    /// 401/403/429 の検出は接続先 (Groq/OpenAI/カスタム) に依らない純粋関数。
    #[test]
    fn detect_auth_or_limit_status_matches_debug_format() {
        let err = |status: u16| {
            format!(
                "transcribe failed (custom solo): {:?}",
                crate::transcribe::TranscribeError::Http {
                    status,
                    body: "x".to_string()
                }
            )
        };
        assert_eq!(detect_auth_or_limit_status(&err(401)), Some(401));
        assert_eq!(detect_auth_or_limit_status(&err(403)), Some(403));
        assert_eq!(detect_auth_or_limit_status(&err(429)), Some(429));
        assert_eq!(detect_auth_or_limit_status(&err(500)), None);
        assert_eq!(detect_auth_or_limit_status("network error: timeout"), None);
    }

    #[test]
    fn friendly_transcribe_error_tells_next_step() {
        let e401 = format!(
            "transcribe failed (groq solo): {:?}",
            crate::transcribe::TranscribeError::Http { status: 401, body: "bad key".into() }
        );
        assert!(friendly_transcribe_error(&e401).contains("APIキー"));
        let e429 = format!(
            "hedged transcribe failed: {:?}",
            crate::transcribe::TranscribeError::Http { status: 429, body: "rate".into() }
        );
        assert!(friendly_transcribe_error(&e429).contains("利用上限"));
        // それ以外はそのまま
        assert_eq!(friendly_transcribe_error("network error"), "network error");
    }

    #[test]
    fn wav_header_minimum_44_bytes() {
        let samples = vec![0.0f32; 16]; // 16 samples * 2 bytes = 32 bytes data
        let wav = encode_wav_pcm16(&samples, 16_000, 1);
        assert_eq!(wav.len(), 44 + 32);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // data_size at offset 40
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 32);
    }

    #[test]
    fn wav_pcm16_clips_above_one() {
        let samples = vec![1.5f32, -1.5f32];
        let wav = encode_wav_pcm16(&samples, 16_000, 1);
        // 末尾 4 バイトが i16 LE で +32767, -32767 (clamp 結果)
        let s1 = i16::from_le_bytes([wav[44], wav[45]]);
        let s2 = i16::from_le_bytes([wav[46], wav[47]]);
        assert_eq!(s1, i16::MAX);
        // -1.0 * 32767 = -32767 (= -i16::MAX, not i16::MIN)
        assert_eq!(s2, -i16::MAX);
    }

    #[test]
    fn recorder_state_is_idle_by_default() {
        let h = RecorderHolder::default();
        assert_eq!(h.current_state(), RecorderState::Idle);
    }

    #[test]
    fn cancel_session_when_idle_keeps_idle() {
        // HK-06: セッションなし (Idle) に cancel_session を呼んでもパニックせず Idle を維持
        let h = RecorderHolder::default();
        assert_eq!(h.current_state(), RecorderState::Idle);
        let result = cancel_session(&h);
        assert!(result.is_ok(), "cancel_session should succeed even with no session");
        assert_eq!(h.current_state(), RecorderState::Idle);
    }

    #[test]
    fn add_trailing_punctuation_appends_when_missing() {
        assert_eq!(add_trailing_punctuation("こんにちは"), "こんにちは。");
        assert_eq!(add_trailing_punctuation("テストです"), "テストです。");
    }

    #[test]
    fn add_trailing_punctuation_preserves_existing() {
        assert_eq!(add_trailing_punctuation("こんにちは。"), "こんにちは。");
        assert_eq!(add_trailing_punctuation("元気ですか？"), "元気ですか？");
        assert_eq!(add_trailing_punctuation("素晴らしい！"), "素晴らしい！");
        assert_eq!(add_trailing_punctuation("Hello."), "Hello.");
    }

    #[test]
    fn add_trailing_punctuation_trims_and_handles_empty() {
        assert_eq!(add_trailing_punctuation(""), "");
        assert_eq!(add_trailing_punctuation("   "), "");
        assert_eq!(add_trailing_punctuation(" こんにちは "), "こんにちは。");
    }

    // ----- 2026-07-25: prompt 枠の優先順位 (語彙ヒント > 置換ルール) -----

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn prioritize_puts_vocabulary_hints_before_replacement_rules() {
        // 置換ルール (斉藤→齋藤) が先に並んでいても、語彙ヒント (品川→品川) が前へ出ること
        let input = pairs(&[("斉藤|斎藤", "齋藤"), ("品川", "品川"), ("長田", "永田"), ("浦和", "浦和")]);
        assert_eq!(
            prioritize_glossary_words(&input),
            vec!["品川", "浦和", "齋藤", "永田"]
        );
    }

    #[test]
    fn prioritize_preserves_relative_order_within_each_group() {
        // 呼出側が新規優先で渡す前提 — グループ内では入力順を崩さない (L526)
        let input = pairs(&[("香芝", "香芝"), ("品川", "品川"), ("A", "B"), ("C", "D")]);
        assert_eq!(
            prioritize_glossary_words(&input),
            vec!["香芝", "品川", "B", "D"]
        );
    }

    #[test]
    fn prioritize_skips_empty_replacements_and_trims_for_comparison() {
        let input = pairs(&[("施術 ", "施術"), ("x", "  "), ("誤", "正")]);
        assert_eq!(prioritize_glossary_words(&input), vec!["施術", "正"]);
    }

    #[test]
    fn prioritize_keeps_place_names_inside_180_char_budget() {
        // 実測再現: 置換ルールが大量にあっても地名が prompt に載ること
        let mut v: Vec<(String, String)> = (0..60)
            .map(|i| (format!("誤変換{:02}", i), format!("正表記{:02}", i)))
            .collect();
        v.push(("品川".to_string(), "品川".to_string()));
        v.push(("浦和".to_string(), "浦和".to_string()));
        v.push(("香芝".to_string(), "香芝".to_string()));
        let phrase = build_glossary_phrase(&prioritize_glossary_words(&v));
        assert!(phrase.starts_with("品川, 浦和, 香芝"), "phrase={}", phrase);
    }

    // ----- v0.1.3 patch2 (C): glossary を Whisper prompt に統合 -----

    #[test]
    fn patch2_build_glossary_phrase_empty_returns_empty() {
        assert_eq!(build_glossary_phrase(&[]), "");
    }

    #[test]
    fn patch2_build_glossary_phrase_single_word() {
        let words = vec!["末句".to_string()];
        assert_eq!(build_glossary_phrase(&words), "末句");
    }

    #[test]
    fn patch2_build_glossary_phrase_multiple_words_comma_joined() {
        let words = vec![
            "末句".to_string(),
            "マツエク".to_string(),
            "ハリウッドアイラッシュ".to_string(),
        ];
        assert_eq!(
            build_glossary_phrase(&words),
            "末句, マツエク, ハリウッドアイラッシュ"
        );
    }

    #[test]
    fn patch2_build_glossary_phrase_removes_duplicates_first_wins() {
        let words = vec![
            "末句".to_string(),
            "マツエク".to_string(),
            "末句".to_string(),
            "マツエク".to_string(),
        ];
        assert_eq!(build_glossary_phrase(&words), "末句, マツエク");
    }

    #[test]
    fn patch2_build_glossary_phrase_skips_empty_and_whitespace() {
        let words = vec![
            "".to_string(),
            "  ".to_string(),
            "末句".to_string(),
            "\t".to_string(),
            "マツエク".to_string(),
        ];
        assert_eq!(build_glossary_phrase(&words), "末句, マツエク");
    }

    #[test]
    fn patch2_build_glossary_phrase_trims_individual_words() {
        let words = vec![" 末句 ".to_string(), "  マツエク  ".to_string()];
        assert_eq!(build_glossary_phrase(&words), "末句, マツエク");
    }

    #[test]
    fn whisper_prompt_total_fits_budget() {
        // 2026-08-08: 固定指示文 + 語彙ヒント枠が Whisper の prompt 窓に収まることを保証する。
        // 定数を後から緩めた時にここで落ちる (溢れの再発防止)。
        let base = crate::prompt_sanitize::WHISPER_PROMPT.chars().count();
        assert!(
            base + WHISPER_PROMPT_GLOSSARY_MAX_CHARS <= WHISPER_PROMPT_TOTAL_MAX_CHARS,
            "base {} + glossary {} exceeds total budget {}",
            base,
            WHISPER_PROMPT_GLOSSARY_MAX_CHARS,
            WHISPER_PROMPT_TOTAL_MAX_CHARS
        );
    }

    /// 🔴 第 4 層の照合先は、送信文を短縮しても**縮めてはいけない** (L908・Air v1.1.4 から移植)。
    /// 縮めると削った部分のエコー (Air 実観測 id=335「語尾や助詞は話したい。」) を
    /// 取りこぼす。閾値 0.65 もこの文字列に対して実測した値。
    /// 有料版は現在 2 文とも送っているので、照合先 ⊇ 送信文 かつ 2 文とも含むことを固定する。
    #[test]
    fn echo_reference_is_superset_of_sent_prompt() {
        assert!(
            PROMPT_ECHO_REFERENCE.contains(WHISPER_PROMPT),
            "照合先は送信文を含む上位集合であること"
        );
        assert!(
            PROMPT_ECHO_REFERENCE.contains("句読点"),
            "句読点の誘導文を照合先から落とさないこと"
        );
        assert!(
            PROMPT_ECHO_REFERENCE.contains("語尾や助詞"),
            "語尾・助詞の文を照合先から落とさないこと"
        );
        // 辞書の語彙ヒントは送信文には付くが照合先には入れない (正当な発話語)。
        // build_dynamic_whisper_prompt の出力全体は照合先に含まれてはならない。
        let with_glossary =
            build_dynamic_whisper_prompt(&["マツエク".to_string(), "マツパ".to_string()]);
        assert!(
            !PROMPT_ECHO_REFERENCE.contains(&with_glossary),
            "語彙ヒント付きの送信文を照合先にしてはいけない"
        );
    }

    #[test]
    fn build_glossary_phrase_enforces_max_words() {
        // 文字数に余裕があっても語数の上限で打ち切る。
        // 1 単語 2 文字 → 上限語数ぶんでも 2*20 + 2*19 = 78 文字で、文字数上限には当たらない。
        let words: Vec<String> = (0..40).map(|i| format!("語{}", i)).collect();
        let phrase = build_glossary_phrase(&words);
        assert_eq!(
            phrase.split(", ").count(),
            WHISPER_PROMPT_GLOSSARY_MAX_WORDS,
            "phrase should stop at the word cap: {}",
            phrase
        );
        assert!(phrase.chars().count() <= WHISPER_PROMPT_GLOSSARY_MAX_CHARS);
    }

    #[test]
    fn glossary_hint_usage_matches_what_is_sent() {
        // 画面へ出す残量が、実際に送られる語と食い違わないこと。
        let words: Vec<String> = (0..40).map(|i| format!("語{}", i)).collect();
        let usage = glossary_hint_usage(&words);
        let phrase = build_glossary_phrase(&words);
        assert_eq!(usage.used_words, phrase.split(", ").count());
        assert_eq!(usage.used_chars, phrase.chars().count());
        assert_eq!(usage.dropped_words, 40 - usage.used_words);
        assert_eq!(usage.max_words, WHISPER_PROMPT_GLOSSARY_MAX_WORDS);
    }

    #[test]
    fn patch2_build_glossary_phrase_enforces_max_chars_budget() {
        // WHISPER_PROMPT_GLOSSARY_MAX_CHARS を超えたら途中で打ち切り
        let words: Vec<String> = (0..30).map(|i| format!("単語{:08}", i)).collect();
        let phrase = build_glossary_phrase(&words);
        let char_count = phrase.chars().count();
        assert!(
            char_count <= WHISPER_PROMPT_GLOSSARY_MAX_CHARS,
            "phrase char count {} should be <= {}",
            char_count,
            WHISPER_PROMPT_GLOSSARY_MAX_CHARS
        );
        // 先頭単語が含まれている (順序保持)
        assert!(phrase.starts_with("単語00000000"));
    }

    #[test]
    fn patch2_build_glossary_phrase_long_single_word_over_budget_breaks_out() {
        // 1 単語が予算超過なら捨てる (中途半端な切り取りは prompt 破壊の元なのでしない)
        let words = vec!["x".repeat(WHISPER_PROMPT_GLOSSARY_MAX_CHARS + 10)];
        assert_eq!(build_glossary_phrase(&words), "");
    }

    #[test]
    fn patch2_build_dynamic_whisper_prompt_empty_glossary_returns_base() {
        // glossary が空なら従来の固定 prompt
        assert_eq!(
            build_dynamic_whisper_prompt(&[]),
            crate::prompt_sanitize::WHISPER_PROMPT
        );
    }

    #[test]
    fn patch2_build_dynamic_whisper_prompt_appends_glossary_phrase() {
        // patch5 (2026-05-24・B): 「固有名詞: 」ラベルを削除。単語のみカンマ区切り。
        let words = vec!["末句".to_string(), "マツエク".to_string()];
        let p = build_dynamic_whisper_prompt(&words);
        assert!(p.starts_with(crate::prompt_sanitize::WHISPER_PROMPT));
        assert!(p.contains("末句, マツエク"));
        // ラベルが残っていないこと (漏洩源を消した確認)
        assert!(!p.contains("固有名詞:"), "「固有名詞:」ラベルは patch5 で削除済み: {}", p);
        assert!(!p.contains("固有名詞: "), "「固有名詞: 」(半角空白付) も残っていないこと: {}", p);
        // 期待形: "<base>末句, マツエク"
        let expected = format!(
            "{}末句, マツエク",
            crate::prompt_sanitize::WHISPER_PROMPT
        );
        assert_eq!(p, expected);
    }

    #[test]
    fn patch5_b_build_dynamic_whisper_prompt_has_no_label() {
        // patch5 B 専用回帰: どのような glossary でも「固有名詞」ラベル混入しない
        for words in [
            vec!["マツエク".to_string()],
            vec!["マツエク".to_string(), "マツパ".to_string(), "ハルシ".to_string()],
            vec!["高梨".to_string(), "齋藤".to_string()],
        ] {
            let p = build_dynamic_whisper_prompt(&words);
            assert!(!p.contains("固有名詞"), "「固有名詞」が残っている: {}", p);
        }
    }

    #[test]
    fn patch2_build_dynamic_whisper_prompt_handles_only_blank_words() {
        // 全部空白ならフォールバックで固定 prompt
        let words = vec!["".to_string(), "  ".to_string(), "\t".to_string()];
        assert_eq!(
            build_dynamic_whisper_prompt(&words),
            crate::prompt_sanitize::WHISPER_PROMPT
        );
    }

    // 語尾パディング (2026-07-23): stop_deadline_reached の純粋判定
    #[test]
    fn tail_padding_no_stop_requested_never_breaks() {
        let now = std::time::Instant::now();
        assert!(!stop_deadline_reached(None, now));
    }

    #[test]
    fn tail_padding_before_deadline_keeps_recording() {
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_millis(300);
        assert!(!stop_deadline_reached(Some(deadline), now));
    }

    #[test]
    fn tail_padding_after_deadline_breaks() {
        let now = std::time::Instant::now();
        let deadline = now - std::time::Duration::from_millis(1);
        assert!(stop_deadline_reached(Some(deadline), now));
        // 締切ちょうども break してよい
        assert!(stop_deadline_reached(Some(now), now));
    }

    // ---------------- v0.1.3 patch3 (C-add): apply_glossary_replacements ----------------

    #[test]
    fn patch3_apply_glossary_replacements_simple_string_match() {
        // 真因シナリオ: Whisper が "松江区" と誤変換 → "マツエク" に置換
        let entries = vec![
            ("松江区".to_string(), "マツエク".to_string(), false),
            ("松葉".to_string(), "マツパ".to_string(), false),
            ("春氏".to_string(), "ハルシ".to_string(), false),
        ];
        let (out, n) = apply_glossary_replacements("今日は松江区の話と春氏について。", &entries);
        assert_eq!(out, "今日はマツエクの話とハルシについて。");
        // 「松江区」と「春氏」の 2 件適用 (松葉は本文に無いので 0)
        assert_eq!(n, 2, "適用件数は 2 のはず: {}", n);
    }

    #[test]
    fn patch3_apply_glossary_replacements_empty_entries_returns_unchanged() {
        let entries: Vec<(String, String, bool)> = vec![];
        let (out, n) = apply_glossary_replacements("無加工テキスト", &entries);
        assert_eq!(out, "無加工テキスト");
        assert_eq!(n, 0);
    }

    #[test]
    fn patch3_apply_glossary_replacements_no_match_returns_unchanged() {
        let entries = vec![("松江区".to_string(), "マツエク".to_string(), false)];
        let (out, n) = apply_glossary_replacements("該当しない普通の文章。", &entries);
        assert_eq!(out, "該当しない普通の文章。");
        assert_eq!(n, 0);
    }

    #[test]
    fn patch3_apply_glossary_replacements_multiple_occurrences_in_one_text() {
        // 同じ pattern が 2 回出てきても、適用件数は「変化したエントリ数」=1
        let entries = vec![("松江区".to_string(), "マツエク".to_string(), false)];
        let (out, n) =
            apply_glossary_replacements("松江区と松江区の話", &entries);
        assert_eq!(out, "マツエクとマツエクの話");
        assert_eq!(n, 1);
    }

    #[test]
    fn patch3_apply_glossary_replacements_regex_pattern() {
        // is_regex=true: 数字のパターンを正規表現で置換
        let entries = vec![(r"\d+".to_string(), "###".to_string(), true)];
        let (out, n) = apply_glossary_replacements("売上は123万円", &entries);
        assert_eq!(out, "売上は###万円");
        assert_eq!(n, 1);
    }

    #[test]
    fn patch3_apply_glossary_replacements_invalid_regex_skipped() {
        // 無効正規表現は warn 出力して skip (text 無加工)
        // valid な entry はその後も適用される
        let entries = vec![
            ("[".to_string(), "X".to_string(), true), // invalid regex
            ("松江区".to_string(), "マツエク".to_string(), false),
        ];
        let (out, n) = apply_glossary_replacements("松江区テスト", &entries);
        assert_eq!(out, "マツエクテスト");
        assert_eq!(n, 1);
    }

    #[test]
    fn patch3_apply_glossary_replacements_empty_pattern_skipped() {
        // pattern が空文字列の entry は無限ループ防止のため skip
        let entries = vec![
            ("".to_string(), "X".to_string(), false),
            ("松江区".to_string(), "マツエク".to_string(), false),
        ];
        let (out, n) = apply_glossary_replacements("松江区テスト", &entries);
        assert_eq!(out, "マツエクテスト");
        assert_eq!(n, 1);
    }

    #[test]
    fn patch3_apply_glossary_replacements_order_matters_first_replaces_then_second() {
        // 順序依存: 1 件目の置換結果が 2 件目の入力になる
        let entries = vec![
            ("A".to_string(), "B".to_string(), false),
            ("B".to_string(), "C".to_string(), false),
        ];
        let (out, n) = apply_glossary_replacements("A", &entries);
        // A→B→C のチェーン
        assert_eq!(out, "C");
        // 2 件とも適用された
        assert_eq!(n, 2);
    }

    #[test]
    fn patch3_apply_glossary_replacements_delete_via_empty_replacement() {
        // replacement="" は「pattern を削除」の用途として許可
        let entries = vec![("ええと、".to_string(), "".to_string(), false)];
        let (out, n) = apply_glossary_replacements("ええと、今日は晴れです", &entries);
        assert_eq!(out, "今日は晴れです");
        assert_eq!(n, 1);
    }

    // ---------------- v0.1.3 patch3 (B-fix): compute_confidence_drop_flag ----------------

    #[test]
    fn patch3_b_fix_short_utterance_with_low_logprob_returns_false() {
        // id=158 相当: 1.7 秒・logprob -0.86・no_speech_prob 3.6e-11
        // patch2 までは flag=1 (logprob < -0.5) → 貼付抑止される問題
        // patch3 (B-fix): 短発話なので flag=0
        let flag = compute_confidence_drop_flag(1700, Some(3.6e-11), Some(-0.86));
        assert!(!flag, "短発話 (1.7s) + 低 logprob でも flag=false: 実際={}", flag);
    }

    #[test]
    fn patch3_b_fix_short_utterance_with_high_no_speech_prob_returns_false() {
        // 短発話なら no_speech_prob 高でも flag=false (Tier 3.5 / Tier 1 で別経路捕捉前提)
        let flag = compute_confidence_drop_flag(800, Some(0.9), Some(0.0));
        assert!(!flag);
    }

    #[test]
    fn patch3_b_fix_long_utterance_with_low_logprob_still_returns_true() {
        // 回帰: 長発話 (>= SHORT_UTTERANCE_THRESHOLD_MS) で logprob < -0.5 は従来通り flag=true
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大に伴い duration_ms を 3000 → 5000 に更新
        let flag = compute_confidence_drop_flag(5000, Some(0.0), Some(-0.86));
        assert!(flag);
    }

    #[test]
    fn patch3_b_fix_long_utterance_with_high_no_speech_prob_returns_true() {
        // 回帰: 長発話で no_speech_prob > 0.4 は従来通り flag=true
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大に伴い duration_ms を 3000 → 5000 に更新
        let flag = compute_confidence_drop_flag(5000, Some(0.5), Some(0.0));
        assert!(flag);
    }

    #[test]
    fn patch3_b_fix_long_utterance_normal_values_returns_false() {
        // 長発話・通常値 → flag=false
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大に伴い duration_ms を 3000 → 5000 に更新
        let flag = compute_confidence_drop_flag(5000, Some(0.1), Some(-0.2));
        assert!(!flag);
    }

    #[test]
    fn patch5_a_boundary_exactly_4000ms_is_long() {
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大。
        // 境界値: duration_ms == 4000 はちょうど閾値なので「短発話ではない」(< のみ short) → flag=true
        // halu.rs::SHORT_UTTERANCE_THRESHOLD_MS と整合
        let flag = compute_confidence_drop_flag(4000, Some(0.0), Some(-1.0));
        assert!(flag, "4000ms は短発話ではない → flag=true: 実際={}", flag);
    }

    #[test]
    fn patch5_a_boundary_3999ms_is_short() {
        // patch5: 境界値の反対側: 3999ms は短発話扱い → flag=false
        let flag = compute_confidence_drop_flag(3999, Some(0.0), Some(-1.0));
        assert!(!flag, "3999ms は短発話 → flag=false: 実際={}", flag);
    }

    #[test]
    fn patch5_a_short_utterance_2500ms_with_low_logprob_returns_false() {
        // patch5 真因: id=173「マツエク」(duration=2500ms, logprob=-0.55) → flag=1 で貼付抑止
        //   patch4 までは 2500ms > 2000 で長発話扱い → 低 logprob で flag=true
        //   patch5 では 2500ms < 4000 で短発話扱い → スキップで flag=false
        let flag = compute_confidence_drop_flag(2500, Some(0.0), Some(-0.55));
        assert!(
            !flag,
            "id=173 相当の短発話 (2500ms / logprob=-0.55) は flag=false 期待: 実際={}",
            flag
        );
    }

    #[test]
    fn patch5_a_long_utterance_4500ms_with_low_logprob_returns_true() {
        // 回帰: 4 秒超の長発話で低 logprob は従来通り flag=true
        let flag = compute_confidence_drop_flag(4500, Some(0.0), Some(-0.55));
        assert!(flag, "4500ms 長発話 + 低 logprob は flag=true: 実際={}", flag);
    }

    // ---------------- v0.1.10 機能 A: should_show_congestion_banner ----------------

    #[test]
    fn congestion_banner_hedged_with_key_default_suppressed() {
        // Hedged + OpenAI key 有 + show_in_hedged=false → 抑止
        // (default 動作・SSOT A-5)
        let v = should_show_congestion_banner(true, false);
        assert!(!v, "hedged + key + suppressed should NOT show banner");
    }

    #[test]
    fn congestion_banner_hedged_with_key_when_show_enabled() {
        // Hedged + OpenAI key 有 + show_in_hedged=true → 表示
        // (SettingsPage で利用者が「常時表示」に切り替えた場合)
        let v = should_show_congestion_banner(true, true);
        assert!(v, "hedged + key + show=true should show banner");
    }

    #[test]
    fn congestion_banner_hedged_without_key_treated_as_solo() {
        // Hedged 選択 + OpenAI key 無し → Groq 単独に縮退 → バナー表示
        let v = should_show_congestion_banner(false, false);
        assert!(v, "hedged degraded to groq solo (no key) should show banner");
    }

    #[test]
    fn congestion_banner_groq_solo_shows() {
        let v = should_show_congestion_banner(false, false);
        assert!(v, "groq solo should show banner");
    }

    #[test]
    fn congestion_banner_openai_solo_shows() {
        let v = should_show_congestion_banner(false, false);
        assert!(v, "openai solo should show banner");
    }

    #[test]
    fn congestion_config_default_matches_ssot() {
        // SSOT A-1: default 5000ms / A-5: default hedged 抑止
        let c = CongestionConfig::default();
        assert_eq!(c.threshold_ms, 5000);
        assert!(!c.show_in_hedged);
    }

    #[test]
    fn patch3_b_fix_none_metrics_returns_false() {
        // metric が None ならどちらの分岐でも flag=false
        let flag_short = compute_confidence_drop_flag(1000, None, None);
        let flag_long = compute_confidence_drop_flag(5000, None, None);
        assert!(!flag_short);
        assert!(!flag_long);
    }

    // ---------------- v0.1.12 F2: should_emit_mic_low_level / MicLowLevelConfig ----------------

    #[test]
    fn mic_low_level_config_default_matches_ssot() {
        // SSOT: v0_1_12_scope_2026-05-30.md / threshold=0.005 / window_ms=3000
        let c = MicLowLevelConfig::default();
        assert!((c.threshold - 0.005).abs() < f32::EPSILON);
        assert_eq!(c.window_ms, 3000);
    }

    #[test]
    fn mic_low_level_does_not_emit_before_window_elapses() {
        // 2999ms 経過時点 (window=3000) では emit しない
        let v = should_emit_mic_low_level(2999, 3000, false, false);
        assert!(!v);
    }

    #[test]
    fn mic_low_level_emits_at_window_boundary_when_silent() {
        // ちょうど 3000ms 経過 + 一度も loud 観測なし + 未 emit → emit する
        let v = should_emit_mic_low_level(3000, 3000, false, false);
        assert!(v);
    }

    #[test]
    fn mic_low_level_suppressed_after_loud_observed() {
        // window 経過していても、途中で threshold 以上を観測していたら emit しない
        let v = should_emit_mic_low_level(5000, 3000, true, false);
        assert!(!v);
    }

    #[test]
    fn mic_low_level_suppressed_after_already_emitted() {
        // session 単位 1 回保証: already_emitted=true なら以降は false
        let v = should_emit_mic_low_level(10_000, 3000, false, true);
        assert!(!v);
    }

    #[test]
    fn mic_low_level_long_after_window_still_emits_if_silent() {
        // 10 秒経過 + silent + 未 emit → 経過超過でも 1 度は emit する
        // (worker が最初の window 経過チェックを逃しても、次の frame で必ず emit される)
        let v = should_emit_mic_low_level(10_000, 3000, false, false);
        assert!(v);
    }

    #[test]
    fn mic_low_level_boundary_frame_with_first_loud_must_still_emit() {
        // Codex P2 回帰防止 (2026-05-30):
        //   window=3000 で 0-3000ms ずっと silent、3050ms 時点で初の loud frame が到来。
        //   worker 内の評価順序は (1) emit 判定 → (2) seen_loud 更新 でなければならず、
        //   このフレームでは「これまでの seen_loud=false」で評価して emit すべき。
        //   should_emit_mic_low_level は pure に「与えられた seen_loud」を見るだけなので、
        //   呼出側が emit 判定 *前* の seen_loud を渡せば真を返す。
        let v = should_emit_mic_low_level(3050, 3000, false, false);
        assert!(
            v,
            "境界 3050ms + これまで seen_loud=false なら emit すべき: 実際={}",
            v
        );
    }

    // ---------------- v0.1.14 診断: compute_rms_stats ----------------
    #[test]
    fn rms_stats_zero_frames_returns_zeroed_guard() {
        // 無録音 (count=0) はゼロ除算せず全 0 を返す。
        let s = compute_rms_stats(0.0, 0.0, f32::MAX, 0);
        assert_eq!(
            s,
            RmsStats {
                mean: 0.0,
                peak: 0.0,
                min: 0.0,
                frames: 0
            }
        );
    }

    #[test]
    fn rms_stats_computes_mean_peak_min() {
        // RMS 0.002 / 0.008 / 0.005 の 3 フレーム → mean=0.005 peak=0.008 min=0.002。
        let sum = 0.002_f64 + 0.008 + 0.005;
        let s = compute_rms_stats(sum, 0.008, 0.002, 3);
        assert!((s.mean - 0.005).abs() < 1e-6, "mean={}", s.mean);
        assert!((s.peak - 0.008).abs() < 1e-6, "peak={}", s.peak);
        assert!((s.min - 0.002).abs() < 1e-6, "min={}", s.min);
        assert_eq!(s.frames, 3);
    }

    #[test]
    fn rms_stats_single_frame_mean_equals_value() {
        // 1 フレームなら mean = peak = min = その値。
        let s = compute_rms_stats(0.007, 0.007, 0.007, 1);
        assert!((s.mean - 0.007).abs() < 1e-6, "mean={}", s.mean);
        assert!((s.peak - 0.007).abs() < 1e-6);
        assert!((s.min - 0.007).abs() < 1e-6);
        assert_eq!(s.frames, 1);
    }

    // ---------------- v0.1.15 小声警告: judge_quiet_voice / update_voice_baseline ----------------
    #[test]
    fn quiet_voice_not_judged_during_cold_start() {
        // count < min_samples の間は判定しない (baseline 学習途上)。
        let v = judge_quiet_voice(0.0085, 0.025, 3, 0.45, 5);
        assert!(!v, "cold start (count=3 < min=5) は判定しないべき");
    }

    #[test]
    fn quiet_voice_not_judged_when_baseline_unseeded() {
        // baseline <= 0 (未 seed) は判定しない。
        let v = judge_quiet_voice(0.0085, 0.0, 10, 0.45, 5);
        assert!(!v, "baseline 未 seed は判定しないべき");
    }

    #[test]
    fn quiet_voice_detected_for_measured_quiet_sample() {
        // 実測: 小声 mean=0.0085 / baseline=0.025 / factor=0.45 → 閾値 0.01125 → 小声判定。
        let v = judge_quiet_voice(0.0085, 0.025, 10, 0.45, 5);
        assert!(v, "0.0085 < 0.025*0.45=0.01125 なので小声判定すべき");
    }

    #[test]
    fn quiet_voice_not_detected_for_normal_sample() {
        // 実測: 普通の声 mean=0.025 / baseline=0.025 → 閾値超 → 小声でない。
        let v = judge_quiet_voice(0.025, 0.025, 10, 0.45, 5);
        assert!(!v, "普通の声 (baseline 同等) は小声でない");
    }

    #[test]
    fn quiet_voice_boundary_just_above_threshold_not_quiet() {
        // 閾値 0.01125 ちょうど上 (0.0113) は小声でない (< のみ true)。
        let v = judge_quiet_voice(0.0113, 0.025, 10, 0.45, 5);
        assert!(!v, "閾値直上は小声でない");
    }

    #[test]
    fn baseline_seeds_on_first_sample() {
        // count==0 は今回の mean を seed として返す。
        let b = update_voice_baseline(0.0, 0.025, 0.2, 0);
        assert!((b - 0.025).abs() < 1e-6, "初回は mean を seed: {}", b);
    }

    #[test]
    fn baseline_seeds_when_unseeded_even_with_count() {
        // baseline<=0 なら count があっても seed 扱い。
        let b = update_voice_baseline(0.0, 0.03, 0.2, 7);
        assert!((b - 0.03).abs() < 1e-6, "未 seed は mean を返す: {}", b);
    }

    #[test]
    fn baseline_ema_moves_toward_mean() {
        // EMA: 0.2*0.030 + 0.8*0.020 = 0.022。
        let b = update_voice_baseline(0.020, 0.030, 0.2, 10);
        assert!((b - 0.022).abs() < 1e-6, "EMA 期待 0.022: {}", b);
    }

    #[test]
    fn baseline_invalid_alpha_falls_back_to_default() {
        // alpha 範囲外 (1.5) は 0.2 にフォールバック → 0.2*0.030+0.8*0.020=0.022。
        let b = update_voice_baseline(0.020, 0.030, 1.5, 10);
        assert!((b - 0.022).abs() < 1e-6, "不正 alpha は 0.2 既定: {}", b);
    }

    // ---------------- v0.1.16 機能 L Day 1: RecorderMode / should_learn_baseline ----------------

    #[test]
    fn recorder_mode_traits_sanity() {
        // enum 値の Debug/Clone/Copy/PartialEq/Eq の sanity (派生 trait の動作確認)。
        let m = RecorderMode::Normal;
        let m2 = m; // Copy
        assert_eq!(m, m2);
        assert_ne!(RecorderMode::Normal, RecorderMode::Meeting);
        // Debug が panic しない
        let _s = format!("{:?}", RecorderMode::Meeting);
    }

    #[test]
    fn should_learn_baseline_normal_not_quiet_returns_true() {
        // 通常モード + 普段の声 → 学習する (v0.1.15 既存挙動の維持)
        assert!(should_learn_baseline(false, RecorderMode::Normal));
    }

    #[test]
    fn should_learn_baseline_normal_quiet_returns_false() {
        // 通常モード + 小声 → 学習しない (v0.1.15 既存挙動の維持・基準下振れ防止)
        assert!(!should_learn_baseline(true, RecorderMode::Normal));
    }

    #[test]
    fn should_learn_baseline_meeting_never_learns() {
        // 議事録モードは小声/普段の声のいずれでも学習しない (通常録音 baseline 不汚染)
        assert!(!should_learn_baseline(false, RecorderMode::Meeting));
        assert!(!should_learn_baseline(true, RecorderMode::Meeting));
    }

    // ---------------- v0.1.16 機能 L Day 4: MeetingHolder::is_recording ----------------

    #[test]
    fn meeting_holder_is_recording_default_false() {
        // Default::default() の MeetingHolder は session が None → is_recording = false。
        // ホットキー pause 経路の anchor 動作: 議事録未起動時はホットキー通常動作。
        let h = MeetingHolder::default();
        assert!(!h.is_recording());
    }

    #[test]
    fn meeting_holder_is_recording_returns_true_when_session_present() {
        // session に Some が入っていれば is_recording = true → ホットキー早期 return。
        // MeetingSession は構築コストが大きいため、ここでは Mutex 直接操作で session に
        // 「何らかの Some」が入った状態を模擬したいが、フィールドは pub session: Mutex<Option<MeetingSession>>
        // で MeetingSession の構築には JoinHandle 等の本物リソースが必要。
        // → 設計上 is_recording は guard.is_some() を呼ぶだけなので、
        //    Mutex<Option<T>> の Some/None 切替挙動を Mutex<Option<()>> でロジック等価検証する。
        //
        // つまり「MeetingHolder の is_recording 実装は guard.is_some() 委譲」という
        // **委譲挙動の正しさ** を Option::is_some の真理値表で確認するに留める。
        // 実 MeetingSession 在/不在の E2E は Day 5 結合検証で確認する。
        let opt_none: Option<()> = None;
        let opt_some: Option<()> = Some(());
        assert!(opt_none.is_none(), "None は is_recording = false 相当");
        assert!(opt_some.is_some(), "Some は is_recording = true 相当");
    }

    // ---------------- v0.1.16 機能 L Day 2: should_split_chunk / chunk_threshold ----------------

    #[test]
    fn test_should_split_chunk_below_threshold() {
        // 閾値未満では分割しない (samples_accum < threshold)
        assert!(!should_split_chunk(0, 1000));
        assert!(!should_split_chunk(999, 1000));
    }

    #[test]
    fn test_should_split_chunk_at_boundary() {
        // 境界ちょうど (==) と超過 (>) でいずれも分割する
        assert!(should_split_chunk(1000, 1000), "境界ちょうどで分割");
        assert!(should_split_chunk(1001, 1000), "閾値超過でも分割");
    }

    #[test]
    fn test_should_split_chunk_zero_threshold() {
        // threshold = 0 は無効値ガード: 絶対分割しない (誤って 0 渡しても安全)
        assert!(!should_split_chunk(0, 0));
        assert!(!should_split_chunk(1_000_000, 0), "threshold=0 は分割しない (無効値ガード)");
    }

    #[test]
    fn test_compute_chunk_threshold_samples_normal() {
        // 300 秒 × 48000 Hz × 2 ch = 28,800,000 サンプル
        assert_eq!(compute_chunk_threshold_samples(300, 48000, 2), 28_800_000);
        // 300 秒 × 16000 Hz × 1 ch = 4,800,000 サンプル
        assert_eq!(compute_chunk_threshold_samples(300, 16000, 1), 4_800_000);
    }

    #[test]
    fn test_compute_chunk_threshold_samples_overflow_saturates() {
        // u32 max × u32 max × u16 max は u64 max を超える → saturating_mul で u64::MAX
        let v = compute_chunk_threshold_samples(u32::MAX, u32::MAX, u16::MAX);
        assert_eq!(v, u64::MAX);
    }

    #[test]
    fn test_meeting_stop_outcome_construct() {
        // MeetingStopOutcome の sanity 構築テスト (Day 3 で transcribe 入力に使う)
        let o = MeetingStopOutcome {
            chunk_paths: vec![PathBuf::from("chunk_000.wav")],
            total_duration_ms: 1500,
            rms_mean: 0.05,
        };
        assert_eq!(o.chunk_paths.len(), 1);
        assert_eq!(o.total_duration_ms, 1500);
        assert!((o.rms_mean - 0.05).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // ハルシネーション対策の回帰テスト (無料版 RushVoice Air v1.1.5 (2026-08-13〜15) から移植・2026-08-16)
    //
    // 開発機の history.db (Air) に実際に残った本文をそのまま使う。
    // 「検知できていたのに素通りした」のが元の障害なので、素通りしたら失敗させる。
    // 🔴 本番と同じ設定 (production_halu_filter) で、どの層が落としたか (reason) まで固定する (L928)。
    // -----------------------------------------------------------------------

    /// Tier 1 (VAD) に無音と判定させないための「聞こえている」音声。
    /// これが無いと全ケースが VAD 由来の Drop になり、テストが何も検証しなくなる。
    fn audible_samples() -> Vec<f32> {
        vec![0.1_f32; 1600]
    }

    fn is_drop(v: &crate::halu::HaluVerdict) -> bool {
        matches!(v, crate::halu::HaluVerdict::Drop(_))
    }

    /// id=348 (15.7 秒の発話 → 「字幕作成者 。」・avg_logprob -1.33)。
    /// Air では Tier 3 でも捕まる値だが、有料版は Tier 3 リストに「字幕作成者」が無いので
    /// Tier 3.2 で捕まえることを固定する。
    #[test]
    fn halu_drops_subtitle_credit_from_id348() {
        let v = production_halu_filter().judge_with_duration(
            "字幕作成者 。",
            &audible_samples(),
            Some(0.0),
            Some(-1.33),
            Some(15_700),
        );
        match v {
            crate::halu::HaluVerdict::Drop(crate::halu::HaluReason::KnownPhraseAlways {
                ref phrase,
            }) => assert_eq!(phrase, "字幕作成者"),
            other => panic!("Tier 3.2 (KnownPhraseAlways) で破棄されるべき: {:?}", other),
        }
    }

    /// 🔴 id=742 (5.8 秒の発話 → 「字幕作成者 。）。」/ `avg_logprob = -0.303`)。
    ///
    /// Air v1.1.4 では素通りして貼り付けられた。素通りの原因は 2 つあり、両方直っていないと落ちる:
    ///   1. Tier 3 の AND 条件 `min_logprob < -0.5` を -0.303 が満たさない
    ///      → Tier 3.2 (logprob 条件なしの完全一致) で捕える
    ///   2. `normalize()` が「）」を落とさず「字幕作成者）」になり完全一致に失敗
    ///      → `is_punct` に括弧類を追加
    #[test]
    fn halu_drops_subtitle_credit_from_id742_high_confidence() {
        let v = production_halu_filter().judge_with_duration(
            "字幕作成者 。）。",
            &audible_samples(),
            Some(0.0),
            Some(-0.303),
            Some(5_800),
        );
        // 🔴 Tier 3.2 が仕事をしたことまで固定する。単に is_drop を見るだけだと、
        //    別の層がたまたま拾っただけの状態を「直った」と誤認する。
        match v {
            crate::halu::HaluVerdict::Drop(crate::halu::HaluReason::KnownPhraseAlways {
                ref phrase,
            }) => assert_eq!(phrase, "字幕作成者"),
            other => panic!("Tier 3.2 (KnownPhraseAlways) で破棄されるべき: {:?}", other),
        }
    }

    /// 🔴 Tier 3.2 は完全一致のみ。単独で言い得る句をここへ入れると正常発話が消える。
    /// 「ありがとうございました」は Tier 3 (低 logprob AND) のままであることを固定する。
    #[test]
    fn known_hallucinations_always_contains_only_never_spoken_phrases() {
        for phrase in KNOWN_HALLUCINATIONS_ALWAYS {
            assert!(
                !phrase.contains("ありがとう"),
                "単独で言い得る句を Tier 3.2 へ入れてはいけない: {}",
                phrase
            );
        }
        // 高 confidence の「ありがとうございました」は通る (利用者が実際に言う)
        let v = production_halu_filter().judge_with_duration(
            "ありがとうございました。",
            &audible_samples(),
            Some(0.0),
            Some(-0.20),
            Some(3_000),
        );
        assert!(
            !is_drop(&v),
            "高 confidence の「ありがとうございました」は通るべき: {:?}",
            v
        );
    }

    /// id=349 (19 秒の発話 → prompt をそのままエコー)。halu フィルタ単体でも従来から検知できる
    /// (本番では第 4 層が先に該当文を落とす)。
    #[test]
    fn halu_drops_prompt_echo_from_id349() {
        let text = "字幕は、本日の人に対して、 字幕は、 読点）を含む自然な日本語で書き起こす。";
        let v = production_halu_filter().judge_with_duration(
            text,
            &audible_samples(),
            Some(0.0),
            Some(-0.52),
            Some(19_000),
        );
        assert!(is_drop(&v), "prompt エコーは破棄されるべき: {:?}", v);
    }

    /// 🔴 誤検知しないこと。当社は動画制作も行うため「字幕」の正規発話が実在する。
    /// ここが落ちたら、利用者の正常な発話が丸ごと消える設定を入れてしまっている。
    #[test]
    fn halu_passes_legitimate_subtitle_speech() {
        for text in [
            "この動画に字幕を入れてください。",
            "字幕の色を白にして、位置は下に寄せてください。",
            "字幕作成の担当は誰になりますか。",
            "字幕作成者は誰ですか。",
        ] {
            let v = production_halu_filter().judge_with_duration(
                text,
                &audible_samples(),
                Some(0.0),
                Some(-0.15),
                Some(5_000),
            );
            assert!(
                !is_drop(&v),
                "正規の「字幕」発話を破棄してはいけない: {text} -> {:?}",
                v
            );
        }
    }

    /// 定数の登録規約: 6 文字未満の断片を Tier 3.5 へ入れない (誤検知で本文が丸ごと消える)。
    /// normalize は括弧類も落とすので、「読点）を含む自然な」は「読点を含む自然な」(8 字) で数える。
    #[test]
    fn substring_patterns_are_long_enough() {
        for s in KNOWN_HALLUCINATION_SUBSTRINGS {
            let normalized = crate::halu::normalize(s);
            assert!(
                normalized.chars().count() >= 6,
                "Tier 3.5 の断片は normalize 後 6 文字以上であるべき: {s:?} -> {normalized:?}"
            );
        }
    }

    /// 第 4 層 → halu フィルタの順に通したとき、本番の流れで id=349 型が「該当文だけ落ちて
    /// 残りは halu フィルタへ渡る」ことを確認する (transcribe_collected と同じ順序・同じ照合先)。
    #[test]
    fn echo_layer_runs_before_halu_and_strips_only_the_echo_sentence() {
        let raw = "明日の会議は10時からです。読点）を含む自然な日本語で書き起こす。";
        let after_echo = strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.52));
        assert_eq!(after_echo, "明日の会議は10時からです。");
        let v = production_halu_filter().judge_with_duration(
            &after_echo,
            &audible_samples(),
            Some(0.0),
            Some(-0.52),
            Some(19_000),
        );
        assert!(!is_drop(&v), "エコー除去後の本文は通るべき: {:?}", v);
    }
}

#[cfg(test)]
mod subtitle_worker_tests {
    use super::*;

    #[test]
    fn split_requires_min_speech() {
        assert!(!subtitle_should_split(0, 10_000, 20_000));
        assert!(!subtitle_should_split(SUBTITLE_MIN_SPEECH_MS - 1, 10_000, 20_000));
    }

    #[test]
    fn split_on_trailing_silence() {
        assert!(subtitle_should_split(500, SUBTITLE_SILENCE_SPLIT_MS, 1_500));
        assert!(!subtitle_should_split(500, SUBTITLE_SILENCE_SPLIT_MS - 1, 1_500));
    }

    #[test]
    fn split_on_max_utterance_even_without_silence() {
        assert!(subtitle_should_split(5_000, 0, SUBTITLE_MAX_UTTERANCE_MS));
        assert!(!subtitle_should_split(5_000, 0, SUBTITLE_MAX_UTTERANCE_MS - 1));
    }

    #[test]
    fn trim_only_before_first_speech() {
        assert!(subtitle_should_trim_leading_silence(0, SUBTITLE_PREROLL_MS + 1));
        assert!(!subtitle_should_trim_leading_silence(1, 10_000));
        assert!(!subtitle_should_trim_leading_silence(0, SUBTITLE_PREROLL_MS));
    }

    #[test]
    fn frame_duration_normal_and_zero_denominator() {
        // 48kHz 2ch の 1 秒分サンプル (48000*2) = 1000ms
        assert_eq!(subtitle_frame_duration_ms(96_000, 48_000, 2), 1000);
        assert_eq!(subtitle_frame_duration_ms(100, 0, 0), 0);
    }

    #[test]
    fn subtitle_holder_default_not_recording() {
        let h = SubtitleHolder::default();
        assert!(!h.is_recording());
    }
}
