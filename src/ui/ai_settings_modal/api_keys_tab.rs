//! AI 設定モーダル: 「API キー・接続」タブ描画。

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, div, px, rgba,
};

use crate::ai::provider::AiProviderKind;
use crate::ai::providers::credentials::CredentialSource;
use crate::ui::ai_chat_panel::AiChatState;
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

use super::{
    AiSettingsModalState, format_credential_source_badge, is_http_configurable_provider,
    render_provider_pills,
};

pub(super) fn render_api_keys_tab(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let kind = &modal.selected_provider;
    let source = chat.cached_credential_source_for(kind);
    let source_text = format_credential_source_badge(kind, &source);

    let mut panel = v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2p5()
        .child(render_provider_pills(kind, "ai-cfg-prov", cx))
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .items_center()
                .justify_between()
                .gap_2()
                .flex_wrap()
                .p_2()
                .rounded(px(6.0))
                .bg(rgba(0x0f17_2aff))
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(rgba(0xe2e8_f0ff))
                        .child(kind.display_label()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(
                            if source == CredentialSource::Missing && kind.requires_api_key() {
                                rgba(0xf871_71ff)
                            } else {
                                rgba(0x94a3_b8ff)
                            },
                        )
                        .child(source_text),
                ),
        );

    if let CredentialSource::EnvVar(env_var) = source {
        let notice = tr("ai.env_override_notice").replace("{env}", env_var);
        panel = panel.child(
            div()
                .p_2()
                .rounded(px(6.0))
                .border_1()
                .border_color(rgba(0xf59e_0bff))
                .text_xs()
                .text_color(rgba(0xfde6_8aff))
                .child(notice),
        );
    }

    if kind.requires_api_key() {
        panel = panel.child(render_api_key_controls(modal, kind, cx));
    } else if matches!(kind, AiProviderKind::Plugin { .. }) {
        panel = panel.child(render_cli_auth_note(kind));
    }

    if is_http_configurable_provider(kind) {
        panel = panel.child(render_base_url_controls(modal, kind, cx));
    }
    panel.into_any_element()
}

fn render_api_key_controls(
    modal: &AiSettingsModalState,
    kind: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let has_keyring = kind.keyring_account().is_some();
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
                .child(tr("ai.settings_api_key_heading")),
        )
        .child(Input::new(&modal.api_key_input).small().mask_toggle())
        .child(
            h_flex()
                .gap_1p5()
                .flex_wrap()
                .child(
                    Button::new("ai-cfg-save-keyring")
                        .xsmall()
                        .primary()
                        .disabled(!has_keyring)
                        .label(tr("ai.settings_save_keyring"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_key_to_keyring(window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-save-json")
                        .xsmall()
                        .outline()
                        .label(tr("ai.settings_save_json"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_key_to_json(window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-clear-key")
                        .xsmall()
                        .ghost()
                        .label(tr("ai.settings_clear_key"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_modal_credentials(window, cx);
                        })),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgba(0x94a3_b8ff))
                .child(tr("ai.settings_priority_hint")),
        )
        .into_any_element()
}

fn render_cli_auth_note(kind: &AiProviderKind) -> AnyElement {
    let detail = match kind {
        AiProviderKind::Plugin { plugin_id } if plugin_id == "sea.ai.antigravity" => {
            "Antigravity (`agy CLI`) は `agy` 側のログイン済みアカウントをそのまま使用します。連続呼び出し上限対策として「エージェント・予算」タブで呼出間隔 1500ms 以上を推奨します。"
        }
        _ => "ACP CLI は各ツール (`claude` / `codex` / `copilot`) の既存ログイン状態を使用します。",
    };
    div()
        .w_full()
        .min_w(px(0.))
        .p_2p5()
        .rounded(px(6.0))
        .bg(rgba(0x0f17_2a99))
        .border_1()
        .border_color(rgba(0x3341_55ff))
        .text_xs()
        .text_color(rgba(0xcbd5_e1ff))
        .child(detail)
        .into_any_element()
}

fn render_base_url_controls(
    modal: &AiSettingsModalState,
    kind: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let heading = tr("ai.settings_base_url_heading").replace("{url}", kind.default_base_url());
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
        .child(Input::new(&modal.base_url_input).small())
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Button::new("ai-cfg-save-url")
                        .xsmall()
                        .primary()
                        .label(tr("ai.settings_save_url"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_base_url(false, window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-reset-url")
                        .xsmall()
                        .ghost()
                        .label(tr("ai.settings_reset_url"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_base_url(true, window, cx);
                        })),
                ),
        )
        .into_any_element()
}
