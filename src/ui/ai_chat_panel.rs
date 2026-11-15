//! VS Code 風 AI チャットパネル状態管理、非同期ストリーミングポーリング、差分適用/1クリック Undo UI。

use std::sync::mpsc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{Context, IntoElement, ParentElement as _, Styled as _, div};

use crate::ai::context_builder::build_ai_request;
use crate::ai::planner::{AiEditPlan, dry_run_validate};
use crate::ai::plugin_host::AiPluginRegistry;
use crate::ai::provider::{
    AiChatMessage, AiError, AiProvider, AiProviderKind, AiRequest, AiRole, AiStreamEvent,
    SecretString,
};
use crate::ai::providers::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::ai::providers::credentials::resolve_api_key;
use crate::ai::providers::gemini::{GeminiConfig, GeminiProvider};
use crate::ai::providers::mock::MockAiProvider;
use crate::ai::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::ui::app_controller::AppController;
use crate::ui::window::SeaEditorWindow;

/// チャット履歴に表示する 1 メッセージ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiChatTurn {
    /// 発言者ロール。
    pub role: AiRole,
    /// 表示本文。
    pub text: String,
}

/// ドライラン済み・ユーザー承認待ちの編集差分提案。
#[derive(Debug, Clone)]
pub struct PendingAiProposal {
    /// 元のユーザー指示。
    pub prompt: String,
    /// 使用されたモデル名。
    pub model: String,
    /// ドライラン検証を通過した適用待ち計画。
    pub plan: AiEditPlan,
}

struct AiChatJobHandle {
    rx: mpsc::Receiver<AiStreamEvent>,
    prompt: String,
}

/// UI 状態と履歴を管理する AI チャットパネル状態。
pub struct AiChatState {
    /// 選択中 AI プロバイダ種別。
    pub selected_provider: AiProviderKind,
    /// 選択中のモデル ID。
    pub model_name: String,
    /// `OpenAI` 互換（Ollama 等）のベース URL。
    pub custom_base_url: String,
    /// 会話スレッド履歴。
    pub turns: Vec<AiChatTurn>,
    /// ストリーミング受信中の部分テキスト。
    pub streaming_preview: String,
    /// 上部ステータス文言。
    pub status_banner: String,
    /// 承認待ちの差分カード情報。
    pub pending_proposal: Option<PendingAiProposal>,
    /// 直近のエラー文言。
    pub last_error: Option<String>,
    /// 直前の承認適用を 1 クリック Undo できるか。
    pub can_one_click_undo: bool,
    runtime_api_key: Option<SecretString>,
    running_job: Option<AiChatJobHandle>,
}

impl std::fmt::Debug for AiChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiChatState")
            .field("selected_provider", &self.selected_provider)
            .field("model_name", &self.model_name)
            .field("turns_len", &self.turns.len())
            .field("has_runtime_key", &self.runtime_api_key.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for AiChatState {
    fn default() -> Self {
        let default_kind = AiProviderKind::OpenAi;
        Self {
            selected_provider: default_kind.clone(),
            model_name: default_kind.default_model().to_owned(),
            custom_base_url: default_kind.default_base_url().to_owned(),
            turns: Vec::new(),
            streaming_preview: String::new(),
            status_banner: "自然言語で編集指示を入力してください（適用前に差分を確認できます）"
                .to_owned(),
            pending_proposal: None,
            last_error: None,
            can_one_click_undo: false,
            runtime_api_key: None,
            running_job: None,
        }
    }
}

impl AiChatState {
    /// プロバイダ種別を切り替え、既定モデル・ベース URL を更新する。
    pub fn select_provider(&mut self, kind: AiProviderKind) {
        kind.default_model().clone_into(&mut self.model_name);
        kind.default_base_url()
            .clone_into(&mut self.custom_base_url);
        self.selected_provider = kind;
        self.last_error = None;
    }

    /// メモリ上の一時 API キーを設定または解除する（プロジェクト JSON には保存されない）。
    pub fn set_runtime_api_key(&mut self, key: Option<SecretString>) {
        self.runtime_api_key = key;
    }

    /// 現在選択中のプロバイダ向けに有効な認証情報が解決できるか返す。
    #[must_use]
    pub fn has_resolved_credentials(&self) -> bool {
        if !self.selected_provider.requires_api_key() {
            return true;
        }
        resolve_api_key(&self.selected_provider, self.runtime_api_key.as_ref()).is_some()
    }

    /// 非同期処理を実行中かどうか。
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.running_job.is_some()
    }

    /// 現在の UI 設定から [`AiProvider`] 実装を構築する。
    #[must_use]
    pub fn build_provider(&self) -> Box<dyn AiProvider> {
        let model = if self.model_name.trim().is_empty() {
            self.selected_provider.default_model().to_owned()
        } else {
            self.model_name.clone()
        };
        let key = self.runtime_api_key.clone();
        match &self.selected_provider {
            AiProviderKind::Mock => Box::new(MockAiProvider::with_responses([
                "{\"type\":\"batch\",\"commands\":[]}",
            ])),
            AiProviderKind::Plugin { plugin_id } => {
                let registry = AiPluginRegistry::discover_default();
                match registry.resolve_provider(plugin_id) {
                    Ok(provider) => Box::new(provider.with_model(model)),
                    Err(err) => Box::new(FailedPluginProvider {
                        id: plugin_id.clone(),
                        message: err.to_string(),
                    }),
                }
            }
            AiProviderKind::OpenAi => {
                let mut cfg = OpenAiCompatConfig::openai(model, key);
                if !self.custom_base_url.trim().is_empty() {
                    cfg.base_url.clone_from(&self.custom_base_url);
                }
                Box::new(OpenAiCompatProvider::new(cfg))
            }
            AiProviderKind::OpenRouter => {
                let mut cfg = OpenAiCompatConfig::openrouter(model, key);
                if !self.custom_base_url.trim().is_empty() {
                    cfg.base_url.clone_from(&self.custom_base_url);
                }
                Box::new(OpenAiCompatProvider::new(cfg))
            }
            AiProviderKind::OllamaCompat => {
                let base = if self.custom_base_url.trim().is_empty() {
                    AiProviderKind::OllamaCompat.default_base_url().to_owned()
                } else {
                    self.custom_base_url.clone()
                };
                Box::new(OpenAiCompatProvider::new(
                    OpenAiCompatConfig::local_compatible(base, model, key),
                ))
            }
            AiProviderKind::Gemini => Box::new(GeminiProvider::new(GeminiConfig::new(model, key))),
            AiProviderKind::Anthropic => {
                Box::new(AnthropicProvider::new(AnthropicConfig::new(model, key)))
            }
        }
    }

    fn recent_history(&self) -> Vec<AiChatMessage> {
        self.turns
            .iter()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|t| AiChatMessage {
                role: t.role,
                content: t.text.clone(),
            })
            .collect()
    }
}

impl AppController {
    /// 指定 [`AiProvider`] を使って同期的に AI 編集案を取得し、ドライラン検証済み提案カードとして保持する。
    ///
    /// # Errors
    ///
    /// 空指示、プロバイダ通信失敗、JSON 不正、またはタイムライン検証違反時にエラー文言を返す。
    pub fn submit_ai_prompt_with_provider(
        &mut self,
        prompt: &str,
        provider: &dyn AiProvider,
    ) -> Result<(), String> {
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Err("AI への指示文が空です".to_owned());
        }

        let history = self.ai_chat.recent_history();
        let req = build_ai_request(
            &self.document,
            self.clock.current_frame(),
            self.selected_clip_id.as_ref(),
            self.selected_layer_id.as_ref(),
            &history,
            trimmed,
        )
        .map_err(|err| err.to_string())?;

        self.ai_chat.turns.push(AiChatTurn {
            role: AiRole::User,
            text: trimmed.to_owned(),
        });
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.last_error = None;
        self.ai_chat.pending_proposal = None;
        self.ai_chat.can_one_click_undo = false;

        let mut streamed_buf = String::new();
        let mut status_buf = String::new();
        let response = provider
            .send_streaming(&req, &mut |evt| match evt {
                AiStreamEvent::Status(msg) => status_buf = msg,
                AiStreamEvent::DeltaText(delta) => streamed_buf.push_str(&delta),
                AiStreamEvent::Completed(_) | AiStreamEvent::Failed(_) => {}
            })
            .map_err(|err| {
                let msg = err.to_string();
                self.ai_chat.last_error = Some(msg.clone());
                self.ai_chat.status_banner = format!("AI 通信エラー: {msg}");
                msg
            })?;

        self.ai_chat.streaming_preview = streamed_buf;
        if !status_buf.is_empty() {
            self.ai_chat.status_banner = status_buf;
        }
        self.finalize_ai_response(trimmed, &response.model, &response.raw_text)
    }

    /// UI からの指示をバックグラウンドスレッドで非同期開始する。
    ///
    /// # Errors
    ///
    /// 空指示、既存リクエスト実行中、またはリクエスト生成失敗時。
    pub fn start_ai_prompt_async(&mut self, prompt: &str) -> Result<(), String> {
        if self.ai_chat.is_busy() {
            return Err("前の AI リクエストを処理中です".to_owned());
        }
        let trimmed = prompt.trim().to_owned();
        if trimmed.is_empty() {
            return Err("AI への指示文が空です".to_owned());
        }
        let history = self.ai_chat.recent_history();
        let req = build_ai_request(
            &self.document,
            self.clock.current_frame(),
            self.selected_clip_id.as_ref(),
            self.selected_layer_id.as_ref(),
            &history,
            &trimmed,
        )
        .map_err(|err| err.to_string())?;

        self.ai_chat.turns.push(AiChatTurn {
            role: AiRole::User,
            text: trimmed.clone(),
        });
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.last_error = None;
        self.ai_chat.pending_proposal = None;
        self.ai_chat.can_one_click_undo = false;
        "AI へ送信中...".clone_into(&mut self.ai_chat.status_banner);

        let provider = self.ai_chat.build_provider();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut forward = |evt: AiStreamEvent| {
                let _ = tx.send(evt);
            };
            if let Err(err) = provider.send_streaming(&req, &mut forward) {
                let _ = tx.send(AiStreamEvent::Failed(format_ai_error(&err)));
            }
        });
        self.ai_chat.running_job = Some(AiChatJobHandle {
            rx,
            prompt: trimmed,
        });
        Ok(())
    }

    /// 非同期 AI チャット受信キューをポーリングし、UI 更新が必要なら `true` を返す。
    pub fn poll_ai_chat(&mut self) -> bool {
        let Some(handle) = self.ai_chat.running_job.take() else {
            return false;
        };
        let mut changed = false;
        let mut completed = None;
        let mut failed = None;

        while let Ok(evt) = handle.rx.try_recv() {
            changed = true;
            match evt {
                AiStreamEvent::Status(msg) => self.ai_chat.status_banner = msg,
                AiStreamEvent::DeltaText(delta) => self.ai_chat.streaming_preview.push_str(&delta),
                AiStreamEvent::Completed(resp) => completed = Some(resp),
                AiStreamEvent::Failed(err) => failed = Some(err),
            }
        }

        if let Some(resp) = completed {
            let _ = self.finalize_ai_response(&handle.prompt, &resp.model, &resp.raw_text);
            return true;
        }
        if let Some(err_msg) = failed {
            self.ai_chat.last_error = Some(err_msg.clone());
            self.ai_chat.status_banner = format!("AI エラー: {err_msg}");
            return true;
        }

        self.ai_chat.running_job = Some(handle);
        changed
    }

    /// 承認待ちの編集提案をタイムラインへ 1 ステップ（単一 Undo 単位）で適用する。
    ///
    /// # Errors
    ///
    /// 承認待ち提案が存在しない、または現在ドキュメント上で整合しない場合。
    pub fn approve_pending_ai_proposal(&mut self) -> Result<(), String> {
        let proposal = self
            .ai_chat
            .pending_proposal
            .take()
            .ok_or_else(|| "承認待ちの AI 提案はありません".to_owned())?;
        if let Err(err) = self.dispatch_command(&proposal.plan.command) {
            let msg = format!("提案の適用に失敗しました: {err}");
            self.ai_chat.last_error = Some(msg.clone());
            return Err(msg);
        }
        self.ai_chat.can_one_click_undo = true;
        self.ai_chat.streaming_preview.clear();
        "AI 提案をタイムラインに適用しました（1クリック Undo 可能）"
            .clone_into(&mut self.ai_chat.status_banner);
        "AI 編集プランを適用しました".clone_into(&mut self.status_message);
        self.ai_chat.turns.push(AiChatTurn {
            role: AiRole::Assistant,
            text: format!("[適用済み] {}", proposal.plan.summary_lines.join(" / ")),
        });
        Ok(())
    }

    /// 承認待ちの AI 編集提案を適用せずに破棄する。
    pub fn discard_pending_ai_proposal(&mut self) {
        self.ai_chat.pending_proposal = None;
        self.ai_chat.streaming_preview.clear();
        "AI 提案を破棄しました".clone_into(&mut self.ai_chat.status_banner);
    }

    /// 直前に適用した AI 編集プランを 1 クリックで Undo する。
    ///
    /// # Errors
    ///
    /// Undo 履歴が存在しない場合。
    pub fn undo_last_ai_plan(&mut self) -> Result<(), String> {
        if !self.undo()? {
            return Err("Undo できる履歴がありません".to_owned());
        }
        self.ai_chat.can_one_click_undo = false;
        "直前の AI 編集を取り消しました（Undo）".clone_into(&mut self.ai_chat.status_banner);
        Ok(())
    }

    fn finalize_ai_response(
        &mut self,
        prompt: &str,
        model: &str,
        raw_text: &str,
    ) -> Result<(), String> {
        match dry_run_validate(&self.document, raw_text) {
            Ok(plan) => {
                let summary_preview = plan.summary_lines.join("\n");
                self.ai_chat.turns.push(AiChatTurn {
                    role: AiRole::Assistant,
                    text: summary_preview,
                });
                self.ai_chat.pending_proposal = Some(PendingAiProposal {
                    prompt: prompt.to_owned(),
                    model: model.to_owned(),
                    plan,
                });
                "ドライラン検証 OK: 「承認して適用」で実行できます"
                    .clone_into(&mut self.ai_chat.status_banner);
                Ok(())
            }
            Err(err) => {
                let msg = format!("コマンド検証エラー（タイムラインは無変更）: {err}");
                self.ai_chat.last_error = Some(msg.clone());
                self.ai_chat.turns.push(AiChatTurn {
                    role: AiRole::Assistant,
                    text: msg.clone(),
                });
                self.ai_chat.status_banner.clone_from(&msg);
                Err(msg)
            }
        }
    }
}

fn format_ai_error(err: &AiError) -> String {
    err.to_string()
}

struct FailedPluginProvider {
    id: String,
    message: String,
}

impl AiProvider for FailedPluginProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        &self.id
    }

    fn send_streaming(
        &self,
        _request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<crate::ai::provider::AiResponse, AiError> {
        on_event(AiStreamEvent::Failed(self.message.clone()));
        Err(AiError::PluginError(self.message.clone()))
    }
}

/// 右側 AI Chat タブの VS Code 風チャットパネルを描画する。
pub fn render_ai_chat_panel(
    controller: &AppController,
    ai_chat_input: &gpui_kit::Entity<InputState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let chat = &controller.ai_chat;
    let cred_label = if chat.has_resolved_credentials() {
        "認証: OK (Env/Keyring)".to_owned()
    } else if let Some(var) = chat.selected_provider.default_env_var() {
        format!("認証未設定: 環境変数 {var}")
    } else {
        "認証不要".to_owned()
    };

    let mut panel = v_flex()
        .size_full()
        .p_3()
        .gap_2()
        .text_xs()
        .child(render_provider_bar(&chat.selected_provider, cx))
        .child(
            h_flex()
                .justify_between()
                .text_color(theme.muted_foreground)
                .child(format!("Model: {}", chat.model_name))
                .child(cred_label),
        )
        .child(render_thread_box(chat, cx));

    if let Some(proposal) = &chat.pending_proposal {
        panel = panel.child(render_diff_card(proposal, cx));
    }
    if chat.can_one_click_undo {
        panel = panel.child(
            Button::new("ai-one-click-undo")
                .ghost()
                .small()
                .label("直前の AI 適用を取り消す (Undo)")
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.controller.undo_last_ai_plan();
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        );
    }
    if let Some(err) = &chat.last_error {
        panel = panel.child(div().text_color(theme.danger).child(err.clone()));
    }

    panel.child(render_prompt_bar(chat.is_busy(), ai_chat_input, cx))
}

fn render_provider_bar(
    selected: &AiProviderKind,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let options = [
        ("p-openai", "OpenAI", AiProviderKind::OpenAi),
        ("p-openrouter", "OpenRouter", AiProviderKind::OpenRouter),
        ("p-ollama", "Local", AiProviderKind::OllamaCompat),
        ("p-gemini", "Gemini", AiProviderKind::Gemini),
        ("p-claude", "Claude", AiProviderKind::Anthropic),
        (
            "p-agy",
            "Antigravity",
            AiProviderKind::Plugin {
                plugin_id: "sea.ai.antigravity".to_owned(),
            },
        ),
        (
            "p-acp",
            "ACP CLI",
            AiProviderKind::Plugin {
                plugin_id: "sea.ai.acp".to_owned(),
            },
        ),
    ];
    let mut row = h_flex().gap_1().flex_wrap();
    for (id, label, kind) in options {
        let is_active = *selected == kind;
        let btn = Button::new(id).small().label(label);
        let btn = if is_active {
            btn.primary()
        } else {
            btn.ghost()
        };
        row = row.child(btn.on_click(cx.listener(move |this, _, _, cx| {
            this.controller.ai_chat.select_provider(kind.clone());
            cx.notify();
        })));
    }
    row
}

fn render_thread_box(chat: &AiChatState, cx: &mut Context<SeaEditorWindow>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let mut box_el = v_flex()
        .flex_1()
        .p_2()
        .gap_2()
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .rounded_md()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(chat.status_banner.clone()),
        );

    for turn in chat.turns.iter().rev().take(6).rev() {
        let prefix = match turn.role {
            AiRole::User => "You: ",
            AiRole::Assistant => "AI: ",
            AiRole::System => "Sys: ",
        };
        let color = if turn.role == AiRole::User {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        box_el = box_el.child(
            div()
                .text_color(color)
                .child(format!("{prefix}{}", turn.text)),
        );
    }

    if chat.is_busy() && !chat.streaming_preview.is_empty() {
        let snippet: String = chat
            .streaming_preview
            .chars()
            .rev()
            .take(180)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        box_el = box_el.child(
            div()
                .text_color(theme.muted_foreground)
                .child(format!("受信中: {snippet}")),
        );
    }
    box_el
}

fn render_diff_card(
    proposal: &PendingAiProposal,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let mut card = v_flex()
        .p_2()
        .gap_1()
        .border_1()
        .border_color(theme.border)
        .rounded_md()
        .child(
            div()
                .text_color(theme.foreground)
                .child(format!("適用差分カード ({})", proposal.model)),
        );
    for line in proposal.plan.summary_lines.iter().take(6) {
        card = card.child(div().text_color(theme.muted_foreground).child(line.clone()));
    }
    card.child(
        h_flex()
            .gap_2()
            .child(
                Button::new("ai-apply-plan")
                    .primary()
                    .small()
                    .label("承認して適用")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.controller.approve_pending_ai_proposal();
                        this.sync_widgets_from_controller(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ai-discard-plan")
                    .ghost()
                    .small()
                    .label("破棄")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.discard_pending_ai_proposal();
                        cx.notify();
                    })),
            ),
    )
}

fn render_prompt_bar(
    is_busy: bool,
    ai_chat_input: &gpui_kit::Entity<InputState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let send_label = if is_busy { "送信中..." } else { "送信" };
    h_flex()
        .gap_2()
        .child(div().flex_1().child(Input::new(ai_chat_input)))
        .child(
            Button::new("ai-send-btn")
                .primary()
                .small()
                .label(send_label)
                .on_click(cx.listener(|this, _, window, cx| {
                    let msg = this.ai_chat_input.read(cx).value().to_string();
                    if !msg.trim().is_empty() && !this.controller.ai_chat.is_busy() {
                        if let Err(err) = this.controller.start_ai_prompt_async(&msg) {
                            this.controller.ai_chat.last_error = Some(err);
                        } else {
                            this.ai_chat_input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                            });
                        }
                    }
                    cx.notify();
                })),
        )
}
