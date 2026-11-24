//! `OpenAI` 互換 / Gemini / Anthropic 間で共有する `ureq` HTTP POST・SSE ストリーム受信ヘルパー。

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::ai::provider::{AiError, AiStreamEvent};

const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_COLLECTED_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHED_AGENTS: usize = 8;

/// JSON POST 要求パラメータ。
pub struct HttpJsonRequest<'a> {
    /// リクエスト先完全 URL。
    pub url: &'a str,
    /// HTTP ヘッダ一覧。
    pub headers: &'a [(&'a str, &'a str)],
    /// JSON ボディ。
    pub body: &'a Value,
    /// 全体タイムアウト（秒）。
    pub timeout_secs: u64,
}

/// JSON GET 要求パラメータ。
pub struct HttpGetRequest<'a> {
    /// リクエスト先完全 URL。
    pub url: &'a str,
    /// HTTP ヘッダ一覧。
    pub headers: &'a [(&'a str, &'a str)],
    /// 全体タイムアウト（秒）。
    pub timeout_secs: u64,
}

/// 指定 URL へ JSON GET を行い、2xx 以外のステータス時に本文付き [`AiError::HttpStatus`] を返す。
///
/// # Errors
///
/// URL 検証失敗・通信失敗・HTTP 4xx/5xx エラー・JSON 解析失敗時に [`AiError`] を返す。
pub fn get_json_sync(request: &HttpGetRequest<'_>) -> Result<Value, AiError> {
    validate_http_url_and_headers(request.url, request.headers)?;
    let agent = cached_ureq_agent(request.timeout_secs);
    let mut req = agent.get(request.url).header("Accept", "application/json");
    for (name, val) in request.headers {
        req = req.header(*name, *val);
    }

    let mut resp = req
        .call()
        .map_err(|err| AiError::Transport(err.to_string()))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let err_body = resp
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| "<unreadable error body>".to_owned());
        let snippet: String = err_body.chars().take(600).collect();
        return Err(AiError::HttpStatus {
            status,
            body: snippet,
        });
    }
    resp.body_mut()
        .read_json()
        .map_err(|err| AiError::ParseFailed(format!("JSON response parse error: {err}")))
}

/// 指定 URL へ JSON POST を行い、2xx 以外のステータス時に本文付き [`AiError::HttpStatus`] を返す。
///
/// ストリーミング応答時は `text/event-stream` イベント単位パーサを呼び出し、非 SSE の JSON 応答へも自動フォールバックする。
/// 状態値 `state` をクロージャ間で所有権引き回しすることで、呼び出し側の二重可変借用を回避する。
/// `on_sse_event(&mut state, event_name, payload)` は各 SSE イベントごとに呼ばれ、`Ok(true)` を返すとストリーム読み取りを正常終了する。
///
/// # Errors
///
/// 通信失敗・HTTP エラー・JSON/SSE 解析エラー時に [`AiError`] を返す。
pub fn post_json_sse_or_sync<S, T, FSse, FFinish, FSync>(
    request: &HttpJsonRequest<'_>,
    mut state: S,
    mut on_sse_event: FSse,
    finish_sse: FFinish,
    on_sync_json: FSync,
) -> Result<T, AiError>
where
    FSse: FnMut(&mut S, Option<&str>, &str) -> Result<bool, AiError>,
    FFinish: FnOnce(S) -> Result<T, AiError>,
    FSync: FnOnce(S, &Value) -> Result<T, AiError>,
{
    let (mut resp, content_type) = send_json_post(request, "text/event-stream, application/json")?;

    if content_type.contains("text/event-stream") {
        read_sse_events(resp.body_mut().as_reader(), &mut |evt, payload| {
            on_sse_event(&mut state, evt, payload)
        })?;
        finish_sse(state)
    } else {
        let json_val: Value = resp
            .body_mut()
            .read_json()
            .map_err(|err| AiError::ParseFailed(format!("JSON response parse error: {err}")))?;
        on_sync_json(state, &json_val)
    }
}

/// 指定 URL へ JSON POST を行い、2xx 以外のステータス時に本文付き [`AiError::HttpStatus`] を返す。
///
/// ストリーミング応答時は `text/event-stream` 行単位パーサを呼び出し、非 SSE の JSON 応答へも自動フォールバックする。
///
/// # Errors
///
/// 通信失敗・HTTP エラー・JSON/SSE 解析エラー時に [`AiError`] を返す。
pub fn post_json_streaming_or_sync<FChunk, FSync>(
    request: &HttpJsonRequest<'_>,
    mut on_sse_payload: FChunk,
    mut on_sync_json: FSync,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError>
where
    FChunk: FnMut(&str) -> Result<Option<String>, AiError>,
    FSync: FnMut(&Value) -> Result<String, AiError>,
{
    post_json_sse_or_sync(
        request,
        (String::new(), on_event),
        |(collected, evt_cb), _event_name, payload| {
            if let Some(delta) = on_sse_payload(payload)?
                && !delta.is_empty()
            {
                append_collected_delta(collected, &delta, *evt_cb)?;
            }
            Ok(false)
        },
        |(collected, _)| Ok(collected),
        |(_, evt_cb), json_val| {
            let text = on_sync_json(json_val)?;
            if !text.is_empty() {
                evt_cb(AiStreamEvent::DeltaText(text.clone()));
            }
            Ok(text)
        },
    )
}

fn append_collected_delta(
    collected: &mut String,
    delta: &str,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<(), AiError> {
    if collected.len().saturating_add(delta.len()) > MAX_SSE_COLLECTED_BYTES {
        return Err(AiError::ParseFailed(format!(
            "SSE accumulated stream exceeded max size ({MAX_SSE_COLLECTED_BYTES} bytes)"
        )));
    }
    on_event(AiStreamEvent::DeltaText(delta.to_owned()));
    collected.push_str(delta);
    Ok(())
}

/// 指定 URL へ非ストリーミングの JSON POST を行い、完全な JSON 応答オブジェクトを返す。
///
/// マルチモーダルや Native Tool Calling など、完全構造化応答を一括取得するエージェントターン向け。
///
/// # Errors
///
/// 通信失敗・HTTP 4xx/5xx エラー・JSON 解析エラー時に [`AiError`] を返す。
pub fn post_json_sync(request: &HttpJsonRequest<'_>) -> Result<Value, AiError> {
    let (mut resp, _) = send_json_post(request, "application/json")?;
    resp.body_mut()
        .read_json()
        .map_err(|err| AiError::ParseFailed(format!("JSON response parse error: {err}")))
}

fn validate_http_url_and_headers(url: &str, headers: &[(&str, &str)]) -> Result<(), AiError> {
    if url.bytes().any(|b| b < b' ' || b == 0x7F) {
        return Err(AiError::Transport(
            "HTTP URL contains forbidden control characters".to_owned(),
        ));
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    let Some(rest) = lower.strip_prefix("http://") else {
        return Err(AiError::Transport(
            "only https:// or http:// URLs are permitted for AI providers".to_owned(),
        ));
    };
    let has_sensitive_auth = headers.iter().any(|(name, _)| {
        name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("x-goog-api-key")
            || name.eq_ignore_ascii_case("proxy-authorization")
    });
    if has_sensitive_auth && !is_loopback_http_authority(rest) {
        return Err(AiError::Transport(
            "refusing to transmit API credentials over plaintext non-loopback http://".to_owned(),
        ));
    }
    Ok(())
}

fn is_loopback_http_authority(url_after_scheme: &str) -> bool {
    let authority = url_after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, right)| right);
    let host = if let Some(after_open) = host_port.strip_prefix('[') {
        after_open.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn cached_ureq_agent(timeout_secs: u64) -> ureq::Agent {
    static AGENTS: OnceLock<Mutex<HashMap<u64, ureq::Agent>>> = OnceLock::new();
    let clamped = timeout_secs.max(5);
    let cache = AGENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        if let Some(existing) = guard.get(&clamped) {
            return existing.clone();
        }
        if guard.len() >= MAX_CACHED_AGENTS {
            guard.clear();
        }
        let created = build_ureq_agent(clamped);
        guard.insert(clamped, created.clone());
        return created;
    }
    build_ureq_agent(clamped)
}

fn build_ureq_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn send_json_post(
    request: &HttpJsonRequest<'_>,
    accept_header: &str,
) -> Result<(ureq::http::Response<ureq::Body>, String), AiError> {
    validate_http_url_and_headers(request.url, request.headers)?;
    let agent = cached_ureq_agent(request.timeout_secs);

    let mut req = agent
        .post(request.url)
        .header("Content-Type", "application/json")
        .header("Accept", accept_header);
    for (name, val) in request.headers {
        req = req.header(*name, *val);
    }

    let mut resp = req
        .send_json(request.body)
        .map_err(|err| AiError::Transport(err.to_string()))?;

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if status >= 400 {
        let err_body = resp
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| "<unreadable error body>".to_owned());
        let snippet: String = err_body.chars().take(600).collect();
        return Err(AiError::HttpStatus {
            status,
            body: snippet,
        });
    }

    Ok((resp, content_type))
}

#[cfg(test)]
fn read_sse_lines<R, FChunk>(
    reader: R,
    on_sse_payload: &mut FChunk,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError>
where
    R: std::io::Read,
    FChunk: FnMut(&str) -> Result<Option<String>, AiError>,
{
    let mut collected = String::new();
    read_sse_events(reader, &mut |_event_name, payload| {
        if let Some(delta) = on_sse_payload(payload)?
            && !delta.is_empty()
        {
            append_collected_delta(&mut collected, &delta, on_event)?;
        }
        Ok(false)
    })?;
    Ok(collected)
}

fn read_sse_events<R, FSse>(reader: R, on_sse_event: &mut FSse) -> Result<(), AiError>
where
    R: std::io::Read,
    FSse: FnMut(Option<&str>, &str) -> Result<bool, AiError>,
{
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    let mut event_name: Option<String> = None;
    let mut event_data = String::new();
    let line_limit = u64::try_from(MAX_SSE_LINE_BYTES.saturating_add(1)).unwrap_or(u64::MAX);

    loop {
        line.clear();
        let read = std::io::Read::take(&mut buf_reader, line_limit).read_line(&mut line)?;
        if read == 0 {
            if !event_data.is_empty() {
                let _ = flush_sse_event(&mut event_name, &mut event_data, on_sse_event)?;
            }
            break;
        }
        if read > MAX_SSE_LINE_BYTES && !line.ends_with('\n') {
            return Err(AiError::ParseFailed(format!(
                "SSE line exceeded max size ({MAX_SSE_LINE_BYTES} bytes)"
            )));
        }
        let raw_line = line.trim_end_matches(['\r', '\n']);
        if raw_line.is_empty() {
            if flush_sse_event(&mut event_name, &mut event_data, on_sse_event)? {
                break;
            }
            continue;
        }
        if raw_line.starts_with(':') {
            continue;
        }
        let (field, value_raw) = raw_line.split_once(':').unwrap_or((raw_line, ""));
        let value = value_raw.strip_prefix(' ').unwrap_or(value_raw);
        if field == "event" {
            event_name = Some(value.trim().to_owned());
        } else if field == "data" {
            let extra = usize::from(!event_data.is_empty()) + value.len();
            if event_data.len().saturating_add(extra) > MAX_SSE_EVENT_BYTES {
                return Err(AiError::ParseFailed(format!(
                    "SSE event payload exceeded max size ({MAX_SSE_EVENT_BYTES} bytes)"
                )));
            }
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(value);
        }
    }
    Ok(())
}

fn flush_sse_event<FSse>(
    event_name: &mut Option<String>,
    event_data: &mut String,
    on_sse_event: &mut FSse,
) -> Result<bool, AiError>
where
    FSse: FnMut(Option<&str>, &str) -> Result<bool, AiError>,
{
    let name = event_name.take();
    if event_data.is_empty() {
        return Ok(false);
    }
    let payload_owned = std::mem::take(event_data);
    let payload = payload_owned.trim();
    if payload.is_empty() {
        return Ok(false);
    }
    if payload == "[DONE]" {
        return Ok(true);
    }
    if name.as_deref() == Some("error") {
        let snippet: String = payload.chars().take(600).collect();
        return Err(AiError::ParseFailed(format!("SSE error event: {snippet}")));
    }
    on_sse_event(name.as_deref(), payload)
}

#[cfg(test)]
pub(crate) mod test_http {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener};
    use std::thread::JoinHandle;

    pub(crate) fn spawn_single_response_server(
        content_type: &'static str,
        response_body: &'static str,
    ) -> std::io::Result<(SocketAddr, JoinHandle<std::io::Result<String>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept()?;
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            let mut header_end = None;
            let mut expected_total = None;

            loop {
                let n = sock.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if header_end.is_none()
                    && let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let end = pos + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&raw[..end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected_total = Some(end + content_len);
                }
                if let Some(total) = expected_total
                    && raw.len() >= total
                {
                    break;
                }
            }

            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len(),
            );
            sock.write_all(resp.as_bytes())?;
            sock.flush()?;
            let _ = sock.shutdown(Shutdown::Write);
            Ok(String::from_utf8_lossy(&raw).into_owned())
        });
        Ok((addr, handle))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn url_validation_blocks_insecure_plaintext_auth_and_non_http() {
        let headers = [("Authorization", "Bearer secret")];
        assert!(validate_http_url_and_headers("https://api.openai.com/v1/chat", &headers).is_ok());
        assert!(validate_http_url_and_headers("http://127.0.0.1:8080/v1", &headers).is_ok());
        assert!(validate_http_url_and_headers("http://localhost:11434/v1", &headers).is_ok());
        assert!(validate_http_url_and_headers("http://[::1]:11434/v1", &headers).is_ok());
        assert!(validate_http_url_and_headers("http://192.168.1.50:11434/v1", &[]).is_ok());
        assert!(
            validate_http_url_and_headers("http://example.com/v1", &headers).is_err(),
            "non-loopback plaintext http with API credentials must be rejected"
        );
        assert!(validate_http_url_and_headers("file:///etc/passwd", &[]).is_err());
    }

    #[test]
    fn sse_reader_joins_multiline_data_and_surfaces_error_events() -> Result<(), AiError> {
        let stream = "data: {\"a\":\ndata: 1}\n\ndata: [DONE]\n\n";
        let mut events = Vec::new();
        let text = read_sse_lines(
            Cursor::new(stream.as_bytes()),
            &mut |payload| {
                let parsed: Value = serde_json::from_str(payload)
                    .map_err(|e| AiError::ParseFailed(e.to_string()))?;
                Ok(parsed.get("a").map(std::string::ToString::to_string))
            },
            &mut |evt| {
                if let AiStreamEvent::DeltaText(s) = evt {
                    events.push(s);
                }
            },
        )?;
        assert_eq!(text, "1");
        assert_eq!(events, vec!["1"]);

        let err_stream = "event: error\ndata: {\"error\":{\"message\":\"overloaded\"}}\n\n";
        let Err(err) = read_sse_lines(
            Cursor::new(err_stream.as_bytes()),
            &mut |_| Ok(None),
            &mut |_| {},
        ) else {
            return Err(AiError::ParseFailed(
                "SSE error event must return Err".to_owned(),
            ));
        };
        assert!(err.to_string().contains("overloaded"));
        Ok(())
    }

    #[test]
    fn post_json_sse_or_sync_supports_sse_and_json_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let sse = "event: ping\ndata: {\"n\":1}\n\ndata: {\"n\":2}\n\ndata: [DONE]\n\n";
        let (addr1, srv1) = test_http::spawn_single_response_server("text/event-stream", sse)?;
        let url1 = format!("http://{addr1}/v1/test");
        let body = serde_json::json!({"ok": true});
        let req1 = HttpJsonRequest {
            url: &url1,
            headers: &[],
            body: &body,
            timeout_secs: 5,
        };
        let (sum, seen) = post_json_sse_or_sync(
            &req1,
            Vec::new(),
            |seen, evt, payload| {
                let v: Value = serde_json::from_str(payload)
                    .map_err(|e| AiError::ParseFailed(e.to_string()))?;
                seen.push((evt.map(ToOwned::to_owned), v["n"].as_u64().unwrap_or(0)));
                Ok(false)
            },
            |seen| {
                let sum = seen.iter().map(|(_, n)| *n).sum::<u64>();
                Ok((sum, seen))
            },
            |seen, _| Ok((0, seen)),
        )?;
        assert_eq!(sum, 3);
        assert_eq!(seen, vec![(Some("ping".to_owned()), 1), (None, 2)]);
        let _ = srv1.join().map_err(|_| "srv1 join")??;

        let (addr2, srv2) =
            test_http::spawn_single_response_server("application/json", r#"{"n":42}"#)?;
        let url2 = format!("http://{addr2}/v1/test");
        let req2 = HttpJsonRequest {
            url: &url2,
            headers: &[],
            body: &body,
            timeout_secs: 5,
        };
        let fallback = post_json_sse_or_sync(
            &req2,
            (),
            |(), _, _| Ok(false),
            |()| Ok(0),
            |(), v| Ok(v["n"].as_u64().unwrap_or(0)),
        )?;
        assert_eq!(fallback, 42);
        let _ = srv2.join().map_err(|_| "srv2 join")??;
        Ok(())
    }
}
