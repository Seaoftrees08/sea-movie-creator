//! `sea-movie-creator` 用 自律ムービー構成エージェントプラグイン (`sea-agent-director`)。
//!
//! `stdin`/`stdout` の双方向 NDJSON (`sea_agent_v1`) で本体と通信し、
//! セッション中は常駐してフォローアップ調整やセッション復元に応答します。

mod director;
mod protocol_io;

use std::io;
use std::ops::ControlFlow;

use sea_agent_protocol::{HostToAgentMessage, PROTOCOL_VERSION};

use crate::director::DirectorSession;
use crate::protocol_io::AgentTransport;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut transport = AgentTransport::new(stdin.lock(), stdout.lock());
    let mut session: Option<DirectorSession> = None;

    loop {
        let msg = match transport.recv_message() {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(err) => {
                let _ = io::Write::write_all(
                    &mut io::stderr(),
                    format!("[sea-agent-director] NDJSON read error: {err}\n").as_bytes(),
                );
                transport.send_error("protocol_error", err, false);
                break;
            }
        };

        if matches!(
            dispatch_host_message(&mut transport, &mut session, msg),
            ControlFlow::Break(())
        ) {
            break;
        }
    }
}

fn dispatch_host_message(
    transport: &mut AgentTransport<'_>,
    session: &mut Option<DirectorSession>,
    msg: HostToAgentMessage,
) -> ControlFlow<()> {
    match msg {
        HostToAgentMessage::StartTask {
            protocol_version,
            goal,
            attached_folders,
            attached_clips,
            capabilities,
            tools,
            timeline_summary,
            agent_memory,
            ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                transport.send_error(
                    "unsupported_protocol",
                    format!(
                        "プロトコルバージョン不一致: 期待 `{PROTOCOL_VERSION}`, 受信 `{protocol_version}`"
                    ),
                    false,
                );
                return ControlFlow::Break(());
            }
            transport.available_tools = tools;
            let mut ctx =
                DirectorSession::from_host_init(capabilities, agent_memory, timeline_summary);
            let flow = ctx.handle_start_task(transport, &goal, &attached_folders, &attached_clips);
            if matches!(flow, ControlFlow::Break(())) {
                return ControlFlow::Break(());
            }
            *session = Some(ctx);
        }
        HostToAgentMessage::ResumeSession {
            protocol_version,
            capabilities,
            tools,
            timeline_summary,
            agent_memory,
            ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                transport.send_error(
                    "unsupported_protocol",
                    format!(
                        "プロトコルバージョン不一致: 期待 `{PROTOCOL_VERSION}`, 受信 `{protocol_version}`"
                    ),
                    false,
                );
                return ControlFlow::Break(());
            }
            transport.available_tools = tools;
            let ctx = DirectorSession::from_host_init(capabilities, agent_memory, timeline_summary);
            *session = Some(ctx);
            transport.status(
                "保存済みセッションを復元しました。続けて調整指示を入力できます。",
                None,
                None,
            );
        }
        HostToAgentMessage::FollowUp {
            prompt,
            attached_folders,
            attached_clips,
            timeline_summary,
        } => {
            if let Some(ctx) = session.as_mut() {
                return ctx.handle_follow_up(
                    transport,
                    &prompt,
                    &attached_folders,
                    &attached_clips,
                    timeline_summary,
                );
            }
            transport.send_error(
                "session_not_initialized",
                "StartTask または ResumeSession より前に FollowUp が送信されました。",
                true,
            );
        }
        HostToAgentMessage::Cancel { .. } => {
            return ControlFlow::Break(());
        }
        HostToAgentMessage::UserAnswer { .. }
        | HostToAgentMessage::ToolResult { .. }
        | HostToAgentMessage::LlmResponse { .. } => {
            let _ = io::Write::write_all(
                &mut io::stderr(),
                b"[sea-agent-director] ignored orphan reply at idle loop\n",
            );
        }
    }
    ControlFlow::Continue(())
}
