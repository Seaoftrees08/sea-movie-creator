//! `sea-movie-creator mcp --project <path>` のローカル MCP サーバー (`stdio` 主体 + ループバック HTTP Bearer 認証)。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use thiserror::Error;

use crate::ai::context_builder::{build_command_schema_value, build_timeline_context};
use crate::ai::planner::dry_run_validate;
use crate::core::ids::{ClipId, LayerId};
use crate::core::time::FrameIndex;
use crate::timeline::command::EditCommand;
use crate::timeline::dispatcher::CommandDispatcher;
use crate::timeline::document::ProjectDocument;
use crate::timeline::serialization::{SerializationError, load_project_file, save_project_file};

/// MCP サーバー起動・実行時に発生し得るエラー。
#[derive(Debug, Error)]
pub enum McpServerError {
    /// I/O エラー。
    #[error("MCP I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// プロジェクト読込・保存エラー。
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    /// JSON 解析エラー。
    #[error("MCP JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// 非ループバックアドレスへのバインド拒否。
    #[error("loopback HTTP MCP server must bind exclusively to 127.0.0.1 / ::1")]
    NonLoopbackForbidden,
}

/// MCP ツール状態（タイムラインドキュメント、`CommandDispatcher`、保存先パス）。
#[derive(Debug)]
pub struct McpServerState {
    document: ProjectDocument,
    dispatcher: CommandDispatcher,
    project_path: Option<PathBuf>,
}

impl McpServerState {
    /// 既存ドキュメントと任意保存パスから MCP 状態を構築する。
    #[must_use]
    pub fn new(document: ProjectDocument, project_path: Option<PathBuf>) -> Self {
        Self {
            document,
            dispatcher: CommandDispatcher::new(),
            project_path,
        }
    }

    /// プロジェクトファイルから状態を読み込む（未指定時は新規ドキュメントを使用）。
    ///
    /// # Errors
    ///
    /// ファイル読み込みまたはドキュメント検証に失敗した場合。
    pub fn load_or_new(project_path: Option<&Path>) -> Result<Self, McpServerError> {
        match project_path {
            Some(path) => {
                let doc = load_project_file(path)?;
                Ok(Self::new(doc, Some(path.to_path_buf())))
            }
            None => Ok(Self::new(
                ProjectDocument::new("Untitled MCP Project"),
                None,
            )),
        }
    }

    /// 現在のプロジェクトドキュメント参照を返す。
    #[must_use]
    pub const fn document(&self) -> &ProjectDocument {
        &self.document
    }

    /// 単一の JSON-RPC 2.0 メッセージを処理し、応答が必要なら JSON 値を返す。
    #[must_use]
    pub fn handle_message(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return id
                .as_ref()
                .map(|req_id| jsonrpc_err(req_id, -32600, "missing jsonrpc method"));
        };

        if id.is_none() || method.starts_with("notifications/") {
            return None;
        }
        let req_id = id.unwrap_or(Value::Null);

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "sea-movie-creator",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tool_list_response()),
            "tools/call" => self.handle_tool_call(msg.get("params").unwrap_or(&Value::Null)),
            _ => {
                return Some(jsonrpc_err(
                    &req_id,
                    -32601,
                    &format!("method not found: {method}"),
                ));
            }
        };

        Some(match result {
            Ok(val) => json!({ "jsonrpc": "2.0", "id": req_id, "result": val }),
            Err((code, message)) => jsonrpc_err(&req_id, code, &message),
        })
    }

    fn handle_tool_call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params["name"]
            .as_str()
            .ok_or((-32602, "tools/call requires params.name".to_owned()))?;
        let args = params.get("arguments").unwrap_or(&Value::Null);

        let payload = match name {
            "get_timeline_summary" => self.tool_get_timeline_summary(args),
            "get_command_schema" => self.tool_get_command_schema(),
            "preview_edit_command" => self.tool_preview_command(args),
            "apply_edit_command" => self.tool_apply_command(args),
            "undo" => self.tool_undo_redo(true),
            "redo" => self.tool_undo_redo(false),
            other => {
                return Err((-32602, format!("unknown MCP tool `{other}`")));
            }
        };

        Ok(match payload {
            Ok(data) => tool_ok_envelope(&data),
            Err(err_text) => tool_error_envelope(&err_text),
        })
    }

    fn tool_get_timeline_summary(&self, args: &Value) -> Result<Value, String> {
        let playhead = FrameIndex::new(args["playhead_frame"].as_u64().unwrap_or(0));
        let clip = args["selected_clip_id"]
            .as_str()
            .and_then(|s| ClipId::new(s).ok());
        let layer = args["selected_layer_id"]
            .as_str()
            .and_then(|s| LayerId::new(s).ok());
        let summary =
            build_timeline_context(&self.document, playhead, clip.as_ref(), layer.as_ref());
        serde_json::to_value(summary).map_err(|e| e.to_string())
    }

    fn tool_get_command_schema(&self) -> Result<Value, String> {
        let _ = &self.document;
        build_command_schema_value().map_err(|e| e.to_string())
    }

    fn tool_preview_command(&self, args: &Value) -> Result<Value, String> {
        let raw_cmd = extract_command_json(args)?;
        let plan = dry_run_validate(&self.document, &raw_cmd).map_err(|e| e.to_string())?;
        Ok(json!({
            "valid": true,
            "summary_lines": plan.summary_lines,
            "affected_targets": plan.affected_targets,
        }))
    }

    fn tool_apply_command(&mut self, args: &Value) -> Result<Value, String> {
        let cmd: EditCommand =
            serde_json::from_value(args["command"].clone()).map_err(|e| e.to_string())?;
        let report = self
            .dispatcher
            .dispatch(&mut self.document, &cmd)
            .map_err(|e| e.to_string())?;
        self.persist_if_configured()?;
        Ok(json!({
            "applied": true,
            "created_assets": report.created_assets.len(),
            "created_layers": report.created_layers.len(),
            "created_clips": report.created_clips.len(),
            "total_duration_frames": self.document.total_duration_frames(),
        }))
    }

    fn tool_undo_redo(&mut self, is_undo: bool) -> Result<Value, String> {
        let performed = if is_undo {
            self.dispatcher
                .undo(&mut self.document)
                .map_err(|e| e.to_string())?
        } else {
            self.dispatcher
                .redo(&mut self.document)
                .map_err(|e| e.to_string())?
        };
        let done = performed.is_some();
        if done {
            self.persist_if_configured()?;
        }
        Ok(json!({
            "performed": done,
            "total_duration_frames": self.document.total_duration_frames(),
        }))
    }

    fn persist_if_configured(&self) -> Result<(), String> {
        if let Some(path) = &self.project_path {
            save_project_file(&self.document, path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn extract_command_json(args: &Value) -> Result<String, String> {
    let val = args
        .get("command")
        .filter(|v| !v.is_null())
        .ok_or_else(|| "missing `arguments.command`".to_owned())?;
    if let Some(s) = val.as_str() {
        Ok(s.to_owned())
    } else {
        serde_json::to_string(val).map_err(|e| e.to_string())
    }
}

fn tool_ok_envelope(val: &Value) -> Value {
    let text = serde_json::to_string(val).unwrap_or_else(|_| "{}".to_owned());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

fn tool_error_envelope(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn jsonrpc_err(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_list_response() -> Value {
    json!({
        "tools": [
            {
                "name": "get_timeline_summary",
                "description": "現在のタイムライン状態（トラック・クリップ・素材・再生ヘッド）のコンパクト要約を返します。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "playhead_frame": { "type": "integer", "minimum": 0 },
                        "selected_clip_id": { "type": "string" },
                        "selected_layer_id": { "type": "string" }
                    }
                }
            },
            {
                "name": "get_command_schema",
                "description": "EditCommand 統一コマンド配列の JSON Schema を返します。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "preview_edit_command",
                "description": "タイムラインを書き換えずに EditCommand の事前ドライラン検証と差分要約を行います。",
                "inputSchema": {
                    "type": "object",
                    "required": ["command"],
                    "properties": { "command": { "type": "object" } }
                }
            },
            {
                "name": "apply_edit_command",
                "description": "EditCommand を CommandDispatcher 経由で単一 Undo 単位として実行し、必要に応じてプロジェクトファイルへ保存します。",
                "inputSchema": {
                    "type": "object",
                    "required": ["command"],
                    "properties": { "command": { "type": "object" } }
                }
            },
            {
                "name": "undo",
                "description": "直前の EditCommand 操作を 1 ステップ取り消します。",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "redo",
                "description": "取り消した EditCommand 操作を 1 ステップやり直します。",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

/// 標準入出力 (`NDJSON` / `Content-Length` 両対応) 上で MCP サーバーを実行する。
///
/// # Errors
///
/// ストリーム読み書きに失敗した場合に [`McpServerError`] を返す。
pub fn run_mcp_stdio<R: BufRead, W: Write>(
    state: &mut McpServerState,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), McpServerError> {
    while let Some((frame, uses_content_length)) = read_mcp_frame(reader)? {
        let msg: Value = serde_json::from_str(&frame)?;
        if let Some(resp) = state.handle_message(&msg) {
            let encoded = serde_json::to_string(&resp)?;
            if uses_content_length {
                write!(
                    writer,
                    "Content-Length: {}\r\n\r\n{}",
                    encoded.len(),
                    encoded
                )?;
            } else {
                writeln!(writer, "{encoded}")?;
            }
            writer.flush()?;
        }
    }
    Ok(())
}

fn read_mcp_frame<R: BufRead>(reader: &mut R) -> Result<Option<(String, bool)>, McpServerError> {
    let mut first_line = String::new();
    loop {
        first_line.clear();
        let read = reader.read_line(&mut first_line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = first_line.trim();
        if !trimmed.is_empty() {
            break;
        }
    }

    let trimmed = first_line.trim();
    if let Some((key, val)) = trimmed.split_once(':')
        && key.trim().eq_ignore_ascii_case("content-length")
    {
        let mut content_len = val.trim().parse::<usize>().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid Content-Length")
        })?;
        let mut header_line = String::new();
        loop {
            header_line.clear();
            let n = reader.read_line(&mut header_line)?;
            if n == 0 || header_line.trim().is_empty() {
                break;
            }
            if let Some((hk, hv)) = header_line.split_once(':')
                && hk.trim().eq_ignore_ascii_case("content-length")
                && let Ok(parsed) = hv.trim().parse::<usize>()
            {
                content_len = parsed;
            }
        }
        let mut body = vec![0_u8; content_len];
        reader.read_exact(&mut body)?;
        let text = String::from_utf8(body).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "MCP body must be UTF-8")
        })?;
        return Ok(Some((text, true)));
    }

    Ok(Some((trimmed.to_owned(), false)))
}

/// 起動ごとのランダム Bearer トークンを生成する。
#[must_use]
pub fn generate_bearer_token() -> String {
    let mut parts = [0_u64; 4];
    for (idx, slot) in parts.iter_mut().enumerate() {
        let rs = RandomState::new();
        let mut hasher = rs.build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_usize(idx);
        *slot = hasher.finish();
    }
    format!(
        "sea_mcp_{:016x}{:016x}{:016x}{:016x}",
        parts[0], parts[1], parts[2], parts[3]
    )
}

/// `127.0.0.1` ループバック限定・起動毎 Bearer トークン認証付き HTTP MCP サーバー。
pub struct McpLoopbackHttpServer {
    addr: SocketAddr,
    bearer_token: String,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for McpLoopbackHttpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpLoopbackHttpServer")
            .field("addr", &self.addr)
            .field("bearer_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl McpLoopbackHttpServer {
    /// `127.0.0.1` の任意空きポートまたは指定ループバックアドレスへ HTTP MCP サーバーを起動する。
    ///
    /// # Errors
    ///
    /// 非ループバックアドレス指定またはソケットバインド失敗時に [`McpServerError`] を返す。
    pub fn bind(bind_addr: SocketAddr, state: McpServerState) -> Result<Self, McpServerError> {
        if !bind_addr.ip().is_loopback() {
            return Err(McpServerError::NonLoopbackForbidden);
        }
        let listener = TcpListener::bind(bind_addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let bearer_token = generate_bearer_token();
        let expected_auth = format!("Bearer {bearer_token}");
        let shared_state = Arc::new(Mutex::new(state));
        let running = Arc::new(AtomicBool::new(true));
        let running_flag = Arc::clone(&running);

        let handle = thread::spawn(move || {
            while running_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_http_client(stream, &expected_auth, &shared_state);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            addr,
            bearer_token,
            running,
            handle: Some(handle),
        })
    }

    /// バインド済みローカルアドレス (`127.0.0.1:PORT`) を返す。
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// この起動インスタンスに発行された Bearer トークンを返す。
    #[must_use]
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }
}

impl Drop for McpLoopbackHttpServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_http_client(
    mut stream: TcpStream,
    expected_auth: &str,
    state: &Arc<Mutex<McpServerState>>,
) -> Result<(), std::io::Error> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let (method, auth_header, body) = read_http_request(&mut stream)?;

    if !auth_header
        .as_deref()
        .is_some_and(|hdr| constant_time_eq(hdr.as_bytes(), expected_auth.as_bytes()))
    {
        return write_http_response(
            &mut stream,
            "401 Unauthorized",
            r#"{"error":"missing or invalid Bearer token"}"#,
        );
    }

    if !method.eq_ignore_ascii_case("POST") {
        return write_http_response(
            &mut stream,
            "405 Method Not Allowed",
            r#"{"error":"only POST is accepted"}"#,
        );
    }

    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return write_http_response(
            &mut stream,
            "400 Bad Request",
            r#"{"error":"invalid JSON-RPC body"}"#,
        );
    };

    let response_opt = state
        .lock()
        .map_err(|_| std::io::Error::other("MCP state mutex poisoned"))?
        .handle_message(&msg);

    if let Some(resp) = response_opt {
        let encoded = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_owned());
        write_http_response(&mut stream, "200 OK", &encoded)
    } else {
        write_http_response(&mut stream, "204 No Content", "")
    }
}

fn read_http_request(
    stream: &mut TcpStream,
) -> Result<(String, Option<String>, Vec<u8>), std::io::Error> {
    let mut raw = Vec::new();
    let mut buf = [0_u8; 4096];
    let mut header_end = None;
    let mut expected_total = None;

    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if header_end.is_none()
            && let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n")
        {
            let end = pos + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&raw[..end]);
            let len = parse_content_length(&headers).unwrap_or(0);
            expected_total = Some(end + len);
        }
        if let Some(total) = expected_total
            && raw.len() >= total
        {
            break;
        }
    }

    let end = header_end.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "incomplete HTTP headers")
    })?;
    let header_str = String::from_utf8_lossy(&raw[..end]);
    let mut lines = header_str.lines();
    let req_line = lines.next().unwrap_or("");
    let method = req_line.split_whitespace().next().unwrap_or("").to_owned();
    let auth_header = lines.find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("authorization")
            .then(|| v.trim().to_owned())
    });
    let total = expected_total.unwrap_or(end).min(raw.len());
    Ok((method, auth_header, raw[end..total].to_vec()))
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse::<usize>().ok())
            .flatten()
    })
}

fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), std::io::Error> {
    let raw = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(raw.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn mcp_stdio_supports_initialize_preview_apply_and_undo()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = McpServerState::new(ProjectDocument::new("MCP Test"), None);
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let schema_req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_command_schema","arguments":{}}}"#;
        let apply_req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"apply_edit_command","arguments":{"command":{"type":"add_layer","name":"MCP Layer"}}}}"#;
        let undo_req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"undo","arguments":{}}}"#;

        let input = format!("{init_req}\n{schema_req}\n{apply_req}\n{undo_req}\n");
        let mut reader = Cursor::new(input.into_bytes());
        let mut out = Vec::new();
        run_mcp_stdio(&mut state, &mut reader, &mut out)?;

        let out_str = String::from_utf8(out)?;
        let responses: Vec<Value> = out_str
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(responses.len(), 4);
        assert_eq!(
            responses[0]["result"]["serverInfo"]["name"].as_str(),
            Some("sea-movie-creator")
        );
        assert_eq!(responses[2]["result"]["isError"].as_bool(), Some(false));
        assert_eq!(responses[3]["result"]["isError"].as_bool(), Some(false));
        assert_eq!(state.document().layers.len(), 0);
        Ok(())
    }

    #[test]
    fn mcp_loopback_http_enforces_bearer_token() -> Result<(), Box<dyn std::error::Error>> {
        let state = McpServerState::new(ProjectDocument::new("HTTP MCP"), None);
        let server = McpLoopbackHttpServer::bind("127.0.0.1:0".parse()?, state)?;
        let url = format!("http://{}/mcp", server.addr());

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let unauth_resp = agent
            .post(&url)
            .send_json(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))?;
        assert_eq!(unauth_resp.status(), 401);

        let mut auth_resp = agent
            .post(&url)
            .header(
                "Authorization",
                &format!("Bearer {}", server.bearer_token()),
            )
            .send_json(json!({"jsonrpc":"2.0","id":2,"method":"ping"}))?;
        assert_eq!(auth_resp.status(), 200);
        let body: Value = auth_resp.body_mut().read_json()?;
        assert_eq!(body["id"], 2);
        Ok(())
    }
}
