//! Google Gemini REST / SSE API クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingLevel, AiToolCall, AiTurnMessage,
    SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync, post_json_sync};

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
        let model = self.config.model.trim();
        if model.is_empty() || model.contains(['/', '?', '#', '\\', ' ']) || model.contains("..") {
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
        let raw_text = post_json_streaming_or_sync(
            &http_req,
            parse_gemini_json_text,
            parse_gemini_sync_text,
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
        self.validate_model_id()?;
        let key = require_api_key(&AiProviderKind::Gemini, self.config.api_key.as_ref())?
            .ok_or_else(|| AiError::MissingCredentials {
                provider: "Google Gemini".to_owned(),
                env_hint: "GEMINI_API_KEY".to_owned(),
            })?;

        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/models/{}:generateContent", self.config.model);
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
        let json_val = post_json_sync(&http_req)?;
        parse_gemini_agent_response(&json_val, &self.config.model)
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
        AiTurnMessage::Assistant { text, tool_calls } => {
            let mut gem_parts = Vec::new();
            if let Some(txt) = text
                && !txt.is_empty()
            {
                gem_parts.push(json!({ "text": txt }));
            }
            for call in tool_calls {
                gem_parts.push(json!({
                    "functionCall": {
                        "name": call.name,
                        "args": call.arguments,
                    }
                }));
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
            tool_calls.push(AiToolCall {
                id: format!("gem_call_{idx}"),
                name: name.to_owned(),
                arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!text_buf.is_empty()).then_some(text_buf),
        tool_calls,
        model: model.to_owned(),
        finish_reason: candidate["finishReason"].as_str().map(ToOwned::to_owned),
    })
}

fn parse_gemini_json_text(payload: &str) -> Result<Option<String>, AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Gemini SSE JSON parse failed: {err}")))?;
    Ok(extract_candidate_text(&val))
}

fn parse_gemini_sync_text(val: &Value) -> Result<String, AiError> {
    extract_candidate_text(val)
        .ok_or_else(|| AiError::ParseFailed("Gemini response missing candidates text".to_owned()))
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
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"{\"type\":\"rename_layer\",\"layer_id\":\"L1\"}"}]}}]}"#;
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
