//! AI 設定モーダル: 「エージェント・予算」タブ描画。

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, div, px, rgba,
};

use crate::ai::provider::{AgentRuntimeSettings, VisionPackingMode};
use crate::ai::settings::UsageDisplayPreference;
use crate::ui::ai_chat_panel::AiChatState;
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

use super::AiSettingsModalState;

pub(super) fn render_agent_budget_tab(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2p5()
        .child(render_agent_safety_card(&chat.runtime_settings, cx))
        .child(render_budget_and_display_card(modal, chat, cx))
        .into_any_element()
}

fn render_agent_safety_card(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut step_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_steps_label")),
    );
    for (idx, &val) in [0_u32, 20, 50].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-steps", idx))
            .xsmall()
            .label(if val == 0 {
                tr("ai.settings_unlimited")
            } else {
                val.to_string()
            });
        btn = if cur.max_steps_per_turn == val {
            btn.primary()
        } else {
            btn.outline()
        };
        step_row = step_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.max_steps_per_turn = val;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }

    let mut interval_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_interval_label")),
    );
    for (idx, &ms) in [0_u64, 500, 1500, 3000].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-interval", idx))
            .xsmall()
            .label(format!("{ms}ms"));
        btn = if cur.min_llm_interval_ms == ms {
            btn.primary()
        } else {
            btn.outline()
        };
        interval_row = interval_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.min_llm_interval_ms = ms;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }

    let mut batch_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_vision_batch_label")),
    );
    for (idx, &count) in [2_usize, 6, 12].iter().enumerate() {
        let label = tr("ai.settings_vision_batch_unit").replace("{count}", &count.to_string());
        let mut btn = Button::new(("ai-modal-vision", idx)).xsmall().label(label);
        btn = if cur.vision_batch_size == count {
            btn.primary()
        } else {
            btn.outline()
        };
        batch_row = batch_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.vision_batch_size = count;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }

    let vision_controls = render_vision_compression_controls(cur, cx);

    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2()
        .p_2p5()
        .rounded(px(6.0))
        .bg(rgba(0x0f17_2a99))
        .border_1()
        .border_color(rgba(0x3341_55ff))
        .child(step_row)
        .child(interval_row)
        .child(batch_row)
        .child(vision_controls)
        .child(
            div()
                .text_xs()
                .text_color(rgba(0x94a3_b8ff))
                .child(tr("ai.settings_agy_interval_note")),
        )
        .into_any_element()
}

fn render_vision_compression_controls(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let packing_row = render_vision_packing_row(cur, cx);
    let grid_row = render_vision_grid_row(cur, cx);
    let tile_row = render_vision_tile_row(cur, cx);
    let quality_row = render_vision_quality_row(cur, cx);

    v_flex()
        .gap_1p5()
        .child(packing_row)
        .child(grid_row)
        .child(tile_row)
        .child(quality_row)
        .into_any_element()
}

fn render_vision_packing_row(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut packing_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_vision_packing_label")),
    );
    for (idx, (mode, label_key)) in [
        (
            VisionPackingMode::ContactSheet,
            "ai.settings_vision_packing_sheet",
        ),
        (
            VisionPackingMode::Individual,
            "ai.settings_vision_packing_individual",
        ),
    ]
    .iter()
    .enumerate()
    {
        let mode = *mode;
        let mut btn = Button::new(("ai-modal-vision-packing", idx))
            .xsmall()
            .label(tr(label_key));
        btn = if cur.vision_packing_mode == mode {
            btn.primary()
        } else {
            btn.outline()
        };
        packing_row = packing_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.vision_packing_mode = mode;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }
    packing_row.into_any_element()
}

fn render_vision_grid_row(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut grid_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_vision_sheet_grid_label")),
    );
    for (idx, &(cols, rows)) in [(2_u8, 2_u8), (3, 2), (4, 2)].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-vision-grid", idx))
            .xsmall()
            .label(format!(
                "{cols}x{rows} ({})",
                u16::from(cols) * u16::from(rows)
            ));
        btn = if cur.vision_contact_sheet_cols == cols && cur.vision_contact_sheet_rows == rows {
            btn.primary()
        } else {
            btn.outline()
        };
        grid_row = grid_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.vision_contact_sheet_cols = cols;
            next.vision_contact_sheet_rows = rows;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }
    grid_row.into_any_element()
}

fn render_vision_tile_row(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut tile_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_vision_tile_px_label")),
    );
    for (idx, &tile_px) in [256_u32, 320, 448].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-vision-tile", idx))
            .xsmall()
            .label(format!("{tile_px}px"));
        btn = if cur.vision_contact_sheet_tile_px == tile_px {
            btn.primary()
        } else {
            btn.outline()
        };
        tile_row = tile_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.vision_contact_sheet_tile_px = tile_px;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }
    tile_row.into_any_element()
}

fn render_vision_quality_row(
    cur: &AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut quality_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_vision_quality_label")),
    );
    for (idx, &q) in [55_u8, 68, 80].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-vision-quality", idx))
            .xsmall()
            .label(format!("Q{q}"));
        btn = if cur.vision_jpeg_quality == q {
            btn.primary()
        } else {
            btn.outline()
        };
        quality_row = quality_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.runtime_settings.clone();
            next.vision_jpeg_quality = q;
            this.controller.ai_chat.apply_runtime_settings(next);
            cx.notify();
        })));
    }
    quality_row.into_any_element()
}

fn render_budget_and_display_card(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut pref_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child(tr("ai.settings_usage_display_label")),
    );
    for (idx, &pref) in UsageDisplayPreference::all().iter().enumerate() {
        let mut btn = Button::new(("ai-modal-usage-pref", idx))
            .xsmall()
            .label(pref.display_label());
        btn = if chat.user_settings.usage_display_mode == pref {
            btn.primary()
        } else {
            btn.outline()
        };
        pref_row = pref_row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            let mut next = this.controller.ai_chat.user_settings.clone();
            next.usage_display_mode = pref;
            if let Err(err) = this.controller.ai_chat.save_explicit_user_settings(next) {
                this.set_modal_feedback(true, err);
            }
            cx.notify();
        })));
    }

    v_flex()
        .w_full()
        .min_w(px(0.))
        .gap_2()
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
                .child(tr("ai.settings_budget_heading")),
        )
        .child(
            h_flex()
                .w_full()
                .min_w(px(0.))
                .gap_1p5()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(Input::new(&modal.budget_usd_input).small()),
                )
                .child(
                    div().flex_shrink_0().child(
                        Button::new("ai-modal-save-budget")
                            .xsmall()
                            .primary()
                            .label(tr("ai.settings_save_budget"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_modal_budget(cx);
                            })),
                    ),
                ),
        )
        .child(pref_row)
        .into_any_element()
}
