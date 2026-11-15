// prompt_sanitize.rs — v0.1.3 防御第 1 層: Whisper prompt 漏洩の後処理サニタイズ
//
// 背景:
//   Day 17 v0.1.2 配布直後、history.db id=104 にて prompt 漏洩ハルシ事案発生。
//   raw_text = 「読点）を含む自然な日本語で出力してください。ご視聴ありがとうございました。」
//   真因: recorder.rs::WHISPER_PROMPT 「句読点（、。）を含む自然な日本語で出力してください。」が
//        Groq Whisper の応答に prefix として漏れ出した (Whisper の既知挙動・無音時 prompt エコー)。
//
// 設計判断 (v0.1.3 CIO 判断):
//   N-gram サフィックス最長一致方式を採用。
//   - prompt の suffix[i..len] (i=0..len) のうち、raw_text 冒頭に prefix として一致するものの最長を探索。
//   - 一致長 >= 6 文字 (日本語 N-gram 有意境界) の場合のみ切り捨て。
//   - Levenshtein は閾値が恣意的・誤切りリスクあり。連続文字一致は実装単純・誤検知ゼロ。
//
// 6 文字閾値の根拠:
//   - 日本語の 4-5 文字は単語境界として頻出 (「ありがとう」5/「ご視聴」3 等)
//   - 6 文字以上は「文脈に依存する複数単語の連続」となり偶発一致は実質ゼロ
//   - 実観測例: 24 文字の最長一致 → 切り捨て対象 / 3 文字「日本語」単独 → 触らない
//
// NFKC 正規化はしない:
//   - prompt と raw_text は同じ Whisper モデルが返す → 文字コード差異なし (実観測)
//   - NFKC するとサフィックス長がずれて誤切りリスクが上がる
//
// 公開 API:
//   - WHISPER_PROMPT: const &str  ← recorder.rs から参照する単一情報源
//   - PROMPT_ECHO_REFERENCE: const &str  ← 第 4 層の照合専用 (送信しない・縮めない)
//   - sanitize_prompt_leak(prompt, raw_text) -> String
//   - strip_prompt_leak_head_fragment(raw) -> String
//   - strip_prompt_echo_sentences(reference, text, avg_logprob) -> String  (第 4 層)

/// Whisper API に渡す prompt 文字列。
/// recorder.rs::transcribe_collected がこの定数を使う。
/// L190 (プロファイル設定値とプロンプト文面の二重定義禁止) 遵守のため、
/// ここを単一情報源とし recorder.rs 側のハードコードを廃止する。
///
/// 2026-08-08: 約 100 字 → 39 字へ圧縮した。
///
/// 真因: Whisper の `prompt` は**指示を実行するパラメータではない**。直前の文脈テキスト
/// として条件付けに使われるだけで、Whisper は指示追従モデルではないため、長い禁止文
/// (「語尾を変えないでください」等) は効いても副次的でしかない。一方 prompt 窓は
/// 224 トークンしかなく、圧迫すると**語彙ヒントが押し出される**。語彙ヒントは
/// デコードを直接誘導するので費用対効果がはるかに高い。
///
/// 圧縮後も残した意図は 2 つだけ:
///   - 句読点を含む文体を例示する (句読点付き出力を誘導する。これは文体条件付けとして実際に効く)
///   - 語尾・助詞を保つ (v0.1.x で実観測された書き換えへの保険。短く言い換えただけで意図は同じ)
///
/// 総量の上限は recorder.rs::WHISPER_PROMPT_TOTAL_MAX_CHARS 側で担保する。
pub const WHISPER_PROMPT: &str =
    "句読点（、。）を含む自然な日本語で書き起こす。語尾や助詞は話したまま変えない。";

/// 第 4 層 (strip_prompt_echo_sentences) の**照合専用**文言。送信はしない。
/// (無料版 RushVoice Air v1.1.4 (2026-08-14) からの移植・2026-08-16)
///
/// 🔴 WHISPER_PROMPT と分けている理由 (L908):
///   第 4 層は「照合先の文言に似た文」を落とす。照合先を送信文と共用すると、
///   将来 WHISPER_PROMPT を短縮した瞬間、削った部分のエコーを検出できなくなる
///   (Air では送信文を句読点誘導の 1 文へ縮めた後も、削った「語尾や助詞は話したまま変えない。」の
///    エコー (実機 id=335「語尾や助詞は話したい。」) を照合先の分離で捕まえている:
///    被覆率は照合先を分離すれば 0.889・送信文と共用だと 0.000)。
///   照合先は**過去に送ったことのある文言をすべて含む上位集合**として固定し、縮めない。
///
/// 有料版 (本リポジトリ) の中身: 2026-08-08 以降に実際に Whisper へ送っている指示文 2 文
/// (= 現行 WHISPER_PROMPT と同一文字列)。`build_dynamic_whisper_prompt` が末尾へ足す
/// 辞書の語彙ヒント (最大 20 語) は正当な発話語なので照合先に**含めない**。
///
/// 🔴 2026-08-08 より前に送っていた旧指示文 (26 字版・約 100 字版) は**意図的に含めていない**:
///   - 閾値 (0.65 / 0.55) は Air が現行 2 文に対して実履歴で校正した値。照合先を変えると根拠が崩れる
///   - 旧 100 字版は「してください」「変えないでください」等のありふれた bigram を大量に含み、
///     正常発話「それは変えないでください。」が被覆率 0.727 で誤検知になる (2026-08-16 実測)。
///     旧 26 字版を足すだけでも保護テストの「議事録を自然な日本語でまとめてください。」が
///     0.556 になり低確信時に落ちる
///   - 旧 26 字版のエコーは Tier 3.5 の部分一致 (「自然な日本語で出力してください」等) が従来どおり守る
///
/// 新しい prompt 文言を導入する場合は、この定数へ**追記**する形にし、追記後に実履歴で
/// 誤検知が出ないことを測ってから入れること。整合はテスト
/// (`recorder::tests::echo_reference_is_superset_of_sent_prompt`) で担保する。
pub const PROMPT_ECHO_REFERENCE: &str =
    "句読点（、。）を含む自然な日本語で書き起こす。語尾や助詞は話したまま変えない。";

/// prompt 漏洩を検出した場合に切り捨てる最小一致文字数。
/// 5 文字以下は偶発的な短い一致と判定し、raw_text を触らない。
pub const MIN_LEAK_MATCH_CHARS: usize = 6;

/// v0.1.7.2 patch (Plan B): 短断片 prompt 漏洩 strip 用の冒頭断片リスト。
///
/// 背景:
///   Day 28 真因調査で「読点）」混入バグが判明。Whisper prompt の
///   "句読点（、。）を含む自然な日本語で出力してください。" のうち
///   "読点）" 部分のみが冒頭にエコー漏洩し、その後に本文が続くケース
///   (history.db id=547「読点）ダウンロード…」/ id=527「読点） 未払費用…」)
///   が 11.5h で 2 件 (3.6%) 発生。
///
/// 設計判断:
///   - sanitize_prompt_leak (6 char 閾値) は連続サフィックス一致のため、
///     prompt 内で非連続な「読点）」だけの漏洩を検出できない。
///   - halu.rs Tier 3.5 substring は normalize() が「）」を除去するため
///     "読点）" 形で contains 一致せず通過する。
///   - 本関数は完全一致リストベースで冒頭の短断片だけ除去し、本文を保護する。
///   - 「読点」単独 (「」なし) は誤検知防止のためリストに含めない
///     (正規発話「読点を使った文章」を保護するため)。
const PROMPT_LEAK_HEAD_FRAGMENTS: &[&str] = &[
    "読点）",
    "読点)",
    "句読点）",
    "句読点)",
    "、。）",
    "、。)",
    "（、",
    "(、",
];

/// 冒頭の短い prompt 漏洩断片を除去する (v0.1.7.2 patch・Plan B)。
///
/// 戻り値:
///   - 冒頭が PROMPT_LEAK_HEAD_FRAGMENTS のいずれかで始まる場合:
///     その断片を除去し、続く本文を trim_start して返す。
///   - 一致しない場合: raw 文字列をそのまま返す (clone)。
///
/// 6 char 閾値の sanitize_prompt_leak とは別経路。両関数を併用すること。
pub fn strip_prompt_leak_head_fragment(raw: &str) -> String {
    let trimmed = raw.trim_start();
    for frag in PROMPT_LEAK_HEAD_FRAGMENTS {
        if let Some(rest) = trimmed.strip_prefix(frag) {
            return rest.trim_start().to_string();
        }
    }
    raw.to_string()
}

// ---------------------------------------------------------------------------
// 防御第 4 層: prompt エコーの「言い換え」検出 (文字 bigram 被覆率)
// 無料版 RushVoice Air v1.1.3〜v1.1.6 (2026-08-13〜08-15) からの移植 (2026-08-16)。
// 判定ロジック・定数・閾値は Air と同一。照合先 PROMPT_ECHO_REFERENCE だけ本リポジトリの
// 送信文から作っている (上のコメント参照)。
// ---------------------------------------------------------------------------
//
// 背景 (2026-08-13 開発機実機 id=460・Air):
//   raw_text = 「を含む自然な日本語で書き上げる。」(avg_logprob -0.837)
//   WHISPER_PROMPT の「…を含む自然な日本語で書き起こす。」の **言い換え**
//   (書き起こす → 書き上げる) が漏洩した。
//
// 既存 3 層がすべてすり抜けた理由:
//   - sanitize_prompt_leak: prompt との連続一致が「を含む自然な日本語で書き」で
//     途切れる。冒頭 prefix 一致は成立するが、動詞が違うため切った残りが
//     「上げる。」となり本文として残ってしまう (実際は全部ゴミ)。
//   - strip_prompt_leak_head_fragment: 冒頭が「読点）」等ではない。
//   - halu Tier 3.5: 登録語が「自然な日本語で出力」「自然な日本語で書き起こす」で、
//     「書き上げる」形は未登録。**完全一致リスト方式の構造的限界**。
//     新しい言い換えが出るたびに後追いになる。
//
// 設計判断:
//   文字 bigram の被覆率 (対象文の bigram のうち prompt にも在る割合) で判定する。
//   語形変化・脱落・語順の揺れに強く、リストの後追いが要らない。
//
// 閾値の根拠 (開発機の実履歴 147 件 / 379 文で実測・2026-08-13・Air):
//   - 実ハルシ 4 文:  0.941 / 0.889 / 0.889 / 0.786
//   - 正常発話の最大: 0.276 (「読点）を含む自然なことがあったら…」)
//   → 0.65 は両者の中間に広い余裕を取れる。実履歴での誤検知は 0 件。
//   - 合成した際どい正常発話「日本語で書き起こす資料を作って。」= 0.571 も
//     0.65 なら保護される (0.55 では誤検知した)。
//
// 最小長 10 文字の根拠:
//   短文は偶発的に被覆率が跳ね上がる。実測で
//   「日本語で書いた。」(7 字) = 0.667 /「自然な日本語ですね。」(9 字) = 0.750。
//   どちらも正常発話なので、10 字未満は本層の対象外にして保護する。
//   実ハルシ 4 文はすべて 10 字以上 (最短「語尾や助詞は話したい」= 10 字)。
//
// v1.1.6 (2026-08-15) 追記: 0.65 単独では取り逃がす実例が出た。
//   開発機実機 id=881「を含む自然などのように書き起こす。」= 被覆率 **0.600** / `avg_logprob -0.834`。
//   0.65 に 0.05 届かず素通りし、そのまま貼り付けられた。
//
//   🔴 閾値を 0.55 へ単純に下げるのは**してはいけない**。上のコメントのとおり
//   合成した際どい正常発話「日本語で書き起こす資料を作って。」= 0.571 が誤検知になる
//   (実測で再確認済み・エコー 0.600 との差はわずか 0.029)。
//
//   → 代わりに **2 つ目の手がかり `avg_logprob` を足す**。L907 のとおり prompt エコーは
//     正常発話の帯 (-0.10〜-0.30) から外れた値を伴う (id=881 は -0.834)。
//     正常発話は被覆率が高くても logprob は正常帯に収まるので、両方を見れば分離できる。
//
//   実測 (履歴 298 件 / 446 文・2026-08-15・Air):
//     - 被覆率 0.55 以上は id=881 の 1 文のみ。**次点の正常発話は 0.222** と桁で離れている
//       (0.571 は前セッションが作った合成例であって実発話ではない)。
//     - 新ルールでの破棄は 446 文中 1 文 = 狙ったエコーだけ。誤検知 0 件。

/// 第 4 層の被覆率閾値。対象文の bigram のうちこの割合以上が prompt に在れば漏洩と判定。
/// この値以上なら `avg_logprob` を問わず破棄する。
pub const PROMPT_ECHO_MIN_COVERAGE: f32 = 0.65;

/// 第 4 層の**低確信時**の被覆率閾値 (v1.1.6)。
/// `PROMPT_ECHO_LOW_CONF_LOGPROB` を下回る低確信の文に限り、この被覆率で破棄する。
pub const PROMPT_ECHO_LOW_CONF_COVERAGE: f32 = 0.55;

/// 第 4 層の低確信判定に使う `avg_logprob` の上限 (v1.1.6)。
/// 正常発話は -0.10〜-0.30 に収まる (L907)。これを下回る文だけ被覆率の条件を緩める。
pub const PROMPT_ECHO_LOW_CONF_LOGPROB: f32 = -0.50;

/// 第 4 層の最小文字数 (正規化後)。これ未満の短文は偶発一致の危険があるため対象外。
pub const PROMPT_ECHO_MIN_CHARS: usize = 10;

/// 文字 bigram 集合を作る。比較の土台は halu::normalize (空白 + 句読点 + 括弧類除去) に揃える。
fn bigram_set(s: &str) -> std::collections::HashSet<(char, char)> {
    let chars: Vec<char> = crate::halu::normalize(s).chars().collect();
    chars.windows(2).map(|w| (w[0], w[1])).collect()
}

/// `sentence` の bigram のうち `prompt` にも存在する割合 (0.0〜1.0)。
/// bigram が作れない (正規化後 1 文字以下) 場合は 0.0。
fn prompt_bigram_coverage(prompt_bigrams: &std::collections::HashSet<(char, char)>, sentence: &str) -> f32 {
    let b = bigram_set(sentence);
    if b.is_empty() {
        return 0.0;
    }
    let hit = b.intersection(prompt_bigrams).count();
    hit as f32 / b.len() as f32
}

/// prompt の言い換えエコーを **文単位** で取り除く (防御第 4 層)。
///
/// 「該当文だけ落として残りは貼る」方針 (2026-08-13 合意・Air) を踏襲する。
/// 全文が漏洩だった場合は空文字を返すので、呼び出し側で「貼らない」経路へ倒すこと。
///
/// 判定対象は句点 (。！？!?) で区切った文のうち、正規化後 `PROMPT_ECHO_MIN_CHARS`
/// 文字以上のもの。落とす条件は次のいずれか (v1.1.6 で 2 本立てにした):
///   - 被覆率 >= `PROMPT_ECHO_MIN_COVERAGE` (0.65) … `avg_logprob` を問わない
///   - 被覆率 >= `PROMPT_ECHO_LOW_CONF_COVERAGE` (0.55)
///     **かつ** `avg_logprob < PROMPT_ECHO_LOW_CONF_LOGPROB` (-0.50)
///
/// `avg_logprob` が `None` (取得できなかった) の場合は低確信側の条件を使わない
/// = 従来どおり 0.65 のみで判定する (安全側)。
///
/// 🔴 `prompt` には送信文 (dynamic_prompt / WHISPER_PROMPT) ではなく
/// `PROMPT_ECHO_REFERENCE` を渡すこと (L908)。
pub fn strip_prompt_echo_sentences(prompt: &str, text: &str, avg_logprob: Option<f32>) -> String {
    if prompt.is_empty() || text.is_empty() {
        return text.to_string();
    }
    let low_confidence = avg_logprob
        .map(|lp| lp < PROMPT_ECHO_LOW_CONF_LOGPROB)
        .unwrap_or(false);
    let coverage_threshold = if low_confidence {
        PROMPT_ECHO_LOW_CONF_COVERAGE
    } else {
        PROMPT_ECHO_MIN_COVERAGE
    };
    let prompt_bigrams = bigram_set(prompt);
    if prompt_bigrams.is_empty() {
        return text.to_string();
    }

    let mut kept: Vec<String> = Vec::new();
    let mut current = String::new();
    let push_sentence = |sentence: &str, kept: &mut Vec<String>| {
        if sentence.trim().is_empty() {
            return;
        }
        let normalized_len = crate::halu::normalize(sentence).chars().count();
        if normalized_len >= PROMPT_ECHO_MIN_CHARS {
            let coverage = prompt_bigram_coverage(&prompt_bigrams, sentence);
            if coverage >= coverage_threshold {
                log::warn!(
                    "[transcribe] prompt echo (paraphrase) dropped: coverage={:.3} threshold={:.2} low_confidence={} avg_logprob={:?} len={} text={:?}",
                    coverage,
                    coverage_threshold,
                    low_confidence,
                    avg_logprob,
                    normalized_len,
                    sentence.trim()
                );
                return;
            }
        }
        kept.push(sentence.to_string());
    };

    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '。' | '！' | '？' | '!' | '?') {
            let sentence = std::mem::take(&mut current);
            push_sentence(&sentence, &mut kept);
        }
    }
    push_sentence(&current, &mut kept);

    kept.join("").trim().to_string()
}

/// prompt サフィックスが raw_text 冒頭に prefix として現れた場合に切り捨てる。
///
/// 戻り値:
///   - 検出なし: raw_text をそのまま返す (clone)
///   - 検出あり: prefix を除去した残り文字列を trim_start して返す
///
/// アルゴリズム:
///   1. prompt の文字列を char 単位で suffix 候補列挙
///   2. 各 suffix を raw_text 冒頭で starts_with 検証
///   3. 一致したものの中で最長 (= suffix 開始位置が最も早い = i 最小) を採用
///   4. 一致長 (char 数) >= MIN_LEAK_MATCH_CHARS なら raw_text から prefix を除去
pub fn sanitize_prompt_leak(prompt: &str, raw_text: &str) -> String {
    if prompt.is_empty() || raw_text.is_empty() {
        return raw_text.to_string();
    }
    let prompt_chars: Vec<char> = prompt.chars().collect();
    let raw_chars: Vec<char> = raw_text.chars().collect();
    let prompt_len = prompt_chars.len();

    // i = suffix 開始位置 (0 = prompt 全体・最長候補から検証)
    // 最長一致を取りたいので i は 0 から増やしていき、最初にマッチしたものを採用する。
    for i in 0..prompt_len {
        let suffix_len = prompt_len - i;
        if suffix_len < MIN_LEAK_MATCH_CHARS {
            // 残り suffix が閾値未満なら以降を見る意味なし
            break;
        }
        if suffix_len > raw_chars.len() {
            continue;
        }
        let mut matched = true;
        for j in 0..suffix_len {
            if prompt_chars[i + j] != raw_chars[j] {
                matched = false;
                break;
            }
        }
        if matched {
            // raw_text の suffix_len 文字を切り捨てて残りを返す
            let remaining: String = raw_chars.iter().skip(suffix_len).collect();
            return remaining.trim_start().to_string();
        }
    }
    raw_text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_full_prompt_prefix_leak() {
        // 実観測例 (history.db id=104): prompt 全体が冒頭に漏洩
        let raw = format!("{}ご視聴ありがとうございました。", WHISPER_PROMPT);
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, &raw);
        assert_eq!(cleaned, "ご視聴ありがとうございました。");
    }

    #[test]
    fn sanitizes_partial_suffix_leak_clean_subset() {
        // prompt の連続サフィックスがそのまま冒頭にあるケース (clean leak)。
        // 2026-08-08 の prompt 圧縮に追随して、新しい prompt の末尾一文へ差し替えた。
        // raw_text 冒頭にこの suffix が連続一致 → その分を切り捨てる。
        let raw = "語尾や助詞は話したまま変えない。ご視聴ありがとうございました。";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        assert_eq!(cleaned, "ご視聴ありがとうございました。");
    }

    #[test]
    fn does_not_sanitize_mixed_corruption_leak() {
        // history.db id=104 で実観測された「混在型漏洩」: raw_text 冒頭が prompt サフィックスと
        // 連続一致しないケース ("読点)" は prompt の "読点（、。" とは別文字列)。
        // 第 1 層では対応せず、第 3 層 (halu.rs::known_hallucination_substrings) で破棄する設計。
        let raw = "読点）を含む自然な日本語で出力してください。ご視聴ありがとうございました。";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        // 第 1 層では触らない (= raw のまま返す)
        assert_eq!(cleaned, raw);
        // この raw_text は第 3 層 (recorder.rs の known_hallucination_substrings に登録された
        // "読点）を含む自然な" / "を含む自然な日本語で出力" のいずれか) で破棄される。
    }

    #[test]
    fn preserves_text_when_no_leak() {
        let raw = "今日は良い天気ですね。";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn preserves_text_when_match_below_threshold() {
        // 「日本語」3 文字単独 (閾値 6 未満) は触らない
        let raw = "日本語って難しいですね。";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn preserves_text_when_match_in_middle_not_prefix() {
        // prompt 断片が途中に現れても (prefix でなければ) 触らない
        let raw = "今日は句読点（、。）を含む自然な日本語で書きました。";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn handles_empty_inputs() {
        assert_eq!(sanitize_prompt_leak("", "abc"), "abc");
        assert_eq!(sanitize_prompt_leak(WHISPER_PROMPT, ""), "");
        assert_eq!(sanitize_prompt_leak("", ""), "");
    }

    #[test]
    fn handles_raw_text_shorter_than_prompt() {
        // raw_text が prompt より短くても panic しない
        let raw = "短い";
        let cleaned = sanitize_prompt_leak(WHISPER_PROMPT, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn min_leak_match_chars_boundary_at_6() {
        // 6 文字ぴったりの一致は切り捨てる (境界値テスト)
        // prompt 末尾 6 文字 = 「ください。」(5 文字) → 「てください。」(6 文字)
        // ※「てください。」は 6 文字 (て/く/だ/さ/い/。)
        let prompt = "テストてください。";
        let raw = "てください。これは続き";
        let cleaned = sanitize_prompt_leak(prompt, raw);
        assert_eq!(cleaned, "これは続き");
    }

    #[test]
    fn min_leak_match_chars_below_threshold_at_5() {
        // 5 文字一致は触らない
        let prompt = "テストください。";
        let raw = "ください。これは続き";
        let cleaned = sanitize_prompt_leak(prompt, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn longest_match_wins_when_multiple_suffix_candidates() {
        // prompt = "AAAAAA・自然な日本語で出力" のとき
        // raw_text = "・自然な日本語で出力 ここから本文"
        // - i=6 suffix "・自然な日本語で出力" (10 文字) が一致 ← 最長
        // - i=11 suffix "本語で出力" (5 文字以下) は閾値未満
        // → 10 文字切り捨て
        let prompt = "AAAAAA・自然な日本語で出力";
        let raw = "・自然な日本語で出力 ここから本文";
        let cleaned = sanitize_prompt_leak(prompt, raw);
        assert_eq!(cleaned, "ここから本文");
    }

    // ----- v0.1.3 patch2 (D): 動的 prompt 対応テスト -----

    #[test]
    fn patch2_sanitizes_dynamic_prompt_with_glossary_full_leak() {
        // 動的 prompt = "WHISPER_PROMPT固有名詞: 末句, マツエク"
        // 全体が raw_text 冒頭に漏洩したケース
        let dynamic = format!("{}固有名詞: 末句, マツエク", WHISPER_PROMPT);
        let raw = format!("{}本文ここから", dynamic);
        let cleaned = sanitize_prompt_leak(&dynamic, &raw);
        assert_eq!(cleaned, "本文ここから");
    }

    #[test]
    fn patch2_sanitizes_dynamic_prompt_suffix_includes_glossary() {
        // 動的 prompt のサフィックス (固有名詞: 末句, マツエク を含む部分) が
        // raw_text 冒頭に漏洩したケース
        let dynamic = format!("{}固有名詞: 末句, マツエク", WHISPER_PROMPT);
        let raw = "固有名詞: 末句, マツエクお疲れさまでした。".to_string();
        let cleaned = sanitize_prompt_leak(&dynamic, &raw);
        assert_eq!(cleaned, "お疲れさまでした。");
    }

    #[test]
    fn patch2_dynamic_prompt_preserves_normal_text() {
        // glossary 付き動的 prompt でも、正常な発話 (一致なし) は無傷
        let dynamic = format!("{}固有名詞: 末句, マツエク", WHISPER_PROMPT);
        let raw = "今日の売上は予算比 105% で着地しました。";
        let cleaned = sanitize_prompt_leak(&dynamic, raw);
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn patch2_glossary_word_alone_below_threshold_not_sanitized() {
        // glossary 単語 (5 文字以下) が単独で raw_text 冒頭にあっても
        // 6 文字閾値未満なら触らない
        let dynamic = format!("{}固有名詞: 末句", WHISPER_PROMPT);
        let raw = "末句を確認してください。";
        let cleaned = sanitize_prompt_leak(&dynamic, raw);
        // 「末句」(2 文字) は閾値未満 → 触らない
        assert_eq!(cleaned, raw);
    }

    #[test]
    fn patch2_long_glossary_phrase_can_be_sanitized() {
        // 6 文字以上の glossary 句を含むサフィックスが冒頭に漏洩した場合は切り捨て
        let dynamic = format!("{}固有名詞: ハリウッドアイラッシュ", WHISPER_PROMPT);
        // raw_text 冒頭が dynamic の suffix と 6 文字以上連続一致
        // suffix "固有名詞: ハリウッドアイラッシュ" (16 文字) → 切り捨て
        let raw = "固有名詞: ハリウッドアイラッシュ本日もよろしくお願いします。";
        let cleaned = sanitize_prompt_leak(&dynamic, raw);
        assert_eq!(cleaned, "本日もよろしくお願いします。");
    }

    // ----- v0.1.7.2 patch (Plan B): strip_prompt_leak_head_fragment テスト -----

    #[test]
    fn strip_head_yomiten_close() {
        // 実バグ history.db id=547「読点）ダウンロード…」(全角閉じ括弧)
        assert_eq!(
            strip_prompt_leak_head_fragment("読点\u{ff09}ダウンロードした方がいいですか?"),
            "ダウンロードした方がいいですか?"
        );
    }

    #[test]
    fn strip_head_yomiten_close_with_space() {
        // 実バグ history.db id=527「読点） 未払費用…」(全角閉じ括弧 + 半角スペース)
        assert_eq!(
            strip_prompt_leak_head_fragment("読点\u{ff09} 未払費用ってこと"),
            "未払費用ってこと"
        );
    }

    #[test]
    fn strip_head_kuyomiten_variant() {
        // 「句読点）」前置バリアント (全角閉じ括弧)
        assert_eq!(
            strip_prompt_leak_head_fragment("句読点\u{ff09}これはテストです"),
            "これはテストです"
        );
    }

    #[test]
    fn strip_head_no_match_keeps_text() {
        assert_eq!(
            strip_prompt_leak_head_fragment("今日は良い天気です"),
            "今日は良い天気です"
        );
    }

    #[test]
    fn strip_head_legitimate_paren_use() {
        // 「読点」だけで「）」が無いケース・正規発話は保護
        assert_eq!(
            strip_prompt_leak_head_fragment("読点を使った文章です"),
            "読点を使った文章です"
        );
    }

    // ----- 防御第 4 層: strip_prompt_echo_sentences テスト (Air v1.1.3〜v1.1.6 から移植) -----
    //
    // 実データは開発機の history.db (Air・2026-08-13 時点 147 件 / 379 文) で採取した。
    // 照合先 PROMPT_ECHO_REFERENCE は Air と同一文字列なので、被覆率・閾値の根拠はそのまま通用する。

    #[test]
    fn echo_drops_paraphrased_prompt_id460() {
        // 🔴 本層の存在理由。実観測 id=460 (avg_logprob -0.837)。
        // 「書き起こす」→「書き上げる」の言い換えのため既存 3 層が全てすり抜けた。
        let raw = "を含む自然な日本語で書き上げる。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), "");
    }

    #[test]
    fn echo_drops_verbatim_prompt_fragment_id335() {
        // 実観測 id=335: 長い通話文字起こしの中に prompt 断片が複数回混入した。
        let raw = "読点）を含む自然な日本語で書き起こす。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), "");
    }

    #[test]
    fn echo_drops_second_half_paraphrase_id335() {
        // 実観測 id=335: prompt 後半「語尾や助詞は話したまま変えない」の言い換え。
        let raw = "語尾や助詞は話したい。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), "");
    }

    #[test]
    fn echo_keeps_body_and_drops_only_leaked_sentence() {
        // 🔴 「該当文だけ落として残りは貼る」(2026-08-13 合意) の担保。
        let raw = "明日の朝一で先方に連絡してください。を含む自然な日本語で書き上げる。よろしく。";
        assert_eq!(
            strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None),
            "明日の朝一で先方に連絡してください。よろしく。"
        );
    }

    #[test]
    fn echo_preserves_highest_scoring_legitimate_sentence() {
        // 実履歴 379 文のうち被覆率が最も高かった**正常**発話 (id=335・実測 0.276)。
        // ここを落としたら閾値が低すぎる。
        let raw = "読点）を含む自然なことがあったら、その方が、もしかしたら、わかります。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), raw);
    }

    #[test]
    fn echo_preserves_borderline_business_utterance() {
        // 合成した際どい正常発話 (実測 0.571)。閾値 0.55 では誤検知したため 0.65 を採用した。
        // v1.1.6: 低確信側 (0.55) を足したが、**正常な confidence なら従来どおり守られる**
        // ことをここで固定する (avg_logprob を正常帯 -0.20 にして検証)。
        let raw = "日本語で書き起こす資料を作って。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), raw);
        assert_eq!(
            strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.20)),
            raw
        );
    }

    /// 🔴 開発機実機 id=881 (Air・2026-08-15・`avg_logprob = -0.834`)。
    ///
    /// 被覆率 **0.600** で 0.65 に届かず、v1.1.5 まで素通りして貼り付けられていた。
    /// 単純に閾値を下げると `echo_preserves_borderline_business_utterance` (0.571) を
    /// 巻き込むため、`avg_logprob` を第 2 の手がかりにして分離している。
    #[test]
    fn echo_drops_low_confidence_paraphrase_from_id881() {
        let raw = "を含む自然などのように書き起こす。";
        // 低確信 (-0.834) なら破棄される
        assert_eq!(
            strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.834)),
            ""
        );
        // 🔴 avg_logprob が無い / 正常帯なら従来どおり 0.65 判定 = 残す (安全側)
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), raw);
        assert_eq!(
            strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.20)),
            raw
        );
    }

    /// 低確信であっても、本文まで巻き込まないこと (該当文だけ落とす方針の維持)。
    #[test]
    fn echo_low_confidence_drops_only_the_echoed_sentence() {
        let raw = "を含む自然などのように書き起こす。明日の朝一で先方に連絡してください。";
        assert_eq!(
            strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.834)),
            "明日の朝一で先方に連絡してください。"
        );
    }

    /// 🔴 低確信の閾値を下げすぎていないことの固定。
    /// 実履歴 446 文で被覆率 0.55 以上だったのは id=881 の 1 文のみで、
    /// 次点の正常発話は 0.222 だった。低確信でもこの帯は守る。
    #[test]
    fn echo_preserves_normal_speech_even_when_low_confidence() {
        for raw in [
            "所属がない人は見えないんですよね。",
            "日本語で書き起こす作業をお願いします。",
            "議事録を自然な日本語でまとめてください。",
            "この文章の語尾や助詞を整えてください。",
        ] {
            assert_eq!(
                strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.90)),
                raw,
                "低確信でも正常発話は残すべき: {}",
                raw
            );
        }
    }

    #[test]
    fn echo_preserves_short_sentences_below_min_chars() {
        // 短文は偶発的に被覆率が跳ね上がる (実測 0.667 / 0.750) ため最小長で保護する。
        for raw in ["日本語で書いた。", "自然な日本語ですね。"] {
            assert_eq!(
                strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None),
                raw,
                "短文 {:?} を誤って落とした",
                raw
            );
        }
    }

    #[test]
    fn echo_preserves_normal_speech() {
        let raw = "先方からメッセージが来ているので、確認をして拝見をさせてください。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), raw);
    }

    #[test]
    fn echo_handles_empty_inputs() {
        assert_eq!(strip_prompt_echo_sentences("", "abc", None), "abc");
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, "", None), "");
    }

    #[test]
    fn echo_handles_text_without_trailing_punctuation() {
        // 句点で終わらない末尾断片も判定対象にする
        let raw = "を含む自然な日本語で書き上げる";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, None), "");
    }

    #[test]
    fn echo_is_noop_when_prompt_is_empty_after_option_a() {
        // 照合先が空でも panic しない (Air v1.1.3 対策 A「辞書 0 件で prompt を送らない」構成の名残。
        // 有料版は常に送るが、空照合先の防御は残す)。
        let raw = "を含む自然な日本語で書き上げる。";
        assert_eq!(strip_prompt_echo_sentences("", raw, None), raw);
    }

    /// 有料版固有: 辞書の語彙ヒントは照合先に含めない。語彙ヒントで構成される正常発話は落とさない。
    #[test]
    fn echo_does_not_use_glossary_words_as_reference() {
        // 辞書語彙 (例) を並べて話した発話。照合先は指示文 2 文だけなので被覆率は低いまま。
        let raw = "マツエクとマツパの施術予約をお願いします。";
        assert_eq!(strip_prompt_echo_sentences(PROMPT_ECHO_REFERENCE, raw, Some(-0.90)), raw);
        // 照合先に辞書語彙を混ぜていないことの直接確認
        assert!(!PROMPT_ECHO_REFERENCE.contains("マツエク"));
    }
}
