//! `sea-audio-host` 共有メモリ DSP・タイムラインUndo/Redo・不正ファイル拒否・PID 強制終了バイパス結合テスト。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use sea_movie_creator::audio::mixer::AudioMixer;
use sea_movie_creator::audio::plugins::descriptor::SEA_TEST_GAIN_PLUGIN_ID;
use sea_movie_creator::audio::plugins::isolated_host::IsolatedAudioPluginHost;
use sea_movie_creator::audio::plugins::scanner::{
    scan_default_plugin_locations, scan_plugin_directories,
};
use sea_movie_creator::audio::properties::{
    AudioPluginFormat, AudioPluginParamValue, AudioPluginSlot,
};
use sea_movie_creator::core::ids::{AssetRef, ClipRef, LayerRef};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::export::registry::resolve_plugin_executable;
use sea_movie_creator::media::decoder::DecodedAudioPcm;
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AudioProperties, ProjectDocument,
};

fn require_audio_host_bin() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    resolve_plugin_executable(&cwd.join("plugins").join("audio"), "sea-audio-host").ok_or_else(
        || {
            "sea-audio-host binary was not found; run `cargo build --workspace` before running Phase C audio plugin tests"
                .into()
        },
    )
}

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

fn kill_process_by_pid(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(format!("taskkill failed for pid {pid}: {status}").into());
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()?;
        if !status.success() {
            return Err(format!("kill -9 failed for pid {pid}: {status}").into());
        }
    }
    Ok(())
}

fn temp_test_dir(prefix: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[test]
fn invalid_vst3_and_clap_bundles_are_rejected_safely() -> Result<(), Box<dyn std::error::Error>> {
    let host_bin = require_audio_host_bin()?;
    let dir = temp_test_dir("sea-c4-invalid-plugins")?;

    let bad_vst3 = dir.join("broken.vst3");
    std::fs::write(&bad_vst3, b"invalid-vst3-payload")?;
    let bad_clap = dir.join("broken.clap");
    std::fs::write(&bad_clap, b"invalid-clap-payload")?;

    let scan = scan_plugin_directories(std::slice::from_ref(&dir), std::slice::from_ref(&dir));
    assert_eq!(scan.plugins.len(), 0);
    assert_eq!(scan.warnings.len(), 2);

    // 破損パスで直接ワーカー起動を試みても本体はパニックせずバイパス＋ドライ音維持へ遷移する。
    let mut slot = AudioPluginSlot::new(
        "bad_slot",
        "com.example.broken",
        "Broken CLAP",
        AudioPluginFormat::Clap,
    );
    slot.bundle_path = Some(bad_clap.to_string_lossy().into_owned());
    let mut host = IsolatedAudioPluginHost::spawn(&slot, 48_000, Some(&host_bin));
    assert!(host.is_bypassed());
    assert!(host.last_error().is_some());

    let mut samples = vec![0.4_f32, -0.4_f32, 0.2_f32, -0.2_f32];
    let processed = host.process_stereo_block(&mut samples);
    assert!(!processed);
    assert_eq!(samples, vec![0.4_f32, -0.4_f32, 0.2_f32, -0.2_f32]);

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn build_doc_with_gain_plugin(
    sr: u32,
) -> Result<
    (
        ProjectDocument,
        CommandDispatcher,
        sea_movie_creator::core::ids::AssetId,
        sea_movie_creator::core::ids::ClipId,
    ),
    Box<dyn std::error::Error>,
> {
    let duration = FrameCount::new(60)?;
    let mut doc = ProjectDocument::new("c4-plugin-test");
    doc.fps = FrameRate::FPS_30;
    let mut dispatcher = CommandDispatcher::new();

    let mut slot = AudioPluginSlot::new(
        "slot_gain",
        SEA_TEST_GAIN_PLUGIN_ID,
        "Test Gain",
        AudioPluginFormat::BuiltinTest,
    );
    slot.parameters.insert(
        "gain".to_owned(),
        AudioPluginParamValue::Number { value: 1.0 },
    );

    let setup_batch = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddAsset {
                    temp_id: Some("a1".to_owned()),
                    id: None,
                    name: "tone.wav".to_owned(),
                    path: AssetPath::from_absolute("C:/audio/tone.wav"),
                    kind: AssetKind::Audio {
                        sample_rate_hz: sr,
                        channels: 2,
                        source_duration: RationalDuration::new(2, 1)?,
                    },
                },
                EditCommand::AddLayer {
                    temp_id: Some("l1".to_owned()),
                    id: None,
                    name: "Audio 1".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c1".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l1".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Tone".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        midpoints: Vec::new(),
                        kind: NewClipKind::Audio {
                            asset_id: AssetRef::Temp("a1".to_owned()),
                            source_in: RationalDuration::ZERO,
                            playback_rate: PlaybackRate::NORMAL,
                            preserve_pitch: true,
                            audio: AudioProperties {
                                plugin_slots: vec![slot],
                                ..AudioProperties::default()
                            },
                        },
                    }),
                },
            ],
        },
    )?;

    let asset_id = setup_batch
        .created_assets
        .get("a1")
        .ok_or("missing created asset")?
        .clone();
    let clip_id = setup_batch
        .created_clips
        .get("c1")
        .ok_or("missing created clip")?
        .clone();
    Ok((doc, dispatcher, asset_id, clip_id))
}

#[test]
fn isolated_gain_plugin_parameter_undo_and_crash_bypass() -> Result<(), Box<dyn std::error::Error>>
{
    let host_bin = require_audio_host_bin()?;
    let sr = 48_000;
    let sine = Arc::new(make_sine_stereo(sr, 440.0, 2.0, 0.4));
    let (mut doc, mut dispatcher, asset_id, clip_id) = build_doc_with_gain_plugin(sr)?;

    let mut mixer = AudioMixer::new();
    mixer.set_plugin_host_executable_override(Some(host_bin));
    mixer.register_pcm(asset_id, sine);

    // 1. 初期ゲイン 1.0x での基準 RMS 測定
    let mut dry_buf = Vec::new();
    mixer.mix_timeline_window(&doc, 0, 2048, sr, &mut dry_buf);
    let rms_1x = channel_rms(&dry_buf, 0);
    assert!(
        rms_1x > 0.25 && rms_1x < 0.32,
        "unexpected 1x rms: {rms_1x}"
    );
    assert!(!mixer.is_plugin_slot_bypassed(&clip_id, "slot_gain"));

    let worker_pid = mixer
        .plugin_worker_pid(&clip_id, "slot_gain")
        .ok_or("expected active sea-audio-host worker PID")?;

    // 2. SetParameter(AudioPlugin) でゲイン 0.5x へ変更し Undo/Redo も検証
    dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::AudioPlugin {
                clip_id: ClipRef::Id(clip_id.clone()),
                slot_id: "slot_gain".to_owned(),
            },
            path: ParameterPath::PluginParam {
                param_name: "gain".to_owned(),
            },
            value: ParameterValue::Number { value: 0.5 },
        },
    )?;

    let mut half_buf = Vec::new();
    mixer.mix_timeline_window(&doc, 2048, 2048, sr, &mut half_buf);
    let rms_half = channel_rms(&half_buf, 0);
    assert!(
        (rms_half / rms_1x - 0.5).abs() < 0.03,
        "expected ~0.5x rms, ratio={}",
        rms_half / rms_1x
    );

    // Undo で 1.0x へ復帰し、Redo で 0.5x へ戻ることを確認
    dispatcher.undo(&mut doc)?;
    let mut undo_buf = Vec::new();
    mixer.mix_timeline_window(&doc, 4096, 2048, sr, &mut undo_buf);
    let rms_undo = channel_rms(&undo_buf, 0);
    assert!(
        (rms_undo / rms_1x - 1.0).abs() < 0.03,
        "expected ~1.0x after undo, ratio={}",
        rms_undo / rms_1x
    );

    dispatcher.redo(&mut doc)?;
    let mut redo_buf = Vec::new();
    mixer.mix_timeline_window(&doc, 6144, 2048, sr, &mut redo_buf);
    let rms_redo = channel_rms(&redo_buf, 0);
    assert!(
        (rms_redo / rms_1x - 0.5).abs() < 0.03,
        "expected ~0.5x after redo, ratio={}",
        rms_redo / rms_1x
    );

    // 3. 稼働中の外部ワーカープロセス (PID) を強制終了しても本体がクラッシュせず
    //    自動バイパス (ドライ 1.0x 復帰) へ切り替わること。
    kill_process_by_pid(worker_pid)?;
    let mut fallback_buf = Vec::new();
    mixer.mix_timeline_window(&doc, 8192, 2048, sr, &mut fallback_buf);
    assert!(mixer.is_plugin_slot_bypassed(&clip_id, "slot_gain"));
    assert!(
        mixer
            .plugin_slot_last_error(&clip_id, "slot_gain")
            .is_some()
    );
    let rms_bypassed = channel_rms(&fallback_buf, 0);
    assert!(
        (rms_bypassed / rms_1x - 1.0).abs() < 0.03,
        "expected dry 1.0x fallback after worker kill, ratio={}",
        rms_bypassed / rms_1x
    );

    Ok(())
}

#[test]
#[ignore = "requires third-party VST3/CLAP plugins installed on host OS"]
fn live_os_audio_plugin_scan_and_load_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let host_bin = require_audio_host_bin()?;
    let scan = scan_default_plugin_locations();
    for desc in scan.plugins {
        if desc.bundle_path.is_none() {
            continue;
        }
        let format = match desc.format {
            sea_movie_creator::audio::plugins::descriptor::WireAudioPluginFormat::Vst3 => {
                AudioPluginFormat::Vst3
            }
            sea_movie_creator::audio::plugins::descriptor::WireAudioPluginFormat::Clap => {
                AudioPluginFormat::Clap
            }
            sea_movie_creator::audio::plugins::descriptor::WireAudioPluginFormat::BuiltinTest => {
                continue;
            }
        };
        let mut slot = AudioPluginSlot::new("smoke", desc.plugin_id, desc.name, format);
        slot.bundle_path = desc.bundle_path;
        let mut host = IsolatedAudioPluginHost::spawn(&slot, 48_000, Some(&host_bin));
        if !host.is_bypassed() {
            let mut silence = vec![0.0_f32; 512];
            assert!(host.process_stereo_block(&mut silence));
            return Ok(());
        }
    }
    assert!(Path::new(&host_bin).is_file());
    Ok(())
}
