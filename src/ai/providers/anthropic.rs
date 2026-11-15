//! Anthropic Messages API クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiError, AiProvider, AiProviderKind, AiRequest, AiResponse, AiRole, AiStreamEvent,
    AiThinkingLevel, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Anthropic Messages API 設定。
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// API ベース URL。
    pub base_url: String,
    /// モデル ID（例: `claude-sonnet-4-20250514`）。
    pub model: String,
    /// 思考強度レベル（`Auto` 時は拡張思考を付与しない）。
    pub thinking_level: AiThinkingLevel,
    /// 明示指定 API キー（`None` 時は `ANTHROPIC_API_KEY` / OS keyring を参照）。
    pub api_key: Option<SecretString>,
    /// 最大生成トークン数。
    pub max_tokens: u32,
    /// SSE ストリーミングを使うか。
    pub use_streaming: bool,
    /// タイムアウト（秒）。
    pub timeout_secs: u64,
}

impl AnthropicConfig {
    /// 既定の Anthropic API 設定を作成する。
    #[must_use]
    pub fn new(model: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            base_url: AiProviderKind::Anthropic.default_base_url().to_owned(),
            model: model.into(),
            thinking_level: AiThinkingLevel::Auto,
            api_key,
            max_tokens: 4096,
            use_streaming: true,
            timeout_secs: 60,
        }
    }

    /// 思考強度レベルを指定する。
    #[must_use]
    pub const fn with_thinking_level(mut self, thinking_level: AiThinkingLevel) -> Self {
        self.thinking_level = thinking_level;
        self
    }
}

/// Anthropic Messages API プロバイダ実装。
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    config: AnthropicConfig,
    label: String,
}

impl AnthropicProvider {
    /// 設定から Anthropic プロバイダを作成する。
    #[must_use]
    pub fn new(config: AnthropicConfig) -> Self {
        let label = format!("Anthropic Claude ({})", config.model);
        Self { config, label }
    }

    fn build_request_body(&self, request: &AiRequest, stream: bool) -> Value {
        let system_text = format!(
            "{}\n\n# EditCommand JSON Schema\n{}\n\n# Current Timeline Context\n{}",
            request.system_prompt, request.command_schema_json, request.context_summary
        );
        let mut messages = Vec::new();
        for turn in &request.history {
            if turn.role == AiRole::System {
                continue;
            }
            let role = match turn.role {
                AiRole::Assistant => "assistant",
                AiRole::User | AiRole::System => "user",
            };
            messages.push(json!({ "role": role, "content": turn.content }));
        }
        messages.push(json!({ "role": "user", "content": request.user_prompt }));

        let budget = self.config.thinking_level.anthropic_budget_tokens();
        let max_tokens = match budget {
            Some(b) => self.config.max_tokens.max(b.saturating_add(4096)),
            None => self.config.max_tokens,
        };
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "system": system_text,
            "messages": messages,
            "stream": stream,
        });
        if let Some(budget_tokens) = budget {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget_tokens,
            });
        }
        body
    }
}

impl AiProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn display_name(&self) -> &str {
        &self.label
    }

    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        let key = require_api_key(&AiProviderKind::Anthropic, self.config.api_key.as_ref())?
            .ok_or_else(|| AiError::MissingCredentials {
                provider: "Anthropic Claude".to_owned(),
                env_hint: "ANTHROPIC_API_KEY".to_owned(),
            })?;

        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/messages");
        let stream = self.config.use_streaming && request.stream;
        let body = self.build_request_body(request, stream);
        let headers = [
            ("x-api-key", key.expose()),
            ("anthropic-version", ANTHROPIC_API_VERSION),
        ];

        on_event(AiStreamEvent::Status(
            "Anthropic Claude へリクエスト送信中...".to_owned(),
        ));

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &headers,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };
        let raw_text = post_json_streaming_or_sync(
            &http_req,
            parse_anthropic_sse_chunk,
            parse_anthropic_sync_text,
            on_event,
        )?;

        let response = AiResponse {
            raw_text,
            model: self.config.model.clone(),
        };
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

fn parse_anthropic_sse_chunk(payload: &str) -> Result<Option<String>, AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Anthropic SSE JSON parse failed: {err}")))?;
    if val["type"].as_str() == Some("content_block_delta")
        && val["delta"]["type"].as_str() == Some("text_delta")
    {
        return Ok(val["delta"]["text"].as_str().map(ToOwned::to_owned));
    }
    Ok(None)
}

fn parse_anthropic_sync_text(val: &Value) -> Result<String, AiError> {
    let blocks = val["content"].as_array().ok_or_else(|| {
        AiError::ParseFailed("Anthropic response missing content array".to_owned())
    })?;
    let mut joined = String::new();
    for block in blocks {
        if let Some(txt) = block["text"].as_str() {
            joined.push_str(txt);
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::sse::test_http::spawn_single_response_server;

    #[test]
    fn anthropic_sse_deltas_parse_accurately() -> Result<(), Box<dyn std::error::Error>> {
        let sse = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"type\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\\\"undo\\\"}\"}}\n\n",
        );
        let (addr, server) = spawn_single_response_server("text/event-stream", sse)?;

        let mut cfg = AnthropicConfig::new(
            "claude-sonnet-4-20250514",
            Some(SecretString::new("sk-ant-secret")),
        );
        cfg.base_url = format!("http://{addr}/v1");
        let provider = AnthropicProvider::new(cfg);

        let req = AiRequest {
            system_prompt: "JSON".to_owned(),
            command_schema_json: json!({}),
            context_summary: json!({}),
            history: Vec::new(),
            user_prompt: "Undo".to_owned(),
            stream: true,
        };
        let res = provider.send_streaming(&req, &mut |_| {})?;
        assert_eq!(res.raw_text, "{\"type\":\"undo\"}");

        let captured = server
            .join()
            .map_err(|_| "server join failed")??
            .to_ascii_lowercase();
        assert!(captured.contains("x-api-key: sk-ant-secret"));
        assert!(captured.contains("anthropic-version: 2023-06-01"));

        let thinking_body = AnthropicProvider::new(
            AnthropicConfig::new("claude-sonnet-4-20250514", None)
                .with_thinking_level(AiThinkingLevel::High),
        )
        .build_request_body(&req, false);
        assert_eq!(thinking_body["thinking"]["budget_tokens"], json!(10240));
        assert_eq!(thinking_body["max_tokens"], json!(14336));
        Ok(())
    }

    #[test]
    #[ignore = "requires live LLM API credentials"]
    fn live_anthropic_smoke() -> Result<(), Box<dyn std::error::Error>> {
        let provider =
            AnthropicProvider::new(AnthropicConfig::new("claude-sonnet-4-20250514", None));
        let req = AiRequest {
            system_prompt: "Return JSON".to_owned(),
            command_schema_json: json!({}),
            context_summary: json!({}),
            history: Vec::new(),
            user_prompt: "ping".to_owned(),
            stream: false,
        };
        let _res = provider.send_streaming(&req, &mut |_| {})?;
        Ok(())
    }
}
