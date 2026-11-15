//! サンプル正確な再生クロック、`rubato` リサンプラ、および `cpal` 実時間音声出力。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};
use thiserror::Error;

use crate::core::time::{FrameIndex, FrameRate};
use crate::media::decoder::DecodedAudioPcm;
use crate::timeline::document::ProjectDocument;

use super::mixer::AudioMixer;

/// 音声再生・リサンプル処理で発生するエラー。
#[derive(Debug, Error)]
pub enum AudioError {
    /// 既定の出力デバイスが存在しない。
    #[error("no default audio output device available")]
    NoOutputDevice,
    /// デバイス設定やストリーム構築の失敗。
    #[error("audio device stream error: {0}")]
    DeviceError(String),
    /// `rubato` リサンプリング構築・処理エラー。
    #[error("audio resampling failed: {0}")]
    Resample(String),
}

/// 音声サンプルカウンタをマスターとするスレッドセーフな再生位置クロック。
#[derive(Debug)]
pub struct PlaybackClock {
    playing: AtomicBool,
    sample_rate: AtomicU32,
    current_sample: AtomicU64,
    fps_num: AtomicU32,
    fps_den: AtomicU32,
    total_frames: AtomicU64,
    loop_playback: AtomicBool,
}

impl PlaybackClock {
    /// 指定フレームレートと音声サンプルレートでクロックを初期化する。
    #[must_use]
    pub fn new(fps: FrameRate, sample_rate: u32) -> Self {
        Self {
            playing: AtomicBool::new(false),
            sample_rate: AtomicU32::new(sample_rate.max(1)),
            current_sample: AtomicU64::new(0),
            fps_num: AtomicU32::new(fps.numerator().max(1)),
            fps_den: AtomicU32::new(fps.denominator().max(1)),
            total_frames: AtomicU64::new(0),
            loop_playback: AtomicBool::new(false),
        }
    }

    /// 再生中かどうかを設定する。
    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }

    /// 再生中かどうかを返す。
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// 総尺（タイムライン終端フレーム）や FPS を最新状態へ同期する。
    pub fn update_timeline_metrics(&self, fps: FrameRate, total_frames: u64) {
        self.fps_num
            .store(fps.numerator().max(1), Ordering::Release);
        self.fps_den
            .store(fps.denominator().max(1), Ordering::Release);
        self.total_frames.store(total_frames, Ordering::Release);
    }

    /// 指定タイムラインフレーム位置へシークする（最も近いサンプル位置へ厳密換算）。
    pub fn seek_to_frame(&self, frame: FrameIndex) {
        let sr = u64::from(self.sample_rate.load(Ordering::Acquire).max(1));
        let num = u64::from(self.fps_num.load(Ordering::Acquire).max(1));
        let den = u64::from(self.fps_den.load(Ordering::Acquire).max(1));
        let sample = u128::from(frame.get())
            .saturating_mul(u128::from(sr))
            .saturating_mul(u128::from(den))
            .saturating_add(u128::from(num.saturating_sub(1)))
            / u128::from(num);
        let sample_u64 = u64::try_from(sample).unwrap_or(u64::MAX);
        self.current_sample.store(sample_u64, Ordering::Release);
    }

    /// 現在のサンプル位置を返す。
    #[must_use]
    pub fn current_sample(&self) -> u64 {
        self.current_sample.load(Ordering::Acquire)
    }

    /// 出力サンプルレート (Hz) を返す。
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    /// 現在の再生ヘッドに相当するタイムラインフレーム位置を返す。
    #[must_use]
    pub fn current_frame(&self) -> FrameIndex {
        let sample = self.current_sample.load(Ordering::Acquire);
        let sr = u64::from(self.sample_rate.load(Ordering::Acquire).max(1));
        let num = u64::from(self.fps_num.load(Ordering::Acquire).max(1));
        let den = u64::from(self.fps_den.load(Ordering::Acquire).max(1));
        let frame = u128::from(sample).saturating_mul(u128::from(num))
            / (u128::from(sr).saturating_mul(u128::from(den)).max(1));
        FrameIndex::new(u64::try_from(frame).unwrap_or(u64::MAX))
    }

    /// オーディオコールバックが出力したサンプルフレーム数だけクロックを進める。
    pub fn advance_samples(&self, frames: u64) -> u64 {
        if !self.is_playing() || frames == 0 {
            return self.current_sample.load(Ordering::Acquire);
        }
        let next = self
            .current_sample
            .fetch_add(frames, Ordering::AcqRel)
            .saturating_add(frames);
        self.clamp_or_loop_after_advance(next);
        next
    }

    /// 音声デバイスが使えないヘッドレス／無音環境向けに経過秒数でクロックを進める。
    pub fn advance_by_elapsed_seconds(&self, dt_seconds: f64) {
        if !self.is_playing() || dt_seconds <= 0.0 || !dt_seconds.is_finite() {
            return;
        }
        let sr = f64::from(self.sample_rate.load(Ordering::Acquire));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let delta_samples = (dt_seconds * sr).round() as u64;
        if delta_samples > 0 {
            let _ = self.advance_samples(delta_samples);
        }
    }

    fn clamp_or_loop_after_advance(&self, _next_sample: u64) {
        let total = self.total_frames.load(Ordering::Acquire);
        if total == 0 {
            return;
        }
        if self.current_frame().get() >= total {
            if self.loop_playback.load(Ordering::Acquire) {
                self.current_sample.store(0, Ordering::Release);
            } else {
                self.set_playing(false);
                self.seek_to_frame(FrameIndex::new(total));
            }
        }
    }
}

/// `rubato` を用いてインターリーブ済み `f32` PCM のサンプルレートを高品質に変換する。
///
/// # Errors
///
/// サンプルレートまたはチャンネル数が不正、あるいは `rubato` 処理が失敗した場合。
pub fn resample_interleaved_pcm(
    pcm: &DecodedAudioPcm,
    target_sample_rate: u32,
) -> Result<DecodedAudioPcm, AudioError> {
    if pcm.sample_rate == 0 || target_sample_rate == 0 || pcm.channels == 0 {
        return Err(AudioError::Resample(
            "sample rate and channel count must be non-zero".to_owned(),
        ));
    }
    if pcm.sample_rate == target_sample_rate || pcm.interleaved.is_empty() {
        return Ok(DecodedAudioPcm {
            sample_rate: target_sample_rate,
            channels: pcm.channels,
            interleaved: pcm.interleaved.clone(),
        });
    }

    let channels = usize::from(pcm.channels);
    let input_frames = pcm.frame_count();
    let ratio = f64::from(target_sample_rate) / f64::from(pcm.sample_rate);
    let chunk_size = input_frames.clamp(16, 1024);

    let mut resampler = Async::<f32>::new_poly(
        ratio,
        1.0,
        PolynomialDegree::Cubic,
        chunk_size,
        channels,
        FixedAsync::Input,
    )
    .map_err(|err| AudioError::Resample(err.to_string()))?;

    let needed_out = resampler.process_all_needed_output_len(input_frames);
    let in_adapter = InterleavedSlice::new(&pcm.interleaved, channels, input_frames)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    let mut out_samples = vec![0.0_f32; needed_out.saturating_mul(channels)];
    let mut out_adapter = InterleavedSlice::new_mut(&mut out_samples, channels, needed_out)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    let (_consumed, produced) = resampler
        .process_all_into_buffer(&in_adapter, &mut out_adapter, input_frames, None)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    out_samples.truncate(produced.saturating_mul(channels));
    Ok(DecodedAudioPcm {
        sample_rate: target_sample_rate,
        channels: pcm.channels,
        interleaved: out_samples,
    })
}

/// 再生エンジンが参照するスレッド共有コンテキスト。
#[derive(Clone)]
pub struct SharedPlaybackState {
    /// ドキュメントスナップショット（オーディオコールバックが参照）。
    pub document: Arc<Mutex<ProjectDocument>>,
    /// デコード済み PCM を保持するミキサ。
    pub mixer: Arc<Mutex<AudioMixer>>,
    /// サンプル基準のマスター再生クロック。
    pub clock: Arc<PlaybackClock>,
}

/// `cpal` オーディオ出力ストリームのライフタイムを管理するハンドル。
///
/// Drop されるとストリームが自動停止する。
pub struct AudioOutputHandle {
    _stream: Stream,
    output_sample_rate: u32,
}

impl AudioOutputHandle {
    /// デバイスが採用した出力サンプルレート (Hz) を返す。
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.output_sample_rate
    }
}

/// 既定の出力オーディオデバイスを開き、タイムラインミキサとクロックを駆動するストリームを開始する。
///
/// 出力デバイスが存在しない場合やヘッドレス CI 環境ではパニックせず `Err(AudioError)` を返す。
///
/// # Errors
///
/// 出力デバイス未接続、フォーマット未対応、またはストリーム開始に失敗した場合。
pub fn start_audio_output(state: &SharedPlaybackState) -> Result<AudioOutputHandle, AudioError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioError::NoOutputDevice)?;
    let default_cfg = device
        .default_output_config()
        .map_err(|err| AudioError::DeviceError(err.to_string()))?;

    let sample_rate = default_cfg.sample_rate();
    let channels = usize::from(default_cfg.channels().max(1));
    let stream_cfg = StreamConfig {
        channels: default_cfg.channels(),
        sample_rate,
        buffer_size: BufferSize::Default,
    };

    if default_cfg.sample_format() != SampleFormat::F32 {
        return Err(AudioError::DeviceError(format!(
            "Phase A expects f32 output stream, got {:?}",
            default_cfg.sample_format()
        )));
    }

    let state_clone = state.clone();
    let mut scratch_stereo = Vec::<f32>::with_capacity(2048);

    let stream = device
        .build_output_stream(
            stream_cfg,
            move |data: &mut [f32], _| {
                render_callback_f32(
                    &state_clone,
                    &mut scratch_stereo,
                    data,
                    channels,
                    sample_rate,
                );
            },
            |_err| {
                // コールバックエラーはスレッドパニックさせずに無視する（デバイス切断耐性）。
            },
            None,
        )
        .map_err(|err| AudioError::DeviceError(err.to_string()))?;

    stream
        .play()
        .map_err(|err| AudioError::DeviceError(err.to_string()))?;

    Ok(AudioOutputHandle {
        _stream: stream,
        output_sample_rate: sample_rate,
    })
}

fn render_callback_f32(
    state: &SharedPlaybackState,
    scratch_stereo: &mut Vec<f32>,
    output: &mut [f32],
    device_channels: usize,
    sample_rate: u32,
) {
    if !state.clock.is_playing() {
        output.fill(0.0);
        return;
    }

    let frames = output.len() / device_channels.max(1);
    let start_sample = state.clock.current_sample();

    if let (Ok(doc), Ok(mixer)) = (state.document.try_lock(), state.mixer.try_lock()) {
        mixer.mix_timeline_window(&doc, start_sample, frames, sample_rate, scratch_stereo);
        write_stereo_to_device(scratch_stereo, output, device_channels, frames);
        drop(mixer);
        drop(doc);
    } else {
        output.fill(0.0);
    }

    let _ = state.clock.advance_samples(frames as u64);
}

fn write_stereo_to_device(
    stereo: &[f32],
    output: &mut [f32],
    device_channels: usize,
    frames: usize,
) {
    for f in 0..frames {
        let l = stereo.get(f * 2).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
        let r = stereo.get(f * 2 + 1).copied().unwrap_or(l).clamp(-1.0, 1.0);
        let out_off = f * device_channels;
        if device_channels == 1 {
            output[out_off] = f32::midpoint(l, r);
        } else {
            output[out_off] = l;
            output[out_off + 1] = r;
            for ch in 2..device_channels {
                output[out_off + ch] = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_tracks_samples_and_frames_consistently() {
        let clock = PlaybackClock::new(FrameRate::FPS_60, 48_000);
        clock.update_timeline_metrics(FrameRate::FPS_60, 120);
        clock.seek_to_frame(FrameIndex::new(30));
        // 30 frames @ 60fps = 0.5s = 24,000 samples @ 48kHz
        assert_eq!(clock.current_sample(), 24_000);
        assert_eq!(clock.current_frame(), FrameIndex::new(30));

        clock.set_playing(true);
        let _ = clock.advance_samples(16_000); // total 40,000 samples -> floor(40000*60/48000) = 50
        assert_eq!(clock.current_sample(), 40_000);
        assert_eq!(clock.current_frame(), FrameIndex::new(50));
    }

    #[test]
    fn resample_44100_to_48000_produces_expected_length() -> Result<(), AudioError> {
        let src_rate = 44_100u32;
        let channels = 2u16;
        let frames = 4410usize; // 0.1 秒
        let mut interleaved = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            #[allow(clippy::cast_precision_loss)]
            let s = ((i as f32) * 440.0 * std::f32::consts::TAU / (src_rate as f32)).sin();
            interleaved.push(s);
            interleaved.push(s);
        }
        let pcm = DecodedAudioPcm {
            sample_rate: src_rate,
            channels,
            interleaved,
        };
        let out = resample_interleaved_pcm(&pcm, 48_000)?;
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 2);
        let out_frames = out.frame_count();
        assert!((4750..=4850).contains(&out_frames));
        Ok(())
    }
}
