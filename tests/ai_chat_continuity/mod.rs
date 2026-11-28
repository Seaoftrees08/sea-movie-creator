//! 複数ターン継続性・元指示ピン留め・自己修復リトライ・変更0件明示の結合テスト。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sea_movie_creator::ai::agent_host::AgentPluginRegistry;
use sea_movie_creator::ai::provider::{
    AiAgentTurnRequest, AiAgentTurnResponse, AiContentPart, AiProviderKind, AiToolCall,
    AiTurnMessage,
};
use sea_movie_creator::ai::providers::mock::MockAiProvider;
use sea_movie_creator::ui::app_controller::AppController;
use serde_json::json;

fn poll_until<F>(
    controller: &mut AppController,
    timeout: Duration,
    mut pred: F,
) -> Result<(), String>
where
    F: FnMut(&AppController) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        let _ = controller.poll_ai_chat();
        if pred(controller) {
            return Ok(());
        }
        if let Some(err) = &controller.ai_chat.last_error {
            return Err(format!("agent reported unexpected error: {err}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "timed out waiting for agent condition; status_banner=`{}`",
        controller.ai_chat.status_banner
    ))
}

fn render_turn_message_text(msg: &AiTurnMessage) -> String {
    match msg {
        AiTurnMessage::System { text } => text.clone(),
        AiTurnMessage::User { parts } => parts
            .iter()
            .filter_map(|p| match p {
                AiContentPart::Text { text } => Some(text.as_str()),
                AiContentPart::ImageData { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AiTurnMessage::Assistant { text, .. } => text.clone().unwrap_or_default(),
        AiTurnMessage::ToolResult {
            name, content_json, ..
        } => format!("[ToolResult {name}] {content_json}"),
    }
}

fn build_continuity_mock_provider(
    captured_ref: Arc<Mutex<Vec<AiAgentTurnRequest>>>,
    step_ref: Arc<AtomicUsize>,
) -> Arc<MockAiProvider> {
    Arc::new(MockAiProvider::with_agent_handler(move |req| {
        if let Ok(mut guard) = captured_ref.lock() {
            guard.push(req.clone());
        }
        let step = step_ref.fetch_add(1, Ordering::SeqCst);
        match step {
            0 => Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![
                    AiToolCall {
                        id: "c1".to_owned(),
                        name: "get_timeline_summary".to_owned(),
                        arguments: json!({}),
                        thought_signature: None,
                    },
                    AiToolCall {
                        id: "c2".to_owned(),
                        name: "get_capabilities".to_owned(),
                        arguments: json!({ "include_schemas": true }),
                        thought_signature: None,
                    },
                ],
                model: "mock-continuity-v1".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            }),
            1 => Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![AiToolCall {
                    id: "c3".to_owned(),
                    name: "apply_draft_edit_command".to_owned(),
                    arguments: json!({
                        "command": {
                            "type": "set_project_meta",
                            "name": "Continuity Verified Movie"
                        }
                    }),
                    thought_signature: None,
                }],
                model: "mock-continuity-v1".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            }),
            _ => Ok(AiAgentTurnResponse {
                text: Some("タイトルを変更して完了しました。".to_owned()),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                model: "mock-continuity-v1".to_owned(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            }),
        }
    }))
}

#[test]
fn followup_preserves_original_instruction_across_tool_calls_and_updates_memory()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;
    if director.executable_path.is_none() {
        return Err("sea-agent-director binary missing; run cargo build --workspace first".into());
    }

    let captured_requests: Arc<Mutex<Vec<AiAgentTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let step_counter = Arc::new(AtomicUsize::new(0));
    let provider =
        build_continuity_mock_provider(Arc::clone(&captured_requests), Arc::clone(&step_counter));

    let mut controller = AppController::new();
    controller.ai_chat.selected_provider = AiProviderKind::Mock;
    controller.start_agent_prompt_with_provider(
        "プロジェクトタイトルを Continuity Verified Movie に変更してください",
        provider,
        Some(director),
    )?;

    poll_until(&mut controller, Duration::from_secs(20), |c| {
        c.ai_chat.pending_proposal.is_some()
    })?;

    let reqs = captured_requests.lock().map_err(|_| "lock poisoned")?;
    assert!(reqs.len() >= 3);

    // Step 0: messages[0] が # Session Context、messages[1] が # Original User Instruction
    let step0_msgs = &reqs[0].messages;
    assert_eq!(step0_msgs.len(), 2);
    assert!(render_turn_message_text(&step0_msgs[0]).starts_with("# Session Context"));
    assert!(render_turn_message_text(&step0_msgs[1]).starts_with("# Original User Instruction"));

    // Step 1: 複数 ToolResult 後も元のユーザー指示が messages 内に保持されていること
    let step1_joined = reqs[1]
        .messages
        .iter()
        .map(render_turn_message_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(step1_joined.contains("# Original User Instruction"));
    assert!(step1_joined.contains("Continuity Verified Movie に変更してください"));
    assert!(step1_joined.contains("[ToolResult get_timeline_summary]"));
    assert!(step1_joined.contains("[ToolResult get_capabilities]"));

    let recent_followups = controller
        .ai_chat
        .agent_memory
        .get("recent_followups")
        .and_then(|v| v.as_array())
        .ok_or("recent_followups missing in agent_memory")?;
    assert_eq!(recent_followups.len(), 1);
    assert_eq!(recent_followups[0]["changed_timeline"], json!(true));

    let _ = controller.cancel_ai_prompt();
    Ok(())
}

#[test]
fn noop_edit_request_triggers_single_self_repair_and_reports_zero_changes()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;
    if director.executable_path.is_none() {
        return Err("sea-agent-director binary missing; run cargo build --workspace first".into());
    }

    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls_ref = Arc::clone(&llm_calls);

    let provider = Arc::new(MockAiProvider::with_agent_handler(move |_req| {
        let call_idx = llm_calls_ref.fetch_add(1, Ordering::SeqCst);
        if call_idx == 0 {
            return Ok(AiAgentTurnResponse {
                text: Some(
                    "検証が完了しました。\n```json\n{\"type\":\"batch\",\"commands\":[]}\n```"
                        .to_owned(),
                ),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                model: "mock-noop-v1".to_owned(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            });
        }
        Ok(AiAgentTurnResponse {
            text: Some("対象クリップが見つかりませんでした。".to_owned()),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-noop-v1".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        })
    }));

    let mut controller = AppController::new();
    controller.ai_chat.selected_provider = AiProviderKind::Mock;
    controller.start_agent_prompt_with_provider(
        "字幕の縁取り色を赤に修正してください",
        provider,
        Some(director),
    )?;

    poll_until(&mut controller, Duration::from_secs(20), |c| {
        !c.ai_chat.is_busy() && c.ai_chat.turns.len() >= 2
    })?;

    // 初回 + 自己修復 1 回のみ = 計 2 回呼び出し（無限リトライしない）
    assert_eq!(llm_calls.load(Ordering::SeqCst), 2);
    assert!(controller.ai_chat.pending_proposal.is_none());
    let last_turn = controller
        .ai_chat
        .turns
        .last()
        .ok_or("missing assistant turn")?;
    assert!(
        last_turn
            .text
            .contains("タイムラインへの変更は行われませんでした（0 件）")
    );
    assert!(!last_turn.text.contains("\"commands\":[]"));

    let _ = controller.cancel_ai_prompt();
    Ok(())
}
