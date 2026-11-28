//! VS Code 風 AI チャットパネル状態管理、非同期ストリーミングポーリング、差分適用/1クリック Undo UI。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectItem, SelectState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, IndexPath, Side, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::{
    Anchor, Context, Entity, ExternalPaths, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use sea_agent_protocol::AgentUserAnswerPayload;
use serde_json::{Value, json};

use crate::ai::agent_host::{
    AgentHostEvent, AgentPluginRegistry, AgentSessionRunner, AgentSessionStartConfig,
    DiscoveredAgentPlugin,
};
use crate::ai::context_builder::{
    AiSelectionContext, build_ai_request_with_attachments, build_timeline_context,
};
use crate::ai::planner::{AiEditPlan, dry_run_validate};
use crate::ai::plugin_host::{AiPluginRegistry, DiscoveredAiModel};
use crate::ai::provider::{
    AgentRuntimeSettings, AiChatMessage, AiError, AiProvider, AiProviderKind, AiRequest, AiRole,
    AiStreamEvent, AiThinkingLevel, AiTokenUsage, SecretString,
};
use crate::ai::providers::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::ai::providers::credentials::{
    CredentialSource, diagnose_credential_source, resolve_api_key_with_settings,
};
use crate::ai::providers::gemini::{GeminiConfig, GeminiProvider};
use crate::ai::providers::mock::MockAiProvider;
use crate::ai::providers::openai_compat::{
    OpenAiCompatConfig, OpenAiCompatProvider, OpenRouterCreditsSnapshot, fetch_openrouter_credits,
};
use crate::ai::session_store::{
    AiSessionRecord, AiSessionStore, AiSessionSummary, AiSessionUsage, AiTurnActivitySummary,
    AiTurnBadge, PendingAgentQuestionSnapshot, StoredChatTurn,
};
use crate::ai::settings::{
    AI_SETTINGS_FILE_NAME, AiUserSettings, ProbedModelEntry, ResolvedAiModel,
    UsageDisplayPreference,
};
use crate::core::ids::ClipId;
use crate::timeline::document::ProjectDocument;
use crate::ui::ai_activity_view::{
    AiLiveActivityState, ToolCompletionUpdate, render_live_activity_card,
    render_turn_activity_summary,
};
use crate::ui::ai_agent_widgets::{
    render_agent_mode_menu_button, render_ask_user_card, render_attached_folder_chip,
    render_folder_attach_button, render_session_history_drawer, render_tool_approval_card,
};
use crate::ui::ai_settings_modal::AiSettingsTab;
use crate::ui::app_controller::AppController;
use crate::ui::i18n::{tr, tr_fmt};
use crate::ui::window::SeaEditorWindow;

/// チャット履歴に表示する 1 メッセージ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiChatTurn {
    /// セッション内で一意な安定識別子（`1` 始まり単調増加）。
    pub id: u64,
    /// 発言者ロール。
    pub role: AiRole,
    /// 元の生テキスト（ユーザー発言は生原文、AI 出力は生 Markdown）。
    pub text: String,
    /// `TextView::markdown` 描画用に事前変換済みのキャッシュ文字列。
    pub rendered_markdown: SharedString,
    /// 完了・停止・エラー状態を示す任意バッジ。
    pub status_badge: Option<AiTurnBadge>,
    /// 完了済みターンに紐づく折りたたみアクティビティ要約。
    pub activity_log: Option<AiTurnActivitySummary>,
}

const CHAT_HISTORY_COLLAPSE_THRESHOLD: usize = 200;

/// ユーザー入力等のプレーンテキストを Markdown 誤解釈なしで選択可能描画できるようエスケープする。
///
/// - `\r\n` / `\r` を `\n` へ正規化
/// - `CommonMark` の ASCII 記号全般を `\` でエスケープ（`#1`、`1. `、`<tag>`、`~~`、`a=b` 等の誤解釈防止）
/// - 単一改行（前後がともに非空行）のみ末尾 `"  \n"` で改行維持し、空行 `\n\n` には余分なスペースを混ぜない
#[must_use]
pub fn escape_plain_text_for_markdown(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut out = String::with_capacity(normalized.len() + 16);
    for (idx, line) in lines.iter().enumerate() {
        for ch in line.chars() {
            if matches!(
                ch,
                '\\' | '`'
                    | '*'
                    | '_'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '#'
                    | '+'
                    | '-'
                    | '.'
                    | '!'
                    | '|'
                    | '<'
                    | '>'
                    | '~'
                    | '='
            ) {
                out.push('\\');
            }
            out.push(ch);
        }
        if idx + 1 < lines.len() {
            let cur_non_empty = !line.is_empty();
            let next_non_empty = !lines[idx + 1].is_empty();
            if cur_non_empty && next_non_empty {
                out.push_str("  \n");
            } else {
                out.push('\n');
            }
        }
    }
    out
}

impl AiChatTurn {
    /// 指定された ID・ロール・本文・状態バッジからチャットターンを構築する。
    #[must_use]
    pub fn new(id: u64, role: AiRole, text: String, status_badge: Option<AiTurnBadge>) -> Self {
        Self::new_with_activity(id, role, text, status_badge, None)
    }

    /// 完了済みアクティビティログ付きでチャットターンを構築する。
    #[must_use]
    pub fn new_with_activity(
        id: u64,
        role: AiRole,
        text: String,
        status_badge: Option<AiTurnBadge>,
        activity_log: Option<AiTurnActivitySummary>,
    ) -> Self {
        let rendered_markdown = match role {
            AiRole::User => SharedString::from(escape_plain_text_for_markdown(&text)),
            AiRole::Assistant | AiRole::System => SharedString::from(text.clone()),
        };
        Self {
            id,
            role,
            text,
            rendered_markdown,
            status_badge,
            activity_log,
        }
    }
}

/// AI チャットの現在の実行フェーズを表す明示的状態。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AiExecutionState {
    /// 待機中（未送信・セッション復元直後・提案破棄／Undo 直後）。
    #[default]
    Idle,
    /// AI または自律エージェントが応答・作業中。
    Responding {
        /// 応答開始時刻。
        started_at: Instant,
        /// 現在の処理フェーズ文言。
        phase: String,
        /// エージェントステップ進捗 `(step, total_steps)`。
        step: Option<(Option<u32>, Option<u32>)>,
    },
    /// エージェントからの質問カード入力待ち。
    WaitingUserInput {
        /// 質問カードの概要。
        detail: String,
    },
    /// `Manual` モードにおける外部ツール実行承認待ち。
    WaitingApproval {
        /// 承認待ちツール名。
        tool_name: String,
    },
    /// `Manual` モードで編集案を作成済み（ユーザーの「承認して適用」または「破棄」待ち）。
    ProposalReady {
        /// 提案サマリ文言。
        summary: String,
    },
    /// 処理が最終完了（タイムライン適用済み／変更なし応答）。
    Completed {
        /// 完了サマリ文言。
        summary: String,
        /// タイムラインへ変更が反映されたか。
        changed_timeline: bool,
    },
    /// ユーザー操作により停止済み。
    Cancelled,
    /// 通信・検証・実行エラーにより終了。
    Failed {
        /// エラー概要文言。
        summary: String,
    },
}

/// 経過秒数をロケール対応の読みやすい文字列へ整形する。
#[must_use]
pub fn format_elapsed_duration(secs: u64) -> String {
    if secs < 60 {
        let s = secs.to_string();
        tr_fmt("ai.elapsed_secs", &[("secs", &s)])
    } else {
        let mins = (secs / 60).to_string();
        let rem = (secs % 60).to_string();
        tr_fmt("ai.elapsed_mins_secs", &[("mins", &mins), ("secs", &rem)])
    }
}

fn find_json_fence_start(raw: &str) -> Option<usize> {
    let mut earliest = raw.find("{\"tool_calls\"");
    for (idx, _) in raw.match_indices("```") {
        let after = &raw[idx + 3..];
        let line_end = after.find('\n').unwrap_or(after.len());
        let tag = after[..line_end].trim();
        let body_starts_json = tag.is_empty()
            && after[line_end..]
                .strip_prefix('\n')
                .is_some_and(|rest| rest.trim_start().starts_with('{'));
        if tag.eq_ignore_ascii_case("json") || body_starts_json {
            earliest = Some(earliest.map_or(idx, |prev| prev.min(idx)));
            break;
        }
    }
    earliest
}

fn tail_char_snippet(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let total = trimmed.chars().count();
    if total <= max_chars {
        return trimmed.to_owned();
    }
    let skip = total - max_chars;
    let tail: String = trimmed.chars().skip(skip).collect();
    format!("...{tail}")
}

/// ストリーミング中の制御 JSON（`{"tool_calls":...}` や ```` ```json ```` ブロック）をマスクした表示文字列を返す。
#[must_use]
pub fn format_streaming_preview_for_ui(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if raw.trim_start().starts_with('{') {
        let chars_str = raw.chars().count().to_string();
        return tr_fmt("ai.status_building_commands", &[("chars", &chars_str)]);
    }
    if let Some(fence_idx) = find_json_fence_start(raw) {
        let prefix = raw[..fence_idx].trim();
        let json_chars = raw[fence_idx..].chars().count().to_string();
        let masked = tr_fmt("ai.status_building_commands", &[("chars", &json_chars)]);
        if prefix.is_empty() {
            return masked;
        }
        let readable = tail_char_snippet(prefix, 140);
        return format!("{readable}\n{masked}");
    }
    tail_char_snippet(trimmed, 180)
}

fn extract_readable_cancelled_preview(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || raw.trim_start().starts_with('{') {
        return None;
    }
    if let Some(fence_idx) = find_json_fence_start(raw) {
        let prefix = raw[..fence_idx].trim();
        return (!prefix.is_empty()).then(|| prefix.to_owned());
    }
    Some(trimmed.to_owned())
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

/// `Manual` 承認モード時に確認待ち中の外部副作用ツール実行リクエスト。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAgentToolApproval {
    /// リクエスト ID。
    pub request_id: String,
    /// 対象ツール名。
    pub tool_name: String,
    /// 実行引数 JSON。
    pub arguments: Value,
    /// ツールの説明文。
    pub description: String,
}

pub use crate::ai::provider::AiApprovalMode;

struct AiChatJobHandle {
    rx: mpsc::Receiver<AiStreamEvent>,
    prompt: String,
    estimated_input_chars: usize,
    cancelled: Arc<AtomicBool>,
}

type PluginModelsProbeRx = mpsc::Receiver<(String, Result<Vec<ProbedModelEntry>, String>)>;

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

/// チャットパネル上部に開く補助ドロワー種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiChatDrawer {
    /// ドロワーを閉じている状態。
    #[default]
    None,
    /// セッション履歴ドロワー。
    SessionHistory,
}

/// UI 状態と履歴を管理する AI チャットパネル状態。
// 既存の独立した UI フラグ群（Undo 可否・履歴展開・エージェント稼働・検出キャッシュ済み）を個別に保持するため許容
#[allow(clippy::struct_excessive_bools)]
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
    /// 選択中のエージェントプラグイン ID（`None` なら従来の単発編集モード）。
    pub selected_agent_id: Option<String>,
    /// コンポーザへ添付・許可されたローカルメディアフォルダ一覧。
    pub attached_folders: Vec<PathBuf>,
    /// 現在の会話セッション ID（VS Code 風セッション保存・再開用）。
    pub session_id: String,
    /// セッション一覧に表示する見出し。
    pub session_title: String,
    /// セッション作成時刻（UNIX 秒）。
    pub session_created_at_unix_secs: u64,
    /// エージェントが提示中のリッチ対話質問カード（存在する場合）。
    pub pending_question: Option<PendingAgentQuestionSnapshot>,
    /// リッチ対話質問カードの各入力フィールド保持値。
    pub ask_field_values: BTreeMap<String, String>,
    /// `Manual` モード時に確認待ちの外部副作用ツール実行リクエスト。
    pub pending_tool_approval: Option<PendingAgentToolApproval>,
    /// 常駐エージェントの永続メモリ（画像解析キャッシュ・前回設計等）。
    pub agent_memory: Value,
    /// 現在セッションで累積されたトークン・概算コスト利用量。
    pub session_usage: AiSessionUsage,
    /// ステップ上限・最小呼出間隔・Vision 設定。
    pub runtime_settings: AgentRuntimeSettings,
    /// ユーザー設定 (`<settings_dir>/ai_settings.json`)。
    pub user_settings: AiUserSettings,
    /// 設定 JSON ファイルの保存先パス（テスト未注入時は `None`）。
    pub settings_path: Option<PathBuf>,
    /// 起動時の `ai_settings.json` 読込警告（破損時は自動上書きを抑止する）。
    pub settings_load_warning: Option<String>,
    /// `OpenRouter` の残クレジット取得結果。
    pub openrouter_credits: Option<OpenRouterCreditsSnapshot>,
    /// `OpenRouter` クレジット取得の直近エラー。
    pub openrouter_credits_error: Option<String>,
    /// `list_models` でプローブされた CLI プラグイン利用可能モデル一覧。
    pub discovered_plugin_models: BTreeMap<String, Vec<ProbedModelEntry>>,
    /// CLI プラグインの直近プローブ状態メッセージ。
    pub plugin_models_note: BTreeMap<String, String>,
    /// CLI プラグインのプローブ実行中フラグ。
    pub plugin_models_probing: BTreeMap<String, bool>,
    /// プロバイダごとの診断済み認証ソースキャッシュ（UI 毎フレーム描画での Keyring IPC 回避用）。
    pub(crate) cached_credential_sources: BTreeMap<String, CredentialSource>,
    /// セッション保存先ストア（ライブウィンドウ起動時に設定、テストでも任意注入可）。
    pub session_store: Option<AiSessionStore>,
    /// 上部に開いている補助ドロワー。
    pub active_drawer: AiChatDrawer,
    /// エージェントの現在ステップ進捗 `(step, total_steps)`。
    pub agent_step_status: Option<(Option<u32>, Option<u32>)>,
    /// 明示的な AI 実行状態機械。
    pub execution_state: AiExecutionState,
    /// 200 件超の長いセッションで過去ターン全件を展開するか。
    pub show_all_history: bool,
    /// セッション内で次に割り当てる安定ターン ID（`1` 始まり）。
    pub next_turn_id: u64,
    /// 実行中のライブステップ・思考・ストリーム進捗状態。
    pub live_activity: AiLiveActivityState,
    /// 完了済みターンでアクティビティ要約を展開中のターン ID 集合。
    pub expanded_activity_turns: HashSet<u64>,
    /// 次回 UI 描画時に履歴スクロールを最下部へ追従させる保留フラグ。
    pub scroll_to_bottom_pending: bool,
    runtime_api_key: Option<SecretString>,
    running_job: Option<AiChatJobHandle>,
    credits_rx: Option<mpsc::Receiver<Result<OpenRouterCreditsSnapshot, String>>>,
    models_probe_rxs: Vec<PluginModelsProbeRx>,
    pub(crate) active_agent: Option<AgentSessionRunner>,
    pub(crate) agent_provider_override: Option<Arc<dyn AiProvider>>,
    pub(crate) agent_busy: bool,
    pub(crate) agent_active_prompt: String,
    pub(crate) agent_active_model_label: String,
    pub(crate) cached_agents: Vec<DiscoveredAgentPlugin>,
    pub(crate) cached_agents_loaded: bool,
    pub(crate) cached_sessions: Vec<AiSessionSummary>,
}

impl std::fmt::Debug for AiChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiChatState")
            .field("selected_provider", &self.selected_provider)
            .field("selected_agent_id", &self.selected_agent_id)
            .field("session_id", &self.session_id)
            .field("model_name", &self.model_name)
            .field("thinking_level", &self.thinking_level)
            .field("turns_len", &self.turns.len())
            .field("attached_folders_len", &self.attached_folders.len())
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
            status_banner: "自然言語で編集指示を入力してください（フォルダ添付で自律構成も可能）"
                .to_owned(),
            pending_proposal: None,
            last_error: None,
            can_one_click_undo: false,
            approval_mode: AiApprovalMode::Manual,
            attached_clip_ids: Vec::new(),
            selected_agent_id: None,
            attached_folders: Vec::new(),
            session_id: AiSessionStore::generate_session_id(),
            session_title: "新しいセッション".to_owned(),
            session_created_at_unix_secs: AiSessionStore::now_unix_secs(),
            pending_question: None,
            ask_field_values: BTreeMap::new(),
            pending_tool_approval: None,
            agent_memory: json!({}),
            session_usage: AiSessionUsage::default(),
            runtime_settings: AgentRuntimeSettings::default(),
            user_settings: AiUserSettings::default(),
            settings_path: None,
            settings_load_warning: None,
            openrouter_credits: None,
            openrouter_credits_error: None,
            discovered_plugin_models: BTreeMap::new(),
            plugin_models_note: BTreeMap::new(),
            plugin_models_probing: BTreeMap::new(),
            cached_credential_sources: BTreeMap::new(),
            session_store: None,
            active_drawer: AiChatDrawer::None,
            agent_step_status: None,
            execution_state: AiExecutionState::Idle,
            show_all_history: false,
            next_turn_id: 1,
            live_activity: AiLiveActivityState::default(),
            expanded_activity_turns: HashSet::new(),
            scroll_to_bottom_pending: false,
            runtime_api_key: None,
            running_job: None,
            credits_rx: None,
            models_probe_rxs: Vec::new(),
            active_agent: None,
            agent_provider_override: None,
            agent_busy: false,
            agent_active_prompt: String::new(),
            agent_active_model_label: String::new(),
            cached_agents: Vec::new(),
            cached_agents_loaded: false,
            cached_sessions: Vec::new(),
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

    /// 導入済みエージェントプラグイン一覧を再走査してキャッシュを更新する。
    pub fn refresh_discovered_agents(&mut self) {
        self.cached_agents = AgentPluginRegistry::discover_default().plugins().to_vec();
        self.cached_agents_loaded = true;
    }

    /// セッション一覧キャッシュをストレージから再読込する。
    pub fn refresh_session_history(&mut self) {
        self.cached_sessions = self
            .session_store
            .as_ref()
            .and_then(|store| store.list_sessions().ok())
            .unwrap_or_default();
    }

    /// ライブウィンドウ起動用：既定のユーザー設定配下セッションストアと `settings/ai_settings.json` を読み込み、前回の選択状態を復元する。
    pub fn enable_user_session_store(&mut self) {
        if let Some(store) = AiSessionStore::default_user_store() {
            if let Ok(settings) = store.load_runtime_settings() {
                self.runtime_settings = settings;
            }
            self.session_store = Some(store);
        }
        let (settings, path, warn) = AiUserSettings::load_or_default_user();
        self.settings_path = path;
        self.settings_load_warning = warn;
        self.apply_user_settings_snapshot(settings);
        self.refresh_discovered_agents();
        self.refresh_session_history();
        self.refresh_active_provider_side_data();
    }

    /// 指定した設定ディレクトリ配下でセッションストアと `ai_settings.json` を有効化する（テスト・隔離環境用）。
    pub fn enable_settings_in_dir(&mut self, settings_dir: &Path) {
        let store = AiSessionStore::with_paths(
            settings_dir.join("ai_sessions"),
            settings_dir.join("agent_settings.json"),
        );
        self.session_store = Some(store);
        self.settings_path = Some(settings_dir.join(AI_SETTINGS_FILE_NAME));
        // テスト・隔離環境では端末ホストの旧 %APPDATA% 設定を誤読しないようレガシーフォールバックを無効化する
        let (settings, warn) = AiUserSettings::load_or_default_at(settings_dir, false);
        self.settings_load_warning = warn;
        self.apply_user_settings_snapshot(settings);
        self.refresh_discovered_agents();
        self.refresh_session_history();
    }

    /// 読み込んだ [`AiUserSettings`] の選択状態・カスタム Base URL・エージェント設定をチャット状態へ反映する。
    pub fn apply_user_settings_snapshot(&mut self, settings: AiUserSettings) {
        let last = settings.last_selection.clone();
        self.runtime_settings = settings.runtime.clone();
        self.user_settings = settings;
        self.selected_provider = last.provider.clone();
        self.custom_base_url = self
            .user_settings
            .base_url_for(&self.selected_provider)
            .unwrap_or_else(|| self.selected_provider.default_base_url())
            .to_owned();
        self.approval_mode = last.approval_mode;
        self.selected_agent_id = last.selected_agent_id;
        self.model_name = if last.model_name.trim().is_empty() {
            self.selected_provider.preferred_default_model().to_owned()
        } else {
            last.model_name
        };
        self.normalize_thinking_level_for_active_model(last.thinking_level);
        self.agent_provider_override = None;
        self.last_error = None;
        self.refresh_all_credential_sources();
    }

    /// 現在のプロバイダ／モデル／思考／エージェント設定を [`AiUserSettings`] へ反映し、安全ならディスク保存する。
    pub fn sync_selection_and_persist(&mut self) {
        self.user_settings.last_selection.provider = self.selected_provider.clone();
        self.user_settings
            .last_selection
            .model_name
            .clone_from(&self.model_name);
        self.user_settings.last_selection.thinking_level = self.thinking_level;
        self.user_settings.last_selection.approval_mode = self.approval_mode;
        self.user_settings
            .last_selection
            .selected_agent_id
            .clone_from(&self.selected_agent_id);
        self.user_settings
            .set_base_url(&self.selected_provider, &self.custom_base_url);
        self.user_settings.runtime = self.runtime_settings.clone();

        if self.settings_load_warning.is_some() {
            return;
        }
        if let Some(path) = &self.settings_path {
            let _ = self.user_settings.save_to_path(path);
        }
    }

    /// 設定モーダルでの明示変更を検証済み [`AiUserSettings`] として保存し、警告フラグを解除する。
    ///
    /// # Errors
    ///
    /// ディスク保存 I/O に失敗した場合にエラー文言を返す。
    pub fn save_explicit_user_settings(&mut self, mut next: AiUserSettings) -> Result<(), String> {
        next.last_selection.provider = self.selected_provider.clone();
        next.last_selection.model_name.clone_from(&self.model_name);
        next.last_selection.thinking_level = self.thinking_level;
        next.last_selection.approval_mode = self.approval_mode;
        next.last_selection
            .selected_agent_id
            .clone_from(&self.selected_agent_id);
        next.runtime = self.runtime_settings.clone();
        if let Some(path) = &self.settings_path {
            next.save_to_path(path)?;
        }
        self.settings_load_warning = None;
        if let Some(updated_url) = next.base_url_for(&self.selected_provider) {
            updated_url.clone_into(&mut self.custom_base_url);
        } else {
            self.selected_provider
                .default_base_url()
                .clone_into(&mut self.custom_base_url);
        }
        self.user_settings = next;
        self.refresh_all_credential_sources();
        Ok(())
    }

    /// `{ } JSON` 直接編集など、`last_selection` 全体を含む明示保存を実行して即時反映する。
    ///
    /// # Errors
    ///
    /// ディスク保存 I/O に失敗した場合にエラー文言を返す。
    pub fn save_and_apply_user_settings(&mut self, next: AiUserSettings) -> Result<(), String> {
        if let Some(path) = &self.settings_path {
            next.save_to_path(path)?;
        }
        self.settings_load_warning = None;
        self.apply_user_settings_snapshot(next);
        self.sync_active_agent_runtime();
        Ok(())
    }

    /// 許可メディアフォルダを添付する（ディレクトリのみ受理・正規化重複除去）。
    pub fn attach_folder(&mut self, path: PathBuf) {
        if !path.is_dir() {
            return;
        }
        let canon = fs::canonicalize(&path).unwrap_or(path);
        if !self.attached_folders.contains(&canon) {
            self.attached_folders.push(canon);
        }
    }

    /// UI からのフォルダ添付・ドロップ用：初回待機中かつ単発モードなら自動で既定エージェントへ切り替える。
    pub fn attach_folder_with_auto_agent(&mut self, path: PathBuf) {
        let had_folders = !self.attached_folders.is_empty();
        self.attach_folder(path);
        if !had_folders
            && !self.attached_folders.is_empty()
            && self.selected_agent_id.is_none()
            && self.turns.is_empty()
        {
            if !self.cached_agents_loaded {
                self.refresh_discovered_agents();
            }
            if let Some(director) = self
                .cached_agents
                .iter()
                .find(|p| p.manifest.id == "sea.agent.director")
            {
                self.selected_agent_id = Some(director.manifest.id.clone());
            } else if let Some(first) = self.cached_agents.first() {
                self.selected_agent_id = Some(first.manifest.id.clone());
            }
            self.sync_selection_and_persist();
        }
    }

    /// 添付済みフォルダを解除する。
    pub fn detach_folder(&mut self, path: &Path) {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.attached_folders.retain(|p| p != path && p != &canon);
    }

    /// 単発チャット (`None`) と自律エージェントプラグイン (`Some(id)`) を切り替える。
    pub fn select_agent(&mut self, agent_id: Option<String>) {
        if self.selected_agent_id != agent_id {
            if let Some(runner) = self.active_agent.take() {
                runner.cancel();
            }
            self.agent_busy = false;
            self.agent_step_status = None;
            self.selected_agent_id = agent_id;
            self.sync_selection_and_persist();
        }
    }

    /// 編集プランの承認モード (`Manual` / `Autopilot`) を切り替える。
    pub fn set_approval_mode(&mut self, mode: AiApprovalMode) {
        self.approval_mode = mode;
        self.sync_active_agent_runtime();
        self.sync_selection_and_persist();
    }

    /// ステップ上限・呼出間隔設定を更新・保存し、常駐エージェントにも即時反映する。
    pub fn apply_runtime_settings(&mut self, settings: AgentRuntimeSettings) {
        self.runtime_settings = settings.clone();
        self.user_settings.runtime = settings;
        if let Some(store) = &self.session_store {
            let _ = store.save_runtime_settings(&self.runtime_settings);
        }
        self.sync_active_agent_runtime();
        self.sync_selection_and_persist();
    }

    fn sync_active_agent_runtime(&self) {
        if let Some(runner) = &self.active_agent {
            let provider = self
                .agent_provider_override
                .clone()
                .unwrap_or_else(|| Arc::from(self.build_provider()));
            let _ = runner.update_runtime(
                provider,
                self.model_name.clone(),
                self.thinking_level,
                self.approval_mode,
                self.runtime_settings.clone(),
            );
        }
    }

    /// 現在のターンのトークン使用量（およびモデル単価からの概算コスト）をセッション累計へ反映する。
    pub fn record_turn_usage(&mut self, usage: &AiTokenUsage) {
        let model = self.active_resolved_model();
        let fallback_cost = model.and_then(|m| {
            usage.resolved_cost_usd(m.input_price_per_1m_usd, m.output_price_per_1m_usd)
        });
        self.session_usage.record_turn(usage, fallback_cost);
    }

    /// 現在のチャット／エージェント状態をセッションストアへ保存する。
    pub fn persist_session(&mut self) {
        let Some(store) = &self.session_store else {
            return;
        };
        if self.turns.is_empty()
            && self.attached_folders.is_empty()
            && self.pending_question.is_none()
        {
            return;
        }
        let now = AiSessionStore::now_unix_secs();
        let rec = AiSessionRecord {
            schema_version: 1,
            session_id: self.session_id.clone(),
            title: self.session_title.clone(),
            created_at_unix_secs: self.session_created_at_unix_secs,
            updated_at_unix_secs: now,
            provider: Some(self.selected_provider.clone()),
            agent_plugin_id: self.selected_agent_id.clone(),
            model_name: self.model_name.clone(),
            thinking_level: self.thinking_level,
            approval_mode: self.approval_mode,
            attached_folders: self
                .attached_folders
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            turns: self
                .turns
                .iter()
                .map(|t| StoredChatTurn {
                    role: t.role,
                    text: t.text.clone(),
                    timestamp_unix_secs: now,
                    status_badge: t.status_badge,
                    activity_log: t.activity_log.clone(),
                })
                .collect(),
            pending_question: self.pending_question.clone(),
            agent_memory: self.agent_memory.clone(),
            session_usage: self.session_usage.clone(),
        };
        if store.save_session(&rec).is_ok() {
            self.refresh_session_history();
        }
    }

    /// プロバイダ種別を切り替え、保存済み Base URL・既定モデル・思考レベルを更新して永続化する。
    pub fn select_provider(&mut self, kind: AiProviderKind) {
        let restored_base = self
            .user_settings
            .base_url_for(&kind)
            .unwrap_or_else(|| kind.default_base_url())
            .to_owned();
        let default_model = kind.preferred_default_model().to_owned();
        self.custom_base_url = restored_base;
        self.selected_provider = kind;
        self.model_name = default_model;
        let default_thinking = self.active_resolved_model().map_or_else(
            || self.selected_provider.default_thinking_level(),
            |m| m.default_thinking,
        );
        self.normalize_thinking_level_for_active_model(default_thinking);
        self.agent_provider_override = None;
        self.last_error = None;
        self.sync_active_agent_runtime();
        let active_provider = self.selected_provider.clone();
        self.refresh_credential_source_for(&active_provider);
        self.sync_selection_and_persist();
    }

    /// モデルを選択し、対応していれば `Medium` を標準選択（非対応時はモデル既定）とする。
    pub fn select_model_default_medium(&mut self, model_id: &str) {
        self.select_model(model_id, AiThinkingLevel::Medium);
    }

    /// モデルと思考レベルを同時に選択し、そのモデルで無効なレベルは自動補正して永続化する。
    pub fn select_model(&mut self, model_id: &str, thinking: AiThinkingLevel) {
        model_id.clone_into(&mut self.model_name);
        self.normalize_thinking_level_for_active_model(thinking);
        self.agent_provider_override = None;
        self.last_error = None;
        self.sync_active_agent_runtime();
        self.sync_selection_and_persist();
    }

    fn normalize_thinking_level_for_active_model(&mut self, desired: AiThinkingLevel) {
        let resolved = self.active_resolved_model();
        self.thinking_level = match resolved {
            Some(model) if model.thinking_levels.is_empty() => AiThinkingLevel::Auto,
            Some(model) if model.thinking_levels.contains(&desired) => desired,
            Some(model) => model.default_thinking,
            None => desired,
        };
    }

    /// 現在選択中のモデルに対して思考レベルを切り替え、永続化する。
    pub fn select_thinking_level(&mut self, level: AiThinkingLevel) {
        let allowed = self.active_thinking_options();
        self.thinking_level = if allowed.is_empty() {
            AiThinkingLevel::Auto
        } else if allowed.contains(&level) {
            level
        } else {
            self.active_resolved_model()
                .map_or(AiThinkingLevel::Auto, |m| m.default_thinking)
        };
        self.agent_provider_override = None;
        self.last_error = None;
        self.sync_active_agent_runtime();
        self.sync_selection_and_persist();
    }

    /// 指定プロバイダの標準カタログ・カスタムモデル・検出済み CLI モデルを統合した一覧を返す。
    #[must_use]
    pub fn resolved_models_for_provider(&self, kind: &AiProviderKind) -> Vec<ResolvedAiModel> {
        let discovered = match kind {
            AiProviderKind::Plugin { plugin_id } => self
                .discovered_plugin_models
                .get(plugin_id)
                .map_or(&[][..], Vec::as_slice),
            _ => &[],
        };
        self.user_settings
            .resolve_models_with_discovered(kind, discovered)
    }

    /// 現在選択中プロバイダの利用可能モデル一覧を返す。
    #[must_use]
    pub fn resolved_models_for_current_provider(&self) -> Vec<ResolvedAiModel> {
        self.resolved_models_for_provider(&self.selected_provider)
    }

    /// 現在選択中モデルの解決済み定義（カスタム設定・CLI プローブ結果を含む）を返す。
    #[must_use]
    pub fn active_resolved_model(&self) -> Option<ResolvedAiModel> {
        self.resolved_models_for_current_provider()
            .into_iter()
            .find(|m| m.id == self.model_name)
    }

    /// 現在選択中モデルで切り替え可能な思考レベル一覧を返す。
    #[must_use]
    pub fn active_thinking_options(&self) -> Vec<AiThinkingLevel> {
        self.active_resolved_model()
            .map_or_else(Vec::new, |m| m.thinking_levels)
    }

    /// UI 向けの短いモデル表示名（未登録モデルは ID そのまま）を返す。
    #[must_use]
    pub fn active_model_display_name(&self) -> String {
        self.active_resolved_model()
            .map_or_else(|| self.model_name.clone(), |m| m.display_name)
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

    /// 指定プロバイダの有効な認証情報取得元を再診断して UI 描画用キャッシュへ保存する。
    pub fn refresh_credential_source_for(&mut self, kind: &AiProviderKind) -> CredentialSource {
        let runtime = if &self.selected_provider == kind {
            self.runtime_api_key.as_ref()
        } else {
            None
        };
        let source = diagnose_credential_source(kind, runtime, Some(&self.user_settings));
        self.cached_credential_sources
            .insert(kind.settings_key(), source.clone());
        source
    }

    /// UI 選択対象となる全プロバイダの認証情報取得元キャッシュを更新する。
    pub fn refresh_all_credential_sources(&mut self) {
        for kind in AiProviderKind::ui_selectable_providers() {
            self.refresh_credential_source_for(&kind);
        }
        let active = self.selected_provider.clone();
        self.refresh_credential_source_for(&active);
    }

    /// UI 描画向けにキャッシュ済みの認証情報取得元を返す（Keyring IPC を発生させない）。
    #[must_use]
    pub fn cached_credential_source_for(&self, kind: &AiProviderKind) -> CredentialSource {
        if &self.selected_provider == kind && self.runtime_api_key.is_some() {
            return CredentialSource::RuntimeOverride;
        }
        self.cached_credential_sources
            .get(&kind.settings_key())
            .cloned()
            .unwrap_or(CredentialSource::Missing)
    }

    /// キャッシュ済みの情報に基づき現在選択中プロバイダ向けの認証情報が有効か返す。
    #[must_use]
    pub fn has_cached_credentials(&self) -> bool {
        if !self.selected_provider.requires_api_key() {
            return true;
        }
        self.cached_credential_source_for(&self.selected_provider) != CredentialSource::Missing
    }

    /// 指定プロバイダに対する現在の有効な認証情報取得元を診断して返す。
    #[must_use]
    pub fn credential_source_for(&self, kind: &AiProviderKind) -> CredentialSource {
        let runtime = if &self.selected_provider == kind {
            self.runtime_api_key.as_ref()
        } else {
            None
        };
        diagnose_credential_source(kind, runtime, Some(&self.user_settings))
    }

    /// 現在選択中プロバイダの有効な認証情報取得元を診断して返す。
    #[must_use]
    pub fn credential_source(&self) -> CredentialSource {
        self.credential_source_for(&self.selected_provider)
    }

    /// メモリ上の一時 API キーを設定または解除する（プロジェクト JSON には保存されない）。
    pub fn set_runtime_api_key(&mut self, key: Option<SecretString>) {
        self.runtime_api_key = key;
        self.agent_provider_override = None;
        let active = self.selected_provider.clone();
        self.refresh_credential_source_for(&active);
        self.sync_active_agent_runtime();
    }

    /// 現在選択中のプロバイダ向けに有効な認証情報が解決できるか返す。
    #[must_use]
    pub fn has_resolved_credentials(&self) -> bool {
        if !self.selected_provider.requires_api_key() {
            return true;
        }
        resolve_api_key_with_settings(
            &self.selected_provider,
            self.runtime_api_key.as_ref(),
            Some(&self.user_settings),
        )
        .is_some()
    }

    /// `OpenRouter` クレジット取得のバックグラウンド通信が実行中かどうかを返す。
    #[must_use]
    pub const fn is_openrouter_credits_loading(&self) -> bool {
        self.credits_rx.is_some()
    }

    /// `OpenRouter` の残クレジットをバックグラウンドスレッドで非同期取得する。
    pub fn refresh_openrouter_credits_async(&mut self) {
        if self.is_openrouter_credits_loading() {
            return;
        }
        let Some(api_key) = resolve_api_key_with_settings(
            &AiProviderKind::OpenRouter,
            self.runtime_api_key.as_ref(),
            Some(&self.user_settings),
        ) else {
            self.openrouter_credits = None;
            self.openrouter_credits_error = None;
            return;
        };
        let base_url = if self.selected_provider == AiProviderKind::OpenRouter
            && !self.custom_base_url.trim().is_empty()
        {
            self.custom_base_url.clone()
        } else {
            self.user_settings
                .base_url_for(&AiProviderKind::OpenRouter)
                .unwrap_or_else(|| AiProviderKind::OpenRouter.default_base_url())
                .to_owned()
        };
        let (tx, rx) = mpsc::channel();
        self.credits_rx = Some(rx);
        self.openrouter_credits_error = None;
        std::thread::spawn(move || {
            let res = fetch_openrouter_credits(&base_url, &api_key, 10).map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
    }

    /// 指定 CLI プラグインの `list_models` をバックグラウンドで実行し、利用可能モデル一覧を更新する。
    pub fn probe_plugin_models_async(&mut self, plugin_id: &str) {
        let trimmed = plugin_id.trim();
        if trimmed.is_empty()
            || self
                .plugin_models_probing
                .get(trimmed)
                .copied()
                .unwrap_or(false)
        {
            return;
        }
        let target_id = trimmed.to_owned();
        let (tx, rx) = mpsc::channel();
        self.models_probe_rxs.push(rx);
        self.plugin_models_probing.insert(target_id.clone(), true);
        self.plugin_models_note.remove(&target_id);
        std::thread::spawn(move || {
            let res = (|| -> Result<Vec<ProbedModelEntry>, String> {
                let reg = AiPluginRegistry::discover_default();
                let provider = reg
                    .resolve_provider(&target_id)
                    .map_err(|e| e.to_string())?;
                let discovered = provider
                    .probe_available_models()
                    .map_err(|e| e.to_string())?;
                Ok(discovered
                    .iter()
                    .map(DiscoveredAiModel::to_probed_entry)
                    .collect())
            })();
            let _ = tx.send((target_id, res));
        });
    }

    /// 現在選択中の CLI プラグインの `list_models` プローブを開始する。
    pub fn probe_current_plugin_models_async(&mut self) {
        if let AiProviderKind::Plugin { plugin_id } = &self.selected_provider {
            let id = plugin_id.clone();
            self.probe_plugin_models_async(&id);
        }
    }

    /// 現在プロバイダに応じて `OpenRouter` クレジットまたは CLI モデル一覧を初回のみ自動取得する。
    pub fn refresh_active_provider_side_data(&mut self) {
        match &self.selected_provider {
            AiProviderKind::OpenRouter
                if self.openrouter_credits.is_none()
                    && !self.is_openrouter_credits_loading()
                    && self.has_resolved_credentials() =>
            {
                self.refresh_openrouter_credits_async();
            }
            AiProviderKind::Plugin { plugin_id }
                if !self.discovered_plugin_models.contains_key(plugin_id) =>
            {
                let id = plugin_id.clone();
                self.probe_plugin_models_async(&id);
            }
            _ => {}
        }
    }

    pub(crate) fn poll_background_probes(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = self.credits_rx.take() {
            match rx.try_recv() {
                Ok(Ok(snapshot)) => {
                    self.openrouter_credits = Some(snapshot);
                    self.openrouter_credits_error = None;
                    changed = true;
                }
                Ok(Err(err)) => {
                    self.openrouter_credits_error = Some(err);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.credits_rx = Some(rx);
                }
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        if !self.models_probe_rxs.is_empty() {
            let receivers = std::mem::take(&mut self.models_probe_rxs);
            for rx in receivers {
                match rx.try_recv() {
                    Ok((plugin_id, Ok(entries))) => {
                        self.plugin_models_probing.insert(plugin_id.clone(), false);
                        if entries.is_empty() {
                            self.plugin_models_note.insert(
                                plugin_id,
                                "CLI からモデルが検出されませんでした（認証状態を確認してください）"
                                    .to_owned(),
                            );
                        } else {
                            self.plugin_models_note.insert(
                                plugin_id.clone(),
                                format!("CLI から {} 件のモデルを検出しました", entries.len()),
                            );
                            self.discovered_plugin_models.insert(plugin_id, entries);
                        }
                        changed = true;
                    }
                    Ok((plugin_id, Err(err))) => {
                        self.plugin_models_probing.insert(plugin_id.clone(), false);
                        self.plugin_models_note
                            .insert(plugin_id, format!("モデル検出エラー: {err}"));
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        self.models_probe_rxs.push(rx);
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {}
                }
            }
        }
        changed
    }

    /// 非同期処理を実行中かどうか。
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.running_job.is_some() || self.agent_busy
    }

    /// 停止ボタンを押せる（および新規送信をブロックすべき）待機・処理中状態かどうか。
    #[must_use]
    pub const fn can_cancel(&self) -> bool {
        self.is_busy() || self.pending_question.is_some() || self.pending_tool_approval.is_some()
    }

    fn clear_pending_agent_interactions(&mut self) {
        self.pending_question = None;
        self.ask_field_values.clear();
        self.pending_tool_approval = None;
        self.streaming_preview.clear();
    }

    /// 1 秒周期の経過秒数タイマーで UI を再描画すべき状態かどうか。
    #[must_use]
    pub const fn should_tick_elapsed_timer(&self) -> bool {
        matches!(self.execution_state, AiExecutionState::Responding { .. })
    }

    /// 応答中 (`AiExecutionState::Responding`) の経過秒数を返す。
    #[must_use]
    pub fn responding_elapsed_secs(&self) -> Option<u64> {
        match &self.execution_state {
            AiExecutionState::Responding { started_at, .. } => Some(started_at.elapsed().as_secs()),
            _ => None,
        }
    }

    /// 新規ターンを安定連番 ID 付きで履歴へ追加する。
    pub fn push_turn(&mut self, role: AiRole, text: String, status_badge: Option<AiTurnBadge>) {
        self.push_turn_with_activity(role, text, status_badge, None);
    }

    /// アクティビティログ付きで新規ターンを履歴へ追加する。
    pub fn push_turn_with_activity(
        &mut self,
        role: AiRole,
        text: String,
        status_badge: Option<AiTurnBadge>,
        activity_log: Option<AiTurnActivitySummary>,
    ) {
        let id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        self.turns.push(AiChatTurn::new_with_activity(
            id,
            role,
            text,
            status_badge,
            activity_log,
        ));
        self.scroll_to_bottom_pending = true;
    }

    /// 保留中の自動最下部スクロール要求を消費して `true`/`false` を返す。
    pub fn take_scroll_to_bottom_pending(&mut self) -> bool {
        std::mem::take(&mut self.scroll_to_bottom_pending)
    }

    fn set_responding_phase(&mut self, phase: String, step: Option<(Option<u32>, Option<u32>)>) {
        let started_at = match &self.execution_state {
            AiExecutionState::Responding { started_at, .. } => *started_at,
            _ => Instant::now(),
        };
        self.execution_state = AiExecutionState::Responding {
            started_at,
            phase,
            step,
        };
    }

    /// 会話履歴・提案差分・エラー状態をクリアし、新しいチャット／セッションを開始する。
    pub fn clear_conversation(&mut self) {
        self.new_session();
    }

    /// 常駐エージェントを含めて現在の会話状態をリセットし、新規セッションを発行する。
    pub fn new_session(&mut self) {
        if self.is_busy() {
            return;
        }
        if let Some(runner) = self.active_agent.take() {
            runner.cancel();
        }
        self.agent_busy = false;
        self.agent_step_status = None;
        self.agent_active_prompt.clear();
        self.agent_active_model_label.clear();
        self.session_id = AiSessionStore::generate_session_id();
        "新しいセッション".clone_into(&mut self.session_title);
        self.session_created_at_unix_secs = AiSessionStore::now_unix_secs();
        self.turns.clear();
        self.next_turn_id = 1;
        self.show_all_history = false;
        self.live_activity.reset();
        self.expanded_activity_turns.clear();
        self.scroll_to_bottom_pending = false;
        self.execution_state = AiExecutionState::Idle;
        self.streaming_preview.clear();
        self.pending_proposal = None;
        self.pending_question = None;
        self.ask_field_values.clear();
        self.pending_tool_approval = None;
        self.agent_memory = json!({});
        self.last_error = None;
        self.can_one_click_undo = false;
        self.attached_clip_ids.clear();
        self.attached_folders.clear();
        self.session_usage = AiSessionUsage::default();
        self.agent_provider_override = None;
        "自然言語で編集指示を入力してください（フォルダ添付で自律構成も可能）"
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
        let key = resolve_api_key_with_settings(
            &self.selected_provider,
            self.runtime_api_key.as_ref(),
            Some(&self.user_settings),
        );
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
            AiProviderKind::Gemini => {
                let mut cfg = GeminiConfig::new(model, key).with_thinking_level(thinking);
                if !self.custom_base_url.trim().is_empty() {
                    cfg.base_url.clone_from(&self.custom_base_url);
                }
                Box::new(GeminiProvider::new(cfg))
            }
            AiProviderKind::Anthropic => {
                let mut cfg = AnthropicConfig::new(model, key).with_thinking_level(thinking);
                if !self.custom_base_url.trim().is_empty() {
                    cfg.base_url.clone_from(&self.custom_base_url);
                }
                Box::new(AnthropicProvider::new(cfg))
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
        self.ai_chat.push_turn(AiRole::User, display_turn, None);
        self.ai_chat.live_activity.begin_turn();
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
        let mut live_activity = AiLiveActivityState::default();
        live_activity.begin_turn();
        let response = provider
            .send_streaming(&req, &mut |evt| match evt {
                AiStreamEvent::Status(msg) => status_buf = msg,
                AiStreamEvent::DeltaText(delta) => streamed_buf.push_str(&delta),
                AiStreamEvent::ThoughtDelta(t) => live_activity.on_thought_delta(&t),
                AiStreamEvent::ActivityHint(l) => live_activity.on_activity_hint(&l),
                AiStreamEvent::Completed(_) | AiStreamEvent::Failed(_) => {}
            })
            .map_err(|err| {
                let msg = err.to_string();
                self.ai_chat.last_error = Some(msg.clone());
                self.ai_chat.status_banner = format!("AI 通信エラー: {msg}");
                msg
            })?;

        self.ai_chat.live_activity = live_activity;

        self.ai_chat.streaming_preview = streamed_buf;
        if !status_buf.is_empty() {
            self.ai_chat.status_banner = status_buf;
        }
        let usage = response.usage.clone().unwrap_or_else(|| {
            AiTokenUsage::estimate_from_chars(
                req.estimated_input_chars(),
                response.raw_text.chars().count(),
            )
        });
        self.ai_chat.record_turn_usage(&usage);
        self.finalize_ai_response(trimmed, &response.model, &response.raw_text)
    }

    /// UI からの指示をバックグラウンドスレッド（または常駐エージェント）で非同期開始する。
    ///
    /// # Errors
    ///
    /// 空指示、既存リクエスト実行中、エージェント起動失敗、またはリクエスト生成失敗時。
    pub fn start_ai_prompt_async(&mut self, prompt: &str) -> Result<(), String> {
        if self.ai_chat.can_cancel() {
            if !self.ai_chat.is_busy() {
                return Err(tr("ai.err_waiting_action"));
            }
            return Err("前の AI リクエストを処理中です".to_owned());
        }
        let trimmed = prompt.trim().to_owned();
        if trimmed.is_empty() {
            return Err("AI への指示文が空です".to_owned());
        }

        if self.ai_chat.selected_agent_id.is_some()
            || self
                .ai_chat
                .active_agent
                .as_ref()
                .is_some_and(AgentSessionRunner::is_alive)
        {
            let provider = self
                .ai_chat
                .agent_provider_override
                .clone()
                .unwrap_or_else(|| Arc::from(self.ai_chat.build_provider()));
            return self.start_agent_prompt_with_provider(&trimmed, provider, None);
        }

        let req = self.prepare_ai_request_and_turn(&trimmed)?;
        let estimated_input_chars = req.estimated_input_chars();
        if self.ai_chat.session_title == "新しいセッション" {
            self.ai_chat.session_title = AiSessionStore::derive_title_from_prompt(&trimmed);
        }
        self.ai_chat.persist_session();
        let phase = tr("ai.status_responding");
        self.ai_chat.status_banner.clone_from(&phase);
        self.ai_chat.execution_state = AiExecutionState::Responding {
            started_at: Instant::now(),
            phase,
            step: None,
        };

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
            estimated_input_chars,
            cancelled: cancel_flag,
        });
        Ok(())
    }

    /// 指定されたプロバイダと任意プラグイン定義を使い、常駐エージェントタスク（またはフォローアップ）を開始する。
    ///
    /// # Errors
    ///
    /// 空指示、既存実行中、対象プラグイン未検出、またはプロセス起動失敗時にエラー説明文字列を返す。
    pub fn start_agent_prompt_with_provider(
        &mut self,
        prompt: &str,
        provider: Arc<dyn AiProvider>,
        plugin_override: Option<DiscoveredAgentPlugin>,
    ) -> Result<(), String> {
        if self.ai_chat.can_cancel() {
            if !self.ai_chat.is_busy() {
                return Err(tr("ai.err_waiting_action"));
            }
            return Err("前の AI / エージェントリクエストを処理中です".to_owned());
        }
        let trimmed = prompt.trim().to_owned();
        if trimmed.is_empty() {
            return Err("AI への指示文が空です".to_owned());
        }

        let plugin = resolve_target_agent_plugin(&self.ai_chat, plugin_override)?;
        self.ai_chat.prune_attachments(&self.document);
        let attached_clips: Vec<String> = self
            .ai_chat
            .attached_clip_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        let timeline_summary = build_compact_timeline_summary(
            &self.document,
            self.clock.current_frame(),
            self.selected_clip_id.as_ref(),
            self.selected_layer_id.as_ref(),
        );

        let start_res = self.dispatch_agent_prompt_or_followup(
            &plugin,
            &trimmed,
            &attached_clips,
            timeline_summary,
            &provider,
        );

        if let Err(err) = start_res {
            self.ai_chat.last_error = Some(err.clone());
            self.ai_chat.status_banner = format!("エージェント起動・送信エラー: {err}");
            self.ai_chat.execution_state = AiExecutionState::Failed {
                summary: err.clone(),
            };
            return Err(err);
        }

        self.ai_chat.agent_provider_override = Some(provider);
        self.commit_agent_turn_state(&trimmed, &plugin, &attached_clips);
        self.ai_chat.agent_busy = true;
        let phase = "エージェントを起動中...".to_owned();
        self.ai_chat.status_banner.clone_from(&phase);
        self.ai_chat.execution_state = AiExecutionState::Responding {
            started_at: Instant::now(),
            phase,
            step: None,
        };
        self.ai_chat.persist_session();
        Ok(())
    }

    fn dispatch_agent_prompt_or_followup(
        &mut self,
        plugin: &DiscoveredAgentPlugin,
        trimmed: &str,
        attached_clips: &[String],
        timeline_summary: String,
        provider: &Arc<dyn AiProvider>,
    ) -> Result<(), String> {
        let alive_matches = self
            .ai_chat
            .active_agent
            .as_ref()
            .is_some_and(|r| r.is_alive() && r.plugin_id() == plugin.manifest.id);
        if alive_matches {
            let runner = self
                .ai_chat
                .active_agent
                .as_ref()
                .ok_or_else(|| "active agent missing".to_owned())?;
            return runner
                .update_runtime(
                    Arc::clone(provider),
                    self.ai_chat.model_name.clone(),
                    self.ai_chat.thinking_level,
                    self.ai_chat.approval_mode,
                    self.ai_chat.runtime_settings.clone(),
                )
                .and_then(|()| {
                    runner.send_follow_up(
                        trimmed.to_owned(),
                        self.ai_chat.attached_folders.clone(),
                        attached_clips.to_vec(),
                        self.document.clone(),
                        timeline_summary,
                    )
                })
                .map_err(|e| e.to_string());
        }
        if let Some(old) = self.ai_chat.active_agent.take() {
            old.cancel();
        }
        let cfg = AgentSessionStartConfig {
            plugin: plugin.clone(),
            session_id: self.ai_chat.session_id.clone(),
            goal: trimmed.to_owned(),
            resume: false,
            attached_folders: self.ai_chat.attached_folders.clone(),
            attached_clips: attached_clips.to_vec(),
            base_document: self.document.clone(),
            timeline_summary,
            agent_memory: self.ai_chat.agent_memory.clone(),
            provider: Arc::clone(provider),
            provider_kind: self.ai_chat.selected_provider.clone(),
            model_name: self.ai_chat.model_name.clone(),
            thinking_level: self.ai_chat.thinking_level,
            approval_mode: self.ai_chat.approval_mode,
            runtime_settings: self.ai_chat.runtime_settings.clone(),
            tool_registry: None,
            extra_extensions: Vec::new(),
            project_dir: self
                .project_path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
        };
        let runner = AgentSessionRunner::start(cfg).map_err(|err| err.to_string())?;
        self.ai_chat.active_agent = Some(runner);
        Ok(())
    }

    fn commit_agent_turn_state(
        &mut self,
        trimmed: &str,
        plugin: &DiscoveredAgentPlugin,
        attached_clips: &[String],
    ) {
        self.ai_chat.selected_agent_id = Some(plugin.manifest.id.clone());

        let mut tags = Vec::new();
        if !self.ai_chat.attached_folders.is_empty() {
            tags.push(format!(
                "フォルダ {} 件",
                self.ai_chat.attached_folders.len()
            ));
        }
        if !attached_clips.is_empty() {
            tags.push(format!("クリップ {} 件", attached_clips.len()));
        }
        let display_turn = if tags.is_empty() {
            trimmed.to_owned()
        } else {
            format!("[添付: {}]\n{trimmed}", tags.join(", "))
        };
        if self.ai_chat.turns.is_empty() && self.ai_chat.session_title == "新しいセッション"
        {
            self.ai_chat.session_title = AiSessionStore::derive_title_from_prompt(trimmed);
        }
        self.ai_chat.push_turn(AiRole::User, display_turn, None);
        self.ai_chat.live_activity.begin_turn();
        self.ai_chat.attached_clip_ids.clear();
        self.ai_chat.pending_proposal = None;
        self.ai_chat.pending_question = None;
        self.ai_chat.ask_field_values.clear();
        self.ai_chat.pending_tool_approval = None;
        self.ai_chat.last_error = None;
        self.ai_chat.can_one_click_undo = false;
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.live_activity.begin_turn();
        self.ai_chat.agent_step_status = None;
        trimmed.clone_into(&mut self.ai_chat.agent_active_prompt);
        self.ai_chat.agent_active_model_label =
            format!("{} ({})", plugin.manifest.name, self.ai_chat.model_name);
    }

    /// 現在表示中の `ask_user` リッチ質問カードへ回答を返送し、エージェントを続行させる。
    ///
    /// # Errors
    ///
    /// 保留中質問がない、常駐エージェントが終了済み、または IPC 送信失敗時。
    pub fn answer_pending_agent_question(
        &mut self,
        choice_id: Option<String>,
        field_values: BTreeMap<String, String>,
        free_text: Option<String>,
        skipped: bool,
    ) -> Result<(), String> {
        let req_id = self
            .ai_chat
            .pending_question
            .as_ref()
            .map(|q| q.request_id.clone())
            .ok_or_else(|| "回答待ちの質問カードはありません".to_owned())?;
        let runner = self
            .ai_chat
            .active_agent
            .as_ref()
            .filter(|r| r.is_alive())
            .ok_or_else(|| {
                let err = "常駐エージェントプロセスが終了しています".to_owned();
                self.ai_chat.last_error = Some(err.clone());
                self.ai_chat.status_banner.clone_from(&err);
                err
            })?;

        let answer_summary = if skipped {
            "[質問応答] おまかせで続行".to_owned()
        } else {
            let filled: Vec<String> = field_values
                .iter()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| format!("{k}={}", v.trim()))
                .collect();
            let choice_txt = choice_id.as_deref().unwrap_or("submit");
            if filled.is_empty() {
                format!("[選択] {choice_txt}")
            } else {
                format!("[選択: {choice_txt}] {}", filled.join(", "))
            }
        };

        let payload = AgentUserAnswerPayload {
            choice_id,
            field_values,
            free_text,
            skipped,
        };
        if let Err(err) = runner.send_user_answer(req_id, payload) {
            let msg = err.to_string();
            self.ai_chat.last_error = Some(msg.clone());
            self.ai_chat.status_banner = format!("回答送信エラー: {msg}");
            return Err(msg);
        }
        self.ai_chat.pending_question = None;
        self.ai_chat.ask_field_values.clear();
        self.ai_chat.last_error = None;
        self.ai_chat.push_turn(AiRole::User, answer_summary, None);
        self.ai_chat.agent_busy = true;
        let phase = "回答を受けて構成を設計中...".to_owned();
        self.ai_chat.status_banner.clone_from(&phase);
        self.ai_chat.execution_state = AiExecutionState::Responding {
            started_at: Instant::now(),
            phase,
            step: None,
        };
        self.ai_chat.persist_session();
        Ok(())
    }

    /// `Manual` モードで確認待ち中の外部副作用ツール実行を許可または拒否する。
    ///
    /// # Errors
    ///
    /// 保留中の承認要求がない、またはエージェントプロセスへ送信できない場合。
    pub fn answer_agent_tool_approval(
        &mut self,
        approved: bool,
        reason: Option<String>,
    ) -> Result<(), String> {
        let (req_id, tool_name) = self
            .ai_chat
            .pending_tool_approval
            .as_ref()
            .map(|p| (p.request_id.clone(), p.tool_name.clone()))
            .ok_or_else(|| "承認待ちのツール実行はありません".to_owned())?;
        let runner = self
            .ai_chat
            .active_agent
            .as_ref()
            .filter(|r| r.is_alive())
            .ok_or_else(|| {
                let err = "常駐エージェントプロセスが終了しています".to_owned();
                self.ai_chat.last_error = Some(err.clone());
                self.ai_chat.status_banner.clone_from(&err);
                err
            })?;

        if let Err(err) = runner.send_tool_approval(req_id, approved, reason) {
            let msg = err.to_string();
            self.ai_chat.last_error = Some(msg.clone());
            self.ai_chat.status_banner = format!("承認応答エラー: {msg}");
            return Err(msg);
        }
        self.ai_chat.pending_tool_approval = None;
        self.ai_chat.last_error = None;
        self.ai_chat.agent_busy = true;
        let msg = if approved {
            format!("外部ツール `{tool_name}` を実行中...")
        } else {
            format!("外部ツール `{tool_name}` の実行を拒否しました")
        };
        self.ai_chat.status_banner.clone_from(&msg);
        self.ai_chat.execution_state = AiExecutionState::Responding {
            started_at: Instant::now(),
            phase: msg,
            step: self.ai_chat.agent_step_status,
        };
        Ok(())
    }

    /// 保存済みセッション ID を読み込み、UI・履歴・常駐エージェントを再開する。
    ///
    /// # Errors
    ///
    /// ストア未初期化、指定セッション読込失敗、または再開起動失敗時。
    pub fn open_saved_ai_session(&mut self, session_id: &str) -> Result<(), String> {
        self.open_saved_ai_session_with_provider(session_id, None, None)
    }

    /// 指定プロバイダ・任意プラグインを使って保存済みセッションを読み込み復元する（テスト兼用）。
    ///
    /// # Errors
    ///
    /// ストア未設定またはセッション読込・復元失敗時。
    pub fn open_saved_ai_session_with_provider(
        &mut self,
        session_id: &str,
        provider: Option<Arc<dyn AiProvider>>,
        plugin_override: Option<DiscoveredAgentPlugin>,
    ) -> Result<(), String> {
        if self.ai_chat.is_busy() {
            return Err("実行中の処理を停止してからセッションを切り替えてください".to_owned());
        }
        let store = self
            .ai_chat
            .session_store
            .clone()
            .ok_or_else(|| "セッションストアが初期化されていません".to_owned())?;
        let rec = store.load_session(session_id).map_err(|e| e.to_string())?;

        if let Some(runner) = self.ai_chat.active_agent.take() {
            runner.cancel();
        }
        self.apply_restored_session_record(rec);

        self.ai_chat.agent_provider_override.clone_from(&provider);
        let resolved_provider =
            provider.unwrap_or_else(|| Arc::from(self.ai_chat.build_provider()));
        self.spawn_resumed_agent_if_needed(resolved_provider, plugin_override);
        self.ai_chat.refresh_session_history();
        self.ai_chat.refresh_active_provider_side_data();
        "セッションを復元しました（続けて調整指示を出せます）"
            .clone_into(&mut self.ai_chat.status_banner);
        Ok(())
    }

    fn apply_restored_session_record(&mut self, rec: AiSessionRecord) {
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
        self.ai_chat.execution_state = AiExecutionState::Idle;
        self.ai_chat.show_all_history = false;
        self.ai_chat.live_activity.reset();
        self.ai_chat.expanded_activity_turns.clear();
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.pending_proposal = None;
        self.ai_chat.pending_question = None;
        self.ai_chat.pending_tool_approval = None;
        self.ai_chat.last_error = None;
        self.ai_chat.can_one_click_undo = false;

        self.ai_chat.session_id = rec.session_id;
        self.ai_chat.session_title = rec.title;
        self.ai_chat.session_created_at_unix_secs = rec.created_at_unix_secs;
        if let Some(p) = rec.provider {
            if rec.model_name.is_empty() {
                p.preferred_default_model()
                    .clone_into(&mut self.ai_chat.model_name);
            }
            self.ai_chat
                .user_settings
                .base_url_for(&p)
                .unwrap_or_else(|| p.default_base_url())
                .clone_into(&mut self.ai_chat.custom_base_url);
            self.ai_chat.selected_provider = p;
        }
        if !rec.model_name.is_empty() {
            self.ai_chat.model_name = rec.model_name;
        }
        self.ai_chat.thinking_level = rec.thinking_level;
        self.ai_chat.approval_mode = rec.approval_mode;
        self.ai_chat
            .selected_agent_id
            .clone_from(&rec.agent_plugin_id);
        self.ai_chat.attached_folders = rec
            .attached_folders
            .into_iter()
            .map(PathBuf::from)
            .collect();
        self.ai_chat.turns.clear();
        self.ai_chat.next_turn_id = 1;
        for t in rec.turns {
            self.ai_chat
                .push_turn_with_activity(t.role, t.text, t.status_badge, t.activity_log);
        }
        // ResumeSession 後のプラグインはアイドル待機に入るため、未回答質問カードをそのまま復元すると永続待ちハングになる
        self.ai_chat.ask_field_values.clear();
        self.ai_chat.agent_memory = rec.agent_memory;
        self.ai_chat.session_usage = rec.session_usage;
        self.ai_chat.active_drawer = AiChatDrawer::None;
        self.ai_chat.refresh_all_credential_sources();
    }

    fn spawn_resumed_agent_if_needed(
        &mut self,
        provider: Arc<dyn AiProvider>,
        plugin_override: Option<DiscoveredAgentPlugin>,
    ) {
        let Some(agent_id) = self.ai_chat.selected_agent_id.clone() else {
            return;
        };
        if !self.ai_chat.cached_agents_loaded && plugin_override.is_none() {
            self.ai_chat.refresh_discovered_agents();
        }
        let plugin = plugin_override.or_else(|| {
            self.ai_chat
                .cached_agents
                .iter()
                .find(|p| p.manifest.id == agent_id)
                .cloned()
        });
        let Some(p) = plugin.filter(|p| p.executable_path.is_some()) else {
            return;
        };
        let timeline_summary = build_compact_timeline_summary(
            &self.document,
            self.clock.current_frame(),
            self.selected_clip_id.as_ref(),
            self.selected_layer_id.as_ref(),
        );
        let cfg = AgentSessionStartConfig {
            plugin: p,
            session_id: self.ai_chat.session_id.clone(),
            goal: String::new(),
            resume: true,
            attached_folders: self.ai_chat.attached_folders.clone(),
            attached_clips: Vec::new(),
            base_document: self.document.clone(),
            timeline_summary,
            agent_memory: self.ai_chat.agent_memory.clone(),
            provider,
            provider_kind: self.ai_chat.selected_provider.clone(),
            model_name: self.ai_chat.model_name.clone(),
            thinking_level: self.ai_chat.thinking_level,
            approval_mode: self.ai_chat.approval_mode,
            runtime_settings: self.ai_chat.runtime_settings.clone(),
            tool_registry: None,
            extra_extensions: Vec::new(),
            project_dir: self
                .project_path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
        };
        if let Ok(runner) = AgentSessionRunner::start(cfg) {
            self.ai_chat.active_agent = Some(runner);
        }
    }

    /// 保存済みセッションを削除する（現在開いているセッションを消した場合は新規セッションへ移行）。
    ///
    /// # Errors
    ///
    /// ストア未初期化または削除失敗時。
    pub fn delete_saved_ai_session(&mut self, session_id: &str) -> Result<(), String> {
        let store = self
            .ai_chat
            .session_store
            .as_ref()
            .ok_or_else(|| "セッションストアが初期化されていません".to_owned())?;
        store
            .delete_session(session_id)
            .map_err(|e| e.to_string())?;
        if self.ai_chat.session_id == session_id {
            self.ai_chat.new_session();
        }
        self.ai_chat.refresh_session_history();
        Ok(())
    }

    /// アプリ終了時に常駐エージェントプロセスを同期終了し、孤児プロセスの残留を防ぐ。
    pub fn shutdown_ai_agent(&mut self) {
        if let Some(handle) = self.ai_chat.running_job.take() {
            handle.cancelled.store(true, Ordering::Release);
        }
        if let Some(runner) = self.ai_chat.active_agent.take() {
            runner.shutdown();
        }
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
    }

    fn record_cancelled_preview(&mut self) {
        let partial = std::mem::take(&mut self.ai_chat.streaming_preview);
        let activity = self
            .ai_chat
            .live_activity
            .finalize_summary(self.ai_chat.responding_elapsed_secs(), true);
        let readable = extract_readable_cancelled_preview(&partial);
        if readable.is_some() || activity.is_some() {
            let body = readable.map_or_else(
                || "[停止済み]".to_owned(),
                |text| format!("[停止済み]\n{text}"),
            );
            self.ai_chat.push_turn_with_activity(
                AiRole::Assistant,
                body,
                Some(AiTurnBadge::Cancelled),
                activity,
            );
        }
    }

    /// 実行中の AI リクエストまたは常駐エージェントを中断し、再送信できる状態へ戻す（中断時 `true`）。
    pub fn cancel_ai_prompt(&mut self) -> bool {
        if let Some(handle) = self.ai_chat.running_job.take() {
            handle.cancelled.store(true, Ordering::Release);
            drop(handle);

            self.record_cancelled_preview();
            self.ai_chat.execution_state = AiExecutionState::Cancelled;
            "AI 生成を停止しました".clone_into(&mut self.ai_chat.status_banner);
            "AI 生成を停止しました".clone_into(&mut self.status_message);
            return true;
        }

        if let Some(runner) = self.ai_chat.active_agent.take() {
            let was_busy = self.ai_chat.agent_busy
                || self.ai_chat.pending_question.is_some()
                || self.ai_chat.pending_tool_approval.is_some();
            runner.cancel();
            self.ai_chat.agent_busy = false;
            self.ai_chat.agent_step_status = None;
            self.record_cancelled_preview();
            self.ai_chat.clear_pending_agent_interactions();
            if was_busy {
                self.ai_chat.execution_state = AiExecutionState::Cancelled;
                "エージェント実行を停止しました".clone_into(&mut self.ai_chat.status_banner);
                "エージェント実行を停止しました".clone_into(&mut self.status_message);
                return true;
            }
        } else if self.ai_chat.can_cancel() {
            // 異常終了等で active_agent が既に失われた後に待機状態だけ残留した場合の安全解除
            self.ai_chat.agent_busy = false;
            self.ai_chat.agent_step_status = None;
            self.record_cancelled_preview();
            self.ai_chat.clear_pending_agent_interactions();
            self.ai_chat.execution_state = AiExecutionState::Cancelled;
            "待機中のエージェント操作を解除しました".clone_into(&mut self.ai_chat.status_banner);
            "待機中のエージェント操作を解除しました".clone_into(&mut self.status_message);
            return true;
        }
        false
    }

    fn finish_running_job_error(&mut self, err_msg: &str) {
        self.ai_chat.streaming_preview.clear();
        let activity = self
            .ai_chat
            .live_activity
            .finalize_summary(self.ai_chat.responding_elapsed_secs(), false);
        self.ai_chat.last_error = Some(err_msg.to_owned());
        self.ai_chat.execution_state = AiExecutionState::Failed {
            summary: err_msg.to_owned(),
        };
        self.ai_chat.push_turn_with_activity(
            AiRole::Assistant,
            format!("エラー: {err_msg}"),
            Some(AiTurnBadge::Error),
            activity,
        );
        self.ai_chat.persist_session();
    }

    /// 非同期 AI チャット・バックグラウンドプローブおよび常駐エージェント受信キューをポーリングし、UI 更新が必要なら `true` を返す。
    pub fn poll_ai_chat(&mut self) -> bool {
        let mut changed = self.ai_chat.poll_background_probes();

        if let Some(handle) = self.ai_chat.running_job.take() {
            let mut completed = None;
            let mut failed = None;
            let mut disconnected = false;

            loop {
                match handle.rx.try_recv() {
                    Ok(evt) => {
                        changed = true;
                        match evt {
                            AiStreamEvent::Status(msg) => {
                                self.ai_chat.status_banner.clone_from(&msg);
                                self.ai_chat.set_responding_phase(msg, None);
                            }
                            AiStreamEvent::DeltaText(delta) => {
                                self.ai_chat.streaming_preview.push_str(&delta);
                                self.ai_chat
                                    .live_activity
                                    .on_stream_updated(&self.ai_chat.streaming_preview);
                            }
                            AiStreamEvent::ThoughtDelta(t) => {
                                self.ai_chat.live_activity.on_thought_delta(&t);
                            }
                            AiStreamEvent::ActivityHint(l) => {
                                self.ai_chat.live_activity.on_activity_hint(&l);
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
                let usage = resp.usage.clone().unwrap_or_else(|| {
                    AiTokenUsage::estimate_from_chars(
                        handle.estimated_input_chars,
                        resp.raw_text.chars().count(),
                    )
                });
                self.ai_chat.record_turn_usage(&usage);
                if self.ai_chat.selected_provider == AiProviderKind::OpenRouter {
                    self.ai_chat.refresh_openrouter_credits_async();
                }
                let _ = self.finalize_ai_response(&handle.prompt, &resp.model, &resp.raw_text);
                self.ai_chat.persist_session();
                return true;
            }
            if let Some(err_msg) = failed {
                self.ai_chat.status_banner = format!("AI エラー: {err_msg}");
                self.finish_running_job_error(&err_msg);
                return true;
            }
            if disconnected {
                let err_msg = "AI プロバイダが応答を返さず終了しました";
                err_msg.clone_into(&mut self.ai_chat.status_banner);
                self.finish_running_job_error(err_msg);
                return true;
            }
            self.ai_chat.running_job = Some(handle);
        }

        if let Some(runner) = self.ai_chat.active_agent.take() {
            let events = runner.poll_events();
            let mut exited = false;
            let mut need_persist = false;
            if !events.is_empty() {
                changed = true;
                for evt in events {
                    let (did_exit, persist_evt) = self.handle_agent_host_event(evt);
                    if did_exit {
                        exited = true;
                    }
                    if persist_evt {
                        need_persist = true;
                    }
                }
                if need_persist {
                    self.ai_chat.persist_session();
                }
            }
            if !exited && runner.is_alive() {
                self.ai_chat.active_agent = Some(runner);
            } else if !exited {
                // Exited イベント無しでプロセスが終了した場合も待機中カード等を解放してロックアウトを防ぐ
                let (_, persist_evt) = self.handle_agent_exited_event("unexpected_exit");
                changed = true;
                if persist_evt {
                    self.ai_chat.persist_session();
                }
            }
        }

        changed
    }

    fn handle_agent_host_event(&mut self, evt: AgentHostEvent) -> (bool, bool) {
        match evt {
            AgentHostEvent::Status {
                message,
                step,
                total_steps,
            } => self.handle_agent_status_event(message, step, total_steps),
            AgentHostEvent::StreamDelta { text } => {
                self.ai_chat.streaming_preview.push_str(&text);
                self.ai_chat
                    .live_activity
                    .on_stream_updated(&self.ai_chat.streaming_preview);
                (false, false)
            }
            AgentHostEvent::ThoughtDelta { text } => {
                self.ai_chat.live_activity.on_thought_delta(&text);
                (false, false)
            }
            AgentHostEvent::ActivityHint { label } => {
                self.ai_chat.live_activity.on_activity_hint(&label);
                (false, false)
            }
            AgentHostEvent::ToolStarted {
                call_id,
                verb,
                target,
                ..
            } => {
                self.ai_chat
                    .live_activity
                    .on_tool_started(&call_id, &verb, &target);
                (false, false)
            }
            AgentHostEvent::ToolActivity {
                call_id,
                tool_name,
                verb,
                target,
                ok,
                summary,
                duration_ms,
            } => {
                let update = ToolCompletionUpdate {
                    call_id: &call_id,
                    verb: &verb,
                    target: &target,
                    ok,
                    summary: &summary,
                    duration_ms,
                };
                self.handle_agent_tool_activity_event(update, &tool_name)
            }
            AgentHostEvent::MemoryUpdated { agent_memory } => {
                self.ai_chat.agent_memory = agent_memory;
                (false, true)
            }
            AgentHostEvent::UsageReported { usage } => self.handle_agent_usage_reported(&usage),
            evt @ AgentHostEvent::AskUser { .. } => self.handle_agent_ask_user_variant(evt),
            evt @ AgentHostEvent::ToolApprovalRequired { .. } => {
                self.handle_agent_tool_approval_variant(evt)
            }
            AgentHostEvent::Proposal {
                summary_markdown,
                plan,
                agent_memory,
                ..
            } => self.handle_agent_proposal_tuple(summary_markdown, *plan, agent_memory),
            AgentHostEvent::AgentError {
                code,
                message,
                recoverable,
            } => self.handle_agent_error_event(&code, &message, recoverable),
            AgentHostEvent::Exited { reason } => self.handle_agent_exited_event(&reason),
        }
    }

    fn handle_agent_ask_user_variant(&mut self, evt: AgentHostEvent) -> (bool, bool) {
        if let AgentHostEvent::AskUser {
            request_id,
            title,
            message,
            fields,
            choices,
            allow_skip,
        } = evt
        {
            self.handle_agent_ask_user_event(PendingAgentQuestionSnapshot {
                request_id,
                title,
                message,
                fields,
                choices,
                allow_skip,
            });
            return (false, true);
        }
        (false, false)
    }

    fn handle_agent_tool_approval_variant(&mut self, evt: AgentHostEvent) -> (bool, bool) {
        if let AgentHostEvent::ToolApprovalRequired {
            request_id,
            tool_name,
            arguments,
            description,
        } = evt
        {
            self.handle_agent_tool_approval_event(request_id, &tool_name, arguments, description);
        }
        (false, false)
    }

    fn handle_agent_usage_reported(&mut self, usage: &AiTokenUsage) -> (bool, bool) {
        self.ai_chat.record_turn_usage(usage);
        if self.ai_chat.selected_provider == AiProviderKind::OpenRouter {
            self.ai_chat.refresh_openrouter_credits_async();
        }
        (false, true)
    }

    fn handle_agent_proposal_tuple(
        &mut self,
        summary_markdown: String,
        plan: AiEditPlan,
        agent_memory: Value,
    ) -> (bool, bool) {
        self.handle_agent_proposal_event(summary_markdown, plan, agent_memory);
        (false, true)
    }

    fn handle_agent_tool_activity_event(
        &mut self,
        update: ToolCompletionUpdate<'_>,
        tool_name: &str,
    ) -> (bool, bool) {
        let ok = update.ok;
        let summary_text = update.summary.to_owned();
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.live_activity.clear_stream_progress();
        self.ai_chat.live_activity.on_tool_completed(update);
        let banner = if ok {
            format!("ツール実行完了: {tool_name}")
        } else {
            format!("ツール失敗 ({tool_name}): {summary_text}")
        };
        self.ai_chat.status_banner.clone_from(&banner);
        self.ai_chat
            .set_responding_phase(banner, self.ai_chat.agent_step_status);
        (false, false)
    }

    fn handle_agent_status_event(
        &mut self,
        message: String,
        step: Option<u32>,
        total_steps: Option<u32>,
    ) -> (bool, bool) {
        self.ai_chat.status_banner.clone_from(&message);
        if step.is_some() || total_steps.is_some() {
            self.ai_chat.agent_step_status = Some((step, total_steps));
        }
        if step.is_some() {
            self.ai_chat.streaming_preview.clear();
            self.ai_chat.live_activity.clear_stream_progress();
        }
        self.ai_chat
            .set_responding_phase(message, self.ai_chat.agent_step_status);
        (false, false)
    }

    fn handle_agent_tool_approval_event(
        &mut self,
        request_id: String,
        tool_name: &str,
        arguments: Value,
        description: String,
    ) {
        self.ai_chat.agent_busy = false;
        self.ai_chat.execution_state = AiExecutionState::WaitingApproval {
            tool_name: tool_name.to_owned(),
        };
        self.ai_chat.pending_tool_approval = Some(PendingAgentToolApproval {
            request_id,
            tool_name: tool_name.to_owned(),
            arguments,
            description,
        });
        self.ai_chat.status_banner =
            format!("Manual 承認待ち: ツール `{tool_name}` の実行許可が必要です");
    }

    fn handle_agent_exited_event(&mut self, reason: &str) -> (bool, bool) {
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
        self.ai_chat.clear_pending_agent_interactions();
        if reason != "cancelled" {
            self.ai_chat.status_banner = format!("エージェントプロセス終了: {reason}");
            if matches!(
                self.ai_chat.execution_state,
                AiExecutionState::Responding { .. }
                    | AiExecutionState::WaitingUserInput { .. }
                    | AiExecutionState::WaitingApproval { .. }
            ) {
                self.ai_chat.execution_state = AiExecutionState::Failed {
                    summary: self.ai_chat.status_banner.clone(),
                };
            }
        }
        (true, true)
    }

    fn handle_agent_error_event(
        &mut self,
        code: &str,
        message: &str,
        recoverable: bool,
    ) -> (bool, bool) {
        let activity = self
            .ai_chat
            .live_activity
            .finalize_summary(self.ai_chat.responding_elapsed_secs(), false);
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
        self.ai_chat.clear_pending_agent_interactions();
        let formatted = format!("[{code}] {message}");
        self.ai_chat.last_error = Some(formatted.clone());
        self.ai_chat.execution_state = AiExecutionState::Failed {
            summary: formatted.clone(),
        };
        self.ai_chat.push_turn_with_activity(
            AiRole::Assistant,
            format!("エラー: {formatted}"),
            Some(AiTurnBadge::Error),
            activity,
        );
        self.ai_chat.status_banner = if recoverable {
            format!("中断 ({code}): 別プロバイダへ切替や追加入力で続行できます")
        } else {
            format!("致命的エラー: {formatted}")
        };
        (!recoverable, true)
    }

    fn handle_agent_ask_user_event(&mut self, snap: PendingAgentQuestionSnapshot) {
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.live_activity.clear_stream_progress();
        self.ai_chat.ask_field_values.clear();
        for field in &snap.fields {
            if let Some(def) = &field.default_value {
                self.ai_chat
                    .ask_field_values
                    .insert(field.id.clone(), def.clone());
            }
        }
        self.ai_chat.push_turn(
            AiRole::Assistant,
            format!("[確認] {}\n{}", snap.title, snap.message),
            None,
        );
        self.ai_chat.execution_state = AiExecutionState::WaitingUserInput {
            detail: snap.title.clone(),
        };
        self.ai_chat.pending_question = Some(snap);
        self.ai_chat.scroll_to_bottom_pending = true;
        "エージェントが確認事項を送りました（カードから入力または選択してください）"
            .clone_into(&mut self.ai_chat.status_banner);
    }

    fn handle_agent_proposal_event(
        &mut self,
        summary_markdown: String,
        plan: AiEditPlan,
        agent_memory: Value,
    ) {
        let activity = self
            .ai_chat
            .live_activity
            .finalize_summary(self.ai_chat.responding_elapsed_secs(), false);
        self.ai_chat.agent_busy = false;
        self.ai_chat.agent_step_status = None;
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.last_error = None;
        self.ai_chat.agent_memory = agent_memory;
        if plan.is_no_op() {
            self.ai_chat.pending_proposal = None;
            self.ai_chat.push_turn_with_activity(
                AiRole::Assistant,
                summary_markdown,
                Some(AiTurnBadge::CompletedNoChange),
                activity,
            );
            let completed_summary = tr("ai.status_completed_no_change");
            self.ai_chat.status_banner.clone_from(&completed_summary);
            self.ai_chat.execution_state = AiExecutionState::Completed {
                summary: completed_summary,
                changed_timeline: false,
            };
        } else if self.ai_chat.approval_mode == AiApprovalMode::Autopilot {
            self.ai_chat.pending_proposal = None;
            let _ = self.apply_ai_plan_with_summary_and_activity(
                &plan,
                true,
                Some(&summary_markdown),
                activity,
            );
        } else {
            let mut body = summary_markdown;
            if !body.contains("承認して適用") && !body.contains("準備完了") {
                body.push_str(
                    "\n\n✅ 編集案の作成が完了しました。下の「承認して適用」でタイムラインへ反映できます。",
                );
            }
            self.ai_chat.push_turn_with_activity(
                AiRole::Assistant,
                body,
                Some(AiTurnBadge::ProposalReady),
                activity,
            );
            self.ai_chat.pending_proposal = Some(PendingAiProposal {
                prompt: self.ai_chat.agent_active_prompt.clone(),
                model: self.ai_chat.agent_active_model_label.clone(),
                plan,
            });
            let proposal_summary = tr("ai.status_completed_proposal");
            self.ai_chat.status_banner.clone_from(&proposal_summary);
            self.ai_chat.execution_state = AiExecutionState::ProposalReady {
                summary: proposal_summary,
            };
        }
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
        self.apply_ai_plan_with_summary_and_activity(&proposal.plan, false, None, None)
    }

    fn apply_ai_plan_with_summary_and_activity(
        &mut self,
        plan: &AiEditPlan,
        is_autopilot: bool,
        reply_note: Option<&str>,
        activity: Option<AiTurnActivitySummary>,
    ) -> Result<(), String> {
        if let Err(err) = self.dispatch_command(&plan.command) {
            let msg = format!("提案の適用に失敗しました: {err}");
            self.ai_chat.last_error = Some(msg.clone());
            self.ai_chat.push_turn_with_activity(
                AiRole::Assistant,
                msg.clone(),
                Some(AiTurnBadge::Error),
                activity,
            );
            self.ai_chat.status_banner.clone_from(&msg);
            self.ai_chat.execution_state = AiExecutionState::Failed {
                summary: msg.clone(),
            };
            return Err(msg);
        }
        self.inspector.invalidate_sync();
        self.ai_chat.can_one_click_undo = true;
        self.ai_chat.streaming_preview.clear();
        let (banner, status, prefix) = if is_autopilot {
            (
                tr("ai.status_completed_applied"),
                "AI 編集プランを自動適用しました",
                "✅ 作業が完了し、タイムラインへ自動適用しました [Autopilot 適用済み]:",
            )
        } else {
            (
                tr("ai.status_completed_applied"),
                "AI 編集プランを適用しました",
                "[適用済み]",
            )
        };
        let summary_joined = plan.summary_lines.join(" / ");
        let turn_text = match reply_note.map(str::trim).filter(|s| !s.is_empty()) {
            Some(note) => format!("{note}\n\n{prefix} {summary_joined}"),
            None => format!("{prefix} {summary_joined}"),
        };
        self.ai_chat.status_banner.clone_from(&banner);
        status.clone_into(&mut self.status_message);
        self.ai_chat.execution_state = AiExecutionState::Completed {
            summary: banner,
            changed_timeline: true,
        };
        self.ai_chat.push_turn_with_activity(
            AiRole::Assistant,
            turn_text,
            Some(AiTurnBadge::Applied),
            activity,
        );
        self.ai_chat.persist_session();
        Ok(())
    }

    /// 承認待ちの AI 編集提案を適用せずに破棄する。
    pub fn discard_pending_ai_proposal(&mut self) {
        self.ai_chat.pending_proposal = None;
        self.ai_chat.streaming_preview.clear();
        self.ai_chat.execution_state = AiExecutionState::Idle;
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
        self.ai_chat.execution_state = AiExecutionState::Idle;
        "直前の AI 編集を取り消しました（Undo）".clone_into(&mut self.ai_chat.status_banner);
        Ok(())
    }

    fn finalize_ai_response(
        &mut self,
        prompt: &str,
        model: &str,
        raw_text: &str,
    ) -> Result<(), String> {
        self.ai_chat.streaming_preview.clear();
        let activity = self
            .ai_chat
            .live_activity
            .finalize_summary(self.ai_chat.responding_elapsed_secs(), false);
        match dry_run_validate(&self.document, raw_text) {
            Ok(plan) if plan.is_no_op() => {
                let note = plan
                    .reply_note
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("応答を受信しました。");
                let reply_text = format!(
                    "{note}\n\n✅ 応答が完了しました（タイムラインへの変更はありません）。"
                );
                self.ai_chat.last_error = None;
                self.ai_chat.pending_proposal = None;
                self.ai_chat.push_turn_with_activity(
                    AiRole::Assistant,
                    reply_text,
                    Some(AiTurnBadge::CompletedNoChange),
                    activity,
                );
                let completed_summary = tr("ai.status_completed_no_change");
                self.ai_chat.status_banner.clone_from(&completed_summary);
                self.ai_chat.execution_state = AiExecutionState::Completed {
                    summary: completed_summary,
                    changed_timeline: false,
                };
                Ok(())
            }
            Ok(plan) => {
                self.ai_chat.last_error = None;
                if self.ai_chat.approval_mode == AiApprovalMode::Autopilot {
                    self.ai_chat.pending_proposal = None;
                    return self.apply_ai_plan_with_summary_and_activity(
                        &plan,
                        true,
                        plan.reply_note.as_deref(),
                        activity,
                    );
                }
                let summary_body = plan.summary_lines.join("\n");
                let mut preview = match plan
                    .reply_note
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(note) if !summary_body.is_empty() => format!("{note}\n\n{summary_body}"),
                    Some(note) => note.to_owned(),
                    None => summary_body,
                };
                preview.push_str(
                    "\n\n✅ 編集案の作成が完了しました。下の「承認して適用」でタイムラインへ反映できます。",
                );
                self.ai_chat.push_turn_with_activity(
                    AiRole::Assistant,
                    preview,
                    Some(AiTurnBadge::ProposalReady),
                    activity,
                );
                self.ai_chat.pending_proposal = Some(PendingAiProposal {
                    prompt: prompt.to_owned(),
                    model: model.to_owned(),
                    plan,
                });
                let proposal_summary = tr("ai.status_completed_proposal");
                self.ai_chat.status_banner.clone_from(&proposal_summary);
                self.ai_chat.execution_state = AiExecutionState::ProposalReady {
                    summary: proposal_summary,
                };
                Ok(())
            }
            Err(err) => {
                let msg = format!("コマンド検証エラー（タイムラインは無変更）: {err}");
                self.ai_chat.last_error = Some(msg.clone());
                self.ai_chat.push_turn_with_activity(
                    AiRole::Assistant,
                    msg.clone(),
                    Some(AiTurnBadge::Error),
                    activity,
                );
                self.ai_chat.status_banner.clone_from(&msg);
                self.ai_chat.execution_state = AiExecutionState::Failed {
                    summary: msg.clone(),
                };
                Err(msg)
            }
        }
    }
}

fn resolve_target_agent_plugin(
    chat: &AiChatState,
    plugin_override: Option<DiscoveredAgentPlugin>,
) -> Result<DiscoveredAgentPlugin, String> {
    if let Some(explicit) = plugin_override {
        return Ok(explicit);
    }
    let reg = AgentPluginRegistry::discover_default();
    let target_id = chat
        .selected_agent_id
        .as_deref()
        .unwrap_or("sea.agent.director");
    reg.find_by_id(target_id)
        .or_else(|| reg.plugins().first())
        .cloned()
        .ok_or_else(|| {
            format!("エージェントプラグイン `{target_id}` が見つかりません（cargo build --workspace でビルドされているか確認してください）")
        })
}

fn build_compact_timeline_summary(
    doc: &ProjectDocument,
    playhead: crate::core::time::FrameIndex,
    selected_clip: Option<&ClipId>,
    selected_layer: Option<&crate::core::ids::LayerId>,
) -> String {
    let ctx = build_timeline_context(doc, playhead, selected_clip, selected_layer);
    serde_json::to_string(&ctx).unwrap_or_else(|_| {
        format!(
            "project={}, tracks={}, assets={}",
            doc.name,
            doc.layers.len(),
            doc.assets.len()
        )
    })
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

/// AI チャットパネル描画に必要なウィジェット参照束。
#[derive(Clone, Copy)]
pub struct AiChatPanelWidgets<'a> {
    /// プロバイダ選択コンボボックス。
    pub ai_provider_select: &'a Entity<SelectState<Vec<AiProviderSelectItem>>>,
    /// プロンプト入力欄。
    pub ai_chat_input: &'a Entity<TextareaState>,
    /// 対話確認カード用の入力欄一覧。
    pub ai_ask_inputs: &'a [(String, Entity<InputState>)],
    /// チャット履歴コンテナ用のフォーカスハンドル。
    pub ai_chat_history_focus: &'a FocusHandle,
    /// チャット履歴スクロールハンドル。
    pub ai_chat_scroll_handle: &'a ScrollHandle,
}

/// 右側 AI Chat タブの VS Code 風チャットパネルを描画する。
///
/// 上部はプロバイダ切替・セッション履歴・設定のみのシンプルなヘッダとし、
/// 会話領域・リッチ対話カード・承認差分カードを下側へ伸縮させ、
/// モデル／エージェント／フォルダ添付を内包したコンポーザーを最下部に固定配置する。
pub fn render_ai_chat_panel(
    controller: &AppController,
    widgets: AiChatPanelWidgets<'_>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let AiChatPanelWidgets {
        ai_provider_select,
        ai_chat_input,
        ai_ask_inputs,
        ai_chat_history_focus,
        ai_chat_scroll_handle,
    } = widgets;
    let chat = &controller.ai_chat;
    let theme = cx.theme().clone();
    let drag_theme = theme.clone();

    v_flex()
        .size_full()
        .min_h(px(0.))
        .text_xs()
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style.border_1().border_color(drag_theme.primary)
        })
        .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
            let mut attached = false;
            for p in paths.paths() {
                if p.is_dir() {
                    this.controller
                        .ai_chat
                        .attach_folder_with_auto_agent(p.clone());
                    attached = true;
                }
            }
            if attached {
                cx.notify();
            }
        }))
        .child(render_compact_header(chat, ai_provider_select, cx))
        .children(chat.settings_load_warning.as_ref().map(|warn| {
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_1p5()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(gpui_kit::rgba(0xf59e_0bff))
                .bg(theme.secondary)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_xs()
                        .text_color(gpui_kit::rgba(0xfde6_8aff))
                        .child(warn.clone()),
                )
                .child(
                    Button::new("ai-repair-settings-json")
                        .xsmall()
                        .outline()
                        .label(tr("ai.open_json"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_ai_settings_json_modal(window, cx);
                        })),
                )
                .into_any_element()
        }))
        .children(match chat.active_drawer {
            AiChatDrawer::SessionHistory => {
                Some(render_session_history_drawer(chat, cx).into_any_element())
            }
            AiChatDrawer::None => None,
        })
        .child(render_chat_body(
            chat,
            ai_ask_inputs,
            ai_chat_history_focus,
            ai_chat_scroll_handle,
            cx,
        ))
        .children(render_status_bar(chat, &theme))
        .child(render_prompt_composer(controller, ai_chat_input, cx))
}

fn format_cred_label(provider: &AiProviderKind, has_cred: bool) -> String {
    if !provider.requires_api_key() {
        return match provider {
            AiProviderKind::OllamaCompat => tr("ai.cred_local"),
            AiProviderKind::Plugin { .. } => tr("ai.cred_cli"),
            _ => "認証不要".to_owned(),
        };
    }
    if has_cred {
        tr("ai.cred_ok")
    } else {
        tr("ai.cred_missing")
    }
}

fn render_compact_header(
    chat: &AiChatState,
    ai_provider_select: &Entity<SelectState<Vec<AiProviderSelectItem>>>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let has_cred = chat.has_cached_credentials();
    let missing_required = chat.selected_provider.requires_api_key() && !has_cred;
    let cred_color = if missing_required {
        theme.danger
    } else {
        theme.muted_foreground
    };

    let mut history_btn = Button::new("ai-session-history-toggle")
        .xsmall()
        .label(tr("ai.history"));
    history_btn = if chat.active_drawer == AiChatDrawer::SessionHistory {
        history_btn.primary()
    } else {
        history_btn.ghost()
    };

    let settings_btn = Button::new("ai-agent-settings-toggle")
        .ghost()
        .xsmall()
        .label("⚙");

    let cred_badge = div()
        .id("ai-header-cred-btn")
        .cursor_pointer()
        .px_1()
        .py_0p5()
        .rounded(px(4.))
        .hover(|s| s.bg(theme.secondary))
        .text_color(cred_color)
        .child(format_cred_label(&chat.selected_provider, has_cred))
        .on_click(cx.listener(|this, _, window, cx| {
            this.open_ai_settings_modal(AiSettingsTab::ApiKeys, window, cx);
        }));

    let mut right_actions = h_flex()
        .items_center()
        .gap_0p5()
        .flex_shrink_0()
        .child(cred_badge)
        .child(history_btn.on_click(cx.listener(|this, _, _, cx| {
            this.controller.ai_chat.active_drawer =
                if this.controller.ai_chat.active_drawer == AiChatDrawer::SessionHistory {
                    AiChatDrawer::None
                } else {
                    this.controller.ai_chat.refresh_session_history();
                    AiChatDrawer::SessionHistory
                };
            cx.notify();
        })))
        .child(settings_btn.on_click(cx.listener(|this, _, window, cx| {
            this.open_ai_settings_modal(AiSettingsTab::ApiKeys, window, cx);
        })));

    if !chat.turns.is_empty() || chat.active_agent.is_some() {
        right_actions = right_actions.child(
            Button::new("ai-new-chat")
                .ghost()
                .xsmall()
                .disabled(chat.is_busy())
                .label("+")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.ai_chat.new_session();
                    this.sync_widgets_from_controller(cx);
                    cx.notify();
                })),
        );
    }

    h_flex()
        .w_full()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .gap_1()
        .px_2()
        .py_1p5()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div().flex_1().min_w(px(64.)).overflow_hidden().child(
                Select::new(ai_provider_select)
                    .menu_width(px(230.))
                    .with_size(Size::XSmall),
            ),
        )
        .child(right_actions)
}

fn render_chat_body(
    chat: &AiChatState,
    ai_ask_inputs: &[(String, Entity<InputState>)],
    ai_chat_history_focus: &FocusHandle,
    ai_chat_scroll_handle: &ScrollHandle,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::AnyElement {
    use gpui_kit::IntoElement as _;

    let theme = cx.theme().clone();
    let is_idle_empty = chat.turns.is_empty()
        && !chat.is_busy()
        && chat.pending_proposal.is_none()
        && chat.pending_question.is_none()
        && chat.pending_tool_approval.is_none()
        && chat.last_error.is_none()
        && !chat.can_one_click_undo;

    if is_idle_empty {
        return render_welcome_empty_state(chat, &theme).into_any_element();
    }

    let mut thread = v_flex().w_full().gap_2().px_2().py_2();
    let collapsed_count = collapsed_history_count(chat.turns.len(), chat.show_all_history);
    if collapsed_count > 0 {
        let count_str = collapsed_count.to_string();
        thread = thread.child(
            Button::new("ai-show-older-history")
                .ghost()
                .xsmall()
                .label(tr_fmt("ai.show_older_history", &[("count", &count_str)]))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.ai_chat.show_all_history = true;
                    cx.notify();
                })),
        );
    }
    for turn in &chat.turns[collapsed_count..] {
        thread = thread.child(render_chat_turn(
            turn,
            &chat.expanded_activity_turns,
            &theme,
            cx,
        ));
    }
    if chat.is_busy() {
        thread = thread.child(render_live_activity_card(chat, &theme, cx));
    }
    if let Some(approval) = &chat.pending_tool_approval {
        thread = thread.child(render_tool_approval_card(approval, cx));
    }
    if let Some(question) = &chat.pending_question {
        thread = thread.child(render_ask_user_card(question, ai_ask_inputs, cx));
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
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Err(err) = this.controller.undo_last_ai_plan() {
                        this.controller.ai_chat.last_error = Some(err);
                    }
                    this.sync_widgets_from_controller(cx);
                    window.focus(&this.workspace_focus, cx);
                    cx.notify();
                })),
        );
    }
    if let Some(err) = &chat.last_error {
        thread = thread.child(render_error_card(err, &theme));
    }

    div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .w_full()
        .min_h(px(0.))
        .overflow_y_hidden()
        .child(
            div()
                .id("ai-chat-history-scroll")
                .track_focus(ai_chat_history_focus)
                .size_full()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(ai_chat_scroll_handle)
                .child(thread),
        )
        .vertical_scrollbar(ai_chat_scroll_handle)
        .into_any_element()
}

const fn collapsed_history_count(total_turns: usize, show_all_history: bool) -> usize {
    if total_turns > CHAT_HISTORY_COLLAPSE_THRESHOLD && !show_all_history {
        total_turns - CHAT_HISTORY_COLLAPSE_THRESHOLD
    } else {
        0
    }
}

fn render_error_card(err: &str, theme: &gpui_kit::component::Theme) -> impl IntoElement {
    let md: SharedString = escape_plain_text_for_markdown(err).into();
    v_flex()
        .w_full()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.danger)
        .text_color(theme.danger)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(div().text_xs().font_semibold().child("エラー"))
                .child(
                    Clipboard::new("ai-error-copy")
                        .value(err.to_owned())
                        .tooltip(tr("ai.copy_message")),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .child(TextView::markdown("ai-error-md", md).selectable(true)),
        )
}

fn render_welcome_empty_state(
    chat: &AiChatState,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
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
    welcome
}

fn render_status_bar(
    chat: &AiChatState,
    theme: &gpui_kit::component::Theme,
) -> Option<gpui_kit::AnyElement> {
    use gpui_kit::IntoElement as _;

    let (icon, text, color) = match &chat.execution_state {
        AiExecutionState::Idle if chat.turns.is_empty() => return None,
        AiExecutionState::Idle => ("•", chat.status_banner.clone(), theme.muted_foreground),
        AiExecutionState::Responding { phase, step, .. } => {
            let elapsed = chat
                .responding_elapsed_secs()
                .map(|s| format!(" ({})", format_elapsed_duration(s)))
                .unwrap_or_default();
            let step_tag = match step {
                Some((Some(s), Some(t))) => format!("[{s}/{t}] "),
                Some((Some(s), None)) => format!("[ステップ {s}] "),
                _ => String::new(),
            };
            ("⏳", format!("{step_tag}{phase}{elapsed}"), theme.primary)
        }
        AiExecutionState::WaitingUserInput { detail } => (
            "?",
            format!("質問カードへの回答待ち: {detail}"),
            gpui_kit::rgba(0xf59e_0bff).into(),
        ),
        AiExecutionState::WaitingApproval { tool_name } => (
            "⚠",
            format!("Manual 承認待ち: `{tool_name}`"),
            gpui_kit::rgba(0xf59e_0bff).into(),
        ),
        AiExecutionState::ProposalReady { summary } => {
            ("✓", summary.clone(), gpui_kit::rgba(0x22c5_5eff).into())
        }
        AiExecutionState::Completed {
            summary,
            changed_timeline,
        } => {
            let c = if *changed_timeline {
                gpui_kit::rgba(0x22c5_5eff).into()
            } else {
                theme.muted_foreground
            };
            ("✓", summary.clone(), c)
        }
        AiExecutionState::Cancelled => (
            "■",
            "停止済み（再送信できます）".to_owned(),
            theme.muted_foreground,
        ),
        AiExecutionState::Failed { summary } => ("✕", format!("エラー: {summary}"), theme.danger),
    };

    Some(
        h_flex()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .text_xs()
            .child(div().flex_shrink_0().text_color(color).child(icon))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(color)
                    .child(text),
            )
            .into_any_element(),
    )
}

fn turn_badge_label(badge: AiTurnBadge) -> &'static str {
    match badge {
        AiTurnBadge::Applied => "完了・適用済み",
        AiTurnBadge::ProposalReady => "完了・承認待ち",
        AiTurnBadge::CompletedNoChange => "完了・変更なし",
        AiTurnBadge::Cancelled => "停止済み",
        AiTurnBadge::Error => "エラー",
    }
}

fn render_chat_turn(
    turn: &AiChatTurn,
    expanded_activity_turns: &HashSet<u64>,
    theme: &gpui_kit::component::Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::AnyElement {
    use gpui_kit::IntoElement as _;

    let copy_btn = Clipboard::new(("ai-turn-copy", turn.id))
        .value(turn.text.clone())
        .tooltip(tr("ai.copy_message"));

    match turn.role {
        AiRole::User => v_flex()
            .w_full()
            .gap_0p5()
            .p_2()
            .rounded_md()
            .bg(theme.secondary)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(div().text_color(theme.muted_foreground).child("You"))
                    .child(copy_btn),
            )
            .child(
                div().w_full().min_w(px(0.)).child(
                    TextView::markdown(("ai-turn-user", turn.id), turn.rendered_markdown.clone())
                        .selectable(true),
                ),
            )
            .into_any_element(),
        AiRole::Assistant => {
            let mut left_meta = h_flex()
                .items_center()
                .gap_1p5()
                .child(div().text_color(theme.muted_foreground).child("AI"));
            if let Some(badge) = turn.status_badge {
                left_meta = left_meta.child(
                    div()
                        .px_1()
                        .rounded(px(4.))
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .child(turn_badge_label(badge)),
                );
            }
            v_flex()
                .w_full()
                .gap_0p5()
                .px_1()
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .child(left_meta)
                        .child(copy_btn),
                )
                .children(turn.activity_log.as_ref().map(|summary| {
                    render_turn_activity_summary(
                        turn.id,
                        summary,
                        expanded_activity_turns.contains(&turn.id),
                        theme,
                        cx,
                    )
                }))
                .child(
                    div().w_full().min_w(px(0.)).child(
                        TextView::markdown(("ai-turn-md", turn.id), turn.rendered_markdown.clone())
                            .selectable(true),
                    ),
                )
                .into_any_element()
        }
        AiRole::System => v_flex()
            .w_full()
            .gap_0p5()
            .px_1()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(div().text_color(theme.muted_foreground).child("System"))
                    .child(copy_btn),
            )
            .child(
                div().w_full().min_w(px(0.)).child(
                    TextView::markdown(("ai-turn-sys", turn.id), turn.rendered_markdown.clone())
                        .selectable(true),
                ),
            )
            .into_any_element(),
    }
}

fn render_diff_card(
    proposal: &PendingAiProposal,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let summary_lines: String = proposal
        .plan
        .summary_lines
        .iter()
        .take(6)
        .map(|l| format!("- {}", escape_plain_text_for_markdown(l)))
        .collect::<Vec<_>>()
        .join("\n");
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
    if !summary_lines.is_empty() {
        card = card.child(
            div().w_full().min_w(px(0.)).child(
                TextView::markdown("ai-diff-summary-md", SharedString::from(summary_lines))
                    .selectable(true),
            ),
        );
    }
    card.child(
        h_flex()
            .gap_2()
            .child(
                Button::new("ai-apply-plan")
                    .primary()
                    .small()
                    .label(tr("ai.approve"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Err(err) = this.controller.approve_pending_ai_proposal() {
                            this.controller.ai_chat.last_error = Some(err);
                        }
                        this.sync_widgets_from_controller(cx);
                        window.focus(&this.workspace_focus, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ai-discard-plan")
                    .ghost()
                    .small()
                    .label(tr("ai.discard"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.controller.discard_pending_ai_proposal();
                        window.focus(&this.workspace_focus, cx);
                        cx.notify();
                    })),
            ),
    )
}

fn render_model_menu_button(
    chat: &AiChatState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let models = chat.resolved_models_for_current_provider();
    let supports_custom = chat.selected_provider.supports_custom_models();
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

            for model in &models {
                let is_selected = model.id == current_model;
                let row_label = model.format_menu_label(is_selected, current_thinking);
                let model_id = model.id.clone();
                let weak_item = weak.clone();
                popup = popup.item(
                    PopupMenuItem::new(row_label)
                        .checked(is_selected)
                        .disabled(!model.available)
                        .on_click(move |_, _, cx| {
                            let _ = weak_item.update(cx, |view, cx| {
                                view.controller
                                    .ai_chat
                                    .select_model_default_medium(&model_id);
                                cx.notify();
                            });
                        }),
                );
            }

            popup = popup.separator();
            if supports_custom {
                let weak_manage = weak.clone();
                popup = popup.item(PopupMenuItem::new(tr("ai.add_manage_models")).on_click(
                    move |_, window, cx| {
                        let _ = weak_manage.update(cx, |view, cx| {
                            view.open_ai_settings_modal(AiSettingsTab::Models, window, cx);
                        });
                    },
                ));
            } else {
                let weak_refresh = weak.clone();
                popup = popup.item(PopupMenuItem::new(tr("ai.refresh_models")).on_click(
                    move |_, _, cx| {
                        let _ = weak_refresh.update(cx, |view, cx| {
                            view.controller.ai_chat.probe_current_plugin_models_async();
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
                for &level in &options {
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
    _controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();

    Button::new("ai-attach-context-btn")
        .ghost()
        .xsmall()
        .label("📎 添付 ▾")
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, cx| {
            let (selected_items, attached_set, all_clips) = weak
                .read_with(cx, |view, _| {
                    let selected_ids = view.controller.effective_selected_clip_ids();
                    let selected_items: Vec<(ClipId, String)> = selected_ids
                        .iter()
                        .filter_map(|cid| {
                            view.controller
                                .document
                                .find_clip(cid)
                                .map(|(_, c)| (cid.clone(), c.name.clone()))
                        })
                        .collect();
                    let attached_set = view.controller.ai_chat.attached_clip_ids.clone();
                    let all_clips: Vec<(ClipId, String, String, u64, u64)> = view
                        .controller
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
                    (selected_items, attached_set, all_clips)
                })
                .unwrap_or_default();

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
    _controller: &AppController,
    ai_chat_input: &Entity<TextareaState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let weak = cx.weak_entity();
    let input_entity = ai_chat_input.clone();

    Button::new("ai-at-mention-btn")
        .ghost()
        .xsmall()
        .label("@ ファイル ▾")
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, cx| {
            let names = weak
                .read_with(cx, |view, _| {
                    matching_at_mention_names(&view.controller.document, "")
                })
                .unwrap_or_default();

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
        .child(render_agent_mode_menu_button(&controller.ai_chat, cx))
        .child(render_folder_attach_button(cx))
        .child(render_attach_context_button(controller, cx))
        .child(render_at_mention_button(controller, ai_chat_input, cx));

    for (idx, folder) in controller.ai_chat.attached_folders.iter().enumerate() {
        row = row.child(render_attached_folder_chip(idx, folder, cx));
    }

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

    let action_btn = if chat.can_cancel() {
        Button::new("ai-stop-btn")
            .danger()
            .xsmall()
            .label(tr("ai.stop"))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.controller.cancel_ai_prompt() {
                    this.sync_widgets_from_controller(cx);
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
        )
        .child(render_usage_footer(chat, cx));

    let mut outer = v_flex().w_full().px_2().pb_2().pt_1().flex_shrink_0();
    if let Some(ac_bar) = render_at_autocomplete_bar(controller, ai_chat_input, cx) {
        outer = outer.child(ac_bar);
    }
    outer.child(card)
}

fn format_compact_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let tenths = tokens / 100;
        if tenths.is_multiple_of(10) || tokens >= 100_000 {
            return format!("{}k", tokens / 1_000);
        }
        return format!("{}.{}k", tenths / 10, tenths % 10);
    }
    let tenths_m = tokens / 100_000;
    if tenths_m.is_multiple_of(10) {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}.{}M", tenths_m / 10, tenths_m % 10)
    }
}

fn format_usd(amount: f64) -> String {
    if !amount.is_finite() || amount <= 0.0 {
        return "$0.00".to_owned();
    }
    if amount < 0.01 {
        return format!("${amount:.4}");
    }
    if amount < 1.0 {
        return format!("${amount:.3}");
    }
    format!("${amount:.2}")
}

fn context_usage_info(chat: &AiChatState) -> Option<(u64, u64, u64)> {
    let win = chat
        .active_resolved_model()
        .and_then(|m| m.context_window_tokens)
        .filter(|&w| w > 0)?;
    let used = chat.session_usage.last_context_tokens;
    let remaining = win.saturating_sub(used);
    Some((win, used, remaining))
}

fn format_session_used_footer(chat: &AiChatState, ctx: Option<(u64, u64, u64)>) -> String {
    let prefix = tr("ai.usage_session");
    let est = if chat.session_usage.has_estimated_tokens {
        "~"
    } else {
        ""
    };
    let tok_str = format!(
        "{est}{} tok",
        format_compact_tokens(chat.session_usage.total_tokens)
    );
    let cost_str = chat
        .session_usage
        .has_known_cost
        .then(|| format_usd(chat.session_usage.cost_usd));
    let budget = chat
        .user_settings
        .budget_usd
        .filter(|b| b.is_finite() && *b > 0.0);
    let pct_str = if let Some(b) = budget
        && chat.session_usage.has_known_cost
    {
        let pct = ((chat.session_usage.cost_usd / b) * 100.0).clamp(0.0, 999.0);
        Some(format!("{pct:.0}%"))
    } else {
        ctx.map(|(win, used, _)| format!("{}%", used.saturating_mul(100) / win))
    };

    match chat.user_settings.usage_display_mode {
        UsageDisplayPreference::Tokens => format!("{prefix} {tok_str}"),
        UsageDisplayPreference::Cost => {
            if chat.selected_provider == AiProviderKind::OllamaCompat {
                format!("{prefix} Local ($0.00)")
            } else if let Some(cost) = cost_str {
                format!("{prefix} {cost}")
            } else {
                format!("{prefix} $— ({tok_str})")
            }
        }
        UsageDisplayPreference::Percent => pct_str.map_or_else(
            || format!("{prefix} {tok_str}"),
            |pct| format!("{prefix} {pct}"),
        ),
        UsageDisplayPreference::Auto => {
            if chat.selected_provider == AiProviderKind::OllamaCompat {
                format!("{prefix} {tok_str} (Local)")
            } else if matches!(chat.selected_provider, AiProviderKind::Plugin { .. }) {
                if let Some(cost) = cost_str {
                    format!("{prefix} {tok_str} ({cost})")
                } else if let Some(pct) = pct_str {
                    format!("{prefix} {tok_str} ({pct})")
                } else {
                    format!("{prefix} {tok_str}")
                }
            } else if let Some(cost) = cost_str {
                format!("{prefix} {tok_str} ({cost})")
            } else {
                format!("{prefix} {tok_str}")
            }
        }
    }
}

fn format_remaining_openrouter(chat: &AiChatState, ctx_label: Option<&str>) -> String {
    let prefix = tr("ai.usage_remaining");
    if chat.is_openrouter_credits_loading() {
        return format!("{prefix} {}", tr("ai.usage_loading"));
    }
    if let Some(credits) = &chat.openrouter_credits {
        let usd = format_usd(credits.remaining_usd);
        return match chat.user_settings.usage_display_mode {
            UsageDisplayPreference::Tokens => {
                ctx_label.map_or_else(|| format!("{prefix} {usd}"), |c| format!("{prefix} {c}"))
            }
            UsageDisplayPreference::Cost => format!("{prefix} {usd}"),
            UsageDisplayPreference::Percent => credits.remaining_percent.map_or_else(
                || format!("{prefix} {usd}"),
                |p| format!("{prefix} {p:.0}%"),
            ),
            UsageDisplayPreference::Auto => credits.remaining_percent.map_or_else(
                || format!("{prefix} {usd}"),
                |p| format!("{prefix} {usd} ({p:.0}%)"),
            ),
        };
    }
    if chat.openrouter_credits_error.is_some() {
        let err_label = tr("ai.usage_credits_err");
        return ctx_label.map_or_else(
            || format!("{prefix} {err_label}"),
            |ctx| format!("{prefix} {ctx} / {err_label}"),
        );
    }
    if let Some(ctx) = ctx_label {
        return format!("{prefix} {ctx}");
    }
    if chat.has_cached_credentials() {
        format!("{prefix} {}", tr("ai.usage_credits_unfetched"))
    } else {
        format!("{prefix} {}", tr("ai.usage_key_missing"))
    }
}

fn format_remaining_footer(chat: &AiChatState, ctx: Option<(u64, u64, u64)>) -> String {
    let prefix = tr("ai.usage_remaining");
    let ctx_label = ctx.map(|(win, _, rem)| {
        let rem_pct = rem.saturating_mul(100) / win;
        format!("Ctx {} ({rem_pct}%)", format_compact_tokens(rem))
    });
    let budget_rem = chat
        .user_settings
        .budget_usd
        .filter(|b| b.is_finite() && *b > 0.0)
        .map(|budget| {
            let rem = (budget - chat.session_usage.cost_usd).max(0.0);
            let pct = ((rem / budget) * 100.0).clamp(0.0, 100.0);
            (format_usd(rem), format!("{pct:.0}%"))
        });

    match &chat.selected_provider {
        AiProviderKind::OpenRouter => format_remaining_openrouter(chat, ctx_label.as_deref()),
        AiProviderKind::OllamaCompat => {
            let detail = ctx_label.unwrap_or_else(|| tr("ai.usage_local_free"));
            format!("{prefix} {detail} / Local")
        }
        AiProviderKind::Plugin { .. } => {
            let cli_label = tr("ai.usage_cli_managed");
            ctx_label.map_or_else(
                || format!("{prefix} {cli_label}"),
                |ctx_str| format!("{prefix} {ctx_str} / {cli_label}"),
            )
        }
        AiProviderKind::Mock => ctx_label.map_or_else(
            || format!("{prefix} Mock"),
            |ctx_str| format!("{prefix} {ctx_str}"),
        ),
        AiProviderKind::OpenAi | AiProviderKind::Gemini | AiProviderKind::Anthropic => {
            match chat.user_settings.usage_display_mode {
                UsageDisplayPreference::Tokens => {
                    ctx_label.map_or_else(|| format!("{prefix} —"), |c| format!("{prefix} {c}"))
                }
                UsageDisplayPreference::Cost => {
                    if let Some((usd, _)) = budget_rem {
                        format!("{prefix} {usd}")
                    } else if let Some(c) = ctx_label {
                        format!("{prefix} {c}")
                    } else {
                        format!("{prefix} {}", tr("ai.usage_budget_unset"))
                    }
                }
                UsageDisplayPreference::Percent => {
                    if let Some((_, pct)) = budget_rem {
                        format!("{prefix} {pct}")
                    } else if let Some((win, _, rem)) = ctx {
                        format!("{prefix} {}%", rem.saturating_mul(100) / win)
                    } else {
                        format!("{prefix} —")
                    }
                }
                UsageDisplayPreference::Auto => match (budget_rem, ctx_label) {
                    (Some((usd, _)), Some(c)) => format!("{prefix} {usd} / {c}"),
                    (Some((usd, pct)), None) => format!("{prefix} {usd} ({pct})"),
                    (None, Some(c)) => format!("{prefix} {c}"),
                    (None, None) => format!("{prefix} {}", tr("ai.usage_budget_unset")),
                },
            }
        }
    }
}

/// 送信ボタン下フッタに表示する `(セッション使用量文字列, 残量文字列)` をプロバイダ別形式で生成する。
#[must_use]
pub fn format_usage_footer_parts(chat: &AiChatState) -> (String, String) {
    let ctx = context_usage_info(chat);
    (
        format_session_used_footer(chat, ctx),
        format_remaining_footer(chat, ctx),
    )
}

fn render_usage_footer(chat: &AiChatState, cx: &mut Context<SeaEditorWindow>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let (session_str, remaining_str) = format_usage_footer_parts(chat);

    let mut right = h_flex().items_center().gap_1().child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(remaining_str),
    );
    if chat.selected_provider == AiProviderKind::OpenRouter {
        right = right.child(
            Button::new("ai-usage-refresh-credits")
                .xsmall()
                .ghost()
                .label("↻")
                .disabled(chat.is_openrouter_credits_loading())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.ai_chat.refresh_openrouter_credits_async();
                    cx.notify();
                })),
        );
    }

    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_1()
        .flex_wrap()
        .pt_0p5()
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(session_str),
        )
        .child(right)
}

#[cfg(test)]
mod tests {
    use super::{
        AiApprovalMode, AiChatState, apply_at_completion, build_provider_select_items,
        find_trailing_at_query, format_at_mention_token, format_usage_footer_parts,
        matching_at_mention_names, provider_safety_notice, provider_select_index,
    };
    use crate::ai::provider::{AiProviderKind, AiThinkingLevel, AiTokenUsage};
    use crate::ai::providers::mock::MockAiProvider;
    use crate::ai::providers::openai_compat::OpenRouterCreditsSnapshot;
    use crate::ai::settings::UsageDisplayPreference;
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
        assert_eq!(chat.active_thinking_options(), Vec::new());
        assert_eq!(chat.active_model_label(), "Orchestrator (Argon)");

        chat.select_thinking_level(AiThinkingLevel::High);
        assert_eq!(chat.thinking_level, AiThinkingLevel::Auto);

        let models = chat.resolved_models_for_provider(&agy);
        let hydrogen = models
            .iter()
            .find(|m| m.id == "gemini-4-hydrogen")
            .ok_or("hydrogen preset missing")?;
        let formatted = hydrogen.format_menu_label(true, AiThinkingLevel::Extra);
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
        chat.push_turn(
            crate::ai::provider::AiRole::User,
            "テロップを追加して".to_owned(),
            None,
        );
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

        let models = chat.resolved_models_for_provider(&agy);
        let hydrogen = models
            .iter()
            .find(|m| m.id == "gemini-4-hydrogen")
            .ok_or("hydrogen preset missing")?;
        let unselected_label = hydrogen.format_menu_label(false, AiThinkingLevel::Extra);
        assert!(unselected_label.contains("Medium"));

        chat.select_model_default_medium("gemini-pro-orchestrator-g1-pro");
        assert_eq!(chat.thinking_level, AiThinkingLevel::Auto);
        Ok(())
    }

    #[test]
    fn usage_footer_formats_across_providers_and_preferences() {
        let mut chat = AiChatState::default();
        chat.select_provider(AiProviderKind::OpenRouter);
        chat.openrouter_credits = Some(OpenRouterCreditsSnapshot {
            total_credits_usd: 10.0,
            total_usage_usd: 5.18,
            remaining_usd: 4.82,
            remaining_percent: Some(48.2),
        });
        chat.record_turn_usage(&AiTokenUsage {
            input_tokens: 1_200,
            output_tokens: 300,
            total_tokens: 1_500,
            cost_usd: Some(0.015),
            is_estimated: false,
        });
        let (left, right) = format_usage_footer_parts(&chat);
        assert!(left.contains("1.5k tok") && left.contains("$0.015"));
        assert!(right.contains("$4.82") && right.contains("48%"));

        chat.user_settings.usage_display_mode = UsageDisplayPreference::Tokens;
        let (tok_left, tok_right) = format_usage_footer_parts(&chat);
        assert!(!tok_left.contains("$0.015"));
        assert!(tok_right.contains("Ctx"));

        chat.user_settings.usage_display_mode = UsageDisplayPreference::Percent;
        let (_pct_left, pct_right) = format_usage_footer_parts(&chat);
        assert!(pct_right.contains("48%"));

        chat.user_settings.usage_display_mode = UsageDisplayPreference::Auto;
        chat.select_provider(AiProviderKind::OllamaCompat);
        let (loc_left, loc_right) = format_usage_footer_parts(&chat);
        assert!(loc_left.contains("Local"));
        assert!(loc_right.contains("Local"));

        chat.user_settings.usage_display_mode = UsageDisplayPreference::Cost;
        let (loc_cost_left, _) = format_usage_footer_parts(&chat);
        assert!(loc_cost_left.contains("Local ($0.00)"));
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

        // 手動編集 dispatch_command 後に can_one_click_undo がリセットされること
        controller.submit_ai_prompt_with_provider(
            "もう1つ追加",
            &MockAiProvider::with_responses([r#"{"type":"add_layer","name":"Layer B"}"#]),
        )?;
        assert!(controller.ai_chat.can_one_click_undo);
        controller.dispatch_command(&crate::timeline::command::EditCommand::AddLayer {
            temp_id: None,
            id: None,
            name: "Manual Layer".to_owned(),
            insert_index: None,
        })?;
        assert!(!controller.ai_chat.can_one_click_undo);

        // API キー更新時に agent_provider_override がクリアされること
        controller.ai_chat.agent_provider_override =
            Some(std::sync::Arc::new(MockAiProvider::new()));
        controller.ai_chat.set_runtime_api_key(None);
        assert!(controller.ai_chat.agent_provider_override.is_none());
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
            estimated_input_chars: 0,
            cancelled: std::sync::Arc::clone(&cancelled),
        });
        controller.ai_chat.streaming_preview =
            "字幕の色を調整しています。\n```json\n{\"type\":\"batch\"".to_owned();
        assert!(controller.ai_chat.is_busy());

        assert!(controller.cancel_ai_prompt());
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert!(!controller.ai_chat.is_busy());
        assert_eq!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Cancelled
        );
        assert_eq!(controller.ai_chat.streaming_preview, "");
        assert_eq!(controller.ai_chat.status_banner, "AI 生成を停止しました");
        let last = controller.ai_chat.turns.last();
        assert!(last.is_some_and(|t| t.text.contains("[停止済み]")
            && t.text.contains("字幕の色を調整しています。")
            && !t.text.contains("\"type\":\"batch\"")));
        assert_eq!(
            last.and_then(|t| t.status_badge),
            Some(crate::ai::session_store::AiTurnBadge::Cancelled)
        );
        assert!(!controller.cancel_ai_prompt());
        drop(tx);

        // 先頭から制御 JSON のみの断片は履歴へ残さず破棄すること
        let (tx2, rx2) = std::sync::mpsc::channel();
        let turns_before = controller.ai_chat.turns.len();
        controller.ai_chat.running_job = Some(super::AiChatJobHandle {
            rx: rx2,
            prompt: "テスト指示2".to_owned(),
            estimated_input_chars: 0,
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        controller.ai_chat.streaming_preview = "{\"tool_calls\":[".to_owned();
        assert!(controller.cancel_ai_prompt());
        assert_eq!(controller.ai_chat.turns.len(), turns_before);
        drop(tx2);
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

    #[test]
    fn open_saved_session_resets_custom_base_url_and_clears_override()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::ai::session_store::{AiSessionRecord, AiSessionStore};

        let dir = std::env::temp_dir().join(format!(
            "sea_restore_prov_{}_{}",
            std::process::id(),
            AiSessionStore::generate_session_id()
        ));
        let store = AiSessionStore::new(&dir);
        let rec = AiSessionRecord {
            schema_version: 1,
            session_id: "sess_restore_gemini".to_owned(),
            title: "Gemini セッション".to_owned(),
            created_at_unix_secs: 1,
            updated_at_unix_secs: 2,
            provider: Some(AiProviderKind::Gemini),
            agent_plugin_id: None,
            model_name: "gemini-2.5-flash".to_owned(),
            thinking_level: AiThinkingLevel::Auto,
            approval_mode: AiApprovalMode::Manual,
            attached_folders: Vec::new(),
            turns: Vec::new(),
            pending_question: None,
            agent_memory: serde_json::Value::Null,
            session_usage: crate::ai::session_store::AiSessionUsage::default(),
        };
        store.save_session(&rec)?;

        let mut controller = AppController::new();
        controller.ai_chat.session_store = Some(store);
        controller
            .ai_chat
            .select_provider(AiProviderKind::OllamaCompat);
        controller.ai_chat.agent_provider_override =
            Some(std::sync::Arc::new(MockAiProvider::new()));
        assert!(controller.ai_chat.custom_base_url.contains("11434"));

        controller.open_saved_ai_session_with_provider("sess_restore_gemini", None, None)?;
        assert_eq!(controller.ai_chat.selected_provider, AiProviderKind::Gemini);
        assert_eq!(
            controller.ai_chat.custom_base_url,
            AiProviderKind::Gemini.default_base_url()
        );
        assert!(controller.ai_chat.agent_provider_override.is_none());

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn execution_state_transitions_and_can_cancel_guard_waiting_states() {
        use crate::ai::session_store::PendingAgentQuestionSnapshot;

        let mut controller = AppController::new();
        assert_eq!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Idle
        );
        assert!(!controller.ai_chat.can_cancel());

        // 質問カード待機中も can_cancel() == true で新規送信がブロックされる
        controller.ai_chat.pending_question = Some(PendingAgentQuestionSnapshot {
            request_id: "q1".to_owned(),
            title: "スタイル確認".to_owned(),
            message: "選んでください".to_owned(),
            fields: Vec::new(),
            choices: Vec::new(),
            allow_skip: true,
        });
        controller.ai_chat.execution_state = super::AiExecutionState::WaitingUserInput {
            detail: "スタイル確認".to_owned(),
        };
        assert!(controller.ai_chat.can_cancel());
        assert!(controller.start_ai_prompt_async("追加指示").is_err());

        controller.ai_chat.pending_question = None;
        controller.ai_chat.pending_tool_approval = Some(super::PendingAgentToolApproval {
            request_id: "a1".to_owned(),
            tool_name: "ext_tool".to_owned(),
            arguments: serde_json::json!({}),
            description: "desc".to_owned(),
        });
        assert!(controller.ai_chat.can_cancel());
        assert!(controller.start_ai_prompt_async("追加指示").is_err());

        // new_session で execution_state / show_all_history / next_turn_id が初期化されること
        controller.ai_chat.pending_tool_approval = None;
        controller.ai_chat.show_all_history = true;
        controller
            .ai_chat
            .push_turn(crate::ai::provider::AiRole::User, "hello".to_owned(), None);
        assert_eq!(controller.ai_chat.next_turn_id, 2);
        controller.ai_chat.new_session();
        assert_eq!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Idle
        );
        assert!(!controller.ai_chat.show_all_history);
        assert_eq!(controller.ai_chat.next_turn_id, 1);
    }

    #[test]
    fn streaming_preview_masks_leading_and_inline_control_json() {
        let leading = super::format_streaming_preview_for_ui("{\"tool_calls\":[{\"name\":\"x\"}]}");
        assert!(!leading.contains("tool_calls"));
        assert!(leading.contains("29"));

        let inline = super::format_streaming_preview_for_ui(
            "タイムラインを確認します。\n```json\n{\"tool_calls\":[{\"name\":\"get_timeline_summary\"}]}\n```",
        );
        assert!(inline.contains("タイムラインを確認します。"));
        assert!(!inline.contains("tool_calls"));

        let plain = super::format_streaming_preview_for_ui("字幕の縁取りを赤色に変更しました。");
        assert_eq!(plain, "字幕の縁取りを赤色に変更しました。");
    }

    #[test]
    fn completion_preserves_reply_note_and_merges_autopilot_into_single_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1) 通常チャット + Manual: reply_note 保持 & ProposalReady 遷移
        let mut controller = AppController::new();
        let provider = MockAiProvider::with_responses([r#"トラックを追加しました。```json
{"type":"add_layer","name":"Manual Layer"}
```"#]);
        controller.submit_ai_prompt_with_provider("トラック追加", &provider)?;
        assert!(matches!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::ProposalReady { .. }
        ));
        let last = controller.ai_chat.turns.last().ok_or("missing turn")?;
        assert!(last.text.contains("トラックを追加しました。"));
        assert!(last.text.contains("承認して適用"));
        assert_eq!(
            last.status_badge,
            Some(crate::ai::session_store::AiTurnBadge::ProposalReady)
        );

        controller.approve_pending_ai_proposal()?;
        assert!(matches!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Completed {
                changed_timeline: true,
                ..
            }
        ));

        // 2) 自律エージェント + Autopilot: summary_markdown と適用結果が 1 ターンに統合されること
        let mut auto_ctrl = AppController::new();
        auto_ctrl
            .ai_chat
            .set_approval_mode(AiApprovalMode::Autopilot);
        auto_ctrl.ai_chat.push_turn(
            crate::ai::provider::AiRole::User,
            "レイヤー追加して".to_owned(),
            None,
        );
        let raw_plan = r#"{"type":"add_layer","name":"Auto Layer"}"#;
        let plan = crate::ai::planner::dry_run_validate(&auto_ctrl.document, raw_plan)?;
        let turns_before = auto_ctrl.ai_chat.turns.len();
        auto_ctrl.handle_agent_proposal_event(
            "レイヤーを追加しました。".to_owned(),
            plan,
            serde_json::json!({}),
        );
        assert_eq!(auto_ctrl.ai_chat.turns.len(), turns_before + 1);
        let merged = auto_ctrl
            .ai_chat
            .turns
            .last()
            .ok_or("missing merged turn")?;
        assert!(merged.text.contains("レイヤーを追加しました。"));
        assert!(merged.text.contains("[Autopilot 適用済み]"));
        assert_eq!(
            merged.status_badge,
            Some(crate::ai::session_store::AiTurnBadge::Applied)
        );
        Ok(())
    }

    #[test]
    fn elapsed_duration_respects_ja_and_en_locales() {
        let prev = crate::ui::i18n::active_language_code();
        let _ = crate::ui::i18n::set_active_language("ja");
        assert_eq!(super::format_elapsed_duration(12), "12秒");
        assert_eq!(super::format_elapsed_duration(65), "1分5秒");

        let _ = crate::ui::i18n::set_active_language("en");
        assert_eq!(super::format_elapsed_duration(12), "12s");
        assert_eq!(super::format_elapsed_duration(65), "1m 5s");

        let _ = crate::ui::i18n::set_active_language(&prev);
    }

    #[test]
    fn escape_plain_text_for_markdown_escapes_symbols_and_keeps_empty_lines_clean() {
        let input = "#1の設定\r\n*斜体* `code` <tag> ~~削除~~ a=b | 1. 項目\r\n\r\n次の段落";
        let escaped = super::escape_plain_text_for_markdown(input);
        assert!(escaped.contains("\\#1の設定  \n"));
        assert!(escaped.contains(
            "\\*斜体\\* \\`code\\` \\<tag\\> \\~\\~削除\\~\\~ a\\=b \\| 1\\. 項目\n\n次の段落"
        ));
        assert!(!escaped.contains("  \n\n"));

        let turn =
            super::AiChatTurn::new(1, crate::ai::provider::AiRole::User, input.to_owned(), None);
        assert_eq!(turn.text, input);
        assert_eq!(turn.rendered_markdown.as_ref(), escaped);
    }

    #[test]
    fn history_keeps_more_than_16_turns_and_collapses_only_past_200() {
        let mut chat = AiChatState::default();
        for i in 0..25 {
            chat.push_turn(crate::ai::provider::AiRole::User, format!("msg {i}"), None);
        }
        assert_eq!(chat.turns.len(), 25);
        assert_eq!(
            super::collapsed_history_count(chat.turns.len(), chat.show_all_history),
            0
        );

        for i in 25..215 {
            chat.push_turn(
                crate::ai::provider::AiRole::Assistant,
                format!("reply {i}"),
                None,
            );
        }
        assert_eq!(chat.turns.len(), 215);
        assert_eq!(super::collapsed_history_count(chat.turns.len(), false), 15);
        assert_eq!(super::collapsed_history_count(chat.turns.len(), true), 0);
    }

    #[test]
    fn waiting_state_clears_on_agent_exit_fatal_error_and_orphan_cancel() {
        use crate::ai::agent_host::AgentHostEvent;
        use crate::ai::session_store::PendingAgentQuestionSnapshot;

        // 1) WaitingUserInput 中のプロセス終了 (Exited) で待機状態が解放され再送信できること
        let mut controller = AppController::new();
        controller.handle_agent_host_event(AgentHostEvent::AskUser {
            request_id: "req1".to_owned(),
            title: "スタイル確認".to_owned(),
            message: "選択してください".to_owned(),
            fields: Vec::new(),
            choices: Vec::new(),
            allow_skip: true,
        });
        assert!(controller.ai_chat.can_cancel());
        let (did_exit, _) = controller.handle_agent_host_event(AgentHostEvent::Exited {
            reason: "crashed".to_owned(),
        });
        assert!(did_exit);
        assert!(controller.ai_chat.pending_question.is_none());
        assert!(!controller.ai_chat.can_cancel());
        assert!(matches!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Failed { .. }
        ));

        // 2) WaitingApproval 中の非回復 AgentError で待機状態が解放されること
        controller.handle_agent_host_event(AgentHostEvent::ToolApprovalRequired {
            request_id: "req2".to_owned(),
            tool_name: "ext_tool".to_owned(),
            arguments: serde_json::json!({}),
            description: "desc".to_owned(),
        });
        assert!(controller.ai_chat.can_cancel());
        let (fatal_exit, _) = controller.handle_agent_host_event(AgentHostEvent::AgentError {
            code: "fatal".to_owned(),
            message: "terminated".to_owned(),
            recoverable: false,
        });
        assert!(fatal_exit);
        assert!(controller.ai_chat.pending_tool_approval.is_none());
        assert!(!controller.ai_chat.can_cancel());

        // 3) active_agent が None のまま待機カードが残留しても cancel_ai_prompt で強制解除できること
        controller.ai_chat.pending_tool_approval = Some(super::PendingAgentToolApproval {
            request_id: "req3".to_owned(),
            tool_name: "orphan".to_owned(),
            arguments: serde_json::json!({}),
            description: "desc".to_owned(),
        });
        controller.ai_chat.pending_question = Some(PendingAgentQuestionSnapshot {
            request_id: "req4".to_owned(),
            title: "orphan".to_owned(),
            message: "msg".to_owned(),
            fields: Vec::new(),
            choices: Vec::new(),
            allow_skip: false,
        });
        controller.ai_chat.streaming_preview = "途中の応答テキスト".to_owned();
        assert!(controller.ai_chat.can_cancel());
        assert!(controller.cancel_ai_prompt());
        assert!(!controller.ai_chat.can_cancel());
        assert_eq!(controller.ai_chat.streaming_preview, "");
        assert_eq!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Cancelled
        );
        let last_turn = controller.ai_chat.turns.last();
        assert!(last_turn.is_some_and(|turn| {
            turn.status_badge == Some(crate::ai::session_store::AiTurnBadge::Cancelled)
                && turn.text.contains("途中の応答テキスト")
        }));
    }

    #[test]
    fn status_without_step_preserves_progress_and_untagged_or_inline_json_is_masked() {
        use crate::ai::agent_host::AgentHostEvent;

        let mut controller = AppController::new();
        controller.handle_agent_host_event(AgentHostEvent::Status {
            message: "分析中".to_owned(),
            step: Some(2),
            total_steps: Some(4),
        });
        controller.handle_agent_host_event(AgentHostEvent::Status {
            message: "構成データを生成中... (200文字)".to_owned(),
            step: None,
            total_steps: None,
        });
        assert_eq!(
            controller.ai_chat.agent_step_status,
            Some((Some(2), Some(4)))
        );
        assert!(matches!(
            controller.ai_chat.execution_state,
            super::AiExecutionState::Responding {
                step: Some((Some(2), Some(4))),
                ..
            }
        ));

        let untagged = super::format_streaming_preview_for_ui(
            "前置きです。\n```\n{\"tool_calls\":[{\"name\":\"x\"}]}\n```",
        );
        assert!(untagged.contains("前置きです。"));
        assert!(!untagged.contains("tool_calls"));

        let inline_raw = "確認中 {\"tool_calls\":[{\"name\":\"get_timeline_summary\"}]}";
        let masked_inline = super::format_streaming_preview_for_ui(inline_raw);
        assert!(masked_inline.contains("確認中"));
        assert!(!masked_inline.contains("tool_calls"));
        assert_eq!(
            super::extract_readable_cancelled_preview(inline_raw).as_deref(),
            Some("確認中")
        );
    }

    #[test]
    fn activity_events_accumulate_persist_across_sessions_and_trigger_scroll_to_bottom()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::ai::agent_host::AgentHostEvent;
        use crate::ai::session_store::{AiActivityStepStatus, AiSessionStore};

        let store_dir = std::env::temp_dir().join(format!(
            "sea_ai_activity_roundtrip_{}_{}",
            std::process::id(),
            AiSessionStore::generate_session_id()
        ));
        let store = AiSessionStore::new(&store_dir);

        let mut controller = AppController::new();
        controller.ai_chat.session_store = Some(store);
        controller.ai_chat.live_activity.begin_turn();
        controller.handle_agent_host_event(AgentHostEvent::ThoughtDelta {
            text: "タイムライン構成を確認します".to_owned(),
        });
        controller.handle_agent_host_event(AgentHostEvent::ToolStarted {
            call_id: "c1".to_owned(),
            tool_name: "get_timeline_summary".to_owned(),
            verb: "Analyzed".to_owned(),
            target: "タイムライン構成".to_owned(),
        });
        controller.handle_agent_host_event(AgentHostEvent::ToolActivity {
            call_id: "c1".to_owned(),
            tool_name: "get_timeline_summary".to_owned(),
            verb: "Analyzed".to_owned(),
            target: "タイムライン構成".to_owned(),
            ok: true,
            summary: "ok".to_owned(),
            duration_ms: Some(18),
        });
        assert_eq!(controller.ai_chat.live_activity.steps.len(), 1);
        assert!(
            controller
                .ai_chat
                .live_activity
                .thought_tail_cache
                .contains("タイムライン構成を確認します")
        );

        controller.handle_agent_host_event(AgentHostEvent::AskUser {
            request_id: "ask_1".to_owned(),
            title: "対象選択".to_owned(),
            message: "番号を指定してください".to_owned(),
            fields: Vec::new(),
            choices: Vec::new(),
            allow_skip: true,
        });
        assert!(controller.ai_chat.take_scroll_to_bottom_pending());
        assert!(!controller.ai_chat.take_scroll_to_bottom_pending());

        // 進行中ステップをキャンセル確定してターン履歴へ保存・復元できること
        controller.handle_agent_host_event(AgentHostEvent::ToolStarted {
            call_id: "c2".to_owned(),
            tool_name: "apply_draft_edit_command".to_owned(),
            verb: "Editing".to_owned(),
            target: "タイムライン編集".to_owned(),
        });
        assert!(controller.cancel_ai_prompt());
        let cancelled_turn = controller
            .ai_chat
            .turns
            .last()
            .ok_or("expected cancelled turn")?;
        let activity = cancelled_turn
            .activity_log
            .as_ref()
            .ok_or("expected activity_log on cancelled turn")?;
        assert_eq!(activity.steps.len(), 2);
        assert_eq!(activity.steps[0].status, AiActivityStepStatus::Done);
        assert_eq!(activity.steps[0].duration_ms, Some(18));
        assert_eq!(activity.steps[1].status, AiActivityStepStatus::Cancelled);

        controller.ai_chat.persist_session();
        let sid = controller.ai_chat.session_id.clone();
        let mut controller2 = AppController::new();
        controller2.ai_chat.session_store = controller.ai_chat.session_store.clone();
        controller2.open_saved_ai_session_with_provider(&sid, None, None)?;
        let restored_last = controller2
            .ai_chat
            .turns
            .last()
            .ok_or("expected restored turn")?;
        assert_eq!(
            restored_last.activity_log.as_ref().map(|a| a.steps.len()),
            Some(2)
        );
        assert!(controller2.ai_chat.take_scroll_to_bottom_pending());

        let _ = std::fs::remove_dir_all(&store_dir);
        Ok(())
    }
}
