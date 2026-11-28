//! `sea-movie-creator` 用 Agent Client Protocol (`ACP` over stdio JSON-RPC 2.0) アダプタプラグイン。
//!
//! `claude` / `codex` / `copilot --acp --stdio` 等の外部エージェントと標準入出力 JSON-RPC で対話します。

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, ExitCode, Stdio};

fn apply_background_process_flags(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

#[cfg(test)]
use sea_ai_protocol::ORIGINAL_INSTRUCTION_MARKER;
#[cfg(test)]
use sea_ai_protocol::extract_acp_update_message_delta;
use sea_ai_protocol::{
    AcpImageInput, AcpSessionUpdateEvent, ChildKillGuard, HistoryMessage, MAX_AI_NDJSON_LINE_BYTES,
    MAX_HISTORY_PROMPT_BYTES, acp_supports_image_prompt, build_acp_prompt_content_blocks,
    build_acp_server_request_deny_response, extract_acp_update_event,
    format_conversation_history_section, is_empty_json_value, read_ndjson_line_limited,
    spawn_stderr_drain, truncate_utf8_chars, validate_acp_images,
};
use serde::Deserialize;
use serde_json::{Value, json};

const STDERR_DRAIN_CAP_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct PluginRunError {
    code: &'static str,
    message: String,
}

impl PluginRunError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn exec(message: impl Into<String>) -> Self {
        Self::new("execution_failed", message)
    }
}

impl From<String> for PluginRunError {
    fn from(message: String) -> Self {
        Self::exec(message)
    }
}

#[derive(Debug, Deserialize)]
struct RequestEnvelope {
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    params: CallParams,
}

fn default_method() -> String {
    "generate_command".to_owned()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CallParams {
    system_prompt: String,
    command_schema_json: Value,
    context_summary: Value,
    history: Vec<HistoryMessage>,
    user_prompt: String,
    model: Option<String>,
    external_cli: Option<String>,
    external_args: Vec<String>,
    images: Vec<AcpImageInput>,
}

fn emit_event(event: &Value) {
    if let Ok(line) = serde_json::to_string(event) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn resolve_agent_command(params: &CallParams) -> Result<(PathBuf, Vec<String>, String), String> {
    let profile = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude");

    let (default_bin, default_args): (&str, Vec<String>) = match profile {
        "codex" => ("codex", vec!["--acp".to_owned()]),
        "copilot" => ("copilot", vec!["--acp".to_owned(), "--stdio".to_owned()]),
        _ => ("claude", vec!["--acp".to_owned()]),
    };

    let cli_name = params
        .external_cli
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("SEA_ACP_CMD")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| default_bin.to_owned());

    let args = if !params.external_args.is_empty() {
        params.external_args.clone()
    } else if let Ok(raw) = env::var("SEA_ACP_ARGS")
        && !raw.trim().is_empty()
    {
        raw.split_whitespace().map(ToOwned::to_owned).collect()
    } else {
        default_args
    };

    let resolved = resolve_executable_in_path(&cli_name).ok_or_else(|| {
        format!(
            "ACP 外部エージェント CLI (`{cli_name}`) が見つかりませんでした。\
             プロファイル `{profile}` 用の CLI をインストールして PATH を設定してください。"
        )
    })?;

    Ok((resolved, args, profile.to_owned()))
}

fn resolve_executable_in_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    let path_var = env::var_os("PATH")?;
    let mut suffixes = vec![String::new()];
    if cfg!(windows) {
        suffixes.extend([".exe".to_owned(), ".cmd".to_owned(), ".bat".to_owned()]);
    }
    for dir in env::split_paths(&path_var) {
        for ext in &suffixes {
            let full = if name.ends_with(ext.as_str()) {
                dir.join(name)
            } else {
                dir.join(format!("{name}{ext}"))
            };
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn build_initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "sea-movie-creator",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    })
}

fn build_acp_prompt_text(params: &CallParams) -> String {
    let mut sections = vec![params.system_prompt.clone()];
    if !is_empty_json_value(&params.command_schema_json) {
        sections.push(format!(
            "# Required Output JSON Schema\n{}",
            params.command_schema_json
        ));
    }
    if !is_empty_json_value(&params.context_summary) {
        sections.push(format!("# Timeline Context\n{}", params.context_summary));
    }
    let history_section =
        format_conversation_history_section(&params.history, MAX_HISTORY_PROMPT_BYTES);
    if !history_section.is_empty() {
        sections.push(history_section);
    }
    sections.push(format!("# Instruction\n{}", params.user_prompt));
    sections.join("\n\n")
}

#[cfg(test)]
fn extract_acp_update_text(msg: &Value) -> Option<String> {
    if msg["method"].as_str() != Some("session/update") {
        return None;
    }
    extract_acp_update_message_delta(&msg["params"])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpStreamPhase {
    Waiting,
    Thinking,
    Responding,
}

fn format_acp_activity_label(title: &str, status: Option<&str>) -> String {
    match status {
        Some(st) => format!("{title} ({st})"),
        None => title.to_owned(),
    }
}

fn process_acp_update_msg(
    msg: &Value,
    phase: &mut AcpStreamPhase,
    streamed: &mut String,
    emit: &mut dyn FnMut(Value),
) -> bool {
    if msg["method"].as_str() != Some("session/update") {
        return false;
    }
    match extract_acp_update_event(&msg["params"]) {
        Some(AcpSessionUpdateEvent::ThoughtDelta(chunk)) => {
            if *phase != AcpStreamPhase::Thinking {
                *phase = AcpStreamPhase::Thinking;
                emit(json!({ "event": "status", "message": "思考中..." }));
            }
            emit(json!({ "event": "thought", "text": chunk }));
            true
        }
        Some(AcpSessionUpdateEvent::MessageDelta(chunk)) => {
            if *phase != AcpStreamPhase::Responding {
                *phase = AcpStreamPhase::Responding;
                emit(json!({ "event": "status", "message": "応答を生成中..." }));
            }
            streamed.push_str(&chunk);
            emit(json!({ "event": "delta", "text": chunk }));
            true
        }
        Some(AcpSessionUpdateEvent::ToolActivity { title, status }) => {
            let label = format_acp_activity_label(&title, status.as_deref());
            emit(json!({ "event": "activity", "label": label }));
            true
        }
        None => false,
    }
}

fn write_jsonrpc_line(stdin: &mut ChildStdin, payload: &Value) -> Result<(), String> {
    let text =
        serde_json::to_string(payload).map_err(|err| format!("ACP JSON エンコード失敗: {err}"))?;
    writeln!(stdin, "{text}")
        .and_then(|()| stdin.flush())
        .map_err(|err| format!("ACP stdin 書き込み失敗: {err}"))
}

fn handle_server_request(stdin: &mut ChildStdin, msg: &Value) -> Result<bool, String> {
    let Some(reply) = build_acp_server_request_deny_response(msg) else {
        return Ok(false);
    };
    write_jsonrpc_line(stdin, &reply)?;
    Ok(true)
}

fn read_until_response<R: BufRead>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    expected_id: u64,
    streamed: &mut String,
) -> Result<Value, String> {
    let mut phase = AcpStreamPhase::Waiting;
    loop {
        let Some(line) = read_ndjson_line_limited(reader, MAX_AI_NDJSON_LINE_BYTES)
            .map_err(|err| format!("ACP stdout 読取エラー: {err}"))?
        else {
            return Err(format!(
                "ACP エージェントが id={expected_id} 応答前に終了しました"
            ));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed).map_err(|err| {
            format!(
                "ACP JSON 解析エラー: {err} (line: {})",
                truncate_utf8_chars(trimmed, 200)
            )
        })?;

        if handle_server_request(stdin, &msg)? {
            continue;
        }
        if process_acp_update_msg(&msg, &mut phase, streamed, &mut |ev| emit_event(&ev)) {
            continue;
        }

        if msg["id"].as_u64() == Some(expected_id) {
            if let Some(err_obj) = msg.get("error").filter(|v| !v.is_null()) {
                return Err(format!("ACP エラー応答: {err_obj}"));
            }
            return Ok(msg);
        }
    }
}

fn extract_acp_usage(prompt_resp: &Value) -> Option<Value> {
    let usage = prompt_resp["result"].get("usage")?;
    let obj = usage.as_object()?;
    let input_tokens = obj
        .get("inputTokens")
        .or_else(|| obj.get("input_tokens"))
        .or_else(|| obj.get("promptTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = obj
        .get("outputTokens")
        .or_else(|| obj.get("output_tokens"))
        .or_else(|| obj.get("completionTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = obj
        .get("totalTokens")
        .or_else(|| obj.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }
    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cost_usd": null,
        "is_estimated": false,
    }))
}

fn probe_acp_profiles(params: &CallParams) -> Vec<Value> {
    let profiles: [(&str, &str, &str, &[&str]); 3] = [
        ("claude", "Claude Code (ACP)", "claude", &[]),
        (
            "codex",
            "OpenAI Codex CLI (ACP)",
            "codex",
            &["auto", "low", "medium", "high"],
        ),
        ("copilot", "GitHub Copilot CLI (ACP)", "copilot", &[]),
    ];
    let explicit_cli = params
        .external_cli
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    profiles
        .iter()
        .map(|(id, label, default_bin, levels)| {
            let candidate = explicit_cli.unwrap_or(default_bin);
            let detected = resolve_executable_in_path(candidate);
            let available = detected.is_some();
            let note = if let Some(path) = detected {
                format!("ACP 利用可能 ({})", path.display())
            } else {
                format!("`{candidate}` が PATH 上に未検出")
            };
            let default_thinking = if levels.is_empty() { "auto" } else { "medium" };
            json!({
                "id": id,
                "display_name": label,
                "badge": null,
                "thinking_levels": levels,
                "default_thinking": default_thinking,
                "available": available,
                "note": note,
            })
        })
        .collect()
}

fn run_list_models(params: &CallParams) {
    let models = probe_acp_profiles(params);
    emit_event(&json!({
        "event": "models",
        "models": models,
    }));
}

fn ensure_acp_vision_supported(
    init_resp: &Value,
    images: &[AcpImageInput],
) -> Result<(), PluginRunError> {
    if !images.is_empty() && acp_supports_image_prompt(init_resp) == Some(false) {
        return Err(PluginRunError::new(
            "vision_unsupported",
            "接続先の ACP エージェントは画像入力 (promptCapabilities.image) に非対応です",
        ));
    }
    Ok(())
}

fn build_acp_prompt_request(id: u64, session_id: &str, params: &CallParams) -> Value {
    let prompt_text = build_acp_prompt_text(params);
    let blocks = build_acp_prompt_content_blocks(&prompt_text, &params.images);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": blocks
        }
    })
}

fn execute_acp_session_io(
    reader: &mut BufReader<std::process::ChildStdout>,
    child_stdin: &mut ChildStdin,
    params: &CallParams,
    streamed: &mut String,
) -> Result<Value, PluginRunError> {
    write_jsonrpc_line(child_stdin, &build_initialize_request(1))?;
    let init_resp = read_until_response(reader, child_stdin, 1, streamed)?;
    ensure_acp_vision_supported(&init_resp, &params.images)?;

    let cwd = env::current_dir().map_or_else(|_| ".".to_owned(), |p| p.display().to_string());
    write_jsonrpc_line(
        child_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": cwd, "mcpServers": [] }
        }),
    )?;
    let session_resp = read_until_response(reader, child_stdin, 2, streamed)?;
    let session_id = session_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or("default-session")
        .to_owned();

    emit_event(&json!({
        "event": "status",
        "message": "プロンプトを送信して応答待機中...",
    }));
    write_jsonrpc_line(
        child_stdin,
        &build_acp_prompt_request(3, &session_id, params),
    )?;
    read_until_response(reader, child_stdin, 3, streamed).map_err(PluginRunError::from)
}

fn run_acp_session(params: &CallParams) -> Result<(), PluginRunError> {
    if !params.images.is_empty() {
        validate_acp_images(&params.images)?;
    }
    let (agent_bin, agent_args, profile) = resolve_agent_command(params)
        .map_err(|msg| PluginRunError::new("missing_external_cli", msg))?;

    emit_event(&json!({
        "event": "status",
        "message": format!("ACP エージェント ({}) に接続中...", agent_bin.display()),
    }));

    let mut cmd = Command::new(&agent_bin);
    apply_background_process_flags(&mut cmd);
    let child = cmd
        .args(&agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| PluginRunError::exec(format!("ACP エージェント起動失敗: {err}")))?;
    let mut guard = ChildKillGuard::new(child);
    let stderr_handle = guard
        .take_stderr()
        .map(|stderr| spawn_stderr_drain(stderr, STDERR_DRAIN_CAP_BYTES));

    let mut child_stdin = guard
        .take_stdin()
        .ok_or_else(|| PluginRunError::exec("ACP stdin パイプ取得失敗"))?;
    let stdout = guard
        .take_stdout()
        .ok_or_else(|| PluginRunError::exec("ACP stdout パイプ取得失敗"))?;
    let mut reader = BufReader::new(stdout);
    let mut streamed = String::new();

    let conv_res = execute_acp_session_io(&mut reader, &mut child_stdin, params, &mut streamed);
    drop(child_stdin);
    drop(reader);

    if conv_res.is_err() {
        guard.kill_and_wait();
    }
    let status =
        if conv_res.is_ok() {
            Some(guard.wait_status().map_err(|err| {
                PluginRunError::exec(format!("ACP プロセス終了待機エラー: {err}"))
            })?)
        } else {
            None
        };
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let prompt_resp = match conv_res {
        Ok(resp) => resp,
        Err(mut err) => {
            if !stderr.is_empty() {
                err.message = format!("{} ({stderr})", err.message);
            }
            return Err(err);
        }
    };
    if let Some(st) = status
        && !st.success()
    {
        return Err(PluginRunError::exec(format!(
            "ACP エージェントが {st} で終了しました: {stderr}"
        )));
    }

    let raw_text = if streamed.is_empty() {
        prompt_resp["result"]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    } else {
        streamed
    };
    let mut completed = json!({
        "event": "completed",
        "raw_text": raw_text,
        "model": format!("acp:{profile}"),
    });
    if let Some(usage) = extract_acp_usage(&prompt_resp) {
        completed["usage"] = usage;
    }
    emit_event(&completed);
    Ok(())
}

fn main() -> ExitCode {
    let mut input = String::new();
    let mut stdin = std::io::stdin().lock();
    if let Err(err) = stdin.read_line(&mut input) {
        emit_event(&json!({ "event": "error", "code": "io_error", "message": err.to_string() }));
        return ExitCode::FAILURE;
    }
    let req: RequestEnvelope = match serde_json::from_str(input.trim()) {
        Ok(parsed) => parsed,
        Err(err) => {
            emit_event(&json!({
                "event": "error",
                "code": "invalid_request",
                "message": format!("無効な入力 JSON: {err}"),
            }));
            return ExitCode::FAILURE;
        }
    };
    if req.method == "list_models" {
        run_list_models(&req.params);
        return ExitCode::SUCCESS;
    }
    if let Err(err) = run_acp_session(&req.params) {
        emit_event(&json!({ "event": "error", "code": err.code, "message": err.message }));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_initialize_and_prompt_payload_shape() {
        let init = build_initialize_request(1);
        assert_eq!(init["method"].as_str(), Some("initialize"));
        assert_eq!(init["params"]["protocolVersion"].as_u64(), Some(1));
    }

    #[test]
    fn extract_acp_update_text_reads_agent_message_chunk() {
        let note = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "{\"type\":\"undo\"}" }
                }
            }
        });
        assert_eq!(
            extract_acp_update_text(&note).as_deref(),
            Some("{\"type\":\"undo\"}")
        );
    }

    #[test]
    fn list_models_request_and_acp_profile_probe_work() {
        let req: RequestEnvelope =
            serde_json::from_str(r#"{"protocol":"sea_ai_v1","method":"list_models","params":{}}"#)
                .expect("valid list_models json");
        assert_eq!(req.method, "list_models");
        let profiles = probe_acp_profiles(&req.params);
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0]["id"], json!("claude"));
        assert_eq!(profiles[1]["id"], json!("codex"));
        assert_eq!(profiles[2]["id"], json!("copilot"));

        let usage = extract_acp_usage(&json!({
            "result": { "usage": { "input_tokens": 18, "output_tokens": 7 } }
        }));
        assert_eq!(
            usage.as_ref().and_then(|u| u["total_tokens"].as_u64()),
            Some(25)
        );
    }

    #[test]
    fn extract_acp_update_text_filters_non_message_chunks() {
        let thought = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "internal thought" }
                }
            }
        });
        assert_eq!(extract_acp_update_text(&thought), None);

        let tool_call = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "content": { "type": "text", "text": "tool running" }
                }
            }
        });
        assert_eq!(extract_acp_update_text(&tool_call), None);
    }

    #[test]
    fn build_acp_prompt_expands_history_and_omits_empty_sections() {
        let params = CallParams {
            system_prompt: "SYS".to_owned(),
            command_schema_json: json!({}),
            context_summary: Value::Null,
            history: vec![
                HistoryMessage {
                    role: "user".to_owned(),
                    content: format!("{ORIGINAL_INSTRUCTION_MARKER}\n最初の指示"),
                },
                HistoryMessage {
                    role: "assistant".to_owned(),
                    content: "中間応答".to_owned(),
                },
            ],
            user_prompt: "最新のツール結果".to_owned(),
            ..CallParams::default()
        };
        let prompt = build_acp_prompt_text(&params);
        assert!(!prompt.contains("# EditCommand JSON Schema"));
        assert!(!prompt.contains("# Timeline Context"));
        assert!(prompt.contains("# Conversation History"));
        assert!(prompt.contains("最初の指示"));
        assert!(prompt.contains("# Instruction\n最新のツール結果"));

        // 128 KiB 圧縮で元指示がピン留めされること
        let huge_ctx = format!("# Session Context\n{}", "あ".repeat(40_000));
        let huge_tool = format!("[ToolResult scan status=OK]\n{}", "い".repeat(30_000));
        let section = format_conversation_history_section(
            &[
                HistoryMessage {
                    role: "user".to_owned(),
                    content: huge_ctx,
                },
                HistoryMessage {
                    role: "user".to_owned(),
                    content: format!("{ORIGINAL_INSTRUCTION_MARKER}\nピン留め指示"),
                },
                HistoryMessage {
                    role: "user".to_owned(),
                    content: huge_tool,
                },
                HistoryMessage {
                    role: "user".to_owned(),
                    content: "[ToolResult latest status=OK]\n{\"ok\":true}".to_owned(),
                },
            ],
            MAX_HISTORY_PROMPT_BYTES,
        );
        assert!(section.len() <= MAX_HISTORY_PROMPT_BYTES);
        assert!(section.contains("ピン留め指示"));
    }

    #[test]
    fn process_acp_update_msg_transitions_phases_and_separates_events() {
        let mut phase = AcpStreamPhase::Waiting;
        let mut streamed = String::new();
        let mut events = Vec::new();

        let thought_msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "字幕を確認中" }
                }
            }
        });
        assert!(process_acp_update_msg(
            &thought_msg,
            &mut phase,
            &mut streamed,
            &mut |v| events.push(v),
        ));
        assert_eq!(phase, AcpStreamPhase::Thinking);

        let tool_msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "title": "Inspecting clips",
                    "status": "in_progress"
                }
            }
        });
        assert!(process_acp_update_msg(
            &tool_msg,
            &mut phase,
            &mut streamed,
            &mut |v| events.push(v),
        ));

        let delta_msg = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "完了しました。" }
                }
            }
        });
        assert!(process_acp_update_msg(
            &delta_msg,
            &mut phase,
            &mut streamed,
            &mut |v| events.push(v),
        ));
        assert_eq!(phase, AcpStreamPhase::Responding);
        assert_eq!(streamed, "完了しました。");

        assert_eq!(
            events[0],
            json!({ "event": "status", "message": "思考中..." })
        );
        assert_eq!(
            events[1],
            json!({ "event": "thought", "text": "字幕を確認中" })
        );
        assert_eq!(
            events[2],
            json!({ "event": "activity", "label": "Inspecting clips (in_progress)" })
        );
        assert_eq!(
            events[3],
            json!({ "event": "status", "message": "応答を生成中..." })
        );
        assert_eq!(
            events[4],
            json!({ "event": "delta", "text": "完了しました。" })
        );
    }

    #[test]
    fn acp_prompt_embeds_images_and_rejects_false_capabilities() {
        let params = CallParams {
            system_prompt: "SYS".to_owned(),
            user_prompt: "check".to_owned(),
            images: vec![AcpImageInput {
                mime_type: "image/jpeg".to_owned(),
                base64_data: "AQID".to_owned(),
            }],
            ..CallParams::default()
        };
        let req = build_acp_prompt_request(3, "s1", &params);
        assert_eq!(req["params"]["prompt"][1]["type"], json!("image"));
        assert_eq!(req["params"]["prompt"][1]["mimeType"], json!("image/jpeg"));

        let unsupported = json!({
            "result": { "agentCapabilities": { "promptCapabilities": { "image": false } } }
        });
        let err = match ensure_acp_vision_supported(&unsupported, &params.images) {
            Ok(()) => return,
            Err(e) => e,
        };
        assert_eq!(err.code, "vision_unsupported");
    }
}
