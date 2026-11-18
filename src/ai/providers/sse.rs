//! `OpenAI` 互換 / Gemini / Anthropic 間で共有する `ureq` HTTP POST・SSE ストリーム受信ヘルパー。

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::Value;

use crate::ai::provider::{AiError, AiStreamEvent};

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
    let (mut resp, content_type) = send_json_post(request, "text/event-stream, application/json")?;

    if content_type.contains("text/event-stream") {
        read_sse_lines(resp.body_mut().as_reader(), &mut on_sse_payload, on_event)
    } else {
        let json_val: Value = resp
            .body_mut()
            .read_json()
            .map_err(|err| AiError::ParseFailed(format!("JSON response parse error: {err}")))?;
        let text = on_sync_json(&json_val)?;
        if !text.is_empty() {
            on_event(AiStreamEvent::DeltaText(text.clone()));
        }
        Ok(text)
    }
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

fn send_json_post(
    request: &HttpJsonRequest<'_>,
    accept_header: &str,
) -> Result<(ureq::http::Response<ureq::Body>, String), AiError> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(request.timeout_secs.max(5))))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = config.into();

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

fn read_sse_lines<R, FChunk>(
    reader: R,
    on_sse_payload: &mut FChunk,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<String, AiError>
where
    R: std::io::Read,
    FChunk: FnMut(&str) -> Result<Option<String>, AiError>,
{
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    let mut collected = String::new();

    loop {
        line.clear();
        let read = buf_reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') || trimmed.starts_with("event:") {
            continue;
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            if payload == "[DONE]" {
                break;
            }
            continue;
        }
        if let Some(delta) = on_sse_payload(payload)?
            && !delta.is_empty()
        {
            on_event(AiStreamEvent::DeltaText(delta.clone()));
            collected.push_str(&delta);
        }
    }
    Ok(collected)
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
