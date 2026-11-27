//! `OpenAI` 互換 / Gemini / Anthropic 間で共有する `ureq` HTTP POST・SSE ストリーム受信ヘルパー。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use serde_json::Value;

use crate::ai::provider::AiError;
#[cfg(test)]
use crate::ai::provider::AiStreamEvent;

const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_COLLECTED_BYTES: usize = 16 * 1024 * 1024;
const MAX_SSE_EVENTS: usize = 65_536;
const MAX_CACHED_AGENTS: usize = 8;

/// JSON POST 要求パラメータ。
pub struct HttpJsonRequest<'a> {
    /// リクエスト先完全 URL。
    pub url: &'a str,
    /// HTTP ヘッダ一覧。
    pub headers: &'a [(&'a str, &'a str)],
    /// JSON ボディ。
    pub body: &'a Value,
    /// アイドル読み取り／接続タイムアウト基準（秒）。
    pub timeout_secs: u64,
    /// 実行中キャンセル通知フラグ（任意）。
    pub cancel_flag: Option<Arc<AtomicBool>>,
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
    state: S,
    on_sse_event: FSse,
    finish_sse: FFinish,
    on_sync_json: FSync,
) -> Result<T, AiError>
where
    FSse: FnMut(&mut S, Option<&str>, &str) -> Result<bool, AiError>,
    FFinish: FnOnce(S) -> Result<T, AiError>,
    FSync: FnOnce(S, &Value) -> Result<T, AiError>,
{
    post_json_sse_or_sync_with_eof(
        request,
        state,
        on_sse_event,
        |s, _saw_done| finish_sse(s),
        on_sync_json,
    )
}

/// `[DONE]` 終端シグナルの有無も `finish_sse` へ渡す SSE / JSON POST ヘルパー。
///
/// # Errors
///
/// キャンセル・通信失敗・HTTP エラー・JSON/SSE 解析エラー時に [`AiError`] を返す。
pub fn post_json_sse_or_sync_with_eof<S, T, FSse, FFinish, FSync>(
    request: &HttpJsonRequest<'_>,
    mut state: S,
    mut on_sse_event: FSse,
    finish_sse: FFinish,
    on_sync_json: FSync,
) -> Result<T, AiError>
where
    FSse: FnMut(&mut S, Option<&str>, &str) -> Result<bool, AiError>,
    FFinish: FnOnce(S, bool) -> Result<T, AiError>,
    FSync: FnOnce(S, &Value) -> Result<T, AiError>,
{
    if let Some(flag) = request.cancel_flag.as_ref() {
        if flag.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        return run_post_cancellable(request, flag, state, on_sse_event, finish_sse, on_sync_json);
    }

    let (mut resp, content_type) = send_json_post(request, "text/event-stream, application/json")?;
    if content_type.contains("text/event-stream") {
        let saw_done = read_sse_events(resp.body_mut().as_reader(), &mut |evt, payload| {
            on_sse_event(&mut state, evt, payload)
        })?;
        finish_sse(state, saw_done)
    } else {
        let json_val: Value = resp
            .body_mut()
            .read_json()
            .map_err(|err| AiError::ParseFailed(format!("JSON response parse error: {err}")))?;
        on_sync_json(state, &json_val)
    }
}

enum CancellablePostMsg {
    SseEvent(Option<String>, String),
    SseEof { saw_done: bool },
    SyncJson(Value),
    Err(AiError),
}

fn run_post_cancellable<S, T, FSse, FFinish, FSync>(
    request: &HttpJsonRequest<'_>,
    flag: &Arc<AtomicBool>,
    mut state: S,
    mut on_sse_event: FSse,
    finish_sse: FFinish,
    on_sync_json: FSync,
) -> Result<T, AiError>
where
    FSse: FnMut(&mut S, Option<&str>, &str) -> Result<bool, AiError>,
    FFinish: FnOnce(S, bool) -> Result<T, AiError>,
    FSync: FnOnce(S, &Value) -> Result<T, AiError>,
{
    validate_http_url_and_headers(request.url, request.headers)?;
    let url = request.url.to_owned();
    let headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let body = request.body.clone();
    let timeout_secs = request.timeout_secs;
    let cancel_worker = Arc::clone(flag);
    let (tx, rx) = mpsc::sync_channel::<CancellablePostMsg>(64);

    std::thread::spawn(move || {
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let req = HttpJsonRequest {
            url: &url,
            headers: &header_refs,
            body: &body,
            timeout_secs,
            cancel_flag: None,
        };
        let (mut resp, content_type) =
            match send_json_post(&req, "text/event-stream, application/json") {
                Ok(v) => v,
                Err(err) => {
                    let _ = tx.send(CancellablePostMsg::Err(err));
                    return;
                }
            };
        if content_type.contains("text/event-stream") {
            let sse_res = read_sse_events(resp.body_mut().as_reader(), &mut |evt, payload| {
                if cancel_worker.load(Ordering::Relaxed) {
                    return Err(AiError::Cancelled);
                }
                tx.send(CancellablePostMsg::SseEvent(
                    evt.map(ToOwned::to_owned),
                    payload.to_owned(),
                ))
                .map_err(|_| AiError::Cancelled)?;
                Ok(false)
            });
            match sse_res {
                Ok(saw_done) => {
                    let _ = tx.send(CancellablePostMsg::SseEof { saw_done });
                }
                Err(err) => {
                    let _ = tx.send(CancellablePostMsg::Err(err));
                }
            }
        } else {
            match resp.body_mut().read_json::<Value>() {
                Ok(val) => {
                    let _ = tx.send(CancellablePostMsg::SyncJson(val));
                }
                Err(err) => {
                    let _ = tx.send(CancellablePostMsg::Err(AiError::ParseFailed(format!(
                        "JSON response parse error: {err}"
                    ))));
                }
            }
        }
    });

    loop {
        if flag.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(CancellablePostMsg::SseEvent(evt, payload)) => {
                if on_sse_event(&mut state, evt.as_deref(), &payload)? {
                    return finish_sse(state, true);
                }
            }
            Ok(CancellablePostMsg::SseEof { saw_done }) => {
                return finish_sse(state, saw_done);
            }
            Ok(CancellablePostMsg::SyncJson(val)) => {
                return on_sync_json(state, &val);
            }
            Ok(CancellablePostMsg::Err(err)) => return Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if flag.load(Ordering::Relaxed) {
                    return Err(AiError::Cancelled);
                }
                return Err(AiError::Transport(
                    "HTTP response stream worker disconnected unexpectedly".to_owned(),
                ));
            }
        }
    }
}

#[cfg(test)]
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

fn cached_streaming_ureq_agent(timeout_secs: u64) -> ureq::Agent {
    static STREAM_AGENTS: OnceLock<Mutex<HashMap<u64, ureq::Agent>>> = OnceLock::new();
    let clamped = timeout_secs.max(5);
    let cache = STREAM_AGENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        if let Some(existing) = guard.get(&clamped) {
            return existing.clone();
        }
        if guard.len() >= MAX_CACHED_AGENTS {
            guard.clear();
        }
        let created = build_streaming_ureq_agent(clamped);
        guard.insert(clamped, created.clone());
        return created;
    }
    build_streaming_ureq_agent(clamped)
}

fn build_streaming_ureq_agent(timeout_secs: u64) -> ureq::Agent {
    // 長時間の Thinking や大規模生成で正常チャンク受信中に 60 秒打ち切りされないよう、
    // 全体 timeout_global は設定せず接続・応答ヘッダ・チャンク間アイドルタイムアウトで管理する
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(timeout_secs.min(30))))
        .timeout_send_request(Some(Duration::from_secs(timeout_secs)))
        .timeout_send_body(Some(Duration::from_secs(timeout_secs)))
        .timeout_recv_response(Some(Duration::from_secs(timeout_secs.max(120))))
        .timeout_per_read(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
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
    let agent = cached_streaming_ureq_agent(request.timeout_secs);

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

fn read_sse_events<R, FSse>(reader: R, on_sse_event: &mut FSse) -> Result<bool, AiError>
where
    R: std::io::Read,
    FSse: FnMut(Option<&str>, &str) -> Result<bool, AiError>,
{
    read_sse_events_with_limits(
        reader,
        on_sse_event,
        MAX_SSE_LINE_BYTES,
        MAX_SSE_COLLECTED_BYTES,
        MAX_SSE_EVENTS,
    )
}

fn read_sse_events_with_limits<R, FSse>(
    reader: R,
    on_sse_event: &mut FSse,
    max_line_bytes: usize,
    max_collected_bytes: usize,
    max_events: usize,
) -> Result<bool, AiError>
where
    R: std::io::Read,
    FSse: FnMut(Option<&str>, &str) -> Result<bool, AiError>,
{
    let mut buf_reader = BufReader::new(reader);
    let mut raw_bytes = Vec::new();
    let mut event_name: Option<String> = None;
    let mut event_data = String::new();
    let mut is_first_line = true;
    let mut limits = SseStreamLimitAcc {
        total_collected_bytes: 0,
        total_events: 0,
        max_collected_bytes,
        max_events,
    };
    let line_limit = u64::try_from(max_line_bytes.saturating_add(1)).unwrap_or(u64::MAX);

    loop {
        raw_bytes.clear();
        // UTF-8 デコード前にバイト長上限を判定することで、境界跨ぎマルチバイト文字でも正しく上限エラーにする
        let read = buf_reader
            .by_ref()
            .take(line_limit)
            .read_until(b'\n', &mut raw_bytes)?;
        if read == 0 {
            if event_name.as_deref() == Some("error") || !event_data.is_empty() {
                let outcome =
                    flush_sse_event(&mut event_name, &mut event_data, on_sse_event, &mut limits)?;
                return Ok(matches!(outcome, FlushOutcome::Done));
            }
            return Ok(false);
        }
        if read > max_line_bytes && !raw_bytes.ends_with(b"\n") {
            return Err(AiError::ParseFailed(format!(
                "SSE line exceeded max size ({max_line_bytes} bytes)"
            )));
        }
        let line_owned = String::from_utf8(std::mem::take(&mut raw_bytes)).map_err(|err| {
            AiError::ParseFailed(format!("SSE stream contained invalid UTF-8: {err}"))
        })?;
        let mut raw_line = line_owned.as_str();
        if is_first_line {
            is_first_line = false;
            raw_line = raw_line.strip_prefix('\u{FEFF}').unwrap_or(raw_line);
        }
        // WHATWG SSE 仕様準拠で末尾の単一 \n / \r\n のみを除去する
        let raw_line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if raw_line.is_empty() {
            match flush_sse_event(&mut event_name, &mut event_data, on_sse_event, &mut limits)? {
                FlushOutcome::Done => return Ok(true),
                FlushOutcome::StopEarly => return Ok(false),
                FlushOutcome::Continue => continue,
            }
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
}

struct SseStreamLimitAcc {
    total_collected_bytes: usize,
    total_events: usize,
    max_collected_bytes: usize,
    max_events: usize,
}

enum FlushOutcome {
    Continue,
    StopEarly,
    Done,
}

fn flush_sse_event<FSse>(
    event_name: &mut Option<String>,
    event_data: &mut String,
    on_sse_event: &mut FSse,
    limits: &mut SseStreamLimitAcc,
) -> Result<FlushOutcome, AiError>
where
    FSse: FnMut(Option<&str>, &str) -> Result<bool, AiError>,
{
    let name = event_name.take();
    if name.as_deref() == Some("error") {
        let payload_owned = std::mem::take(event_data);
        let payload = payload_owned.trim();
        let detail = if payload.is_empty() {
            "empty error event"
        } else {
            payload
        };
        let snippet: String = detail.chars().take(600).collect();
        return Err(AiError::ParseFailed(format!("SSE error event: {snippet}")));
    }
    if event_data.is_empty() {
        return Ok(FlushOutcome::Continue);
    }
    let payload_owned = std::mem::take(event_data);
    let payload = payload_owned.trim();
    if payload.is_empty() {
        return Ok(FlushOutcome::Continue);
    }
    if payload == "[DONE]" {
        return Ok(FlushOutcome::Done);
    }
    limits.total_events = limits.total_events.saturating_add(1);
    if limits.total_events > limits.max_events {
        return Err(AiError::ParseFailed(format!(
            "SSE event count exceeded max limit ({} events)",
            limits.max_events
        )));
    }
    limits.total_collected_bytes = limits.total_collected_bytes.saturating_add(payload.len());
    if limits.total_collected_bytes > limits.max_collected_bytes {
        return Err(AiError::ParseFailed(format!(
            "SSE accumulated stream exceeded max size ({} bytes)",
            limits.max_collected_bytes
        )));
    }
    if on_sse_event(name.as_deref(), payload)? {
        Ok(FlushOutcome::StopEarly)
    } else {
        Ok(FlushOutcome::Continue)
    }
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
            cancel_flag: None,
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
            cancel_flag: None,
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

    #[test]
    fn sse_reader_enforces_limits_utf8_bom_crlf_and_empty_error_event() -> Result<(), AiError> {
        // 1. UTF-8 BOM と単一 CRLF・値末尾の \r 保持を確認
        let bom_stream = "\u{FEFF}data: {\"txt\":\"hi\r\"}\r\n\r\ndata: [DONE]\r\n\r\n";
        let mut payloads = Vec::new();
        let saw_done = read_sse_events(Cursor::new(bom_stream.as_bytes()), &mut |_, payload| {
            payloads.push(payload.to_owned());
            Ok(false)
        })?;
        assert!(saw_done);
        assert_eq!(payloads, vec!["{\"txt\":\"hi\r\"}"]);

        // 2. event: error かつ data 空でもエラー化すること
        let empty_err_stream = "event: error\n\n";
        assert!(matches!(
            read_sse_events(Cursor::new(empty_err_stream.as_bytes()), &mut |_, _| Ok(
                false
            )),
            Err(AiError::ParseFailed(_))
        ));

        // 3. マルチバイト文字が line 上限を跨いでも UTF-8 InvalidData でなく ParseFailed になること
        let mut long_utf8 = b"data: ".to_vec();
        long_utf8.extend("あ".repeat(12).as_bytes());
        long_utf8.push(b'\n');
        let err_line = read_sse_events_with_limits(
            Cursor::new(long_utf8),
            &mut |_, _| Ok(false),
            20,
            4096,
            100,
        );
        assert!(
            matches!(err_line, Err(AiError::ParseFailed(ref m)) if m.contains("max size")),
            "expected ParseFailed(max size), got: {err_line:?}"
        );

        // 4. 累積ペイロード上限超過で ParseFailed になること
        let many_chunks = "data: 1234567890\n\ndata: 1234567890\n\n";
        let err_total = read_sse_events_with_limits(
            Cursor::new(many_chunks.as_bytes()),
            &mut |_, _| Ok(false),
            1024,
            15,
            100,
        );
        assert!(matches!(err_total, Err(AiError::ParseFailed(_))));
        Ok(())
    }

    #[test]
    fn post_json_sse_aborts_promptly_when_cancel_flag_is_set()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let srv = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"n\":1}\n\n";
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.flush();
                std::thread::sleep(Duration::from_secs(3));
            }
        });

        let flag = Arc::new(AtomicBool::new(false));
        let flag_setter = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            flag_setter.store(true, Ordering::Relaxed);
        });

        let url = format!("http://{addr}/v1/cancel");
        let body = serde_json::json!({});
        let req = HttpJsonRequest {
            url: &url,
            headers: &[],
            body: &body,
            timeout_secs: 10,
            cancel_flag: Some(Arc::clone(&flag)),
        };
        let started = std::time::Instant::now();
        let res = post_json_sse_or_sync::<_, (), _, _, _>(
            &req,
            (),
            |(), _, _| Ok(false),
            |()| Ok(()),
            |(), _| Ok(()),
        );
        assert!(matches!(res, Err(AiError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = srv.join();
        Ok(())
    }
}
