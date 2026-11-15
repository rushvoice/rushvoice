// clipboard/macos_impl.rs — macOS 実装 (Phase 5 Step C-1)
//
// Windows 版 (windows_impl.rs) と完全に同じ公開 API を提供する macOS 実装。
// 呼出元 (commands.rs / lib.rs) は cfg(target_os) で出し分け済みのため変更不要。
//
// 設計:
//   - クリップボード書込/読出 : NSPasteboard (Cocoa AppKit)
//   - 修飾キー付きキーイベント : Quartz Event Services (core-graphics の CGEvent)
//   - Cmd+V を 4 イベント (Cmd down → V down → V up → Cmd up) で送信
//
// 仕様 (Windows 版に準ずる):
//   - 文字起こし結果を pasteboard に UTF-8 (NSString) で書き込み
//   - CGEvent で Cmd+V を送信してアクティブアプリにペースト
//   - PA-02: 旧 pasteboard 退避 → 新テキスト貼付 → 500ms 後に非同期復元
//   - paste_text: copy → 30ms sleep → send_ctrl_v (Cmd+V) のフロー
//
// 権限 (重要):
//   - CGEvent.post は **TCC アクセシビリティ権限** が必要
//     (「システム設定 > プライバシーとセキュリティ > アクセシビリティ」で
//      RushVoice.app を許可しないと送信されない・初回 OS プロンプト発生)
//   - NSPasteboard 書込/読出は権限不要 (sandbox 外であれば誰でも可)
//   - 詳細は Phase 5 計画書 §7 (TCC 申請の UX) 参照
//
// 関数名について:
//   send_ctrl_v は Windows 版とのシグネチャ互換のため名前を維持。
//   macOS 実装内では実際に Cmd+V を送信する。中立名 (send_paste_shortcut 等) への
//   リネーム判断は呼出側を巻き込む変更となるため後続セッションに委ねる。
//
// スレッド安全性:
//   - NSPasteboard.generalPasteboard はメインスレッド以外からも使用可能
//     (Apple ドキュメント: Pasteboard operations are thread-safe)
//   - CGEvent.post も任意スレッドから呼出可能
//   - tokio::spawn 内 (paste_text_with_restore の復元処理) も問題なし

#![cfg(target_os = "macos")]

use std::time::Duration;

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use log::warn; // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

// macOS 仮想キーコード (HIToolbox/Events.h より)
const KVK_ANSI_V: u16 = 0x09;

/// テキストを macOS pasteboard (NSPasteboardTypeString) にコピーする。
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    autoreleasepool(|_| unsafe {
        let pasteboard = NSPasteboard::generalPasteboard();
        // clearContents は次回 write 用の changeCount を進めて既存内容を消す
        pasteboard.clearContents();
        let ns_text = NSString::from_str(text);
        let ok = pasteboard.setString_forType(&ns_text, NSPasteboardTypeString);
        if ok {
            Ok(())
        } else {
            Err("NSPasteboard setString:forType: returned false".to_string())
        }
    })
}

/// Cmd+V を CGEvent で送信する (アクティブアプリにペースト)。
///
/// NOTE: Windows 版との互換性のため関数名は `send_ctrl_v` のまま。
///       macOS では Cmd+V を送信する。
///
/// TCC アクセシビリティ権限が無い場合、post は失敗しないがイベントが届かない
/// (OS が黙って破棄)。事前に権限ガイダンスを出す責務は UI 層にある。
pub fn send_ctrl_v() -> Result<(), String> {
    // CombinedSessionState: 物理キーボード + ソフトイベントを統合した状態を継承
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| "CGEventSource::new failed".to_string())?;

    // 4 イベントを生成 (Cmd down → V down → V up → Cmd up)
    // 修飾キー down/up 自体は (Win版同様) 個別の key event として送る方式を取らず、
    // V キーの flags に CGEventFlagCommand を載せる方が一般的かつ確実。
    // 但し flags 経路だけだと一部アプリが Cmd を検知し損ねるため、安全策として
    // Cmd 自体の down/up も flagsChanged として送る (Carbon 時代からの定番)。
    let v_down = CGEvent::new_keyboard_event(source.clone(), KVK_ANSI_V, true)
        .map_err(|_| "CGEvent::new_keyboard_event (V down) failed".to_string())?;
    v_down.set_flags(CGEventFlags::CGEventFlagCommand);

    let v_up = CGEvent::new_keyboard_event(source, KVK_ANSI_V, false)
        .map_err(|_| "CGEvent::new_keyboard_event (V up) failed".to_string())?;
    v_up.set_flags(CGEventFlags::CGEventFlagCommand);

    // HID tap (= HIDSystem) に post: ハードウェアレベル相当でアクティブアプリに届く
    v_down.post(CGEventTapLocation::HID);
    v_up.post(CGEventTapLocation::HID);

    Ok(())
}

/// 文字起こし結果をクリップボードに置いて自動的に Cmd+V でペーストする。
/// clipboard 書き込み後にアプリが読み取れるよう 30ms 待機。
pub fn paste_text(text: &str) -> Result<(), String> {
    copy_to_clipboard(text)?;
    std::thread::sleep(Duration::from_millis(30));
    send_ctrl_v()
}

// ---------------------------------------------------------------------------
// PA-02: clipboard Push / Pop
// ---------------------------------------------------------------------------

/// PA-02: 現在の pasteboard の文字列内容を退避する。
/// - NSPasteboardTypeString 以外 (画像等) の場合は None。
/// - clipboard が空の場合も None。
pub fn get_clipboard_unicode() -> Option<String> {
    autoreleasepool(|_| unsafe {
        let pasteboard = NSPasteboard::generalPasteboard();
        let ns_string = pasteboard.stringForType(NSPasteboardTypeString)?;
        Some(ns_string.to_string())
    })
}

// ---------------------------------------------------------------------------
// PA-04 (2026-08-06): Windows 側で先行して入れた復元ガードを macOS へ移植
// ---------------------------------------------------------------------------
//
// 移植前の macOS 実装には、Windows で 2026-07-22 に修正した競合対策
// (世代カウンタ・内容一致ガード) が入っておらず、次の 2 つの実害があった。
//   1) 連続発話で前回の復元タイマーが今回の貼付テキストを上書きする
//      (「前の内容が貼られる」)
//   2) 500ms 以内にユーザーが別の内容をコピーすると、それを旧内容で踏み潰す
// 加えて Windows PA-04 と同じく、復元を見送る際に退避内容を捨てていたため
// 連続発話でユーザーのコピー内容が失われる問題もあった。
//
// なお全フォーマット退避 (画像・ファイル) は Windows 側のみ実装済み。
// macOS の NSPasteboard 版は実機検証ができないため本セッションでは見送る
// (テキスト以外は従来どおり復元されない)。

static PASTE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 復元待ちのユーザー内容。世代チェックで復元を見送っても**捨てずに残す**ので、
/// 連続発話が途切れた時点で最初にコピーした内容が戻る。
static PENDING_RESTORE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// RushVoice が最後に pasteboard へ置いた文字起こしテキスト。
static LAST_PASTED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// PA-02/PA-04: 文字起こし結果をペーストし、一定時間後に旧 pasteboard 内容を非同期復元する。
///
/// 手順:
///   A) 旧 pasteboard を退避 (NSPasteboardTypeString のみ)
///      ただし自分が直前に貼った残骸なら退避を更新しない
///   B) 新テキストを copy + 30ms 待機 + Cmd+V
///   C) 500ms 後に復元。ただし
///      ガード1: 新しい貼付が走っていたら見送る (退避内容は次の世代へ引き継ぐ)
///      ガード2: pasteboard が自分の貼付テキスト以外に変わっていたら復元しない
///
/// 呼出元 (lib.rs handle_hotkey_event) は tauri::async_runtime::spawn 内なので
/// tokio::spawn は runtime 保証済み。
pub fn paste_text_with_restore(text: &str) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    // Step A: 旧 pasteboard 退避
    {
        let current = get_clipboard_unicode();
        let last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if super::should_replace_backup(current.as_deref(), last.as_deref()) {
            if current.is_none() {
                warn!(
                    target: "rushvoice.clipboard.pa04",
                    "PA-04: backup skipped — pasteboard has no NSPasteboardTypeString content"
                );
            }
            let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
            *pending = current;
        }
    }

    let my_gen = PASTE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    // Step B: 新テキストを copy + Cmd+V (既存 paste_text 経路)
    copy_to_clipboard(text)?;
    {
        let mut last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner());
        *last = Some(text.to_string());
    }
    std::thread::sleep(Duration::from_millis(30));
    send_ctrl_v()?;

    // Step C: 500ms 後に旧内容を復元 (fire-and-forget)
    let pasted = text.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // ガード1: 復元待ちの間に新しい貼付が走った → 最新世代に任せる
        if PASTE_GENERATION.load(Ordering::SeqCst) != my_gen {
            log::info!("[paste] PA-04 restore deferred to newer paste (gen {})", my_gen);
            return;
        }
        // ガード2: pasteboard が貼付テキスト以外に変わっていた
        //   → ユーザーが新しくコピーしたので退避は破棄する
        match get_clipboard_unicode() {
            Some(cur) if cur == pasted => {}
            _ => {
                log::info!("[paste] PA-04 restore skipped: pasteboard changed externally (gen {})", my_gen);
                let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
                *pending = None;
                return;
            }
        }
        let old = {
            let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
            pending.take()
        };
        let Some(old) = old else {
            return;
        };
        match copy_to_clipboard(&old) {
            Ok(()) => {
                let mut last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner());
                *last = None;
                log::info!("[paste] PA-04 pasteboard restored (gen {})", my_gen);
            }
            Err(e) => warn!(
                target: "rushvoice.clipboard.pa04",
                "PA-04: pasteboard restore failed: {}",
                e
            ),
        }
    });
    Ok(())
}
