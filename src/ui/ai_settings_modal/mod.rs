//! VS Code 風の AI 設定モーダル UI（API キー・接続先・カスタムモデル・エージェント安全・予算・`{ } JSON` 編集）。

mod actions;
mod agent_budget_tab;
mod api_keys_tab;
mod json_view;
mod models_tab;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{EditorState, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement as _,
    MouseButton, ParentElement as _, Styled as _, Window, div, px, rgba,
};

use crate::ai::provider::AiProviderKind;
use crate::ai::providers::credentials::CredentialSource;
use crate::core::app_settings_dir;
use crate::ui::ai_chat_panel::AiChatState;
use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

/// VS Code 風 AI 設定モーダル内の GUI カテゴリタブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiSettingsTab {
    /// API キー・認証方式・カスタムベース URL。
    #[default]
    ApiKeys,
    /// カスタムモデル追加／CLI 利用可能モデル一覧。
    Models,
    /// エージェント実行上限・LLM 間隔・予算上限・トークン表示設定。
    AgentAndBudget,
}

/// AI 設定モーダルの表示モード（GUI フォーム vs `{ } JSON` 直接編集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiSettingsViewMode {
    /// カテゴリ別の GUI フォーム。
    #[default]
    Gui,
    /// `settings/ai_settings.json` の生の JSON 編集ビュー。
    Json,
}

/// AI 設定モーダルの編集中ウィジェット状態。
pub struct AiSettingsModalState {
    /// 選択中の GUI タブ。
    pub active_tab: AiSettingsTab,
    /// 現在の表示モード (`Gui` / `Json`)。
    pub view_mode: AiSettingsViewMode,
    /// モーダル内で編集対象としている AI プロバイダ種別。
    pub selected_provider: AiProviderKind,
    /// API キー入力欄（マスク表示）。
    pub api_key_input: Entity<InputState>,
    /// カスタムベース URL 入力欄。
    pub base_url_input: Entity<InputState>,
    /// カスタムモデル ID 入力欄。
    pub model_id_input: Entity<InputState>,
    /// カスタムモデル表示名入力欄。
    pub model_name_input: Entity<InputState>,
    /// カスタムモデル コンテキスト長入力欄。
    pub model_context_input: Entity<InputState>,
    /// カスタムモデル 入力単価 ($/1M tok) 入力欄。
    pub model_in_price_input: Entity<InputState>,
    /// カスタムモデル 出力単価 ($/1M tok) 入力欄。
    pub model_out_price_input: Entity<InputState>,
    /// 追加するカスタムモデルを `Thinking` (`Auto/Low/Med/High`) 対応として登録するか。
    pub custom_model_supports_thinking: bool,
    /// ユーザー設定予算上限 (USD) 入力欄。
    pub budget_usd_input: Entity<InputState>,
    /// `{ } JSON` 直接編集コードエディタ状態（`tree-sitter` JSON ハイライト対応）。
    pub json_editor: Entity<EditorState>,
    /// 操作結果のフィードバック (`(is_error, message)`)。
    pub feedback: Option<(bool, String)>,
}

impl AiSettingsModalState {
    /// モーダル内のいずれかのテキスト入力欄にフォーカスがあるか返す。
    #[must_use]
    pub fn is_any_input_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.api_key_input,
            &self.base_url_input,
            &self.model_id_input,
            &self.model_name_input,
            &self.model_context_input,
            &self.model_in_price_input,
            &self.model_out_price_input,
            &self.budget_usd_input,
        ]
        .iter()
        .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
            || self
                .json_editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
    }
}

/// VS Code 風 AI 設定モーダルオーバーレイを描画する。
pub fn render_ai_settings_modal_overlay(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let body = match modal.view_mode {
        AiSettingsViewMode::Gui => render_gui_body(modal, chat, cx),
        AiSettingsViewMode::Json => json_view::render_json_mode_view(modal, chat, cx),
    };

    let mut dialog = v_flex()
        .id("ai-settings-modal-panel")
        .w(px(760.))
        .max_w(px(760.))
        .max_h(px(620.))
        .gap_2p5()
        .p_4()
        .rounded(px(8.0))
        .bg(rgba(0x1e29_3bff))
        .border_1()
        .border_color(rgba(0x4755_69ff))
        .shadow_lg()
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(render_modal_header(modal, chat, cx))
        .child(body);

    if let Some((is_err, msg)) = &modal.feedback {
        let color = if *is_err {
            rgba(0xf871_71ff)
        } else {
            rgba(0x4ade_80ff)
        };
        dialog = dialog.child(div().text_xs().text_color(color).child(msg.clone()));
    }

    div()
        .id("ai-settings-backdrop")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x0000_0099))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| {
                this.close_ai_settings_modal();
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            }),
        )
        .child(dialog)
        .into_any_element()
}

fn render_modal_header(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mode_label = match modal.view_mode {
        AiSettingsViewMode::Gui => tr("ai.open_json"),
        AiSettingsViewMode::Json => tr("ai.open_gui"),
    };
    let folder_target = settings_folder_for_chat(chat);

    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .pb_2()
        .border_b_1()
        .border_color(rgba(0x3341_55ff))
        .child(
            div()
                .text_base()
                .font_semibold()
                .text_color(rgba(0xf8fa_fcff))
                .child(tr("ai.settings_modal_title")),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1p5()
                .child(
                    Button::new("ai-settings-open-folder")
                        .xsmall()
                        .ghost()
                        .disabled(folder_target.is_none())
                        .label(tr("ai.open_settings_folder"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(dir) = &folder_target {
                                match open_directory_in_os(dir) {
                                    Ok(()) => this.set_modal_feedback(
                                        false,
                                        format!("フォルダを開きました: {}", dir.display()),
                                    ),
                                    Err(err) => this.set_modal_feedback(true, err),
                                }
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("ai-settings-toggle-json")
                        .xsmall()
                        .outline()
                        .label(mode_label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_modal_view_mode(window, cx);
                        })),
                )
                .child(
                    Button::new("ai-settings-close")
                        .xsmall()
                        .ghost()
                        .label(tr("common.close"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_ai_settings_modal();
                            window.focus(&this.workspace_focus, cx);
                            cx.notify();
                        })),
                ),
        )
        .into_any_element()
}

fn render_gui_body(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let tab_content = match modal.active_tab {
        AiSettingsTab::ApiKeys => api_keys_tab::render_api_keys_tab(modal, chat, cx),
        AiSettingsTab::Models => models_tab::render_models_tab(modal, chat, cx),
        AiSettingsTab::AgentAndBudget => agent_budget_tab::render_agent_budget_tab(modal, chat, cx),
    };

    h_flex()
        .w_full()
        .h(px(470.))
        .min_h(px(0.))
        .items_start()
        .gap_3()
        .overflow_hidden()
        .child(render_modal_sidebar(modal, cx))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .min_h(px(0.))
                .overflow_hidden()
                .child(
                    div().size_full().overflow_y_scrollbar().child(
                        v_flex()
                            .w_full()
                            .min_w(px(0.))
                            .gap_2p5()
                            .pr_2()
                            .child(tab_content),
                    ),
                ),
        )
        .into_any_element()
}

fn render_modal_sidebar(
    modal: &AiSettingsModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let items = [
        (AiSettingsTab::ApiKeys, tr("ai.settings_tab_api_keys")),
        (AiSettingsTab::Models, tr("ai.settings_tab_models")),
        (
            AiSettingsTab::AgentAndBudget,
            tr("ai.settings_tab_agent_budget"),
        ),
    ];
    let mut col = v_flex()
        .w(px(164.))
        .h_full()
        .flex_shrink_0()
        .gap_1()
        .pr_2()
        .border_r_1()
        .border_color(rgba(0x3341_55ff));
    for (idx, (tab, label)) in items.into_iter().enumerate() {
        let mut btn = Button::new(("ai-settings-tab", idx)).small().label(label);
        btn = if modal.active_tab == tab {
            btn.primary()
        } else {
            btn.ghost()
        };
        col = col.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            if let Some(m) = &mut this.ai_settings_modal {
                m.active_tab = tab;
                m.feedback = None;
            }
            cx.notify();
        })));
    }
    col.into_any_element()
}

pub(super) fn render_provider_pills(
    selected: &AiProviderKind,
    id_prefix: &'static str,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut row = h_flex().w_full().min_w(px(0.)).gap_1().flex_wrap();
    for (idx, kind) in AiProviderKind::ui_selectable_providers()
        .into_iter()
        .enumerate()
    {
        let is_selected = &kind == selected;
        let mut btn = Button::new((id_prefix, idx))
            .xsmall()
            .label(kind.ui_combo_label());
        btn = if is_selected {
            btn.primary()
        } else {
            btn.outline()
        };
        row = row.child(btn.on_click(cx.listener(move |this, _, window, cx| {
            this.select_modal_provider(&kind, window, cx);
        })));
    }
    row.into_any_element()
}

pub(super) fn load_initial_json_text(chat: &AiChatState) -> String {
    if chat.settings_load_warning.is_some()
        && let Some(path) = &chat.settings_path
        && let Ok(raw) = fs::read_to_string(path)
    {
        return raw;
    }
    chat.user_settings
        .to_pretty_json()
        .unwrap_or_else(|_| "{\n  \"schema_version\": 1\n}".to_owned())
}

pub(super) fn format_post_key_save_note(
    kind: &AiProviderKind,
    chat: &AiChatState,
    base_msg: &str,
) -> String {
    match chat.cached_credential_source_for(kind) {
        CredentialSource::EnvVar(env) => {
            format!("{base_msg}（注意: 環境変数 {env} が優先されています）")
        }
        CredentialSource::SettingsJson if base_msg.contains("Keyring") => {
            format!("{base_msg}（注意: ai_settings.json の保存値が優先されています）")
        }
        _ => base_msg.to_owned(),
    }
}

pub(super) fn format_credential_source_badge(
    kind: &AiProviderKind,
    source: &CredentialSource,
) -> String {
    if matches!(kind, AiProviderKind::Plugin { .. }) {
        return "外部 CLI 側で認証管理".to_owned();
    }
    if matches!(kind, AiProviderKind::OllamaCompat) && *source == CredentialSource::Missing {
        return "認証不要 (ローカル接続)".to_owned();
    }
    match source {
        CredentialSource::RuntimeOverride => "現在有効: 実行時一時キー".to_owned(),
        CredentialSource::EnvVar(var) => format!("現在有効: 環境変数 ({var})"),
        CredentialSource::SettingsJson => "現在有効: settings/ai_settings.json".to_owned(),
        CredentialSource::Keyring => "現在有効: OS Keyring".to_owned(),
        CredentialSource::Missing => format!(
            "未設定（環境変数 {} または下欄で登録）",
            kind.default_env_var().unwrap_or("API_KEY")
        ),
    }
}

pub(super) const fn is_http_configurable_provider(kind: &AiProviderKind) -> bool {
    matches!(
        kind,
        AiProviderKind::OpenAi
            | AiProviderKind::OpenRouter
            | AiProviderKind::OllamaCompat
            | AiProviderKind::Gemini
            | AiProviderKind::Anthropic
    )
}

pub(super) fn parse_positive_context_tokens(raw: &str) -> Result<Option<u64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .ok()
        .filter(|&v| v > 0)
        .map(Some)
        .ok_or_else(|| "コンテキスト長は 1 以上の正の整数で入力してください".to_owned())
}

pub(super) fn parse_optional_usd(raw: &str, label: &str) -> Result<Option<f64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let val = trimmed
        .parse::<f64>()
        .map_err(|_| format!("{label} は数値で入力してください"))?;
    if !val.is_finite() || val < 0.0 {
        return Err(format!("{label} は 0 以上の数値で入力してください"));
    }
    Ok(Some(val))
}

pub(super) fn format_model_meta_line(
    ctx: Option<&u64>,
    in_usd: Option<f64>,
    out_usd: Option<f64>,
) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = ctx {
        parts.push(format!("Ctx:{}k", tokens / 1_000));
    }
    if let (Some(i), Some(o)) = (in_usd, out_usd) {
        parts.push(format!("${i:.2}/${o:.2} per 1M"));
    }
    parts.join("  ")
}

fn settings_folder_for_chat(chat: &AiChatState) -> Option<PathBuf> {
    chat.settings_path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(app_settings_dir)
}

fn open_directory_in_os(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|err| {
        format!(
            "設定フォルダの作成に失敗しました ({}): {err}",
            dir.display()
        )
    })?;
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("explorer");
        c.arg(dir);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(dir);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(dir);
        c
    };
    cmd.spawn()
        .map(|_| ())
        .map_err(|err| format!("フォルダを開けませんでした: {err}"))
}

#[cfg(test)]
mod tests {
    use super::{
        format_credential_source_badge, format_model_meta_line, format_post_key_save_note,
        is_http_configurable_provider, parse_optional_usd, parse_positive_context_tokens,
    };
    use crate::ai::provider::AiProviderKind;
    use crate::ai::providers::credentials::CredentialSource;
    use crate::ui::ai_chat_panel::AiChatState;

    #[test]
    fn context_and_price_parsers_validate_inputs_strictly() {
        assert_eq!(parse_positive_context_tokens(""), Ok(None));
        assert_eq!(parse_positive_context_tokens("128000"), Ok(Some(128_000)));
        assert!(parse_positive_context_tokens("0").is_err());
        assert!(parse_positive_context_tokens("-10").is_err());
        assert!(parse_positive_context_tokens("abc").is_err());

        assert_eq!(parse_optional_usd("  ", "入力単価"), Ok(None));
        assert_eq!(parse_optional_usd("0", "入力単価"), Ok(Some(0.0)));
        assert_eq!(parse_optional_usd("1.5", "出力単価"), Ok(Some(1.5)));
        assert!(parse_optional_usd("-0.1", "入力単価").is_err());
        assert!(parse_optional_usd("NaN", "入力単価").is_err());
    }

    #[test]
    fn badge_meta_and_post_save_formatters_work_cleanly() {
        assert!(
            format_credential_source_badge(
                &AiProviderKind::OpenRouter,
                &CredentialSource::EnvVar("OPENROUTER_API_KEY"),
            )
            .contains("OPENROUTER_API_KEY")
        );
        assert!(
            format_credential_source_badge(
                &AiProviderKind::OllamaCompat,
                &CredentialSource::Missing,
            )
            .contains("認証不要")
        );
        assert!(
            format_credential_source_badge(
                &AiProviderKind::Plugin {
                    plugin_id: "sea.ai.antigravity".to_owned(),
                },
                &CredentialSource::Missing,
            )
            .contains("CLI")
        );

        assert_eq!(
            format_model_meta_line(Some(&128_000), Some(0.55), Some(2.19)),
            "Ctx:128k  $0.55/$2.19 per 1M"
        );
        assert!(is_http_configurable_provider(&AiProviderKind::OpenRouter));
        assert!(!is_http_configurable_provider(&AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        }));

        let chat = AiChatState::default();
        assert_eq!(
            format_post_key_save_note(&AiProviderKind::OpenRouter, &chat, "保存しました"),
            "保存しました"
        );
    }
}
