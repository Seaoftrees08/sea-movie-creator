//! 結合テスト用の決定論的外部 CLI フィクスチャ（`agy stream-json` / `ACP stdio` エミュレータ）。

use std::env;
use std::io::{BufRead, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

fn run_agy_stream_json() -> Result<(), String> {
    let mut stdin_line = String::new();
    let _ = std::io::stdin()
        .lock()
        .read_line(&mut stdin_line)
        .map_err(|e| e.to_string())?;

    let mut out = std::io::stdout().lock();
    let part1 = json!({
        "type": "content_block_delta",
        "delta": { "type": "text_delta", "text": "{\"type\":\"add_layer\"," }
    });
    let part2 = json!({
        "type": "content_block_delta",
        "delta": { "type": "text_delta", "text": "\"name\":\"Agy Layer\"}" }
    });
    let done = json!({
        "type": "result",
        "result": "{\"type\":\"add_layer\",\"name\":\"Agy Layer\"}"
    });

    for item in [part1, part2, done] {
        writeln!(out, "{item}").map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;
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
                writeln!(
                    out,
                    "{}",
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": 1 } })
                )
                .map_err(|e| e.to_string())?;
            }
            Some("session/new") => {
                writeln!(
                    out,
                    "{}",
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "sessionId": "fixture-sess-1" } })
                )
                .map_err(|e| e.to_string())?;
            }
            Some("session/prompt") => {
                let u1 = json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "fixture-sess-1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "{\"type\":\"add_layer\"," }
                        }
                    }
                });
                let u2 = json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "fixture-sess-1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "\"name\":\"ACP Layer\"}" }
                        }
                    }
                });
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "stopReason": "end_turn" }
                });
                writeln!(out, "{u1}").map_err(|e| e.to_string())?;
                writeln!(out, "{u2}").map_err(|e| e.to_string())?;
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
        "agy-stream-json" => run_agy_stream_json(),
        "acp-stdio" => run_acp_stdio(),
        other => Err(format!("unknown SEA_AI_FIXTURE_MODE: `{other}`")),
    };
    if let Err(err) = res {
        let _ = writeln!(std::io::stderr(), "{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
