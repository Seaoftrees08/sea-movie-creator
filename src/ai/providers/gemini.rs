//! Google Gemini REST / SSE API クライアント。

use std::sync::{Arc, atomic::AtomicBool};

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingBlock, AiThinkingLevel, AiTokenUsage,
    AiToolCall, AiTurnMessage, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_sse_or_sync};

const MAX_GEMINI_FLASH_THINKING_BUDGET: u32 = 24_576;

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
        let is_modern_gemini = lower_model.contains("gemini-3") || lower_model.contains("gemini-4");
        let is_thinking_capable = is_modern_gemini
            || lower_model.contains("gemini-2.5")
            || lower_model.contains("thinking");
        if !is_thinking_capable {
            return;
        }
        if is_modern_gemini {
            if let Some(level) = self.config.thinking_level.gemini_thinking_level() {
                gen_cfg["thinkingConfig"] = json!({
                    "thinkingLevel": level,
                    "includeThoughts": true,
                });
            } else {
                // Gemini 3/4 は Auto でも既定で思考するため includeThoughts を明示有効化する
                gen_cfg["thinkingConfig"] = json!({ "includeThoughts": true });
            }
        } else if let Some(raw_budget) = self.config.thinking_level.gemini_thinking_budget() {
            let budget = if lower_model.contains("flash") {
                raw_budget.min(MAX_GEMINI_FLASH_THINKING_BUDGET)
            } else {
                raw_budget
            };
            let mut cfg = json!({ "thinkingBudget": budget });
            if budget > 0 {
                cfg["includeThoughts"] = json!(true);
            }
            gen_cfg["thinkingConfig"] = cfg;
        } else {
            gen_cfg["thinkingConfig"] = json!({ "includeThoughts": true });
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

        let lower_model = self.config.model.to_ascii_lowercase();
        let is_modern_gemini = lower_model.contains("gemini-3") || lower_model.contains("gemini-4");
        let mut gen_cfg = json!({});
        if let Some(temp) = request.temperature {
            gen_cfg["temperature"] = json!(temp);
        } else if !is_modern_gemini {
            gen_cfg["temperature"] = json!(0.2);
        }
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
            cancel_flag: None,
        };
        let (raw_text, sse_usage, sync_usage, on_event) = post_json_sse_or_sync(
            &http_req,
            (
                String::new(),
                None::<String>,
                None::<AiTokenUsage>,
                None::<AiTokenUsage>,
                on_event,
            ),
            |(collected, finish_reason, sse_u, _, evt_cb), _event_name, payload| {
                process_gemini_streaming_payload(
                    payload,
                    collected,
                    finish_reason,
                    sse_u,
                    *evt_cb,
                )?;
                Ok(false)
            },
            |(collected, finish_reason, sse_u, sync_u, evt_cb)| {
                if finish_reason.is_none() {
                    return Err(AiError::ParseFailed(
                        "Gemini SSE stream terminated prematurely without finishReason".to_owned(),
                    ));
                }
                Ok((collected, sse_u, sync_u, evt_cb))
            },
            |(_, _, sse_u, mut sync_u, evt_cb), val| {
                let text = parse_gemini_sync_text_and_thoughts(val, &mut sync_u, evt_cb)?;
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
        self.send_agent_turn_cancellable(request, None, on_event)
    }

    fn send_agent_turn_cancellable(
        &self,
        request: &AiAgentTurnRequest,
        cancel_flag: Option<&Arc<AtomicBool>>,
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
            cancel_flag: cancel_flag.cloned(),
        };
        post_json_sse_or_sync(
            &http_req,
            (GeminiAgentStreamAcc::default(), on_event),
            |(acc, evt_cb), _event_name, payload| {
                process_gemini_agent_sse_payload(acc, payload, *evt_cb)?;
                Ok(false)
            },
            |(state, _)| {
                if state.finish_reason.is_none() {
                    return Err(AiError::ParseFailed(
                        "Gemini SSE stream terminated prematurely without finishReason".to_owned(),
                    ));
                }
                Ok(AiAgentTurnResponse {
                    text: (!state.text_buf.is_empty()).then_some(state.text_buf),
                    thinking_blocks: state.thinking_blocks,
                    tool_calls: state.tool_calls,
                    model: self.config.model.clone(),
                    finish_reason: state.finish_reason,
                    usage: state.usage,
                })
            },
            |(_, evt_cb), json_val| {
                let parsed = parse_gemini_agent_response_with_events(
                    json_val,
                    &self.config.model,
                    Some(evt_cb),
                )?;
                if let Some(txt) = parsed.text.as_ref().filter(|s| !s.is_empty()) {
                    evt_cb(AiStreamEvent::DeltaText(txt.clone()));
                }
                Ok(parsed)
            },
        )
    }
}

fn is_api_function_call_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with("gem_call_") && !id.starts_with("fallback_call_")
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
            text,
            thinking_blocks,
            tool_calls,
        } => {
            let mut gem_parts = Vec::new();
            let mut non_thought_sigs = Vec::new();
            for tb in thinking_blocks.iter().filter(|b| !b.signature.is_empty()) {
                if tb.thinking.is_empty() {
                    non_thought_sigs.push(tb.signature.as_str());
                } else {
                    gem_parts.push(json!({
                        "text": tb.thinking,
                        "thought": true,
                        "thoughtSignature": tb.signature,
                    }));
                }
            }
            let mut non_thought_iter = non_thought_sigs.into_iter();
            if let Some(txt) = text
                && !txt.is_empty()
            {
                let mut text_part = json!({ "text": txt });
                // functionCall の有無に関わらず非 thought のテキストパート署名を本体 text パートへ復元する
                if let Some(sig) = non_thought_iter.next() {
                    text_part["thoughtSignature"] = json!(sig);
                }
                gem_parts.push(text_part);
            }
            for sig in non_thought_iter {
                gem_parts.push(json!({
                    "text": "",
                    "thoughtSignature": sig,
                }));
            }
            for call in tool_calls {
                let mut fc = json!({
                    "name": call.name,
                    "args": call.arguments,
                });
                if is_api_function_call_id(&call.id) {
                    fc["id"] = json!(call.id);
                }
                let mut part = json!({ "functionCall": fc });
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
            tool_call_id,
            name,
            content_json,
            is_error,
        } => {
            let parsed = serde_json::from_str::<Value>(content_json)
                .unwrap_or_else(|_| Value::String(content_json.clone()));
            let payload = if *is_error {
                json!({ "error": parsed })
            } else {
                json!({ "result": parsed })
            };
            let mut fr = json!({
                "name": name,
                "response": payload,
            });
            if is_api_function_call_id(tool_call_id) {
                fr["id"] = json!(tool_call_id);
            }
            ("user", vec![json!({ "functionResponse": fr })])
        }
    }
}

#[derive(Debug, Default)]
struct GeminiAgentStreamAcc {
    text_buf: String,
    thinking_blocks: Vec<AiThinkingBlock>,
    tool_calls: Vec<AiToolCall>,
    finish_reason: Option<String>,
    usage: Option<AiTokenUsage>,
}

fn check_gemini_payload_error(val: &Value) -> Result<(), AiError> {
    if let Some(err_obj) = val.get("error").filter(|v| !v.is_null()) {
        let msg = err_obj["message"]
            .as_str()
            .unwrap_or("Gemini API returned error payload");
        return Err(AiError::ParseFailed(format!("Gemini error: {msg}")));
    }
    let has_candidates = val["candidates"]
        .as_array()
        .is_some_and(|arr| !arr.is_empty());
    if !has_candidates
        && let Some(reason) = val["promptFeedback"]["blockReason"]
            .as_str()
            .filter(|s| !s.is_empty())
    {
        return Err(AiError::ParseFailed(format!(
            "Gemini prompt blocked: {reason}"
        )));
    }
    Ok(())
}

fn process_gemini_streaming_payload(
    payload: &str,
    collected: &mut String,
    finish_reason: &mut Option<String>,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Gemini SSE JSON parse failed: {err}")))?;
    check_gemini_payload_error(&val)?;
    if let Some(usage) = extract_gemini_usage(&val) {
        *captured_usage = Some(usage);
    }
    if let Some(reason) = val["candidates"][0]["finishReason"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        *finish_reason = Some(reason.to_owned());
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
    check_gemini_payload_error(&val)?;
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
            let txt_opt = part["text"].as_str().filter(|s| !s.is_empty());
            if let Some(txt) = txt_opt {
                if is_thought {
                    on_event(AiStreamEvent::ThoughtDelta(txt.to_owned()));
                } else {
                    on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                    acc.text_buf.push_str(txt);
                }
            }
            let part_sig = part["thoughtSignature"]
                .as_str()
                .or_else(|| part["thought_signature"].as_str())
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            if let Some(fc) = part.get("functionCall").filter(|v| v.is_object())
                && let Some(name) = fc["name"].as_str()
            {
                let idx = acc.tool_calls.len();
                let call_id = fc["id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map_or_else(|| format!("gem_call_{idx}"), ToOwned::to_owned);
                acc.tool_calls.push(AiToolCall {
                    id: call_id,
                    name: name.to_owned(),
                    arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                    thought_signature: part_sig,
                });
            } else if let Some(sig) = part_sig {
                acc.thinking_blocks.push(AiThinkingBlock {
                    thinking: if is_thought {
                        txt_opt.unwrap_or("").to_owned()
                    } else {
                        String::new()
                    },
                    signature: sig,
                    redacted_data: None,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn parse_gemini_agent_response(val: &Value, model: &str) -> Result<AiAgentTurnResponse, AiError> {
    parse_gemini_agent_response_with_events(val, model, None)
}

fn parse_gemini_agent_response_with_events(
    val: &Value,
    model: &str,
    mut on_event: Option<&mut dyn FnMut(AiStreamEvent)>,
) -> Result<AiAgentTurnResponse, AiError> {
    check_gemini_payload_error(val)?;
    let candidate = &val["candidates"][0];
    let parts = candidate["content"]["parts"]
        .as_array()
        .ok_or_else(|| AiError::ParseFailed("Gemini agent response missing parts".to_owned()))?;
    let mut text_buf = String::new();
    let mut thinking_blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        let is_thought = part["thought"].as_bool() == Some(true);
        let txt_opt = part["text"].as_str();
        if let Some(txt) = txt_opt.filter(|s| !s.is_empty()) {
            if is_thought {
                if let Some(cb) = on_event.as_deref_mut() {
                    cb(AiStreamEvent::ThoughtDelta(txt.to_owned()));
                }
            } else {
                text_buf.push_str(txt);
            }
        }
        let part_sig = part["thoughtSignature"]
            .as_str()
            .or_else(|| part["thought_signature"].as_str())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        if let Some(fc) = part.get("functionCall").filter(|v| v.is_object())
            && let Some(name) = fc["name"].as_str()
        {
            let idx = tool_calls.len();
            let call_id = fc["id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map_or_else(|| format!("gem_call_{idx}"), ToOwned::to_owned);
            tool_calls.push(AiToolCall {
                id: call_id,
                name: name.to_owned(),
                arguments: fc.get("args").cloned().unwrap_or_else(|| json!({})),
                thought_signature: part_sig,
            });
        } else if let Some(sig) = part_sig {
            thinking_blocks.push(AiThinkingBlock {
                thinking: if is_thought {
                    txt_opt.unwrap_or("").to_owned()
                } else {
                    String::new()
                },
                signature: sig,
                redacted_data: None,
            });
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!text_buf.is_empty()).then_some(text_buf),
        thinking_blocks,
        tool_calls,
        model: model.to_owned(),
        finish_reason: candidate["finishReason"].as_str().map(ToOwned::to_owned),
        usage: extract_gemini_usage(val),
    })
}

fn parse_gemini_sync_text_and_thoughts(
    val: &Value,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError> {
    check_gemini_payload_error(val)?;
    if let Some(usage) = extract_gemini_usage(val) {
        *captured_usage = Some(usage);
    }
    if let Some(parts) = val["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if part["thought"].as_bool() == Some(true)
                && let Some(txt) = part["text"].as_str().filter(|s| !s.is_empty())
            {
                on_event(AiStreamEvent::ThoughtDelta(txt.to_owned()));
            }
        }
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
        assert!(cap.contains("streamGenerateContent?alt=sse"), "cap={cap}");
        let (_, json_body) = cap.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
        let req_json: Value = serde_json::from_str(json_body)?;
        assert_eq!(
            req_json["contents"][0]["parts"][0]["functionCall"]["id"],
            json!("prev_1")
        );
        assert_eq!(
            req_json["contents"][0]["parts"][0]["thoughtSignature"],
            json!("prev_sig_99")
        );
        assert_eq!(
            req_json["generationConfig"]["thinkingConfig"]["includeThoughts"],
            json!(true)
        );
        Ok(())
    }

    #[test]
    fn gemini_thinking_config_flash_clamp_non_fc_signature_and_call_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. gemini-2.5-flash Extra clamp to 24576 + includeThoughts
        let flash = GeminiProvider::new(
            GeminiConfig::new("gemini-2.5-flash", None).with_thinking_level(AiThinkingLevel::Extra),
        );
        let flash_body = flash.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "S".to_owned(),
            messages: vec![AiTurnMessage::user_text("hi")],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert_eq!(
            flash_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(24576)
        );
        assert_eq!(
            flash_body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            json!(true)
        );

        // 2. gemini-3 omits default temperature when None, sets includeThoughts, and non-thinking model omits thinkingConfig
        let g3 = GeminiProvider::new(GeminiConfig::new("gemini-3.0-pro", None));
        let g3_body = g3.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "S".to_owned(),
            messages: vec![AiTurnMessage::user_text("hi")],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert!(g3_body["generationConfig"].get("temperature").is_none());
        assert_eq!(
            g3_body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            json!(true)
        );
        let g15 = GeminiProvider::new(
            GeminiConfig::new("gemini-1.5-flash", None).with_thinking_level(AiThinkingLevel::High),
        );
        let g15_body = g15.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "S".to_owned(),
            messages: vec![AiTurnMessage::user_text("hi")],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert!(g15_body["generationConfig"].get("thinkingConfig").is_none());

        // 3. non-functionCall thoughtSignature & functionCall.id preservation + sync ThoughtDelta
        assert_gemini_sig_call_id_and_block_reason(&g3)?;
        Ok(())
    }

    fn assert_gemini_sig_call_id_and_block_reason(
        g3: &GeminiProvider,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sync_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "考え中", "thought": true },
                        { "text": "応答です", "thoughtSignature": "sig_text_only" },
                        { "functionCall": { "id": "api_fc_99", "name": "get_timeline_summary", "args": {} } }
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let mut thoughts = Vec::new();
        let parsed = parse_gemini_agent_response_with_events(
            &sync_resp,
            "gemini-3.0-pro",
            Some(&mut |evt| {
                if let AiStreamEvent::ThoughtDelta(t) = evt {
                    thoughts.push(t);
                }
            }),
        )?;
        assert_eq!(thoughts, vec!["考え中"]);
        assert_eq!(parsed.thinking_blocks.len(), 1);
        assert_eq!(parsed.thinking_blocks[0].signature, "sig_text_only");
        assert_eq!(parsed.tool_calls[0].id, "api_fc_99");

        let next_req = AiAgentTurnRequest {
            system_prompt: "S".to_owned(),
            messages: vec![
                AiTurnMessage::Assistant {
                    text: parsed.text,
                    thinking_blocks: parsed.thinking_blocks,
                    tool_calls: parsed.tool_calls,
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "api_fc_99".to_owned(),
                    name: "get_timeline_summary".to_owned(),
                    content_json: "{}".to_owned(),
                    is_error: false,
                },
            ],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        };
        let next_body = g3.build_agent_request_body(&next_req);
        let next_s = next_body.to_string();
        assert!(next_s.contains("sig_text_only"));
        assert!(next_s.contains("api_fc_99"));

        let mut acc = GeminiAgentStreamAcc::default();
        assert!(
            process_gemini_agent_sse_payload(
                &mut acc,
                r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#,
                &mut |_| {},
            )
            .is_err()
        );

        // #02 & #03: thought パート先頭配置・非 thought 署名 text_part 優先配置・自前 ID (gem_call_*/fallback_call_*) 非送信
        let (_role, mixed_parts) = gemini_turn_message_to_parts(&AiTurnMessage::Assistant {
            text: Some("呼び出します".to_owned()),
            thinking_blocks: vec![
                AiThinkingBlock {
                    thinking: String::new(),
                    signature: "sig_text_mixed".to_owned(),
                    redacted_data: None,
                },
                AiThinkingBlock {
                    thinking: "まず考える".to_owned(),
                    signature: "sig_thought_first".to_owned(),
                    redacted_data: None,
                },
            ],
            tool_calls: vec![AiToolCall {
                id: "gem_call_0".to_owned(),
                name: "get_timeline_summary".to_owned(),
                arguments: json!({}),
                thought_signature: Some("sig_fc_0".to_owned()),
            }],
        });
        assert_eq!(mixed_parts.len(), 3);
        assert_eq!(mixed_parts[0]["thought"], json!(true));
        assert_eq!(
            mixed_parts[0]["thoughtSignature"],
            json!("sig_thought_first")
        );
        assert_eq!(mixed_parts[1]["text"], json!("呼び出します"));
        assert_eq!(mixed_parts[1]["thoughtSignature"], json!("sig_text_mixed"));
        assert!(mixed_parts[2]["functionCall"].get("id").is_none());

        let (_role_fr, fr_parts) = gemini_turn_message_to_parts(&AiTurnMessage::ToolResult {
            tool_call_id: "gem_call_0".to_owned(),
            name: "get_timeline_summary".to_owned(),
            content_json: "{}".to_owned(),
            is_error: false,
        });
        assert!(fr_parts[0]["functionResponse"].get("id").is_none());
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
