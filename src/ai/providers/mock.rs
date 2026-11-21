//! ネットワーク不要の決定論的テスト・オフライン確認用 Mock AI プロバイダ。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiError, AiProvider, AiRequest, AiResponse,
    AiStreamEvent, AiTokenUsage,
};

type MockHandler = Arc<dyn Fn(&AiRequest) -> Result<String, AiError> + Send + Sync>;
type MockAgentHandler =
    Arc<dyn Fn(&AiAgentTurnRequest) -> Result<AiAgentTurnResponse, AiError> + Send + Sync>;

/// 自動テストおよびローカル UI 確認用の決定論的 AI プロバイダ。
#[derive(Clone, Default)]
pub struct MockAiProvider {
    canned_responses: Arc<Mutex<VecDeque<Result<String, AiError>>>>,
    handler: Option<MockHandler>,
    recorded_requests: Arc<Mutex<Vec<AiRequest>>>,
    canned_agent_turns: Arc<Mutex<VecDeque<Result<AiAgentTurnResponse, AiError>>>>,
    agent_handler: Option<MockAgentHandler>,
    recorded_agent_requests: Arc<Mutex<Vec<AiAgentTurnRequest>>>,
    vision_enabled: bool,
    native_tools_enabled: bool,
    chunk_size: usize,
}

impl std::fmt::Debug for MockAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockAiProvider")
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

impl MockAiProvider {
    /// 空のキューを持つ [`MockAiProvider`] を作成する。
    #[must_use]
    pub fn new() -> Self {
        Self {
            canned_responses: Arc::new(Mutex::new(VecDeque::new())),
            handler: None,
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
            canned_agent_turns: Arc::new(Mutex::new(VecDeque::new())),
            agent_handler: None,
            recorded_agent_requests: Arc::new(Mutex::new(Vec::new())),
            vision_enabled: true,
            native_tools_enabled: true,
            chunk_size: 24,
        }
    }

    /// 固定 JSON / テキスト応答列を返すプロバイダを簡便に構築する。
    #[must_use]
    pub fn with_responses<I, S>(responses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let provider = Self::new();
        for item in responses {
            provider.push_response(item.into());
        }
        provider
    }

    /// エージェントターン用の固定応答列を返すプロバイダを構築する。
    #[must_use]
    pub fn with_agent_turns<I>(turns: I) -> Self
    where
        I: IntoIterator<Item = AiAgentTurnResponse>,
    {
        let provider = Self::new();
        for turn in turns {
            provider.push_agent_turn(turn);
        }
        provider
    }

    /// リクエストを受け取って動的に応答 JSON を組み立てるクロージャを設定する。
    #[must_use]
    pub fn with_handler<F>(handler: F) -> Self
    where
        F: Fn(&AiRequest) -> Result<String, AiError> + Send + Sync + 'static,
    {
        let mut provider = Self::new();
        provider.handler = Some(Arc::new(handler));
        provider
    }

    /// エージェントターン要求を受け取って動的に応答を返すクロージャを設定する。
    #[must_use]
    pub fn with_agent_handler<F>(handler: F) -> Self
    where
        F: Fn(&AiAgentTurnRequest) -> Result<AiAgentTurnResponse, AiError> + Send + Sync + 'static,
    {
        let mut provider = Self::new();
        provider.agent_handler = Some(Arc::new(handler));
        provider
    }

    /// Vision / Native Tool Calling 対応能力フラグをテスト用に上書きする。
    #[must_use]
    pub const fn with_capabilities(mut self, vision: bool, native_tools: bool) -> Self {
        self.vision_enabled = vision;
        self.native_tools_enabled = native_tools;
        self
    }

    /// 成功テキスト応答をキューへ追加する。
    pub fn push_response(&self, text: impl Into<String>) {
        if let Ok(mut queue) = self.canned_responses.lock() {
            queue.push_back(Ok(text.into()));
        }
    }

    /// エラー応答をキューへ追加する。
    pub fn push_error(&self, err: AiError) {
        if let Ok(mut queue) = self.canned_responses.lock() {
            queue.push_back(Err(err));
        }
    }

    /// エージェントターン用応答をキューへ追加する。
    pub fn push_agent_turn(&self, response: AiAgentTurnResponse) {
        if let Ok(mut queue) = self.canned_agent_turns.lock() {
            queue.push_back(Ok(response));
        }
    }

    /// これまで受信した [`AiRequest`] のスナップショットを返す。
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<AiRequest> {
        self.recorded_requests
            .lock()
            .map_or_else(|_| Vec::new(), |guard| guard.clone())
    }

    /// これまで受信した [`AiAgentTurnRequest`] のスナップショットを返す。
    #[must_use]
    pub fn recorded_agent_requests(&self) -> Vec<AiAgentTurnRequest> {
        self.recorded_agent_requests
            .lock()
            .map_or_else(|_| Vec::new(), |guard| guard.clone())
    }
}

impl AiProvider for MockAiProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn display_name(&self) -> &'static str {
        "Mock AI Provider (Offline)"
    }

    fn supports_vision(&self) -> bool {
        self.vision_enabled
    }

    fn supports_native_tools(&self) -> bool {
        self.native_tools_enabled
    }

    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        if let Ok(mut log) = self.recorded_requests.lock() {
            log.push(request.clone());
        }
        on_event(AiStreamEvent::Status(
            "Mock AI 応答を生成しています...".to_owned(),
        ));

        let queued = self
            .canned_responses
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front());

        let raw_text = match queued {
            Some(Ok(text)) => text,
            Some(Err(err)) => return Err(err),
            None => {
                if let Some(handler) = &self.handler {
                    handler(request)?
                } else {
                    return Err(AiError::PluginError(
                        "MockAiProvider 応答キューが空です".to_owned(),
                    ));
                }
            }
        };

        let chunk_chars = self.chunk_size.max(8);
        let mut chars = raw_text.chars();
        loop {
            let chunk: String = chars.by_ref().take(chunk_chars).collect();
            if chunk.is_empty() {
                break;
            }
            on_event(AiStreamEvent::DeltaText(chunk));
        }

        let usage = AiTokenUsage::estimate_from_chars(
            request
                .system_prompt
                .len()
                .saturating_add(request.user_prompt.len()),
            raw_text.len(),
        );
        let response = AiResponse {
            raw_text,
            model: "mock-editor-v1".to_owned(),
            usage: Some(usage),
        };
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
    }

    fn send_agent_turn(
        &self,
        request: &AiAgentTurnRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiAgentTurnResponse, AiError> {
        if let Ok(mut log) = self.recorded_agent_requests.lock() {
            log.push(request.clone());
        }
        on_event(AiStreamEvent::Status(
            "Mock Agent ターンを実行中...".to_owned(),
        ));

        let queued = self
            .canned_agent_turns
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front());
        match queued {
            Some(Ok(turn)) => Ok(turn),
            Some(Err(err)) => Err(err),
            None => {
                if let Some(handler) = &self.agent_handler {
                    handler(request)
                } else {
                    Err(AiError::PluginError(
                        "MockAiProvider エージェント応答キューが空です".to_owned(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mock_provider_records_requests_and_streams_chunks() -> Result<(), Box<dyn std::error::Error>>
    {
        let mock = MockAiProvider::with_responses(["{\"type\":\"undo\"}"]);
        let req = AiRequest {
            system_prompt: "sys".to_owned(),
            command_schema_json: json!({}),
            context_summary: json!({ "playhead_frame": 15 }),
            history: Vec::new(),
            user_prompt: "直前を取り消して".to_owned(),
            stream: true,
        };
        let mut streamed = String::new();
        let res = mock.send_streaming(&req, &mut |evt| {
            if let AiStreamEvent::DeltaText(delta) = evt {
                streamed.push_str(&delta);
            }
        })?;
        assert_eq!(res.raw_text, "{\"type\":\"undo\"}");
        assert_eq!(streamed, res.raw_text);
        assert_eq!(mock.recorded_requests().len(), 1);
        assert_eq!(mock.recorded_requests()[0].user_prompt, "直前を取り消して");
        Ok(())
    }
}
