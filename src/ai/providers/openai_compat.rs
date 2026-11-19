//! OpenAI・OpenRouter・Ollama・LM Studio・vLLM 共通 HTTP/SSE クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingLevel, AiToolCall, AiTurnMessage,
    SecretString, format_tools_fallback_instructions, parse_fallback_agent_response,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync, post_json_sync};

/// `OpenAI` 互換チャット補完 API クライアント設定。
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// 論理プロバイダ種別 (`OpenAi` / `OpenRouter` / `OllamaCompat`)。
    pub kind: AiProviderKind,
    /// API ベース URL（末尾スラッシュ不要。例: `https://openrouter.ai/api/v1`）。
    pub base_url: String,
    /// 使用モデル ID。
    pub model: String,
    /// 推論・思考強度レベル（`Auto` 時は未指定）。
    pub thinking_level: AiThinkingLevel,
    /// 明示指定のランタイム API キー（`None` 時は環境変数 / OS keyring を参照）。
    pub api_key: Option<SecretString>,
    /// SSE ストリーミングを使用するか。
    pub use_streaming: bool,
    /// HTTP タイムアウト（秒）。
    pub timeout_secs: u64,
}

impl OpenAiCompatConfig {
    /// `OpenAI` 公式 API 用既定設定を作成する。
    #[must_use]
    pub fn openai(model: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            kind: AiProviderKind::OpenAi,
            base_url: AiProviderKind::OpenAi.default_base_url().to_owned(),
            model: model.into(),
            thinking_level: AiThinkingLevel::Auto,
            api_key,
            use_streaming: true,
            timeout_secs: 60,
        }
    }

    /// `OpenRouter` API 用既定設定を作成する。
    #[must_use]
    pub fn openrouter(model: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            kind: AiProviderKind::OpenRouter,
            base_url: AiProviderKind::OpenRouter.default_base_url().to_owned(),
            model: model.into(),
            thinking_level: AiThinkingLevel::Auto,
            api_key,
            use_streaming: true,
            timeout_secs: 60,
        }
    }

    /// Ollama / LM Studio / vLLM 等ローカル `OpenAI` 互換エンドポイント設定を作成する。
    #[must_use]
    pub fn local_compatible(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<SecretString>,
    ) -> Self {
        Self {
            kind: AiProviderKind::OllamaCompat,
            base_url: base_url.into(),
            model: model.into(),
            thinking_level: AiThinkingLevel::Auto,
            api_key,
            use_streaming: true,
            timeout_secs: 120,
        }
    }

    /// 推論・思考強度レベルを指定する。
    #[must_use]
    pub const fn with_thinking_level(mut self, thinking_level: AiThinkingLevel) -> Self {
        self.thinking_level = thinking_level;
        self
    }
}

/// `OpenAI` 互換チャット補完プロバイダ実装。
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    config: OpenAiCompatConfig,
    label: String,
}

impl OpenAiCompatProvider {
    /// 設定からプロバイダインスタンスを構築する。
    #[must_use]
    pub fn new(config: OpenAiCompatConfig) -> Self {
        let label = format!("{} ({})", config.kind.display_label(), config.model);
        Self { config, label }
    }

    fn is_openai_reasoning_model(&self) -> bool {
        if !matches!(self.config.kind, AiProviderKind::OpenAi) {
            return false;
        }
        let model_lower = self.config.model.to_ascii_lowercase();
        model_lower.starts_with("o1")
            || model_lower.starts_with("o3")
            || model_lower.starts_with("o4")
            || model_lower.starts_with("gpt-5")
    }

    fn build_request_body(&self, request: &AiRequest, stream: bool) -> Value {
        let system_text = format!(
            "{}\n\n# EditCommand JSON Schema\n{}\n\n# Current Timeline Context\n{}",
            request.system_prompt, request.command_schema_json, request.context_summary
        );
        let mut messages = vec![json!({
            "role": "system",
            "content": system_text,
        })];
        for turn in &request.history {
            let role = match turn.role {
                AiRole::System => "system",
                AiRole::User => "user",
                AiRole::Assistant => "assistant",
            };
            messages.push(json!({ "role": role, "content": turn.content }));
        }
        messages.push(json!({ "role": "user", "content": request.user_prompt }));

        let effort = self.config.thinking_level.reasoning_effort();
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": stream,
        });
        if !self.is_openai_reasoning_model()
            && (effort.is_none() || !matches!(self.config.kind, AiProviderKind::OpenAi))
        {
            body["temperature"] = json!(0.1);
        }
        if let Some(effort_str) = effort {
            if matches!(self.config.kind, AiProviderKind::OpenRouter) {
                body["reasoning"] = json!({ "effort": effort_str });
            } else {
                body["reasoning_effort"] = json!(effort_str);
            }
        }
        // Ollama/OpenAI 互換の多くで有効な JSON 出力モードヒントを付与
        if matches!(
            self.config.kind,
            AiProviderKind::OpenAi | AiProviderKind::OpenRouter
        ) {
            body["response_format"] = json!({ "type": "json_object" });
        }
        body
    }

    fn build_agent_request_body(&self, request: &AiAgentTurnRequest) -> Value {
        let native_tools = self.supports_native_tools();
        let mut sys = request.system_prompt.clone();
        if !native_tools {
            sys.push_str(&format_tools_fallback_instructions(&request.tools));
        }
        let mut messages = vec![json!({
            "role": "system",
            "content": sys,
        })];
        for turn in &request.messages {
            messages.push(openai_turn_message_to_json(turn, native_tools));
        }

        let effort = self.config.thinking_level.reasoning_effort();
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": false,
        });
        if !self.is_openai_reasoning_model()
            && (effort.is_none() || !matches!(self.config.kind, AiProviderKind::OpenAi))
        {
            body["temperature"] = json!(request.temperature.unwrap_or(0.2));
        }
        if let Some(effort_str) = effort {
            if matches!(self.config.kind, AiProviderKind::OpenRouter) {
                body["reasoning"] = json!({ "effort": effort_str });
            } else {
                body["reasoning_effort"] = json!(effort_str);
            }
        }
        if native_tools && !request.tools.is_empty() {
            let tool_defs: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tool_defs);
        } else if request.tools.is_empty()
            && request.response_json_schema.is_some()
            && matches!(
                self.config.kind,
                AiProviderKind::OpenAi | AiProviderKind::OpenRouter
            )
        {
            body["response_format"] = json!({ "type": "json_object" });
        }
        body
    }

    fn build_headers(&self, maybe_key: Option<&SecretString>) -> Vec<(String, String)> {
        let mut owned_headers = Vec::new();
        if let Some(key) = maybe_key {
            owned_headers.push((
                "Authorization".to_owned(),
                format!("Bearer {}", key.expose()),
            ));
        }
        if matches!(self.config.kind, AiProviderKind::OpenRouter) {
            owned_headers.push((
                "HTTP-Referer".to_owned(),
                "https://github.com/sea-movie-creator".to_owned(),
            ));
            owned_headers.push(("X-Title".to_owned(), "sea-movie-creator".to_owned()));
        }
        owned_headers
    }
}

fn is_likely_ollama_vision_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    [
        "gemma3",
        "llava",
        "bakllava",
        "moondream",
        "minicpm-v",
        "pixtral",
        "internvl",
        "granite-vision",
        "llama3.2-vision",
        "qwen2.5vl",
        "qwen2.5-vl",
        "qwen2-vl",
        "qwen3-vl",
        "vision",
    ]
    .iter()
    .any(|token| m.contains(token))
        || m.contains("-vl:")
        || m.ends_with("-vl")
}

impl AiProvider for OpenAiCompatProvider {
    fn id(&self) -> &'static str {
        match self.config.kind {
            AiProviderKind::OpenRouter => "openrouter",
            AiProviderKind::OllamaCompat => "ollama_compat",
            _ => "openai",
        }
    }

    fn display_name(&self) -> &str {
        &self.label
    }

    fn supports_vision(&self) -> bool {
        if !matches!(self.config.kind, AiProviderKind::OllamaCompat) {
            return true;
        }
        is_likely_ollama_vision_model(&self.config.model)
    }

    fn supports_native_tools(&self) -> bool {
        matches!(
            self.config.kind,
            AiProviderKind::OpenAi | AiProviderKind::OpenRouter
        )
    }

    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        let maybe_key = require_api_key(&self.config.kind, self.config.api_key.as_ref())?;
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/chat/completions");
        let stream = self.config.use_streaming && request.stream;
        let body = self.build_request_body(request, stream);

        on_event(AiStreamEvent::Status(format!(
            "{} へリクエスト送信中...",
            self.config.kind.display_label()
        )));

        let owned_headers = self.build_headers(maybe_key.as_ref());
        let header_refs: Vec<(&str, &str)> = owned_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &header_refs,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };

        let raw_text = post_json_streaming_or_sync(
            &http_req,
            parse_openai_sse_chunk,
            parse_openai_sync_json,
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
        let maybe_key = require_api_key(&self.config.kind, self.config.api_key.as_ref())?;
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/chat/completions");
        let body = self.build_agent_request_body(request);

        on_event(AiStreamEvent::Status(format!(
            "{} エージェント推論中...",
            self.config.kind.display_label()
        )));

        let owned_headers = self.build_headers(maybe_key.as_ref());
        let header_refs: Vec<(&str, &str)> = owned_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &header_refs,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };
        let val = post_json_sync(&http_req)?;
        if self.supports_native_tools() {
            parse_openai_agent_response(&val, &self.config.model)
        } else {
            let text = parse_openai_sync_json(&val)?;
            Ok(parse_fallback_agent_response(&text, &self.config.model))
        }
    }
}

fn openai_turn_message_to_json(msg: &AiTurnMessage, native_tools: bool) -> Value {
    match msg {
        AiTurnMessage::System { text } => json!({ "role": "system", "content": text }),
        AiTurnMessage::User { parts } => {
            let has_images = parts
                .iter()
                .any(|p| matches!(p, AiContentPart::ImageData { .. }));
            if !has_images
                && parts.len() == 1
                && let AiContentPart::Text { text } = &parts[0]
            {
                return json!({ "role": "user", "content": text });
            }
            let content: Vec<Value> = parts
                .iter()
                .map(|part| match part {
                    AiContentPart::Text { text } => json!({ "type": "text", "text": text }),
                    AiContentPart::ImageData {
                        mime_type,
                        base64_data,
                    } => json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{mime_type};base64,{base64_data}")
                        }
                    }),
                })
                .collect();
            json!({ "role": "user", "content": content })
        }
        AiTurnMessage::Assistant { text, tool_calls } => {
            if tool_calls.is_empty() || !native_tools {
                let fallback = if tool_calls.is_empty() {
                    text.clone().unwrap_or_default()
                } else {
                    json!({ "message": text, "tool_calls": tool_calls }).to_string()
                };
                return json!({ "role": "assistant", "content": fallback });
            }
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments.to_string(),
                        }
                    })
                })
                .collect();
            json!({
                "role": "assistant",
                "content": text,
                "tool_calls": calls,
            })
        }
        AiTurnMessage::ToolResult {
            tool_call_id,
            name,
            content_json,
            is_error,
        } => {
            if native_tools {
                json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_json,
                })
            } else {
                let status = if *is_error { "ERROR" } else { "OK" };
                json!({
                    "role": "user",
                    "content": format!("[ToolResult {name} id={tool_call_id} status={status}]\n{content_json}"),
                })
            }
        }
    }
}

fn parse_openai_agent_response(val: &Value, model: &str) -> Result<AiAgentTurnResponse, AiError> {
    let choice = &val["choices"][0];
    let message = choice
        .get("message")
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            AiError::ParseFailed("OpenAI response missing choices[0].message".to_owned())
        })?;
    let text = message["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    let mut tool_calls = Vec::new();
    if let Some(arr) = message["tool_calls"].as_array() {
        for (idx, call) in arr.iter().enumerate() {
            if let Some(name) = call["function"]["name"].as_str() {
                let id = call["id"]
                    .as_str()
                    .map_or_else(|| format!("oai_call_{idx}"), ToOwned::to_owned);
                let args = match &call["function"]["arguments"] {
                    Value::String(raw) => serde_json::from_str::<Value>(raw)
                        .unwrap_or_else(|_| Value::String(raw.clone())),
                    other if !other.is_null() => other.clone(),
                    _ => json!({}),
                };
                tool_calls.push(AiToolCall {
                    id,
                    name: name.to_owned(),
                    arguments: args,
                });
            }
        }
    }

    Ok(AiAgentTurnResponse {
        text,
        tool_calls,
        model: model.to_owned(),
        finish_reason: choice["finish_reason"].as_str().map(ToOwned::to_owned),
    })
}

fn parse_openai_sse_chunk(payload: &str) -> Result<Option<String>, AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("OpenAI SSE JSON parse failed: {err}")))?;
    let delta = val["choices"][0]["delta"]["content"]
        .as_str()
        .map(ToOwned::to_owned);
    Ok(delta)
}

fn parse_openai_sync_json(val: &Value) -> Result<String, AiError> {
    val["choices"][0]["message"]["content"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AiError::ParseFailed("OpenAI response missing choices[0].message.content".to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::sse::test_http::spawn_single_response_server;

    fn sample_request(stream: bool) -> AiRequest {
        AiRequest {
            system_prompt: "Output EditCommand JSON.".to_owned(),
            command_schema_json: json!({ "title": "EditCommand" }),
            context_summary: json!({ "playhead_frame": 0 }),
            history: Vec::new(),
            user_prompt: "Add subtitle".to_owned(),
            stream,
        }
    }

    #[test]
    fn openrouter_streams_sse_deltas_from_local_server() -> Result<(), Box<dyn std::error::Error>> {
        let sse_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"type\\\":\\\"rename_layer\\\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\",\\\"layer_id\\\":\\\"L1\\\"}\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (addr, server) = spawn_single_response_server("text/event-stream", sse_body)?;

        let mut cfg = OpenAiCompatConfig::openrouter(
            "openai/gpt-4o-mini",
            Some(SecretString::new("or-secret-test")),
        );
        cfg.base_url = format!("http://{addr}/v1");
        let provider = OpenAiCompatProvider::new(cfg);

        // Debug 出力にシークレットが含まれないことも検証
        assert!(!format!("{provider:?}").contains("or-secret-test"));

        let mut deltas = Vec::new();
        let reply = provider.send_streaming(&sample_request(true), &mut |evt| {
            if let AiStreamEvent::DeltaText(chunk) = evt {
                deltas.push(chunk);
            }
        })?;
        assert_eq!(deltas.len(), 2);
        assert_eq!(
            reply.raw_text,
            "{\"type\":\"rename_layer\",\"layer_id\":\"L1\"}"
        );

        let captured_req = server.join().map_err(|_| "server join failed")??;
        let captured_lower = captured_req.to_ascii_lowercase();
        assert!(captured_lower.contains("authorization: bearer or-secret-test"));
        assert!(captured_lower.contains("x-title: sea-movie-creator"));

        let openai_thinking = OpenAiCompatProvider::new(
            OpenAiCompatConfig::openai("o4-mini", None).with_thinking_level(AiThinkingLevel::High),
        )
        .build_request_body(&sample_request(false), false);
        assert_eq!(openai_thinking["reasoning_effort"], json!("high"));
        assert!(openai_thinking.get("temperature").is_none());

        let router_thinking = OpenAiCompatProvider::new(
            OpenAiCompatConfig::openrouter("openai/gpt-5", None)
                .with_thinking_level(AiThinkingLevel::Medium),
        )
        .build_request_body(&sample_request(false), false);
        assert_eq!(router_thinking["reasoning"]["effort"], json!("medium"));
        Ok(())
    }

    #[test]
    fn openai_native_tools_and_ollama_json_fallback_work() -> Result<(), Box<dyn std::error::Error>>
    {
        let native_resp = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"inspect_media_folder","arguments":"{\"path\":\"C:/wedding\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let (addr1, srv1) = spawn_single_response_server("application/json", native_resp)?;
        let mut oai_cfg = OpenAiCompatConfig::openai("gpt-4.1-mini", Some(SecretString::new("k")));
        oai_cfg.base_url = format!("http://{addr1}/v1");
        let oai = OpenAiCompatProvider::new(oai_cfg);
        assert!(oai.supports_vision());
        assert!(oai.supports_native_tools());

        let turn_req = AiAgentTurnRequest {
            system_prompt: "Plan".to_owned(),
            messages: vec![AiTurnMessage::User {
                parts: vec![
                    AiContentPart::text("見て"),
                    AiContentPart::image_bytes("image/jpeg", b"jpg"),
                ],
            }],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "inspect_media_folder".to_owned(),
                description: "scan".to_owned(),
                parameters_schema: json!({ "type": "object" }),
            }],
            response_json_schema: None,
            temperature: None,
        };
        let res1 = oai.send_agent_turn(&turn_req, &mut |_| {})?;
        assert_eq!(res1.tool_calls.len(), 1);
        assert_eq!(res1.tool_calls[0].id, "call_1");
        assert_eq!(res1.tool_calls[0].arguments["path"], "C:/wedding");
        let cap1 = srv1.join().map_err(|_| "join1")??;
        assert!(cap1.contains("image_url"));
        assert!(cap1.contains("\"tools\""));

        // Ollama は Native Tool Calling なしで統一 JSON アクション出力フォールバックを使う
        let ollama_resp = r#"{"choices":[{"message":{"content":"{\"tool_calls\":[{\"name\":\"inspect_media_folder\",\"arguments\":{\"path\":\"C:/ol\"}}]}"},"finish_reason":"stop"}]}"#;
        let (addr2, srv2) = spawn_single_response_server("application/json", ollama_resp)?;
        let ollama = OpenAiCompatProvider::new(OpenAiCompatConfig::local_compatible(
            format!("http://{addr2}/v1"),
            "gemma3:12b",
            None,
        ));
        assert!(ollama.supports_vision());
        assert!(!ollama.supports_native_tools());
        let res2 = ollama.send_agent_turn(&turn_req, &mut |_| {})?;
        assert_eq!(res2.tool_calls.len(), 1);
        assert_eq!(res2.tool_calls[0].arguments["path"], "C:/ol");
        let cap2 = srv2.join().map_err(|_| "join2")??;
        assert!(cap2.contains("JSON Action Mode"));

        let gpt5_body = OpenAiCompatProvider::new(OpenAiCompatConfig::openai("gpt-5", None))
            .build_agent_request_body(&turn_req);
        assert!(gpt5_body.get("temperature").is_none());

        let text_ollama = OpenAiCompatProvider::new(OpenAiCompatConfig::local_compatible(
            "http://127.0.0.1:11434/v1",
            "qwen3:8b",
            None,
        ));
        assert!(!text_ollama.supports_vision());
        let llama_ollama = OpenAiCompatProvider::new(OpenAiCompatConfig::local_compatible(
            "http://127.0.0.1:11434/v1",
            "llama3.2",
            None,
        ));
        assert!(!llama_ollama.supports_vision());
        Ok(())
    }

    #[test]
    #[ignore = "requires live LLM API credentials"]
    fn live_openai_or_openrouter_smoke() -> Result<(), Box<dyn std::error::Error>> {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig::openai("gpt-4.1-mini", None));
        let _res = provider.send_streaming(&sample_request(false), &mut |_| {})?;
        Ok(())
    }
}
