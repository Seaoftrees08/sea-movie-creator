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
}
