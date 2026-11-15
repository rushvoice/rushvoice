// 話し終わりのあとの「幻聴」対策 (2026-11-14)
//
// 実例 (history.db id=8127・Groq whisper-large-v3-turbo・30.3 秒):
//   「…望ましいよね店船の中心を見ると、 コーティングについて、 コーティングについて、
//     コーティングについて、 コーティングを作る。」
//   「店船の中心を見ると」以降は話していない。halu.rs は segments[0] の値と全文の既知句しか
//   見ないため、末尾にだけ付いた繰り返しは素通りしていた。録音は内蔵マイクへの切替えで
//   入力が小さく、送信前に 4.8 倍へ増幅されていた = 話し終わりの環境雑音も一緒に増幅されて
//   Whisper へ届いていた。
//
// 3 段で防ぐ (どれも「末尾」か「明らかな繰り返し」だけに効かせ、正常な長文は削らない):
//   1. trailing_trim_len   : 送信前に、最後の発話から一定時間より後ろの無音・雑音を切る
//   2. drop_suspicious_tail_segments : 末尾のセグメントが Whisper 公式の失敗の目安
//      (圧縮率 > 2.4 / 平均対数確率 < -1.0 / 無音確率 > 0.6) に当たれば、その分を捨てる
//   3. collapse_repeated_phrases : 同じ句が続けて何度も出たら 1 回にする
//
// 目安の出典: openai/whisper transcribe() の既定値 (compression_ratio_threshold=2.4,
// logprob_threshold=-1.0, no_speech_threshold=0.6)。Groq の API はこれらを引数で受けないため、
// 返ってきた verbose_json の値でこちら側が判定する。末尾無音の切り落としは faster-whisper の
// VAD 前処理と同じ考え方を、依存を増やさずエネルギーで行う。

/// 末尾無音判定のフレーム長 (ms)。
const FRAME_MS: usize = 30;
/// 雑音の床 (フレーム RMS の下位 10%) の何倍を「発話」とみなすか。
const SPEECH_OVER_FLOOR: f32 = 3.0;
/// 発話とみなすのに必要な連続フレーム数 (= 90ms)。単発のクリック音で末尾が延びないようにする。
const MIN_SPEECH_RUN_FRAMES: usize = 3;
/// 最後の発話のあとに残す余白 (ms)。語尾の弱い音を切らないための余裕。
const HANGOVER_MS: usize = 800;
/// これ未満しか削れないなら切らない (ms)。
const MIN_TRIM_MS: usize = 1000;

/// 送信する音声の長さ (サンプル数) を返す。末尾に長い無音・雑音があれば、
/// 最後の発話 + `HANGOVER_MS` までに縮める。判断できないときは元の長さのまま。
///
/// 判定は増幅前の音声で行うこと (増幅は倍率が全体に掛かるだけなので結果は同じだが、
/// 呼び出し側の約束として増幅前に揃える)。
pub fn trailing_trim_len(samples: &[f32], sample_rate: usize) -> usize {
    let len = samples.len();
    let frame = sample_rate * FRAME_MS / 1000;
    if frame == 0 || len < frame * 10 {
        return len;
    }
    let rms: Vec<f32> = samples
        .chunks(frame)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[sorted.len() / 10].max(1e-5);
    let threshold = floor * SPEECH_OVER_FLOOR;

    // 後ろから見て、MIN_SPEECH_RUN_FRAMES 連続で閾値を超えた最後の位置を探す
    let mut run = 0usize;
    let mut last_speech_frame: Option<usize> = None;
    for idx in (0..rms.len()).rev() {
        if rms[idx] > threshold {
            run += 1;
            if run >= MIN_SPEECH_RUN_FRAMES {
                // run は idx から後ろ向きに数えたので、発話の終わりは idx + run - 1
                last_speech_frame = Some(idx + run - 1);
                break;
            }
        } else {
            run = 0;
        }
    }
    let Some(last) = last_speech_frame else {
        return len; // 発話が見つからない = 無音判定は halu 側に任せる
    };
    let end = ((last + 1) * frame + sample_rate * HANGOVER_MS / 1000).min(len);
    if len - end < sample_rate * MIN_TRIM_MS / 1000 {
        return len;
    }
    end
}

/// Whisper 公式の「この区間は失敗」の目安。
const COMPRESSION_RATIO_MAX: f64 = 2.4;
const AVG_LOGPROB_MIN: f64 = -1.0;
const NO_SPEECH_PROB_MAX: f64 = 0.6;

fn segment_is_suspicious(seg: &serde_json::Value) -> bool {
    let f = |k: &str| seg.get(k).and_then(|v| v.as_f64());
    f("compression_ratio").map_or(false, |v| v > COMPRESSION_RATIO_MAX)
        || f("avg_logprob").map_or(false, |v| v < AVG_LOGPROB_MIN)
        || f("no_speech_prob").map_or(false, |v| v > NO_SPEECH_PROB_MAX)
}

/// 末尾の怪しいセグメントを本文から外す。先頭のセグメントは必ず残す
/// (全体が怪しいときの扱いは halu.rs の役目)。
/// 戻り値: (新しい本文, 外したセグメント数)。外せないときは元の本文と 0。
pub fn drop_suspicious_tail_segments(text: &str, parsed: &serde_json::Value) -> (String, usize) {
    let Some(segs) = parsed.get("segments").and_then(|v| v.as_array()) else {
        return (text.to_string(), 0);
    };
    let mut out = text.trim_end().to_string();
    let mut dropped = 0usize;
    for seg in segs.iter().skip(1).rev() {
        if !segment_is_suspicious(seg) {
            break;
        }
        let seg_text = seg.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
        if seg_text.is_empty() {
            dropped += 1;
            continue;
        }
        if !out.ends_with(seg_text) {
            break; // 本文と区間が対応しない (後処理済み等) なら触らない
        }
        out.truncate(out.len() - seg_text.len());
        out = out.trim_end().to_string();
        dropped += 1;
    }
    if out.trim().is_empty() {
        return (text.to_string(), 0);
    }
    (out, dropped)
}

/// 句を何文字まで見るか。
const MAX_UNIT_CHARS: usize = 40;

/// 同じ句が続けて繰り返されたら 1 回にする。句と句の間の空白の有無は問わない。
///   - 句に文字 (かな・漢字・英数) が 4 字以上 → 3 回以上で 1 回にする
///   - 2〜3 字の短い句 (「いや」「どん」等) → 話し言葉で普通に重なる (実データに
///     「どんどんどんどん」= 4 回あり) ので 6 回以上のときだけ
///   - 1 字の繰り返し (「ーーー」「ははは」) は触らない
/// 戻り値: (新しい本文, 取り除いた繰り返しの回数)
pub fn collapse_repeated_phrases(text: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut removed = 0usize;
    let mut i = 0usize;
    'outer: while i < n {
        if !chars[i].is_whitespace() {
            for len in 2..=MAX_UNIT_CHARS {
                if i + len * 3 > n {
                    break;
                }
                let unit = &chars[i..i + len];
                if unit[len - 1].is_whitespace() {
                    continue;
                }
                let letters = unit.iter().filter(|c| c.is_alphanumeric()).count();
                if letters < 2 {
                    continue;
                }
                let need = if letters >= 4 { 3 } else { 6 };
                // 繰り返し回数を数える (間の空白は飛ばす)
                let mut reps = 1usize;
                let mut end = i + len; // 最後に一致した句の直後
                loop {
                    let mut p = end;
                    while p < n && chars[p].is_whitespace() {
                        p += 1;
                    }
                    if p + len <= n && chars[p..p + len] == *unit {
                        reps += 1;
                        end = p + len;
                    } else {
                        break;
                    }
                }
                if reps >= need {
                    out.extend(unit.iter());
                    removed += reps - 1;
                    i = end;
                    continue 'outer;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "ナーチャリングをステップ配信していくみたいなのが望ましいよね店船の中心を見ると、 コーティングについて、 コーティングについて、 コーティングについて、 コーティングを作る。";

    #[test]
    fn collapses_real_case_8127() {
        let (out, removed) = collapse_repeated_phrases(REAL);
        assert_eq!(removed, 2);
        assert_eq!(
            out,
            "ナーチャリングをステップ配信していくみたいなのが望ましいよね店船の中心を見ると、 コーティングについて、 コーティングを作る。"
        );
    }

    #[test]
    fn collapses_without_spaces_and_at_end() {
        let (out, removed) = collapse_repeated_phrases("本文です。ありがとうございます。ありがとうございます。ありがとうございます。");
        assert_eq!(removed, 2);
        assert_eq!(out, "本文です。ありがとうございます。");
    }

    #[test]
    fn keeps_normal_speech_and_short_doubling() {
        for s in [
            "いやいやいや、それは違うと思う",
            "それらをどんどんどんどん確認して確定して",
            "はい、はい、はい、はい、了解",
            "どんどん進めてください。はい、はい。",
            "Aの店舗とBの店舗とCの店舗を比べる",
            "ーーーーー",
            "123123",
        ] {
            assert_eq!(collapse_repeated_phrases(s), (s.to_string(), 0), "{s}");
        }
    }

    #[test]
    fn keeps_twice_repeated_long_phrase() {
        // 2 回までの言い直しは話者の意図の可能性があるので残す
        let s = "確認してください、確認してください。";
        assert_eq!(collapse_repeated_phrases(s).1, 0);
    }

    #[test]
    fn collapses_short_unit_only_from_six() {
        let (out, removed) = collapse_repeated_phrases("はい、はい、はい、はい、はい、はい、了解");
        assert_eq!(removed, 5);
        assert_eq!(out, "はい、了解");
    }

    fn seg(text: &str, cr: f64, lp: f64, ns: f64) -> serde_json::Value {
        serde_json::json!({"text": text, "compression_ratio": cr, "avg_logprob": lp, "no_speech_prob": ns})
    }

    #[test]
    fn drops_only_suspicious_tail_segments() {
        let parsed = serde_json::json!({"segments": [
            seg(" 前半の話です。", 1.2, -0.2, 0.0),
            seg(" 後半の話です。", 1.3, -0.3, 0.01),
            seg(" コーティングについて、コーティングについて、", 3.1, -0.4, 0.0),
        ]});
        let text = "前半の話です。 後半の話です。 コーティングについて、コーティングについて、";
        let (out, d) = drop_suspicious_tail_segments(text, &parsed);
        assert_eq!(d, 1);
        assert_eq!(out, "前半の話です。 後半の話です。");
    }

    #[test]
    fn keeps_text_when_tail_is_fine_or_first_only() {
        let ok = serde_json::json!({"segments": [seg("a", 1.0, -0.2, 0.0), seg("b", 1.1, -0.3, 0.0)]});
        assert_eq!(drop_suspicious_tail_segments("a b", &ok), ("a b".into(), 0));
        let one = serde_json::json!({"segments": [seg("a", 5.0, -2.0, 0.9)]});
        assert_eq!(drop_suspicious_tail_segments("a", &one), ("a".into(), 0));
        let none = serde_json::json!({"text": "x"});
        assert_eq!(drop_suspicious_tail_segments("x", &none), ("x".into(), 0));
    }

    #[test]
    fn mid_suspicious_segment_is_kept() {
        // 途中の区間は判定しない (末尾から連続している分だけ)
        let parsed = serde_json::json!({"segments": [
            seg("a", 1.0, -0.2, 0.0), seg("b", 3.0, -0.2, 0.0), seg("c", 1.0, -0.2, 0.0),
        ]});
        assert_eq!(drop_suspicious_tail_segments("a b c", &parsed), ("a b c".into(), 0));
    }

    // ---- 末尾無音の切り落とし ----

    const SR: usize = 16_000;

    /// 擬似音声: 発話は 200Hz 正弦 + 揺らぎ、雑音は小さな擬似乱数
    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((*seed >> 16) as f32 / 32768.0 - 1.0) * amp
            })
            .collect()
    }
    fn speech(n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|k| (k as f32 * 2.0 * std::f32::consts::PI * 200.0 / SR as f32).sin() * amp)
            .collect()
    }

    #[test]
    fn trims_long_noisy_tail() {
        let mut seed = 1;
        let mut s = Vec::new();
        // 話す 20 秒 (途中に 0.4 秒の間を何度も挟む) → 6 秒の雑音
        for _ in 0..20 {
            s.extend(speech(SR * 6 / 10, 0.02));
            s.extend(noise(SR * 4 / 10, 0.003, &mut seed));
        }
        let speech_end = s.len() - SR * 4 / 10;
        s.extend(noise(SR * 6, 0.003, &mut seed));
        let end = trailing_trim_len(&s, SR);
        assert!(end < s.len(), "should trim");
        assert!(end >= speech_end + SR * 7 / 10, "keeps hangover after last speech: end={end}");
        assert!(end <= speech_end + SR * 12 / 10, "trimmed most of the tail: end={end}");
    }

    #[test]
    fn keeps_audio_with_short_tail_or_no_speech() {
        let mut seed = 7;
        let mut s = speech(SR * 5, 0.02);
        s.extend(noise(SR / 2, 0.003, &mut seed)); // 0.5 秒だけの余白
        assert_eq!(trailing_trim_len(&s, SR), s.len());
        let silent = noise(SR * 5, 0.003, &mut seed);
        // 全部雑音: 下位 10% の 3 倍を超える連続区間はまず無い → 切らない
        assert_eq!(trailing_trim_len(&silent, SR), silent.len());
        let digital_silence = vec![0.0f32; SR * 5];
        assert_eq!(trailing_trim_len(&digital_silence, SR), digital_silence.len());
    }

    #[test]
    fn keeps_speech_that_runs_to_the_end() {
        let mut seed = 3;
        let mut s = noise(SR * 3, 0.003, &mut seed);
        s.extend(speech(SR * 10, 0.02));
        assert_eq!(trailing_trim_len(&s, SR), s.len());
    }

    #[test]
    fn quiet_mic_still_works_relatively() {
        // Bluetooth ハンズフリーの小さい入力 (発話 RMS ≈ 0.001) でも相対判定で動く
        let mut seed = 11;
        let mut s = speech(SR * 8, 0.0015);
        let speech_end = s.len();
        s.extend(noise(SR * 5, 0.0002, &mut seed));
        let end = trailing_trim_len(&s, SR);
        assert!(end < s.len() && end >= speech_end + SR * 7 / 10);
    }
}
