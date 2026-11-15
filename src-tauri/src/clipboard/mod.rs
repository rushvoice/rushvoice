// clipboard/mod.rs — Phase 5 Step A-1: クロスプラットフォーム抽象化
//
// 設計方針:
//   - trait による抽象化は採用しない (YAGNI)
//   - #[cfg(target_os = "...")] で同名関数の実装を出し分け
//   - 呼出元 (commands.rs / lib.rs) は `crate::clipboard::paste_text_with_restore(...)`
//     のまま変更不要
//
// 履歴:
//   - Phase 1 Day 6 Stage 2-C-2: 初版 (Windows 専用・SendInput Ctrl+V)
//   - Phase 1 Day 7 Stage 2-D PA-02: clipboard Push/Pop (500ms 非同期復元)
//   - Phase 5 Step A-1: Windows 実装を windows_impl に分離 + macOS スタブ追加
//
// 公開 API (プラットフォーム共通シグネチャ):
//   - copy_to_clipboard(text: &str) -> Result<(), String>
//   - send_ctrl_v() -> Result<(), String>
//     ※ macOS では Cmd+V を送信する想定 (関数名は互換性維持・Step C で中立名検討)
//   - paste_text(text: &str) -> Result<(), String>
//   - get_clipboard_unicode() -> Option<String>
//   - paste_text_with_restore(text: &str) -> Result<(), String>

/// 今のクリップボード内容を「ユーザーがコピーしたもの」として退避すべきかを判定する。
/// 自分が直前に貼った文字起こしテキストがそのまま残っているだけなら退避しない
/// (退避すると、ユーザーの元内容が文字起こしの残骸で上書きされてしまう)。
///
/// PA-04 (2026-08-06) で Windows / macOS 双方が使う共通判定。プラットフォーム非依存な
/// 純粋関数なのでここに置き、実装側から参照する。
pub(crate) fn should_replace_backup(current_text: Option<&str>, last_pasted: Option<&str>) -> bool {
    match (current_text, last_pasted) {
        (Some(cur), Some(last)) => cur != last,
        _ => true,
    }
}

#[cfg(target_os = "windows")]
mod windows_impl;

#[cfg(target_os = "windows")]
pub use windows_impl::{
    copy_to_clipboard, get_clipboard_unicode, paste_or_type_text, paste_text,
    paste_text_with_restore, send_ctrl_v,
};

#[cfg(target_os = "macos")]
mod macos_impl;

#[cfg(target_os = "macos")]
pub use macos_impl::{
    copy_to_clipboard, get_clipboard_unicode, paste_text, paste_text_with_restore, send_ctrl_v,
};

// macOS には Windows Terminal + prompt_toolkit のブラケットペースト不具合 (PA-03)
// が存在しないため、paste_or_type_text は常にクリップボード貼付にフォールバックする。
#[cfg(target_os = "macos")]
pub fn paste_or_type_text(text: &str) -> Result<(), String> {
    paste_text_with_restore(text)
}
