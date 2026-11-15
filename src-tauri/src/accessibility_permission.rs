//! macOS アクセシビリティ権限ダイアログを明示的に発火するモジュール
//! (mac/mic-permission-prompt 2026-06-15 追加・ホットキー機能修復)。
//!
//! # 背景
//!
//! グローバルホットキー (CGEventTap) を作成するには macOS アクセシビリティ権限が必須。
//! 権限が無いと `CGEventTapCreate` が NULL を返し、F12 等のホットキーが完全に動かない:
//!
//!   [hotkey] KeyboardHook::start failed: accessibility permission required
//!            (CGEventTapCreate returned NULL)
//!
//! しかも macOS は CGEventTap 作成失敗時に **TCC ダイアログを自動で出さない**。
//! ユーザーは System Settings → Privacy → Accessibility を自力で開いて、
//! アプリを手動で追加 (+) → トグル ON する必要があるが、macOS 14+ で当該リストは
//! アプリが「アクセシビリティを使いたい」と一度宣言した後でないと表示されない。
//! → 完全に詰む。
//!
//! # 対策
//!
//! `AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true})` を起動時に
//! 呼ぶ。これは権限チェック関数だが、`kAXTrustedCheckOptionPrompt = true` を渡すと:
//!
//!   - 既に許可済なら true を返す (副作用なし)
//!   - 未許可なら false を返しつつ、**OS が「アクセシビリティを許可してください」**
//!     **ダイアログを表示**する (Open System Settings ボタン付き)
//!
//! ユーザーが Open System Settings → トグル ON → アプリ再起動 で hotkey が動く。
//!
//! 注: 入力監視 (Input Monitoring) はホットキー実装が CGEventTap (Accessibility)
//! を使っているので別途不要 (Accessibility 内包)。
//!
//! # 参考
//!
//! - Apple Doc: <https://developer.apple.com/documentation/applicationservices/1462089-axisprocesstrustedwithoptions>

/// 起動時に 1 度だけ呼ぶ。許可済なら no-op、未許可なら macOS が許可ダイアログを表示。
/// 呼び出しはどのスレッドからでもよい (ただし lib.rs では main thread から呼んでいる)。
///
/// 戻り値: 現時点で許可されているか (false でも、ダイアログを通じてユーザーが
/// あとから許可することは可能。ユーザーが許可した結果は次回起動で反映される)。
#[cfg(target_os = "macos")]
pub fn request_accessibility_permission() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};

    // ApplicationServices フレームワークの C 関数。
    // SAFETY: フレームワーク自体は OS 標準で常にロード済 (Cocoa app の前提)。
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        // kAXTrustedCheckOptionPrompt は static const CFStringRef。
        // 名前は "AXTrustedCheckOptionPrompt"。
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    // {kAXTrustedCheckOptionPrompt: true} 辞書を組み立て。
    // SAFETY: kAXTrustedCheckOptionPrompt はフレームワーク内で永続。
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key, value)]);

    // SAFETY: options は live CFDictionary。
    let trusted = unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };

    if trusted {
        log::info!("[accessibility] already trusted (hotkey 利用可能)");
    } else {
        log::warn!(
            "[accessibility] NOT trusted。OS がアクセシビリティ許可ダイアログを表示しているはずです。\
             ユーザーが System Settings → Privacy → Accessibility で RushVoice を ON にして\
             アプリを再起動するまで F12 ホットキーは動きません"
        );
    }
    trusted
}

/// 他 OS では no-op (常に true)。
/// Windows は WH_KEYBOARD_LL を使うため OS 権限不要。
#[cfg(not(target_os = "macos"))]
pub fn request_accessibility_permission() -> bool {
    true
}
