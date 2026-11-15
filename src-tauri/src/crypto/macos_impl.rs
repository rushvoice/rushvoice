// crypto/macos_impl.rs — Phase 5 Step C-3: macOS Keychain Services 実装
//
// Phase 5 Step A-3 で追加したスタブを security-framework の `passwords` モジュール
// (set/get/delete_generic_password) ベースの本実装に置き換え。
//
// 設計:
//   - Keychain Generic Password として 2 件 (groq_api_key / openai_api_key) を保存
//     - service: "com.olive.rushvoice"
//     - account: "groq_api_key" / "openai_api_key"
//   - set_generic_password は upsert (create or update) 動作なので SecItemUpdate 分岐不要
//   - 取得失敗 (errSecItemNotFound = -25300) は当該フィールドのみ None に丸める
//     → 起動継続可 (env var / 平文 config フォールバック経路を辿れる)
//   - 保存時は SecretsFile を JSON でメタデータとして path にも書き出す
//     (encrypted フィールドは "(stored in macOS Keychain)" プレースホルダ・Windows 版互換)
//
// protect_bytes / unprotect_bytes:
//   Keychain は key-value 抽象であり「任意バイト列を端末固有鍵で暗号化/復号する DPAPI 風 API」
//   ではない。呼出元 (lib.rs / commands.rs / migration.rs) はこの 2 関数を直接使っておらず
//   (windows_impl 内部から save_secrets / load_secrets が呼ぶのみ)、macOS 側で内部呼出も
//   発生しないため、これら 2 関数は明示 Err で「macOS では使えない」を表明する設計。
//   将来呼ばれた場合に意味不明な挙動を防ぐ意図。
//
// TCC キーチェーン権限:
//   初回 SecItemAdd / SecItemCopyMatching 呼び出し時に
//   "RushVoice wants to use your confidential information stored in <keychain>"
//   プロンプトが表示される。
//   - "Always Allow" を選ぶと code signing identity 単位で永続許可
//   - debug build (ad-hoc signed) は再ビルドごとに hash が変わり再プロンプトになる場合あり
//   - リリース build は Developer ID Application 署名で安定
//   - 「常に許可」誘導 UI は Phase 5 別タスクの UI 層で実装
//
// バージョン: security-framework 3.7.0 (Cargo.lock transitive と一致)

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use security_framework_sys::base::errSecItemNotFound;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Keychain item の service 名 (Reverse-DNS 慣例)。
/// `~/Library/Keychains/login.keychain-db` 内で本プロダクトの全 entry を識別。
const SERVICE: &str = "com.olive.rushvoice";

/// Keychain item の account 名。
/// 同じ service 配下に複数アカウントを並べて区別する。
const ACCOUNT_GROQ: &str = "groq_api_key";
const ACCOUNT_OPENAI: &str = "openai_api_key";
/// 旧版の RushVoice アカウント (ゲートウェイ) のログイン情報。2026-11-15 に終了。
/// 読むのは「残っていたか」の判定だけ。保存のたびに消す。
const ACCOUNT_GATEWAY_JWT: &str = "gateway_jwt";
/// カスタム接続先 (OpenAI 互換) のキー。ローカルサーバーなら無くてよい。
const ACCOUNT_CUSTOM_TRANSCRIBE: &str = "custom_transcribe_api_key";
const ACCOUNT_CUSTOM_LLM: &str = "custom_llm_api_key";

/// SecretsFile.encrypted フィールドに置くプレースホルダ。
/// 実 API キーは Keychain にあるため、ファイル側には保存しない。
/// (Windows DPAPI 版は Base64(DPAPI BLOB) を入れるが、macOS では Keychain serviceName 参照のみ)
const KEYCHAIN_PLACEHOLDER: &str = "__stored_in_macos_keychain__";

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("macOS Keychain error: {0}")]
    Keychain(String),
    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// v1 互換 JSON スキーマ (Windows 実装と同一定義)。
///
/// macOS 版では `*_encrypted` フィールドに実 BLOB は入らず、
/// `KEYCHAIN_PLACEHOLDER` 文字列が書かれる (Keychain serviceName 参照のみ)。
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
    /// 旧版のゲートウェイのログイン情報 (読むだけ・書き出さない)。
    #[serde(rename = "gatewayJwtEncrypted", default, skip_serializing)]
    pub gateway_jwt_encrypted: String,
    #[serde(rename = "customTranscribeApiKeyEncrypted", default)]
    pub custom_transcribe_api_key_encrypted: String,
    #[serde(rename = "customLlmApiKeyEncrypted", default)]
    pub custom_llm_api_key_encrypted: String,
}

/// 復号済み API キーバンドル (Windows 実装と同一定義)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsBundle {
    pub groq_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    /// 文字起こしのカスタム接続先のキー (任意)。
    pub custom_transcribe_api_key: Option<String>,
    /// 整文・翻訳のカスタム接続先のキー (任意)。
    pub custom_llm_api_key: Option<String>,
    /// 旧版のゲートウェイのログイン情報が Keychain に残っていたか (値は持たない)。
    pub had_legacy_gateway_login: bool,
}

/// macOS では Keychain が key-value 抽象であり、任意バイト列を端末固有鍵で
/// 暗号化/復号する DPAPI 風 API を提供しない。呼出元は save_secrets / load_secrets を
/// 経由するため本関数は内部からも外部からも呼ばれない (lib.rs / commands.rs / migration.rs
/// で grep 済 / 2026-05-28 確認)。誤呼出を検出するため明示 Err。
pub fn protect_bytes(_plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
    Err(CryptoError::Keychain(
        "protect_bytes is not supported on macOS; use save_secrets/load_secrets instead".into(),
    ))
}

/// 同上理由で明示 Err。
pub fn unprotect_bytes(_enc: &[u8]) -> Result<Vec<u8>, CryptoError> {
    Err(CryptoError::Keychain(
        "unprotect_bytes is not supported on macOS; use save_secrets/load_secrets instead".into(),
    ))
}

/// 平文 API キー → Keychain 保存 + SecretsFile メタデータ書出。
///
/// - 各キーが Some(non-empty) のときだけ Keychain に書込 (`set_generic_password` は upsert)
/// - None や空文字列のときは既存 Keychain item を削除 (`delete_generic_password`)
///   - 既存無しのとき errSecItemNotFound が返るのでそれは無視
/// - SecretsFile はメタデータ (timestamp + machine + user) としてファイルに残す
///   - encrypted フィールドには KEYCHAIN_PLACEHOLDER を入れる (Windows 版 BLOB と同じ位置)
pub fn save_secrets(path: &Path, bundle: &SecretsBundle) -> Result<(), CryptoError> {
    let groq_api_key = bundle.groq_api_key.as_deref();
    let openai_api_key = bundle.openai_api_key.as_deref();
    let custom_transcribe = bundle.custom_transcribe_api_key.as_deref();
    let custom_llm = bundle.custom_llm_api_key.as_deref();
    write_or_delete(ACCOUNT_GROQ, groq_api_key)?;
    write_or_delete(ACCOUNT_OPENAI, openai_api_key)?;
    write_or_delete(ACCOUNT_CUSTOM_TRANSCRIBE, custom_transcribe)?;
    write_or_delete(ACCOUNT_CUSTOM_LLM, custom_llm)?;
    // 旧版のゲートウェイのログイン情報は保存のたびに消す
    write_or_delete(ACCOUNT_GATEWAY_JWT, None)?;

    let machine = hostname_macos();
    let user = std::env::var("USER").unwrap_or_default();

    // ファイルに残すのはあくまでメタデータ。実 API キーは Keychain 側。
    let secrets = SecretsFile {
        encrypted_at: Utc::now(),
        scope: "CurrentUser".to_string(),
        machine,
        user,
        groq_api_key_encrypted: presence_marker(groq_api_key),
        openai_api_key_encrypted: presence_marker(openai_api_key),
        gateway_jwt_encrypted: String::new(),
        custom_transcribe_api_key_encrypted: presence_marker(custom_transcribe),
        custom_llm_api_key_encrypted: presence_marker(custom_llm),
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

/// Keychain から API キー取得。
///
/// - 取得失敗 (errSecItemNotFound) は当該フィールドのみ None に丸める
/// - UTF-8 不正・他の Keychain エラーも当該フィールドのみ None に丸める
///   (Windows 版 `load_secrets` が「片方失敗しても他方は使える」設計と同じ)
/// - ファイル自体が無く Keychain にも何も無い場合は Ok(None) を返す
///   (Windows 版「ファイル無し→Ok(None)」と同じ起動可能性を保証)
pub fn load_secrets(_path: &Path) -> Result<Option<SecretsBundle>, CryptoError> {
    let bundle = SecretsBundle {
        groq_api_key: load_account(ACCOUNT_GROQ),
        openai_api_key: load_account(ACCOUNT_OPENAI),
        custom_transcribe_api_key: load_account(ACCOUNT_CUSTOM_TRANSCRIBE),
        custom_llm_api_key: load_account(ACCOUNT_CUSTOM_LLM),
        had_legacy_gateway_login: load_account(ACCOUNT_GATEWAY_JWT).is_some(),
    };

    // 全部無いなら従来通り「未設定扱い」
    if bundle == SecretsBundle::default() {
        return Ok(None);
    }
    Ok(Some(bundle))
}

/// macOS 標準の secrets メタデータ保存先パス。
///
/// `~/Library/Application Support/RushVoice/config.secrets.encrypted.json`
/// 実 API キーは Keychain 側 (本ファイルにはメタデータのみ)。
/// HOME 未設定時はカレントディレクトリ配下にフォールバック (テスト用)。
pub fn default_secrets_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("RushVoice")
            .join("config.secrets.encrypted.json"),
        Err(_) => PathBuf::from("RushVoice").join("config.secrets.encrypted.json"),
    }
}

// ---------------------------------------------------------------------------
// internal helpers
// ---------------------------------------------------------------------------

/// 値があれば upsert・無ければ既存 entry を削除。
fn write_or_delete(account: &str, value: Option<&str>) -> Result<(), CryptoError> {
    match value {
        Some(s) if !s.is_empty() => set_generic_password(SERVICE, account, s.as_bytes())
            .map_err(|e| CryptoError::Keychain(format!("set {}: {}", account, e))),
        _ => match delete_generic_password(SERVICE, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == errSecItemNotFound => Ok(()),
            Err(e) => Err(CryptoError::Keychain(format!(
                "delete {}: {}",
                account, e
            ))),
        },
    }
}

/// 1 アカウントを取得して UTF-8 文字列に変換。失敗は全て None。
fn load_account(account: &str) -> Option<String> {
    match get_generic_password(SERVICE, account) {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(_) => None, // errSecItemNotFound も他のエラーも全て None 扱い
    }
}

/// SecretsFile のフィールド表示用マーカー (実 BLOB は Keychain にあるため非保持)。
fn presence_marker(value: Option<&str>) -> String {
    match value {
        Some(s) if !s.is_empty() => KEYCHAIN_PLACEHOLDER.to_string(),
        _ => String::new(),
    }
}

/// macOS の hostname 取得。失敗時は空文字。
/// std には直接 API が無いため `HOSTNAME` env が立っていれば利用 (シェル依存)、
/// そうでなければ空にして SecretsFile に書き込む (BLOB 不要のメタ情報なので影響軽微)。
fn hostname_macos() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default()
}

// 実際のログインキーチェーンを書き換えるため、通常の `cargo test` では動かさない (#[ignore])。
// CI (test-macos-keychain.yml) で使い捨てのキーチェーンを既定にしてから `--ignored` で動かす。
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rushvoice-keychain-test-{}.json",
            std::process::id()
        ))
    }

    #[test]
    #[ignore]
    fn keychain_round_trip_and_gateway_removal() {
        let path = tmp_path();
        // 旧版のログイン情報が残っている状態を作る
        set_generic_password(SERVICE, ACCOUNT_GATEWAY_JWT, b"legacy").unwrap();
        let before = load_secrets(&path).unwrap().unwrap();
        assert!(before.had_legacy_gateway_login);

        let bundle = SecretsBundle {
            groq_api_key: Some("test-groq".into()),
            openai_api_key: None,
            custom_transcribe_api_key: None,
            custom_llm_api_key: Some("test-custom-llm".into()),
            had_legacy_gateway_login: false,
        };
        save_secrets(&path, &bundle).unwrap();

        let loaded = load_secrets(&path).unwrap().unwrap();
        assert_eq!(loaded, bundle, "保存した値が読める・旧ログイン情報は消える");

        let meta = fs::read_to_string(&path).unwrap();
        assert!(!meta.contains("test-groq"), "メタデータに値を書かない");
        assert!(!meta.contains("gatewayJwtEncrypted"));

        // 空で保存すると消え、未設定扱いに戻る
        save_secrets(&path, &SecretsBundle::default()).unwrap();
        assert_eq!(load_secrets(&path).unwrap(), None);
        let _ = fs::remove_file(&path);
    }
}
