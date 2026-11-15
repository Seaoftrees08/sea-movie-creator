//! マルチレイヤータイムライン音声の最小ミキシング（ゲイン・パン・ミュート・速度反映）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::core::ids::AssetId;
use crate::core::time::{FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration};
use crate::media::decoder::DecodedAudioPcm;
use crate::timeline::document::{AudioProperties, Clip, ClipKind, ProjectDocument};

const SILENCE_FLOOR_DB: f32 = -96.0;
const EVAL_BLOCK_SAMPLES: usize = 64;

/// タイムライン上の複数音声クリップを指定サンプル範囲でステレオ `f32` にミックスする器。
#[derive(Debug, Default, Clone)]
pub struct AudioMixer {
    assets: HashMap<AssetId, Arc<DecodedAudioPcm>>,
}

impl AudioMixer {
    /// 空のミキサを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// デコード済み PCM データをアセット ID に紐づけて登録する。
    pub fn register_pcm(&mut self, asset_id: AssetId, pcm: Arc<DecodedAudioPcm>) {
        self.assets.insert(asset_id, pcm);
    }

    /// 指定アセットの登録を削除する。
    pub fn remove_asset(&mut self, asset_id: &AssetId) {
        self.assets.remove(asset_id);
    }

    /// 登録済みアセット数を返す。
    #[must_use]
    pub fn registered_asset_count(&self) -> usize {
        self.assets.len()
    }

    /// タイムライン上の連続サンプル区間 `[start_sample .. start_sample + frame_count)` を
    /// ステレオインターリーブ `f32` (`L, R, L, R, ...`) として合成する。
    ///
    /// ゲインはブロック先頭時刻で評価し、パンは `-1.0..=1.0` のステレオバランスで適用する。
    pub fn mix_timeline_window(
        &self,
        document: &ProjectDocument,
        start_sample: u64,
        frame_count: usize,
        output_sample_rate: u32,
        out_stereo_interleaved: &mut Vec<f32>,
    ) {
        out_stereo_interleaved.clear();
        out_stereo_interleaved.resize(frame_count.saturating_mul(2), 0.0_f32);
        if frame_count == 0 || output_sample_rate == 0 {
            return;
        }

        for layer in &document.layers {
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
                let Some(pcm) = self.assets.get(&binding.asset_id) else {
                    continue;
                };
                mix_clip_into_buffer(
                    clip,
                    &binding,
                    pcm,
                    document.fps,
                    start_sample,
                    frame_count,
                    output_sample_rate,
                    out_stereo_interleaved,
                );
            }
        }
    }
}

struct ClipAudioBinding<'a> {
    asset_id: AssetId,
    source_in: RationalDuration,
    playback_rate: PlaybackRate,
    props: &'a AudioProperties,
}

fn extract_audio_binding(clip: &Clip) -> Option<ClipAudioBinding<'_>> {
    match &clip.kind {
        ClipKind::Audio {
            asset_id,
            source_in,
            playback_rate,
            audio,
            ..
        }
        | ClipKind::Video {
            asset_id,
            source_in,
            playback_rate,
            audio: Some(audio),
            ..
        } => Some(ClipAudioBinding {
            asset_id: asset_id.clone(),
            source_in: *source_in,
            playback_rate: *playback_rate,
            props: audio,
        }),
        ClipKind::Video { audio: None, .. } | ClipKind::Image { .. } | ClipKind::Subtitle(_) => {
            None
        }
    }
}

struct MixWindow {
    start_sample: u64,
    frame_count: usize,
    output_sample_rate: u32,
}

#[allow(clippy::too_many_arguments)]
fn mix_clip_into_buffer(
    clip: &Clip,
    binding: &ClipAudioBinding<'_>,
    pcm: &DecodedAudioPcm,
    fps: FrameRate,
    start_sample: u64,
    frame_count: usize,
    output_sample_rate: u32,
    out_stereo: &mut [f32],
) {
    let win = MixWindow {
        start_sample,
        frame_count,
        output_sample_rate,
    };
    let clip_start_sec = frames_to_seconds_f64(clip.timeline_start.get(), fps);
    let clip_end_sec = frames_to_seconds_f64(
        clip.timeline_start
            .get()
            .saturating_add(clip.duration.get()),
        fps,
    );

    let out_rate_f64 = f64::from(win.output_sample_rate);
    #[allow(clippy::cast_precision_loss)]
    let win_start_sec = (win.start_sample as f64) / out_rate_f64;
    #[allow(clippy::cast_precision_loss)]
    let win_end_sec =
        ((win.start_sample.saturating_add(win.frame_count as u64)) as f64) / out_rate_f64;
    if clip_end_sec <= win_start_sec || clip_start_sec >= win_end_sec {
        return;
    }

    let pcm_frames = pcm.frame_count();
    if pcm_frames == 0 || pcm.sample_rate == 0 {
        return;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let local_begin = ((clip_start_sec - win_start_sec).max(0.0) * out_rate_f64).floor() as usize;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let local_end = ((clip_end_sec - win_start_sec).max(0.0) * out_rate_f64)
        .ceil()
        .min(win.frame_count as f64) as usize;
    if local_begin >= local_end {
        return;
    }

    let rate_scale = binding.playback_rate.as_f64();
    let src_in_sec = binding.source_in.as_seconds_f64();
    let src_rate_f64 = f64::from(pcm.sample_rate);

    let mut idx = local_begin;
    while idx < local_end {
        let block_end = (idx + EVAL_BLOCK_SAMPLES).min(local_end);
        #[allow(clippy::cast_precision_loss)]
        let block_tl_sec = (win.start_sample.saturating_add(idx as u64) as f64) / out_rate_f64;
        let (gain_l, gain_r) =
            evaluate_stereo_gains(clip, binding.props, fps, block_tl_sec - clip_start_sec);

        for sample_idx in idx..block_end {
            #[allow(clippy::cast_precision_loss)]
            let tl_sec = (win.start_sample.saturating_add(sample_idx as u64) as f64) / out_rate_f64;
            if tl_sec < clip_start_sec || tl_sec >= clip_end_sec {
                continue;
            }
            let src_sec = src_in_sec + (tl_sec - clip_start_sec) * rate_scale;
            if src_sec < 0.0 {
                continue;
            }
            let src_pos = src_sec * src_rate_f64;
            let (left, right) = sample_pcm_linear(pcm, src_pos);
            let out_off = sample_idx * 2;
            out_stereo[out_off] += left * gain_l;
            out_stereo[out_off + 1] += right * gain_r;
        }
        idx = block_end;
    }
}

fn evaluate_stereo_gains(
    clip: &Clip,
    props: &AudioProperties,
    fps: FrameRate,
    elapsed_in_clip_sec: f64,
) -> (f32, f32) {
    let fps_f64 = fps.as_f64();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let offset_frames = (elapsed_in_clip_sec.max(0.0) * fps_f64).floor() as u64;
    let clamped_frame = offset_frames.min(clip.duration.get().saturating_sub(1));
    let tl_frame = FrameIndex::new(clip.timeline_start.get().saturating_add(clamped_frame));
    let ctx = clip.evaluation_context(tl_frame, fps).unwrap_or_else(|| {
        crate::core::animation::EvaluationContext::new(
            0,
            clip.effect_offset_frames,
            FrameCount::ONE,
            fps,
        )
    });

    #[allow(clippy::cast_possible_truncation)]
    let gain_db = props.gain_db.evaluate(ctx).unwrap_or(0.0) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let pan = props.pan.evaluate(ctx).unwrap_or(0.0).clamp(-1.0, 1.0) as f32;

    let amp = db_to_linear(gain_db);
    let balance_l = (1.0_f32 - pan).clamp(0.0_f32, 1.0_f32);
    let balance_r = (1.0_f32 + pan).clamp(0.0_f32, 1.0_f32);
    (amp * balance_l, amp * balance_r)
}

/// dB 値（`-96 dB` 以下は無音、`0 dB` で等倍 `1.0`）をリニア振幅倍率に変換する。
#[must_use]
pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() || db <= SILENCE_FLOOR_DB {
        return 0.0;
    }
    10.0_f32.powf(db / 20.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn sample_pcm_linear(pcm: &DecodedAudioPcm, pos_frames: f64) -> (f32, f32) {
    let frame_count = pcm.frame_count();
    if frame_count == 0 || pos_frames < 0.0 || !pos_frames.is_finite() {
        return (0.0, 0.0);
    }
    let idx0 = pos_frames.floor() as usize;
    if idx0 >= frame_count {
        return (0.0, 0.0);
    }
    let frac = (pos_frames - (idx0 as f64)) as f32;
    let idx1 = (idx0 + 1).min(frame_count - 1);

    let (l0, r0) = read_stereo_frame(pcm, idx0);
    let (l1, r1) = read_stereo_frame(pcm, idx1);
    let l = l0 + (l1 - l0) * frac;
    let r = r0 + (r1 - r0) * frac;
    (l, r)
}

fn read_stereo_frame(pcm: &DecodedAudioPcm, frame_idx: usize) -> (f32, f32) {
    let ch = usize::from(pcm.channels);
    if ch == 0 {
        return (0.0, 0.0);
    }
    let base = frame_idx * ch;
    let l = pcm.interleaved.get(base).copied().unwrap_or(0.0);
    let r = if ch >= 2 {
        pcm.interleaved.get(base + 1).copied().unwrap_or(l)
    } else {
        l
    };
    (l, r)
}

fn frames_to_seconds_f64(frames: u64, fps: FrameRate) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let f = frames as f64;
    f * f64::from(fps.denominator()) / f64::from(fps.numerator().max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::AnimatedParameter;
    use crate::core::ids::{ClipId, LayerId};
    use crate::timeline::document::Layer;

    fn sine_pcm(sample_rate: u32, frames: usize, value: f32) -> Arc<DecodedAudioPcm> {
        Arc::new(DecodedAudioPcm {
            sample_rate,
            channels: 1,
            interleaved: vec![value; frames],
        })
    }

    #[test]
    fn mixer_applies_gain_pan_and_layer_mute() -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("AudioTest");
        doc.fps = FrameRate::FPS_60;
        let asset_id = AssetId::new("asset-tone")?;
        let mut layer = Layer::new(LayerId::new("layer-audio")?, "AudioTrack");

        // -6 dB ≈ 0.501187、pan = +1.0 (右のみ) -> 左=0.0, 右≈0.501
        let props = AudioProperties {
            gain_db: AnimatedParameter::constant(-6.0),
            pan: AnimatedParameter::constant(1.0),
            muted: false,
        };
        let clip_id = ClipId::new("clip-tone")?;
        layer.clips.push(Clip {
            id: clip_id,
            name: "Tone".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(60)?,
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

        let mut mixer = AudioMixer::new();
        mixer.register_pcm(asset_id, sine_pcm(48_000, 48_000, 1.0));

        let mut out = Vec::new();
        mixer.mix_timeline_window(&doc, 0, 128, 48_000, &mut out);
        assert_eq!(out.len(), 256);
        assert!(out[0].abs() < 1e-6);
        let expected_r = db_to_linear(-6.0);
        assert!((out[1] - expected_r).abs() < 1e-4);

        // レイヤーミュート時は完全に無音になる。
        doc.layers[0].muted = true;
        mixer.mix_timeline_window(&doc, 0, 128, 48_000, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        Ok(())
    }
}
