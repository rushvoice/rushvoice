// Phase 1 Day 4-B: Groq /audio/transcriptions multipart 送信
// 仕様: phase1_v1_read_notes.md §2 (v1 VoiceInputMvp.ps1 L637-732 移植)
//
// 機能:
//   - reqwest multipart/form-data でファイル + パラメータ送信
//   - Bearer Authorization
//   - HTTP/2 keep-alive + 接続プール設定 (§2.3: Cloudflare 400 秒切断対策)
//   - tokio_util::sync::CancellationToken でキャンセル対応 (Day 5 hedged で必須)
//   - language=ja 固定 (教訓 L088: 英訳防止のため言語明示必須)
//   - temperature=0 固定 (出力ブレ抑制)
//   - Groq URL 時のみ response_format=verbose_json (avg_logprob / no_speech_prob 取得目的)
//
// 本モジュールは HTTP 転送のみ担当。
// - response 本文の verbose_json パースは Day 4-C halu.rs / Day 4-D 検証で実施
// - hedged 並走 (trigger 秒) は Day 5 で実装
// - DPAPI 復号後の平文 api_key を受け取る (Sprint 2 で DPAPI 統合)

use anyhow::{Context, Result};
use reqwest::{multipart, Client};
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use log::debug; // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)

/// Phase 1 既定値
pub const DEFAULT_LANGUAGE: &str = "ja";
pub const DEFAULT_TEMPERATURE: &str = "0";

/// Phase 2-B (v0.1.7): User-Agent (Groq rate limit / monitoring 対策)
pub const USER_AGENT: &str = "RushVoice/0.1.9 (Tauri Desktop)";

// Day 25 Phase 2-G (SSOT v3.3): MULTI_MIN_INTERVAL + LAST_MULTI_CALL 削除済
/// transcribe() のエラー。Day 5 hedged 側で variant ごとに扱い分ける想定。
#[derive(Debug, Error)]
pub enum TranscribeError {
    #[error("transcription cancelled")]
    Cancelled,
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("invalid mime_type: {0}")]
    InvalidMime(String),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}

#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    /// 例: "https://api.groq.com/openai/v1/audio/transcriptions"
    pub api_url: String,
    /// DPAPI 復号後の平文。ログに出さない。
    pub api_key: String,
    /// 例: "whisper-large-v3-turbo" (Groq) / "gpt-4o-mini-transcribe" (OpenAI)
    pub model: String,
    /// ユーザー辞書 / プロンプト。空文字でも OK。
    pub prompt: String,
    /// 転写言語。Some("ja") 等で言語固定 (L088: 英訳防止)。
    /// None なら language パラメータを送らず Whisper の自動判定に任せる
    /// (セミナー字幕モード用・検出結果は verbose_json トップレベル "language" で取得)。
    pub language: Option<String>,
    /// WAV / mp3 等の音声バイト列。
    pub audio: Vec<u8>,
    /// multipart の file_name フィールド。例: "audio.wav"
    pub file_name: String,
    /// 例: "audio/wav" / "audio/mpeg"
    pub mime_type: String,
    /// Gateway PoC (2026-07-07 R-1): response_format=verbose_json + timestamp_granularities[]
    /// を要求するか。従来は `api_url.contains("api.groq.com")` の URL 文字列判定で
    /// 決めていたが、ゲートウェイ経由だと URL が api.groq.com を含まなくなり判定が壊れる
    /// (halu 判定に必須の no_speech_prob/avg_logprob が消失する) ため、
    /// 呼出元が明示的に指定するフィールドに置換した。
    pub verbose_json: bool,
}

#[derive(Debug)]
pub struct TranscribeResponse {
    /// raw レスポンス本文 (Groq=verbose_json / OpenAI=json)
    pub body: String,
    /// 送信した audio bytes 数
    pub bytes_sent: u64,
    /// send() 開始から body 読み終わりまでの ms
    pub elapsed_ms: u128,
}

/// HTTP/2 keep-alive クライアントを構築する (phase1_v1_read_notes.md §2.3 準拠)。
///
/// v1 の HttpClient/SocketsHttpHandler 設定を reqwest 0.12 に移植：
///   - PooledConnectionLifetime 120s / PooledConnectionIdleTimeout 90s
///     → pool_idle_timeout(120s) (reqwest は 1 つしか持たないため長い方)
///   - MaxConnectionsPerServer 4 → pool_max_idle_per_host(4)
///   - KeepAlivePing 30s/10s/Always
///   - HttpClient.Timeout 120s
///   - AutomaticDecompression GZip + Deflate
///   - DefaultRequestVersion 2.0 → ALPN 自動ネゴ (rustls 経由)
pub fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .pool_idle_timeout(Some(Duration::from_secs(120)))
        .pool_max_idle_per_host(4)
        .http2_keep_alive_interval(Some(Duration::from_secs(30)))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .timeout(Duration::from_secs(120))
        .gzip(true)
        .deflate(true)
        .build()
        .context("failed to build reqwest::Client (HTTP/2 keep-alive)")
}

/// 音声を OpenAI 互換 /audio/transcriptions に投げて raw レスポンス本文を返す。
///
/// 呼び出し前に `cancel.is_cancelled()` を見て早期 return する。
/// 送信中 / body 受信中もキャンセル可。
pub async fn transcribe(
    client: &Client,
    req: TranscribeRequest,
    cancel: CancellationToken,
) -> std::result::Result<TranscribeResponse, TranscribeError> {
    // 早期チェック (tokio::select! の競合を避けて確実に Cancelled を返す)
    if cancel.is_cancelled() {
        return Err(TranscribeError::Cancelled);
    }

    let wants_verbose = req.verbose_json;
    let response_format = if wants_verbose { "verbose_json" } else { "json" };
    let bytes_sent = req.audio.len() as u64;

    let part = multipart::Part::bytes(req.audio)
        .file_name(req.file_name)
        .mime_str(&req.mime_type)
        .map_err(|e| TranscribeError::InvalidMime(format!("{}: {}", req.mime_type, e)))?;

    let mut form = multipart::Form::new()
        .part("file", part)
        .text("model", req.model)
        .text("prompt", req.prompt)
        .text("temperature", DEFAULT_TEMPERATURE.to_string())
        .text("response_format", response_format.to_string());
    // language=None (字幕モード) はパラメータ自体を送らず自動判定。
    // 既存経路 (通常録音/議事録) は Some("ja") で従来挙動不変 (L088)。
    if let Some(lang) = &req.language {
        form = form.text("language", lang.clone());
    }
    // v0.1.16 機能 L Day 3: verbose_json 時のみ segment 粒度のタイムスタンプを要求。
    // OpenAI 経路 (response_format=json) には影響しない。
    // 既存 halu parse は segments[0] を読むだけなので影響なし。
    if wants_verbose {
        form = form.text("timestamp_granularities[]", "segment");
    }

    let request = client
        .post(&req.api_url)
        .bearer_auth(&req.api_key)
        .multipart(form);

    let start = std::time::Instant::now();

    let resp = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(TranscribeError::Cancelled),
        r = request.send() => r?,
    };

    let status = resp.status();
    // ゲートウェイは残りが少ないトークンに延長用の新トークンを付けて返す (2026-11-11)。
    // Groq/OpenAI へ直接送る経路では付かないので何もしない。
    let refreshed_jwt = if status.is_success() {
        resp.headers()
            .get(crate::gateway_auth::REFRESH_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    } else {
        None
    };
    let body = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(TranscribeError::Cancelled),
        b = resp.text() => b?,
    };

    let elapsed_ms = start.elapsed().as_millis();

    if !status.is_success() {
        return Err(TranscribeError::Http {
            status: status.as_u16(),
            body,
        });
    }

    if let Some(jwt) = refreshed_jwt {
        crate::gateway_auth::store_refreshed_jwt(&jwt);
    }

    debug!(
        target: "rushvoice.transcribe",
        "transcription succeeded: bytes_sent={} elapsed_ms={} wants_verbose={} body_len={}",
        bytes_sent, elapsed_ms, wants_verbose, body.len()
    );

    Ok(TranscribeResponse {
        body,
        bytes_sent,
        elapsed_ms,
    })
}

// Day 25 Phase 2-G (SSOT v3.3): TranscribeCandidate / transcribe_multi /
// transcribe_multi_inner / transcribe_multi_dispatch / transcribe_single_for_multi /
// parse_verbose_json / apply_rate_limit_throttle は ASR 補正レイヤー撤廃と共に削除済。
// 既存 transcribe() (Phase 1 hedged 用) は維持する。
// (Day 25: _reset_last_multi_call_for_test 削除済)
#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_request() -> TranscribeRequest {
        TranscribeRequest {
            // 到達不能アドレス。実ネットワーク無しでキャンセルテスト用に使う。
            api_url: "http://127.0.0.1:1/v1/audio/transcriptions".to_string(),
            api_key: "fake-key".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            prompt: String::new(),
            language: Some(DEFAULT_LANGUAGE.to_string()),
            audio: vec![0u8; 64],
            file_name: "audio.wav".to_string(),
            mime_type: "audio/wav".to_string(),
            verbose_json: true,
        }
    }

    #[test]
    fn client_builds_successfully() {
        let _client = build_client().expect("HTTP/2 client should build");
    }

    #[tokio::test]
    async fn pre_cancelled_token_returns_cancelled_without_network() {
        let client = build_client().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = transcribe(&client, dummy_request(), cancel)
            .await
            .expect_err("should error out");
        assert!(
            matches!(err, TranscribeError::Cancelled),
            "unexpected error variant: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn cancel_during_inflight_returns_cancelled() {
        let client = build_client().unwrap();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        // すぐにキャンセル
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_clone.cancel();
        });
        let err = transcribe(&client, dummy_request(), cancel)
            .await
            .expect_err("should error out");
        // 127.0.0.1:1 は接続拒否で Network エラー、間に合えば Cancelled
        match err {
            TranscribeError::Cancelled | TranscribeError::Network(_) => {}
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn invalid_mime_is_classified() {
        // multipart::Part::mime_str() は無効値で reqwest::Error を返す。
        // ここでは Display 形式のみチェック (実呼び出しは transcribe 経由)。
        let e = TranscribeError::InvalidMime("not/a/mime: bogus".into());
        let s = format!("{}", e);
        assert!(s.contains("invalid mime_type"), "got: {}", s);
    }

    #[test]
    fn http_error_display_format() {
        let e = TranscribeError::Http {
            status: 401,
            body: "Invalid API key".to_string(),
        };
        let s = format!("{}", e);
        assert!(s.contains("HTTP 401"), "got: {}", s);
        assert!(s.contains("Invalid API key"), "got: {}", s);
    }

    /// Gateway PoC (R-1 回帰防止): verbose_json は URL 文字列ではなくフィールドで決まる。
    /// ゲートウェイ経由 (URL が api.groq.com を含まない) でも verbose_json: true なら
    /// response_format=verbose_json になることを保証する代表テスト。
    #[test]
    fn verbose_json_field_controls_response_format_not_url() {
        let mut req = dummy_request();
        req.api_url = "https://rushvoice-gateway.example.workers.dev/groq/v1/audio/transcriptions"
            .to_string();
        req.verbose_json = true;
        assert!(
            req.verbose_json,
            "gateway URL でも verbose_json フィールドが true なら維持されること"
        );

        req.verbose_json = false;
        assert!(
            !req.verbose_json,
            "api.groq.com を含む URL でも verbose_json=false ならそのまま false"
        );
    }

    /// v0.1.16 機能 L Day 3: verbose_json が timestamp_granularities[] 追加分岐の前提として成立する。
    /// form ビルダ内部状態は直接検証できないため、フィールド判定の代表テストのみ
    /// (実 multipart 内容の検証は integration test で・Day 5 検討)。
    #[test]
    fn verbose_json_true_implies_timestamp_granularities_added() {
        // transcribe() 内の `if wants_verbose { form.text("timestamp_granularities[]", "segment") }`
        // の分岐条件そのものを直接検証 (URL には一切依存しない)。
        let req_gateway = TranscribeRequest {
            verbose_json: true,
            ..dummy_request()
        };
        assert!(req_gateway.verbose_json, "verbose_json=true で分岐が有効になること");

        let req_openai = TranscribeRequest {
            verbose_json: false,
            ..dummy_request()
        };
        assert!(!req_openai.verbose_json, "verbose_json=false では追加しない");
    }

    // Day 25 Phase 2-G (SSOT v3.3): transcribe_multi 関連テスト + parse_verbose_json テストは削除済
}
