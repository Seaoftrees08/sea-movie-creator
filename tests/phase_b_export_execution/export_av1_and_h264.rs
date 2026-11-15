//! Phase B-5: `sea-export-av1` / `sea-export-h264` 実プラグイン呼び出しとデコード往復検証。

use std::f32::consts::TAU;
use std::fs::{self, File};
use std::io::Cursor;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use image::{ImageFormat, Rgba, RgbaImage};
use matroska_demuxer::{Frame as MkvFrame, MatroskaFile, TrackType as MkvTrackType};
use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::color::RgbaColor;
use sea_movie_creator::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::export::registry::{PluginRegistry, RegistryError};
use sea_movie_creator::export::runner::{
    ExportCancelToken, ExportError, ExportJobRequest, ExportMediaSources, ExportPhase,
    run_export_job,
};
use sea_movie_creator::media::decoder::{
    BackendPreference, open_audio_decoder, open_video_decoder, probe_media,
};
use sea_movie_creator::media::visual_properties::{
    SubtitleProperties, TextAlignment, VisualProperties,
};
use sea_movie_creator::timeline::command::{EditCommand, NewClipKind, NewClipSpec};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AudioProperties, ProjectDocument, ProjectResolution,
};
use sea_movie_creator::ui::inspector_panel::{InspectorParameterSlot, build_expression_command};

const OUT_WIDTH: u32 = 320;
const OUT_HEIGHT: u32 = 180;
const TOTAL_FRAMES: u32 = 18;
const FPS_NUM: u32 = 24;
const SAMPLE_RATE: u32 = 48_000;

struct TempTestDir(PathBuf);

impl TempTestDir {
    fn new(tag: &str) -> Result<Self, std::io::Error> {
        let dir = std::env::temp_dir().join(format!("sea_b5_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn discover_and_require_plugin(
    plugin_id: &str,
    preset_id: &str,
) -> Result<PluginRegistry, Box<dyn std::error::Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("plugins")
        .join("export");
    let registry = PluginRegistry::discover_in_dirs(&[manifest_dir]);
    match registry.resolve_runnable(plugin_id, preset_id) {
        Ok((_, _)) => Ok(registry),
        Err(RegistryError::MissingExecutable {
            plugin_id,
            entrypoint,
        }) => Err(format!(
            "plugin binary '{entrypoint}' for '{plugin_id}' not found; run `cargo build --workspace --all-targets` before running tests"
        )
        .into()),
        Err(err) => Err(err.into()),
    }
}

fn write_two_tone_png(
    path: &Path,
    width: u32,
    height: u32,
    top_rgba: [u8; 4],
    bottom_rgba: [u8; 4],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut img = RgbaImage::new(width, height);
    for (_, y, px) in img.enumerate_pixels_mut() {
        *px = if y < height / 2 {
            Rgba(top_rgba)
        } else {
            Rgba(bottom_rgba)
        };
    }
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut buf, ImageFormat::Png)?;
    fs::write(path, buf.into_inner())?;
    Ok(())
}

fn write_stereo_wav_16bit(
    path: &Path,
    sample_rate: u32,
    duration_secs: f32,
    freq_hz: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let total_frames = ((sample_rate as f32) * duration_secs).round() as usize;
    let channels: u16 = 2;
    let data_bytes = u32::try_from(total_frames * usize::from(channels) * 2)?;
    let riff_size = 36_u32.saturating_add(data_bytes);

    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for i in 0..total_frames {
        #[allow(clippy::cast_precision_loss)]
        let time_sec = (i as f32) / (sample_rate as f32);
        let sample_val = (TAU * freq_hz * time_sec).sin() * 0.45;
        #[allow(clippy::cast_possible_truncation)]
        let q = (sample_val * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&q.to_le_bytes());
        out.extend_from_slice(&q.to_le_bytes());
    }
    fs::write(path, out)?;
    Ok(())
}

fn build_e2e_project(
    png_path: &Path,
    img_w: u32,
    img_h: u32,
    wav_path: &Path,
    bg_color: RgbaColor,
    with_animation_and_split: bool,
) -> Result<ProjectDocument, Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("PhaseB5ExportE2E");
    doc.fps = FrameRate::new(FPS_NUM, 1)?;
    doc.resolution = ProjectResolution::new(
        NonZeroU32::new(OUT_WIDTH).ok_or("non-zero width")?,
        NonZeroU32::new(OUT_HEIGHT).ok_or("non-zero height")?,
    );
    doc.background_color = bg_color;

    let img_asset_id = AssetId::new("asset-img")?;
    let bgm_asset_id = AssetId::new("asset-bgm")?;
    let img_clip_id = ClipId::new("clip-img")?;
    let v_layer = LayerId::new("layer-visual")?;
    let s_layer = LayerId::new("layer-sub")?;
    let a_layer = LayerId::new("layer-audio")?;

    let mut dispatcher = CommandDispatcher::new();
    let assets_layers = build_assets_and_layers_batch(
        png_path,
        (img_w, img_h),
        wav_path,
        (&img_asset_id, &bgm_asset_id),
        (&v_layer, &s_layer, &a_layer),
    );
    dispatcher.dispatch(&mut doc, &assets_layers)?;

    let clips_batch = build_clips_batch(
        &img_asset_id,
        &bgm_asset_id,
        &img_clip_id,
        (&v_layer, &s_layer, &a_layer),
    )?;
    dispatcher.dispatch(&mut doc, &clips_batch)?;

    if with_animation_and_split {
        apply_animation_and_split(&mut doc, &mut dispatcher, &img_clip_id)?;
    }
    doc.validate()?;
    Ok(doc)
}

fn build_assets_and_layers_batch(
    png_path: &Path,
    img_dims: (u32, u32),
    wav_path: &Path,
    asset_ids: (&AssetId, &AssetId),
    layers: (&LayerId, &LayerId, &LayerId),
) -> EditCommand {
    EditCommand::Batch {
        commands: vec![
            EditCommand::AddAsset {
                temp_id: None,
                id: Some(asset_ids.0.clone()),
                name: "TwoTone".to_owned(),
                path: AssetPath::from_absolute(png_path.to_string_lossy().into_owned()),
                kind: AssetKind::Image {
                    width: img_dims.0,
                    height: img_dims.1,
                },
            },
            EditCommand::AddAsset {
                temp_id: None,
                id: Some(asset_ids.1.clone()),
                name: "ToneBgm".to_owned(),
                path: AssetPath::from_absolute(wav_path.to_string_lossy().into_owned()),
                kind: AssetKind::Audio {
                    sample_rate_hz: SAMPLE_RATE,
                    channels: 2,
                    source_duration: RationalDuration::from_millis(1_000),
                },
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(layers.0.clone()),
                name: "ImageLayer".to_owned(),
                insert_index: None,
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(layers.1.clone()),
                name: "SubLayer".to_owned(),
                insert_index: None,
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(layers.2.clone()),
                name: "AudioLayer".to_owned(),
                insert_index: None,
            },
        ],
    }
}

fn build_clips_batch(
    img_asset_id: &AssetId,
    bgm_asset_id: &AssetId,
    img_clip_id: &ClipId,
    layers: (&LayerId, &LayerId, &LayerId),
) -> Result<EditCommand, Box<dyn std::error::Error>> {
    let clip_len = FrameCount::new(u64::from(TOTAL_FRAMES))?;
    Ok(EditCommand::Batch {
        commands: vec![
            EditCommand::AddClip {
                temp_id: None,
                id: Some(img_clip_id.clone()),
                layer_id: LayerRef::Id(layers.0.clone()),
                clip: Box::new(NewClipSpec {
                    name: "Card".to_owned(),
                    timeline_start: FrameIndex::ZERO,
                    duration: clip_len,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    kind: NewClipKind::Image {
                        asset_id: AssetRef::Id(img_asset_id.clone()),
                        visual: VisualProperties::default(),
                    },
                }),
            },
            EditCommand::AddClip {
                temp_id: None,
                id: None,
                layer_id: LayerRef::Id(layers.1.clone()),
                clip: Box::new(NewClipSpec {
                    name: "Title".to_owned(),
                    timeline_start: FrameIndex::ZERO,
                    duration: clip_len,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    kind: NewClipKind::Subtitle(SubtitleProperties {
                        text: "Phase B-5 実書き出し\nテスト字幕".to_owned(),
                        font_family: "sans-serif".to_owned(),
                        font_size: AnimatedParameter::constant(28.0),
                        text_color: RgbaColor::WHITE,
                        outline_color: RgbaColor::BLACK,
                        outline_width: 2.0,
                        alignment: TextAlignment::Center,
                        visual: VisualProperties::default(),
                    }),
                }),
            },
            EditCommand::AddClip {
                temp_id: None,
                id: None,
                layer_id: LayerRef::Id(layers.2.clone()),
                clip: Box::new(NewClipSpec {
                    name: "Bgm".to_owned(),
                    timeline_start: FrameIndex::ZERO,
                    duration: clip_len,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    kind: NewClipKind::Audio {
                        asset_id: AssetRef::Id(bgm_asset_id.clone()),
                        source_in: RationalDuration::ZERO,
                        playback_rate: PlaybackRate::NORMAL,
                        preserve_pitch: true,
                        audio: AudioProperties::default(),
                    },
                }),
            },
        ],
    })
}

fn apply_animation_and_split(
    doc: &mut ProjectDocument,
    dispatcher: &mut CommandDispatcher,
    img_clip_id: &ClipId,
) -> Result<(), Box<dyn std::error::Error>> {
    let snap = doc.find_clip(img_clip_id).ok_or("img clip")?.1.clone();
    dispatcher.dispatch(
        doc,
        &build_expression_command(
            &snap,
            InspectorParameterSlot::PositionX,
            "sin(t * 6.283185) * 45.0",
            0.0,
            2.0,
        )?,
    )?;
    let snap = doc.find_clip(img_clip_id).ok_or("img clip")?.1.clone();
    dispatcher.dispatch(
        doc,
        &build_expression_command(
            &snap,
            InspectorParameterSlot::RotationDeg,
            "t * 60.0",
            0.0,
            2.0,
        )?,
    )?;

    let split_right = ClipId::new("clip-img-right")?;
    dispatcher.dispatch(
        doc,
        &EditCommand::SplitClip {
            clip_id: ClipRef::Id(img_clip_id.clone()),
            split_at_timeline_frame: FrameIndex::new(u64::from(TOTAL_FRAMES / 2)),
            right_temp_id: None,
            right_id: Some(split_right.clone()),
        },
    )?;
    let (_, right_clip) = doc.find_clip(&split_right).ok_or("split right clip")?;
    assert_eq!(right_clip.effect_offset_frames, i64::from(TOTAL_FRAMES / 2));
    Ok(())
}

fn verify_matroska_container(mkv_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut mkv = MatroskaFile::open(File::open(mkv_path)?)?;
    let mut video_track = None;
    let mut audio_track = None;
    for track in mkv.tracks() {
        match track.track_type() {
            MkvTrackType::Video => {
                video_track = Some((
                    track.track_number().get(),
                    track.codec_id().to_owned(),
                    track
                        .video()
                        .map(|v| (v.pixel_width().get(), v.pixel_height().get())),
                ));
            }
            MkvTrackType::Audio => {
                audio_track = Some((
                    track.track_number().get(),
                    track.codec_id().to_owned(),
                    track
                        .audio()
                        .map(|a| (a.sampling_frequency(), a.channels().get())),
                ));
            }
            _ => {}
        }
    }

    let (v_num, v_codec, v_dims) = video_track.ok_or("MKV に映像トラックが見つかりません")?;
    assert_eq!(v_codec, "V_AV1");
    assert_eq!(v_dims, Some((u64::from(OUT_WIDTH), u64::from(OUT_HEIGHT))));

    let (a_num, a_codec, a_fmt) = audio_track.ok_or("MKV に音声トラックが見つかりません")?;
    assert_eq!(a_codec, "A_FLAC");
    let (sr, ch) = a_fmt.ok_or("音声トラックフォーマット欠損")?;
    assert!((sr - f64::from(SAMPLE_RATE)).abs() < 1.0);
    assert_eq!(ch, 2);

    let mut frame = MkvFrame::default();
    let mut video_packets = 0_usize;
    let mut audio_packets = 0_usize;
    while mkv.next_frame(&mut frame)? {
        if frame.track == v_num {
            video_packets += 1;
        } else if frame.track == a_num {
            audio_packets += 1;
        }
    }
    assert!(video_packets >= usize::try_from(TOTAL_FRAMES)?);
    assert!(audio_packets >= 1);
    Ok(())
}

fn verify_av1_mkv_decode_roundtrip(mkv_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let info = probe_media(mkv_path, BackendPreference::OpenSoftware)?;
    assert!(info.has_video);
    assert!(info.has_audio);
    assert_eq!(info.width, Some(OUT_WIDTH));
    assert_eq!(info.height, Some(OUT_HEIGHT));
    assert_eq!(info.audio_sample_rate, Some(SAMPLE_RATE));
    assert_eq!(info.audio_channels, Some(2));
    let duration = info.duration_seconds.unwrap_or(0.0);
    assert!(
        duration > 0.5,
        "期待した総再生時間 (>0.5s) に満たない: {duration}"
    );

    let mut vdec = open_video_decoder(mkv_path, BackendPreference::OpenSoftware)?;
    let mut decoded_frames = Vec::new();
    while let Some(frame) = vdec.decode_next_frame()? {
        assert_eq!(frame.buffer.width, OUT_WIDTH);
        assert_eq!(frame.buffer.height, OUT_HEIGHT);
        decoded_frames.push(frame.buffer.to_bgra8());
    }
    assert!(
        decoded_frames.len() >= usize::try_from(TOTAL_FRAMES - 2)?,
        "デコードフレーム数が少なすぎる: {}",
        decoded_frames.len()
    );

    let first = decoded_frames.first().ok_or("first frame missing")?;
    let last = decoded_frames.last().ok_or("last frame missing")?;
    let min_luma = first
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[0].max(px[1]).max(px[2]))
        .min()
        .unwrap_or(0);
    let max_luma = first
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[0].max(px[1]).max(px[2]))
        .max()
        .unwrap_or(0);
    assert!(
        max_luma.saturating_sub(min_luma) > 40,
        "デコード映像の明暗コントラストが不足: min={min_luma}, max={max_luma}"
    );
    let frame_diff_sum: u64 = first
        .iter()
        .zip(last.iter())
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    assert!(
        frame_diff_sum > 5_000,
        "アニメーション・分割付きプロジェクトなのに先頭/末尾フレーム差分が小さすぎる: {frame_diff_sum}"
    );

    let mut adec = open_audio_decoder(mkv_path, BackendPreference::OpenSoftware)?;
    let pcm = adec.decode_all_interleaved()?;
    assert_eq!(pcm.sample_rate, SAMPLE_RATE);
    assert_eq!(pcm.channels, 2);
    assert!(
        pcm.frame_count() >= (SAMPLE_RATE as usize) / 2,
        "デコード音声サンプル数が不足: {}",
        pcm.frame_count()
    );
    #[allow(clippy::cast_precision_loss)]
    let rms = (pcm
        .interleaved
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>()
        / (pcm.interleaved.len().max(1) as f64))
        .sqrt();
    assert!(rms > 0.05, "デコード音声が無音または極小レベル: rms={rms}");
    Ok(())
}

#[test]
fn export_av1_fast_mkv_full_pipeline_decodes_cleanly() -> Result<(), Box<dyn std::error::Error>> {
    let registry = discover_and_require_plugin("sea.export.av1", "av1_fast")?;
    let temp_dir = TempTestDir::new("av1_e2e")?;
    let png_path = temp_dir.path().join("card.png");
    let wav_path = temp_dir.path().join("bgm.wav");
    let mkv_path = temp_dir.path().join("output.mkv");

    write_two_tone_png(&png_path, 160, 90, [20, 210, 240, 255], [240, 110, 30, 255])?;
    write_stereo_wav_16bit(&wav_path, SAMPLE_RATE, 1.0, 440.0)?;

    let doc = build_e2e_project(
        &png_path,
        160,
        90,
        &wav_path,
        RgbaColor::new(16, 22, 32, 255),
        true,
    )?;
    let mut sources = ExportMediaSources::new(Some(temp_dir.path().to_path_buf()));
    let req = ExportJobRequest::full_project(
        &doc,
        "sea.export.av1".to_owned(),
        "av1_fast".to_owned(),
        mkv_path.clone(),
        serde_json::json!({}),
    );

    let mut phases = Vec::new();
    let outcome = run_export_job(
        &doc,
        &registry,
        &req,
        &mut sources,
        &ExportCancelToken::new(),
        |prog| phases.push(prog.phase),
    )?;
    assert_eq!(outcome.frames_encoded, u64::from(TOTAL_FRAMES));
    assert!(outcome.file_size_bytes > 500);
    assert!(mkv_path.is_file());
    assert!(phases.contains(&ExportPhase::Preparing));
    assert!(phases.contains(&ExportPhase::Encoding));
    assert!(phases.contains(&ExportPhase::Completed));

    verify_matroska_container(&mkv_path)?;
    verify_av1_mkv_decode_roundtrip(&mkv_path)?;
    Ok(())
}

#[test]
fn export_h264_external_ffmpeg_missing_binary_reports_clear_error_without_residual_file()
-> Result<(), Box<dyn std::error::Error>> {
    let registry = discover_and_require_plugin("sea.export.h264", "h264_external_ffmpeg")?;
    let temp_dir = TempTestDir::new("h264_missing_ff")?;
    let png_path = temp_dir.path().join("card.png");
    let wav_path = temp_dir.path().join("bgm.wav");
    let mp4_path = temp_dir.path().join("should_not_exist.mp4");

    write_two_tone_png(&png_path, 160, 90, [220, 40, 40, 255], [40, 80, 220, 255])?;
    write_stereo_wav_16bit(&wav_path, SAMPLE_RATE, 1.0, 440.0)?;

    let doc = build_e2e_project(&png_path, 160, 90, &wav_path, RgbaColor::BLACK, false)?;
    let mut sources = ExportMediaSources::new(Some(temp_dir.path().to_path_buf()));
    let req = ExportJobRequest::full_project(
        &doc,
        "sea.export.h264".to_owned(),
        "h264_external_ffmpeg".to_owned(),
        mp4_path.clone(),
        serde_json::json!({
            "ffmpeg_path": "nonexistent_sea_ffmpeg_executable_xyz_999"
        }),
    );

    let err = run_export_job(
        &doc,
        &registry,
        &req,
        &mut sources,
        &ExportCancelToken::new(),
        |_| {},
    )
    .expect_err("存在しない外部 ffmpeg 指定時は明確に失敗すること");
    assert!(
        matches!(err, ExportError::PluginFailed { .. }),
        "unexpected error variant: {err}"
    );
    assert!(err.to_string().contains("ffmpeg"));
    assert!(!mp4_path.exists());
    Ok(())
}

#[test]
#[ignore = "requires OS hardware H.264 encoder"]
fn export_h264_windows_mf_mp4_roundtrip_and_orientation() -> Result<(), Box<dyn std::error::Error>>
{
    let registry = discover_and_require_plugin("sea.export.h264", "h264_mf_hw")?;
    let temp_dir = TempTestDir::new("h264_mf")?;
    let png_path = temp_dir.path().join("orientation.png");
    let wav_path = temp_dir.path().join("bgm.wav");
    let mp4_path = temp_dir.path().join("output.mp4");

    // 上半分＝赤 [240, 30, 30]、下半分＝青 [30, 60, 240] で上下反転バグを検出する
    write_two_tone_png(
        &png_path,
        OUT_WIDTH,
        OUT_HEIGHT,
        [240, 30, 30, 255],
        [30, 60, 240, 255],
    )?;
    write_stereo_wav_16bit(&wav_path, SAMPLE_RATE, 1.0, 523.25)?;

    let doc = build_e2e_project(
        &png_path,
        OUT_WIDTH,
        OUT_HEIGHT,
        &wav_path,
        RgbaColor::BLACK,
        false,
    )?;
    let mut sources = ExportMediaSources::new(Some(temp_dir.path().to_path_buf()));
    let req = ExportJobRequest::full_project(
        &doc,
        "sea.export.h264".to_owned(),
        "h264_mf_hw".to_owned(),
        mp4_path.clone(),
        serde_json::json!({ "bitrate_kbps": 4000 }),
    );

    let outcome = run_export_job(
        &doc,
        &registry,
        &req,
        &mut sources,
        &ExportCancelToken::new(),
        |_| {},
    )?;
    assert_eq!(outcome.frames_encoded, u64::from(TOTAL_FRAMES));
    assert!(outcome.file_size_bytes > 500);
    assert!(mp4_path.is_file());

    let info = probe_media(&mp4_path, BackendPreference::Auto)?;
    assert!(info.has_video);
    assert_eq!(info.width, Some(OUT_WIDTH));
    assert_eq!(info.height, Some(OUT_HEIGHT));

    let mut vdec = open_video_decoder(&mp4_path, BackendPreference::Auto)?;
    let first = vdec
        .decode_next_frame()?
        .ok_or("MP4 先頭フレームをデコードできません")?;
    let bgra = first.buffer.to_bgra8();
    let stride = (OUT_WIDTH as usize) * 4;
    let top_px_idx = 20 * stride + 40 * 4;
    let bot_px_idx = (OUT_HEIGHT as usize - 20) * stride + 40 * 4;
    // BGRA: [B, G, R, A] -> 上部は R > B、下部は B > R であること（正順ストライド検証）
    assert!(
        bgra[top_px_idx + 2] > bgra[top_px_idx].saturating_add(60),
        "上端付近の赤成分が不足 (上下反転の可能性): BGRA={:?}",
        &bgra[top_px_idx..top_px_idx + 4]
    );
    assert!(
        bgra[bot_px_idx] > bgra[bot_px_idx + 2].saturating_add(60),
        "下端付近の青成分が不足 (上下反転の可能性): BGRA={:?}",
        &bgra[bot_px_idx..bot_px_idx + 4]
    );
    Ok(())
}
