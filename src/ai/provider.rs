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

/// 組み込み HTTP・Mock・外部プラグインを統一的に扱う AI プロバイダトレイト。
pub trait AiProvider: Send + Sync {
    /// 識別子文字列。
    fn id(&self) -> &str;

    /// UI 表示名称。
    fn display_name(&self) -> &str;

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
}
