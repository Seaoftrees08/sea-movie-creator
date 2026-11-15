use sea_movie_creator::core::animation::{
    AnimatedParameter, EasingCurve, ExtrapolationMode, Keyframe,
};
use sea_movie_creator::core::ids::{ClipId, ClipRef, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex, FrameRate};
use sea_movie_creator::media::visual_properties::{
    EvaluatedVisual, SubtitleProperties, VisualProperties,
};
use sea_movie_creator::timeline::command::{
    CommandError, EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget,
    ParameterValue,
};
use sea_movie_creator::timeline::dispatcher::CommandDispatcher;
use sea_movie_creator::timeline::document::{ClipKind, ProjectDocument};

fn clip_visual(clip: &sea_movie_creator::timeline::document::Clip) -> Option<&VisualProperties> {
    match &clip.kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Some(visual),
        ClipKind::Subtitle(sub) => Some(&sub.visual),
        ClipKind::Audio { .. } => None,
    }
}

fn eval_clip_at(
    doc: &ProjectDocument,
    clip_id: &ClipId,
    timeline_frame: u64,
) -> Result<EvaluatedVisual, Box<dyn std::error::Error>> {
    let (_, clip) = doc.find_clip(clip_id).ok_or("clip missing")?;
    let ctx = clip
        .evaluation_context(FrameIndex::new(timeline_frame), doc.fps)
        .ok_or("frame outside clip")?;
    let visual = clip_visual(clip).ok_or("clip not visual")?;
    Ok(visual.evaluate(ctx)?)
}

fn assert_visual_close(a: EvaluatedVisual, b: EvaluatedVisual) {
    assert!((a.x - b.x).abs() < 1e-9);
    assert!((a.y - b.y).abs() < 1e-9);
    assert!((a.scale_x - b.scale_x).abs() < 1e-9);
    assert!((a.rotation_deg - b.rotation_deg).abs() < 1e-9);
    assert!((a.opacity - b.opacity).abs() < 1e-9);
    for (ca, cb) in a.matrix.coeffs.iter().zip(b.matrix.coeffs.iter()) {
        assert!((ca - cb).abs() < 1e-9);
    }
}

#[test]
fn easing_keyframes_and_expression_remain_continuous_across_split_and_trim_left()
-> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Animation Cut Consistency");
    doc.fps = FrameRate::FPS_60;
    let mut dispatcher = CommandDispatcher::new();

    let visual = VisualProperties {
        x: AnimatedParameter::Easing {
            start_value: -200.0,
            end_value: 200.0,
            curve: EasingCurve::EaseInOut,
            extrapolation: ExtrapolationMode::Clamp,
        },
        y: AnimatedParameter::Keyframes {
            keyframes: vec![
                Keyframe::new(0, 10.0),
                Keyframe::with_easing(
                    60,
                    90.0,
                    EasingCurve::CubicBezier {
                        x1: 0.42,
                        y1: 0.0,
                        x2: 0.58,
                        y2: 1.0,
                    },
                ),
                Keyframe::new(119, -30.0),
            ],
            extrapolation: ExtrapolationMode::Clamp,
        },
        rotation_deg: AnimatedParameter::expression("sin(pi * p) * 90 + t * 15"),
        scale_x: AnimatedParameter::constant(1.5),
        ..VisualProperties::default()
    };

    let mut sub = SubtitleProperties::new("Animated");
    sub.visual = visual;

    let init = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddLayer {
                    temp_id: Some("l1".to_owned()),
                    id: None,
                    name: "Sub Layer".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c1".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l1".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Anim".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration: FrameCount::new(120)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: NewClipKind::Subtitle(sub),
                    }),
                },
            ],
        },
    )?;

    let left_id = init.created_clips["c1"].clone();

    // カット前ベースラインをフレームごとに採取
    let mut baseline = Vec::with_capacity(120);
    for frame in 0..120_u64 {
        baseline.push(eval_clip_at(&doc, &left_id, frame)?);
    }

    // frame 40 で分割し、右クリップの頭をさらに 15 フレーム左トリム（55..120 が残る）
    let split = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::SplitClip {
                    clip_id: ClipRef::Id(left_id.clone()),
                    split_at_timeline_frame: FrameIndex::new(40),
                    right_temp_id: Some("right".to_owned()),
                    right_id: None,
                },
                EditCommand::TrimLeft {
                    clip_id: ClipRef::Temp("right".to_owned()),
                    delta_frames: 15,
                },
            ],
        },
    )?;
    let right_id = split.created_clips["right"].clone();

    for frame in 0..40_u64 {
        let actual = eval_clip_at(&doc, &left_id, frame)?;
        assert_visual_close(actual, baseline[usize::try_from(frame)?]);
    }
    for frame in 55..120_u64 {
        let actual = eval_clip_at(&doc, &right_id, frame)?;
        assert_visual_close(actual, baseline[usize::try_from(frame)?]);
    }
    Ok(())
}

#[test]
fn invalid_expression_is_rejected_at_dispatch_time() -> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Bad Expr");
    let mut dispatcher = CommandDispatcher::new();
    let init = dispatcher.dispatch(
        &mut doc,
        &EditCommand::Batch {
            commands: vec![
                EditCommand::AddLayer {
                    temp_id: Some("l1".to_owned()),
                    id: None,
                    name: "Sub".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("c1".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("l1".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Clip".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration: FrameCount::new(60)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: NewClipKind::Subtitle(SubtitleProperties::new("Hi")),
                    }),
                },
            ],
        },
    )?;

    let err = dispatcher.dispatch(
        &mut doc,
        &EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(init.created_clips["c1"].clone()),
            },
            path: ParameterPath::X,
            value: ParameterValue::Animated {
                param: AnimatedParameter::expression("unknown_var + 1"),
            },
        },
    );
    assert!(matches!(err, Err(CommandError::Incompatible(_))));
    Ok(())
}
