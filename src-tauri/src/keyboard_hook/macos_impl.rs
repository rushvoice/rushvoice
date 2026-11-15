// keyboard_hook/macos_impl.rs — Phase 5 Step C-2: macOS CGEventTap + CFRunLoop 実装
//
// Windows 版 (windows_impl.rs WH_KEYBOARD_LL) と完全に同じ公開 API を提供する macOS 実装。
// 呼出元 (lib.rs) は cfg(target_os) で出し分け済みのため変更不要。
//
// 設計:
//   - グローバルキーフック : CGEventTap (kCGSessionEventTap + kCGHeadInsertEventTap + Default)
//   - イベントループ      : 別 thread + CFRunLoop (CFRunLoopRun + CFRunLoopStop)
//   - 状態保持           : HOOK_STATE OnceLock<Mutex<Option<HookState>>> (Win 版と完全同型)
//   - 修飾キー状態        : CGEventSourceKeyState を直接 FFI で叩く (core-graphics 0.25 が
//                          binding 提供していないため本ファイル内で extern "C" 宣言)
//
// 仕様 (Win 版に準ずる):
//   - Toggle / PushToTalk 両モード
//   - 左右修飾キー区別 (kCGEventFlagsChanged + kVK_*_Control / kVK_*_Shift 等で identify)
//   - 修飾キー単独ホットキー (例 ControlRight 単体) は code_to_macos_keycode で keycode 解決
//   - ESC キャンセル (録音中のみ) は CallbackResult::Drop で他アプリへの伝播抑止
//   - repeat 抑止 (autoRepeat フィールド) で modifier 連発を吸収
//
// 権限 (TCC アクセシビリティ):
//   - CGEventTap は **アクセシビリティ権限** が必須
//   - 未許可だと CGEventTapCreate が NULL を返す → start() が Err を返す
//   - 設定アプリへの誘導 deeplink: x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility
//   - 権限取得 UI は本ファイルでは扱わない (Phase 5 別タスク。Err 文字列を UI 層で判定して表示)
//
// スレッド安全性:
//   - CGEventTap callback は CFRunLoop thread から呼ばれる (CGEvent も valid な所有権を持つ)
//   - HOOK_STATE は Mutex なので clone コールバック後 lock を解放してから発火 (Win と同型)
//
// Phase 5 Step A-2: スタブ版 (Err 即返却)
// Phase 5 Step C-2: 本実装 (CGEventTap + CFRunLoop)

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};

use crate::hotkey::{
    code_to_vk, parse_modifier_side, HotkeyConfig, HotkeyMode, ModifierKind, ModifierSide,
};

// ---------------------------------------------------------------------------
// CGEventSourceKeyState FFI (core-graphics 0.25 未バインド)
// ---------------------------------------------------------------------------
// Win 版 GetAsyncKeyState 相当。指定 keycode が現在押されているかを HID または
// CombinedSessionState 状態から取得する。
//   - stateID: CGEventSourceStateID (1 = HIDSystemState を採用 = 物理キーの直近状態)
//   - keycode: CGKeyCode (u16 / macOS HIToolbox Events.h の virtual key code)
//   - 戻り値:  押下中なら true
extern "C" {
    fn CGEventSourceKeyState(
        state_id: i32,
        key: u16,
    ) -> bool;
}

const CG_EVENT_SOURCE_STATE_HID: i32 = 1;

// ---------------------------------------------------------------------------
// 公開 API (Windows 版と完全同シグネチャ)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    /// HK-06: 録音中に ESC が押されたことを通知する。
    Cancel,
}

pub type HotkeyCallback = Arc<dyn Fn(HotkeyEvent) + Send + Sync + 'static>;

struct HookState {
    config: HotkeyConfig,
    callback: HotkeyCallback,
    /// Pressed イベント repeat 抑止
    is_currently_pressed: bool,
    /// HK-06: 録音中フラグ。true のときのみ ESC を Cancel として扱い他アプリへ伝播しない。
    is_recording: bool,
    /// 直近の修飾キー flags スナップショット (FlagsChanged 差分検出に使用)
    last_flags: CGEventFlags,
}

static HOOK_STATE: OnceLock<Mutex<Option<HookState>>> = OnceLock::new();

pub struct KeyboardHook {
    /// CFRunLoop を別 thread で稼働させる。停止時に stop() を呼ぶため Arc<Mutex<Option<>>>。
    runloop: Arc<Mutex<Option<CFRunLoop>>>,
    /// 念のため stop 二重発行防止
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl KeyboardHook {
    /// CGEventTap を起動する。
    /// TCC アクセシビリティ権限が無い場合 CGEventTapCreate が失敗するため、
    /// Err("accessibility permission required (CGEventTapCreate returned NULL)") を返す。
    pub fn start(config: HotkeyConfig, callback: HotkeyCallback) -> Result<Self, String> {
        let state_cell = HOOK_STATE.get_or_init(|| Mutex::new(None));
        {
            let mut guard = state_cell.lock().map_err(|e| e.to_string())?;
            *guard = Some(HookState {
                config,
                callback,
                is_currently_pressed: false,
                is_recording: false,
                last_flags: CGEventFlags::CGEventFlagNull,
            });
        }

        let runloop: Arc<Mutex<Option<CFRunLoop>>> = Arc::new(Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let runloop_for_thread = runloop.clone();

        // 初期化結果を main thread に返すための channel
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(0);

        let handle = thread::spawn(move || {
            // CGEventTap を作成 (session tap + head insert + default = active filter)
            // 監視: KeyDown / KeyUp / FlagsChanged の 3 種
            let tap_result = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                vec![
                    CGEventType::KeyDown,
                    CGEventType::KeyUp,
                    CGEventType::FlagsChanged,
                ],
                tap_callback,
            );

            let tap = match tap_result {
                Ok(t) => t,
                Err(()) => {
                    let _ = init_tx.send(Err(
                        "accessibility permission required (CGEventTapCreate returned NULL)"
                            .to_string(),
                    ));
                    return;
                }
            };

            // RunLoopSource を作って current thread の RunLoop に追加
            let loop_source = match tap.mach_port().create_runloop_source(0) {
                Ok(s) => s,
                Err(_) => {
                    let _ = init_tx.send(Err(
                        "CFMachPortCreateRunLoopSource failed".to_string()
                    ));
                    return;
                }
            };

            let current = CFRunLoop::get_current();
            current.add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
            tap.enable();

            // RunLoop を runloop_for_thread に格納してから RunLoop に入る
            {
                if let Ok(mut g) = runloop_for_thread.lock() {
                    *g = Some(current);
                }
            }

            let _ = init_tx.send(Ok(()));

            // CFRunLoopRun: stop() が呼ばれるまでブロック
            CFRunLoop::run_current();

            // CFRunLoop を抜けたら tap は Drop で CFMachPortInvalidate される
            drop(tap);
        });

        match init_rx.recv() {
            Ok(Ok(())) => Ok(KeyboardHook {
                runloop,
                stop_flag,
                handle: Some(handle),
            }),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("hook init channel closed: {}", e)),
        }
    }

    /// ホットキー設定を更新する。RunLoop thread はそのまま継続。
    /// is_currently_pressed は reset して、設定変更直後の押下状態誤検出を防ぐ。
    pub fn update_config(&self, new_config: HotkeyConfig) -> Result<(), String> {
        let state_cell = HOOK_STATE.get().ok_or("HOOK_STATE not initialized")?;
        let mut guard = state_cell.lock().map_err(|e| e.to_string())?;
        if let Some(state) = guard.as_mut() {
            state.config = new_config;
            state.is_currently_pressed = false;
        }
        Ok(())
    }

    /// HK-06: 録音状態フラグを更新する。
    /// - `true`: 録音開始後 → ESC を Cancel として扱い他アプリへ伝播しない (CallbackResult::Drop)
    /// - `false`: 録音停止/キャンセル/文字起こし終了後 → ESC を透過 (CallbackResult::Keep)
    pub fn set_recording(&self, val: bool) -> Result<(), String> {
        let state_cell = HOOK_STATE.get().ok_or("HOOK_STATE not initialized")?;
        let mut guard = state_cell.lock().map_err(|e| e.to_string())?;
        if let Some(state) = guard.as_mut() {
            state.is_recording = val;
        }
        Ok(())
    }

    /// RunLoop thread を停止させる。Drop でも呼ばれる。
    fn shutdown(&mut self) {
        if self.stop_flag.swap(true, Ordering::AcqRel) {
            return; // 二重 shutdown 防止
        }
        if let Ok(g) = self.runloop.lock() {
            if let Some(rl) = g.as_ref() {
                rl.stop();
            }
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// CGEventTap callback
// ---------------------------------------------------------------------------

/// Tap callback 本体。
/// Win 版 low_level_keyboard_proc と同じロジックフローを CGEvent ベースに置換。
fn tap_callback(
    _proxy: *const c_void,
    etype: CGEventType,
    event: &core_graphics::event::CGEvent,
) -> CallbackResult {
    // out-of-band: tap が disabled になったら enable し直す方が安全だが、
    // CallbackResult 経由では re-enable できないため Keep で透過 (運用上は再起動で対応)
    match etype {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            return CallbackResult::Keep;
        }
        _ => {}
    }

    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let auto_repeat =
        event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;
    let flags = event.get_flags();

    let state_cell = match HOOK_STATE.get() {
        Some(s) => s,
        None => return CallbackResult::Keep,
    };
    let mut guard = match state_cell.try_lock() {
        Ok(g) => g,
        Err(_) => return CallbackResult::Keep,
    };
    // 早期 None チェックのみ (state 参照は各ブロック内で都度 as_mut() で取り直す)
    if guard.as_ref().is_none() {
        return CallbackResult::Keep;
    }

    // ESC ハンドリング (録音中のみ Cancel + Drop)
    // kVK_Escape = 0x35
    const KVK_ESCAPE: u16 = 0x35;
    if keycode == KVK_ESCAPE && matches!(etype, CGEventType::KeyDown) {
        let state = guard.as_mut().expect("checked Some above");
        if state.is_recording {
            let cb = state.callback.clone();
            drop(guard);
            cb(HotkeyEvent::Cancel);
            return CallbackResult::Drop;
        }
        return CallbackResult::Keep;
    }

    // 期待 keycode を解決 (state 借用を block scope に閉じる)
    let expected_keycode = {
        let state = guard.as_ref().expect("checked Some above");
        match config_to_macos_keycode(&state.config.code) {
            Some(k) => k,
            None => return CallbackResult::Keep,
        }
    };

    // FlagsChanged は「修飾キー単独ホットキー (例 ControlRight 単体)」のときのみ意味を持つ。
    // メインキー = 修飾キーの場合は flags 差分で down/up を判定する必要がある。
    // それ以外 (Ctrl+Shift+NonConvert 等) は KeyDown/KeyUp 経路で処理する。
    if matches!(etype, CGEventType::FlagsChanged) {
        // FlagsChanged で expected と異なる修飾キーは無視
        // mac/hotkey-cmd-either-side: 設定 code が総称 ("Meta" 等) なら左右両方マッチさせる
        let main_key_match = {
            let state = guard.as_ref().expect("checked Some above");
            matches_main_key_keycode(&state.config.code, keycode)
        };
        if !main_key_match {
            let state = guard.as_mut().expect("checked Some above");
            state.last_flags = flags;
            return CallbackResult::Keep;
        }
        // 期待 keycode の修飾キー: flags 差分で down/up を判定
        // state の借用は block scope に閉じて、guard 自身を handle_press/release に move
        let (was_down, is_down) = {
            let state = guard.as_mut().expect("checked Some above");
            let prev = state.last_flags;
            state.last_flags = flags;
            (
                is_modifier_down_in_flags(expected_keycode, prev),
                is_modifier_down_in_flags(expected_keycode, flags),
            )
        };

        if !was_down && is_down {
            return handle_press(guard, expected_keycode);
        }
        if was_down && !is_down {
            return handle_release(guard, expected_keycode);
        }
        return CallbackResult::Keep;
    }

    // 通常キー (KeyDown / KeyUp)
    // 修飾キー単独 keycode の通常 KeyDown/KeyUp は来ない (FlagsChanged 経由) のでスキップ
    // mac/hotkey-cmd-either-side: 設定 code が総称 ("Meta" 等) なら左右両方マッチさせる
    let main_key_match = {
        let state = guard.as_ref().expect("checked Some above");
        matches_main_key_keycode(&state.config.code, keycode)
    };
    if !main_key_match {
        return CallbackResult::Keep;
    }

    // KeyDown/KeyUp 判定用フラグも block scope に閉じる
    let is_currently_pressed = {
        let state = guard.as_ref().expect("checked Some above");
        state.is_currently_pressed
    };

    match etype {
        CGEventType::KeyDown => {
            // 同じキーが連射 (autoRepeat) されている場合は repeat 抑止
            if auto_repeat || is_currently_pressed {
                return CallbackResult::Drop;
            }
            handle_press(guard, expected_keycode)
        }
        CGEventType::KeyUp => {
            if !is_currently_pressed {
                return CallbackResult::Keep;
            }
            handle_release(guard, expected_keycode)
        }
        _ => CallbackResult::Keep,
    }
}

/// 期待ホットキー押下 → Pressed callback 発火 (Win 版 EventType::Down と同型)
///
/// 呼出元で `guard.as_mut()` 経由で state 検査済みなので、本関数内では
/// `guard.as_mut().expect(...)` で取り直して借用を再開する (案 B: state 引数廃止)。
fn handle_press(
    mut guard: std::sync::MutexGuard<'_, Option<HookState>>,
    expected_keycode: u16,
) -> CallbackResult {
    let state = guard
        .as_mut()
        .expect("HOOK_STATE must be Some when handle_press is called");
    // modifiers がすべて押されていないと発動しない
    // メインキー自身が修飾キーの場合は self exclude (要望 2026-05-21 Win 版踏襲)
    if !check_modifiers_match_macos_keycode(&state.config.modifiers, expected_keycode) {
        return CallbackResult::Keep;
    }
    state.is_currently_pressed = true;
    let cb = state.callback.clone();
    drop(guard);
    cb(HotkeyEvent::Pressed);
    CallbackResult::Drop
}

/// 期待ホットキー離上 → Released callback 発火 (Win 版 EventType::Up と同型)
fn handle_release(
    mut guard: std::sync::MutexGuard<'_, Option<HookState>>,
    _expected_keycode: u16,
) -> CallbackResult {
    let state = guard
        .as_mut()
        .expect("HOOK_STATE must be Some when handle_release is called");
    state.is_currently_pressed = false;
    let is_toggle = matches!(state.config.mode, HotkeyMode::Toggle);
    if is_toggle {
        CallbackResult::Drop
    } else {
        let cb = state.callback.clone();
        drop(guard);
        cb(HotkeyEvent::Released);
        CallbackResult::Drop
    }
}

// ---------------------------------------------------------------------------
// keycode 変換: W3C UI Events Code -> macOS HIToolbox keycode
// ---------------------------------------------------------------------------
// Win 版 code_to_vk は Windows VK を返すので、macOS では別マッピングが必要。
// 日本語 IME (NonConvert / Convert / KanaMode) は macOS では JIS キーボードの
// 英数 (kVK_JIS_Eisu = 102) / かな (kVK_JIS_Kana = 104) 系に対応する。
//
// 戻り値: 該当 keycode、無ければ None
/// mac/hotkey-cmd-either-side (2026-06-15):
/// 設定 code が「左右どちらでもマッチ」を意味する総称コードか判定。
/// 該当する場合、callback で受け取った keycode が左右どちらでも match させる。
///
/// `config_to_macos_keycode` は代表値 (左側) のみ返すため、tap_callback で
/// 「keycode != expected_keycode」フィルタを通すと右側が落ちる。本関数で総称判定し
/// 右側も拾えるようにする。
fn matches_main_key_keycode(config_code: &str, event_keycode: u16) -> bool {
    match config_code {
        "Control" | "Ctrl" => event_keycode == 0x3B || event_keycode == 0x3E,
        "Shift" => event_keycode == 0x38 || event_keycode == 0x3C,
        "Alt" | "Option" => event_keycode == 0x3A || event_keycode == 0x3D,
        "Meta" | "Cmd" | "Command" | "Win" | "Windows" | "Super" => {
            event_keycode == 0x37 || event_keycode == 0x36
        }
        // それ以外 (左右指定済 or 通常キー) は単一 keycode と等価比較
        other => config_to_macos_keycode(other).map_or(false, |kc| kc == event_keycode),
    }
}

fn config_to_macos_keycode(code: &str) -> Option<u16> {
    Some(match code {
        // 日本語 IME 系 (JIS キーボード)
        "NonConvert" => 0x66, // kVK_JIS_Eisu (英数)
        "Convert" => 0x68,    // kVK_JIS_Kana (かな)
        "KanaMode" => 0x68,   // = kVK_JIS_Kana
        "Lang1" => 0x68,      // = かな
        "Lang2" => 0x66,      // = 英数

        // 修飾キー単独 (kVK_* 値は HIToolbox Events.h より)
        "ControlLeft" => 0x3B,  // kVK_Control
        "ControlRight" => 0x3E, // kVK_RightControl
        "ShiftLeft" => 0x38,    // kVK_Shift
        "ShiftRight" => 0x3C,   // kVK_RightShift
        "AltLeft" => 0x3A,      // kVK_Option
        "AltRight" => 0x3D,     // kVK_RightOption
        "MetaLeft" => 0x37,     // kVK_Command
        "MetaRight" => 0x36,    // kVK_RightCommand

        // mac/hotkey-cmd-either-side (2026-06-15): 左右どちらでもマッチする総称コード。
        // Logitech MX Master 等のマウスが送る Cmd キーは左右どちらで認識されるか
        // ユーザー側で制御できないため、生成的に「両側どちらでも」を選べる選択肢を提供する。
        // 戻り値は「代表値」(左側) を返す: tap_callback は config 文字列を見て両側マッチ判定する
        // (matches_main_key_keycode 経由)。is_modifier_down_in_flags は左右どちらでも同じ
        // flag (CGEventFlagCommand 等) を見るのでこの代表値で問題ない。
        "Control" | "Ctrl" => 0x3B,
        "Shift" => 0x38,
        "Alt" | "Option" => 0x3A,
        "Meta" | "Cmd" | "Command" | "Win" | "Windows" | "Super" => 0x37,

        // 制御キー
        "Space" => 0x31,
        "Enter" => 0x24,
        "Escape" => 0x35,
        "Tab" => 0x30,
        "Backspace" => 0x33,
        "Delete" => 0x75,
        "Insert" => return None, // macOS 標準キーボードに Insert は無い
        "Home" => 0x73,
        "End" => 0x77,
        "PageUp" => 0x74,
        "PageDown" => 0x79,
        "ArrowUp" => 0x7E,
        "ArrowDown" => 0x7D,
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "CapsLock" => 0x39,

        // アルファベット A-Z (kVK_ANSI_A=0x00 ... 連続ではないので個別マッピング)
        "KeyA" => 0x00,
        "KeyB" => 0x0B,
        "KeyC" => 0x08,
        "KeyD" => 0x02,
        "KeyE" => 0x0E,
        "KeyF" => 0x03,
        "KeyG" => 0x05,
        "KeyH" => 0x04,
        "KeyI" => 0x22,
        "KeyJ" => 0x26,
        "KeyK" => 0x28,
        "KeyL" => 0x25,
        "KeyM" => 0x2E,
        "KeyN" => 0x2D,
        "KeyO" => 0x1F,
        "KeyP" => 0x23,
        "KeyQ" => 0x0C,
        "KeyR" => 0x0F,
        "KeyS" => 0x01,
        "KeyT" => 0x11,
        "KeyU" => 0x20,
        "KeyV" => 0x09,
        "KeyW" => 0x0D,
        "KeyX" => 0x07,
        "KeyY" => 0x10,
        "KeyZ" => 0x06,

        // Digit0-9 (kVK_ANSI_0=0x1D, 1=0x12, 2=0x13, ..., 9=0x19)
        "Digit0" => 0x1D,
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit5" => 0x17,
        "Digit6" => 0x16,
        "Digit7" => 0x1A,
        "Digit8" => 0x1C,
        "Digit9" => 0x19,

        // F1-F12 (kVK_F1=0x7A, F2=0x78, F3=0x63, F4=0x76, F5=0x60, F6=0x61,
        //         F7=0x62, F8=0x64, F9=0x65, F10=0x6D, F11=0x67, F12=0x6F)
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,

        _ => return None,
    })
}

/// macOS keycode が修飾キーかどうか + どの kind/side か判定
fn macos_keycode_modifier_kind_side(keycode: u16) -> Option<(ModifierKind, ModifierSide)> {
    Some(match keycode {
        0x3B => (ModifierKind::Control, ModifierSide::Left),
        0x3E => (ModifierKind::Control, ModifierSide::Right),
        0x38 => (ModifierKind::Shift, ModifierSide::Left),
        0x3C => (ModifierKind::Shift, ModifierSide::Right),
        0x3A => (ModifierKind::Alt, ModifierSide::Left),
        0x3D => (ModifierKind::Alt, ModifierSide::Right),
        0x37 => (ModifierKind::Meta, ModifierSide::Left),
        0x36 => (ModifierKind::Meta, ModifierSide::Right),
        _ => return None,
    })
}

/// macOS keycode の修飾キーが、与えられた CGEventFlags 内で「押下中」と表現されているか
///
/// CGEventFlags は device-independent (左右区別なし) なので、左右区別は keycode 単体で
/// 表現できない → 「flag bit on かつ keycode が flags に紐づく kind」で代用する。
fn is_modifier_down_in_flags(keycode: u16, flags: CGEventFlags) -> bool {
    match keycode {
        0x3B | 0x3E => flags.contains(CGEventFlags::CGEventFlagControl),
        0x38 | 0x3C => flags.contains(CGEventFlags::CGEventFlagShift),
        0x3A | 0x3D => flags.contains(CGEventFlags::CGEventFlagAlternate),
        0x37 | 0x36 => flags.contains(CGEventFlags::CGEventFlagCommand),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// check_modifiers_match
// ---------------------------------------------------------------------------

/// 設定 modifiers と実際の修飾キー押下状態を厳密に照合する (Win 版と同セマンティクス)。
///
/// 引数:
///   - modifiers: 設定上の修飾キー文字列リスト
///   - main_vk:   Windows VK コード (公開 API は Win 由来のため呼出側は Windows VK を渡す)
///                macOS では本関数内で Windows VK → macOS keycode に変換した上で判定する。
///
/// ⚠️ 互換性のため引数名は `main_vk` のまま (Win 由来)。実体は「ホットキーのメインキーが
///    修飾キーか否か」を判定する用途しか無いため、Windows VK → ModifierKind の変換は
///    `windows_vk_modifier_kind` で行う。
pub fn check_modifiers_match(modifiers: &[String], main_vk: u32) -> bool {
    let mut required: Vec<(ModifierKind, ModifierSide)> = Vec::new();
    for s in modifiers {
        if let Some(parsed) = parse_modifier_side(s) {
            required.push(parsed);
        } else {
            // 不明な修飾キー文字列 → 設定不整合 → 発動させない
            return false;
        }
    }

    for (kind, side) in &required {
        if !is_modifier_pressed_macos(*kind, *side) {
            return false;
        }
    }

    let main_is_modifier_kind = windows_vk_modifier_kind(main_vk);
    for kind in [
        ModifierKind::Control,
        ModifierKind::Shift,
        ModifierKind::Alt,
        ModifierKind::Meta,
    ] {
        // メインキー自身の kind はチェックから除外
        if main_is_modifier_kind == Some(kind) {
            continue;
        }
        let is_required = required.iter().any(|(k, _)| *k == kind);
        let is_pressed = is_modifier_pressed_macos(kind, ModifierSide::Either);
        if !is_required && is_pressed {
            return false;
        }
    }

    true
}

/// Windows VK → ModifierKind (Win 版 main_key_modifier_kind と同型・互換のため Win VK を受ける)
fn windows_vk_modifier_kind(vk: u32) -> Option<ModifierKind> {
    match vk {
        0xA2 | 0xA3 => Some(ModifierKind::Control),
        0xA0 | 0xA1 => Some(ModifierKind::Shift),
        0xA4 | 0xA5 => Some(ModifierKind::Alt),
        0x5B | 0x5C => Some(ModifierKind::Meta),
        _ => None,
    }
}

/// macOS で修飾キーが押されているか CGEventSourceKeyState で問い合わせる
fn is_modifier_pressed_macos(kind: ModifierKind, side: ModifierSide) -> bool {
    let (l_kc, r_kc) = match kind {
        ModifierKind::Control => (0x3B_u16, 0x3E_u16),
        ModifierKind::Shift => (0x38, 0x3C),
        ModifierKind::Alt => (0x3A, 0x3D),
        ModifierKind::Meta => (0x37, 0x36),
    };
    let left = unsafe { CGEventSourceKeyState(CG_EVENT_SOURCE_STATE_HID, l_kc) };
    let right = unsafe { CGEventSourceKeyState(CG_EVENT_SOURCE_STATE_HID, r_kc) };
    match side {
        ModifierSide::Either => left || right,
        ModifierSide::Left => left,
        ModifierSide::Right => right,
    }
}

// ---------------------------------------------------------------------------
// 内部ヘルパー: tap callback 内で使う macOS keycode ベースの check_modifiers_match
// ---------------------------------------------------------------------------
//
// 公開 API `check_modifiers_match` は Win VK を取るため呼出元 (lib.rs) の互換性確保用。
// tap callback 内では既に macOS keycode を持っているので、変換ロスを避けて専用関数を用意。
fn check_modifiers_match_macos_keycode(modifiers: &[String], main_keycode: u16) -> bool {
    let mut required: Vec<(ModifierKind, ModifierSide)> = Vec::new();
    for s in modifiers {
        if let Some(parsed) = parse_modifier_side(s) {
            required.push(parsed);
        } else {
            return false;
        }
    }

    for (kind, side) in &required {
        if !is_modifier_pressed_macos(*kind, *side) {
            return false;
        }
    }

    let main_is_modifier_kind = macos_keycode_modifier_kind_side(main_keycode).map(|(k, _)| k);
    for kind in [
        ModifierKind::Control,
        ModifierKind::Shift,
        ModifierKind::Alt,
        ModifierKind::Meta,
    ] {
        if main_is_modifier_kind == Some(kind) {
            continue;
        }
        let is_required = required.iter().any(|(k, _)| *k == kind);
        let is_pressed = is_modifier_pressed_macos(kind, ModifierSide::Either);
        if !is_required && is_pressed {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// 未使用警告除け: code_to_vk は公開 API check_modifiers_match との Win 互換 contract
// 維持のため import している (Win VK 入力で呼ばれる前提)。本ファイル内で直接は使わない。
// ---------------------------------------------------------------------------
#[allow(dead_code)]
fn _unused_win_compat_helpers() {
    let _ = code_to_vk("Space");
}

#[cfg(test)]
mod tests {
    use super::*;

    // CGEventTap / CGEventSourceKeyState は OS 統合が必須なので、純関数のみテスト。

    #[test]
    fn check_modifiers_match_with_unknown_modifier_returns_false() {
        let modifiers = vec!["BogusModifier".to_string()];
        assert!(!check_modifiers_match(&modifiers, 0));
    }

    #[test]
    fn config_to_macos_keycode_letters() {
        assert_eq!(config_to_macos_keycode("KeyA"), Some(0x00));
        assert_eq!(config_to_macos_keycode("KeyV"), Some(0x09));
        assert_eq!(config_to_macos_keycode("KeyZ"), Some(0x06));
    }

    #[test]
    fn config_to_macos_keycode_digits() {
        assert_eq!(config_to_macos_keycode("Digit0"), Some(0x1D));
        assert_eq!(config_to_macos_keycode("Digit9"), Some(0x19));
    }

    #[test]
    fn config_to_macos_keycode_function_keys() {
        assert_eq!(config_to_macos_keycode("F1"), Some(0x7A));
        assert_eq!(config_to_macos_keycode("F12"), Some(0x6F));
    }

    #[test]
    fn config_to_macos_keycode_japanese_ime() {
        assert_eq!(config_to_macos_keycode("NonConvert"), Some(0x66));
        assert_eq!(config_to_macos_keycode("Convert"), Some(0x68));
    }

    #[test]
    fn config_to_macos_keycode_modifiers_individual() {
        assert_eq!(config_to_macos_keycode("ControlLeft"), Some(0x3B));
        assert_eq!(config_to_macos_keycode("ControlRight"), Some(0x3E));
        assert_eq!(config_to_macos_keycode("ShiftLeft"), Some(0x38));
        assert_eq!(config_to_macos_keycode("MetaRight"), Some(0x36));
    }

    // mac/hotkey-cmd-either-side (2026-06-15): 総称コードの代表値返却テスト
    #[test]
    fn config_to_macos_keycode_either_side_aliases() {
        // 左側を代表値として返す
        assert_eq!(config_to_macos_keycode("Control"), Some(0x3B));
        assert_eq!(config_to_macos_keycode("Ctrl"), Some(0x3B));
        assert_eq!(config_to_macos_keycode("Shift"), Some(0x38));
        assert_eq!(config_to_macos_keycode("Alt"), Some(0x3A));
        assert_eq!(config_to_macos_keycode("Option"), Some(0x3A));
        assert_eq!(config_to_macos_keycode("Meta"), Some(0x37));
        assert_eq!(config_to_macos_keycode("Cmd"), Some(0x37));
        assert_eq!(config_to_macos_keycode("Command"), Some(0x37));
        assert_eq!(config_to_macos_keycode("Win"), Some(0x37));
        assert_eq!(config_to_macos_keycode("Windows"), Some(0x37));
        assert_eq!(config_to_macos_keycode("Super"), Some(0x37));
    }

    // mac/hotkey-cmd-either-side: matches_main_key_keycode が左右両方マッチする
    #[test]
    fn matches_main_key_keycode_either_side_meta() {
        // 総称 'Meta' は左 (0x37) も右 (0x36) も match
        assert!(matches_main_key_keycode("Meta", 0x37));
        assert!(matches_main_key_keycode("Meta", 0x36));
        assert!(matches_main_key_keycode("Cmd", 0x37));
        assert!(matches_main_key_keycode("Cmd", 0x36));
        // 'Meta' は Ctrl (0x3B) には match しない
        assert!(!matches_main_key_keycode("Meta", 0x3B));
        // 'F12' (0x6F) は通常通り単一 keycode
        assert!(matches_main_key_keycode("F12", 0x6F));
        assert!(!matches_main_key_keycode("F12", 0x6E));
    }

    #[test]
    fn matches_main_key_keycode_either_side_control_shift_alt() {
        assert!(matches_main_key_keycode("Control", 0x3B));
        assert!(matches_main_key_keycode("Control", 0x3E));
        assert!(matches_main_key_keycode("Shift", 0x38));
        assert!(matches_main_key_keycode("Shift", 0x3C));
        assert!(matches_main_key_keycode("Alt", 0x3A));
        assert!(matches_main_key_keycode("Alt", 0x3D));
        assert!(matches_main_key_keycode("Option", 0x3A));
        assert!(matches_main_key_keycode("Option", 0x3D));
    }

    #[test]
    fn matches_main_key_keycode_specific_side_does_not_match_other() {
        // 左右指定は片側のみ match (後方互換)
        assert!(matches_main_key_keycode("MetaLeft", 0x37));
        assert!(!matches_main_key_keycode("MetaLeft", 0x36));
        assert!(matches_main_key_keycode("MetaRight", 0x36));
        assert!(!matches_main_key_keycode("MetaRight", 0x37));
    }

    #[test]
    fn config_to_macos_keycode_invalid_returns_none() {
        assert_eq!(config_to_macos_keycode("Bogus"), None);
        assert_eq!(config_to_macos_keycode(""), None);
        assert_eq!(config_to_macos_keycode("KeyAA"), None);
        assert_eq!(config_to_macos_keycode("Insert"), None);
    }

    #[test]
    fn macos_keycode_modifier_kind_side_classifies_correctly() {
        assert_eq!(
            macos_keycode_modifier_kind_side(0x3B),
            Some((ModifierKind::Control, ModifierSide::Left))
        );
        assert_eq!(
            macos_keycode_modifier_kind_side(0x3E),
            Some((ModifierKind::Control, ModifierSide::Right))
        );
        assert_eq!(
            macos_keycode_modifier_kind_side(0x38),
            Some((ModifierKind::Shift, ModifierSide::Left))
        );
        assert_eq!(
            macos_keycode_modifier_kind_side(0x36),
            Some((ModifierKind::Meta, ModifierSide::Right))
        );
        assert_eq!(macos_keycode_modifier_kind_side(0x00), None); // KeyA
        assert_eq!(macos_keycode_modifier_kind_side(0x35), None); // Escape
    }

    #[test]
    fn windows_vk_modifier_kind_classifies_correctly() {
        assert_eq!(windows_vk_modifier_kind(0xA2), Some(ModifierKind::Control));
        assert_eq!(windows_vk_modifier_kind(0xA3), Some(ModifierKind::Control));
        assert_eq!(windows_vk_modifier_kind(0xA0), Some(ModifierKind::Shift));
        assert_eq!(windows_vk_modifier_kind(0xA1), Some(ModifierKind::Shift));
        assert_eq!(windows_vk_modifier_kind(0xA4), Some(ModifierKind::Alt));
        assert_eq!(windows_vk_modifier_kind(0xA5), Some(ModifierKind::Alt));
        assert_eq!(windows_vk_modifier_kind(0x5B), Some(ModifierKind::Meta));
        assert_eq!(windows_vk_modifier_kind(0x5C), Some(ModifierKind::Meta));
        assert_eq!(windows_vk_modifier_kind(0x1D), None);
        assert_eq!(windows_vk_modifier_kind(0x41), None);
    }

    #[test]
    fn is_modifier_down_in_flags_recognizes_each_kind() {
        let ctrl_flag = CGEventFlags::CGEventFlagControl;
        let shift_flag = CGEventFlags::CGEventFlagShift;
        let alt_flag = CGEventFlags::CGEventFlagAlternate;
        let cmd_flag = CGEventFlags::CGEventFlagCommand;

        assert!(is_modifier_down_in_flags(0x3B, ctrl_flag));
        assert!(is_modifier_down_in_flags(0x3E, ctrl_flag));
        assert!(!is_modifier_down_in_flags(0x3B, shift_flag));

        assert!(is_modifier_down_in_flags(0x38, shift_flag));
        assert!(is_modifier_down_in_flags(0x3C, shift_flag));

        assert!(is_modifier_down_in_flags(0x3A, alt_flag));
        assert!(is_modifier_down_in_flags(0x3D, alt_flag));

        assert!(is_modifier_down_in_flags(0x37, cmd_flag));
        assert!(is_modifier_down_in_flags(0x36, cmd_flag));

        // 非修飾キーは常に false
        assert!(!is_modifier_down_in_flags(0x00, ctrl_flag)); // KeyA
        assert!(!is_modifier_down_in_flags(0x35, ctrl_flag)); // Escape
    }
}
