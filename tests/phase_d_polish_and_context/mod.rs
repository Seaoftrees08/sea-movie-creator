use std::fs;
use std::path::PathBuf;

use sea_movie_creator::ai::context_builder::{
    TimelineLengthSummary, build_ai_request, build_timeline_context, default_editor_system_prompt,
};
use sea_movie_creator::core::animation::{
    AnimatedParameter, EasingCurve, EvaluationContext, ExtrapolationMode,
};
use sea_movie_creator::core::ids::{ClipId, ClipRef, LayerId, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex, FrameRate};
use sea_movie_creator::export::registry::{PluginRegistry, RegistryError};
use sea_movie_creator::export::runner::{
    ExportCancelToken, ExportJobRequest, ExportMediaSources, resolve_export_frame_range,
    run_export_job,
};
use sea_movie_creator::media::visual_properties::SubtitleProperties;
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::document::{FrameRange, ProjectDocument, TimelineLengthMode};
use sea_movie_creator::ui::app_controller::{AppController, PlaybackState, TimeDisplayUnit};
use sea_movie_creator::ui::inspector::{InspectorParameterSlot, build_expression_command};
use sea_movie_creator::ui::timeline::context_menu::{
    evaluate_clip_menu_availability, evaluate_lane_menu_availability,
    midpoint_effect_frame_for_timeline_frame, midpoint_timeline_frame, segment_index_at_frame,
};

struct TempExportDir(PathBuf);

impl TempExportDir {
    fn new(tag: &str) -> Result<Self, std::io::Error> {
        let dir = std::env::temp_dir().join(format!("sea-d5-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempExportDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn setup_dummy_plugin_dir(root: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let plugin_dir = root.join("dummy");
    fs::create_dir_all(&plugin_dir)?;
    let manifest = serde_json::json!({
        "schema_version": 1,
        "id": "sea.export.dummy",
        "name": "Dummy Protocol Plugin",
        "version": "0.1.0",
        "kind": "export",
        "entrypoint": "sea-export-dummy",
        "containers": ["json"],
        "license": {
            "spdx": "MIT OR Apache-2.0",
            "notice": "Protocol integration test dummy plugin.",
            "requires_external_binary": false
        },
        "capabilities": {
            "supports_alpha": true,
            "supports_audio": true,
            "pixel_format": "bgra8",
            "audio_sample_format": "f32_interleaved",
            "hw_accelerated": false
        },
        "presets": [
            {
                "id": "dummy_verify",
                "label": "Dummy Verify",
                "description": "IPC checksum verifier",
                "video_codec": "raw-xor",
                "audio_codec": "raw-f32",
                "container_ext": "json",
                "default_options": {}
            }
        ]
    });
    fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(plugin_dir)
}

fn seed_two_layer_controller()
-> Result<(AppController, LayerId, LayerId, ClipId), Box<dyn std::error::Error>> {
    let mut ctrl = AppController::from_document(ProjectDocument::new("D5Test"));
    ctrl.document.fps = FrameRate::new(30, 1)?;
    let l1 = LayerId::new("layer-back")?;
    let l2 = LayerId::new("layer-front")?;
    let c1 = ClipId::new("clip-main")?;
    ctrl.dispatch_command(&EditCommand::Batch {
        commands: vec![
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(l1.clone()),
                name: "Back".to_owned(),
                insert_index: None,
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(l2.clone()),
                name: "Front".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: None,
                id: Some(c1.clone()),
                layer_id: LayerRef::Id(l1.clone()),
                clip: Box::new(NewClipSpec {
                    name: "MainSub".to_owned(),
                    timeline_start: FrameIndex::new(30),
                    duration: FrameCount::new(90)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("Intro")),
                }),
            },
        ],
    })?;
    ctrl.select_single_clip(Some(c1.clone()));
    Ok((ctrl, l1, l2, c1))
}

#[test]
fn context_menu_availability_and_locked_guards() -> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, l1, _l2, c1) = seed_two_layer_controller()?;
    ctrl.seek_to_frame(FrameIndex::new(60));
    ctrl.toggle_snap_enabled();
    ctrl.toggle_time_display_unit();

    let lane_avail = evaluate_lane_menu_availability(&ctrl, &l1, FrameIndex::new(15));
    assert!(!lane_avail.flags.layer_locked);
    assert!(!lane_avail.flags.has_clipboard);
    assert!(!lane_avail.flags.has_selection_range);
    assert!(!lane_avail.toggles.snap_enabled);
    assert!(lane_avail.toggles.show_frames);
    assert_eq!(ctrl.time_display_unit, TimeDisplayUnit::Frames);

    ctrl.copy_selected_clip()?;
    ctrl.set_selection_range_start_at(FrameIndex::new(10))?;
    let lane_after = evaluate_lane_menu_availability(&ctrl, &l1, FrameIndex::new(15));
    assert!(lane_after.flags.has_clipboard);
    assert!(lane_after.flags.has_selection_range);

    let (_, clip_ref) = ctrl.document.find_clip(&c1).ok_or("clip missing")?;
    let clip_avail = evaluate_clip_menu_availability(
        &ctrl.document,
        clip_ref,
        false,
        FrameIndex::new(50),
        FrameIndex::new(60),
    );
    assert!(clip_avail.split.can_split_at_click);
    assert!(clip_avail.split.can_split_at_playhead);
    assert!(clip_avail.split.can_add_midpoint_at_click);
    assert!(clip_avail.mutation.can_add_midpoint_at_playhead);
    assert!(!clip_avail.mutation.can_align_end_to_playhead);
    assert_eq!(
        midpoint_effect_frame_for_timeline_frame(clip_ref, FrameIndex::new(50)),
        Some(20)
    );

    ctrl.add_midpoint(&c1, 30)?;
    let (_, clip_with_mp) = ctrl.document.find_clip(&c1).ok_or("clip missing")?;
    assert_eq!(segment_index_at_frame(clip_with_mp, FrameIndex::new(40)), 0);
    assert_eq!(segment_index_at_frame(clip_with_mp, FrameIndex::new(75)), 1);
    assert_eq!(
        midpoint_timeline_frame(clip_with_mp, 30),
        FrameIndex::new(60)
    );
    assert_eq!(
        midpoint_effect_frame_for_timeline_frame(clip_with_mp, FrameIndex::new(60)),
        None
    );

    ctrl.toggle_layer_locked(&l1)?;
    let locked_lane = evaluate_lane_menu_availability(&ctrl, &l1, FrameIndex::new(15));
    assert!(locked_lane.flags.layer_locked);
    let (_, locked_clip) = ctrl.document.find_clip(&c1).ok_or("clip missing")?;
    let locked_clip_avail = evaluate_clip_menu_availability(
        &ctrl.document,
        locked_clip,
        true,
        FrameIndex::new(50),
        FrameIndex::new(100),
    );
    assert!(!locked_clip_avail.split.can_split_at_click);
    assert!(!locked_clip_avail.split.can_split_at_playhead);
    assert!(!locked_clip_avail.mutation.can_align_end_to_playhead);
    assert!(ctrl.split_clip_at_frame(&c1, FrameIndex::new(50)).is_err());
    Ok(())
}

#[test]
fn context_menu_actions_and_full_undo_parity() -> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, l1, l2, c1) = seed_two_layer_controller()?;
    let baseline_doc = ctrl.document.clone();
    let baseline_undo = ctrl.dispatcher.history().undo_len();

    // 1. Track lane & layer context actions
    ctrl.insert_default_subtitle_at(Some(&l1), FrameIndex::new(0))?;
    ctrl.insert_layer_at(1, Some("MidTrack".to_owned()))?;
    ctrl.rename_layer(&l2, "ForegroundRenamed")?;
    ctrl.toggle_layer_visible(&l2)?;
    ctrl.toggle_layer_muted(&l2)?;

    // 2. Range, length & copy/paste context actions
    ctrl.set_selection_range_start_at(FrameIndex::new(15))?;
    ctrl.set_selection_range_end_at(FrameIndex::new(75))?;
    ctrl.set_timeline_length(TimelineLengthMode::Manual {
        end_frame: FrameCount::new(360)?,
    })?;
    ctrl.select_single_clip(Some(c1.clone()));
    ctrl.copy_selected_clip()?;
    ctrl.paste_clipboard_at(Some(&l2), FrameIndex::new(40))?;

    // 3. Clip alignment, midpoint & split context actions
    ctrl.seek_to_frame(FrameIndex::new(180));
    ctrl.align_clip_end_to_playhead(&c1)?;
    ctrl.add_midpoint(&c1, 45)?;
    ctrl.move_midpoint(&c1, 45, 50)?;
    ctrl.remove_midpoint(&c1, 50)?;

    // 4. TrackBarRow context menu slot actions (curve copy, flatten, expression, reset)
    run_slot_context_ops(&mut ctrl, &c1)?;
    ctrl.split_clip_at_frame(&c1, FrameIndex::new(135))?;

    // Full Undo audit back to baseline_doc
    let steps = ctrl.dispatcher.history().undo_len() - baseline_undo;
    assert!(steps >= 17, "expected >= 17 undo steps, got {steps}");
    for _ in 0..steps {
        assert!(ctrl.undo()?);
    }
    assert_eq!(
        serde_json::to_value(&ctrl.document)?,
        serde_json::to_value(&baseline_doc)?
    );
    assert!(ctrl.dispatcher.history().can_redo());
    assert!(ctrl.redo()?);
    assert!(ctrl.undo()?);
    Ok(())
}

fn run_slot_context_ops(
    ctrl: &mut AppController,
    clip_id: &ClipId,
) -> Result<(), Box<dyn std::error::Error>> {
    let easing_x = EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(clip_id.clone()),
        },
        path: ParameterPath::X,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Easing {
                start_value: 0.0,
                end_value: 180.0,
                curve: EasingCurve::EaseInOut,
                extrapolation: ExtrapolationMode::default(),
            },
        },
    };
    ctrl.dispatch_command(&easing_x)?;
    ctrl.copy_clip_slot_curve(
        clip_id,
        InspectorParameterSlot::PositionX,
        InspectorParameterSlot::PositionY,
    )?;
    {
        let (_, c_now) = ctrl.document.find_clip(clip_id).ok_or("clip")?;
        let y_param = InspectorParameterSlot::PositionY
            .extract_param(c_now)
            .ok_or("missing Y")?;
        assert!(
            matches!(y_param, AnimatedParameter::Easing { end_value, .. } if (*end_value - 180.0).abs() < 1e-6)
        );
    }
    ctrl.seek_to_frame(FrameIndex::new(135));
    let expected_at_135 = {
        let (_, c_now) = ctrl.document.find_clip(clip_id).ok_or("clip")?;
        let y_param = InspectorParameterSlot::PositionY
            .extract_param(c_now)
            .ok_or("missing Y")?;
        let eval_ctx = EvaluationContext::new(
            135 - c_now.timeline_start.get(),
            c_now.effect_offset_frames,
            c_now.effect_duration_frames.unwrap_or(c_now.duration),
            ctrl.document.fps,
        );
        y_param.evaluate(eval_ctx)?
    };
    ctrl.flatten_clip_slot_to_constant(clip_id, InspectorParameterSlot::PositionY)?;
    {
        let (_, c_now) = ctrl.document.find_clip(clip_id).ok_or("clip")?;
        let y_param = InspectorParameterSlot::PositionY
            .extract_param(c_now)
            .ok_or("missing Y")?;
        assert!(
            matches!(y_param, AnimatedParameter::Constant { value } if (*value - expected_at_135).abs() < 1e-6),
            "expected constant {expected_at_135}, got {y_param:?}"
        );
    }
    ctrl.activate_slot_expression_editor(clip_id, InspectorParameterSlot::PositionX)?;
    let expr_cmd = {
        let (_, c_now) = ctrl.document.find_clip(clip_id).ok_or("clip")?;
        build_expression_command(c_now, InspectorParameterSlot::PositionX, "t * 10", 0.0, 1.0)?
    };
    ctrl.dispatch_command(&expr_cmd)?;
    ctrl.reset_clip_slot_to_default(clip_id, InspectorParameterSlot::PositionX)?;
    Ok(())
}

#[test]
fn follow_selected_on_seek_tracks_frontmost_unlocked_clip() -> Result<(), Box<dyn std::error::Error>>
{
    let (mut ctrl, l_back, l_front, c_back) = seed_two_layer_controller()?;
    let c_front = ClipId::new("clip-front")?;
    let l_locked = LayerId::new("layer-locked")?;
    let c_locked = ClipId::new("clip-locked")?;
    ctrl.dispatch_command(&EditCommand::Batch {
        commands: vec![
            EditCommand::AddClip {
                temp_id: None,
                id: Some(c_front.clone()),
                layer_id: LayerRef::Id(l_front.clone()),
                clip: Box::new(NewClipSpec {
                    name: "FrontSub".to_owned(),
                    timeline_start: FrameIndex::new(150),
                    duration: FrameCount::new(60)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("Front")),
                }),
            },
            EditCommand::AddLayer {
                temp_id: None,
                id: Some(l_locked.clone()),
                name: "LockedFront".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: None,
                id: Some(c_locked.clone()),
                layer_id: LayerRef::Id(l_locked.clone()),
                clip: Box::new(NewClipSpec {
                    name: "LockedSub".to_owned(),
                    timeline_start: FrameIndex::new(30),
                    duration: FrameCount::new(90)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("Locked")),
                }),
            },
            EditCommand::SetLayerFlags {
                layer_id: LayerRef::Id(l_locked.clone()),
                name: None,
                visible: None,
                muted: None,
                locked: Some(true),
            },
        ],
    })?;

    ctrl.select_single_clip(Some(c_back.clone()));
    ctrl.toggle_follow_selected_on_seek();
    assert!(ctrl.follow_selected_on_seek);

    // Within current selected clip -> keeps selection on c_back (skips locked l_locked)
    ctrl.seek_to_frame(FrameIndex::new(45));
    assert_eq!(ctrl.selected_clip_id.as_ref(), Some(&c_back));

    // Outside current clip -> jumps to frontmost unlocked clip at 165 (`c_front`)
    ctrl.seek_to_frame(FrameIndex::new(165));
    assert_eq!(ctrl.selected_clip_id.as_ref(), Some(&c_front));

    // When advancing via tick_playback during playback back onto frame 50 -> switches back to c_back (skipping locked l_locked)
    ctrl.seek_to_frame(FrameIndex::new(0));
    ctrl.selected_clip_id = Some(c_front);
    ctrl.playback_state = PlaybackState::Playing;
    ctrl.clock.seek_to_frame(FrameIndex::new(50));
    ctrl.tick_playback(1.0 / 30.0);
    assert_eq!(ctrl.selected_clip_id.as_ref(), Some(&c_back));
    let _ = (l_back, l_front, l_locked, c_locked);
    Ok(())
}

#[test]
fn selection_only_export_range_and_runner_execution() -> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, _l1, _l2, _c1) = seed_two_layer_controller()?;
    assert_eq!(
        resolve_export_frame_range(&ctrl.document, false),
        (FrameIndex::ZERO, FrameIndex::new(120))
    );
    ctrl.set_selection_range(Some(FrameRange {
        start: FrameIndex::new(20),
        end: FrameIndex::new(50),
    }))?;
    assert_eq!(
        resolve_export_frame_range(&ctrl.document, true),
        (FrameIndex::new(20), FrameIndex::new(50))
    );

    // Clamped beyond total_duration_frames()
    ctrl.set_selection_range(Some(FrameRange {
        start: FrameIndex::new(80),
        end: FrameIndex::new(900),
    }))?;
    assert_eq!(
        resolve_export_frame_range(&ctrl.document, true),
        (FrameIndex::new(80), FrameIndex::new(120))
    );

    // Restore exact 30-frame selection [30, 60) and execute via sea-export-dummy
    ctrl.set_selection_range(Some(FrameRange {
        start: FrameIndex::new(30),
        end: FrameIndex::new(60),
    }))?;
    let tmp = TempExportDir::new("sel-export")?;
    let _plugin_dir = setup_dummy_plugin_dir(tmp.path())?;
    let registry = PluginRegistry::discover_in_dirs(&[tmp.path().to_path_buf()]);
    if let Err(RegistryError::MissingExecutable { entrypoint, .. }) =
        registry.resolve_runnable("sea.export.dummy", "dummy_verify")
    {
        return Err(format!("missing dummy binary `{entrypoint}`; build workspace first").into());
    }

    ctrl.export_dialog.registry = registry.clone();
    ctrl.export_dialog.selected_plugin_idx = 0;
    ctrl.export_dialog.selected_preset_idx = 0;
    let out_file = tmp.path().join("sel_out.json");
    ctrl.export_dialog.output_path = out_file.to_string_lossy().into_owned();
    ctrl.export_selection_only = true;

    let req: ExportJobRequest = ctrl.build_export_request()?;
    assert_eq!(req.start_frame, FrameIndex::new(30));
    assert_eq!(req.end_frame, FrameIndex::new(60));

    let mut sources = ExportMediaSources::new(None);
    let cancel = ExportCancelToken::new();
    let outcome = run_export_job(
        &ctrl.document,
        &registry,
        &req,
        &mut sources,
        &cancel,
        |_| {},
    )?;
    assert_eq!(outcome.frames_encoded, 30);
    assert!(out_file.exists());
    Ok(())
}

#[test]
fn ai_context_includes_selection_range_length_and_midpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, l1, _l2, c1) = seed_two_layer_controller()?;
    ctrl.set_timeline_length(TimelineLengthMode::Manual {
        end_frame: FrameCount::new(300)?,
    })?;
    ctrl.set_selection_range(Some(FrameRange {
        start: FrameIndex::new(30),
        end: FrameIndex::new(90),
    }))?;
    ctrl.add_midpoint(&c1, 45)?;

    let ctx = build_timeline_context(&ctrl.document, FrameIndex::new(45), Some(&c1), Some(&l1));
    assert_eq!(
        ctx.timeline_length,
        TimelineLengthSummary::Manual { end_frame: 300 }
    );
    assert_eq!(ctx.selection_range_frames, Some([30, 90]));
    assert_eq!(ctx.total_duration_frames, 300);
    assert_eq!(ctx.layers[0].clips[0].midpoints, vec![45]);

    let req = build_ai_request(
        &ctrl.document,
        FrameIndex::new(45),
        Some(&c1),
        Some(&l1),
        &[],
        "範囲と中間点を確認",
    )?;
    assert_eq!(
        req.context_summary["selection_range_frames"],
        serde_json::json!([30, 90])
    );
    assert_eq!(
        req.context_summary["timeline_length"],
        serde_json::json!({"mode": "manual", "end_frame": 300})
    );
    let prompt = default_editor_system_prompt();
    assert!(prompt.contains("SetTimelineLength"));
    assert!(prompt.contains("SetSelectionRange"));
    assert!(prompt.contains("AddMidpoint"));
    let _ = EvaluationContext::new(0, 0, FrameCount::new(1)?, FrameRate::new(30, 1)?);
    Ok(())
}
