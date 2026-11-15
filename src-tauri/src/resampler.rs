// Phase 1 Day 4-A: Resampler module
// 仕様: phase1_sprint_plan_2026-05-20.md §2.2 Day 4-A / 内部設計メモ
//
// 入力: RawFrame (device-native, e.g., 48kHz × 2ch interleaved f32 × 100ms = 9600 サンプル)
// 出力: TranscriptionFrame (TARGET_SAMPLE_RATE=16kHz × TARGET_CHANNELS=1 × f32)
//
// 処理:
//   1. interleaved stereo → mono ダウンミックス (チャンネル平均)
//   2. mono リサンプル (rubato FftFixedIn / 48kHz → 16kHz など固定比率に対応)
//
// L118 準拠: TranscriptionFrame には sample_rate / channels を必須フィールドで含め
//            下流（VAD / transcribe / halu）の解像度誤認を防ぐ。

use anyhow::{anyhow, Context, Result};
use rubato::{FftFixedIn, Resampler as RubatoResampler};
use log::info; // tracing は受け皿(subscriber)なしで出力が破棄されるため log へ置換 (教訓 L286)

use crate::audio::{RawFrame, FRAME_DURATION_MS, TARGET_CHANNELS, TARGET_SAMPLE_RATE};

/// リサンプル後のフレーム。
/// samples は mono 連続 (interleaved ではない)。
#[derive(Debug)]
pub struct TranscriptionFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

pub struct Resampler {
    inner: FftFixedIn<f32>,
    input_sample_rate: u32,
    input_channels: u16,
    /// 1 フレーム分の interleaved サンプル数 (= mono_chunk_size × channels)。
    /// audio.rs の frame_samples と一致しなければエラーにする。
    expected_input_len: usize,
    /// mono ダウンミックス後の 1 フレーム分のサンプル数 (rubato 入力サイズ)。
    mono_chunk_size: usize,
    /// 再利用バッファ。process ごとに clear → push する。
    mono_buf: Vec<f32>,
}

impl Resampler {
    /// `device_sample_rate` / `device_channels` は `AudioRecorder::start()` が決めた値を渡す。
    /// 100ms 単位のフレームを想定 (FRAME_DURATION_MS = 100)。
    pub fn new(device_sample_rate: u32, device_channels: u16) -> Result<Self> {
        if device_channels == 0 {
            return Err(anyhow!("device_channels must be >= 1"));
        }
        if device_sample_rate == 0 {
            return Err(anyhow!("device_sample_rate must be > 0"));
        }

        let mono_chunk_size =
            (device_sample_rate as usize) * (FRAME_DURATION_MS as usize) / 1000;
        if mono_chunk_size == 0 {
            return Err(anyhow!(
                "computed mono_chunk_size = 0 (sample_rate={}, duration_ms={})",
                device_sample_rate,
                FRAME_DURATION_MS
            ));
        }

        let inner = FftFixedIn::<f32>::new(
            device_sample_rate as usize,
            TARGET_SAMPLE_RATE as usize,
            mono_chunk_size,
            1, // sub_chunks: 1 で十分 (チャンク境界の追加分割不要)
            1, // channels: mono にダウンミックス済みの 1ch を渡す
        )
        .context("failed to construct rubato::FftFixedIn")?;

        let expected_input_len = mono_chunk_size * device_channels as usize;

        info!(
            target: "rushvoice.resampler",
            "resampler constructed: input_rate={} input_channels={} output_rate={} output_channels={} mono_chunk_size={} expected_input_len={}",
            device_sample_rate, device_channels, TARGET_SAMPLE_RATE, TARGET_CHANNELS, mono_chunk_size, expected_input_len
        );

        Ok(Self {
            inner,
            input_sample_rate: device_sample_rate,
            input_channels: device_channels,
            expected_input_len,
            mono_chunk_size,
            mono_buf: Vec::with_capacity(mono_chunk_size),
        })
    }

    pub fn output_sample_rate(&self) -> u32 {
        TARGET_SAMPLE_RATE
    }

    pub fn output_channels(&self) -> u16 {
        TARGET_CHANNELS
    }

    /// RawFrame を TranscriptionFrame に変換する。
    /// 入力 frame の解像度 (sample_rate / channels / 長さ) が new() 時点と一致しない場合はエラー。
    pub fn process(&mut self, frame: RawFrame) -> Result<TranscriptionFrame> {
        if frame.sample_rate != self.input_sample_rate {
            return Err(anyhow!(
                "sample_rate mismatch: resampler expects {}, frame has {}",
                self.input_sample_rate,
                frame.sample_rate
            ));
        }
        if frame.channels != self.input_channels {
            return Err(anyhow!(
                "channels mismatch: resampler expects {}, frame has {}",
                self.input_channels,
                frame.channels
            ));
        }
        if frame.samples.len() != self.expected_input_len {
            return Err(anyhow!(
                "samples length mismatch: expected {}, got {}",
                self.expected_input_len,
                frame.samples.len()
            ));
        }

        // 1) interleaved → mono ダウンミックス (チャンネル平均)
        self.mono_buf.clear();
        let n_ch = self.input_channels as usize;
        if n_ch == 1 {
            self.mono_buf.extend_from_slice(&frame.samples);
        } else {
            let scale = 1.0_f32 / n_ch as f32;
            for chunk in frame.samples.chunks_exact(n_ch) {
                let sum: f32 = chunk.iter().sum();
                self.mono_buf.push(sum * scale);
            }
        }
        debug_assert_eq!(self.mono_buf.len(), self.mono_chunk_size);

        // 2) mono リサンプル (rubato は &[Vec<f32>] / Vec<Vec<f32>> 設計)
        //    1ch なので slice で 1 本だけ渡す。
        let input_channels: [&[f32]; 1] = [self.mono_buf.as_slice()];
        let output_buffers = self
            .inner
            .process(&input_channels, None)
            .context("rubato resampling failed")?;
        let output_samples = output_buffers
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("rubato returned 0 output channels"))?;

        Ok(TranscriptionFrame {
            samples: output_samples,
            sample_rate: TARGET_SAMPLE_RATE,
            channels: TARGET_CHANNELS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FRAME_DURATION_MS;

    fn frame_at(sample_rate: u32, channels: u16, fill: f32) -> RawFrame {
        let len = (sample_rate as usize) * (channels as usize) * (FRAME_DURATION_MS as usize)
            / 1000;
        RawFrame {
            samples: vec![fill; len],
            sample_rate,
            channels,
        }
    }

    #[test]
    fn output_rate_and_channels_are_target() {
        let mut r = Resampler::new(48000, 2).unwrap();
        let out = r.process(frame_at(48000, 2, 0.0)).unwrap();
        assert_eq!(out.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(out.channels, TARGET_CHANNELS);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut r = Resampler::new(48000, 2).unwrap();
        let out = r.process(frame_at(48000, 2, 0.0)).unwrap();
        // 完全な無音入力ならリサンプル後も無音 (FFT 数値誤差を許容)
        let max_abs = out
            .samples
            .iter()
            .copied()
            .fold(0.0_f32, |acc, x| acc.max(x.abs()));
        assert!(max_abs < 1e-6, "silence in -> non-silence out (max_abs={})", max_abs);
    }

    #[test]
    fn mono_input_passes_through_resampler() {
        // 48000 → 16000 で device_channels=1 のケース (デバイス側がすでに mono)
        let mut r = Resampler::new(48000, 1).unwrap();
        let out = r.process(frame_at(48000, 1, 0.5)).unwrap();
        assert_eq!(out.channels, TARGET_CHANNELS);
        assert!(!out.samples.is_empty());
    }

    #[test]
    fn rejects_mismatched_sample_rate() {
        let mut r = Resampler::new(48000, 2).unwrap();
        let err = r.process(frame_at(44100, 2, 0.0)).unwrap_err();
        assert!(err.to_string().contains("sample_rate mismatch"));
    }

    #[test]
    fn rejects_mismatched_channels() {
        let mut r = Resampler::new(48000, 2).unwrap();
        let err = r.process(frame_at(48000, 1, 0.0)).unwrap_err();
        assert!(err.to_string().contains("channels mismatch"));
    }

    #[test]
    fn rejects_wrong_length() {
        let mut r = Resampler::new(48000, 2).unwrap();
        let bad = RawFrame {
            samples: vec![0.0; 100],
            sample_rate: 48000,
            channels: 2,
        };
        let err = r.process(bad).unwrap_err();
        assert!(err.to_string().contains("samples length mismatch"));
    }
}
