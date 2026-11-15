//! macOS マイク権限ダイアログを明示的に発火するモジュール (mac/mic-permission-prompt 2026-06-15)。
//!
//! # 背景
//!
//! cpal (CoreAudio HAL) は macOS のマイク権限が無いと無音 (peak=0) を返すだけで、
//! TCC マイク許可ダイアログを発火させない。そのため:
//!
//!   - System Settings → Privacy & Security → Microphone に RushVoice が
//!     **そもそも登録されない** (ユーザーが手動で許可することすら不可能)。
//!   - ホットキーを押しても画面上は何も起きず、ただ無音で文字起こし結果が空になる
//!     (ユーザー側で何が悪いのか切り分け不能で完全に詰む)。
//!
//! 実機 (macOS 26.4 Tahoe) で再現確認。Info.plist の NSMicrophoneUsageDescription だけでは
//! 不十分で、明示的な権限要求 API 呼び出しが必要。
//!
//! # 対策
//!
//! 起動時に `AVCaptureDevice.requestAccessForMediaType_completionHandler` を呼ぶ。
//! 完了 block は OS が TCC ダイアログ判定後に呼ぶ (notDetermined なら dialog 表示 → 応答待ち、
//! 既に granted/denied なら即時)。これでユーザーが「許可」を押せば com.olive.rushvoice が
//! Privacy リストに登録され、以降の cpal による録音が普通に動く。
//!
//! # 重要な前提条件 (これらが欠けると即 denied で詰む)
//!
//! 1. `tauri.conf.json` の `bundle.macOS.entitlements` で
//!    **`com.apple.security.device.audio-input`** を有効化していること。
//!    Tauri 2 はデフォルトで Hardened Runtime 有効で署名するため、entitlement が無いと
//!    OS は Info.plist のチェックすら行わず即 denied で返す。
//! 2. `Info.plist` (src-tauri/Info.plist 経由で自動マージ) に
//!    **`NSMicrophoneUsageDescription`** が存在すること。
//! 3. 本関数は **メインスレッド**から呼ぶこと (Tauri setup() の中で
//!    `app.handle().run_on_main_thread(...)` 経由)。AVFoundation の TCC 系 API は
//!    別スレッドから呼ぶと dialog が出ない事例を確認。
//! 4. setup() 同期内ではなく **NSApplication.run() 開始後** に呼ぶこと
//!    (tokio::spawn + sleep でディレイ)。早すぎると OS が dialog を出せず即 denied。
//!
//! # 参考
//!
//! - Apple Doc: <https://developer.apple.com/documentation/avfoundation/avcapturedevice>
//! - Hardened Runtime entitlement: <https://developer.apple.com/documentation/security/hardened_runtime>

/// 起動時に 1 度だけ呼ぶ (Tauri setup() 内から非同期ディレイ + main thread dispatch 経由)。
/// 完了 block は非同期で実行されるため、結果は app log (`[mic_permission] granted` /
/// `[mic_permission] denied`) で確認する。
///
/// 実装メモ:
///   - `objc2-av-foundation` の正規バインディング経由 (自作 msg_send! でブロックを渡す方式は
///     ABI ミスマッチ起因と思われる「即 denied」を観測したため避ける)。
///   - 先に `NSApplication.activateIgnoringOtherApps` を呼んで前景化する
///     (background 状態だと OS が dialog を出さない事例があるため保険)。
#[cfg(target_os = "macos")]
pub fn request_microphone_permission() {
    use block2::RcBlock;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::msg_send;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};

    // 先に NSApplication をアクティブにしておく。バックグラウンド状態だと dialog 不発の事例あり。
    unsafe {
        if let Some(nsapp_cls) = AnyClass::get(c"NSApplication") {
            let nsapp: *mut AnyObject = msg_send![nsapp_cls, sharedApplication];
            if !nsapp.is_null() {
                let _: () = msg_send![nsapp, activateIgnoringOtherApps: true];
            }
        }
    }

    // AVMediaTypeAudio は AVFoundation の `extern static NSString *const`。
    // AVFoundation 未 init の異常時に None が返るため安全に取り出す。
    // SAFETY: AVFoundation は build.rs でリンク済 + 既に NSApplication が起動済なので
    // 通常 Some が返る。
    let media_type = match unsafe { AVMediaTypeAudio } {
        Some(m) => m,
        None => {
            log::warn!("[mic_permission] AVMediaTypeAudio is null — AVFoundation 未初期化?");
            return;
        }
    };

    // 現状の認可状態をログ (デバッグ用):
    //   0 = notDetermined → dialog 出る想定
    //   1 = restricted
    //   2 = denied
    //   3 = authorized
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    log::info!(
        "[mic_permission] current AVCaptureDevice.authorizationStatus(audio) = {:?} \
         (0=notDetermined, 2=denied, 3=authorized)",
        status
    );

    // 完了ハンドラ。OS は内部で TCC ダイアログ判定を行い、ユーザー応答後にここを呼ぶ。
    // 既に granted/denied が確定している場合は即時呼ばれる。
    // BOOL は objc2::runtime::Bool で受ける (block2 0.6 は素の `bool` 引数を IntoBlock 実装しない)。
    let completion = RcBlock::new(|granted: Bool| {
        if granted.as_bool() {
            log::info!("[mic_permission] granted");
        } else {
            log::warn!(
                "[mic_permission] denied. cpal は無音 (peak=0) を返します。\
                 System Settings → Privacy → Microphone で RushVoice を ON にしてください"
            );
        }
    });

    // クラスメソッド:
    //   + (void)requestAccessForMediaType:(AVMediaType)mediaType
    //                  completionHandler:(void (^)(BOOL granted))handler
    //
    // SAFETY: media_type は valid な NSString reference。completion は RcBlock 内で valid。
    // OS は受け取った block を内部で Block_copy して非同期実行するため、msg_send 戻り後に
    // 呼び出し側スコープを抜けても安全 (block2 RcBlock の参照渡しは drop 不要)。
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &completion);
    }
    log::info!(
        "[mic_permission] AVCaptureDevice.requestAccessForMediaType(audio) requested \
         (notDetermined なら OS 側で dialog 表示中。granted/denied ログを待つ)"
    );
}

/// 他 OS (Windows / Linux) では no-op。
/// Windows は TCC 相当が無く、cpal がそのままマイクを開く。
#[cfg(not(target_os = "macos"))]
pub fn request_microphone_permission() {
    // intentionally empty
}
