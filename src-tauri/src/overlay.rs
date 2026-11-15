// overlay.rs — Overlay window 位置・サイズ管理ヘルパー
//
// Phase 1 Day 7 P2-1: lib.rs と commands.rs に重複していた「画面下端中央配置」ロジックを集約。
// 将来 overlay の配置先 (右下/左下等) を変更する場合、本ファイル 1 箇所の修正で全箇所反映される。

use tauri::{PhysicalPosition, WebviewWindow};

/// 指定された WebviewWindow を、現在のモニターの画面下端中央 (タスクバー上 40px) に配置する。
///
/// 引数:
/// - `window`: 配置対象の WebviewWindow (通常 overlay)
/// - `logical_w` / `logical_h`: window の論理サイズ (DPI スケール前)
///
/// 失敗パターン:
/// - `current_monitor()` が None or Err → 何もせず Ok を返す (画面情報取得不能でも crash しない)
/// - `set_position` 失敗 → warn ログを残して Ok を返す (UX 劣化だが致命的ではない)
pub fn position_bottom_center(
    window: &WebviewWindow,
    logical_w: f64,
    logical_h: f64,
) -> Result<(), tauri::Error> {
    let monitor = match window.current_monitor()? {
        Some(m) => m,
        None => {
            log::warn!("[overlay] current_monitor returned None; skipping reposition");
            return Ok(());
        }
    };

    let monitor_size = monitor.size();
    let scale = monitor.scale_factor();
    let win_w = (logical_w * scale) as i32;
    let win_h = (logical_h * scale) as i32;
    // Day 9 (2026-05-23 指摘): margin_bottom 40 → 60 → 42px。
    // 段階 1: 40 → 60 (Windows 10/11 タスクバー 48px 飲み込み問題を解消)
    // 段階 2: 60 → 42 (pill 視認後・要望「pill 高さ 18px 分下げてよい」)
    // タスクバー 48px ぴったりの環境では pill 下端と 6px 余裕。
    let margin_bottom = (42.0 * scale) as i32;
    let x = (monitor_size.width as i32 - win_w) / 2;
    let y = monitor_size.height as i32 - win_h - margin_bottom;

    // Day 9: 配置後の物理座標を info ログ。タスクバー裏隠れ事案の再発時に確認できるよう、
    // monitor サイズ・scale・最終 (x, y) を残す。
    log::info!(
        "[overlay] reposition: monitor=({}x{} scale={:.2}) win=({}x{}) pos=({},{}) margin_bottom={}",
        monitor_size.width, monitor_size.height, scale,
        win_w, win_h, x, y, margin_bottom
    );

    if let Err(e) = window.set_position(PhysicalPosition::new(x, y)) {
        log::warn!("[overlay] set_position failed: {}", e);
    }

    Ok(())
}
