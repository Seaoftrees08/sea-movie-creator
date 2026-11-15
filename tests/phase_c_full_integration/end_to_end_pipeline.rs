//! プロジェクト作成 → AI バッチ提案/承認/1クリック Undo → 速度/プラグイン合成 → プラグイン発見 & 実書き出し/デコード完全往復テスト。

use std::fs::{self, File};
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{ImageFormat, Rgba, RgbaImage};
use sea_movie_creator::ai::providers::mock::MockAiProvider;
use sea_movie_creator::audio::mixer::AudioMixer;
use sea_movie_creator::audio::plugins::descriptor::SEA_TEST_GAIN_PLUGIN_ID;
use sea_movie_creator::audio::properties::{
    AudioPluginFormat, AudioPluginParamValue, AudioPluginSlot,
};
use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::ids::{AssetRef, ClipId, ClipRef, LayerRef};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::export::registry::{
    PluginRegistry, RegistryError, resolve_plugin_executable,
};
use sea_movie_creator::export::runner::{
    ExportCancelToken, ExportError, ExportJobRequest, ExportMediaSources, ExportPhase,
    run_export_job,
};
use sea_movie_creator::media::decoder::{
    BackendPreference, DecodedAudioPcm, open_audio_decoder, open_video_decoder, probe_media,
};
use sea_movie_creator::media::visual_properties::{SubtitleProperties, VisualProperties};
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AudioProperties, ProjectDocument,
};
use sea_movie_creator::ui::app_controller::AppController;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 90;
const TOTAL_FRAMES: u32 = 15;
const SAMPLE_RATE: u32 = 48_000;

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(tag: &str) -> Result<Self, std::io::Error> {
        let dir = std::env::temp_dir().join(format!("sea_c5_e2e_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn require_workspace_plugins() -> Result<(PluginRegistry, PathBuf), Box<dyn std::error::Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join("export");
    let registry = PluginRegistry::discover_in_dirs(&[manifest_dir]);

    for (plugin_id, preset_id) in [
        ("sea.export.av1", "av1_fast"),
        ("sea.export.h264", "h264_external_ffmpeg"),
        ("sea.export.h265", "h265_external_ffmpeg"),
    ] {
        match registry.resolve_runnable(plugin_id, preset_id) {
            Ok((_, _)) => {}
            Err(RegistryError::MissingExecutable {
                plugin_id,
                entrypoint,
            }) => {
                return Err(format!(
                    "plugin executable '{entrypoint}' for '{plugin_id}' not found; run `cargo build --workspace --all-targets` first"
                )
                .into());
            }
            Err(err) => return Err(err.into()),
        }
    }

    let cwd = std::env::current_dir()?;
    let audio_host =
        resolve_plugin_executable(&cwd.join("plugins").join("audio"), "sea-audio-host").ok_or(
            "sea-audio-host binary missing; run `cargo build --workspace --all-targets` first",
        )?;
    Ok((registry, audio_host))
}

fn write_test_png(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut img = RgbaImage::new(WIDTH, HEIGHT);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = if (x + y) % 16 < 8 {
            Rgba([230, 80, 40, 255])
        } else {
            Rgba([30, 120, 220, 255])
        };
    }
    let mut out = File::create(path)?;
    img.write_to(&mut out, ImageFormat::Png)?;
    Ok(())
}

fn write_test_wav_16bit_stereo(
    path: &Path,
    sample_rate: u32,
    seconds: f64,
    freq_hz: f64,
) -> Result<Arc<DecodedAudioPcm>, Box<dyn std::error::Error>> {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let total_frames = (f64::from(sample_rate) * seconds).round() as usize;
    let mut pcm_f32 = Vec::with_capacity(total_frames * 2);
    let mut wav_bytes_le = Vec::with_capacity(total_frames * 4);

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    for i in 0..total_frames {
        let t = (i as f64) / f64::from(sample_rate);
        let sample = ((2.0 * std::f64::consts::PI * freq_hz * t).sin() * 0.45) as f32;
        pcm_f32.push(sample);
        pcm_f32.push(sample);
        let quantized =
            (sample * f32::from(i16::MAX)).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        let le = quantized.to_le_bytes();
        wav_bytes_le.extend_from_slice(&le);
        wav_bytes_le.extend_from_slice(&le);
    }

    let data_bytes = u32::try_from(wav_bytes_le.len())?;
    let byte_rate = sample_rate.saturating_mul(4);
    let mut f = File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&data_bytes.saturating_add(36).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16_u32.to_le_bytes())?;
    f.write_all(&1_u16.to_le_bytes())?;
    f.write_all(&2_u16.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&4_u16.to_le_bytes())?;
    f.write_all(&16_u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    f.write_all(&wav_bytes_le)?;

    Ok(Arc::new(DecodedAudioPcm {
        sample_rate,
        channels: 2,
        interleaved: pcm_f32,
    }))
}

fn build_asset_and_layer_cmds(
    png_path: &Path,
    wav_path: &Path,
) -> Result<Vec<EditCommand>, Box<dyn std::error::Error>> {
    Ok(vec![
        EditCommand::AddAsset {
            temp_id: Some("img_asset".to_owned()),
            id: None,
            name: "card.png".to_owned(),
            path: AssetPath::from_absolute(png_path.to_string_lossy().into_owned()),
            kind: AssetKind::Image {
                width: WIDTH,
                height: HEIGHT,
            },
        },
        EditCommand::AddAsset {
            temp_id: Some("bgm_asset".to_owned()),
            id: None,
            name: "bgm.wav".to_owned(),
            path: AssetPath::from_absolute(wav_path.to_string_lossy().into_owned()),
            kind: AssetKind::Audio {
                sample_rate_hz: SAMPLE_RATE,
                channels: 2,
                source_duration: RationalDuration::new(2, 1)?,
            },
        },
        EditCommand::AddLayer {
            temp_id: Some("l_vid".to_owned()),
            id: None,
            name: "Video".to_owned(),
            insert_index: None,
        },
        EditCommand::AddLayer {
            temp_id: Some("l_sub".to_owned()),
            id: None,
            name: "Subtitle".to_owned(),
            insert_index: None,
        },
        EditCommand::AddLayer {
            temp_id: Some("l_aud".to_owned()),
            id: None,
            name: "Audio".to_owned(),
            insert_index: None,
        },
    ])
}

fn build_ai_batch_for_e2e(
    png_path: &Path,
    wav_path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let dur = FrameCount::new(u64::from(TOTAL_FRAMES))?;
    let mut gain_slot = AudioPluginSlot::new(
        "e2e_gain",
        SEA_TEST_GAIN_PLUGIN_ID,
        "Test Gain",
        AudioPluginFormat::BuiltinTest,
    );
    gain_slot.parameters.insert(
        "gain".to_owned(),
        AudioPluginParamValue::Number { value: 0.5 },
    );

    let mut commands = build_asset_and_layer_cmds(png_path, wav_path)?;
    commands.extend([
        EditCommand::AddClip {
            temp_id: Some("c_img".to_owned()),
            id: None,
            layer_id: LayerRef::Temp("l_vid".to_owned()),
            clip: Box::new(NewClipSpec {
                name: "Card".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: dur,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                kind: NewClipKind::Image {
                    asset_id: AssetRef::Temp("img_asset".to_owned()),
                    visual: VisualProperties::default(),
                },
            }),
        },
        EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Temp("c_img".to_owned()),
            },
            path: ParameterPath::X,
            value: ParameterValue::Animated {
                param: AnimatedParameter::expression("t * 25.0"),
            },
        },
        EditCommand::AddClip {
            temp_id: Some("c_sub".to_owned()),
            id: None,
            layer_id: LayerRef::Temp("l_sub".to_owned()),
            clip: Box::new(NewClipSpec {
                name: "Sub".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: dur,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                kind: NewClipKind::Subtitle(SubtitleProperties::new("E2E Integration")),
            }),
        },
        EditCommand::AddClip {
            temp_id: Some("c_aud".to_owned()),
            id: None,
            layer_id: LayerRef::Temp("l_aud".to_owned()),
            clip: Box::new(NewClipSpec {
                name: "Tone".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: dur,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                kind: NewClipKind::Audio {
                    asset_id: AssetRef::Temp("bgm_asset".to_owned()),
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::new(3, 2)?,
                    preserve_pitch: true,
                    audio: AudioProperties {
                        plugin_slots: vec![gain_slot],
                        ..AudioProperties::default()
                    },
                },
            }),
        },
    ]);
    let json = serde_json::to_string_pretty(&EditCommand::Batch { commands })?;
    Ok(format!("構成案です:\n```json\n{json}\n```"))
}

fn verify_h265_plugin_invocation(
    registry: &PluginRegistry,
    doc: &ProjectDocument,
    temp_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let nonexistent = "nonexistent_hevc_ffmpeg_xyz_999";
    let bad_out = temp_dir.join("h265_should_fail.mp4");
    let req = ExportJobRequest::full_project(
        doc,
        "sea.export.h265".to_owned(),
        "h265_external_ffmpeg".to_owned(),
        bad_out.clone(),
        serde_json::json!({ "ffmpeg_path": nonexistent }),
    );
    let mut sources = ExportMediaSources::new(Some(temp_dir.to_path_buf()));
    let res = run_export_job(
        doc,
        registry,
        &req,
        &mut sources,
        &ExportCancelToken::new(),
        |_| {},
    );
    let Err(err) = res else {
        return Err("expected h265 missing external ffmpeg to fail clearly".into());
    };
    assert!(matches!(err, ExportError::PluginFailed { .. }));
    assert!(!bad_out.exists());
    Ok(())
}

fn verify_audio_plugin_isolation_on_doc(
    doc: &ProjectDocument,
    audio_host: PathBuf,
    audio_asset_id: sea_movie_creator::core::ids::AssetId,
    audio_clip_id: &ClipId,
    pcm: Arc<DecodedAudioPcm>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut mixer = AudioMixer::new();
    mixer.set_plugin_host_executable_override(Some(audio_host));
    mixer.register_pcm(audio_asset_id, pcm);

    let mut out = Vec::new();
    mixer.mix_timeline_window(doc, 0, 2048, SAMPLE_RATE, &mut out);
    let pid = mixer
        .plugin_worker_pid(audio_clip_id, "e2e_gain")
        .ok_or("expected active e2e audio plugin worker PID")?;
    assert!(out.iter().any(|s| s.abs() > 0.01));
    assert!(!mixer.is_plugin_slot_bypassed(audio_clip_id, "e2e_gain"));
    assert!(pid > 0 && pid != std::process::id());
    Ok(())
}

fn verify_av1_mkv_export_and_decode(
    registry: &PluginRegistry,
    doc: &ProjectDocument,
    temp_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mkv_path = temp_dir.join("e2e_output.mkv");
    let req = ExportJobRequest::full_project(
        doc,
        "sea.export.av1".to_owned(),
        "av1_fast".to_owned(),
        mkv_path.clone(),
        serde_json::json!({}),
    );
    let mut sources = ExportMediaSources::new(Some(temp_dir.to_path_buf()));
    let mut phases = Vec::new();
    let outcome = run_export_job(
        doc,
        registry,
        &req,
        &mut sources,
        &ExportCancelToken::new(),
        |p| phases.push(p.phase),
    )?;
    assert_eq!(outcome.frames_encoded, u64::from(TOTAL_FRAMES));
    assert!(outcome.file_size_bytes > 500);
    assert!(mkv_path.is_file());
    assert!(phases.contains(&ExportPhase::Completed));

    let info = probe_media(&mkv_path, BackendPreference::OpenSoftware)?;
    assert!(info.has_video && info.has_audio);
    assert_eq!(info.width, Some(WIDTH));
    assert_eq!(info.height, Some(HEIGHT));

    let mut vdec = open_video_decoder(&mkv_path, BackendPreference::OpenSoftware)?;
    let frame0 = vdec
        .decode_next_frame()?
        .ok_or("expected at least one decoded AV1 video frame")?;
    assert_eq!(frame0.buffer.width, WIDTH);
    assert_eq!(frame0.buffer.height, HEIGHT);

    let mut adec = open_audio_decoder(&mkv_path, BackendPreference::OpenSoftware)?;
    let decoded_audio = adec.decode_all_interleaved()?;
    let non_zero = decoded_audio.interleaved.iter().any(|s| s.abs() > 0.001);
    assert!(!decoded_audio.interleaved.is_empty() && non_zero);
    Ok(())
}

#[test]
fn phase_c_end_to_end_ai_editing_audio_plugins_and_export_roundtrip()
-> Result<(), Box<dyn std::error::Error>> {
    let (registry, audio_host) = require_workspace_plugins()?;
    let temp = TempDirGuard::new("full")?;
    let png_path = temp.path().join("card.png");
    let wav_path = temp.path().join("bgm.wav");
    write_test_png(&png_path)?;
    let pcm = write_test_wav_16bit_stereo(&wav_path, SAMPLE_RATE, 2.0, 440.0)?;

    let mut controller = AppController::new();
    controller.document.fps = FrameRate::FPS_30;
    controller.document.resolution.width = NonZeroU32::new(WIDTH).ok_or("non-zero width")?;
    controller.document.resolution.height = NonZeroU32::new(HEIGHT).ok_or("non-zero height")?;

    let ai_reply = build_ai_batch_for_e2e(&png_path, &wav_path)?;
    let mock = MockAiProvider::with_handler(move |_| Ok(ai_reply.clone()));
    controller.submit_ai_prompt_with_provider("E2E 動画構成を作成して", &mock)?;
    assert_eq!(controller.document.layers.len(), 0);

    controller.approve_pending_ai_proposal()?;
    assert_eq!(controller.document.layers.len(), 3);
    assert!(controller.ai_chat.can_one_click_undo);

    // 1クリック Undo -> 3レイヤーが巻き戻り、Redo で再適用されること
    controller.undo_last_ai_plan()?;
    assert_eq!(controller.document.layers.len(), 0);
    controller.redo()?;
    assert_eq!(controller.document.layers.len(), 3);

    // Phase C-3/C-4: オーディオクリップと sea-audio-host 隔離ワーカー連携を確認
    let audio_asset_id = controller.document.assets[1].id.clone();
    let audio_clip_id = controller.document.layers[2].clips[0].id.clone();
    verify_audio_plugin_isolation_on_doc(
        &controller.document,
        audio_host,
        audio_asset_id,
        &audio_clip_id,
        pcm,
    )?;

    // Phase C-5: H.265 プラグイン設定の起動・検証
    verify_h265_plugin_invocation(&registry, &controller.document, temp.path())?;

    // AV1 + FLAC 実動画書き出しおよびオープン SW デコード検証
    verify_av1_mkv_export_and_decode(&registry, &controller.document, temp.path())?;
    Ok(())
}
