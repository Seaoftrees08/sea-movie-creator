//! Phase B-3: 100 レイヤータイムライン、ドラッグ移動/トリム・スナップ、Copy/Paste/Duplicate、Undo の検証。

use sea_movie_creator::core::ids::{ClipId, LayerId};
use sea_movie_creator::core::time::{FrameCount, FrameIndex};
use sea_movie_creator::media::visual_properties::SubtitleProperties;
use sea_movie_creator::timeline::clipboard::{
    build_collision_free_paste_command, build_duplicate_selection_command, copy_clips,
};
use sea_movie_creator::timeline::document::{Clip, ClipKind, Layer, ProjectDocument};
use sea_movie_creator::timeline::snap::{SnapConfig, clip_intersects_window};
use sea_movie_creator::ui::app_controller::AppController;
use sea_movie_creator::ui::timeline_panel::{DragMode, TimelineDragState};

fn make_subtitle_clip(
    id: &str,
    name: &str,
    start: u64,
    dur: u64,
) -> Result<Clip, Box<dyn std::error::Error>> {
    Ok(Clip {
        id: ClipId::new(id)?,
        name: name.to_owned(),
        timeline_start: FrameIndex::new(start),
        duration: FrameCount::new(dur)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Subtitle(SubtitleProperties::new(name)),
    })
}

#[test]
fn hundred_layers_culling_drag_trim_snap_copy_paste_and_duplicate()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    controller.timeline_px_per_frame = 2.0;

    // 100 レイヤー＋横長タイムライン（可視窓カリング確認）を構築
    let mut doc = ProjectDocument::new("multi_layer_100");
    for ix in 0..100_u32 {
        let mut layer = Layer::new(LayerId::new(format!("layer_{ix}"))?, format!("T{}", ix + 1));
        if ix == 0 {
            layer.clips.push(make_subtitle_clip("c0", "Intro", 0, 60)?);
            layer
                .clips
                .push(make_subtitle_clip("c_far", "FarClip", 5000, 120)?);
        } else if ix == 1 {
            layer
                .clips
                .push(make_subtitle_clip("c1", "Overlay", 100, 50)?);
        }
        doc.layers.push(layer);
    }
    doc.validate()?;
    controller.document = doc;

    // 可視ウィンドウ [0..600) では FarClip (5000..5120) が描画対象から間引かれる
    assert!(clip_intersects_window(0, 60, 0, 600));
    assert!(!clip_intersects_window(5000, 120, 0, 600));

    // スナップ付きドラッグ移動: Overlay (100..150 on layer 1) を Intro.end (60) 付近 (dx=-36px => -18f => 82? Wait target=64 -> snap 60!) + row 2 へ
    let c1_id = ClipId::new("c1")?;
    let snap = controller
        .document
        .find_clip(&c1_id)
        .ok_or("c1 missing")?
        .1
        .clone();
    let mut drag = TimelineDragState::begin(&snap, 1, 200.0, 50.0, DragMode::Move);
    // dx = -72px @ 2px/f = -36f -> raw_start = 64f -> snap threshold 8f -> Intro.end = 60f に吸着!
    // dy = +44px = +1 row -> layer 2
    let guide = drag.update_from_cursor(
        (128.0, 94.0),
        controller.timeline_px_per_frame,
        &controller.document,
        FrameIndex::new(300),
        &SnapConfig::default(),
    );
    assert_eq!(guide, Some(60));
    assert_eq!(drag.current_start_frame, 60);
    assert_eq!(drag.current_layer_idx, 2);
    drag.commit(&mut controller)
        .map_err(std::io::Error::other)?;
    assert_eq!(controller.document.layers[2].clips.len(), 1);
    assert_eq!(
        controller.document.layers[2].clips[0].timeline_start.get(),
        60
    );

    // 左端トリム: +20px (+10f) -> start 70, duration 40, effect_offset 10
    let moved_snap = controller.document.layers[2].clips[0].clone();
    let mut trim_l = TimelineDragState::begin(&moved_snap, 2, 120.0, 100.0, DragMode::TrimLeft);
    let _ = trim_l.update_from_cursor(
        (140.0, 100.0),
        controller.timeline_px_per_frame,
        &controller.document,
        FrameIndex::ZERO,
        &SnapConfig {
            enabled: false,
            ..SnapConfig::default()
        },
    );
    assert_eq!(trim_l.current_start_frame, 70);
    assert_eq!(trim_l.current_duration_frames, 40);
    trim_l
        .commit(&mut controller)
        .map_err(std::io::Error::other)?;
    assert_eq!(
        controller.document.layers[2].clips[0].effect_offset_frames,
        10
    );

    // コピー＆貼り付け（衝突回避）＆ Duplicate & Undo
    let c0_id = ClipId::new("c0")?;
    let clipboard = copy_clips(&controller.document, std::slice::from_ref(&c0_id))?;
    let paste_cmd = build_collision_free_paste_command(
        &clipboard,
        &controller.document,
        FrameIndex::new(20),
        0,
    )?;
    controller
        .dispatch_command(&paste_cmd)
        .map_err(std::io::Error::other)?;
    assert_eq!(controller.document.layers[0].clips.len(), 3);
    assert_eq!(
        controller.document.layers[0].clips[1].timeline_start.get(),
        60
    );

    controller.selected_clip_id = Some(c1_id);
    controller
        .duplicate_selected_clip()
        .map_err(std::io::Error::other)?;
    assert_eq!(controller.document.layers[2].clips.len(), 2);

    // Build duplicate command spanning multiple layers directly
    let c_layer2 = controller.document.layers[2].clips[0].id.clone();
    let multi_dup = build_duplicate_selection_command(&controller.document, &[c0_id, c_layer2])?;
    controller
        .dispatch_command(&multi_dup)
        .map_err(std::io::Error::other)?;
    assert!(controller.document.layers[0].clips.len() >= 4);

    // Undo が原子単位で戻ること
    assert!(controller.undo().map_err(std::io::Error::other)?);
    assert_eq!(controller.document.layers[0].clips.len(), 3);
    Ok(())
}
