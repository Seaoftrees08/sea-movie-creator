//! Phase B-2: 静止画デコード・日本語字幕描画・4 モード切替・分割後変換連続性の結合検証。

use std::io::Cursor;
use std::num::NonZeroU32;
use std::sync::Arc;

use image::{ImageFormat, Rgba, RgbaImage};
use sea_movie_creator::core::animation::{AnimatedParameter, EasingCurve, EvaluationContext};
use sea_movie_creator::core::color::RgbaColor;
use sea_movie_creator::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex};
use sea_movie_creator::media::compositor::build_render_scene;
use sea_movie_creator::media::frame_cache::{DEFAULT_FRAME_CACHE_BUDGET_BYTES, FrameCache};
use sea_movie_creator::media::image_loader::{inspect_image_file, load_image_into_cache};
use sea_movie_creator::media::subtitle_renderer::render_subtitle_cached;
use sea_movie_creator::media::visual_properties::{
    BlendMode, SubtitleProperties, TextAlignment, VisualProperties,
};
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, ClipKind, ProjectDocument, ProjectResolution,
};
use sea_movie_creator::ui::inspector_panel::{
    InspectorMode, InspectorParameterSlot, build_expression_command, build_switch_mode_command,
    build_upsert_keyframe_command,
};
use sea_movie_creator::ui::surface_bridge::composite_timeline_preview;

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new(tag: &str) -> Result<Self, std::io::Error> {
        let dir = std::env::temp_dir().join(format!("sea_b2_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_sample_png(
    path: &std::path::Path,
    w: u32,
    h: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut img = RgbaImage::new(w, h);
    for (_, _, px) in img.enumerate_pixels_mut() {
        *px = Rgba([20, 120, 240, 255]);
    }
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut bytes, ImageFormat::Png)?;
    std::fs::write(path, bytes.into_inner())?;
    Ok(())
}

#[test]
fn inspector_four_modes_image_subtitle_and_split_continuity()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TestDir::new("split_flow")?;
    let png_path = dir.0.join("overlay.png");
    write_sample_png(&png_path, 160, 90)?;
    let info = inspect_image_file(&png_path)?;
    assert_eq!((info.width, info.height), (160, 90));

    let mut frame_cache = FrameCache::new(DEFAULT_FRAME_CACHE_BUDGET_BYTES);
    let img_asset_id = AssetId::new("overlay_img")?;
    let img_frame = load_image_into_cache(&png_path, &img_asset_id, Some(128), &mut frame_cache)?;
    assert_eq!((img_frame.width, img_frame.height), (128, 72));

    let img_clip_id = ClipId::new("img_clip")?;
    let sub_clip_id = ClipId::new("sub_clip")?;
    let (mut doc, mut dispatcher) = setup_phase_b2_document(
        &png_path,
        info.width,
        info.height,
        &img_asset_id,
        &img_clip_id,
        &sub_clip_id,
    )?;

    configure_keyframes_expression_and_color(&mut doc, &mut dispatcher, &img_clip_id)?;
    verify_preview_and_split_continuity(
        &mut doc,
        &mut dispatcher,
        &img_asset_id,
        &img_frame,
        &img_clip_id,
        &sub_clip_id,
    )
}

fn setup_phase_b2_document(
    png_path: &std::path::Path,
    width: u32,
    height: u32,
    img_asset_id: &AssetId,
    img_clip_id: &ClipId,
    sub_clip_id: &ClipId,
) -> Result<(ProjectDocument, CommandDispatcher), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("phase_b2_test");
    doc.resolution = ProjectResolution::new(
        NonZeroU32::new(640).ok_or("non-zero")?,
        NonZeroU32::new(360).ok_or("non-zero")?,
    );
    let mut dispatcher = CommandDispatcher::new();
    let v_layer = LayerId::new("v1")?;
    let s_layer = LayerId::new("sub1")?;
    let clip_len = FrameCount::new(61)?;

    let setup = EditCommand::Batch {
        commands: vec![
            EditCommand::AddAsset {
                temp_id: None,
                id: Some(img_asset_id.clone()),
                name: "overlay.png".to_owned(),
                path: AssetPath::new(None, Some(png_path.to_string_lossy().into_owned())),
                kind: AssetKind::Image { width, height },
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(v_layer.clone()),
                name: "Overlay".to_owned(),
                insert_index: None,
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(s_layer.clone()),
                name: "Subtitles".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: None,
                id: Some(img_clip_id.clone()),
                layer_id: LayerRef::Id(v_layer),
                clip: Box::new(NewClipSpec {
                    name: "Logo".to_owned(),
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
                id: Some(sub_clip_id.clone()),
                layer_id: LayerRef::Id(s_layer),
                clip: Box::new(NewClipSpec {
                    name: "Subtitle".to_owned(),
                    timeline_start: FrameIndex::ZERO,
                    duration: clip_len,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    kind: NewClipKind::Subtitle(SubtitleProperties {
                        text: "こんにちは Phase B-2\n2行目字幕".to_owned(),
                        font_family: "sans-serif".to_owned(),
                        font_size: AnimatedParameter::constant(32.0),
                        text_color: RgbaColor::WHITE,
                        outline_color: RgbaColor::new(0, 0, 0, 255),
                        outline_width: 2.5,
                        alignment: TextAlignment::Center,
                        visual: VisualProperties::default(),
                    }),
                }),
            },
        ],
    };
    dispatcher.dispatch(&mut doc, &setup)?;
    Ok((doc, dispatcher))
}

fn configure_keyframes_expression_and_color(
    doc: &mut ProjectDocument,
    dispatcher: &mut CommandDispatcher,
    img_clip_id: &ClipId,
) -> Result<(), Box<dyn std::error::Error>> {
    let img_snap = doc.find_clip(img_clip_id).ok_or("img_clip")?.1.clone();
    let switch_to_kf = build_switch_mode_command(
        doc,
        &img_snap,
        InspectorParameterSlot::PositionX,
        InspectorMode::Keyframes,
        FrameIndex::ZERO,
    )?;
    dispatcher.dispatch(doc, &switch_to_kf)?;

    let img_snap = doc.find_clip(img_clip_id).ok_or("img_clip")?.1.clone();
    dispatcher.dispatch(
        doc,
        &build_upsert_keyframe_command(
            &img_snap,
            InspectorParameterSlot::PositionX,
            0,
            -60.0,
            EasingCurve::Linear,
        )?,
    )?;
    let img_snap = doc.find_clip(img_clip_id).ok_or("img_clip")?.1.clone();
    dispatcher.dispatch(
        doc,
        &build_upsert_keyframe_command(
            &img_snap,
            InspectorParameterSlot::PositionX,
            60,
            60.0,
            EasingCurve::Linear,
        )?,
    )?;

    dispatcher.dispatch(
        doc,
        &build_expression_command(
            &img_snap,
            InspectorParameterSlot::RotationDeg,
            "p * 90",
            45.0,
            2.0,
        )?,
    )?;

    dispatcher.dispatch(
        doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(img_clip_id.clone()),
            },
            path: ParameterPath::BlendMode,
            value: ParameterValue::BlendMode {
                value: BlendMode::Screen,
            },
        },
    )?;

    dispatcher.dispatch(
        doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(img_clip_id.clone()),
            },
            path: ParameterPath::Brightness,
            value: ParameterValue::Animated {
                param: AnimatedParameter::constant(0.15),
            },
        },
    )?;
    Ok(())
}

fn verify_preview_and_split_continuity(
    doc: &mut ProjectDocument,
    dispatcher: &mut CommandDispatcher,
    img_asset_id: &AssetId,
    img_frame: &Arc<sea_movie_creator::media::decoder::VideoFrameBuffer>,
    img_clip_id: &ClipId,
    sub_clip_id: &ClipId,
) -> Result<(), Box<dyn std::error::Error>> {
    let before_clip = doc.find_clip(img_clip_id).ok_or("before clip")?.1.clone();
    let before_ctx = EvaluationContext::new(
        30,
        before_clip.effect_offset_frames,
        before_clip
            .effect_duration_frames
            .unwrap_or(before_clip.duration),
        doc.fps,
    );
    let ClipKind::Image {
        visual: vis_before, ..
    } = &before_clip.kind
    else {
        return Err("expected image".into());
    };
    let eval_before = vis_before.evaluate(before_ctx)?;
    assert!((eval_before.x - 0.0).abs() < 1e-6);
    assert!((eval_before.rotation_deg - 135.0).abs() < 1e-6);
    assert!((eval_before.brightness - 0.15).abs() < 1e-6);
    let mat_before = eval_before.matrix;

    let sub_clip = doc.find_clip(sub_clip_id).ok_or("sub clip")?.1.clone();
    let ClipKind::Subtitle(sub_props) = &sub_clip.kind else {
        return Err("expected subtitle".into());
    };
    let sub_buf = render_subtitle_cached(sub_props, 32.0, 640);
    assert!(sub_buf.data.as_chunks::<4>().0.iter().any(|px| px[3] > 180));

    let scene = build_render_scene(doc, FrameIndex::new(30), &mut |id, _| {
        (id == img_asset_id).then(|| Arc::clone(img_frame))
    });
    assert_eq!(scene.layers.len(), 2);

    let cpu_out = composite_timeline_preview(doc, FrameIndex::new(30), 320, 180, &mut |id, _| {
        (id == img_asset_id).then(|| Arc::clone(img_frame))
    });
    assert!(!cpu_out.has_missing_media);
    assert!(
        cpu_out
            .buffer
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[0] > 40 || px[2] > 40)
    );

    let img_right_id = ClipId::new("img_right")?;
    let sub_right_id = ClipId::new("sub_right")?;
    dispatcher.dispatch(
        doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::SplitClip {
                    clip_id: ClipRef::Id(img_clip_id.clone()),
                    split_at_timeline_frame: FrameIndex::new(30),
                    right_temp_id: None,
                    right_id: Some(img_right_id.clone()),
                },
                EditCommand::SplitClip {
                    clip_id: ClipRef::Id(sub_clip_id.clone()),
                    split_at_timeline_frame: FrameIndex::new(30),
                    right_temp_id: None,
                    right_id: Some(sub_right_id.clone()),
                },
            ],
        },
    )?;

    let after_clip = doc.find_clip(&img_right_id).ok_or("right clip")?.1.clone();
    assert_eq!(after_clip.effect_offset_frames, 30);
    let after_ctx = EvaluationContext::new(
        0,
        after_clip.effect_offset_frames,
        after_clip
            .effect_duration_frames
            .unwrap_or(after_clip.duration),
        doc.fps,
    );
    let ClipKind::Image {
        visual: vis_after, ..
    } = &after_clip.kind
    else {
        return Err("expected right image".into());
    };
    let eval_after = vis_after.evaluate(after_ctx)?;
    let mat_after = eval_after.matrix;
    for &(px, py) in &[(0.0, 0.0), (80.0, -45.0), (-50.0, 60.0)] {
        let (ax, ay) = mat_before.transform_point(px, py);
        let (bx, by) = mat_after.transform_point(px, py);
        assert!((ax - bx).abs() < 1e-6);
        assert!((ay - by).abs() < 1e-6);
    }

    let sub_right = doc.find_clip(&sub_right_id).ok_or("sub right")?.1.clone();
    assert_eq!(sub_right.effect_offset_frames, 30);
    Ok(())
}
