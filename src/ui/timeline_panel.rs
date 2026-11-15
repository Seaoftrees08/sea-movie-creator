//! タイムラインパネル公開ファサード（`crate::ui::timeline` モジュール群の再エクスポート）。

pub use crate::ui::timeline::ruler::{RulerTick, compute_ruler_ticks, format_timeline_timestamp};
pub use crate::ui::timeline::state::{
    CTRL_DRAG_THRESHOLD_PX, DragMode, DragSnapEnv, DraggedClipItem, H_SCROLLBAR_HEIGHT_PX,
    MIDPOINT_HIT_RADIUS_PX, MarqueeSelectionState, MidpointDragState, PendingCtrlGesture,
    RULER_HEIGHT_PX, SelectionRangeDragState, TIMELINE_END_HIT_PX, TIMELINE_HEADER_HEIGHT_PX,
    TRACK_HEADER_WIDTH, TRIM_EDGE_HIT_PX, TimelineDragState, TimelineEndDragState,
    TimelinePointerGesture, canvas_width_for_duration, canvas_x_from_frame, collect_group_items,
    frame_at_canvas_local_x, frame_at_timeline_window_x, parse_timeline_length_input,
    px_per_frame_from_zoom_ratio, row_height_px, set_timeline_zoom_ratio_anchored,
    zoom_ratio_from_px_per_frame, zoom_timeline_anchored,
};
pub use crate::ui::timeline::waveform_view::sample_clip_waveform_bars;
pub use crate::ui::timeline::{
    handle_timeline_file_drop, handle_timeline_scroll_wheel, handle_timeline_window_mouse_move,
    handle_timeline_window_mouse_up, render_timeline_length_modal_overlay, render_timeline_panel,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::{ClipId, LayerId};
    use crate::core::time::{FrameCount, FrameIndex, FrameRate};
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::{Clip, ClipKind, Layer};
    use crate::timeline::snap::SnapConfig;
    use crate::ui::app_controller::{AppController, TimeDisplayUnit, TimelineRowDensity};

    #[test]
    fn timeline_drag_move_and_trims_commit_accurately() -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = AppController::new();
        let l1 = LayerId::new("v1")?;
        let l2 = LayerId::new("v2")?;
        let c1 = ClipId::new("clip_drag")?;
        let mut layer1 = Layer::new(l1, "Track 1");
        layer1.clips.push(Clip {
            id: c1.clone(),
            name: "Sub".to_owned(),
            timeline_start: FrameIndex::new(30),
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("Drag Test")),
        });
        let layer2 = Layer::new(l2, "Track 2");
        controller.document.layers.push(layer1);
        controller.document.layers.push(layer2);
        controller.timeline_px_per_frame = 2.0;

        let snap = controller
            .document
            .find_clip(&c1)
            .ok_or("missing clip")?
            .1
            .clone();
        let mut drag = TimelineDragState::begin(&snap, 0, 100.0, 20.0, DragMode::Move);
        // dx = +80px @ 2px/frame = +40f -> 70f, dy = +44px = +1 row -> layer 1
        let _ = drag.update_from_cursor(
            (180.0, 64.0),
            controller.timeline_px_per_frame,
            &controller.document,
            FrameIndex::ZERO,
            &SnapConfig {
                enabled: false,
                ..SnapConfig::default()
            },
        );
        assert_eq!(drag.current_layer_idx, 1);
        assert_eq!(drag.current_start_frame, 70);
        drag.commit(&mut controller)
            .map_err(std::io::Error::other)?;
        assert_eq!(
            controller.document.layers[1].clips[0].timeline_start.get(),
            70
        );

        // TrimRight: -20px @ 2px/frame = -10f -> duration 60 -> 50
        let snap2 = controller
            .document
            .find_clip(&c1)
            .ok_or("missing moved clip")?
            .1
            .clone();
        let mut trim = TimelineDragState::begin(&snap2, 1, 200.0, 60.0, DragMode::TrimRight);
        let _ = trim.update_from_cursor(
            (180.0, 60.0),
            controller.timeline_px_per_frame,
            &controller.document,
            FrameIndex::ZERO,
            &SnapConfig::default(),
        );
        assert_eq!(trim.current_duration_frames, 50);
        trim.commit(&mut controller)
            .map_err(std::io::Error::other)?;
        assert_eq!(controller.document.layers[1].clips[0].duration.get(), 50);
        Ok(())
    }

    #[test]
    fn frame_at_timeline_window_x_handles_header_scroll_and_zoom() {
        assert_eq!(frame_at_timeline_window_x(0.0, 0, 2.0), None);
        assert_eq!(frame_at_timeline_window_x(135.9, 100, 2.0), None);
        assert_eq!(
            frame_at_timeline_window_x(TRACK_HEADER_WIDTH, 100, 2.0),
            Some(FrameIndex::new(100))
        );
        assert_eq!(
            frame_at_timeline_window_x(TRACK_HEADER_WIDTH + 60.0, 120, 2.0),
            Some(FrameIndex::new(150))
        );
        assert_eq!(
            frame_at_timeline_window_x(TRACK_HEADER_WIDTH + 10.0, 50, 0.2),
            Some(FrameIndex::new(100))
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn ruler_ticks_never_overlap_across_zoom_range() {
        for ratio_step in 0..=10 {
            let ppf = px_per_frame_from_zoom_ratio((ratio_step as f32) / 10.0);
            for unit in [TimeDisplayUnit::Timecode, TimeDisplayUnit::Frames] {
                let min_gap_px = match unit {
                    TimeDisplayUnit::Timecode => 84.0,
                    TimeDisplayUnit::Frames => 72.0,
                };
                let ticks = compute_ruler_ticks(0, 50_000, ppf, FrameRate::FPS_60, unit);
                let majors: Vec<u64> = ticks
                    .iter()
                    .filter(|t| t.is_major)
                    .map(|t| t.frame)
                    .collect();
                assert_ne!(majors.len(), 0);
                for pair in majors.windows(2) {
                    let gap_px = ((pair[1] - pair[0]) as f32) * ppf;
                    assert!(
                        gap_px >= min_gap_px - 0.1,
                        "overlap at ppf={ppf}, unit={unit:?}: gap_px={gap_px}"
                    );
                }
            }
        }
    }

    #[test]
    fn row_density_heights_and_anchored_zoom_keep_cursor_frame() {
        assert!((row_height_px(TimelineRowDensity::Compact) - 22.0).abs() < 0.01);
        assert!((row_height_px(TimelineRowDensity::Medium) - 30.0).abs() < 0.01);
        assert!((row_height_px(TimelineRowDensity::Large) - 44.0).abs() < 0.01);

        let mut controller = AppController::new();
        controller.timeline_px_per_frame = 2.0;
        controller.timeline_scroll_x_frames = 100;
        let cursor_x = 160.0;
        // anchor frame = 100 + 160 / 2.0 = 180
        zoom_timeline_anchored(&mut controller, 2.0, cursor_x);
        assert!((controller.timeline_px_per_frame - 4.0).abs() < 0.01);
        // scroll + 160 / 4.0 == 180 => scroll == 140
        assert_eq!(controller.timeline_scroll_x_frames, 140);
    }

    #[test]
    fn parse_timeline_length_supports_frames_seconds_and_timecode()
    -> Result<(), Box<dyn std::error::Error>> {
        let fps = FrameRate::FPS_30;
        assert_eq!(parse_timeline_length_input("450", fps)?, 450);
        assert_eq!(parse_timeline_length_input("450f", fps)?, 450);
        assert_eq!(parse_timeline_length_input("10s", fps)?, 300);
        assert_eq!(parse_timeline_length_input("00:01:02:15", fps)?, 1875);
        assert!(parse_timeline_length_input("0", fps).is_err());
        Ok(())
    }
}
