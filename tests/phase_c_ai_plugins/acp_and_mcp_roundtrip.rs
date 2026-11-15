//! Antigravity CLI アダプタ・ACP stdio アダプタ・`sea-movie-creator mcp` の実プロセス疎通テスト。

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use sea_movie_creator::ai::context_builder::build_ai_request;
use sea_movie_creator::ai::mcp_server::{McpLoopbackHttpServer, McpServerState};
use sea_movie_creator::ai::planner::dry_run_validate;
use sea_movie_creator::ai::plugin_host::AiPluginRegistry;
use sea_movie_creator::ai::provider::{AiError, AiProvider, AiStreamEvent};
use sea_movie_creator::core::time::FrameIndex;
use sea_movie_creator::timeline::document::ProjectDocument;
use sea_movie_creator::timeline::serialization::{load_project_file, save_project_file};

struct TempTestDir(PathBuf);

impl TempTestDir {
    fn create(label: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "sea_mc_phase_c2_{label}_{}_{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve_built_binary(
    name: &str,
    is_example: bool,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let file_name = if env::consts::EXE_SUFFIX.is_empty() {
        name.to_owned()
    } else {
        format!("{name}{}", env::consts::EXE_SUFFIX)
    };

    let mut search_roots = Vec::new();
    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR")
        && !target_dir.trim().is_empty()
    {
        let root = PathBuf::from(target_dir);
        search_roots.push(root.join("debug"));
        search_roots.push(root.join("release"));
    }
    if let Ok(cur_exe) = env::current_exe()
        && let Some(dir) = cur_exe.parent()
    {
        for ancestor in dir.ancestors().take(8) {
            search_roots.push(ancestor.to_path_buf());
            search_roots.push(ancestor.join("target").join("debug"));
            search_roots.push(ancestor.join("target").join("release"));
        }
    }
    let manifest_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    search_roots.push(manifest_root.join("target").join("debug"));
    search_roots.push(manifest_root.join("target").join("release"));

    for root in search_roots {
        let candidate = if is_example {
            root.join("examples").join(&file_name)
        } else {
            root.join(&file_name)
        };
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "missing workspace binary `{file_name}` (is_example={is_example}; run `cargo build --workspace --all-targets` first)"
    )
    .into())
}

#[test]
fn antigravity_plugin_streams_agy_output_and_errors_cleanly_when_missing()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture_bin = resolve_built_binary("ai_cli_fixture", true)?;
    let _agy_bin = resolve_built_binary("sea-ai-antigravity", false)?;
    let registry = AiPluginRegistry::discover_default();

    let provider = registry
        .resolve_provider("sea.ai.antigravity")?
        .with_external_cli(fixture_bin.display().to_string())
        .with_env("SEA_AI_FIXTURE_MODE", "agy-stream-json");

    let doc = ProjectDocument::new("Antigravity Test");
    let req = build_ai_request(&doc, FrameIndex::ZERO, None, None, &[], "Add Agy layer")?;
    let mut deltas = Vec::new();
    let res = provider.send_streaming(&req, &mut |evt| {
        if let AiStreamEvent::DeltaText(chunk) = evt {
            deltas.push(chunk);
        }
    })?;
    assert!(deltas.len() >= 2);
    let plan = dry_run_validate(&doc, &res.raw_text)?;
    assert!(
        plan.summary_lines
            .iter()
            .any(|line| line.contains("Agy Layer"))
    );

    // 外部 agy バイナリ未インストール時は明確なエラーメッセージで失敗する
    let missing_provider = registry
        .resolve_provider("sea.ai.antigravity")?
        .with_external_cli("definitely_missing_agy_binary_xyz");
    let err = missing_provider
        .send_streaming(&req, &mut |_| {})
        .err()
        .ok_or("missing agy binary should return an explicit error")?;
    assert!(matches!(err, AiError::PluginError(_)));
    assert!(err.to_string().contains("agy"));
    Ok(())
}

#[test]
fn acp_plugin_runs_initialize_session_prompt_and_streams_delta()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture_bin = resolve_built_binary("ai_cli_fixture", true)?;
    let _acp_bin = resolve_built_binary("sea-ai-acp", false)?;
    let registry = AiPluginRegistry::discover_default();

    let provider = registry
        .resolve_provider("sea.ai.acp")?
        .with_model("copilot")
        .with_external_cli(fixture_bin.display().to_string())
        .with_env("SEA_AI_FIXTURE_MODE", "acp-stdio");

    let doc = ProjectDocument::new("ACP Test");
    let req = build_ai_request(&doc, FrameIndex::ZERO, None, None, &[], "Add ACP layer")?;
    let mut deltas = Vec::new();
    let res = provider.send_streaming(&req, &mut |evt| {
        if let AiStreamEvent::DeltaText(chunk) = evt {
            deltas.push(chunk);
        }
    })?;
    assert!(deltas.len() >= 2);
    let plan = dry_run_validate(&doc, &res.raw_text)?;
    assert!(
        plan.summary_lines
            .iter()
            .any(|line| line.contains("ACP Layer"))
    );
    Ok(())
}

#[test]
fn mcp_stdio_binary_and_loopback_bearer_auth_persist_project_edits()
-> Result<(), Box<dyn std::error::Error>> {
    let main_bin = resolve_built_binary("sea-movie-creator", false)?;
    let temp = TempTestDir::create("mcp")?;
    let proj_path = temp.0.join("project.smc.json");
    save_project_file(&ProjectDocument::new("MCP E2E Project"), &proj_path)?;

    let mut child = Command::new(&main_bin)
        .args(["mcp", "--project"])
        .arg(&proj_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().ok_or("missing mcp stdin")?;
    let stdout = child.stdout.take().ok_or("missing mcp stdout")?;
    let mut reader = BufReader::new(stdout);

    let apply_cmd = json!({
        "type": "batch",
        "commands": [
            { "type": "add_layer", "temp_id": "l_mcp", "name": "MCP Subtitles" },
            {
                "type": "add_clip",
                "temp_id": "c_mcp",
                "layer_id": { "temp": "l_mcp" },
                "clip": {
                    "name": "MCP Sub",
                    "timeline_start": 0,
                    "duration": 60,
                    "kind": { "kind": "subtitle", "text": "MCP 生成字幕" }
                }
            }
        ]
    });

    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"preview_edit_command","arguments":{"command":apply_cmd}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"apply_edit_command","arguments":{"command":apply_cmd}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"undo","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"redo","arguments":{}}}),
    ];

    for msg in &messages {
        writeln!(stdin, "{}", serde_json::to_string(msg)?)?;
    }
    stdin.flush()?;
    drop(stdin);

    let mut responses = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            responses.push(serde_json::from_str::<Value>(trimmed)?);
        }
        line.clear();
    }
    let status = child.wait()?;
    assert!(status.success());
    assert_eq!(responses.len(), 6);
    assert_eq!(responses[2]["result"]["isError"].as_bool(), Some(false));
    assert_eq!(responses[3]["result"]["isError"].as_bool(), Some(false));
    assert_eq!(responses[5]["result"]["isError"].as_bool(), Some(false));

    let reloaded = load_project_file(&proj_path)?;
    assert_eq!(reloaded.layers.len(), 1);
    assert_eq!(reloaded.layers[0].name, "MCP Subtitles");
    assert_eq!(reloaded.layers[0].clips.len(), 1);

    // HTTP ループバックは 127.0.0.1 限定・Bearer トークン必須を検証
    assert!(
        McpLoopbackHttpServer::bind(
            "0.0.0.0:0".parse()?,
            McpServerState::new(reloaded.clone(), None),
        )
        .is_err()
    );
    let http =
        McpLoopbackHttpServer::bind("127.0.0.1:0".parse()?, McpServerState::new(reloaded, None))?;
    assert!(http.addr().ip().is_loopback());
    Ok(())
}

#[test]
#[ignore = "requires installed external AI CLI"]
fn live_external_ai_cli_version_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("agy").arg("--version").output()?;
    assert!(output.status.success());
    Ok(())
}
