//! Google Gemini REST / SSE API クライアント。

use serde_json::{Value, json};

use crate::ai::provider::{
    AiError, AiProvider, AiProviderKind, AiRequest, AiResponse, AiRole, AiStreamEvent, SecretString,
};
use crate::ai::providers::credentials::require_api_key;
use crate::ai::providers::sse::{HttpJsonRequest, post_json_streaming_or_sync};

/// Google Gemini API 接続設定。
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    /// API ベース URL。
    pub base_url: String,
    /// モデル ID（例: `gemini-2.5-flash`）。
    pub model: String,
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
            api_key,
            use_streaming: true,
            timeout_secs: 60,
        }
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

    fn build_request_body(request: &AiRequest) -> Value {
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

        json!({
            "systemInstruction": {
                "parts": [{ "text": system_text }]
            },
            "contents": contents,
            "generationConfig": {
                "temperature": 0.1,
                "responseMimeType": "application/json",
            }
        })
    }
}

impl AiProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn display_name(&self) -> &str {
        &self.label
    }

    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
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
        let body = Self::build_request_body(request);
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
