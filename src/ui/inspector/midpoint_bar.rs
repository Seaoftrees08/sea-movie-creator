//! クリップ上部の中間点ナビゲーション・セグメント切替バー。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::base::Disableable as _;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, relative, rgba,
};

use crate::core::ids::ClipId;
use crate::core::time::FrameIndex;
use crate::timeline::document::Clip;
use crate::ui::app_controller::AppController;
use crate::ui::inspector::segment_ops::effect_frame_at_playhead;
use crate::ui::window::SeaEditorWindow;

/// 中間点セグメント操作バーを構築する。
///
/// `[開始f] [◀] [中間点プログレス・セグメント列] [▶] [終了f] [+] [-]`
pub fn render_midpoint_bar(
    controller: &AppController,
    clip: &Clip,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let grid = clip.grid_frames();
    let seg_count = grid.len().saturating_sub(1).max(1);
    let active_seg = controller
        .selected_segment_index
        .unwrap_or(0)
        .min(seg_count - 1);
    let seg_start = grid[active_seg];
    let seg_end = grid.get(active_seg + 1).copied().unwrap_or(seg_start);

    let playhead = controller.clock.current_frame();
    let eff_head = effect_frame_at_playhead(clip, playhead);
    let can_add = can_add_midpoint_at(clip, playhead);
    let can_remove = can_remove_midpoint_near(clip, playhead);

    v_flex()
        .gap_1()
        .p_1p5()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(render_midpoint_header_row(
            clip.id.clone(),
            active_seg,
            seg_count,
            eff_head,
            can_add,
            can_remove,
            theme,
            cx,
        ))
        .child(render_midpoint_track_row(
            &clip.id, &grid, active_seg, seg_start, seg_end, eff_head, theme, cx,
        ))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_midpoint_header_row(
    clip_id: ClipId,
    active_seg: usize,
    seg_count: usize,
    eff_head: i64,
    can_add: bool,
    can_remove: bool,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid_add = clip_id.clone();
    let cid_rem = clip_id;
    let mid_count = seg_count.saturating_sub(1);

    h_flex()
        .items_center()
        .justify_between()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "区間 {}/{} · 中間点 {} · @{}f",
                    active_seg + 1,
                    seg_count,
                    mid_count,
                    eff_head
                )),
        )
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new("midpoint-add-btn")
                        .ghost()
                        .small()
                        .disabled(!can_add)
                        .label("+中間点")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.controller.add_midpoint_at_playhead().is_ok() {
                                select_segment_after_midpoint_change(this, &cid_add, true);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("midpoint-remove-btn")
                        .ghost()
                        .small()
                        .disabled(!can_remove)
                        .label("-削除")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.controller.remove_midpoint_near_playhead().is_ok() {
                                select_segment_after_midpoint_change(this, &cid_rem, false);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
                            }
                            cx.notify();
                        })),
                ),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_midpoint_track_row(
    clip_id: &ClipId,
    grid: &[i64],
    active_seg: usize,
    seg_start: i64,
    seg_end: i64,
    eff_head: i64,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let seg_count = grid.len().saturating_sub(1).max(1);
    let cid_prev = clip_id.clone();
    let cid_next = clip_id.clone();
    let total_span = grid
        .last()
        .and_then(|&last| grid.first().map(|&first| (last - first).max(1)))
        .unwrap_or(1);

    h_flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .w(px(40.0))
                .text_xs()
                .text_color(theme.foreground)
                .child(format!("{seg_start}f")),
        )
        .child(
            Button::new("midpoint-seg-prev")
                .ghost()
                .small()
                .disabled(active_seg == 0)
                .label("◀")
                .on_click(cx.listener(move |this, _, window, cx| {
                    let prev = active_seg.saturating_sub(1);
                    this.controller.select_clip_segment(&cid_prev, prev);
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        )
        .child(render_segment_strip(
            clip_id, grid, active_seg, total_span, eff_head, theme, cx,
        ))
        .child(
            Button::new("midpoint-seg-next")
                .ghost()
                .small()
                .disabled(active_seg + 1 >= seg_count)
                .label("▶")
                .on_click(cx.listener(move |this, _, window, cx| {
                    let next = (active_seg + 1).min(seg_count.saturating_sub(1));
                    this.controller.select_clip_segment(&cid_next, next);
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        )
        .child(
            div()
                .w(px(40.0))
                .text_xs()
                .text_color(theme.foreground)
                .child(format!("{seg_end}f")),
        )
}

#[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
fn render_segment_strip(
    clip_id: &ClipId,
    grid: &[i64],
    active_seg: usize,
    total_span: i64,
    eff_head: i64,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let seg_count = grid.len().saturating_sub(1).max(1);
    let mut strip = h_flex()
        .flex_1()
        .h(px(16.0))
        .gap_0p5()
        .p_0p5()
        .rounded_sm()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border);

    for ix in 0..seg_count {
        let s_start = grid[ix];
        let s_end = grid.get(ix + 1).copied().unwrap_or(s_start);
        let frac = if grid.len() >= 2 {
            ((s_end - s_start).max(1) as f32 / total_span as f32).clamp(0.05, 1.0)
        } else {
            1.0
        };
        let is_selected = ix == active_seg;
        let contains_head = eff_head >= s_start && eff_head <= s_end;
        let cid = clip_id.clone();
        let bg = if is_selected {
            rgba(0x3b82_f6cc)
        } else if contains_head {
            rgba(0x3341_55ff)
        } else {
            rgba(0x1e29_3bff)
        };
        strip = strip.child(
            div()
                .id(("midpoint-seg-pill", ix))
                .w(relative(frac))
                .h_full()
                .rounded_xs()
                .bg(bg)
                .border_1()
                .border_color(if is_selected {
                    rgba(0x60a5_faff)
                } else {
                    rgba(0x3341_55ff)
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.controller.select_clip_segment(&cid, ix);
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        );
    }

    strip
}

fn can_add_midpoint_at(clip: &Clip, playhead: FrameIndex) -> bool {
    if clip.duration.get() < 3 {
        return false;
    }
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    let head = playhead.get();
    if !(start < head && head + 1 < end) {
        return false;
    }
    let eff = effect_frame_at_playhead(clip, playhead);
    !clip.visible_midpoints().contains(&eff)
}

fn can_remove_midpoint_near(clip: &Clip, playhead: FrameIndex) -> bool {
    if clip.visible_midpoints().is_empty() {
        return false;
    }
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    let head = playhead.get();
    start <= head && head < end
}

fn select_segment_after_midpoint_change(win: &mut SeaEditorWindow, clip_id: &ClipId, added: bool) {
    let head = win.controller.clock.current_frame();
    if let Some((_, clip)) = win.controller.document.find_clip(clip_id) {
        let grid = clip.grid_frames();
        let eff = effect_frame_at_playhead(clip, head);
        let seg_count = grid.len().saturating_sub(1).max(1);
        let target = if added {
            grid.windows(2)
                .position(|w| eff >= w[0] && eff < w[1])
                .or_else(|| grid.iter().position(|&g| g == eff))
                .unwrap_or(0)
                .min(seg_count - 1)
        } else {
            win.controller
                .selected_segment_index
                .unwrap_or(0)
                .min(seg_count - 1)
        };
        win.controller.select_clip_segment(clip_id, target);
    }
}
