//! 右側インスペクタ・タブコンテナ（クリップ設定 / AIチャット / レイヤー / プロジェクト）。

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::SelectState;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    Context, Entity, FocusHandle, IntoElement, ParentElement as _, ScrollHandle, Styled as _, div,
    px, rgba,
};

use crate::timeline::document::TimelineLengthMode;
use crate::ui::ai_chat_panel::{AiChatPanelWidgets, AiProviderSelectItem, render_ai_chat_panel};
use crate::ui::app_controller::{AppController, RightPanelTab};
use crate::ui::i18n::{
    active_language_code, available_languages, primary_lang_dir, reload_languages,
    set_active_language, tr, tr_fmt,
};
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
    pub ai_chat_input: &'a Entity<TextareaState>,
    /// エージェント質問カード用の動的入力欄一覧。
    pub ai_ask_inputs: &'a [(String, Entity<InputState>)],
    /// チャット履歴コンテナ用のフォーカスハンドル。
    pub ai_chat_history_focus: &'a FocusHandle,
    /// チャット履歴の明示スクロールハンドル。
    pub ai_chat_scroll_handle: &'a ScrollHandle,
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
        0 => tr("tab.clip_none"),
        1 => tr("tab.clip"),
        n => {
            let n_str = n.to_string();
            tr_fmt("tab.clip_count", &[("n", &n_str)])
        }
    };

    let tabs = [
        (clip_tab_label, RightPanelTab::ClipInspector),
        (tr("tab.ai"), RightPanelTab::AiChat),
        (tr("tab.layers"), RightPanelTab::LayerList),
        (tr("tab.project"), RightPanelTab::ProjectInfo),
    ];

    let tab_bar = h_flex()
        .gap_1()
        .p_1p5()
        .border_b_1()
        .border_color(theme.border)
        .flex_wrap()
        .children(tabs.into_iter().enumerate().map(|(ix, (label, tab))| {
            let mut btn = Button::new(("right-tab-btn", ix)).small().label(label);
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
            .overflow_hidden()
            .child(render_ai_chat_panel(
                controller,
                AiChatPanelWidgets {
                    ai_provider_select: widgets.ai_provider_select,
                    ai_chat_input: widgets.ai_chat_input,
                    ai_ask_inputs: widgets.ai_ask_inputs,
                    ai_chat_history_focus: widgets.ai_chat_history_focus,
                    ai_chat_scroll_handle: widgets.ai_chat_scroll_handle,
                },
                cx,
            ))
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
                            "T{} {} ({})",
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
    let total_str = total_f.to_string();
    let length_desc = match doc.timeline_length {
        TimelineLengthMode::AutoToLastClip => tr_fmt("timeline.len_auto", &[("total", &total_str)]),
        TimelineLengthMode::Manual { end_frame } => {
            let end_s = end_frame.get().to_string();
            tr_fmt("timeline.len_manual", &[("total", &end_s)])
        }
    };
    let w_str = res.width.to_string();
    let h_str = res.height.to_string();
    let fps_str = format!("{}", doc.fps.as_f64());
    let tracks_str = doc.layers.len().to_string();
    let assets_str = doc.assets.len().to_string();

    v_flex()
        .gap_2()
        .w_full()
        .child(
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
                        .child(tr_fmt(
                            "project.resolution",
                            &[("w", &w_str), ("h", &h_str), ("fps", &fps_str)],
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr_fmt(
                            "project.summary",
                            &[
                                ("tracks", &tracks_str),
                                ("assets", &assets_str),
                                ("len", &length_desc),
                            ],
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
                                .label(tr("project.len_auto_btn"))
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
                                .label(tr("project.len_manual_btn"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_timeline_length_modal(window, cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr_fmt(
                            "project.status",
                            &[("status", &controller.status_message)],
                        )),
                ),
        )
        .child(render_language_card(theme, cx))
}

fn render_language_card(theme: &Theme, cx: &mut Context<SeaEditorWindow>) -> impl IntoElement {
    let active_lang = active_language_code();
    let languages = available_languages();
    let lang_dir_text = primary_lang_dir().display().to_string();

    v_flex()
        .gap_1p5()
        .p_2()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .child(tr("project.language_title")),
                )
                .child(
                    Button::new("proj-lang-reload-btn")
                        .ghost()
                        .xsmall()
                        .label(tr("project.language_reload"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let n = reload_languages();
                            this.controller.status_message =
                                format!("lang/ を再読み込みしました ({n} 言語)");
                            this.reload_app_menus(cx);
                            cx.notify();
                        })),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(languages.into_iter().enumerate().map(|(ix, lang)| {
                    let is_active = lang.code == active_lang;
                    let code = lang.code.clone();
                    let mut btn = Button::new(("proj-lang-btn", ix))
                        .small()
                        .label(format!("{} ({})", lang.name, lang.code));
                    btn = if is_active {
                        btn.primary()
                    } else {
                        btn.ghost()
                    };
                    btn.on_click(cx.listener(move |this, _, _, cx| {
                        if let Ok(info) = set_active_language(&code) {
                            this.controller.status_message =
                                format!("Language: {} ({})", info.name, info.code);
                            this.reload_app_menus(cx);
                        }
                        cx.notify();
                    }))
                })),
        )
        .child(
            div()
                .text_size(gpui_kit::px(10.0))
                .text_color(theme.muted_foreground)
                .child(format!("{}: {lang_dir_text}", tr("project.language_hint"))),
        )
}
