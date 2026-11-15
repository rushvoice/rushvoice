// Day 25 Phase 2-G (SSOT v3.3): alias_db / transcribe_select は ASR 補正レイヤー削除と共に撤廃
pub mod app_rules_db;
pub mod audio;
pub mod clipboard;
pub mod commands;
pub mod config_db;
pub mod crypto;
pub mod endpoints;
pub mod glossary_csv;
pub mod glossary_db;
pub mod halu;
pub mod hedged;
pub mod history_db;
pub mod hotkey;
pub mod hotkey_conflict;
pub mod keyboard_hook;
pub mod llm_postprocess;
pub mod accessibility_permission;
pub mod meeting;
pub mod meeting_commands;
pub mod meeting_logs_db;
pub mod mic_permission;
pub mod migration;
pub mod overlay;
pub mod paths;
pub mod profile_db;
pub mod prompt_sanitize;
pub mod recent_audio;
pub mod recorder;
pub mod tail_guard;
pub mod resampler;
pub mod subtitle;
pub mod subtitle_commands;
pub mod transcribe;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

/// Day 9 TRAY-01 P1-1: トレイ「終了」経由 vs ×ボタン の判別フラグ。
/// true なら CloseRequested ハンドラは prevent_close せず、app.exit(0) を貫通させる。
/// 詳細: tray.quit メニューハンドラで true をセット → app.exit(0) → CloseRequested 発火時に通す。
pub type ExitFlag = Arc<AtomicBool>;

/// v0.1.7.1 タスク B 改訂 (2026-05-28): 履歴の保持件数 (直近 N 件)。
///
/// 起動時 `history_db.delete_keep_recent(HISTORY_RETENTION_COUNT)` で cleanup が走る。
/// 旧仕様 (90 日 → 7 日の期間ベース) を撤回し、件数ベース「直近 50 件保持」に変更。
/// 判断 (2026-05-28): 1 日の録音件数にバラつきがあるため、件数で管理する方が体感に合う。
///
/// 件数ベース化による影響:
/// - 既存 v0.1.7 / v0.1.7.1 ユーザー (パイロット利用者の PC) は次回起動時 cleanup で
///   51 件目以降 (id 昇順で古いもの) を一括削除
/// - 編集学習データ (edited_text による diff → glossary 自動提案) は履歴行とともに消えるが、
///   `glossary_db` 側に保存された置換ルールは独立 DB のため影響なし
/// - frontend HistoryPage.tsx の表示文言も「直近 50 件の文字起こし」に同期
/// - PAGE_SIZE = HISTORY_RETENTION_COUNT = 50 のため、追加読込 (hasMore) UI は撤去済
pub const HISTORY_RETENTION_COUNT: i64 = 50;

/// Phase 2 A: アクティブプロファイルキーの in-memory State。
/// ホットキー録音完了後の LLM 後処理に使用する。
/// デフォルト値は "raw" (後処理なし)。
/// config_db への永続化は Phase 2 A の optional 拡張として ConfigDb KV に保存する。
pub struct ActiveProfileKey(pub std::sync::Mutex<String>);

use crate::app_rules_db::{default_app_rules_db_path, AppRulesDb};
use crate::config_db::{default_config_db_path, ConfigDb};
use crate::glossary_db::{default_glossary_db_path, GlossaryDb};
use crate::history_db::{default_history_db_path, HistoryDb};
use crate::hotkey::{HotkeyConfig, HotkeyMode, HOTKEY_CONFIG_KEY};
use crate::meeting_logs_db::{default_meeting_logs_db_path, MeetingLogsDb};
use crate::profile_db::{default_profile_db_path, ProfileDb};
use crate::keyboard_hook::{HotkeyCallback, HotkeyEvent, KeyboardHook};
use crate::recorder::{
    cancel_session, start_session, stop_session_and_transcribe, RecorderHolder, RecorderState,
};

/// アプリ全体で 1 つだけ持つ KeyboardHook。set_hotkey_config から update_config で書き換える。
pub type HookHolder = Arc<Mutex<Option<KeyboardHook>>>;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 2026-11-06: panic をログファイルへ残す (無料版 v1.0.1 と同じ)。
    // windows_subsystem = "windows" のリリースビルドでは stderr が無いため、
    // 既定の panic 表示はどこにも残らない。spawn した処理の中の panic はアプリも落とさず、
    // 「起動はしているのに反応しない」状態が無痕跡で残る。
    // 元の hook へもつなぎ、開発時の stderr 表示は残す。
    {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".into());
            log::error!("[panic] at {}: {}", loc, info);
            prev(info);
        }));
    }

    let hook_holder: HookHolder = Arc::new(Mutex::new(None));
    let hook_holder_for_setup = hook_holder.clone();

    // Day 9 TRAY-01 P1-1: ExitFlag — CloseRequested ハンドラと app.exit の衝突回避用
    let exit_flag: ExitFlag = Arc::new(AtomicBool::new(false));
    let exit_flag_for_setup = exit_flag.clone();

    // Day 9 AUT-01 P1-2: 自動起動時は --minimized 引数付きで起動される。
    // メイン window を初期 hide してトレイのみ常駐 (PC 起動毎の手動 × を不要に)。
    let start_minimized = std::env::args().any(|a| a == "--minimized");

    tauri::Builder::default()
        // Day 12 Step 1-A: Single Instance Lock (autostart より「前」に登録するのが公式推奨)
        // 2 度目以降の起動は即終了し、このコールバックが 1 度目の process 内で実行される。
        // - argv: 2 度目 process の起動引数 (--minimized 等を含む可能性あり)
        // - cwd:  2 度目 process の cwd
        // ハンドラの挙動 (2026-07-22 改訂・フォーカス横取り事故対応):
        // - ユーザーの明示起動 (--minimized なし) → メインウィンドウを show + focus
        // - --minimized 付き二重起動 (autostart 多重発火・デプロイスクリプト・
        //   旧 rushvoice-hello / 新 RushVoice インストール併存) → 何もしない。
        //   旧実装は無条件 show + focus で、作業中アプリからフォーカスを奪い
        //   「変換完了時の Ctrl+V 貼付が RushVoice 本体に吸われる」実害が発生した (報告)。
        //   2 度目自体は即終了するので overlay/履歴の重複は発生しない。
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            log::info!(
                "[single-instance] another instance attempted to start; argv={:?} cwd={:?}",
                argv, cwd
            );
            let minimized_relaunch = argv.iter().any(|a| a == "--minimized");
            if minimized_relaunch {
                log::info!("[single-instance] minimized relaunch — window not shown (focus preserved)");
                return;
            }
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        // v0.1.10 機能 E: 辞書 CSV save/open ダイアログ (DictionaryPage で使用)
        .plugin(tauri_plugin_dialog::init())
        // mac/url-scheme-trigger (2026-06-16): URL Scheme `rushvoice://toggle` で外部ツール
        // (Logi Options+ Smart Action / Shortcuts.app 等) から録音 ON/OFF を発火させる。
        // ホットキー (キー入力) 経路がHID kext で詰まる環境 (Logi Options+) の代替ルート。
        .plugin(tauri_plugin_deep_link::init())
        // Day 40 議事録履歴リンク: md ファイルとフォルダを OS 既定アプリで開く
        // (MeetingHistoryPage の「md を開く」「保存フォルダを開く」ボタンで使用)
        .plugin(tauri_plugin_opener::init())
        // v0.1.10 機能 D: ホットキー競合検出専用 (試験 register → 即 unregister のみ使用)
        //
        // 既存 WH_KEYBOARD_LL (keyboard_hook::KeyboardHook) は引き続き実運用のホットキー処理を担う。
        // global-shortcut plugin は probe_conflict が衝突検査のためだけに使い、
        // 検査終了時に必ず unregister する (= アプリ動作中は未登録状態を維持)。
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(RecorderHolder::default())
        // マイク常時スタンバイ (2026-07-23): Bluetooth HFP 冒頭切れの根本対策 State
        .manage(std::sync::Mutex::new(None::<crate::audio::MicStandby>))
        // v0.1.16 機能 L Day 2: 議事録モード専用 State (通常録音 RecorderHolder とは別)
        .manage(crate::recorder::MeetingHolder::default())
        .manage(crate::recorder::SubtitleHolder::default())
        .manage(crate::subtitle_commands::SubtitleConfigState::default())
        .manage(hook_holder.clone())
        .manage(exit_flag.clone())
        // Phase 2 A: アクティブプロファイル (起動時に config_db から復元・失敗時は "raw")
        .manage(ActiveProfileKey(std::sync::Mutex::new({
            let key = ConfigDb::open(&default_config_db_path())
                .ok()
                .and_then(|db| db.get_kv("active_profile_key").ok().flatten())
                .unwrap_or_else(|| "raw".to_string());
            log::info!("[profile] active_profile_key loaded: {:?}", key);
            key
        })))
        .setup(move |app| {
            // v0.1.3 patch2 (E): release build でもログ有効化 (cfg ガード解除)。
            //
            // 真因: v0.1.3 patch2 まで release では tauri-plugin-log が初期化されず、
            //   log::info!/warn!/error! が一切ファイル出力されなかったため、
            //   confidence_drop_flag・halu 判定・glossary fetch などの動作確認が事後不可能だった。
            //
            // 出力先 (Windows):
            //   %LOCALAPPDATA%\com.olive.rushvoice\logs\RushVoice.log
            //   (tauri-plugin-log デフォルト = app_log_dir / RotatingFile・productName.log)
            //
            // ローテーション (2026-11-06 変更・無料版 v1.0.1 と同じ):
            //   旧: plugin デフォルト = 40KB + KeepOne (= 40KB に届くとファイルごと削除)。
            //       開発機は 1 日で 40KB を超えるため、11-06 昼に文字起こしが止まったときの
            //       ログが夕方には消えていて、原因を調べられなかった。
            //   新: 2MB + KeepSome(3) (= 届いたら日付付きの名前で退避し 3 世代残す)。
            // ログレベル: Info (warn/error も含む・debug は除外)。
            //
            // 配布時の追加情報源として patch2 以降は開発機からログファイルを直接確認可能。
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(2_000_000)
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                    .build(),
            )?;

            // 段階2 Phase 1-1 (2026-08-16): 無料版 RushVoice Air からの引き継ぎ。
            //
            // ログ初期化の直後・DB を開く前に 1 回だけ走らせる。中で行うのは
            // 「Air 側に有って有料版側に無い履歴 / 単語登録をコピーする」だけで、
            // 既にファイルがあれば何もしない (＝2 回目以降の起動は実質 no-op)。
            // Air 側は読むだけ・無改変。詳細は crate::paths のファイル先頭コメント。
            match crate::paths::migrate_legacy_data_dir() {
                copied if copied.is_empty() => {
                    log::info!("[migrate] 無料版からの引き継ぎ: 対象なし");
                }
                copied => {
                    log::info!("[migrate] 無料版から引き継ぎました: {copied:?}");
                }
            }

            // mac/mic-permission-prompt (2026-06-14): macOS マイク権限ダイアログを発火。
            // cpal (CoreAudio HAL) は TCC マイク許可ダイアログを発火させずに無音を
            // 返すだけなので、明示的に API を呼んでダイアログを出す。
            //
            // タイミング & スレッド要件:
            //   1. setup() 同期内で呼ぶと NSApplication.run() がまだ始まっておらず OS が
            //      ダイアログを出せずに即 denied で返る (Tahoe 26.x 観測)。
            //   2. AVFoundation の TCC 系 API は **メインスレッド**で呼ばないとダイアログが
            //      表示されない (tokio worker から呼ぶと即 denied)。
            // 解決:
            //   tokio で 1.5 秒待ってから `run_on_main_thread` でメインスレッドへ戻して呼ぶ。
            //   setup() return 後・NSApplication が active・main window が available になり、
            //   かつ main thread で実行されるので dialog が確実に出る。
            let handle_for_mic = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let _ = handle_for_mic.run_on_main_thread(|| {
                    crate::mic_permission::request_microphone_permission();
                });
            });

            // mac/mic-permission-prompt (2026-06-15): macOS アクセシビリティ権限ダイアログを発火。
            // CGEventTap (グローバルホットキー) は accessibility 権限が必須。権限が無いと
            // CGEventTapCreate が NULL を返し F12 等が完全に動かないが、OS はダイアログを
            // 自動で出さない (ユーザーは何が悪いか分からず詰む)。AXIsProcessTrustedWithOptions
            // を呼んで OS にダイアログ表示させる。
            //
            // 注: 戻り値が false でも「ダイアログ表示を依頼した」だけなので OK。
            // ユーザー応答後に System Settings で ON にされたら次回起動で hotkey が動く。
            //
            // タイミング・スレッド要件は mic_permission と同じ (setup() 同期ではダメ・main thread 必須)。
            let handle_for_axs = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
                let _ = handle_for_axs.run_on_main_thread(|| {
                    crate::accessibility_permission::request_accessibility_permission();
                });
            });

            // Day 5 後半: 起動時 v1 → v2 設定マイグレーション (best-effort)
            {
                let v1 = migration::default_v1_config_path();
                let v2_db = config_db::default_config_db_path();
                let v2_secrets = crypto::default_secrets_path();
                match migration::migrate_v1_to_v2(&v1, &v2_db, &v2_secrets) {
                    Ok(Some(report)) => log::info!(
                        "[migration] v1 -> v2 migrated: items_count={} keys={:?}",
                        report.items_count, report.keys_migrated
                    ),
                    Ok(None) => log::info!("[migration] no v1 config or already migrated"),
                    Err(e) => log::error!("[migration] failed: {:?}", e),
                }
            }

            // 2026-11-15: 有料版のゲートウェイ終了。旧版でログインしていた利用者のログイン情報を捨て、
            // 「自分のキーか接続先を設定してください」の案内を出す印を立てる (冪等・best-effort)。
            endpoints::retire_gateway_login();

            // Day 25 Phase 2-G (SSOT v3.3): v0.1.6 (alias.db あり) → v0.1.7 マイグレーション。
            // Day 24 で実装した alias.db を既存ユーザー PC から削除する (best-effort)。
            // Day 39 Mac 致命バグ修正: APPDATA 直接参照を crate::paths::rushvoice_data_dir() 経由に変更。
            // Mac 側に alias.db 由来のレガシーファイルは存在しないが、API 統一のため同経路で読む。
            {
                let alias_db_path = paths::rushvoice_data_dir().join("alias.db");
                let removed = migration::cleanup_alias_db_v0_1_7(&alias_db_path);
                if removed > 0 {
                    log::info!(
                        "[migration] alias.db cleanup (v0.1.6 -> v0.1.7): {} file(s) removed",
                        removed
                    );
                }
            }

            // Phase 2 D: HistoryDb 初期化 + 起動時カットオフ (Q2) + State 登録
            // best-effort: 失敗しても起動継続 (履歴機能だけ無効化される)
            //
            // v0.1.7.1 タスク B 改訂 (2026-05-28 要望): 期間ベース (90 日 → 7 日) を撤回し、
            // 件数ベース「直近 50 件保持」に変更。1 日の録音件数にバラつきがあるため、
            // 件数で管理する方が利用者の体感に合う。
            // 既存ユーザーは初回起動の cleanup で 51 件目以降 (id 昇順で古いもの) が自動削除される。
            {
                let history_path = default_history_db_path();
                match HistoryDb::open(&history_path) {
                    Ok(history_db) => {
                        match history_db.delete_keep_recent(HISTORY_RETENTION_COUNT) {
                            Ok(removed) => log::info!(
                                "[history] cleanup: {} rows removed (keep most recent {})",
                                removed, HISTORY_RETENTION_COUNT
                            ),
                            Err(e) => log::warn!("[history] cleanup failed: {:?}", e),
                        }
                        app.manage(Mutex::new(history_db));
                        log::info!("[history] HistoryDb state managed: {:?}", history_path);
                    }
                    Err(e) => log::error!(
                        "[history] open failed (history feature disabled): {:?}", e
                    ),
                }
            }

            // Phase 2 C Wave 1: GlossaryDb 初期化 + State 登録
            // best-effort: 失敗しても起動継続 (辞書 UI だけ無効化される)
            {
                let glossary_path = default_glossary_db_path();
                match GlossaryDb::open(&glossary_path) {
                    Ok(glossary_db) => {
                        app.manage(Mutex::new(glossary_db));
                        log::info!("[glossary] GlossaryDb state managed: {:?}", glossary_path);
                    }
                    Err(e) => log::error!(
                        "[glossary] open failed (glossary feature disabled): {:?}", e
                    ),
                }
            }

            // Phase 2 C Wave 1 Step 3: AppRulesDb 初期化 + State 登録 (AT-06)
            // best-effort: 失敗しても起動継続 (アプリ別ルール UI だけ無効化される)
            {
                let app_rules_path = default_app_rules_db_path();
                match AppRulesDb::open(&app_rules_path) {
                    Ok(app_rules_db) => {
                        // v0.1.7.1 タスク A: autostart デフォルト ON 化 (A 案・1 度だけ自動 enable)
                        // 新規 install / v0.1.7 → v0.1.7.1 アップグレード時に 1 回だけ enable() を呼ぶ。
                        // ユーザーが UI で OFF にした場合、フラグは立ったままなので再度 ON に戻すことはない。
                        // schema_meta テーブルの `autostart_default_applied_v017_1` キーで管理。
                        // best-effort: 失敗してもアプリ起動は継続する (autostart 機能のみ未設定状態になる)。
                        apply_autostart_default_v017_1(app.handle(), &app_rules_db);
                        // v0.1.12 F3 (L265): NSIS Uninstall でレジストリ Run キーが消失する罠の
                        // 自動復元 (フラグ立 + is_enabled()=false の整合性チェック)。best-effort。
                        ensure_autostart_consistency(app.handle(), &app_rules_db);
                        app.manage(Mutex::new(app_rules_db));
                        log::info!("[app_rules] AppRulesDb state managed: {:?}", app_rules_path);
                    }
                    Err(e) => log::error!(
                        "[app_rules] open failed (app rules feature disabled): {:?}", e
                    ),
                }
            }

            // Phase 2 C Wave 2: ProfileDb 初期化 + ビルトインプリセット 3 種投入 + State 登録
            // best-effort: 失敗しても起動継続 (プロファイル UI だけ無効化される)
            {
                let profile_path = default_profile_db_path();
                match ProfileDb::open(&profile_path) {
                    Ok(profile_db) => {
                        app.manage(Mutex::new(profile_db));
                        log::info!("[profile] ProfileDb state managed: {:?}", profile_path);
                    }
                    Err(e) => log::error!(
                        "[profile] open failed (profile feature disabled): {:?}", e
                    ),
                }
            }

            // v0.1.16 機能 L (議事録モード) Day 1: MeetingLogsDb 初期化 + State 登録
            // best-effort: 失敗しても起動継続 (議事録 UI だけ無効化される)
            {
                let meeting_path = default_meeting_logs_db_path();
                match MeetingLogsDb::open(&meeting_path) {
                    Ok(meeting_db) => {
                        app.manage(Mutex::new(meeting_db));
                        log::info!(
                            "[meeting] MeetingLogsDb state managed: {:?}",
                            meeting_path
                        );
                    }
                    Err(e) => log::error!(
                        "[meeting] open failed (meeting feature disabled): {:?}", e
                    ),
                }
            }

            // Day 25 Phase 2-G (SSOT v3.3): AliasDb 初期化ブロック削除 (ASR 補正レイヤー撤廃)

            // Stage 2-C-2: Overlay window 作成 (hidden start, click-through)
            match create_overlay_window(app.handle()) {
                Ok(_) => log::info!("[overlay] window created (hidden)"),
                Err(e) => log::error!("[overlay] create failed: {}", e),
            }

            // Phase 2-F (Day 24): 起動時 overlay visibility 保証
            // --minimized (autostart 経由) でない通常起動時は overlay が必ず可視であることを保証する。
            // create_overlay_window 内で .visible(true) で build しているが、
            // OS 側の z-order / focus 競合で隠れたまま立ち上がる事案を防ぐ二重保証。
            // --minimized 経路でも overlay は常駐 indicator として表示する (main window のみ hide)。
            if !start_minimized {
                show_overlay_with_trigger(app.handle(), "startup_guard");
            } else {
                // autostart 経由でも overlay は表示する (開発機ピル消失事案 2026-05-27 対策)
                show_overlay_with_trigger(app.handle(), "startup_guard_minimized");
            }

            // Day 9 TRAY-01: タスクトレイ常駐
            if let Err(e) = setup_tray(app.handle()) {
                log::error!("[tray] setup failed: {}", e);
            }

            // Day 9 TRAY-01: メインウィンドウの×ボタンで終了せず最小化
            // P1-1: ExitFlag セット時 (= トレイ「終了」経由) は prevent_close しない。
            if let Some(main_win) = app.get_webview_window("main") {
                let main_clone = main_win.clone();
                let flag_clone = exit_flag_for_setup.clone();
                main_win.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        if flag_clone.load(Ordering::SeqCst) {
                            // tray.quit 経由・素直に閉じる
                            return;
                        }
                        api.prevent_close();
                        if let Err(e) = main_clone.hide() {
                            log::warn!("[tray] hide main window failed: {}", e);
                        }
                    }
                });

                // Day 9 AUT-01 P1-2: 自動起動 (--minimized) なら初期 hide
                if start_minimized {
                    if let Err(e) = main_win.hide() {
                        log::warn!("[tray] initial hide for --minimized failed: {}", e);
                    } else {
                        log::info!("[tray] launched with --minimized; main window hidden");
                    }
                }
            } else {
                log::warn!("[tray] main window not found at setup");
            }

            // Stage 2-C-2: KeyboardHook 起動 (WH_KEYBOARD_LL)
            let config = load_hotkey_config_or_default()
                .unwrap_or_else(|_| HotkeyConfig::default());
            let app_handle = app.handle().clone();
            let callback: HotkeyCallback = Arc::new(move |event: HotkeyEvent| {
                let app = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    handle_hotkey_event(app, event).await;
                });
            });
            match KeyboardHook::start(config.clone(), callback) {
                Ok(hook) => {
                    if let Ok(mut guard) = hook_holder_for_setup.lock() {
                        *guard = Some(hook);
                    }
                    log::info!(
                        "[hotkey] WH_KEYBOARD_LL started: code={} modifiers={:?} mode={:?}",
                        config.code, config.modifiers, config.mode
                    );
                }
                Err(e) => log::error!("[hotkey] KeyboardHook::start failed: {}", e),
            }

            // mac/url-scheme-trigger (2026-06-16): URL Scheme ハンドラ登録
            //
            // `rushvoice://toggle` URL が開かれた時 (Logi Options+ Smart Action /
            // Shortcuts.app / Finder の URL バー / ターミナル `open rushvoice://toggle` 等) に
            // ホットキー押下 (HotkeyEvent::Pressed) と同等の Toggle 動作を発火させる。
            //
            // Logi Options+ がマウスのカスタムキーを HID kext で握っており
            // CGEventTap 経路で受け取れない環境向けの代替ルート。実機確認: 2026-06-16
            // (MX Master 4 のサイドボタン → Logi Smart Action → ラッパー .app → URL → 録音 ON/OFF)。
            //
            // 認識する URL:
            //   rushvoice://toggle     → ホットキーと同等 (Toggle なら録音 ON/OFF を切替)
            //   rushvoice://start      → 強制的に録音開始 (将来用・現状は toggle と同義)
            //   rushvoice://stop       → 強制的に録音停止 (将来用・現状は toggle と同義)
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let app_handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    let urls = event.urls();
                    for url in urls {
                        log::info!("[deep-link] received: {}", url);
                        // host を見てコマンドを判定 (現状は全部 Pressed で toggle)
                        // 将来的に start/stop で別動作にしたい場合は分岐追加。
                        let host = url.host_str().unwrap_or("");
                        if matches!(host, "toggle" | "start" | "stop") {
                            let app = app_handle.clone();
                            tauri::async_runtime::spawn(async move {
                                handle_hotkey_event(app, HotkeyEvent::Pressed).await;
                            });
                        } else {
                            log::warn!(
                                "[deep-link] unknown command host={:?} url={}",
                                host, url
                            );
                        }
                    }
                });
            }

            // v0.1.10 機能 D: 起動時ホットキー競合検査 (Q3 確定: 起動時 + 設定変更時の両方)
            //
            // tauri-plugin-global-shortcut の register/unregister を「試験」で叩き、
            // 衝突なら config_db に ConflictState::Conflict を保存し
            // `hotkey:conflict-detected` event を emit する (HotkeyConfigDialog が listen)。
            // 未衝突なら ConflictState::None を保存し `hotkey:conflict-cleared` を emit する
            // (前回起動時から状態が変わっていない場合でも UI が初期化できるよう冪等に通知)。
            //
            // best-effort: probe 自体が失敗 (plugin 初期化失敗等) しても起動継続する。
            {
                use crate::hotkey_conflict::{probe_conflict, ConflictState};
                let app_handle = app.handle().clone();
                let probe_config = config.clone();
                match probe_conflict(&app_handle, &probe_config) {
                    Ok(()) => {
                        log::info!(
                            "[hotkey-conflict] startup probe: no conflict (code={} mods={:?})",
                            probe_config.code, probe_config.modifiers
                        );
                        if let Ok(db) = ConfigDb::open(&default_config_db_path()) {
                            if let Ok(j) = ConflictState::None.to_json() {
                                let _ = db.set_hotkey_conflict_state_json(&j);
                            }
                        }
                        if let Err(e) = app_handle.emit("hotkey:conflict-cleared", &probe_config) {
                            log::warn!(
                                "[hotkey-conflict] startup emit conflict-cleared failed: {}",
                                e
                            );
                        }
                    }
                    Err(reason) => {
                        log::warn!(
                            "[hotkey-conflict] startup probe: conflict detected (code={} mods={:?}) reason={}",
                            probe_config.code, probe_config.modifiers, reason
                        );
                        let detected_at = chrono::Utc::now().timestamp();
                        let state =
                            ConflictState::from_conflict(&probe_config, detected_at);
                        if let Ok(db) = ConfigDb::open(&default_config_db_path()) {
                            if let Ok(j) = state.to_json() {
                                let _ = db.set_hotkey_conflict_state_json(&j);
                            }
                        }
                        if let Err(e) = app_handle.emit("hotkey:conflict-detected", &state) {
                            log::warn!(
                                "[hotkey-conflict] startup emit conflict-detected failed: {}",
                                e
                            );
                        }
                    }
                }
            }

            // マイク常時スタンバイ (2026-07-23): 起動時に kv を読んで適用 (best-effort)
            commands::apply_mic_standby_state(app.handle());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_setting,
            commands::set_setting,
            // マイク選択 GUI (2026-07-23)
            commands::list_input_devices,
            // マイク常時スタンバイ (2026-07-23)
            commands::get_mic_standby,
            commands::set_mic_standby,
            commands::apply_mic_standby,
            commands::save_api_key,
            commands::has_api_key,
            commands::delete_api_key,
            commands::test_api_key,
            commands::test_stored_api_key,
            // 2026-11-15: カスタム接続先 (OpenAI 互換・ローカル LLM) の接続テストと、ゲートウェイ終了の案内
            commands::test_endpoint_url,
            commands::get_gateway_retired_notice,
            commands::dismiss_gateway_retired_notice,
            commands::start_recording,
            commands::stop_recording,
            commands::get_recording_state,
            commands::get_hotkey_config,
            commands::set_hotkey_config,
            // v0.1.10 機能 D: ホットキー競合状態取得
            commands::get_hotkey_conflict_state,
            commands::set_overlay_size,
            // Phase 2 D: 履歴 SQLite (HistoryDb State)
            commands::list_history,
            commands::get_history,
            commands::update_history_edit,
            commands::delete_history_item,
            commands::clear_history,
            commands::history_count,
            // Phase 2 C Wave 1: 辞書 SQLite (GlossaryDb State)
            commands::list_glossary,
            commands::glossary_hint_usage,
            commands::upsert_glossary_entry,
            commands::delete_glossary_entry,
            commands::find_glossary_duplicate,
            commands::find_glossary_overlapping,
            // v0.1.10 機能 E: 辞書 CSV インポート/エクスポート
            commands::glossary_export_csv,
            commands::glossary_import_csv_preview,
            commands::glossary_import_csv_commit,
            // Phase 2 C Wave 1 Step 3: アプリ別ルール SQLite (AppRulesDb State) AT-06
            commands::list_app_rules,
            commands::upsert_app_rule,
            commands::delete_app_rule,
            commands::replace_all_app_rules,
            // Phase 2 C Wave 2: プロファイル SQLite (ProfileDb State) PT-01〜PT-10
            commands::list_profiles,
            commands::get_profile,
            commands::upsert_profile,
            commands::delete_profile,
            commands::reset_profile_to_default,
            // Phase 2 A: アクティブプロファイル in-memory State
            commands::get_active_profile_key,
            commands::set_active_profile_key,
            // v0.1.10 機能 A: 混雑通知バナー設定 (config_db settings_kv)
            commands::get_congestion_config,
            commands::set_congestion_config,
            // v0.1.12 F3 (A 案): autostart enable/disable ラッパ
            // (plugin 呼び出し + autostart_default_applied_v017_1 フラグ操作をセットで実施)
            commands::enable_autostart,
            commands::disable_autostart,
            // v0.1.16 機能 L (議事録モード) Day 1: 6 commands 雛形登録
            // 録音/転写ロジックは Day 2-3 で実装。Day 1 は DB CRUD + state emit のみ。
            meeting_commands::meeting_start,
            meeting_commands::meeting_stop,
            meeting_commands::meeting_list,
            meeting_commands::meeting_get,
            meeting_commands::meeting_delete,
            meeting_commands::meeting_export,
            // Day 39 履歴リンク機能 (Backend のみ・frontend は Day 40):
            meeting_commands::meeting_get_export_md_path,
            meeting_commands::meeting_get_export_dir,
            // セミナー字幕モード (feature/seminar-subtitle)
            subtitle_commands::subtitle_start,
            subtitle_commands::subtitle_stop,
            subtitle_commands::subtitle_get_state,
            subtitle_commands::subtitle_open_window,
            subtitle_commands::subtitle_close_window,
            // Day 25 Phase 2-G (SSOT v3.3): alias 関連 6 commands は ASR 補正レイヤー撤廃と共に削除
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// overlay (pill) の idle 時論理サイズ。create_overlay_window の初期サイズと
/// reset_overlay_position の再配置サイズで共有する (値のドリフト防止)。
const OVERLAY_IDLE_W: f64 = 175.0;
const OVERLAY_IDLE_H: f64 = 18.0;

/// Stage 2-C-2: 常駐 overlay window (画面下端中央・always-on-top + click-through)
/// 2026-05-22 修正: 640x80 → 360x50 → 240x40 → 動的サイズ (要望)
/// state 変化時に set_overlay_size command で動的調整
/// 2026-05-23 Day 9 (指摘): 高さ半減 32 → 18px・margin_bottom 40 → 60px
///   (タスクバー裏に飲み込まれて pill が見えない事案を解消)
fn create_overlay_window(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    // hash-based routing: #/overlay を main.tsx で検出して Overlay.tsx をレンダ
    // 2026-05-23 Day 9: 横幅 200 → 175 → 125 → 175 (idle は 175 で確定・利用者 OK)
    const OVERLAY_W: f64 = OVERLAY_IDLE_W;
    const OVERLAY_H: f64 = OVERLAY_IDLE_H;
    let window = WebviewWindowBuilder::new(
        app,
        "overlay",
        WebviewUrl::App(PathBuf::from("index.html#/overlay")),
    )
    .title("RushVoice Overlay")
    .inner_size(OVERLAY_W, OVERLAY_H)
    .always_on_top(true)
    .decorations(false)
    .transparent(true)
    .resizable(false)
    .focused(false)
    // Aqua Voice 風 常駐 indicator: 起動時から idle pill 表示 (要望 2026-05-21)
    .visible(true)
    .skip_taskbar(true)
    .build()?;

    // 画面下端中央に配置 (P2-1: ヘルパー切り出し)
    if let Err(e) = overlay::position_bottom_center(&window, OVERLAY_W, OVERLAY_H) {
        log::warn!("[overlay] initial position failed: {}", e);
    }

    // click-through: マウスイベントを下のウィンドウに透過
    if let Err(e) = window.set_ignore_cursor_events(true) {
        log::warn!("[overlay] set_ignore_cursor_events failed: {}", e);
    }

    Ok(())
}

fn load_hotkey_config_or_default() -> Result<HotkeyConfig, String> {
    let db = ConfigDb::open(&default_config_db_path()).map_err(|e| e.to_string())?;
    match db.get_kv(HOTKEY_CONFIG_KEY).map_err(|e| e.to_string())? {
        Some(json) => HotkeyConfig::from_json(&json).map_err(|e| e.to_string()),
        None => Ok(HotkeyConfig::default()),
    }
}

/// HK-06: HookHolder から KeyboardHook.set_recording を呼ぶヘルパー。
/// is_recording フラグを全録音開始/停止箇所で一元管理する。
/// ⚠️ false の呼び忘れ = Idle 時の ESC を吸い込む致命バグ → このヘルパー経由で確実に呼ぶ。
fn set_hook_recording(hook_holder: &HookHolder, val: bool) {
    if let Ok(guard) = hook_holder.lock() {
        if let Some(hook) = guard.as_ref() {
            if let Err(e) = hook.set_recording(val) {
                log::warn!("[hotkey] set_recording({}) failed: {}", val, e);
            }
        }
    }
}

/// v0.1.16 機能 L Day 4: 議事録 Recording 中はホットキー処理を pause する判定 (L-R7・SSOT 準拠)。
///
/// 純粋関数で抽出して unit テスト可能化 (L294 適用)。
/// 既存 `KeyboardHook` の API は触らない方針 (本体不変・C 完了条件遵守) のため、
/// ディスパッチ層 (`handle_hotkey_event` 冒頭) で「議事録録音中なら早期 return」する。
///
/// 返り値:
///   - `meeting_recording = true`  → false (ホットキーを処理しない・skip)
///   - `meeting_recording = false` → true  (通常通り処理する)
///
/// 設計選択: `KeyboardHook` 側に `set_paused` メソッドを追加するより、
/// **状態確認は handle_hotkey_event 内で都度行う** ほうが既存 hook の API 変更不要 +
/// 配線箇所が 1 箇所に集約されるため保守容易 (L294 既存純粋関数不変原則と整合)。
pub fn should_dispatch_hotkey(meeting_recording: bool) -> bool {
    !meeting_recording
}

/// ホットキーの押下を1つずつ処理するための順番待ち (2026-10-31)。
static HOTKEY_TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
/// Toggle の押下を処理中に受けて、順番待ちしているものがあるか (1回分だけ取っておく)。
static TOGGLE_PRESS_QUEUED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 押下の順番を取る。前の押下の処理 (止める→書き起こし→貼り付け) が終わるまで待つ。
/// Toggle の押下は処理中に1回分だけ取っておき、終わったらすぐ実行する (= 次の録音を始める)。
/// 2回目以降は連打とみなして捨てる (戻り値 None)。PushToTalk は押す/離すの対を崩さないよう全部待つ。
async fn acquire_hotkey_turn(
    app: &tauri::AppHandle,
    event: HotkeyEvent,
    mode: &HotkeyMode,
) -> Option<tokio::sync::MutexGuard<'static, ()>> {
    use std::sync::atomic::Ordering;
    let turn = HOTKEY_TURN.get_or_init(|| tokio::sync::Mutex::new(()));
    if let Ok(g) = turn.try_lock() {
        return Some(g);
    }
    let toggle_press =
        matches!(event, HotkeyEvent::Pressed) && matches!(mode, HotkeyMode::Toggle);
    if !toggle_press {
        return Some(turn.lock().await);
    }
    if TOGGLE_PRESS_QUEUED.swap(true, Ordering::SeqCst) {
        log::info!("[hotkey-event] ignored: another press is already queued while busy");
        return None;
    }
    log::info!("[hotkey-event] queued: busy (stopping/transcribing/pasting); will run as soon as done");
    let _ = app.emit("recorder:queued", ());
    let g = turn.lock().await;
    TOGGLE_PRESS_QUEUED.store(false, Ordering::SeqCst);
    log::info!("[hotkey-event] running queued press");
    Some(g)
}

/// Stage 2-C-2: ホットキーイベントのハンドラ。Toggle / PushToTalk のモード分岐。
/// emit event + overlay window の show/hide も担当。
async fn handle_hotkey_event(app: tauri::AppHandle, event: HotkeyEvent) {
    log::info!("[hotkey-event] received: {:?}", event);

    // v0.1.16 機能 L Day 4 (L-R7): 議事録録音中はホットキー処理を skip する。
    // MeetingHolder::is_recording は内部で Mutex を try-take し、poisoned 時は false を返す
    // 安全側設計のため、通常録音への影響なし (false なら従来通り処理続行)。
    let meeting_recording = app
        .try_state::<crate::recorder::MeetingHolder>()
        .map(|h| h.is_recording())
        .unwrap_or(false);
    // セミナー字幕モード実行中も同様に skip (マイクの奪い合い防止・議事録と同じ扱い)。
    let subtitle_recording = app
        .try_state::<crate::recorder::SubtitleHolder>()
        .map(|h| h.is_recording())
        .unwrap_or(false);
    if !should_dispatch_hotkey(meeting_recording || subtitle_recording) {
        log::info!(
            "[hotkey-event] skipped: {} in progress",
            if meeting_recording { "meeting recording" } else { "subtitle session" }
        );
        return;
    }
    // Day 25 Phase 2-G (SSOT v3.3): multi pipeline 経路は ASR 補正レイヤー撤廃と共に削除
    let config = match load_hotkey_config_or_default() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[hotkey-event] load_hotkey_config failed: {}", e);
            let _ = app.emit("recorder:error", e);
            show_overlay(&app);
            return;
        }
    };

    // 2026-10-31: 押下は1つずつ順番に処理する (ESC は待たせない)。
    // 以前は押下ごとに並行して走り、止める処理の最中に来た押下が「まだ録音中」と見て
    // 二重に止めようとして失敗したり、書き起こし・貼り付け中の押下が合図なしで捨てられていた。
    let _hotkey_turn = if matches!(event, HotkeyEvent::Cancel) {
        None
    } else {
        match acquire_hotkey_turn(&app, event, &config.mode).await {
            Some(g) => Some(g),
            None => return,
        }
    };

    // 接続先 (Groq / OpenAI / カスタム) を先に決める。未設定なら録音を始めずに知らせる
    // (endpoints::load_transcribe_setup が設定画面への案内を含むメッセージを返す)。
    let setup = match endpoints::load_transcribe_setup() {
        Ok(s) => s,
        Err(e) => {
            // ESC (取り消し) は接続先が無くても通す
            if !matches!(event, HotkeyEvent::Cancel) {
                let _ = app.emit("recorder:error", e);
                show_overlay(&app);
                return;
            }
            // 取り消しでは使わないので空の組で埋める
            endpoints::TranscribeSetup {
                primary: endpoints::TranscribeEndpoint::default(),
                fallback: None,
            }
        }
    };

    let holder = app.state::<RecorderHolder>();
    let hook_holder = app.state::<HookHolder>();

    match (event, config.mode) {
        (HotkeyEvent::Pressed, HotkeyMode::Toggle) => {
            // 指摘 (2026-05-22): 2 回目押下後 transcribing 表示が即座に出るよう、
            // toggle_recording を経由せず Idle/Recording を見て直接分岐し、
            // transcribing emit を await の前に行う。
            let current = holder.current_state();
            match current {
                // v0.1.12 F2: マイク入力低レベル警告設定を config_db から読み込んで worker に渡す
                RecorderState::Idle => match start_session(
                    &holder,
                    Some(app.clone()),
                    commands::load_mic_low_level_config_or_default(),
                ) {
                    Ok(_) => {
                        set_hook_recording(&hook_holder, true);   // HK-06
                        show_overlay(&app);
                        let _ = app.emit("recorder:state-changed", "recording");
                    }
                    Err(e) => {
                        log::warn!("[hotkey-event] start_session failed: {}", e);
                        let _ = app.emit("recorder:error", e);
                        show_overlay(&app);
                    }
                },
                RecorderState::Recording => {
                    set_hook_recording(&hook_holder, false);  // HK-06
                    // 「文字起こし中」を await 前に先行 emit (致命 UX バグ修正)
                    let _ = app.emit("recorder:state-changed", "transcribing");
                    // v0.1.3 patch2 (C): glossary を fetch して Whisper prompt 末尾に注入
                    // v0.1.3 patch3 (C-add): 後処理 pattern→replacement 置換用 entries も取得
                    let glossary_words = commands::collect_glossary_words(&app);
                    let glossary_entries = commands::collect_glossary_entries(&app);
                    log::info!(
                        "[hotkey] glossary_words count={} entries count={}",
                        glossary_words.len(),
                        glossary_entries.len()
                    );
                    // v0.1.10 機能 A: 混雑通知バナー設定を読み込んで recorder に渡す
                    let congestion = commands::load_congestion_config_or_default();
                    match stop_session_and_transcribe(
                        &holder,
                        setup.clone(),
                        glossary_words,
                        glossary_entries,
                        Some(app.clone()),
                        congestion,
                    ).await {
                        Ok(outcome) => {
                            // 2026-07-25: LLM 後処理後テキスト (履歴 refined_text 用・診断材料)
                            let mut refined_for_history: Option<String> = None;
                            // v0.1.3 防御第 2 層: 空 text + flag=1 で clipboard 貼付抑止
                            if outcome.text.is_empty() && outcome.confidence_drop_flag {
                                log::warn!(
                                    "[paste] suppressed (toggle): confidence_drop_flag=1, raw_text retained in history"
                                );
                                let _ = app.emit(
                                    "recorder:halu-suppressed",
                                    "⚠️ ハルシネーション検出のため貼付を中止しました (履歴から手動コピー可)",
                                );
                            } else if !outcome.text.is_empty() {
                                // Phase 2 A: アクティブプロファイルで LLM 後処理
                                let final_text = commands::apply_profile_postprocess(
                                    &app, &outcome.text
                                ).await;
                                if let Err(e) = crate::clipboard::paste_or_type_text(&final_text) {
                                    log::warn!("[paste] failed: {}", e);
                                }
                                refined_for_history = Some(final_text.clone());
                                // v0.1.15 小声警告: 貼付後に小声なら警告 emit + baseline 学習更新。
                                // v0.1.16 機能 L: toggle 経路 = 通常録音 → RecorderMode::Normal。
                                commands::handle_quiet_voice_warning(
                                    &app,
                                    outcome.session_rms_mean,
                                    crate::recorder::RecorderMode::Normal,
                                );
                            }
                            // Phase 2 D: 履歴 INSERT (best-effort)
                            commands::insert_history_best_effort(&app, &outcome, refined_for_history.as_deref());
                            let display_text = outcome.raw_text_for_history.clone();
                            let _ = app.emit("recorder:state-changed", "idle");
                            let _ = app.emit("recorder:transcribed", display_text);
                        }
                        Err(e) => {
                            log::warn!("[hotkey-event] stop/transcribe failed: {}", e);
                            let _ = app.emit("recorder:state-changed", "idle");
                            let _ = app.emit("recorder:error", e);
                        }
                    }
                }
                RecorderState::Transcribing => {
                    log::warn!("[hotkey-event] pressed while recorder is transcribing (not via hotkey); ignored");
                    let _ = app.emit::<String>(
                        "recorder:error",
                        "文字起こし処理中です".into(),
                    );
                }
            }
        }
        (HotkeyEvent::Pressed, HotkeyMode::PushToTalk) => {
            // v0.1.12 F2: マイク入力低レベル警告設定を config_db から読み込んで worker に渡す
            match start_session(
                &holder,
                Some(app.clone()),
                commands::load_mic_low_level_config_or_default(),
            ) {
                Ok(_) => {
                    set_hook_recording(&hook_holder, true);   // HK-06
                    show_overlay(&app);
                    let _ = app.emit("recorder:state-changed", "recording");
                }
                Err(e) => {
                    log::warn!("[hotkey-event] start_session failed: {}", e);
                    let _ = app.emit("recorder:error", e);
                    show_overlay(&app);
                }
            }
        }
        (HotkeyEvent::Released, HotkeyMode::PushToTalk) => {
            set_hook_recording(&hook_holder, false);  // HK-06
            let _ = app.emit("recorder:state-changed", "transcribing");
            // v0.1.3 patch2 (C): glossary を fetch して Whisper prompt 末尾に注入
            // v0.1.3 patch3 (C-add): 後処理 pattern→replacement 置換用 entries も取得
            let glossary_words = commands::collect_glossary_words(&app);
            let glossary_entries = commands::collect_glossary_entries(&app);
            log::info!(
                "[hotkey-ptt] glossary_words count={} entries count={}",
                glossary_words.len(),
                glossary_entries.len()
            );
            // v0.1.10 機能 A: 混雑通知バナー設定を読み込んで recorder に渡す
            let congestion = commands::load_congestion_config_or_default();
            match stop_session_and_transcribe(
                &holder,
                setup.clone(),
                glossary_words,
                glossary_entries,
                Some(app.clone()),
                congestion,
            ).await {
                Ok(outcome) => {
                    // 2026-07-25: LLM 後処理後テキスト (履歴 refined_text 用・診断材料)
                    let mut refined_for_history: Option<String> = None;
                    // v0.1.3 防御第 2 層: 空 text + flag=1 で clipboard 貼付抑止
                    if outcome.text.is_empty() && outcome.confidence_drop_flag {
                        log::warn!(
                            "[paste] suppressed (ptt): confidence_drop_flag=1, raw_text retained in history"
                        );
                        let _ = app.emit(
                            "recorder:halu-suppressed",
                            "⚠️ ハルシネーション検出のため貼付を中止しました (履歴から手動コピー可)",
                        );
                    } else if !outcome.text.is_empty() {
                        // Phase 2 A: アクティブプロファイルで LLM 後処理
                        let final_text = commands::apply_profile_postprocess(
                            &app, &outcome.text
                        ).await;
                        if let Err(e) = crate::clipboard::paste_or_type_text(&final_text) {
                            log::warn!("[paste] failed: {}", e);
                        }
                        refined_for_history = Some(final_text.clone());
                        // v0.1.15 小声警告: 貼付後に小声なら警告 emit + baseline 学習更新。
                        // v0.1.16 機能 L: ptt 経路 = 通常録音 → RecorderMode::Normal。
                        commands::handle_quiet_voice_warning(
                            &app,
                            outcome.session_rms_mean,
                            crate::recorder::RecorderMode::Normal,
                        );
                    }
                    // Phase 2 D: 履歴 INSERT (best-effort)
                    commands::insert_history_best_effort(&app, &outcome, refined_for_history.as_deref());
                    let display_text = outcome.raw_text_for_history.clone();
                    let _ = app.emit("recorder:state-changed", "idle");
                    let _ = app.emit("recorder:transcribed", display_text);
                }
                Err(e) => {
                    log::warn!("[hotkey-event] stop/transcribe failed: {}", e);
                    let _ = app.emit("recorder:state-changed", "idle");
                    let _ = app.emit("recorder:error", e);
                }
            }
        }
        (HotkeyEvent::Released, HotkeyMode::Toggle) => {
            // Toggle モード時の Released は無視 (toggle_recording は Pressed のみで遷移)
        }
        (HotkeyEvent::Cancel, _) => {
            // HK-06: ESC キャンセル — transcribe 起動なしで即 Idle に戻す
            log::info!("[hotkey-event] cancel requested (ESC)");
            set_hook_recording(&hook_holder, false);
            match cancel_session(&holder) {
                Ok(_) => {
                    let _ = app.emit("recorder:state-changed", "idle");
                    let _ = app.emit("recorder:cancelled", ());
                }
                Err(e) => {
                    log::warn!("[cancel] cancel_session failed: {}", e);
                    let _ = app.emit("recorder:state-changed", "idle");
                }
            }
        }
    }
}

fn show_overlay(app: &tauri::AppHandle) {
    show_overlay_with_trigger(app, "default")
}

/// Phase 2-F (Day 24): overlay window を強制再表示する (trigger ラベル付き)。
///
/// trigger ラベル例: "default" / "tray_menu" / "startup_guard" / "hotkey_event"
/// hidden 状態からの復帰経路を追跡しやすくするため log::info! で記録する。
/// show() + set_focus() の両方を呼び、隠れていても確実に前面化する。
fn show_overlay_with_trigger(app: &tauri::AppHandle, trigger: &str) {
    if let Some(overlay) = app.get_webview_window("overlay") {
        match overlay.is_visible() {
            Ok(true) => log::info!("[overlay] show requested (trigger={}): already visible", trigger),
            Ok(false) => log::info!("[overlay] show requested (trigger={}): currently hidden, restoring", trigger),
            Err(e) => log::warn!("[overlay] is_visible check failed (trigger={}): {:?}", trigger, e),
        }
        match overlay.show() {
            Ok(_) => log::info!("[overlay] show OK (trigger={})", trigger),
            Err(e) => log::error!("[overlay] show failed (trigger={}): {:?}", trigger, e),
        }
        // Day 31 (2026-05-30 報告): always_on_top は build 時 1 回のみで、Windows が
        // 他の最前面/全画面ウィンドウ出現時に topmost を解除する → ピルが背面に隠れる。
        // show のたびに set_always_on_top(true) を再表明して最前面を維持する。
        if let Err(e) = overlay.set_always_on_top(true) {
            log::warn!("[overlay] set_always_on_top(true) failed (trigger={}): {:?}", trigger, e);
        }
        // overlay は focused(false) で build しているため通常 set_focus は不要だが、
        // ピル消失事案 (2026-05-27) の復帰経路では明示的に呼んで OS 側の z-order 復元を強制。
        // focus 失敗は致命的でないため warn のみ。
        if trigger == "tray_menu" {
            if let Err(e) = overlay.set_focus() {
                log::warn!("[overlay] set_focus failed (trigger={}): {:?}", trigger, e);
            }
        }
    } else {
        log::error!("[overlay] window not found (trigger={}, get_webview_window returned None)", trigger);
    }
}

/// Phase 2-F (Day 24): overlay window を本来の定位置 (画面下端中央) にリセット配置。
///
/// Day 31 (2026-05-30 報告) 変更: リセット先を右下 → 画面下端中央へ。
/// 旧実装は「画面外飛び救済」で右下 (width-200, height-100) 固定だったが、
/// 下端中央も必ず画面内かつユーザーの想定する定位置のため、下中央へ戻す方が自然。
/// 位置計算は通常配置と同じ `position_bottom_center` ヘルパーに集約 (挙動一致)。
fn reset_overlay_position(app: &tauri::AppHandle) {
    let Some(overlay) = app.get_webview_window("overlay") else {
        log::error!("[overlay] reset_position: window not found");
        return;
    };
    if let Err(e) = overlay::position_bottom_center(&overlay, OVERLAY_IDLE_W, OVERLAY_IDLE_H) {
        log::warn!("[overlay] reset_position: position_bottom_center failed: {:?}", e);
    }
    // 配置後に必ず show + 最前面再表明して確実に視認可能にする (ピル消失復帰の最終手段)。
    if let Err(e) = overlay.show() {
        log::warn!("[overlay] reset_position: show failed: {:?}", e);
    }
    if let Err(e) = overlay.set_always_on_top(true) {
        log::warn!("[overlay] reset_position: set_always_on_top failed: {:?}", e);
    }
    log::info!("[overlay] reset_position OK (bottom-center)");
}

/// Day 9 TRAY-01: タスクトレイアイコンとメニューを構築。
///
/// メニュー: 「ウィンドウを表示」「---」「終了」
/// 左クリック: メインウィンドウを show + focus (隠れていれば表示)
/// 右クリック: メニュー表示 (OS native)
/// 「終了」選択: app.exit(0) — 完全終了
fn setup_tray(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let show_item = MenuItem::with_id(app, "tray.show", "ウィンドウを表示", true, None::<&str>)?;
    // Phase 2-F (Day 24): ピル消失対策メニュー 3 件追加
    // - ピル表示: overlay を強制 show + set_focus (隠れていた場合の復帰)
    // - ピル位置リセット: overlay を画面右下にリセット配置 (画面外飛び対策)
    // - 再起動: app.restart() (最終手段・全 state リセット)
    let show_overlay_item = MenuItem::with_id(
        app, "tray.show_overlay", "ピル表示", true, None::<&str>,
    )?;
    let reset_overlay_item = MenuItem::with_id(
        app, "tray.reset_overlay_position", "ピル位置リセット", true, None::<&str>,
    )?;
    let restart_item = MenuItem::with_id(app, "tray.restart", "再起動", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "tray.quit", "終了", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &show_overlay_item,
            &reset_overlay_item,
            &sep1,
            &restart_item,
            &sep,
            &quit_item,
        ],
    )?;

    // P1-3: default_window_icon() が None (非 bundle ビルド等) でも tray を出すため、
    //       icons/icon.png を include_bytes! で埋め込みフォールバック。
    let icon = match app.default_window_icon().cloned() {
        Some(i) => i,
        None => {
            log::warn!("[tray] default_window_icon unavailable; using embedded fallback");
            Image::from_bytes(include_bytes!("../icons/icon.png"))?
        }
    };

    let _tray = TrayIconBuilder::with_id("rushvoice-tray")
        .tooltip("RushVoice v2")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray.show" => show_main_window(app),
            // Phase 2-F (Day 24): ピル消失復帰
            "tray.show_overlay" => {
                log::info!("[tray] show_overlay selected");
                show_overlay_with_trigger(app, "tray_menu");
            }
            // Phase 2-F (Day 24): ピル位置リセット (画面右下)
            "tray.reset_overlay_position" => {
                log::info!("[tray] reset_overlay_position selected");
                reset_overlay_position(app);
            }
            // Phase 2-F (Day 24): 再起動 (最終手段)
            "tray.restart" => {
                log::info!("[tray] restart selected");
                app.restart();
            }
            "tray.quit" => {
                // P1-1: ExitFlag セットしてから exit。CloseRequested ハンドラを通過させる。
                log::info!("[tray] quit selected");
                if let Some(flag) = app.try_state::<ExitFlag>() {
                    flag.store(true, Ordering::SeqCst);
                } else {
                    log::warn!("[tray] ExitFlag managed state missing; exit may be blocked");
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左クリックアップで window 表示 (OS 標準慣習)
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    log::info!("[tray] tray icon created");
    Ok(())
}

/// TRAY-01: メインウィンドウを show + focus。隠れていても unminimize + 前面化。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if let Err(e) = win.unminimize() {
            log::debug!("[tray] unminimize (likely already restored): {}", e);
        }
        if let Err(e) = win.show() {
            log::warn!("[tray] show failed: {}", e);
        }
        if let Err(e) = win.set_focus() {
            log::warn!("[tray] set_focus failed: {}", e);
        }
    } else {
        log::warn!("[tray] main window not found");
    }
}

/// v0.1.7.1 タスク A: autostart デフォルト ON 化 (A 案)。
///
/// 仕様:
/// - 新規 install / v0.1.7 以前からのアップグレード時に「1 度だけ」自動で
///   `tauri_plugin_autostart::ManagerExt::autolaunch().enable()` を呼ぶ。
/// - 完了したら `app_rules.db` の `schema_meta` テーブルに
///   `autostart_default_applied_v017_1 = "1"` (マイグレーションマーカー・不変) を立て、
///   以後は何もしない。`commands::disable_autostart` でも本マーカーは消さない (set のみ)。
/// - v0.1.12 F3 A 案 (Codex P1 追加修正) で 2 フラグ設計に分離:
///   - 本関数初回実行時に user_intent="enabled" も併せて投入する。
///   - 起動時の `ensure_autostart_consistency` は user_intent のみを参照する。
///   - ユーザーが `commands::disable_autostart` で user_intent="disabled" にすれば
///     以後再 enable されない (マーカーが立っているため本関数も skip 継続)。
///
/// 設計判断:
/// - v0.1.12 で `commands::enable_autostart` / `commands::disable_autostart` を導入し、
///   plugin 呼び出しと user_intent 操作をセットで実施する形に変更 (UI からの直接 plugin 呼びを廃止)。
///   UI 側 (SettingsPage.tsx) の状態取得 `isAutostartEnabled()` は plugin 側 is_enabled を継続利用。
/// - 既存パイロット (パイロット環境) では v0.1.6 → v0.1.7.1 経由でフラグ未投入のため
///   1 回だけ enable される。v0.1.7 で既に手動 ON にしていた場合、autolaunch().enable() は
///   idempotent なので再度呼んでも問題ない (tauri-plugin-autostart 2.5.1 の `enable()` は
///   レジストリ上書きで二重登録にはならない)。
fn apply_autostart_default_v017_1(
    app: &tauri::AppHandle,
    app_rules_db: &app_rules_db::AppRulesDb,
) {
    use tauri_plugin_autostart::ManagerExt;

    const FLAG_KEY: &str = "autostart_default_applied_v017_1";

    match app_rules_db.get_meta(FLAG_KEY) {
        Ok(Some(_)) => {
            log::info!(
                "[autostart-default] already applied (flag '{}' set); skipping",
                FLAG_KEY
            );
            return;
        }
        Ok(None) => {
            log::info!(
                "[autostart-default] flag '{}' not set; enabling autostart by default (v0.1.7.1)",
                FLAG_KEY
            );
        }
        Err(e) => {
            log::warn!(
                "[autostart-default] get_meta('{}') failed: {:?}; skipping (will retry next launch)",
                FLAG_KEY, e
            );
            return;
        }
    }

    let manager = app.autolaunch();
    if let Err(e) = manager.enable() {
        log::warn!(
            "[autostart-default] autolaunch().enable() failed: {:?}; flag NOT set, will retry next launch",
            e
        );
        return;
    }

    if let Err(e) = app_rules_db.set_meta(FLAG_KEY, "1") {
        log::warn!(
            "[autostart-default] set_meta('{}') failed: {:?}; enable() succeeded but flag NOT persisted; \
             next launch will enable() again (idempotent, no harm)",
            FLAG_KEY, e
        );
        return;
    }

    // v0.1.12 F3 (A 案 / Codex P1 追加修正): user_intent も "enabled" で初期化。
    // 以後 ensure_autostart_consistency はこの user_intent を見て復元判定する。
    // best-effort: 失敗しても enable() は成功扱い (次回起動時に再投入される可能性あり)。
    if let Err(e) = app_rules_db.set_meta(
        commands::AUTOSTART_USER_INTENT_KEY,
        commands::AUTOSTART_USER_INTENT_ENABLED,
    ) {
        log::warn!(
            "[autostart-default] set_meta('{}') failed: {:?}; migration flag set but user_intent NOT persisted",
            commands::AUTOSTART_USER_INTENT_KEY, e
        );
    }

    log::info!(
        "[autostart-default] autostart enabled successfully; flag '{}' persisted (one-time)",
        FLAG_KEY
    );
}

/// v0.1.12 F3 (L265): NSIS Uninstall 後の autostart 整合性チェック (起動時 best-effort)。
///
/// 背景 / 真因 (L265):
/// - NSIS インストーラの "Uninstall before installing" シーケンスで
///   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` の登録が消失する。
/// - 一方で `apply_autostart_default_v017_1` が立てるフラグは
///   `app_rules.db` (永続) に残るため、再インストール後も
///   「フラグ立=既に適用済」と判定され 1 度だけ enable のロジックが発火しない。
/// - 結果: ユーザー視点では「autostart ON のはず」なのに OS 再起動後に起動しなくなる。
///
/// 対策方針 (v0.1.12 F3 A 案 / Codex P1 追加修正で 2 フラグ設計に確定):
/// - 判定キー = `autostart_user_intent` (値: "enabled" / "disabled")。
///   `commands::enable_autostart` / `commands::disable_autostart` がユーザー操作時に更新する。
/// - user_intent="enabled" かつ `autolaunch().is_enabled()`=false の場合のみ復元 enable する
///   (NSIS Uninstall 等でレジストリだけ消えたケース)。
/// - user_intent="disabled" は何もしない (= ユーザー OFF 意図を尊重)。
/// - user_intent 未設定は何もしない (= 初回 install で apply_autostart_default が動く前 or
///   未マイグレ状態。初回 enable パスはあちら任せ)。
/// - `apply_autostart_default_v017_1` の不変マイグレーションフラグ
///   (`autostart_default_applied_v017_1`) はここでは参照しない。
///   役割分離: マイグレーション完了マーカー vs ユーザー意思表示。
/// - best-effort: 失敗してもアプリ起動は継続する。
fn ensure_autostart_consistency(
    app: &tauri::AppHandle,
    app_rules_db: &app_rules_db::AppRulesDb,
) {
    use tauri_plugin_autostart::ManagerExt;

    let intent_raw = match app_rules_db.get_meta(commands::AUTOSTART_USER_INTENT_KEY) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "[autostart-restore] get_meta('{}') failed: {:?}; skipping",
                commands::AUTOSTART_USER_INTENT_KEY,
                e
            );
            return;
        }
    };

    // v0.1.12 F3 (Codex P1 2nd round): 既存パイロット PC 用バックフィル。
    // user_intent 未設定 + マイグレーションフラグ set = v0.1.7.1〜v0.1.11 で
    // 既に default ON 化が実施済の既存 install。これらは user_intent="enabled"
    // 相当として扱い (= ユーザーが明示的に OFF にしていない)、本関数で復元対象とする。
    // 同時に user_intent="enabled" を永続化して次回以降は通常パスに合流させる。
    let effective_intent: Option<String> = match intent_raw {
        Some(v) => Some(v),
        None => {
            let migration_done = matches!(
                app_rules_db.get_meta(commands::AUTOSTART_MIGRATION_FLAG_KEY),
                Ok(Some(_))
            );
            if migration_done {
                log::info!(
                    "[autostart-restore] user_intent unset but migration flag present; \
                     backfilling user_intent='{}' (existing pilot install)",
                    commands::AUTOSTART_USER_INTENT_ENABLED
                );
                if let Err(e) = app_rules_db.set_meta(
                    commands::AUTOSTART_USER_INTENT_KEY,
                    commands::AUTOSTART_USER_INTENT_ENABLED,
                ) {
                    log::warn!(
                        "[autostart-restore] backfill set_meta('{}') failed: {:?}; \
                         proceeding with in-memory intent only",
                        commands::AUTOSTART_USER_INTENT_KEY,
                        e
                    );
                }
                Some(commands::AUTOSTART_USER_INTENT_ENABLED.to_string())
            } else {
                None
            }
        }
    };

    match effective_intent.as_deref() {
        Some(s) if s == commands::AUTOSTART_USER_INTENT_ENABLED => {
            // 復元対象
        }
        Some(s) if s == commands::AUTOSTART_USER_INTENT_DISABLED => {
            // ユーザー OFF 意図を尊重 (no-op)
            return;
        }
        Some(other) => {
            log::warn!(
                "[autostart-restore] unexpected user_intent value '{}'; treating as no-op",
                other
            );
            return;
        }
        None => {
            // 真の初回 install (マイグレーション未実行)。
            // 初回 enable パスは apply_autostart_default_v017_1 が担当するため no-op。
            return;
        }
    }

    let manager = app.autolaunch();
    match manager.is_enabled() {
        Ok(false) => {
            log::info!(
                "[autostart-restore] user_intent='enabled' but is_enabled()=false; \
                 re-enabling (NSIS uninstall recovery)"
            );
            if let Err(e) = manager.enable() {
                log::warn!("[autostart-restore] re-enable failed: {:?}", e);
            } else {
                log::info!("[autostart-restore] re-enable succeeded");
            }
        }
        Ok(true) => {
            // 既に enabled = 正常 (no-op)
        }
        Err(e) => log::warn!("[autostart-restore] is_enabled() failed: {:?}", e),
    }
}

// ============================================================================
// v0.1.16 機能 L Day 4: 純粋関数 unit tests
// ============================================================================

#[cfg(test)]
mod hotkey_dispatch_tests {
    use super::should_dispatch_hotkey;

    /// 議事録録音中でない場合はホットキー処理を続行する (= true)。
    /// 通常録音フロー (toggle/PTT) が今までどおり動く anchor。
    #[test]
    fn should_dispatch_hotkey_dispatches_when_not_meeting() {
        assert!(should_dispatch_hotkey(false));
    }

    /// 議事録録音中はホットキー処理を skip する (= false)。
    /// L-R7 (議事録 Recording 中はホットキー pause) の純粋関数表現。
    #[test]
    fn should_dispatch_hotkey_skips_when_meeting() {
        assert!(!should_dispatch_hotkey(true));
    }

    /// 真理値表として唯一の意味のある条件 = 引数の論理反転。
    /// 引数 true/false の両入力でそれぞれ反対の値を返すことで等価性を検証する。
    #[test]
    fn should_dispatch_hotkey_is_logical_not() {
        for b in [true, false] {
            assert_eq!(should_dispatch_hotkey(b), !b);
        }
    }
}

