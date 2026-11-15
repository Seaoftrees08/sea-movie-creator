//! 右側インスペクタ・タブコンテナ（クリップ設定 / AIチャット / レイヤー / プロジェクト）。

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::SelectState;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{Context, Entity, IntoElement, ParentElement as _, Styled as _, div, px, rgba};

use crate::timeline::document::TimelineLengthMode;
use crate::ui::ai_chat_panel::{AiProviderSelectItem, render_ai_chat_panel};
use crate::ui::app_controller::{AppController, RightPanelTab};
use crate::ui::inspector_panel::{InspectorWidgetBank, InspectorWidgets, render_clip_inspector};
use crate::ui::window::SeaEditorWindow;

/// 右パネル描画に必要な外部入力ウィジェットへの参照束。
#[derive(Clone, Copy)]
pub struct RightPanelWidgets<'a> {
    /// インスペクタ全体の入力・カラーピッカー保持バンク。
    pub inspector_bank: &'a InspectorWidgetBank,
    /// AI プロバイダ選択コンボボックス状態。
    pub ai_provider_select: &'a Entity<SelectState<Vec<AiProviderSelectItem>>>,
    /// AI チャット入力ボックス状態。
    pub ai_chat_input: &'a Entity<InputState>,
}

/// 4 タブ（クリップ設定 / AI / レイヤー / プロジェクト情報）を切り替える右側パネルを構築する。
pub fn render_right_panel(
    controller: &AppController,
    widgets: RightPanelWidgets<'_>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let selected_count = controller.effective_selected_clip_ids().len();
    let clip_tab_label = match selected_count {
        0 => "クリップ (選択なし)".to_owned(),
        1 => "クリップ".to_owned(),
        n => format!("クリップ ({n}個)"),
    };

    let tabs = [
        (clip_tab_label.as_str(), RightPanelTab::ClipInspector),
        ("AIチャット", RightPanelTab::AiChat),
        ("レイヤー", RightPanelTab::LayerList),
        ("プロジェクト", RightPanelTab::ProjectInfo),
    ];

    let tab_bar = h_flex()
        .gap_1()
        .p_1p5()
        .border_b_1()
        .border_color(theme.border)
        .flex_wrap()
        .children(tabs.into_iter().enumerate().map(|(ix, (label, tab))| {
            let mut btn = Button::new(("right-tab-btn", ix))
                .small()
                .label(label.to_owned());
            btn = if controller.right_tab == tab {
                btn.primary()
            } else {
                btn.ghost()
            };
            btn.on_click(cx.listener(move |this, _, window, cx| {
                this.controller.right_tab = tab;
                if tab == RightPanelTab::ClipInspector {
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                }
                cx.notify();
            }))
        }));

    let body = match controller.right_tab {
        RightPanelTab::ClipInspector => div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .child(div().p_2().child(render_clip_inspector(
                controller,
                InspectorWidgets {
                    bank: widgets.inspector_bank,
                },
                cx,
            )))
            .into_any_element(),
        RightPanelTab::AiChat => div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .p_2()
            .child(render_ai_chat_tab(controller, &widgets, &theme, cx))
            .into_any_element(),
        RightPanelTab::LayerList => div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .p_2()
            .child(render_layers_tab(controller, &theme, cx))
            .into_any_element(),
        RightPanelTab::ProjectInfo => div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .p_2()
            .child(render_project_tab(controller, &theme, cx))
            .into_any_element(),
    };

    v_flex()
        .size_full()
        .bg(theme.background)
        .border_l_1()
        .border_color(theme.border)
        .child(tab_bar)
        .child(body)
}

fn render_ai_chat_tab(
    controller: &AppController,
    widgets: &RightPanelWidgets<'_>,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut root = v_flex()
        .gap_2()
        .w_full()
        .child(render_ai_chat_panel(
            controller,
            widgets.ai_provider_select,
            widgets.ai_chat_input,
            cx,
        ))
        .child(
            h_flex()
                .gap_1()
                .child(div().flex_1().child(Input::new(widgets.ai_chat_input)))
                .child(
                    Button::new("ai-chat-send-btn")
                        .primary()
                        .small()
                        .disabled(controller.ai_chat.is_busy())
                        .label("送信")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_ai_chat_input(window, cx);
                        })),
                ),
        );

    if let Some((_, clip)) = controller.selected_clip()
        && let crate::timeline::document::ClipKind::Video { playback_rate, .. }
        | crate::timeline::document::ClipKind::Audio { playback_rate, .. } = &clip.kind
    {
        let rate = *playback_rate;
        root = root.child(
            h_flex()
                .items_center()
                .gap_1()
                .flex_wrap()
                .p_1p5()
                .rounded_sm()
                .bg(theme.secondary)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("選択クリップ速度: {rate:?}")),
                )
                .child(speed_button("0.5x", 1, 2, cx))
                .child(speed_button("1.0x", 1, 1, cx))
                .child(speed_button("2.0x", 2, 1, cx)),
        );
    }
    root
}

fn speed_button(
    label: &'static str,
    num: u32,
    den: u32,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    Button::new(("right-speed", num * 10 + den))
        .ghost()
        .small()
        .label(label)
        .on_click(cx.listener(move |this, _, _, cx| {
            let _ = this.controller.set_selected_clip_playback_rate(num, den);
            cx.notify();
        }))
}

fn render_layers_tab(
    controller: &AppController,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut list = v_flex().gap_1p5().w_full();
    for (idx, layer) in controller.document.layers.iter().enumerate() {
        let lid_sel = layer.id.clone();
        let lid_vis = layer.id.clone();
        let lid_mute = layer.id.clone();
        let lid_lock = layer.id.clone();
        let is_selected = controller.selected_layer_id.as_ref() == Some(&layer.id);

        list = list.child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_1()
                .p_1p5()
                .rounded_sm()
                .bg(if is_selected {
                    rgba(0x1d4e_d833)
                } else {
                    theme.secondary.into()
                })
                .border_1()
                .border_color(if is_selected {
                    rgba(0x3b82_f6ff)
                } else {
                    theme.border.into()
                })
                .child(
                    Button::new(("layer-tab-sel", idx))
                        .ghost()
                        .small()
                        .label(format!(
                            "L{} {} ({})",
                            idx + 1,
                            layer.name,
                            layer.clips.len()
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.controller.selected_layer_id = Some(lid_sel.clone());
                            cx.notify();
                        })),
                )
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            Button::new(("layer-vis-btn", idx))
                                .ghost()
                                .small()
                                .label(if layer.visible { "👁" } else { "×" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = this.controller.toggle_layer_visible(&lid_vis);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("layer-mute-btn", idx))
                                .ghost()
                                .small()
                                .label(if layer.muted { "🔇" } else { "🔊" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = this.controller.toggle_layer_muted(&lid_mute);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("layer-lock-btn", idx))
                                .ghost()
                                .small()
                                .label(if layer.locked { "🔒" } else { "🔓" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = this.controller.toggle_layer_locked(&lid_lock);
                                    cx.notify();
                                })),
                        ),
                ),
        );
    }
    list
}

fn render_project_tab(
    controller: &AppController,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let doc = &controller.document;
    let res = doc.resolution;
    let total_f = doc.total_duration_frames();
    let length_desc = match doc.timeline_length {
        TimelineLengthMode::AutoToLastClip => format!("自動 ({total_f}f)"),
        TimelineLengthMode::Manual { end_frame } => format!("手動 ({}f)", end_frame.get()),
    };

    v_flex()
        .gap_2()
        .p_2()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(div().text_sm().font_semibold().child(doc.name.clone()))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "解像度: {}x{} @ {}fps",
                    res.width,
                    res.height,
                    doc.fps.as_f64()
                )),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "レイヤー: {} / 素材: {} / 総尺: {length_desc}",
                    doc.layers.len(),
                    doc.assets.len(),
                )),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .child(
                    Button::new("proj-auto-len-btn")
                        .ghost()
                        .small()
                        .label("総尺を自動へ")
                        .on_click(cx.listener(|this, _, _, cx| {
                            let _ = this
                                .controller
                                .set_timeline_length(TimelineLengthMode::AutoToLastClip);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("proj-manual-len-btn")
                        .ghost()
                        .small()
                        .label("総尺を設定...")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_timeline_length_modal(window, cx);
                        })),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("状態: {}", controller.status_message)),
        )
}
