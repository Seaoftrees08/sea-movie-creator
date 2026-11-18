//! `sea-movie-creator` 用 自律ムービー構成エージェントプラグイン (`sea-agent-director`)。
//!
//! `stdin`/`stdout` の双方向 NDJSON (`sea_agent_v1`) で本体と通信し、
//! セッション中は常駐してフォローアップ調整やセッション復元に応答します。

mod director;
mod protocol_io;

use std::io;

use sea_agent_protocol::HostToAgentMessage;

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

        match msg {
            HostToAgentMessage::StartTask {
                goal,
                attached_folders,
                capabilities,
                tools,
                timeline_summary,
                agent_memory,
                ..
            } => {
                transport.available_tools = tools;
                let mut ctx =
                    DirectorSession::from_host_init(capabilities, agent_memory, timeline_summary);
                ctx.handle_start_task(&mut transport, &goal, &attached_folders);
                session = Some(ctx);
            }
            HostToAgentMessage::ResumeSession {
                capabilities,
                tools,
                timeline_summary,
                agent_memory,
                ..
            } => {
                transport.available_tools = tools;
                let ctx =
                    DirectorSession::from_host_init(capabilities, agent_memory, timeline_summary);
                session = Some(ctx);
                transport.status(
                    "保存済みセッションを復元しました。続けて調整指示を入力できます。",
                    None,
                    None,
                );
            }
            HostToAgentMessage::FollowUp {
                prompt,
                attached_folders,
                timeline_summary,
                ..
            } => {
                if let Some(ref mut ctx) = session {
                    ctx.handle_follow_up(
                        &mut transport,
                        &prompt,
                        &attached_folders,
                        timeline_summary,
                    );
                } else {
                    transport.send_error(
                        "session_not_initialized",
                        "StartTask または ResumeSession より前に FollowUp が送信されました。",
                        true,
                    );
                }
            }
            HostToAgentMessage::Cancel { .. } => {
                break;
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
    }
}
