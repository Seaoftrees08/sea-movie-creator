//! `sea-movie-creator` と外部エージェントプラグイン間でやり取りする
//! 双方向 NDJSON（`sea_agent_v1`）ワイヤープロトコル型と入出力ヘルパ。
//!
//! 本クレートは `serde` / `serde_json` のみに依存し、Tokio や重い非同期ランタイムを持ち込みません。

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 双方向エージェントプロトコル識別子（`agent-plugin.json` の `protocol` と一致）。
pub const PROTOCOL_VERSION: &str = "sea_agent_v1";

/// `ask_user` リッチ対話カード内の入力フィールド種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAskFieldKind {
    /// 1 行テキスト入力。
    #[default]
    Text,
    /// 複数行テキスト入力。
    Textarea,
    /// 数値入力。
    Number,
}

/// `ask_user` リッチ対話カード内の入力欄定義。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAskField {
    /// 回答マップのキーとなるフィールド識別子。
    pub id: String,
    /// ユーザーに表示するラベル。
    pub label: String,
    /// 入力欄のプレースホルダ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// 初期値。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// フィールド入力種別。
    #[serde(default)]
    pub kind: AgentAskFieldKind,
}

/// `ask_user` リッチ対話カード内の選択肢ボタン定義。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAskChoice {
    /// 選択結果としてエージェントへ返される識別子。
    pub id: String,
    /// ボタン表示ラベル。
    pub label: String,
    /// 補足説明（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 主アクション（強調表示）かどうか。
    #[serde(default)]
    pub is_primary: bool,
}

/// ユーザーが `ask_user` カードへ返却した回答内容。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentUserAnswerPayload {
    /// 押下された選択肢ボタン ID（存在する場合）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_id: Option<String>,
    /// 入力フォーム値（`field.id -> value`）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_values: BTreeMap<String, String>,
    /// 自由入力テキスト（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
    /// ユーザーが質問をスキップ（「おまかせで続行」等）したかどうか。
    #[serde(default)]
    pub skipped: bool,
}

/// Anthropic 等がマルチターン Tool Use で再送を要求する署名付き思考ブロック。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentThinkingBlock {
    /// 思考テキスト本体。
    #[serde(default)]
    pub thinking: String,
    /// プロバイダ発行の署名文字列。
    #[serde(default)]
    pub signature: String,
    /// Anthropic `redacted_thinking` ブロックの生データ（任意・後方互換）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_data: Option<String>,
}

/// `llm_chat` / `llm_response` で扱うツール呼び出し記述。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLlmToolCall {
    /// ホストまたはプロバイダが採番した呼び出し ID。
    pub id: String,
    /// 呼び出すツール名。
    pub name: String,
    /// 引数 JSON オブジェクト。
    #[serde(default)]
    pub arguments: Value,
    /// Gemini 3/4 等が返すツール呼び出し思考署名（任意・後方互換）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// エージェントがホストの LLM ゲートウェイへ送信する会話メッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLlmMessage {
    /// `"system"` / `"user"` / `"assistant"` / `"tool"`。
    pub role: String,
    /// 本文テキスト。
    #[serde(default)]
    pub text: String,
    /// ホスト側の許可フォルダ検証・サムネイル縮小を経て添付される画像ファイルパス群。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_paths: Vec<String>,
    /// assistant メッセージに保持された署名付き思考ブロック群（任意・後方互換）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_blocks: Vec<AgentThinkingBlock>,
    /// assistant メッセージに含めるツール呼び出し要求。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AgentLlmToolCall>,
    /// `role == "tool"` の場合に紐付ける対象 `AgentLlmToolCall::id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// ホストからエージェントへ開示する自己記述ツール定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolSpec {
    /// 一意なツール識別子。
    pub name: String,
    /// 役割・呼び出し条件・引数の説明文。
    pub description: String,
    /// 引数用 JSON Schema オブジェクト。
    pub input_schema: Value,
    /// 安全分類（`"read_only"` / `"draft_mutation"` / `"external_side_effect"`）。
    pub safety_class: String,
    /// 提供元区分（`"builtin"` / `"plugin:<id>"` 等）。
    #[serde(default)]
    pub source: String,
}

/// ホスト → エージェントプラグイン（`stdin` NDJSON）メッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToAgentMessage {
    /// 新規タスク（またはセッション）の開始指示。
    StartTask {
        /// プロトコル識別子（通常 `"sea_agent_v1"`）。
        protocol_version: String,
        /// ホストが割り当てたセッション ID。
        session_id: String,
        /// ユーザーの目標・指示文。
        goal: String,
        /// ユーザーが明示的に添付・許可したメディアフォルダ一覧。
        #[serde(default)]
        attached_folders: Vec<String>,
        /// 明示的に添付されたタイムライン上クリップ ID / 名前。
        #[serde(default)]
        attached_clips: Vec<String>,
        /// 本体機能・対応コマンド・フォント・インストール済み拡張の自己申告スナップショット。
        capabilities: Value,
        /// ホストで利用可能なツール群（将来の拡張ツールもここに含まれる）。
        #[serde(default)]
        tools: Vec<AgentToolSpec>,
        /// 現在のタイムライン要約テキスト。
        timeline_summary: String,
        /// 引き継ぐセッション記憶（初期値は空オブジェクト等）。
        #[serde(default)]
        agent_memory: Value,
    },
    /// 保存済みセッションの復元指示（プラグイン側メモリとホスト状態を再同期）。
    ResumeSession {
        /// プロトコル識別子（通常 `"sea_agent_v1"`）。
        protocol_version: String,
        /// 復元対象セッション ID。
        session_id: String,
        /// 現在の本体機能・拡張スナップショット。
        capabilities: Value,
        /// 利用可能ツール群。
        #[serde(default)]
        tools: Vec<AgentToolSpec>,
        /// 現在のタイムライン要約。
        timeline_summary: String,
        /// 永続化されていた `agent_memory`。
        #[serde(default)]
        agent_memory: Value,
    },
    /// 同一常駐セッション上でのフォローアップ調整指示（構成適用後の微調整等）。
    FollowUp {
        /// 追加のユーザー指示文。
        prompt: String,
        /// 追加・更新された許可フォルダ一覧。
        #[serde(default)]
        attached_folders: Vec<String>,
        /// 添付クリップ一覧。
        #[serde(default)]
        attached_clips: Vec<String>,
        /// 最新のタイムライン要約テキスト。
        timeline_summary: String,
    },
    /// `ask_user` に対するユーザー回答の返送。
    UserAnswer {
        /// 対応する `AskUser::request_id`。
        request_id: String,
        /// ユーザー回答データ。
        answer: AgentUserAnswerPayload,
    },
    /// `call_tool` の実行結果または承認拒否・エラー通知。
    ToolResult {
        /// 対応する `CallTool::request_id`。
        request_id: String,
        /// ツール実行が成功したか。
        ok: bool,
        /// 成功時の戻り JSON。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// 失敗または拒否時の説明メッセージ。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// ツール登録が動的に変化した場合の最新ツール一覧（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_tools: Option<Vec<AgentToolSpec>>,
    },
    /// `llm_chat` に対するモデル推論応答。
    LlmResponse {
        /// 対応する `LlmChat::request_id`。
        request_id: String,
        /// 推論呼び出しが成功したか。
        ok: bool,
        /// モデルが出力したテキスト。
        #[serde(default)]
        text: String,
        /// モデルが返した署名付き思考ブロック群（任意・後方互換）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        thinking_blocks: Vec<AgentThinkingBlock>,
        /// モデルが要求したツール呼び出し一覧。
        #[serde(default)]
        tool_calls: Vec<AgentLlmToolCall>,
        /// 完了理由（`"stop"`, `"tool_calls"` 等）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        /// 失敗時のエラー詳細。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 実行中タスクの中断要求。
    Cancel {
        /// 中断理由メモ（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// エージェントプラグイン → ホスト（`stdout` NDJSON）メッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentToHostMessage {
    /// 処理進捗ステータス通知。
    Status {
        /// UI へ表示する進捗テキスト。
        message: String,
        /// 現在のステップ番号（1 始まり任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u32>,
        /// 想定総ステップ数（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_steps: Option<u32>,
    },
    /// ホストツールの呼び出し要求。
    CallTool {
        /// リクエスト識別子。
        request_id: String,
        /// 呼び出すツール名。
        tool_name: String,
        /// 引数 JSON。
        #[serde(default)]
        arguments: Value,
    },
    /// ホストの LLM ゲートウェイを通じた推論要求。
    LlmChat {
        /// リクエスト識別子。
        request_id: String,
        /// システムプロンプト（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        /// 会話履歴メッセージ一覧。
        messages: Vec<AgentLlmMessage>,
        /// モデルへ提示するツール一覧（任意）。
        #[serde(default)]
        tools: Vec<AgentToolSpec>,
        /// 構造化出力を希望する場合の JSON Schema（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_json_schema: Option<Value>,
        /// JSON オブジェクト形式出力を優先するか。
        #[serde(default)]
        prefer_json_object: bool,
    },
    /// チャット内リッチ対話カードを用いたユーザー確認質問。
    AskUser {
        /// リクエスト識別子。
        request_id: String,
        /// カード見出し。
        title: String,
        /// 質問本文。
        message: String,
        /// 入力フォーム定義。
        #[serde(default)]
        fields: Vec<AgentAskField>,
        /// クイック選択ボタン定義。
        #[serde(default)]
        choices: Vec<AgentAskChoice>,
        /// スキップ（「おまかせで続行」）ボタンを許可するか。
        #[serde(default)]
        allow_skip: bool,
    },
    /// 検証済みタイムライン適用提案（UI 上の適用差分カードまたは Autopilot 自動適用対象）。
    Proposal {
        /// 構成内容・工夫点を説明する Markdown 要約。
        summary_markdown: String,
        /// 本体 `EditCommand`（通常 `{"type":"batch","commands":[...]}`）の JSON 表現。
        command: Value,
        /// セッションへ永続化・マージする `agent_memory` 更新パッチ（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_memory_patch: Option<Value>,
        /// フォローアップ微調整用に保持する `MovieBlueprint` 等の高レベル設計 JSON（任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blueprint: Option<Value>,
    },
    /// エラー報告。
    Error {
        /// 機械判別用エラーコード（例: `"rate_limited"`, `"no_media"`, `"invalid_args"`）。
        code: String,
        /// 人間向けエラー説明。
        message: String,
        /// 同じセッションで再試行・別プロバイダ切替・フォローアップにより回復可能か。
        #[serde(default)]
        recoverable: bool,
        /// 中断前に取得済みの `agent_memory` 更新パッチ（429/403 時の解析キャッシュ保全用・任意）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_memory_patch: Option<Value>,
    },
}

/// エージェント向けエラーメッセージ生成の簡易コンストラクタ。
#[must_use]
pub fn protocol_error(
    code: impl Into<String>,
    message: impl Into<String>,
    recoverable: bool,
) -> AgentToHostMessage {
    protocol_error_with_memory(code, message, recoverable, None)
}

/// 解析キャッシュ等の `agent_memory` 部分更新を伴うエラーメッセージコンストラクタ。
#[must_use]
pub fn protocol_error_with_memory(
    code: impl Into<String>,
    message: impl Into<String>,
    recoverable: bool,
    agent_memory_patch: Option<Value>,
) -> AgentToHostMessage {
    AgentToHostMessage::Error {
        code: code.into(),
        message: message.into(),
        recoverable,
        agent_memory_patch,
    }
}

/// NDJSON 1 行あたりの最大許容バイト数（16 MiB）。
pub const MAX_NDJSON_LINE_BYTES: usize = 16 * 1024 * 1024;

/// 1 メッセージを NDJSON（1 行 JSON + 改行）として書き出してフラッシュする。
///
/// # Errors
///
/// JSON シリアライズ失敗またはストリーム書込・フラッシュ失敗時に `io::Error` を返す。
pub fn write_ndjson_line<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> io::Result<()> {
    let encoded = serde_json::to_string(msg)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    writer.write_all(encoded.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// NDJSON ストリームから次の有効な 1 行を読み取ってデシリアライズする。
///
/// 空行・空白行はスキップし、EOF 到達時は `Ok(None)` を返す。
/// 1 行あたりの読取サイズは `MAX_NDJSON_LINE_BYTES`（16 MiB）までに制限する。
///
/// # Errors
///
/// ストリーム読取失敗、上限サイズ超過、または不正な JSON 行を検出した場合に `io::Error` を返す。
pub fn read_ndjson_line<R: BufRead, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    read_ndjson_line_bounded(reader, MAX_NDJSON_LINE_BYTES)
}

fn read_ndjson_line_bounded<R: BufRead, T: DeserializeOwned>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<T>> {
    let mut buf = Vec::new();
    let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
    loop {
        buf.clear();
        let bytes = std::io::Read::take(&mut *reader, limit).read_until(b'\n', &mut buf)?;
        if bytes == 0 {
            return Ok(None);
        }
        if bytes > max_bytes && !buf.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NDJSON line exceeds max size ({max_bytes} bytes)"),
            ));
        }
        let line = std::str::from_utf8(&buf)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<T>(trimmed)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        return Ok(Some(parsed));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    #[test]
    fn host_and_agent_messages_roundtrip_over_ndjson() -> io::Result<()> {
        let host_msgs = vec![
            HostToAgentMessage::StartTask {
                protocol_version: PROTOCOL_VERSION.to_owned(),
                session_id: "sess_1".to_owned(),
                goal: "結婚式プロフィールムービーを3分で作成".to_owned(),
                attached_folders: vec!["D:/wedding".to_owned()],
                attached_clips: Vec::new(),
                capabilities: json!({"schema_version": 1}),
                tools: vec![AgentToolSpec {
                    name: "inspect_media_folder".to_owned(),
                    description: "Inspect folder".to_owned(),
                    input_schema: json!({"type": "object"}),
                    safety_class: "read_only".to_owned(),
                    source: "builtin".to_owned(),
                }],
                timeline_summary: "empty".to_owned(),
                agent_memory: json!({}),
            },
            HostToAgentMessage::UserAnswer {
                request_id: "ask_1".to_owned(),
                answer: AgentUserAnswerPayload {
                    choice_id: Some("customize".to_owned()),
                    field_values: BTreeMap::from([
                        ("groom".to_owned(), "太郎".to_owned()),
                        ("bride".to_owned(), "花子".to_owned()),
                    ]),
                    free_text: None,
                    skipped: false,
                },
            },
            HostToAgentMessage::FollowUp {
                prompt: "エンディングを5秒延ばして".to_owned(),
                attached_folders: Vec::new(),
                attached_clips: Vec::new(),
                timeline_summary: "slides=10".to_owned(),
            },
            HostToAgentMessage::Cancel {
                reason: Some("user_clicked_stop".to_owned()),
            },
        ];

        let mut buf = Vec::new();
        for msg in &host_msgs {
            write_ndjson_line(&mut buf, msg)?;
        }
        let mut cursor = Cursor::new(buf);
        for expected in &host_msgs {
            let decoded: Option<HostToAgentMessage> = read_ndjson_line(&mut cursor)?;
            assert_eq!(decoded.as_ref(), Some(expected));
        }
        assert_eq!(
            read_ndjson_line::<_, HostToAgentMessage>(&mut cursor)?,
            None
        );
        Ok(())
    }

    #[test]
    fn agent_messages_roundtrip_over_ndjson() -> io::Result<()> {
        let agent_msgs = vec![
            AgentToHostMessage::Status {
                message: "写真を解析中 (4/10)...".to_owned(),
                step: Some(2),
                total_steps: Some(3),
            },
            AgentToHostMessage::AskUser {
                request_id: "ask_1".to_owned(),
                title: "構成の希望を教えてください".to_owned(),
                message: "お二人のお名前と雰囲気を選んでください".to_owned(),
                fields: vec![AgentAskField {
                    id: "names".to_owned(),
                    label: "新郎・新婦名".to_owned(),
                    placeholder: Some("太郎 & 花子".to_owned()),
                    default_value: None,
                    kind: AgentAskFieldKind::Text,
                }],
                choices: vec![AgentAskChoice {
                    id: "auto".to_owned(),
                    label: "おまかせで続行".to_owned(),
                    description: None,
                    is_primary: true,
                }],
                allow_skip: true,
            },
            AgentToHostMessage::Proposal {
                summary_markdown: "3分の構成を作成しました".to_owned(),
                command: json!({"type": "batch", "commands": []}),
                agent_memory_patch: Some(json!({"analyzed_count": 10})),
                blueprint: Some(json!({"target_duration_seconds": 180.0})),
            },
            protocol_error_with_memory(
                "rate_limited",
                "API 制限到達",
                true,
                Some(json!({"analyzed_count": 4})),
            ),
        ];

        let mut agent_buf = Vec::new();
        for msg in &agent_msgs {
            write_ndjson_line(&mut agent_buf, msg)?;
        }
        let mut agent_cursor = Cursor::new(agent_buf);
        for expected in &agent_msgs {
            let decoded: Option<AgentToHostMessage> = read_ndjson_line(&mut agent_cursor)?;
            assert_eq!(decoded.as_ref(), Some(expected));
        }
        assert_eq!(
            read_ndjson_line::<_, AgentToHostMessage>(&mut agent_cursor)?,
            None
        );
        Ok(())
    }

    #[test]
    fn oversized_ndjson_line_returns_invalid_data() -> Result<(), String> {
        let oversized = "{\"type\":\"cancel\"}\n".repeat(10);
        let mut cursor = Cursor::new(oversized.as_bytes());
        let Err(err) = read_ndjson_line_bounded::<_, HostToAgentMessage>(&mut cursor, 8) else {
            return Err("line exceeding byte bound must be rejected".to_owned());
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds max size"));

        // マルチバイト UTF-8 文字の途中でサイズ上限に達してもサイズ超過エラーを返すこと
        let multibyte = format!("{{\"msg\":\"{}\"}}\n", "あ".repeat(10));
        let mut mb_cursor = Cursor::new(multibyte.as_bytes());
        let Err(mb_err) = read_ndjson_line_bounded::<_, HostToAgentMessage>(&mut mb_cursor, 10)
        else {
            return Err("multibyte line exceeding byte bound must be rejected".to_owned());
        };
        assert_eq!(mb_err.kind(), io::ErrorKind::InvalidData);
        assert!(mb_err.to_string().contains("exceeds max size"));
        Ok(())
    }

    #[test]
    fn thinking_blocks_and_thought_signature_roundtrip_and_stay_backward_compatible()
    -> io::Result<()> {
        let msg = HostToAgentMessage::LlmResponse {
            request_id: "req_1".to_owned(),
            ok: true,
            text: "思考完了".to_owned(),
            thinking_blocks: vec![AgentThinkingBlock {
                thinking: "step 1".to_owned(),
                signature: "sig_abc".to_owned(),
                redacted_data: Some("redacted_payload".to_owned()),
            }],
            tool_calls: vec![AgentLlmToolCall {
                id: "call_1".to_owned(),
                name: "get_timeline_summary".to_owned(),
                arguments: json!({}),
                thought_signature: Some("gem_sig_1".to_owned()),
            }],
            finish_reason: Some("tool_calls".to_owned()),
            error: None,
        };
        let mut buf = Vec::new();
        write_ndjson_line(&mut buf, &msg)?;
        let mut cursor = Cursor::new(buf);
        let decoded: Option<HostToAgentMessage> = read_ndjson_line(&mut cursor)?;
        assert_eq!(decoded.as_ref(), Some(&msg));

        // 旧プロトコル JSON（thinking_blocks / thought_signature 欠如）も問題なく読めること
        let legacy_llm_resp = r#"{"type":"llm_response","request_id":"r0","ok":true,"text":"hi","tool_calls":[{"id":"c0","name":"x","arguments":{}}]}"#;
        let parsed_resp: HostToAgentMessage =
            serde_json::from_str(legacy_llm_resp).map_err(io::Error::other)?;
        match parsed_resp {
            HostToAgentMessage::LlmResponse {
                thinking_blocks,
                tool_calls,
                ..
            } => {
                assert_eq!(thinking_blocks, Vec::<AgentThinkingBlock>::new());
                assert_eq!(tool_calls[0].thought_signature, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }

        let legacy_msg = r#"{"role":"assistant","text":"ok","tool_calls":[]}"#;
        let parsed_msg: AgentLlmMessage =
            serde_json::from_str(legacy_msg).map_err(io::Error::other)?;
        assert_eq!(parsed_msg.thinking_blocks, Vec::<AgentThinkingBlock>::new());
        Ok(())
    }
}
