//! Day 5 後半 DPAPI 実機検証 example
//!
//! 実行: 環境変数を設定してから
//!   $env:GROQ_API_KEY = "gsk_..."
//!   $env:OPENAI_API_KEY = "sk-..."
//!   cargo run --example day5_dpapi_real
//!
//! 動作:
//! 1. GROQ_API_KEY / OPENAI_API_KEY を環境変数から読み取り（未設定ならダミー）
//! 2. tmp/config.secrets.encrypted.json に DPAPI 暗号化 → Base64 → JSON 保存
//! 3. ファイルを読み戻し DPAPI 復号 → 平文比較
//! 4. 検証成功なら "PASS"・失敗なら "FAIL" を出力
//! 5. テストファイルは検証後に削除

use std::env;
use std::fs;

use app_lib::crypto::{load_secrets, save_secrets};

fn main() {
    let groq = env::var("GROQ_API_KEY").unwrap_or_else(|_| "dummy-groq-key-for-day5".to_string());
    let openai = env::var("OPENAI_API_KEY").unwrap_or_else(|_| "dummy-openai-key-for-day5".to_string());

    // OS の一時ディレクトリに書き出す (リポジトリ管轄外 = 万が一クリーンアップが
    // 飛んでも git に DPAPI 暗号文がコミットされない・Codex review P2 対策)
    let tmp_dir = env::temp_dir().join("rushvoice_day5_dpapi");
    let _ = fs::create_dir_all(&tmp_dir);
    let path = tmp_dir.join("config.secrets.encrypted.json");

    println!("[Day5-DPAPI] target file: {}", path.display());
    println!(
        "[Day5-DPAPI] groq plaintext len = {} / openai plaintext len = {}",
        groq.len(),
        openai.len()
    );

    // feature/gateway-login: 第 4 引数 = gateway JWT (この検証 example では未使用)
    if let Err(e) = save_secrets(&path, Some(&groq), Some(&openai), None) {
        eprintln!("[Day5-DPAPI] FAIL save_secrets: {:?}", e);
        std::process::exit(1);
    }

    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "[Day5-DPAPI] saved: {} bytes (JSON + Base64 + DPAPI ciphertext)",
        file_size
    );

    let bundle = match load_secrets(&path) {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!("[Day5-DPAPI] FAIL load_secrets returned None");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[Day5-DPAPI] FAIL load_secrets: {:?}", e);
            std::process::exit(1);
        }
    };

    let groq_dec = bundle.groq_api_key.as_deref().unwrap_or("");
    let openai_dec = bundle.openai_api_key.as_deref().unwrap_or("");

    let groq_ok = groq_dec == groq;
    let openai_ok = openai_dec == openai;

    println!(
        "[Day5-DPAPI] groq  round-trip = {} (decoded len = {})",
        if groq_ok { "PASS" } else { "FAIL" },
        groq_dec.len()
    );
    println!(
        "[Day5-DPAPI] openai round-trip = {} (decoded len = {})",
        if openai_ok { "PASS" } else { "FAIL" },
        openai_dec.len()
    );

    let _ = fs::remove_file(&path);
    println!("[Day5-DPAPI] cleanup: file removed");

    if groq_ok && openai_ok {
        println!("[Day5-DPAPI] OVERALL: PASS");
    } else {
        eprintln!("[Day5-DPAPI] OVERALL: FAIL");
        std::process::exit(1);
    }
}
