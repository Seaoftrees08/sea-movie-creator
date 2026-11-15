//! 速度変更・アニメ音量/パン・クリップ分割連続性・波形キャッシュ・ソフトリミッタ結合テスト。

use std::sync::Arc;

use sea_movie_creator::audio::mixer::AudioMixer;
use sea_movie_creator::core::animation::{AnimatedParameter, EasingCurve, ExtrapolationMode};
use sea_movie_creator::core::ids::{AssetRef, ClipRef, LayerRef};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::decoder::DecodedAudioPcm;
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AudioProperties, ProjectDocument,
};

fn make_sine_stereo(sample_rate: u32, freq_hz: f64, secs: f64, amp: f32) -> DecodedAudioPcm {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let frames = (f64::from(sample_rate) * secs).round() as usize;
    let mut interleaved = Vec::with_capacity(frames * 2);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    for i in 0..frames {
        let t = (i as f64) / f64::from(sample_rate);
        let s = ((2.0 * std::f64::consts::PI * freq_hz * t).sin() as f32) * amp;
        interleaved.push(s);
        interleaved.push(s);
    }
    DecodedAudioPcm {
        sample_rate,
        channels: 2,
        interleaved,
    }
}

fn channel_rms(stereo: &[f32], channel: usize) -> f64 {
    let pairs = stereo.as_chunks::<2>().0;
    if pairs.is_empty() {
        return 0.0;
    }
    let sum: f64 = pairs
        .iter()
        .map(|pair| {
            let v = f64::from(pair[channel]);
            v * v
        })
        .sum();
    #[allow(clippy::cast_precision_loss)]
    {
        (sum / (pairs.len() as f64)).sqrt()
    }
}

fn dominant_freq_hz(stereo: &[f32], sample_rate: u32, min_hz: usize, max_hz: usize) -> f64 {
    let mono: Vec<f64> = stereo
        .as_chunks::<2>()
        .0
        .iter()
        .take(8192)
        .map(|lr| f64::midpoint(f64::from(lr[0]), f64::from(lr[1])))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let n = mono.len().max(1) as f64;
    let sr = f64::from(sample_rate);
    let mut best_freq = 0.0;
    let mut best_mag = -1.0;

    #[allow(clippy::cast_precision_loss)]
    for hz in (min_hz..=max_hz).step_by(5) {
        let f = hz as f64;
        let (mut re, mut im) = (0.0_f64, 0.0_f64);
        for (i, &x) in mono.iter().enumerate() {
            let angle = 2.0 * std::f64::consts::PI * f * (i as f64) / sr;
            let (s, c) = angle.sin_cos();
            re += x * c;
            im -= x * s;
        }
        let mag = re.hypot(im) / n;
        if mag > best_mag {
            best_mag = mag;
            best_freq = f;
        }
    }
    best_freq
}

fn build_stretched_fade_doc()
-> Result<(ProjectDocument, sea_movie_creator::core::ids::AssetId), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("PhaseC-3 Audio Mix");
    doc.fps = FrameRate::FPS_30;
    let mut dispatcher = CommandDispatcher::new();

    let report = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddAsset {
                    temp_id: Some("a_tone".to_owned()),
                    id: None,
                    name: "tone440.wav".to_owned(),
                    path: AssetPath::from_absolute("C:/audio/tone440.wav"),
                    kind: AssetKind::Audio {
                        sample_rate_hz: 48_000,
                        channels: 2,
                        source_duration: RationalDuration::new(6, 1)?,
                    },
                },
                EditCommand::AddLayer {
                    temp_id: Some("l_audio".to_owned()),
                    id: None,
                    name: "BGM".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c_bgm".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l_audio".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "BGM 1.5x".to_owned(),
                        timeline_start: FrameIndex::ZERO,
                        duration: FrameCount::new(90)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: NewClipKind::Audio {
                            asset_id: AssetRef::Temp("a_tone".to_owned()),
                            source_in: RationalDuration::ZERO,
                            playback_rate: PlaybackRate::new(3, 2)?,
                            preserve_pitch: true,
                            audio: AudioProperties::default(),
                        },
                    }),
                },
                EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Temp("c_bgm".to_owned()),
                    },
                    path: ParameterPath::GainDb,
                    value: ParameterValue::Animated {
                        param: AnimatedParameter::Easing {
                            start_value: -24.0,
                            end_value: 0.0,
                            curve: EasingCurve::Linear,
                            extrapolation: ExtrapolationMode::Clamp,
                        },
                    },
                },
                EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Temp("c_bgm".to_owned()),
                    },
                    path: ParameterPath::Pan,
                    value: ParameterValue::Animated {
                        param: AnimatedParameter::expression("-1.0 + p * 2.0"),
                    },
                },
                EditCommand::SplitClip {
                    clip_id: ClipRef::Temp("c_bgm".to_owned()),
                    split_at_timeline_frame: FrameIndex::new(45),
                    right_temp_id: Some("c_bgm_r".to_owned()),
                    right_id: None,
                },
            ],
        },
    )?;
    Ok((doc, report.created_assets["a_tone"].clone()))
}

#[test]
fn playback_rate_stretch_animated_gain_pan_split_continuity_and_waveform()
-> Result<(), Box<dyn std::error::Error>> {
    let (doc, asset_id) = build_stretched_fade_doc()?;
    let mut mixer = AudioMixer::new();
    mixer.register_pcm(
        asset_id.clone(),
        Arc::new(make_sine_stereo(48_000, 440.0, 6.0, 0.7)),
    );

    // 開始寄り (0.1s..0.3s): -24dB に近くかつ左パン優位
    let mut early = Vec::new();
    mixer.mix_timeline_window(&doc, 4_800, 9_600, 48_000, &mut early);
    let early_l = channel_rms(&early, 0);
    let early_r = channel_rms(&early, 1);
    assert!(early_l > early_r * 1.5);

    // 終端寄り (2.5s..2.7s、分割後右クリップ): 0dB に近く右パン優位かつ音量増加
    let mut late = Vec::new();
    mixer.mix_timeline_window(&doc, 120_000, 9_600, 48_000, &mut late);
    let late_l = channel_rms(&late, 0);
    let late_r = channel_rms(&late, 1);
    assert!(late_r > late_l * 1.5);
    assert!(late_r > early_l * 4.0);

    // 分割境界 (1.2s..1.8s) でも 1.5x 再生かつピッチ保持 (~440 Hz)
    let mut across_split = Vec::new();
    mixer.mix_timeline_window(&doc, 57_600, 28_800, 48_000, &mut across_split);
    assert!(across_split.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    let pitch = dominant_freq_hz(&across_split, 48_000, 200, 900);
    assert!(
        (pitch - 440.0).abs() <= 15.0,
        "expected ~440Hz, got {pitch}"
    );

    // 波形ピークキャッシュ
    let peaks = mixer
        .waveform_peaks(&asset_id, 480)
        .ok_or("waveform peaks missing")?;
    assert_eq!(peaks.bins.len(), 600);
    assert!(peaks.bins[10].max > 0.6 && peaks.bins[10].min < -0.6);
    Ok(())
}

#[test]
fn varispeed_without_pitch_preserve_shifts_frequency_and_limiter_clamps_sum()
-> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Varispeed + Limiter");
    doc.fps = FrameRate::FPS_30;
    let mut dispatcher = CommandDispatcher::new();

    let report = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddAsset {
                    temp_id: Some("a_tone".to_owned()),
                    id: None,
                    name: "tone.wav".to_owned(),
                    path: AssetPath::from_absolute("C:/audio/tone.wav"),
                    kind: AssetKind::Audio {
                        sample_rate_hz: 48_000,
                        channels: 2,
                        source_duration: RationalDuration::new(4, 1)?,
                    },
                },
                EditCommand::AddLayer {
                    temp_id: Some("l1".to_owned()),
                    id: None,
                    name: "Track 1".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c1".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l1".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Double Varispeed".to_owned(),
                        timeline_start: FrameIndex::ZERO,
                        duration: FrameCount::new(45)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: NewClipKind::Audio {
                            asset_id: AssetRef::Temp("a_tone".to_owned()),
                            source_in: RationalDuration::ZERO,
                            playback_rate: PlaybackRate::new(2, 1)?,
                            preserve_pitch: false,
                            audio: AudioProperties {
                                gain_db: AnimatedParameter::constant(8.0),
                                ..AudioProperties::default()
                            },
                        },
                    }),
                },
            ],
        },
    )?;

    let asset_id = report.created_assets["a_tone"].clone();
    let mut mixer = AudioMixer::new();
    mixer.register_pcm(
        asset_id,
        Arc::new(make_sine_stereo(48_000, 440.0, 4.0, 0.8)),
    );

    let mut out = Vec::new();
    mixer.mix_timeline_window(&doc, 4_800, 12_000, 48_000, &mut out);
    assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    let pitch = dominant_freq_hz(&out, 48_000, 300, 1200);
    assert!(
        (pitch - 880.0).abs() <= 20.0,
        "expected ~880Hz, got {pitch}"
    );
    Ok(())
}
