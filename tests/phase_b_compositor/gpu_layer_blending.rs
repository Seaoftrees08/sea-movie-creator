//! Phase B-1: `RenderScene` 構築、アンカー回転頂点、色調整・半透明・回転の `wgpu` 合成検証。

use std::num::NonZeroU32;
use std::sync::Arc;

use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::color::RgbaColor;
use sea_movie_creator::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex, RationalDuration};
use sea_movie_creator::media::compositor::{
    RenderLayerItem, RenderScene, WgpuCompositor, build_render_scene, compute_layer_quad_ndc,
};
use sea_movie_creator::media::decoder::VideoFrameBuffer;
use sea_movie_creator::media::visual_properties::{BlendMode, EvaluatedVisual, VisualProperties};
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AssetStatus, Clip, ClipKind, Layer, ProjectDocument, ProjectResolution,
};
use sea_movie_creator::ui::surface_bridge::{
    PreviewSurfaceBridge, RenderImageSurfaceBridge, composite_timeline_preview,
};

fn make_image_layer_cmds(
    asset_id: &AssetId,
    layer_id: &LayerId,
    clip_id: &ClipId,
    width: u32,
    height: u32,
) -> Result<Vec<EditCommand>, Box<dyn std::error::Error>> {
    Ok(vec![
        EditCommand::AddAsset {
            temp_id: None,
            id: Some(asset_id.clone()),
            name: format!("{asset_id}.png"),
            path: AssetPath::from_absolute(format!("{asset_id}.png")),
            kind: AssetKind::Image { width, height },
        },
        EditCommand::AddLayer {
            temp_id: None,
            id: Some(layer_id.clone()),
            name: layer_id.to_string(),
            insert_index: None,
        },
        EditCommand::AddClip {
            temp_id: None,
            id: Some(clip_id.clone()),
            layer_id: LayerRef::Id(layer_id.clone()),
            clip: Box::new(NewClipSpec {
                name: clip_id.to_string(),
                timeline_start: FrameIndex::ZERO,
                duration: FrameCount::new(60)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: NewClipKind::Image {
                    asset_id: AssetRef::Id(asset_id.clone()),
                    visual: VisualProperties::default(),
                },
            }),
        },
    ])
}

fn setup_two_layer_doc() -> Result<(ProjectDocument, AssetId, AssetId), Box<dyn std::error::Error>>
{
    let mut doc = ProjectDocument::new("PhaseB1Test");
    let size_200 = NonZeroU32::new(200).ok_or("200 must be non-zero")?;
    doc.resolution = ProjectResolution::new(size_200, size_200);

    let mut dispatcher = CommandDispatcher::new();
    let asset_bottom = AssetId::new("bg_blue")?;
    let asset_top = AssetId::new("fg_red")?;
    let clip_bottom = ClipId::new("c_bottom")?;
    let clip_top = ClipId::new("c_top")?;
    let layer_bottom = LayerId::new("layer_bg")?;
    let layer_top = LayerId::new("layer_fg")?;

    let mut commands = make_image_layer_cmds(&asset_bottom, &layer_bottom, &clip_bottom, 200, 200)?;
    commands.extend(make_image_layer_cmds(
        &asset_top, &layer_top, &clip_top, 100, 40,
    )?);
    for (path, val) in [
        (ParameterPath::Opacity, 0.5),
        (ParameterPath::RotationDeg, 90.0),
        (ParameterPath::HueDeg, 120.0),
    ] {
        commands.push(EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(clip_top.clone()),
            },
            path,
            value: ParameterValue::Animated {
                param: AnimatedParameter::constant(val),
            },
        });
    }
    dispatcher.dispatch(&mut doc, &EditCommand::Batch { commands })?;
    Ok((doc, asset_bottom, asset_top))
}

#[test]
fn multi_layer_scene_evaluates_color_rotation_and_surface_bridge_upload()
-> Result<(), Box<dyn std::error::Error>> {
    let (doc, bg_id, fg_id) = setup_two_layer_doc()?;
    let bg_buf = Arc::new(VideoFrameBuffer::solid_bgra8(
        200,
        200,
        RgbaColor::new(0, 0, 220, 255),
    ));
    let fg_buf = Arc::new(VideoFrameBuffer::solid_bgra8(
        100,
        40,
        RgbaColor::new(255, 0, 0, 255),
    ));

    let mut lookup = |id: &AssetId, _: u64| -> Option<Arc<VideoFrameBuffer>> {
        if id == &bg_id {
            Some(Arc::clone(&bg_buf))
        } else if id == &fg_id {
            Some(Arc::clone(&fg_buf))
        } else {
            None
        }
    };

    let scene = build_render_scene(&doc, FrameIndex::ZERO, &mut lookup);
    assert!(!scene.has_missing_media);
    assert_eq!(scene.layers.len(), 2);
    assert_eq!(scene.layers[0].cache_key.asset_id, bg_id);
    assert_eq!(scene.layers[1].cache_key.asset_id, fg_id);
    assert!((scene.layers[1].visual.opacity - 0.5).abs() < 1e-6);
    assert!((scene.layers[1].visual.hue_deg - 120.0).abs() < 1e-6);

    // 100x40 を時計回り90度回転 -> 200x200 キャンバスでは幅 [-20..20] (NDC ±0.2), 高さ [-50..50] (NDC ±0.5)
    let top_quad = scene.layers[1].quad_vertices;
    // 左上角 (-50, -20) -> 時計回り90度で (+20, -50) -> NDC (+0.2, +0.5)
    assert!((top_quad[0].ndc_pos[0] - 0.2).abs() < 1e-4);
    assert!((top_quad[0].ndc_pos[1] - 0.5).abs() < 1e-4);

    let fallback = composite_timeline_preview(&doc, FrameIndex::ZERO, 200, 200, &mut lookup);
    let mut bridge = RenderImageSurfaceBridge::new();
    bridge.submit_bgra_frame(&fallback.buffer);
    assert_eq!(bridge.current_frame_id(), 1);
    assert_eq!(bridge.current_dimensions(), Some((200, 200)));
    assert!(bridge.front_image().is_some());
    Ok(())
}

#[test]
fn missing_asset_generates_fallback_layer_in_render_scene() -> Result<(), Box<dyn std::error::Error>>
{
    let mut doc = ProjectDocument::new("MissingFallback");
    let gone = AssetId::new("gone")?;
    doc.assets
        .push(sea_movie_creator::timeline::document::Asset {
            id: gone.clone(),
            name: "missing.mkv".to_owned(),
            path: AssetPath::from_absolute("missing.mkv"),
            kind: AssetKind::Video {
                width: Some(640),
                height: Some(360),
                default_fps: None,
                duration_frames: None,
                source_duration: RationalDuration::new(2, 1)?,
                has_audio: false,
            },
            status: AssetStatus::Missing {
                expected_path: "missing.mkv".to_owned(),
            },
        });
    let mut layer = Layer::new(LayerId::new("l1")?, "L1");
    layer.clips.push(Clip {
        id: ClipId::new("c1")?,
        name: "Missing".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: FrameCount::new(30)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Image {
            asset_id: gone,
            visual: VisualProperties::default(),
        },
    });
    doc.layers.push(layer);

    let mut empty_lookup = |_: &AssetId, _: u64| None;
    let scene = build_render_scene(&doc, FrameIndex::ZERO, &mut empty_lookup);
    assert!(scene.has_missing_media);
    assert_eq!(scene.layers.len(), 1);
    assert!(scene.layers[0].is_missing_placeholder);
    Ok(())
}

#[test]
#[ignore = "requires wgpu graphics adapter"]
fn wgpu_renders_alpha_rotated_and_hue_shifted_layers_to_bgra8()
-> Result<(), Box<dyn std::error::Error>> {
    let mut compositor = WgpuCompositor::try_new()?;

    let bg_buf = Arc::new(VideoFrameBuffer::solid_bgra8(
        128,
        128,
        RgbaColor::new(0, 0, 200, 255),
    ));
    let fg_buf = Arc::new(VideoFrameBuffer::solid_bgra8(
        80,
        40,
        RgbaColor::new(255, 0, 0, 255),
    ));

    let bg_visual =
        EvaluatedVisual::from_components(0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, BlendMode::Normal);
    let fg_visual = EvaluatedVisual::from_components(
        0.0,
        0.0,
        1.0,
        1.0,
        90.0,
        0.0,
        0.0,
        0.5,
        BlendMode::Normal,
    )
    .with_color_adjustments(0.0, 1.0, 1.0, 120.0, 1.0);

    let scene = RenderScene {
        project_width: 128,
        project_height: 128,
        background_color: RgbaColor::BLACK,
        layers: vec![
            RenderLayerItem {
                cache_key: sea_movie_creator::media::compositor::texture_pool::GpuTextureKey {
                    asset_id: AssetId::new("bg")?,
                    frame_index: 0,
                    width: 128,
                    height: 128,
                },
                source_buffer: bg_buf,
                visual: bg_visual,
                quad_vertices: compute_layer_quad_ndc(128, 128, 128, 128, &bg_visual),
                is_missing_placeholder: false,
            },
            RenderLayerItem {
                cache_key: sea_movie_creator::media::compositor::texture_pool::GpuTextureKey {
                    asset_id: AssetId::new("fg")?,
                    frame_index: 0,
                    width: 80,
                    height: 40,
                },
                source_buffer: fg_buf,
                visual: fg_visual,
                quad_vertices: compute_layer_quad_ndc(80, 40, 128, 128, &fg_visual),
                is_missing_placeholder: false,
            },
        ],
        has_missing_media: false,
    };

    let full = compositor.render_scene_to_bgra8(&scene, 128, 128)?;
    assert_eq!(full.width, 128);
    assert_eq!(full.height, 128);

    // 80x40 を90度回転 -> 縦長 (40x80) なので (64, 30) には赤->色相+120度回転後の緑 (~128) が50%合成される
    let inside_idx = (30 * 128 + 64) * 4;
    let b_in = full.data[inside_idx];
    let g_in = full.data[inside_idx + 1];
    let r_in = full.data[inside_idx + 2];
    assert!(
        g_in > 95 && g_in < 160,
        "expected blended green ~128, got {g_in}"
    );
    assert!(
        b_in > 75 && b_in < 125,
        "expected blended blue ~100, got {b_in}"
    );
    assert!(
        r_in < 15,
        "expected red ~0 after +120deg hue rotation, got {r_in}"
    );

    // 回転後の横幅外 (x=10, y=64) は背景の青 (0, 0, 200) がそのまま残る
    let outside_idx = (64 * 128 + 10) * 4;
    assert!(full.data[outside_idx] > 180);
    assert!(full.data[outside_idx + 1] < 10);

    // 縮小 (64x64) ダウンスケール読出しも正常動作すること
    let scaled = compositor.render_scene_to_bgra8(&scene, 64, 64)?;
    assert_eq!(scaled.width, 64);
    assert_eq!(scaled.height, 64);
    assert_eq!(scaled.data.len(), 64 * 64 * 4);
    Ok(())
}
