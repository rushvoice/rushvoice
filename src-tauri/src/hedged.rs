// Phase 1 Day 5: trigger 秒 hedged + 従来パス (250ms × 2 リトライ)
// 仕様: phase1_v1_read_notes.md §1.3 / §1.4 / §1.5 (v1 VoiceInputMvp.ps1 L3409-3533 移植)
//
// フロー要約:
//   Phase A: primary を spawn → trigger 秒タイマー vs primary_fut で select
//     - Phase A 内成功     → 即返却 (winner=Primary, fallback_triggered=false)
//     - Phase A 内早期失敗 → ログ + Phase B 起動 (敗者リカバリ経路)
//     - Phase A タイムアウト → Phase B 起動 (primary は走らせ続ける)
//   Phase B: fallback を spawn → select! で先着勝者
//     - 勝者 Ok  → 即返却
//     - 勝者 Err → 敗者完了待ち (v1 L3480-3490 のループ等価)
//     - 両方 Err → HedgedError::BothFailed
//   finally: cancel.cancel() で敗者中断 (OpenAI 課金最小化)
//
// 従来パス (enable_fallback=false): primary 単独 + retry_count 回まで retry_delay_ms 待機リトライ
//
// テスト容易性のため、HTTP 呼び出しを行う関数本体を `dispatch_hedged_inner<F, Fut>`
// で受け取り、本番は transcribe::transcribe() を渡し、テストはモック関数を渡す。

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use log::{info, warn}; // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)

use crate::transcribe::{
    transcribe as transcribe_fn, TranscribeError, TranscribeRequest, TranscribeResponse,
};

/// hedged 動作設定。Phase 1 既定値は v1 と同じ。
#[derive(Debug, Clone)]
pub struct HedgedConfig {
    /// primary 単独待機の上限秒。これを超えたら fallback を並走起動。既定 3.0s。
    pub trigger_secs: f64,
    /// fallback が有効か。false なら primary 単独 + 250ms × N 回リトライ。
    pub enable_fallback: bool,
    /// 従来パスのリトライ最大回数。既定 2。
    pub retry_count: u8,
    /// 従来パスのリトライ待機 ms。既定 250。
    pub retry_delay_ms: u64,
}

impl Default for HedgedConfig {
    fn default() -> Self {
        Self {
            trigger_secs: 3.0,
            enable_fallback: true,
            retry_count: 2,
            retry_delay_ms: 250,
        }
    }
}

/// どちらが勝者か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinnerProvider {
    Primary,
    Fallback,
}

/// hedged 成功時のレスポンス。
#[derive(Debug)]
pub struct HedgedResponse {
    /// raw レスポンス本文 (verbose_json or json)
    pub body: String,
    /// 勝者
    pub winner: WinnerProvider,
    /// fallback が dispatch されたか (= trigger 秒超過 or primary 早期失敗)
    pub fallback_triggered: bool,
    /// dispatch_hedged 開始から決着までの ms
    pub elapsed_ms: u128,
    /// 勝者の送信バイト数
    pub bytes_sent: u64,
    /// 勝者単独の所要 ms (winner = primary なら primary, fallback なら fallback 自身)
    /// v0.1.9: history.db.primary_ms 用 (実態は「勝者の elapsed_ms」)
    pub winner_ms: u128,
}

impl HedgedResponse {
    /// history.db.hedged_winner 用文字列 ("primary" / "fallback")。
    pub fn winner_str(&self) -> &'static str {
        match self.winner {
            WinnerProvider::Primary => "primary",
            WinnerProvider::Fallback => "fallback",
        }
    }
}

/// hedged エラー。両方失敗時は両エラーを保持する。
#[derive(Debug, Error)]
pub enum HedgedError {
    #[error("primary and fallback both failed (primary: {primary}, fallback: {fallback})")]
    BothFailed {
        primary: TranscribeError,
        fallback: TranscribeError,
    },
    #[error("primary failed and fallback disabled (after {attempts} attempts): {source}")]
    PrimaryOnly {
        source: TranscribeError,
        attempts: u8,
    },
    #[error("hedged dispatch cancelled")]
    Cancelled,
}

/// 本番用エントリポイント。`transcribe::transcribe()` を call_fn として注入する。
pub async fn dispatch_hedged(
    client: reqwest::Client,
    primary: TranscribeRequest,
    fallback: Option<TranscribeRequest>,
    config: HedgedConfig,
    cancel: CancellationToken,
) -> Result<HedgedResponse, HedgedError> {
    let client_clone = client.clone();
    let call_fn = move |req: TranscribeRequest, c: CancellationToken| {
        let client = client_clone.clone();
        async move { transcribe_fn(&client, req, c).await }
    };
    dispatch_hedged_inner(call_fn, primary, fallback, config, cancel).await
}

/// テスト容易性のため `call_fn` を注入する内部実装。
///
/// `call_fn` は `(TranscribeRequest, CancellationToken) -> Future<Result<TranscribeResponse, TranscribeError>>`
/// を返す Clone 可能な関数。本番では `transcribe::transcribe()` を呼ぶラッパー、テストでは遅延/失敗を
/// 制御するモックを渡す。
pub async fn dispatch_hedged_inner<F, Fut>(
    call_fn: F,
    primary: TranscribeRequest,
    fallback: Option<TranscribeRequest>,
    config: HedgedConfig,
    cancel: CancellationToken,
) -> Result<HedgedResponse, HedgedError>
where
    F: Fn(TranscribeRequest, CancellationToken) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = std::result::Result<TranscribeResponse, TranscribeError>> + Send + 'static,
{
    let start = Instant::now();

    // 従来パス (fallback 無効 or fallback request が None)
    if !config.enable_fallback || fallback.is_none() {
        return run_primary_with_retries(call_fn, primary, &config, cancel, start).await;
    }
    let fallback_req = fallback.expect("fallback is Some by guard above");

    // ===== Phase A: primary 先発 + trigger 秒待機 =====
    let primary_cancel = cancel.child_token();
    let fallback_cancel = cancel.child_token();

    let trigger_ms = (config.trigger_secs * 1000.0) as u64;
    info!(
        target: "rushvoice.hedged",
        "[PROVIDER=primary] dispatched (fallback armed, trigger={}s)",
        config.trigger_secs
    );

    let primary_fut = call_fn.clone()(primary, primary_cancel.clone());
    tokio::pin!(primary_fut);

    let phase_a: Option<Result<TranscribeResponse, TranscribeError>> = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            primary_cancel.cancel();
            return Err(HedgedError::Cancelled);
        }
        result = &mut primary_fut => Some(result),
        _ = sleep(Duration::from_millis(trigger_ms)) => None,
    };

    let primary_early_err: Option<TranscribeError> = match phase_a {
        Some(Ok(resp)) => {
            // Phase A 内成功 → primary 単独勝者 (fallback dispatch せず・課金ゼロ)
            let elapsed_ms = start.elapsed().as_millis();
            info!(
                target: "rushvoice.hedged",
                "[PROVIDER=primary] completed in {}ms",
                resp.elapsed_ms
            );
            // 念のため fallback 側 cancel (まだ起動していないが child_token のクリーンアップ)
            fallback_cancel.cancel();
            return Ok(HedgedResponse {
                body: resp.body,
                winner: WinnerProvider::Primary,
                fallback_triggered: false,
                elapsed_ms,
                bytes_sent: resp.bytes_sent,
                winner_ms: resp.elapsed_ms,
            });
        }
        Some(Err(e)) => {
            // Phase A 内早期失敗 → fallback 起動 (敗者リカバリ経路)
            warn!(
                target: "rushvoice.hedged",
                "[PROVIDER=primary] early fault: {}. Switching to fallback.",
                e
            );
            Some(e)
        }
        None => {
            // trigger 超過 → fallback 起動 (primary は走らせ続ける)
            info!(
                target: "rushvoice.hedged",
                "[FALLBACK] fallback dispatched after {}s (primary still pending)",
                config.trigger_secs
            );
            None
        }
    };

    // ===== Phase B: fallback を起動 =====
    let fallback_fut = call_fn(fallback_req, fallback_cancel.clone());
    tokio::pin!(fallback_fut);

    // primary が既に早期失敗していたら primary_fut はもう待たない (poll 済みなので drop)。
    // primary 残存ケース: Phase A タイムアウト。両方を select! で競合させる。
    let result: Result<(TranscribeResponse, WinnerProvider), BothFailed> =
        if let Some(primary_err) = primary_early_err {
            // primary は既に Err 返却済 → fallback の結果だけ待つ
            let fallback_result: Result<TranscribeResponse, TranscribeError> = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    primary_cancel.cancel();
                    fallback_cancel.cancel();
                    return Err(HedgedError::Cancelled);
                }
                r = &mut fallback_fut => r,
            };
            match fallback_result {
                Ok(resp) => Ok((resp, WinnerProvider::Fallback)),
                Err(fallback_err) => Err(BothFailed {
                    primary: primary_err,
                    fallback: fallback_err,
                }),
            }
        } else {
            // primary も fallback も生きている → 先着勝者
            race_two(
                &mut primary_fut,
                &mut fallback_fut,
                &cancel,
                &primary_cancel,
                &fallback_cancel,
            )
            .await
        };

    match result {
        Ok((resp, winner)) => {
            // 勝者確定 → 敗者キャンセル
            primary_cancel.cancel();
            fallback_cancel.cancel();
            let elapsed_ms = start.elapsed().as_millis();
            match winner {
                WinnerProvider::Primary => {
                    info!(
                        target: "rushvoice.hedged",
                        "[LOSER] fallback discarded (primary recovered, won t={}ms after fallback armed)",
                        resp.elapsed_ms
                    );
                }
                WinnerProvider::Fallback => {
                    info!(
                        target: "rushvoice.hedged",
                        "[FALLBACK] fallback won t={}ms (primary slow)",
                        resp.elapsed_ms
                    );
                }
            }
            let winner_ms = resp.elapsed_ms;
            Ok(HedgedResponse {
                body: resp.body,
                winner,
                fallback_triggered: true,
                elapsed_ms,
                bytes_sent: resp.bytes_sent,
                winner_ms,
            })
        }
        Err(BothFailed { primary, fallback }) => {
            primary_cancel.cancel();
            fallback_cancel.cancel();
            // 親 cancel 発火で両プロバイダーが同時に TranscribeError::Cancelled を返した
            // ケース (race_two の parent_cancel アーム経由 含む) は BothFailed ではなく
            // Cancelled として正規化する (Codex review P1 対応・2026-05-21)。
            if matches!(primary, TranscribeError::Cancelled)
                && matches!(fallback, TranscribeError::Cancelled)
            {
                return Err(HedgedError::Cancelled);
            }
            Err(HedgedError::BothFailed { primary, fallback })
        }
    }
}

/// fallback 無効時の従来パス: primary を最大 retry_count 回試行。
async fn run_primary_with_retries<F, Fut>(
    call_fn: F,
    primary: TranscribeRequest,
    config: &HedgedConfig,
    cancel: CancellationToken,
    start: Instant,
) -> Result<HedgedResponse, HedgedError>
where
    F: Fn(TranscribeRequest, CancellationToken) -> Fut + Clone,
    Fut: Future<Output = std::result::Result<TranscribeResponse, TranscribeError>>,
{
    info!(
        target: "rushvoice.hedged",
        "[PROVIDER=primary] dispatched (fallback disabled)"
    );

    let mut last_err: Option<TranscribeError> = None;
    let max_attempts = config.retry_count.max(1);

    for attempt in 1..=max_attempts {
        if cancel.is_cancelled() {
            return Err(HedgedError::Cancelled);
        }
        let req = primary.clone();
        match call_fn.clone()(req, cancel.clone()).await {
            Ok(resp) => {
                let elapsed_ms = start.elapsed().as_millis();
                info!(
                    target: "rushvoice.hedged",
                    "[PROVIDER=primary] completed in {}ms (attempt {}/{})",
                    resp.elapsed_ms,
                    attempt,
                    max_attempts
                );
                return Ok(HedgedResponse {
                    body: resp.body,
                    winner: WinnerProvider::Primary,
                    fallback_triggered: false,
                    elapsed_ms,
                    bytes_sent: resp.bytes_sent,
                    winner_ms: resp.elapsed_ms,
                });
            }
            Err(e) => {
                if matches!(e, TranscribeError::Cancelled) {
                    return Err(HedgedError::Cancelled);
                }
                if attempt < max_attempts {
                    warn!(
                        target: "rushvoice.hedged",
                        "[RETRY] Attempt {}/{} failed: {}. Retrying after {}ms...",
                        attempt,
                        max_attempts,
                        e,
                        config.retry_delay_ms
                    );
                    last_err = Some(e);
                    sleep(Duration::from_millis(config.retry_delay_ms)).await;
                } else {
                    last_err = Some(e);
                }
            }
        }
    }
    Err(HedgedError::PrimaryOnly {
        source: last_err.expect("last_err must be Some after failing all attempts"),
        attempts: max_attempts,
    })
}

/// 「両方失敗」専用の中間エラー型。`race_two` の戻り値のために使う。
struct BothFailed {
    primary: TranscribeError,
    fallback: TranscribeError,
}

/// primary / fallback の future を競合させ、先着の Ok を勝者とする。
/// 勝者 Err → 敗者完了待ち → 敗者 Ok なら敗者勝利、敗者も Err なら BothFailed。
async fn race_two(
    primary_fut: &mut Pin<&mut impl Future<Output = Result<TranscribeResponse, TranscribeError>>>,
    fallback_fut: &mut Pin<&mut impl Future<Output = Result<TranscribeResponse, TranscribeError>>>,
    parent_cancel: &CancellationToken,
    primary_cancel: &CancellationToken,
    fallback_cancel: &CancellationToken,
) -> Result<(TranscribeResponse, WinnerProvider), BothFailed> {
    let first = tokio::select! {
        biased;
        _ = parent_cancel.cancelled() => {
            primary_cancel.cancel();
            fallback_cancel.cancel();
            // 親キャンセルは BothFailed に詰めて返す (呼び出し側で Cancelled として扱うが、
            // ここは戻り値の表現上 Err 経路を使う)
            return Err(BothFailed {
                primary: TranscribeError::Cancelled,
                fallback: TranscribeError::Cancelled,
            });
        }
        r = primary_fut.as_mut() => (WinnerProvider::Primary, r),
        r = fallback_fut.as_mut() => (WinnerProvider::Fallback, r),
    };

    match first {
        (winner, Ok(resp)) => Ok((resp, winner)),
        (failed_side, Err(err_first)) => {
            // 敗者リカバリ: 残った片方を待つ
            let second = match failed_side {
                WinnerProvider::Primary => fallback_fut.as_mut().await.map(|r| (r, WinnerProvider::Fallback)),
                WinnerProvider::Fallback => primary_fut.as_mut().await.map(|r| (r, WinnerProvider::Primary)),
            };
            match second {
                Ok((resp, winner)) => Ok((resp, winner)),
                Err(err_second) => {
                    let (primary, fallback) = match failed_side {
                        WinnerProvider::Primary => (err_first, err_second),
                        WinnerProvider::Fallback => (err_second, err_first),
                    };
                    Err(BothFailed { primary, fallback })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn req(api_url: &str) -> TranscribeRequest {
        TranscribeRequest {
            api_url: api_url.to_string(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            prompt: String::new(),
            language: Some("ja".to_string()),
            audio: vec![0u8; 32],
            file_name: "audio.wav".to_string(),
            mime_type: "audio/wav".to_string(),
            verbose_json: true,
        }
    }

    fn ok_resp(body: &str, elapsed_ms: u128) -> TranscribeResponse {
        TranscribeResponse {
            body: body.to_string(),
            bytes_sent: 32,
            elapsed_ms,
        }
    }

    /// 指定 ms 待ってから Ok(body) を返すモック関数を作る。
    fn mock_ok(delay_ms: u64, body: &'static str) -> impl Fn(TranscribeRequest, CancellationToken) -> Pin<Box<dyn Future<Output = Result<TranscribeResponse, TranscribeError>> + Send>> + Clone {
        move |_req, cancel| {
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(TranscribeError::Cancelled),
                    _ = sleep(Duration::from_millis(delay_ms)) => Ok(ok_resp(body, delay_ms as u128)),
                }
            })
        }
    }

    /// 指定 ms 待ってから Http エラーを返すモック関数を作る。
    fn mock_err_http(delay_ms: u64, status: u16) -> impl Fn(TranscribeRequest, CancellationToken) -> Pin<Box<dyn Future<Output = Result<TranscribeResponse, TranscribeError>> + Send>> + Clone {
        move |_req, cancel| {
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(TranscribeError::Cancelled),
                    _ = sleep(Duration::from_millis(delay_ms)) => Err(TranscribeError::Http { status, body: "mock error".into() }),
                }
            })
        }
    }

    /// primary / fallback を URL で振り分けて呼び出すモック関数を作る。
    fn mock_dispatch<P, Pfut, F, Ffut>(
        primary_fn: P,
        fallback_fn: F,
    ) -> impl Fn(TranscribeRequest, CancellationToken) -> Pin<Box<dyn Future<Output = Result<TranscribeResponse, TranscribeError>> + Send>> + Clone
    where
        P: Fn(TranscribeRequest, CancellationToken) -> Pfut + Clone + Send + 'static,
        Pfut: Future<Output = Result<TranscribeResponse, TranscribeError>> + Send + 'static,
        F: Fn(TranscribeRequest, CancellationToken) -> Ffut + Clone + Send + 'static,
        Ffut: Future<Output = Result<TranscribeResponse, TranscribeError>> + Send + 'static,
    {
        move |req, cancel| {
            let is_primary = req.api_url.contains("primary");
            let primary_fn = primary_fn.clone();
            let fallback_fn = fallback_fn.clone();
            Box::pin(async move {
                if is_primary {
                    primary_fn(req, cancel).await
                } else {
                    fallback_fn(req, cancel).await
                }
            })
        }
    }

    fn fast_config() -> HedgedConfig {
        HedgedConfig {
            trigger_secs: 0.3,
            enable_fallback: true,
            retry_count: 2,
            retry_delay_ms: 50,
        }
    }

    #[tokio::test]
    async fn primary_wins_within_trigger() {
        // primary 100ms 成功 / fallback は dispatch されないはず
        let dispatch_called = Arc::new(AtomicUsize::new(0));
        let dc = dispatch_called.clone();
        let mock_p = mock_ok(100, "primary-body");
        let mock_f = {
            let dc = dc.clone();
            move |req: TranscribeRequest, cancel: CancellationToken| {
                dc.fetch_add(1, Ordering::SeqCst);
                let inner = mock_ok(1000, "fallback-body");
                inner(req, cancel)
            }
        };
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let resp = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            CancellationToken::new(),
        )
        .await
        .expect("primary should win");
        assert_eq!(resp.winner, WinnerProvider::Primary);
        assert!(!resp.fallback_triggered);
        assert_eq!(resp.body, "primary-body");
        assert_eq!(
            dispatch_called.load(Ordering::SeqCst),
            0,
            "fallback must NOT be dispatched when primary wins within trigger"
        );
    }

    #[tokio::test]
    async fn fallback_wins_after_trigger() {
        // primary 2000ms / fallback 100ms (trigger 300ms 超過後に dispatch)
        let mock_p = mock_ok(2000, "primary-body");
        let mock_f = mock_ok(100, "fallback-body");
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let resp = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            CancellationToken::new(),
        )
        .await
        .expect("fallback should win");
        assert_eq!(resp.winner, WinnerProvider::Fallback);
        assert!(resp.fallback_triggered);
        assert_eq!(resp.body, "fallback-body");
    }

    #[tokio::test]
    async fn primary_early_fault_fallback_succeeds() {
        // primary 即 Err / fallback 100ms 成功
        let mock_p = mock_err_http(50, 500);
        let mock_f = mock_ok(100, "fallback-body");
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let resp = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            CancellationToken::new(),
        )
        .await
        .expect("fallback should recover");
        assert_eq!(resp.winner, WinnerProvider::Fallback);
        assert!(resp.fallback_triggered, "fallback should be triggered after primary early fault");
    }

    #[tokio::test]
    async fn primary_recovers_after_fallback_armed() {
        // primary 500ms 成功 (trigger 300ms 超過後に勝利) / fallback 2000ms
        let mock_p = mock_ok(500, "primary-body");
        let mock_f = mock_ok(2000, "fallback-body");
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let resp = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            CancellationToken::new(),
        )
        .await
        .expect("primary should recover after fallback armed");
        assert_eq!(resp.winner, WinnerProvider::Primary);
        assert!(resp.fallback_triggered, "fallback armed before primary completed");
        assert_eq!(resp.body, "primary-body");
    }

    #[tokio::test]
    async fn both_fail() {
        // primary Err / fallback Err
        let mock_p = mock_err_http(50, 500);
        let mock_f = mock_err_http(100, 503);
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let err = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            CancellationToken::new(),
        )
        .await
        .expect_err("should fail when both providers fail");
        match err {
            HedgedError::BothFailed { primary, fallback } => {
                assert!(matches!(primary, TranscribeError::Http { status: 500, .. }));
                assert!(matches!(fallback, TranscribeError::Http { status: 503, .. }));
            }
            other => panic!("expected BothFailed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn parent_cancel_returns_cancelled_not_both_failed() {
        // Codex review P1 回帰防止: 親 cancel 発火時は HedgedError::Cancelled を返す
        // (両 provider が TranscribeError::Cancelled になっても BothFailed に丸めない)
        let mock_p = mock_ok(2000, "primary-body");
        let mock_f = mock_ok(2000, "fallback-body");
        let call_fn = mock_dispatch(mock_p, mock_f);
        let primary = req("https://primary.example/");
        let fallback = req("https://fallback.example/");
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        // dispatch 開始後すぐにキャンセル (trigger 超過後の race_two 中をキャンセル)
        tokio::spawn(async move {
            sleep(Duration::from_millis(400)).await;
            cancel_clone.cancel();
        });
        let err = dispatch_hedged_inner(
            call_fn,
            primary,
            Some(fallback),
            fast_config(),
            cancel,
        )
        .await
        .expect_err("should error out due to parent cancel");
        assert!(
            matches!(err, HedgedError::Cancelled),
            "expected Cancelled, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn fallback_disabled_retries_then_succeeds() {
        // primary: 1 回目 Err / 2 回目 Ok。fallback 無効。
        let attempt = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt.clone();
        let call_fn = move |_req: TranscribeRequest, cancel: CancellationToken| {
            let n = attempt_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(TranscribeError::Cancelled),
                    _ = sleep(Duration::from_millis(20)) => {
                        if n == 0 {
                            Err(TranscribeError::Http { status: 500, body: "first attempt".into() })
                        } else {
                            Ok(ok_resp("primary-retry-ok", 20))
                        }
                    }
                }
            }) as Pin<Box<dyn Future<Output = Result<TranscribeResponse, TranscribeError>> + Send>>
        };
        let config = HedgedConfig {
            trigger_secs: 1.0,
            enable_fallback: false,
            retry_count: 2,
            retry_delay_ms: 30,
        };
        let primary = req("https://primary.example/");
        let resp = dispatch_hedged_inner(
            call_fn,
            primary,
            None,
            config,
            CancellationToken::new(),
        )
        .await
        .expect("retry should recover");
        assert_eq!(resp.winner, WinnerProvider::Primary);
        assert!(!resp.fallback_triggered);
        assert_eq!(resp.body, "primary-retry-ok");
        assert_eq!(attempt.load(Ordering::SeqCst), 2, "should attempt twice");
    }
}
