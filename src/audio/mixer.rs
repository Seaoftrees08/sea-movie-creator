//! タイムライン音声クリップの合成、`PlaybackRate` 時間伸縮、アニメーション音量/パン評価、ソフトリミッタ。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::audio::playback::resample_interleaved_pcm;
use crate::audio::plugins::isolated_host::IsolatedAudioPluginRack;
use crate::audio::properties::{db_to_linear_f64, evaluate_audio_params, soft_limit_sample};
use crate::audio::time_stretch::time_stretch_pcm;
use crate::audio::waveform::{WaveformCache, WaveformPeaks};
use crate::core::animation::EvaluationContext;
use crate::core::ids::{AssetId, ClipId};
use crate::core::time::{FrameCount, FrameRate, PlaybackRate, RationalDuration};
use crate::media::decoder::DecodedAudioPcm;
use crate::timeline::document::{AudioProperties, Clip, ClipKind, ProjectDocument};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreparedAudioKey {
    asset_id: AssetId,
    target_sample_rate: u32,
    rate_num: u32,
    rate_den: u32,
    preserve_pitch: bool,
}

/// デコード済み素材 PCM を保持し、タイムライン任意サンプル区間をステレオミックスするミキサ。
#[derive(Debug, Default)]
pub struct AudioMixer {
    sources: HashMap<AssetId, Arc<DecodedAudioPcm>>,
    prepared_cache: Mutex<HashMap<PreparedAudioKey, Arc<DecodedAudioPcm>>>,
    waveform_cache: Mutex<WaveformCache>,
    plugin_rack: Mutex<IsolatedAudioPluginRack>,
    plugin_host_override: Option<PathBuf>,
}

impl Clone for AudioMixer {
    fn clone(&self) -> Self {
        let mut rack = IsolatedAudioPluginRack::new();
        rack.set_host_executable_override(self.plugin_host_override.clone());
        Self {
            sources: self.sources.clone(),
            prepared_cache: Mutex::new(HashMap::new()),
            waveform_cache: Mutex::new(WaveformCache::new()),
            plugin_rack: Mutex::new(rack),
            plugin_host_override: self.plugin_host_override.clone(),
        }
    }
}

impl AudioMixer {
    /// 空のミキサを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// テスト／検証用に `sea-audio-host` 実行ファイルパスを明示指定する。
    pub fn set_plugin_host_executable_override(&mut self, path: Option<PathBuf>) {
        self.plugin_host_override.clone_from(&path);
        if let Ok(mut rack) = self.plugin_rack.lock() {
            rack.set_host_executable_override(path);
        }
    }

    /// 指定クリップ・プラグインスロットが異常検知等でバイパス状態にあるか返す。
    #[must_use]
    pub fn is_plugin_slot_bypassed(&self, clip_id: &ClipId, slot_id: &str) -> bool {
        self.plugin_rack
            .lock()
            .is_ok_and(|rack| rack.is_slot_bypassed(clip_id, slot_id))
    }

    /// 指定クリップ・プラグインスロットで稼働中の外部ワーカー `PID` を返す。
    #[must_use]
    pub fn plugin_worker_pid(&self, clip_id: &ClipId, slot_id: &str) -> Option<u32> {
        self.plugin_rack
            .lock()
            .ok()
            .and_then(|rack| rack.worker_pid(clip_id, slot_id))
    }

    /// 指定クリップ・プラグインスロットの直近エラーメッセージを返す。
    #[must_use]
    pub fn plugin_slot_last_error(&self, clip_id: &ClipId, slot_id: &str) -> Option<String> {
        self.plugin_rack
            .lock()
            .ok()
            .and_then(|rack| rack.slot_last_error(clip_id, slot_id).map(str::to_owned))
    }

    /// 手動復旧用にバイパス状態となった全プラグインホストを破棄して再接続を許可する。
    pub fn reset_bypassed_plugin_slots(&self) {
        if let Ok(mut rack) = self.plugin_rack.lock() {
            rack.reset_bypassed_slots();
        }
    }

    /// アセット ID にデコード済み PCM を関連付ける。既存キャッシュがあれば破棄する。
    pub fn register_pcm(&mut self, asset_id: AssetId, pcm: Arc<DecodedAudioPcm>) {
        if let Ok(mut guard) = self.prepared_cache.lock() {
            guard.retain(|k, _| k.asset_id != asset_id);
        }
        if let Ok(mut wf) = self.waveform_cache.lock() {
            wf.invalidate_asset(&asset_id);
        }
        self.sources.insert(asset_id, pcm);
    }

    /// 登録済み素材の元 PCM 参照を返す。
    #[must_use]
    pub fn get_pcm(&self, asset_id: &AssetId) -> Option<&Arc<DecodedAudioPcm>> {
        self.sources.get(asset_id)
    }

    /// 指定アセット・ビン幅の波形ピークキャッシュを取得または生成する。
    #[must_use]
    pub fn waveform_peaks(
        &self,
        asset_id: &AssetId,
        samples_per_peak: usize,
    ) -> Option<Arc<WaveformPeaks>> {
        let raw = self.sources.get(asset_id)?;
        let mut wf = self.waveform_cache.lock().ok()?;
        Some(wf.get_or_build(asset_id, raw, samples_per_peak))
    }

    /// タイムライン上の指定サンプルウィンドウをステレオ `f32` (`L, R` 交互) へミックスする。
    ///
    /// 各クリップの [`PlaybackRate`] (`preserve_pitch` 有無)、アニメーション音量/パン (`dB`, `pan`)、
    /// レイヤーミュートおよびソフトリミッタを適用して `|sample| <= 1.0` を保証する。
    pub fn mix_timeline_window(
        &self,
        doc: &ProjectDocument,
        timeline_start_sample: u64,
        output_frames: usize,
        target_sample_rate: u32,
        output_stereo: &mut Vec<f32>,
    ) {
        let needed = output_frames.saturating_mul(2);
        output_stereo.clear();
        output_stereo.resize(needed, 0.0);

        if output_frames == 0 || target_sample_rate == 0 {
            return;
        }

        let sr_f64 = f64::from(target_sample_rate);
        let fps_f64 = doc.fps.as_f64();
        #[allow(clippy::cast_precision_loss)]
        let win_start_sec = (timeline_start_sample as f64) / sr_f64;
        #[allow(clippy::cast_precision_loss)]
        let win_end_sec =
            ((timeline_start_sample.saturating_add(output_frames as u64)) as f64) / sr_f64;

        for layer in &doc.layers {
            if layer.muted {
                continue;
            }
            for clip in &layer.clips {
                let Some(binding) = extract_audio_binding(clip) else {
                    continue;
                };
                if binding.props.muted {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)]
                let clip_start_sec = (clip.timeline_start.get() as f64) / fps_f64;
                #[allow(clippy::cast_precision_loss)]
                let clip_end_sec = ((clip
                    .timeline_start
                    .get()
                    .saturating_add(clip.duration.get()))
                    as f64)
                    / fps_f64;
                if clip_end_sec <= win_start_sec || clip_start_sec >= win_end_sec {
                    continue;
                }
                let Some(pcm) = self.get_or_prepare_pcm(
                    binding.asset_id,
                    target_sample_rate,
                    binding.playback_rate,
                    binding.preserve_pitch,
                ) else {
                    continue;
                };
                if pcm.interleaved.is_empty() || pcm.sample_rate == 0 {
                    continue;
                }
                let has_enabled_plugin = binding.props.plugin_slots.iter().any(|s| s.enabled);
                if has_enabled_plugin {
                    let mut clip_buf = vec![0.0_f32; needed];
                    mix_clip_into_window(
                        clip,
                        &binding,
                        &pcm,
                        doc.fps,
                        target_sample_rate,
                        win_start_sec,
                        output_frames,
                        &mut clip_buf,
                    );
                    let eval_ctx =
                        build_window_eval_context(clip, doc.fps, win_start_sec, clip_start_sec);
                    if let Ok(mut rack) = self.plugin_rack.lock() {
                        rack.process_clip_slots(
                            &clip.id,
                            &binding.props.plugin_slots,
                            eval_ctx,
                            target_sample_rate,
                            &mut clip_buf,
                        );
                    }
                    for (dst, src) in output_stereo.iter_mut().zip(clip_buf.iter().copied()) {
                        *dst += src;
                    }
                } else {
                    mix_clip_into_window(
                        clip,
                        &binding,
                        &pcm,
                        doc.fps,
                        target_sample_rate,
                        win_start_sec,
                        output_frames,
                        output_stereo,
                    );
                }
            }
        }

        for sample in output_stereo.iter_mut() {
            *sample = soft_limit_sample(*sample);
        }
    }

    fn get_or_prepare_pcm(
        &self,
        asset_id: &AssetId,
        target_sample_rate: u32,
        playback_rate: PlaybackRate,
        preserve_pitch: bool,
    ) -> Option<Arc<DecodedAudioPcm>> {
        let key = PreparedAudioKey {
            asset_id: asset_id.clone(),
            target_sample_rate,
            rate_num: playback_rate.numerator(),
            rate_den: playback_rate.denominator(),
            preserve_pitch,
        };
        if let Ok(guard) = self.prepared_cache.lock()
            && let Some(hit) = guard.get(&key)
        {
            return Some(Arc::clone(hit));
        }

        let raw = self.sources.get(asset_id)?;
        let sr_matched = if raw.sample_rate == target_sample_rate {
            Arc::clone(raw)
        } else {
            let resampled = resample_interleaved_pcm(raw, target_sample_rate).ok()?;
            Arc::new(resampled)
        };

        let final_pcm = if playback_rate == PlaybackRate::NORMAL {
            sr_matched
        } else {
            let stretched = time_stretch_pcm(&sr_matched, playback_rate, preserve_pitch).ok()?;
            Arc::new(stretched)
        };

        if let Ok(mut guard) = self.prepared_cache.lock() {
            guard.insert(key, Arc::clone(&final_pcm));
        }
        Some(final_pcm)
    }
}

struct AudioClipBinding<'a> {
    asset_id: &'a AssetId,
    source_in: RationalDuration,
    playback_rate: PlaybackRate,
    preserve_pitch: bool,
    props: &'a AudioProperties,
}

const fn extract_audio_binding(clip: &Clip) -> Option<AudioClipBinding<'_>> {
    match &clip.kind {
        ClipKind::Audio {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            audio,
        }
        | ClipKind::Video {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            audio: Some(audio),
            ..
        } => Some(AudioClipBinding {
            asset_id,
            source_in: *source_in,
            playback_rate: *playback_rate,
            preserve_pitch: *preserve_pitch,
            props: audio,
        }),
        ClipKind::Video { audio: None, .. } | ClipKind::Image { .. } | ClipKind::Subtitle(_) => {
            None
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
fn mix_clip_into_window(
    clip: &Clip,
    binding: &AudioClipBinding<'_>,
    pcm: &DecodedAudioPcm,
    fps: FrameRate,
    target_sample_rate: u32,
    win_start_sec: f64,
    output_frames: usize,
    output_stereo: &mut [f32],
) {
    const SUBBLOCK: usize = 32;

    let sr_f64 = f64::from(target_sample_rate);
    let fps_f64 = fps.as_f64();
    let clip_start_sec = (clip.timeline_start.get() as f64) / fps_f64;
    let clip_end_sec = ((clip
        .timeline_start
        .get()
        .saturating_add(clip.duration.get())) as f64)
        / fps_f64;
    let speed = binding.playback_rate.as_f64();
    let stretched_in_sec = binding.source_in.as_seconds_f64() / speed;
    let effect_dur = clip.effect_duration_frames.unwrap_or(clip.duration);

    let mut sub_start = 0;
    while sub_start < output_frames {
        let sub_end = (sub_start + SUBBLOCK).min(output_frames);
        let t0 = win_start_sec + (sub_start as f64) / sr_f64;
        let t1 = win_start_sec + ((sub_end - 1) as f64) / sr_f64;
        let local0 = (t0 - clip_start_sec).max(0.0);
        let local1 = (t1 - clip_start_sec).max(0.0);

        let (g0_l, g0_r) = evaluate_stereo_gains(clip, binding.props, fps, effect_dur, local0);
        let (g1_l, g1_r) = evaluate_stereo_gains(clip, binding.props, fps, effect_dur, local1);
        let span = ((sub_end - sub_start).saturating_sub(1)).max(1) as f32;

        for idx in sub_start..sub_end {
            let tl_sec = win_start_sec + (idx as f64) / sr_f64;
            if tl_sec < clip_start_sec || tl_sec >= clip_end_sec {
                continue;
            }
            let src_sec = stretched_in_sec + (tl_sec - clip_start_sec);
            if src_sec < 0.0 {
                continue;
            }
            let frac = ((idx - sub_start) as f32) / span;
            let gl = g0_l + (g1_l - g0_l) * frac;
            let gr = g0_r + (g1_r - g0_r) * frac;
            let src_pos = src_sec * f64::from(pcm.sample_rate);
            let (left, right) = sample_pcm_linear(pcm, src_pos);
            let base = idx * 2;
            output_stereo[base] += left * gl;
            output_stereo[base + 1] += right * gr;
        }
        sub_start = sub_end;
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn build_window_eval_context(
    clip: &Clip,
    fps: FrameRate,
    win_start_sec: f64,
    clip_start_sec: f64,
) -> EvaluationContext {
    let local_sec = (win_start_sec - clip_start_sec).max(0.0);
    let local_frame = (local_sec * fps.as_f64()).round().max(0.0) as u64;
    let effect_dur = clip.effect_duration_frames.unwrap_or(clip.duration);
    EvaluationContext::new(local_frame, clip.effect_offset_frames, effect_dur, fps)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn evaluate_stereo_gains(
    clip: &Clip,
    props: &AudioProperties,
    fps: FrameRate,
    effect_duration: FrameCount,
    local_sec: f64,
) -> (f32, f32) {
    let local_frame = (local_sec * fps.as_f64()).round().max(0.0) as u64;
    let ctx = EvaluationContext::new(local_frame, clip.effect_offset_frames, effect_duration, fps);
    match evaluate_audio_params(props, ctx) {
        Ok(eval) => (eval.gain_l as f32, eval.gain_r as f32),
        Err(_) => (1.0, 1.0),
    }
}

/// `dB` 値をリニア振幅倍率へ変換する（`-96 dB` 以下は無音 `0.0`）。
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn db_to_linear(db: f32) -> f32 {
    db_to_linear_f64(f64::from(db)) as f32
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn sample_pcm_linear(pcm: &DecodedAudioPcm, pos_frames: f64) -> (f32, f32) {
    let total_frames = pcm.frame_count();
    if total_frames == 0 || !pos_frames.is_finite() || pos_frames < 0.0 {
        return (0.0, 0.0);
    }
    let idx0 = pos_frames.floor() as usize;
    if idx0 >= total_frames {
        return (0.0, 0.0);
    }
    let frac = (pos_frames - (idx0 as f64)) as f32;
    let idx1 = (idx0 + 1).min(total_frames - 1);
    let ch = usize::from(pcm.channels);

    let (l0, r0) = read_frame_lr(&pcm.interleaved, ch, idx0);
    let (l1, r1) = read_frame_lr(&pcm.interleaved, ch, idx1);
    let l = (l1 - l0).mul_add(frac, l0);
    let r = (r1 - r0).mul_add(frac, r0);
    (l, r)
}

fn read_frame_lr(buf: &[f32], channels: usize, frame_idx: usize) -> (f32, f32) {
    let base = frame_idx * channels;
    match channels {
        0 => (0.0, 0.0),
        1 => {
            let m = buf[base];
            (m, m)
        }
        _ => (buf[base], buf[base + 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::AnimatedParameter;
    use crate::core::ids::{ClipId, LayerId};
    use crate::core::time::{FrameIndex, RationalDuration};
    use crate::timeline::document::Layer;

    #[test]
    fn mixer_applies_gain_pan_and_layer_mute() -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("AudioTest");
        doc.fps = FrameRate::FPS_30;
        let asset_id = AssetId::new("a1")?;
        let mut layer = Layer::new(LayerId::new("l1")?, "Audio");

        let props = AudioProperties {
            gain_db: AnimatedParameter::constant(-6.0),
            pan: AnimatedParameter::constant(1.0),
            muted: false,
            plugin_slots: Vec::new(),
        };
        layer.clips.push(Clip {
            id: ClipId::new("c1")?,
            name: "Tone".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(30)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Audio {
                asset_id: asset_id.clone(),
                source_in: RationalDuration::ZERO,
                playback_rate: PlaybackRate::NORMAL,
                preserve_pitch: true,
                audio: props,
            },
        });
        doc.layers.push(layer);

        let pcm = DecodedAudioPcm {
            sample_rate: 48_000,
            channels: 1,
            interleaved: vec![1.0; 48_000],
        };
        let mut mixer = AudioMixer::new();
        mixer.register_pcm(asset_id, Arc::new(pcm));

        let mut out = Vec::new();
        mixer.mix_timeline_window(&doc, 0, 128, 48_000, &mut out);
        assert_eq!(out.len(), 256);
        let expected_r = db_to_linear(-6.0);
        assert!(out[0].abs() < 1e-6);
        assert!((out[1] - expected_r).abs() < 1e-4);

        doc.layers[0].muted = true;
        mixer.mix_timeline_window(&doc, 0, 128, 48_000, &mut out);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
        Ok(())
    }

    #[test]
    fn mixer_soft_limiter_bounds_overlapping_loud_clips() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut doc = ProjectDocument::new("LimiterTest");
        doc.fps = FrameRate::FPS_30;
        let asset_id = AssetId::new("loud")?;
        for idx in 0..2 {
            let mut layer = Layer::new(LayerId::new(format!("l{idx}"))?, "Loud");
            layer.clips.push(Clip {
                id: ClipId::new(format!("c{idx}"))?,
                name: "Loud".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: FrameCount::new(30)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                kind: ClipKind::Audio {
                    asset_id: asset_id.clone(),
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::NORMAL,
                    preserve_pitch: true,
                    audio: AudioProperties {
                        gain_db: AnimatedParameter::constant(6.0),
                        ..AudioProperties::default()
                    },
                },
            });
            doc.layers.push(layer);
        }

        let mut mixer = AudioMixer::new();
        mixer.register_pcm(
            asset_id,
            Arc::new(DecodedAudioPcm {
                sample_rate: 48_000,
                channels: 2,
                interleaved: vec![0.95; 48_000 * 2],
            }),
        );
        let mut out = Vec::new();
        mixer.mix_timeline_window(&doc, 0, 256, 48_000, &mut out);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert!(out[0] >= 0.9 && out[0] <= 1.0);
        Ok(())
    }
}
