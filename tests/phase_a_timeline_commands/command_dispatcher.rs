use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::color::RgbaColor;
use sea_movie_creator::core::ids::{AssetRef, ClipRef, LayerRef};
use sea_movie_creator::core::schema::edit_command_schema_json;
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::visual_properties::{SubtitleProperties, VisualProperties};
use sea_movie_creator::timeline::command::{
    CommandError, EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget,
    ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::{CommandDispatcher, DispatchReport};
use sea_movie_creator::timeline::document::{
    AssetKind, AssetPath, AssetStatus, AudioProperties, ClipKind, DocumentValidationError,
    ProjectDocument,
};
use sea_movie_creator::timeline::operations::{build_paste_command, copy_clips};

fn sample_video_kind(secs: u64) -> Result<AssetKind, Box<dyn std::error::Error>> {
    Ok(AssetKind::Video {
        has_audio: true,
        default_fps: Some(FrameRate::FPS_29_97),
        duration_frames: Some(FrameCount::new(secs * 30)?),
        width: Some(1920),
        height: Some(1080),
        source_duration: RationalDuration::new(secs, 1)?,
    })
}

fn setup_split_batch() -> Result<EditCommand, Box<dyn std::error::Error>> {
    Ok(EditCommand::Batch {
        commands: vec![
            EditCommand::AddAsset {
                temp_id: Some("v_tmp".to_owned()),
                id: None,
                name: "clip.mkv".to_owned(),
                path: AssetPath::from_absolute("C:/media/clip.mkv"),
                kind: sample_video_kind(20)?,
            },
            EditCommand::AddLayer {
                temp_id: Some("l_tmp".to_owned()),
                id: None,
                name: "Main Video".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: Some("c_tmp".to_owned()),
                id: None,
                layer_id: LayerRef::Temp("l_tmp".to_owned()),
                clip: Box::new(NewClipSpec {
                    name: "Scene 1".to_owned(),
                    timeline_start: FrameIndex::new(0),
                    duration: FrameCount::new(120)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Video {
                        asset_id: AssetRef::Temp("v_tmp".to_owned()),
                        source_in: RationalDuration::new(1, 2)?,
                        playback_rate: PlaybackRate::new(3, 2)?,
                        preserve_pitch: true,
                        visual: VisualProperties::default(),
                        audio: Some(AudioProperties::default()),
                    },
                }),
            },
            EditCommand::SplitClip {
                clip_id: ClipRef::Temp("c_tmp".to_owned()),
                split_at_timeline_frame: FrameIndex::new(50),
                right_temp_id: Some("c_right".to_owned()),
                right_id: None,
            },
            EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Temp("c_right".to_owned()),
                },
                path: ParameterPath::Opacity,
                value: ParameterValue::Animated {
                    param: AnimatedParameter::constant(0.5),
                },
            },
        ],
    })
}

fn setup_video_and_subtitle_project(
    doc: &mut ProjectDocument,
    dispatcher: &mut CommandDispatcher,
) -> Result<DispatchReport, Box<dyn std::error::Error>> {
    Ok(dispatcher.dispatch(
        doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddAsset {
                    temp_id: Some("a_vid".to_owned()),
                    id: None,
                    name: "short.mkv".to_owned(),
                    path: AssetPath::from_absolute("C:/old/short.mkv"),
                    kind: sample_video_kind(2)?,
                },
                EditCommand::AddLayer {
                    temp_id: Some("l_vid".to_owned()),
                    id: None,
                    name: "Video".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddLayer {
                    temp_id: Some("l_txt".to_owned()),
                    id: None,
                    name: "Subtitle".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c_vid".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l_vid".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Short".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration: FrameCount::new(45)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        midpoints: Vec::new(),
                        kind: NewClipKind::Video {
                            asset_id: AssetRef::Temp("a_vid".to_owned()),
                            source_in: RationalDuration::ZERO,
                            playback_rate: PlaybackRate::NORMAL,
                            preserve_pitch: true,
                            visual: VisualProperties::default(),
                            audio: Some(AudioProperties::default()),
                        },
                    }),
                },
                EditCommand::AddClip {
                    temp_id: Some("c_sub".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l_txt".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Sub".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration: FrameCount::new(30)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        midpoints: Vec::new(),
                        kind: NewClipKind::Subtitle(SubtitleProperties::new("Before")),
                    }),
                },
            ],
        },
    )?)
}

#[test]
fn batch_with_temp_ids_split_trim_and_undo_redo_roundtrip() -> Result<(), Box<dyn std::error::Error>>
{
    let mut doc = ProjectDocument::new("PhaseA-2 Commands");
    doc.fps = FrameRate::FPS_29_97;
    let mut dispatcher = CommandDispatcher::new();
    let setup_batch = setup_split_batch()?;

    let dry = dispatcher.dry_run_validate(&doc, &setup_batch)?;
    assert!(dry.created_assets.contains_key("v_tmp"));
    assert_eq!(doc.assets.len(), 0);
    assert!(!dispatcher.history().can_undo());

    let report = dispatcher.dispatch(&mut doc, &setup_batch)?;
    let left_id = &report.created_clips["c_tmp"];
    let right_id = &report.created_clips["c_right"];

    let (_, left_clip) = doc.find_clip(left_id).ok_or("left missing")?;
    let (_, right_clip) = doc.find_clip(right_id).ok_or("right missing")?;
    assert_eq!(left_clip.timeline_start, FrameIndex::new(0));
    assert_eq!(left_clip.duration, FrameCount::new(50)?);
    assert_eq!(left_clip.effect_offset_frames, 0);
    assert_eq!(right_clip.timeline_start, FrameIndex::new(50));
    assert_eq!(right_clip.duration, FrameCount::new(70)?);
    assert_eq!(right_clip.effect_offset_frames, 50);

    let ClipKind::Video {
        source_in, visual, ..
    } = &right_clip.kind
    else {
        return Err("expected video clip".into());
    };
    assert_eq!(visual.opacity, AnimatedParameter::constant(0.5));
    // 50 * 1001 * 3 / (30000 * 2) + 1/2 = 1001/400 + 200/400 = 1201/400 sec
    assert_eq!(*source_in, RationalDuration::new(1201, 400)?);

    let snapshot = doc.clone();
    dispatcher.dispatch(
        &mut doc,
        &EditCommand::TrimLeft {
            clip_id: ClipRef::Id(right_id.clone()),
            delta_frames: 10,
        },
    )?;
    let (_, trimmed) = doc.find_clip(right_id).ok_or("trimmed missing")?;
    assert_eq!(trimmed.timeline_start, FrameIndex::new(60));
    assert_eq!(trimmed.duration, FrameCount::new(60)?);
    assert_eq!(trimmed.effect_offset_frames, 60);

    dispatcher.undo(&mut doc)?;
    assert_eq!(doc, snapshot);
    dispatcher.redo(&mut doc)?;
    assert_eq!(
        doc.find_clip(right_id).ok_or("redone missing")?.1.duration,
        FrameCount::new(60)?
    );

    dispatcher.undo(&mut doc)?;
    dispatcher.undo(&mut doc)?;
    assert_eq!(doc.assets.len(), 0);
    assert_eq!(doc.layers.len(), 0);

    dispatcher.redo(&mut doc)?;
    assert_eq!(doc, snapshot);
    Ok(())
}

#[test]
fn failed_batch_rolls_back_atomically_without_polluting_history()
-> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Atomic Rollback");
    let mut dispatcher = CommandDispatcher::new();
    let l1 = sea_movie_creator::core::ids::LayerId::new("l1")?;

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::AddLayer {
            temp_id: None,
            id: Some(l1.clone()),
            name: "L1".to_owned(),
            insert_index: None,
        },
    )?;
    let baseline = doc.clone();
    let baseline_undo_len = dispatcher.history().undo_len();

    let failing_batch = EditCommand::Batch {
        commands: vec![
            EditCommand::AddClip {
                temp_id: Some("c1".to_owned()),
                id: None,
                layer_id: LayerRef::Id(l1.clone()),
                clip: Box::new(NewClipSpec {
                    name: "First".to_owned(),
                    timeline_start: FrameIndex::new(0),
                    duration: FrameCount::new(60)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("OK")),
                }),
            },
            EditCommand::AddClip {
                temp_id: None,
                id: None,
                layer_id: LayerRef::Id(l1),
                clip: Box::new(NewClipSpec {
                    name: "Overlap".to_owned(),
                    timeline_start: FrameIndex::new(30),
                    duration: FrameCount::new(60)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("NG")),
                }),
            },
        ],
    };

    let err = dispatcher.dispatch(&mut doc, &failing_batch);
    assert!(matches!(
        err,
        Err(CommandError::Validation(
            DocumentValidationError::OverlappingClips { .. }
        ))
    ));
    assert_eq!(doc, baseline);
    assert_eq!(dispatcher.history().undo_len(), baseline_undo_len);
    Ok(())
}

#[test]
fn playback_rate_validation_and_subtitle_parameter_updates()
-> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Params Check");
    doc.fps = FrameRate::FPS_30;
    let mut dispatcher = CommandDispatcher::new();
    let init = setup_video_and_subtitle_project(&mut doc, &mut dispatcher)?;

    let vid_clip_id = init.created_clips["c_vid"].clone();
    let sub_clip_id = init.created_clips["c_sub"].clone();

    let too_fast = dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetPlaybackRate {
            clip_id: ClipRef::Id(vid_clip_id.clone()),
            new_rate: PlaybackRate::new(2, 1)?,
            preserve_pitch: Some(false),
        },
    );
    assert!(matches!(
        too_fast,
        Err(CommandError::Validation(
            DocumentValidationError::SourceRangeExceedsAsset { .. }
        ))
    ));

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetPlaybackRate {
            clip_id: ClipRef::Id(vid_clip_id),
            new_rate: PlaybackRate::new(5, 4)?,
            preserve_pitch: Some(false),
        },
    )?;

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(sub_clip_id.clone()),
            },
            path: ParameterPath::SubtitleText,
            value: ParameterValue::Text {
                value: "After".to_owned(),
            },
        },
    )?;
    dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(sub_clip_id.clone()),
            },
            path: ParameterPath::SubtitleTextColor,
            value: ParameterValue::Color {
                value: RgbaColor::new(255, 220, 64, 255),
            },
        },
    )?;

    let ClipKind::Subtitle(sub) = &doc.find_clip(&sub_clip_id).ok_or("sub missing")?.1.kind else {
        return Err("expected subtitle".into());
    };
    assert_eq!(sub.text, "After");
    assert_eq!(sub.text_color, RgbaColor::new(255, 220, 64, 255));
    Ok(())
}

#[test]
fn copy_paste_duplicate_locked_layer_and_relink_asset() -> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Clipboard Lock Relink");
    doc.fps = FrameRate::FPS_30;
    let mut dispatcher = CommandDispatcher::new();
    let init = setup_video_and_subtitle_project(&mut doc, &mut dispatcher)?;

    let vid_asset_id = init.created_assets["a_vid"].clone();
    let txt_layer_id = init.created_layers["l_txt"].clone();
    let vid_clip_id = init.created_clips["c_vid"].clone();
    let sub_clip_id = init.created_clips["c_sub"].clone();

    let clipboard = copy_clips(&doc, std::slice::from_ref(&sub_clip_id))?;
    let paste_cmd = build_paste_command(&clipboard, &doc, FrameIndex::new(100), 1)?;
    dispatcher.dispatch(&mut doc, &paste_cmd)?;
    assert_eq!(doc.layers[1].clips.len(), 2);

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetLayerFlags {
            layer_id: LayerRef::Id(txt_layer_id),
            name: None,
            visible: None,
            muted: None,
            locked: Some(true),
        },
    )?;
    let locked_move = dispatcher.dispatch(
        &mut doc,
        &EditCommand::MoveClip {
            clip_id: ClipRef::Id(sub_clip_id),
            target_layer_id: None,
            new_timeline_start: FrameIndex::new(10),
        },
    );
    assert!(matches!(locked_move, Err(CommandError::LayerLocked(_))));

    let remove_in_use = dispatcher.dispatch(
        &mut doc,
        &EditCommand::RemoveAsset {
            asset_id: vid_asset_id.clone(),
        },
    );
    assert!(matches!(remove_in_use, Err(CommandError::AssetInUse(_))));

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::RelinkAsset {
            asset_id: vid_asset_id.clone(),
            new_path: AssetPath::new(
                Some("media/relinked.mkv".to_owned()),
                Some("D:/new/relinked.mkv".to_owned()),
            ),
            new_kind: None,
        },
    )?;
    let asset = doc.find_asset(&vid_asset_id).ok_or("asset missing")?;
    assert_eq!(asset.status, AssetStatus::Available);

    dispatcher.dispatch(
        &mut doc,
        &EditCommand::DuplicateClip {
            clip_id: ClipRef::Id(vid_clip_id),
            target_layer_id: None,
            new_timeline_start: FrameIndex::new(200),
            new_temp_id: None,
            new_id: None,
        },
    )?;
    assert_eq!(doc.layers[0].clips.len(), 2);

    let schema = edit_command_schema_json()?;
    assert!(schema.contains("set_playback_rate"));
    Ok(())
}
