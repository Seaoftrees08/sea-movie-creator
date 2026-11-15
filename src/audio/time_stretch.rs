//! 再生速度 (`PlaybackRate`) に応じたピッチ維持タイムストレッチ (`WSOLA`) および変則ピッチ変更 (`rubato`)。

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

use crate::audio::playback::AudioError;
use crate::core::time::PlaybackRate;
use crate::media::decoder::DecodedAudioPcm;

/// タイムストレッチ処理の動作モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeStretchMode {
    /// ピッチを維持して速度のみ変更する（純粋 Rust `WSOLA`）。
    PitchPreservingWsola,
    /// ピッチも速度に比例して変更する（`rubato` 高品位リサンプラ）。
    ResampleVarispeed,
}

/// 指定された再生速度・ピッチ保持設定に従って PCM 音声の尺（および任意でピッチ）を変換する。
///
/// # Errors
///
/// サンプルレート／チャンネル数不正、または `rubato` リサンプリング失敗時に [`AudioError`] を返す。
pub fn time_stretch_pcm(
    pcm: &DecodedAudioPcm,
    playback_rate: PlaybackRate,
    preserve_pitch: bool,
) -> Result<DecodedAudioPcm, AudioError> {
    if pcm.sample_rate == 0 || pcm.channels == 0 {
        return Err(AudioError::Resample(
            "sample rate and channel count must be non-zero".to_owned(),
        ));
    }
    if pcm.interleaved.is_empty() || playback_rate == PlaybackRate::NORMAL {
        return Ok(pcm.clone());
    }

    let mode = if preserve_pitch {
        TimeStretchMode::PitchPreservingWsola
    } else {
        TimeStretchMode::ResampleVarispeed
    };

    match mode {
        TimeStretchMode::ResampleVarispeed => varispeed_resample(pcm, playback_rate),
        TimeStretchMode::PitchPreservingWsola => Ok(wsola_time_stretch(pcm, playback_rate)),
    }
}

/// 入力フレーム数と再生速度から理論出力フレーム数を計算する。
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn compute_stretched_frame_count(in_frames: usize, playback_rate: PlaybackRate) -> usize {
    if in_frames == 0 {
        return 0;
    }
    let ratio = f64::from(playback_rate.denominator()) / f64::from(playback_rate.numerator());
    ((in_frames as f64) * ratio).round().max(1.0) as usize
}

#[allow(clippy::cast_precision_loss)]
fn varispeed_resample(
    pcm: &DecodedAudioPcm,
    playback_rate: PlaybackRate,
) -> Result<DecodedAudioPcm, AudioError> {
    let channels = usize::from(pcm.channels);
    let in_frames = pcm.frame_count();
    let target_frames = compute_stretched_frame_count(in_frames, playback_rate);
    let ratio = f64::from(playback_rate.denominator()) / f64::from(playback_rate.numerator());

    if in_frames < 16 {
        return Ok(linear_scale_frames(pcm, target_frames));
    }

    let chunk_size = in_frames.clamp(16, 1024);
    let mut resampler = Async::<f32>::new_poly(
        ratio,
        1.0,
        PolynomialDegree::Cubic,
        chunk_size,
        channels,
        FixedAsync::Input,
    )
    .map_err(|err| AudioError::Resample(err.to_string()))?;

    let needed_out = resampler.process_all_needed_output_len(in_frames);
    let in_adapter = InterleavedSlice::new(&pcm.interleaved, channels, in_frames)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    let mut out_samples = vec![0.0_f32; needed_out.saturating_mul(channels)];
    let mut out_adapter = InterleavedSlice::new_mut(&mut out_samples, channels, needed_out)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    let (_consumed, produced) = resampler
        .process_all_into_buffer(&in_adapter, &mut out_adapter, in_frames, None)
        .map_err(|err| AudioError::Resample(err.to_string()))?;

    let exact_len = target_frames.saturating_mul(channels);
    out_samples.truncate(produced.saturating_mul(channels));
    if out_samples.len() > exact_len {
        out_samples.truncate(exact_len);
    } else if out_samples.len() < exact_len {
        out_samples.resize(exact_len, 0.0);
    }

    Ok(DecodedAudioPcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        interleaved: out_samples,
    })
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn linear_scale_frames(pcm: &DecodedAudioPcm, target_frames: usize) -> DecodedAudioPcm {
    let channels = usize::from(pcm.channels);
    let in_frames = pcm.frame_count().max(1);
    let mut out = Vec::with_capacity(target_frames.saturating_mul(channels));
    for o in 0..target_frames {
        let pos = if target_frames <= 1 {
            0.0
        } else {
            (o as f64) * ((in_frames.saturating_sub(1)) as f64)
                / ((target_frames.saturating_sub(1)) as f64)
        };
        let i0 = (pos.floor() as usize).min(in_frames - 1);
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (pos - (i0 as f64)) as f32;
        for c in 0..channels {
            let s0 = pcm.interleaved[i0 * channels + c];
            let s1 = pcm.interleaved[i1 * channels + c];
            out.push((s1 - s0).mul_add(frac, s0));
        }
    }
    DecodedAudioPcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        interleaved: out,
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn wsola_time_stretch(pcm: &DecodedAudioPcm, playback_rate: PlaybackRate) -> DecodedAudioPcm {
    let channels = usize::from(pcm.channels);
    let in_frames = pcm.frame_count();
    let target_frames = compute_stretched_frame_count(in_frames, playback_rate);
    if in_frames < 128 {
        return linear_scale_frames(pcm, target_frames);
    }

    let win_len = (2048).min(in_frames & !1).max(64);
    let syn_hop = (win_len / 4).max(16);
    let max_delta = (win_len / 4).min(256) as isize;
    let speed = playback_rate.as_f64();

    let hann: Vec<f32> = (0..win_len)
        .map(|k| {
            let phase = (2.0 * std::f64::consts::PI * (k as f64)) / (win_len as f64);
            (0.5 * (1.0 - phase.cos())) as f32
        })
        .collect();
    let mono = downmix_mono(pcm);

    let mut out = vec![0.0_f32; target_frames.saturating_mul(channels)];
    let mut norm = vec![0.0_f32; target_frames];
    let mut prev_ana: usize = 0;

    // 初期ウィンドウ (syn_pos = 0)
    accumulate_window(
        &pcm.interleaved,
        channels,
        in_frames,
        &hann,
        0,
        0,
        &mut out,
        &mut norm,
        target_frames,
    );

    let mut syn_pos = syn_hop;
    while syn_pos < target_frames {
        let nominal_ana = ((syn_pos as f64) * speed).round() as isize;
        let natural_ref = prev_ana.saturating_add(syn_hop);
        let best_ana =
            find_best_overlap_offset(&mono, natural_ref, nominal_ana, win_len / 2, max_delta);

        accumulate_window(
            &pcm.interleaved,
            channels,
            in_frames,
            &hann,
            best_ana,
            syn_pos,
            &mut out,
            &mut norm,
            target_frames,
        );
        prev_ana = best_ana;
        syn_pos += syn_hop;
    }

    for (frame_idx, &w) in norm.iter().enumerate() {
        let inv = if w > 1e-5 { 1.0 / w } else { 1.0 };
        let base = frame_idx * channels;
        for ch in 0..channels {
            out[base + ch] *= inv;
        }
    }

    DecodedAudioPcm {
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        interleaved: out,
    }
}

fn downmix_mono(pcm: &DecodedAudioPcm) -> Vec<f32> {
    let channels = usize::from(pcm.channels).max(1);
    #[allow(clippy::cast_precision_loss)]
    let inv_ch = 1.0 / (channels as f32);
    pcm.interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() * inv_ch)
        .collect()
}

#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn find_best_overlap_offset(
    mono: &[f32],
    natural_ref: usize,
    nominal_ana: isize,
    corr_len: usize,
    max_delta: isize,
) -> usize {
    let in_len = mono.len();
    if natural_ref >= in_len || in_len <= corr_len {
        return usize::try_from(nominal_ana.max(0))
            .unwrap_or(0)
            .min(in_len.saturating_sub(1));
    }

    let valid_corr = corr_len.min(in_len - natural_ref);
    let mut best_score = f32::NEG_INFINITY;
    let mut best_pos = usize::try_from(nominal_ana.clamp(0, (in_len - 1) as isize)).unwrap_or(0);

    let max_start = in_len.saturating_sub(valid_corr) as isize;
    let start_delta = (-max_delta).max(-nominal_ana);
    let end_delta = max_delta.min(max_start - nominal_ana);

    let mut delta = start_delta;
    while delta <= end_delta {
        let cand = usize::try_from(nominal_ana + delta).unwrap_or(0);
        let mut dot = 0.0_f32;
        let mut energy = 1e-6_f32;
        for k in (0..valid_corr).step_by(2) {
            let a = mono[natural_ref + k];
            let b = mono[cand + k];
            dot += a * b;
            energy += b * b;
        }
        let score = dot / energy.sqrt();
        if score > best_score {
            best_score = score;
            best_pos = cand;
        }
        delta += 2;
    }
    best_pos
}

#[allow(clippy::too_many_arguments)]
fn accumulate_window(
    input: &[f32],
    channels: usize,
    in_frames: usize,
    hann: &[f32],
    ana_start: usize,
    syn_start: usize,
    out: &mut [f32],
    norm: &mut [f32],
    target_frames: usize,
) {
    let count = hann
        .len()
        .min(in_frames.saturating_sub(ana_start))
        .min(target_frames.saturating_sub(syn_start));
    for (k, &w) in hann.iter().take(count).enumerate() {
        let in_base = (ana_start + k) * channels;
        let out_base = (syn_start + k) * channels;
        norm[syn_start + k] += w;
        for ch in 0..channels {
            out[out_base + ch] += input[in_base + ch] * w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_pcm(sample_rate: u32, freq_hz: f64, secs: f64, channels: u16) -> DecodedAudioPcm {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let frames = (f64::from(sample_rate) * secs).round() as usize;
        let ch = usize::from(channels);
        let mut interleaved = Vec::with_capacity(frames * ch);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        for i in 0..frames {
            let t = (i as f64) / f64::from(sample_rate);
            let s = ((2.0 * std::f64::consts::PI * freq_hz * t).sin() * 0.6) as f32;
            for _ in 0..ch {
                interleaved.push(s);
            }
        }
        DecodedAudioPcm {
            sample_rate,
            channels,
            interleaved,
        }
    }

    fn estimate_peak_freq_dft(pcm: &DecodedAudioPcm, min_hz: usize, max_hz: usize) -> f64 {
        let ch = usize::from(pcm.channels);
        let mono: Vec<f64> = pcm
            .interleaved
            .chunks_exact(ch)
            .skip(1024)
            .take(8192)
            .map(|f| f64::from(f[0]))
            .collect();
        #[allow(clippy::cast_precision_loss)]
        let n = mono.len() as f64;
        let sr = f64::from(pcm.sample_rate);
        let mut best_freq = 0.0;
        let mut best_mag = -1.0;
        #[allow(clippy::cast_precision_loss)]
        for hz in (min_hz..=max_hz).step_by(5) {
            let f = hz as f64;
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            for (idx, &x) in mono.iter().enumerate() {
                let angle = 2.0 * std::f64::consts::PI * f * (idx as f64) / sr;
                let (sin_a, cos_a) = angle.sin_cos();
                re += x * cos_a;
                im -= x * sin_a;
            }
            let mag = re.hypot(im) / n;
            if mag > best_mag {
                best_mag = mag;
                best_freq = f;
            }
        }
        best_freq
    }

    #[test]
    fn wsola_preserves_pitch_while_changing_duration_and_rubato_shifts_pitch()
    -> Result<(), Box<dyn std::error::Error>> {
        let pcm = sine_pcm(48_000, 440.0, 1.5, 2);
        let rate_1_5x = PlaybackRate::new(3, 2)?;

        let pitch_kept = time_stretch_pcm(&pcm, rate_1_5x, true)?;
        assert_eq!(pitch_kept.frame_count(), 48_000);
        let freq_kept = estimate_peak_freq_dft(&pitch_kept, 200, 900);
        assert!(
            (freq_kept - 440.0).abs() <= 15.0,
            "expected preserved ~440 Hz, got {freq_kept}"
        );

        let varispeed = time_stretch_pcm(&pcm, rate_1_5x, false)?;
        assert_eq!(varispeed.frame_count(), 48_000);
        let freq_shifted = estimate_peak_freq_dft(&varispeed, 200, 900);
        assert!(
            (freq_shifted - 660.0).abs() <= 15.0,
            "expected shifted ~660 Hz, got {freq_shifted}"
        );

        let rate_half = PlaybackRate::new(1, 2)?;
        let slowed = time_stretch_pcm(&pcm, rate_half, true)?;
        assert_eq!(slowed.frame_count(), 144_000);
        let freq_slowed = estimate_peak_freq_dft(&slowed, 200, 900);
        assert!(
            (freq_slowed - 440.0).abs() <= 15.0,
            "expected slow ~440 Hz, got {freq_slowed}"
        );
        Ok(())
    }
}
