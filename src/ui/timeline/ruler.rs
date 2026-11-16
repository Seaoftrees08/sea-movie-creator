//! タイムライン上部ルーラー（動的目盛り・時間表示切替・プレイヘッドスクラブ・選択範囲指定）。

use gpui_kit::component::h_flex;
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _,
    Styled as _, div, px, rgba,
};

use crate::core::time::{FrameIndex, FrameRate};
use crate::ui::app_controller::{AppController, MIN_TIMELINE_PX_PER_FRAME, TimeDisplayUnit};
use crate::ui::timeline::state::{
    RULER_HEIGHT_PX, SelectionRangeDragState, TRACK_HEADER_WIDTH, TimelinePointerGesture,
    canvas_width_for_duration, canvas_x_from_frame, frame_at_canvas_local_x,
};
use crate::ui::window::SeaEditorWindow;

const TIMECODE_MIN_LABEL_PX: f32 = 84.0;
const FRAMES_MIN_LABEL_PX: f32 = 72.0;
const MINOR_TICK_MIN_PX: f32 = 6.0;

/// ルーラー上の単一目盛り情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulerTick {
    /// タイムライン上のフレーム番号。
    pub frame: u64,
    /// 大目盛り（ラベル表示あり）かどうか。
    pub is_major: bool,
    /// 大目盛り時に表示するテキスト。
    pub label: Option<String>,
}

/// 指定フレーム位置を表示単位 (`Timecode` / `Frames`) に従って整形する。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn format_timeline_timestamp(frame: u64, fps: FrameRate, unit: TimeDisplayUnit) -> String {
    match unit {
        TimeDisplayUnit::Frames => format!("{frame}f"),
        TimeDisplayUnit::Timecode => {
            let nominal_fps = (fps.as_f64().round().max(1.0) as u64).max(1);
            let ff = frame % nominal_fps;
            let total_secs = frame / nominal_fps;
            let ss = total_secs % 60;
            let mm = (total_secs / 60) % 60;
            let hh = total_secs / 3600;
            format!("{hh:02}:{mm:02}:{ss:02}:{ff:02}")
        }
    }
}

/// 可視フレーム区間・ズーム倍率・表示単位に応じた重なり無しルーラー目盛りを算出する。
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn compute_ruler_ticks(
    start_frame: u64,
    end_frame: u64,
    px_per_frame: f32,
    fps: FrameRate,
    unit: TimeDisplayUnit,
) -> Vec<RulerTick> {
    if end_frame < start_frame {
        return Vec::new();
    }
    let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
    let min_major_px = match unit {
        TimeDisplayUnit::Timecode => TIMECODE_MIN_LABEL_PX,
        TimeDisplayUnit::Frames => FRAMES_MIN_LABEL_PX,
    };
    let major_step = pick_major_step_frames(safe_ppf, min_major_px, fps, unit);
    let minor_subdiv = pick_minor_subdivisions(major_step, safe_ppf);
    let step = if let Some(sub) = minor_subdiv {
        (major_step / sub).max(1)
    } else {
        major_step
    };

    let first = (start_frame / step) * step;
    let mut ticks = Vec::new();
    let mut f = first;
    while f <= end_frame.saturating_add(step) {
        let is_major = f.is_multiple_of(major_step);
        if f >= start_frame || is_major {
            let label = is_major.then(|| format_timeline_timestamp(f, fps, unit));
            ticks.push(RulerTick {
                frame: f,
                is_major,
                label,
            });
        }
        if ticks.len() >= 1024 {
            break;
        }
        f = f.saturating_add(step);
    }
    ticks
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn pick_major_step_frames(
    px_per_frame: f32,
    min_label_px: f32,
    fps: FrameRate,
    unit: TimeDisplayUnit,
) -> u64 {
    let candidates: Vec<u64> = match unit {
        TimeDisplayUnit::Frames => vec![
            1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000,
            100_000,
        ],
        TimeDisplayUnit::Timecode => {
            let n = (fps.as_f64().round().max(1.0) as u64).max(1);
            let mut list = vec![1, 2, 5, 10, 15, (n / 2).max(1), n];
            for mult in [2_u64, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600, 7200] {
                list.push(n.saturating_mul(mult));
            }
            list.sort_unstable();
            list.dedup();
            list
        }
    };
    for &step in &candidates {
        if (step as f32) * px_per_frame >= min_label_px {
            return step;
        }
    }
    *candidates.last().unwrap_or(&3600)
}

#[allow(clippy::cast_precision_loss)]
fn pick_minor_subdivisions(major_step: u64, px_per_frame: f32) -> Option<u64> {
    for sub in [5_u64, 4, 2] {
        if major_step > sub && major_step.is_multiple_of(sub) {
            let minor_frames = major_step / sub;
            if (minor_frames as f32) * px_per_frame >= MINOR_TICK_MIN_PX {
                return Some(sub);
            }
        }
    }
    None
}

/// タイムラインルーラー（左コーナー時間表示 + 目盛り + 選択範囲 + プレイヘッド）を描画する。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn render_timeline_ruler(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let corner = render_ruler_corner(controller, cx);
    let lane = build_ruler_lane(controller, cx);

    h_flex()
        .h(px(RULER_HEIGHT_PX))
        .w_full()
        .bg(rgba(0x1b1e_24ff))
        .border_b_1()
        .border_color(rgba(0x363c_49ff))
        .items_center()
        .child(corner)
        .child(lane)
}

fn render_ruler_corner(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let playhead = controller.clock.current_frame().get();
    let time_text = format_timeline_timestamp(
        playhead,
        controller.document.fps,
        controller.time_display_unit,
    );
    let unit_badge = match controller.time_display_unit {
        TimeDisplayUnit::Timecode => "TC",
        TimeDisplayUnit::Frames => "F",
    };

    div()
        .id("timeline-ruler-unit-toggle")
        .w(px(TRACK_HEADER_WIDTH))
        .h_full()
        .px_2()
        .bg(rgba(0x1e22_2aff))
        .border_r_1()
        .border_color(rgba(0x363c_49ff))
        .flex()
        .items_center()
        .justify_between()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.controller.toggle_time_display_unit();
                cx.notify();
            }),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgba(0x38bd_f8ff))
                .child(time_text),
        )
        .child(
            div()
                .px_1()
                .rounded(px(3.))
                .bg(rgba(0x2a2f_3aff))
                .text_size(px(9.))
                .text_color(rgba(0x9ca3_afff))
                .child(unit_badge),
        )
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn build_ruler_lane(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let playhead = controller.clock.current_frame().get();
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
    let head_x = canvas_x_from_frame(playhead, scroll_x, ppf);

    let lane = div()
        .id("timeline-ruler-seek")
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(rgba(0x1619_20ff))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, evt: &MouseDownEvent, window, cx| {
                on_ruler_mouse_down(this, evt, window, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, evt: &MouseDownEvent, _, _| {
                crate::ui::timeline::context_menu::record_ruler_right_click(
                    this,
                    f32::from(evt.position.x),
                );
            }),
        );

    let lane = decorate_ruler_ranges(lane, controller, scroll_x, ppf);
    let lane = append_ruler_ticks(lane, &ticks, scroll_x, ppf);
    let lane = append_ruler_playhead(lane, head_x);
    crate::ui::timeline::context_menu::attach_ruler_context_menu(lane, cx.entity())
}

fn on_ruler_mouse_down(
    this: &mut SeaEditorWindow,
    evt: &MouseDownEvent,
    window: &mut gpui_kit::Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let origin_x = this.controller.timeline_canvas_origin_x();
    let local_x = f32::from(evt.position.x) - origin_x;
    let ppf = this
        .controller
        .timeline_px_per_frame
        .max(MIN_TIMELINE_PX_PER_FRAME);
    let frame = frame_at_canvas_local_x(local_x, this.controller.timeline_scroll_x_frames, ppf);
    if evt.modifiers.shift {
        this.controller.timeline_gesture = Some(TimelinePointerGesture::SelectionRange(
            SelectionRangeDragState {
                anchor_frame: frame,
                current_frame: frame,
            },
        ));
    } else {
        this.controller.timeline_gesture = Some(TimelinePointerGesture::RulerScrub);
        this.controller.seek_to_frame(FrameIndex::new(frame));
        this.mark_sliders_dirty();
        this.sync_sliders_now(window, cx);
    }
    cx.notify();
}

fn decorate_ruler_ranges(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    controller: &AppController,
    scroll_x: u64,
    ppf: f32,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let total_dur = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::TimelineEnd(end_drag)) => end_drag.current_end_frame,
        _ => controller.document.total_duration_frames(),
    };
    let end_x = canvas_x_from_frame(total_dur, scroll_x, ppf);
    if end_x >= 0.0 {
        lane = lane
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(end_x))
                    .right_0()
                    .bg(rgba(0x0f11_15aa)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(end_x))
                    .w(px(2.))
                    .bg(rgba(0x6b72_80ff)),
            );
    }

    let active_selection = match controller.timeline_gesture.as_ref() {
        Some(TimelinePointerGesture::SelectionRange(sel)) => Some(sel.frame_range()),
        _ => controller.document.selection_range,
    };
    if let Some(range) = active_selection {
        let sel_x = canvas_x_from_frame(range.start.get(), scroll_x, ppf);
        let sel_w = canvas_width_for_duration(range.len_frames(), ppf).max(2.0);
        lane = lane.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(sel_x))
                .w(px(sel_w))
                .bg(rgba(0x38bd_f830))
                .border_l_1()
                .border_r_1()
                .border_color(rgba(0x38bd_f8cc)),
        );
    }
    lane
}

fn append_ruler_ticks(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    ticks: &[RulerTick],
    scroll_x: u64,
    ppf: f32,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    for tick in ticks {
        let x = canvas_x_from_frame(tick.frame, scroll_x, ppf);
        if x < -8.0 {
            continue;
        }
        let tick_h = if tick.is_major { 10.0 } else { 5.0 };
        let tick_color = if tick.is_major {
            rgba(0x9ca3_afff)
        } else {
            rgba(0x4b55_63ff)
        };
        lane = lane.child(
            div()
                .absolute()
                .bottom_0()
                .left(px(x))
                .w(px(1.))
                .h(px(tick_h))
                .bg(tick_color),
        );
        if let Some(label) = &tick.label {
            lane = lane.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(x + 3.0))
                    .text_size(px(10.))
                    .text_color(rgba(0xd1d5_dbff))
                    .child(label.clone()),
            );
        }
    }
    lane
}

fn append_ruler_playhead(
    mut lane: gpui_kit::Stateful<gpui_kit::Div>,
    head_x: f32,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    if head_x >= -6.0 {
        lane = lane
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(head_x - 5.0))
                    .w(px(11.))
                    .h(px(9.))
                    .rounded_b(px(2.))
                    .bg(rgba(0x38bd_f8ff))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(8.))
                    .text_color(rgba(0x0f17_2aff))
                    .child("▽"),
            )
            .child(
                div()
                    .absolute()
                    .top(px(8.))
                    .bottom_0()
                    .left(px(head_x))
                    .w(px(2.))
                    .bg(rgba(0x38bd_f8ff)),
            );
    }
    lane
}
