//! プレビューパネルの直接キャンバス操作・マスター音声波形・トランスポートバー描画群。

pub mod canvas_ops;
pub mod master_waveform;
pub mod panel;

pub use canvas_ops::{
    DragModifiers, HANDLE_HIT_RADIUS_PX, PREVIEW_GRID_STEP_PX, PreviewClipDragSnapshot,
    PreviewDragMode, PreviewDragState, PreviewHandleKind, PreviewLayerHitQuad,
    PreviewViewportTransform, ROTATE_HANDLE_OFFSET_PX, UpdatedClipVisualParams,
    build_preview_hit_quads, clip_has_missing_asset, clip_source_dimensions, clip_visual_props,
    clip_visual_props_mut, compute_drag_updated_clip, find_missing_asset_clip_at_playhead,
    handle_screen_positions, hit_test_preview_layers, hit_test_selection_handle,
    preview_visual_edit_commands, shift_animated_param, step_zoom_preset, zoom_around_cursor,
};
pub use master_waveform::{
    MASTER_WAVEFORM_BIN_MS, MASTER_WAVEFORM_CHUNK_SECS, MASTER_WAVEFORM_MAX_CHUNKS,
    MASTER_WAVEFORM_SAMPLE_RATE, MasterWaveformCache, MasterWaveformChunk, bins_per_chunk,
    compute_master_waveform_chunk_peaks, sample_waveform_bars_for_timeline,
};
pub use panel::render_preview_panel;
