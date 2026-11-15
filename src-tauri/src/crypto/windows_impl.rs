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
    /// Gateway ログイン (feature/gateway-login): メール+コードログインで得た JWT。
    /// v1 スキーマには無い追加フィールド (serde default で後方互換)。
    #[serde(rename = "gatewayJwtEncrypted", default)]
    pub gateway_jwt_encrypted: String,
}

/// 復号済み API キーバンドル。フィールド単位に Option を持ち、片方だけ復号失敗しても他方は使える。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsBundle {
    pub groq_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    /// Gateway ログイン JWT (feature/gateway-login)。ログに出さない。
    pub gateway_jwt: Option<String>,
}

/// 平文 → DPAPI → Base64 → JSON。
/// 入力は Option で、None のフィールドは空文字列として保存 (v1 と同じ振る舞い)。
pub fn save_secrets(
    path: &Path,
    groq_api_key: Option<&str>,
    openai_api_key: Option<&str>,
    gateway_jwt: Option<&str>,
) -> Result<(), CryptoError> {
    let groq_enc = match groq_api_key {
        Some(s) if !s.is_empty() => B64.encode(protect_bytes(s.as_bytes())?),
        _ => String::new(),
    };
    let openai_enc = match openai_api_key {
        Some(s) if !s.is_empty() => B64.encode(protect_bytes(s.as_bytes())?),
        _ => String::new(),
    };
    let jwt_enc = match gateway_jwt {
        Some(s) if !s.is_empty() => B64.encode(protect_bytes(s.as_bytes())?),
        _ => String::new(),
    };

    let machine = std::env::var("COMPUTERNAME").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();

    let secrets = SecretsFile {
        encrypted_at: Utc::now(),
        scope: "CurrentUser".to_string(),
        machine,
        user,
        groq_api_key_encrypted: groq_enc,
        openai_api_key_encrypted: openai_enc,
        gateway_jwt_encrypted: jwt_enc,
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

    let groq = decode_field(&file.groq_api_key_encrypted);
    let openai = decode_field(&file.openai_api_key_encrypted);
    let gateway_jwt = decode_field(&file.gateway_jwt_encrypted);

    Ok(Some(SecretsBundle {
        groq_api_key: groq,
        openai_api_key: openai,
        gateway_jwt,
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
    fn save_load_roundtrip_both_keys() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_both");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let _ = fs::remove_file(&path);

        save_secrets(
            &path,
            Some("gsk_dummy_groq_key"),
            Some("sk-dummy_openai_key"),
            None,
        )
        .expect("save");
        let bundle = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_dummy_groq_key"));
        assert_eq!(
            bundle.openai_api_key.as_deref(),
            Some("sk-dummy_openai_key")
        );
        assert_eq!(bundle.gateway_jwt, None);

        let _ = fs::remove_file(&path);
    }

    /// Gateway ログイン: JWT roundtrip + 削除 (None 上書き) を確認。
    #[test]
    fn save_load_roundtrip_gateway_jwt() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_gateway_jwt");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let _ = fs::remove_file(&path);

        save_secrets(&path, Some("gsk_dummy"), None, Some("dummy.jwt.value")).expect("save");
        let bundle = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(bundle.gateway_jwt.as_deref(), Some("dummy.jwt.value"));
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_dummy"));

        // ログアウト相当 (jwt=None) で JWT のみ消え、他キーは維持される
        save_secrets(
            &path,
            bundle.groq_api_key.as_deref(),
            bundle.openai_api_key.as_deref(),
            None,
        )
        .expect("save 2");
        let bundle2 = load_secrets(&path).expect("load 2").expect("Some");
        assert_eq!(bundle2.gateway_jwt, None);
        assert_eq!(bundle2.groq_api_key.as_deref(), Some("gsk_dummy"));

        let _ = fs::remove_file(&path);
    }

    /// 後方互換: gatewayJwtEncrypted フィールドが無い既存 JSON も読める (serde default)。
    #[test]
    fn load_legacy_json_without_gateway_field() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_legacy");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let groq_enc = B64.encode(protect_bytes(b"gsk_legacy").expect("protect"));
        let json = format!(
            r#"{{"encryptedAt":"2026-01-01T00:00:00Z","scope":"CurrentUser","machine":"m","user":"u","groqApiKeyEncrypted":"{}","openAiApiKeyEncrypted":""}}"#,
            groq_enc
        );
        fs::write(&path, json).expect("write");
        let bundle = load_secrets(&path).expect("load").expect("Some");
        assert_eq!(bundle.groq_api_key.as_deref(), Some("gsk_legacy"));
        assert_eq!(bundle.gateway_jwt, None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_load_roundtrip_groq_only() {
        let dir = env::temp_dir().join("rushvoice_dpapi_test_groq");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.secrets.encrypted.json");
        let _ = fs::remove_file(&path);

        save_secrets(&path, Some("gsk_only_groq"), None, None).expect("save");
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
