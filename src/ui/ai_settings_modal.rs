//! VS Code 風の AI 設定モーダル UI（API キー・接続先・カスタムモデル・エージェント安全・予算・`{ } JSON` 編集）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, Styled as _, Window, div, px, rgba,
};

use crate::ai::provider::{AiProviderKind, SecretString};
use crate::ai::providers::credentials::{
    CredentialSource, delete_secret_from_keyring, save_secret_to_keyring,
};
use crate::ai::settings::{AiCustomModelConfig, AiUserSettings, UsageDisplayPreference};
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
    /// ユーザー設定予算上限 (USD) 入力欄。
    pub budget_usd_input: Entity<InputState>,
    /// `{ } JSON` 生編集テキストエリア。
    pub json_editor: Entity<TextareaState>,
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

impl SeaEditorWindow {
    /// VS Code 風 AI 設定モーダルを開く。
    pub fn open_ai_settings_modal(
        &mut self,
        tab: AiSettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_ai_settings_modal_with_mode(tab, AiSettingsViewMode::Gui, window, cx);
    }

    /// VS Code 風 AI 設定モーダルを `{ } JSON` 編集モードで開く。
    pub fn open_ai_settings_json_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_ai_settings_modal_with_mode(
            AiSettingsTab::ApiKeys,
            AiSettingsViewMode::Json,
            window,
            cx,
        );
    }

    fn open_ai_settings_modal_with_mode(
        &mut self,
        tab: AiSettingsTab,
        view_mode: AiSettingsViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected_provider = self.controller.ai_chat.selected_provider.clone();
        let initial_url = self
            .controller
            .ai_chat
            .user_settings
            .base_url_for(&selected_provider)
            .unwrap_or_else(|| selected_provider.default_base_url())
            .to_owned();
        let initial_budget = self
            .controller
            .ai_chat
            .user_settings
            .budget_usd
            .map_or_else(String::new, |v| format!("{v:.2}"));
        let initial_json = load_initial_json_text(&self.controller.ai_chat);

        let api_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("新しい API キーを入力...")
                .masked(true)
        });
        let base_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(selected_provider.default_base_url())
                .default_value(initial_url)
        });
        let model_id_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("例: deepseek/deepseek-r1"));
        let model_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("表示名（任意）"));
        let model_context_input = cx.new(|cx| InputState::new(window, cx).placeholder("128000"));
        let model_in_price_input = cx.new(|cx| InputState::new(window, cx).placeholder("0.55"));
        let model_out_price_input = cx.new(|cx| InputState::new(window, cx).placeholder("2.19"));
        let budget_usd_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: 10.00（空欄で無制限）")
                .default_value(initial_budget)
        });
        let json_editor = cx.new(|cx| TextareaState::new(window, cx).rows(14).soft_wrap(false));
        json_editor.update(cx, |s, cx| s.set_value(&initial_json, window, cx));

        let mut focus_subs = Vec::new();
        for entity in [
            &api_key_input,
            &base_url_input,
            &model_id_input,
            &model_name_input,
            &model_context_input,
            &model_in_price_input,
            &model_out_price_input,
            &budget_usd_input,
        ] {
            let fh = entity.read(cx).focus_handle(cx);
            focus_subs.push(cx.on_focus_in(&fh, window, |_, _, cx| cx.notify()));
            focus_subs.push(cx.on_blur(&fh, window, |_, _, cx| cx.notify()));
        }
        let json_fh = json_editor.read(cx).focus_handle(cx);
        focus_subs.push(cx.on_focus_in(&json_fh, window, |_, _, cx| cx.notify()));
        focus_subs.push(cx.on_blur(&json_fh, window, |_, _, cx| cx.notify()));

        self.modal_focus_subs = focus_subs;
        self.dynamic_input_sub = None;
        self.ai_settings_modal = Some(AiSettingsModalState {
            active_tab: tab,
            view_mode,
            selected_provider,
            api_key_input,
            base_url_input,
            model_id_input,
            model_name_input,
            model_context_input,
            model_in_price_input,
            model_out_price_input,
            budget_usd_input,
            json_editor,
            feedback: None,
        });
        self.controller.ai_chat.refresh_active_provider_side_data();
        cx.notify();
    }

    /// AI 設定モーダルを閉じて一時入力状態を破棄する。
    pub fn close_ai_settings_modal(&mut self) {
        self.ai_settings_modal = None;
        self.dynamic_input_sub = None;
        self.modal_focus_subs.clear();
    }

    fn select_modal_provider(
        &mut self,
        kind: &AiProviderKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next_url = self
            .controller
            .ai_chat
            .user_settings
            .base_url_for(kind)
            .unwrap_or_else(|| kind.default_base_url())
            .to_owned();
        if let Some(modal) = &mut self.ai_settings_modal {
            modal.selected_provider = kind.clone();
            modal.feedback = None;
            let url_input = modal.base_url_input.clone();
            let key_input = modal.api_key_input.clone();
            url_input.update(cx, |s, cx| s.set_value(&next_url, window, cx));
            key_input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if let AiProviderKind::Plugin { plugin_id } = kind
            && !self
                .controller
                .ai_chat
                .discovered_plugin_models
                .contains_key(plugin_id)
        {
            self.controller.ai_chat.probe_plugin_models_async(plugin_id);
        }
        cx.notify();
    }

    fn toggle_modal_view_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target_mode = match self.ai_settings_modal.as_ref().map(|m| m.view_mode) {
            Some(AiSettingsViewMode::Gui) => AiSettingsViewMode::Json,
            Some(AiSettingsViewMode::Json) => AiSettingsViewMode::Gui,
            None => return,
        };
        if target_mode == AiSettingsViewMode::Json {
            let text = load_initial_json_text(&self.controller.ai_chat);
            if let Some(modal) = &self.ai_settings_modal {
                modal
                    .json_editor
                    .update(cx, |s, cx| s.set_value(&text, window, cx));
            }
        }
        if let Some(modal) = &mut self.ai_settings_modal {
            modal.view_mode = target_mode;
            modal.feedback = None;
        }
        cx.notify();
    }

    fn save_modal_key_to_keyring(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, raw_key, key_input)) = self.ai_settings_modal.as_ref().map(|m| {
            (
                m.selected_provider.clone(),
                m.api_key_input.read(cx).value().to_string(),
                m.api_key_input.clone(),
            )
        }) else {
            return;
        };
        let secret = SecretString::new(&raw_key);
        if secret.is_empty() {
            self.set_modal_feedback(true, "保存する API キーを入力してください".to_owned());
            cx.notify();
            return;
        }
        let Some(account) = kind.keyring_account() else {
            self.set_modal_feedback(
                true,
                "このプロバイダは OS Keyring 保存に対応していません".to_owned(),
            );
            cx.notify();
            return;
        };
        if let Err(err) = save_secret_to_keyring(account, &secret) {
            self.set_modal_feedback(true, err.to_string());
            cx.notify();
            return;
        }
        key_input.update(cx, |s, cx| s.set_value("", window, cx));
        self.controller.ai_chat.set_runtime_api_key(None);
        if kind == AiProviderKind::OpenRouter {
            self.controller.ai_chat.refresh_openrouter_credits_async();
        }
        let msg = format_post_key_save_note(
            &kind,
            &self.controller.ai_chat,
            "OS Keyring に安全保存しました",
        );
        self.set_modal_feedback(false, msg);
        cx.notify();
    }

    fn save_modal_key_to_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, raw_key, key_input)) = self.ai_settings_modal.as_ref().map(|m| {
            (
                m.selected_provider.clone(),
                m.api_key_input.read(cx).value().to_string(),
                m.api_key_input.clone(),
            )
        }) else {
            return;
        };
        if raw_key.trim().is_empty() {
            self.set_modal_feedback(true, "保存する API キーを入力してください".to_owned());
            cx.notify();
            return;
        }
        let mut next = self.controller.ai_chat.user_settings.clone();
        next.set_api_key(&kind, &raw_key);
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        key_input.update(cx, |s, cx| s.set_value("", window, cx));
        self.controller.ai_chat.set_runtime_api_key(None);
        if kind == AiProviderKind::OpenRouter {
            self.controller.ai_chat.refresh_openrouter_credits_async();
        }
        let msg = format_post_key_save_note(
            &kind,
            &self.controller.ai_chat,
            "settings/ai_settings.json に保存しました",
        );
        self.set_modal_feedback(false, msg);
        cx.notify();
    }

    fn clear_modal_credentials(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, key_input)) = self
            .ai_settings_modal
            .as_ref()
            .map(|m| (m.selected_provider.clone(), m.api_key_input.clone()))
        else {
            return;
        };
        key_input.update(cx, |s, cx| s.set_value("", window, cx));
        let mut next = self.controller.ai_chat.user_settings.clone();
        next.remove_api_key(&kind);
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        if let Some(account) = kind.keyring_account()
            && let Err(err) = delete_secret_from_keyring(account)
        {
            self.set_modal_feedback(true, err.to_string());
            cx.notify();
            return;
        }
        if self.controller.ai_chat.selected_provider == kind {
            self.controller.ai_chat.set_runtime_api_key(None);
        }
        if kind == AiProviderKind::OpenRouter {
            self.controller.ai_chat.openrouter_credits = None;
            self.controller.ai_chat.openrouter_credits_error = None;
        }
        let msg = match self.controller.ai_chat.credential_source_for(&kind) {
            CredentialSource::EnvVar(env) => {
                format!("保存済みキーを削除しました（環境変数 {env} が引き続き有効です）")
            }
            _ => "Keyring / JSON の保存済みキーを削除しました".to_owned(),
        };
        self.set_modal_feedback(false, msg);
        cx.notify();
    }

    fn save_modal_base_url(
        &mut self,
        reset_default: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((kind, url_input)) = self
            .ai_settings_modal
            .as_ref()
            .map(|m| (m.selected_provider.clone(), m.base_url_input.clone()))
        else {
            return;
        };
        let target_url = if reset_default {
            kind.default_base_url().to_owned()
        } else {
            url_input.read(cx).value().trim().to_owned()
        };
        let effective = if target_url.is_empty() {
            kind.default_base_url().to_owned()
        } else {
            target_url
        };
        url_input.update(cx, |s, cx| s.set_value(&effective, window, cx));
        let mut next = self.controller.ai_chat.user_settings.clone();
        next.set_base_url(&kind, &effective);
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        if self.controller.ai_chat.selected_provider == kind {
            self.controller
                .ai_chat
                .custom_base_url
                .clone_from(&effective);
            self.controller.ai_chat.sync_selection_and_persist();
        }
        self.set_modal_feedback(false, format!("Base URL を更新しました: {effective}"));
        cx.notify();
    }

    fn upsert_modal_custom_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = &self.ai_settings_modal else {
            return;
        };
        let kind = modal.selected_provider.clone();
        let model_cfg = match build_custom_model_from_inputs(modal, cx) {
            Ok(cfg) => cfg,
            Err(err) => {
                self.set_modal_feedback(true, err);
                cx.notify();
                return;
            }
        };
        let saved_id = model_cfg.id.clone();
        let mut next = self.controller.ai_chat.user_settings.clone();
        if let Err(err) = next.upsert_custom_model(&kind, model_cfg) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        if let Some(m) = &self.ai_settings_modal {
            m.model_id_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            m.model_name_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            m.model_context_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            m.model_in_price_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            m.model_out_price_input
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.set_modal_feedback(false, format!("カスタムモデル `{saved_id}` を保存しました"));
        cx.notify();
    }

    fn save_modal_budget(&mut self, cx: &mut Context<Self>) {
        let Some(raw) = self
            .ai_settings_modal
            .as_ref()
            .map(|m| m.budget_usd_input.read(cx).value().to_string())
        else {
            return;
        };
        let trimmed = raw.trim();
        let parsed = if trimmed.is_empty() {
            None
        } else {
            match trimmed.parse::<f64>() {
                Ok(v) if v.is_finite() && v > 0.0 => Some(v),
                _ => {
                    self.set_modal_feedback(
                        true,
                        "予算上限 (USD) には正の数値を入力するか、無制限にする場合は空欄にしてください".to_owned(),
                    );
                    cx.notify();
                    return;
                }
            }
        };
        let mut next = self.controller.ai_chat.user_settings.clone();
        next.budget_usd = parsed;
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
        } else {
            self.set_modal_feedback(false, "予算上限を保存しました".to_owned());
        }
        cx.notify();
    }

    fn format_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .ai_settings_modal
            .as_ref()
            .map(|m| m.json_editor.clone())
        else {
            return;
        };
        let raw = editor.read(cx).value().to_string();
        match AiUserSettings::parse_and_validate_json(&raw).and_then(|s| s.to_pretty_json()) {
            Ok(pretty) => {
                editor.update(cx, |s, cx| s.set_value(&pretty, window, cx));
                self.set_modal_feedback(false, "JSON を検証・整形しました".to_owned());
            }
            Err(err) => self.set_modal_feedback(true, err),
        }
        cx.notify();
    }

    fn reload_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = load_initial_json_text(&self.controller.ai_chat);
        if let Some(modal) = &mut self.ai_settings_modal {
            modal
                .json_editor
                .update(cx, |s, cx| s.set_value(&text, window, cx));
            modal.feedback = Some((false, "ディスクまたは現在状態から再読込しました".to_owned()));
        }
        cx.notify();
    }

    fn save_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .ai_settings_modal
            .as_ref()
            .map(|m| m.json_editor.clone())
        else {
            return;
        };
        let raw = editor.read(cx).value().to_string();
        let parsed = match AiUserSettings::parse_and_validate_json(&raw) {
            Ok(s) => s,
            Err(err) => {
                self.set_modal_feedback(true, err);
                cx.notify();
                return;
            }
        };
        if let Err(err) = self.controller.ai_chat.save_and_apply_user_settings(parsed) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        self.sync_sliders_now(window, cx);
        self.controller.ai_chat.refresh_active_provider_side_data();
        if let Ok(pretty) = self.controller.ai_chat.user_settings.to_pretty_json() {
            editor.update(cx, |s, cx| s.set_value(&pretty, window, cx));
        }
        self.set_modal_feedback(
            false,
            "settings/ai_settings.json を検証して保存しました".to_owned(),
        );
        cx.notify();
    }

    fn set_modal_feedback(&mut self, is_error: bool, message: String) {
        if let Some(modal) = &mut self.ai_settings_modal {
            modal.feedback = Some((is_error, message));
        }
    }
}

fn load_initial_json_text(chat: &AiChatState) -> String {
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

fn format_post_key_save_note(kind: &AiProviderKind, chat: &AiChatState, base_msg: &str) -> String {
    match chat.credential_source_for(kind) {
        CredentialSource::EnvVar(env) => {
            format!("{base_msg}（注意: 環境変数 {env} が優先されています）")
        }
        CredentialSource::SettingsJson if base_msg.contains("Keyring") => {
            format!("{base_msg}（注意: ai_settings.json の保存値が優先されています）")
        }
        _ => base_msg.to_owned(),
    }
}

fn build_custom_model_from_inputs(
    modal: &AiSettingsModalState,
    cx: &App,
) -> Result<AiCustomModelConfig, String> {
    let id = modal.model_id_input.read(cx).value().trim().to_owned();
    if id.is_empty() {
        return Err("モデル ID を入力してください".to_owned());
    }
    let name_raw = modal.model_name_input.read(cx).value().trim().to_owned();
    let display_name = (!name_raw.is_empty()).then_some(name_raw);

    let ctx_raw = modal.model_context_input.read(cx).value().trim().to_owned();
    let context_window_tokens = if ctx_raw.is_empty() {
        None
    } else {
        Some(
            ctx_raw
                .parse::<u64>()
                .map_err(|_| "コンテキスト長は正の整数で入力してください".to_owned())?,
        )
    };
    let input_price_per_1m_usd = parse_optional_usd(
        modal.model_in_price_input.read(cx).value().as_ref(),
        "入力単価",
    )?;
    let output_price_per_1m_usd = parse_optional_usd(
        modal.model_out_price_input.read(cx).value().as_ref(),
        "出力単価",
    )?;

    Ok(AiCustomModelConfig {
        id,
        display_name,
        thinking_levels: Vec::new(),
        default_thinking: None,
        context_window_tokens,
        input_price_per_1m_usd,
        output_price_per_1m_usd,
    })
}

fn parse_optional_usd(raw: &str, label: &str) -> Result<Option<f64>, String> {
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

/// VS Code 風 AI 設定モーダルオーバーレイを描画する。
pub fn render_ai_settings_modal_overlay(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let body = match modal.view_mode {
        AiSettingsViewMode::Gui => render_gui_body(modal, chat, cx),
        AiSettingsViewMode::Json => render_json_mode_view(modal, chat, cx),
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
        AiSettingsTab::ApiKeys => render_api_keys_tab(modal, chat, cx),
        AiSettingsTab::Models => render_models_tab(modal, chat, cx),
        AiSettingsTab::AgentAndBudget => render_agent_budget_tab(modal, chat, cx),
    };

    h_flex()
        .w_full()
        .h(px(470.))
        .min_h(px(0.))
        .gap_3()
        .child(render_modal_sidebar(modal, cx))
        .child(
            div()
                .flex_1()
                .h_full()
                .min_h(px(0.))
                .overflow_y_scrollbar()
                .child(v_flex().w_full().gap_2p5().pr_2().child(tab_content)),
        )
        .into_any_element()
}

fn render_modal_sidebar(
    modal: &AiSettingsModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let items = [
        (AiSettingsTab::ApiKeys, "API キー・接続"),
        (AiSettingsTab::Models, "モデル管理"),
        (AiSettingsTab::AgentAndBudget, "エージェント・予算"),
    ];
    let mut col = v_flex()
        .w(px(164.))
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

fn render_provider_pills(
    selected: &AiProviderKind,
    id_prefix: &'static str,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut row = h_flex().w_full().gap_1().flex_wrap();
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

fn render_api_keys_tab(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let kind = &modal.selected_provider;
    let source = chat.credential_source_for(kind);
    let source_text = format_credential_source_badge(kind, &source);

    let mut panel = v_flex()
        .w_full()
        .gap_2p5()
        .child(render_provider_pills(kind, "ai-cfg-prov", cx))
        .child(
            h_flex()
                .items_center()
                .justify_between()
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

fn format_credential_source_badge(kind: &AiProviderKind, source: &CredentialSource) -> String {
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

fn is_http_configurable_provider(kind: &AiProviderKind) -> bool {
    matches!(
        kind,
        AiProviderKind::OpenAi
            | AiProviderKind::OpenRouter
            | AiProviderKind::OllamaCompat
            | AiProviderKind::Gemini
            | AiProviderKind::Anthropic
    )
}

fn render_api_key_controls(
    modal: &AiSettingsModalState,
    kind: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let has_keyring = kind.keyring_account().is_some();
    v_flex()
        .w_full()
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
                .child("API キー設定（プロジェクトファイル `.smc.json` には保存されません）"),
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
                        .label("OS Keyring に保存（推奨）")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_key_to_keyring(window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-save-json")
                        .xsmall()
                        .outline()
                        .label("JSON に保存")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_key_to_json(window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-clear-key")
                        .xsmall()
                        .ghost()
                        .label("クリア")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_modal_credentials(window, cx);
                        })),
                ),
        )
        .child(div().text_xs().text_color(rgba(0x94a3_b8ff)).child(
            "解決優先順位: 実行時上書き > 環境変数 > settings/ai_settings.json > OS Keyring",
        ))
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
    v_flex()
        .w_full()
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
                .child(format!(
                    "エンドポイント Base URL（既定: {}）",
                    kind.default_base_url()
                )),
        )
        .child(Input::new(&modal.base_url_input).small())
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Button::new("ai-cfg-save-url")
                        .xsmall()
                        .primary()
                        .label("URL を保存")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_base_url(false, window, cx);
                        })),
                )
                .child(
                    Button::new("ai-cfg-reset-url")
                        .xsmall()
                        .ghost()
                        .label("既定に戻す")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_modal_base_url(true, window, cx);
                        })),
                ),
        )
        .into_any_element()
}

fn render_models_tab(
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
        .gap_2p5()
        .child(render_custom_model_add_form(modal, kind, cx))
        .child(list)
        .into_any_element()
}

fn render_custom_model_row(
    kind: &AiProviderKind,
    model: &crate::ai::settings::ResolvedAiModel,
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
    let mut actions = h_flex().items_center().gap_1().child(
        Button::new(("ai-mdl-use", idx))
            .xsmall()
            .label(if is_active { "選択中" } else { "使う" })
            .disabled(is_active)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.controller.ai_chat.selected_provider != switch_kind {
                    this.controller.ai_chat.select_provider(switch_kind.clone());
                }
                this.controller
                    .ai_chat
                    .select_model_default_medium(&select_id);
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
                .label("削除")
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut next = this.controller.ai_chat.user_settings.clone();
                    next.remove_custom_model(&del_kind, &model_id);
                    match this.controller.ai_chat.save_explicit_user_settings(next) {
                        Ok(()) => this.set_modal_feedback(
                            false,
                            format!("モデル `{model_id}` を削除しました"),
                        ),
                        Err(err) => this.set_modal_feedback(true, err),
                    }
                    cx.notify();
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

fn render_custom_model_add_form(
    modal: &AiSettingsModalState,
    kind: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    v_flex()
        .w_full()
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
                .child(format!("{} へモデルを追加 / 上書き", kind.ui_combo_label())),
        )
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.model_id_input).small()),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.model_name_input).small()),
                ),
        )
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.model_context_input).small()),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.model_in_price_input).small()),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.model_out_price_input).small()),
                ),
        )
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgba(0x94a3_b8ff))
                        .child("左から: ID / 表示名 / Ctx長 / 入力$/1M / 出力$/1M"),
                )
                .child(
                    Button::new("ai-mdl-add-btn")
                        .xsmall()
                        .primary()
                        .label("追加 / 保存")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.upsert_modal_custom_model(window, cx);
                        })),
                ),
        )
        .into_any_element()
}

fn format_model_meta_line(ctx: Option<&u64>, in_usd: Option<f64>, out_usd: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = ctx {
        parts.push(format!("Ctx:{}k", tokens / 1_000));
    }
    if let (Some(i), Some(o)) = (in_usd, out_usd) {
        parts.push(format!("${i:.2}/${o:.2} per 1M"));
    }
    parts.join("  ")
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
    let mut panel = v_flex().w_full().gap_2().child(
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .p_2()
            .rounded(px(6.0))
            .bg(rgba(0x0f17_2a99))
            .border_1()
            .border_color(rgba(0x3341_55ff))
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(rgba(0xcbd5_e1ff))
                    .child("この接続先は外部 CLI 側で利用可能モデルが決まるため、検出済みモデルのみ一覧表示します（自由追加不可）。"),
            )
            .child(
                Button::new("ai-mdl-rescan")
                    .xsmall()
                    .outline()
                    .disabled(probing || plugin_id.is_empty())
                    .label(if probing { "検出中..." } else { "再スキャン" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.ai_chat.probe_plugin_models_async(&probe_id);
                        cx.notify();
                    })),
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
    model: &crate::ai::settings::ResolvedAiModel,
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
            Button::new(("ai-cli-mdl-use", idx))
                .xsmall()
                .disabled(!model.available || is_active)
                .label(if is_active { "選択中" } else { "使う" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.controller.ai_chat.selected_provider != switch_kind {
                        this.controller.ai_chat.select_provider(switch_kind.clone());
                    }
                    this.controller
                        .ai_chat
                        .select_model_default_medium(&model_id);
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_agent_budget_tab(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_2p5()
        .child(render_agent_safety_card(&chat.runtime_settings, cx))
        .child(render_budget_and_display_card(modal, chat, cx))
        .into_any_element()
}

fn render_agent_safety_card(
    cur: &crate::ai::provider::AgentRuntimeSettings,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut step_row = h_flex().items_center().gap_1p5().flex_wrap().child(
        div()
            .text_xs()
            .text_color(rgba(0xcbd5_e1ff))
            .child("自律ステップ上限:"),
    );
    for (idx, &val) in [0_u32, 20, 50].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-steps", idx))
            .xsmall()
            .label(if val == 0 {
                "無制限".to_owned()
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
            .child("最小 LLM 呼出間隔:"),
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
            .child("Vision バッチ画像枚数:"),
    );
    for (idx, &count) in [2_usize, 6, 12].iter().enumerate() {
        let mut btn = Button::new(("ai-modal-vision", idx))
            .xsmall()
            .label(format!("{count}枚"));
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

    v_flex()
        .gap_2()
        .p_2p5()
        .rounded(px(6.0))
        .bg(rgba(0x0f17_2a99))
        .border_1()
        .border_color(rgba(0x3341_55ff))
        .child(step_row)
        .child(interval_row)
        .child(batch_row)
        .child(
            div()
                .text_xs()
                .text_color(rgba(0x94a3_b8ff))
                .child("※ `agy CLI` 等での BAN / レート制限予防には 1500ms 以上を推奨します。"),
        )
        .into_any_element()
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
            .child("チャット下部表示形式:"),
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
                .child("任意セッション予算上限 (USD) とトークン残量表示"),
        )
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&modal.budget_usd_input).small()),
                )
                .child(
                    Button::new("ai-modal-save-budget")
                        .xsmall()
                        .primary()
                        .label("予算を保存")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.save_modal_budget(cx);
                        })),
                ),
        )
        .child(pref_row)
        .into_any_element()
}

fn render_json_mode_view(
    modal: &AiSettingsModalState,
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let path_label = chat.settings_path.as_ref().map_or_else(
        || "settings/ai_settings.json (未決定)".to_owned(),
        |p| p.display().to_string(),
    );

    let mut col = v_flex().w_full().gap_2().child(
        div()
            .text_xs()
            .text_color(rgba(0x94a3_b8ff))
            .child(format!("設定ファイル: {path_label}")),
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

    col.child(
        div()
            .w_full()
            .h(px(360.))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgba(0x3341_55ff))
            .bg(rgba(0x0f17_2aff))
            .child(Textarea::new(&modal.json_editor).appearance(false).small()),
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
                            .label("整形")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.format_modal_json(window, cx);
                            })),
                    )
                    .child(
                        Button::new("ai-json-reload")
                            .xsmall()
                            .ghost()
                            .label("再読込")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reload_modal_json(window, cx);
                            })),
                    ),
            )
            .child(
                Button::new("ai-json-save")
                    .xsmall()
                    .primary()
                    .label("検証して保存")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.save_modal_json(window, cx);
                    })),
            ),
    )
    .into_any_element()
}
