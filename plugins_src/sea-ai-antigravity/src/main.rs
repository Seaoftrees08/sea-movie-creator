//! `sea-movie-creator` 用 Google Antigravity (`agy acp --sandbox --stdio`) 公式 ACP アダプタプラグイン。
//!
//! 認証バイパス・OAuth トークン読み取り・内部エンドポイント直叩きは一切行わず、
//! 公式 `agy` CLI が提供する外部エディタ向け ACP (Agent Client Protocol) サーバーを
//! `--sandbox`＋空の一時作業ディレクトリで起動して JSON-RPC 通信を行います。

use std::env;
use std::fs;
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
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use sea_ai_protocol::ORIGINAL_INSTRUCTION_MARKER;
use sea_ai_protocol::{
    AcpImageInput, HistoryMessage, MAX_HISTORY_PROMPT_BYTES, acp_supports_image_prompt,
    build_acp_prompt_content_blocks, extract_acp_update_message_delta,
    format_conversation_history_section, is_empty_json_value, validate_acp_images,
};
use serde::Deserialize;
use serde_json::{Value, json};

static SANDBOX_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempSandboxDir {
    path: PathBuf,
}

impl TempSandboxDir {
    fn create() -> Result<Self, String> {
        let seq = SANDBOX_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("sea_agy_acp_{}_{seq}", std::process::id()));
        fs::create_dir_all(&path)
            .map_err(|err| format!("一時サンドボックスディレクトリ作成失敗: {err}"))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSandboxDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
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
    thinking_level: Option<String>,
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

fn normalize_requested_model(model: Option<&str>) -> Option<&str> {
    model
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != "plugin-default")
}

fn normalize_requested_thinking_level(level: Option<&str>) -> Option<&'static str> {
    let trimmed = level?.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra" => Some("xhigh"),
        _ => None,
    }
}

fn build_agy_acp_args(external_args: &[String]) -> Vec<String> {
    let mut args = vec![
        "acp".to_owned(),
        "--sandbox".to_owned(),
        "--stdio".to_owned(),
    ];
    for extra in external_args {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            args.push(trimmed.to_owned());
        }
    }
    args
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

fn resolve_agy_cli(override_cli: Option<&str>) -> Result<PathBuf, String> {
    if let Some(explicit) = override_cli.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_executable_in_path(explicit).ok_or_else(|| {
            format!("指定された `agy` 実行ファイルが見つかりませんでした: `{explicit}`")
        });
    }
    if let Ok(env_cli) = env::var("SEA_ANTIGRAVITY_CLI")
        && !env_cli.trim().is_empty()
    {
        return resolve_executable_in_path(env_cli.trim()).ok_or_else(|| {
            format!("環境変数 `SEA_ANTIGRAVITY_CLI` の実行ファイルが見つかりません: `{env_cli}`")
        });
    }
    resolve_executable_in_path("agy").ok_or_else(|| {
        "Antigravity CLI (`agy`) が PATH 上に見つかりませんでした。\
         Antigravity をインストールし `agy acp --login` 等でサインインした状態で PATH を設定してください。"
            .to_owned()
    })
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

fn build_session_new_request(id: u64, sandbox_cwd: &Path) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": {
            "cwd": sandbox_cwd.display().to_string(),
            "mcpServers": []
        }
    })
}

fn session_has_config_option(session_resp: &Value, config_id: &str) -> bool {
    session_resp["result"]["configOptions"]
        .as_array()
        .is_some_and(|opts| opts.iter().any(|opt| opt["id"].as_str() == Some(config_id)))
}

fn config_option_allows_value(resp: &Value, config_id: &str, candidate: &str) -> bool {
    resp["result"]["configOptions"]
        .as_array()
        .and_then(|opts| {
            opts.iter()
                .find(|opt| opt["id"].as_str() == Some(config_id))
        })
        .and_then(|opt| opt["options"].as_array())
        .is_some_and(|vals| vals.iter().any(|v| v["value"].as_str() == Some(candidate)))
}

fn build_set_config_option_request(
    id: u64,
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/set_config_option",
        "params": {
            "sessionId": session_id,
            "configId": config_id,
            "value": value,
        }
    })
}

fn build_agy_prompt_text(params: &CallParams) -> String {
    let mut sections = vec![
        params.system_prompt.clone(),
        "# Sandbox Constraints\nDo NOT execute shell commands, modify files, browse URLs, or invoke workspace tools.".to_owned(),
    ];
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

fn build_session_prompt_request(id: u64, session_id: &str, params: &CallParams) -> Value {
    let prompt_text = build_agy_prompt_text(params);
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

fn extract_acp_message_delta(msg: &Value) -> Option<String> {
    if msg["method"].as_str() != Some("session/update") {
        return None;
    }
    extract_acp_update_message_delta(&msg["params"])
}

fn build_permission_deny_response(id: &Value, params: &Value) -> Value {
    let reject_id = params["options"].as_array().and_then(|opts| {
        opts.iter().find_map(|opt| {
            let kind = opt["kind"].as_str()?;
            if kind == "reject_once" || kind == "reject_always" {
                opt["optionId"].as_str().map(ToOwned::to_owned)
            } else {
                None
            }
        })
    });

    let outcome = match reject_id {
        Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
        None => json!({ "outcome": "cancelled" }),
    };

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "outcome": outcome }
    })
}

fn write_jsonrpc_line(stdin: &mut ChildStdin, payload: &Value) -> Result<(), String> {
    let text = serde_json::to_string(payload)
        .map_err(|err| format!("Antigravity ACP JSON エンコード失敗: {err}"))?;
    writeln!(stdin, "{text}")
        .and_then(|()| stdin.flush())
        .map_err(|err| format!("Antigravity ACP stdin 書き込み失敗: {err}"))
}

fn handle_server_request(stdin: &mut ChildStdin, msg: &Value) -> Result<bool, String> {
    let Some(req_id) = msg.get("id").filter(|v| !v.is_null()) else {
        return Ok(false);
    };
    let Some(method) = msg["method"].as_str() else {
        return Ok(false);
    };

    let reply = if method == "session/request_permission" {
        build_permission_deny_response(req_id, &msg["params"])
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32601,
                "message": "method not supported by sea-movie-creator ACP host"
            }
        })
    };
    write_jsonrpc_line(stdin, &reply)?;
    Ok(true)
}

fn format_acp_rpc_error(err_obj: &Value) -> String {
    let lower = err_obj.to_string().to_lowercase();
    if lower.contains("auth") || lower.contains("login") || lower.contains("-32000") {
        return format!(
            "Antigravity ACP 認証エラー: {err_obj}（先にターミナルで `agy acp --login` を実行してサインインするか、Gemini API キー方式をご利用ください）"
        );
    }
    format!("Antigravity ACP エラー応答: {err_obj}")
}

fn read_acp_until_response<R: BufRead>(
    reader: &mut R,
    stdin: &mut ChildStdin,
    expected_id: u64,
    streamed: &mut String,
) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|err| format!("Antigravity ACP stdout 読取エラー: {err}"))?;
        if read == 0 {
            return Err(format!(
                "Antigravity ACP (`agy acp`) が id={expected_id} 応答前に終了しました"
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed)
            .map_err(|err| format!("Antigravity ACP JSON 解析エラー: {err} (line: {trimmed})"))?;

        if handle_server_request(stdin, &msg)? {
            continue;
        }
        if let Some(chunk) = extract_acp_message_delta(&msg) {
            streamed.push_str(&chunk);
            emit_event(&json!({ "event": "delta", "text": chunk }));
            continue;
        }
        if msg["id"].as_u64() == Some(expected_id) {
            if let Some(err_obj) = msg.get("error").filter(|v| !v.is_null()) {
                return Err(format_acp_rpc_error(err_obj));
            }
            return Ok(msg);
        }
    }
}

fn extract_fallback_result_text(prompt_resp: &Value) -> String {
    if let Some(text) = prompt_resp["result"]["text"].as_str() {
        return text.to_owned();
    }
    if let Some(items) = prompt_resp["result"]["content"].as_array() {
        let mut buf = String::new();
        for item in items {
            if let Some(text) = item["text"].as_str() {
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return buf;
        }
    }
    String::new()
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

fn map_acp_thought_level_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("auto"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra" => Some("extra"),
        _ => None,
    }
}

fn extract_models_from_session_new(session_resp: &Value) -> Vec<Value> {
    let config_options = session_resp["result"]["configOptions"].as_array();
    let thought_levels: Vec<&'static str> = config_options
        .and_then(|opts| {
            opts.iter()
                .find(|opt| opt["id"].as_str() == Some("thought_level"))
        })
        .and_then(|opt| opt["options"].as_array())
        .map(|vals| {
            vals.iter()
                .filter_map(|v| v["value"].as_str().and_then(map_acp_thought_level_name))
                .collect()
        })
        .unwrap_or_default();
    let default_thinking = if thought_levels.contains(&"medium") {
        "medium"
    } else if thought_levels.is_empty() {
        "auto"
    } else {
        thought_levels[0]
    };

    if let Some(model_opt) =
        config_options.and_then(|opts| opts.iter().find(|opt| opt["id"].as_str() == Some("model")))
        && let Some(vals) = model_opt["options"].as_array()
        && !vals.is_empty()
    {
        return vals
            .iter()
            .filter_map(|entry| {
                let id = entry["value"].as_str().or_else(|| entry.as_str())?.trim();
                if id.is_empty() {
                    return None;
                }
                let label = entry["name"]
                    .as_str()
                    .or_else(|| entry["label"].as_str())
                    .unwrap_or(id);
                Some(json!({
                    "id": id,
                    "display_name": label,
                    "badge": null,
                    "thinking_levels": thought_levels,
                    "default_thinking": default_thinking,
                    "available": true,
                    "note": "agy CLI 利用可能",
                }))
            })
            .collect();
    }

    if let Some(avail) = session_resp["result"]["models"]["availableModels"].as_array()
        && !avail.is_empty()
    {
        return avail
            .iter()
            .filter_map(|entry| {
                let id = entry["modelId"]
                    .as_str()
                    .or_else(|| entry["id"].as_str())?
                    .trim();
                if id.is_empty() {
                    return None;
                }
                let name = entry["name"].as_str().unwrap_or(id);
                Some(json!({
                    "id": id,
                    "display_name": name,
                    "badge": null,
                    "thinking_levels": thought_levels,
                    "default_thinking": default_thinking,
                    "available": true,
                    "note": "agy CLI 利用可能",
                }))
            })
            .collect();
    }

    if let Some(cur) = session_resp["result"]["models"]["currentModelId"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![json!({
            "id": cur,
            "display_name": cur,
            "badge": null,
            "thinking_levels": thought_levels,
            "default_thinking": default_thinking,
            "available": true,
            "note": "agy CLI 利用可能",
        })];
    }

    Vec::new()
}

struct AcpConversationOutcome {
    fallback_text: String,
    active_model: String,
    usage: Option<Value>,
}

fn ensure_acp_vision_supported(init_resp: &Value, images: &[AcpImageInput]) -> Result<(), String> {
    if !images.is_empty() && acp_supports_image_prompt(init_resp) == Some(false) {
        return Err(
            "vision_unsupported: 接続先の ACP エージェントは画像入力 (promptCapabilities.image) に非対応です"
                .to_owned(),
        );
    }
    Ok(())
}

fn execute_acp_conversation(
    reader: &mut BufReader<std::process::ChildStdout>,
    stdin: &mut ChildStdin,
    sandbox_cwd: &Path,
    params: &CallParams,
    streamed: &mut String,
) -> Result<AcpConversationOutcome, String> {
    write_jsonrpc_line(stdin, &build_initialize_request(1))?;
    let init_resp = read_acp_until_response(reader, stdin, 1, streamed)?;
    ensure_acp_vision_supported(&init_resp, &params.images)?;

    write_jsonrpc_line(stdin, &build_session_new_request(2, sandbox_cwd))?;
    let session_resp = read_acp_until_response(reader, stdin, 2, streamed)?;
    let session_id = session_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or("default-session")
        .to_owned();

    let requested_model = normalize_requested_model(params.model.as_deref());
    let requested_thinking = normalize_requested_thinking_level(params.thinking_level.as_deref());
    let mut latest_config_resp = session_resp.clone();
    let mut next_id = 3_u64;
    if let Some(model_id) = requested_model
        && session_has_config_option(&latest_config_resp, "model")
    {
        write_jsonrpc_line(
            stdin,
            &build_set_config_option_request(next_id, &session_id, "model", model_id),
        )?;
        latest_config_resp = read_acp_until_response(reader, stdin, next_id, streamed)?;
        next_id += 1;
    }

    let mut applied_thinking: Option<&'static str> = None;
    if let Some(level) = requested_thinking
        && config_option_allows_value(&latest_config_resp, "thought_level", level)
    {
        write_jsonrpc_line(
            stdin,
            &build_set_config_option_request(next_id, &session_id, "thought_level", level),
        )?;
        let _thought_resp = read_acp_until_response(reader, stdin, next_id, streamed)?;
        applied_thinking = Some(level);
        next_id += 1;
    }

    write_jsonrpc_line(
        stdin,
        &build_session_prompt_request(next_id, &session_id, params),
    )?;
    let prompt_resp = read_acp_until_response(reader, stdin, next_id, streamed)?;
    let base_model = requested_model
        .or_else(|| session_resp["result"]["models"]["currentModelId"].as_str())
        .unwrap_or("antigravity");
    let active_model = match applied_thinking {
        Some(level) => format!("{base_model} ({level})"),
        None => base_model.to_owned(),
    };

    Ok(AcpConversationOutcome {
        fallback_text: extract_fallback_result_text(&prompt_resp),
        active_model,
        usage: extract_acp_usage(&prompt_resp),
    })
}

fn run_list_models(params: &CallParams) -> Result<(), String> {
    let agy_bin = resolve_agy_cli(params.external_cli.as_deref()).map_err(|msg| {
        emit_event(&json!({
            "event": "error",
            "code": "missing_external_cli",
            "message": msg,
        }));
        msg
    })?;

    let sandbox_dir = TempSandboxDir::create()?;
    let args = build_agy_acp_args(&params.external_args);
    let mut cmd = Command::new(&agy_bin);
    apply_background_process_flags(&mut cmd);
    let mut child = cmd
        .args(&args)
        .current_dir(sandbox_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("`agy acp` 起動に失敗しました: {err}"))?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "`agy acp` stdin パイプ取得失敗".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "`agy acp` stdout パイプ取得失敗".to_owned())?;
    let mut reader = BufReader::new(stdout);
    let mut streamed = String::new();

    let probe_res = (|| -> Result<Value, String> {
        write_jsonrpc_line(&mut child_stdin, &build_initialize_request(1))?;
        let _init = read_acp_until_response(&mut reader, &mut child_stdin, 1, &mut streamed)?;
        write_jsonrpc_line(
            &mut child_stdin,
            &build_session_new_request(2, sandbox_dir.path()),
        )?;
        read_acp_until_response(&mut reader, &mut child_stdin, 2, &mut streamed)
    })();
    drop(child_stdin);

    if probe_res.is_err() {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("`agy acp` 終了待機エラー: {err}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let session_resp = match probe_res {
        Ok(resp) => resp,
        Err(err) => {
            if stderr.is_empty() {
                return Err(err);
            }
            return Err(format!("{err} ({stderr})"));
        }
    };
    if !output.status.success() {
        return Err(format!(
            "`agy acp` モデル取得が終了コード {} で失敗しました: {stderr}",
            output.status
        ));
    }

    let models = extract_models_from_session_new(&session_resp);
    emit_event(&json!({
        "event": "models",
        "models": models,
    }));
    Ok(())
}

fn run_request(params: &CallParams) -> Result<(), String> {
    if !params.images.is_empty() {
        validate_acp_images(&params.images)?;
    }
    let agy_bin = resolve_agy_cli(params.external_cli.as_deref()).map_err(|msg| {
        emit_event(&json!({
            "event": "error",
            "code": "missing_external_cli",
            "message": msg,
        }));
        msg
    })?;

    let sandbox_dir = TempSandboxDir::create()?;
    let args = build_agy_acp_args(&params.external_args);

    emit_event(&json!({
        "event": "status",
        "message": format!("Antigravity 公式 ACP ({}) に接続中...", agy_bin.display()),
    }));

    let mut cmd = Command::new(&agy_bin);
    apply_background_process_flags(&mut cmd);
    let mut child = cmd
        .args(&args)
        .current_dir(sandbox_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("`agy acp` 起動に失敗しました: {err}"))?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "`agy acp` stdin パイプ取得失敗".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "`agy acp` stdout パイプ取得失敗".to_owned())?;
    let mut reader = BufReader::new(stdout);
    let mut streamed = String::new();

    let conv_res = execute_acp_conversation(
        &mut reader,
        &mut child_stdin,
        sandbox_dir.path(),
        params,
        &mut streamed,
    );
    drop(child_stdin);

    let output = child
        .wait_with_output()
        .map_err(|err| format!("`agy acp` 終了待機エラー: {err}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

    let outcome = match conv_res {
        Ok(res) => res,
        Err(err) => {
            if stderr.is_empty() {
                return Err(err);
            }
            return Err(format!("{err} ({stderr})"));
        }
    };

    if !output.status.success() {
        return Err(format!(
            "`agy acp` が終了コード {} で失敗しました: {stderr}",
            output.status
        ));
    }

    let raw_text = if streamed.is_empty() {
        outcome.fallback_text
    } else {
        streamed
    };
    let mut completed = json!({
        "event": "completed",
        "raw_text": raw_text,
        "model": format!("antigravity:{}", outcome.active_model),
    });
    if let Some(usage) = outcome.usage {
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
    let res = match req.method.as_str() {
        "list_models" => run_list_models(&req.params),
        _ => run_request(&req.params),
    };
    if let Err(err) = res {
        let code = if err.starts_with("vision_unsupported:") {
            "vision_unsupported"
        } else {
            "execution_failed"
        };
        emit_event(&json!({ "event": "error", "code": code, "message": err }));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_agy_acp_args_uses_official_acp_sandbox_stdio() {
        let args = build_agy_acp_args(&["--disable-telemetry".to_owned(), "  ".to_owned()]);
        assert_eq!(
            args,
            vec!["acp", "--sandbox", "--stdio", "--disable-telemetry"]
        );
        assert!(!args.iter().any(|a| a == "--print" || a == "-p"));
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn normalize_requested_model_skips_plugin_default() {
        assert_eq!(normalize_requested_model(None), None);
        assert_eq!(normalize_requested_model(Some("   ")), None);
        assert_eq!(normalize_requested_model(Some("plugin-default")), None);
        assert_eq!(
            normalize_requested_model(Some(" gemini-4-argon ")),
            Some("gemini-4-argon")
        );
    }

    #[test]
    fn extract_acp_message_delta_ignores_thoughts_and_tool_calls() {
        let thought = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "thinking not json {" }
                }
            }
        });
        assert_eq!(extract_acp_message_delta(&thought), None);

        let tool_call = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "content": { "type": "text", "text": "running tool" }
                }
            }
        });
        assert_eq!(extract_acp_message_delta(&tool_call), None);

        let message = json!({
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
            extract_acp_message_delta(&message).as_deref(),
            Some("{\"type\":\"undo\"}")
        );
    }

    #[test]
    fn build_permission_deny_response_prefers_reject_option_and_falls_back_to_cancelled() {
        let with_reject = build_permission_deny_response(
            &json!(99),
            &json!({
                "options": [
                    { "optionId": "allow_once", "kind": "allow_once" },
                    { "optionId": "reject_once", "kind": "reject_once" }
                ]
            }),
        );
        assert_eq!(with_reject["id"], json!(99));
        assert_eq!(
            with_reject["result"]["outcome"],
            json!({ "outcome": "selected", "optionId": "reject_once" })
        );

        let fallback = build_permission_deny_response(&json!("req-2"), &json!({ "options": [] }));
        assert_eq!(fallback["id"], json!("req-2"));
        assert_eq!(
            fallback["result"]["outcome"],
            json!({ "outcome": "cancelled" })
        );
    }

    #[test]
    fn thinking_level_normalization_and_config_option_checks_work()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(normalize_requested_thinking_level(None), None);
        assert_eq!(normalize_requested_thinking_level(Some("auto")), None);
        assert_eq!(
            normalize_requested_thinking_level(Some("Extra")),
            Some("xhigh")
        );
        assert_eq!(
            normalize_requested_thinking_level(Some("HIGH")),
            Some("high")
        );

        let cfg = json!({
            "result": {
                "configOptions": [
                    { "id": "model", "options": [{ "value": "gemini-4-hydrogen" }] },
                    { "id": "thought_level", "options": [
                        { "value": "low" },
                        { "value": "medium" },
                        { "value": "high" },
                        { "value": "xhigh" }
                    ]}
                ]
            }
        });
        assert!(session_has_config_option(&cfg, "model"));
        assert!(config_option_allows_value(&cfg, "thought_level", "xhigh"));
        assert!(!config_option_allows_value(&cfg, "thought_level", "auto"));

        let extracted = extract_models_from_session_new(&cfg);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0]["id"], json!("gemini-4-hydrogen"));
        assert_eq!(
            extracted[0]["thinking_levels"],
            json!(["low", "medium", "high", "extra"])
        );
        assert_eq!(extracted[0]["available"].as_bool(), Some(true));

        let usage_resp = json!({
            "result": {
                "usage": { "inputTokens": 40, "outputTokens": 15, "totalTokens": 55 }
            }
        });
        let parsed_usage = extract_acp_usage(&usage_resp).ok_or("missing usage")?;
        assert_eq!(parsed_usage["total_tokens"], json!(55));
        Ok(())
    }

    #[test]
    fn build_agy_prompt_expands_history_and_omits_empty_sections_and_batch_constraint() {
        let params = CallParams {
            system_prompt: "SYS".to_owned(),
            command_schema_json: json!({}),
            context_summary: Value::Null,
            history: vec![
                HistoryMessage {
                    role: "user".to_owned(),
                    content: "# Session Context\n{\"clips\":1}".to_owned(),
                },
                HistoryMessage {
                    role: "user".to_owned(),
                    content: "# Original User Instruction\n4項目の修正をして".to_owned(),
                },
                HistoryMessage {
                    role: "assistant".to_owned(),
                    content: "{\"tool_calls\":[{\"name\":\"get_timeline_summary\"}]}".to_owned(),
                },
            ],
            user_prompt: "[ToolResult get_timeline_summary status=OK]\n{}".to_owned(),
            ..CallParams::default()
        };
        let prompt = build_agy_prompt_text(&params);
        assert!(!prompt.contains("Always output a valid EditCommand"));
        assert!(!prompt.contains("# EditCommand JSON Schema"));
        assert!(!prompt.contains("# Timeline Context"));
        assert!(prompt.contains("# Conversation History"));
        assert!(prompt.contains("[user]\n# Original User Instruction\n4項目の修正をして"));
        assert!(prompt.contains("[assistant]\n{\"tool_calls\""));
        assert!(prompt.contains("# Instruction\n[ToolResult get_timeline_summary"));
    }

    #[test]
    fn history_compression_pins_original_instruction_and_preserves_utf8() {
        let big_context = format!("# Session Context\n{}", "あ".repeat(25_000));
        let old_tool = format!("[ToolResult inspect status=OK]\n{}", "い".repeat(15_000));
        let original = format!("{ORIGINAL_INSTRUCTION_MARKER}\n必ず字幕の縁取り色を変更して");
        let mid_turn = "う".repeat(10_000);
        let latest_tool = "[ToolResult get_capabilities status=OK]\n{\"ok\":true}".to_owned();

        let history = vec![
            HistoryMessage {
                role: "user".to_owned(),
                content: big_context,
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: original.clone(),
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: old_tool,
            },
            HistoryMessage {
                role: "assistant".to_owned(),
                content: mid_turn,
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: latest_tool.clone(),
            },
        ];

        let section = format_conversation_history_section(&history, MAX_HISTORY_PROMPT_BYTES);
        assert!(section.len() <= MAX_HISTORY_PROMPT_BYTES);
        assert!(section.contains(&original));
        assert!(section.contains(&latest_tool));

        // 通常チャット（マーカーなし）では古いターンから省略され直近ターンが残ること
        let normal_history = vec![
            HistoryMessage {
                role: "user".to_owned(),
                content: format!("古い指示 {}", "え".repeat(30_000)),
            },
            HistoryMessage {
                role: "assistant".to_owned(),
                content: format!("古い応答 {}", "お".repeat(30_000)),
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: "直近の依頼".to_owned(),
            },
        ];
        let normal_section = format_conversation_history_section(&normal_history, 40 * 1024);
        assert!(normal_section.len() <= 40 * 1024);
        assert!(normal_section.contains("直近の依頼"));
        assert!(normal_section.contains("...(earlier messages omitted)..."));
        assert!(!normal_section.contains("古い指示"));
    }

    #[test]
    fn prompt_request_embeds_images_and_rejects_false_prompt_capabilities() {
        let params = CallParams {
            system_prompt: "SYS".to_owned(),
            user_prompt: "check image".to_owned(),
            images: vec![AcpImageInput {
                mime_type: "image/jpeg".to_owned(),
                base64_data: "AQID".to_owned(),
            }],
            ..CallParams::default()
        };
        let req = build_session_prompt_request(3, "s1", &params);
        let prompt_arr = req["params"]["prompt"].as_array().expect("prompt array");
        assert_eq!(prompt_arr.len(), 2);
        assert_eq!(prompt_arr[1]["type"], json!("image"));
        assert_eq!(prompt_arr[1]["mimeType"], json!("image/jpeg"));
        assert_eq!(prompt_arr[1]["data"], json!("AQID"));

        let unsupported_init = json!({
            "result": { "agentCapabilities": { "promptCapabilities": { "image": false } } }
        });
        let err = ensure_acp_vision_supported(&unsupported_init, &params.images)
            .expect_err("must reject when promptCapabilities.image is false");
        assert!(err.starts_with("vision_unsupported:"));
    }
}
