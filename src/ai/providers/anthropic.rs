//! Anthropic Messages API クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingLevel, AiToolCall, AiTurnMessage,
    SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync, post_json_sync};

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

    fn build_agent_request_body(&self, request: &AiAgentTurnRequest) -> Value {
        let mut system_text = request.system_prompt.clone();
        let mut messages = Vec::new();
        for turn in &request.messages {
            match turn {
                AiTurnMessage::System { text } => {
                    system_text.push_str("\n\n");
                    system_text.push_str(text);
                }
                other => messages.push(anthropic_turn_message_to_json(other)),
            }
        }

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
            "stream": false,
        });
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters_schema,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
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

    fn supports_vision(&self) -> bool {
        true
    }

    fn supports_native_tools(&self) -> bool {
        true
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

    fn send_agent_turn(
        &self,
        request: &AiAgentTurnRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiAgentTurnResponse, AiError> {
        let key = require_api_key(&AiProviderKind::Anthropic, self.config.api_key.as_ref())?
            .ok_or_else(|| AiError::MissingCredentials {
                provider: "Anthropic Claude".to_owned(),
                env_hint: "ANTHROPIC_API_KEY".to_owned(),
            })?;

        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/messages");
        let body = self.build_agent_request_body(request);
        let headers = [
            ("x-api-key", key.expose()),
            ("anthropic-version", ANTHROPIC_API_VERSION),
        ];

        on_event(AiStreamEvent::Status(
            "Anthropic Claude エージェント推論中...".to_owned(),
        ));

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &headers,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };
        let val = post_json_sync(&http_req)?;
        parse_anthropic_agent_response(&val, &self.config.model)
    }
}

fn anthropic_turn_message_to_json(msg: &AiTurnMessage) -> Value {
    match msg {
        AiTurnMessage::System { text } => json!({ "role": "user", "content": text }),
        AiTurnMessage::User { parts } => {
            let content: Vec<Value> = parts
                .iter()
                .map(|part| match part {
                    AiContentPart::Text { text } => json!({ "type": "text", "text": text }),
                    AiContentPart::ImageData {
                        mime_type,
                        base64_data,
                    } => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": base64_data,
                        }
                    }),
                })
                .collect();
            json!({ "role": "user", "content": content })
        }
        AiTurnMessage::Assistant { text, tool_calls } => {
            let mut content = Vec::new();
            if let Some(txt) = text
                && !txt.is_empty()
            {
                content.push(json!({ "type": "text", "text": txt }));
            }
            for call in tool_calls {
                content.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            json!({ "role": "assistant", "content": content })
        }
        AiTurnMessage::ToolResult {
            tool_call_id,
            content_json,
            is_error,
            ..
        } => json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": content_json,
                "is_error": is_error,
            }]
        }),
    }
}

fn parse_anthropic_agent_response(
    val: &Value,
    model: &str,
) -> Result<AiAgentTurnResponse, AiError> {
    let blocks = val["content"].as_array().ok_or_else(|| {
        AiError::ParseFailed("Anthropic response missing content array".to_owned())
    })?;
    let mut text_buf = String::new();
    let mut tool_calls = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(txt) = block["text"].as_str() {
                    text_buf.push_str(txt);
                }
            }
            Some("tool_use") => {
                if let Some(name) = block["name"].as_str() {
                    let id = block["id"]
                        .as_str()
                        .map_or_else(|| format!("ant_call_{idx}"), ToOwned::to_owned);
                    tool_calls.push(AiToolCall {
                        id,
                        name: name.to_owned(),
                        arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!text_buf.is_empty()).then_some(text_buf),
        tool_calls,
        model: model.to_owned(),
        finish_reason: val["stop_reason"].as_str().map(ToOwned::to_owned),
    })
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
    fn anthropic_agent_turn_supports_vision_and_tool_use() -> Result<(), Box<dyn std::error::Error>>
    {
        let resp_body = r#"{"content":[{"type":"text","text":"確認します。"},{"type":"tool_use","id":"toolu_01","name":"inspect_media_folder","input":{"path":"C:/photos"}}],"stop_reason":"tool_use"}"#;
        let (addr, server) = spawn_single_response_server("application/json", resp_body)?;

        let mut cfg = AnthropicConfig::new(
            "claude-sonnet-4-20250514",
            Some(SecretString::new("sk-ant-vision")),
        );
        cfg.base_url = format!("http://{addr}/v1");
        let provider = AnthropicProvider::new(cfg);
        assert!(provider.supports_vision());
        assert!(provider.supports_native_tools());

        let turn_req = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![AiTurnMessage::User {
                parts: vec![
                    AiContentPart::text("解析して"),
                    AiContentPart::image_bytes("image/jpeg", b"imgbytes"),
                ],
            }],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "inspect_media_folder".to_owned(),
                description: "フォルダ調査".to_owned(),
                parameters_schema: json!({ "type": "object" }),
            }],
            response_json_schema: None,
            temperature: Some(0.2),
        };
        let res = provider.send_agent_turn(&turn_req, &mut |_| {})?;
        assert_eq!(res.text.as_deref(), Some("確認します。"));
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].id, "toolu_01");
        assert_eq!(res.tool_calls[0].arguments["path"], "C:/photos");

        let cap = server.join().map_err(|_| "server join")??;
        let (_, json_body) = cap.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
        let parsed_body: Value = serde_json::from_str(json_body)?;
        assert_eq!(parsed_body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            parsed_body["messages"][0]["content"][1]["source"]["type"],
            "base64"
        );
        assert_eq!(parsed_body["tools"][0]["name"], "inspect_media_folder");
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
