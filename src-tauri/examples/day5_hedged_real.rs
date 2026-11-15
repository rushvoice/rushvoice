// Phase 1 Day 5: hedged 実機検証 (Groq primary + OpenAI fallback)
//
// 検証シナリオ (合計 3 件):
//   A) trigger=3.0s 既定 → Groq が trigger 内勝利 (fallback_triggered=false)
//      OpenAI 課金ゼロを実証。
//   B) trigger=0.05s 短縮 → 確実に fallback 起動 (fallback_triggered=true)
//      tokio::select! の Phase A タイムアウト経路 + 敗者キャンセル動作確認。
//   C) 無効な Groq API キーで primary 早期失敗 → fallback 回復
//      Phase A 内早期失敗経路 (敗者リカバリ) を実証。
//
// 実行方法 (PowerShell・ワークツリーから):
//   cd src-tauri
//   $env:GROQ_API_KEY = "gsk_..."
//   $env:OPENAI_API_KEY = "sk-..."
//   cargo run --example day5_hedged_real
//
// 任意の環境変数:
//   RUSHVOICE_GROQ_URL    (既定: https://api.groq.com/openai/v1/audio/transcriptions)
//   RUSHVOICE_GROQ_MODEL  (既定: whisper-large-v3-turbo)
//   RUSHVOICE_OPENAI_URL  (既定: https://api.openai.com/v1/audio/transcriptions)
//   RUSHVOICE_OPENAI_MODEL (既定: gpt-4o-mini-transcribe)
//
// コスト最小化:
//   - 1 秒の無音 WAV を使用 (実発話不要・OpenAI 課金は 1 秒分のみ)
//   - シナリオ A で Groq 勝利すれば OpenAI 未消費
//   - シナリオ B/C のみ OpenAI を消費する
//
// 注意:
//   - API キーをログに出さない (len のみ表示)
//   - 各シナリオ間に 500ms ウェイト (HTTP/2 connection reuse 観測)

use std::time::Instant;

use anyhow::{Context, Result};
use app_lib::hedged::{self, HedgedConfig, HedgedResponse, WinnerProvider};
use app_lib::transcribe::{self, TranscribeRequest};
use tokio_util::sync::CancellationToken;

const DEFAULT_GROQ_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
const DEFAULT_GROQ_MODEL: &str = "whisper-large-v3-turbo";
const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini-transcribe";

const SAMPLE_RATE_HZ: u32 = 16_000;
const DURATION_SEC: u32 = 1;
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;

#[tokio::main]
async fn main() -> Result<()> {
    let groq_key = std::env::var("GROQ_API_KEY").context("GROQ_API_KEY 環境変数が未設定")?;
    let openai_key =
        std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY 環境変数が未設定")?;
    let groq_url =
        std::env::var("RUSHVOICE_GROQ_URL").unwrap_or_else(|_| DEFAULT_GROQ_URL.to_string());
    let groq_model =
        std::env::var("RUSHVOICE_GROQ_MODEL").unwrap_or_else(|_| DEFAULT_GROQ_MODEL.to_string());
    let openai_url =
        std::env::var("RUSHVOICE_OPENAI_URL").unwrap_or_else(|_| DEFAULT_OPENAI_URL.to_string());
    let openai_model = std::env::var("RUSHVOICE_OPENAI_MODEL")
        .unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());

    println!("================================================================");
    println!("Day 5: hedged 実機検証 (Groq primary + OpenAI fallback)");
    println!("  Groq   URL: {}", groq_url);
    println!("  Groq   model: {}", groq_model);
    println!("  Groq   key: {} chars (hidden)", groq_key.len());
    println!("  OpenAI URL: {}", openai_url);
    println!("  OpenAI model: {}", openai_model);
    println!("  OpenAI key: {} chars (hidden)", openai_key.len());
    println!("================================================================");

    let wav = make_silence_wav(DURATION_SEC, SAMPLE_RATE_HZ, CHANNELS, BITS_PER_SAMPLE);
    println!(
        "[setup] audio: {} bytes ({}s @ {}Hz mono {}-bit)\n",
        wav.len(),
        DURATION_SEC,
        SAMPLE_RATE_HZ,
        BITS_PER_SAMPLE
    );

    let client = transcribe::build_client().context("HTTP/2 client build failed")?;

    let primary_req_template = TranscribeRequest {
        api_url: groq_url.clone(),
        api_key: groq_key.clone(),
        model: groq_model.clone(),
        prompt: String::new(),
        language: Some("ja".to_string()),
        audio: wav.clone(),
        file_name: "test.wav".into(),
        mime_type: "audio/wav".into(),
        // Gateway PoC (SSOT §4-B): Groq 経路 → verbose_json 前提 (旧 URL 判定と同じ挙動)。
        verbose_json: true,
    };
    let fallback_req_template = TranscribeRequest {
        api_url: openai_url.clone(),
        api_key: openai_key.clone(),
        model: openai_model.clone(),
        prompt: String::new(),
        language: Some("ja".to_string()),
        audio: wav.clone(),
        file_name: "test.wav".into(),
        mime_type: "audio/wav".into(),
        // Gateway PoC (SSOT §4-B): OpenAI 経路 → response_format=json (旧 URL 判定と同じ挙動)。
        verbose_json: false,
    };

    let mut all_passed = true;

    // ===== シナリオ A: 既定 trigger=3.0s → Groq 勝利・OpenAI 未起動 =====
    println!("----------------------------------------------------------------");
    println!("[Scenario A] trigger=3.0s (既定) → Groq が trigger 内勝利を期待");
    println!("----------------------------------------------------------------");
    let cfg_a = HedgedConfig::default();
    let resp_a = run_scenario(
        &client,
        primary_req_template.clone(),
        Some(fallback_req_template.clone()),
        cfg_a,
    )
    .await;
    match &resp_a {
        Ok(r) => {
            print_response("A", r);
            if r.winner != WinnerProvider::Primary || r.fallback_triggered {
                eprintln!("  ✗ FAIL: 期待=Primary単独勝利・fallback非起動");
                all_passed = false;
            } else {
                println!("  ✓ PASS: Primary単独勝利・OpenAI 課金ゼロ");
            }
        }
        Err(e) => {
            eprintln!("  ✗ FAIL: {}", e);
            all_passed = false;
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ===== シナリオ B: trigger=0.05s → fallback 起動・どちらが勝つかは時系列依存 =====
    println!("\n----------------------------------------------------------------");
    println!("[Scenario B] trigger=0.05s → fallback 強制起動");
    println!("----------------------------------------------------------------");
    let cfg_b = HedgedConfig {
        trigger_secs: 0.05,
        enable_fallback: true,
        retry_count: 2,
        retry_delay_ms: 250,
    };
    let resp_b = run_scenario(
        &client,
        primary_req_template.clone(),
        Some(fallback_req_template.clone()),
        cfg_b,
    )
    .await;
    match &resp_b {
        Ok(r) => {
            print_response("B", r);
            if !r.fallback_triggered {
                eprintln!("  ✗ FAIL: trigger=0.05s でも fallback 未起動");
                all_passed = false;
            } else {
                println!("  ✓ PASS: fallback_triggered=true (winner={:?})", r.winner);
            }
        }
        Err(e) => {
            eprintln!("  ✗ FAIL: {}", e);
            all_passed = false;
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ===== シナリオ C: 無効 Groq キー → Phase A 早期失敗 → fallback 勝利 =====
    println!("\n----------------------------------------------------------------");
    println!("[Scenario C] 無効 Groq キー → Primary 早期失敗 → Fallback 勝利");
    println!("----------------------------------------------------------------");
    let mut bad_primary = primary_req_template.clone();
    bad_primary.api_key = "gsk_INVALID_KEY_FOR_TEST_xxxxxxxxxxxxxxxxxxxxxx".to_string();
    let cfg_c = HedgedConfig::default();
    let resp_c = run_scenario(
        &client,
        bad_primary,
        Some(fallback_req_template.clone()),
        cfg_c,
    )
    .await;
    match &resp_c {
        Ok(r) => {
            print_response("C", r);
            if r.winner != WinnerProvider::Fallback || !r.fallback_triggered {
                eprintln!("  ✗ FAIL: 期待=Fallback勝利・fallback_triggered=true");
                all_passed = false;
            } else {
                println!("  ✓ PASS: Fallback 勝利・敗者リカバリ動作");
            }
        }
        Err(e) => {
            eprintln!("  ✗ FAIL: {}", e);
            all_passed = false;
        }
    }

    println!("\n================================================================");
    if all_passed {
        println!("✓ All 3 scenarios PASSED");
        Ok(())
    } else {
        Err(anyhow::anyhow!("one or more scenarios failed"))
    }
}

async fn run_scenario(
    client: &reqwest::Client,
    primary: TranscribeRequest,
    fallback: Option<TranscribeRequest>,
    config: HedgedConfig,
) -> Result<HedgedResponse> {
    let cancel = CancellationToken::new();
    let start = Instant::now();
    let result = hedged::dispatch_hedged(client.clone(), primary, fallback, config, cancel).await;
    let elapsed_total = start.elapsed().as_millis();
    println!("  wall-clock elapsed: {} ms", elapsed_total);
    result.map_err(|e| anyhow::anyhow!("hedged dispatch failed: {}", e))
}

fn print_response(name: &str, r: &HedgedResponse) {
    println!(
        "  [{}] winner={:?} fallback_triggered={} elapsed_ms={} bytes_sent={}",
        name, r.winner, r.fallback_triggered, r.elapsed_ms, r.bytes_sent
    );
    let body_preview: String = r.body.chars().take(120).collect();
    println!("  body preview ({} chars): {}", r.body.len(), body_preview);
}

/// 無音の WAV (PCM・little endian) を生成する。
fn make_silence_wav(
    duration_sec: u32,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
) -> Vec<u8> {
    let bytes_per_sample = (bits_per_sample / 8) as u32;
    let block_align = channels as u32 * bytes_per_sample;
    let n_samples = duration_sec * sample_rate;
    let data_size = n_samples * block_align;
    let byte_rate = sample_rate * block_align;
    let riff_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&(block_align as u16).to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend(std::iter::repeat(0u8).take(data_size as usize));
    buf
}
