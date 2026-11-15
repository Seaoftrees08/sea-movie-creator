use std::sync::Arc;

use sea_movie_creator::audio::mixer::AudioMixer;
use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::decoder::DecodedAudioPcm;
use sea_movie_creator::media::visual_properties::SubtitleProperties;
use sea_movie_creator::timeline::document::{
    AudioProperties, Clip, ClipKind, FrameRange, Layer, TimelineLengthMode,
};
use sea_movie_creator::timeline::snap::SnapConfig;
use sea_movie_creator::ui::app_controller::{AppController, TimeDisplayUnit};
use sea_movie_creator::ui::timeline_panel::{
    DragMode, DragSnapEnv, MarqueeSelectionState, MidpointDragState, PendingCtrlGesture,
    SelectionRangeDragState, TimelineDragState, TimelineEndDragState, collect_group_items,
    compute_ruler_ticks, format_timeline_timestamp, px_per_frame_from_zoom_ratio,
    sample_clip_waveform_bars, zoom_timeline_anchored,
};

fn sub_clip(
    id: &str,
    text: &str,
    start: u64,
    dur: u64,
) -> Result<Clip, Box<dyn std::error::Error>> {
    Ok(Clip {
        id: ClipId::new(id)?,
        name: text.to_owned(),
        timeline_start: FrameIndex::new(start),
        duration: FrameCount::new(dur)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Subtitle(SubtitleProperties::new(text)),
    })
}

#[test]
fn marquee_multi_drag_collision_locked_and_auto_layer_creation_work()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let l1 = LayerId::new("l1")?;
    let l2 = LayerId::new("l2")?;
    let mut layer1 = Layer::new(l1, "T1");
    let c1 = sub_clip("c1", "First", 20, 40)?;
    let c2 = sub_clip("c2", "Blocker", 120, 40)?;
    layer1.clips.push(c1.clone());
    layer1.clips.push(c2);

    let mut layer2 = Layer::new(l2, "T2");
    let c3 = sub_clip("c3", "Second", 20, 40)?;
    layer2.clips.push(c3.clone());
    controller.document.layers.push(layer1);
    controller.document.layers.push(layer2);
    controller.timeline_px_per_frame = 2.0;

    // マーキー矩形で c1 (20..60 on row 0) と c3 (20..60 on row 1) を一括選択
    let marquee = MarqueeSelectionState {
        start_local: (40.0, 5.0),
        current_local: (110.0, 50.0),
        start_window_y: 5.0,
        base_selection: Vec::new(),
        additive: false,
    };
    let hits = marquee.compute_selected_clips(&controller.document, 0, 2.0, 30.0);
    assert_eq!(hits.len(), 2);
    controller.set_selected_clips(hits.clone());

    // 複数選択グループ移動: +200px @ 2px/f = +100f => 120..160 で Blocker と衝突 -> preview_valid = false
    let group = collect_group_items(&controller.document, &hits);
    let mut drag = TimelineDragState::begin_with_options(
        &c1,
        0,
        100.0,
        15.0,
        DragMode::Move,
        30.0,
        false,
        group,
    );
    let nosnap_cfg = SnapConfig {
        enabled: false,
        ..SnapConfig::default()
    };
    let _ = drag.update_from_cursor_env(
        (300.0, 15.0),
        2.0,
        30.0,
        &DragSnapEnv {
            doc: &controller.document,
            playhead: FrameIndex::ZERO,
            snap_cfg: &nosnap_cfg,
            shift_bypass_snap: false,
        },
    );
    assert!(!drag.preview_valid);
    assert!(drag.commit(&mut controller).is_err());

    // さらに下 (+60px = +2 row) へドラッグ -> layer 2/3 が自動生成され原子移動成功
    let _ = drag.update_from_cursor_env(
        (140.0, 75.0), // +20f, +2 rows -> row 2 と row 3
        2.0,
        30.0,
        &DragSnapEnv {
            doc: &controller.document,
            playhead: FrameIndex::ZERO,
            snap_cfg: &nosnap_cfg,
            shift_bypass_snap: false,
        },
    );
    assert!(drag.preview_valid);
    drag.commit(&mut controller)
        .map_err(std::io::Error::other)?;
    assert_eq!(controller.document.layers.len(), 4);
    assert_eq!(
        controller
            .document
            .find_clip(&c1.id)
            .ok_or("c1")?
            .1
            .timeline_start
            .get(),
        40
    );
    assert_eq!(
        controller
            .document
            .find_clip(&c3.id)
            .ok_or("c3")?
            .1
            .timeline_start
            .get(),
        40
    );

    // ロックされたレイヤーへの移動も preview_valid = false で拒否
    assert_locked_layer_drag_rejected(&mut controller, &c1.id)?;

    // 単一 Undo でグループ移動と自動レイヤー追加を一括復元
    assert!(controller.undo()?);
    assert_eq!(controller.document.layers.len(), 2);
    Ok(())
}

fn assert_locked_layer_drag_rejected(
    controller: &mut AppController,
    clip_id: &ClipId,
) -> Result<(), Box<dyn std::error::Error>> {
    controller.document.layers[0].locked = true;
    let moved_c1 = controller
        .document
        .find_clip(clip_id)
        .ok_or("c1")?
        .1
        .clone();
    let mut locked_drag = TimelineDragState::begin_with_options(
        &moved_c1,
        2,
        100.0,
        75.0,
        DragMode::Move,
        30.0,
        false,
        Vec::new(),
    );
    let snap_cfg = SnapConfig::default();
    let _ = locked_drag.update_from_cursor_env(
        (100.0, 15.0),
        2.0,
        30.0,
        &DragSnapEnv {
            doc: &controller.document,
            playhead: FrameIndex::ZERO,
            snap_cfg: &snap_cfg,
            shift_bypass_snap: false,
        },
    );
    assert!(!locked_drag.preview_valid);
    assert!(locked_drag.commit(controller).is_err());
    Ok(())
}

#[test]
fn pending_ctrl_threshold_and_duplicate_move_work_end_to_end()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let l1 = LayerId::new("l1")?;
    let mut layer1 = Layer::new(l1, "T1");
    let c1 = sub_clip("orig", "Title", 10, 50)?;
    layer1.clips.push(c1.clone());
    controller.document.layers.push(layer1);

    let pending = PendingCtrlGesture {
        clip_id: c1.id.clone(),
        origin_layer_idx: 0,
        start_mouse_x: 50.0,
        start_mouse_y: 15.0,
        row_height_px: 30.0,
    };
    // 4px 未満の微小移動では複製ドラッグへ昇格せずトグルクリック扱い
    assert!(
        pending
            .try_promote_to_duplicate_drag(52.0, 16.0, &controller.document, &[])
            .is_none()
    );

    // 4px 以上で複製ドラッグへ昇格
    let mut dup_drag = pending
        .try_promote_to_duplicate_drag(55.0, 15.0, &controller.document, &[])
        .ok_or("expected duplicate promotion")?;
    assert!(dup_drag.duplicate_mode);
    let env_nosnap = DragSnapEnv {
        doc: &controller.document,
        playhead: FrameIndex::ZERO,
        snap_cfg: &SnapConfig {
            enabled: false,
            ..SnapConfig::default()
        },
        shift_bypass_snap: false,
    };
    let _ = dup_drag.update_from_cursor_env(
        (210.0, 15.0), // +160px @ 2px/f = +80f -> frame 90
        2.0,
        30.0,
        &env_nosnap,
    );
    assert!(dup_drag.preview_valid);
    dup_drag
        .commit(&mut controller)
        .map_err(std::io::Error::other)?;
    assert_eq!(controller.document.layers[0].clips.len(), 2);
    assert_eq!(
        controller.document.layers[0].clips[1].timeline_start.get(),
        90
    );
    assert!(controller.undo()?);
    assert_eq!(controller.document.layers[0].clips.len(), 1);
    Ok(())
}

#[test]
fn midpoint_drag_snap_shift_bypass_selection_range_and_timeline_end_work()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let l1 = LayerId::new("l1")?;
    let mut layer1 = Layer::new(l1, "T1");
    let mut c1 = sub_clip("c1", "Clip", 10, 90)?;
    c1.midpoints = vec![20, 60];
    layer1.clips.push(c1.clone());
    controller.document.layers.push(layer1);

    // Shift+ルーラードラッグによる選択範囲確定
    let sel_drag = SelectionRangeDragState {
        anchor_frame: 50,
        current_frame: 40,
    };
    assert_eq!(
        sel_drag.frame_range(),
        FrameRange {
            start: FrameIndex::new(40),
            end: FrameIndex::new(51),
        }
    );
    controller.set_selection_range(Some(sel_drag.frame_range()))?;

    // 中間点ドラッグ: start(10) + rel(20 -> raw 28 => abs 38) は 40 (selection_range.start) に吸着
    // Shift 押下中はスナップを無視して 28 のまま
    let mut md = MidpointDragState::begin(&c1, 20, 100.0).ok_or("midpoint init failed")?;
    let md_snap_cfg = SnapConfig::default();
    let md_env = DragSnapEnv {
        doc: &controller.document,
        playhead: FrameIndex::new(200),
        snap_cfg: &md_snap_cfg,
        shift_bypass_snap: false,
    };
    let guide_snap = md.update_from_cursor(
        116.0, // +16px @ 2px/f = +8f -> raw rel 28 (abs 38), dist to 40 = 2 <= 8
        2.0, &md_env,
    );
    assert_eq!(guide_snap, Some(40));
    assert_eq!(md.current_rel_frame, 30);

    let md_env_shift = DragSnapEnv {
        shift_bypass_snap: true,
        ..md_env
    };
    let guide_shift = md.update_from_cursor(116.0, 2.0, &md_env_shift);
    assert_eq!(guide_shift, None);
    assert_eq!(md.current_rel_frame, 28);
    md.commit(&mut controller).map_err(std::io::Error::other)?;
    assert_eq!(
        controller
            .document
            .find_clip(&c1.id)
            .ok_or("c1")?
            .1
            .midpoints,
        vec![28, 60]
    );

    // タイムライン終端ドラッグ -> Manual モード変更
    let mut end_drag = TimelineEndDragState {
        origin_end_frame: 300,
        current_end_frame: 300,
        start_mouse_x: 600.0,
    };
    let end_snap_cfg = SnapConfig::default();
    let end_env = DragSnapEnv {
        doc: &controller.document,
        playhead: FrameIndex::ZERO,
        snap_cfg: &end_snap_cfg,
        shift_bypass_snap: false,
    };
    let _ = end_drag.update_from_cursor(
        800.0, // +200px @ 2px/f = +100f -> 400
        2.0, &end_env,
    );
    end_drag
        .commit(&mut controller)
        .map_err(std::io::Error::other)?;
    assert_eq!(
        controller.document.timeline_length,
        TimelineLengthMode::Manual {
            end_frame: FrameCount::new(400)?,
        }
    );

    // アンカー付きズームとルーラー目盛りチェック
    controller.timeline_px_per_frame = px_per_frame_from_zoom_ratio(0.5);
    let old_ppf = controller.timeline_px_per_frame;
    controller.timeline_scroll_x_frames = 50;
    zoom_timeline_anchored(&mut controller, 1.5, 120.0);
    assert!((controller.timeline_px_per_frame - old_ppf * 1.5).abs() < 0.01);

    let label = format_timeline_timestamp(65, FrameRate::FPS_30, TimeDisplayUnit::Timecode);
    assert_eq!(label, "00:00:02:05");
    let ticks = compute_ruler_ticks(0, 600, 1.0, FrameRate::FPS_30, TimeDisplayUnit::Frames);
    assert!(ticks.iter().any(|t| t.is_major && t.label.is_some()));
    Ok(())
}

#[test]
fn audio_waveform_bars_align_to_source_in_and_playback_rate()
-> Result<(), Box<dyn std::error::Error>> {
    let asset_id = AssetId::new("bgm_ramp")?;
    // 1.0 秒分のランプ波形 (前半 0.1 / 後半 0.9) @ 48kHz モノラル
    let mut samples = Vec::with_capacity(48_000);
    for ix in 0..48_000_usize {
        let v = if ix < 24_000 { 0.1_f32 } else { 0.9_f32 };
        samples.push(v);
    }
    let pcm = Arc::new(DecodedAudioPcm {
        sample_rate: 48_000,
        channels: 1,
        interleaved: samples,
    });
    let mut mixer = AudioMixer::new();
    mixer.register_pcm(asset_id.clone(), pcm);

    let mut controller = AppController::new();
    controller.document.fps = FrameRate::FPS_60;

    let clip_start = Clip {
        id: ClipId::new("aud0")?,
        name: "BGM 0".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: FrameCount::new(30)?, // 0.5s
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Audio {
            asset_id: asset_id.clone(),
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties::default(),
        },
    };
    let clip_late = Clip {
        id: ClipId::new("aud1")?,
        name: "BGM 0.5s".to_owned(),
        timeline_start: FrameIndex::new(30),
        duration: FrameCount::new(30)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Audio {
            asset_id,
            source_in: RationalDuration::new(1, 2)?, // 0.5s 以降を参照
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties::default(),
        },
    };

    let bars_early = sample_clip_waveform_bars(&mixer, &controller.document, &clip_start, 16);
    let bars_late = sample_clip_waveform_bars(&mixer, &controller.document, &clip_late, 16);
    assert_eq!(bars_early.len(), 16);
    assert_eq!(bars_late.len(), 16);
    assert!(bars_early[2] < 0.2);
    assert!(bars_late[2] > 0.8);
    Ok(())
}
