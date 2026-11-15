//! 下部タイムラインパネル: 仮想トラック一覧・クリップ選択・再生ヘッドシーク。

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Size, Styled as _, div, px, rgba, size,
};

use crate::core::time::FrameIndex;
use crate::timeline::document::{AssetStatus, Clip, ClipKind};
use crate::ui::app_controller::AppController;
use crate::ui::window::SeaEditorWindow;

const TRACK_HEADER_WIDTH: f32 = 136.0;
const TRACK_ROW_HEIGHT: f32 = 44.0;

/// 下部タイムライン領域（ルーラー＋仮想スクロールトラック＋再生ヘッド）を描画する。
pub fn render_timeline_panel(
    controller: &AppController,
    scroll_handle: &VirtualListScrollHandle,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let layer_count = controller.document.layers.len();
    let item_sizes: Rc<Vec<Size<Pixels>>> =
        Rc::new(vec![size(px(1000.), px(TRACK_ROW_HEIGHT)); layer_count]);
    let view_entity = cx.entity();

    v_flex()
        .size_full()
        .bg(theme.background)
        .border_t_1()
        .border_color(theme.border)
        .child(render_timeline_ruler(controller, cx))
        .child(render_track_area(
            layer_count,
            view_entity,
            scroll_handle,
            item_sizes,
            cx,
        ))
}

fn render_timeline_ruler(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let playhead = controller.clock.current_frame().get();
    let px_per_frame = controller.timeline_px_per_frame.max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let head_x = (playhead as f32) * px_per_frame;

    h_flex()
        .h(px(28.))
        .w_full()
        .border_b_1()
        .border_color(theme.border)
        .items_center()
        .child(
            div()
                .w(px(TRACK_HEADER_WIDTH))
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .border_r_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Timeline"),
        )
        .child(
            div()
                .id("timeline-ruler-seek")
                .flex_1()
                .h_full()
                .relative()
                .bg(rgba(0x1619_20ff))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        let raw_x = f32::from(evt.position.x) - TRACK_HEADER_WIDTH;
                        let scale = this.controller.timeline_px_per_frame.max(1.0);
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let target = (raw_x / scale).max(0.0).round() as u64;
                        this.controller.seek_to_frame(FrameIndex::new(target));
                        this.mark_sliders_dirty();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(head_x))
                        .w(px(2.))
                        .bg(rgba(0x38bd_f8ff)),
                ),
        )
}

fn render_track_area(
    layer_count: usize,
    view_entity: gpui_kit::Entity<SeaEditorWindow>,
    scroll_handle: &VirtualListScrollHandle,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let theme = cx.theme();
    if layer_count == 0 {
        return div()
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("タイムラインは空です。「ファイル > メディアを読み込む...」から素材を追加できます。")
            .into_any_element();
    }

    div()
        .flex_1()
        .w_full()
        .overflow_hidden()
        .child(
            v_virtual_list(
                view_entity,
                "timeline-track-list",
                item_sizes,
                |view, visible_range, _window, cx| {
                    visible_range
                        .filter_map(|ix| render_track_row(&view.controller, ix, cx))
                        .collect()
                },
            )
            .track_scroll(scroll_handle),
        )
        .into_any_element()
}

fn render_track_row(
    controller: &AppController,
    layer_ix: usize,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<AnyElement> {
    let layer = controller.document.layers.get(layer_ix)?;
    let border_color = cx.theme().border;
    let fg_color = cx.theme().foreground;
    let px_per_frame = controller.timeline_px_per_frame.max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let head_x = (controller.clock.current_frame().get() as f32) * px_per_frame;

    let clip_views: Vec<AnyElement> = layer
        .clips
        .iter()
        .enumerate()
        .map(|(clip_ix, clip)| {
            render_clip_block(controller, &layer.id, clip, clip_ix, px_per_frame, cx)
        })
        .collect();

    Some(
        h_flex()
            .h(px(TRACK_ROW_HEIGHT))
            .w_full()
            .border_b_1()
            .border_color(border_color)
            .child(
                div()
                    .w(px(TRACK_HEADER_WIDTH))
                    .h_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(border_color)
                    .text_xs()
                    .text_color(fg_color)
                    .child(if layer.muted {
                        format!("{} [M]", layer.name)
                    } else {
                        layer.name.clone()
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .children(clip_views)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(head_x))
                            .w(px(2.))
                            .bg(rgba(0x38bd_f8ff)),
                    ),
            )
            .into_any_element(),
    )
}

#[allow(clippy::cast_precision_loss)]
fn render_clip_block(
    controller: &AppController,
    layer_id: &crate::core::ids::LayerId,
    clip: &Clip,
    clip_ix: usize,
    px_per_frame: f32,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let left_px = (clip.timeline_start.get() as f32) * px_per_frame;
    let width_px = ((clip.duration.get() as f32) * px_per_frame).max(14.0);
    let is_selected = controller.selected_clip_id.as_ref() == Some(&clip.id);
    let is_missing = match &clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => controller
            .document
            .find_asset(asset_id)
            .is_some_and(|a| matches!(a.status, AssetStatus::Missing { .. })),
        ClipKind::Subtitle(_) => false,
    };

    let bg = if is_missing {
        rgba(0x991b_1bff)
    } else if is_selected {
        rgba(0x2563_ebff)
    } else {
        match &clip.kind {
            ClipKind::Video { .. } => rgba(0x1d4e_d8cc),
            ClipKind::Audio { .. } => rgba(0x0596_69cc),
            ClipKind::Image { .. } => rgba(0x7c3a_edcc),
            ClipKind::Subtitle(_) => rgba(0xd977_06cc),
        }
    };
    let border_col = if is_selected {
        rgba(0xfacc_15ff)
    } else {
        rgba(0x4755_69ff)
    };
    let lid = layer_id.clone();
    let cid = clip.id.clone();

    div()
        .id((
            "timeline-clip",
            (clip_ix as u64) + clip.timeline_start.get(),
        ))
        .absolute()
        .top(px(4.))
        .bottom(px(4.))
        .left(px(left_px))
        .w(px(width_px))
        .px_1()
        .rounded_sm()
        .bg(bg)
        .border_1()
        .border_color(border_col)
        .overflow_hidden()
        .flex()
        .items_center()
        .text_xs()
        .text_color(rgba(0xf8fa_fcff))
        .child(clip.name.clone())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.controller.selected_layer_id = Some(lid.clone());
                this.controller.selected_clip_id = Some(cid.clone());
                this.mark_sliders_dirty();
                cx.notify();
            }),
        )
        .into_any_element()
}
