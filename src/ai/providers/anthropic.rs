//! Anthropic Messages API クライアント。

use std::collections::BTreeMap;
use std::sync::{Arc, atomic::AtomicBool};

use serde_json::{Value, json};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiError, AiProvider, AiProviderKind,
    AiRequest, AiResponse, AiRole, AiStreamEvent, AiThinkingBlock, AiThinkingLevel, AiTokenUsage,
    AiToolCall, AiTurnMessage, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_sse_or_sync};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const MAX_ANTHROPIC_STREAM_BLOCKS: usize = 256;

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
        if request.tools.is_empty()
            && let Some(schema) = &request.response_json_schema
        {
            system_text.push_str("\n\nRespond ONLY with JSON matching this schema:\n");
            system_text.push_str(&schema.to_string());
        }

        let mut messages: Vec<Value> = Vec::new();
        for turn in &request.messages {
            if let AiTurnMessage::System { text } = turn {
                if !text.is_empty() {
                    system_text.push_str("\n\n");
                    system_text.push_str(text);
                }
                continue;
            }
            let (role, mut blocks) = anthropic_turn_message_to_blocks(turn);
            if blocks.is_empty() {
                continue;
            }
            if let Some(last) = messages.last_mut()
                && last["role"].as_str() == Some(role)
                && let Some(existing) = last["content"].as_array_mut()
            {
                existing.append(&mut blocks);
                continue;
            }
            messages.push(json!({
                "role": role,
                "content": blocks,
            }));
        }

        // 過去の tool_use 付き Assistant メッセージに有効 thinking 署名が欠落している場合や、
        // マージ後に先頭連続 thinking 制約／非 assistant 終端を満たせない場合は thinking を抑止して HTTP 400 を防ぐ。
        let missing_history_thinking_signature = request.messages.iter().any(|turn| {
            matches!(
                turn,
                AiTurnMessage::Assistant {
                    thinking_blocks,
                    tool_calls,
                    ..
                } if !tool_calls.is_empty()
                    && !thinking_blocks.iter().any(AiThinkingBlock::is_valid_anthropic)
            )
        });
        let violates_anthropic_thinking_layout =
            !messages_satisfy_anthropic_thinking_rules(&messages);
        let budget = (!missing_history_thinking_signature && !violates_anthropic_thinking_layout)
            .then(|| self.config.thinking_level.anthropic_budget_tokens())
            .flatten();
        let max_tokens = match budget {
            Some(b) => self.config.max_tokens.max(b.saturating_add(4096)),
            None => self.config.max_tokens,
        };
        let mut body = json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "system": system_text,
            "messages": messages,
            "stream": self.config.use_streaming,
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
        } else if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
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
            cancel_flag: None,
        };
        let (raw_text, sse_usage, sync_usage, on_event) = post_json_sse_or_sync(
            &http_req,
            (
                String::new(),
                false,
                None::<AiTokenUsage>,
                None::<AiTokenUsage>,
                on_event,
            ),
            |(collected, saw_terminal, sse_u, _, evt_cb), _event_name, payload| {
                process_anthropic_streaming_sse_payload(
                    payload,
                    collected,
                    saw_terminal,
                    sse_u,
                    *evt_cb,
                )?;
                Ok(false)
            },
            |(collected, saw_terminal, sse_u, sync_u, evt_cb)| {
                if !saw_terminal {
                    return Err(AiError::ParseFailed(
                        "Anthropic SSE stream terminated prematurely before message_delta/message_stop"
                            .to_owned(),
                    ));
                }
                Ok((collected, sse_u, sync_u, evt_cb))
            },
            |(_, _, sse_u, mut sync_u, evt_cb), val| {
                let text = parse_anthropic_sync_text_and_thoughts(val, &mut sync_u, evt_cb)?;
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
            cancel_flag: cancel_flag.cloned(),
        };
        post_json_sse_or_sync(
            &http_req,
            (AnthropicAgentStreamAcc::default(), on_event),
            |(acc, evt_cb), _event_name, payload| {
                process_anthropic_agent_sse_payload(acc, payload, *evt_cb)?;
                Ok(false)
            },
            |(state, _)| finalize_anthropic_agent_stream(state, &self.config.model),
            |(_, evt_cb), val| {
                let parsed = parse_anthropic_agent_response(val, &self.config.model)?;
                for tb in &parsed.thinking_blocks {
                    if !tb.thinking.is_empty() {
                        evt_cb(AiStreamEvent::ThoughtDelta(tb.thinking.clone()));
                    }
                }
                if let Some(txt) = parsed.text.as_ref().filter(|s| !s.is_empty()) {
                    evt_cb(AiStreamEvent::DeltaText(txt.clone()));
                }
                Ok(parsed)
            },
        )
    }
}

fn messages_satisfy_anthropic_thinking_rules(messages: &[Value]) -> bool {
    // 末尾が assistant（prefill）の場合は thinking と非互換
    if messages
        .last()
        .is_some_and(|m| m["role"].as_str() == Some("assistant"))
    {
        return false;
    }
    for msg in messages {
        if msg["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg["content"].as_array() else {
            continue;
        };
        let has_tool_use = blocks
            .iter()
            .any(|b| b["type"].as_str() == Some("tool_use"));
        if !has_tool_use {
            continue;
        }
        let mut seen_non_thinking = false;
        let mut has_leading_thinking = false;
        for b in blocks {
            let is_thinking = matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"));
            if is_thinking {
                if seen_non_thinking {
                    return false;
                }
                has_leading_thinking = true;
            } else {
                seen_non_thinking = true;
            }
        }
        if !has_leading_thinking {
            return false;
        }
    }
    true
}

fn anthropic_turn_message_to_blocks(msg: &AiTurnMessage) -> (&'static str, Vec<Value>) {
    match msg {
        AiTurnMessage::System { text } => ("user", vec![json!({ "type": "text", "text": text })]),
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
            ("user", content)
        }
        AiTurnMessage::Assistant {
            text,
            thinking_blocks,
            tool_calls,
        } => {
            let mut content = Vec::new();
            for tb in thinking_blocks {
                if let Some(data) = &tb.redacted_data {
                    content.push(json!({
                        "type": "redacted_thinking",
                        "data": data,
                    }));
                } else if !tb.signature.is_empty() {
                    content.push(json!({
                        "type": "thinking",
                        "thinking": tb.thinking,
                        "signature": tb.signature,
                    }));
                }
            }
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
            ("assistant", content)
        }
        AiTurnMessage::ToolResult {
            tool_call_id,
            content_json,
            is_error,
            ..
        } => (
            "user",
            vec![json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": content_json,
                "is_error": is_error,
            })],
        ),
    }
}

#[derive(Debug)]
enum AnthropicStreamBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Text,
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
        initial_input: Option<Value>,
    },
}

#[derive(Debug, Default)]
struct AnthropicAgentStreamAcc {
    text_buf: String,
    blocks: BTreeMap<usize, AnthropicStreamBlock>,
    finish_reason: Option<String>,
    received_message_stop: bool,
    usage: Option<AiTokenUsage>,
}

fn process_anthropic_streaming_sse_payload(
    payload: &str,
    collected: &mut String,
    saw_terminal: &mut bool,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Anthropic SSE JSON parse failed: {err}")))?;
    if val["type"].as_str() == Some("error") {
        let msg = val["error"]["message"]
            .as_str()
            .unwrap_or("Anthropic SSE error");
        return Err(AiError::ParseFailed(format!("Anthropic SSE error: {msg}")));
    }
    if val["type"].as_str() == Some("message_start")
        && let Some(msg_usage) = extract_anthropic_usage_obj(val["message"].get("usage"))
    {
        merge_anthropic_usage(captured_usage, &msg_usage);
    } else if val["type"].as_str() == Some("message_delta") {
        *saw_terminal = true;
        if let Some(delta_usage) = extract_anthropic_usage_obj(val.get("usage")) {
            merge_anthropic_usage(captured_usage, &delta_usage);
        }
    } else if val["type"].as_str() == Some("message_stop") {
        *saw_terminal = true;
    }
    if val["type"].as_str() == Some("content_block_start") {
        let cb = &val["content_block"];
        match cb["type"].as_str() {
            Some("thinking") => {
                if let Some(thinking) = cb["thinking"].as_str().filter(|s| !s.is_empty()) {
                    on_event(AiStreamEvent::ThoughtDelta(thinking.to_owned()));
                }
            }
            Some("text") => {
                if let Some(txt) = cb["text"].as_str().filter(|s| !s.is_empty()) {
                    on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                    collected.push_str(txt);
                }
            }
            _ => {}
        }
    } else if val["type"].as_str() == Some("content_block_delta") {
        match val["delta"]["type"].as_str() {
            Some("thinking_delta") => {
                if let Some(thinking) = val["delta"]["thinking"].as_str().filter(|s| !s.is_empty())
                {
                    on_event(AiStreamEvent::ThoughtDelta(thinking.to_owned()));
                }
            }
            Some("text_delta") => {
                if let Some(txt) = val["delta"]["text"].as_str().filter(|s| !s.is_empty()) {
                    on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                    collected.push_str(txt);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn process_anthropic_agent_sse_payload(
    acc: &mut AnthropicAgentStreamAcc,
    payload: &str,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let val: Value = serde_json::from_str(payload).map_err(|err| {
        AiError::ParseFailed(format!("Anthropic agent SSE JSON parse failed: {err}"))
    })?;
    match val["type"].as_str() {
        Some("error") => {
            let msg = val["error"]["message"]
                .as_str()
                .unwrap_or("Anthropic SSE error");
            return Err(AiError::ParseFailed(format!("Anthropic SSE error: {msg}")));
        }
        Some("message_start") => {
            if let Some(msg_usage) = extract_anthropic_usage_obj(val["message"].get("usage")) {
                merge_anthropic_usage(&mut acc.usage, &msg_usage);
            }
        }
        Some("message_delta") => {
            if let Some(delta_usage) = extract_anthropic_usage_obj(val.get("usage")) {
                merge_anthropic_usage(&mut acc.usage, &delta_usage);
            }
            if let Some(reason) = val["delta"]["stop_reason"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                acc.finish_reason = Some(reason.to_owned());
            }
        }
        Some("message_stop") => {
            acc.received_message_stop = true;
        }
        Some("content_block_start") => {
            handle_anthropic_content_block_start(acc, &val, on_event)?;
        }
        Some("content_block_delta") => {
            handle_anthropic_content_block_delta(acc, &val, on_event);
        }
        _ => {}
    }
    Ok(())
}

fn handle_anthropic_content_block_start(
    acc: &mut AnthropicAgentStreamAcc,
    val: &Value,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    let fallback_idx = acc.blocks.len();
    let idx = val["index"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(fallback_idx);
    if !acc.blocks.contains_key(&idx) && acc.blocks.len() >= MAX_ANTHROPIC_STREAM_BLOCKS {
        return Err(AiError::ParseFailed(format!(
            "Anthropic SSE stream exceeded max blocks ({MAX_ANTHROPIC_STREAM_BLOCKS})"
        )));
    }
    let cb = &val["content_block"];
    let block = match cb["type"].as_str() {
        Some("thinking") => {
            let thinking = cb["thinking"].as_str().unwrap_or("").to_owned();
            if !thinking.is_empty() {
                on_event(AiStreamEvent::ThoughtDelta(thinking.clone()));
            }
            AnthropicStreamBlock::Thinking {
                thinking,
                signature: cb["signature"].as_str().unwrap_or("").to_owned(),
            }
        }
        Some("redacted_thinking") => AnthropicStreamBlock::RedactedThinking {
            data: cb["data"].as_str().unwrap_or("").to_owned(),
        },
        Some("tool_use") => AnthropicStreamBlock::ToolUse {
            id: cb["id"]
                .as_str()
                .map_or_else(|| format!("ant_call_{idx}"), ToOwned::to_owned),
            name: cb["name"].as_str().unwrap_or("").to_owned(),
            partial_json: String::new(),
            initial_input: cb
                .get("input")
                .filter(|v| v.is_object() && !v.as_object().is_some_and(serde_json::Map::is_empty))
                .cloned(),
        },
        _ => {
            if let Some(txt) = cb["text"].as_str().filter(|s| !s.is_empty()) {
                on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                acc.text_buf.push_str(txt);
            }
            AnthropicStreamBlock::Text
        }
    };
    acc.blocks.insert(idx, block);
    Ok(())
}

fn handle_anthropic_content_block_delta(
    acc: &mut AnthropicAgentStreamAcc,
    val: &Value,
    on_event: &mut dyn FnMut(AiStreamEvent),
) {
    let idx = val["index"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let delta = &val["delta"];
    match delta["type"].as_str() {
        Some("thinking_delta") => {
            if let Some(chunk) = delta["thinking"].as_str().filter(|s| !s.is_empty()) {
                on_event(AiStreamEvent::ThoughtDelta(chunk.to_owned()));
                if let Some(AnthropicStreamBlock::Thinking { thinking, .. }) =
                    acc.blocks.get_mut(&idx)
                {
                    thinking.push_str(chunk);
                }
            }
        }
        Some("signature_delta") => {
            if let Some(sig) = delta["signature"].as_str()
                && let Some(AnthropicStreamBlock::Thinking { signature, .. }) =
                    acc.blocks.get_mut(&idx)
            {
                signature.push_str(sig);
            }
        }
        Some("text_delta") => {
            if let Some(txt) = delta["text"].as_str().filter(|s| !s.is_empty()) {
                on_event(AiStreamEvent::DeltaText(txt.to_owned()));
                acc.text_buf.push_str(txt);
            }
        }
        Some("input_json_delta") => {
            if let Some(part) = delta["partial_json"].as_str()
                && let Some(AnthropicStreamBlock::ToolUse { partial_json, .. }) =
                    acc.blocks.get_mut(&idx)
            {
                partial_json.push_str(part);
            }
        }
        _ => {}
    }
}

fn finalize_anthropic_agent_stream(
    acc: AnthropicAgentStreamAcc,
    model: &str,
) -> Result<AiAgentTurnResponse, AiError> {
    if !acc.received_message_stop && acc.finish_reason.is_none() {
        return Err(AiError::ParseFailed(
            "Anthropic SSE stream terminated prematurely before message_stop".to_owned(),
        ));
    }
    let mut thinking_blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for block in acc.blocks.into_values() {
        match block {
            AnthropicStreamBlock::Thinking {
                thinking,
                signature,
            } => {
                if !signature.is_empty() {
                    thinking_blocks.push(AiThinkingBlock {
                        thinking,
                        signature,
                        redacted_data: None,
                    });
                }
            }
            AnthropicStreamBlock::RedactedThinking { data } => {
                if !data.is_empty() {
                    thinking_blocks.push(AiThinkingBlock {
                        thinking: String::new(),
                        signature: String::new(),
                        redacted_data: Some(data),
                    });
                }
            }
            AnthropicStreamBlock::ToolUse {
                id,
                name,
                partial_json,
                initial_input,
            } => {
                if name.trim().is_empty() {
                    continue;
                }
                let trimmed = partial_json.trim();
                let arguments = if trimmed.is_empty() {
                    initial_input.unwrap_or_else(|| json!({}))
                } else {
                    serde_json::from_str::<Value>(trimmed).map_err(|err| {
                        AiError::ParseFailed(format!(
                            "Anthropic tool_use input JSON parse failed: {err}"
                        ))
                    })?
                };
                tool_calls.push(AiToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: None,
                });
            }
            AnthropicStreamBlock::Text => {}
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!acc.text_buf.is_empty()).then_some(acc.text_buf),
        thinking_blocks,
        tool_calls,
        model: model.to_owned(),
        finish_reason: acc.finish_reason,
        usage: acc.usage,
    })
}

fn parse_anthropic_agent_response(
    val: &Value,
    model: &str,
) -> Result<AiAgentTurnResponse, AiError> {
    let blocks = val["content"].as_array().ok_or_else(|| {
        AiError::ParseFailed("Anthropic response missing content array".to_owned())
    })?;
    let mut text_buf = String::new();
    let mut thinking_blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        match block["type"].as_str() {
            Some("thinking") => {
                if let Some(sig) = block["signature"].as_str().filter(|s| !s.is_empty()) {
                    thinking_blocks.push(AiThinkingBlock {
                        thinking: block["thinking"].as_str().unwrap_or("").to_owned(),
                        signature: sig.to_owned(),
                        redacted_data: None,
                    });
                }
            }
            Some("redacted_thinking") => {
                if let Some(data) = block["data"].as_str().filter(|s| !s.is_empty()) {
                    thinking_blocks.push(AiThinkingBlock {
                        thinking: String::new(),
                        signature: String::new(),
                        redacted_data: Some(data.to_owned()),
                    });
                }
            }
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
                        thought_signature: None,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(AiAgentTurnResponse {
        text: (!text_buf.is_empty()).then_some(text_buf),
        thinking_blocks,
        tool_calls,
        model: model.to_owned(),
        finish_reason: val["stop_reason"].as_str().map(ToOwned::to_owned),
        usage: extract_anthropic_usage_obj(val.get("usage")),
    })
}

#[cfg(test)]
fn parse_anthropic_sse_chunk(
    payload: &str,
    captured_usage: &mut Option<AiTokenUsage>,
) -> Result<Option<String>, AiError> {
    let val: Value = serde_json::from_str(payload)
        .map_err(|err| AiError::ParseFailed(format!("Anthropic SSE JSON parse failed: {err}")))?;
    if val["type"].as_str() == Some("error") {
        let msg = val["error"]["message"]
            .as_str()
            .unwrap_or("Anthropic SSE error");
        return Err(AiError::ParseFailed(format!("Anthropic SSE error: {msg}")));
    }
    if val["type"].as_str() == Some("message_start")
        && let Some(msg_usage) = extract_anthropic_usage_obj(val["message"].get("usage"))
    {
        merge_anthropic_usage(captured_usage, &msg_usage);
    } else if val["type"].as_str() == Some("message_delta")
        && let Some(delta_usage) = extract_anthropic_usage_obj(val.get("usage"))
    {
        merge_anthropic_usage(captured_usage, &delta_usage);
    }
    if val["type"].as_str() == Some("content_block_delta")
        && val["delta"]["type"].as_str() == Some("text_delta")
    {
        return Ok(val["delta"]["text"].as_str().map(ToOwned::to_owned));
    }
    Ok(None)
}

fn parse_anthropic_sync_text_and_thoughts(
    val: &Value,
    captured_usage: &mut Option<AiTokenUsage>,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError> {
    if let Some(u) = extract_anthropic_usage_obj(val.get("usage")) {
        *captured_usage = Some(u);
    }
    if let Some(blocks) = val["content"].as_array() {
        for block in blocks {
            if block["type"].as_str() == Some("thinking")
                && let Some(thought) = block["thinking"].as_str().filter(|s| !s.is_empty())
            {
                on_event(AiStreamEvent::ThoughtDelta(thought.to_owned()));
            }
        }
    }
    extract_anthropic_content_text(val)
}

#[cfg(test)]
fn parse_anthropic_sync_text(
    val: &Value,
    captured_usage: &mut Option<AiTokenUsage>,
) -> Result<String, AiError> {
    parse_anthropic_sync_text_and_thoughts(val, captured_usage, &mut |_| {})
}

fn extract_anthropic_content_text(val: &Value) -> Result<String, AiError> {
    let blocks = val["content"].as_array().ok_or_else(|| {
        AiError::ParseFailed("Anthropic response missing content array".to_owned())
    })?;
    let mut joined = String::new();
    for block in blocks {
        if block["type"].as_str() == Some("text")
            && let Some(txt) = block["text"].as_str()
        {
            joined.push_str(txt);
        }
    }
    Ok(joined)
}

fn extract_anthropic_usage_obj(usage_opt: Option<&Value>) -> Option<AiTokenUsage> {
    let usage = usage_opt?.as_object()?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }
    Some(AiTokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens.saturating_add(output_tokens),
        cost_usd: None,
        is_estimated: false,
    })
}

fn merge_anthropic_usage(acc: &mut Option<AiTokenUsage>, incoming: &AiTokenUsage) {
    let target = acc.get_or_insert_with(AiTokenUsage::default);
    if incoming.input_tokens > 0 {
        target.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens > 0 {
        target.output_tokens = incoming.output_tokens;
    }
    target.total_tokens = target.input_tokens.saturating_add(target.output_tokens);
    target.is_estimated = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::sse::test_http::spawn_single_response_server;

    #[test]
    fn anthropic_sse_deltas_parse_accurately() -> Result<(), Box<dyn std::error::Error>> {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":210}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"type\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"\\\"undo\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":55}}\n\n",
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
        let usage = res.usage.as_ref().ok_or("expected Anthropic usage")?;
        assert_eq!(usage.input_tokens, 210);
        assert_eq!(usage.output_tokens, 55);
        assert_eq!(usage.total_tokens, 265);

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
    fn anthropic_merges_tool_results_and_ignores_thinking_blocks()
    -> Result<(), Box<dyn std::error::Error>> {
        // 並列 ToolResult マージ、空 Assistant 除外、署名欠落 tool_use 履歴時の thinking 抑止・temperature 反映、thinking ブロック除外を検証
        let multi_provider = AnthropicProvider::new(
            AnthropicConfig::new("claude-sonnet-4-20250514", None)
                .with_thinking_level(AiThinkingLevel::High),
        );
        let multi_req = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::Assistant {
                    text: None,
                    thinking_blocks: Vec::new(),
                    tool_calls: vec![AiToolCall {
                        id: "t0".to_owned(),
                        name: "legacy_tool".to_owned(),
                        arguments: json!({}),
                        thought_signature: None,
                    }],
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "t1".to_owned(),
                    name: "tool_a".to_owned(),
                    content_json: "{\"a\":1}".to_owned(),
                    is_error: false,
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "t2".to_owned(),
                    name: "tool_b".to_owned(),
                    content_json: "{\"b\":2}".to_owned(),
                    is_error: false,
                },
            ],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "tool_a".to_owned(),
                description: "a".to_owned(),
                parameters_schema: json!({ "type": "object" }),
            }],
            response_json_schema: None,
            temperature: Some(0.3),
        };
        let multi_body = multi_provider.build_agent_request_body(&multi_req);
        assert_eq!(multi_body["messages"].as_array().map(Vec::len), Some(2));
        assert_eq!(multi_body["messages"][1]["role"], json!("user"));
        assert_eq!(
            multi_body["messages"][1]["content"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert!(multi_body.get("thinking").is_none());
        assert!(
            multi_body["temperature"]
                .as_f64()
                .is_some_and(|t| (t - 0.3).abs() < 1e-6)
        );

        let sync_val = json!({
            "content": [
                { "type": "thinking", "thinking": "secret", "text": "ignored" },
                { "type": "text", "text": "{\"ok\":1}" }
            ],
            "usage": { "input_tokens": 50, "output_tokens": 12 }
        });
        let mut sync_usage = None;
        assert_eq!(
            parse_anthropic_sync_text(&sync_val, &mut sync_usage)?,
            "{\"ok\":1}"
        );
        assert_eq!(sync_usage.as_ref().map(|u| u.total_tokens), Some(62));
        let mut err_usage = None;
        assert!(
            parse_anthropic_sse_chunk(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                &mut err_usage,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn anthropic_agent_turn_streams_thinking_and_preserves_signature_across_turns()
    -> Result<(), Box<dyn std::error::Error>> {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"確認先決めよう\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_ant_xyz\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ユーザーに確認します。\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_ask_1\",\"name\":\"ask_user\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"title\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"対象選択\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":40}}\n\n"
        );
        let (addr, srv) = spawn_single_response_server("text/event-stream", sse)?;
        let mut cfg = AnthropicConfig::new(
            "claude-sonnet-4-20250514",
            Some(SecretString::new("sk-ant-sse")),
        )
        .with_thinking_level(AiThinkingLevel::High);
        cfg.base_url = format!("http://{addr}/v1");
        let provider = AnthropicProvider::new(cfg);

        let turn1 = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![AiTurnMessage::User {
                parts: vec![AiContentPart::text("直して")],
            }],
            tools: vec![crate::ai::provider::AiToolDefinition {
                name: "ask_user".to_owned(),
                description: "ask".to_owned(),
                parameters_schema: json!({ "type": "object" }),
            }],
            response_json_schema: None,
            temperature: Some(0.2),
        };
        let mut thoughts = Vec::new();
        let mut texts = Vec::new();
        let res1 = provider.send_agent_turn(&turn1, &mut |evt| match evt {
            AiStreamEvent::ThoughtDelta(t) => thoughts.push(t),
            AiStreamEvent::DeltaText(t) => texts.push(t),
            _ => {}
        })?;
        assert_eq!(thoughts, vec!["確認先決めよう"]);
        assert_eq!(texts, vec!["ユーザーに確認します。"]);
        assert_eq!(res1.thinking_blocks.len(), 1);
        assert_eq!(res1.thinking_blocks[0].thinking, "確認先決めよう");
        assert_eq!(res1.thinking_blocks[0].signature, "sig_ant_xyz");
        assert_eq!(res1.tool_calls.len(), 1);
        assert_eq!(res1.tool_calls[0].arguments["title"], "対象選択");
        assert_eq!(res1.usage.as_ref().map(|u| u.total_tokens), Some(140));
        let cap1 = srv.join().map_err(|_| "srv join")??;
        let (_, json_body1) = cap1.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
        let parsed_body1: Value = serde_json::from_str(json_body1)?;
        assert_eq!(parsed_body1["stream"], json!(true));
        assert_eq!(parsed_body1["thinking"]["type"], json!("enabled"));
        assert_eq!(parsed_body1["thinking"]["budget_tokens"], json!(10240));

        // 2ターン目: thinking_blocks 付き履歴を引き回した場合、tools 併用時でも thinking が有効で
        // assistant の content 配列で thinking が tool_use より前に配置される
        let turn2 = AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::User {
                    parts: vec![AiContentPart::text("直して")],
                },
                AiTurnMessage::Assistant {
                    text: res1.text,
                    thinking_blocks: res1.thinking_blocks,
                    tool_calls: res1.tool_calls,
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "toolu_ask_1".to_owned(),
                    name: "ask_user".to_owned(),
                    content_json: "{\"ok\":true}".to_owned(),
                    is_error: false,
                },
            ],
            tools: turn1.tools,
            response_json_schema: None,
            temperature: Some(0.2),
        };
        let body2 = provider.build_agent_request_body(&turn2);
        assert!(body2.get("thinking").is_some());
        let assistant_content = body2["messages"][1]["content"]
            .as_array()
            .ok_or("assistant content array")?;
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert_eq!(assistant_content[0]["signature"], "sig_ant_xyz");
        assert_eq!(assistant_content[1]["type"], "text");
        assert_eq!(assistant_content[2]["type"], "tool_use");
        Ok(())
    }

    #[test]
    fn anthropic_redacted_thinking_empty_sig_adjacent_merge_eof_and_sync_thoughts()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. redacted_thinking の SSE 受信と次ターン再送（元順序保持）
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_xyz\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"get_timeline_summary\",\"input\":{}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (addr, srv) = spawn_single_response_server("text/event-stream", sse)?;
        let mut cfg = AnthropicConfig::new(
            "claude-sonnet-4-20250514",
            Some(SecretString::new("sk-ant-redacted")),
        )
        .with_thinking_level(AiThinkingLevel::High);
        cfg.base_url = format!("http://{addr}/v1");
        let provider = AnthropicProvider::new(cfg);

        let res = provider.send_agent_turn(
            &AiAgentTurnRequest {
                system_prompt: "Director".to_owned(),
                messages: vec![AiTurnMessage::user_text("チェックして")],
                tools: Vec::new(),
                response_json_schema: None,
                temperature: None,
            },
            &mut |_| {},
        )?;
        assert_eq!(res.thinking_blocks.len(), 1);
        assert_eq!(
            res.thinking_blocks[0].redacted_data.as_deref(),
            Some("opaque_xyz")
        );
        let _ = srv.join().map_err(|_| "srv join")??;

        let follow_body = provider.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::user_text("チェックして"),
                AiTurnMessage::Assistant {
                    text: None,
                    thinking_blocks: res.thinking_blocks,
                    tool_calls: res.tool_calls,
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "tu_1".to_owned(),
                    name: "get_timeline_summary".to_owned(),
                    content_json: "{}".to_owned(),
                    is_error: false,
                },
            ],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert!(follow_body.get("thinking").is_some());
        assert_eq!(
            follow_body["messages"][1]["content"][0]["type"],
            json!("redacted_thinking")
        );
        assert_eq!(
            follow_body["messages"][1]["content"][0]["data"],
            json!("opaque_xyz")
        );

        // 2. 空署名のみの thinking_blocks では thinking を無効化し HTTP 400 を回避
        assert_empty_sig_and_bad_merge_disable_thinking(&provider);
        // 3. truncated SSE & sync ThoughtDelta
        assert_truncated_eof_and_sync_thoughts()?;
        Ok(())
    }

    fn assert_empty_sig_and_bad_merge_disable_thinking(provider: &AnthropicProvider) {
        let empty_sig_body = provider.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::Assistant {
                    text: Some("x".to_owned()),
                    thinking_blocks: vec![AiThinkingBlock {
                        thinking: "unsigned".to_owned(),
                        signature: String::new(),
                        redacted_data: None,
                    }],
                    tool_calls: vec![AiToolCall {
                        id: "c0".to_owned(),
                        name: "noop".to_owned(),
                        arguments: json!({}),
                        thought_signature: None,
                    }],
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "c0".to_owned(),
                    name: "noop".to_owned(),
                    content_json: "{}".to_owned(),
                    is_error: false,
                },
            ],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert!(empty_sig_body.get("thinking").is_none());

        let merged_bad_order = provider.build_agent_request_body(&AiAgentTurnRequest {
            system_prompt: "Director".to_owned(),
            messages: vec![
                AiTurnMessage::Assistant {
                    text: Some("先行テキスト".to_owned()),
                    thinking_blocks: Vec::new(),
                    tool_calls: Vec::new(),
                },
                AiTurnMessage::Assistant {
                    text: None,
                    thinking_blocks: vec![AiThinkingBlock {
                        thinking: "t".to_owned(),
                        signature: "sig".to_owned(),
                        redacted_data: None,
                    }],
                    tool_calls: vec![AiToolCall {
                        id: "c1".to_owned(),
                        name: "noop".to_owned(),
                        arguments: json!({}),
                        thought_signature: None,
                    }],
                },
                AiTurnMessage::ToolResult {
                    tool_call_id: "c1".to_owned(),
                    name: "noop".to_owned(),
                    content_json: "{}".to_owned(),
                    is_error: false,
                },
            ],
            tools: Vec::new(),
            response_json_schema: None,
            temperature: None,
        });
        assert!(merged_bad_order.get("thinking").is_none());
    }

    fn assert_truncated_eof_and_sync_thoughts() -> Result<(), Box<dyn std::error::Error>> {
        let truncated_acc = AnthropicAgentStreamAcc::default();
        assert!(finalize_anthropic_agent_stream(truncated_acc, "claude").is_err());

        let sync_val = json!({
            "content": [
                { "type": "thinking", "thinking": "sync思考", "signature": "sig1" },
                { "type": "text", "text": "本文" }
            ]
        });
        let mut sync_usage = None;
        let mut sync_thoughts = Vec::new();
        let sync_txt =
            parse_anthropic_sync_text_and_thoughts(&sync_val, &mut sync_usage, &mut |evt| {
                if let AiStreamEvent::ThoughtDelta(t) = evt {
                    sync_thoughts.push(t);
                }
            })?;
        assert_eq!(sync_thoughts, vec!["sync思考"]);
        assert_eq!(sync_txt, "本文");
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
