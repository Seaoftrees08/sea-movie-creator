//! Google Gemini REST / SSE API クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingLevel, AiTokenUsage, AiToolCall,
    AiTurnMessage, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_sse_or_sync};

/// Google Gemini API 接続設定。
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    /// API ベース URL。
    pub base_url: String,
    /// モデル ID（例: `gemini-2.5-flash`）。
    pub model: String,
    /// 思考強度レベル（`Auto` 時は `thinkingConfig` 未付与）。
    pub thinking_level: AiThinkingLevel,
    /// 明示指定 API キー（`None` 時は `GEMINI_API_KEY` / OS keyring を使用）。
    pub api_key: Option<SecretString>,
    /// SSE ストリーミングを使うか。
    pub use_streaming: bool,
    /// タイムアウト（秒）。
    pub timeout_secs: u64,
}

impl GeminiConfig {
    /// 既定の Gemini API 設定を作成する。
    #[must_use]
    pub fn new(model: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            base_url: AiProviderKind::Gemini.default_base_url().to_owned(),
            model: model.into(),
            thinking_level: AiThinkingLevel::Auto,
            api_key,
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

/// Google Gemini REST / SSE プロバイダ実装。
#[derive(Debug, Clone)]
pub struct GeminiProvider {
    config: GeminiConfig,
    label: String,
}

impl GeminiProvider {
    /// 設定から Gemini プロバイダを生成する。
    #[must_use]
    pub fn new(config: GeminiConfig) -> Self {
        let label = format!("Google Gemini ({})", config.model);
        Self { config, label }
    }

    fn build_request_body(&self, request: &AiRequest) -> Value {
        let system_text = format!(
            "{}\n\n# EditCommand JSON Schema\n{}\n\n# Current Timeline Context\n{}",
            request.system_prompt, request.command_schema_json, request.context_summary
        );
        let mut contents = Vec::new();
        for turn in &request.history {
            let role = match turn.role {
                AiRole::Assistant => "model",
                AiRole::User | AiRole::System => "user",
            };
            contents.push(json!({
                "role": role,
                "parts": [{ "text": turn.content }],
            }));
        }
        contents.push(json!({
            "role": "user",
            "parts": [{ "text": request.user_prompt }],
        }));

        let mut gen_cfg = json!({
            "temperature": 0.1,
            "responseMimeType": "application/json",
        });
        self.apply_thinking_config(&mut gen_cfg);

        json!({
            "systemInstruction": {
                "parts": [{ "text": system_text }]
            },
            "contents": contents,
            "generationConfig": gen_cfg,
        })
    }

    fn apply_thinking_config(&self, gen_cfg: &mut Value) {
        let lower_model = self.config.model.to_ascii_lowercase();
        if lower_model.contains("gemini-3") || lower_model.contains("gemini-4") {
            if let Some(level) = self.config.thinking_level.gemini_thinking_level() {
                gen_cfg["thinkingConfig"] = json!({ "thinkingLevel": level });
            }
        } else if let Some(budget) = self.config.thinking_level.gemini_thinking_budget() {
            gen_cfg["thinkingConfig"] = json!({ "thinkingBudget": budget });
        }
    }

    fn validate_model_id(&self) -> Result<(), AiError> {
        let model = self.config.model.as_str();
        if model.is_empty()
            || model.contains("..")
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(AiError::Transport(format!(
                "invalid Gemini model identifier: {}",
                self.config.model
            )));
        }
        Ok(())
    }

    fn build_agent_request_body(&self, request: &AiAgentTurnRequest) -> Value {
        let mut system_parts = vec![json!({ "text": request.system_prompt })];
        let mut contents: Vec<Value> = Vec::new();
        for turn in &request.messages {
            if let AiTurnMessage::System { text } = turn {
                if !text.is_empty() {
                    system_parts.push(json!({ "text": text }));
                }
                continue;
            }
            let (role, mut parts) = gemini_turn_message_to_parts(turn);
            if parts.is_empty() {
                continue;
            }
            if let Some(last) = contents.last_mut()
                && last["role"].as_str() == Some(role)
                && let Some(existing_parts) = last["parts"].as_array_mut()
            {
                existing_parts.append(&mut parts);
                continue;
            }
            contents.push(json!({
                "role": role,
                "parts": parts,
            }));
        }

        let temp = request.temperature.unwrap_or(0.2);
        let mut gen_cfg = json!({ "temperature": temp });
        if request.tools.is_empty()
            && let Some(schema) = &request.response_json_schema
        {
            gen_cfg["responseMimeType"] = json!("application/json");
            gen_cfg["responseJsonSchema"] = schema.clone();
        }
        self.apply_thinking_config(&mut gen_cfg);

        let mut body = json!({
            "systemInstruction": {
                "parts": system_parts,
            },
            "contents": contents,
            "generationConfig": gen_cfg,
        });
        if !request.tools.is_empty() {
            let decls: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parametersJsonSchema": tool.parameters_schema,
                    })
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": decls }]);
        }
        body
    }
}

impl AiProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
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
        self.validate_model_id()?;
        let key = require_api_key(&AiProviderKind::Gemini, self.config.api_key.as_ref())?
            .ok_or_else(|| AiError::MissingCredentials {
                provider: "Google Gemini".to_owned(),
                env_hint: "GEMINI_API_KEY".to_owned(),
            })?;

        let base = self.config.base_url.trim_end_matches('/');
        let stream = self.config.use_streaming && request.stream;
        let url = if stream {
            format!(
                "{base}/models/{}:streamGenerateContent?alt=sse",
                self.config.model
            )
        } else {
            format!("{base}/models/{}:generateContent", self.config.model)
        };
        let body = self.build_request_body(request);
        let headers = [("x-goog-api-key", key.expose())];

        on_event(AiStreamEvent::Status(
            "Google Gemini へリクエスト送信中...".to_owned(),
        ));

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &headers,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };
        let (raw_text, sse_usage, sync_usage, on_event) = post_json_sse_or_sync(
            &http_req,
            (
                String::new(),
                None::<AiTokenUsage>,
                None::<AiTokenUsage>,
                on_event,
            ),
            |(collected, sse_u, _, evt_cb), _event_name, payload| {
                process_gemini_streaming_payload(payload, collected, sse_u, *evt_cb)?;
                Ok(false)
            },
            |(collected, sse_u, sync_u, evt_cb)| Ok((collected, sse_u, sync_u, evt_cb)),
            |(_, sse_u, mut sync_u, evt_cb), val| {
                let text = parse_gemini_sync_text(val, &mut sync_u)?;
                if !text.is_empty() {
                    evt_cb(AiStreamEvent::DeltaText(text.clone()));
                }
                Ok((text, sse_u, sync_u, evt_cb))
            },
        )?;

        let response = AiResponse {
            raw_text,
            model: self.config.model.clone(),
            usage: sse_usage.or(sync_usage),
        };
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
    }

    fn send_agent_turn(
        &self,
        request: &AiAgentTurnRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiAgentTurnResponse, AiError> {
        self.validate_model_id()?;
        let key = require_api_key(&AiProviderKind::Gemini, self.config.api_key.as_ref())?
            .ok_or_else(|| AiError::MissingCredentials {
                provider: "Google Gemini".to_owned(),
                env_hint: "GEMINI_API_KEY".to_owned(),
            })?;

        let base = self.config.base_url.trim_end_matches('/');
        let url = if self.config.use_streaming {
            format!(
                "{base}/models/{}:streamGenerateContent?alt=sse",
                self.config.model
            )
        } else {
            format!("{base}/models/{}:generateContent", self.config.model)
        };
        let body = self.build_agent_request_body(request);
        let headers = [("x-goog-api-key", key.expose())];

        on_event(AiStreamEvent::Status(
            "Google Gemini エージェント推論中...".to_owned(),
        ));

        let http_req = HttpJsonRequest {
            url: &url,
            headers: &headers,
            body: &body,
            timeout_secs: self.config.timeout_secs,
        };
        post_json_sse_or_sync(
            &http_req,
            (GeminiAgentStreamAcc::default(), on_event),
            |(acc, evt_cb), _event_name, payload| {
                process_gemini_agent_sse_payload(acc, payload, *evt_cb)?;
                Ok(false)
            },
            |(state, _)| {
                Ok(AiAgentTurnResponse {
                    text: (!state.text_buf.is_empty()).then_some(state.text_buf),
                    thinking_blocks: Vec::new(),
                    tool_calls: state.tool_calls,
                    model: self.config.model.clone(),
                    finish_reason: state.finish_reason,
                    usage: state.usage,
                })
            },
            |(_, evt_cb), json_val| {
                let parsed = parse_gemini_agent_response(json_val, &self.config.model)?;
                if let Some(txt) = parsed.text.as_ref().filter(|s| !s.is_empty()) {
                    evt_cb(AiStreamEvent::DeltaText(txt.clone()));
                }
                Ok(parsed)
            },
        )
    }
}

fn gemini_turn_message_to_parts(msg: &AiTurnMessage) -> (&'static str, Vec<Value>) {
    match msg {
        AiTurnMessage::System { text } => ("user", vec![json!({ "text": text })]),
        AiTurnMessage::User { parts } => {
            let gem_parts: Vec<Value> = parts
                .iter()
                .map(|part| match part {
                    AiContentPart::Text { text } => json!({ "text": text }),
                    AiContentPart::ImageData {
                        mime_type,
                        base64_data,
                    } => json!({
                        "inlineData": {
                            "mimeType": mime_type,
                            "data": base64_data,
                        }
                    }),
                })
                .collect();
            ("user", gem_parts)
        }
        AiTurnMessage::Assistant {
            text, tool_calls, ..
        } => {
            let mut gem_parts = Vec::new();
            if let Some(txt) = text
                && !txt.is_empty()
            {
                gem_parts.push(json!({ "text": txt }));
            }
            for call in tool_calls {
                let mut part = json!({
                    "functionCall": {
                        "name": call.name,
                        "args": call.arguments,
                    }
                });
                if let Some(sig) = &call.thought_signature {
                    part["thoughtSignature"] = json!(sig);
                }
                gem_parts.push(part);
            }
            if gem_parts.is_empty() {
                gem_parts.push(json!({ "text": "" }));
            }
            ("model", gem_parts)
        }
        AiTurnMessage::ToolResult {
            name,
            content_json,
            is_error,
            ..
        } => {
            let parsed = serde_json::from_str::<Value>(content_json)
                .unwrap_or_else(|_| Value::String(content_json.clone()));
            let payload = if *is_error {
                json!({ "error": parsed })
            } else {
                json!({ "result": parsed })
            };
            (
                "user",
                vec![json!({
                    "functionResponse": {
                        "name": name,
                        "response": payload,
                    }
                })],
            )
        }
    }
}

#[derive(Debug, Default)]
struct GeminiAgentStreamAcc {
    text_buf: String,
    tool_calls: Vec<AiToolCall>,
    finish_reason: Option<String>,
    usage: Option<AiTokenUsage>,
}

fn process_gemini_streaming_payload(
    payload: &str,
    collected: &mut String,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Gemini SSE JSON parse failed: {err}")))?;
    if let Some(usage) = extract_gemini_usage(&val) {
        *captured_usage = Some(usage);
    }
    if let Some(parts) = val["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            let Some(txt) = part["text"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if part["thought"].as_bool() == Some(true) {
                on_event(AiStreamEvent::ThoughtDelta(txt.to_owned()));
            } else {
                on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                collected.push_str(txt);
            }
        }
    }
    Ok(())
}

fn process_gemini_agent_sse_payload(
    acc: &mut GeminiAgentStreamAcc,
    payload: &str,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload).map_err(|err| {
        AiError::ParseFailed(format!("Gemini agent SSE JSON parse failed: {err}"))
    })?;
    if let Some(usage) = extract_gemini_usage(&val) {
        acc.usage = Some(usage);
    }
    let candidate = &val["candidates"][0];
    if let Some(reason) = candidate["finishReason"].as_str().filter(|s| !s.is_empty()) {
        acc.finish_reason = Some(reason.to_owned());
    }
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            let is_thought = part["thought"].as_bool() == Some(true);
            if let Some(txt) = part["text"].as_str().filter(|s| !s.is_empty()) {
                if is_thought {
                    on_event(AiStreamEvent::ThoughtDelta(txt.to_owned()));
                } else {
                    on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                    acc.text_buf.push_str(txt);
                }
            }
            if let Some(fc) = part.get("functionCall").filter(|v| v.is_object())
                && let Some(name) = fc["name"].as_str()
            {
                let idx = acc.tool_calls.len();
                let thought_sig = part["thoughtSignature"]
                    .as_str()
                    .or_else(|| part["thought_signature"].as_str())
                    .map(ToOwned::to_owned);
                acc.tool_calls.push(AiToolCall {
                    id: format!("gem_call_{idx}"),
                    name: name.to_owned(),
                    arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                    thought_signature: thought_sig,
                });
            }
        }
    }
    Ok(())
}

fn parse_gemini_agent_response(val: &Value, model: &str) -> Result<AiAgentTurnResponse, AiError> {
    let candidate = &val["candidates"][0];
    let parts = candidate["content"]["parts"]
        .as_array()
        .ok_or_else(|| AiError::ParseFailed("Gemini agent response missing parts".to_owned()))?;
    let mut text_buf = String::new();
    let mut tool_calls = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        let is_thought = part["thought"].as_bool() == Some(true);
        if !is_thought && let Some(txt) = part["text"].as_str() {
            text_buf.push_str(txt);
        }
        if let Some(fc) = part.get("functionCall").filter(|v| v.is_object())
            && let Some(name) = fc["name"].as_str()
        {
            let thought_sig = part["thoughtSignature"]
                .as_str()
                .or_else(|| part["thought_signature"].as_str())
                .map(ToOwned::to_owned);
            tool_calls.push(AiToolCall {
                id: format!("gem_call_{idx}"),
                name: name.to_owned(),
                arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                thought_signature: thought_sig,
            });
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!text_buf.is_empty()).then_some(text_buf),
        thinking_blocks: Vec::new(),
        tool_calls,
        model: model.to_owned(),
        finish_reason: candidate["finishReason"].as_str().map(ToOwned::to_owned),
        usage: extract_gemini_usage(val),
    })
}

fn parse_gemini_sync_text(
    val: &Value,
    captured_usage: &mut Option<AiTokenUsage>,
) -> Result<String, AiError> {
    if let Some(usage) = extract_gemini_usage(val) {
        *captured_usage = Some(usage);
    }
    extract_candidate_text(val)
        .ok_or_else(|| AiError::ParseFailed("Gemini response missing candidates text".to_owned()))
}

fn extract_gemini_usage(val: &Value) -> Option<AiTokenUsage> {
    let meta = val.get("usageMetadata")?.as_object()?;
    let input_tokens = meta
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let candidates = meta
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thoughts = meta
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = candidates.saturating_add(thoughts);
    let total_tokens = meta
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }
    Some(AiTokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cost_usd: None,
        is_estimated: false,
    })
}

fn extract_candidate_text(val: &Value) -> Option<String> {
    let parts = val["candidates"][0]["content"]["parts"].as_array()?;
    let mut joined = String::new();
    for part in parts {
        if part["thought"].as_bool() == Some(true) {
            continue;
        }
        if let Some(txt) = part["text"].as_str() {
            joined.push_str(txt);
        }
    }
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::sse::test_http::spawn_single_response_server;

    #[test]
    fn gemini_sync_and_header_work_against_mock_server() -> Result<(), Box<dyn std::error::Error>> {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"{\"type\":\"rename_layer\",\"layer_id\":\"L1\"}"}]}}],"usageMetadata":{"promptTokenCount":80,"candidatesTokenCount":30,"thoughtsTokenCount":15,"totalTokenCount":125}}"#;
        let (addr, server) = spawn_single_response_server("application/json", body)?;

        let mut cfg = GeminiConfig::new(
            "gemini-2.5-flash",
            Some(SecretString::new("gem-secret-abc")),
        );
        cfg.base_url = format!("http://{addr}/v1beta");
        cfg.use_streaming = false;
        let provider = GeminiProvider::new(cfg);

        let req = AiRequest {
            system_prompt: "JSON".to_owned(),
            command_schema_json: json!({}),
            context_summary: json!({}),
            history: Vec::new(),
            user_prompt: "Rename".to_owned(),
            stream: false,
        };
        let res = provider.send_streaming(&req, &mut |_| {})?;
        assert_eq!(
            res.raw_text,
            "{\"type\":\"rename_layer\",\"layer_id\":\"L1\"}"
        );
        let usage = res.usage.as_ref().ok_or("expected Gemini usage")?;
        assert_eq!(usage.input_tokens, 80);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.total_tokens, 125);

        let captured = server
            .join()
            .map_err(|_| "server join failed")??
            .to_ascii_lowercase();
        assert!(captured.contains("x-goog-api-key: gem-secret-abc"));

        let req_thinking = GeminiProvider::new(
            GeminiConfig::new("gemini-2.5-pro", None).with_thinking_level(AiThinkingLevel::High),
        )
        .build_request_body(&req);
        assert_eq!(
            req_thinking["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(24576)
        );
        Ok(())
    }

    #[test]
    fn gemini_agent_turn_supports_vision_and_function_calls()
    -> Result<(), Box<dyn std::error::Error>> {
        let resp_body = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"inspect_media_folder","args":{"path":"C:/wedding"}}}]},"finishReason":"STOP"}]}"#;
        let (addr, server) = spawn_single_response_server("application/json", resp_body)?;

        let mut cfg = GeminiConfig::new(
            "gemini-2.5-flash",
            Some(SecretString::new("gem-vision-key")),
        );
        cfg.base_url = format!("http://{addr}/v1beta");
        let provider = GeminiProvider::new(cfg);
        assert!(provider.supports_vision());
        assert!(provider.supports_native_tools());

        let turn_req = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![AiTurnMessage::User {
                parts: vec![
                    AiContentPart::text("この写真を見て"),
                    AiContentPart::image_bytes("image/jpeg", b"fakejpeg"),
                ],
            }],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "inspect_media_folder".to_owned(),
                description: "フォルダ走査".to_owned(),
                parameters_schema: json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "path": { "type": "string" } }
                }),
            }],
            response_json_schema: None,
            temperature: Some(0.1),
        };
        let res = provider.send_agent_turn(&turn_req, &mut |_| {})?;
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "inspect_media_folder");
        assert_eq!(res.tool_calls[0].arguments["path"], "C:/wedding");

        let captured = server.join().map_err(|_| "server join failed")??;
        assert!(captured.contains("inlineData"));
        assert!(captured.contains("functionDeclarations"));
        assert!(captured.contains("parametersJsonSchema"));
        Ok(())
    }

    #[test]
    fn gemini_merges_tool_results_filters_thoughts_and_validates_model_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = GeminiProvider::new(GeminiConfig::new("gemini-2.5-flash", None));
        // 並列 ToolResult の単一 user ターン統合、System -> systemInstruction、responseJsonSchema、thought 除外を検証
        let multi_req = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::System {
                    text: "System note".to_owned(),
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "c1".to_owned(),
                    name: "inspect_media_folder".to_owned(),
                    content_json: "{\"count\":10}".to_owned(),
                    is_error: false,
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "c2".to_owned(),
                    name: "get_capabilities".to_owned(),
                    content_json: "{\"fps\":60}".to_owned(),
                    is_error: false,
                },
            ],
            tools: Vec::new(),
            response_json_schema: Some(json!({"type": "object"})),
            temperature: Some(0.1),
        };
        let multi_body = provider.build_agent_request_body(&multi_req);
        assert_eq!(
            multi_body["systemInstruction"]["parts"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(multi_body["contents"].as_array().map(Vec::len), Some(1));
        assert_eq!(multi_body["contents"][0]["role"], json!("user"));
        assert_eq!(
            multi_body["contents"][0]["parts"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            multi_body["generationConfig"]["responseJsonSchema"],
            json!({"type": "object"})
        );

        let thought_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "internal thought", "thought": true },
                        { "text": "{\"ok\":true}" }
                    ]
                }
            }]
        });
        let parsed = parse_gemini_agent_response(&thought_resp, "gemini-2.5-pro")?;
        assert_eq!(parsed.text.as_deref(), Some("{\"ok\":true}"));
        assert_eq!(
            extract_candidate_text(&thought_resp).as_deref(),
            Some("{\"ok\":true}")
        );

        let bad_model = GeminiProvider::new(GeminiConfig::new("gemini-2.5/../../evil", None));
        assert!(bad_model.validate_model_id().is_err());
        let encoded_model = GeminiProvider::new(GeminiConfig::new("gemini-2.5%2fevil", None));
        assert!(encoded_model.validate_model_id().is_err());
        let colon_model = GeminiProvider::new(GeminiConfig::new("gemini:evil", None));
        assert!(colon_model.validate_model_id().is_err());
        Ok(())
    }

    #[test]
    fn gemini_agent_turn_streams_thoughts_and_preserves_thought_signature()
    -> Result<(), Box<dyn std::error::Error>> {
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"どの写真を回すか確認しよう\",\"thought\":true}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"確認カードを表示します。\"},{\"functionCall\":{\"name\":\"ask_user\",\"args\":{\"title\":\"確認\"}},\"thoughtSignature\":\"sig_gem_123\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":10,\"thoughtsTokenCount\":5,\"totalTokenCount\":35}}\n\n"
        );
        let (addr, srv) = spawn_single_response_server("text/event-stream", sse)?;
        let mut cfg = GeminiConfig::new("gemini-3.0-pro", Some(SecretString::new("gem-sse")));
        cfg.base_url = format!("http://{addr}/v1beta");
        let provider = GeminiProvider::new(cfg);

        let turn_req = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![AiTurnMessage::Assistant {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![AiToolCall {
                    id: "prev_1".to_owned(),
                    name: "get_timeline_summary".to_owned(),
                    arguments: json!({}),
                    thought_signature: Some("prev_sig_99".to_owned()),
                }],
            }],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        };
        let mut thoughts = Vec::new();
        let mut deltas = Vec::new();
        let res = provider.send_agent_turn(&turn_req, &mut |evt| match evt {
            AiStreamEvent::ThoughtDelta(t) => thoughts.push(t),
            AiStreamEvent::DeltaText(t) => deltas.push(t),
            _ => {}
        })?;
        assert_eq!(thoughts, vec!["どの写真を回すか確認しよう"]);
        assert_eq!(deltas, vec!["確認カードを表示します。"]);
        assert_eq!(res.text.as_deref(), Some("確認カードを表示します。"));
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(
            res.tool_calls[0].thought_signature.as_deref(),
            Some("sig_gem_123")
        );
        assert_eq!(res.usage.as_ref().map(|u| u.total_tokens), Some(35));

        let cap = srv.join().map_err(|_| "srv join")??;
        assert!(cap.contains("streamGenerateContent?alt=sse"));
        assert!(cap.contains("prev_sig_99"));
        Ok(())
    }

    #[test]
    #[ignore = "requires live LLM API credentials"]
    fn live_gemini_smoke() -> Result<(), Box<dyn std::error::Error>> {
        let provider = GeminiProvider::new(GeminiConfig::new("gemini-2.5-flash", None));
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
