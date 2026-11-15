// crypto/mod.rs — Phase 5 Step A-3: クロスプラットフォーム抽象化
//
// 設計方針 (Step A-1 clipboard / Step A-2 keyboard_hook と同型):
//   - trait による抽象化は採用しない (YAGNI)
//   - #[cfg(target_os = "...")] で同名 API の実装を出し分け
//   - 呼出元 (lib.rs / commands.rs / migration.rs) は
//     `crate::crypto::{save_secrets, load_secrets, default_secrets_path, ...}`
//     のまま変更不要
//
// 履歴:
//   - Phase 1 Day 5 後半: 初版 (Windows DPAPI CryptProtectData / CryptUnprotectData)
//   - v1 互換 JSON スキーマ (Encrypt-ApiKey.ps1 / VoiceInputMvp.ps1 L45-66 互換)
//   - Phase 5 Step A-3: Windows 実装を windows_impl に分離 + macOS スタブ追加
//
// 公開 API (プラットフォーム共通シグネチャ):
//   - enum CryptoError
//   - struct SecretsFile (v1 互換 JSON スキーマ・camelCase)
//   - struct SecretsBundle (復号済み API キーバンドル)
//   - fn protect_bytes(plain: &[u8]) -> Result<Vec<u8>, CryptoError>
//   - fn unprotect_bytes(enc: &[u8]) -> Result<Vec<u8>, CryptoError>
//   - fn save_secrets(path, &SecretsBundle) -> Result<(), CryptoError>
//   - fn load_secrets(path) -> Result<Option<SecretsBundle>, CryptoError>
//   - fn default_secrets_path() -> PathBuf
//
// macOS 実装メモ (Step C 宿題):
//   - DPAPI (CurrentUser スコープ) 相当は macOS Keychain Services
//   - SecKeychainAddGenericPassword / SecKeychainFindGenericPassword
//     (もしくは macOS 10.10+ の SecItemAdd / SecItemCopyMatching kSecClassGenericPassword)
//   - スコープは「同じ macOS ユーザー・同じログイン Keychain」が DPAPI CurrentUser 相当
//   - 初回アクセス時に TCC キーチェーン権限プロンプト (許可/常に許可) が表示される
//     → アプリ起動フロー上の UX 検討が必要 (Step C で対応)
//   - v1 互換 JSON スキーマ (SecretsFile) は形式維持
//     (groqApiKeyEncrypted / openAiApiKeyEncrypted フィールド名は据置)
//     ただし保存される暗号文の中身は DPAPI BLOB ではなく Keychain serviceName 参照 or
//     Keychain から取り出した item を Base64 にした opaque blob となる
//     (端末を跨ぐと復号不能な点は DPAPI と同じ性質)

#[cfg(target_os = "windows")]
mod windows_impl;

#[cfg(target_os = "windows")]
pub use windows_impl::{
    default_secrets_path, load_secrets, protect_bytes, save_secrets, unprotect_bytes,
    CryptoError, SecretsBundle, SecretsFile,
};

#[cfg(target_os = "macos")]
mod macos_impl;

#[cfg(target_os = "macos")]
pub use macos_impl::{
    default_secrets_path, load_secrets, protect_bytes, save_secrets, unprotect_bytes,
    CryptoError, SecretsBundle, SecretsFile,
};
