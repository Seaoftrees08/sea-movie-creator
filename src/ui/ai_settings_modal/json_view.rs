//! AI 設定モーダル: `{ } JSON` 直接編集ビュー描画。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Editor;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, div, px, rgba,
};

use crate::ui::ai_chat_panel::AiChatState;
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

use super::AiSettingsModalState;

pub(super) fn render_json_mode_view(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let path_str = chat.settings_path.as_ref().map_or_else(
        || "settings/ai_settings.json (未決定)".to_owned(),
        |p| p.display().to_string(),
    );
    let path_label = tr("ai.settings_json_file_label").replace("{path}", &path_str);

    let mut col = v_flex().w_full().gap_2().child(
        div()
            .text_xs()
            .text_color(rgba(0x94a3_b8ff))
            .child(path_label),
    );

    if let Some(warn) = &chat.settings_load_warning {
        col = col.child(
            div()
                .p_2()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgba(0xf59e_0bff))
                .text_xs()
                .text_color(rgba(0xfde6_8aff))
                .child(warn.clone()),
        );
    }

    let theme = cx.theme();
    col.child(
        div()
            .w_full()
            .h(px(360.))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(
                Editor::new(&modal.json_editor)
                    .h(px(360.))
                    .appearance(false),
            ),
    )
    .child(
        h_flex()
            .w_full()
            .justify_between()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Button::new("ai-json-format")
                            .xsmall()
                            .outline()
                            .label(tr("ai.settings_json_format"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.format_modal_json(window, cx);
                            })),
                    )
                    .child(
                        Button::new("ai-json-reload")
                            .xsmall()
                            .ghost()
                            .label(tr("ai.settings_json_reload"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reload_modal_json(window, cx);
                            })),
                    ),
            )
            .child(
                Button::new("ai-json-save")
                    .xsmall()
                    .primary()
                    .label(tr("ai.settings_json_save"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_modal_json(window, cx);
                    })),
            ),
    )
    .into_any_element()
}
