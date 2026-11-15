//! Phase D-3 統合テスト実装。

use sea_movie_creator::core::animation::{
    AnimatedParameter, EasingCurve, ExtrapolationMode, Keyframe,
};
use sea_movie_creator::core::color::RgbaColor;
use sea_movie_creator::core::ids::{ClipId, ClipRef, LayerId, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex};
use sea_movie_creator::media::visual_properties::{BlendMode, SubtitleProperties};
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::ui::app_controller::AppController;
use sea_movie_creator::ui::inspector_panel::{
    InspectorMode, InspectorParameterSlot, MovementModeEdit, MovementModePreset,
    SegmentEndpointEdit, TrackBarDragStart, build_batch_relative_xy_offset_command,
    build_expression_command, build_reset_clip_parameters_command,
    build_reset_color_adjustments_command, build_set_segment_endpoint_command,
    build_set_segment_endpoint_with_lock, clip_has_non_default_color_adjustments,
    read_trackbar_segment_view,
};

fn add_test_subtitle(
    controller: &mut AppController,
    layer: &str,
    clip: &str,
    start: u64,
    duration: u64,
) -> Result<ClipId, Box<dyn std::error::Error>> {
    let lid = LayerId::new(layer)?;
    let cid = ClipId::new(clip)?;
    let mut cmds = Vec::new();
    if controller.document.find_layer(&lid).is_none() {
        cmds.push(EditCommand::AddLayer {
            temp_id: None,
            id: Some(lid.clone()),
            name: layer.to_owned(),
            insert_index: None,
        });
    }
    cmds.push(EditCommand::AddClip {
        temp_id: None,
        id: Some(cid.clone()),
        layer_id: LayerRef::Id(lid),
        clip: Box::new(NewClipSpec {
            name: clip.to_owned(),
            timeline_start: FrameIndex::new(start),
            duration: FrameCount::new(duration)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: NewClipKind::Subtitle(SubtitleProperties::new("Hello AviUtl")),
        }),
    });
    controller.dispatch_command(&EditCommand::Batch { commands: cmds })?;
    controller.select_single_clip(Some(cid.clone()));
    Ok(cid)
}

#[test]
fn segment_edits_scale_lock_opacity_ui_and_expression_undo_flow()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let cid = add_test_subtitle(&mut controller, "layer_1", "sub_main", 0, 120)?;

    // Seek to frame 60 and add midpoint
    controller.seek_to_frame(FrameIndex::new(60));
    controller.add_midpoint_at_playhead()?;
    controller.select_clip_segment(&cid, 1);

    // Edit Transparency in UI % (75.0% transparent => 0.25 opacity internal)
    let op_internal = InspectorParameterSlot::Opacity.ui_to_internal(75.0);
    assert!((op_internal - 0.25).abs() < 1e-6);
    let op_cmd = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        build_set_segment_endpoint_command(
            &controller.document,
            clip,
            InspectorParameterSlot::Opacity,
            1,
            true,
            op_internal,
        )?
    };
    controller.dispatch_command(&op_cmd)?;

    // Switch ScaleX with aspect lock -> Batch changes both ScaleX and ScaleY
    let scale_cmd = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        build_set_segment_endpoint_with_lock(
            &controller.document,
            clip,
            &SegmentEndpointEdit {
                slot: InspectorParameterSlot::ScaleX,
                segment_index: 1,
                is_end: true,
                new_value: 2.5,
                scale_aspect_locked: true,
                aspect_ratio_override: None,
            },
        )?
    };
    controller.dispatch_command(&scale_cmd)?;
    {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        let vy = read_trackbar_segment_view(
            &controller.document,
            clip,
            InspectorParameterSlot::ScaleY,
            1,
        )
        .ok_or("vy")?;
        assert!((vy.end_value - 2.5).abs() < 1e-6);
    }

    // Apply EaseInOut movement mode across all segments of Opacity
    let mode_cmd = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        sea_movie_creator::ui::inspector_panel::build_apply_movement_mode_with_options(
            &controller.document,
            clip,
            &MovementModeEdit {
                slot: InspectorParameterSlot::Opacity,
                preset: MovementModePreset::EaseInOut,
                playhead: FrameIndex::new(60),
                bezier_curve: EasingCurve::EaseInOut,
                scale_aspect_locked: false,
            },
        )?
    };
    controller.dispatch_command(&mode_cmd)?;

    // Switch X to Expression f(t)
    let expr_cmd = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        build_expression_command(
            clip,
            InspectorParameterSlot::PositionX,
            "100 * p",
            0.0,
            1.0,
        )?
    };
    controller.dispatch_command(&expr_cmd)?;

    // Full Undo stack back to initial constant state
    assert!(controller.undo()?); // undo expr
    assert!(controller.undo()?); // undo EaseInOut
    assert!(controller.undo()?); // undo scale batch
    assert!(controller.undo()?); // undo opacity edit

    let (_, clip_restored) = controller.document.find_clip(&cid).ok_or("restored")?;
    let vx = read_trackbar_segment_view(
        &controller.document,
        clip_restored,
        InspectorParameterSlot::PositionX,
        0,
    )
    .ok_or("vx")?;
    let vsy = read_trackbar_segment_view(
        &controller.document,
        clip_restored,
        InspectorParameterSlot::ScaleY,
        0,
    )
    .ok_or("vsy")?;
    assert_eq!(vx.mode, InspectorMode::Constant);
    assert!((vsy.end_value - 1.0).abs() < 1e-6);
    Ok(())
}

#[test]
fn continuous_trackbar_drag_commits_single_undo_and_supports_shift_fine_control()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let cid = add_test_subtitle(&mut controller, "layer_1", "sub_drag", 0, 100)?;
    let undo_before = controller.dispatcher.history().undo_len();

    controller.begin_trackbar_drag(TrackBarDragStart {
        clip_id: cid.clone(),
        slot: InspectorParameterSlot::PositionX,
        segment_index: 0,
        is_end: false,
        pointer_x_px: 100.0,
        track_width_px: 120.0,
    });
    // Coarse drag (+30px out of 120px over 3840 range = +960)
    controller.update_trackbar_drag(130.0, false);
    let mid_drag_x = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        read_trackbar_segment_view(
            &controller.document,
            clip,
            InspectorParameterSlot::PositionX,
            0,
        )
        .ok_or("vx")?
        .start_value
    };
    assert!((mid_drag_x - 960.0).abs() < 1e-2);
    // Zero undo entries added while dragging
    assert_eq!(controller.dispatcher.history().undo_len(), undo_before);

    // Fine Shift drag (+10px at 0.1x sensitivity => +32px instead of +320px)
    controller.update_trackbar_drag(140.0, true);
    let shift_drag_x = {
        let (_, clip) = controller.document.find_clip(&cid).ok_or("clip")?;
        read_trackbar_segment_view(
            &controller.document,
            clip,
            InspectorParameterSlot::PositionX,
            0,
        )
        .ok_or("vx")?
        .start_value
    };
    assert!((shift_drag_x - 992.0).abs() < 1e-2);

    // MouseUp commits ONE Undo entry
    assert!(controller.finish_trackbar_drag()?);
    assert_eq!(controller.dispatcher.history().undo_len(), undo_before + 1);

    // One Ctrl+Z returns cleanly to 0.0
    assert!(controller.undo()?);
    let (_, reverted) = controller.document.find_clip(&cid).ok_or("clip")?;
    let reverted_x = read_trackbar_segment_view(
        &controller.document,
        reverted,
        InspectorParameterSlot::PositionX,
        0,
    )
    .ok_or("vx")?
    .start_value;
    assert!(reverted_x.abs() < 1e-6);
    Ok(())
}

#[test]
fn unaligned_keyframes_color_badge_multi_offset_and_reset() -> Result<(), Box<dyn std::error::Error>>
{
    let mut controller = AppController::new();
    let cid1 = add_test_subtitle(&mut controller, "layer_1", "sub_a", 0, 90)?;
    let cid2 = add_test_subtitle(&mut controller, "layer_2", "sub_b", 0, 90)?;

    // Inject unaligned keyframes manually and test alignment when editing endpoint
    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(cid1.clone()),
        },
        path: ParameterPath::Y,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Keyframes {
                keyframes: vec![Keyframe::new(20, 50.0), Keyframe::new(50, 150.0)],
                extrapolation: ExtrapolationMode::Clamp,
            },
        },
    })?;

    // Color adjustment detection & reset
    {
        let (_, clip1) = controller.document.find_clip(&cid1).ok_or("c1")?;
        assert!(!clip_has_non_default_color_adjustments(clip1));
    }
    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(cid1.clone()),
        },
        path: ParameterPath::Gamma,
        value: ParameterValue::Animated {
            param: AnimatedParameter::constant(0.5),
        },
    })?;
    {
        let (_, clip1) = controller.document.find_clip(&cid1).ok_or("c1")?;
        assert!(clip_has_non_default_color_adjustments(clip1));
        let reset_color = build_reset_color_adjustments_command(clip1).ok_or("reset")?;
        controller.dispatch_command(&reset_color)?;
        let (_, clip1_after) = controller.document.find_clip(&cid1).ok_or("c1")?;
        assert!(!clip_has_non_default_color_adjustments(clip1_after));
    }

    // Batch relative XY offset for multiple selected clips
    let batch_xy = build_batch_relative_xy_offset_command(
        &controller.document,
        &[cid1.clone(), cid2.clone()],
        25.0,
        -15.0,
    )?;
    controller.dispatch_command(&batch_xy)?;
    let (_, c2) = controller.document.find_clip(&cid2).ok_or("c2")?;
    let c2_x = read_trackbar_segment_view(
        &controller.document,
        c2,
        InspectorParameterSlot::PositionX,
        0,
    )
    .ok_or("vx")?
    .start_value;
    assert!((c2_x - 25.0).abs() < 1e-6);

    // Full clip parameter reset restores BlendMode Normal and outline/color defaults
    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(cid1.clone()),
        },
        path: ParameterPath::BlendMode,
        value: ParameterValue::BlendMode {
            value: BlendMode::Add,
        },
    })?;
    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(cid1.clone()),
        },
        path: ParameterPath::SubtitleTextColor,
        value: ParameterValue::Color {
            value: RgbaColor::new(255, 0, 0, 255),
        },
    })?;
    let reset_all = {
        let (_, clip1) = controller.document.find_clip(&cid1).ok_or("c1")?;
        build_reset_clip_parameters_command(clip1).ok_or("reset all")?
    };
    controller.dispatch_command(&reset_all)?;
    let (_, c1_final) = controller.document.find_clip(&cid1).ok_or("c1")?;
    if let sea_movie_creator::timeline::document::ClipKind::Subtitle(sub) = &c1_final.kind {
        assert_eq!(sub.visual.blend_mode, BlendMode::Normal);
        assert_eq!(sub.text_color, RgbaColor::WHITE);
    } else {
        return Err("expected subtitle".into());
    }
    Ok(())
}
