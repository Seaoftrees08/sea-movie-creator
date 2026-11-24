//! AI 設定モーダル: 「モデル管理」タブ描画（カスタムモデル管理 & CLI 検出モデル表示）。

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, Entity, IntoElement as _, ParentElement as _, Styled as _, div, px, rgba,
};

use crate::ai::provider::AiProviderKind;
use crate::ai::settings::ResolvedAiModel;
use crate::ui::ai_chat_panel::AiChatState;
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

use super::{AiSettingsModalState, format_model_meta_line, render_provider_pills};

pub(super) fn render_models_tab(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let kind = &modal.selected_provider;
    let body = if kind.supports_custom_models() {
        render_custom_models_panel(modal, chat, cx)
    } else {
        render_plugin_models_panel(kind, chat, cx)
    };

    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2p5()
        .child(render_provider_pills(kind, "ai-mdl-prov", cx))
        .child(body)
        .into_any_element()
}

fn render_custom_models_panel(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let kind = &modal.selected_provider;
    let models = chat.resolved_models_for_provider(kind);
    let mut list = v_flex().w_full().gap_1();
    for (idx, model) in models.into_iter().enumerate() {
        list = list.child(render_custom_model_row(kind, &model, idx, chat, cx));
    }

    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2p5()
        .child(render_custom_model_add_form(modal, kind, cx))
        .child(list)
        .into_any_element()
}

fn render_custom_model_row(
    kind: &AiProviderKind,
    model: &ResolvedAiModel,
    idx: usize,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let is_active = chat.selected_provider == *kind && chat.model_name == model.id;
    let model_id = model.id.clone();
    let meta = format_model_meta_line(
        model.context_window_tokens.as_ref(),
        model.input_price_per_1m_usd,
        model.output_price_per_1m_usd,
    );
    let custom_mark = if model.is_custom { " [Custom]" } else { "" };

    let select_id = model_id.clone();
    let switch_kind = kind.clone();
    let use_label = if is_active {
        tr("ai.settings_selected_model")
    } else {
        tr("ai.settings_use_model")
    };
    let mut actions = h_flex().flex_shrink_0().items_center().gap_1().child(
        Button::new(("ai-mdl-use", idx))
            .xsmall()
            .label(use_label)
            .disabled(is_active)
            .on_click(cx.listener(move |this, _, _, cx| {
                let switched = this.controller.ai_chat.selected_provider != switch_kind;
                if switched {
                    this.controller.ai_chat.select_provider(switch_kind.clone());
                }
                this.controller
                    .ai_chat
                    .select_model_default_medium(&select_id);
                if switched {
                    this.controller.ai_chat.refresh_active_provider_side_data();
                }
                this.sync_widgets_from_controller(cx);
                cx.notify();
            })),
    );
    if model.is_custom {
        let del_kind = kind.clone();
        actions = actions.child(
            Button::new(("ai-mdl-del", idx))
                .xsmall()
                .danger()
                .label(tr("ai.settings_delete_model"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.delete_modal_custom_model(&del_kind, &model_id, cx);
                })),
        );
    }

    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .bg(rgba(0x0f17_2a99))
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(rgba(0xf1f5_f9ff))
                        .child(format!("{}{}", model.display_name, custom_mark)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgba(0x94a3_b8ff))
                        .child(format!("{}  {meta}", model.id)),
                ),
        )
        .child(actions)
        .into_any_element()
}

fn render_flex_input_cell(input: &Entity<InputState>) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.))
        .child(Input::new(input).small())
        .into_any_element()
}

fn render_custom_model_add_form(
    modal: &AiSettingsModalState,
    kind: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let heading = tr("ai.settings_add_model_heading").replace("{provider}", &kind.ui_combo_label());
    let mut thinking_btn = Button::new("ai-mdl-thinking-toggle").xsmall().label(
        if modal.custom_model_supports_thinking {
            tr("ai.settings_thinking_toggle_on")
        } else {
            tr("ai.settings_thinking_toggle_off")
        },
    );
    thinking_btn = if modal.custom_model_supports_thinking {
        thinking_btn.primary()
    } else {
        thinking_btn.outline()
    };

    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_1p5()
        .p_2p5()
        .rounded(px(6.0))
        .bg(rgba(0x0f17_2a99))
        .border_1()
        .border_color(rgba(0x3341_55ff))
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(rgba(0xcbd5_e1ff))
                .child(heading),
        )
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .gap_1p5()
                .child(render_flex_input_cell(&modal.model_id_input))
                .child(render_flex_input_cell(&modal.model_name_input)),
        )
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .gap_1p5()
                .child(render_flex_input_cell(&modal.model_context_input))
                .child(render_flex_input_cell(&modal.model_in_price_input))
                .child(render_flex_input_cell(&modal.model_out_price_input)),
        )
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .gap_1p5()
                .flex_wrap()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgba(0x94a3_b8ff))
                        .child(tr("ai.settings_add_model_hint")),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(thinking_btn.on_click(cx.listener(|this, _, _, cx| {
                            if let Some(m) = &mut this.ai_settings_modal {
                                m.custom_model_supports_thinking =
                                    !m.custom_model_supports_thinking;
                            }
                            cx.notify();
                        })))
                        .child(
                            Button::new("ai-mdl-add-btn")
                                .xsmall()
                                .primary()
                                .label(tr("ai.settings_add_model_btn"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.upsert_modal_custom_model(window, cx);
                                })),
                        ),
                ),
        )
        .into_any_element()
}

fn render_plugin_models_panel(
    kind: &AiProviderKind,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let plugin_id = match kind {
        AiProviderKind::Plugin { plugin_id } => plugin_id.clone(),
        _ => String::new(),
    };
    let probing = chat
        .plugin_models_probing
        .get(&plugin_id)
        .copied()
        .unwrap_or(false);
    let note = chat.plugin_models_note.get(&plugin_id).cloned();
    let models = chat.resolved_models_for_provider(kind);

    let probe_id = plugin_id.clone();
    let rescan_label = if probing {
        tr("ai.settings_probing")
    } else {
        tr("ai.settings_rescan")
    };
    let mut panel = v_flex().w_full().min_w(px(0.)).gap_2().child(
        h_flex()
            .w_full()
            .min_w(px(0.))
            .items_center()
            .justify_between()
            .gap_2()
            .p_2()
            .rounded(px(6.0))
            .bg(rgba(0x0f17_2a99))
            .border_1()
            .border_color(rgba(0x3341_55ff))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(rgba(0xcbd5_e1ff))
                    .child(tr("ai.settings_cli_models_notice")),
            )
            .child(
                div().flex_shrink_0().child(
                    Button::new("ai-mdl-rescan")
                        .xsmall()
                        .outline()
                        .disabled(probing || plugin_id.is_empty())
                        .label(rescan_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.controller.ai_chat.probe_plugin_models_async(&probe_id);
                            cx.notify();
                        })),
                ),
            ),
    );

    if let Some(note_text) = note {
        panel = panel.child(
            div()
                .text_xs()
                .text_color(rgba(0x94a3_b8ff))
                .child(note_text),
        );
    }

    for (idx, model) in models.into_iter().enumerate() {
        panel = panel.child(render_plugin_model_row(kind, &model, idx, chat, cx));
    }
    panel.into_any_element()
}

fn render_plugin_model_row(
    kind: &AiProviderKind,
    model: &ResolvedAiModel,
    idx: usize,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let model_id = model.id.clone();
    let switch_kind = kind.clone();
    let is_active = chat.selected_provider == *kind && chat.model_name == model.id;
    let status = model.availability_note.clone().unwrap_or_else(|| {
        if model.available {
            "利用可能".to_owned()
        } else {
            "未検出".to_owned()
        }
    });
    let use_label = if is_active {
        tr("ai.settings_selected_model")
    } else {
        tr("ai.settings_use_model")
    };
    h_flex()
        .w_full()
        .min_w(px(0.))
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .bg(rgba(0x0f17_2a99))
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(if model.available {
                            rgba(0xf1f5_f9ff)
                        } else {
                            rgba(0x94a3_b8ff)
                        })
                        .child(format!("{} ({})", model.display_name, model.id)),
                )
                .child(div().text_xs().text_color(rgba(0x94a3_b8ff)).child(status)),
        )
        .child(
            div().flex_shrink_0().child(
                Button::new(("ai-cli-mdl-use", idx))
                    .xsmall()
                    .disabled(!model.available || is_active)
                    .label(use_label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let switched = this.controller.ai_chat.selected_provider != switch_kind;
                        if switched {
                            this.controller.ai_chat.select_provider(switch_kind.clone());
                        }
                        this.controller
                            .ai_chat
                            .select_model_default_medium(&model_id);
                        if switched {
                            this.controller.ai_chat.refresh_active_provider_side_data();
                        }
                        this.sync_widgets_from_controller(cx);
                        cx.notify();
                    })),
            ),
        )
        .into_any_element()
}
