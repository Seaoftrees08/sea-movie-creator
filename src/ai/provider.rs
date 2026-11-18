//! AI チャットプロバイダの共通トレイト、設定種別、ストリーミングイベント、機密文字列型。

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// ログや `Debug` 出力で生文字列を漏洩させない API キー保持ラッパ。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretString(String);

impl SecretString {
    /// 新しいシークレット値を生成する（前後空白を除去）。
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into().trim().to_owned())
    }

    /// 秘匿された生の値を参照する（HTTP ヘッダ送出等の送信境界専用）。
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 空文字かどうかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("SecretString(<empty>)")
        } else {
            f.write_str("SecretString(***REDACTED***)")
        }
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***REDACTED***")
    }
}

/// AI プロバイダ通信・構成に関するエラー型。
#[derive(Debug, Error)]
pub enum AiError {
    /// API キーが環境変数にも OS 資格情報ストアにも見当たらない。
    #[error("API key missing for {provider}; set {env_hint} or store it in OS keyring")]
    MissingCredentials {
        /// 対象プロバイダ名。
        provider: String,
        /// 案内用環境変数名。
        env_hint: String,
    },
    /// HTTP ステータスエラー。
    #[error("AI HTTP request failed with status {status}: {body}")]
    HttpStatus {
        /// HTTP ステータスコード。
        status: u16,
        /// エラーレスポンス本文（要約）。
        body: String,
    },
    /// ネットワーク・TLS・タイムアウト等のトランスポートエラー。
    #[error("AI transport error: {0}")]
    Transport(String),
    /// レスポンス JSON / SSE の構造解析失敗。
    #[error("AI response parse failed: {0}")]
    ParseFailed(String),
    /// AI プラグイン／外部 CLI アダプタのエラー。
    #[error("AI plugin failed: {0}")]
    PluginError(String),
    /// 入出力エラー。
    #[error("AI I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// AI モデルの思考・推論強度レベル (`Reasoning Effort` / `Thinking Level`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiThinkingLevel {
    /// プロバイダ既定（明示的な思考パラメータを上書きしない）。
    #[default]
    Auto,
    /// 軽量・高速推論 (`low`)。
    Low,
    /// バランス重視 (`medium`)。
    Medium,
    /// 深い推論 (`high`)。
    High,
    /// 最大推論（Antigravity `xhigh` / `Extra` 等）。
    Extra,
}

impl AiThinkingLevel {
    /// 全レベル候補一覧を返す。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Auto, Self::Low, Self::Medium, Self::High, Self::Extra]
    }

    /// UI 表示用ラベルを返す。
    #[must_use]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Extra => "Extra",
        }
    }

    /// モデルバッジ横などに添える短縮表記（`Auto` のときは `None`）。
    #[must_use]
    pub const fn badge_label(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Low => Some("Low"),
            Self::Medium => Some("Medium"),
            Self::High => Some("High"),
            Self::Extra => Some("Extra"),
        }
    }

    /// `agy acp` の `thought_level` 設定値を返す（`Auto` 時は `None`）。
    #[must_use]
    pub const fn acp_thought_level(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Extra => Some("xhigh"),
        }
    }

    /// `OpenAI` / `OpenRouter` 系 `reasoning_effort` 文字列を返す。
    #[must_use]
    pub const fn reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Extra => Some("xhigh"),
        }
    }

    /// Google Gemini 2.5 向け `thinkingBudget` トークン数を返す。
    #[must_use]
    pub const fn gemini_thinking_budget(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Low => Some(1024),
            Self::Medium => Some(8192),
            Self::High => Some(24576),
            Self::Extra => Some(32768),
        }
    }

    /// Google Gemini 3 / 4 系 REST API 向け `thinkingLevel` 文字列を返す。
    #[must_use]
    pub const fn gemini_thinking_level(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Low => Some("LOW"),
            Self::Medium => Some("MEDIUM"),
            Self::High | Self::Extra => Some("HIGH"),
        }
    }

    /// Anthropic Messages API 向け `thinking.budget_tokens` を返す。
    #[must_use]
    pub const fn anthropic_budget_tokens(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Low => Some(1024),
            Self::Medium => Some(4096),
            Self::High => Some(10240),
            Self::Extra => Some(16384),
        }
    }
}

/// UI のモデル選択メニューに表示するモデル定義。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiModelPreset {
    /// API / プラグインへ渡すモデル識別子（または ACP プロファイル名）。
    pub id: &'static str,
    /// 人間向けの短い表示名。
    pub display_name: &'static str,
    /// UI 補助タグ（`"New"` 等）。
    pub badge: Option<&'static str>,
    /// 切り替え可能な思考レベル一覧（空スライスの場合はモデル固定）。
    pub thinking_levels: &'static [AiThinkingLevel],
    /// このモデル選択時の既定思考レベル。
    pub default_thinking: AiThinkingLevel,
}

const THINKING_AUTO_TO_HIGH: &[AiThinkingLevel] = &[
    AiThinkingLevel::Auto,
    AiThinkingLevel::Low,
    AiThinkingLevel::Medium,
    AiThinkingLevel::High,
];

const THINKING_LOW_TO_HIGH: &[AiThinkingLevel] = &[
    AiThinkingLevel::Low,
    AiThinkingLevel::Medium,
    AiThinkingLevel::High,
];

const THINKING_LOW_TO_EXTRA: &[AiThinkingLevel] = &[
    AiThinkingLevel::Low,
    AiThinkingLevel::Medium,
    AiThinkingLevel::High,
    AiThinkingLevel::Extra,
];

const OPENAI_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "gpt-4.1-mini",
        display_name: "GPT-4.1 Mini",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gpt-4.1",
        display_name: "GPT-4.1",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gpt-5",
        display_name: "GPT-5",
        badge: Some("New"),
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gpt-5-mini",
        display_name: "GPT-5 Mini",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "o4-mini",
        display_name: "o4-mini",
        badge: None,
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "o3",
        display_name: "o3",
        badge: None,
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
];

const OPENROUTER_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "openrouter/auto",
        display_name: "OpenRouter Auto",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "google/gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "anthropic/claude-sonnet-4",
        display_name: "Claude Sonnet 4",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "openai/gpt-5",
        display_name: "GPT-5",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "openai/gpt-4.1-mini",
        display_name: "GPT-4.1 Mini",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
];

const OLLAMA_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "llama3.2",
        display_name: "Llama 3.2",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "qwen3:8b",
        display_name: "Qwen 3 (8B)",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "deepseek-r1:8b",
        display_name: "DeepSeek-R1 (8B)",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gemma3:12b",
        display_name: "Gemma 3 (12B)",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
];

const GEMINI_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "gemini-2.5-flash",
        display_name: "Gemini 2.5 Flash",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gemini-2.5-flash-lite",
        display_name: "Gemini 2.5 Flash-Lite",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
];

const ANTHROPIC_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "claude-sonnet-4-20250514",
        display_name: "Claude Sonnet 4",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "claude-opus-4-1-20250805",
        display_name: "Claude Opus 4.1",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "claude-3-7-sonnet-20250219",
        display_name: "Claude 3.7 Sonnet",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
];

const ANTIGRAVITY_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "gemini-4-argon",
        display_name: "Gemini 4 Argon",
        badge: Some("New"),
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "gemini-4-hydrogen",
        display_name: "Gemini 4 Hydrogen",
        badge: Some("New"),
        thinking_levels: THINKING_LOW_TO_EXTRA,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "gemini-pro-orchestrator-g1-pro",
        display_name: "Orchestrator (Argon)",
        badge: Some("New"),
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "gemini-3.8-flash",
        display_name: "Gemini 3.8 Flash",
        badge: None,
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        badge: Some("New"),
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        badge: Some("New"),
        thinking_levels: THINKING_LOW_TO_HIGH,
        default_thinking: AiThinkingLevel::Medium,
    },
    AiModelPreset {
        id: "gpt-oss-120b-medium",
        display_name: "GPT-OSS 120B (Medium)",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "plugin-default",
        display_name: "Auto (agy Default)",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
];

const ACP_CLI_MODELS: &[AiModelPreset] = &[
    AiModelPreset {
        id: "claude",
        display_name: "Claude Code (ACP)",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "codex",
        display_name: "OpenAI Codex CLI (ACP)",
        badge: None,
        thinking_levels: THINKING_AUTO_TO_HIGH,
        default_thinking: AiThinkingLevel::Auto,
    },
    AiModelPreset {
        id: "copilot",
        display_name: "GitHub Copilot CLI (ACP)",
        badge: None,
        thinking_levels: &[],
        default_thinking: AiThinkingLevel::Auto,
    },
];

const MOCK_MODELS: &[AiModelPreset] = &[AiModelPreset {
    id: "mock-editor-v1",
    display_name: "Mock Editor v1",
    badge: None,
    thinking_levels: &[],
    default_thinking: AiThinkingLevel::Auto,
}];

/// 利用可能な組み込み HTTP・プラグイン AI プロバイダ種別。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    /// ローカル決定論テスト・オフライン確認用モックプロバイダ。
    #[default]
    Mock,
    /// `OpenAI` API (`https://api.openai.com/v1`)。
    OpenAi,
    /// `OpenRouter` API (`https://openrouter.ai/api/v1`)。
    OpenRouter,
    /// Ollama / LM Studio / vLLM 等の `OpenAI` 互換ローカル・カスタムエンドポイント。
    OllamaCompat,
    /// Google Gemini REST API (`generativelanguage.googleapis.com`)。
    Gemini,
    /// Anthropic Messages API (`api.anthropic.com`)。
    Anthropic,
    /// Phase C-2: `plugins/ai/` 配下の外部プロセスプラグイン。
    Plugin {
        /// AI プラグイン ID（例: `"sea.ai.antigravity"`, `"sea.ai.acp.claude"`）。
        plugin_id: String,
    },
}

impl AiProviderKind {
    /// UI 表示用ラベル。
    #[must_use]
    pub fn display_label(&self) -> String {
        match self {
            Self::Mock => "Mock (Offline Test)".to_owned(),
            Self::OpenAi => "OpenAI".to_owned(),
            Self::OpenRouter => "OpenRouter".to_owned(),
            Self::OllamaCompat => "Ollama / LM Studio / vLLM".to_owned(),
            Self::Gemini => "Google Gemini".to_owned(),
            Self::Anthropic => "Anthropic Claude".to_owned(),
            Self::Plugin { plugin_id } => format!("Plugin: {plugin_id}"),
        }
    }

    /// 推奨環境変数名を返す（不要な場合は `None`）。
    #[must_use]
    pub fn default_env_var(&self) -> Option<&'static str> {
        match self {
            Self::OpenAi | Self::OllamaCompat => Some("OPENAI_API_KEY"),
            Self::OpenRouter => Some("OPENROUTER_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::Mock | Self::Plugin { .. } => None,
        }
    }

    /// API キーが必須のプロバイダかどうか。
    #[must_use]
    pub const fn requires_api_key(&self) -> bool {
        matches!(
            self,
            Self::OpenAi | Self::OpenRouter | Self::Gemini | Self::Anthropic
        )
    }

    /// 既定ベース URL を返す。
    #[must_use]
    pub const fn default_base_url(&self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::OllamaCompat => "http://127.0.0.1:11434/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::Mock | Self::Plugin { .. } => "",
        }
    }

    /// 既定モデル ID を返す。
    #[must_use]
    pub const fn default_model(&self) -> &'static str {
        match self {
            Self::Mock => "mock-editor-v1",
            Self::OpenAi => "gpt-4.1-mini",
            Self::OpenRouter => "openrouter/auto",
            Self::OllamaCompat => "llama3.2",
            Self::Gemini => "gemini-2.5-flash",
            Self::Anthropic => "claude-sonnet-4-20250514",
            Self::Plugin { .. } => "plugin-default",
        }
    }

    /// OS 資格情報ストア（keyring）で使用するアカウント識別子。
    #[must_use]
    pub fn keyring_account(&self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("openai_api_key"),
            Self::OpenRouter => Some("openrouter_api_key"),
            Self::OllamaCompat => Some("ollama_compat_api_key"),
            Self::Gemini => Some("gemini_api_key"),
            Self::Anthropic => Some("anthropic_api_key"),
            Self::Mock | Self::Plugin { .. } => None,
        }
    }

    /// UI の API 切り替えコンボボックスに表示するプロバイダ一覧を返す。
    #[must_use]
    pub fn ui_selectable_providers() -> Vec<Self> {
        vec![
            Self::OpenAi,
            Self::OpenRouter,
            Self::OllamaCompat,
            Self::Gemini,
            Self::Anthropic,
            Self::Plugin {
                plugin_id: "sea.ai.antigravity".to_owned(),
            },
            Self::Plugin {
                plugin_id: "sea.ai.acp".to_owned(),
            },
        ]
    }

    /// UI のコンボボックス表示向けラベルを返す。
    #[must_use]
    pub fn ui_combo_label(&self) -> String {
        match self {
            Self::OpenAi => "OpenAI API".to_owned(),
            Self::OpenRouter => "OpenRouter API".to_owned(),
            Self::OllamaCompat => "Local (Ollama / LM Studio)".to_owned(),
            Self::Gemini => "Google Gemini API".to_owned(),
            Self::Anthropic => "Anthropic Claude API".to_owned(),
            Self::Plugin { plugin_id } => match plugin_id.as_str() {
                "sea.ai.antigravity" => "Antigravity (agy CLI)".to_owned(),
                "sea.ai.acp" => "ACP CLI (Claude / Codex / Copilot)".to_owned(),
                _ => format!("Plugin: {plugin_id}"),
            },
            Self::Mock => "Mock (Offline Test)".to_owned(),
        }
    }

    /// このプロバイダで利用可能なモデル定義カタログを返す。
    #[must_use]
    pub fn model_catalog(&self) -> &'static [AiModelPreset] {
        match self {
            Self::Mock => MOCK_MODELS,
            Self::OpenAi => OPENAI_MODELS,
            Self::OpenRouter => OPENROUTER_MODELS,
            Self::OllamaCompat => OLLAMA_MODELS,
            Self::Gemini => GEMINI_MODELS,
            Self::Anthropic => ANTHROPIC_MODELS,
            Self::Plugin { plugin_id } => match plugin_id.as_str() {
                "sea.ai.antigravity" => ANTIGRAVITY_MODELS,
                "sea.ai.acp" => ACP_CLI_MODELS,
                _ => MOCK_MODELS,
            },
        }
    }

    /// モデル ID に一致するカタログ定義を検索する。
    #[must_use]
    pub fn model_preset(&self, model_id: &str) -> Option<&'static AiModelPreset> {
        self.model_catalog().iter().find(|m| m.id == model_id)
    }

    /// UI でこのプロバイダを選んだときの推奨初期モデル ID を返す。
    #[must_use]
    pub fn preferred_default_model(&self) -> &'static str {
        self.model_catalog()
            .first()
            .map_or_else(|| self.default_model(), |m| m.id)
    }

    /// UI でこのプロバイダを選んだときの推奨初期思考レベルを返す。
    #[must_use]
    pub fn default_thinking_level(&self) -> AiThinkingLevel {
        self.model_catalog()
            .first()
            .map_or(AiThinkingLevel::Auto, |m| m.default_thinking)
    }
}

/// 会話メッセージの発言者ロール。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiRole {
    /// システム指示。
    System,
    /// ユーザー入力。
    User,
    /// AI アシスタント応答。
    Assistant,
}

/// チャットスレッド上の 1 ターン分のメッセージ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiChatMessage {
    /// 発言ロール。
    pub role: AiRole,
    /// 本文テキスト。
    pub content: String,
}

/// AI プロバイダへ渡す 1 回分の編集推論リクエスト。
#[derive(Debug, Clone)]
pub struct AiRequest {
    /// `EditCommand` ルール・座標系・出力形式を指定するシステムプロンプト。
    pub system_prompt: String,
    /// `schemars` 自動生成の `EditCommand` JSON Schema。
    pub command_schema_json: serde_json::Value,
    /// 現在タイムラインのコンパクト要約 JSON。
    pub context_summary: serde_json::Value,
    /// 直近の会話履歴。
    pub history: Vec<AiChatMessage>,
    /// 今回のユーザー指示文。
    pub user_prompt: String,
    /// 可能であればストリーミング受信を試みる。
    pub stream: bool,
}

/// AI プロバイダからの完了応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiResponse {
    /// モデルが出力した生テキスト（通常は `EditCommand` JSON を含む）。
    pub raw_text: String,
    /// 実際に応答を返したモデル名／プロバイダ名。
    pub model: String,
}

/// ストリーミング受信・非同期処理中に通知されるイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiStreamEvent {
    /// 接続中・待機中などの状態変化通知。
    Status(String),
    /// 逐次生成されたテキスト差分チャンク。
    DeltaText(String),
    /// 全文受信完了。
    Completed(AiResponse),
    /// 失敗通知。
    Failed(String),
}

/// AI 編集プランの承認・適用モード。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiApprovalMode {
    /// 適用前に差分カードを確認し、「承認して適用」押下で反映する（既定）。
    #[default]
    Manual,
    /// ドライラン検証を通過した編集プランを自動でタイムラインへ適用する（1 クリック Undo 可能）。
    Autopilot,
}

impl AiApprovalMode {
    /// UI セレクタに表示する短いモード名を返す。
    #[must_use]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Autopilot => "Autopilot",
        }
    }

    /// ドロップダウンメニュー内で表示する説明付きラベルを返す。
    #[must_use]
    pub const fn menu_label(self) -> &'static str {
        match self {
            Self::Manual => "Manual（承認して適用）",
            Self::Autopilot => "Autopilot（検証後に自動適用）",
        }
    }
}

/// エージェント実行時の安全制御・レート制限・Vision バッチ設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntimeSettings {
    /// 1 ターンあたりの自律ステップ（ツール／LLM 呼出）上限。`0` は無制限。
    #[serde(default)]
    pub max_steps_per_turn: u32,
    /// 外部 LLM 呼び出し間の最小待機時間（ミリ秒）。高頻度連続呼び出しによるアカウント制限対策用。
    #[serde(default)]
    pub min_llm_interval_ms: u64,
    /// Vision バッチ解析で 1 リクエストに含める画像枚数。
    #[serde(default = "default_vision_batch_size")]
    pub vision_batch_size: usize,
    /// Vision 送信用サムネイルの長辺上限ピクセル数。
    #[serde(default = "default_vision_max_edge_px")]
    pub vision_max_edge_px: u32,
}

const fn default_vision_batch_size() -> usize {
    6
}

const fn default_vision_max_edge_px() -> u32 {
    512
}

impl Default for AgentRuntimeSettings {
    fn default() -> Self {
        Self {
            max_steps_per_turn: 0,
            min_llm_interval_ms: 0,
            vision_batch_size: default_vision_batch_size(),
            vision_max_edge_px: default_vision_max_edge_px(),
        }
    }
}

/// マルチモーダル会話メッセージ内の 1 コンテンツパート。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AiContentPart {
    /// テキストパート。
    Text {
        /// 本文テキスト。
        text: String,
    },
    /// Base64 エンコードされた画像データ（長辺縮小済みサムネイル等）。
    ImageData {
        /// MIME 種別（例: `"image/jpeg"`, `"image/png"`）。
        mime_type: String,
        /// Base64 文字列。
        base64_data: String,
    },
}

impl AiContentPart {
    /// テキストパートを構築する。
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// バイト列を Base64 化して画像パートを構築する。
    #[must_use]
    pub fn image_bytes(mime_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self::ImageData {
            mime_type: mime_type.into(),
            base64_data: encode_base64(bytes),
        }
    }
}

/// LLM へ通知するツール定義（Function Calling / JSON フォールバック共通）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiToolDefinition {
    /// 一意なツール識別名。
    pub name: String,
    /// ツール機能・使い方の説明文。
    pub description: String,
    /// 引数の JSON Schema オブジェクト。
    pub parameters_schema: serde_json::Value,
}

/// LLM 応答から抽出された 1 件のツール呼び出し要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiToolCall {
    /// プロバイダまたはホストが割り当てる呼び出し ID。
    pub id: String,
    /// 呼び出すツール名。
    pub name: String,
    /// ツール引数 JSON。
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// マルチターン・ツール実行を含むエージェント推論用メッセージ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AiTurnMessage {
    /// システム追記事項。
    System {
        /// 本文。
        text: String,
    },
    /// ユーザー入力（テキストおよび任意画像）。
    User {
        /// コンテンツパート列。
        parts: Vec<AiContentPart>,
    },
    /// アシスタントのテキスト応答・ツール呼び出し宣言。
    Assistant {
        /// 応答テキスト（ツール呼び出しのみ時は `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// 要求されたツール呼び出し一覧。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<AiToolCall>,
    },
    /// ツール実行結果のフィードバック。
    ToolResult {
        /// 対応する [`AiToolCall::id`]。
        tool_call_id: String,
        /// 実行したツール名。
        name: String,
        /// JSON 文字列化された実行結果。
        content_json: String,
        /// 実行エラーだった場合 `true`。
        #[serde(default)]
        is_error: bool,
    },
}

impl AiTurnMessage {
    /// テキストのみのユーザーメッセージを生成する。
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            parts: vec![AiContentPart::text(text)],
        }
    }
}

/// マルチモーダル・ツール呼び出し対応の AI エージェントターンリクエスト。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiAgentTurnRequest {
    /// システム指示文。
    pub system_prompt: String,
    /// これまでの会話・ツール実行履歴。
    pub messages: Vec<AiTurnMessage>,
    /// 今回のターンで利用可能なツール一覧。
    #[serde(default)]
    pub tools: Vec<AiToolDefinition>,
    /// `tools` 未使用時に期待する構造化 JSON Schema（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_json_schema: Option<serde_json::Value>,
    /// 温度パラメータ（省略時は既定値 `0.2` 相当）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// マルチモーダル・ツール呼び出し対応の AI エージェントターン応答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiAgentTurnResponse {
    /// テキスト応答（存在する場合）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// モデルが要求したツール呼び出し一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AiToolCall>,
    /// 実際に応答したモデル名。
    pub model: String,
    /// プロバイダが返した終了理由（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// 組み込み HTTP・Mock・外部プラグインを統一的に扱う AI プロバイダトレイト。
pub trait AiProvider: Send + Sync {
    /// 識別子文字列。
    fn id(&self) -> &str;

    /// UI 表示名称。
    fn display_name(&self) -> &str;

    /// 画像（マルチモーダル）入力に対応しているか。
    fn supports_vision(&self) -> bool {
        false
    }

    /// Native Tool Calling / Function Calling に対応しているか。
    fn supports_native_tools(&self) -> bool {
        false
    }

    /// プロンプトを送信し、逐次イベントをコールバックへ流しながら完了応答を返す。
    ///
    /// # Errors
    ///
    /// 認証情報欠如・通信失敗・レスポンス解析不正時に [`AiError`] を返す。
    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError>;

    /// マルチモーダル・ツール呼び出し対応のエージェントターンを実行する。
    ///
    /// 既定実装は Native Tool Calling 非対応プロバイダ向けに `tools` 定義をテキストプロンプトへ
    /// 埋め込み、[`Self::send_streaming`] 経由で構造化 JSON 応答を解析するフォールバックを提供します。
    ///
    /// # Errors
    ///
    /// 認証情報欠如・通信失敗・応答解析エラー時に [`AiError`] を返す。
    fn send_agent_turn(
        &self,
        request: &AiAgentTurnRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiAgentTurnResponse, AiError> {
        fallback_agent_turn_via_streaming(self, request, on_event)
    }
}

/// バイト列を標準 Base64（RFC 4648 パディング付き）文字列へエンコードする。
#[must_use]
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[((triple >> 18) & 0x3F) as usize]));
        out.push(char::from(TABLE[((triple >> 12) & 0x3F) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(TABLE[((triple >> 6) & 0x3F) as usize]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(TABLE[(triple & 0x3F) as usize]));
        } else {
            out.push('=');
        }
    }
    out
}

/// テキストベースの統一 JSON アクションモード向けツール指示文を生成する。
#[must_use]
pub(crate) fn format_tools_fallback_instructions(tools: &[AiToolDefinition]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let tools_json = serde_json::to_string_pretty(tools).unwrap_or_else(|_| "[]".to_owned());
    format!(
        "\n\n# Available Tools (JSON Action Mode)\n\
         If you need to invoke tools before answering, reply ONLY with a JSON object:\n\
         {{\"tool_calls\":[{{\"name\":\"<tool_name>\",\"arguments\":{{...}}}}],\"message\":\"optional status\"}}\n\
         If no tool calls are needed, reply with your final text or JSON directly.\n\n{tools_json}"
    )
}

fn fallback_agent_turn_via_streaming<P: AiProvider + ?Sized>(
    provider: &P,
    request: &AiAgentTurnRequest,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<AiAgentTurnResponse, AiError> {
    let mut sys = request.system_prompt.clone();
    sys.push_str(&format_tools_fallback_instructions(&request.tools));

    let mut history = Vec::new();
    let mut last_user_prompt = String::new();
    for (idx, msg) in request.messages.iter().enumerate() {
        let is_last = idx + 1 == request.messages.len();
        let (role, content) = render_turn_message_as_text(msg);
        if is_last && role == AiRole::User {
            last_user_prompt = content;
        } else {
            history.push(AiChatMessage { role, content });
        }
    }
    if last_user_prompt.is_empty() {
        "Continue based on the tool results above.".clone_into(&mut last_user_prompt);
    }

    let legacy_req = AiRequest {
        system_prompt: sys,
        command_schema_json: request
            .response_json_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({})),
        context_summary: serde_json::json!({}),
        history,
        user_prompt: last_user_prompt,
        stream: false,
    };
    let resp = provider.send_streaming(&legacy_req, on_event)?;
    Ok(parse_fallback_agent_response(&resp.raw_text, &resp.model))
}

fn render_turn_message_as_text(msg: &AiTurnMessage) -> (AiRole, String) {
    match msg {
        AiTurnMessage::System { text } => (AiRole::System, text.clone()),
        AiTurnMessage::User { parts } => {
            let mut texts = Vec::new();
            let mut img_count = 0_usize;
            for part in parts {
                match part {
                    AiContentPart::Text { text } => texts.push(text.clone()),
                    AiContentPart::ImageData { .. } => img_count += 1,
                }
            }
            if img_count > 0 {
                texts.push(format!(
                    "[{img_count} image(s) attached; provider text-fallback]"
                ));
            }
            (AiRole::User, texts.join("\n"))
        }
        AiTurnMessage::Assistant { text, tool_calls } => {
            if tool_calls.is_empty() {
                (AiRole::Assistant, text.clone().unwrap_or_default())
            } else {
                let payload = serde_json::json!({
                    "message": text,
                    "tool_calls": tool_calls,
                });
                (AiRole::Assistant, payload.to_string())
            }
        }
        AiTurnMessage::ToolResult {
            tool_call_id,
            name,
            content_json,
            is_error,
        } => {
            let status = if *is_error { "ERROR" } else { "OK" };
            (
                AiRole::User,
                format!("[ToolResult {name} id={tool_call_id} status={status}]\n{content_json}"),
            )
        }
    }
}

pub(crate) fn parse_fallback_agent_response(raw_text: &str, model: &str) -> AiAgentTurnResponse {
    if let Some(val) = extract_json_object_for_tools(raw_text)
        && let Some(calls_arr) = val.get("tool_calls").and_then(serde_json::Value::as_array)
    {
        let mut parsed_calls = Vec::new();
        for (idx, item) in calls_arr.iter().enumerate() {
            if let Some(name) = item.get("name").and_then(serde_json::Value::as_str)
                && !name.trim().is_empty()
            {
                let id = item
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map_or_else(|| format!("fallback_call_{idx}"), ToOwned::to_owned);
                let arguments = item
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                parsed_calls.push(AiToolCall {
                    id,
                    name: name.to_owned(),
                    arguments,
                });
            }
        }
        if !parsed_calls.is_empty() {
            let note = val
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            return AiAgentTurnResponse {
                text: note,
                tool_calls: parsed_calls,
                model: model.to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
            };
        }
    }

    AiAgentTurnResponse {
        text: (!raw_text.trim().is_empty()).then(|| raw_text.to_owned()),
        tool_calls: Vec::new(),
        model: model.to_owned(),
        finish_reason: Some("stop".to_owned()),
    }
}

fn extract_json_object_for_tools(raw: &str) -> Option<serde_json::Value> {
    let trimmed = raw.trim();
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && val.is_object()
    {
        return Some(val);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&trimmed[start..=end])
        .ok()
        .filter(serde_json::Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_masks_debug_and_display() {
        let secret = SecretString::new("sk-live-very-secret-12345");
        let dbg = format!("{secret:?}");
        let disp = format!("{secret}");
        assert!(!dbg.contains("sk-live"));
        assert!(!dbg.contains("12345"));
        assert!(dbg.contains("REDACTED"));
        assert!(!disp.contains("12345"));
        assert_eq!(secret.expose(), "sk-live-very-secret-12345");
    }

    #[test]
    fn provider_catalog_and_thinking_level_mappings_are_consistent()
    -> Result<(), Box<dyn std::error::Error>> {
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        assert_eq!(agy.preferred_default_model(), "gemini-4-argon");
        assert_eq!(agy.default_thinking_level(), AiThinkingLevel::Medium);
        let hydrogen = agy
            .model_preset("gemini-4-hydrogen")
            .ok_or("hydrogen preset missing")?;
        assert!(hydrogen.thinking_levels.contains(&AiThinkingLevel::Extra));
        assert_eq!(AiThinkingLevel::Extra.acp_thought_level(), Some("xhigh"));
        assert_eq!(AiThinkingLevel::Auto.acp_thought_level(), None);
        assert_eq!(AiProviderKind::ui_selectable_providers().len(), 7);
        Ok(())
    }

    #[test]
    fn base64_encoding_matches_rfc4648_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn fallback_agent_parser_extracts_tool_calls_from_fenced_json() {
        let raw = "調べてみます。\n```json\n{\"tool_calls\":[{\"name\":\"inspect_media_folder\",\"arguments\":{\"path\":\"C:/photos\"}}],\"message\":\"スキャン開始\"}\n```";
        let parsed = parse_fallback_agent_response(raw, "ollama:qwen3");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "inspect_media_folder");
        assert_eq!(parsed.tool_calls[0].arguments["path"], "C:/photos");
        assert_eq!(parsed.text.as_deref(), Some("スキャン開始"));
    }
}
