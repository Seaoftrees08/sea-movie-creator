//! インスペクタパネル公開ファサード（後方互換エクスポート）。

pub use crate::ui::inspector::{
    ActiveTrackBarDrag, InspectorMode, InspectorParameterSlot, InspectorState, InspectorWidgetBank,
    InspectorWidgets, MovementModeEdit, MovementModePreset, SegmentEndpointEdit, TrackBarDragStart,
    TrackBarSegmentView, align_clip_after_transient_edit, build_apply_bezier_curve_command,
    build_apply_movement_mode_command, build_apply_movement_mode_with_options,
    build_batch_relative_xy_offset_command, build_expression_command,
    build_remove_keyframe_command, build_reset_clip_parameters_command,
    build_reset_color_adjustments_command, build_set_segment_endpoint_command,
    build_set_segment_endpoint_with_lock, build_switch_mode_command,
    build_transient_drag_param_pair, build_upsert_keyframe_command,
    clip_has_non_default_color_adjustments, effect_frame_at_playhead, evaluation_context_for_clip,
    read_trackbar_segment_view, render_clip_inspector, sample_parameter_curve,
    validate_expression_and_tuning,
};
