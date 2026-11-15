// keyboard_hook/windows_impl.rs — Phase 1 Day 6 Stage 2-C-2: WH_KEYBOARD_LL 低レベルキーボードフック
//
// 仕様:
//   - v1 RushVoice (PowerShell + 埋込C#) と同じ WH_KEYBOARD_LL 経路を Rust + windows crate で再実装
//   - tauri-plugin-global-shortcut では IME が無変換キー等を横取りするため取得不能 → 本フックで OS より前に捕捉
//   - 左右修飾キー (左Ctrl/右Ctrl/左Shift/右Shift/左Alt/右Alt) を厳密に区別
//   - Toggle / PushToTalk 両モード対応 (HotkeyMode に応じて Released 通知の有無を分岐)
//
// 設計:
//   - グローバル static HOOK_STATE で hook callback から設定にアクセス (extern "system" fn は capture 不可)。
//   - フック thread: SetWindowsHookExW + GetMessageW ループと、見張りタイマーでの張り直しだけを行う。
//   - 通知 thread: フックから channel で受けたイベントでコールバックを呼ぶ。ログもここで書く。
//   - hook callback は設定ホットキーだけ他アプリへの伝播を抑止 (LRESULT(1)) する。
//
// 2026-10-31 反応が悪い・キーボード全体が入らなくなる問題の対策:
//   フックの処理が遅れると Windows はキー入力全体を待たせる (20:59〜21:03 キーボード全体が入らず、
//   アプリの再起動で戻った)。外されると再起動まで戻らない (20:44〜20:48)。
//   (A) フックの中では待たない: Mutex は try_lock だけ (取れなければ素通し)、ログを書かない、
//       コールバック (tokio の spawn) も呼ばない。イベントは channel へ送るだけ。
//   (B) フック thread を THREAD_PRIORITY_TIME_CRITICAL で動かす。
//   (C) 見張り: WATCHDOG_INTERVAL_MS ごとに新しいフックを張ってから古いフックを外す
//       (Windows に外されても最長この間隔で戻る)。古いフックの解除失敗＝外されていた、として記録。
//   (D) 詰まりの検知: 通知 thread が1秒ごとにフック thread へ合図を送り、返事が遅れたらログに残す。
//       フック処理の所要時間・キー到着の遅れも記録してログに残す。
//   (E) is_currently_pressed が離し (Up) の取りこぼしで残っても、メインキーの Down が
//       STUCK_RESET_MS 以上途切れたあとの Down は新しい押下として扱う。
//   (F) 修飾キーは、フックが受けた Down/Up から持つ状態と GetAsyncKeyState の OR で判定する
//       (素早く押したときの取りこぼし防止)。離しの取りこぼしは MOD_STALE_MS で消す。
//
// Phase 5 Step A-2: 元 keyboard_hook.rs から本ファイルへ移動

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{HINSTANCE, HMODULE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KillTimer, PostThreadMessageW, SetTimer,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
};

use crate::hotkey::{
    code_to_vk, parse_modifier_side, HotkeyConfig, HotkeyMode, ModifierKind, ModifierSide,
};

/// 見張りの間隔。この間隔でフックを張り直す (Windows に外されても最長この時間で戻る)。
const WATCHDOG_INTERVAL_MS: u32 = 10_000;
/// 通知 thread がフック thread の詰まりを確かめる間隔。
const PROBE_INTERVAL_MS: u64 = 1_000;
/// フック thread の返事がこれ以上遅れたら「詰まり」としてログに残す。
const STALL_LOG_MS: u32 = 1_000;
/// 診断の数値をまとめてログに出す間隔 (確かめる回数)。1秒 × 10 = 10秒。
const DIAG_EVERY_PROBES: u32 = 10;
/// 見張りが「動いている」ことをログに残す間隔 (確かめる回数)。1秒 × 3600 = 1時間。
const ALIVE_EVERY_PROBES: u32 = 3_600;
/// キー到着の遅れがこれ以上ならログに残す。
const SLOW_DELIVERY_LOG_MS: u32 = 300;
/// フック処理の所要時間がこれ以上ならログに残す。
const SLOW_PROC_LOG_MS: u32 = 50;
/// メインキーの Down がこれ以上途切れたあとの Down は「離しの取りこぼし」とみなし新しい押下として扱う。
/// キーリピートの開始待ちは最長 1000ms (コントロールパネルの最長設定) なので、それより長くする。
const STUCK_RESET_MS: u32 = 1_500;
/// 修飾キーの押下記録が、実際には離れていてこれ以上古ければ取りこぼしとみなして消す。
const MOD_STALE_MS: u32 = 2_000;
/// 通知 thread → フック thread の詰まり確認の合図。
const WM_APP_PROBE: u32 = WM_APP + 0x31;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    /// HK-06: 録音中に ESC が押されたことを通知する。transcribe 起動なしで Idle に戻す。
    Cancel,
}

pub type HotkeyCallback = Arc<dyn Fn(HotkeyEvent) + Send + Sync + 'static>;

struct HookState {
    config: HotkeyConfig,
    /// 通知 thread への送り口 (送るだけで待たない)
    event_tx: Sender<HotkeyEvent>,
    /// Pressed イベント repeat 抑止 (キーを押し続けると WM_KEYDOWN が連発するため)
    is_currently_pressed: bool,
    /// メインキーの最後の Down の時刻 (GetTickCount 基準)。取りこぼし判定に使う。
    last_main_down: u32,
}

static HOOK_STATE: OnceLock<Mutex<Option<HookState>>> = OnceLock::new();
/// HK-06: 録音中フラグ。true のときのみ ESC を Cancel として扱い他アプリへの伝播を止める。
/// フックの中で Mutex を待たないよう原子変数で持つ。
static IS_RECORDING: AtomicBool = AtomicBool::new(false);

// ---- 見張り用の数値 (フック thread ではログを書かず、通知 thread が読んでまとめて書く) ----
static MAX_DELIVERY_LAG_MS: AtomicU32 = AtomicU32::new(0);
static MAX_PROC_MS: AtomicU32 = AtomicU32::new(0);
static STUCK_RESETS: AtomicU32 = AtomicU32::new(0);
static LOST_MODIFIER_UPS: AtomicU32 = AtomicU32::new(0);
/// try_lock が取れず素通しした回数
static LOCK_BUSY_PASSES: AtomicU32 = AtomicU32::new(0);
static REINSTALLS: AtomicU32 = AtomicU32::new(0);
/// 張り直しのとき、古いフックが既に外れていた (Windows に外された) 回数
static HOOK_WAS_GONE: AtomicU32 = AtomicU32::new(0);
static REINSTALL_FAILED: AtomicU32 = AtomicU32::new(0);
/// フック thread が最後に詰まり確認の合図を受けた時刻 (GetTickCount)
static LAST_PROBE_ACK: AtomicU32 = AtomicU32::new(0);

// ---- 修飾キーの押下状態 (フックが受けた Down/Up から持つ) ----
/// 0 = 離れている。それ以外 = 最後に Down を受けた時刻 (GetTickCount、0 を避けるため最小 1)。
static MOD_DOWN_AT: [AtomicU32; 8] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
const MOD_VKS: [u16; 8] = [
    VK_LCONTROL.0,
    VK_RCONTROL.0,
    VK_LSHIFT.0,
    VK_RSHIFT.0,
    VK_LMENU.0,
    VK_RMENU.0,
    VK_LWIN.0,
    VK_RWIN.0,
];

fn mod_index(vk: u32) -> Option<usize> {
    MOD_VKS.iter().position(|&m| m as u32 == vk)
}

fn record_modifier_event(vk: u32, down: bool, now: u32) {
    if let Some(i) = mod_index(vk) {
        MOD_DOWN_AT[i].store(if down { now.max(1) } else { 0 }, Ordering::Relaxed);
    }
}

/// 押下記録 (`down_at`、0 = 離れている) と今の時刻・実際のキー状態から
/// (押されているとみなすか, 記録を消すべきか) を返す。純関数 (テスト用に分離)。
fn resolve_tracked(down_at: u32, now: u32, async_down: bool) -> (bool, bool) {
    if down_at == 0 {
        return (false, false);
    }
    if !async_down && now.wrapping_sub(down_at) > MOD_STALE_MS {
        return (false, true);
    }
    (true, false)
}

/// メインキーが押しっぱなし扱いのまま、前の Down から `STUCK_RESET_MS` を超えて途切れていたら true。
fn should_reset_stuck(last_down: u32, now: u32) -> bool {
    now.wrapping_sub(last_down) > STUCK_RESET_MS
}

/// 詰まり確認の合図を送ってから `elapsed` 経っても返事が無ければ詰まり。純関数 (テスト用に分離)。
fn is_stalled(sent_at: u32, ack_at: u32, now: u32) -> bool {
    // 返事が送った時刻以降なら届いている
    let acked = ack_at.wrapping_sub(sent_at) < u32::MAX / 2;
    !acked && now.wrapping_sub(sent_at) >= STALL_LOG_MS
}

/// フックの外 (設定変更など) から使う。待ってよい側。
fn lock_state(cell: &Mutex<Option<HookState>>) -> MutexGuard<'_, Option<HookState>> {
    cell.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct KeyboardHook {
    hook_thread_id: Arc<AtomicU32>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    notifier: Option<JoinHandle<()>>,
}

unsafe fn install_hook(h_mod: HMODULE) -> windows::core::Result<HHOOK> {
    unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            HINSTANCE(h_mod.0),
            0,
        )
    }
}

impl KeyboardHook {
    /// フック thread と通知 thread を起動し、設定を HOOK_STATE に格納する。
    /// すでに別の KeyboardHook が動いている場合の挙動は未定義 (二重起動は禁止)。
    pub fn start(config: HotkeyConfig, callback: HotkeyCallback) -> Result<Self, String> {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<HotkeyEvent>();
        let state_cell = HOOK_STATE.get_or_init(|| Mutex::new(None));
        {
            let mut guard = lock_state(state_cell);
            *guard = Some(HookState {
                config,
                event_tx,
                is_currently_pressed: false,
                last_main_down: 0,
            });
        }
        IS_RECORDING.store(false, Ordering::Relaxed);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let hook_thread_id = Arc::new(AtomicU32::new(0));
        let stop_for_thread = stop_flag.clone();
        let tid_for_thread = hook_thread_id.clone();

        // (u32 thread id, 優先度を上げられたか, 見張りタイマーを作れたか)
        let (init_tx, init_rx) =
            std::sync::mpsc::sync_channel::<Result<(u32, bool, bool), String>>(0);

        let handle = thread::spawn(move || unsafe {
            // (B) PC が重いときもフックの返事が遅れないよう、優先度を最高にする
            let priority_ok =
                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL).is_ok();

            let h_mod = match GetModuleHandleW(None) {
                Ok(h) => h,
                Err(e) => {
                    let _ = init_tx.send(Err(format!("GetModuleHandleW failed: {:?}", e)));
                    return;
                }
            };
            let mut hook_id: HHOOK = match install_hook(h_mod) {
                Ok(h) => h,
                Err(e) => {
                    let _ = init_tx.send(Err(format!("SetWindowsHookExW failed: {:?}", e)));
                    return;
                }
            };

            // (C) 見張りのタイマー (thread のメッセージキューに WM_TIMER が来る)
            let timer_id = SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None);

            let my_tid = GetCurrentThreadId();
            tid_for_thread.store(my_tid, Ordering::Release);
            LAST_PROBE_ACK.store(GetTickCount(), Ordering::Relaxed);
            let _ = init_tx.send(Ok((my_tid, priority_ok, timer_id != 0)));

            // GetMessage ループ (フック thread が message queue を持たないと callback が呼ばれない)
            // ここではログを書かない (書き込みで詰まるとキーボード全体が待たされるため)
            let mut msg = MSG::default();
            loop {
                if stop_for_thread.load(Ordering::Acquire) {
                    break;
                }
                let ret = GetMessageW(&mut msg, None, 0, 0);
                if ret.0 <= 0 {
                    // 0 = WM_QUIT, -1 = error
                    break;
                }
                if msg.hwnd.0.is_null() {
                    if msg.message == WM_TIMER {
                        reinstall_hook(&mut hook_id, h_mod);
                        continue;
                    }
                    if msg.message == WM_APP_PROBE {
                        LAST_PROBE_ACK.store(GetTickCount(), Ordering::Relaxed);
                        continue;
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if timer_id != 0 {
                let _ = KillTimer(None, timer_id);
            }
            let _ = UnhookWindowsHookEx(hook_id);
        });

        let (hook_tid, priority_ok, timer_ok) = match init_rx.recv() {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(format!("hook init channel closed: {}", e)),
        };
        log::info!(
            "[hotkey] hook thread ready: priority=TIME_CRITICAL({}) watchdog_interval_ms={}{}",
            if priority_ok { "ok" } else { "failed" },
            WATCHDOG_INTERVAL_MS,
            if timer_ok { "" } else { " (SetTimer failed: reinstall disabled)" }
        );

        let stop_for_notifier = stop_flag.clone();
        let notifier = thread::spawn(move || {
            notifier_loop(event_rx, callback, hook_tid, stop_for_notifier);
        });

        Ok(KeyboardHook {
            hook_thread_id,
            stop_flag,
            handle: Some(handle),
            notifier: Some(notifier),
        })
    }

    /// ホットキー設定を更新する。フック thread はそのまま継続。
    /// is_currently_pressed は reset して、設定変更直後の押下状態誤検出を防ぐ。
    pub fn update_config(&self, new_config: HotkeyConfig) -> Result<(), String> {
        let state_cell = HOOK_STATE.get().ok_or("HOOK_STATE not initialized")?;
        let mut guard = lock_state(state_cell);
        if let Some(state) = guard.as_mut() {
            state.config = new_config;
            state.is_currently_pressed = false;
        }
        Ok(())
    }

    /// HK-06: 録音状態フラグを更新する。
    /// - `true`: 録音開始後に呼ぶ → ESC を Cancel として扱い他アプリへ伝播しない。
    /// - `false`: 録音停止/キャンセル/文字起こし終了後に必ず呼ぶ →
    ///   ESC を透過させ (CallNextHookEx)、Idle 時に他アプリの ESC を吸い込まない。
    ///
    /// ⚠️ set_recording(false) の呼び忘れは「Idle 時に ESC が全アプリで効かなくなる」致命バグ。
    ///    lib.rs 内の全録音開始/停止箇所で必ず呼ぶこと。
    pub fn set_recording(&self, val: bool) -> Result<(), String> {
        IS_RECORDING.store(val, Ordering::Relaxed);
        Ok(())
    }

    /// フック thread と通知 thread を終了させる。Drop でも呼ばれる。
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        let tid = self.hook_thread_id.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.notifier.take() {
            let _ = h.join();
        }
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// (C) フック thread 上 (フックの処理の外・WM_TIMER) で呼ぶ。新しいフックを張ってから古いフックを外す。
/// ログは書かず、結果を数値に残す (通知 thread がログに出す)。
unsafe fn reinstall_hook(hook_id: &mut HHOOK, h_mod: HMODULE) {
    match unsafe { install_hook(h_mod) } {
        Ok(new_id) => {
            let old = std::mem::replace(hook_id, new_id);
            REINSTALLS.fetch_add(1, Ordering::Relaxed);
            if unsafe { UnhookWindowsHookEx(old) }.is_err() {
                HOOK_WAS_GONE.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(_) => {
            REINSTALL_FAILED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 通知 thread: フックから届いたイベントでコールバックを呼び、
/// (D) 1秒ごとにフック thread の詰まりを確かめ、診断の数値をログに出す。
fn notifier_loop(
    event_rx: Receiver<HotkeyEvent>,
    callback: HotkeyCallback,
    hook_tid: u32,
    stop: Arc<AtomicBool>,
) {
    let mut probe_sent_at: Option<u32> = None;
    let mut stall_logged = false;
    let mut probes: u32 = 0;
    let mut last_probe = std::time::Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => callback(ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_probe.elapsed() < Duration::from_millis(PROBE_INTERVAL_MS) {
            continue;
        }
        last_probe = std::time::Instant::now();
        probes = probes.wrapping_add(1);
        let now = unsafe { GetTickCount() };
        let ack = LAST_PROBE_ACK.load(Ordering::Relaxed);

        // 前回の合図への返事を確かめる
        match probe_sent_at {
            Some(sent) if is_stalled(sent, ack, now) => {
                if !stall_logged {
                    log::warn!(
                        "[hotkey] hook thread stalled: no response for {}ms (keyboard input is delayed while this lasts)",
                        now.wrapping_sub(sent)
                    );
                    stall_logged = true;
                }
                // 返事が来るまで次の合図は送らない
                continue;
            }
            Some(sent) => {
                if stall_logged {
                    log::warn!(
                        "[hotkey] hook thread recovered after {}ms",
                        ack.wrapping_sub(sent)
                    );
                    stall_logged = false;
                }
            }
            None => {}
        }
        probe_sent_at = Some(now);
        unsafe {
            let _ = PostThreadMessageW(hook_tid, WM_APP_PROBE, WPARAM(0), LPARAM(0));
        }

        if probes % DIAG_EVERY_PROBES == 0 {
            log_diagnostics();
        }
        if probes % ALIVE_EVERY_PROBES == 0 {
            log::info!(
                "[hotkey] watchdog alive: reinstalls={}",
                REINSTALLS.load(Ordering::Relaxed)
            );
        }
    }
}

fn log_diagnostics() {
    let gone = HOOK_WAS_GONE.swap(0, Ordering::Relaxed);
    if gone > 0 {
        log::warn!(
            "[hotkey] watchdog: previous hook was already gone {} time(s) (likely removed by Windows after LowLevelHooksTimeout); reinstalled",
            gone
        );
    }
    let failed = REINSTALL_FAILED.swap(0, Ordering::Relaxed);
    if failed > 0 {
        log::warn!("[hotkey] watchdog: reinstall failed {} time(s) (kept current hook)", failed);
    }
    let lag = MAX_DELIVERY_LAG_MS.swap(0, Ordering::Relaxed);
    if lag >= SLOW_DELIVERY_LOG_MS {
        log::warn!("[hotkey] slow key delivery: max_lag_ms={} (PC busy or hook thread waiting)", lag);
    }
    let proc_ms = MAX_PROC_MS.swap(0, Ordering::Relaxed);
    if proc_ms >= SLOW_PROC_LOG_MS {
        log::warn!("[hotkey] slow hook processing: max_proc_ms={}", proc_ms);
    }
    let stuck = STUCK_RESETS.swap(0, Ordering::Relaxed);
    if stuck > 0 {
        log::warn!(
            "[hotkey] cleared stuck pressed state {} time(s) (key-up was missed; treated next press as new)",
            stuck
        );
    }
    let lost = LOST_MODIFIER_UPS.swap(0, Ordering::Relaxed);
    if lost > 0 {
        log::warn!("[hotkey] cleared {} missed modifier key-up(s)", lost);
    }
    let busy = LOCK_BUSY_PASSES.swap(0, Ordering::Relaxed);
    if busy > 0 {
        log::warn!("[hotkey] passed {} key event(s) through because settings were being updated", busy);
    }
}

#[derive(Copy, Clone)]
enum EventType {
    Down,
    Up,
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let start = unsafe { GetTickCount() };
    let ret = unsafe { hook_body(code, wparam, lparam, start) };
    let took = unsafe { GetTickCount() }.wrapping_sub(start);
    MAX_PROC_MS.fetch_max(took, Ordering::Relaxed);
    ret
}

/// フックの本体。ここでは待たない (try_lock だけ・ログなし・channel へ送るだけ)。
unsafe fn hook_body(code: i32, wparam: WPARAM, lparam: LPARAM, now: u32) -> LRESULT {
    let pass = || unsafe { CallNextHookEx(None, code, wparam, lparam) };
    if code < 0 {
        return pass();
    }

    let kb_info_ptr = lparam.0 as *const KBDLLHOOKSTRUCT;
    if kb_info_ptr.is_null() {
        return pass();
    }
    let kb_info = unsafe { &*kb_info_ptr };
    let vk = kb_info.vkCode;

    let event_type = match wparam.0 as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => EventType::Down,
        WM_KEYUP | WM_SYSKEYUP => EventType::Up,
        _ => return pass(),
    };

    // 見張り用: キーが押されてからフックに届くまでの遅れ (1分以上は時刻の基準違いとみなし無視)
    let lag = now.wrapping_sub(kb_info.time);
    if lag < 60_000 {
        MAX_DELIVERY_LAG_MS.fetch_max(lag, Ordering::Relaxed);
    }

    // (F) 修飾キーの押下状態をフックが受けた順に持つ (全キーで、他の判定より先に)
    record_modifier_event(vk, matches!(event_type, EventType::Down), now);

    let state_cell = match HOOK_STATE.get() {
        Some(s) => s,
        None => return pass(),
    };

    // HK-06: ESC キャンセル検出 (録音中のみ発動)
    const VK_ESCAPE: u32 = 0x1B;
    if vk == VK_ESCAPE {
        if matches!(event_type, EventType::Down) && IS_RECORDING.load(Ordering::Relaxed) {
            if let Ok(guard) = state_cell.try_lock() {
                if let Some(state) = guard.as_ref() {
                    let _ = state.event_tx.send(HotkeyEvent::Cancel);
                    return LRESULT(1); // 録音中 ESC は他アプリへ伝播しない
                }
            }
        }
        // 録音中でない場合は透過 (CallNextHookEx)
        return pass();
    }

    // (A) 待たない: 設定変更と重なった瞬間は素通しする
    let mut guard = match state_cell.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            LOCK_BUSY_PASSES.fetch_add(1, Ordering::Relaxed);
            return pass();
        }
    };
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return pass(),
    };

    let expected_vk = match code_to_vk(&state.config.code) {
        Some(v) => v,
        None => return pass(),
    };

    // 修飾キー単独イベントは expected_vk と一致するときだけ処理する。
    // mac/hotkey-cmd-either-side の Windows 対応 (2026-07-04): 設定 code が総称
    // ("Control"/"Meta" 等) の場合は左右どちらの VK もマッチさせる (macos_impl と同仕様)。
    let vk_matches = match generic_modifier_vk_pair(&state.config.code) {
        Some((l, r)) => vk == l || vk == r,
        None => vk == expected_vk,
    };
    if !vk_matches {
        return pass();
    }

    let is_toggle = matches!(state.config.mode, HotkeyMode::Toggle);

    // Codex P1 (2026-05-21): key-up は modifiers の押下状態に関係なく状態クリアする。
    // 「Ctrl+Shift+NonConvert を押して、Shift を先に離して、NonConvert を離す」のような
    // 一般的な release order で、modifier check を先に通すと is_currently_pressed が
    // クリアされず、次のホットキー押下が repeat 抑止で発火しなくなる。
    // PushToTalk モードでも同様の問題で Released emit されず録音 stuck する。
    match event_type {
        EventType::Down => {
            // (E) 押しっぱなし扱いのまま Down が途切れていた = 離しを取りこぼした
            let stuck = state.is_currently_pressed
                && should_reset_stuck(state.last_main_down, now);
            state.last_main_down = now;

            // Down: modifiers がすべて押されていないと発動しない
            // メインキー自身が修飾キーの場合は self exclude (要望 2026-05-21)
            if !check_modifiers_match_at(&state.config.modifiers, expected_vk, now) {
                return pass();
            }
            if state.is_currently_pressed && !stuck {
                // repeat 抑止: OS の処理は抑止して他アプリに渡さない
                return LRESULT(1);
            }
            if stuck {
                STUCK_RESETS.fetch_add(1, Ordering::Relaxed);
                if !is_toggle {
                    // PushToTalk: 取りこぼした離しの分を先に知らせて、録音を閉じてから新しく始める
                    let _ = state.event_tx.send(HotkeyEvent::Released);
                }
            }
            state.is_currently_pressed = true;
            let _ = state.event_tx.send(HotkeyEvent::Pressed);
            LRESULT(1)
        }
        EventType::Up => {
            // Up: modifiers 状態に関係なくメインキー release で必ずクリア
            // (release order の任意性に対応)
            if !state.is_currently_pressed {
                return pass();
            }
            state.is_currently_pressed = false;
            if !is_toggle {
                let _ = state.event_tx.send(HotkeyEvent::Released);
            }
            LRESULT(1)
        }
    }
}

/// 設定 code が「左右どちらでもマッチ」を意味する総称修飾キーコードなら
/// (左VK, 右VK) のペアを返す。左右指定コード・通常キーは None。
/// mac/hotkey-cmd-either-side (macos_impl::is_generic_modifier_code) の Windows 対応。
fn generic_modifier_vk_pair(code: &str) -> Option<(u32, u32)> {
    match code.to_ascii_lowercase().as_str() {
        "control" | "ctrl" => Some((VK_LCONTROL.0 as u32, VK_RCONTROL.0 as u32)),
        "shift" => Some((VK_LSHIFT.0 as u32, VK_RSHIFT.0 as u32)),
        "alt" | "option" => Some((VK_LMENU.0 as u32, VK_RMENU.0 as u32)),
        "meta" | "cmd" | "command" | "win" | "windows" | "super" => {
            Some((VK_LWIN.0 as u32, VK_RWIN.0 as u32))
        }
        _ => None,
    }
}

/// 設定 modifiers と実際の修飾キー押下状態を厳密に照合する。
///
/// ロジック:
///   1. 期待される修飾キーリストを (kind, side) に分解
///   2. 各 (kind, side) について「対応するキーが押されているか」を確認
///   3. 期待されていない kind が押されている場合は NG
///      (例: 設定 ["Control"] のとき Shift が同時に押されていたら発動させない)
///   4. メインキーが修飾キーの場合 (例: ControlRight 単体)、その kind は
///      「自分自身」なので「期待されていない」チェックから除外
pub fn check_modifiers_match(modifiers: &[String], main_vk: u32) -> bool {
    check_modifiers_match_at(modifiers, main_vk, unsafe { GetTickCount() })
}

fn check_modifiers_match_at(modifiers: &[String], main_vk: u32, now: u32) -> bool {
    let mut required: Vec<(ModifierKind, ModifierSide)> = Vec::new();
    for s in modifiers {
        if let Some(parsed) = parse_modifier_side(s) {
            required.push(parsed);
        } else {
            // 不明な modifier 文字列 → 設定不整合 → 発動させない
            return false;
        }
    }

    for (kind, side) in &required {
        if !is_modifier_pressed(*kind, *side, now) {
            return false;
        }
    }

    let main_is_modifier_kind = main_key_modifier_kind(main_vk);
    for kind in [
        ModifierKind::Control,
        ModifierKind::Shift,
        ModifierKind::Alt,
        ModifierKind::Meta,
    ] {
        // メインキー自身の kind はチェックから除外 (例: ControlRight 単体時の Control チェック)
        if main_is_modifier_kind == Some(kind) {
            continue;
        }
        let is_required = required.iter().any(|(k, _)| *k == kind);
        let is_pressed = is_modifier_pressed(kind, ModifierSide::Either, now);
        if !is_required && is_pressed {
            return false;
        }
    }

    true
}

/// メインキー VK が修飾キー (左右どちらか) ならその kind を返す。それ以外は None。
fn main_key_modifier_kind(vk: u32) -> Option<ModifierKind> {
    let v = vk as u16;
    if v == VK_LCONTROL.0 || v == VK_RCONTROL.0 {
        Some(ModifierKind::Control)
    } else if v == VK_LSHIFT.0 || v == VK_RSHIFT.0 {
        Some(ModifierKind::Shift)
    } else if v == VK_LMENU.0 || v == VK_RMENU.0 {
        Some(ModifierKind::Alt)
    } else if v == VK_LWIN.0 || v == VK_RWIN.0 {
        Some(ModifierKind::Meta)
    } else {
        None
    }
}

fn is_modifier_pressed(kind: ModifierKind, side: ModifierSide, now: u32) -> bool {
    let (l_vk, r_vk) = match kind {
        ModifierKind::Control => (VK_LCONTROL.0, VK_RCONTROL.0),
        ModifierKind::Shift => (VK_LSHIFT.0, VK_RSHIFT.0),
        ModifierKind::Alt => (VK_LMENU.0, VK_RMENU.0),
        ModifierKind::Meta => (VK_LWIN.0, VK_RWIN.0),
    };
    let left = is_vk_down(l_vk, now);
    let right = is_vk_down(r_vk, now);
    match side {
        ModifierSide::Either => left || right,
        ModifierSide::Left => left,
        ModifierSide::Right => right,
    }
}

/// (F) フックが受けた Down/Up の記録と GetAsyncKeyState のどちらかが押下なら押下。
fn is_vk_down(vk: u16, now: u32) -> bool {
    let async_down = unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0 };
    let Some(i) = mod_index(vk as u32) else {
        return async_down;
    };
    let (tracked, clear) = resolve_tracked(MOD_DOWN_AT[i].load(Ordering::Relaxed), now, async_down);
    if clear {
        MOD_DOWN_AT[i].store(0, Ordering::Relaxed);
        LOST_MODIFIER_UPS.fetch_add(1, Ordering::Relaxed);
    }
    tracked || async_down
}

#[cfg(test)]
mod tests {
    // WH_KEYBOARD_LL の動作は OS 統合が必須なので、純関数のみテスト。
    use super::*;

    #[test]
    fn check_modifiers_match_with_unknown_modifier_returns_false() {
        let modifiers = vec!["BogusModifier".to_string()];
        assert!(!check_modifiers_match(&modifiers, 0));
    }

    #[test]
    fn check_modifiers_match_with_empty_list() {
        // GetAsyncKeyState 依存なので緩い smoke test
        let _ = check_modifiers_match(&[], 0);
    }

    #[test]
    fn generic_modifier_vk_pair_matches_both_sides() {
        // 総称コードは (左VK, 右VK) ペアを返す (mac/hotkey-cmd-either-side Windows 対応)
        assert_eq!(generic_modifier_vk_pair("Control"), Some((0xA2, 0xA3)));
        assert_eq!(generic_modifier_vk_pair("shift"), Some((0xA0, 0xA1)));
        assert_eq!(generic_modifier_vk_pair("Alt"), Some((0xA4, 0xA5)));
        assert_eq!(generic_modifier_vk_pair("Meta"), Some((0x5B, 0x5C)));
        assert_eq!(generic_modifier_vk_pair("Cmd"), Some((0x5B, 0x5C)));
        // 左右指定コード・通常キーは None (従来どおり expected_vk 厳密一致)
        assert_eq!(generic_modifier_vk_pair("ControlRight"), None);
        assert_eq!(generic_modifier_vk_pair("F12"), None);
        assert_eq!(generic_modifier_vk_pair("KeyA"), None);
    }

    #[test]
    fn main_key_modifier_kind_classifies_correctly() {
        assert_eq!(main_key_modifier_kind(0xA2), Some(ModifierKind::Control));
        assert_eq!(main_key_modifier_kind(0xA3), Some(ModifierKind::Control));
        assert_eq!(main_key_modifier_kind(0xA0), Some(ModifierKind::Shift));
        assert_eq!(main_key_modifier_kind(0xA1), Some(ModifierKind::Shift));
        assert_eq!(main_key_modifier_kind(0xA4), Some(ModifierKind::Alt));
        assert_eq!(main_key_modifier_kind(0xA5), Some(ModifierKind::Alt));
        assert_eq!(main_key_modifier_kind(0x5B), Some(ModifierKind::Meta));
        assert_eq!(main_key_modifier_kind(0x5C), Some(ModifierKind::Meta));
        assert_eq!(main_key_modifier_kind(0x1D), None); // NonConvert
        assert_eq!(main_key_modifier_kind(0x41), None); // KeyA
    }

    #[test]
    fn resolve_tracked_fast_press_counts_before_async_catches_up() {
        // 素早く押した直後: GetAsyncKeyState はまだ離れているが、フックは Down を受けている
        assert_eq!(resolve_tracked(1_000, 1_030, false), (true, false));
        assert_eq!(resolve_tracked(0, 1_030, false), (false, false));
        // 押し続けている (長くても実際に押されていれば消さない)
        assert_eq!(resolve_tracked(1_000, 60_000, true), (true, false));
    }

    #[test]
    fn resolve_tracked_clears_missed_key_up() {
        assert_eq!(resolve_tracked(1_000, 1_000 + MOD_STALE_MS + 1, false), (false, true));
        // GetTickCount の一周 (49.7日) をまたいでも正しく経過時間を測る
        assert_eq!(resolve_tracked(u32::MAX - 10, 100, false), (true, false));
        assert_eq!(resolve_tracked(u32::MAX - 10, MOD_STALE_MS + 100, false), (false, true));
    }

    #[test]
    fn should_reset_stuck_only_after_gap() {
        // キーリピート (約30〜500ms 間隔) では解除しない
        assert!(!should_reset_stuck(1_000, 1_033));
        assert!(!should_reset_stuck(1_000, 2_000));
        assert!(should_reset_stuck(1_000, 1_000 + STUCK_RESET_MS + 1));
        assert!(should_reset_stuck(u32::MAX - 10, STUCK_RESET_MS));
    }

    #[test]
    fn is_stalled_detects_missing_ack() {
        // 返事あり
        assert!(!is_stalled(1_000, 1_005, 3_000));
        // 返事なし・まだ短い
        assert!(!is_stalled(1_000, 900, 1_500));
        // 返事なし・長い → 詰まり
        assert!(is_stalled(1_000, 900, 2_100));
        // GetTickCount の一周をまたぐ
        assert!(!is_stalled(u32::MAX - 5, 3, 2_000));
        assert!(is_stalled(u32::MAX - 5, u32::MAX - 100, 2_000));
    }

    #[test]
    fn record_modifier_event_tracks_down_and_up() {
        record_modifier_event(VK_RWIN.0 as u32, true, 0);
        // 時刻 0 でも「押されている」と区別できる (0 は離れている印)
        assert_eq!(MOD_DOWN_AT[7].load(Ordering::Relaxed), 1);
        record_modifier_event(VK_RWIN.0 as u32, false, 50);
        assert_eq!(MOD_DOWN_AT[7].load(Ordering::Relaxed), 0);
        record_modifier_event(0x1D, true, 50);
    }
}
