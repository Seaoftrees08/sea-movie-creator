//! OpenAI・OpenRouter・Ollama・LM Studio・vLLM 共通 HTTP/SSE クライアント。

use std::collections::BTreeMap;
use std::sync::{Arc, atomic::AtomicBool};

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingLevel, AiTokenUsage, AiToolCall,
    AiTurnMessage, SecretString, format_tools_fallback_instructions, parse_fallback_agent_response,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{
    HttpGetRequest, HttpJsonRequest, get_json_sync, post_json_sse_or_sync_with_eof,
};

const MAX_OPENAI_STREAM_TOOL_CALLS: usize = 256;

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
            if stream {
                body["stream_options"] = json!({ "include_usage": true });
            }
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
        let stream = self.config.use_streaming;
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": stream,
        });
        if stream
            && matches!(
                self.config.kind,
                AiProviderKind::OpenAi | AiProviderKind::OpenRouter
            )
        {
            body["stream_options"] = json!({ "include_usage": true });
        }
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
            cancel_flag: None,
        };

        let (raw_text, sse_usage, sync_usage, on_event) = post_json_sse_or_sync_with_eof(
            &http_req,
            (
                String::new(),
                None::<String>,
                None::<AiTokenUsage>,
                None::<AiTokenUsage>,
                on_event,
            ),
            |(collected, finish_reason, sse_u, _, evt_cb), _event_name, payload| {
                process_openai_streaming_sse_payload(
                    payload,
                    collected,
                    finish_reason,
                    sse_u,
                    *evt_cb,
                )?;
                Ok(false)
            },
            |(collected, finish_reason, sse_u, sync_u, evt_cb), saw_done| {
                if !saw_done && finish_reason.is_none() {
                    return Err(AiError::ParseFailed(
                        "OpenAI SSE stream terminated prematurely without [DONE] or finish_reason"
                            .to_owned(),
                    ));
                }
                Ok((collected, sse_u, sync_u, evt_cb))
            },
            |(_, _, sse_u, mut sync_u, evt_cb), val| {
                let text = parse_openai_sync_json_and_thoughts(val, &mut sync_u, evt_cb)?;
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
            cancel_flag: cancel_flag.cloned(),
        };
        let native_tools = self.supports_native_tools();
        post_json_sse_or_sync_with_eof(
            &http_req,
            (OpenAiAgentStreamAcc::default(), on_event),
            |(acc, evt_cb), _event_name, payload| {
                process_openai_agent_sse_chunk(acc, payload, *evt_cb)?;
                Ok(false)
            },
            |(acc, _), saw_done| {
                finalize_openai_agent_stream(acc, saw_done, native_tools, &self.config.model)
            },
            |(_, evt_cb), val| {
                let parsed = if native_tools {
                    parse_openai_agent_response_with_events(val, &self.config.model, Some(evt_cb))?
                } else {
                    let mut captured_usage: Option<AiTokenUsage> = None;
                    let text =
                        parse_openai_sync_json_and_thoughts(val, &mut captured_usage, evt_cb)?;
                    let mut p = parse_fallback_agent_response(&text, &self.config.model);
                    p.usage = captured_usage;
                    if p.tool_calls.is_empty()
                        && let Some(reason) = val["choices"][0]["finish_reason"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                    {
                        p.finish_reason = Some(reason.to_owned());
                    }
                    p
                };
                if let Some(txt) = parsed.text.as_ref().filter(|s| !s.is_empty()) {
                    evt_cb(AiStreamEvent::DeltaText(txt.clone()));
                }
                Ok(parsed)
            },
        )
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
        AiTurnMessage::Assistant {
            text, tool_calls, ..
        } => {
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

fn check_openai_payload_error(val: &Value) -> Result<(), AiError> {
    if let Some(err_obj) = val.get("error").filter(|v| !v.is_null()) {
        let msg = err_obj["message"]
            .as_str()
            .unwrap_or("OpenAI compatible API returned error payload");
        return Err(AiError::ParseFailed(format!("OpenAI error: {msg}")));
    }
    Ok(())
}

fn parse_openai_agent_response_with_events(
    val: &Value,
    model: &str,
    on_event: Option<&mut dyn FnMut(AiStreamEvent)>,
) -> Result<AiAgentTurnResponse, AiError> {
    check_openai_payload_error(val)?;
    let choice = &val["choices"][0];
    let message = choice
        .get("message")
        .filter(|v| v.is_object())
        .ok_or_else(|| {
            AiError::ParseFailed("OpenAI response missing choices[0].message".to_owned())
        })?;
    if let Some(reasoning) = message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str())
        .filter(|s| !s.is_empty())
        && let Some(cb) = on_event
    {
        cb(AiStreamEvent::ThoughtDelta(reasoning.to_owned()));
    }
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
                    Value::String(raw) if raw.trim().is_empty() => json!({}),
                    Value::String(raw) => serde_json::from_str::<Value>(raw).map_err(|err| {
                        AiError::ParseFailed(format!(
                            "OpenAI tool_calls arguments JSON parse failed for `{name}`: {err}"
                        ))
                    })?,
                    other if !other.is_null() => other.clone(),
                    _ => json!({}),
                };
                tool_calls.push(AiToolCall {
                    id,
                    name: name.to_owned(),
                    arguments: args,
                    thought_signature: None,
                });
            }
        }
    }

    Ok(AiAgentTurnResponse {
        text,
        thinking_blocks: Vec::new(),
        tool_calls,
        model: model.to_owned(),
        finish_reason: choice["finish_reason"].as_str().map(ToOwned::to_owned),
        usage: extract_openai_usage(val),
    })
}

#[derive(Debug, Default)]
struct PartialOpenAiToolCall {
    id: String,
    name: String,
    arguments_raw: String,
}

#[derive(Debug, Default)]
struct OpenAiAgentStreamAcc {
    text_buf: String,
    tool_calls: BTreeMap<usize, PartialOpenAiToolCall>,
    finish_reason: Option<String>,
    usage: Option<AiTokenUsage>,
}

fn merge_openai_usage(acc: &mut Option<AiTokenUsage>, incoming: &AiTokenUsage) {
    let target = acc.get_or_insert_with(AiTokenUsage::default);
    if incoming.input_tokens > 0 {
        target.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens > 0 {
        target.output_tokens = incoming.output_tokens;
    }
    let summed = target.input_tokens.saturating_add(target.output_tokens);
    target.total_tokens = incoming.total_tokens.max(summed);
    if incoming.cost_usd.is_some() {
        target.cost_usd = incoming.cost_usd;
    }
    target.is_estimated = false;
}

fn process_openai_streaming_sse_payload(
    payload: &str,
    collected: &mut String,
    finish_reason: &mut Option<String>,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("OpenAI SSE JSON parse failed: {err}")))?;
    check_openai_payload_error(&val)?;
    if let Some(usage) = extract_openai_usage(&val) {
        merge_openai_usage(captured_usage, &usage);
    }
    let choice = &val["choices"][0];
    if let Some(reason) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
        *finish_reason = Some(reason.to_owned());
    }
    let delta = &choice["delta"];
    if let Some(reasoning) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .filter(|s| !s.is_empty())
    {
        on_event(AiStreamEvent::ThoughtDelta(reasoning.to_owned()));
    }
    if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
        on_event(AiStreamEvent::DeltaText(content.to_owned()));
        collected.push_str(content);
    }
    Ok(())
}

fn process_openai_agent_sse_chunk(
    acc: &mut OpenAiAgentStreamAcc,
    payload: &str,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("OpenAI agent SSE parse failed: {err}")))?;
    check_openai_payload_error(&val)?;
    if let Some(usage) = extract_openai_usage(&val) {
        merge_openai_usage(&mut acc.usage, &usage);
    }
    let choice = &val["choices"][0];
    if let Some(reason) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
        acc.finish_reason = Some(reason.to_owned());
    }
    let delta = &choice["delta"];
    if let Some(reasoning) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .filter(|s| !s.is_empty())
    {
        on_event(AiStreamEvent::ThoughtDelta(reasoning.to_owned()));
    }
    if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
        on_event(AiStreamEvent::DeltaText(content.to_owned()));
        acc.text_buf.push_str(content);
    }
    if let Some(arr) = delta["tool_calls"].as_array() {
        for tc in arr {
            let incoming_id = tc["id"].as_str().filter(|s| !s.is_empty());
            let incoming_name = tc["function"]["name"].as_str().filter(|s| !s.is_empty());
            let idx = resolve_openai_stream_tool_index(acc, tc, incoming_id, incoming_name)?;
            if !acc.tool_calls.contains_key(&idx)
                && acc.tool_calls.len() >= MAX_OPENAI_STREAM_TOOL_CALLS
            {
                return Err(AiError::ParseFailed(format!(
                    "OpenAI SSE stream exceeded max tool calls ({MAX_OPENAI_STREAM_TOOL_CALLS})"
                )));
            }
            let entry = acc.tool_calls.entry(idx).or_default();
            let same_id_repeat =
                incoming_id.is_some_and(|id| !entry.id.is_empty() && entry.id == id);
            if let Some(id) = incoming_id {
                id.clone_into(&mut entry.id);
            }
            if let Some(name) = incoming_name
                && !(same_id_repeat && entry.name == name)
            {
                entry.name.push_str(name);
            }
            match &tc["function"]["arguments"] {
                Value::String(args) => entry.arguments_raw.push_str(args),
                other if !other.is_null() => entry.arguments_raw.push_str(&other.to_string()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn resolve_openai_stream_tool_index(
    acc: &OpenAiAgentStreamAcc,
    tc: &Value,
    incoming_id: Option<&str>,
    incoming_name: Option<&str>,
) -> Result<usize, AiError> {
    if let Some(explicit) = tc["index"].as_u64().and_then(|n| usize::try_from(n).ok()) {
        return Ok(explicit);
    }
    if let Some(existing_idx) = incoming_id.and_then(|id| {
        acc.tool_calls
            .iter()
            .rev()
            .find_map(|(&k, e)| (e.id == id).then_some(k))
    }) {
        return Ok(existing_idx);
    }
    if incoming_id.is_some() || incoming_name.is_some() {
        return match acc.tool_calls.keys().next_back() {
            Some(&last) => last.checked_add(1).ok_or_else(|| {
                AiError::ParseFailed("OpenAI SSE tool_calls index overflow".to_owned())
            }),
            None => Ok(0),
        };
    }
    Ok(acc.tool_calls.keys().next_back().copied().unwrap_or(0))
}

fn finalize_openai_agent_stream(
    acc: OpenAiAgentStreamAcc,
    saw_done: bool,
    native_tools: bool,
    model: &str,
) -> Result<AiAgentTurnResponse, AiError> {
    if !saw_done && acc.finish_reason.is_none() {
        return Err(AiError::ParseFailed(
            "OpenAI SSE stream terminated prematurely without [DONE] or finish_reason".to_owned(),
        ));
    }
    let mut tool_calls = Vec::new();
    for (idx, partial) in acc.tool_calls {
        if partial.name.trim().is_empty() {
            continue;
        }
        let id = if partial.id.is_empty() {
            format!("oai_call_{idx}")
        } else {
            partial.id
        };
        let trimmed = partial.arguments_raw.trim();
        let arguments = if trimmed.is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(trimmed).map_err(|err| {
                AiError::ParseFailed(format!(
                    "OpenAI streaming tool_calls arguments JSON parse failed for `{}`: {err}",
                    partial.name
                ))
            })?
        };
        tool_calls.push(AiToolCall {
            id,
            name: partial.name,
            arguments,
            thought_signature: None,
        });
    }
    if !native_tools && tool_calls.is_empty() && !acc.text_buf.trim().is_empty() {
        let mut parsed = parse_fallback_agent_response(&acc.text_buf, model);
        parsed.usage = acc.usage;
        if parsed.tool_calls.is_empty() && acc.finish_reason.is_some() {
            parsed.finish_reason = acc.finish_reason;
        }
        return Ok(parsed);
    }
    Ok(AiAgentTurnResponse {
        text: (!acc.text_buf.is_empty()).then_some(acc.text_buf),
        thinking_blocks: Vec::new(),
        tool_calls,
        model: model.to_owned(),
        finish_reason: acc.finish_reason,
        usage: acc.usage,
    })
}

fn parse_openai_sync_json_and_thoughts(
    val: &Value,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError> {
    check_openai_payload_error(val)?;
    if let Some(usage) = extract_openai_usage(val) {
        merge_openai_usage(captured_usage, &usage);
    }
    let msg = &val["choices"][0]["message"];
    if let Some(reasoning) = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str())
        .filter(|s| !s.is_empty())
    {
        on_event(AiStreamEvent::ThoughtDelta(reasoning.to_owned()));
    }
    msg["content"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AiError::ParseFailed("OpenAI response missing choices[0].message.content".to_owned())
        })
}

fn extract_openai_usage(val: &Value) -> Option<AiTokenUsage> {
    let usage = val.get("usage")?.as_object()?;
    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    let cost_usd = usage
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && *c >= 0.0);
    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 && cost_usd.is_none() {
        return None;
    }
    Some(AiTokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cost_usd,
        is_estimated: false,
    })
}

/// `OpenRouter` `GET /credits` API から取得するクレジット残高スナップショット。
#[derive(Debug, Clone, PartialEq)]
pub struct OpenRouterCreditsSnapshot {
    /// 累計購入・付与クレジット（USD）。
    pub total_credits_usd: f64,
    /// 累計消費クレジット（USD）。
    pub total_usage_usd: f64,
    /// 残クレジット額（USD）。
    pub remaining_usd: f64,
    /// 残割合（0.0..=100.0、`total_credits_usd <= 0` 時は `None`）。
    pub remaining_percent: Option<f64>,
}

/// `OpenRouter` `GET /credits` 応答 JSON を [`OpenRouterCreditsSnapshot`] へ変換する。
///
/// # Errors
///
/// `data.total_credits` または `data.total_usage` が数値として読み取れない場合。
pub fn parse_openrouter_credits_json(val: &Value) -> Result<OpenRouterCreditsSnapshot, AiError> {
    let data = val.get("data").unwrap_or(val);
    let total_credits_usd = data
        .get("total_credits")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            AiError::ParseFailed(
                "OpenRouter /credits response missing data.total_credits".to_owned(),
            )
        })?;
    let total_usage_usd = data
        .get("total_usage")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            AiError::ParseFailed("OpenRouter /credits response missing data.total_usage".to_owned())
        })?;
    let remaining_usd = (total_credits_usd - total_usage_usd).max(0.0);
    let remaining_percent = if total_credits_usd > 0.0 {
        Some(((remaining_usd / total_credits_usd) * 100.0).clamp(0.0, 100.0))
    } else {
        None
    };
    Ok(OpenRouterCreditsSnapshot {
        total_credits_usd,
        total_usage_usd,
        remaining_usd,
        remaining_percent,
    })
}

/// `OpenRouter` の `GET {base_url}/credits` を呼び出して残クレジットを取得する。
///
/// # Errors
///
/// 通信失敗・認証/HTTP エラー・JSON 解析エラー時に [`AiError`] を返す。
pub fn fetch_openrouter_credits(
    base_url: &str,
    api_key: &SecretString,
    timeout_secs: u64,
) -> Result<OpenRouterCreditsSnapshot, AiError> {
    let base = base_url.trim().trim_end_matches('/');
    let url = format!("{base}/credits");
    let auth = format!("Bearer {}", api_key.expose());
    let headers = [
        ("Authorization", auth.as_str()),
        ("HTTP-Referer", "https://github.com/sea-movie-creator"),
        ("X-Title", "sea-movie-creator"),
    ];
    let val = get_json_sync(&HttpGetRequest {
        url: &url,
        headers: &headers,
        timeout_secs,
    })?;
    parse_openrouter_credits_json(&val)
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
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":45,\"total_tokens\":165,\"cost\":0.0025}}\n\n",
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
        let usage = reply.usage.as_ref().ok_or("expected SSE usage")?;
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.total_tokens, 165);
        assert!(usage.cost_usd.is_some_and(|c| (c - 0.0025).abs() < 1e-8));
        assert!(!usage.is_estimated);

        let captured_req = server.join().map_err(|_| "server join failed")??;
        let captured_lower = captured_req.to_ascii_lowercase();
        assert!(captured_lower.contains("authorization: bearer or-secret-test"));
        assert!(captured_lower.contains("x-title: sea-movie-creator"));
        assert!(captured_lower.contains("include_usage"));

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
    fn openrouter_credits_fetch_and_parse_work_with_mock_server()
    -> Result<(), Box<dyn std::error::Error>> {
        let credits_body = r#"{"data":{"total_credits":10.0,"total_usage":2.5}}"#;
        let (addr, server) = spawn_single_response_server("application/json", credits_body)?;
        let snap = fetch_openrouter_credits(
            &format!("http://{addr}/api/v1"),
            &SecretString::new("demo-or-key"),
            5,
        )?;
        assert!((snap.total_credits_usd - 10.0).abs() < 1e-6);
        assert!((snap.total_usage_usd - 2.5).abs() < 1e-6);
        assert!((snap.remaining_usd - 7.5).abs() < 1e-6);
        assert!(
            snap.remaining_percent
                .is_some_and(|p| (p - 75.0).abs() < 1e-6)
        );

        let req = server.join().map_err(|_| "server join")??;
        let lower = req.to_ascii_lowercase();
        assert!(lower.starts_with("get /api/v1/credits"));
        assert!(lower.contains("authorization: bearer demo-or-key"));
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
    fn openai_agent_turn_streams_reasoning_and_chunked_tool_calls()
    -> Result<(), Box<dyn std::error::Error>> {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"考える...\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"実行します。\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_stream_1\",\"function\":{\"name\":\"apply_draft_\",\"arguments\":\"{\\\"k\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"edit_command\",\"arguments\":\"123}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":12,\"total_tokens\":42}}\n\n",
            "data: [DONE]\n\n"
        );
        let (addr, srv) = spawn_single_response_server("text/event-stream", sse)?;
        let mut cfg = OpenAiCompatConfig::openai("gpt-4.1-mini", Some(SecretString::new("k")));
        cfg.base_url = format!("http://{addr}/v1");
        let provider = OpenAiCompatProvider::new(cfg);

        let turn_req = AiAgentTurnRequest {
            system_prompt: "Plan".to_owned(),
            messages: vec![AiTurnMessage::User {
                parts: vec![AiContentPart::text("修正して")],
            }],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "apply_draft_edit_command".to_owned(),
                description: "edit".to_owned(),
                parameters_schema: json!({ "type": "object" }),
            }],
            response_json_schema: None,
            temperature: None,
        };
        let mut thoughts = Vec::new();
        let mut texts = Vec::new();
        let res = provider.send_agent_turn(&turn_req, &mut |evt| match evt {
            AiStreamEvent::ThoughtDelta(t) => thoughts.push(t),
            AiStreamEvent::DeltaText(t) => texts.push(t),
            _ => {}
        })?;
        assert_eq!(thoughts, vec!["考える..."]);
        assert_eq!(texts, vec!["実行します。"]);
        assert_eq!(res.text.as_deref(), Some("実行します。"));
        assert_eq!(res.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].id, "call_stream_1");
        assert_eq!(res.tool_calls[0].name, "apply_draft_edit_command");
        assert_eq!(res.tool_calls[0].arguments["k"], json!(123));
        assert_eq!(res.usage.as_ref().map(|u| u.total_tokens), Some(42));
        let cap = srv.join().map_err(|_| "srv join")??;
        let (_, json_body) = cap.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
        let parsed_body: Value = serde_json::from_str(json_body)?;
        assert_eq!(parsed_body["stream"], json!(true));
        assert_eq!(parsed_body["stream_options"]["include_usage"], json!(true));
        Ok(())
    }

    #[test]
    fn openai_stream_preserves_length_finish_rejects_broken_json_and_handles_no_index_and_sync_thoughts()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. native_tools = false のフォールバックでツール無し時は Acc の finish_reason("length") を保持
        let acc_len = OpenAiAgentStreamAcc {
            text_buf: "途中の応答テキスト".to_owned(),
            tool_calls: BTreeMap::new(),
            finish_reason: Some("length".to_owned()),
            usage: None,
        };
        let res_len = finalize_openai_agent_stream(acc_len, true, false, "ollama:qwen3")?;
        assert_eq!(res_len.finish_reason.as_deref(), Some("length"));

        // 2. streaming tool_calls の不正 JSON は空 {} に握り潰さず ParseFailed にすること
        let mut bad_calls = BTreeMap::new();
        bad_calls.insert(
            0,
            PartialOpenAiToolCall {
                id: "c1".to_owned(),
                name: "apply_draft_edit_command".to_owned(),
                arguments_raw: "{\"broken\":".to_owned(),
            },
        );
        let acc_bad_json = OpenAiAgentStreamAcc {
            text_buf: String::new(),
            tool_calls: bad_calls,
            finish_reason: Some("tool_calls".to_owned()),
            usage: None,
        };
        assert!(finalize_openai_agent_stream(acc_bad_json, true, true, "gpt-4o").is_err());

        // 3. index 欠損＋オブジェクト arguments でも継続結合できること
        let mut acc_no_idx = OpenAiAgentStreamAcc::default();
        process_openai_agent_sse_chunk(
            &mut acc_no_idx,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"no_idx_1","function":{"name":"apply_","arguments":"{\"a\":"}}]}}]}"#,
            &mut |_| {},
        )?;
        process_openai_agent_sse_chunk(
            &mut acc_no_idx,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}"#,
            &mut |_| {},
        )?;
        process_openai_agent_sse_chunk(
            &mut acc_no_idx,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"no_idx_2","function":{"name":"noop","arguments":{"ok":true}}}]}}]}"#,
            &mut |_| {},
        )?;
        let merged = finalize_openai_agent_stream(acc_no_idx, true, true, "gpt-4o")?;
        assert_eq!(merged.tool_calls.len(), 2);
        assert_eq!(merged.tool_calls[0].id, "no_idx_1");
        assert_eq!(merged.tool_calls[0].arguments["a"], json!(1));
        assert_eq!(merged.tool_calls[1].arguments["ok"], json!(true));

        // 4. ストリーム内 {"error": ...} と premature EOF を拒否すること
        let mut err_acc = OpenAiAgentStreamAcc::default();
        assert!(
            process_openai_agent_sse_chunk(
                &mut err_acc,
                r#"{"error":{"message":"upstream overloaded"}}"#,
                &mut |_| {},
            )
            .is_err()
        );
        assert!(
            finalize_openai_agent_stream(OpenAiAgentStreamAcc::default(), false, true, "gpt-4o")
                .is_err()
        );

        // 5. sync JSON 応答でも reasoning_content から ThoughtDelta を送出すること
        let sync_val = json!({
            "choices": [{
                "message": {
                    "reasoning_content": "sync推論",
                    "content": "完了"
                },
                "finish_reason": "stop"
            }]
        });
        let mut thoughts = Vec::new();
        let parsed = parse_openai_agent_response_with_events(
            &sync_val,
            "deepseek-r1",
            Some(&mut |evt| {
                if let AiStreamEvent::ThoughtDelta(t) = evt {
                    thoughts.push(t);
                }
            }),
        )?;
        assert_eq!(thoughts, vec!["sync推論"]);
        assert_eq!(parsed.text.as_deref(), Some("完了"));
        Ok(())
    }

    #[test]
    fn openai_empty_args_usage_merge_and_repeated_id_no_index_work()
    -> Result<(), Box<dyn std::error::Error>> {
        // 空引数 `"arguments": ""` の sync tool_calls、usage マージ、index 欠損 + 同一 id 再送を正しく処理すること
        let empty_arg_sync = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_empty",
                        "type": "function",
                        "function": { "name": "get_timeline_summary", "arguments": "" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let empty_parsed =
            parse_openai_agent_response_with_events(&empty_arg_sync, "gpt-4o", None)?;
        assert_eq!(empty_parsed.tool_calls[0].arguments, json!({}));

        let mut usage_acc = None;
        merge_openai_usage(
            &mut usage_acc,
            &AiTokenUsage {
                input_tokens: 100,
                output_tokens: 0,
                total_tokens: 100,
                ..AiTokenUsage::default()
            },
        );
        merge_openai_usage(
            &mut usage_acc,
            &AiTokenUsage {
                input_tokens: 0,
                output_tokens: 40,
                total_tokens: 40,
                ..AiTokenUsage::default()
            },
        );
        assert_eq!(usage_acc.as_ref().map(|u| u.total_tokens), Some(140));

        let mut same_id_acc = OpenAiAgentStreamAcc::default();
        process_openai_agent_sse_chunk(
            &mut same_id_acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"rep_1","function":{"name":"noop","arguments":"{\"x\":"}}]}}]}"#,
            &mut |_| {},
        )?;
        process_openai_agent_sse_chunk(
            &mut same_id_acc,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"rep_1","function":{"name":"noop","arguments":"1}"}}]},"finish_reason":"tool_calls"}]}"#,
            &mut |_| {},
        )?;
        let same_id_merged = finalize_openai_agent_stream(same_id_acc, true, true, "gpt-4o")?;
        assert_eq!(same_id_merged.tool_calls.len(), 1);
        assert_eq!(same_id_merged.tool_calls[0].name, "noop");
        assert_eq!(same_id_merged.tool_calls[0].arguments["x"], json!(1));
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
