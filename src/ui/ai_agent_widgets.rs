//! 自律エージェントモード切替、メディアフォルダ添付、VS Code 風セッション管理、
//! リッチ対話質問（`ask_user`）カード、外部ツール承認カード、および安全レート設定カードを描画する UI 部品。

use std::collections::BTreeMap;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Side, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _,
    PathPromptOptions, SharedString, Styled as _, div, px,
};

use crate::ai::session_store::PendingAgentQuestionSnapshot;
use crate::ui::ai_chat_panel::{AiChatDrawer, AiChatState, PendingAgentToolApproval};
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

/// チャット質問カード用入力フィールド群の保持型。
pub(crate) type AskFieldInputs = (String, Vec<(String, Entity<InputState>)>);

/// 単発チャットか、`plugins/agent/*` の自律エージェントプラグインかを選択するメニューボタン。
pub(crate) fn render_agent_mode_menu_button(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let current_agent_id = chat.selected_agent_id.clone();

    let active_label = match current_agent_id.as_deref() {
        None => "単発 ▾".to_owned(),
        Some(id) => {
            let found_name = chat
                .cached_agents
                .iter()
                .find(|p| p.manifest.id == id)
                .map_or_else(
                    || id.to_owned(),
                    |p| short_agent_display_name(&p.manifest.name),
                );
            format!("Agent:{found_name} ▾")
        }
    };

    let available_agents = chat.cached_agents.clone();
    Button::new("ai-agent-mode-picker")
        .ghost()
        .xsmall()
        .disabled(chat.is_busy())
        .label(active_label)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut popup = menu
                .min_w(px(250.))
                .check_side(Side::Right)
                .label("実行モード選択");

            let weak_single = weak.clone();
            popup = popup
                .item(
                    PopupMenuItem::new("通常チャット（単発編集）")
                        .checked(current_agent_id.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = weak_single.update(cx, |view, cx| {
                                view.controller.ai_chat.select_agent(None);
                                cx.notify();
                            });
                        }),
                )
                .separator();

            if available_agents.is_empty() {
                popup = popup.label("（plugins/agent/ にエージェント未検出）");
            } else {
                popup = popup.label("自律エージェントプラグイン");
                for plugin in &available_agents {
                    let id = plugin.manifest.id.clone();
                    let is_selected = current_agent_id.as_deref() == Some(id.as_str());
                    let status = if plugin.executable_path.is_some() {
                        ""
                    } else {
                        " [未ビルド]"
                    };
                    let label = format!("{}{status}", plugin.manifest.name);
                    let weak_item = weak.clone();
                    popup = popup.item(PopupMenuItem::new(label).checked(is_selected).on_click(
                        move |_, _, cx| {
                            let _ = weak_item.update(cx, |view, cx| {
                                view.controller.ai_chat.select_agent(Some(id.clone()));
                                cx.notify();
                            });
                        },
                    ));
                }
            }
            let weak_reload = weak.clone();
            popup
                .separator()
                .item(
                    PopupMenuItem::new("エージェント一覧を再読込").on_click(move |_, _, cx| {
                        let _ = weak_reload.update(cx, |view, cx| {
                            view.controller.ai_chat.refresh_discovered_agents();
                            cx.notify();
                        });
                    }),
                )
        })
}

fn short_agent_display_name(raw: &str) -> String {
    if raw.contains("Director") {
        "Director".to_owned()
    } else {
        raw.chars().take(12).collect()
    }
}

/// メディアフォルダ選択ダイアログを開くボタンを描画する。
pub(crate) fn render_folder_attach_button(cx: &mut Context<SeaEditorWindow>) -> impl IntoElement {
    Button::new("ai-attach-folder-btn")
        .ghost()
        .xsmall()
        .label(format!("📁 {}", tr("ai.attach_folder")))
        .on_click(cx.listener(|_, _, _, cx| {
            SeaEditorWindow::prompt_attach_ai_folder(cx);
        }))
}

/// コンポーザ上に表示する添付フォルダチップ 1 件を構築する（× クリックで解除）。
pub(crate) fn render_attached_folder_chip(
    idx: usize,
    folder: &std::path::Path,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let label_name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| folder.display().to_string(), ToOwned::to_owned);
    let detach_path = folder.to_path_buf();
    Button::new(("ai-attached-folder-chip", idx))
        .outline()
        .xsmall()
        .label(format!("📁 {label_name} ×"))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.controller.ai_chat.detach_folder(&detach_path);
            cx.notify();
        }))
}

/// VS Code 風セッション管理ドロワー（履歴一覧・開く・削除・新規作成）を描画する。
pub(crate) fn render_session_history_drawer(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let mut card = v_flex()
        .w_full()
        .p_2()
        .gap_1p5()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.secondary);

    card = card.child(
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child("セッション履歴"),
            )
            .child(
                Button::new("ai-session-new-inline")
                    .primary()
                    .xsmall()
                    .disabled(chat.is_busy())
                    .label(format!("+ {}", tr("ai.new_session")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.ai_chat.new_session();
                        this.controller.ai_chat.active_drawer = AiChatDrawer::None;
                        this.sync_widgets_from_controller(cx);
                        cx.notify();
                    })),
            ),
    );

    let sessions = &chat.cached_sessions;

    if sessions.is_empty() {
        card = card.child(
            div()
                .text_color(theme.muted_foreground)
                .child("保存済みセッションはありません。指示を送ると自動保存されます。"),
        );
        return card;
    }

    for (idx, summary) in sessions.iter().take(8).enumerate() {
        let is_current = summary.session_id == chat.session_id;
        let open_id = summary.session_id.clone();
        let del_id = summary.session_id.clone();
        let meta_badge = format!("({}件)", summary.turn_count);
        let row_label = if is_current {
            format!("● {} {meta_badge}", summary.title)
        } else {
            format!("{} {meta_badge}", summary.title)
        };

        let mut open_btn = Button::new(("ai-session-open", idx))
            .xsmall()
            .disabled(chat.is_busy())
            .label(row_label);
        open_btn = if is_current {
            open_btn.primary()
        } else {
            open_btn.ghost()
        };

        card = card.child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(open_btn.on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(err) = this.controller.open_saved_ai_session(&open_id) {
                                this.controller.ai_chat.last_error = Some(err);
                            }
                            this.sync_widgets_from_controller(cx);
                            cx.notify();
                        }))),
                )
                .child(
                    Button::new(("ai-session-del", idx))
                        .ghost()
                        .xsmall()
                        .disabled(chat.is_busy())
                        .label("×")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Err(err) = this.controller.delete_saved_ai_session(&del_id) {
                                this.controller.ai_chat.last_error = Some(err);
                            }
                            this.sync_widgets_from_controller(cx);
                        })),
                ),
        );
    }

    card
}

/// エージェントからのリッチ対話質問 (`ask_user`) カードを描画する。
pub(crate) fn render_ask_user_card(
    question: &PendingAgentQuestionSnapshot,
    ask_inputs: &[(String, Entity<InputState>)],
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let mut card = v_flex()
        .w_full()
        .p_2p5()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(theme.primary)
        .bg(theme.secondary)
        .child(
            div()
                .font_semibold()
                .text_color(theme.foreground)
                .child(question.title.clone()),
        )
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(question.message.clone()),
        );

    for field in &question.fields {
        let mut field_col = v_flex().w_full().gap_1().child(
            div()
                .text_color(theme.foreground)
                .child(field.label.clone()),
        );
        if let Some((_, entity)) = ask_inputs.iter().find(|(id, _)| id == &field.id) {
            field_col = field_col.child(Input::new(entity).small());
        }
        card = card.child(field_col);
    }

    let mut actions = h_flex().w_full().gap_1p5().flex_wrap();
    for (idx, choice) in question.choices.iter().enumerate() {
        let cid = choice.id.clone();
        let mut btn = Button::new(("ai-ask-choice", idx))
            .small()
            .label(choice.label.clone());
        btn = if choice.is_primary {
            btn.primary()
        } else {
            btn.outline()
        };
        actions = actions.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            this.submit_ai_ask_answer(Some(cid.clone()), false, cx);
        })));
    }
    if question.allow_skip || question.choices.is_empty() {
        actions = actions.child(
            Button::new("ai-ask-skip")
                .ghost()
                .small()
                .label(tr("ai.ask_skip"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.submit_ai_ask_answer(Some("skip".to_owned()), true, cx);
                })),
        );
    }

    card.child(actions)
}

/// `Manual` モード時に外部副作用ツールの実行許可を求めるカードを描画する。
pub(crate) fn render_tool_approval_card(
    approval: &PendingAgentToolApproval,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let args_preview = serde_json::to_string_pretty(&approval.arguments).unwrap_or_default();

    v_flex()
        .w_full()
        .p_2()
        .gap_1p5()
        .rounded_md()
        .border_1()
        .border_color(theme.warning)
        .bg(theme.secondary)
        .child(
            div()
                .font_semibold()
                .text_color(theme.foreground)
                .child(format!("外部ツール実行の確認: {}", approval.tool_name)),
        )
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(approval.description.clone()),
        )
        .child(
            div()
                .p_1()
                .rounded_sm()
                .bg(theme.background)
                .text_color(theme.muted_foreground)
                .child(args_preview.chars().take(240).collect::<String>()),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("ai-tool-allow")
                        .primary()
                        .small()
                        .label(tr("ai.tool_approve"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Err(err) = this.controller.answer_agent_tool_approval(true, None)
                            {
                                this.controller.ai_chat.last_error = Some(err);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("ai-tool-deny")
                        .ghost()
                        .small()
                        .label(tr("ai.tool_deny"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Err(err) = this.controller.answer_agent_tool_approval(
                                false,
                                Some("ユーザーが実行を拒否しました".to_owned()),
                            ) {
                                this.controller.ai_chat.last_error = Some(err);
                            }
                            cx.notify();
                        })),
                ),
        )
}

/// `SeaEditorWindow` 用のフォルダ選択・リッチ対話回答同期ヘルパ群。
impl SeaEditorWindow {
    /// OS フォルダ選択ダイアログを開き、選ばれたディレクトリを AI コンテキストへ添付する。
    pub(crate) fn prompt_attach_ai_folder(cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(SharedString::from("ムービー構成に使うフォルダを選択")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && !paths.is_empty()
            {
                let _ = this.update(cx, |view, cx| {
                    for p in paths {
                        view.controller.ai_chat.attach_folder_with_auto_agent(p);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// `pending_question` に合わせて動的な `InputState` エンティティ群を作成・破棄する。
    pub(crate) fn sync_ai_ask_inputs(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = self.controller.ai_chat.pending_question.as_ref() else {
            if self.ai_ask_inputs.is_some() {
                self.ai_ask_inputs = None;
                self.ai_ask_subs.clear();
            }
            return;
        };

        let same_request = self
            .ai_ask_inputs
            .as_ref()
            .is_some_and(|(req_id, _)| req_id == &question.request_id);
        if same_request {
            return;
        }

        self.ai_ask_subs.clear();
        let mut fields_entities = Vec::with_capacity(question.fields.len());
        for field in &question.fields {
            let initial_val = self
                .controller
                .ai_chat
                .ask_field_values
                .get(&field.id)
                .cloned()
                .or_else(|| field.default_value.clone())
                .unwrap_or_default();
            let placeholder_txt = field.placeholder.clone().unwrap_or_default();
            let entity = cx.new(|cx| {
                let mut state = InputState::new(window, cx);
                if !placeholder_txt.is_empty() {
                    state = state.placeholder(placeholder_txt);
                }
                if !initial_val.is_empty() {
                    state = state.default_value(&initial_val);
                }
                state
            });
            let fh = entity.read(cx).focus_handle(cx);
            self.ai_ask_subs
                .push(cx.on_focus_in(&fh, window, |_, _, cx| cx.notify()));
            self.ai_ask_subs
                .push(cx.on_blur(&fh, window, |_, _, cx| cx.notify()));
            fields_entities.push((field.id.clone(), entity));
        }
        self.ai_ask_inputs = Some((question.request_id.clone(), fields_entities));
    }

    /// 現在表示中の `ask_user` カード入力値を取り出してエージェントへ回答を送信する。
    pub(crate) fn submit_ai_ask_answer(
        &mut self,
        choice_id: Option<String>,
        skipped: bool,
        cx: &mut Context<Self>,
    ) {
        let mut values = BTreeMap::new();
        if let Some((_, ref entities)) = self.ai_ask_inputs {
            for (field_id, entity) in entities {
                let val = entity.read(cx).value().to_string();
                values.insert(field_id.clone(), val);
            }
        }
        if let Err(err) = self
            .controller
            .answer_pending_agent_question(choice_id, values, None, skipped)
        {
            self.controller.ai_chat.last_error = Some(err);
        } else {
            self.ai_ask_inputs = None;
            self.ai_ask_subs.clear();
        }
        cx.notify();
    }
}
