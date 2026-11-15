//! 中央プレビューパネル: BGRA `RenderImage` 表示、再生バー、「Media Offline」警告表示。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Context, Entity, IntoElement, ObjectFit, ParentElement as _, Styled as _, StyledImage as _,
    Window, div, img, px, rgba,
};

use crate::ui::app_controller::{AppController, PlaybackState};
use crate::ui::window::SeaEditorWindow;

/// プレビューキャンバス上部の情報バー・映像表示・トランスポート操作を描画する。
pub fn render_preview_panel(
    controller: &mut AppController,
    scrub_slider: &Entity<SliderState>,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    for stale_img in controller.surface_bridge.drain_pending_drops() {
        let _ = window.drop_image(stale_img);
    }

    let playhead = controller.clock.current_frame().get();
    let total = controller.document.total_duration_frames().max(1);
    let fps = controller.document.fps;
    let res = controller.document.resolution;
    let play_label = match controller.playback_state {
        PlaybackState::Playing => "Pause",
        PlaybackState::Stopped | PlaybackState::Paused | PlaybackState::Seeking => "Play",
    };
    let is_offline = controller.has_media_offline_at_playhead;
    let front_image = controller.surface_bridge.front_image();
    let theme = cx.theme();

    v_flex()
        .size_full()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .child(render_preview_header(
            res.width.get(),
            res.height.get(),
            fps.as_f64(),
            playhead,
            total,
            cx,
        ))
        .child(render_preview_viewport(front_image, is_offline))
        .child(render_transport_bar(
            play_label,
            playhead,
            total,
            scrub_slider,
            cx,
        ))
}

fn render_preview_header(
    width: u32,
    height: u32,
    fps: f64,
    playhead: u64,
    total: u64,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .h(px(30.))
        .px_3()
        .justify_between()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(format!("Preview  {width}x{height}  {fps:.2}fps"))
        .child(format!("Frame {playhead} / {total}"))
}

fn render_preview_viewport(
    front_image: Option<std::sync::Arc<gpui_kit::RenderImage>>,
    is_offline: bool,
) -> impl IntoElement {
    div()
        .flex_1()
        .w_full()
        .min_h(px(140.))
        .relative()
        .bg(rgba(0x0b0d_11ff))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .children(
            front_image.map(|arc_img| img(arc_img).size_full().object_fit(ObjectFit::Contain)),
        )
        .children(is_offline.then(render_media_offline_overlay))
}

fn render_media_offline_overlay() -> impl IntoElement {
    div()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgba(0xb91c_1ce6))
        .border_1()
        .border_color(rgba(0xfca5_a5ff))
        .text_color(rgba(0xffff_ffff))
        .text_sm()
        .child("Media Offline — 対象ファイルが見つかりません（再リンクしてください）")
}

fn render_transport_bar(
    play_label: &'static str,
    playhead: u64,
    total: u64,
    scrub_slider: &Entity<SliderState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .h(px(44.))
        .px_3()
        .gap_2()
        .items_center()
        .border_t_1()
        .border_color(theme.border)
        .child(
            Button::new("transport-start")
                .ghost()
                .small()
                .label("|<")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller
                        .seek_to_frame(crate::core::time::FrameIndex::ZERO);
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        )
        .child(
            Button::new("transport-play-pause")
                .primary()
                .small()
                .label(play_label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.toggle_play_pause();
                    cx.notify();
                })),
        )
        .child(
            Button::new("transport-stop")
                .ghost()
                .small()
                .label("Stop")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.stop();
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        )
        .child(div().flex_1().px_2().child(Slider::new(scrub_slider)))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("{playhead}f / {total}f")),
        )
        .child(
            Button::new("transport-split")
                .ghost()
                .small()
                .label("Split")
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.controller.split_selected_clip_at_playhead();
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        )
}
