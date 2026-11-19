//! `stdin`/`stdout` の NDJSON 往復通信を同期 RPC として扱うドライバ。
//!
//! ホストへの出力はすべて `stdout` 1 行 JSON とし、診断ログは `stderr` へのみ書き出します。

use std::io::{self, BufReader, StdinLock, StdoutLock};

use sea_agent_protocol::{
    AgentAskChoice, AgentAskField, AgentLlmMessage, AgentLlmToolCall, AgentToHostMessage,
    AgentToolSpec, AgentUserAnswerPayload, HostToAgentMessage, protocol_error_with_memory,
    read_ndjson_line, write_ndjson_line,
};
use serde::Serialize;
use serde_json::Value;

/// RPC 待機中にホストからキャンセル要求を受け取った場合の制御結果。
#[derive(Debug)]
pub enum RpcError {
    /// ユーザーがキャンセルした（または EOF 切断）。
    Cancelled(String),
    /// ストリーム I/O またはプロトコルエラー。
    Io(String),
    /// ホストまたは LLM / ツールが論理エラーを返した（プロンプト文脈に利用可能）。
    Remote(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled(r) => write!(f, "cancelled: {r}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Remote(e) => write!(f, "{e}"),
        }
    }
}

/// ホストとの双方向 NDJSON 会話セッション I/O ハンドル。
pub struct AgentTransport<'a> {
    reader: BufReader<StdinLock<'a>>,
    writer: StdoutLock<'a>,
    next_req_seq: u64,
    /// ホストが現在保有する利用可能ツール一覧（`updated_tools` で自動更新）。
    pub available_tools: Vec<AgentToolSpec>,
}

impl<'a> AgentTransport<'a> {
    /// 標準入出力ロックからトランスポートを構築する。
    #[must_use]
    pub fn new(stdin: StdinLock<'a>, stdout: StdoutLock<'a>) -> Self {
        Self {
            reader: BufReader::new(stdin),
            writer: stdout,
            next_req_seq: 1,
            available_tools: Vec::new(),
        }
    }

    fn alloc_request_id(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}_{}", self.next_req_seq);
        self.next_req_seq = self.next_req_seq.saturating_add(1);
        id
    }

    /// 次のホストメッセージを読み取る（EOF は `Ok(None)`）。
    ///
    /// # Errors
    ///
    /// NDJSON 読取・パース失敗時にエラー文字列を返す。
    pub fn recv_message(&mut self) -> Result<Option<HostToAgentMessage>, String> {
        read_ndjson_line(&mut self.reader).map_err(|err| err.to_string())
    }

    /// プラグイン側メッセージを `stdout` へ送出する。
    ///
    /// # Errors
    ///
    /// 書込失敗時にエラー文字列を返す。
    pub fn send_message(&mut self, msg: &AgentToHostMessage) -> Result<(), String> {
        write_ndjson_line(&mut self.writer, msg).map_err(|err| err.to_string())
    }

    /// 進捗ステータスを送信する。
    pub fn status(
        &mut self,
        message: impl Into<String>,
        step: Option<u32>,
        total_steps: Option<u32>,
    ) {
        let _ = self.send_message(&AgentToHostMessage::Status {
            message: message.into(),
            step,
            total_steps,
        });
    }

    /// 回復可能・致命エラーを送信する。
    pub fn send_error(&mut self, code: &str, message: impl Into<String>, recoverable: bool) {
        self.send_error_with_memory(code, message, recoverable, None);
    }

    /// 取得済み解析キャッシュ等の `agent_memory` 部分パッチ付きでエラーを送信する。
    pub fn send_error_with_memory(
        &mut self,
        code: &str,
        message: impl Into<String>,
        recoverable: bool,
        agent_memory_patch: Option<Value>,
    ) {
        let _ = self.send_message(&protocol_error_with_memory(
            code,
            message,
            recoverable,
            agent_memory_patch,
        ));
    }

    /// ホストツールを呼び出し、結果を待つ。
    ///
    /// # Errors
    ///
    /// キャンセル・I/O 障害・ツール実行失敗時に [`RpcError`] を返す。
    pub fn call_tool(&mut self, tool_name: &str, arguments: Value) -> Result<Value, RpcError> {
        let req_id = self.alloc_request_id("tool");
        self.send_message(&AgentToHostMessage::CallTool {
            request_id: req_id.clone(),
            tool_name: tool_name.to_owned(),
            arguments,
        })
        .map_err(RpcError::Io)?;

        loop {
            match self.recv_message().map_err(RpcError::Io)? {
                None => return Err(RpcError::Cancelled("stdin closed".to_owned())),
                Some(HostToAgentMessage::Cancel { reason }) => {
                    return Err(RpcError::Cancelled(
                        reason.unwrap_or_else(|| "host cancelled".to_owned()),
                    ));
                }
                Some(HostToAgentMessage::ToolResult {
                    request_id,
                    ok,
                    result,
                    error,
                    updated_tools,
                }) if request_id == req_id => {
                    if let Some(tools) = updated_tools {
                        self.available_tools = tools;
                    }
                    if ok {
                        return Ok(result.unwrap_or(Value::Null));
                    }
                    return Err(RpcError::Remote(
                        error.unwrap_or_else(|| format!("tool `{tool_name}` failed")),
                    ));
                }
                Some(other) => {
                    let _ = io::Write::write_all(
                        &mut io::stderr(),
                        format!(
                            "[sea-agent-director] ignored unexpected message during tool wait: {other:?}\n"
                        )
                        .as_bytes(),
                    );
                }
            }
        }
    }

    /// ホスト LLM ゲートウェイに推論を依頼し、応答を取得する（スライス借用により不要な全量クローンを避ける）。
    ///
    /// # Errors
    ///
    /// キャンセル・通信障害・プロバイダエラー時に [`RpcError`] を返す。
    pub fn llm_chat(
        &mut self,
        system_prompt: Option<&str>,
        messages: &[AgentLlmMessage],
        tools: &[AgentToolSpec],
        response_json_schema: Option<&Value>,
        prefer_json_object: bool,
    ) -> Result<(String, Vec<AgentLlmToolCall>), RpcError> {
        #[derive(Serialize)]
        struct LlmChatRef<'a> {
            #[serde(rename = "type")]
            msg_type: &'static str,
            request_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            system_prompt: Option<&'a str>,
            messages: &'a [AgentLlmMessage],
            tools: &'a [AgentToolSpec],
            #[serde(skip_serializing_if = "Option::is_none")]
            response_json_schema: Option<&'a Value>,
            prefer_json_object: bool,
        }

        let req_id = self.alloc_request_id("llm");
        let wire = LlmChatRef {
            msg_type: "llm_chat",
            request_id: &req_id,
            system_prompt,
            messages,
            tools,
            response_json_schema,
            prefer_json_object,
        };
        write_ndjson_line(&mut self.writer, &wire).map_err(|err| RpcError::Io(err.to_string()))?;

        loop {
            match self.recv_message().map_err(RpcError::Io)? {
                None => return Err(RpcError::Cancelled("stdin closed".to_owned())),
                Some(HostToAgentMessage::Cancel { reason }) => {
                    return Err(RpcError::Cancelled(
                        reason.unwrap_or_else(|| "host cancelled".to_owned()),
                    ));
                }
                Some(HostToAgentMessage::LlmResponse {
                    request_id,
                    ok,
                    text,
                    tool_calls,
                    error,
                    ..
                }) if request_id == req_id => {
                    if ok {
                        return Ok((text, tool_calls));
                    }
                    return Err(RpcError::Remote(
                        error.unwrap_or_else(|| "llm_chat returned failure".to_owned()),
                    ));
                }
                Some(_) => {}
            }
        }
    }

    /// ユーザーへリッチ対話質問カードを提示し、回答を待つ。
    ///
    /// # Errors
    ///
    /// キャンセルまたは通信切断時に [`RpcError`] を返す。
    pub fn ask_user(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        fields: Vec<AgentAskField>,
        choices: Vec<AgentAskChoice>,
        allow_skip: bool,
    ) -> Result<AgentUserAnswerPayload, RpcError> {
        let req_id = self.alloc_request_id("ask");
        self.send_message(&AgentToHostMessage::AskUser {
            request_id: req_id.clone(),
            title: title.into(),
            message: message.into(),
            fields,
            choices,
            allow_skip,
        })
        .map_err(RpcError::Io)?;

        loop {
            match self.recv_message().map_err(RpcError::Io)? {
                None => return Err(RpcError::Cancelled("stdin closed".to_owned())),
                Some(HostToAgentMessage::Cancel { reason }) => {
                    return Err(RpcError::Cancelled(
                        reason.unwrap_or_else(|| "host cancelled".to_owned()),
                    ));
                }
                Some(HostToAgentMessage::UserAnswer { request_id, answer })
                    if request_id == req_id =>
                {
                    return Ok(answer);
                }
                Some(_) => {}
            }
        }
    }
}
