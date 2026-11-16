//! 下部タイムラインパネル: `AviUtl` 風高密度タイムライン・動的ルーラー・仮想スクロール・直接ドラッグ編集。

pub mod context_menu;
pub mod ruler;
pub mod state;
pub mod track_view;
pub mod waveform_view;

use std::rc::Rc;

use gpui_kit::component::{VirtualListScrollHandle, h_flex, v_flex, v_virtual_list};
use gpui_kit::{
    AnyElement, Context, ExternalPaths, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PinchEvent, ScrollDelta,
    ScrollEvent, Size, Styled as _, Window, div, point, px, rgba, size,
};

use crate::core::time::FrameIndex;
use crate::timeline::document::TimelineLengthMode;
use crate::ui::app_controller::{
    AppController, MIN_TIMELINE_PX_PER_FRAME, TimeDisplayUnit, TimelineRowDensity,
};
use crate::ui::i18n::{tr, tr_fmt};
use crate::ui::window::SeaEditorWindow;

use self::ruler::{compute_ruler_ticks, render_timeline_ruler};
use self::state::{
    H_SCROLLBAR_HEIGHT_PX, TIMELINE_HEADER_HEIGHT_PX, TRACK_HEADER_WIDTH, TimelinePointerGesture,
    compute_horizontal_scroll_frames, compute_wheel_zoom_factor, frame_at_canvas_local_x,
    frame_at_timeline_window_x, row_height_px, set_timeline_zoom_ratio_anchored,
    should_scroll_tracks_vertically, zoom_ratio_from_px_per_frame, zoom_timeline_anchored,
};
use self::track_view::render_track_row;

pub use self::ruler::{RulerTick, format_timeline_timestamp};
pub use self::state::{
    CTRL_DRAG_THRESHOLD_PX, DragMode, DragSnapEnv, DraggedClipItem, MarqueeSelectionState,
    MidpointDragState, PendingCtrlGesture, SelectionRangeDragState, TimelineDragState,
    TimelineEndDragState, canvas_x_from_frame, parse_timeline_length_input,
};

/// 下部タイムライン領域（ヘッダバー・ルーラー・仮想トラック一覧・水平スクロールバー）を描画する。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn render_timeline_panel(
    controller: &AppController,
    scroll_handle: &VirtualListScrollHandle,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let layer_count = controller.document.layers.len();
    let row_h = row_height_px(controller.timeline_row_density);
    let item_sizes: Rc<Vec<Size<gpui_kit::Pixels>>> =
        Rc::new(vec![size(px(1200.), px(row_h)); layer_count]);
    let view_entity = cx.entity();

    let ppf = controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let scroll_x = controller.timeline_scroll_x_frames;
    let visible_frames =
        ((controller.timeline_viewport_canvas_width_px.max(320.0)) / ppf).ceil() as u64;
    let end_frame = scroll_x.saturating_add(visible_frames.saturating_add(60));
    let ticks = compute_ruler_ticks(
        scroll_x,
        end_frame,
        ppf,
        controller.document.fps,
        controller.time_display_unit,
    );

    v_flex()
        .id("timeline-panel-root")
        .size_full()
        .relative()
        .bg(rgba(0x181b_21ff))
        .border_t_1()
        .border_color(rgba(0x363c_49ff))
        .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
            handle_timeline_scroll_wheel(this, ev, cx);
        }))
        .on_pinch(cx.listener(|this, ev: &PinchEvent, _, cx| {
            handle_timeline_pinch_zoom(this, ev, cx);
        }))
        .drag_over::<ExternalPaths>(|style, _, _, _| {
            style
                .bg(rgba(0x0f17_2a33))
                .border_1()
                .border_color(rgba(0x38bd_f899))
        })
        .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            handle_timeline_file_drop(this, paths, None, window, cx);
        }))
        .child(render_timeline_header(controller, cx))
        .child(render_timeline_ruler(controller, cx))
        .child(render_track_viewport(
            controller,
            view_entity,
            scroll_handle,
            item_sizes,
            ticks,
            cx,
        ))
        .child(render_h_scrollbar(controller, cx))
}

/// ウィンドウ座標からのドロップファイルを読み込み・挿入する。
pub fn handle_timeline_file_drop(
    view: &mut SeaEditorWindow,
    paths: &ExternalPaths,
    target_layer: Option<&crate::core::ids::LayerId>,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let cursor_x = f32::from(window.mouse_position().x);
    let drop_frame = frame_at_timeline_window_x(
        cursor_x,
        view.controller.timeline_scroll_x_frames,
        view.controller.timeline_px_per_frame,
    );
    match view
        .controller
        .import_media_files_at_drop(paths.paths(), target_layer, drop_frame)
    {
        Ok(_) => {
            view.controller.inspector.invalidate_sync();
            view.sync_sliders_now(window, cx);
        }
        Err(err) => {
            view.controller.status_message = format!("ドロップ追加失敗: {err}");
        }
    }
    cx.notify();
}

fn render_timeline_header(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let zoom_box = render_zoom_bar(controller, cx);
    let controls = render_header_controls(controller, cx);

    h_flex()
        .h(px(TIMELINE_HEADER_HEIGHT_PX))
        .w_full()
        .bg(rgba(0x2024_2cff))
        .border_b_1()
        .border_color(rgba(0x363c_49ff))
        .items_center()
        .child(zoom_box)
        .child(controls)
}

fn render_zoom_bar(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let ratio = zoom_ratio_from_px_per_frame(controller.timeline_px_per_frame);
    let bar_width = 76.0_f32;
    let fill_w = (bar_width * ratio).clamp(2.0, bar_width);

    h_flex()
        .w(px(TRACK_HEADER_WIDTH))
        .h_full()
        .px_2()
        .gap_1()
        .items_center()
        .justify_between()
        .border_r_1()
        .border_color(rgba(0x363c_49ff))
        .child(
            div()
                .id("tl-zoom-out")
                .px_1()
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgba(0x9ca3_afff))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        let center = this.controller.timeline_viewport_canvas_width_px * 0.5;
                        zoom_timeline_anchored(&mut this.controller, 0.8, center);
                        cx.notify();
                    }),
                )
                .child("−"),
        )
        .child(
            div()
                .id("tl-zoom-slider-rail")
                .w(px(bar_width))
                .h(px(8.))
                .rounded(px(4.))
                .bg(rgba(0x1113_18ff))
                .border_1()
                .border_color(rgba(0x363c_49ff))
                .relative()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        let anchor = this.controller.timeline_viewport_canvas_width_px * 0.5;
                        let approx_left = 26.0_f32;
                        let ratio =
                            ((f32::from(evt.position.x) - approx_left) / bar_width).clamp(0.0, 1.0);
                        set_timeline_zoom_ratio_anchored(&mut this.controller, ratio, anchor);
                        this.controller.timeline_gesture = Some(TimelinePointerGesture::ZoomBar {
                            bar_left_x: approx_left,
                            bar_width,
                        });
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .h_full()
                        .w(px(fill_w))
                        .rounded(px(3.))
                        .bg(rgba(0x38bd_f8ff)),
                ),
        )
        .child(
            div()
                .id("tl-zoom-in")
                .px_1()
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgba(0x9ca3_afff))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        let center = this.controller.timeline_viewport_canvas_width_px * 0.5;
                        zoom_timeline_anchored(&mut this.controller, 1.25, center);
                        cx.notify();
                    }),
                )
                .child("+"),
        )
}

fn render_header_controls(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let total = controller.document.total_duration_frames();
    let total_str = total.to_string();
    let len_label = match controller.document.timeline_length {
        TimelineLengthMode::AutoToLastClip => tr_fmt("timeline.len_auto", &[("total", &total_str)]),
        TimelineLengthMode::Manual { end_frame } => {
            let end_s = end_frame.get().to_string();
            tr_fmt("timeline.len_manual", &[("total", &end_s)])
        }
    };
    let density_label = match controller.timeline_row_density {
        TimelineRowDensity::Compact => tr("timeline.density_compact"),
        TimelineRowDensity::Medium => tr("timeline.density_medium"),
        TimelineRowDensity::Large => tr("timeline.density_large"),
    };
    let unit_label = match controller.time_display_unit {
        TimeDisplayUnit::Timecode => "TC",
        TimeDisplayUnit::Frames => "Frame",
    };

    h_flex()
        .flex_1()
        .h_full()
        .px_2()
        .gap_2()
        .items_center()
        .justify_between()
        .child(
            h_flex()
                .gap(px(6.))
                .items_center()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgba(0xe5e7_ebff))
                        .child(tr("timeline.title")),
                )
                .child(header_pill(
                    "tl-unit-pill",
                    unit_label,
                    false,
                    cx.listener(|this, _, _, cx| {
                        this.controller.toggle_time_display_unit();
                        cx.notify();
                    }),
                ))
                .child(header_pill(
                    "tl-length-modal-btn",
                    len_label,
                    matches!(
                        controller.document.timeline_length,
                        TimelineLengthMode::Manual { .. }
                    ),
                    cx.listener(|this, _, window, cx| {
                        this.open_timeline_length_modal(window, cx);
                    }),
                ))
                .child(header_pill(
                    "tl-follow-toggle",
                    tr("timeline.follow"),
                    controller.follow_selected_on_seek,
                    cx.listener(|this, _, _, cx| {
                        this.controller.toggle_follow_selected_on_seek();
                        cx.notify();
                    }),
                ))
                .child(header_pill(
                    "tl-snap-toggle",
                    tr("timeline.snap"),
                    controller.snap_config.enabled,
                    cx.listener(|this, _, _, cx| {
                        this.controller.toggle_snap_enabled();
                        cx.notify();
                    }),
                ))
                .child(header_pill(
                    "tl-density-cycle",
                    density_label,
                    false,
                    cx.listener(|this, _, _, cx| {
                        this.controller.cycle_timeline_row_density();
                        cx.notify();
                    }),
                ))
                .child(header_pill(
                    "tl-add-layer-btn",
                    tr("timeline.add_track"),
                    false,
                    cx.listener(|this, _, _, cx| {
                        if let Err(err) = this.controller.add_empty_layer() {
                            this.controller.status_message = err;
                        }
                        cx.notify();
                    }),
                )),
        )
        .child(render_compact_status(controller))
}

fn header_pill(
    id: &'static str,
    label: impl Into<String>,
    active: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .py(px(1.))
        .rounded(px(3.))
        .bg(if active {
            rgba(0x0c4a_6eff)
        } else {
            rgba(0x2a2f_3aff)
        })
        .border_1()
        .border_color(if active {
            rgba(0x38bd_f8ff)
        } else {
            rgba(0x363c_49ff)
        })
        .cursor_pointer()
        .text_size(px(10.))
        .text_color(if active {
            rgba(0xe0f2_feff)
        } else {
            rgba(0xd1d5_dbff)
        })
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label.into())
}

fn render_compact_status(controller: &AppController) -> impl IntoElement {
    let msg = controller.status_message.as_str();
    let is_err = msg.starts_with("編集制約") || msg.starts_with("ドロップ") || msg.contains("失敗");
    let color = if is_err {
        rgba(0xf871_71ff)
    } else {
        rgba(0x9ca3_afff)
    };
    div()
        .max_w(px(280.))
        .overflow_hidden()
        .text_size(px(10.))
        .text_color(color)
        .child(msg.to_owned())
}

fn render_track_viewport(
    controller: &AppController,
    view_entity: gpui_kit::Entity<SeaEditorWindow>,
    scroll_handle: &VirtualListScrollHandle,
    item_sizes: Rc<Vec<Size<gpui_kit::Pixels>>>,
    ticks: Vec<RulerTick>,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let layer_count = item_sizes.len();
    if layer_count == 0 {
        return div()
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(rgba(0x9ca3_afff))
            .child(tr("timeline.empty_hint"))
            .into_any_element();
    }

    let ticks_rc = Rc::new(ticks);
    let mut area = div()
        .id("timeline-track-viewport")
        .flex_1()
        .w_full()
        .relative()
        .overflow_hidden()
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
            handle_timeline_window_mouse_move(this, ev, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let _ = ev;
                handle_timeline_window_mouse_up(this, window, cx);
            }),
        )
        .child(
            v_virtual_list(
                view_entity,
                "timeline-track-list",
                item_sizes,
                move |view, visible_range, _window, cx| {
                    visible_range
                        .filter_map(|ix| render_track_row(view, ix, &ticks_rc, cx))
                        .collect()
                },
            )
            .track_scroll(scroll_handle),
        );

    if let Some(TimelinePointerGesture::Marquee(marquee)) = controller.timeline_gesture.as_ref() {
        let min_x = marquee.start_local.0.min(marquee.current_local.0).max(0.0);
        let max_x = marquee.start_local.0.max(marquee.current_local.0).max(0.0);
        let min_y = marquee.start_local.1.min(marquee.current_local.1).max(0.0);
        let max_y = marquee.start_local.1.max(marquee.current_local.1).max(0.0);
        area = area.child(
            div()
                .absolute()
                .left(px(TRACK_HEADER_WIDTH + min_x))
                .top(px(min_y))
                .w(px((max_x - min_x).max(2.0)))
                .h(px((max_y - min_y).max(2.0)))
                .bg(rgba(0x38bd_f824))
                .border_1()
                .border_color(rgba(0x38bd_f8ff)),
        );
    }

    area.into_any_element()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn render_h_scrollbar(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let ppf = controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let viewport_w = controller.timeline_viewport_canvas_width_px.max(320.0);
    let visible_frames = (viewport_w / ppf).max(10.0) as u64;
    let content_frames = controller.document.ruler_display_span_frames().max(
        controller
            .timeline_scroll_x_frames
            .saturating_add(visible_frames),
    );
    let thumb_frac = ((visible_frames as f32) / (content_frames.max(1) as f32)).clamp(0.06, 1.0);
    let scroll_frac = ((controller.timeline_scroll_x_frames as f32)
        / (content_frames.max(1) as f32))
        .clamp(0.0, 1.0 - thumb_frac);
    let thumb_left = viewport_w * scroll_frac;
    let thumb_w = (viewport_w * thumb_frac).max(24.0);

    let sel_info =
        if !controller.selected_clip_ids.is_empty() && controller.selected_clip_ids.len() > 1 {
            format!("{}個選択中", controller.selected_clip_ids.len())
        } else if let Some(r) = controller.document.selection_range {
            format!("選択: [{}..{})", r.start.get(), r.end.get())
        } else {
            String::new()
        };

    h_flex()
        .h(px(H_SCROLLBAR_HEIGHT_PX))
        .w_full()
        .bg(rgba(0x1417_1dff))
        .border_t_1()
        .border_color(rgba(0x2a2f_3aff))
        .items_center()
        .child(
            div()
                .w(px(TRACK_HEADER_WIDTH))
                .h_full()
                .px_2()
                .border_r_1()
                .border_color(rgba(0x2a2f_3aff))
                .flex()
                .items_center()
                .text_size(px(9.))
                .text_color(rgba(0x9ca3_afff))
                .child(sel_info),
        )
        .child(
            div()
                .id("tl-h-scrollbar-track")
                .flex_1()
                .h_full()
                .relative()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        let mx = f32::from(evt.position.x);
                        let local_x = (mx - this.controller.timeline_canvas_origin_x()).max(0.0);
                        let frac = (local_x / viewport_w).clamp(0.0, 1.0);
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let next_scroll = ((content_frames as f32) * frac).round() as u64;
                        this.controller.timeline_scroll_x_frames = next_scroll;
                        this.controller.timeline_gesture =
                            Some(TimelinePointerGesture::HScrollbar {
                                start_mouse_x: mx,
                                origin_scroll_frames: next_scroll,
                                track_width_px: viewport_w,
                                content_frames,
                            });
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .bottom(px(2.))
                        .left(px(thumb_left))
                        .w(px(thumb_w))
                        .rounded(px(4.))
                        .bg(rgba(0x4b55_63ff)),
                ),
        )
}

/// タイムライン上のマウスホイール（`Ctrl`+ホイール=カーソル基準拡大縮小、通常=時間軸スクロール、
/// 左トラックヘッダ上／`Alt`+ホイール=縦トラック一覧スクロール）を処理する。
#[allow(clippy::cast_possible_truncation)]
pub fn handle_timeline_scroll_wheel(
    this: &mut SeaEditorWindow,
    ev: &ScrollEvent,
    cx: &mut Context<SeaEditorWindow>,
) {
    let (dx, dy) = match ev.delta {
        ScrollDelta::Pixels(pt) => (f32::from(pt.x), f32::from(pt.y)),
        ScrollDelta::Lines(pt) => (pt.x * 36.0, pt.y * 36.0),
    };
    if dx.abs() < 1e-4 && dy.abs() < 1e-4 {
        return;
    }

    if ev.modifiers.control {
        let Some(factor) = compute_wheel_zoom_factor(dx, dy) else {
            return;
        };
        let cursor_canvas_x =
            (f32::from(ev.position.x) - this.controller.timeline_canvas_origin_x()).max(0.0);
        zoom_timeline_anchored(&mut this.controller, factor, cursor_canvas_x);
        this.timeline_scroll
            .base_handle()
            .set_offset(point(px(0.0), px(-this.controller.timeline_scroll_y_px)));
        cx.stop_propagation();
        cx.notify();
        return;
    }

    if should_scroll_tracks_vertically(
        f32::from(ev.position.x),
        this.controller.timeline_canvas_origin_x(),
        ev.modifiers.alt,
        ev.modifiers.shift,
        dx,
        dy,
    ) {
        let row_h = row_height_px(this.controller.timeline_row_density);
        #[allow(clippy::cast_precision_loss)]
        let max_y = (this.controller.document.layers.len() as f32 * row_h).max(0.0);
        let next_y = (-this.controller.timeline_scroll_y_px + dy).clamp(-max_y, 0.0);
        this.timeline_scroll
            .base_handle()
            .set_offset(point(px(0.0), px(next_y)));
        this.controller.timeline_scroll_y_px = (-next_y).max(0.0);
        cx.stop_propagation();
        cx.notify();
        return;
    }

    if let Some(next_scroll) = compute_horizontal_scroll_frames(
        this.controller.timeline_scroll_x_frames,
        dx,
        dy,
        this.controller.timeline_px_per_frame,
    ) {
        this.controller.timeline_scroll_x_frames = next_scroll;
    }
    this.timeline_scroll
        .base_handle()
        .set_offset(point(px(0.0), px(-this.controller.timeline_scroll_y_px)));
    cx.stop_propagation();
    cx.notify();
}

/// タッチパッドのピンチジェスチャーでカーソル位置基準のタイムライン縮尺を変更する。
pub fn handle_timeline_pinch_zoom(
    this: &mut SeaEditorWindow,
    ev: &PinchEvent,
    cx: &mut Context<SeaEditorWindow>,
) {
    if ev.delta.abs() < 1e-4 {
        return;
    }
    let factor = (1.0 + ev.delta).clamp(0.5, 2.0);
    let cursor_canvas_x =
        (f32::from(ev.position.x) - this.controller.timeline_canvas_origin_x()).max(0.0);
    zoom_timeline_anchored(&mut this.controller, factor, cursor_canvas_x);
    cx.stop_propagation();
    cx.notify();
}

/// ウィンドウ座標基準の位置と修飾キーでタイムライン操作を更新する。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn handle_timeline_window_mouse_move(
    this: &mut SeaEditorWindow,
    ev: &MouseMoveEvent,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    if let Some(mut legacy_drag) = this.controller.timeline_drag.take() {
        let env = DragSnapEnv {
            doc: &this.controller.document,
            playhead: this.controller.clock.current_frame(),
            snap_cfg: &this.controller.snap_config,
            shift_bypass_snap: ev.modifiers.shift,
        };
        let guide = legacy_drag.update_from_cursor_env(
            (f32::from(ev.position.x), f32::from(ev.position.y)),
            this.controller.timeline_px_per_frame,
            row_height_px(this.controller.timeline_row_density),
            &env,
        );
        this.controller.snap_guide_frame = guide;
        this.controller.timeline_drag = Some(legacy_drag);
        cx.notify();
        return;
    }

    let Some(gesture) = this.controller.timeline_gesture.take() else {
        return;
    };
    let next = route_gesture_mouse_move(this, gesture, ev, window, cx);
    this.controller.timeline_gesture = Some(next);
    cx.notify();
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn route_gesture_mouse_move(
    this: &mut SeaEditorWindow,
    mut gesture: TimelinePointerGesture,
    ev: &MouseMoveEvent,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) -> TimelinePointerGesture {
    let mx = f32::from(ev.position.x);
    let my = f32::from(ev.position.y);
    let ppf = this
        .controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let origin_x = this.controller.timeline_canvas_origin_x();
    let row_h = row_height_px(this.controller.timeline_row_density);

    if let Some(promoted) =
        route_drag_gesture_move(this, &mut gesture, (mx, my), ev.modifiers.shift)
    {
        return promoted;
    }

    match &mut gesture {
        TimelinePointerGesture::Marquee(marquee) => {
            let local_x = (mx - origin_x).max(0.0);
            let local_y = (marquee.start_local.1 + (my - marquee.start_window_y)).max(0.0);
            marquee.current_local = (local_x, local_y);
            let hits = marquee.compute_selected_clips(
                &this.controller.document,
                this.controller.timeline_scroll_x_frames,
                ppf,
                row_h,
            );
            this.controller.set_selected_clips(hits);
        }
        TimelinePointerGesture::RulerScrub => {
            let frame = frame_at_canvas_local_x(
                mx - origin_x,
                this.controller.timeline_scroll_x_frames,
                ppf,
            );
            this.controller.seek_to_frame(FrameIndex::new(frame));
            this.mark_sliders_dirty();
            this.sync_sliders_now(window, cx);
        }
        TimelinePointerGesture::SelectionRange(sel) => {
            sel.current_frame = frame_at_canvas_local_x(
                mx - origin_x,
                this.controller.timeline_scroll_x_frames,
                ppf,
            );
        }
        TimelinePointerGesture::ZoomBar {
            bar_left_x,
            bar_width,
        } => {
            let ratio = ((mx - *bar_left_x) / bar_width.max(1.0)).clamp(0.0, 1.0);
            let anchor = this.controller.timeline_viewport_canvas_width_px * 0.5;
            set_timeline_zoom_ratio_anchored(&mut this.controller, ratio, anchor);
        }
        TimelinePointerGesture::HScrollbar {
            start_mouse_x,
            origin_scroll_frames,
            track_width_px,
            content_frames,
        } => {
            let dx = mx - *start_mouse_x;
            let d_frames =
                ((dx / track_width_px.max(1.0)) * (*content_frames as f32)).round() as i64;
            let next = i64::try_from(*origin_scroll_frames).unwrap_or(0) + d_frames;
            this.controller.timeline_scroll_x_frames = u64::try_from(next.max(0)).unwrap_or(0);
        }
        _ => {}
    }
    gesture
}

fn route_drag_gesture_move(
    this: &mut SeaEditorWindow,
    gesture: &mut TimelinePointerGesture,
    cursor: (f32, f32),
    shift_bypass: bool,
) -> Option<TimelinePointerGesture> {
    let (mx, my) = cursor;
    let ppf = this
        .controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let row_h = row_height_px(this.controller.timeline_row_density);
    let env = DragSnapEnv {
        doc: &this.controller.document,
        playhead: this.controller.clock.current_frame(),
        snap_cfg: &this.controller.snap_config,
        shift_bypass_snap: shift_bypass,
    };
    match gesture {
        TimelinePointerGesture::PendingCtrl(pending) => {
            let selected = this.controller.effective_selected_clip_ids();
            if let Some(mut drag) =
                pending.try_promote_to_duplicate_drag(mx, my, env.doc, &selected)
            {
                let guide = drag.update_from_cursor_env((mx, my), ppf, row_h, &env);
                this.controller.snap_guide_frame = guide;
                return Some(TimelinePointerGesture::ClipDrag(drag));
            }
        }
        TimelinePointerGesture::ClipDrag(drag) => {
            this.controller.snap_guide_frame =
                drag.update_from_cursor_env((mx, my), ppf, row_h, &env);
        }
        TimelinePointerGesture::MidpointDrag(md) => {
            this.controller.snap_guide_frame = md.update_from_cursor(mx, ppf, &env);
        }
        TimelinePointerGesture::TimelineEnd(end_drag) => {
            this.controller.snap_guide_frame = end_drag.update_from_cursor(mx, ppf, &env);
        }
        _ => {}
    }
    None
}

/// タイムライン及びウィンドウ全体のマウスボタン解放を確定・キャンセル処理する。
pub fn handle_timeline_window_mouse_up(
    this: &mut SeaEditorWindow,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    this.controller.snap_guide_frame = None;
    if let Some(drag) = this.controller.timeline_drag.take() {
        if let Err(err) = drag.commit(&mut this.controller) {
            this.controller.status_message = format!("編集制約: {err}");
        }
        this.controller.inspector.invalidate_sync();
        this.sync_sliders_now(window, cx);
        cx.notify();
        return;
    }

    let Some(gesture) = this.controller.timeline_gesture.take() else {
        return;
    };
    match gesture {
        TimelinePointerGesture::PendingCtrl(pending) => {
            this.controller.toggle_clip_selection(&pending.clip_id);
        }
        TimelinePointerGesture::ClipDrag(drag) => {
            if let Err(err) = drag.commit(&mut this.controller) {
                this.controller.status_message = format!("編集制約: {err}");
            }
        }
        TimelinePointerGesture::MidpointDrag(md) => {
            if let Err(err) = md.commit(&mut this.controller) {
                this.controller.status_message = format!("中間点移動失敗: {err}");
            }
        }
        TimelinePointerGesture::Marquee(marquee) => {
            let ppf = this
                .controller
                .timeline_px_per_frame
                .max(MIN_TIMELINE_PX_PER_FRAME);
            let row_h = row_height_px(this.controller.timeline_row_density);
            let hits = marquee.compute_selected_clips(
                &this.controller.document,
                this.controller.timeline_scroll_x_frames,
                ppf,
                row_h,
            );
            this.controller.set_selected_clips(hits);
        }
        TimelinePointerGesture::SelectionRange(sel) => {
            if let Err(err) = this.controller.set_selection_range(Some(sel.frame_range())) {
                this.controller.status_message = format!("選択範囲設定失敗: {err}");
            }
        }
        TimelinePointerGesture::TimelineEnd(end_drag) => {
            if let Err(err) = end_drag.commit(&mut this.controller) {
                this.controller.status_message = format!("総尺変更失敗: {err}");
            }
        }
        TimelinePointerGesture::RulerScrub
        | TimelinePointerGesture::ZoomBar { .. }
        | TimelinePointerGesture::HScrollbar { .. } => {}
    }

    this.controller.inspector.invalidate_sync();
    this.sync_sliders_now(window, cx);
    cx.notify();
}

/// 手動タイムライン総尺入力モーダルオーバーレイを描画する。
pub fn render_timeline_length_modal_overlay(
    view: &SeaEditorWindow,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let is_manual = view
        .timeline_length_modal
        .as_ref()
        .is_some_and(|m| m.is_manual);
    let total = view.controller.document.total_duration_frames();

    div()
        .id("timeline-length-modal-backdrop")
        .absolute()
        .inset_0()
        .bg(rgba(0x0000_0088))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(render_timeline_length_modal_card(
            view, is_manual, total, cx,
        ))
}

fn render_timeline_length_modal_card(
    view: &SeaEditorWindow,
    is_manual: bool,
    total_frames: u64,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut card = v_flex()
        .w(px(340.))
        .p_4()
        .gap_3()
        .rounded(px(6.))
        .bg(rgba(0x2226_2eff))
        .border_1()
        .border_color(rgba(0x363c_49ff))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgba(0xe5e7_ebff))
                .child(tr("timeline.modal_title")),
        )
        .child(
            h_flex()
                .gap_2()
                .child(header_pill(
                    "tl-len-mode-auto",
                    tr("timeline.modal_auto"),
                    !is_manual,
                    cx.listener(|this, _, _, cx| {
                        if let Some(modal) = &mut this.timeline_length_modal {
                            modal.is_manual = false;
                        }
                        cx.notify();
                    }),
                ))
                .child(header_pill(
                    "tl-len-mode-manual",
                    tr("timeline.modal_manual"),
                    is_manual,
                    cx.listener(|this, _, _, cx| {
                        if let Some(modal) = &mut this.timeline_length_modal {
                            modal.is_manual = true;
                        }
                        cx.notify();
                    }),
                )),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgba(0x9ca3_afff))
                .child(format!(
                    "現在の総尺: {total_frames}f (例: 600 / 10s / 00:00:10:00)"
                )),
        );

    if is_manual && let Some(modal) = &view.timeline_length_modal {
        card = card.child(gpui_kit::component::input::Input::new(&modal.input));
    }

    card.child(
        h_flex()
            .justify_end()
            .gap_2()
            .child(header_pill(
                "tl-len-cancel",
                tr("common.cancel"),
                false,
                cx.listener(|this, _, _, cx| {
                    this.close_timeline_length_modal();
                    cx.notify();
                }),
            ))
            .child(header_pill(
                "tl-len-apply",
                tr("timeline.modal_apply"),
                true,
                cx.listener(|this, _, window, cx| {
                    this.apply_timeline_length_modal(window, cx);
                }),
            )),
    )
}
