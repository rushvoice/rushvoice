// keyboard_hook/mod.rs — Phase 5 Step A-2: クロスプラットフォーム抽象化
//
// 設計方針 (Step A-1 clipboard と同型):
//   - trait による抽象化は採用しない (YAGNI)
//   - #[cfg(target_os = "...")] で同名 API の実装を出し分け
//   - 呼出元 (lib.rs) は `crate::keyboard_hook::{HotkeyCallback, HotkeyEvent, KeyboardHook}`
//     のまま変更不要
//
// 履歴:
//   - Phase 1 Day 6 Stage 2-C-2: 初版 (Windows WH_KEYBOARD_LL 実装)
//   - Phase 1 Day 7 Stage 2-D HK-06: ESC キャンセル + set_recording 追加
//   - Phase 5 Step A-2: Windows 実装を windows_impl に分離 + macOS スタブ追加
//
// 公開 API (プラットフォーム共通シグネチャ):
//   - enum HotkeyEvent { Pressed, Released, Cancel }
//   - type HotkeyCallback = Arc<dyn Fn(HotkeyEvent) + Send + Sync + 'static>
//   - struct KeyboardHook
//       - pub fn start(config: HotkeyConfig, callback: HotkeyCallback) -> Result<Self, String>
//       - pub fn update_config(&self, new_config: HotkeyConfig) -> Result<(), String>
//       - pub fn set_recording(&self, val: bool) -> Result<(), String>
//   - pub fn check_modifiers_match(modifiers: &[String], main_vk: u32) -> bool
//
// macOS 実装メモ (Step C 宿題):
//   - WH_KEYBOARD_LL に相当する低レベルキーボードフックは CGEventTap を使う
//     (CGEventTapCreate + CGEventTapEnable + CFRunLoop)
//   - 修飾キー単独ホットキー検出は kCGEventFlagsChanged で modifier flag 差分監視
//   - check_modifiers_match の GetAsyncKeyState 相当は CGEventSourceKeyState
//   - ESC キャンセル時の「他アプリへの伝播抑止」は CGEventTap callback で nil 返却

#[cfg(target_os = "windows")]
mod windows_impl;

#[cfg(target_os = "windows")]
pub use windows_impl::{check_modifiers_match, HotkeyCallback, HotkeyEvent, KeyboardHook};

#[cfg(target_os = "macos")]
mod macos_impl;

#[cfg(target_os = "macos")]
pub use macos_impl::{check_modifiers_match, HotkeyCallback, HotkeyEvent, KeyboardHook};
