// crypto/windows_impl.rs — Phase 5 Step A-3: Windows DPAPI 実装 (Phase 1 Day 5 後半から移動)
//
// Phase 1 Day 5 後半 / 申し送り §1 + 精読ノート §3 / 教訓 L113-L123
//
// - スコープ: CurrentUser (同じ Windows ユーザー・同じ端末でのみ復号可)
// - 別端末・別ユーザーは再暗号化必須
// - 保存ファイル: <project_root>/config.secrets.encrypted.json
//   (v1 Encrypt-ApiKey.ps1 / VoiceInputMvp.ps1 L45-66 と完全互換)
// - 復号失敗時は静かに None 返却 (v1 同様・平文 config or env var にフォールバック)
//
// Phase 5 Step A-3 で crypto.rs から本ファイルに移動。中身 (関数本体) は無変更。
// pre-existing clippy 警告 (windows crate の mutable reference 系) は baseline として保持。

#![cfg(target_os = "windows")]

use std::fs;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
};

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("Windows DPAPI error: {0}")]
    WinApi(#[from] windows::core::Error),
    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// CRYPT_INTEGER_BLOB を組み立てて CryptProtectData を呼ぶ。
/// CurrentUser スコープ・entropy なし・UI 抑止 (CRYPTPROTECT_UI_FORBIDDEN)。
pub fn protect_bytes(plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if plain.is_empty() {
        return Ok(Vec::new());
    }
    // 入力 BLOB は読み取り専用扱いだが win32 シグネチャ上 *mut なので mut にする。
    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &mut in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )?;
    }
    // pbData は LocalAlloc されたバッファ。Vec にコピーしてから LocalFree。
    let result = unsafe {
        std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec()
    };
    unsafe {
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut std::ffi::c_void));
    }
    Ok(result)
}

/// CryptUnprotectData。失敗時はエラーを返す (上位で None に丸める)。
pub fn unprotect_bytes(enc: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if enc.is_empty() {
        return Ok(Vec::new());
    }
    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: enc.len() as u32,
        pbData: enc.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &mut in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )?;
    }
    let result = unsafe {
        std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec()
    };
    unsafe {
        let _ = LocalFree(HLOCAL(out_blob.pbData as *mut std::ffi::c_void));
    }
    Ok(result)
}

/// v1 互換 JSON スキーマ。
/// VoiceInputMvp.ps1 L45-66 + Encrypt-ApiKey.ps1 L26-31 と同じフィールド名 (camelCase)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsFile {
    #[serde(rename = "encryptedAt")]
    pub encrypted_at: DateTime<Utc>,
    pub scope: String,
    pub machine: String,
    pub user: String,
    #[serde(rename = "groqApiKeyEncrypted", default)]
    pub groq_api_key_encrypted: String,
    #[serde(rename = "openAiApiKeyEncrypted", default)]
    pub openai_api_key_encrypted: String,
    /// 旧版の RushVoice アカウント (ゲートウェイ) のログイン情報。2026-11-15 に終了。
    /// 読むだけで書き出さない (次の保存で消える)。残っていたら移行の案内に使う。
    #[serde(rename = "gatewayJwtEncrypted", default, skip_serializing)]
    pub gateway_jwt_encrypted: String,
    /// 文字起こしのカスタム接続先 (OpenAI 互換) のキー。ローカルサーバーなら空でよい。
    #[serde(rename = "customTranscribeApiKeyEncrypted", default)]
    pub custom_transcribe_api_key_encrypted: String,
    /// 整文・翻訳のカスタム接続先 (OpenAI 互換) のキー。ローカルサーバーなら空でよい。
    #[serde(rename = "customLlmApiKeyEncrypted", default)]
    pub custom_llm_api_key_encrypted: String,
}

/// 復号済み API キーバンドル。フィールド単位に Option を持ち、片方だけ復号失敗しても他方は使える。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsBundle {
    pub groq_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    /// 文字起こしのカスタム接続先のキー (任意)。
    pub custom_transcribe_api_key: Option<String>,
    /// 整文・翻訳のカスタム接続先のキー (任意)。
    pub custom_llm_api_key: Option<String>,
    /// 旧版のゲートウェイのログイン情報がファイルに残っていたか (値は持たない)。
    pub had_legacy_gateway_login: bool,
}

/// 平文 → DPAPI → Base64 → JSON。
/// None・空文字のフィールドは空文字列として保存 (v1 と同じ振る舞い)。
/// 旧版のゲートウェイのログイン情報は書き出さない (= 保存すると消える)。
pub fn save_secrets(path: &Path, bundle: &SecretsBundle) -> Result<(), CryptoError> {
    fn enc(v: &Option<String>) -> Result<String, CryptoError> {
        Ok(match v.as_deref() {
            Some(s) if !s.is_empty() => B64.encode(protect_bytes(s.as_bytes())?),
            _ => String::new(),
        })
    }

    let machine = std::env::var("COMPUTERNAME").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();

    let secrets = SecretsFile {
        encrypted_at: Utc::now(),
        scope: "CurrentUser".to_string(),
        machine,
        user,
        groq_api_key_encrypted: enc(&bundle.groq_api_key)?,
        openai_api_key_encrypted: enc(&bundle.openai_api_key)?,
        gateway_jwt_encrypted: String::new(),
        custom_transcribe_api_key_encrypted: enc(&bundle.custom_transcribe_api_key)?,
        custom_llm_api_key_encrypted: enc(&bundle.custom_llm_api_key)?,
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(&secrets)?;
    fs::write(path, json)?;
    Ok(())
}

/// JSON → Base64 → DPAPI → 平文。
/// 各フィールドが空 / Base64 不正 / 復号失敗のいずれでも、当該フィールドのみ None に丸める (v1 仕様)。
/// ファイル自体が無い場合は Ok(None)。
pub fn load_secrets(path: &Path) -> Result<Option<SecretsBundle>, CryptoError> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    let file: SecretsFile = serde_json::from_str(&raw)?;

    Ok(Some(SecretsBundle {
        groq_api_key: decode_field(&file.groq_api_key_encrypted),
        openai_api_key: decode_field(&file.openai_api_key_encrypted),
        custom_transcribe_api_key: decode_field(&file.custom_transcribe_api_key_encrypted),
        custom_llm_api_key: decode_field(&file.custom_llm_api_key_encrypted),
        had_legacy_gateway_login: !file.gateway_jwt_encrypted.is_empty(),
    }))
}

/// Base64 + DPAPI 復号 + UTF-8 を 1 ステップ。途中で失敗したら None を返す (v1 の "" 返却に相当)。
fn decode_field(b64: &str) -> Option<String> {
    if b64.is_empty() {
        return None;
    }
    let enc = B64.decode(b64).ok()?;
    let plain = unprotect_bytes(&enc).ok()?;
    String::from_utf8(plain).ok()
}

/// `%APPDATA%/RushVoice/config.secrets.encrypted.json` の標準パス。
/// Windows アプリ慣例。config.db (config_db::default_config_db_path) と同じディレクトリ。
/// APPDATA 未設定時はカレントディレクトリ配下にフォールバック (テスト時用)。
pub fn default_secrets_path() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(app) => PathBuf::from(app)
            .join("RushVoice")
            .join("config.secrets.encrypted.json"),
        Err(_) => PathBuf::from("RushVoice").join("config.secrets.encrypted.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn protect_unprotect_roundtrip_ascii() {
        let plain = b"gsk_test_dummy_key_1234567890";
        let enc = protect_bytes(plain).expect("protect");
        assert_ne!(enc.as_slice(), plain, "暗号文は平文と一致してはならない");
        let dec = unprotect_bytes(&enc).expect("unprotect");
        assert_eq!(dec, plain);
    }

    #[test]
    fn protect_unprotect_roundtrip_utf8() {
        let plain = "オリーブDPAPIテスト🔐".as_bytes();
        let enc = protect_bytes(plain).expect("protect");
        let dec = unprotect_bytes(&enc).expect("unprotect");
        assert_eq!(dec, plain);
    }

    #[test]
    fn protect_empty_returns_empty() {
        // 空入力は API を呼ばずに空 Vec を返す
        let enc = protect_bytes(&[]).expect("protect empty");
        assert!(enc.is_empty());
        let dec = unprotect_bytes(&[]).expect("unprotect empty");
        assert!(dec.is_empty());
    }

    #[test]
    fn unprotect_garbage_returns_error() {
        // 適当なバイト列は DPAPI 復号で失敗するはず
        let garbage = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let result = unprotect_bytes(&garbage);
        assert!(result.is_err(), "ランダムバイトは復号失敗すべき");
    }

    #[test]
    fn decode_field_handles_empty_and_garbage() {
        assert_eq!(decode_field(""), None);
        assert_eq!(decode_field("!!!not_base64!!!"), None);
        // 有効な Base64 だが DPAPI 不正 → None
        let bogus_b64 = B64.encode(b"random_bytes_not_dpapi_payload");
        assert_eq!(decode_field(&bogus_b64), None);
    }

    #[test]
    fn save_load_roundtrip_all_keys() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_all");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let _ = fs::remove_file(&path);

        let b = SecretsBundle {
            groq_api_key: Some("dummy_groq_value".into()),
            openai_api_key: Some("dummy_openai_value".into()),
            custom_transcribe_api_key: Some("dummy_custom_asr".into()),
            custom_llm_api_key: Some("dummy_custom_llm".into()),
            had_legacy_gateway_login: false,
        };
        save_secrets(&path, &b).expect("save");
        let loaded = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(loaded, b);

        let _ = fs::remove_file(&path);
    }

    /// 旧版のゲートウェイのログイン情報が残ったファイル: 読めて、印が立ち、保存し直すと消える。
    #[test]
    fn legacy_gateway_login_is_detected_and_dropped_on_save() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_legacy_gateway");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let groq_enc = B64.encode(protect_bytes(b"dummy_groq").expect("protect"));
        let jwt_enc = B64.encode(protect_bytes(b"dummy.jwt.value").expect("protect"));
        let json = format!(
            r#"{{"encryptedAt":"2026-01-01T00:00:00Z","scope":"CurrentUser","machine":"m","user":"u","groqApiKeyEncrypted":"{}","openAiApiKeyEncrypted":"","gatewayJwtEncrypted":"{}"}}"#,
            groq_enc, jwt_enc
        );
        fs::write(&path, json).expect("write");
        let loaded = load_secrets(&path).expect("load").expect("Some");
        assert!(loaded.had_legacy_gateway_login);
        assert_eq!(loaded.groq_api_key.as_deref(), Some("dummy_groq"));

        save_secrets(&path, &loaded).expect("save");
        let raw = fs::read_to_string(&path).expect("read");
        assert!(!raw.contains("gatewayJwtEncrypted"), "保存し直したら旧ログイン情報は書かれない");
        let again = load_secrets(&path).expect("load 2").expect("Some");
        assert!(!again.had_legacy_gateway_login);
        assert_eq!(again.groq_api_key.as_deref(), Some("dummy_groq"));
        let _ = fs::remove_file(&path);
    }

    /// 後方互換: 追加フィールドが無い v1 の JSON も読める (serde default)。
    #[test]
    fn load_legacy_v1_json() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_legacy");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let groq_enc = B64.encode(protect_bytes(b"dummy_legacy").expect("protect"));
        let json = format!(
            r#"{{"encryptedAt":"2026-01-01T00:00:00Z","scope":"CurrentUser","machine":"m","user":"u","groqApiKeyEncrypted":"{}","openAiApiKeyEncrypted":""}}"#,
            groq_enc
        );
        fs::write(&path, json).expect("write");
        let loaded = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(loaded.groq_api_key.as_deref(), Some("dummy_legacy"));
        assert!(!loaded.had_legacy_gateway_login);
        assert_eq!(loaded.custom_llm_api_key, None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_load_roundtrip_groq_only() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_groq");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let _ = fs::remove_file(&path);

        let only_groq = SecretsBundle {
            groq_api_key: Some("gsk_only_groq".into()),
            ..Default::default()
        };
        save_secrets(&path, &only_groq).expect("save");
        let bundle = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_only_groq"));
        assert_eq!(bundle.openai_api_key, None);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_none() {
        let path = env::temp_dir().join("rushvoice_dpapi_nonexistent_xyz.json");
        let _ = fs::remove_file(&path);
        let result = load_secrets(&path).expect("load");
        assert!(result.is_none(), "存在しないファイルは Ok(None)");
    }
}
