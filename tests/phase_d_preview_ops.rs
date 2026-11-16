//! Phase D-4 プレビュー直接操作・マスター音声波形・ショートカット・素材再リンク結合テスト。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sea_movie_creator::core::animation::{AnimatedParameter, EasingCurve, ExtrapolationMode};
use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::decoder::DecodedAudioPcm;
use sea_movie_creator::media::visual_properties::{Matrix2D, VisualProperties};
use sea_movie_creator::timeline::command::{
    EditCommand, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind, FrameRange, Layer,
};
use sea_movie_creator::ui::app_controller::{AppController, PreviewZoomMode};
use sea_movie_creator::ui::key_config::{KeyConfig, ShortcutAction};
use sea_movie_creator::ui::preview::{
    DragModifiers, MASTER_WAVEFORM_MAX_CHUNKS, MasterWaveformCache, PreviewViewportTransform,
    build_preview_hit_quads, clip_visual_props, hit_test_selection_handle,
    sample_waveform_bars_for_timeline, step_zoom_preset, zoom_around_cursor,
};

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("sea_d4_{prefix}_{}_{unique}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup_two_image_controller(
    locked_front: bool,
) -> Result<(AppController, ClipId, ClipId), Box<dyn std::error::Error>> {
    let mut ctrl = AppController::new();
    ctrl.document.fps = FrameRate::FPS_30;
    ctrl.preview_viewport_bounds.set((0.0, 0.0, 1920.0, 1080.0));
    ctrl.preview_zoom = PreviewZoomMode::Percent(100);
    ctrl.show_preview_guides = true;

    let asset_id = AssetId::new("img-box")?;
    ctrl.document.assets.push(Asset {
        id: asset_id.clone(),
        name: "box.png".to_owned(),
        path: AssetPath::new(Some("box.png".to_owned()), None),
        kind: AssetKind::Image {
            width: 200,
            height: 100,
        },
        status: AssetStatus::Available,
    });

    let back_id = ClipId::new("clip-back")?;
    let front_id = ClipId::new("clip-front")?;
    let dur = FrameCount::new(90)?;

    let mut back_layer = Layer::new(LayerId::new("l-back")?, "Back");
    back_layer.clips.push(Clip {
        id: back_id.clone(),
        name: "Back".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: dur,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Image {
            asset_id: asset_id.clone(),
            visual: VisualProperties::default(),
        },
    });

    let mut front_layer = Layer::new(LayerId::new("l-front")?, "Front");
    front_layer.locked = locked_front;
    front_layer.clips.push(Clip {
        id: front_id.clone(),
        name: "Front".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: dur,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Image {
            asset_id,
            visual: VisualProperties::default(),
        },
    });

    ctrl.document.layers = vec![back_layer, front_layer];
    Ok((ctrl, back_id, front_id))
}

fn eval_constant(param: &AnimatedParameter) -> Option<f64> {
    match param {
        AnimatedParameter::Constant { value } => Some(*value),
        _ => None,
    }
}

#[test]
fn preview_canvas_hit_test_lock_passthrough_and_deselect() -> Result<(), Box<dyn std::error::Error>>
{
    let (mut ctrl, _back_id, front_id) = setup_two_image_controller(false)?;
    ctrl.handle_preview_left_mouse_down((960.0, 540.0), false);
    assert_eq!(ctrl.selected_clip_id.as_ref(), Some(&front_id));
    let _ = ctrl.finish_preview_drag();

    // 空白背景クリックで選択解除
    ctrl.handle_preview_left_mouse_down((40.0, 40.0), false);
    assert!(ctrl.selected_clip_id.is_none());

    // 前面レイヤーがロックされていると背面クリップを貫通選択する
    let (mut locked_ctrl, back_id, _front_id) = setup_two_image_controller(true)?;
    locked_ctrl.handle_preview_left_mouse_down((960.0, 540.0), false);
    assert_eq!(locked_ctrl.selected_clip_id.as_ref(), Some(&back_id));
    let _ = locked_ctrl.finish_preview_drag();
    Ok(())
}

#[test]
fn preview_direct_move_cancel_and_single_undo_redo_work() -> Result<(), Box<dyn std::error::Error>>
{
    let (mut ctrl, _back_id, front_id) = setup_two_image_controller(false)?;
    ctrl.select_single_clip(Some(front_id.clone()));

    // Shift で X/Y 軸ロック + Ctrl で 40px グリッドスナップ移動（アンカー中心を外した本体をクリック）
    ctrl.handle_preview_left_mouse_down((920.0, 540.0), false);
    ctrl.update_preview_drag(
        (920.0 + 95.0, 540.0 + 18.0),
        DragModifiers {
            shift: true,
            ctrl: true,
            alt: false,
        },
    );
    assert!(ctrl.finish_preview_drag()?);

    let (_, moved_clip) = ctrl
        .document
        .find_clip(&front_id)
        .ok_or("missing front clip")?;
    let vis = clip_visual_props(moved_clip).ok_or("missing visual")?;
    assert_eq!(eval_constant(&vis.x), Some(80.0));
    assert_eq!(eval_constant(&vis.y), Some(0.0));

    // Undo 1回で開始前 (0.0, 0.0) へ戻る
    assert!(ctrl.undo()?);
    let (_, undone_clip) = ctrl
        .document
        .find_clip(&front_id)
        .ok_or("missing front clip")?;
    let vis_undone = clip_visual_props(undone_clip).ok_or("missing visual")?;
    assert_eq!(eval_constant(&vis_undone.x), Some(0.0));

    assert!(ctrl.redo()?);
    let (_, redone_clip) = ctrl
        .document
        .find_clip(&front_id)
        .ok_or("missing front clip")?;
    let vis_redone = clip_visual_props(redone_clip).ok_or("missing visual")?;
    assert_eq!(eval_constant(&vis_redone.x), Some(80.0));

    // Escape キャンセルでは確定値が元に戻り Undo スタックも増えない
    let undo_len_before_cancel = ctrl.dispatcher.history().undo_len();
    ctrl.handle_preview_left_mouse_down((1000.0, 540.0), false);
    ctrl.update_preview_drag((1100.0, 620.0), DragModifiers::default());
    ctrl.cancel_preview_drag();
    let (_, canceled_clip) = ctrl
        .document
        .find_clip(&front_id)
        .ok_or("missing front clip")?;
    let vis_canceled = clip_visual_props(canceled_clip).ok_or("missing visual")?;
    assert_eq!(eval_constant(&vis_canceled.x), Some(80.0));
    assert_eq!(ctrl.dispatcher.history().undo_len(), undo_len_before_cancel);
    assert!(!ctrl.dispatcher.history().can_redo());
    Ok(())
}

#[test]
fn preview_scale_rotate_and_anchor_drag_preserve_geometry_and_undo()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, _back_id, front_id) = setup_two_image_controller(false)?;
    ctrl.select_single_clip(Some(front_id.clone()));

    // Alt + 右辺ハンドルドラッグ: アンカー中心基準で拡縮し X は変化しない
    let right_edge_pt = (1060.0_f32, 540.0_f32);
    ctrl.handle_preview_left_mouse_down(right_edge_pt, false);
    ctrl.update_preview_drag(
        (1110.0, 540.0),
        DragModifiers {
            alt: true,
            ..DragModifiers::default()
        },
    );
    assert!(ctrl.finish_preview_drag()?);
    let (_, scaled) = ctrl.document.find_clip(&front_id).ok_or("clip")?;
    let s_vis = clip_visual_props(scaled).ok_or("vis")?;
    assert!((eval_constant(&s_vis.scale_x).unwrap_or(0.0) - 1.5).abs() < 1e-6);
    assert!((eval_constant(&s_vis.x).unwrap_or(99.0)).abs() < 1e-6);

    // アンカーハンドルを (960, 540) から (+30, -20) へドラッグしても左上角のワールド位置は不変
    let vp = PreviewViewportTransform {
        bounds: (0.0, 0.0, 1920.0, 1080.0),
        proj_size: (1920, 1080),
        zoom: PreviewZoomMode::Percent(100),
        pan_px: (0.0, 0.0),
    };
    let quads = build_preview_hit_quads(&ctrl.document, FrameIndex::ZERO, &vp);
    let fq = quads.iter().find(|q| q.clip_id == front_id).ok_or("quad")?;
    assert!(hit_test_selection_handle(fq, 960.0, 540.0).is_some());
    let old_tl = fq.visual.matrix.transform_point(-100.0, -50.0);

    ctrl.handle_preview_left_mouse_down((960.0, 540.0), false);
    ctrl.update_preview_drag((990.0, 520.0), DragModifiers::default());
    assert!(ctrl.finish_preview_drag()?);

    let (_, anchored) = ctrl.document.find_clip(&front_id).ok_or("clip")?;
    let a_vis = clip_visual_props(anchored).ok_or("vis")?;
    let nx = eval_constant(&a_vis.x).ok_or("x")?;
    let ny = eval_constant(&a_vis.y).ok_or("y")?;
    let ax = eval_constant(&a_vis.anchor_x).ok_or("ax")?;
    let ay = eval_constant(&a_vis.anchor_y).ok_or("ay")?;
    let m_after = Matrix2D::from_transform((nx, ny), (1.5, 1.0), (ax, ay), 0.0);
    let new_tl = m_after.transform_point(-100.0, -50.0);
    assert!((old_tl.0 - new_tl.0).abs() < 1e-5);
    assert!((old_tl.1 - new_tl.1).abs() < 1e-5);

    // Undo 2回で初期状態に戻る
    assert!(ctrl.undo()?);
    assert!(ctrl.undo()?);
    let (_, initial) = ctrl.document.find_clip(&front_id).ok_or("clip")?;
    let i_vis = clip_visual_props(initial).ok_or("vis")?;
    assert_eq!(eval_constant(&i_vis.scale_x), Some(1.0));
    Ok(())
}

#[test]
fn preview_drag_shifts_easing_in_parallel_and_blocks_expression_with_notice()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut ctrl, _back_id, front_id) = setup_two_image_controller(false)?;
    let _ = ctrl.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: sea_movie_creator::core::ids::ClipRef::Id(front_id.clone()),
        },
        path: ParameterPath::X,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Easing {
                start_value: 10.0,
                end_value: 50.0,
                curve: EasingCurve::Linear,
                extrapolation: ExtrapolationMode::Clamp,
            },
        },
    })?;
    ctrl.select_single_clip(Some(front_id.clone()));

    // Easing 付きクリップの水平移動は start/end が平行移動する
    ctrl.handle_preview_left_mouse_down((970.0, 540.0), false);
    ctrl.update_preview_drag((1000.0, 560.0), DragModifiers::default());
    assert!(ctrl.finish_preview_drag()?);
    let (_, clip_after) = ctrl.document.find_clip(&front_id).ok_or("clip")?;
    let vis = clip_visual_props(clip_after).ok_or("vis")?;
    assert_eq!(
        vis.x,
        AnimatedParameter::Easing {
            start_value: 40.0,
            end_value: 80.0,
            curve: EasingCurve::Linear,
            extrapolation: ExtrapolationMode::Clamp,
        }
    );

    // Expression が設定されている場合は通知を出してブロックする
    let _ = ctrl.dispatch_command(&EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: sea_movie_creator::core::ids::ClipRef::Id(front_id.clone()),
        },
        path: ParameterPath::X,
        value: ParameterValue::Animated {
            param: AnimatedParameter::Expression {
                expr: "sin(t)".to_owned(),
                base_offset: 0.0,
                scale: 1.0,
            },
        },
    })?;
    ctrl.handle_preview_left_mouse_down((960.0, 560.0), false);
    ctrl.update_preview_drag((1010.0, 560.0), DragModifiers::default());
    assert!(
        ctrl.preview_notice
            .as_deref()
            .is_some_and(|m| m.contains("数式"))
    );
    ctrl.cancel_preview_drag();
    Ok(())
}

#[test]
fn master_waveform_revision_invalidation_lru_and_wheel_zoom_math()
-> Result<(), Box<dyn std::error::Error>> {
    let mut ctrl = AppController::new();
    ctrl.document.fps = FrameRate::FPS_30;
    let aid = AssetId::new("audio-1")?;
    ctrl.document.assets.push(Asset {
        id: aid.clone(),
        name: "beep.wav".to_owned(),
        path: AssetPath::new(Some("beep.wav".to_owned()), None),
        kind: AssetKind::Audio {
            sample_rate_hz: 8_000,
            channels: 1,
            source_duration: RationalDuration::from_millis(2_000),
        },
        status: AssetStatus::Available,
    });
    let lid = LayerId::new("la")?;
    let cid = ClipId::new("ca")?;
    let mut layer = Layer::new(lid.clone(), "Audio");
    layer.clips.push(Clip {
        id: cid,
        name: "Tone".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: FrameCount::new(60)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Audio {
            asset_id: aid.clone(),
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties::default(),
        },
    });
    ctrl.document.layers.push(layer);
    ctrl.mixer.register_pcm(
        aid,
        Arc::new(DecodedAudioPcm {
            sample_rate: 8_000,
            channels: 1,
            interleaved: vec![0.5; 16_000],
        }),
    );

    let rev0 = ctrl.doc_audio_revision;
    let bars_active = sample_waveform_bars_for_timeline(&mut ctrl.master_waveform, 60, 30.0, 40);
    assert_eq!(bars_active.len(), 40);

    // コマンドによるレイヤーミュートで doc_audio_revision が進みキャッシュが再評価される
    let _ = ctrl.dispatch_command(&EditCommand::SetLayerFlags {
        layer_id: sea_movie_creator::core::ids::LayerRef::Id(lid),
        name: None,
        visible: None,
        muted: Some(true),
        locked: None,
    })?;
    assert!(ctrl.doc_audio_revision > rev0);
    let mut wf = MasterWaveformCache::new();
    let muted_chunk = wf.ensure_chunk_sync(ctrl.doc_audio_revision, 0, &ctrl.document, &ctrl.mixer);
    assert!(muted_chunk.peaks[10] < 1e-4);
    for idx in 0..20_u64 {
        let _ = wf.ensure_chunk_sync(ctrl.doc_audio_revision, idx, &ctrl.document, &ctrl.mixer);
    }
    assert_eq!(wf.len(), MASTER_WAVEFORM_MAX_CHUNKS);

    // ホイール倍率プリセットとカーソル直下ズームオフセット
    assert_eq!(
        step_zoom_preset(PreviewZoomMode::Percent(100), 1.0, 1),
        PreviewZoomMode::Percent(150)
    );
    let next_pan = zoom_around_cursor(
        (0.0, 0.0, 1000.0, 500.0),
        (1920, 1080),
        PreviewZoomMode::Percent(100),
        (0.0, 0.0),
        PreviewZoomMode::Percent(200),
        (700.0, 250.0),
    );
    assert!((next_pan.0 + 200.0).abs() < 1e-3);
    Ok(())
}

#[test]
fn key_config_and_aviutl_shortcut_controller_ops_work() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDirGuard::create("keycfg")?;
    let cfg_path = dir.path().join("key_config.json");
    let mut cfg = KeyConfig::with_defaults();
    let dup = cfg.set_binding(ShortcutAction::SplitClip, "Ctrl+Space");
    assert_eq!(dup, vec![ShortcutAction::StopPlayback]);
    cfg.save_to_path(&cfg_path)?;
    let reloaded = KeyConfig::load_from_path(&cfg_path)?;
    assert_eq!(
        reloaded.binding_for(ShortcutAction::SplitClip),
        "ctrl-space"
    );

    let (mut ctrl, back_id, front_id) = setup_two_image_controller(false)?;
    ctrl.select_single_clip(Some(front_id.clone()));

    // 微調整 Nudge / Rotate と Undo Parity
    ctrl.nudge_selected_clips_xy(1.0, -1.0)?;
    ctrl.rotate_selected_clips_by(1.0)?;
    let (_, after_nudge) = ctrl.document.find_clip(&front_id).ok_or("clip")?;
    let n_vis = clip_visual_props(after_nudge).ok_or("vis")?;
    assert_eq!(eval_constant(&n_vis.x), Some(1.0));
    assert_eq!(eval_constant(&n_vis.y), Some(-1.0));
    assert_eq!(eval_constant(&n_vis.rotation_deg), Some(1.0));
    assert!(ctrl.undo()?);
    assert!(ctrl.undo()?);

    // 選択範囲開始/終了と 全選択 / カット / Undo
    ctrl.seek_to_frame(FrameIndex::new(15));
    ctrl.set_selection_range_start_at_playhead()?;
    ctrl.seek_to_frame(FrameIndex::new(60));
    ctrl.set_selection_range_end_at_playhead()?;
    assert_eq!(
        ctrl.document.selection_range,
        Some(FrameRange {
            start: FrameIndex::new(15),
            end: FrameIndex::new(61),
        })
    );

    ctrl.step_half_second(false);
    assert_eq!(ctrl.clock.current_frame().get(), 45);
    ctrl.seek_to_end();
    assert_eq!(
        ctrl.clock.current_frame().get(),
        ctrl.document.total_duration_frames()
    );

    // 重なりクリップ巡回とカット -> Undo
    ctrl.seek_to_frame(FrameIndex::new(10));
    assert!(ctrl.cycle_clip_at_playhead());
    assert_eq!(ctrl.selected_clip_id.as_ref(), Some(&back_id));

    ctrl.select_all_clips();
    assert_eq!(ctrl.effective_selected_clip_ids().len(), 2);
    ctrl.cut_selected_clips()?;
    assert!(ctrl.clipboard.is_some());
    assert!(ctrl.document.find_clip(&front_id).is_none());
    assert!(ctrl.undo()?);
    assert!(ctrl.document.find_clip(&front_id).is_some());

    // T キー相当の再生位置字幕追加と Undo
    let sub_id = ctrl.add_subtitle_at_playhead()?;
    assert!(ctrl.document.find_clip(&sub_id).is_some());
    assert!(ctrl.undo()?);
    assert!(ctrl.document.find_clip(&sub_id).is_none());
    Ok(())
}

#[test]
fn relink_missing_asset_updates_status_and_undoes() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDirGuard::create("relink")?;
    let png_path = dir.path().join("replacement.png");
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 200, 120, 255]));
    img.save(&png_path)?;

    let mut ctrl = AppController::new();
    let aid = AssetId::new("missing-1")?;
    ctrl.document.assets.push(Asset {
        id: aid.clone(),
        name: "missing.png".to_owned(),
        path: AssetPath::new(Some("missing.png".to_owned()), None),
        kind: AssetKind::Image {
            width: 8,
            height: 8,
        },
        status: AssetStatus::Missing {
            expected_path: "missing.png".to_owned(),
        },
    });
    let mut layer = Layer::new(LayerId::new("l0")?, "V1");
    layer.clips.push(Clip {
        id: ClipId::new("c0")?,
        name: "Missing".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: FrameCount::new(30)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Image {
            asset_id: aid.clone(),
            visual: VisualProperties::default(),
        },
    });
    ctrl.document.layers.push(layer);

    ctrl.relink_asset(&aid, &png_path)?;
    assert_eq!(
        ctrl.document.find_asset(&aid).map(|a| &a.status),
        Some(&AssetStatus::Available)
    );
    assert!(ctrl.undo()?);
    assert!(matches!(
        ctrl.document.find_asset(&aid).map(|a| &a.status),
        Some(AssetStatus::Missing { .. })
    ));
    Ok(())
}
