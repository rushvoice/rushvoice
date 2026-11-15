// hotkey.rs — Phase 1 Day 6 Stage 2-C-1: グローバルホットキー設定
//
// 仕様: 内部設計メモ
//
// 設計概要:
//   - HotkeyConfig は W3C UI Events Code 名 (例 "NonConvert") + Modifier 名 ("Control", "Shift") + Mode で保持
//   - JSON で config_db.settings_kv の "hotkey.config" キーに保存
//   - tauri_plugin_global_shortcut::Shortcut への変換は to_shortcut() で
//   - デフォルト: Ctrl+Shift+NonConvert / Toggle (要望 2026-05-21)
//   - Mode::PushToTalk は Stage 2-C-1 では UI 側で disabled 扱い (枠組みは用意)

use serde::{Deserialize, Serialize};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
use thiserror::Error;

pub const HOTKEY_CONFIG_KEY: &str = "hotkey.config";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyMode {
    /// 押すたびに録音 ON/OFF を切替 (Stage 2-C-1 標準)
    #[default]
    Toggle,
    /// 押下中だけ録音 (Stage 2-C-2 で frontend に解禁)
    PushToTalk,
}

/// ホットキー設定。frontend と JSON で双方向にやり取りする。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HotkeyConfig {
    /// W3C UI Events Code 名。例: "NonConvert", "KeyA", "Space"
    pub code: String,
    /// 修飾キー名 (大小区別なし)。例: ["Control", "Shift"]
    /// 順序非依存・重複は to_shortcut() の OR 合成で吸収される
    pub modifiers: Vec<String>,
    pub mode: HotkeyMode,
}

#[derive(Debug, Error)]
pub enum HotkeyError {
    #[error("invalid key code: {0}")]
    InvalidCode(String),
    #[error("invalid modifier: {0}")]
    InvalidModifier(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            code: "NonConvert".to_string(),
            modifiers: vec!["Control".to_string(), "Shift".to_string()],
            mode: HotkeyMode::Toggle,
        }
    }
}

impl HotkeyConfig {
    pub fn from_json(json: &str) -> Result<Self, HotkeyError> {
        Ok(serde_json::from_str(json)?)
    }

    pub fn to_json(&self) -> Result<String, HotkeyError> {
        Ok(serde_json::to_string(self)?)
    }

    /// tauri_plugin_global_shortcut::Shortcut に変換する。
    /// 修飾キーが空なら Shortcut::new(None, code) になり、純粋な単キーホットキー扱い。
    pub fn to_shortcut(&self) -> Result<Shortcut, HotkeyError> {
        let code = parse_code(&self.code)?;
        let mut mods = Modifiers::empty();
        for m in &self.modifiers {
            mods |= parse_modifier(m)?;
        }
        let mods_opt = if mods.is_empty() { None } else { Some(mods) };
        Ok(Shortcut::new(mods_opt, code))
    }
}

/// W3C UI Events Code 文字列 → global-hotkey::Code 変換。
///
/// 全 215 variant をサポートする必要はないため、よく使う日本語 IME キー / アルファベット /
/// 数字 / ファンクション / 主要制御キーに限定。範囲外は InvalidCode を返す。
pub fn parse_code(s: &str) -> Result<Code, HotkeyError> {
    // 日本語 IME 関連
    let code = match s {
        "NonConvert" => Code::NonConvert,
        "Convert" => Code::Convert,
        "KanaMode" => Code::KanaMode,
        "Lang1" => Code::Lang1,
        "Lang2" => Code::Lang2,
        "Hiragana" => Code::Hiragana,
        "Katakana" => Code::Katakana,

        // 制御キー
        "Space" => Code::Space,
        "Enter" => Code::Enter,
        "Escape" => Code::Escape,
        "Tab" => Code::Tab,
        "Backspace" => Code::Backspace,
        "Delete" => Code::Delete,
        "Insert" => Code::Insert,
        "Home" => Code::Home,
        "End" => Code::End,
        "PageUp" => Code::PageUp,
        "PageDown" => Code::PageDown,
        "ArrowUp" => Code::ArrowUp,
        "ArrowDown" => Code::ArrowDown,
        "ArrowLeft" => Code::ArrowLeft,
        "ArrowRight" => Code::ArrowRight,
        "CapsLock" => Code::CapsLock,

        // アルファベット
        "KeyA" => Code::KeyA, "KeyB" => Code::KeyB, "KeyC" => Code::KeyC,
        "KeyD" => Code::KeyD, "KeyE" => Code::KeyE, "KeyF" => Code::KeyF,
        "KeyG" => Code::KeyG, "KeyH" => Code::KeyH, "KeyI" => Code::KeyI,
        "KeyJ" => Code::KeyJ, "KeyK" => Code::KeyK, "KeyL" => Code::KeyL,
        "KeyM" => Code::KeyM, "KeyN" => Code::KeyN, "KeyO" => Code::KeyO,
        "KeyP" => Code::KeyP, "KeyQ" => Code::KeyQ, "KeyR" => Code::KeyR,
        "KeyS" => Code::KeyS, "KeyT" => Code::KeyT, "KeyU" => Code::KeyU,
        "KeyV" => Code::KeyV, "KeyW" => Code::KeyW, "KeyX" => Code::KeyX,
        "KeyY" => Code::KeyY, "KeyZ" => Code::KeyZ,

        // 数字
        "Digit0" => Code::Digit0, "Digit1" => Code::Digit1, "Digit2" => Code::Digit2,
        "Digit3" => Code::Digit3, "Digit4" => Code::Digit4, "Digit5" => Code::Digit5,
        "Digit6" => Code::Digit6, "Digit7" => Code::Digit7, "Digit8" => Code::Digit8,
        "Digit9" => Code::Digit9,

        // ファンクション
        "F1" => Code::F1, "F2" => Code::F2, "F3" => Code::F3,
        "F4" => Code::F4, "F5" => Code::F5, "F6" => Code::F6,
        "F7" => Code::F7, "F8" => Code::F8, "F9" => Code::F9,
        "F10" => Code::F10, "F11" => Code::F11, "F12" => Code::F12,

        _ => return Err(HotkeyError::InvalidCode(s.to_string())),
    };
    Ok(code)
}

/// 修飾キー文字列 → Modifiers 変換。大小区別なし・別名 (Ctrl/Win/Meta) も受ける。
///
/// Stage 2-C-2 (2026-05-21): 左右区別キー (ControlLeft/Right, ShiftLeft/Right, AltLeft/Right) も受け付ける。
/// global-hotkey の Modifiers は左右区別しないため、ここでは総称扱いに丸める (バックエンドは
/// WH_KEYBOARD_LL なので Modifiers 自体は使われず、check_modifier_match() が真の照合役)。
pub fn parse_modifier(s: &str) -> Result<Modifiers, HotkeyError> {
    let mods = match s.to_ascii_lowercase().as_str() {
        "control" | "ctrl" | "controlleft" | "controlright" => Modifiers::CONTROL,
        "shift" | "shiftleft" | "shiftright" => Modifiers::SHIFT,
        "alt" | "option" | "altleft" | "altright" => Modifiers::ALT,
        "meta" | "super" | "win" | "windows" | "command" | "cmd"
        | "metaleft" | "metaright" => Modifiers::SUPER,
        _ => return Err(HotkeyError::InvalidModifier(s.to_string())),
    };
    Ok(mods)
}

/// Stage 2-C-2: 修飾キー文字列が「総称」「左のみ」「右のみ」「無関係」のどれかを判定。
///
/// WH_KEYBOARD_LL hook 内で「Ctrl が押されているか」を判断する際に使う。
/// 設定: `["Control"]` なら左右どちらでも OK / `["ControlLeft"]` なら左のみ /
/// `["ControlRight"]` なら右のみ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierSide {
    /// 左右どちらでもマッチ (旧来の "Control" / "Shift" / "Alt" / "Meta")
    Either,
    /// 左のみマッチ (W3C "ControlLeft" 等)
    Left,
    /// 右のみマッチ (W3C "ControlRight" 等)
    Right,
}

/// 修飾キーの種類 (Ctrl/Shift/Alt/Meta)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKind {
    Control,
    Shift,
    Alt,
    Meta,
}

/// 修飾キー文字列を ModifierKind + ModifierSide に分解。WH_KEYBOARD_LL hook で照合に使う。
pub fn parse_modifier_side(s: &str) -> Option<(ModifierKind, ModifierSide)> {
    let l = s.to_ascii_lowercase();
    match l.as_str() {
        "control" | "ctrl" => Some((ModifierKind::Control, ModifierSide::Either)),
        "controlleft" => Some((ModifierKind::Control, ModifierSide::Left)),
        "controlright" => Some((ModifierKind::Control, ModifierSide::Right)),
        "shift" => Some((ModifierKind::Shift, ModifierSide::Either)),
        "shiftleft" => Some((ModifierKind::Shift, ModifierSide::Left)),
        "shiftright" => Some((ModifierKind::Shift, ModifierSide::Right)),
        "alt" | "option" => Some((ModifierKind::Alt, ModifierSide::Either)),
        "altleft" => Some((ModifierKind::Alt, ModifierSide::Left)),
        "altright" => Some((ModifierKind::Alt, ModifierSide::Right)),
        "meta" | "super" | "win" | "windows" | "command" | "cmd" => {
            Some((ModifierKind::Meta, ModifierSide::Either))
        }
        "metaleft" => Some((ModifierKind::Meta, ModifierSide::Left)),
        "metaright" => Some((ModifierKind::Meta, ModifierSide::Right)),
        _ => None,
    }
}

/// W3C UI Events Code 名 → Windows Virtual Key (VK) コード変換。
/// Stage 2-C-2 keyboard_hook で「メインキーが押されたか」を判定するのに使う。
///
/// 参考: https://docs.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes
pub fn code_to_vk(code: &str) -> Option<u32> {
    Some(match code {
        // 日本語 IME 関連
        "NonConvert" => 0x1D, // VK_NONCONVERT
        "Convert" => 0x1C,    // VK_CONVERT
        "KanaMode" => 0x15,   // VK_KANA / VK_HANGUL
        "Lang1" => 0x15,      // Windows では VK_KANA と同値
        "Lang2" => 0x19,      // VK_HANJA / VK_KANJI

        // 修飾キー単独 (メインキーとして使う場合・要望 2026-05-21)
        // 例: ControlRight 単体ホットキーで「右Ctrl 押すだけで録音」UX
        "ControlLeft" => 0xA2,  // VK_LCONTROL
        "ControlRight" => 0xA3, // VK_RCONTROL
        "ShiftLeft" => 0xA0,    // VK_LSHIFT
        "ShiftRight" => 0xA1,   // VK_RSHIFT
        "AltLeft" => 0xA4,      // VK_LMENU
        "AltRight" => 0xA5,     // VK_RMENU
        "MetaLeft" => 0x5B,     // VK_LWIN
        "MetaRight" => 0x5C,    // VK_RWIN

        // mac/hotkey-cmd-either-side (2026-06-15): 左右どちらでもマッチする総称コード。
        // 戻り値は代表値 (左側)。Windows 版 keyboard_hook 側で総称コード判定して右側も拾う。
        "Control" | "Ctrl" => 0xA2,
        "Shift" => 0xA0,
        "Alt" | "Option" => 0xA4,
        "Meta" | "Cmd" | "Command" | "Win" | "Windows" | "Super" => 0x5B,

        // 制御キー
        "Space" => 0x20,
        "Enter" => 0x0D,
        "Escape" => 0x1B,
        "Tab" => 0x09,
        "Backspace" => 0x08,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowUp" => 0x26,
        "ArrowDown" => 0x28,
        "ArrowLeft" => 0x25,
        "ArrowRight" => 0x27,
        "CapsLock" => 0x14,

        // アルファベット A-Z → 0x41-0x5A
        s if s.len() == 4 && s.starts_with("Key") => {
            let c = s.chars().nth(3)?;
            if c.is_ascii_uppercase() {
                0x41 + (c as u32 - 'A' as u32)
            } else {
                return None;
            }
        }

        // Digit0-9 → 0x30-0x39
        s if s.len() == 6 && s.starts_with("Digit") => {
            let c = s.chars().nth(5)?;
            if c.is_ascii_digit() {
                0x30 + (c as u32 - '0' as u32)
            } else {
                return None;
            }
        }

        // F1-F12 → 0x70-0x7B
        s if s.starts_with('F') => {
            let n: u32 = s[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                0x70 + n - 1
            } else {
                return None;
            }
        }

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ctrl_shift_nonconvert_toggle() {
        let c = HotkeyConfig::default();
        assert_eq!(c.code, "NonConvert");
        assert_eq!(c.modifiers, vec!["Control".to_string(), "Shift".to_string()]);
        assert_eq!(c.mode, HotkeyMode::Toggle);
    }

    #[test]
    fn json_roundtrip_default() {
        let c = HotkeyConfig::default();
        let s = c.to_json().unwrap();
        let parsed = HotkeyConfig::from_json(&s).unwrap();
        assert_eq!(c, parsed);
    }

    #[test]
    fn json_roundtrip_pushtotalk() {
        let c = HotkeyConfig {
            code: "Space".to_string(),
            modifiers: vec!["Alt".to_string()],
            mode: HotkeyMode::PushToTalk,
        };
        let s = c.to_json().unwrap();
        assert!(s.contains("\"pushtotalk\""), "got: {}", s);
        let parsed = HotkeyConfig::from_json(&s).unwrap();
        assert_eq!(c, parsed);
    }

    #[test]
    fn parse_code_japanese_ime_keys() {
        assert!(matches!(parse_code("NonConvert").unwrap(), Code::NonConvert));
        assert!(matches!(parse_code("Convert").unwrap(), Code::Convert));
        assert!(matches!(parse_code("KanaMode").unwrap(), Code::KanaMode));
        assert!(matches!(parse_code("Lang1").unwrap(), Code::Lang1));
    }

    #[test]
    fn parse_code_letters_and_digits() {
        assert!(matches!(parse_code("KeyA").unwrap(), Code::KeyA));
        assert!(matches!(parse_code("KeyZ").unwrap(), Code::KeyZ));
        assert!(matches!(parse_code("Digit0").unwrap(), Code::Digit0));
        assert!(matches!(parse_code("Digit9").unwrap(), Code::Digit9));
    }

    #[test]
    fn parse_code_invalid_returns_error() {
        assert!(matches!(parse_code("BogusKey"), Err(HotkeyError::InvalidCode(_))));
        assert!(matches!(parse_code(""), Err(HotkeyError::InvalidCode(_))));
        // KeyAA や Digit10 のようなパターン外
        assert!(matches!(parse_code("KeyAA"), Err(HotkeyError::InvalidCode(_))));
    }

    #[test]
    fn parse_modifier_case_insensitive_with_aliases() {
        assert_eq!(parse_modifier("Control").unwrap(), Modifiers::CONTROL);
        assert_eq!(parse_modifier("CTRL").unwrap(), Modifiers::CONTROL);
        assert_eq!(parse_modifier("shift").unwrap(), Modifiers::SHIFT);
        assert_eq!(parse_modifier("Alt").unwrap(), Modifiers::ALT);
        assert_eq!(parse_modifier("Win").unwrap(), Modifiers::SUPER);
        assert_eq!(parse_modifier("Cmd").unwrap(), Modifiers::SUPER);
    }

    #[test]
    fn parse_modifier_invalid_returns_error() {
        assert!(matches!(parse_modifier("Fn"), Err(HotkeyError::InvalidModifier(_))));
        assert!(matches!(parse_modifier(""), Err(HotkeyError::InvalidModifier(_))));
    }

    #[test]
    fn to_shortcut_default_yields_ctrl_shift_nonconvert() {
        let c = HotkeyConfig::default();
        let s = c.to_shortcut().unwrap();
        // Shortcut の中身は private なので、key/mods アクセサで照合
        assert!(matches!(s.key, Code::NonConvert));
        let expected = Modifiers::CONTROL | Modifiers::SHIFT;
        assert_eq!(s.mods, expected);
    }

    #[test]
    fn to_shortcut_no_modifiers_is_none() {
        let c = HotkeyConfig {
            code: "F1".to_string(),
            modifiers: vec![],
            mode: HotkeyMode::Toggle,
        };
        let s = c.to_shortcut().unwrap();
        assert!(matches!(s.key, Code::F1));
        assert_eq!(s.mods, Modifiers::empty());
    }

    #[test]
    fn to_shortcut_invalid_code_propagates() {
        let c = HotkeyConfig {
            code: "BogusKey".to_string(),
            modifiers: vec!["Control".to_string()],
            mode: HotkeyMode::Toggle,
        };
        assert!(matches!(c.to_shortcut(), Err(HotkeyError::InvalidCode(_))));
    }

    // ----- Stage 2-C-2 code_to_vk テスト -----

    #[test]
    fn code_to_vk_japanese_ime() {
        assert_eq!(code_to_vk("NonConvert"), Some(0x1D));
        assert_eq!(code_to_vk("Convert"), Some(0x1C));
        assert_eq!(code_to_vk("KanaMode"), Some(0x15));
        assert_eq!(code_to_vk("Lang1"), Some(0x15));
        assert_eq!(code_to_vk("Lang2"), Some(0x19));
    }

    #[test]
    fn code_to_vk_letters_and_digits() {
        assert_eq!(code_to_vk("KeyA"), Some(0x41));
        assert_eq!(code_to_vk("KeyZ"), Some(0x5A));
        assert_eq!(code_to_vk("KeyE"), Some(0x45));
        assert_eq!(code_to_vk("Digit0"), Some(0x30));
        assert_eq!(code_to_vk("Digit9"), Some(0x39));
    }

    #[test]
    fn code_to_vk_function_keys() {
        assert_eq!(code_to_vk("F1"), Some(0x70));
        assert_eq!(code_to_vk("F12"), Some(0x7B));
    }

    #[test]
    fn code_to_vk_control_keys() {
        assert_eq!(code_to_vk("Space"), Some(0x20));
        assert_eq!(code_to_vk("Enter"), Some(0x0D));
        assert_eq!(code_to_vk("Tab"), Some(0x09));
        assert_eq!(code_to_vk("ArrowUp"), Some(0x26));
        assert_eq!(code_to_vk("CapsLock"), Some(0x14));
    }

    #[test]
    fn code_to_vk_invalid_returns_none() {
        assert_eq!(code_to_vk("Bogus"), None);
        assert_eq!(code_to_vk(""), None);
        assert_eq!(code_to_vk("KeyAA"), None);
        assert_eq!(code_to_vk("Digit10"), None);
        assert_eq!(code_to_vk("F13"), None);
        assert_eq!(code_to_vk("F0"), None);
    }

    // ----- Stage 2-C-2 parse_modifier_side テスト (左右区別) -----

    #[test]
    fn modifier_side_generic_is_either() {
        assert_eq!(
            parse_modifier_side("Control"),
            Some((ModifierKind::Control, ModifierSide::Either))
        );
        assert_eq!(
            parse_modifier_side("ctrl"),
            Some((ModifierKind::Control, ModifierSide::Either))
        );
        assert_eq!(
            parse_modifier_side("Shift"),
            Some((ModifierKind::Shift, ModifierSide::Either))
        );
    }

    #[test]
    fn modifier_side_left_and_right() {
        assert_eq!(
            parse_modifier_side("ControlLeft"),
            Some((ModifierKind::Control, ModifierSide::Left))
        );
        assert_eq!(
            parse_modifier_side("ControlRight"),
            Some((ModifierKind::Control, ModifierSide::Right))
        );
        assert_eq!(
            parse_modifier_side("AltRight"),
            Some((ModifierKind::Alt, ModifierSide::Right))
        );
        assert_eq!(
            parse_modifier_side("ShiftLeft"),
            Some((ModifierKind::Shift, ModifierSide::Left))
        );
        assert_eq!(
            parse_modifier_side("MetaRight"),
            Some((ModifierKind::Meta, ModifierSide::Right))
        );
    }

    #[test]
    fn modifier_side_invalid_is_none() {
        assert_eq!(parse_modifier_side("Fn"), None);
        assert_eq!(parse_modifier_side(""), None);
        assert_eq!(parse_modifier_side("BogusModifier"), None);
    }

    #[test]
    fn parse_modifier_accepts_left_right_variants() {
        assert_eq!(parse_modifier("ControlLeft").unwrap(), Modifiers::CONTROL);
        assert_eq!(parse_modifier("ControlRight").unwrap(), Modifiers::CONTROL);
        assert_eq!(parse_modifier("AltRight").unwrap(), Modifiers::ALT);
        assert_eq!(parse_modifier("ShiftLeft").unwrap(), Modifiers::SHIFT);
        assert_eq!(parse_modifier("MetaRight").unwrap(), Modifiers::SUPER);
    }

    #[test]
    fn to_shortcut_modifier_order_independent() {
        let a = HotkeyConfig {
            code: "KeyA".to_string(),
            modifiers: vec!["Control".to_string(), "Shift".to_string()],
            mode: HotkeyMode::Toggle,
        };
        let b = HotkeyConfig {
            code: "KeyA".to_string(),
            modifiers: vec!["Shift".to_string(), "Control".to_string()],
            mode: HotkeyMode::Toggle,
        };
        assert_eq!(a.to_shortcut().unwrap().mods, b.to_shortcut().unwrap().mods);
    }
}
