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
use sea_ai_protocol::{
    HistoryMessage, MAX_HISTORY_PROMPT_BYTES, extract_acp_update_message_delta,
    format_conversation_history_section, is_empty_json_value,
};
use serde::Deserialize;
use serde_json::{Value, json};

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
            "# EditCommand JSON Schema\n{}",
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

fn extract_acp_update_text(msg: &Value) -> Option<String> {
    if msg["method"].as_str() != Some("session/update") {
        return None;
    }
    extract_acp_update_message_delta(&msg["params"])
}

fn write_jsonrpc_line(stdin: &mut ChildStdin, payload: &Value) -> Result<(), String> {
    let text =
        serde_json::to_string(payload).map_err(|err| format!("ACP JSON エンコード失敗: {err}"))?;
    writeln!(stdin, "{text}")
        .and_then(|()| stdin.flush())
        .map_err(|err| format!("ACP stdin 書き込み失敗: {err}"))
}

fn read_until_response<R: BufRead>(
    reader: &mut R,
    expected_id: u64,
    streamed: &mut String,
) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|err| format!("ACP stdout 読取エラー: {err}"))?;
        if read == 0 {
            return Err(format!(
                "ACP エージェントが id={expected_id} 応答前に終了しました"
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed)
            .map_err(|err| format!("ACP JSON 解析エラー: {err} (line: {trimmed})"))?;

        if let Some(chunk) = extract_acp_update_text(&msg) {
            streamed.push_str(&chunk);
            emit_event(&json!({ "event": "delta", "text": chunk }));
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

fn run_acp_session(params: &CallParams) -> Result<(), String> {
    let (agent_bin, agent_args, profile) = resolve_agent_command(params).map_err(|msg| {
        emit_event(&json!({
            "event": "error",
            "code": "missing_external_cli",
            "message": msg,
        }));
        msg
    })?;

    emit_event(&json!({
        "event": "status",
        "message": format!("ACP エージェント ({}) に接続中...", agent_bin.display()),
    }));

    let mut cmd = Command::new(&agent_bin);
    apply_background_process_flags(&mut cmd);
    let mut child = cmd
        .args(&agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("ACP エージェント起動失敗: {err}"))?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "ACP stdin パイプ取得失敗".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ACP stdout パイプ取得失敗".to_owned())?;
    let mut reader = BufReader::new(stdout);
    let mut streamed = String::new();

    write_jsonrpc_line(&mut child_stdin, &build_initialize_request(1))?;
    let _init_resp = read_until_response(&mut reader, 1, &mut streamed)?;

    let cwd = env::current_dir().map_or_else(|_| ".".to_owned(), |p| p.display().to_string());
    write_jsonrpc_line(
        &mut child_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": cwd, "mcpServers": [] }
        }),
    )?;
    let session_resp = read_until_response(&mut reader, 2, &mut streamed)?;
    let session_id = session_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or("default-session")
        .to_owned();

    let prompt_text = build_acp_prompt_text(params);
    write_jsonrpc_line(
        &mut child_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt_text }]
            }
        }),
    )?;
    drop(child_stdin);

    let prompt_resp = read_until_response(&mut reader, 3, &mut streamed)?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("ACP プロセス終了待機エラー: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "ACP エージェントが {} で終了しました: {stderr}",
            output.status
        ));
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
        emit_event(&json!({ "event": "error", "code": "execution_failed", "message": err }));
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
}
