//! AI 設定モーダルに対する操作アクション・保存ハンドラ。

use gpui_kit::component::input::{EditorState, InputState};
use gpui_kit::{App, AppContext as _, Context, Focusable as _, Window};

use crate::ai::provider::{AiProviderKind, AiThinkingLevel, SecretString};
use crate::ai::providers::credentials::{
    CredentialSource, delete_secret_from_keyring, save_secret_to_keyring,
};
use crate::ai::settings::{AiCustomModelConfig, AiUserSettings};
use crate::ui::window::SeaEditorWindow;

use super::{
    AiSettingsModalState, AiSettingsTab, AiSettingsViewMode, format_post_key_save_note,
    load_initial_json_text, parse_optional_usd, parse_positive_context_tokens,
};

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
        self.controller.ai_chat.refresh_all_credential_sources();
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
                .placeholder(crate::ui::i18n::tr("ai.settings_api_key_placeholder"))
                .masked(true)
        });
        let base_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(selected_provider.default_base_url())
                .default_value(initial_url)
        });
        let model_id_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::tr("ai.settings_model_id_placeholder"))
        });
        let model_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::tr("ai.settings_model_name_placeholder"))
        });
        let model_context_input = cx.new(|cx| InputState::new(window, cx).placeholder("128000"));
        let model_in_price_input = cx.new(|cx| InputState::new(window, cx).placeholder("0.55"));
        let model_out_price_input = cx.new(|cx| InputState::new(window, cx).placeholder("2.19"));
        let budget_usd_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::tr("ai.settings_budget_placeholder"))
                .default_value(initial_budget)
        });
        // Ctrl+F 検索パネルとモーダル内の Escape 閉じ操作が競合しないよう searchable(false) を明示する。
        let json_editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .searchable(false)
        });
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
            custom_model_supports_thinking: false,
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

    pub(super) fn select_modal_provider(
        &mut self,
        kind: &AiProviderKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.ai_chat.refresh_credential_source_for(kind);
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

    pub(super) fn toggle_modal_view_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn save_modal_key_to_keyring(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if self.controller.ai_chat.selected_provider == kind {
            self.controller.ai_chat.set_runtime_api_key(None);
        }
        self.controller.ai_chat.refresh_credential_source_for(&kind);
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

    pub(super) fn save_modal_key_to_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.controller.ai_chat.selected_provider == kind {
            self.controller.ai_chat.set_runtime_api_key(None);
        }
        self.controller.ai_chat.refresh_credential_source_for(&kind);
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

    pub(super) fn clear_modal_credentials(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let source = self.controller.ai_chat.refresh_credential_source_for(&kind);
        if kind == AiProviderKind::OpenRouter {
            self.controller.ai_chat.openrouter_credits = None;
            self.controller.ai_chat.openrouter_credits_error = None;
        }
        let msg = match source {
            CredentialSource::EnvVar(env) => {
                format!("保存済みキーを削除しました（環境変数 {env} が引き続き有効です）")
            }
            _ => "Keyring / JSON の保存済みキーを削除しました".to_owned(),
        };
        self.set_modal_feedback(false, msg);
        cx.notify();
    }

    pub(super) fn save_modal_base_url(
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

    pub(super) fn upsert_modal_custom_model(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if let Some(m) = &mut self.ai_settings_modal {
            m.custom_model_supports_thinking = false;
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

    pub(super) fn delete_modal_custom_model(
        &mut self,
        kind: &AiProviderKind,
        model_id: &str,
        cx: &mut Context<Self>,
    ) {
        let mut next = self.controller.ai_chat.user_settings.clone();
        if !next.remove_custom_model(kind, model_id) {
            self.set_modal_feedback(
                true,
                format!("モデル `{model_id}` はカスタム定義に見つかりませんでした"),
            );
            cx.notify();
            return;
        }
        if let Err(err) = self.controller.ai_chat.save_explicit_user_settings(next) {
            self.set_modal_feedback(true, err);
            cx.notify();
            return;
        }
        if self.controller.ai_chat.selected_provider == *kind
            && self.controller.ai_chat.model_name == model_id
        {
            let fallback = kind.preferred_default_model().to_owned();
            self.controller
                .ai_chat
                .select_model_default_medium(&fallback);
        }
        self.set_modal_feedback(false, format!("モデル `{model_id}` を削除しました"));
        cx.notify();
    }

    pub(super) fn save_modal_budget(&mut self, cx: &mut Context<Self>) {
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
                        "予算上限 (USD) には正の数値を入力するか、無制限にする場合は空欄にしてください"
                            .to_owned(),
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

    pub(super) fn format_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn reload_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.controller.ai_chat.refresh_all_credential_sources();
        let text = load_initial_json_text(&self.controller.ai_chat);
        if let Some(modal) = &mut self.ai_settings_modal {
            modal
                .json_editor
                .update(cx, |s, cx| s.set_value(&text, window, cx));
            modal.feedback = Some((false, "ディスクまたは現在状態から再読込しました".to_owned()));
        }
        cx.notify();
    }

    pub(super) fn save_modal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.controller.ai_chat.openrouter_credits = None;
        self.controller.ai_chat.openrouter_credits_error = None;
        self.sync_modal_widgets_from_settings(window, cx);
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

    fn sync_modal_widgets_from_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next_provider = self.controller.ai_chat.selected_provider.clone();
        let next_url = self
            .controller
            .ai_chat
            .user_settings
            .base_url_for(&next_provider)
            .unwrap_or_else(|| next_provider.default_base_url())
            .to_owned();
        let next_budget = self
            .controller
            .ai_chat
            .user_settings
            .budget_usd
            .map_or_else(String::new, |v| format!("{v:.2}"));
        if let Some(modal) = &mut self.ai_settings_modal {
            modal.selected_provider = next_provider;
            modal
                .base_url_input
                .update(cx, |s, cx| s.set_value(&next_url, window, cx));
            modal
                .budget_usd_input
                .update(cx, |s, cx| s.set_value(&next_budget, window, cx));
            modal
                .api_key_input
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    pub(super) fn set_modal_feedback(&mut self, is_error: bool, message: String) {
        if let Some(modal) = &mut self.ai_settings_modal {
            modal.feedback = Some((is_error, message));
        }
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
    let context_window_tokens = parse_positive_context_tokens(&ctx_raw)?;
    let input_price_per_1m_usd = parse_optional_usd(
        modal.model_in_price_input.read(cx).value().as_ref(),
        "入力単価",
    )?;
    let output_price_per_1m_usd = parse_optional_usd(
        modal.model_out_price_input.read(cx).value().as_ref(),
        "出力単価",
    )?;
    let (thinking_levels, default_thinking) = if modal.custom_model_supports_thinking {
        (
            vec![
                AiThinkingLevel::Auto,
                AiThinkingLevel::Low,
                AiThinkingLevel::Medium,
                AiThinkingLevel::High,
            ],
            Some(AiThinkingLevel::Medium),
        )
    } else {
        (Vec::new(), None)
    };

    Ok(AiCustomModelConfig {
        id,
        display_name,
        thinking_levels,
        default_thinking,
        context_window_tokens,
        input_price_per_1m_usd,
        output_price_per_1m_usd,
    })
}
