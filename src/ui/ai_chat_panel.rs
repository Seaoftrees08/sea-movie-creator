//! VS Code 風 AI チャットパネル状態管理、非同期ストリーミングポーリング、差分適用/1クリック Undo UI。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectItem, SelectState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Side, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::{
    Anchor, Context, Entity, IntoElement, ParentElement as _, SharedString, Styled as _, Window,
    div, px,
};

use crate::ai::context_builder::{AiSelectionContext, build_ai_request_with_attachments};
use crate::ai::planner::{AiEditPlan, dry_run_validate};
use crate::ai::plugin_host::AiPluginRegistry;
use crate::ai::provider::{
    AiChatMessage, AiError, AiModelPreset, AiProvider, AiProviderKind, AiRequest, AiRole,
    AiStreamEvent, AiThinkingLevel, SecretString,
};
use crate::ai::providers::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::ai::providers::credentials::resolve_api_key;
use crate::ai::providers::gemini::{GeminiConfig, GeminiProvider};
use crate::ai::providers::mock::MockAiProvider;
use crate::ai::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::core::ids::ClipId;
use crate::timeline::document::ProjectDocument;
use crate::ui::app_controller::AppController;
use crate::ui::i18n::tr;
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

pub use crate::ai::provider::AiApprovalMode;

struct AiChatJobHandle {
    rx: mpsc::Receiver<AiStreamEvent>,
    prompt: String,
    cancelled: Arc<AtomicBool>,
}

/// API / プロバイダ切り替えコンボボックス用アイテム。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiProviderSelectItem {
    /// コンボボックスに表示する名称。
    pub title: SharedString,
    /// 選択時に適用されるプロバイダ種別。
    pub kind: AiProviderKind,
}

impl SelectItem for AiProviderSelectItem {
    type Value = AiProviderKind;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.kind
    }
}

/// API 切り替えコンボボックス初期化用のプロバイダ一覧を生成する。
#[must_use]
pub fn build_provider_select_items() -> Vec<AiProviderSelectItem> {
    AiProviderKind::ui_selectable_providers()
        .into_iter()
        .map(|kind| AiProviderSelectItem {
            title: kind.ui_combo_label().into(),
            kind,
        })
        .collect()
}

/// 選択中プロバイダに対応するコンボボックス上の `IndexPath` を返す。
#[must_use]
pub fn provider_select_index(kind: &AiProviderKind) -> Option<IndexPath> {
    AiProviderKind::ui_selectable_providers()
        .iter()
        .position(|candidate| candidate == kind)
        .map(IndexPath::new)
}

/// UI 状態と履歴を管理する AI チャットパネル状態。
pub struct AiChatState {
    /// 選択中 AI プロバイダ種別。
    pub selected_provider: AiProviderKind,
    /// 選択中のモデル ID。
    pub model_name: String,
    /// 選択中の思考・推論レベル (`Low` / `Medium` / `High` / `Extra` / `Auto`)。
    pub thinking_level: AiThinkingLevel,
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
    /// 提案の適用方法 (`Manual` / `Autopilot`)。
    pub approval_mode: AiApprovalMode,
    /// コンポーザへ添付されたタイムラインクリップ ID 一覧。
    pub attached_clip_ids: Vec<ClipId>,
    runtime_api_key: Option<SecretString>,
    running_job: Option<AiChatJobHandle>,
}

impl std::fmt::Debug for AiChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiChatState")
            .field("selected_provider", &self.selected_provider)
            .field("model_name", &self.model_name)
            .field("thinking_level", &self.thinking_level)
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
            model_name: default_kind.preferred_default_model().to_owned(),
            thinking_level: default_kind.default_thinking_level(),
            custom_base_url: default_kind.default_base_url().to_owned(),
            turns: Vec::new(),
            streaming_preview: String::new(),
            status_banner: "自然言語で編集指示を入力してください（適用前に差分を確認できます）"
                .to_owned(),
            pending_proposal: None,
            last_error: None,
            can_one_click_undo: false,
            approval_mode: AiApprovalMode::Manual,
            attached_clip_ids: Vec::new(),
            runtime_api_key: None,
            running_job: None,
        }
    }
}

impl AiChatState {
    /// タイムラインクリップを添付一覧へ追加する（重複は無視）。
    pub fn attach_clip(&mut self, clip_id: ClipId) {
        if !self.attached_clip_ids.contains(&clip_id) {
            self.attached_clip_ids.push(clip_id);
        }
    }

    /// 添付済みのクリップを一覧から外す。
    pub fn detach_clip(&mut self, clip_id: &ClipId) {
        self.attached_clip_ids.retain(|id| id != clip_id);
    }

    /// 添付クリップ一覧をすべて解除する。
    pub fn clear_attachments(&mut self) {
        self.attached_clip_ids.clear();
    }

    /// 現在ドキュメント上に存在しない添付クリップ ID を整理する。
    pub fn prune_attachments(&mut self, doc: &ProjectDocument) {
        self.attached_clip_ids
            .retain(|cid| doc.find_clip(cid).is_some());
    }
    /// 編集プランの承認モード (`Manual` / `Autopilot`) を切り替える。
    pub const fn set_approval_mode(&mut self, mode: AiApprovalMode) {
        self.approval_mode = mode;
    }

    /// プロバイダ種別を切り替え、既定モデル・思考レベル・ベース URL を更新する。
    pub fn select_provider(&mut self, kind: AiProviderKind) {
        kind.preferred_default_model()
            .clone_into(&mut self.model_name);
        self.thinking_level = kind.default_thinking_level();
        kind.default_base_url()
            .clone_into(&mut self.custom_base_url);
        self.selected_provider = kind;
        self.last_error = None;
    }

    /// モデルを選択し、対応していれば `Medium` を標準選択（非対応時はモデル既定）とする。
    pub fn select_model_default_medium(&mut self, model_id: &str) {
        self.select_model(model_id, AiThinkingLevel::Medium);
    }

    /// モデルと思考レベルを同時に選択し、そのモデルで無効なレベルは自動補正する。
    pub fn select_model(&mut self, model_id: &str, thinking: AiThinkingLevel) {
        model_id.clone_into(&mut self.model_name);
        self.thinking_level = match self.selected_provider.model_preset(model_id) {
            Some(preset) if preset.thinking_levels.is_empty() => AiThinkingLevel::Auto,
            Some(preset) if preset.thinking_levels.contains(&thinking) => thinking,
            Some(preset) => preset.default_thinking,
            None => thinking,
        };
        self.last_error = None;
    }

    /// 現在選択中のモデルに対して思考レベルを切り替える。
    pub fn select_thinking_level(&mut self, level: AiThinkingLevel) {
        let allowed = self.active_thinking_options();
        self.thinking_level = if allowed.is_empty() {
            AiThinkingLevel::Auto
        } else if allowed.contains(&level) {
            level
        } else {
            self.active_model_preset()
                .map_or(AiThinkingLevel::Auto, |preset| preset.default_thinking)
        };
        self.last_error = None;
    }

    /// 現在選択中モデルに対応するカタログ定義を返す。
    #[must_use]
    pub fn active_model_preset(&self) -> Option<&'static AiModelPreset> {
        self.selected_provider.model_preset(&self.model_name)
    }

    /// 現在選択中モデルで切り替え可能な思考レベル一覧を返す。
    #[must_use]
    pub fn active_thinking_options(&self) -> &'static [AiThinkingLevel] {
        self.active_model_preset()
            .map_or(&[], |preset| preset.thinking_levels)
    }

    /// UI 向けの短いモデル表示名（未登録モデルは ID そのまま）を返す。
    #[must_use]
    pub fn active_model_display_name(&self) -> String {
        self.active_model_preset().map_or_else(
            || self.model_name.clone(),
            |preset| preset.display_name.to_owned(),
        )
    }

    /// 思考レベルを含めた UI ボタン表示ラベル（例: `Gemini 4 Hydrogen Extra`）を返す。
    #[must_use]
    pub fn active_model_label(&self) -> String {
        let base = self.active_model_display_name();
        if self.active_thinking_options().is_empty() {
            return base;
        }
        match self.thinking_level.badge_label() {
            Some(level_badge) => format!("{base} {level_badge}"),
            None => base,
        }
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

    /// 会話履歴・提案差分・エラー状態をクリアし、新しいチャットを開始する。
    pub fn clear_conversation(&mut self) {
        if self.is_busy() {
            return;
        }
        self.turns.clear();
        self.streaming_preview.clear();
        self.pending_proposal = None;
        self.last_error = None;
        self.can_one_click_undo = false;
        self.attached_clip_ids.clear();
        "自然言語で編集指示を入力してください（適用前に差分を確認できます）"
            .clone_into(&mut self.status_banner);
    }

    /// 現在の UI 設定から [`AiProvider`] 実装を構築する。
    #[must_use]
    pub fn build_provider(&self) -> Box<dyn AiProvider> {
        let model = if self.model_name.trim().is_empty() {
            self.selected_provider.preferred_default_model().to_owned()
        } else {
            self.model_name.clone()
        };
        let thinking = self.thinking_level;
        let key = self.runtime_api_key.clone();
        match &self.selected_provider {
            AiProviderKind::Mock => Box::new(MockAiProvider::with_responses([
                "{\"type\":\"batch\",\"commands\":[]}",
            ])),
            AiProviderKind::Plugin { plugin_id } => {
                let registry = AiPluginRegistry::discover_default();
                match registry.resolve_provider(plugin_id) {
                    Ok(provider) => Box::new(
                        provider
                            .with_model(model)
                            .with_thinking_level(thinking.acp_thought_level().map(str::to_owned)),
                    ),
                    Err(err) => Box::new(FailedPluginProvider {
                        id: plugin_id.clone(),
                        message: err.to_string(),
                    }),
                }
            }
            AiProviderKind::OpenAi => {
                let mut cfg = OpenAiCompatConfig::openai(model, key).with_thinking_level(thinking);
                if !self.custom_base_url.trim().is_empty() {
                    cfg.base_url.clone_from(&self.custom_base_url);
                }
                Box::new(OpenAiCompatProvider::new(cfg))
            }
            AiProviderKind::OpenRouter => {
                let mut cfg =
                    OpenAiCompatConfig::openrouter(model, key).with_thinking_level(thinking);
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
                    OpenAiCompatConfig::local_compatible(base, model, key)
                        .with_thinking_level(thinking),
                ))
            }
            AiProviderKind::Gemini => Box::new(GeminiProvider::new(
                GeminiConfig::new(model, key).with_thinking_level(thinking),
            )),
            AiProviderKind::Anthropic => Box::new(AnthropicProvider::new(
                AnthropicConfig::new(model, key).with_thinking_level(thinking),
            )),
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
    fn prepare_ai_request_and_turn(&mut self, trimmed: &str) -> Result<AiRequest, String> {
        self.ai_chat.prune_attachments(&self.document);
        let attached_ids = self.ai_chat.attached_clip_ids.clone();
        let attached_labels: Vec<String> = attached_ids
            .iter()
            .filter_map(|cid| {
                self.document.find_clip(cid).map(|(_, clip)| {
                    let s = clip.timeline_start.get();
                    let e = s.saturating_add(clip.duration.get());
                    format!("{} ({s}..{e}f)", clip.name)
                })
            })
            .collect();

        let history = self.ai_chat.recent_history();
        let req = build_ai_request_with_attachments(
            &self.document,
            self.clock.current_frame(),
            AiSelectionContext {
                selected_clip: self.selected_clip_id.as_ref(),
                selected_layer: self.selected_layer_id.as_ref(),
                attached_clips: &attached_ids,
            },
            &history,
            trimmed,
        )
        .map_err(|err| err.to_string())?;

        let display_turn = if attached_labels.is_empty() {
            trimmed.to_owned()
        } else {
            format!("[添付: {}]\n{trimmed}", attached_labels.join(", "))
        };
        self.ai_chat.turns.push(AiChatTurn {
            role: AiRole::User,
            text: display_turn,
        });
        self.ai_chat.attached_clip_ids.clear();
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.last_error = None;
        self.ai_chat.pending_proposal = None;
        self.ai_chat.can_one_click_undo = false;
        Ok(req)
    }

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
        let req = self.prepare_ai_request_and_turn(trimmed)?;

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
        let req = self.prepare_ai_request_and_turn(&trimmed)?;
        "AI へ送信中...".clone_into(&mut self.ai_chat.status_banner);

        let provider = self.ai_chat.build_provider();
        let (tx, rx) = mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel_flag);
        std::thread::spawn(move || {
            let mut forward = |evt: AiStreamEvent| {
                if !worker_cancel.load(Ordering::Acquire) {
                    let _ = tx.send(evt);
                }
            };
            if let Err(err) = provider.send_streaming(&req, &mut forward)
                && !worker_cancel.load(Ordering::Acquire)
            {
                let _ = tx.send(AiStreamEvent::Failed(format_ai_error(&err)));
            }
        });
        self.ai_chat.running_job = Some(AiChatJobHandle {
            rx,
            prompt: trimmed,
            cancelled: cancel_flag,
        });
        Ok(())
    }

    /// 実行中の AI リクエストを中断し、再送信できる状態へ戻す（中断時 `true`）。
    pub fn cancel_ai_prompt(&mut self) -> bool {
        let Some(handle) = self.ai_chat.running_job.take() else {
            return false;
        };
        handle.cancelled.store(true, Ordering::Release);
        drop(handle);

        let partial = self.ai_chat.streaming_preview.trim().to_owned();
        self.ai_chat.streaming_preview.clear();
        if !partial.is_empty() {
            self.ai_chat.turns.push(AiChatTurn {
                role: AiRole::Assistant,
                text: format!("[停止済み]\n{partial}"),
            });
        }
        "AI 生成を停止しました".clone_into(&mut self.ai_chat.status_banner);
        "AI 生成を停止しました".clone_into(&mut self.status_message);
        true
    }

    /// 非同期 AI チャット受信キューをポーリングし、UI 更新が必要なら `true` を返す。
    pub fn poll_ai_chat(&mut self) -> bool {
        let Some(handle) = self.ai_chat.running_job.take() else {
            return false;
        };
        let mut changed = false;
        let mut completed = None;
        let mut failed = None;
        let mut disconnected = false;

        loop {
            match handle.rx.try_recv() {
                Ok(evt) => {
                    changed = true;
                    match evt {
                        AiStreamEvent::Status(msg) => self.ai_chat.status_banner = msg,
                        AiStreamEvent::DeltaText(delta) => {
                            self.ai_chat.streaming_preview.push_str(&delta);
                        }
                        AiStreamEvent::Completed(resp) => completed = Some(resp),
                        AiStreamEvent::Failed(err) => failed = Some(err),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
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
        if disconnected {
            let err_msg = "AI プロバイダが応答を返さず終了しました".to_owned();
            self.ai_chat.last_error = Some(err_msg.clone());
            self.ai_chat.status_banner = err_msg;
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
        self.apply_ai_plan(&proposal.plan, false)
    }

    fn apply_ai_plan(&mut self, plan: &AiEditPlan, is_autopilot: bool) -> Result<(), String> {
        if let Err(err) = self.dispatch_command(&plan.command) {
            let msg = format!("提案の適用に失敗しました: {err}");
            self.ai_chat.last_error = Some(msg.clone());
            self.ai_chat.turns.push(AiChatTurn {
                role: AiRole::Assistant,
                text: msg.clone(),
            });
            self.ai_chat.status_banner.clone_from(&msg);
            return Err(msg);
        }
        self.inspector.invalidate_sync();
        self.ai_chat.can_one_click_undo = true;
        self.ai_chat.streaming_preview.clear();
        let (banner, status, prefix) = if is_autopilot {
            (
                "Autopilot: AI 提案を自動適用しました（1クリック Undo 可能）",
                "AI 編集プランを自動適用しました",
                "[Autopilot 適用済み]",
            )
        } else {
            (
                "AI 提案をタイムラインに適用しました（1クリック Undo 可能）",
                "AI 編集プランを適用しました",
                "[適用済み]",
            )
        };
        banner.clone_into(&mut self.ai_chat.status_banner);
        status.clone_into(&mut self.status_message);
        self.ai_chat.turns.push(AiChatTurn {
            role: AiRole::Assistant,
            text: format!("{prefix} {}", plan.summary_lines.join(" / ")),
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
        self.inspector.invalidate_sync();
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
            Ok(plan) if plan.is_no_op() => {
                let reply_text = plan
                    .reply_note
                    .clone()
                    .unwrap_or_else(|| "応答を受信しました（タイムライン変更なし）".to_owned());
                self.ai_chat.last_error = None;
                self.ai_chat.pending_proposal = None;
                self.ai_chat.turns.push(AiChatTurn {
                    role: AiRole::Assistant,
                    text: reply_text,
                });
                "応答を受信しました（タイムライン変更なし）"
                    .clone_into(&mut self.ai_chat.status_banner);
                Ok(())
            }
            Ok(plan) => {
                self.ai_chat.last_error = None;
                if self.ai_chat.approval_mode == AiApprovalMode::Autopilot {
                    self.ai_chat.pending_proposal = None;
                    return self.apply_ai_plan(&plan, true);
                }
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

fn provider_safety_notice(provider: &AiProviderKind) -> Option<&'static str> {
    match provider {
        AiProviderKind::Plugin { plugin_id } if plugin_id == "sea.ai.antigravity" => Some(
            "注意: 公式 agy acp (--sandbox) で実行します。高頻度な連続送信・自動実行はアカウント制限の対象となる恐れがあるため、高頻度用途では Gemini API キー方式をご検討ください。",
        ),
        _ => None,
    }
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
///
/// 上部はプロバイダ切替と認証状態のみのシンプルなヘッダとし、会話領域を下側へ伸縮させ、
/// モデル／思考レベル選択を内包した単一の入力コンポーザーを最下部に固定配置する。
pub fn render_ai_chat_panel(
    controller: &AppController,
    ai_provider_select: &Entity<SelectState<Vec<AiProviderSelectItem>>>,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let chat = &controller.ai_chat;

    v_flex()
        .size_full()
        .min_h(px(0.))
        .text_xs()
        .child(render_compact_header(chat, ai_provider_select, cx))
        .child(render_chat_body(chat, cx))
        .child(render_prompt_composer(controller, ai_chat_input, cx))
}

fn format_cred_label(chat: &AiChatState) -> String {
    if chat.has_resolved_credentials() {
        "認証: OK".to_owned()
    } else if let Some(var) = chat.selected_provider.default_env_var() {
        format!("未設定: {var}")
    } else {
        "認証不要".to_owned()
    }
}

fn render_compact_header(
    chat: &AiChatState,
    ai_provider_select: &Entity<SelectState<Vec<AiProviderSelectItem>>>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let cred_color = if chat.has_resolved_credentials() {
        theme.muted_foreground
    } else {
        theme.danger
    };

    let mut right_actions = h_flex()
        .items_center()
        .gap_1()
        .flex_shrink_0()
        .child(div().text_color(cred_color).child(format_cred_label(chat)));
    if !chat.turns.is_empty() {
        right_actions = right_actions.child(
            Button::new("ai-new-chat")
                .ghost()
                .xsmall()
                .disabled(chat.is_busy())
                .label("+")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.ai_chat.clear_conversation();
                    cx.notify();
                })),
        );
    }

    h_flex()
        .w_full()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1p5()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div().flex_1().min_w(px(90.)).child(
                Select::new(ai_provider_select)
                    .menu_width(px(230.))
                    .with_size(Size::XSmall),
            ),
        )
        .child(right_actions)
}

fn render_chat_body(chat: &AiChatState, cx: &mut Context<SeaEditorWindow>) -> gpui_kit::AnyElement {
    use gpui_kit::IntoElement as _;

    let theme = cx.theme().clone();
    let is_idle_empty = chat.turns.is_empty()
        && !chat.is_busy()
        && chat.pending_proposal.is_none()
        && chat.last_error.is_none()
        && !chat.can_one_click_undo;

    if is_idle_empty {
        let mut welcome = v_flex()
            .flex_1()
            .w_full()
            .min_h(px(0.))
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .py_4()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child("AI 編集アシスタント"),
            )
            .child(
                div()
                    .text_center()
                    .text_color(theme.muted_foreground)
                    .child(chat.status_banner.clone()),
            );
        if let Some(notice) = provider_safety_notice(&chat.selected_provider) {
            welcome = welcome.child(
                div()
                    .mt_2()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .text_color(theme.muted_foreground)
                    .child(notice),
            );
        }
        return welcome.into_any_element();
    }

    let mut thread = v_flex().w_full().gap_2().px_2().py_2();
    for turn in chat.turns.iter().rev().take(12).rev() {
        thread = thread.child(render_chat_turn(turn, &theme));
    }
    if chat.is_busy() {
        let label = if chat.streaming_preview.is_empty() {
            "受信中...".to_owned()
        } else {
            let snippet: String = chat
                .streaming_preview
                .chars()
                .rev()
                .take(180)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            format!("受信中: {snippet}")
        };
        thread = thread.child(div().px_1().text_color(theme.muted_foreground).child(label));
    }
    if let Some(proposal) = &chat.pending_proposal {
        thread = thread.child(render_diff_card(proposal, cx));
    }
    if chat.can_one_click_undo {
        thread = thread.child(
            Button::new("ai-one-click-undo")
                .ghost()
                .xsmall()
                .label(tr("ai.undo_last"))
                .on_click(cx.listener(|this, _, _, cx| {
                    let _ = this.controller.undo_last_ai_plan();
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        );
    }
    if let Some(err) = &chat.last_error {
        thread = thread.child(
            div()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.danger)
                .text_color(theme.danger)
                .child(err.clone()),
        );
    }

    div()
        .flex_1()
        .w_full()
        .min_h(px(0.))
        .overflow_y_scrollbar()
        .child(thread)
        .into_any_element()
}

fn render_chat_turn(turn: &AiChatTurn, theme: &gpui_kit::component::Theme) -> gpui_kit::AnyElement {
    use gpui_kit::IntoElement as _;

    match turn.role {
        AiRole::User => div()
            .w_full()
            .p_2()
            .rounded_md()
            .bg(theme.secondary)
            .text_color(theme.foreground)
            .child(turn.text.clone())
            .into_any_element(),
        AiRole::Assistant => v_flex()
            .w_full()
            .gap_0p5()
            .px_1()
            .child(div().text_color(theme.muted_foreground).child("AI"))
            .child(div().text_color(theme.foreground).child(turn.text.clone()))
            .into_any_element(),
        AiRole::System => div()
            .w_full()
            .px_1()
            .text_color(theme.muted_foreground)
            .child(turn.text.clone())
            .into_any_element(),
    }
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
                    .label(tr("ai.approve"))
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
                    .label(tr("ai.discard"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.discard_pending_ai_proposal();
                        cx.notify();
                    })),
            ),
    )
}

fn format_model_menu_label(
    preset: &AiModelPreset,
    is_selected: bool,
    current_thinking: AiThinkingLevel,
) -> String {
    let mut parts = vec![preset.display_name.to_owned()];
    if !preset.thinking_levels.is_empty() {
        let displayed_level = if is_selected {
            current_thinking
        } else if preset.thinking_levels.contains(&AiThinkingLevel::Medium) {
            AiThinkingLevel::Medium
        } else {
            preset.default_thinking
        };
        parts.push(displayed_level.display_label().to_owned());
    }
    if let Some(badge) = preset.badge {
        parts.push(format!("[{badge}]"));
    }
    parts.join("  ")
}

fn render_model_menu_button(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let catalog = chat.selected_provider.model_catalog();
    let current_model = chat.model_name.clone();
    let current_thinking = chat.thinking_level;
    let trigger_label = format!("{} ▾", chat.active_model_display_name());

    Button::new("ai-model-picker")
        .ghost()
        .xsmall()
        .label(trigger_label)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut popup = menu
                .min_w(px(230.))
                .max_h(px(340.))
                .scrollable(true)
                .check_side(Side::Right)
                .label("Model (クリックで Medium 標準選択)");

            for preset in catalog {
                let is_selected = preset.id == current_model;
                let row_label = format_model_menu_label(preset, is_selected, current_thinking);
                let model_id = preset.id;
                let weak_item = weak.clone();
                popup = popup.item(PopupMenuItem::new(row_label).checked(is_selected).on_click(
                    move |_, _, cx| {
                        let _ = weak_item.update(cx, |view, cx| {
                            view.controller
                                .ai_chat
                                .select_model_default_medium(model_id);
                            cx.notify();
                        });
                    },
                ));
            }
            popup
        })
}

fn render_thinking_menu_button(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<impl IntoElement> {
    let options = chat.active_thinking_options();
    if options.is_empty() {
        return None;
    }
    let weak = cx.weak_entity();
    let current = chat.thinking_level;
    let label = format!("{} ▾", current.display_label());

    Some(
        Button::new("ai-thinking-picker")
            .ghost()
            .xsmall()
            .label(label)
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
                let mut popup = menu
                    .min_w(px(136.))
                    .check_side(Side::Right)
                    .label("Thinking Level");
                for &level in options {
                    let weak_item = weak.clone();
                    popup = popup.item(
                        PopupMenuItem::new(level.display_label())
                            .checked(level == current)
                            .on_click(move |_, _, cx| {
                                let _ = weak_item.update(cx, |view, cx| {
                                    view.controller.ai_chat.select_thinking_level(level);
                                    cx.notify();
                                });
                            }),
                    );
                }
                popup
            }),
    )
}

fn render_approval_mode_menu_button(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let current = chat.approval_mode;
    let label = format!("{} ▾", current.display_label());

    let btn = Button::new("ai-approval-mode-picker")
        .ghost()
        .xsmall()
        .label(label);
    btn.dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
        let mut popup = menu
            .min_w(px(210.))
            .check_side(Side::Right)
            .label("適用モード");
        for mode in [AiApprovalMode::Manual, AiApprovalMode::Autopilot] {
            let weak_item = weak.clone();
            popup = popup.item(
                PopupMenuItem::new(mode.menu_label())
                    .checked(mode == current)
                    .on_click(move |_, _, cx| {
                        let _ = weak_item.update(cx, |view, cx| {
                            view.controller.ai_chat.set_approval_mode(mode);
                            cx.notify();
                        });
                    }),
            );
        }
        popup
    })
}

/// ファイル名・クリップ名を `@<名前> ` 形式のメンション文字列へ整形する。
#[must_use]
pub fn format_at_mention_token(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.contains(char::is_whitespace) {
        format!("@\"{trimmed}\" ")
    } else {
        format!("@{trimmed} ")
    }
}

/// 入力末尾に補完中の `@<検索語>` トークンがあれば `(開始バイト位置, 検索語)` を返す。
#[must_use]
pub fn find_trailing_at_query(input: &str) -> Option<(usize, String)> {
    let at_pos = input.rfind('@')?;
    if at_pos > 0 {
        let prev = input[..at_pos].chars().next_back()?;
        if prev.is_alphanumeric() {
            return None;
        }
    }
    let after = &input[at_pos + 1..];
    let query = after.strip_prefix('"').unwrap_or(after);
    if after.starts_with('"') {
        if query.contains('"') || query.contains('\n') {
            return None;
        }
        return Some((at_pos, query.to_owned()));
    }
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at_pos, query.to_owned()))
}

/// プロジェクト内の全素材・クリップから補完候補名一覧（重複除去済み）を抽出する。
#[must_use]
pub fn matching_at_mention_names(doc: &ProjectDocument, query: &str) -> Vec<String> {
    let q_lower = query.trim().to_ascii_lowercase();
    let mut names: Vec<String> = Vec::new();
    for asset in &doc.assets {
        if !asset.name.trim().is_empty() && !names.contains(&asset.name) {
            names.push(asset.name.clone());
        }
    }
    for layer in &doc.layers {
        for clip in &layer.clips {
            if !clip.name.trim().is_empty() && !names.contains(&clip.name) {
                names.push(clip.name.clone());
            }
        }
    }
    names
        .into_iter()
        .filter(|n| q_lower.is_empty() || n.to_ascii_lowercase().contains(&q_lower))
        .collect()
}

/// 末尾の `@<検索語>` を選択された候補文字列 `@<対象> ` へ置換する。
#[must_use]
pub fn apply_at_completion(current_text: &str, at_byte_start: usize, target_name: &str) -> String {
    let prefix = current_text.get(..at_byte_start).unwrap_or(current_text);
    format!("{prefix}{}", format_at_mention_token(target_name))
}

fn insert_at_mention_into_input(
    ai_chat_input: &Entity<TextareaState>,
    target_name: &str,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let token = format_at_mention_token(target_name);
    ai_chat_input.update(cx, |state, cx| {
        let cur = state.value().to_string();
        let next = if let Some((at_pos, _)) = find_trailing_at_query(&cur) {
            apply_at_completion(&cur, at_pos, target_name)
        } else if cur.is_empty() || cur.ends_with(char::is_whitespace) {
            format!("{cur}{token}")
        } else {
            format!("{cur} {token}")
        };
        state.set_value(&next, window, cx);
        state.focus(window, cx);
    });
    cx.notify();
}

fn render_attach_context_button(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let selected_ids = controller.effective_selected_clip_ids();
    let selected_items: Vec<(ClipId, String)> = selected_ids
        .iter()
        .filter_map(|cid| {
            controller
                .document
                .find_clip(cid)
                .map(|(_, c)| (cid.clone(), c.name.clone()))
        })
        .collect();
    let attached_set = controller.ai_chat.attached_clip_ids.clone();
    let all_clips: Vec<(ClipId, String, String, u64, u64)> = controller
        .document
        .layers
        .iter()
        .flat_map(|layer| {
            layer.clips.iter().map(move |clip| {
                let s = clip.timeline_start.get();
                let e = s.saturating_add(clip.duration.get());
                (clip.id.clone(), clip.name.clone(), layer.name.clone(), s, e)
            })
        })
        .collect();

    Button::new("ai-attach-context-btn")
        .ghost()
        .xsmall()
        .label("📎 添付 ▾")
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut popup = menu
                .min_w(px(230.))
                .max_h(px(300.))
                .scrollable(true)
                .check_side(Side::Right);

            if !selected_items.is_empty() {
                let names: String = selected_items
                    .iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let sel_cids: Vec<ClipId> =
                    selected_items.iter().map(|(id, _)| id.clone()).collect();
                let weak_sel = weak.clone();
                popup = popup
                    .label("選択中オブジェクト")
                    .item(
                        PopupMenuItem::new(format!("選択クリップを添付: {names}")).on_click(
                            move |_, _, cx| {
                                let _ = weak_sel.update(cx, |view, cx| {
                                    for cid in &sel_cids {
                                        view.controller.ai_chat.attach_clip(cid.clone());
                                    }
                                    cx.notify();
                                });
                            },
                        ),
                    )
                    .separator();
            }

            popup = popup.label("タイムラインのクリップを添付");
            if all_clips.is_empty() {
                popup = popup.label("（クリップなし）");
            } else {
                for (cid, cname, lname, s, e) in &all_clips {
                    let weak_clip = weak.clone();
                    let is_attached = attached_set.contains(cid);
                    let target_cid = cid.clone();
                    popup = popup.item(
                        PopupMenuItem::new(format!("{cname} · {lname} ({s}..{e}f)"))
                            .checked(is_attached)
                            .on_click(move |_, _, cx| {
                                let _ = weak_clip.update(cx, |view, cx| {
                                    if is_attached {
                                        view.controller.ai_chat.detach_clip(&target_cid);
                                    } else {
                                        view.controller.ai_chat.attach_clip(target_cid.clone());
                                    }
                                    cx.notify();
                                });
                            }),
                    );
                }
            }
            popup
        })
}

fn render_at_mention_button(
    controller: &AppController,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let input_entity = ai_chat_input.clone();
    let names = matching_at_mention_names(&controller.document, "");

    Button::new("ai-at-mention-btn")
        .ghost()
        .xsmall()
        .label("@ ファイル ▾")
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
            let mut popup = menu
                .min_w(px(210.))
                .max_h(px(280.))
                .scrollable(true)
                .label("@<ファイル名> を挿入");
            if names.is_empty() {
                popup = popup.label("（素材・クリップなし）");
            } else {
                for name in &names {
                    let weak_item = weak.clone();
                    let input_item = input_entity.clone();
                    let name_owned = name.clone();
                    popup = popup.item(PopupMenuItem::new(format!("@{name}")).on_click(
                        move |_, window, cx| {
                            let _ = weak_item.update(cx, |_, cx| {
                                insert_at_mention_into_input(&input_item, &name_owned, window, cx);
                            });
                        },
                    ));
                }
            }
            popup
        })
}

fn render_composer_context_strip(
    controller: &AppController,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let mut row = h_flex()
        .w_full()
        .items_center()
        .gap_1()
        .flex_wrap()
        .child(render_attach_context_button(controller, cx))
        .child(render_at_mention_button(controller, ai_chat_input, cx));

    for (idx, cid) in controller.ai_chat.attached_clip_ids.iter().enumerate() {
        let Some((_, clip)) = controller.document.find_clip(cid) else {
            continue;
        };
        let detach_id = cid.clone();
        row = row.child(
            Button::new(("ai-attached-chip", idx))
                .outline()
                .xsmall()
                .label(format!("📎 {} ×", clip.name))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.ai_chat.detach_clip(&detach_id);
                    cx.notify();
                })),
        );
    }

    div()
        .w_full()
        .pb_0p5()
        .border_color(theme.border)
        .child(row)
}

fn render_at_autocomplete_bar(
    controller: &AppController,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<impl IntoElement> {
    let raw = ai_chat_input.read(cx).value().to_string();
    let (at_pos, query) = find_trailing_at_query(&raw)?;
    let matches = matching_at_mention_names(&controller.document, &query);
    if matches.is_empty() {
        return None;
    }

    let theme = cx.theme().clone();
    let input_entity = ai_chat_input.clone();
    let mut bar = h_flex()
        .w_full()
        .gap_1()
        .p_1()
        .mb_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .flex_wrap();
    for (idx, name) in matches.into_iter().take(6).enumerate() {
        let inp = input_entity.clone();
        let cur = raw.clone();
        bar = bar.child(
            Button::new(("ai-at-suggest", idx))
                .ghost()
                .xsmall()
                .label(format!("@{name}"))
                .on_click(cx.listener(move |_, _, window, cx| {
                    let next = apply_at_completion(&cur, at_pos, &name);
                    inp.update(cx, |state, cx| {
                        state.set_value(&next, window, cx);
                        state.focus(window, cx);
                    });
                    cx.notify();
                })),
        );
    }
    Some(bar)
}

fn render_prompt_composer(
    controller: &AppController,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let chat = &controller.ai_chat;

    let mut selectors = h_flex()
        .items_center()
        .gap_0p5()
        .flex_1()
        .min_w_0()
        .flex_wrap()
        .child(render_model_menu_button(chat, cx));
    if let Some(thinking_btn) = render_thinking_menu_button(chat, cx) {
        selectors = selectors.child(thinking_btn);
    }
    selectors = selectors.child(render_approval_mode_menu_button(chat, cx));

    let action_btn = if chat.is_busy() {
        Button::new("ai-stop-btn")
            .danger()
            .xsmall()
            .label(tr("ai.stop"))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.controller.cancel_ai_prompt() {
                    cx.notify();
                }
            }))
    } else {
        Button::new("ai-send-btn")
            .primary()
            .xsmall()
            .label(tr("ai.send"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.submit_ai_chat_input(window, cx);
            }))
    };

    let card = v_flex()
        .gap_1p5()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(render_composer_context_strip(controller, ai_chat_input, cx))
        .child(Textarea::new(ai_chat_input).appearance(false).small())
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_1()
                .child(selectors)
                .child(action_btn),
        );

    let mut outer = v_flex().w_full().px_2().pb_2().pt_1().flex_shrink_0();
    if let Some(ac_bar) = render_at_autocomplete_bar(controller, ai_chat_input, cx) {
        outer = outer.child(ac_bar);
    }
    outer.child(card)
}

#[cfg(test)]
mod tests {
    use super::{
        AiApprovalMode, AiChatState, apply_at_completion, build_provider_select_items,
        find_trailing_at_query, format_at_mention_token, format_model_menu_label,
        matching_at_mention_names, provider_safety_notice, provider_select_index,
    };
    use crate::ai::provider::{AiProviderKind, AiThinkingLevel};
    use crate::ai::providers::mock::MockAiProvider;
    use crate::core::ids::ClipId;
    use crate::ui::app_controller::AppController;

    #[test]
    fn antigravity_provider_exposes_safety_notice() {
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        let notice = provider_safety_notice(&agy);
        assert!(notice.is_some());
        assert!(notice.is_some_and(|text| text.contains("agy acp") && text.contains("Gemini")));
        assert_eq!(provider_safety_notice(&AiProviderKind::Gemini), None);
    }

    #[test]
    fn provider_combo_items_and_model_thinking_state_transitions_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let items = build_provider_select_items();
        assert_eq!(items.len(), 7);

        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        let agy_idx = provider_select_index(&agy).ok_or("missing Antigravity index")?;
        assert_eq!(agy_idx.row, 5);

        let mut chat = AiChatState::default();
        chat.select_provider(agy.clone());
        assert_eq!(chat.model_name, "gemini-4-argon");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Medium);
        assert_eq!(chat.active_model_label(), "Gemini 4 Argon Medium");

        chat.select_model("gemini-4-hydrogen", AiThinkingLevel::Extra);
        assert_eq!(chat.model_name, "gemini-4-hydrogen");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Extra);
        assert_eq!(chat.active_model_label(), "Gemini 4 Hydrogen Extra");

        // Extra 非対応モデルでは既定思考レベルに補正される
        chat.select_model("gemini-4-argon", AiThinkingLevel::Extra);
        assert_eq!(chat.thinking_level, AiThinkingLevel::Medium);

        // 思考レベルを持たないモデル (Orchestrator) へ切替時は Auto に戻り選択肢も空になる
        chat.select_model("gemini-pro-orchestrator-g1-pro", AiThinkingLevel::High);
        assert_eq!(chat.thinking_level, AiThinkingLevel::Auto);
        assert_eq!(chat.active_thinking_options(), []);
        assert_eq!(chat.active_model_label(), "Orchestrator (Argon)");

        chat.select_thinking_level(AiThinkingLevel::High);
        assert_eq!(chat.thinking_level, AiThinkingLevel::Auto);

        let hydrogen = agy
            .model_preset("gemini-4-hydrogen")
            .ok_or("hydrogen preset missing")?;
        let formatted = format_model_menu_label(hydrogen, true, AiThinkingLevel::Extra);
        assert!(formatted.contains("Gemini 4 Hydrogen"));
        assert!(formatted.contains("Extra"));
        assert!(formatted.contains("[New]"));
        Ok(())
    }

    #[test]
    fn clear_conversation_resets_turns_and_preserves_model_selection() {
        let mut chat = AiChatState::default();
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        chat.select_provider(agy.clone());
        chat.select_model("gemini-4-hydrogen", AiThinkingLevel::Extra);
        chat.turns.push(super::AiChatTurn {
            role: crate::ai::provider::AiRole::User,
            text: "テロップを追加して".to_owned(),
        });
        chat.last_error = Some("temporary error".to_owned());
        chat.can_one_click_undo = true;
        chat.streaming_preview = "partial".to_owned();

        chat.clear_conversation();

        assert_eq!(chat.turns, Vec::new());
        assert_eq!(chat.last_error, None);
        assert!(!chat.can_one_click_undo);
        assert_eq!(chat.streaming_preview, "");
        assert_eq!(chat.selected_provider, agy);
        assert_eq!(chat.model_name, "gemini-4-hydrogen");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Extra);
    }

    #[test]
    fn model_click_defaults_to_medium_when_supported() -> Result<(), Box<dyn std::error::Error>> {
        let mut chat = AiChatState::default();
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        chat.select_provider(agy.clone());
        chat.select_model_default_medium("gemini-4-hydrogen");
        assert_eq!(chat.model_name, "gemini-4-hydrogen");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Medium);

        let hydrogen = agy
            .model_preset("gemini-4-hydrogen")
            .ok_or("hydrogen preset missing")?;
        let unselected_label = format_model_menu_label(hydrogen, false, AiThinkingLevel::Extra);
        assert!(unselected_label.contains("Medium"));

        chat.select_model_default_medium("gemini-pro-orchestrator-g1-pro");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Auto);
        Ok(())
    }

    #[test]
    fn autopilot_mode_applies_plan_automatically_and_supports_one_click_undo()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = AppController::new();
        assert_eq!(controller.ai_chat.approval_mode, AiApprovalMode::Manual);
        controller
            .ai_chat
            .set_approval_mode(AiApprovalMode::Autopilot);

        let provider =
            MockAiProvider::with_responses([r#"{"type":"add_layer","name":"AI Layer"}"#]);
        controller.submit_ai_prompt_with_provider("新しいトラックを追加", &provider)?;

        assert_eq!(controller.document.layers.len(), 1);
        assert_eq!(controller.document.layers[0].name, "AI Layer");
        assert!(controller.ai_chat.pending_proposal.is_none());
        assert!(controller.ai_chat.can_one_click_undo);
        assert!(
            controller
                .ai_chat
                .turns
                .last()
                .is_some_and(|t| t.text.contains("[Autopilot 適用済み]"))
        );

        controller.undo_last_ai_plan()?;
        assert_eq!(controller.document.layers.len(), 0);
        assert!(!controller.ai_chat.can_one_click_undo);
        Ok(())
    }

    #[test]
    fn cancel_ai_prompt_clears_busy_state_and_records_partial_output() {
        let mut controller = AppController::new();
        assert!(!controller.cancel_ai_prompt());

        let (tx, rx) = std::sync::mpsc::channel();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        controller.ai_chat.running_job = Some(super::AiChatJobHandle {
            rx,
            prompt: "テスト指示".to_owned(),
            cancelled: std::sync::Arc::clone(&cancelled),
        });
        controller.ai_chat.streaming_preview = "{\"type\":\"batch\"".to_owned();
        assert!(controller.ai_chat.is_busy());

        assert!(controller.cancel_ai_prompt());
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert!(!controller.ai_chat.is_busy());
        assert_eq!(controller.ai_chat.streaming_preview, "");
        assert_eq!(controller.ai_chat.status_banner, "AI 生成を停止しました");
        assert!(
            controller
                .ai_chat
                .turns
                .last()
                .is_some_and(|t| t.text.contains("[停止済み]"))
        );
        assert!(!controller.cancel_ai_prompt());
        drop(tx);
    }

    #[test]
    fn at_mention_helpers_and_attachments_work_cleanly() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(format_at_mention_token("kaitosai.jpg"), "@kaitosai.jpg ");
        assert_eq!(
            format_at_mention_token("bgm track.wav"),
            "@\"bgm track.wav\" "
        );

        assert_eq!(
            find_trailing_at_query("移動して @kai"),
            Some((13, "kai".to_owned()))
        );
        assert_eq!(find_trailing_at_query("@kaitosai.jpg "), None);
        assert_eq!(find_trailing_at_query("foo@bar"), None);
        assert_eq!(
            apply_at_completion("移動 @kai", 7, "kaitosai.jpg"),
            "移動 @kaitosai.jpg "
        );

        let mut controller = AppController::new();
        let c1 = ClipId::new("c1")?;
        let c2 = ClipId::new("c2")?;
        controller.ai_chat.attach_clip(c1.clone());
        controller.ai_chat.attach_clip(c1.clone());
        controller.ai_chat.attach_clip(c2.clone());
        assert_eq!(controller.ai_chat.attached_clip_ids.len(), 2);

        controller.ai_chat.detach_clip(&c1);
        assert_eq!(controller.ai_chat.attached_clip_ids, vec![c2]);

        let names = matching_at_mention_names(&controller.document, "");
        assert_eq!(names.len(), 0);
        Ok(())
    }
}
