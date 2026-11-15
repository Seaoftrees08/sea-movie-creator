//! OpenAI・OpenRouter・Ollama・LM Studio・vLLM 共通 HTTP/SSE クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiError, AiProvider, AiProviderKind, AiRequest, AiResponse, AiRole, AiStreamEvent, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync};

/// `OpenAI` 互換チャット補完 API クライアント設定。
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// 論理プロバイダ種別 (`OpenAi` / `OpenRouter` / `OllamaCompat`)。
    pub kind: AiProviderKind,
    /// API ベース URL（末尾スラッシュ不要。例: `https://openrouter.ai/api/v1`）。
    pub base_url: String,
    /// 使用モデル ID。
    pub model: String,
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
            api_key,
            use_streaming: true,
            timeout_secs: 120,
        }
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

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": 0.1,
            "stream": stream,
        });
        // Ollama/OpenAI 互換の多くで有効な JSON 出力モードヒントを付与
        if matches!(
            self.config.kind,
            AiProviderKind::OpenAi | AiProviderKind::OpenRouter
        ) {
            body["response_format"] = json!({ "type": "json_object" });
        }
        body
    }
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

        let mut owned_headers: Vec<(String, String)> = Vec::new();
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
