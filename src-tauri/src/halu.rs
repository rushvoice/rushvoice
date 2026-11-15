// Phase 1 Day 4-C: ハルシネーション 3 段 OR 合成フィルタ
// 仕様: 内部設計メモ + lessons_rushvoice.md L088 / L078
//
// 3 段 OR 合成 (L088 厳守・AND 禁止):
//   Tier 1: VAD が音声無しと判定 → Drop
//   Tier 2: no_speech_prob > 閾値 (既定 0.6) → Drop
//   Tier 3: 既知ハルシ句完全一致 AND min_logprob < 閾値 (既定 -2.5) → Drop
//           (※ Tier 3 内の AND は仕様。Tier 間は OR)
//   いずれにも該当しなければ Pass
//
// v0.1.3 patch2 (2026-05-24): 短発話 logprob 判定スキップ (B)
//   - judge() に duration_ms 引数追加 (Option<i64> / None=未取得)
//   - duration_ms.unwrap_or(i64::MAX) < SHORT_UTTERANCE_THRESHOLD_MS のとき
//     Tier 2 (no_speech_prob) と Tier 3 (完全一致 + 低 logprob AND) はスキップ。
//     Tier 1 (VAD) / Tier 1.5 (ASCII 短文) / Tier 3.5 (既知 substring) は維持。
//   - 真因: 短発話 (例: 「ミュージカル」「松井九」) で logprob 自然下降 → false-positive。
//   - 設計: 短発話は「明確な既知ハルシ (substring / 完全一致 + 低 logprob) でない限り通す」。
//
// L078 準拠: 既知ハルシ句は config 経由で渡し、LLM プロンプトには絶対に列挙しない。
//            本ファイルにも具体語をハードコードしない (空 Vec 既定)。
//
// VAD 選定 (Phase 1):
//   - Trait `Vad` で抽象化。本実装は EnergyVad (RMS 閾値・依存ゼロ)。
//   - Silero / webrtc-vad は Phase 2 で `Box<dyn Vad>` でスワップ可能。

use std::fmt;
// v0.1.12.1: tracing::info! は本アプリのログ基盤 (tauri-plugin-log = log クレート系) に
// 受け皿が無く出力されなかった (F1 実機バグ)。log::info! に統一して本番ログへ確実に出す。
use log::info;

// ---------------------------------------------------------------------------
// VAD trait & 既定実装
// ---------------------------------------------------------------------------

pub trait Vad: Send + Sync {
    /// 渡された音声サンプル (16kHz mono f32 推奨) に発話成分があるか判定。
    /// true = 発話あり / false = 無音/雑音のみ。
    fn has_speech(&self, samples: &[f32]) -> bool;
}

/// RMS が閾値以上なら発話ありとみなす簡易 VAD。
///
/// 既定 `rms_threshold = 0.002`:
///   - Phase 1 Day 4-D-2 で実測校正 (利用者デスクトップ PC マイク・2026-05-21)
///   - 発話時 peak ≈ 0.07 / rms ≈ 0.006 → 0.002 で安定通過
///   - ambient 雑音 rms ≈ 0.0005-0.0015 → 通常は弾ける
///   - 完全無音 (D-1) rms = 0 → 弾く
/// マイクゲイン・距離・環境ノイズで変動するため運用時は要再校正。
pub struct EnergyVad {
    rms_threshold: f32,
}

impl EnergyVad {
    pub fn new(rms_threshold: f32) -> Self {
        Self { rms_threshold }
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self {
            rms_threshold: 0.002,
        }
    }
}

impl Vad for EnergyVad {
    fn has_speech(&self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return false;
        }
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        let rms = (sum_sq / samples.len() as f32).sqrt();
        rms >= self.rms_threshold
    }
}

// ---------------------------------------------------------------------------
// HaluConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HaluConfig {
    /// Tier 2 閾値。Groq verbose_json の `no_speech_prob` がこれを超えたら Drop。
    pub no_speech_prob_threshold: f32,
    /// Tier 3 閾値。`min_logprob` (or `avg_logprob`) がこれ未満ならハルシ候補。
    pub min_logprob_threshold: f32,
    /// Tier 3 用 既知ハルシ句リスト (完全一致判定)。
    /// L078: 本フィールドは外部 config / SQLite 等から読み込んで渡す。
    /// 本コードに句をハードコードしてはいけない (空 Vec 既定)。
    pub known_hallucinations: Vec<String>,
    /// Tier 3.2 用 既知ハルシ句リスト (完全一致・**logprob 条件なし**)。
    /// (無料版 RushVoice Air v1.1.5 (2026-08-15) からの移植・2026-08-16)
    ///
    /// 追加の経緯: 「字幕作成者」は `known_hallucinations` (Tier 3) に登録済みだったが、
    /// 開発機実機 id=742 が `avg_logprob = -0.303` (本番閾値 -0.5 を上回る) で
    /// 素通りした。Whisper は YouTube 字幕クレジット由来のハルシを**高 confidence**で
    /// 返すため、logprob を AND 条件にすると原理的に捕まらない。
    ///
    /// 🔴 登録規約: **発話全体がその句だけになることが実務上あり得ない**句に限る。
    /// 完全一致 (normalize 後) のみを見るため、「字幕作成者は誰ですか」等の正規発話は
    /// 一致せず通る。逆に「ありがとうございました」のような**単独で言い得る**句は
    /// ここへ入れてはいけない (従来どおり logprob AND の Tier 3 に置く)。
    pub known_hallucinations_always: Vec<String>,
    /// Tier 3.5 (v0.1.3) 部分一致パターン (contains)。
    /// prompt 漏洩ハルシ等「大きなまとまり」を素早く弾くための辞書。
    /// 短い単語 (「読点」「日本語で」単独等) は誤検知防止のため入れない。
    pub known_hallucination_substrings: Vec<String>,
    /// Tier 1.5 (HL-03) 閾値: 文字数 (空白除外後 char 数) がこれ以下のとき判定対象。
    pub ascii_short_max_len: usize,
    /// Tier 1.5 (HL-03) 閾値: ASCII 文字比率がこれ以上なら英語ハルシ残骸として破棄。
    pub ascii_short_min_ratio: f32,
}

impl Default for HaluConfig {
    fn default() -> Self {
        Self {
            no_speech_prob_threshold: 0.6,
            min_logprob_threshold: -2.5,
            known_hallucinations: Vec::new(),
            known_hallucinations_always: Vec::new(),
            known_hallucination_substrings: Vec::new(),
            ascii_short_max_len: 60,
            ascii_short_min_ratio: 0.70,
        }
    }
}

// ---------------------------------------------------------------------------
// HaluVerdict
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HaluVerdict {
    /// 文字起こし結果を採用。
    Pass,
    /// ハルシと判定して破棄。
    Drop(HaluReason),
}

#[derive(Debug)]
pub enum HaluReason {
    /// Tier 1: VAD が音声無しと判定。
    NoSpeechVad,
    /// Tier 1.5 (HL-03): ASCII 主体の短文 → 英語ハルシ残骸として破棄。
    AsciiShort { len: usize, ascii_ratio: f32 },
    /// Tier 2: no_speech_prob 超過。
    NoSpeechProb { value: f32, threshold: f32 },
    /// Tier 3: 既知ハルシ句 AND 低 logprob。
    KnownPhraseLowLogprob {
        phrase: String,
        min_logprob: f32,
        threshold: f32,
    },
    /// Tier 3.2 (Air v1.1.5 移植): 既知ハルシ句と完全一致 (logprob 条件なし)。
    KnownPhraseAlways { phrase: String },
    /// Tier 3.5 (v0.1.3): 既知ハルシ部分文字列にマッチ (logprob 条件なし・大きなまとまり判定)。
    KnownSubstring {
        substring: String,
    },
}

impl fmt::Display for HaluReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HaluReason::NoSpeechVad => write!(f, "tier1: VAD says no speech"),
            HaluReason::AsciiShort { len, ascii_ratio } => {
                write!(f, "tier1.5: ascii short len={} ratio={:.2}", len, ascii_ratio)
            }
            HaluReason::NoSpeechProb { value, threshold } => {
                write!(f, "tier2: no_speech_prob {:.3} > {:.3}", value, threshold)
            }
            HaluReason::KnownPhraseLowLogprob {
                phrase,
                min_logprob,
                threshold,
            } => write!(
                f,
                "tier3: known phrase \"{}\" AND min_logprob {:.3} < {:.3}",
                phrase, min_logprob, threshold
            ),
            HaluReason::KnownPhraseAlways { phrase } => {
                write!(f, "tier3.2: known phrase (always) \"{}\"", phrase)
            }
            HaluReason::KnownSubstring { substring } => {
                write!(f, "tier3.5: known substring \"{}\"", substring)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HaluFilter
// ---------------------------------------------------------------------------

/// v0.1.3 patch2: 短発話判定の上限 (ms)。これ未満は Tier 2 / Tier 3 をスキップ。
/// 4 秒 = 検証時の通常の短発話 (2-3 秒) を確実にカバー。
///   - patch2 初版: 2000ms (体感「ひと単語〜短いフレーズ」境界)
///   - patch5 (2026-05-24): 2000 → 4000 拡大。
///     真因: id=173「マツエク」(duration=2500ms / logprob=-0.55) が flag=1 で貼付抑止された。
///     利用者明示「誤変換よりも貼り付けられない方が問題」→ 短発話判定範囲を利用者発話幅に合わせる。
///     短すぎると logprob 自然下降で false-positive 連発・長すぎても Tier 3.5 substring 防御は維持。
pub const SHORT_UTTERANCE_THRESHOLD_MS: i64 = 4000;

pub struct HaluFilter<V: Vad> {
    vad: V,
    config: HaluConfig,
}

impl<V: Vad> HaluFilter<V> {
    pub fn new(vad: V, config: HaluConfig) -> Self {
        Self { vad, config }
    }

    /// 3 段 OR 合成で判定する (既存 4 引数 API・後方互換)。
    ///
    /// v0.1.3 patch2 以降は `judge_with_duration` で duration_ms を渡すことで
    /// 短発話 (<2 秒) の Tier 2 / Tier 3 スキップを有効化できる。
    /// 本関数は `judge_with_duration(... None)` の薄いラッパー (既存挙動維持)。
    ///
    /// 引数:
    ///   - `text`: 文字起こし結果
    ///   - `audio_samples`: Tier 1 判定用 (16kHz mono 推奨・TranscriptionFrame.samples)
    ///   - `no_speech_prob`: Groq verbose_json から (None なら Tier 2 スキップ)
    ///   - `min_logprob`: Groq verbose_json から (None なら Tier 3 スキップ)
    ///
    /// 注: Tier 3 は「既知句一致 AND 低 logprob」の AND が tier 内仕様。
    ///     Tier 1/2/3 の間は OR (L088)。
    pub fn judge(
        &self,
        text: &str,
        audio_samples: &[f32],
        no_speech_prob: Option<f32>,
        min_logprob: Option<f32>,
    ) -> HaluVerdict {
        self.judge_with_duration(text, audio_samples, no_speech_prob, min_logprob, None)
    }

    /// v0.1.3 patch2: 5 引数版。duration_ms を渡せる。
    ///
    /// `duration_ms = Some(<SHORT_UTTERANCE_THRESHOLD_MS)` (= 2000) のとき:
    ///   - Tier 2 (no_speech_prob) をスキップ
    ///   - Tier 3 (既知句完全一致 + 低 logprob AND) をスキップ
    ///   - Tier 1 (VAD) / Tier 1.5 (ASCII 短文) / Tier 3.5 (既知 substring) は維持
    ///
    /// 真因対策: 短発話 (例: 「ミュージカル」-0.59 / 「松井九」-0.78) は logprob が
    /// 自然に下がるため、確定ハルシでない正常発話まで false-positive で抑止される問題。
    pub fn judge_with_duration(
        &self,
        text: &str,
        audio_samples: &[f32],
        no_speech_prob: Option<f32>,
        min_logprob: Option<f32>,
        duration_ms: Option<i64>,
    ) -> HaluVerdict {
        // v0.1.3 patch2: 短発話 (<2 秒) は Tier 2 / Tier 3 をスキップする。
        // Tier 1 (VAD) / Tier 1.5 (ASCII 短文) / Tier 3.5 (既知 substring) は維持し、
        // 明確な既知ハルシ・無音以外の正常な短発話は通す設計。
        let is_short_utterance = duration_ms
            .map(|d| d < SHORT_UTTERANCE_THRESHOLD_MS)
            .unwrap_or(false);
        // Tier 1: VAD
        if !self.vad.has_speech(audio_samples) {
            let v = HaluVerdict::Drop(HaluReason::NoSpeechVad);
            // v0.1.12 F1 (L266): debug! -> info! 昇格 (本番ログで Drop 理由を観測可能化)
            info!(target: "rushvoice.halu", "drop reason={:?}", v);
            return v;
        }

        // Tier 1.5 (HL-03): ASCII 主体の短文は英語ハルシ残骸として破棄
        // (VAD/no_speech_prob すり抜けたケースの最終防御層・L088 OR 合成の 4 段目)
        let cleaned_len = text.chars().filter(|c| !c.is_whitespace()).count();
        if cleaned_len > 0 && cleaned_len <= self.config.ascii_short_max_len {
            let ratio = ascii_ratio(text);
            if ratio >= self.config.ascii_short_min_ratio {
                let v = HaluVerdict::Drop(HaluReason::AsciiShort {
                    len: cleaned_len,
                    ascii_ratio: ratio,
                });
                // v0.1.12 F1 (L266): debug! -> info! 昇格
                info!(target: "rushvoice.halu", "drop reason={:?}", v);
                return v;
            }
        }

        // Tier 2: no_speech_prob
        // v0.1.3 patch2: 短発話 (<2 秒) はスキップ (Tier 3.5 substring で別経路検出)
        if !is_short_utterance {
            if let Some(p) = no_speech_prob {
                if p > self.config.no_speech_prob_threshold {
                    let v = HaluVerdict::Drop(HaluReason::NoSpeechProb {
                        value: p,
                        threshold: self.config.no_speech_prob_threshold,
                    });
                    // v0.1.12 F1 (L266): debug! -> info! 昇格
                    info!(target: "rushvoice.halu", "drop reason={:?}", v);
                    return v;
                }
            }
        }

        // Tier 3.2 (Air v1.1.5 移植・2026-08-16): 既知句と完全一致なら logprob を問わず破棄。
        // - logprob 条件なし: Whisper は字幕クレジット由来ハルシを高 confidence で返すため
        //   (実機 id=742 「字幕作成者 。）。」= avg_logprob -0.303 が Tier 3 の -0.5 を上回った)
        // - 短発話スキップの対象外: 完全一致なので短発話でも誤検知しない
        // - 完全一致のみ: 「字幕作成者は誰ですか」等の正規発話は一致せず通る
        if !self.config.known_hallucinations_always.is_empty() {
            let normalized_text = normalize(text);
            if !normalized_text.is_empty() {
                for phrase in &self.config.known_hallucinations_always {
                    if normalize(phrase) == normalized_text {
                        let v = HaluVerdict::Drop(HaluReason::KnownPhraseAlways {
                            phrase: phrase.clone(),
                        });
                        info!(target: "rushvoice.halu", "drop reason={:?}", v);
                        return v;
                    }
                }
            }
        }

        // Tier 3: 既知句完全一致 AND 低 logprob
        // v0.1.3 patch2: 短発話 (<2 秒) はスキップ。短発話は Tier 3.5 substring で破棄。
        if !is_short_utterance {
            if let Some(lp) = min_logprob {
                if lp < self.config.min_logprob_threshold
                    && !self.config.known_hallucinations.is_empty()
                {
                    let normalized_text = normalize(text);
                    if !normalized_text.is_empty() {
                        for phrase in &self.config.known_hallucinations {
                            if normalize(phrase) == normalized_text {
                                let v = HaluVerdict::Drop(HaluReason::KnownPhraseLowLogprob {
                                    phrase: phrase.clone(),
                                    min_logprob: lp,
                                    threshold: self.config.min_logprob_threshold,
                                });
                                // v0.1.12 F1 (L266): debug! -> info! 昇格
                                info!(target: "rushvoice.halu", "drop reason={:?}", v);
                                return v;
                            }
                        }
                    }
                }
            }
        }

        // Tier 3.5 (v0.1.3): 既知ハルシ部分文字列マッチ (大きなまとまり判定)
        // - logprob 条件なし (Groq Whisper は無音時でも高 confidence で返すため)
        // - normalize した text に対して contains で判定 (句読点・空白の差分を吸収)
        // - 「大きなまとまり」(6 文字以上の N-gram) のみ登録すること。短い単語は誤検知防止のため NG。
        if !self.config.known_hallucination_substrings.is_empty() {
            let normalized_text = normalize(text);
            if !normalized_text.is_empty() {
                for sub in &self.config.known_hallucination_substrings {
                    let normalized_sub = normalize(sub);
                    if !normalized_sub.is_empty() && normalized_text.contains(&normalized_sub) {
                        let v = HaluVerdict::Drop(HaluReason::KnownSubstring {
                            substring: sub.clone(),
                        });
                        // v0.1.12 F1 (L266): debug! -> info! 昇格
                        info!(target: "rushvoice.halu", "drop reason={:?}", v);
                        return v;
                    }
                }
            }
        }

        // v0.1.12 F1 (L266): debug! -> info! 昇格 + メタデータ (no_speech_prob/min_logprob/duration_ms) 追加。
        // pass 時の品質指標を本番ログで観測可能にする (ハルシ判定境界の事後分析用)。
        info!(
            target: "rushvoice.halu",
            "pass text_len={} no_speech_prob={:?} min_logprob={:?} duration_ms={:?}",
            text.len(),
            no_speech_prob,
            min_logprob,
            duration_ms
        );
        HaluVerdict::Pass
    }
}

/// テキスト正規化: 空白 + 句読点を除去して完全一致を取りやすくする。
/// 外部 (例: examples) から halu の判定ロジックと整合した比較を行うために pub。
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !is_punct(*c))
        .collect()
}

/// HL-03 用: ASCII 文字割合 (制御文字含む・空白除外後)。
/// 空文字列の場合は 0.0 を返す。
/// is_ascii_alphabetic ではなく is_ascii を使う (数字・記号も英語ハルシの構成要素)。
pub fn ascii_ratio(text: &str) -> f32 {
    let trimmed: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.is_empty() {
        return 0.0;
    }
    let ascii = trimmed.iter().filter(|c| c.is_ascii()).count();
    ascii as f32 / trimmed.len() as f32
}

fn is_punct(c: char) -> bool {
    matches!(
        c,
        '。' | '、'
            | '!'
            | '?'
            | '！'
            | '？'
            | '.'
            | ','
            | '・'
            | '〜'
            | '～'
            // Air v1.1.5 移植 (2026-08-16): 括弧類を追加。実機 id=742 が「字幕作成者 。）。」を返し、
            // 「）」が残るせいで既知句「字幕作成者」との完全一致に失敗した。
            // Whisper は (拍手) (音楽) 等の括弧付き表記も混ぜるため、
            // 照合前に落としておく。
            | '('
            | ')'
            | '（'
            | '）'
            | '「'
            | '」'
            | '『'
            | '』'
    )
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Test 用ダミー VAD (固定値を返す)
    struct AlwaysSpeech;
    impl Vad for AlwaysSpeech {
        fn has_speech(&self, _: &[f32]) -> bool {
            true
        }
    }
    struct AlwaysSilent;
    impl Vad for AlwaysSilent {
        fn has_speech(&self, _: &[f32]) -> bool {
            false
        }
    }

    // ------ EnergyVad ------

    #[test]
    fn energy_vad_silence_is_not_speech() {
        let v = EnergyVad::default();
        assert!(!v.has_speech(&[0.0_f32; 1600]));
    }

    #[test]
    fn energy_vad_loud_signal_is_speech() {
        let v = EnergyVad::default();
        let s: Vec<f32> = (0..1600).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        assert!(v.has_speech(&s));
    }

    #[test]
    fn energy_vad_empty_is_not_speech() {
        let v = EnergyVad::default();
        assert!(!v.has_speech(&[]));
    }

    #[test]
    fn energy_vad_threshold_boundary() {
        let v = EnergyVad::new(0.1);
        // RMS = 0.05 → 閾値未満
        assert!(!v.has_speech(&[0.05_f32; 1000]));
        // RMS = 0.15 → 閾値超過
        assert!(v.has_speech(&[0.15_f32; 1000]));
    }

    // ------ HaluFilter Tier 1 ------

    #[test]
    fn tier1_vad_silent_drops_regardless_of_other_signals() {
        let f = HaluFilter::new(AlwaysSilent, HaluConfig::default());
        let v = f.judge("こんにちは", &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechVad)));
    }

    // ------ HaluFilter Tier 2 ------

    #[test]
    fn tier2_high_no_speech_prob_drops() {
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("こんにちは", &[0.5; 1600], Some(0.7), Some(0.0));
        match v {
            HaluVerdict::Drop(HaluReason::NoSpeechProb { value, threshold }) => {
                assert!((value - 0.7).abs() < 1e-6);
                assert!((threshold - 0.6).abs() < 1e-6);
            }
            other => panic!("expected NoSpeechProb drop, got {:?}", other),
        }
    }

    #[test]
    fn tier2_below_threshold_passes_when_others_ok() {
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("こんにちは", &[0.5; 1600], Some(0.5), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier2_skipped_when_none() {
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("こんにちは", &[0.5; 1600], None, None);
        assert!(matches!(v, HaluVerdict::Pass));
    }

    // ------ HaluFilter Tier 3 ------

    #[test]
    fn tier3_matching_phrase_with_low_logprob_drops() {
        let cfg = HaluConfig {
            known_hallucinations: vec!["テスト用ハルシ句".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge("テスト用ハルシ句", &[0.5; 1600], Some(0.0), Some(-3.0));
        match v {
            HaluVerdict::Drop(HaluReason::KnownPhraseLowLogprob { phrase, .. }) => {
                assert_eq!(phrase, "テスト用ハルシ句");
            }
            other => panic!("expected Tier 3 drop, got {:?}", other),
        }
    }

    #[test]
    fn tier3_matching_phrase_but_logprob_high_passes() {
        // tier 内の AND が破れる: logprob > 閾値なので drop しない
        let cfg = HaluConfig {
            known_hallucinations: vec!["テスト用ハルシ句".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge("テスト用ハルシ句", &[0.5; 1600], Some(0.0), Some(-1.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier3_low_logprob_but_phrase_unknown_passes() {
        // tier 内 AND の他方が破れる: 一致句がない
        let cfg = HaluConfig {
            known_hallucinations: vec!["別の句".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge("普通の発話", &[0.5; 1600], Some(0.0), Some(-5.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier3_normalization_ignores_punctuation_and_spaces() {
        let cfg = HaluConfig {
            known_hallucinations: vec!["こんにちは世界".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        // 句読点・空白入りでも正規化で一致
        let v = f.judge(
            " こんにちは、世界！ ",
            &[0.5; 1600],
            Some(0.0),
            Some(-3.0),
        );
        assert!(matches!(
            v,
            HaluVerdict::Drop(HaluReason::KnownPhraseLowLogprob { .. })
        ));
    }

    #[test]
    fn tier3_empty_known_list_passes() {
        // L078 既定 (空リスト) では Tier 3 は発動しない
        // 注: "any text" は HL-03 Tier 1.5 (ASCII 短文) で弾かれるため
        //     日本語テキスト (ascii_ratio ≈ 0) を使って Tier 3 だけをテスト
        let cfg = HaluConfig::default();
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge("普通の日本語発話です", &[0.5; 1600], Some(0.0), Some(-10.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    // ------ HaluFilter Tier 3.2 (Air v1.1.5 移植・2026-08-16) ------

    #[test]
    fn tier32_matching_phrase_drops_regardless_of_high_logprob() {
        // Tier 3 (AND 低 logprob) では捕まらない高 confidence でも、Tier 3.2 は完全一致だけで落とす。
        // 🔴 どの層が落としたか (reason) まで固定する (L928)。
        let cfg = HaluConfig {
            known_hallucinations_always: vec!["字幕作成者".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration(
            "字幕作成者 。）。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.303),
            Some(5_800),
        );
        match v {
            HaluVerdict::Drop(HaluReason::KnownPhraseAlways { ref phrase }) => {
                assert_eq!(phrase, "字幕作成者")
            }
            other => panic!("expected Tier 3.2 drop, got {:?}", other),
        }
    }

    #[test]
    fn tier32_is_not_skipped_for_short_utterance() {
        // 完全一致なので短発話 (<4 秒) でも誤検知しない → 短発話スキップの対象外。
        let cfg = HaluConfig {
            known_hallucinations_always: vec!["字幕作成者".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration("字幕作成者", &[0.5; 1600], Some(0.0), Some(-0.1), Some(1_500));
        assert!(matches!(
            v,
            HaluVerdict::Drop(HaluReason::KnownPhraseAlways { .. })
        ));
    }

    #[test]
    fn tier32_partial_match_passes() {
        // 完全一致のみ。句を含むだけの正規発話は通る。
        let cfg = HaluConfig {
            known_hallucinations_always: vec!["字幕作成者".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration(
            "字幕作成者は誰ですか。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.1),
            Some(5_000),
        );
        assert!(matches!(v, HaluVerdict::Pass), "got {:?}", v);
    }

    #[test]
    fn tier32_empty_list_is_inert() {
        // 既定 (空リスト) では Tier 3.2 は発動しない (既存挙動を変えない)。
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration("字幕作成者", &[0.5; 1600], Some(0.0), Some(-0.1), Some(5_000));
        assert!(matches!(v, HaluVerdict::Pass), "got {:?}", v);
    }

    #[test]
    fn normalize_strips_brackets_and_quotes() {
        // Air v1.1.5: 「）」が残ると完全一致に失敗する (id=742)。括弧類・かぎ括弧を落とす。
        assert_eq!(normalize("字幕作成者 。）。"), "字幕作成者");
        assert_eq!(normalize("(拍手) 「ありがとう」『音楽』（笑）"), "拍手ありがとう音楽笑");
    }

    // ------ OR 合成の優先順位 ------

    #[test]
    fn or_combine_tier1_takes_precedence() {
        // Tier 1 で drop なら Tier 2/3 の値を見ない
        let cfg = HaluConfig {
            known_hallucinations: vec!["x".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSilent, cfg);
        let v = f.judge("x", &[0.5; 1600], Some(0.99), Some(-10.0));
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechVad)));
    }

    // ------ HL-03 Tier 1.5 ASCII 短文破棄 ------

    #[test]
    fn ascii_ratio_pure_ascii_is_one() {
        assert!((ascii_ratio("Thank you for watching.") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ascii_ratio_pure_japanese_is_zero() {
        assert!(ascii_ratio("こんにちは世界").abs() < 1e-6);
    }

    #[test]
    fn ascii_ratio_empty_is_zero() {
        assert!(ascii_ratio("").abs() < 1e-6);
        assert!(ascii_ratio("   ").abs() < 1e-6);
    }

    #[test]
    fn tier1_5_ascii_short_phrase_drops() {
        // 代表的な Groq Whisper 英語ハルシ残骸を弾く
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge(
            "Thank you for watching.",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        match v {
            HaluVerdict::Drop(HaluReason::AsciiShort { len, ascii_ratio: r }) => {
                assert!(len > 0 && len <= 60);
                assert!(r >= 0.70);
            }
            other => panic!("expected AsciiShort drop, got {:?}", other),
        }
    }

    #[test]
    fn tier1_5_japanese_short_phrase_passes() {
        // 日本語短文は ASCII 比率が低いので通過
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("こんにちは", &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier1_5_long_english_text_passes() {
        // 60 字超の英語は (たとえ ASCII 100%) Tier 1.5 では弾かない
        // (短文ハルシ残骸対策に絞っているため・長文は別 tier 範囲)
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let long_text =
            "This is a normal English sentence which is much longer than the sixty character ceiling.";
        assert!(long_text.chars().filter(|c| !c.is_whitespace()).count() > 60);
        let v = f.judge(long_text, &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier1_5_mixed_jp_en_passes() {
        // 日本語混じり (ratio < 0.70) は通過
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("OK 了解しました", &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier1_5_empty_text_passes() {
        // 空文字 (cleaned_len == 0) は Tier 1.5 では drop せず Pass まで素通り
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge("", &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    // ------ Tier 3.5 (v0.1.3) 部分一致辞書 ------

    #[test]
    fn tier3_5_substring_match_drops_regardless_of_logprob() {
        // logprob が高 (= 高 confidence) でも、部分文字列マッチがあれば破棄
        // 実観測例: 「読点）を含む自然な」を含む prompt 漏洩ハルシ
        let cfg = HaluConfig {
            known_hallucination_substrings: vec![
                "自然な日本語で出力してください".to_string(),
                "読点）を含む自然な".to_string(),
                "を含む自然な日本語で出力".to_string(),
            ],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge(
            "読点）を含む自然な日本語で出力してください。ご視聴ありがとうございました。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.1), // 高 confidence でも drop
        );
        match v {
            HaluVerdict::Drop(HaluReason::KnownSubstring { substring }) => {
                // 最初にヒットした substring が返される (順序依存)
                assert!(
                    substring.contains("自然な")
                        || substring.contains("読点")
                        || substring.contains("出力"),
                    "想定 substring が返された: {}",
                    substring
                );
            }
            other => panic!("expected KnownSubstring drop, got {:?}", other),
        }
    }

    #[test]
    fn tier3_5_no_match_passes() {
        let cfg = HaluConfig {
            known_hallucination_substrings: vec!["自然な日本語で出力してください".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge("今日は良い天気です。", &[0.5; 1600], Some(0.0), Some(0.0));
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier3_5_normalization_strips_punctuation() {
        // text と substring 両方に NFKC 非対称な句読点・空白があっても正規化で一致
        let cfg = HaluConfig {
            known_hallucination_substrings: vec!["自然な日本語で出力".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge(
            " 自然な、日本語で 出力。 続きの本文",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        assert!(matches!(
            v,
            HaluVerdict::Drop(HaluReason::KnownSubstring { .. })
        ));
    }

    #[test]
    fn tier3_5_empty_substring_list_passes() {
        // 既定 (空 Vec) では Tier 3.5 は発動しない
        let cfg = HaluConfig::default();
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge(
            "自然な日本語で出力してください。",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        assert!(matches!(v, HaluVerdict::Pass));
    }

    #[test]
    fn tier3_5_three_prompt_leak_phrases_each_detected() {
        // v0.1.3 報告書記載 3 フレーズが個別に検出されること
        let cfg = HaluConfig {
            known_hallucination_substrings: vec![
                "自然な日本語で出力してください".to_string(),
                "読点）を含む自然な".to_string(),
                "を含む自然な日本語で出力".to_string(),
            ],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        // phrase 1
        let v1 = f.judge(
            "テスト 自然な日本語で出力してください。",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        assert!(matches!(v1, HaluVerdict::Drop(_)));
        // phrase 2
        let v2 = f.judge(
            "テスト 読点）を含む自然な日本語",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        assert!(matches!(v2, HaluVerdict::Drop(_)));
        // phrase 3
        let v3 = f.judge(
            "テスト を含む自然な日本語で出力する",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
        );
        assert!(matches!(v3, HaluVerdict::Drop(_)));
    }

    // ------ v0.1.3 patch: Whisper 既知日本語ハルシ 3 件 ------
    // Groq Whisper が高 confidence (avg_logprob ≈ -0.15) で長文末尾に混入させるパターン。
    // 完全一致 + 低 logprob ベースの Tier 3 では検出できないため Tier 3.5 に追加。

    #[test]
    fn tier3_5_whisper_japanese_halu_three_patterns_each_detected() {
        // v0.1.3 patch: 「ご視聴(ありがとう|頂きありがとう|いただきありがとう)ございました」3 件が
        // それぞれ高 confidence (avg_logprob -0.15) でも個別に検出されること
        let cfg = HaluConfig {
            known_hallucination_substrings: vec![
                "ご視聴ありがとうございました".to_string(),
                "ご視聴頂きありがとうございました".to_string(),
                "ご視聴いただきありがとうございました".to_string(),
            ],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        // pattern 1: 平仮名なし
        let v1 = f.judge(
            "本日の打ち合わせ要点を送ります。ご視聴ありがとうございました。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.15), // Groq 観測値
        );
        assert!(
            matches!(v1, HaluVerdict::Drop(HaluReason::KnownSubstring { .. })),
            "pattern1 がハルシ検出されるべき: {:?}",
            v1
        );
        // pattern 2: 「頂き」(漢字)
        let v2 = f.judge(
            "店舗状況を共有します。ご視聴頂きありがとうございました。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.15),
        );
        assert!(
            matches!(v2, HaluVerdict::Drop(HaluReason::KnownSubstring { .. })),
            "pattern2 がハルシ検出されるべき: {:?}",
            v2
        );
        // pattern 3: 「いただき」(平仮名)
        let v3 = f.judge(
            "経理データを更新しました。ご視聴いただきありがとうございました。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.15),
        );
        assert!(
            matches!(v3, HaluVerdict::Drop(HaluReason::KnownSubstring { .. })),
            "pattern3 がハルシ検出されるべき: {:?}",
            v3
        );
    }

    #[test]
    fn tier3_5_whisper_halu_normal_business_speech_passes() {
        // 回帰テスト: 「ご視聴」を含まない正常な業務発話は通る
        // (誤検知ゼロ確認・利用者業務文脈で発生し得る発話を複数検証)
        let cfg = HaluConfig {
            known_hallucination_substrings: vec![
                "ご視聴ありがとうございました".to_string(),
                "ご視聴頂きありがとうございました".to_string(),
                "ご視聴いただきありがとうございました".to_string(),
            ],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let normal_phrases = vec![
            "本日の売上は予算比 105% で着地しました。",
            "渡辺さんに仕訳の件を確認してください。",
            "ご来店ありがとうございました。", // 「ご視聴」ではない類似表現
            "ご対応ありがとうございました。",
            "ご利用ありがとうございました。",
        ];
        for phrase in normal_phrases {
            let v = f.judge(phrase, &[0.5; 1600], Some(0.0), Some(-0.15));
            assert!(
                matches!(v, HaluVerdict::Pass),
                "正常発話が誤検知された: \"{}\" -> {:?}",
                phrase,
                v
            );
        }
    }

    // ------ v0.1.3 patch2 (B): 短発話 logprob 判定スキップ ------

    #[test]
    fn patch2_short_utterance_skips_tier2_no_speech_prob() {
        // B 効果: 短発話 (<2 秒) なら no_speech_prob 高 (>0.6) でも drop しない
        // 真因: 「ミュージカル」「松井九」等の短発話が logprob 自然下降で false-positive
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "ミュージカル",
            &[0.5; 1600],
            Some(0.9),  // 通常なら Tier 2 で drop
            Some(0.0),
            Some(1200), // 1.2 秒 = 短発話
        );
        assert!(matches!(v, HaluVerdict::Pass), "短発話は Tier 2 スキップで Pass: {:?}", v);
    }

    #[test]
    fn patch2_short_utterance_skips_tier3_known_phrase_low_logprob() {
        // B 効果: 短発話なら Tier 3 (完全一致+低 logprob AND) もスキップ
        let cfg = HaluConfig {
            known_hallucinations: vec!["ミュージカル".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration(
            "ミュージカル",
            &[0.5; 1600],
            Some(0.0),
            Some(-5.0), // 通常なら Tier 3 で drop
            Some(800),  // 0.8 秒 = 短発話
        );
        assert!(matches!(v, HaluVerdict::Pass), "短発話は Tier 3 スキップで Pass: {:?}", v);
    }

    #[test]
    fn patch2_short_utterance_still_drops_via_substring() {
        // B + Tier 3.5: 短発話でも明確な既知 substring がマッチすれば drop
        let cfg = HaluConfig {
            known_hallucination_substrings: vec!["ご視聴ありがとうございました".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration(
            "ご視聴ありがとうございました",
            &[0.5; 1600],
            Some(0.0),
            Some(0.0),
            Some(1500), // 短発話だが substring で drop されるべき
        );
        assert!(
            matches!(v, HaluVerdict::Drop(HaluReason::KnownSubstring { .. })),
            "短発話でも substring 一致は drop: {:?}", v
        );
    }

    #[test]
    fn patch2_short_utterance_still_drops_via_vad() {
        // B + Tier 1: 短発話でも VAD が無音と判定すれば drop
        let f = HaluFilter::new(AlwaysSilent, HaluConfig::default());
        let v = f.judge_with_duration(
            "短い",
            &[0.0; 1600],
            Some(0.0),
            Some(0.0),
            Some(1000),
        );
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechVad)));
    }

    #[test]
    fn patch2_long_utterance_still_evaluates_tier2() {
        // B 回帰: 長発話 (>=SHORT_UTTERANCE_THRESHOLD_MS) では Tier 2 が従来通り発動
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大に伴い duration_ms を 3000 → 5000 に更新
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "これは長い発話のはずです",
            &[0.5; 1600],
            Some(0.9),  // Tier 2 で drop
            Some(0.0),
            Some(5000), // 5 秒 = 長発話 (閾値 4000 超)
        );
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechProb { .. })));
    }

    #[test]
    fn patch5_a_boundary_exactly_4000ms_is_long() {
        // patch5 (2026-05-24): 閾値 2000 → 4000 拡大。
        // 境界値: duration_ms == 4000 はちょうど閾値なので「短発話ではない」(< のみ short) → Tier 2 発動
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "境界",
            &[0.5; 1600],
            Some(0.9),
            Some(0.0),
            Some(4000), // ちょうど 4000 = 短発話ではない → Tier 2 発動
        );
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechProb { .. })));
    }

    #[test]
    fn patch5_a_boundary_3999ms_is_short() {
        // patch5: 境界値 -1ms (3999) は短発話 → Tier 2 スキップ → Pass
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "境界マイナス1",
            &[0.5; 1600],
            Some(0.9), // 通常なら Tier 2 で drop だが短発話スキップで Pass
            Some(0.0),
            Some(3999), // 4000 未満 = 短発話 → Tier 2/3 スキップ
        );
        assert!(matches!(v, HaluVerdict::Pass), "3999ms は短発話扱いで Pass: {:?}", v);
    }

    #[test]
    fn patch5_a_short_utterance_2500ms_with_low_logprob_returns_false() {
        // patch5 真因: id=173 「マツエク」(duration=2500ms, logprob=-0.55) → flag=1 で貼付抑止
        // 4000ms 閾値拡大により 2500ms は短発話扱い → Tier 2/3 スキップで Pass
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "マツエク",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.55), // patch4 までは 2500ms > 2000 で長発話扱い → 低 logprob で drop
            Some(2500),  // patch5 では 2500 < 4000 で短発話扱い → スキップ
        );
        assert!(
            matches!(v, HaluVerdict::Pass),
            "id=173 相当の短発話 (2500ms / logprob=-0.55) は Pass されるべき: {:?}",
            v
        );
    }

    #[test]
    fn patch5_a_long_utterance_4500ms_with_low_logprob_returns_true() {
        // 回帰: 4 秒超の長発話で低 logprob なら従来通り Tier 3 評価が走る
        // (known_hallucinations 一致 + 低 logprob → drop)
        let cfg = HaluConfig {
            known_hallucinations: vec!["長めの既知ハルシ句".to_string()],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge_with_duration(
            "長めの既知ハルシ句",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.55), // 閾値 -0.5 未満ではないので Tier 3 では落ちないことに注意
            Some(4500),  // 長発話
        );
        // logprob -0.55 は既定閾値 -2.5 を超えないので Tier 3 では drop しない → Pass
        // ただし長発話で評価ルートに乗ること (短発話スキップではないこと) を確認
        assert!(matches!(v, HaluVerdict::Pass));

        // 追加: より厳しい logprob -3.0 なら Tier 3 で drop することも併せて確認
        let cfg2 = HaluConfig {
            known_hallucinations: vec!["長めの既知ハルシ句".to_string()],
            ..HaluConfig::default()
        };
        let f2 = HaluFilter::new(AlwaysSpeech, cfg2);
        let v2 = f2.judge_with_duration(
            "長めの既知ハルシ句",
            &[0.5; 1600],
            Some(0.0),
            Some(-3.0),  // 既定閾値 -2.5 未満
            Some(4500),  // 長発話 → Tier 3 評価
        );
        assert!(
            matches!(v2, HaluVerdict::Drop(HaluReason::KnownPhraseLowLogprob { .. })),
            "4500ms 長発話で完全一致 + 低 logprob は drop されるべき: {:?}",
            v2
        );
    }

    #[test]
    fn patch2_duration_none_preserves_legacy_behavior() {
        // duration_ms = None なら既存挙動 (Tier 2 評価される)
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v = f.judge_with_duration(
            "テスト",
            &[0.5; 1600],
            Some(0.9),
            Some(0.0),
            None,
        );
        assert!(matches!(v, HaluVerdict::Drop(HaluReason::NoSpeechProb { .. })));
    }

    #[test]
    fn patch2_legacy_judge_4arg_delegates_to_judge_with_duration_none() {
        // 既存 4 引数 judge() は judge_with_duration(... None) と等価
        let f = HaluFilter::new(AlwaysSpeech, HaluConfig::default());
        let v4 = f.judge("テスト", &[0.5; 1600], Some(0.9), Some(0.0));
        let v5 = f.judge_with_duration("テスト", &[0.5; 1600], Some(0.9), Some(0.0), None);
        assert!(matches!(v4, HaluVerdict::Drop(HaluReason::NoSpeechProb { .. })));
        assert!(matches!(v5, HaluVerdict::Drop(HaluReason::NoSpeechProb { .. })));
    }

    // ------ v0.1.3 patch5.1 (2026-05-24): patch5 C テスト 3 件は revert に伴い削除 ------
    // 削除済: patch5_c_kohyumeishi_variations_each_detected
    //         patch5_c_kohyumeishi_safe_phrases_pass
    //         patch5_c_kohyumeishi_normalize_known_side_effect_documented
    // 理由: 要望により「固有名詞」バリエーション substring を辞書追加しない方針に変更。
    //       normalize() 副作用 (中黒/読点除去 →「固有名詞」単独縮退 → 誤検知 drop) を回避するため。

    #[test]
    fn tier3_5_whisper_halu_embedded_in_long_text_drops() {
        // 観測パターン: 長文業務発話の末尾に「ご視聴ありがとうございました」が混入するケース
        // recorder.rs の本番呼出と同じ設定 (prompt 漏洩 + Whisper ハルシ 6 件) で検証
        let cfg = HaluConfig {
            known_hallucination_substrings: vec![
                "自然な日本語で出力してください".to_string(),
                "読点）を含む自然な".to_string(),
                "を含む自然な日本語で出力".to_string(),
                "ご視聴ありがとうございました".to_string(),
                "ご視聴頂きありがとうございました".to_string(),
                "ご視聴いただきありがとうございました".to_string(),
            ],
            ..HaluConfig::default()
        };
        let f = HaluFilter::new(AlwaysSpeech, cfg);
        let v = f.judge(
            "今日の経営会議では Q2 の出店計画と人材採用の進捗を確認しました。\
             特に大阪エリアの候補物件 3 件について FC 委員会で承認を取ります。\
             ご視聴ありがとうございました。",
            &[0.5; 1600],
            Some(0.0),
            Some(-0.15),
        );
        match v {
            HaluVerdict::Drop(HaluReason::KnownSubstring { substring }) => {
                assert!(
                    substring.contains("ご視聴"),
                    "ご視聴系 substring が検出されるべき: {}",
                    substring
                );
            }
            other => panic!("expected KnownSubstring drop (ご視聴), got {:?}", other),
        }
    }
}
