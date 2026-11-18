// 録音の途中の長い無音・雑音を詰めてから送る (2026-11-18)
//
// 実例 (history.db id=8458・Groq whisper-large-v3-turbo・247.7 秒):
//   「えっと。ご視聴ありがとうございました。です。ラン 、 コーティング、 コーティング、
//     ソーダ、 ジョーマネギ、 骨 銃ネギ、 コイコツ、 コイコツ…ご視聴ありがとうございました。」
//   4 分のうち声らしい音は約 5% (30ms のフレームで雑音の床の 3 倍を超えた割合)。
//   普段の録音は 50〜65%。中央値の音量が雑音の床とほぼ同じ = ほとんど無音で、
//   送信前に約 4 倍へ増幅された雑音から Whisper が文を作っていた。
//   tail_guard.rs は「話し終わりのあと」しか切らないため、先頭・途中の無音には効かなかった。
//
// やること: faster-whisper の VAD 前処理 (声のある区間だけをつないで送る) と同じ考え方を、
// 依存を増やさずエネルギーで行う。判定は tail_guard と同じ物差し (雑音の床の 3 倍・90ms 連続)。
// 正常な録音を削らないように:
//   - 声の区間の前後に PAD_MS の余白を残す (語頭・語尾の弱い音を切らない)
//   - 区間どうしの間が KEEP_GAP_MS 以下なら、間もそのまま残す (普通の息継ぎ・考える間)
//   - それより長い間は JOIN_GAP_MS の無音に置き換える
//   - 削れる量が MIN_REMOVE_MS 未満なら何もしない

const FRAME_MS: usize = 30;
const SPEECH_OVER_FLOOR: f32 = 3.0;
const MIN_SPEECH_RUN_FRAMES: usize = 3;
/// 声の区間の前後に残す余白 (ms)
const PAD_MS: usize = 400;
/// これ以下の間はそのまま残す (ms)
const KEEP_GAP_MS: usize = 1500;
/// 長い間を詰めたあとに挟む無音 (ms)。区間どうしが一続きの文に化けないように少し空ける。
const JOIN_GAP_MS: usize = 500;
/// これ未満しか削れないなら送る音声を変えない (ms)
const MIN_REMOVE_MS: usize = 2000;

/// 詰めた結果。`samples` は送る音声、`kept_ms` / `removed_ms` はログ用。
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    pub samples: Vec<f32>,
    pub kept_ms: usize,
    pub removed_ms: usize,
    /// 声らしいフレームの割合 (0.0〜1.0)
    pub speech_ratio: f32,
}

/// 声のある区間 (サンプル位置の [開始, 終了)) を返す。余白込み・近い区間は結合済み。
fn speech_regions(samples: &[f32], sample_rate: usize) -> Option<(Vec<(usize, usize)>, f32)> {
    let len = samples.len();
    let frame = sample_rate * FRAME_MS / 1000;
    if frame == 0 || len < frame * 10 {
        return None;
    }
    let rms: Vec<f32> = samples
        .chunks(frame)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[sorted.len() / 10].max(1e-5);
    let threshold = floor * SPEECH_OVER_FLOOR;

    // MIN_SPEECH_RUN_FRAMES 以上続いた「声」のフレームの並びを区間にする
    let mut runs: Vec<(usize, usize)> = Vec::new(); // フレーム番号 [s, e)
    let mut start: Option<usize> = None;
    for (i, r) in rms.iter().enumerate() {
        if *r > threshold {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            if i - s >= MIN_SPEECH_RUN_FRAMES {
                runs.push((s, i));
            }
        }
    }
    if let Some(s) = start {
        if rms.len() - s >= MIN_SPEECH_RUN_FRAMES {
            runs.push((s, rms.len()));
        }
    }
    let speech_frames: usize = runs.iter().map(|(s, e)| e - s).sum();
    let speech_ratio = speech_frames as f32 / rms.len() as f32;
    if runs.is_empty() {
        return Some((Vec::new(), speech_ratio));
    }

    let pad = sample_rate * PAD_MS / 1000;
    let keep_gap = sample_rate * KEEP_GAP_MS / 1000;
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for (s, e) in runs {
        let rs = (s * frame).saturating_sub(pad);
        let re = (e * frame + pad).min(len);
        match regions.last_mut() {
            Some(last) if rs <= last.1 + keep_gap => last.1 = last.1.max(re),
            _ => regions.push((rs, re)),
        }
    }
    Some((regions, speech_ratio))
}

/// 先頭・途中・末尾の長い無音を詰めた音声を返す。詰める必要が無い・判断できないときは `None`
/// (呼び出し側は元の音声をそのまま送る)。声の区間が一つも無いときも `None`
/// (無音の扱いは halu.rs の役目)。
///
/// 判定は増幅前の音声で行うこと (tail_guard と同じ約束)。
pub fn compact_silences(samples: &[f32], sample_rate: usize) -> Option<Compacted> {
    let (regions, speech_ratio) = speech_regions(samples, sample_rate)?;
    if regions.is_empty() {
        return None;
    }
    let join_gap = sample_rate * JOIN_GAP_MS / 1000;
    let kept: usize = regions.iter().map(|(s, e)| e - s).sum::<usize>()
        + join_gap * regions.len().saturating_sub(1);
    if kept >= samples.len() || samples.len() - kept < sample_rate * MIN_REMOVE_MS / 1000 {
        return None;
    }
    let mut out = Vec::with_capacity(kept);
    for (i, (s, e)) in regions.iter().enumerate() {
        if i > 0 {
            out.extend(std::iter::repeat(0.0f32).take(join_gap));
        }
        out.extend_from_slice(&samples[*s..*e]);
    }
    let ms = |n: usize| n * 1000 / sample_rate;
    Some(Compacted {
        kept_ms: ms(out.len()),
        removed_ms: ms(samples.len() - out.len()),
        samples: out,
        speech_ratio,
    })
}

/// 声の割合 (0.0〜1.0)。録音全体から雑音の床を決めて数える (`compact_silences` と同じ物差し)。
/// 短すぎて判断できないときは `None`。
pub fn speech_ratio(samples: &[f32], sample_rate: usize) -> Option<f32> {
    speech_regions(samples, sample_rate).map(|(_, r)| r)
}

// ---------------------------------------------------------------------------
// 押し間違いの録音を見分ける (2026-11-18)
//
// 実例 id=8458: 録音を間違えて始めたまま 4 分話さず、雑音から幻の文が貼られた。
// 開発者の PC の直近 20 件の実録音 (2026-11-18) での声の割合:
//   普段の録音   : 全体 0.40〜0.63 / 録音中に測った直近 30 秒の最小 0.37
//   id=8458     : 全体 0.03〜0.05
// 余裕を見て 0.10 で切る。
// ---------------------------------------------------------------------------

/// 録音中、直近この長さの声の割合を見る (ms)
pub const IDLE_WINDOW_MS: u64 = 30_000;
/// 直近 `IDLE_WINDOW_MS` の声の割合がこれ未満なら、録音を自動で止める
pub const IDLE_SPEECH_RATIO_MAX: f32 = 0.10;
/// 録音全体の声の割合がこれ未満なら、文字にせず捨てる
pub const DISCARD_SPEECH_RATIO_MAX: f32 = 0.10;
/// これより短い録音は捨てない (短い一言は割合がぶれるため・ms)
pub const DISCARD_MIN_DURATION_MS: i64 = 5_000;

/// 押し間違いとみなして捨てるか。
pub fn should_discard_as_no_speech(speech_ratio: Option<f32>, duration_ms: i64) -> bool {
    duration_ms >= DISCARD_MIN_DURATION_MS
        && speech_ratio.is_some_and(|r| r < DISCARD_SPEECH_RATIO_MAX)
}

/// 雑音の床を録音中に見積もるための、音量 (RMS) の分布の刻み。1e-5〜1 を 1 桁 24 刻み。
const HIST_BINS: usize = 120;
const HIST_PER_DECADE: f32 = 24.0;

/// 録音中に、直近 `IDLE_WINDOW_MS` の声の割合を数える。
/// 雑音の床は「これまでのフレームの下位 10%」(録音後の判定と同じ物差し) を分布から求める。
#[derive(Debug)]
pub struct OnlineSpeechMeter {
    hist: [u32; HIST_BINS],
    count: u32,
    run_ms: u32,
    /// (フレームの長さ ms, 声か)
    window: std::collections::VecDeque<(u32, bool)>,
    window_ms: u64,
    speech_ms: u64,
    elapsed_ms: u64,
}

impl Default for OnlineSpeechMeter {
    fn default() -> Self {
        Self {
            hist: [0; HIST_BINS],
            count: 0,
            run_ms: 0,
            window: std::collections::VecDeque::new(),
            window_ms: 0,
            speech_ms: 0,
            elapsed_ms: 0,
        }
    }
}

impl OnlineSpeechMeter {
    fn bin_of(rms: f32) -> usize {
        let b = ((rms.max(1e-5).log10() + 5.0) * HIST_PER_DECADE) as isize;
        b.clamp(0, HIST_BINS as isize - 1) as usize
    }

    fn floor(&self) -> f32 {
        let target = (self.count as f32 * 0.1).ceil() as u32;
        let mut acc = 0u32;
        for (b, n) in self.hist.iter().enumerate() {
            acc += n;
            if acc >= target.max(1) {
                return 10f32.powf((b as f32 + 1.0) / HIST_PER_DECADE - 5.0);
            }
        }
        1.0
    }

    /// 1 フレーム (長さ `frame_ms`・音量 `rms`) を足す。
    pub fn push(&mut self, rms: f32, frame_ms: u32) {
        if frame_ms == 0 {
            return;
        }
        self.hist[Self::bin_of(rms)] += 1;
        self.count += 1;
        let threshold = (self.floor() * SPEECH_OVER_FLOOR).max(1e-3);
        // 90ms 以上続いた分だけを声とみなす (単発のクリック音を数えない)
        self.run_ms = if rms > threshold { self.run_ms + frame_ms } else { 0 };
        let min_run_ms = (MIN_SPEECH_RUN_FRAMES * FRAME_MS) as u32;
        let is_speech = self.run_ms >= min_run_ms;
        self.window.push_back((frame_ms, is_speech));
        self.window_ms += frame_ms as u64;
        if is_speech {
            self.speech_ms += frame_ms as u64;
        }
        self.elapsed_ms += frame_ms as u64;
        while self.window_ms > IDLE_WINDOW_MS {
            let Some((ms, sp)) = self.window.pop_front() else { break };
            self.window_ms -= ms as u64;
            if sp {
                self.speech_ms -= ms as u64;
            }
        }
    }

    /// 直近 `IDLE_WINDOW_MS` の声の割合。まだその長さに届いていなければ `None`。
    pub fn recent_ratio(&self) -> Option<f32> {
        if self.elapsed_ms < IDLE_WINDOW_MS || self.window_ms == 0 {
            return None;
        }
        Some(self.speech_ms as f32 / self.window_ms as f32)
    }

    /// 声の無い状態が続いていて、録音を自動で止めるべきか。
    pub fn is_idle(&self) -> bool {
        self.recent_ratio().is_some_and(|r| r < IDLE_SPEECH_RATIO_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = 16_000;

    /// 雑音 (振幅 a の決まった揺れ) を n ミリ秒
    fn noise(ms: usize, a: f32) -> Vec<f32> {
        (0..SR * ms / 1000)
            .map(|i| a * (((i * 7919) % 97) as f32 / 48.0 - 1.0))
            .collect()
    }
    /// 声の代わり (振幅 a の正弦波) を n ミリ秒
    fn tone(ms: usize, a: f32) -> Vec<f32> {
        (0..SR * ms / 1000)
            .map(|i| a * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / SR as f32).sin())
            .collect()
    }

    #[test]
    fn long_inner_silence_is_compacted() {
        // 声 2 秒 → 雑音 60 秒 → 声 2 秒 → 雑音 60 秒 → 声 2 秒
        let mut s = tone(2000, 0.1);
        s.extend(noise(60_000, 0.002));
        s.extend(tone(2000, 0.1));
        s.extend(noise(60_000, 0.002));
        s.extend(tone(2000, 0.1));
        let c = compact_silences(&s, SR).expect("should compact");
        // 声 6 秒 + 余白 (各区間の前後 0.4 秒) + つなぎ 0.5 秒 × 2 ≒ 9 秒前後
        assert!(c.kept_ms < 10_000, "kept {}ms", c.kept_ms);
        assert!(c.kept_ms >= 6_000, "kept {}ms", c.kept_ms);
        assert!(c.removed_ms > 110_000);
        assert!(c.speech_ratio < 0.1);
    }

    #[test]
    fn normal_speech_with_short_pauses_is_untouched() {
        // 声 3 秒と 1 秒の間を交互に 10 回 (普通の話し方)
        let mut s = Vec::new();
        for _ in 0..10 {
            s.extend(tone(3000, 0.1));
            s.extend(noise(1000, 0.002));
        }
        assert_eq!(compact_silences(&s, SR), None);
    }

    #[test]
    fn soft_word_edges_are_kept_by_padding() {
        // 声の前後に弱い音 (床の 2 倍) が 300ms ずつ → 余白 400ms で残る
        let mut s = noise(10_000, 0.002);
        s.extend(tone(300, 0.004));
        s.extend(tone(2000, 0.1));
        s.extend(tone(300, 0.004));
        s.extend(noise(10_000, 0.002));
        let c = compact_silences(&s, SR).expect("should compact");
        assert!(c.kept_ms >= 2_600, "kept {}ms", c.kept_ms);
    }

    #[test]
    fn all_silence_returns_none() {
        assert_eq!(compact_silences(&noise(30_000, 0.002), SR), None);
    }

    /// 30ms ごとの RMS 列を meter に流す
    fn feed(m: &mut OnlineSpeechMeter, samples: &[f32]) {
        for c in samples.chunks(SR * 30 / 1000) {
            let rms = (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt();
            m.push(rms, 30);
        }
    }

    #[test]
    fn meter_stops_after_30s_without_voice() {
        let mut m = OnlineSpeechMeter::default();
        // 最初に 5 秒話し、その後 29 秒は雑音だけ → まだ止めない (直近 30 秒に 1 秒の声)
        feed(&mut m, &tone(5_000, 0.1));
        feed(&mut m, &noise(24_000, 0.002));
        assert!(!m.is_idle());
        // さらに 10 秒雑音 → 直近 30 秒は雑音だけ → 止める
        feed(&mut m, &noise(10_000, 0.002));
        assert!(m.is_idle(), "ratio {:?}", m.recent_ratio());
    }

    #[test]
    fn meter_does_not_stop_normal_speech() {
        let mut m = OnlineSpeechMeter::default();
        // 声 3 秒と間 1.5 秒の繰り返しを 60 秒
        for _ in 0..14 {
            feed(&mut m, &tone(3_000, 0.1));
            feed(&mut m, &noise(1_500, 0.002));
        }
        assert!(!m.is_idle(), "ratio {:?}", m.recent_ratio());
        assert!(m.recent_ratio().unwrap() > 0.5);
    }

    #[test]
    fn meter_waits_for_full_window() {
        let mut m = OnlineSpeechMeter::default();
        feed(&mut m, &noise(20_000, 0.002));
        assert_eq!(m.recent_ratio(), None);
        assert!(!m.is_idle());
    }

    #[test]
    fn discard_rule() {
        assert!(should_discard_as_no_speech(Some(0.04), 247_700)); // id=8458
        assert!(!should_discard_as_no_speech(Some(0.40), 10_600)); // 普段の最小
        assert!(!should_discard_as_no_speech(Some(0.02), 3_000)); // 短い一言は捨てない
        assert!(!should_discard_as_no_speech(None, 60_000)); // 判断できない
    }

    #[test]
    fn too_short_input_returns_none() {
        assert_eq!(compact_silences(&tone(200, 0.1), SR), None);
    }
}
