// hotkey_conflict.rs — v0.1.10 機能 D: ホットキー競合検出
//
// SSOT: 内部設計メモ
//
// 目的:
//   起動時 (setup hook) と設定変更時 (set_hotkey_config) の両方で
//   ユーザーが指定したホットキーが他プロセス / OS によって既に
//   使用されていないかを試験登録経路で検査する。
//
// 検査手法:
//   tauri-plugin-global-shortcut の register / unregister API を使う。
//     1. probe_conflict が register(shortcut) を試みる
//     2. Ok なら未衝突として即 unregister(shortcut) を呼び戻す
//        (= アプリ動作中は global-shortcut 側未登録状態を維持)
//     3. Err なら既に他で登録されている = 衝突 と判定
//
// 既存 WH_KEYBOARD_LL との関係:
//   global-shortcut は「検査専用・即解除」のみで使う。
//   実運用のホットキー処理は引き続き keyboard_hook::KeyboardHook (WH_KEYBOARD_LL)
//   が担当する。WH_KEYBOARD_LL は二重登録に該当しないため衝突検査経路と共存可能。
//
// 永続化:
//   ConflictState は config_db::settings_kv の HOTKEY_CONFLICT_STATE_KEY に
//   JSON 形式で保存する。次回起動時の UI 初期表示に使う。

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::hotkey::HotkeyConfig;

/// 競合状態。
///
/// - `None`: 未衝突 (赤バッジ非表示)
/// - `Conflict`: 衝突中 (赤バッジ + 起動時ダイアログ)
///   - `code`: 衝突したキー (W3C Code 名)
///   - `modifiers`: 衝突した修飾キー
///   - `detected_at`: 検出 UNIX time (秒)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub enum ConflictState {
    #[default]
    None,
    Conflict {
        code: String,
        modifiers: Vec<String>,
        detected_at: i64,
    },
}

impl ConflictState {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// HotkeyConfig から衝突状態を組み立てるヘルパー。
    pub fn from_conflict(cfg: &HotkeyConfig, detected_at: i64) -> Self {
        Self::Conflict {
            code: cfg.code.clone(),
            modifiers: cfg.modifiers.clone(),
            detected_at,
        }
    }
}

/// 修飾キー単独ホットキー (例: `ControlRight` / `ShiftLeft` / `AltLeft`) か判定する。
///
/// HotkeyConfig::to_shortcut() の parse_code は IME キー / 英数 / ファンクション のみ
/// 受け付けるため、修飾キー単独パターンは global-shortcut 側では検査できない。
/// この場合は WH_KEYBOARD_LL 専用ホットキーとして扱い、probe_conflict は常に Ok を返す。
///
/// L138 (修飾キー単独ホットキーは WH_KEYBOARD_LL で 3 段特別処理) の前提を保つ。
fn is_modifier_only_hotkey(cfg: &HotkeyConfig) -> bool {
    if !cfg.modifiers.is_empty() {
        return false;
    }
    let lower = cfg.code.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        // 左右指定 (従来)
        "controlleft"
            | "controlright"
            | "shiftleft"
            | "shiftright"
            | "altleft"
            | "altright"
            | "metaleft"
            | "metaright"
            // mac/hotkey-cmd-either-side (2026-06-15): 左右どちらでもマッチする総称コード。
            // 修飾キー単独として keyboard_hook (CGEventTap / WH_KEYBOARD_LL) で扱うため、
            // global-shortcut の register は不要 (むしろ Cmd 単体は register に失敗するため
            // 競合誤検出になる)。
            | "control"
            | "ctrl"
            | "shift"
            | "alt"
            | "option"
            | "meta"
            | "cmd"
            | "command"
            | "win"
            | "windows"
            | "super"
    )
}

/// IME 系キー (無変換 / 変換 / カナ / Lang1-2 / Hiragana / Katakana) か判定する。
///
/// Windows `RegisterHotKey` API は IME 系仮想キー (VK_NONCONVERT / VK_CONVERT 等) を
/// 正式サポートしていないため、`tauri-plugin-global-shortcut` の `register` は
/// 内部で `RegisterHotKey` を呼ぶ過程で常に Err を返す。
/// 結果として probe_conflict が false positive (実際は未衝突なのに「衝突中」判定) を
/// 出してしまい、default ホットキー `Ctrl+Shift+NonConvert` でも誤検知する。
///
/// false positive を防ぐため、IME 系キーは probe をスキップして常に Ok を返却する。
/// 既存 WH_KEYBOARD_LL は low-level keyboard hook で `RegisterHotKey` を使わないため
/// IME 系キーも問題なく捕捉可能 (L138 修飾キー単独と同列扱い)。
///
/// 対象キー (hotkey::parse_code line 87-94 で実サポートされる IME 系のみ):
///   - NonConvert (無変換)
///   - Convert (変換)
///   - KanaMode (カナ)
///   - Lang1 (Hangul / 英数)
///   - Lang2 (Hanja / かな)
///   - Hiragana (ひらがな)
///   - Katakana (カタカナ)
fn is_ime_key(cfg: &HotkeyConfig) -> bool {
    let lower = cfg.code.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "nonconvert"
            | "convert"
            | "kanamode"
            | "lang1"
            | "lang2"
            | "hiragana"
            | "katakana"
    )
}

/// 試験登録 → 即解除で衝突検査する。
///
/// 戻り値:
///   - Ok(()): 未衝突 (register / unregister 共に成功) または検査スキップ対象
///     (修飾キー単独ホットキー = WH_KEYBOARD_LL 専用パターン)
///   - Err(String): 衝突または変換失敗
///     - "invalid hotkey config: ..." HotkeyConfig → Shortcut 変換失敗
///     - "conflict: ..." register 失敗 (他プロセス占有)
///
/// 注意:
///   - register に成功した場合は必ず unregister する (リソース漏洩防止)。
///   - unregister 自体が失敗する場合は警告ログのみ・呼び出し元には Ok を返す
///     (検査目的は達成済み)。
///   - 修飾キー単独ホットキーは global-shortcut では試験できないため未衝突扱いする。
pub fn probe_conflict(app: &AppHandle, cfg: &HotkeyConfig) -> Result<(), String> {
    // 修飾キー単独ホットキー (ControlRight 等) は global-shortcut の試験対象外。
    // 既存 WH_KEYBOARD_LL 経路でのみ動作する設計のため、衝突検査もスキップする。
    if is_modifier_only_hotkey(cfg) {
        log::debug!(
            "[hotkey-conflict] skip probe: modifier-only hotkey code={} (handled by WH_KEYBOARD_LL only)",
            cfg.code
        );
        return Ok(());
    }

    // IME 系キー (NonConvert / Convert / KanaMode 等) は Windows RegisterHotKey API が
    // 正式サポートしておらず、global-shortcut の register が常に Err を返すため
    // false positive (実際は未衝突なのに「衝突中」判定) を引き起こす。
    // WH_KEYBOARD_LL は IME 系キーを問題なく捕捉できるため probe はスキップする。
    if is_ime_key(cfg) {
        log::debug!(
            "[hotkey-conflict] skip probe: IME key code={} (not supported by Windows RegisterHotKey, handled by WH_KEYBOARD_LL)",
            cfg.code
        );
        return Ok(());
    }

    let shortcut = cfg
        .to_shortcut()
        .map_err(|e| format!("invalid hotkey config: {}", e))?;

    let gs = app.global_shortcut();

    // 既に同 shortcut が global-shortcut 側に登録されている場合の冪等性確保:
    // register が「すでに登録されている」エラーで失敗する可能性を排除するため
    // 念のため先に unregister を試みる (失敗は無視・登録されていないだけの可能性)。
    let _ = gs.unregister(shortcut);

    match gs.register(shortcut) {
        Ok(()) => {
            // 成功 = 未衝突。即解除でアプリ動作中の global-shortcut 未登録状態を維持。
            if let Err(e) = gs.unregister(shortcut) {
                log::warn!(
                    "[hotkey-conflict] probe unregister failed (non-fatal): {}",
                    e
                );
            }
            Ok(())
        }
        Err(e) => {
            // 失敗 = 衝突 (or プラグイン初期化失敗等)。
            log::info!(
                "[hotkey-conflict] probe register failed (conflict suspected): {}",
                e
            );
            Err(format!("conflict: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{HotkeyConfig, HotkeyMode};

    fn sample_config() -> HotkeyConfig {
        HotkeyConfig {
            code: "NonConvert".to_string(),
            modifiers: vec!["Control".to_string(), "Shift".to_string()],
            mode: HotkeyMode::Toggle,
        }
    }

    #[test]
    fn conflict_state_default_is_none() {
        assert_eq!(ConflictState::default(), ConflictState::None);
        assert!(!ConflictState::default().is_conflict());
    }

    #[test]
    fn conflict_state_from_conflict_helper() {
        let cfg = sample_config();
        let s = ConflictState::from_conflict(&cfg, 1717000000);
        assert!(s.is_conflict());
        match s {
            ConflictState::Conflict {
                code,
                modifiers,
                detected_at,
            } => {
                assert_eq!(code, "NonConvert");
                assert_eq!(modifiers, vec!["Control".to_string(), "Shift".to_string()]);
                assert_eq!(detected_at, 1717000000);
            }
            _ => panic!("expected Conflict variant"),
        }
    }

    #[test]
    fn conflict_state_json_roundtrip_none() {
        let s = ConflictState::None;
        let j = s.to_json().unwrap();
        // tag-style serialization
        assert!(j.contains("\"kind\""));
        assert!(j.contains("\"None\""));
        let p = ConflictState::from_json(&j).unwrap();
        assert_eq!(s, p);
    }

    #[test]
    fn conflict_state_json_roundtrip_conflict() {
        let s = ConflictState::Conflict {
            code: "KeyM".to_string(),
            modifiers: vec!["Control".to_string(), "Shift".to_string()],
            detected_at: 1717000000,
        };
        let j = s.to_json().unwrap();
        assert!(j.contains("\"Conflict\""));
        assert!(j.contains("\"KeyM\""));
        let p = ConflictState::from_json(&j).unwrap();
        assert_eq!(s, p);
    }

    #[test]
    fn modifier_only_hotkey_is_recognized() {
        // 修飾キー単独 (L138 仕様)
        for code in [
            "ControlLeft",
            "ControlRight",
            "ShiftLeft",
            "ShiftRight",
            "AltLeft",
            "AltRight",
            "MetaLeft",
            "MetaRight",
        ] {
            let cfg = HotkeyConfig {
                code: code.to_string(),
                modifiers: vec![],
                mode: HotkeyMode::Toggle,
            };
            assert!(
                is_modifier_only_hotkey(&cfg),
                "{} should be modifier-only",
                code
            );
        }
    }

    #[test]
    fn modifier_only_check_rejects_combos_and_regular_keys() {
        // 修飾キー単独でない (組合せ or 通常キー)
        let with_mods = HotkeyConfig {
            code: "ControlRight".to_string(),
            modifiers: vec!["Shift".to_string()],
            mode: HotkeyMode::Toggle,
        };
        assert!(!is_modifier_only_hotkey(&with_mods));

        let regular = HotkeyConfig {
            code: "NonConvert".to_string(),
            modifiers: vec!["Control".to_string()],
            mode: HotkeyMode::Toggle,
        };
        assert!(!is_modifier_only_hotkey(&regular));

        let function_key = HotkeyConfig {
            code: "F1".to_string(),
            modifiers: vec![],
            mode: HotkeyMode::Toggle,
        };
        assert!(!is_modifier_only_hotkey(&function_key));
    }

    #[test]
    fn ime_key_is_recognized() {
        // IME 系キー (RegisterHotKey 非対応・probe スキップ対象)
        for code in [
            "NonConvert",
            "Convert",
            "KanaMode",
            "Lang1",
            "Lang2",
            "Hiragana",
            "Katakana",
        ] {
            // 修飾キーの有無にかかわらず IME 判定は code 単独で行う
            let cfg = HotkeyConfig {
                code: code.to_string(),
                modifiers: vec!["Control".to_string(), "Shift".to_string()],
                mode: HotkeyMode::Toggle,
            };
            assert!(is_ime_key(&cfg), "{} should be detected as IME key", code);

            let cfg_bare = HotkeyConfig {
                code: code.to_string(),
                modifiers: vec![],
                mode: HotkeyMode::Toggle,
            };
            assert!(
                is_ime_key(&cfg_bare),
                "{} (bare) should be detected as IME key",
                code
            );
        }
    }

    #[test]
    fn ime_key_check_rejects_regular_keys() {
        // 通常キーは IME 判定で false を返す
        for code in [
            "KeyA",
            "KeyM",
            "F1",
            "F12",
            "Space",
            "Enter",
            "Digit1",
            "ControlLeft",
            "ShiftRight",
        ] {
            let cfg = HotkeyConfig {
                code: code.to_string(),
                modifiers: vec!["Control".to_string()],
                mode: HotkeyMode::Toggle,
            };
            assert!(
                !is_ime_key(&cfg),
                "{} should NOT be detected as IME key",
                code
            );
        }
    }

    #[test]
    fn conflict_state_is_conflict_discriminates_variants() {
        let n = ConflictState::None;
        let c = ConflictState::Conflict {
            code: "F12".to_string(),
            modifiers: vec![],
            detected_at: 0,
        };
        assert!(!n.is_conflict());
        assert!(c.is_conflict());
    }
}
