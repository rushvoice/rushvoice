// Phase 1 Day 3-B: cpal audio capture
// 仕様: phase1_v1_read_notes.md §5 Day 3-4 / phase1_sprint_plan §2.2 Day 3-4
//
// 重要:
//   - 本モジュールは「デバイスのネイティブ sample_rate / channels」のままサンプルを取得する。
//   - 16 kHz / mono / f32 への変換（リサンプリング + ダウンミックス）は Day 4 で実装する。
//   - そのため戻り値の `RawFrame` には samples だけでなく sample_rate と channels を必ず付与し、
//     呼び出し側が解像度誤認を起こさないようにしている（Codex review P1 2026-05-20 対応）。
//
// AU-02 (Stage 2-D): 既定マイク自動追従。
//   - AudioRecorder::start() は呼び出しの都度 cpal::default_host().default_input_device()
//     を再評価するため、Windows のデフォルトマイク切替は「次回録音開始時」に自動反映される。
//   - 録音中の切替は範囲外 (cpal::Stream は device に bind 済みのため・録音停止→再開で反映)。
//   - 切替検出は rushvoice.audio.au02 target のログを参照 (device 名を毎セッション emit)。

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use tokio::sync::mpsc;
// tracing → log へ置換。本アプリのログ基盤は tauri-plugin-log (log crate) で、
// tracing の受け皿 (subscriber) は初期化されていないため、tracing マクロの出力は
// **全て破棄されていた** (教訓 L286 と同一欠陥・無料版 Air は v1.0.1 で先に修正済み)。
// 特に致命的なのは err_fn の「stream error」= 録音中にマイクデバイスが死んだ時の
// 唯一の証跡が無記録だったこと。
use log::{info, warn};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
pub const TARGET_CHANNELS: u16 = 1;
pub const FRAME_DURATION_MS: u32 = 100;

/// デバイスのネイティブ形式のまま 1 フレーム分の interleaved サンプルを保持する。
/// Day 4 で resampler + downmixer がこれを TARGET_SAMPLE_RATE / TARGET_CHANNELS に変換する。
///
/// v0.1.16 Day 2 (議事録モード): `Clone` を追加。`finalize_chunk` で worker thread 内に
/// ローカル蓄積した `Vec<RawFrame>` から `Resampler::process(frame: RawFrame)` を
/// `for f in frames { resampler.process(f.clone()) }` で呼び出すため。
/// 既存利用箇所 (cpal callback / mpsc tx / resampler テスト) は move/構築のみで
/// `.clone()` 依存がないため、derive 追加は副作用なし。
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

pub struct AudioRecorder {
    _stream: cpal::Stream,
    rx: mpsc::UnboundedReceiver<RawFrame>,
    pub device_sample_rate: u32,
    pub device_channels: u16,
    /// 実際に開いたマイクの名前 (希望のマイクが見つからず既定へ切り替えた場合は既定の名前)
    pub device_name: String,
}

/// マイク選択 GUI (2026-07-23 要望): 入力デバイス名の一覧と Windows 既定デバイス名を返す。
/// 一覧はそのまま設定画面のドロップダウンに出す。列挙失敗は空 Vec (best-effort)。
pub fn list_input_device_names() -> (Vec<String>, Option<String>) {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let names = match host.input_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(e) => {
            warn!(target: "rushvoice.audio", "input_devices() enumeration failed: {}", e);
            Vec::new()
        }
    };
    (names, default_name)
}

impl AudioRecorder {
    /// マイク選択 GUI (2026-07-23): `preferred_name` が Some(非空) なら名前一致の
    /// 入力デバイスを探して使う。見つからない (取り外し済等) 場合は warn を出して
    /// Windows 既定デバイスにフォールバックする (録音を止めないことを優先)。
    pub fn start(preferred_name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let preferred = preferred_name.map(str::trim).filter(|s| !s.is_empty());
        let device = match preferred {
            Some(name) => {
                let found = host
                    .input_devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| d.name().map(|n| n == name).unwrap_or(false)));
                match found {
                    Some(d) => d,
                    None => {
                        warn!(
                            target: "rushvoice.audio",
                            "preferred input device '{}' not found; falling back to default",
                            name
                        );
                        host.default_input_device()
                            .ok_or_else(|| anyhow!("no default input device"))?
                    }
                }
            }
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };

        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
        // AU-02: 既定マイク自動追従の証跡。録音開始ごとにこのログが出れば切替反映を確認できる。
        info!(
            target: "rushvoice.audio.au02",
            "AU-02: default_input_device() resolved at session start: device={}",
            device_name
        );
        let supported = device
            .default_input_config()
            .context("failed to query default input config")?;
        let sample_format = supported.sample_format();
        let device_sample_rate = supported.sample_rate().0;
        let config: cpal::StreamConfig = supported.into();
        let device_channels = config.channels;

        // 1 フレーム = FRAME_DURATION_MS ms 分の interleaved サンプル数（channels 倍含む）
        let frame_samples =
            (device_sample_rate as usize) * (device_channels as usize) * (FRAME_DURATION_MS as usize)
                / 1000;

        info!(
            target: "rushvoice.audio",
            "starting input stream: device={} sample_rate={} channels={} sample_format={:?} frame_samples={} frame_duration_ms={} (downmix + resample deferred to Day 4)",
            device_name, device_sample_rate, device_channels, sample_format, frame_samples, FRAME_DURATION_MS
        );

        let (tx, rx) = mpsc::unbounded_channel::<RawFrame>();
        let mut buf: Vec<f32> = Vec::with_capacity(frame_samples);

        let err_fn = |err| {
            warn!(target: "rushvoice.audio", "stream error: {}", err);
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    push_samples(
                        &mut buf,
                        data.iter().copied(),
                        frame_samples,
                        device_sample_rate,
                        device_channels,
                        &tx,
                    );
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let it = data.iter().map(|&s| s as f32 / i16::MAX as f32);
                    push_samples(
                        &mut buf,
                        it,
                        frame_samples,
                        device_sample_rate,
                        device_channels,
                        &tx,
                    );
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let bias = u16::MAX as f32 / 2.0;
                    let it = data.iter().map(move |&s| (s as f32 - bias) / bias);
                    push_samples(
                        &mut buf,
                        it,
                        frame_samples,
                        device_sample_rate,
                        device_channels,
                        &tx,
                    );
                },
                err_fn,
                None,
            ),
            other => return Err(anyhow!("unsupported sample format: {:?}", other)),
        }
        .context("failed to build input stream")?;

        stream.play().context("failed to start input stream")?;

        Ok(Self {
            _stream: stream,
            rx,
            device_sample_rate,
            device_channels,
            device_name,
        })
    }

    pub async fn next_frame(&mut self) -> Option<RawFrame> {
        self.rx.recv().await
    }

    /// std::thread から呼ぶための同期版（cpal::Stream は !Send なので AudioRecorder ごと
    /// 別 thread に閉じ込める運用が必要・Day 3-C テスト録音などで使う）。
    pub fn next_frame_blocking(&mut self) -> Option<RawFrame> {
        self.rx.blocking_recv()
    }

    /// 待たずに1フレーム取る (待機ストリームの見張り用)。
    fn try_next_frame(&mut self) -> Result<RawFrame, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

/// 今開くべきマイクの名前: 希望のマイクがつながっていればその名前、無ければ Windows 既定のマイク。
/// (2026-10-31 利用者「ショックズ外すこともあるだろう」: 抜き差しに自動で追いつくための判定)
fn desired_input_device_name(preferred: Option<&str>) -> Option<String> {
    let host = cpal::default_host();
    if let Some(name) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        let present = host
            .input_devices()
            .ok()
            .map(|mut it| it.any(|d| d.name().map(|n| n == name).unwrap_or(false)))
            .unwrap_or(false);
        if present {
            return Some(name.to_string());
        }
    }
    host.default_input_device().and_then(|d| d.name().ok())
}

/// 待機中のマイクを開き直すべきか。開いているマイクと今開くべきマイクが違えば true。
/// 録音中 (tap あり) は録音を切らないため開き直さない。純関数 (テスト用に分離)。
fn standby_should_reopen(open_device: &str, desired: Option<&str>, recording: bool) -> bool {
    if recording {
        return false;
    }
    matches!(desired, Some(d) if d != open_device)
}

/// マイク常時スタンバイ (2026-07-23 要望・Shokz 冒頭切れの根本対策)。
///
/// Bluetooth HFP マイクは音声リンク (SCO) の確立に 0.3〜0.5s かかり、その間の
/// 発話は復元不可能。録音していない間も入力ストリームを開きっぱなしにして
/// リンクを維持し、録音開始時のウォームアップギャップを無くす。
///
/// - フレームは読み捨てる (蓄積しない・録音セッションとは独立)
/// - WASAPI 共有モードなので録音セッションのストリームと同一デバイス併用可
/// - 停止は stop() で flag → join (ストリームはフレームを流し続けるため
///   blocking_recv はすぐ戻り、素早く終了する)
pub struct MicStandby {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// 録音開始時に待機ストリームから遡って足す長さ (2026-10-31 指摘「キーを押してから
/// 喋れるまでが長い」への対策)。押す直前の音も拾うので、押しながら話し始めても頭が欠けない。
pub const STANDBY_PREROLL_MS: u32 = 300;
/// 待機ストリームが開けなかった (ヘッドセット未接続等) ときに開き直す間隔。
const STANDBY_RETRY_SECS: u64 = 5;
/// 待機中に「今開くべきマイク」と比べる間隔 (抜き差し・既定マイクの変更に追いつく)。
const STANDBY_DEVICE_CHECK_MS: u64 = 3_000;
/// 待機ストリームからフレームがこれだけ届かなければ、マイクが外れたとみなして開き直す。
const STANDBY_NO_FRAME_REOPEN_MS: u64 = 3_000;

/// 待機ストリームと録音セッションの受け渡し口。
/// - 普段: 直近 STANDBY_PREROLL_MS 分のフレームを ring に保持し、それより古いものは捨てる
/// - 録音中: tap が Some の間はフレームを録音セッションへ流す
struct StandbyShared {
    device_name: String,
    sample_rate: u32,
    channels: u16,
    ring: std::collections::VecDeque<RawFrame>,
    tap: Option<mpsc::UnboundedSender<RawFrame>>,
}

static STANDBY_SHARED: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<StandbyShared>>>> =
    std::sync::Mutex::new(None);

/// 待機ストリームから録音を受け取る口。drop すると待機側の読み捨てに戻る。
pub struct StandbyTap {
    shared: std::sync::Arc<std::sync::Mutex<StandbyShared>>,
    rx: mpsc::UnboundedReceiver<RawFrame>,
    pub sample_rate: u32,
    pub channels: u16,
    /// 押す直前の音 (古い順)
    pub preroll: Vec<RawFrame>,
}

impl StandbyTap {
    pub fn next_frame_blocking(&mut self) -> Option<RawFrame> {
        self.rx.blocking_recv()
    }
}

impl Drop for StandbyTap {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.lock() {
            s.tap = None;
        }
    }
}

/// 待機ストリームが動いていて、`wanted_device` (None なら Windows 既定) と同じマイクなら
/// その場で録音を始められる口を返す。使えなければ None (呼び出し側は従来どおり開く)。
pub fn standby_subscribe(wanted_device: Option<&str>) -> Option<StandbyTap> {
    let shared = STANDBY_SHARED.lock().ok()?.clone()?;
    let wanted = match wanted_device.map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => cpal::default_host().default_input_device()?.name().ok()?,
    };
    standby_subscribe_shared(shared, &wanted)
}

// OS のマイク探索と切り離してあるので、実マイクなしで試験できる。
fn standby_subscribe_shared(
    shared: std::sync::Arc<std::sync::Mutex<StandbyShared>>,
    wanted: &str,
) -> Option<StandbyTap> {
    let (tx, rx) = mpsc::unbounded_channel::<RawFrame>();
    let mut s = shared.lock().ok()?;
    if s.device_name != wanted || s.tap.is_some() {
        return None;
    }
    let preroll: Vec<RawFrame> = s.ring.drain(..).collect();
    s.tap = Some(tx);
    let (sample_rate, channels) = (s.sample_rate, s.channels);
    drop(s);
    Some(StandbyTap {
        shared,
        rx,
        sample_rate,
        channels,
        preroll,
    })
}

impl MicStandby {
    pub fn start(preferred_name: Option<String>) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_ref = stop.clone();
        let handle = std::thread::spawn(move || {
            let stopped = || stop_ref.load(std::sync::atomic::Ordering::Relaxed);
            while !stopped() {
                let mut recorder = match AudioRecorder::start(preferred_name.as_deref()) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(target: "rushvoice.audio", "mic standby: AudioRecorder::start failed (retry in {}s): {:?}", STANDBY_RETRY_SECS, e);
                        for _ in 0..(STANDBY_RETRY_SECS * 10) {
                            if stopped() {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        continue;
                    }
                };
                // 共有に載せる名前は「実際に開いたマイク」。希望のマイクが無くて既定で開いた
                // ときに希望の名前を載せると、あとで希望のマイクを挿した瞬間に、既定のままの
                // ストリームを再利用してしまう。名前が取れなかった ("<unknown>") ときは再利用しない。
                let device_name = if recorder.device_name == "<unknown>" {
                    String::new()
                } else {
                    recorder.device_name.clone()
                };
                let max_frames = (STANDBY_PREROLL_MS / FRAME_DURATION_MS).max(1) as usize;
                let shared = std::sync::Arc::new(std::sync::Mutex::new(StandbyShared {
                    device_name: device_name.clone(),
                    sample_rate: recorder.device_sample_rate,
                    channels: recorder.device_channels,
                    ring: std::collections::VecDeque::with_capacity(max_frames + 1),
                    tap: None,
                }));
                if let Ok(mut g) = STANDBY_SHARED.lock() {
                    *g = Some(shared.clone());
                }
                let wanted_name = desired_input_device_name(preferred_name.as_deref());
                if wanted_name.as_deref().map_or(true, |w| w == recorder.device_name) {
                    info!(target: "rushvoice.audio", "mic standby stream started: device={} preroll_ms={}", recorder.device_name, STANDBY_PREROLL_MS);
                } else {
                    warn!(target: "rushvoice.audio", "mic standby stream started on fallback: device={} (wanted={} not found) preroll_ms={}", recorder.device_name, wanted_name.as_deref().unwrap_or(""), STANDBY_PREROLL_MS);
                }
                let mut ended = false;
                let mut last_frame_at = std::time::Instant::now();
                let mut last_check_at = std::time::Instant::now();
                while !stopped() {
                    // 抜き差し・既定マイクの変更に追いつく (録音中は切らない)
                    if last_check_at.elapsed().as_millis() as u64 >= STANDBY_DEVICE_CHECK_MS {
                        last_check_at = std::time::Instant::now();
                        let recording = shared.lock().map(|s| s.tap.is_some()).unwrap_or(false);
                        if !recording {
                            let desired = desired_input_device_name(preferred_name.as_deref());
                            if standby_should_reopen(&recorder.device_name, desired.as_deref(), recording) {
                                info!(
                                    target: "rushvoice.audio",
                                    "mic standby: device changed: open={} desired={}; reopening",
                                    recorder.device_name,
                                    desired.as_deref().unwrap_or("")
                                );
                                break;
                            }
                        }
                    }
                    let frame = match recorder.try_next_frame() {
                        Ok(f) => f,
                        Err(mpsc::error::TryRecvError::Empty) => {
                            // マイクが外れるとフレームが止まったまま戻らないことがある
                            if last_frame_at.elapsed().as_millis() as u64 >= STANDBY_NO_FRAME_REOPEN_MS {
                                warn!(
                                    target: "rushvoice.audio",
                                    "mic standby: no audio frames for {}ms from device={} (removed?); reopening",
                                    STANDBY_NO_FRAME_REOPEN_MS, recorder.device_name
                                );
                                ended = true;
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            ended = true;
                            break;
                        }
                    };
                    last_frame_at = std::time::Instant::now();
                    if let Ok(mut s) = shared.lock() {
                        if let Some(tx) = s.tap.as_ref() {
                            if tx.send(frame).is_err() {
                                s.tap = None;
                            }
                        } else {
                            s.ring.push_back(frame);
                            while s.ring.len() > max_frames {
                                s.ring.pop_front();
                            }
                        }
                    }
                }
                // 録音中の口を閉じる (受け手は None を受けて録音を締める)
                if let Ok(mut g) = STANDBY_SHARED.lock() {
                    *g = None;
                }
                if let Ok(mut s) = shared.lock() {
                    s.tap = None;
                }
                if ended {
                    warn!(target: "rushvoice.audio", "mic standby stream ended unexpectedly; reopening");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
            info!(target: "rushvoice.audio", "mic standby stream stopped");
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// cpal コールバックから渡される interleaved サンプルを `frame_samples` 単位の
/// 固定長フレームに切り出して `tx` に流す。
///
/// 重要 (Codex P1 2026-05-21 修正): cpal コールバックサイズが `frame_samples` を
/// 割り切らない環境でも、必ず `samples.len() == frame_samples` の RawFrame だけを
/// 送る (端数は次回呼び出しに繰り越す)。
/// これにより resampler.rs::process() の strict-equality 仮定が成立する。
fn push_samples<I>(
    buf: &mut Vec<f32>,
    samples: I,
    frame_samples: usize,
    sample_rate: u32,
    channels: u16,
    tx: &mpsc::UnboundedSender<RawFrame>,
) where
    I: IntoIterator<Item = f32>,
{
    buf.extend(samples);
    while buf.len() >= frame_samples {
        let drained: Vec<f32> = buf.drain(..frame_samples).collect();
        debug_assert_eq!(drained.len(), frame_samples);
        let frame = RawFrame {
            samples: drained,
            sample_rate,
            channels,
        };
        if tx.send(frame).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod standby_reopen_tests {
    use super::standby_should_reopen;

    #[test]
    fn reopens_when_preferred_mic_comes_back() {
        // Shokz が無く既定で開いている → Shokz が戻った
        assert!(standby_should_reopen("Default Mic", Some("Shokz"), false));
    }

    #[test]
    fn reopens_when_preferred_mic_is_removed() {
        // Shokz で開いている → Shokz が外れ、既定が開くべきマイクになった
        assert!(standby_should_reopen("Shokz", Some("Default Mic"), false));
    }

    #[test]
    fn keeps_stream_when_same_or_recording_or_unknown() {
        assert!(!standby_should_reopen("Shokz", Some("Shokz"), false));
        // 録音中は録音を切らない
        assert!(!standby_should_reopen("Shokz", Some("Default Mic"), true));
        // マイクが1つも無い (判定できない) ときは開き直さない
        assert!(!standby_should_reopen("Shokz", None, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standby_fixture(device_name: &str) -> std::sync::Arc<std::sync::Mutex<StandbyShared>> {
        std::sync::Arc::new(std::sync::Mutex::new(StandbyShared {
            device_name: device_name.to_string(),
            sample_rate: 48000,
            channels: 2,
            ring: std::collections::VecDeque::from([RawFrame {
                samples: vec![0.25; 9600],
                sample_rate: 48000,
                channels: 2,
            }]),
            tap: None,
        }))
    }

    #[test]
    fn standby_fallback_is_not_reused_for_reconnected_preferred_device() {
        // 起動時は希望の USB マイクが無く内蔵マイクで開いた。USB を挿したあとは
        // 内蔵マイクの音を使い回さず、USB を開き直すこと。
        let shared = standby_fixture("Built-in input");
        assert!(standby_subscribe_shared(shared.clone(), "USB input").is_none());
        let s = shared.lock().unwrap();
        assert!(s.tap.is_none());
        assert_eq!(s.ring.len(), 1, "mismatch must preserve preroll");
    }

    #[test]
    fn standby_unknown_device_identity_is_not_reused() {
        assert!(standby_subscribe_shared(standby_fixture(""), "USB input").is_none());
    }

    #[test]
    fn standby_matching_device_preserves_format_preroll_and_exclusive_tap() {
        let shared = standby_fixture("USB input");
        let mut tap = standby_subscribe_shared(shared.clone(), "USB input").unwrap();
        assert_eq!((tap.sample_rate, tap.channels), (48000, 2));
        assert_eq!(tap.preroll.len(), 1);
        assert_eq!(tap.preroll[0].samples[0], 0.25);
        assert!(shared.lock().unwrap().ring.is_empty());
        assert!(standby_subscribe_shared(shared.clone(), "USB input").is_none());
        shared.lock().unwrap().tap.as_ref().unwrap().send(RawFrame {
            samples: vec![0.5; 9600], sample_rate: 48000, channels: 2,
        }).unwrap();
        assert_eq!(tap.next_frame_blocking().unwrap().samples[0], 0.5);
        drop(tap);
        assert!(standby_subscribe_shared(shared, "USB input").is_some());
    }

    /// 9600 ちょうどの batch を 1 回入れたら frame 1 本 (= 9600 samples) を出す。
    #[test]
    fn exact_size_emits_single_frame() {
        let (tx, mut rx) = mpsc::unbounded_channel::<RawFrame>();
        let mut buf: Vec<f32> = Vec::new();
        let batch: Vec<f32> = vec![0.5; 9600];
        push_samples(&mut buf, batch.into_iter(), 9600, 48000, 2, &tx);
        let frame = rx.try_recv().expect("should emit one frame");
        assert_eq!(frame.samples.len(), 9600);
        assert_eq!(frame.sample_rate, 48000);
        assert_eq!(frame.channels, 2);
        assert!(buf.is_empty(), "no leftover");
        assert!(rx.try_recv().is_err(), "no extra frame");
    }

    /// 4096 サンプル batch を複数回入れて 9600 単位のフレームに分割されることを確認。
    /// Codex P1 2026-05-21 で指摘された「9600 を割り切らない CPAL コールバック」のリグレッションテスト。
    #[test]
    fn variable_callback_size_drained_in_exact_chunks() {
        let (tx, mut rx) = mpsc::unbounded_channel::<RawFrame>();
        let mut buf: Vec<f32> = Vec::new();
        // 4096 × 3 = 12288 = 9600 + 2688 → frame 1 本 + 2688 残
        push_samples(&mut buf, std::iter::repeat(0.1).take(4096), 9600, 48000, 2, &tx);
        push_samples(&mut buf, std::iter::repeat(0.2).take(4096), 9600, 48000, 2, &tx);
        push_samples(&mut buf, std::iter::repeat(0.3).take(4096), 9600, 48000, 2, &tx);
        let frame = rx.try_recv().expect("first frame");
        assert_eq!(frame.samples.len(), 9600, "frame must be exactly 9600");
        assert!(rx.try_recv().is_err(), "only one frame at this point");
        assert_eq!(buf.len(), 2688, "leftover preserved for next call");
        // さらに 6912 サンプル投入 → 2688 + 6912 = 9600 → ちょうど 2 本目
        push_samples(&mut buf, std::iter::repeat(0.4).take(6912), 9600, 48000, 2, &tx);
        let frame2 = rx.try_recv().expect("second frame");
        assert_eq!(frame2.samples.len(), 9600);
        assert!(buf.is_empty());
    }

    /// 2 倍以上の batch を 1 回で渡したら同じ呼び出し内で複数フレームが出る (while ループ)。
    #[test]
    fn oversized_batch_emits_multiple_frames_in_one_call() {
        let (tx, mut rx) = mpsc::unbounded_channel::<RawFrame>();
        let mut buf: Vec<f32> = Vec::new();
        // 9600 × 2 + 500 = 19700
        push_samples(&mut buf, std::iter::repeat(0.0).take(19_700), 9600, 48000, 2, &tx);
        let a = rx.try_recv().expect("frame 1");
        let b = rx.try_recv().expect("frame 2");
        assert_eq!(a.samples.len(), 9600);
        assert_eq!(b.samples.len(), 9600);
        assert!(rx.try_recv().is_err());
        assert_eq!(buf.len(), 500, "500 sample remainder");
    }
}
