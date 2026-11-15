//! ネットワーク不要の決定論的テスト・オフライン確認用 Mock AI プロバイダ。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::ai::provider::{AiError, AiProvider, AiRequest, AiResponse, AiStreamEvent};

type MockHandler = Arc<dyn Fn(&AiRequest) -> Result<String, AiError> + Send + Sync>;

/// 自動テストおよびローカル UI 確認用の決定論的 AI プロバイダ。
#[derive(Clone, Default)]
pub struct MockAiProvider {
    canned_responses: Arc<Mutex<VecDeque<Result<String, AiError>>>>,
    handler: Option<MockHandler>,
    recorded_requests: Arc<Mutex<Vec<AiRequest>>>,
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

    /// リクエストを受け取って動的に応答 JSON を組み立てるクロージャを設定する。
    #[must_use]
    pub fn with_handler<F>(handler: F) -> Self
    where
        F: Fn(&AiRequest) -> Result<String, AiError> + Send + Sync + 'static,
    {
        Self {
            canned_responses: Arc::new(Mutex::new(VecDeque::new())),
            handler: Some(Arc::new(handler)),
            recorded_requests: Arc::new(Mutex::new(Vec::new())),
            chunk_size: 24,
        }
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

    /// これまで受信した [`AiRequest`] のスナップショットを返す。
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<AiRequest> {
        self.recorded_requests
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

        let response = AiResponse {
            raw_text,
            model: "mock-editor-v1".to_owned(),
        };
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
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
