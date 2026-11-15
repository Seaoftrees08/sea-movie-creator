//! 結合テスト用の決定論的外部 CLI フィクスチャ（`Antigravity ACP --sandbox` / `ACP stdio` エミュレータ）。

use std::env;
use std::io::{BufRead, Lines, StdinLock, StdoutLock, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

fn emit_acp_update(
    out: &mut StdoutLock<'_>,
    session_id: &str,
    update_kind: &str,
    text: &str,
) -> Result<(), String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": update_kind,
                "content": { "type": "text", "text": text }
            }
        }
    });
    writeln!(out, "{payload}").map_err(|e| e.to_string())
}

fn handle_agy_sandbox_prompt(
    lines: &mut Lines<StdinLock<'_>>,
    out: &mut StdoutLock<'_>,
    id: &Value,
) -> Result<(), String> {
    emit_acp_update(
        out,
        "agy-sandbox-1",
        "agent_thought_chunk",
        "thinking not json {",
    )?;
    let perm_req = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "session/request_permission",
        "params": {
            "sessionId": "agy-sandbox-1",
            "options": [
                { "optionId": "allow_once", "kind": "allow_once" },
                { "optionId": "reject_once", "kind": "reject_once" }
            ]
        }
    });
    writeln!(out, "{perm_req}").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;

    let perm_line = lines
        .next()
        .ok_or_else(|| "expected permission response on stdin".to_owned())?
        .map_err(|e| e.to_string())?;
    let perm_resp: Value = serde_json::from_str(perm_line.trim()).map_err(|e| e.to_string())?;
    if perm_resp["id"] != json!(99)
        || perm_resp["result"]["outcome"]["optionId"] != json!("reject_once")
    {
        return Err(format!("unexpected permission response: {perm_resp}"));
    }

    emit_acp_update(
        out,
        "agy-sandbox-1",
        "agent_message_chunk",
        "{\"type\":\"add_layer\",",
    )?;
    emit_acp_update(
        out,
        "agy-sandbox-1",
        "agent_message_chunk",
        "\"name\":\"Agy Layer\"}",
    )?;
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "stopReason": "end_turn" }
    });
    writeln!(out, "{resp}").map_err(|e| e.to_string())
}

fn run_acp_agy_sandbox() -> Result<(), String> {
    let stdin = std::io::stdin().lock();
    let mut lines = stdin.lines();
    let mut out = std::io::stdout().lock();

    while let Some(line) = lines.next() {
        let line = line.map_err(|e| e.to_string())?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            Some("initialize") => {
                let init =
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": 1 } });
                writeln!(out, "{init}").map_err(|e| e.to_string())?;
            }
            Some("session/new") => {
                let sess = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "sessionId": "agy-sandbox-1",
                        "models": { "currentModelId": "gemini-4-argon-medium" },
                        "configOptions": [{ "id": "model", "type": "select", "currentValue": "gemini-4-argon" }]
                    }
                });
                writeln!(out, "{sess}").map_err(|e| e.to_string())?;
            }
            Some("session/prompt") => handle_agy_sandbox_prompt(&mut lines, &mut out, &id)?,
            _ => {}
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn run_acp_stdio() -> Result<(), String> {
    let stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();

    for line in stdin.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            Some("initialize") => {
                let init =
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": 1 } });
                writeln!(out, "{init}").map_err(|e| e.to_string())?;
            }
            Some("session/new") => {
                let sess = json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "fixture-sess-1" } });
                writeln!(out, "{sess}").map_err(|e| e.to_string())?;
            }
            Some("session/prompt") => {
                emit_acp_update(
                    &mut out,
                    "fixture-sess-1",
                    "agent_message_chunk",
                    "{\"type\":\"add_layer\",",
                )?;
                emit_acp_update(
                    &mut out,
                    "fixture-sess-1",
                    "agent_message_chunk",
                    "\"name\":\"ACP Layer\"}",
                )?;
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "stopReason": "end_turn" }
                });
                writeln!(out, "{resp}").map_err(|e| e.to_string())?;
            }
            _ => {}
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let mode = env::var("SEA_AI_FIXTURE_MODE").unwrap_or_default();
    let res = match mode.as_str() {
        "acp-agy-sandbox" => run_acp_agy_sandbox(),
        "acp-stdio" => run_acp_stdio(),
        other => Err(format!("unknown SEA_AI_FIXTURE_MODE: `{other}`")),
    };
    if let Err(err) = res {
        let _ = writeln!(std::io::stderr(), "{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
