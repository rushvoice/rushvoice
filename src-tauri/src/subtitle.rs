// subtitle.rs — セミナー字幕モード (feature/seminar-subtitle) 純粋関数群
//
// 用途: 海外講習で講師の発話を現地語+日本語(+英語 opt)の字幕として外部モニターに表示する。
// 設計方針 (meeting.rs と同じ): 全関数を純粋関数として実装 (外部 I/O ゼロ・テスト容易)。
// HTTP/翻訳 API 呼び出しは subtitle_commands.rs 側が担当する。
//
// 確定仕様 (2026-07-04 合意):
//   - 対応言語: 繁体字中国語(台湾)・インドネシア語・マレー語・タイ語・英語・日本語
//   - 入力言語は Whisper 自動判定 (language パラメータを送らない・transcribe.rs 拡張)
//   - 出力は講習開始時に固定: 現地語 + 日本語の 2 段 + 英語 3 段目 (設定 ON/OFF)
//   - 「検出言語 = 表示言語なら原文そのまま・違えば翻訳」の一律ルール
//   - 固有名詞は辞書 replacement を翻訳プロンプトにヒント注入 (L209: 読みベース辞書は
//     他言語入力に効かないため、置換ではなくプロンプトヒントで対応)

use serde::{Deserialize, Serialize};

/// 出力対象言語コード (frontend の選択肢と一致させる)。
pub const SUPPORTED_TARGET_LANGS: [&str; 5] = ["zh-TW", "id", "ms", "th", "en"];

/// 翻訳プロンプトへの固有名詞ヒント上限 (build_meeting_llm_prompt の 30 語方式を踏襲)。
pub const SUBTITLE_GLOSSARY_MAX_WORDS: usize = 30;
/// 固有名詞ヒントブロックの文字数上限。
pub const SUBTITLE_GLOSSARY_MAX_CHARS: usize = 400;

/// 1 発話分の字幕行 (frontend へ emit する表示テキスト)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleRows {
    /// 現地語 (講習開始時に固定した target言語)。
    pub local: String,
    /// 日本語。
    pub ja: String,
    /// 英語 3 段目 (設定 OFF・target=英語 のときは None)。
    pub en: Option<String>,
}

/// Whisper verbose_json (トップレベル) から発話テキストと検出言語を取り出す。
///
/// 字幕モードは 1 チャンク = 1 発話のため segments ではなくトップレベル "text" を使う。
/// "language" は language パラメータ省略時に Whisper が検出した言語
/// (例: "japanese" / "english" 等の英語名。API 実装によっては "ja" 等のコードの場合もある)。
pub fn parse_transcription(verbose_json_body: &str) -> Result<(String, Option<String>), String> {
    let v: serde_json::Value =
        serde_json::from_str(verbose_json_body).map_err(|e| format!("JSON parse: {}", e))?;
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let lang = v
        .get("language")
        .and_then(|l| l.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok((text, lang))
}

/// verbose_json の segments から最小 avg_logprob と最大 no_speech_prob を抽出する (純粋関数)。
///
/// 用途: 遠い声・不明瞭な声 (隣人の会話拾い等) は確信度が下がる特性を使った
/// 品質フィルタの閾値チューニング用データ収集 (2026-07-05 隣席の会話を拾った事案)。
/// segments 欠落・空なら (None, None)。
pub fn parse_confidence(verbose_json_body: &str) -> (Option<f64>, Option<f64>) {
    let v: serde_json::Value = match serde_json::from_str(verbose_json_body) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let segs = match v.get("segments").and_then(|s| s.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return (None, None),
    };
    let mut min_logprob: Option<f64> = None;
    let mut max_no_speech: Option<f64> = None;
    for s in segs {
        if let Some(lp) = s.get("avg_logprob").and_then(|x| x.as_f64()) {
            min_logprob = Some(min_logprob.map_or(lp, |m: f64| m.min(lp)));
        }
        if let Some(ns) = s.get("no_speech_prob").and_then(|x| x.as_f64()) {
            max_no_speech = Some(max_no_speech.map_or(ns, |m: f64| m.max(ns)));
        }
    }
    (min_logprob, max_no_speech)
}

/// 遠い声フィルタの閾値 (Day2 2026-07-06 実測から決定):
/// 近い声の正常発話は min avg_logprob -0.049〜-0.27 (Day1/Day2 計 21 サンプル)、
/// 遠い声/ノイズ由来の幻聴字幕は -0.75。中間の大きなギャップの中で余裕をみて -0.45。
pub const FAR_VOICE_AVG_LOGPROB_THRESHOLD: f64 = -0.45;

/// 遠い声・不明瞭な音声由来の転写かを判定する (純粋関数)。
///
/// min avg_logprob が閾値未満なら破棄対象。no_speech_prob は Groq では常に 0.0 で
/// 判定に使えない (L121 と同根) ため見ない。conf 欠落 (None) は判定不能として通す
/// (誤破棄より誤表示を許容・字幕は消えると気づけない)。
pub fn is_far_voice(min_avg_logprob: Option<f64>) -> bool {
    matches!(min_avg_logprob, Some(lp) if lp < FAR_VOICE_AVG_LOGPROB_THRESHOLD)
}

/// Whisper の検出言語表記 (英語名 or ISO コード) を本モジュールの言語コードに正規化する。
///
/// 戻り値: "ja" / "en" / "zh" / "id" / "ms" / "th" のいずれか。未知言語は None。
/// 注意: Whisper は繁体字/簡体字を区別しないため中国語は "zh" に潰す
/// (表示言語 "zh-TW" とのマッチは `lang_matches` が前方一致で吸収する)。
pub fn normalize_detected_language(raw: &str) -> Option<&'static str> {
    let l = raw.trim().to_lowercase();
    match l.as_str() {
        "ja" | "japanese" => Some("ja"),
        "en" | "english" => Some("en"),
        "zh" | "chinese" | "mandarin" | "zh-tw" | "zh-cn" => Some("zh"),
        "id" | "indonesian" => Some("id"),
        "ms" | "malay" => Some("ms"),
        "th" | "thai" => Some("th"),
        _ => None,
    }
}

/// 検出言語 (正規化済) が出力言語スロットと同一言語か判定する。
///
/// 中国語は検出側が "zh" (繁/簡区別なし) のため、出力側 "zh-TW" と前方一致で照合する。
pub fn lang_matches(detected_code: &str, output_code: &str) -> bool {
    if detected_code.is_empty() || output_code.is_empty() {
        return false;
    }
    detected_code == output_code
        || output_code.to_lowercase().starts_with(&format!("{}-", detected_code.to_lowercase()))
        || detected_code.to_lowercase().starts_with(&format!("{}-", output_code.to_lowercase()))
}

/// LLM 翻訳プロンプト用の言語名 (英語表記・翻訳指示の明確化のため)。
pub fn lang_name_en(code: &str) -> &'static str {
    match code {
        "zh-TW" => "Traditional Chinese (as used in Taiwan)",
        "id" => "Indonesian",
        "ms" => "Malay",
        "th" => "Thai",
        "ja" => "Japanese",
        _ => "English", // "en" および未知コードは英語へフォールバック
    }
}

/// 翻訳 LLM の system プロンプトを構築する (辞書 replacement をヒント注入)。
///
/// 戻り値 `(prompt, truncated_count)`: build_meeting_llm_prompt と同じ契約。
/// truncated_count = 上限超過で捨てた用語数 (呼出側で log 出力・無言の切り捨て禁止 L286)。
///
/// LLM には JSON のみを返させる ("local"/"ja"/"en" キー)。指示は英語
/// (多言語入出力の安定性優先・llama-3.3-70b)。
pub fn build_translation_prompt(
    target_code: &str,
    include_english: bool,
    glossary_replacements: &[String],
) -> (String, usize) {
    // 重複排除 (出現順保持) + 空白除外 (build_meeting_llm_prompt と同方式)。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    for r in glossary_replacements {
        let t = r.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_string()) {
            unique.push(t.to_string());
        }
    }
    let total = unique.len();
    let mut included: Vec<String> = Vec::new();
    let mut block_len = 0usize;
    for w in unique {
        if included.len() >= SUBTITLE_GLOSSARY_MAX_WORDS {
            break;
        }
        let add = w.chars().count() + 2;
        if block_len + add > SUBTITLE_GLOSSARY_MAX_CHARS {
            break;
        }
        block_len += add;
        included.push(w);
    }
    let truncated = total.saturating_sub(included.len());

    let target_name = lang_name_en(target_code);
    let mut p = String::with_capacity(700 + block_len);
    p.push_str(
        "You are a real-time subtitle translator for a beauty-industry (eyelash extension) training seminar.\n\
         The user message is one utterance transcribed from speech. It may be in any language.\n\
         Translate it and return ONLY a single JSON object. No code fences, no commentary.\n\
         Required keys:\n",
    );
    p.push_str(&format!("  \"local\": the utterance translated into {}.\n", target_name));
    p.push_str("  \"ja\": the utterance translated into Japanese.\n");
    if include_english && target_code != "en" {
        p.push_str("  \"en\": the utterance translated into English.\n");
    }
    p.push_str(
        "Rules:\n\
         - Preserve the meaning exactly. Do not summarize, omit, or add anything.\n\
         - Use a natural spoken style suitable for on-screen subtitles.\n\
         - If the utterance is already in a requested language, output it for that key with obvious \
           speech-recognition errors corrected. Fix ONLY misheard words (e.g. ブルーの乾燥時間 → グルーの乾燥時間); \
           never rephrase, reorder, or polish the rest.\n\
         - The transcript comes from automatic speech recognition and may contain misrecognized words; \
           infer the intended words from context and translate the intended meaning \
           (e.g. Japanese エプステ→エクステ, 手術後→施術後).\n\
         - If a garbled word is a sound-alike near-match of a domain term or a fixed term listed below, \
           restore it to the exact term: まつ毛屈クステ / まつげクスデ / まつげくすで → まつげエクステ, \
           LEDまつげく / LED末役 / LEDマツテク → LEDマツエク, フルー / ブルー (in a glue context) → グルー.\n\
         - Work in two steps internally: (1) derive the corrected source sentence with all garbled words \
           restored, then (2) produce EVERY output key (local / ja / en) from that same corrected sentence. \
           Never translate the uncorrected text for any key, and never leave Japanese kana, kanji, or \
           long-vowel marks (ー) inside non-Japanese output.\n\
         - Domain: professional EYELASH EXTENSION training. Interpret ambiguous beauty terms in this domain: \
           グルー = eyelash extension adhesive (NOT nail glue), 施術 = lash application procedure (NOT surgery), \
           リペア = lash refill. Refer to lashes (bulu mata / 睫毛), not eyes, when the source says まつげ.\n\
         - For Traditional Chinese, use Taiwan beauty-industry terms: まつげエクステ = 接睫毛, \
           施術 = 操作/服務, 持ち = 持久度 (e.g. 讓接睫毛更持久).\n\
         - For Thai, use beauty-industry terms: グルー = กาว, まつげエクステ = การต่อขนตา, \
           施術 = การทำ, 持ち/持続期間 = ความคงทน. Thai output must be pure Thai script \
           (Thai characters and digits; established brand words like LED are allowed); NEVER mix \
           CJK characters, kana, or words from other languages into Thai output.\n",
    );
    if !included.is_empty() {
        p.push_str("Proper nouns / fixed terms (keep their official spelling untranslated): ");
        p.push_str(&included.join(", "));
        p.push('\n');
    }
    (p, truncated)
}

/// LLM 応答 (JSON 期待) から local/ja/en を取り出す。
///
/// コードフェンスや前後の余計なテキストに耐性を持たせるため、
/// 最初の '{' から最後の '}' までを切り出してからパースする。
/// パース不能・全キー欠落は None (呼出側で原文フォールバック)。
pub fn parse_translation_json(body: &str) -> Option<(String, String, Option<String>)> {
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&body[start..=end]).ok()?;
    let get = |key: &str| -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let local = get("local");
    let ja = get("ja");
    let en = get("en");
    if local.is_none() && ja.is_none() && en.is_none() {
        return None;
    }
    Some((local.unwrap_or_default(), ja.unwrap_or_default(), en))
}

/// 表示行を確定する。
///
/// ルール (2026-07-05 要望で改訂):
/// - 検出言語 = 表示言語のスロットは「LLM が聞き間違いだけを補正した同言語テキスト」を優先し、
///   LLM 失敗時のみ認識原文にフォールバックする (旧: 常に原文そのまま。
///   「グルー→ブルー」等の誤認識が日本語段にそのまま出る問題への対策)。
/// - 検出言語と異なるスロットは LLM 翻訳。
/// - LLM 失敗かつ主要 2 段が空なら原文を local スロットに表示 (無表示回避)。
pub fn decide_rows(
    detected_code: Option<&str>,
    target_code: &str,
    include_english: bool,
    original: &str,
    translated: Option<&(String, String, Option<String>)>,
) -> SubtitleRows {
    let det = detected_code.unwrap_or("");
    let (t_local, t_ja, t_en) = match translated {
        Some((l, j, e)) => (l.clone(), j.clone(), e.clone()),
        None => (String::new(), String::new(), None),
    };

    // 同言語スロット: LLM 補正版があればそれ・なければ原文 (フォールバック)
    let same_lang_or = |llm: String| -> String {
        if llm.is_empty() {
            original.to_string()
        } else {
            llm
        }
    };

    let local = if lang_matches(det, target_code) {
        same_lang_or(t_local)
    } else {
        t_local
    };
    let ja = if det == "ja" { same_lang_or(t_ja) } else { t_ja };
    let en = if include_english && target_code != "en" {
        if det == "en" {
            Some(t_en.filter(|s| !s.is_empty()).unwrap_or_else(|| original.to_string()))
        } else {
            t_en
        }
    } else {
        None
    };

    let mut rows = SubtitleRows { local, ja, en };
    // フォールバック: 主要 2 段が両方空 (LLM 失敗 + 検出不一致) なら原文を現地語段に出す。
    if rows.local.is_empty() && rows.ja.is_empty() {
        rows.local = original.to_string();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- parse_transcription ----------------

    #[test]
    fn parse_transcription_extracts_text_and_language() {
        let body = r#"{"text": " こんにちは ", "language": "japanese", "segments": []}"#;
        let (text, lang) = parse_transcription(body).unwrap();
        assert_eq!(text, "こんにちは");
        assert_eq!(lang.as_deref(), Some("japanese"));
    }

    #[test]
    fn parse_transcription_missing_language_is_none() {
        let body = r#"{"text": "hello"}"#;
        let (text, lang) = parse_transcription(body).unwrap();
        assert_eq!(text, "hello");
        assert!(lang.is_none());
    }

    #[test]
    fn parse_transcription_empty_text_ok() {
        let body = r#"{"text": "   ", "language": "english"}"#;
        let (text, _) = parse_transcription(body).unwrap();
        assert!(text.is_empty());
    }

    #[test]
    fn parse_transcription_invalid_json_is_err() {
        assert!(parse_transcription("not json").is_err());
    }


    // ---------------- parse_confidence ----------------

    #[test]
    fn parse_confidence_extracts_min_logprob_and_max_no_speech() {
        let body = r#"{"text":"x","segments":[
            {"avg_logprob":-0.1,"no_speech_prob":0.0},
            {"avg_logprob":-0.8,"no_speech_prob":0.3}
        ]}"#;
        let (lp, ns) = parse_confidence(body);
        assert_eq!(lp, Some(-0.8));
        assert_eq!(ns, Some(0.3));
    }

    #[test]
    fn parse_confidence_missing_segments_is_none() {
        assert_eq!(parse_confidence(r#"{"text":"x"}"#), (None, None));
        assert_eq!(parse_confidence("garbage"), (None, None));
        assert_eq!(parse_confidence(r#"{"segments":[]}"#), (None, None));
    }

    // ---------------- is_far_voice ----------------

    #[test]
    fn far_voice_below_threshold_is_filtered() {
        assert!(is_far_voice(Some(-0.75)), "Day2 実測の遠い声サンプル");
        assert!(!is_far_voice(Some(-0.27)), "Day1 正常発話の最悪値");
        assert!(!is_far_voice(Some(-0.049)), "Day2 正常発話の最良値");
        assert!(!is_far_voice(None), "conf 欠落は判定不能として通す");
        assert!(
            !is_far_voice(Some(FAR_VOICE_AVG_LOGPROB_THRESHOLD)),
            "境界値ちょうどは通す (未満のみ破棄)"
        );
    }

    #[test]
    fn prompt_includes_thai_terms_and_purity_rule() {
        let (p, _) = build_translation_prompt("th", false, &[]);
        assert!(p.contains("กาว"), "タイ語用語対訳 (グルー=กาว)");
        assert!(p.contains("pure Thai script"), "タイ文字純度制約");
    }

    #[test]
    fn prompt_includes_garbled_term_restore_rule() {
        let (p, _) = build_translation_prompt("id", false, &[]);
        assert!(p.contains("まつげエクステ"), "崩れ語→用語復元ルール");
    }

    // ---------------- normalize_detected_language ----------------

    #[test]
    fn normalize_handles_names_and_codes() {
        assert_eq!(normalize_detected_language("japanese"), Some("ja"));
        assert_eq!(normalize_detected_language("ja"), Some("ja"));
        assert_eq!(normalize_detected_language("Chinese"), Some("zh"));
        assert_eq!(normalize_detected_language("indonesian"), Some("id"));
        assert_eq!(normalize_detected_language("MALAY"), Some("ms"));
        assert_eq!(normalize_detected_language("thai"), Some("th"));
        assert_eq!(normalize_detected_language("english"), Some("en"));
        assert_eq!(normalize_detected_language("swahili"), None);
    }

    // ---------------- lang_matches ----------------

    #[test]
    fn lang_matches_exact_and_zh_variant() {
        assert!(lang_matches("ja", "ja"));
        assert!(lang_matches("zh", "zh-TW"));
        assert!(!lang_matches("ja", "en"));
        assert!(!lang_matches("", "en"));
        assert!(!lang_matches("id", "ms"));
    }

    // ---------------- build_translation_prompt ----------------

    #[test]
    fn prompt_includes_target_language_and_ja() {
        let (p, truncated) = build_translation_prompt("zh-TW", false, &[]);
        assert!(p.contains("Traditional Chinese"));
        assert!(p.contains("\"ja\""));
        assert!(!p.contains("\"en\""), "英語 OFF なら en キーを要求しない");
        assert_eq!(truncated, 0);
    }

    #[test]
    fn prompt_includes_en_when_enabled() {
        let (p, _) = build_translation_prompt("id", true, &[]);
        assert!(p.contains("Indonesian"));
        assert!(p.contains("\"en\""));
    }

    #[test]
    fn prompt_omits_en_key_when_target_is_english() {
        // target=en の場合 local 段が英語なので 3 段目は要求しない
        let (p, _) = build_translation_prompt("en", true, &[]);
        assert!(!p.contains("\"en\": the utterance"));
    }

    #[test]
    fn prompt_injects_glossary_and_dedups() {
        let words = vec![
            "LashRush".to_string(),
            "LashRush".to_string(),
            "ハリウッドアイラッシュ".to_string(),
            "  ".to_string(),
        ];
        let (p, truncated) = build_translation_prompt("th", false, &words);
        assert!(p.contains("LashRush"));
        assert!(p.contains("ハリウッドアイラッシュ"));
        assert_eq!(p.matches("LashRush").count(), 1, "重複排除");
        assert_eq!(truncated, 0);
    }

    #[test]
    fn prompt_truncates_over_max_words() {
        let words: Vec<String> = (0..40).map(|i| format!("word{}", i)).collect();
        let (p, truncated) = build_translation_prompt("ms", false, &words);
        assert_eq!(truncated, 40 - SUBTITLE_GLOSSARY_MAX_WORDS);
        assert!(p.contains("word0"));
        assert!(!p.contains("word39"));
    }

    // ---------------- parse_translation_json ----------------

    #[test]
    fn parse_translation_plain_json() {
        let body = r#"{"local": "你好", "ja": "こんにちは", "en": "Hello"}"#;
        let (l, j, e) = parse_translation_json(body).unwrap();
        assert_eq!(l, "你好");
        assert_eq!(j, "こんにちは");
        assert_eq!(e.as_deref(), Some("Hello"));
    }

    #[test]
    fn parse_translation_tolerates_code_fences() {
        let body = "```json\n{\"local\": \"สวัสดี\", \"ja\": \"こんにちは\"}\n```";
        let (l, j, e) = parse_translation_json(body).unwrap();
        assert_eq!(l, "สวัสดี");
        assert_eq!(j, "こんにちは");
        assert!(e.is_none());
    }

    #[test]
    fn parse_translation_garbage_is_none() {
        assert!(parse_translation_json("sorry I cannot").is_none());
        assert!(parse_translation_json("{}").is_none());
        assert!(parse_translation_json("{\"other\": 1}").is_none());
    }

    // ---------------- decide_rows ----------------

    fn tr(l: &str, j: &str, e: Option<&str>) -> (String, String, Option<String>) {
        (l.to_string(), j.to_string(), e.map(|s| s.to_string()))
    }

    /// 日本語で話した → 日本語段は LLM 誤認識補正版を優先・現地語段は翻訳。
    /// (2026-07-05 要望: 「ブルーの乾燥時間」等の誤認識をそのまま出さない)
    #[test]
    fn decide_rows_japanese_input_prefers_corrected_ja() {
        let t = tr("膠水的乾燥時間", "グルーの乾燥時間は湿度によって変わります", Some("Hello"));
        let rows = decide_rows(
            Some("ja"),
            "zh-TW",
            true,
            "ブルーの乾燥時間は湿度によって変わります",
            Some(&t),
        );
        assert_eq!(rows.local, "膠水的乾燥時間");
        assert_eq!(
            rows.ja, "グルーの乾燥時間は湿度によって変わります",
            "日本語段は LLM 補正版を採用"
        );
        assert_eq!(rows.en.as_deref(), Some("Hello"));
    }

    /// 日本語入力 + LLM の ja が空 → 原文にフォールバック。
    #[test]
    fn decide_rows_japanese_input_falls_back_when_ja_empty() {
        let t = tr("你好", "", None);
        let rows = decide_rows(Some("ja"), "zh-TW", false, "こんにちは", Some(&t));
        assert_eq!(rows.ja, "こんにちは", "LLM 補正が無ければ原文");
    }

    /// 現地語 (中国語) で話した → 現地語段は LLM 補正版優先・日本語段は翻訳 (受講者質問の復唱等)。
    #[test]
    fn decide_rows_local_language_input() {
        let t = tr("你好(補正)", "こんにちは", None);
        let rows = decide_rows(Some("zh"), "zh-TW", false, "你好", Some(&t));
        assert_eq!(rows.local, "你好(補正)", "zh 検出は zh-TW 段に補正版優先");
        assert_eq!(rows.ja, "こんにちは");
        assert!(rows.en.is_none());
    }

    /// 英語 3 段目 OFF なら en は常に None。
    #[test]
    fn decide_rows_english_off() {
        let t = tr("Halo", "こんにちは", Some("Hello"));
        let rows = decide_rows(Some("ja"), "id", false, "こんにちは", Some(&t));
        assert!(rows.en.is_none());
    }

    /// target=en のとき 3 段目は不要 (local 段が英語)。
    #[test]
    fn decide_rows_target_en_no_third_row() {
        let t = tr("Hello", "こんにちは", None);
        let rows = decide_rows(Some("ja"), "en", true, "こんにちは", Some(&t));
        assert_eq!(rows.local, "Hello");
        assert!(rows.en.is_none());
    }

    /// LLM 失敗 + 検出不一致 → 原文を local にフォールバック表示。
    #[test]
    fn decide_rows_llm_failure_falls_back_to_original() {
        let rows = decide_rows(Some("th"), "zh-TW", false, "สวัสดี", None);
        assert_eq!(rows.local, "สวัสดี");
        assert_eq!(rows.ja, "");
    }

    /// 検出言語不明 + LLM 成功 → 翻訳結果をそのまま使う。
    #[test]
    fn decide_rows_unknown_detected_uses_translation() {
        let t = tr("你好", "こんにちは", None);
        let rows = decide_rows(None, "zh-TW", false, "??", Some(&t));
        assert_eq!(rows.local, "你好");
        assert_eq!(rows.ja, "こんにちは");
    }
}
