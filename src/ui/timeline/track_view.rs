//! タイムラインのトラック行ヘッダ・クリップ描画・中間点・ドラッグゴースト構築。

use gpui_kit::component::{Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, Context, ExternalPaths, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, SharedString, Styled as _, div, px, rgba,
};

use crate::core::ids::ClipId;
use crate::timeline::document::{AssetStatus, Clip, ClipKind, Layer, ProjectDocument};
use crate::timeline::snap::clip_intersects_window;
use crate::ui::app_controller::{AppController, MIN_TIMELINE_PX_PER_FRAME};
use crate::ui::timeline::ruler::RulerTick;
use crate::ui::timeline::state::{
    DragMode, MarqueeSelectionState, MidpointDragState, PendingCtrlGesture, TRACK_HEADER_WIDTH,
    TimelineDragState, TimelineEndDragState, TimelinePointerGesture, canvas_width_for_duration,
    canvas_x_from_frame, collect_group_items, row_height_px,
};
use crate::ui::timeline::waveform_view::{render_audio_clip_waveform, sample_clip_waveform_bars};
use crate::ui::window::SeaEditorWindow;

struct ClipPalette {
    fill: u32,
    border: u32,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn visible_window_bounds(controller: &AppController) -> (u64, u64) {
    let ppf = controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let scroll_x = controller.timeline_scroll_x_frames;
    let visible_frames =
        ((controller.timeline_viewport_canvas_width_px.max(320.0)) / ppf).ceil() as u64;
    let visible_end = scroll_x.saturating_add(visible_frames).saturating_add(120);
    (scroll_x, visible_end)
}

fn clip_asset_id(clip: &Clip) -> Option<&crate::core::ids::AssetId> {
    match &clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => Some(asset_id),
        ClipKind::Subtitle(_) => None,
    }
}

/// クリップ種別と欠損状態に応じた `AviUtl` 系配色を返す。
#[must_use]
fn clip_palette(doc: &ProjectDocument, clip: &Clip) -> ClipPalette {
    let is_missing = clip_asset_id(clip).is_some_and(|aid| {
        doc.assets
            .iter()
            .find(|a| &a.id == aid)
            .is_some_and(|a| matches!(a.status, AssetStatus::Missing { .. }))
    });
    if is_missing {
        return ClipPalette {
            fill: 0x7f1d_1dff,
            border: 0xef44_44ff,
        };
    }
    match &clip.kind {
        ClipKind::Video { .. } => ClipPalette {
            fill: 0x1d4e_d8ff,
            border: 0x60a5_faff,
        },
        ClipKind::Image { .. } => ClipPalette {
            fill: 0x0369_a1ff,
            border: 0x38bd_f8ff,
        },
        ClipKind::Audio { .. } => ClipPalette {
            fill: 0x991b_1bff,
            border: 0xf871_71ff,
        },
        ClipKind::Subtitle(_) => ClipPalette {
            fill: 0x9240_0eff,
            border: 0xfbbf_24ff,
        },
    }
}

/// 単一トラック行（左ヘッダ + 右クリップレーン）を描画する。
#[allow(clippy::cast_precision_loss)]
pub fn render_track_row(
    view: &SeaEditorWindow,
    layer_idx: usize,
    ticks: &[RulerTick],
    cx: &mut Context<SeaEditorWindow>,
) -> Option<AnyElement> {
    let controller = &view.controller;
    let layer = controller.document.layers.get(layer_idx)?;
    let row_h = row_height_px(controller.timeline_row_density);
    let header = render_track_header(view, layer, layer_idx, cx);
    let lane = render_track_lane(controller, layer, layer_idx, row_h, ticks, cx);

    Some(
        h_flex()
            .id(("timeline-track-row", layer_idx))
            .w_full()
            .h(px(row_h))
            .border_b_1()
            .border_color(rgba(0x2a2f_3aff))
            .child(header)
            .child(lane)
            .into_any_element(),
    )
}

fn render_track_header(
    view: &SeaEditorWindow,
    layer: &Layer,
    layer_idx: usize,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let is_selected = view.controller.selected_layer_id.as_ref() == Some(&layer.id);
    let row_bg = if is_selected {
        rgba(0x2830_40ff)
    } else if layer_idx.is_multiple_of(2) {
        rgba(0x1e22_2aff)
    } else {
        rgba(0x2126_2fff)
    };

    let layer_id_click = layer.id.clone();
    let layer_id_name = layer.id.clone();
    let layer_name_init = layer.name.clone();
    let is_renaming = view
        .layer_rename_target
        .as_ref()
        .is_some_and(|id| id == &layer.id);

    let title_box: AnyElement = if is_renaming && let Some(input) = &view.layer_rename_input {
        div()
            .flex_1()
            .overflow_hidden()
            .child(gpui_kit::component::input::Input::new(input).small())
            .into_any_element()
    } else {
        div()
            .id(("track-name-label", layer_idx))
            .flex_1()
            .overflow_hidden()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, evt: &MouseDownEvent, window, cx| {
                    this.controller.selected_layer_id = Some(layer_id_name.clone());
                    if evt.click_count >= 2 {
                        this.begin_layer_rename(
                            layer_id_name.clone(),
                            &layer_name_init,
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgba(0xe5e7_ebff))
                    .child(layer.name.clone()),
            )
            .into_any_element()
    };

    let flags = render_track_flag_buttons(layer, layer_idx, cx);
    let lid_right = layer.id.clone();

    let header = h_flex()
        .id(("track-header", layer_idx))
        .w(px(TRACK_HEADER_WIDTH))
        .h_full()
        .px_2()
        .gap_1()
        .items_center()
        .justify_between()
        .bg(row_bg)
        .border_r_1()
        .border_color(rgba(0x363c_49ff))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.controller.selected_layer_id = Some(layer_id_click.clone());
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, _, _| {
                crate::ui::timeline::context_menu::record_header_right_click(
                    this,
                    lid_right.clone(),
                );
            }),
        )
        .child(title_box)
        .child(flags);

    crate::ui::timeline::context_menu::attach_track_header_context_menu(
        header,
        layer.id.clone(),
        layer_idx,
        cx.entity(),
    )
}

fn render_track_flag_buttons(
    layer: &Layer,
    layer_idx: usize,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let lid_vis = layer.id.clone();
    let lid_mute = layer.id.clone();
    let lid_lock = layer.id.clone();

    h_flex()
        .gap(px(2.))
        .items_center()
        .child(render_flag_pill(
            ("layer-vis", layer_idx),
            if layer.visible { "👁" } else { "⊘" },
            !layer.visible,
            rgba(0x363c_49ff),
            rgba(0xf59e_0b33),
            cx.listener(move |this, _, window, cx| {
                if let Err(err) = this.controller.toggle_layer_visible(&lid_vis) {
                    this.controller.status_message = err;
                }
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        ))
        .child(render_flag_pill(
            ("layer-mute", layer_idx),
            if layer.muted { "🔇" } else { "🔊" },
            layer.muted,
            rgba(0x363c_49ff),
            rgba(0xef44_4444),
            cx.listener(move |this, _, window, cx| {
                if let Err(err) = this.controller.toggle_layer_muted(&lid_mute) {
                    this.controller.status_message = err;
                }
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        ))
        .child(render_flag_pill(
            ("layer-lock", layer_idx),
            "🔒",
            layer.locked,
            rgba(0x363c_49ff),
            rgba(0xef44_4444),
            cx.listener(move |this, _, _, cx| {
                if let Err(err) = this.controller.toggle_layer_locked(&lid_lock) {
                    this.controller.status_message = err;
                }
                cx.notify();
            }),
        ))
}

fn render_flag_pill(
    id: (&'static str, usize),
    icon: &'static str,
    active_alert: bool,
    normal_bg: gpui_kit::Rgba,
    alert_bg: gpui_kit::Rgba,
    on_click: impl Fn(&MouseDownEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(16.))
        .h(px(16.))
        .rounded(px(3.))
        .bg(if active_alert { alert_bg } else { normal_bg })
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_size(px(10.))
        .text_color(if active_alert {
            rgba(0xfca5_a5ff)
        } else {
            rgba(0xd1d5_dbff)
        })
        .on_mouse_down(MouseButton::Left, move |ev, window, app| {
            app.stop_propagation();
            on_click(ev, window, app);
        })
        .child(icon)
}

#[allow(clippy::cast_precision_loss)]
fn render_track_lane(
    controller: &AppController,
    layer: &Layer,
    layer_idx: usize,
    row_h: f32,
    ticks: &[RulerTick],
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let ppf = controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let (scroll_x, visible_end) = visible_window_bounds(controller);
    let lane_bg = if layer.locked {
        rgba(0x2a1b_1eff)
    } else if layer_idx.is_multiple_of(2) {
        rgba(0x181b_21ff)
    } else {
        rgba(0x1b1e_24ff)
    };
    let lane_lid_rc = layer.id.clone();

    let mut lane = div()
        .id(("timeline-track-lane", layer_idx))
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(lane_bg)
        .drag_over::<ExternalPaths>(|style, _, _, _| {
            style
                .bg(rgba(0x1e3a_5f44))
                .border_1()
                .border_color(rgba(0x38bd_f899))
        })
        .on_drop::<ExternalPaths>(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
            super::handle_timeline_file_drop(this, paths, None, window, cx);
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                on_empty_lane_mouse_down(this, evt, layer_idx, row_h, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, evt: &MouseDownEvent, _, _| {
                crate::ui::timeline::context_menu::record_lane_right_click(
                    this,
                    lane_lid_rc.clone(),
                    f32::from(evt.position.x),
                );
            }),
        );

    lane = append_lane_grid_and_boundaries(lane, controller, scroll_x, ppf, ticks, cx);

    let selected_ids = controller.effective_selected_clip_ids();
    for clip in &layer.clips {
        if !clip_intersects_window(
            clip.timeline_start.get(),
            clip.duration.get(),
            scroll_x,
            visible_end,
        ) {
            continue;
        }
        lane = lane.child(render_clip_block(
            controller,
            clip,
            layer_idx,
            layer.locked,
            &selected_ids,
            cx,
        ));
    }

    lane = append_lane_drag_ghosts(lane, controller, layer_idx, scroll_x, ppf);
    let lane = append_lane_playhead_and_guides(lane, controller, scroll_x, ppf);
    crate::ui::timeline::context_menu::attach_lane_context_menu(lane, layer.id.clone(), cx.entity())
}

fn on_empty_lane_mouse_down(
    this: &mut SeaEditorWindow,
    evt: &MouseDownEvent,
    layer_idx: usize,
    row_h: f32,
    cx: &mut Context<SeaEditorWindow>,
) {
    let origin_x = this.controller.timeline_canvas_origin_x();
    let local_x = (f32::from(evt.position.x) - origin_x).max(0.0);
    let scroll_y = f32::from(this.timeline_scroll.base_handle().offset().y).abs();
    #[allow(clippy::cast_precision_loss)]
    let local_y = (layer_idx as f32).mul_add(row_h, row_h * 0.5) - scroll_y;
    let additive = evt.modifiers.shift || evt.modifiers.control;
    let base_selection = if additive {
        this.controller.effective_selected_clip_ids()
    } else {
        this.controller.clear_clip_selection();
        Vec::new()
    };
    this.controller.timeline_gesture =
        Some(TimelinePointerGesture::Marquee(MarqueeSelectionState {
            start_local: (local_x, local_y.max(0.0)),
            current_local: (local_x, local_y.max(0.0)),
            start_window_y: f32::from(evt.position.y),
            base_selection,
            additive,
        }));
    this.mark_sliders_dirty();
    cx.notify();
}

fn append_lane_grid_and_boundaries(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    controller: &AppController,
    scroll_x: u64,
    ppf: f32,
    ticks: &[RulerTick],
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    for tick in ticks {
        if !tick.is_major {
            continue;
        }
        let x = canvas_x_from_frame(tick.frame, scroll_x, ppf);
        if x >= 0.0 {
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(x))
                    .w(px(1.))
                    .bg(rgba(0xffff_ff0a)),
            );
        }
    }

    let active_sel = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::SelectionRange(sel)) => Some(sel.frame_range()),
        _ => controller.document.selection_range,
    };
    if let Some(range) = active_sel {
        let sx = canvas_x_from_frame(range.start.get(), scroll_x, ppf);
        let sw = canvas_width_for_duration(range.len_frames(), ppf).max(2.0);
        lane = lane.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(sx))
                .w(px(sw))
                .bg(rgba(0x38bd_f818)),
        );
    }

    let total_dur = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::TimelineEnd(end_drag)) => end_drag.current_end_frame,
        _ => controller.document.total_duration_frames(),
    };
    let end_x = canvas_x_from_frame(total_dur, scroll_x, ppf);
    if end_x >= -6.0 {
        lane = lane
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(end_x))
                    .right_0()
                    .bg(rgba(0x0f11_15b0)),
            )
            .child(
                div()
                    .id("timeline-end-boundary-handle")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(end_x - 3.0))
                    .w(px(7.))
                    .cursor_ew_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            let orig_end = this.controller.document.total_duration_frames().max(1);
                            this.controller.timeline_gesture =
                                Some(TimelinePointerGesture::TimelineEnd(TimelineEndDragState {
                                    origin_end_frame: orig_end,
                                    current_end_frame: orig_end,
                                    start_mouse_x: f32::from(evt.position.x),
                                }));
                            cx.notify();
                        }),
                    )
                    .child(div().mx_auto().w(px(2.)).h_full().bg(rgba(0x6b72_80ff))),
            );
    }
    lane
}

/// トラック行の高さ (`row_h`) に対するクリップタイル高さ（上下 `2px` マージン除く）を返す。
#[must_use]
pub(crate) fn clip_block_height_px(row_h: f32) -> f32 {
    (row_h - 4.0).max(8.0)
}

fn create_clip_outer_container(
    clip_id: &ClipId,
    rect: (f32, f32, f32),
    colors: (u32, u32),
    is_selected: bool,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (left_px, width_px, clip_h) = rect;
    let (fill_rgba, border_rgba) = colors;
    let base = div()
        .id(SharedString::from(format!(
            "timeline-clip-block:{}",
            clip_id.as_str()
        )))
        .absolute()
        .top(px(2.))
        .bottom(px(2.))
        .left(px(left_px))
        .w(px(width_px))
        .h(px(clip_h))
        .rounded(px(3.))
        .bg(rgba(fill_rgba))
        .border_color(rgba(border_rgba))
        .overflow_hidden();
    if is_selected {
        base.border_2()
    } else {
        base.border_1()
    }
}

#[allow(clippy::cast_precision_loss)]
fn render_clip_block(
    controller: &AppController,
    clip: &Clip,
    layer_idx: usize,
    layer_locked: bool,
    selected_ids: &[ClipId],
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let row_h = row_height_px(controller.timeline_row_density);
    let clip_h = clip_block_height_px(row_h);
    let ppf = controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let scroll_x = controller.timeline_scroll_x_frames;
    let is_selected = selected_ids.contains(&clip.id);
    let is_primary = controller.selected_clip_id.as_ref() == Some(&clip.id);
    let palette = clip_palette(&controller.document, clip);

    let start_f = clip.timeline_start.get();
    let dur_f = clip.duration.get();
    let left_px = canvas_x_from_frame(start_f, scroll_x, ppf);
    let width_px = canvas_width_for_duration(dur_f, ppf).max(6.0);
    let border_rgba = if is_selected {
        0xfde0_47ff
    } else {
        palette.border
    };

    // 外側ボックスは `.absolute()` + 固定高でリボン外形を確定させ、ContextMenu の `.relative()` を内側へ隔離する。
    let outer = create_clip_outer_container(
        &clip.id,
        (left_px, width_px, clip_h),
        (palette.fill, border_rgba),
        is_selected,
    );

    let mut inner = div()
        .id(SharedString::from(format!(
            "timeline-clip-inner:{}",
            clip.id.as_str()
        )))
        .size_full()
        .relative();

    if crate::ui::timeline::waveform_view::extract_clip_waveform_source(clip).is_some() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bar_cnt = (width_px / 3.0).clamp(4.0, 120.0) as usize;
        let bars =
            sample_clip_waveform_bars(&controller.mixer, &controller.document, clip, bar_cnt);
        if !bars.is_empty() {
            inner = inner.child(render_audio_clip_waveform(&bars, clip_h));
        }
    }

    inner = append_clip_segments(
        inner,
        clip,
        layer_idx,
        layer_locked,
        row_h,
        width_px,
        is_primary
            .then_some(controller.selected_segment_index)
            .flatten(),
        cx,
    );
    inner = append_clip_midpoints(inner, controller, clip, layer_locked, width_px, cx);
    inner = append_clip_trim_handles(inner, clip, layer_idx, layer_locked, cx);

    let label = match &clip.kind {
        ClipKind::Subtitle(sub) if !sub.text.is_empty() => format!("{} [{}]", clip.name, sub.text),
        _ => clip.name.clone(),
    };
    let clip_id_rc = clip.id.clone();
    let clip_id_menu = clip.id.clone();
    let inner = inner
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, evt: &MouseDownEvent, window, cx| {
                crate::ui::timeline::context_menu::record_clip_right_click(
                    this,
                    clip_id_rc.clone(),
                    f32::from(evt.position.x),
                    window,
                    cx,
                );
            }),
        )
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(6.))
                .right(px(6.))
                .overflow_hidden()
                .text_size(px(10.))
                .text_color(rgba(0xf8fa_fcff))
                .child(label),
        );
    let menu_wrapped = crate::ui::timeline::context_menu::attach_clip_context_menu(
        inner,
        clip_id_menu,
        cx.entity(),
    );
    outer.child(menu_wrapped)
}

#[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
fn append_clip_segments(
    mut box_el: gpui_kit::Stateful<gpui_kit::Div>,
    clip: &Clip,
    layer_idx: usize,
    layer_locked: bool,
    row_h: f32,
    clip_width_px: f32,
    active_segment: Option<usize>,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let grid = clip.grid_frames();
    let total_rel = clip
        .grid_frames()
        .last()
        .copied()
        .unwrap_or(1)
        .saturating_sub(*grid.first().unwrap_or(&0))
        .max(1);
    let b0 = *grid.first().unwrap_or(&0);
    let seg_count = grid.len().saturating_sub(1).max(1);

    for seg_idx in 0..seg_count {
        let s_rel = grid.get(seg_idx).copied().unwrap_or(0);
        let e_rel = grid.get(seg_idx + 1).copied().unwrap_or(b0 + total_rel);
        let frac_x = ((s_rel - b0) as f32) / (total_rel as f32);
        let frac_w = ((e_rel - s_rel).max(1) as f32) / (total_rel as f32);
        let seg_left = clip_width_px * frac_x;
        let seg_w = (clip_width_px * frac_w).max(2.0);
        let dimmed = active_segment.is_some_and(|act| act != seg_idx && seg_count > 1);
        let highlighted = active_segment == Some(seg_idx) && seg_count > 1;
        let clip_id_click = clip.id.clone();

        let mut seg_div = div()
            .id(SharedString::from(format!(
                "clip-segment:{}_{seg_idx}",
                clip.id.as_str()
            )))
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(seg_left))
            .w(px(seg_w));
        seg_div = if layer_locked {
            seg_div.cursor_not_allowed()
        } else {
            seg_div.cursor_crosshair()
        }
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, evt: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_clip_segment_mouse_down(
                    this,
                    &clip_id_click,
                    layer_idx,
                    seg_idx,
                    layer_locked,
                    row_h,
                    evt,
                    window,
                    cx,
                );
            }),
        );
        if dimmed {
            seg_div = seg_div.bg(rgba(0x0000_0055));
        }
        if highlighted {
            seg_div = seg_div.border_t_2().border_color(rgba(0x38bd_f8ff));
        }
        box_el = box_el.child(seg_div);
    }
    box_el
}

#[allow(clippy::too_many_arguments)]
fn on_clip_segment_mouse_down(
    this: &mut SeaEditorWindow,
    clip_id: &ClipId,
    layer_idx: usize,
    seg_idx: usize,
    layer_locked: bool,
    row_h: f32,
    evt: &MouseDownEvent,
    window: &mut gpui_kit::Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let mx = f32::from(evt.position.x);
    let my = f32::from(evt.position.y);
    if evt.modifiers.control {
        this.controller.timeline_gesture =
            Some(TimelinePointerGesture::PendingCtrl(PendingCtrlGesture {
                clip_id: clip_id.clone(),
                origin_layer_idx: layer_idx,
                start_mouse_x: mx,
                start_mouse_y: my,
                row_height_px: row_h,
            }));
        cx.notify();
        return;
    }

    let already_in_group = this
        .controller
        .effective_selected_clip_ids()
        .contains(clip_id);
    if !already_in_group || this.controller.effective_selected_clip_ids().len() == 1 {
        this.controller.select_clip_segment(clip_id, seg_idx);
    }

    if !layer_locked && let Some((_, clip_snap)) = this.controller.document.find_clip(clip_id) {
        let clip_owned = clip_snap.clone();
        let active_ids = this.controller.effective_selected_clip_ids();
        let group = collect_group_items(&this.controller.document, &active_ids);
        let drag = TimelineDragState::begin_with_options(
            &clip_owned,
            layer_idx,
            mx,
            my,
            DragMode::Move,
            row_h,
            false,
            group,
        );
        this.controller.timeline_gesture = Some(TimelinePointerGesture::ClipDrag(drag));
    }
    this.controller.inspector.invalidate_sync();
    this.sync_sliders_now(window, cx);
    cx.notify();
}

#[allow(clippy::cast_precision_loss)]
fn append_clip_midpoints(
    mut box_el: gpui_kit::Stateful<gpui_kit::Div>,
    controller: &AppController,
    clip: &Clip,
    layer_locked: bool,
    clip_width_px: f32,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (b0, b1) = clip.effect_bounds();
    let span = (b1 - b0).max(1) as f32;
    let active_mp_rel = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::MidpointDrag(md)) if md.clip_id == clip.id => {
            Some((md.origin_rel_frame, md.current_rel_frame))
        }
        _ => None,
    };

    for (mp_idx, rel) in clip.visible_midpoints().into_iter().enumerate() {
        let display_rel = match active_mp_rel {
            Some((orig, cur)) if orig == rel => cur,
            _ => rel,
        };
        let x = clip_width_px * ((display_rel - b0) as f32 / span);
        let clip_id_mp = clip.id.clone();
        let clip_id_mp_rc = clip.id.clone();

        let diamond = div()
            .id(SharedString::from(format!(
                "clip-midpoint-diamond:{}_{rel}",
                clip.id.as_str()
            )))
            .absolute()
            .top(px(1.))
            .left(px(x - 5.0))
            .w(px(11.))
            .h(px(11.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_ew_resize()
            .text_size(px(10.))
            .text_color(rgba(0xfef0_8aff))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, evt: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.controller.select_clip_segment(&clip_id_mp, mp_idx);
                    if !layer_locked
                        && let Some((_, snap)) = this.controller.document.find_clip(&clip_id_mp)
                        && let Some(drag) =
                            MidpointDragState::begin(snap, rel, f32::from(evt.position.x))
                    {
                        this.controller.timeline_gesture =
                            Some(TimelinePointerGesture::MidpointDrag(drag));
                    }
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, _| {
                    crate::ui::timeline::context_menu::record_midpoint_right_click(
                        this,
                        clip_id_mp_rc.clone(),
                        rel,
                    );
                }),
            )
            .child("◆");

        box_el = box_el
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(x))
                    .w(px(1.))
                    .bg(rgba(0xfef0_8acc)),
            )
            .child(
                crate::ui::timeline::context_menu::attach_midpoint_context_menu(
                    diamond,
                    clip.id.clone(),
                    rel,
                    cx.entity(),
                ),
            );
    }
    box_el
}

fn append_clip_trim_handles(
    box_el: gpui_kit::Stateful<gpui_kit::Div>,
    clip: &Clip,
    layer_idx: usize,
    layer_locked: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    if layer_locked {
        return box_el;
    }
    let cid_l = clip.id.clone();
    let cid_r = clip.id.clone();
    box_el
        .child(
            div()
                .id(SharedString::from(format!(
                    "clip-trim-left:{}",
                    clip.id.as_str()
                )))
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(6.))
                .cursor_ew_resize()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        start_trim_gesture(this, &cid_l, layer_idx, DragMode::TrimLeft, evt, cx);
                    }),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "clip-trim-right:{}",
                    clip.id.as_str()
                )))
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w(px(6.))
                .cursor_ew_resize()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        start_trim_gesture(this, &cid_r, layer_idx, DragMode::TrimRight, evt, cx);
                    }),
                ),
        )
}

fn start_trim_gesture(
    this: &mut SeaEditorWindow,
    clip_id: &ClipId,
    layer_idx: usize,
    mode: DragMode,
    evt: &MouseDownEvent,
    cx: &mut Context<SeaEditorWindow>,
) {
    let row_h = row_height_px(this.controller.timeline_row_density);
    this.controller.select_single_clip(Some(clip_id.clone()));
    if let Some((_, clip_snap)) = this.controller.document.find_clip(clip_id) {
        let drag = TimelineDragState::begin_with_options(
            clip_snap,
            layer_idx,
            f32::from(evt.position.x),
            f32::from(evt.position.y),
            mode,
            row_h,
            false,
            Vec::new(),
        );
        this.controller.timeline_gesture = Some(TimelinePointerGesture::ClipDrag(drag));
    }
    cx.notify();
}

fn append_lane_drag_ghosts(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    controller: &AppController,
    layer_idx: usize,
    scroll_x: u64,
    ppf: f32,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let active_drag = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::ClipDrag(d)) => Some(d),
        _ => controller.timeline_drag.as_ref(),
    };
    let Some(drag) = active_drag else {
        return lane;
    };

    let (fill, border) = if drag.preview_valid {
        (rgba(0x38bd_f830), rgba(0x38bd_f8ff))
    } else {
        (rgba(0xef44_4455), rgba(0xef44_44ff))
    };

    if drag.mode == DragMode::Move {
        for item in &drag.group_items {
            if item.target_layer_idx != layer_idx {
                continue;
            }
            let x = canvas_x_from_frame(item.target_start_frame, scroll_x, ppf);
            let w = canvas_width_for_duration(item.origin_duration_frames, ppf).max(6.0);
            lane = lane.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .bottom(px(2.))
                    .left(px(x))
                    .w(px(w))
                    .rounded(px(3.))
                    .bg(fill)
                    .border_2()
                    .border_color(border),
            );
        }
    } else if drag.origin_layer_idx == layer_idx {
        let x = canvas_x_from_frame(drag.current_start_frame, scroll_x, ppf);
        let w = canvas_width_for_duration(drag.current_duration_frames, ppf).max(6.0);
        lane = lane.child(
            div()
                .absolute()
                .top(px(2.))
                .bottom(px(2.))
                .left(px(x))
                .w(px(w))
                .rounded(px(3.))
                .bg(fill)
                .border_2()
                .border_color(border),
        );
    }
    lane
}

fn append_lane_playhead_and_guides(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    controller: &AppController,
    scroll_x: u64,
    ppf: f32,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    if let Some(guide) = controller.snap_guide_frame {
        let gx = canvas_x_from_frame(guide, scroll_x, ppf);
        if gx >= 0.0 {
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(gx))
                    .w(px(1.))
                    .bg(rgba(0x22d3_eeff)),
            );
        }
    }
    let head_x = canvas_x_from_frame(controller.clock.current_frame().get(), scroll_x, ppf);
    if head_x >= 0.0 {
        lane = lane.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(head_x))
                .w(px(1.5))
                .bg(rgba(0x38bd_f8cc)),
        );
    }
    lane
}

#[cfg(test)]
mod tests {
    use super::{clip_block_height_px, clip_intersects_window, visible_window_bounds};
    use crate::ui::app_controller::AppController;

    #[test]
    fn clip_block_height_fills_track_minus_vertical_margins() {
        assert!((clip_block_height_px(30.0) - 26.0).abs() < 1e-6);
        assert!((clip_block_height_px(22.0) - 18.0).abs() < 1e-6);
        assert!((clip_block_height_px(42.0) - 38.0).abs() < 1e-6);
        assert!((clip_block_height_px(6.0) - 8.0).abs() < 1e-6);
    }

    #[test]
    fn visible_window_bounds_includes_scrolled_split_right_clip() {
        let mut controller = AppController::new();
        controller.timeline_viewport_canvas_width_px = 900.0;
        controller.timeline_px_per_frame = 3.0;
        controller.timeline_scroll_x_frames = 6900;

        let (visible_start, visible_end) = visible_window_bounds(&controller);
        assert_eq!(visible_start, 6900);
        assert!(visible_end > 7200);

        // 左分割クリップ [0..7175) と右分割クリップ [7175..7232) の両方が可視範囲に含まれること
        assert!(clip_intersects_window(0, 7175, visible_start, visible_end));
        assert!(clip_intersects_window(7175, 57, visible_start, visible_end));
    }
}
