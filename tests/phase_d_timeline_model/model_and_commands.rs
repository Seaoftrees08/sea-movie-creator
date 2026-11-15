use sea_movie_creator::core::animation::{
    AnimatedParameter, EasingCurve, ExtrapolationMode, Keyframe,
};
use sea_movie_creator::core::ids::{ClipId, ClipRef, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex, FrameRate};
use sea_movie_creator::timeline::clipboard::{
    build_collision_free_paste_command, build_duplicate_selection_command,
};
use sea_movie_creator::timeline::command::{
    EditCommand, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::document::{FrameRange, TimelineLengthMode};
use sea_movie_creator::timeline::operations::{ClipMovePlan, build_batch_move_clips_command};
use sea_movie_creator::ui::app_controller::AppController;

#[test]
fn timeline_length_manual_auto_and_selection_range_undo_roundtrip()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    controller.document.fps = FrameRate::FPS_29_97;
    assert_eq!(controller.document.ruler_display_span_frames(), 300);

    // 無音・無クリップでも Auto モードなら最小 10s 分シーク可能
    controller.seek_to_frame(FrameIndex::new(250));
    assert_eq!(controller.clock.current_frame().get(), 250);

    // 手動 180f へ縮小するとプレイヘッドも 180f 以下にクランプ
    let manual = TimelineLengthMode::Manual {
        end_frame: FrameCount::new(180)?,
    };
    controller.set_timeline_length(manual)?;
    assert_eq!(controller.document.total_duration_frames(), 180);
    assert_eq!(controller.document.seek_upper_bound_frames(), 180);
    assert_eq!(controller.clock.current_frame().get(), 180);

    controller.seek_to_frame(FrameIndex::new(500));
    assert_eq!(controller.clock.current_frame().get(), 180);

    // Undo で自動モードへ戻る
    assert!(controller.undo()?);
    assert_eq!(
        controller.document.timeline_length,
        TimelineLengthMode::AutoToLastClip
    );

    // 同一フレームでの `[` と `]` は半開 [f, f+1) を作る
    controller.seek_to_frame(FrameIndex::new(42));
    controller.set_selection_range_start_at_playhead()?;
    controller.set_selection_range_end_at_playhead()?;
    assert_eq!(
        controller.document.selection_range,
        Some(FrameRange {
            start: FrameIndex::new(42),
            end: FrameIndex::new(43),
        })
    );

    // JSON 保存・読込ラウンドトリップで timeline_length / selection_range が維持される
    let json = serde_json::to_string_pretty(&controller.document)?;
    let decoded: sea_movie_creator::timeline::document::ProjectDocument =
        serde_json::from_str(&json)?;
    assert_eq!(decoded.selection_range, controller.document.selection_range);
    decoded.validate()?;
    Ok(())
}

#[test]
fn midpoints_cut_paste_and_multi_clip_selection_work_end_to_end()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let clip_id = controller.add_subtitle_clip_at(
        None,
        FrameIndex::new(10),
        FrameCount::new(100)?,
        "Hello World",
    )?;

    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(clip_id.clone()),
        },
        path: ParameterPath::Opacity,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Easing {
                start_value: 0.0,
                end_value: 1.0,
                curve: EasingCurve::EaseInOut,
                extrapolation: ExtrapolationMode::Clamp,
            },
        },
    })?;
    controller.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(clip_id.clone()),
        },
        path: ParameterPath::SubtitleFontSize,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Keyframes {
                keyframes: vec![Keyframe::new(0, 24.0), Keyframe::new(50, 48.0)],
                extrapolation: ExtrapolationMode::Clamp,
            },
        },
    })?;

    // SetParameter(Keyframes) が中間点 50 を自動同期し、全 Easing パラメータも [0, 50, 99] に揃う
    let (_, clip) = controller.document.find_clip(&clip_id).ok_or("clip")?;
    assert_eq!(clip.midpoints, vec![50]);
    if let sea_movie_creator::timeline::document::ClipKind::Subtitle(sub) = &clip.kind {
        assert!(matches!(
            &sub.visual.opacity,
            AnimatedParameter::Keyframes { keyframes, .. }
                if keyframes.iter().map(|kf| kf.frame).collect::<Vec<_>>() == vec![0, 50, 99]
        ));
        assert!(matches!(
            &sub.font_size,
            AnimatedParameter::Keyframes { keyframes, .. }
                if keyframes.iter().map(|kf| kf.frame).collect::<Vec<_>>() == vec![0, 50, 99]
        ));
    }

    // クリップコピー & 衝突回避ペーストで midpoints が継承される
    controller.copy_selected_clip()?;
    let paste_cmd = build_collision_free_paste_command(
        controller.clipboard.as_ref().ok_or("clipboard")?,
        &controller.document,
        FrameIndex::new(50),
        0,
    )?;
    controller.dispatch_command(&paste_cmd)?;
    assert_eq!(controller.document.layers[0].clips.len(), 2);
    let pasted = &controller.document.layers[0].clips[1];
    assert_eq!(pasted.midpoints, vec![50]);

    let first_id = clip_id;
    let pasted_id = pasted.id.clone();

    // 複製ヘルパーでも midpoints 維持
    let dup_cmd =
        build_duplicate_selection_command(&controller.document, std::slice::from_ref(&first_id))?;
    let report = controller.dispatch_command(&dup_cmd)?;
    assert_ne!(report, "");
    let dup_clip = controller.document.layers[0]
        .clips
        .iter()
        .find(|c| c.id != first_id && c.id != pasted_id)
        .ok_or("dup missing")?;
    assert_eq!(dup_clip.midpoints, vec![50]);
    Ok(())
}

#[test]
fn multi_clip_batch_move_selection_and_auto_layer_creation()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let c1 = controller.add_subtitle_clip_at(
        None,
        FrameIndex::new(10),
        FrameCount::new(40)?,
        "First",
    )?;
    let layer_id = controller.document.layers[0].id.clone();
    let c2 = controller.add_subtitle_clip_at(
        Some(&layer_id),
        FrameIndex::new(50),
        FrameCount::new(40)?,
        "Second",
    )?;
    let dup_ids = vec![c1, c2];
    controller.set_selected_clips(dup_ids.clone());
    assert_eq!(controller.effective_selected_clip_ids().len(), 2);

    let batch_shift = build_batch_move_clips_command(
        &controller.document,
        &[
            ClipMovePlan {
                clip_id: dup_ids[0].clone(),
                target_layer_id: layer_id.clone(),
                new_timeline_start: FrameIndex::new(30),
            },
            ClipMovePlan {
                clip_id: dup_ids[1].clone(),
                target_layer_id: layer_id.clone(),
                new_timeline_start: FrameIndex::new(140),
            },
        ],
    )?;
    controller.dispatch_command(&batch_shift)?;
    assert_eq!(
        controller.document.layers[0].clips[0].timeline_start.get(),
        30
    );
    assert_eq!(
        controller.document.layers[0].clips[1].timeline_start.get(),
        140
    );

    // Undo 1 回で一括移動が元に戻る
    assert!(controller.undo()?);
    assert_eq!(
        controller.document.layers[0].clips[0].timeline_start.get(),
        10
    );

    // 全選択・全解除・サブトラックへの別字幕自動追加
    controller.select_all_clips();
    assert_eq!(controller.effective_selected_clip_ids().len(), 2);
    controller.clear_clip_selection();
    assert_eq!(
        controller.effective_selected_clip_ids(),
        Vec::<ClipId>::new()
    );

    // 同じ位置・長さに追加しようとすると空いている別レイヤーを自動追加して配置
    let sub2 = controller.add_subtitle_clip_at(
        None,
        FrameIndex::new(10),
        FrameCount::new(30)?,
        "Second Layer",
    )?;
    assert_eq!(controller.document.layers.len(), 2);
    assert_eq!(
        controller.document.find_clip(&sub2).ok_or("s2")?.0.id,
        controller.document.layers[1].id
    );

    let _ = LayerRef::Id(layer_id);
    Ok(())
}
