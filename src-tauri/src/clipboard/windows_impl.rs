// clipboard/windows_impl.rs — Windows 専用 clipboard 実装
//
// Phase 1 Day 6 Stage 2-C-2 で実装した既存ロジックを Phase 5 Step A-1 で
// windows_impl モジュールに移動。ロジックは無変更。
//
// 仕様 (L058 RushVoice v2 設計):
//   - 文字起こし結果を Windows clipboard に UTF-16 LE で書き込み
//   - SendInput で Ctrl+V を送信してアクティブアプリにペースト
//   - v1 PowerShell + SendKeys から ~450ms かかっていた処理を Rust ネイティブで ~15ms に短縮
//
// PA-02 (Stage 2-D): clipboard Push/Pop
//   - 貼付前に旧 clipboard 内容を退避し、1500ms 後に非同期復元
//   - CF_UNICODETEXT のみ対応 (画像/HTML/RTF 等は範囲外・log warn で通知)
//   - 復元は tokio::spawn fire-and-forget で UX をブロックしない
//   - 2026-07-22: 固定 500ms 復元の race (重いアプリで旧内容が貼られる/連続入力の
//     上書き) を世代カウンタ+内容一致ガードで修正。詳細は paste_text_with_restore の doc。
//
// 制約 (#[cfg(target_os = "windows")]):
//   - UAC で昇格された別アプリへの SendInput は届かない (Windows 仕様)

#![cfg(target_os = "windows")]

use std::time::Duration;

use log::warn; // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)

use super::should_replace_backup;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND, MAX_PATH};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);

/// テキストを Windows clipboard (CF_UNICODETEXT) にコピーする。
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;

    unsafe {
        OpenClipboard(HWND::default())
            .map_err(|e| format!("OpenClipboard: {:?}", e))?;

        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            return Err(format!("EmptyClipboard: {:?}", e));
        }

        let hmem = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(h) => h,
            Err(e) => {
                let _ = CloseClipboard();
                return Err(format!("GlobalAlloc: {:?}", e));
            }
        };

        let ptr = GlobalLock(hmem) as *mut u16;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalLock returned null".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(hmem);

        if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0)) {
            let _ = CloseClipboard();
            return Err(format!("SetClipboardData: {:?}", e));
        }

        let _ = CloseClipboard();
    }
    Ok(())
}

/// Ctrl+V を SendInput で送信する (アクティブアプリにペースト)
pub fn send_ctrl_v() -> Result<(), String> {
    let inputs = [
        make_key(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        make_key(VK_V, KEYBD_EVENT_FLAGS(0)),
        make_key(VK_V, KEYEVENTF_KEYUP),
        make_key(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    unsafe {
        let n = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if n != inputs.len() as u32 {
            return Err(format!("SendInput sent {} of {}", n, inputs.len()));
        }
    }
    Ok(())
}

fn make_key(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 文字起こし結果をクリップボードに置いて自動的に Ctrl+V でペーストする。
/// clipboard 書き込み後にアプリが読み取れるよう 30ms 待機。
pub fn paste_text(text: &str) -> Result<(), String> {
    copy_to_clipboard(text)?;
    remember_pasted(text);
    std::thread::sleep(Duration::from_millis(30));
    send_ctrl_v()
}

// ---------------------------------------------------------------------------
// PA-02: clipboard Push / Pop
// ---------------------------------------------------------------------------

/// PA-02: 現在の clipboard の CF_UNICODETEXT 内容を退避する。
/// - CF_UNICODETEXT 以外のフォーマット (画像等) は対象外 (warn ログのみ)。
/// - clipboard が空 / 対象外フォーマットの場合は None を返す。
pub fn get_clipboard_unicode() -> Option<String> {
    unsafe {
        // CF_UNICODETEXT が利用可能かチェック
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
            return None;
        }
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }
        let result = (|| -> Option<String> {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            // HANDLE.0 は *mut c_void (windows 0.58)。CF_UNICODETEXT は GMEM_MOVEABLE だが
            // OpenClipboard 中はクリップボードマネージャが移動しないため GlobalLock 不要。
            let ptr = handle.0 as *const u16;
            if ptr.is_null() {
                return None;
            }
            // null 終端まで読んで UTF-16 → String に変換
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

/// PA-02 restore race 対策 (2026-07-22): 貼付ごとに世代番号を進め、
/// 復元タスクは「自分が最新世代」の時だけ復元する。連続音声入力で
/// 前回の復元タイマーが次の貼付テキストを上書きする競合を防ぐ。
static PASTE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------------------
// PA-04 (2026-08-06): 全フォーマット退避 + 世代をまたぐ引き継ぎ
// ---------------------------------------------------------------------------
//
// 従来の PA-02 には汚染の穴が 2 つあった。
//   1) 連続発話で世代チェックにより復元が中止されると、退避してあった
//      ユーザーの内容がその場で捨てられ、二度と戻らなかった
//      (URL をコピー → 連続で 2 回話す → URL が消える)
//   2) 退避対象が CF_UNICODETEXT のみで、スクリーンショットやファイルは
//      退避すらされず、貼付で上書きされたまま失われた
//
// 対策として、退避内容を「貼付ごとのローカル変数」ではなく PENDING_RESTORE に
// 保持し、復元を中止する世代は**内容を捨てずに次の世代へ引き継ぐ**。
// 併せて退避を全フォーマットの生バイト列に拡張した。

/// 退避したクリップボードの中身 (フォーマット番号 → 生バイト列)。
/// テキスト (CF_UNICODETEXT) だけでなく画像 (CF_DIB / PNG) や
/// ファイル (CF_HDROP) も同じ仕組みで保持する。
#[derive(Clone, Default)]
struct ClipboardSnapshot {
    entries: Vec<(u32, Vec<u8>)>,
}

impl ClipboardSnapshot {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// CF_UNICODETEXT があれば文字列として返す (貼付テキストとの比較ガード用)。
    fn text(&self) -> Option<String> {
        let bytes = &self
            .entries
            .iter()
            .find(|(fmt, _)| *fmt == CF_UNICODETEXT.0 as u32)?
            .1;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }
}

/// HGLOBAL ではないフォーマット (GDI ハンドル等)。GlobalSize/GlobalLock が
/// 使えないため退避対象から除外する。触るとクラッシュしうる。
const NON_HGLOBAL_FORMATS: &[u32] = &[
    2,      // CF_BITMAP
    3,      // CF_METAFILEPICT
    9,      // CF_PALETTE
    14,     // CF_ENHMETAFILE
    0x0080, // CF_OWNERDISPLAY
    0x0082, // CF_DSPBITMAP
    0x0083, // CF_DSPMETAFILEPICT
    0x008E, // CF_DSPENHMETAFILE
];

/// 退避する 1 フォーマットあたりの上限。4K のスクリーンショット (CF_DIB) は
/// 約 33MB なので、それが通る余裕を見て 64MB とする。
const MAX_SNAPSHOT_FORMAT_BYTES: usize = 64 * 1024 * 1024;
/// 退避全体の上限。これを超えるものは諦める (メモリ肥大の保険)。
const MAX_SNAPSHOT_TOTAL_BYTES: usize = 128 * 1024 * 1024;

/// 復元待ちのユーザー内容。世代チェックで復元を中止しても**ここには残す**ので、
/// 連続発話が途切れた時点で最初にコピーした内容が戻る。
static PENDING_RESTORE: std::sync::Mutex<Option<ClipboardSnapshot>> =
    std::sync::Mutex::new(None);
/// RushVoice が最後にクリップボードへ置いた文字起こしテキスト。
/// 「今クリップボードにあるのは自分が置いた残骸か、ユーザーがコピーした新しい内容か」
/// を見分けるために使う。
static LAST_PASTED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn remember_pasted(text: &str) {
    let mut last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner());
    *last = Some(text.to_string());
}

/// 現在のクリップボードを全フォーマット退避する。
/// 取得できないフォーマットは黙って飛ばす (遅延レンダリングでコピー元が
/// 既に終了している場合など)。
fn capture_clipboard() -> ClipboardSnapshot {
    let mut snapshot = ClipboardSnapshot::default();
    let mut total = 0usize;
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return snapshot;
        }
        let mut fmt = EnumClipboardFormats(0);
        while fmt != 0 {
            if !NON_HGLOBAL_FORMATS.contains(&fmt) {
                if let Some(bytes) = read_clipboard_bytes(fmt) {
                    if bytes.len() > MAX_SNAPSHOT_FORMAT_BYTES
                        || total + bytes.len() > MAX_SNAPSHOT_TOTAL_BYTES
                    {
                        warn!(
                            target: "rushvoice.clipboard.pa04",
                            "PA-04: skipped oversized clipboard format: format={} bytes={}",
                            fmt,
                            bytes.len()
                        );
                    } else {
                        total += bytes.len();
                        snapshot.entries.push((fmt, bytes));
                    }
                }
            }
            fmt = EnumClipboardFormats(fmt);
        }
        let _ = CloseClipboard();
    }
    snapshot
}

/// クリップボードを開いた状態で、1 フォーマットの生バイト列を複製する。
///
/// # Safety
/// 呼出元が OpenClipboard 済みであること。
unsafe fn read_clipboard_bytes(fmt: u32) -> Option<Vec<u8>> {
    let handle = GetClipboardData(fmt).ok()?;
    if handle.0.is_null() {
        return None;
    }
    let hglobal = HGLOBAL(handle.0);
    let size = GlobalSize(hglobal);
    if size == 0 {
        return None;
    }
    let ptr = GlobalLock(hglobal) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let mut buf = vec![0u8; size];
    std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), size);
    let _ = GlobalUnlock(hglobal);
    Some(buf)
}

/// 退避しておいた内容をクリップボードへ書き戻す。
fn restore_clipboard(snapshot: &ClipboardSnapshot) -> Result<(), String> {
    if snapshot.is_empty() {
        return Ok(());
    }
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e| format!("OpenClipboard: {:?}", e))?;
        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            return Err(format!("EmptyClipboard: {:?}", e));
        }
        for (fmt, bytes) in &snapshot.entries {
            let hmem = match GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        target: "rushvoice.clipboard.pa04",
                        "PA-04: GlobalAlloc failed on restore: format={} error={:?}",
                        *fmt, e
                    );
                    continue;
                }
            };
            let ptr = GlobalLock(hmem) as *mut u8;
            if ptr.is_null() {
                continue;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(hmem);
            if let Err(e) = SetClipboardData(*fmt, HANDLE(hmem.0)) {
                warn!(
                    target: "rushvoice.clipboard.pa04",
                    "PA-04: SetClipboardData failed on restore: format={} error={:?}",
                    *fmt, e
                );
            }
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

/// PA-02: 文字起こし結果をペーストし、一定時間後に旧 clipboard 内容を非同期復元する。
///
/// 手順:
///   A) 旧 clipboard を退避 (CF_UNICODETEXT のみ)
///   B) 新テキストを copy + 30ms 待機 + Ctrl+V
///   C) 1500ms 後に旧内容を復元 (tokio::spawn fire-and-forget)
///
/// 復元前ガード (2026-07-22 「別の内容が貼られる」事案の修正):
///   1) 世代チェック — 復元待ちの間に新しい貼付が走っていたら復元しない
///   2) 内容チェック — clipboard が貼付テキストのままの時だけ復元する
///      (ユーザーや他アプリが書き換えていたらそれを尊重して復元しない)
///   復元遅延 500ms→1500ms: 重いアプリ (Electron/IME 確定中) は Ctrl+V の
///   clipboard 読取が 500ms を超えることがあり、復元後に読むと旧内容が
///   貼られてしまう。ガード 2) により遅延を伸ばしても副作用はない。
///
/// 呼出元 (lib.rs handle_hotkey_event) は tauri::async_runtime::spawn 内なので
/// tokio::spawn は runtime 保証済み。
pub fn paste_text_with_restore(text: &str) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    // Step A: 旧 clipboard 退避 (全フォーマット)
    //   今あるのが「自分が直前に貼った残骸」なら退避を更新しない。
    //   更新してしまうと、連続発話のたびにユーザーの元内容が
    //   文字起こしテキストで上書きされ、最終的に戻せなくなる (PA-04 原因1)。
    {
        let current = capture_clipboard();
        let current_text = current.text();
        let last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if should_replace_backup(current_text.as_deref(), last.as_deref()) {
            if current.is_empty() {
                warn!(
                    target: "rushvoice.clipboard.pa04",
                    "PA-04: nothing to back up — clipboard empty or unsupported formats only"
                );
            }
            let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
            *pending = if current.is_empty() { None } else { Some(current) };
        }
    }

    // 世代を進める (これ以前に spawn された復元タスクは無効化される)
    let my_gen = PASTE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    // Step B: 新テキストを copy + Ctrl+V (既存 paste_text 経路)
    copy_to_clipboard(text)?;
    remember_pasted(text);
    std::thread::sleep(Duration::from_millis(30));
    send_ctrl_v()?;

    // Step C: 1500ms 後に旧内容を復元 (fire-and-forget)
    let pasted = text.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // ガード 1: 復元待ちの間に新しい貼付が走った → 自分は古い世代なので中止。
        //   PENDING_RESTORE は消さずに残し、最新世代の復元タスクに任せる。
        if PASTE_GENERATION.load(Ordering::SeqCst) != my_gen {
            log::info!("[paste] PA-04 restore deferred to newer paste (gen {})", my_gen);
            return;
        }
        // ガード 2: clipboard が貼付テキスト以外に変わっていた
        //   → ユーザーが新しくコピーしたので、退避は破棄して何もしない
        match get_clipboard_unicode() {
            Some(cur) if cur == pasted => {}
            _ => {
                log::info!("[paste] PA-04 restore skipped: clipboard changed externally (gen {})", my_gen);
                let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
                *pending = None;
                return;
            }
        }
        let snapshot = {
            let mut pending = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
            pending.take()
        };
        let Some(snapshot) = snapshot else {
            return;
        };
        let formats = snapshot.entries.len();
        match restore_clipboard(&snapshot) {
            Ok(()) => {
                let mut last = LAST_PASTED.lock().unwrap_or_else(|e| e.into_inner());
                *last = None;
                log::info!("[paste] PA-04 restored {} clipboard format(s) (gen {})", formats, my_gen);
            }
            Err(e) => warn!(
                target: "rushvoice.clipboard.pa04",
                "PA-04: clipboard restore failed: {}",
                e
            ),
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// PA-03: 直接タイプ方式 (クリップボード非経由・prompt_toolkit 系 TUI の
// ブラケットペースト未対応バグ回避用)
// ---------------------------------------------------------------------------
//
// 背景 (2026-07-10 Hermes Agent 貼付不可調査):
//   Windows Terminal + prompt_toolkit (Hermes CLI) では Ctrl+V (手動/合成
//   問わず) が一切反映されない既知不具合がある一方、直接タイピングは正常に
//   動作することを確認済み。クリップボード経由の paste_text_with_restore
//   はこの種のアプリでは無効なため、1 文字ずつ SendInput(KEYEVENTF_UNICODE)
//   で打鍵する代替経路を用意する。
//
// クリップボードを介さないため PA-02 の退避/復元は不要。

/// 1 文字を Unicode キーイベント (down/up) の INPUT ペアに変換する。
/// KEYEVENTF_UNICODE は wVk を無視するため VIRTUAL_KEY(0) で送る。
fn make_unicode_key(ch: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: ch,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 物理修飾キー (Ctrl/Shift/Alt/Win) が全て離されるまで待つ (最大 timeout_ms)。
/// ホットキー (Ctrl+Shift+無変換) の押下が残ったまま Unicode 打鍵すると、
/// ターミナル系アプリでは Ctrl+文字 の制御コードに化けて入力が消えるため。
fn wait_modifiers_released(timeout_ms: u64) {
    const MODS: &[VIRTUAL_KEY] = &[VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN];
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let held = unsafe {
            MODS.iter()
                .any(|vk| (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0)
        };
        if !held {
            return;
        }
        if std::time::Instant::now() >= deadline {
            warn!(
                target: "rushvoice.clipboard.pa03",
                "PA-03: modifiers still held after {}ms — typing anyway",
                timeout_ms
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// テキストを SendInput で 1 文字ずつ直接タイプする (クリップボード不使用)。
/// サロゲートペア (絵文字等) はそのまま UTF-16 コードユニット単位で送る
/// (Windows の SendInput はサロゲートペアを 2 つの Unicode キーイベントとして
/// 正しく合成することを許容している)。
pub fn type_text(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return Ok(());
    }

    // ホットキー由来の Ctrl/Shift 押しっぱなし対策 (最大 1.5 秒待機)
    wait_modifiers_released(1500);

    let mut inputs: Vec<INPUT> = Vec::with_capacity(wide.len() * 2);
    for ch in wide {
        inputs.push(make_unicode_key(ch, KEYBD_EVENT_FLAGS(0)));
        inputs.push(make_unicode_key(ch, KEYEVENTF_KEYUP));
    }

    unsafe {
        let n = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if n != inputs.len() as u32 {
            return Err(format!("SendInput (type) sent {} of {}", n, inputs.len()));
        }
    }
    log::info!(
        "[paste] PA-03 type mode: sent {} utf16 units as direct key events",
        inputs.len() / 2
    );
    Ok(())
}

/// アプリ名(実行ファイル名, 大文字小文字区別なし)を照合し、これらのプロセスが
/// フォアグラウンドの場合はクリップボード貼付ではなく直接タイプ方式を使う。
/// prompt_toolkit 等 Ctrl+V ブラケットペーストが機能しない Windows Terminal
/// ホストアプリが対象 (2026-07-10 Hermes Agent 事案)。
const TYPE_MODE_PROCESSES: &[&str] = &["windowsterminal.exe", "openconsole.exe"];

// Electron-style editors can defer consuming Ctrl+V. Restoring the previous
// clipboard value races that deferred read and can paste unrelated old text.
// Direct Unicode typing is also unsuitable here because a busy editor can
// process a long burst out of order. Keep the transcription on the clipboard
// and paste it as one operation; the user can overwrite it with the next copy.
const NO_RESTORE_CLIPBOARD_PROCESSES: &[&str] = &["codex.exe", "chatgpt.exe"];

/// ウィンドウクラス名での判定 (プロセス名判定の補完)。
/// コンソールウィンドウはフォアグラウンドプロセスが起動アプリ側
/// (cmd.exe / pwsh.exe / hermes.exe 等) になるためプロセス名の allowlist では
/// 網羅できない。クラス名なら従来コンソール (ConsoleWindowClass) と
/// Windows Terminal (CASCADIA_HOSTING_WINDOW_CLASS) を確実に検出できる
/// (2026-07-10 foreground=cmd.exe で type モード不発だった事案の修正)。
const TYPE_MODE_WINDOW_CLASSES: &[&str] = &[
    "ConsoleWindowClass",
    "CASCADIA_HOSTING_WINDOW_CLASS",
];

/// 現在フォアグラウンドのウィンドウクラス名を返す。取得失敗時は None。
fn foreground_window_class() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// 現在フォアグラウンドのウィンドウを所有するプロセスの実行ファイル名を返す
/// (例: "WindowsTerminal.exe")。取得失敗時は None。
fn foreground_process_exe_name() -> Option<String> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() || len == 0 {
            return None;
        }
        let full_path = String::from_utf16_lossy(&buf[..len as usize]);
        full_path
            .rsplit(['\\', '/'])
            .next()
            .map(|s| s.to_string())
    }
}

/// 文字起こし結果を貼り付ける統一エントリポイント。
/// フォアグラウンドアプリが `TYPE_MODE_PROCESSES` に該当する場合は直接タイプ、
/// それ以外は従来通りクリップボード貼付 (高速・大半のアプリで動作) を使う。
pub fn paste_or_type_text(text: &str) -> Result<(), String> {
    let fg_name = foreground_process_exe_name();
    let fg_class = foreground_window_class();
    let by_process = fg_name
        .as_deref()
        .map(|name| TYPE_MODE_PROCESSES.contains(&name.to_lowercase().as_str()))
        .unwrap_or(false);
    let no_restore_clipboard = fg_name
        .as_deref()
        .map(|name| NO_RESTORE_CLIPBOARD_PROCESSES.contains(&name.to_lowercase().as_str()))
        .unwrap_or(false);
    let by_class = fg_class
        .as_deref()
        .map(|class| TYPE_MODE_WINDOW_CLASSES.contains(&class))
        .unwrap_or(false);
    let use_type_mode = by_process || by_class;

    let route = if use_type_mode {
        "type"
    } else if no_restore_clipboard {
        "clipboard-no-restore"
    } else {
        "clipboard-restore"
    };
    log::info!(
        "[paste] PA-03 route: foreground={:?} class={:?} mode={}",
        fg_name,
        fg_class,
        route
    );

    if use_type_mode {
        type_text(text)
    } else if no_restore_clipboard {
        paste_text(text)
    } else {
        paste_text_with_restore(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PA-04 原因1: 自分が貼った残骸を退避対象にしてはならない。
    /// これを取り違えると、連続発話でユーザーの元内容が失われる。
    #[test]
    fn leftover_of_our_own_paste_is_not_backed_up() {
        assert!(!should_replace_backup(Some("文字起こし結果"), Some("文字起こし結果")));
    }

    #[test]
    fn user_copied_content_is_backed_up() {
        assert!(should_replace_backup(Some("https://example.com"), Some("文字起こし結果")));
        // まだ一度も貼っていない (起動直後)
        assert!(should_replace_backup(Some("https://example.com"), None));
        // 画像のみ等でテキストが取れない場合も、退避対象の入れ替えは行う
        assert!(should_replace_backup(None, Some("文字起こし結果")));
    }

    #[test]
    fn snapshot_text_reads_utf16_up_to_nul() {
        let mut bytes: Vec<u8> = Vec::new();
        for unit in "あいう".encode_utf16() {
            bytes.extend_from_slice(&unit.to_ne_bytes());
        }
        bytes.extend_from_slice(&0u16.to_ne_bytes()); // NUL 終端
        bytes.extend_from_slice(&0x0041u16.to_ne_bytes()); // 終端より後ろは無視される
        let snapshot = ClipboardSnapshot {
            entries: vec![(CF_UNICODETEXT.0 as u32, bytes)],
        };
        assert_eq!(snapshot.text().as_deref(), Some("あいう"));
    }

    #[test]
    fn snapshot_without_text_format_has_no_text() {
        // CF_DIB (8) だけ = スクリーンショットのみをコピーした状態
        let snapshot = ClipboardSnapshot {
            entries: vec![(8, vec![0u8; 16])],
        };
        assert!(snapshot.text().is_none());
        assert!(!snapshot.is_empty());
    }
}
