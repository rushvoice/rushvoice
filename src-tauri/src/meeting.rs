// meeting.rs — v0.1.16 機能 L Day 3: 議事録モード純粋関数群
//
// SSOT: 内部設計メモ §機能L
// 委任 prompt: 内部設計メモ
//
// 設計方針:
// - 全関数を純粋関数として実装(外部 I/O / 状態依存ゼロ・テスト容易)
// - parse_segments: verbose_json body から MeetingSegment 配列を抽出
// - apply_global_offset: chunk_index × chunk_secs 秒を各 segment に加算(時刻補正)
// - split_into_paragraphs: ギャップ閾値と最大字数で改段
// - format_txt / format_md / format_srt: 整形 3 種
// - 一切の chunk_index/start_ms/end_ms は u64 ms 単位で扱う(f64 累積誤差排除)

use serde::{Deserialize, Serialize};

/// verbose_json から復元した 1 segment 分のタイムスタンプ + テキスト。
///
/// `start_ms` / `end_ms` は `apply_global_offset` 適用後はグローバル時刻
/// (録音開始からの ms)、適用前はチャンクローカル時刻。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingSegment {
    /// このセグメントが属するチャンク番号 (0 始まり)。デバッグ・再 transcribe 用。
    pub chunk_index: usize,
    /// グローバル/ローカル時刻 (ms 単位)。
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// `split_into_paragraphs` の出力単位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paragraph {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// `meeting.rs` 内で発生し得るエラー。
#[derive(Debug, thiserror::Error)]
pub enum MeetingError {
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("segments 配列が verbose_json body に存在しません")]
    SegmentsMissing,
    #[error("segment 内に start/end フィールドがありません")]
    SegmentMalformed,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// verbose_json body から MeetingSegment 配列を抽出する純粋関数。
///
/// Groq whisper-large-v3-turbo の verbose_json レスポンスは以下の形:
/// ```json
/// {
///   "text": "...",
///   "segments": [
///     {"id": 0, "start": 0.0, "end": 3.5, "text": "...", ...},
///     {"id": 1, "start": 3.5, "end": 7.2, "text": "...", ...}
///   ],
///   ...
/// }
/// ```
///
/// 各 segment の start / end は秒単位の f64。本関数は ms 単位の u64 に変換する
/// (round で四捨五入)。グローバル時刻補正は `apply_global_offset` 側で行う。
///
/// segments が空の場合は Ok(空配列) を返す (チャンクに発話なし・正常系)。
/// segments キー自体が欠落の場合は Err(SegmentsMissing)。
pub fn parse_segments(
    verbose_json_body: &str,
    chunk_index: usize,
) -> Result<Vec<MeetingSegment>, MeetingError> {
    let v: serde_json::Value = serde_json::from_str(verbose_json_body)?;
    let arr = v
        .get("segments")
        .and_then(|s| s.as_array())
        .ok_or(MeetingError::SegmentsMissing)?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let start_sec = item
            .get("start")
            .and_then(|x| x.as_f64())
            .ok_or(MeetingError::SegmentMalformed)?;
        let end_sec = item
            .get("end")
            .and_then(|x| x.as_f64())
            .ok_or(MeetingError::SegmentMalformed)?;
        let text = item
            .get("text")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            continue; // 空テキストの segment はスキップ
        }
        out.push(MeetingSegment {
            chunk_index,
            start_ms: (start_sec * 1000.0).round().max(0.0) as u64,
            end_ms: (end_sec * 1000.0).round().max(0.0) as u64,
            text,
        });
    }
    Ok(out)
}

/// チャンクローカル時刻 (start_ms/end_ms) にグローバル offset を加算する。
///
/// offset = chunk_index × chunk_secs × 1000 ms
///
/// **重要**: chunk_secs は固定値 (300 秒) を呼出側から渡す。サンプル数境界で
/// 5 分チャンク化している (Day 2 `should_split_chunk` / `compute_chunk_threshold_samples`) ため、
/// 各チャンクは厳密に chunk_secs 秒を超えない (L-R8 防止)。
///
/// 引数 `segs` を mutate ではなく新 Vec を返す (純粋関数)。
pub fn apply_global_offset(
    segs: Vec<MeetingSegment>,
    chunk_index: usize,
    chunk_secs: u32,
) -> Vec<MeetingSegment> {
    let offset_ms = (chunk_index as u64)
        .saturating_mul(chunk_secs as u64)
        .saturating_mul(1000);
    segs.into_iter()
        .map(|s| MeetingSegment {
            chunk_index: s.chunk_index,
            start_ms: s.start_ms.saturating_add(offset_ms),
            end_ms: s.end_ms.saturating_add(offset_ms),
            text: s.text,
        })
        .collect()
}

/// segment 配列を「ギャップ閾値」と「最大字数」で段落に区切る。
///
/// 規則 (SSOT §機能L 段落化):
///   - 隣接 segment の (next.start - prev.end) ≥ gap_threshold_ms → 改段
///   - 累積文字数 ≥ max_chars → 改段
///   - segment 内テキスト末尾は空白で連結 (日本語想定で空白なし)
///
/// 引数 `segs` は時系列順 (apply_global_offset 適用後) を前提とする。
pub fn split_into_paragraphs(
    segs: &[MeetingSegment],
    gap_threshold_ms: u64,
    max_chars: usize,
) -> Vec<Paragraph> {
    if segs.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Paragraph> = Vec::new();
    let mut cur_start = segs[0].start_ms;
    let mut cur_end = segs[0].end_ms;
    let mut cur_text = segs[0].text.clone();

    for w in segs.windows(2) {
        let prev = &w[0];
        let next = &w[1];
        let gap = next.start_ms.saturating_sub(prev.end_ms);
        let next_len = cur_text.chars().count() + next.text.chars().count();
        let should_break = gap >= gap_threshold_ms || next_len >= max_chars;

        if should_break {
            out.push(Paragraph {
                start_ms: cur_start,
                end_ms: cur_end,
                text: cur_text.clone(),
            });
            cur_start = next.start_ms;
            cur_end = next.end_ms;
            cur_text = next.text.clone();
        } else {
            cur_text.push_str(&next.text);
            cur_end = next.end_ms;
        }
    }
    out.push(Paragraph {
        start_ms: cur_start,
        end_ms: cur_end,
        text: cur_text,
    });
    out
}

/// v0.1.16 精度向上 ブロック3: 議事録専用の固定 LLM 後処理 system prompt (方針部)。
///
/// active profile に依存せず議事録は常にこのプロンプトで整形する (CEO 決定)。
/// `apply_llm_postprocess` が内部で日本語強制 suffix と絵文字指示を自動付与するため、
/// ここには「フィラー整形・同音異義語補正・固有名詞補正・情報を変えない」方針のみ記述する。
///
/// 固有名詞ヒント (MEETING_PROPER_NOUN_HINTS) と辞書由来の登録済用語は
/// `build_meeting_llm_prompt` で動的に末尾連結する。
pub const MEETING_LLM_PROMPT_BASE: &str = "以下は会議の文字起こしです。次の方針で整えてください。\n\
- 同音異義語の変換ミスを文脈から判断して正しい漢字に直す（例：音声入力の文脈なら「返還」ではなく「変換」）。\n\
- 固有名詞（人名・会社名・店舗名）の明らかな誤変換を文脈から正す。\n\
- 「えー」「あのー」等のフィラーや言い淀みを自然に整える。\n\
- 文意・情報は一切変えない。要約・省略・追加をしない。\n\
- 話している内容をそのまま、読みやすい日本語にするだけ。";

/// v0.1.16 精度向上 (Day 37): 固有名詞ヒント。
///
/// 実機で頻発した誤変換 (議事録→擬似録 等) を LLM に明示する。人名・社名などの固有名詞は
/// 利用者が辞書に登録し、「登録済み用語」ブロックとして渡す (コードに埋め込まない)。
/// 「文脈に合致する場合のみ直す」と明記し、一般的な意味で使われている語まで変えないよう抑制する。
pub const MEETING_PROPER_NOUN_HINTS: &str = "\n\nこの会議に登場する固有名詞です。文脈に合致する場合のみ正しい表記に直してください（一般的な意味で使われている場合は変えないこと）:\n\
- 議事録（「擬似録」と誤りやすい）\n\
- RushVoice / Whisper";

/// 辞書由来 登録済用語の最大注入数 (語数上限)。
pub const MEETING_GLOSSARY_HINT_MAX_WORDS: usize = 30;
/// 辞書由来 登録済用語ブロックの最大文字数 (上限・超過時は切る)。
pub const MEETING_GLOSSARY_HINT_MAX_CHARS: usize = 500;

/// v0.1.16 精度向上 (Day 37): 議事録専用 LLM プロンプトを辞書語込みで組み立てる純粋関数。
///
/// 構成: `MEETING_LLM_PROMPT_BASE` + `MEETING_PROPER_NOUN_HINTS` + (辞書登録済用語があれば)
/// 「登録済み用語: A, B, C」ブロック。
///
/// `replacements` には辞書 (GlossaryDb) の replacement 側 (正しい表記) を渡す。
/// 重複排除 (出現順保持) し、上限 `MEETING_GLOSSARY_HINT_MAX_WORDS` 語 かつ
/// 連結後の用語ブロックが `MEETING_GLOSSARY_HINT_MAX_CHARS` 字を超えないよう切る。
///
/// 戻り値 `(prompt, truncated_count)`:
///   - `prompt` = 完成プロンプト
///   - `truncated_count` = 語数 / 文字数上限により**捨てた**用語の件数 (呼出側で log 出力する。
///     無言の切り捨て禁止)。
///
/// 純粋関数 (I/O・ログ無し・L294)。空文字・空白のみの replacement は除外する。
pub fn build_meeting_llm_prompt(replacements: &[String]) -> (String, usize) {
    // 重複排除 (出現順保持) + 空白除外。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    for r in replacements {
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
    let mut block_len = 0usize; // 用語ブロック (区切り含む) の概算文字数
    for w in unique {
        if included.len() >= MEETING_GLOSSARY_HINT_MAX_WORDS {
            break;
        }
        // 追加した場合の概算長 (語長 + ", " 2 字)。
        let add = w.chars().count() + 2;
        if block_len + add > MEETING_GLOSSARY_HINT_MAX_CHARS {
            break;
        }
        block_len += add;
        included.push(w);
    }
    let truncated = total.saturating_sub(included.len());

    let mut prompt = String::with_capacity(
        MEETING_LLM_PROMPT_BASE.len() + MEETING_PROPER_NOUN_HINTS.len() + block_len + 32,
    );
    prompt.push_str(MEETING_LLM_PROMPT_BASE);
    prompt.push_str(MEETING_PROPER_NOUN_HINTS);
    if !included.is_empty() {
        prompt.push_str("\n\n登録済み用語: ");
        prompt.push_str(&included.join(", "));
    }
    (prompt, truncated)
}

/// v0.1.16 精度向上 ブロック3: segment 群を段落グループに分ける純粋関数。
///
/// `split_into_paragraphs` の本体 (分割基準: ギャップ gap_threshold_ms / 最大 max_chars 字) を
/// **そのまま再利用**し (本体不変・L294)、LLM 後処理の入力単位 (段落 + 先頭時刻) を得る。
///
/// 戻り値の各 `Paragraph` は:
///   - `start_ms` = その段落の先頭 segment の start_ms (時刻継承)
///   - `end_ms`   = その段落の末尾 segment の end_ms
///   - `text`     = 段落内テキスト連結
///
/// 空入力では空配列。段落数が LLM 呼び出し回数になる。
pub fn group_segments_into_paragraphs(
    segments: &[MeetingSegment],
    gap_threshold_ms: u64,
    max_chars: usize,
) -> Vec<Paragraph> {
    split_into_paragraphs(segments, gap_threshold_ms, max_chars)
}

/// v0.1.16 精度向上 ブロック3 (M5 最重要): LLM 整形結果を段落にマージする純粋関数。
///
/// `originals` (辞書適用済 + 段落化済) と、各段落に対応する LLM 整形結果 `refined` を受け、
/// **段落単位のフォールバック**を機械的に決定する:
///   - `refined[i]` が `Some(t)` かつ trim 後非空 → その段落だけ LLM 整形テキストを採用
///   - `refined[i]` が `None` または空 → originals[i] の辞書適用済テキストにフォールバック
///
/// 時刻 (start_ms / end_ms) は常に originals 側を維持する (LLM は文字列のみ整形)。
///
/// **不変条件**: 戻り値の長さは必ず `originals.len()` と一致する
/// (どの段落も消えない = 録音データを失わない)。`refined` が originals より短くても
/// 足りない分は自動でフォールバック。これが「全体失敗でも Completed 到達」の核。
pub fn merge_refined_paragraphs(
    originals: &[Paragraph],
    refined: &[Option<String>],
) -> Vec<Paragraph> {
    originals
        .iter()
        .enumerate()
        .map(|(i, orig)| {
            let text = match refined.get(i) {
                Some(Some(t)) if !t.trim().is_empty() => t.trim().to_string(),
                _ => orig.text.clone(),
            };
            Paragraph {
                start_ms: orig.start_ms,
                end_ms: orig.end_ms,
                text,
            }
        })
        .collect()
}

/// hh:mm:ss 形式の時刻文字列。
fn format_timestamp(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// SRT 形式の時刻文字列 (hh:mm:ss,SSS)。
fn format_srt_timestamp(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let mss = ms % 1000;
    format!("{:02}:{:02}:{:02},{:03}", h, m, s, mss)
}

/// txt: 段落ごとに「[hh:mm:ss] 本文」改行 2 つ区切り。
pub fn format_txt(paragraphs: &[Paragraph]) -> String {
    let mut out = String::new();
    for p in paragraphs {
        out.push_str(&format!(
            "[{}] {}\n\n",
            format_timestamp(p.start_ms),
            p.text
        ));
    }
    out
}

/// md: タイトル付き + 段落ごとに見出しなしで時刻付き。
pub fn format_md(paragraphs: &[Paragraph], title: &str) -> String {
    let mut out = String::new();
    let title_line = if title.trim().is_empty() {
        "# 議事録".to_string()
    } else {
        format!("# {}", title.trim())
    };
    out.push_str(&title_line);
    out.push_str("\n\n");
    for p in paragraphs {
        out.push_str(&format!(
            "**[{}]** {}\n\n",
            format_timestamp(p.start_ms),
            p.text
        ));
    }
    out
}

/// srt: 字幕形式。SRT は segment 単位 (段落ではなく) で生成 (字幕仕様慣習)。
/// segments が空なら空文字列を返す (呼出側で「SRT 生成失敗」のフォールバック扱い)。
pub fn format_srt(segs: &[MeetingSegment]) -> String {
    let mut out = String::new();
    for (i, s) in segs.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_srt_timestamp(s.start_ms),
            format_srt_timestamp(s.end_ms),
            s.text
        ));
    }
    out
}

/// Day 39 (2026-06-04) リピート除去フィルタ (L340 対策・純粋関数)。
///
/// Whisper が無音 / 極小入力に対し同一フレーズを大量にループ生成する事案
/// (Day 33-38 で観測: mean=0.005 以下で「ご視聴ありがとうございました」200 回ループ等)
/// の対策。議事録モード `transcribe_chunk` 完了後に各チャンクの segments に適用する想定。
///
/// アルゴリズム:
///   1. 隣接する同一テキスト (trim 後比較) を連続カウントしてラン (run) を作る
///   2. ラン長 `>= n` なら最初の 1 件だけ残し、それ以降を drop
///   3. drop 件数 (圧縮された個数) を返す (`>0` なら呼出側で警告 emit)
///
/// 設計判断:
///   - text の trim 比較で whitespace のみ違いを同一視 (Whisper 出力は前後空白あり)
///   - ラン境界を独立判定するので、ABABAB のようなパターンは各 run=1 で誤発火しない
///   - n=0/1 は no-op (誤発火防止・ガード)
///   - chunk_index / start_ms / end_ms は最初の 1 件のものを残す
///     (run 全体の時間範囲を残すかは呼出側の判断・本関数は最小実装)
///
/// 引数:
///   - segments: 入力セグメント列 (parse_segments / apply_global_offset 適用後想定)
///   - n: 圧縮閾値 (>=2)。1 以下は no-op
///
/// 戻り値: `(圧縮後セグメント列, drop 件数)`
pub fn filter_repeat_segments(
    segments: &[MeetingSegment],
    n: usize,
) -> (Vec<MeetingSegment>, usize) {
    if segments.is_empty() || n < 2 {
        return (segments.to_vec(), 0);
    }
    let mut result = Vec::with_capacity(segments.len());
    let mut dropped: usize = 0;
    let mut i = 0;
    while i < segments.len() {
        let current_text = segments[i].text.trim();
        let mut run_end = i + 1;
        while run_end < segments.len() && segments[run_end].text.trim() == current_text {
            run_end += 1;
        }
        let run_length = run_end - i;
        if run_length >= n {
            // 圧縮: 最初の 1 件だけ残す
            result.push(segments[i].clone());
            dropped += run_length - 1;
        } else {
            // 通常: 全件残す
            for s in segments.iter().take(run_end).skip(i) {
                result.push(s.clone());
            }
        }
        i = run_end;
    }
    (result, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body(segments_json: &str) -> String {
        format!(r#"{{"text":"dummy","segments":{}}}"#, segments_json)
    }

    #[test]
    fn parse_segments_basic() {
        let body = sample_body(
            r#"[
            {"id":0,"start":0.0,"end":2.5,"text":"こんにちは"},
            {"id":1,"start":2.5,"end":5.0,"text":"よろしく"}
        ]"#,
        );
        let segs = parse_segments(&body, 0).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].start_ms, 0);
        assert_eq!(segs[0].end_ms, 2500);
        assert_eq!(segs[0].text, "こんにちは");
        assert_eq!(segs[0].chunk_index, 0);
        assert_eq!(segs[1].chunk_index, 0);
    }

    #[test]
    fn parse_segments_skips_empty_text() {
        let body = sample_body(
            r#"[
            {"id":0,"start":0.0,"end":1.0,"text":"   "},
            {"id":1,"start":1.0,"end":2.0,"text":"hello"}
        ]"#,
        );
        let segs = parse_segments(&body, 0).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "hello");
    }

    #[test]
    fn parse_segments_missing_key_returns_err() {
        let body = r#"{"text":"dummy"}"#;
        let err = parse_segments(body, 0).unwrap_err();
        assert!(
            matches!(err, MeetingError::SegmentsMissing),
            "unexpected: {:?}",
            err
        );
    }

    #[test]
    fn parse_segments_empty_array_returns_empty_vec() {
        let body = sample_body("[]");
        let segs = parse_segments(&body, 0).unwrap();
        assert!(segs.is_empty());
    }

    #[test]
    fn parse_segments_propagates_chunk_index() {
        let body = sample_body(r#"[{"id":0,"start":0.0,"end":1.0,"text":"x"}]"#);
        let segs = parse_segments(&body, 7).unwrap();
        assert_eq!(segs[0].chunk_index, 7);
    }

    #[test]
    fn apply_global_offset_chunk_zero_is_identity() {
        let segs = vec![MeetingSegment {
            chunk_index: 0,
            start_ms: 100,
            end_ms: 500,
            text: "a".into(),
        }];
        let out = apply_global_offset(segs.clone(), 0, 300);
        assert_eq!(out, segs);
    }

    #[test]
    fn apply_global_offset_adds_300sec_per_chunk() {
        let segs = vec![MeetingSegment {
            chunk_index: 2,
            start_ms: 100,
            end_ms: 500,
            text: "a".into(),
        }];
        let out = apply_global_offset(segs, 2, 300);
        assert_eq!(out[0].start_ms, 2 * 300 * 1000 + 100);
        assert_eq!(out[0].end_ms, 2 * 300 * 1000 + 500);
    }

    #[test]
    fn split_into_paragraphs_empty_returns_empty() {
        let out = split_into_paragraphs(&[], 800, 200);
        assert!(out.is_empty());
    }

    #[test]
    fn split_into_paragraphs_single_segment() {
        let segs = vec![MeetingSegment {
            chunk_index: 0,
            start_ms: 0,
            end_ms: 1000,
            text: "ab".into(),
        }];
        let out = split_into_paragraphs(&segs, 800, 200);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "ab");
    }

    #[test]
    fn split_into_paragraphs_breaks_on_gap() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 0,
                end_ms: 1000,
                text: "a".into(),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 2000,
                end_ms: 3000,
                text: "b".into(),
            }, // gap = 1000ms >= 800
        ];
        let out = split_into_paragraphs(&segs, 800, 200);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "a");
        assert_eq!(out[1].text, "b");
    }

    #[test]
    fn split_into_paragraphs_breaks_on_max_chars() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 0,
                end_ms: 100,
                text: "あ".repeat(150),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 200,
                end_ms: 300,
                text: "い".repeat(60),
            },
        ];
        let out = split_into_paragraphs(&segs, 800, 200);
        assert!(
            out.len() >= 2,
            "字数超過で改段する: {:?}",
            out.iter().map(|p| p.text.chars().count()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn split_into_paragraphs_no_break_within_threshold() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 0,
                end_ms: 1000,
                text: "a".into(),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 1100,
                end_ms: 2000,
                text: "b".into(),
            }, // gap = 100ms < 800
        ];
        let out = split_into_paragraphs(&segs, 800, 200);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "ab");
        assert_eq!(out[0].start_ms, 0);
        assert_eq!(out[0].end_ms, 2000);
    }

    #[test]
    fn format_timestamp_hms() {
        assert_eq!(format_timestamp(0), "00:00:00");
        assert_eq!(format_timestamp(3_661_000), "01:01:01");
        assert_eq!(format_timestamp(92_000), "00:01:32");
    }

    #[test]
    fn format_srt_timestamp_with_millis() {
        assert_eq!(format_srt_timestamp(3_661_500), "01:01:01,500");
        assert_eq!(format_srt_timestamp(0), "00:00:00,000");
    }

    #[test]
    fn format_txt_basic() {
        let ps = vec![Paragraph {
            start_ms: 92_000,
            end_ms: 95_000,
            text: "本文".into(),
        }];
        let s = format_txt(&ps);
        assert!(s.contains("[00:01:32]"));
        assert!(s.contains("本文"));
    }

    #[test]
    fn format_md_uses_title() {
        let ps = vec![Paragraph {
            start_ms: 0,
            end_ms: 1000,
            text: "ab".into(),
        }];
        let s = format_md(&ps, "ミーティング 5/31");
        assert!(s.starts_with("# ミーティング 5/31"));
        assert!(s.contains("**[00:00:00]**"));
    }

    #[test]
    fn format_md_empty_title_falls_back() {
        let ps = vec![Paragraph {
            start_ms: 0,
            end_ms: 1000,
            text: "ab".into(),
        }];
        let s = format_md(&ps, "   ");
        assert!(s.starts_with("# 議事録"));
    }

    #[test]
    fn format_srt_numbered_segments() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 0,
                end_ms: 1500,
                text: "一".into(),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 1500,
                end_ms: 3000,
                text: "二".into(),
            },
        ];
        let s = format_srt(&segs);
        assert!(s.contains("1\n00:00:00,000 --> 00:00:01,500\n一"));
        assert!(s.contains("2\n00:00:01,500 --> 00:00:03,000\n二"));
    }

    #[test]
    fn format_srt_empty_returns_empty_string() {
        let s = format_srt(&[]);
        assert!(s.is_empty());
    }

    // ---------------- v0.1.16 精度向上 ブロック3: group_segments_into_paragraphs ----------------

    #[test]
    fn group_segments_into_paragraphs_empty_returns_empty() {
        let out = group_segments_into_paragraphs(&[], 800, 200);
        assert!(out.is_empty());
    }

    #[test]
    fn group_segments_into_paragraphs_single_paragraph_inherits_first_start() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 5000,
                end_ms: 6000,
                text: "あ".into(),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 6100,
                end_ms: 7000,
                text: "い".into(),
            }, // gap=100ms < 800 → 同段落
        ];
        let out = group_segments_into_paragraphs(&segs, 800, 200);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_ms, 5000, "先頭 segment の start を継承");
        assert_eq!(out[0].end_ms, 7000);
        assert_eq!(out[0].text, "あい");
    }

    #[test]
    fn group_segments_into_paragraphs_multiple_on_gap() {
        let segs = vec![
            MeetingSegment {
                chunk_index: 0,
                start_ms: 0,
                end_ms: 1000,
                text: "前半".into(),
            },
            MeetingSegment {
                chunk_index: 0,
                start_ms: 3000,
                end_ms: 4000,
                text: "後半".into(),
            }, // gap=2000ms >= 800 → 改段
        ];
        let out = group_segments_into_paragraphs(&segs, 800, 200);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "前半");
        assert_eq!(out[1].text, "後半");
        assert_eq!(out[1].start_ms, 3000, "2 段落目は次 segment の start を継承");
    }

    // ---------------- v0.1.16 精度向上 ブロック3 (M5): merge_refined_paragraphs ----------------

    fn para(start: u64, end: u64, text: &str) -> Paragraph {
        Paragraph {
            start_ms: start,
            end_ms: end,
            text: text.to_string(),
        }
    }

    #[test]
    fn merge_refined_uses_llm_text_when_present() {
        let originals = vec![para(0, 1000, "辞書版テキスト")];
        let refined = vec![Some("LLM整形テキスト".to_string())];
        let out = merge_refined_paragraphs(&originals, &refined);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "LLM整形テキスト");
        assert_eq!(out[0].start_ms, 0, "時刻は originals 維持");
        assert_eq!(out[0].end_ms, 1000);
    }

    #[test]
    fn merge_refined_falls_back_on_none() {
        let originals = vec![para(0, 1000, "辞書版A"), para(2000, 3000, "辞書版B")];
        // 1 段落目は成功、2 段落目は LLM 失敗 (None)
        let refined = vec![Some("LLM版A".to_string()), None];
        let out = merge_refined_paragraphs(&originals, &refined);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "LLM版A");
        assert_eq!(out[1].text, "辞書版B", "None はフォールバック");
    }

    #[test]
    fn merge_refined_falls_back_on_empty_string() {
        let originals = vec![para(0, 1000, "辞書版")];
        let refined = vec![Some("   ".to_string())]; // 空白のみ → フォールバック
        let out = merge_refined_paragraphs(&originals, &refined);
        assert_eq!(out[0].text, "辞書版");
    }

    #[test]
    fn merge_refined_shorter_refined_does_not_drop_paragraphs() {
        // refined が originals より短くても (全体失敗で空 Vec 等)、段落は消えない
        let originals = vec![para(0, 1000, "一"), para(2000, 3000, "二"), para(4000, 5000, "三")];
        let refined: Vec<Option<String>> = vec![]; // LLM 全体失敗を模擬
        let out = merge_refined_paragraphs(&originals, &refined);
        assert_eq!(out.len(), 3, "全体失敗でも段落は失われない (Completed 到達保証)");
        assert_eq!(out[0].text, "一");
        assert_eq!(out[1].text, "二");
        assert_eq!(out[2].text, "三");
    }

    #[test]
    fn merge_refined_empty_originals_returns_empty() {
        let out = merge_refined_paragraphs(&[], &[Some("x".to_string())]);
        assert!(out.is_empty());
    }

    // ---------------- v0.1.16 Day 37: build_meeting_llm_prompt ----------------

    #[test]
    fn build_prompt_includes_base_and_proper_noun_hints() {
        let (p, truncated) = build_meeting_llm_prompt(&[]);
        assert_eq!(truncated, 0);
        // 方針部 + 固有名詞ヒントが必ず含まれる
        assert!(p.contains("以下は会議の文字起こしです"));
        assert!(p.contains("擬似録"));
        assert!(p.contains("RushVoice / Whisper"));
        // 辞書語が無ければ「登録済み用語:」ブロックは付かない
        assert!(!p.contains("登録済み用語:"));
    }

    #[test]
    fn build_prompt_appends_dedup_glossary_terms() {
        let reps = vec![
            "RushVoice".to_string(),
            "LashRush".to_string(),
            "RushVoice".to_string(), // 重複 → 1 回だけ
            "  ".to_string(),        // 空白のみ → 除外
            "".to_string(),          // 空 → 除外
        ];
        let (p, truncated) = build_meeting_llm_prompt(&reps);
        assert_eq!(truncated, 0);
        assert!(p.contains("登録済み用語: RushVoice, LashRush"));
        // 重複は 1 回だけ (2 回目の RushVoice が無いこと)
        let occurrences = p.matches("RushVoice").count();
        // 固有名詞ヒントにも "RushVoice" が 1 回あるため、ヒント分 1 + 用語ブロック 1 = 2
        assert_eq!(occurrences, 2, "用語ブロックでは重複排除される");
    }

    #[test]
    fn build_prompt_truncates_by_word_count() {
        // 上限 30 語を超える短い語を 35 個渡す → 5 語切られる
        let reps: Vec<String> = (0..35).map(|i| format!("語{}", i)).collect();
        let (p, truncated) = build_meeting_llm_prompt(&reps);
        assert_eq!(truncated, 5, "30 語超過分 5 語が切られる");
        assert!(p.contains("登録済み用語:"));
        // 含まれる語は 30 語 (語0..語29)
        assert!(p.contains("語0"));
        assert!(p.contains("語29"));
        assert!(!p.contains("語30"));
    }

    #[test]
    fn build_prompt_truncates_by_char_limit() {
        // 各語が長い (100 字 + 連番で一意) → 文字数上限 500 で数語しか入らず残りは切られる。
        let reps: Vec<String> = (0..10)
            .map(|i| format!("{}{}", "い".repeat(100), i))
            .collect();
        let (_p, truncated) = build_meeting_llm_prompt(&reps);
        assert!(truncated > 0, "文字数上限で一部が切られる");
    }

    // -----------------------------------------------------------------------
    // Day 39 (2026-06-04) filter_repeat_segments テスト (L340 リピート除去フィルタ)
    // -----------------------------------------------------------------------

    fn seg(text: &str, start_ms: u64, end_ms: u64) -> MeetingSegment {
        MeetingSegment {
            chunk_index: 0,
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    #[test]
    fn filter_repeat_empty_input_returns_empty() {
        let (out, dropped) = filter_repeat_segments(&[], 5);
        assert!(out.is_empty());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_repeat_single_segment_unchanged() {
        let segs = vec![seg("一回だけ", 0, 1000)];
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_repeat_all_different_unchanged() {
        let segs = vec![
            seg("aaa", 0, 1000),
            seg("bbb", 1000, 2000),
            seg("ccc", 2000, 3000),
        ];
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_repeat_four_consecutive_below_n5_unchanged() {
        // N=5 で 4 連続 → 圧縮しない (閾値未満)
        let segs = vec![
            seg("同じ", 0, 1000),
            seg("同じ", 1000, 2000),
            seg("同じ", 2000, 3000),
            seg("同じ", 3000, 4000),
        ];
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 4, "4 件 < N=5 で圧縮しない");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_repeat_five_consecutive_compressed_to_one() {
        // N=5 で 5 連続 → 1 件に圧縮・dropped=4
        let segs: Vec<MeetingSegment> = (0..5)
            .map(|i| seg("ご視聴ありがとうございました", i * 1000, (i + 1) * 1000))
            .collect();
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(dropped, 4);
        assert_eq!(out[0].text, "ご視聴ありがとうございました");
        assert_eq!(out[0].start_ms, 0, "最初の 1 件の start_ms が残る");
    }

    #[test]
    fn filter_repeat_two_hundred_consecutive_compressed() {
        // 200 連続 → 1 件 + dropped=199 (L340 で実際に起きた事案規模)
        let segs: Vec<MeetingSegment> = (0..200)
            .map(|i| seg("ループ句", i * 1000, (i + 1) * 1000))
            .collect();
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(dropped, 199);
    }

    #[test]
    fn filter_repeat_mixed_pattern_each_run_independent() {
        // AAAAA BBB AAAAA → run1 圧縮 + BBB そのまま + run3 圧縮
        let mut segs = Vec::new();
        for i in 0..5 {
            segs.push(seg("A", i * 1000, (i + 1) * 1000));
        }
        for i in 5..8 {
            segs.push(seg("B", i * 1000, (i + 1) * 1000));
        }
        for i in 8..13 {
            segs.push(seg("A", i * 1000, (i + 1) * 1000));
        }
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        // run1=A 圧縮(1件・4 drop), B*3 そのまま(3件・0 drop), run3=A 圧縮(1件・4 drop)
        assert_eq!(out.len(), 1 + 3 + 1);
        assert_eq!(dropped, 4 + 4);
        assert_eq!(out[0].text, "A");
        assert_eq!(out[1].text, "B");
        assert_eq!(out[4].text, "A");
    }

    #[test]
    fn filter_repeat_whitespace_only_difference_treated_same() {
        // 前後空白だけ違いを同一視 (trim 比較)
        let segs = vec![
            seg("こんにちは", 0, 1000),
            seg(" こんにちは ", 1000, 2000),
            seg("こんにちは", 2000, 3000),
            seg("\tこんにちは\n", 3000, 4000),
            seg("こんにちは", 4000, 5000),
        ];
        let (out, dropped) = filter_repeat_segments(&segs, 5);
        assert_eq!(out.len(), 1, "trim 後同一 5 連続 → 1 件圧縮");
        assert_eq!(dropped, 4);
    }

    #[test]
    fn filter_repeat_n_zero_no_op() {
        let segs: Vec<MeetingSegment> = (0..10)
            .map(|i| seg("X", i * 1000, (i + 1) * 1000))
            .collect();
        let (out, dropped) = filter_repeat_segments(&segs, 0);
        assert_eq!(out.len(), 10, "n=0 は no-op (誤発火防止)");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn filter_repeat_n_one_no_op() {
        let segs: Vec<MeetingSegment> = (0..10)
            .map(|i| seg("X", i * 1000, (i + 1) * 1000))
            .collect();
        let (out, dropped) = filter_repeat_segments(&segs, 1);
        assert_eq!(out.len(), 10, "n=1 は no-op (全 1 件 run が圧縮対象になる暴発を防止)");
        assert_eq!(dropped, 0);
    }
}
