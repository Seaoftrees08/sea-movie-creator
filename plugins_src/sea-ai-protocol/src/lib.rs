//! `sea-movie-creator` 本体と AI CLI プラグイン（`sea-ai-antigravity` / `sea-ai-acp`）で共有する
//! 会話履歴フォーマット・多段圧縮・ACP チャンク抽出ヘルパー。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `# Conversation History` セクションの最大バイト数（`128 KiB`）。
pub const MAX_HISTORY_PROMPT_BYTES: usize = 128 * 1024;

/// `# Original User Instruction` ピン留め対象メッセージ単体の最大バイト数（`64 KiB`）。
pub const PINNED_INSTRUCTION_MAX_BYTES: usize = 64 * 1024;

/// 自律エージェントの元指示を識別・ピン留めするための先頭マーカー。
pub const ORIGINAL_INSTRUCTION_MARKER: &str = "# Original User Instruction";

/// 外部 AI プラグインの 1 呼び出しあたり許可する最大画像枚数。
pub const MAX_PLUGIN_VISION_IMAGES: usize = 12;

/// 外部 AI プラグインの 1 呼び出しあたり許可する画像 Base64 データ合計上限（8 MiB）。
pub const MAX_PLUGIN_VISION_BASE64_BYTES: usize = 8 * 1024 * 1024;

/// 外部 AI プラグインおよび ACP `session/prompt` へ渡す画像入力パート。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpImageInput {
    /// 画像 MIME タイプ（例: `"image/jpeg"`）。
    pub mime_type: String,
    /// Base64 エンコード済み画像バイト列。
    pub base64_data: String,
}

/// 外部プラグインへ送信する画像配列の枚数・サイズ・必須項目の妥当性を検証する。
///
/// # Errors
///
/// 枚数上限・合計サイズ上限超過、または `mime_type` / `base64_data` が空の場合にエラー文字列を返す。
pub fn validate_acp_images(images: &[AcpImageInput]) -> Result<(), String> {
    if images.len() > MAX_PLUGIN_VISION_IMAGES {
        return Err(format!(
            "plugin vision image count {} exceeds limit {MAX_PLUGIN_VISION_IMAGES}",
            images.len()
        ));
    }
    let mut total_bytes = 0_usize;
    for (idx, img) in images.iter().enumerate() {
        if img.mime_type.trim().is_empty() || img.base64_data.trim().is_empty() {
            return Err(format!(
                "plugin vision image[{idx}] has empty mime_type or base64_data"
            ));
        }
        total_bytes = total_bytes.saturating_add(img.base64_data.len());
    }
    if total_bytes > MAX_PLUGIN_VISION_BASE64_BYTES {
        return Err(format!(
            "plugin vision total base64 size {total_bytes} bytes exceeds limit {MAX_PLUGIN_VISION_BASE64_BYTES} bytes"
        ));
    }
    Ok(())
}

/// ACP `initialize` 応答から画像入力対応 (`promptCapabilities.image`) を読み取る。
///
/// 明示的に `true`/`false` が返された場合は `Some(bool)`、未記載時は `None` を返す。
#[must_use]
pub fn acp_supports_image_prompt(init_resp: &Value) -> Option<bool> {
    init_resp["result"]["agentCapabilities"]["promptCapabilities"]["image"]
        .as_bool()
        .or_else(|| init_resp["result"]["capabilities"]["promptCapabilities"]["image"].as_bool())
}

/// ACP `session/prompt` 用の `prompt` コンテンツブロック配列を構築する。
#[must_use]
pub fn build_acp_prompt_content_blocks(prompt_text: &str, images: &[AcpImageInput]) -> Vec<Value> {
    let mut blocks = Vec::with_capacity(1 + images.len());
    blocks.push(serde_json::json!({
        "type": "text",
        "text": prompt_text,
    }));
    for img in images {
        blocks.push(serde_json::json!({
            "type": "image",
            "mimeType": img.mime_type,
            "data": img.base64_data,
        }));
    }
    blocks
}

const HISTORY_HEADER: &str = "# Conversation History\n";
const OMITTED_MARKER: &str = "...(earlier messages omitted)...";

/// `sea_ai_v1` の `history` 配列要素。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryMessage {
    /// 発言ロール（`system` / `user` / `assistant`）。
    pub role: String,
    /// メッセージ本文。
    pub content: String,
}

/// JSON 値が `null` または空オブジェクト `{}` の場合に `true` を返す。
#[must_use]
pub fn is_empty_json_value(val: &Value) -> bool {
    val.is_null() || val.as_object().is_some_and(serde_json::Map::is_empty)
}

/// ACP `session/update` から抽出した構造化イベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpSessionUpdateEvent {
    /// 本文チャンク（`agent_message_chunk` または種別省略時）。
    MessageDelta(String),
    /// 思考過程チャンク（`agent_thought_chunk`）。
    ThoughtDelta(String),
    /// 外部ツール実行状況（`tool_call` / `tool_call_update`）。
    ToolActivity {
        /// 表示用タイトル・コマンド概要。
        title: String,
        /// 状態（例: `"pending"`, `"in_progress"`, `"completed"`, `"failed"`）。
        status: Option<String>,
    },
}

fn extract_update_text(update: &Value) -> Option<String> {
    update["content"]["text"]
        .as_str()
        .or_else(|| update["text"].as_str())
        .or_else(|| update["delta"].as_str())
        .map(ToOwned::to_owned)
}

fn extract_tool_activity_title(update: &Value) -> Option<String> {
    update["title"]
        .as_str()
        .or_else(|| update["content"]["text"].as_str())
        .or_else(|| update["kind"].as_str())
        .or_else(|| update["toolCallId"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// ACP `session/update` の `params` を本文・思考・ツール活動へ分類して抽出する。
#[must_use]
pub fn extract_acp_update_event(params: &Value) -> Option<AcpSessionUpdateEvent> {
    let update = params.get("update").unwrap_or(params);
    match update["sessionUpdate"].as_str() {
        Some("agent_thought_chunk") => extract_update_text(update)
            .filter(|s| !s.is_empty())
            .map(AcpSessionUpdateEvent::ThoughtDelta),
        Some("tool_call" | "tool_call_update") => {
            let title = extract_tool_activity_title(update)?;
            let status = update["status"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            Some(AcpSessionUpdateEvent::ToolActivity { title, status })
        }
        Some("agent_message_chunk") | None => {
            extract_update_text(update).map(AcpSessionUpdateEvent::MessageDelta)
        }
        Some(_) => None,
    }
}

/// ACP `session/update` の `params` から `agent_message_chunk` テキストのみを抽出する。
///
/// `agent_thought_chunk` や `tool_call` 等は混入防止のため `None` を返す。
#[must_use]
pub fn extract_acp_update_message_delta(params: &Value) -> Option<String> {
    match extract_acp_update_event(params) {
        Some(AcpSessionUpdateEvent::MessageDelta(text)) => Some(text),
        _ => None,
    }
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn truncate_utf8_suffix(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn excerpt_head_tail(s: &str, max_bytes: usize, marker: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let sep = format!("\n...({marker})...\n");
    if max_bytes <= sep.len() + 32 {
        return truncate_utf8_prefix(s, max_bytes).to_owned();
    }
    let avail = max_bytes - sep.len();
    let head_budget = (avail * 3) / 5;
    let tail_budget = avail - head_budget;
    let head = truncate_utf8_prefix(s, head_budget);
    let tail = truncate_utf8_suffix(s, tail_budget);
    format!("{head}{sep}{tail}")
}

fn normalize_history_role(role: &str) -> &str {
    let trimmed = role.trim();
    if trimmed.is_empty() { "user" } else { trimmed }
}

fn format_truncated_entry(role: &str, content: &str, max_entry_bytes: usize) -> String {
    let prefix = format!("[{role}]\n");
    let full_len = prefix.len() + content.len();
    if full_len <= max_entry_bytes {
        return format!("{prefix}{content}");
    }
    let suffix = "...(truncated)";
    let body_budget = max_entry_bytes.saturating_sub(prefix.len() + suffix.len());
    if body_budget == 0 {
        return truncate_utf8_prefix(&format!("{prefix}{content}"), max_entry_bytes).to_owned();
    }
    format!(
        "{prefix}{}{suffix}",
        truncate_utf8_prefix(content, body_budget)
    )
}

fn format_history_entry(msg: &HistoryMessage, max_entry_bytes: Option<usize>) -> String {
    let role = normalize_history_role(&msg.role);
    match max_entry_bytes {
        Some(cap) => format_truncated_entry(role, &msg.content, cap),
        None => format!("[{role}]\n{}", msg.content),
    }
}

fn compress_history_entries(
    history: &[HistoryMessage],
    entries: &mut [String],
    pinned_idx: Option<usize>,
    session_cap: usize,
    old_tool_cap: usize,
    last_tool_cap: Option<usize>,
) {
    let last_tool_idx = history
        .iter()
        .rposition(|m| m.content.trim_start().starts_with("[ToolResult "));
    for (idx, msg) in history.iter().enumerate() {
        if pinned_idx == Some(idx) {
            continue;
        }
        let trimmed = msg.content.trim_start();
        let role = normalize_history_role(&msg.role);
        if trimmed.starts_with("# Session Context") {
            let compressed = excerpt_head_tail(
                &msg.content,
                session_cap,
                "Session Context 圧縮済み: 必要なら get_timeline_summary を呼び出してください",
            );
            entries[idx] = format!("[{role}]\n{compressed}");
        } else if trimmed.starts_with("[ToolResult ") {
            let target_cap = if last_tool_idx == Some(idx) {
                last_tool_cap
            } else {
                Some(old_tool_cap)
            };
            if let Some(cap) = target_cap {
                let compressed = excerpt_head_tail(&msg.content, cap, "ToolResult 省略");
                entries[idx] = format!("[{role}]\n{compressed}");
            }
        }
    }
}

fn joined_history_bytes(entries: &[String]) -> usize {
    let body: usize = entries.iter().map(String::len).sum();
    let seps = entries.len().saturating_sub(1) * 2;
    HISTORY_HEADER.len() + body + seps
}

fn render_omitted_blocks(entries: &[String], keep: &[bool]) -> String {
    let mut blocks = Vec::new();
    let mut in_omitted = false;
    for (idx, entry) in entries.iter().enumerate() {
        if keep[idx] {
            in_omitted = false;
            blocks.push(entry.as_str());
        } else if !in_omitted {
            in_omitted = true;
            blocks.push(OMITTED_MARKER);
        }
    }
    format!("{HISTORY_HEADER}{}", blocks.join("\n\n"))
}

fn fit_pinned_entry_within_budget(
    entries: &mut [String],
    keep: &mut [bool],
    pin: usize,
    max_bytes: usize,
) {
    keep[pin] = true;
    let rendered_len = render_omitted_blocks(entries, keep).len();
    if rendered_len <= max_bytes {
        return;
    }
    let excess = rendered_len - max_bytes;
    let suffix = "...(truncated)";
    if entries[pin].len() <= excess + suffix.len() + 16 {
        keep[pin] = false;
        return;
    }
    let target_len = entries[pin].len() - excess;
    let prefix_len = target_len.saturating_sub(suffix.len());
    let truncated = truncate_utf8_prefix(&entries[pin], prefix_len).to_owned();
    entries[pin] = format!("{truncated}{suffix}");
    if render_omitted_blocks(entries, keep).len() > max_bytes {
        keep[pin] = false;
    }
}

fn assemble_omitted_history(
    entries: &mut [String],
    pinned_idx: Option<usize>,
    max_bytes: usize,
) -> String {
    let mut keep = vec![false; entries.len()];
    if let Some(pin) = pinned_idx
        && pin < entries.len()
    {
        fit_pinned_entry_within_budget(entries, &mut keep, pin, max_bytes);
    }

    for idx in (0..entries.len()).rev() {
        if keep[idx] {
            continue;
        }
        keep[idx] = true;
        if render_omitted_blocks(entries, &keep).len() > max_bytes {
            keep[idx] = false;
            break;
        }
    }

    if keep.iter().any(|&k| k) {
        return render_omitted_blocks(entries, &keep);
    }

    let fallback_src = pinned_idx
        .and_then(|pin| entries.get(pin))
        .or_else(|| entries.last())
        .map_or("", String::as_str);
    if max_bytes <= HISTORY_HEADER.len() + 4 {
        return truncate_utf8_prefix(fallback_src, max_bytes).to_owned();
    }
    let body_budget = max_bytes - HISTORY_HEADER.len();
    format!(
        "{HISTORY_HEADER}{}",
        truncate_utf8_prefix(fallback_src, body_budget)
    )
}

/// 会話履歴配列を `max_bytes` 以内の `# Conversation History` セクションへ整形する。
///
/// 履歴内に `# Original User Instruction` マーカー付きメッセージが存在する場合は
/// ピン留め保護し、`# Session Context` および `[ToolResult ` の多段圧縮を優先したうえで、
/// それでも収まらない場合のみ中間ターンを `...(earlier messages omitted)...` で省略する。
#[must_use]
pub fn format_conversation_history_section(history: &[HistoryMessage], max_bytes: usize) -> String {
    if history.is_empty() || max_bytes == 0 {
        return String::new();
    }
    let pinned_idx = history.iter().rposition(|m| {
        m.content
            .trim_start()
            .starts_with(ORIGINAL_INSTRUCTION_MARKER)
    });
    let overhead_reserve = HISTORY_HEADER.len() + (OMITTED_MARKER.len() + 4) * 2;
    let pin_cap =
        PINNED_INSTRUCTION_MAX_BYTES.min(max_bytes.saturating_sub(overhead_reserve).max(64));

    let mut entries: Vec<String> = history
        .iter()
        .enumerate()
        .map(|(idx, msg)| {
            let cap = (pinned_idx == Some(idx)).then_some(pin_cap);
            format_history_entry(msg, cap)
        })
        .collect();

    if joined_history_bytes(&entries) > max_bytes {
        compress_history_entries(
            history,
            &mut entries,
            pinned_idx,
            24 * 1024,
            16 * 1024,
            None,
        );
    }
    if joined_history_bytes(&entries) > max_bytes {
        compress_history_entries(history, &mut entries, pinned_idx, 8 * 1024, 4 * 1024, None);
    }
    for last_tool_cap in [16 * 1024, 8 * 1024, 4 * 1024] {
        if joined_history_bytes(&entries) <= max_bytes {
            break;
        }
        compress_history_entries(
            history,
            &mut entries,
            pinned_idx,
            8 * 1024,
            4 * 1024,
            Some(last_tool_cap),
        );
    }
    if joined_history_bytes(&entries) <= max_bytes {
        return format!("{HISTORY_HEADER}{}", entries.join("\n\n"));
    }
    assemble_omitted_history(&mut entries, pinned_idx, max_bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn two_gap_omission_around_pinned_instruction_never_exceeds_max_bytes() {
        let max_bytes = 2_048;
        let history = vec![
            HistoryMessage {
                role: "user".to_owned(),
                content: format!("# Session Context\n{}", "文脈".repeat(600)),
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: format!("{ORIGINAL_INSTRUCTION_MARKER}\n字幕の縁取りを赤に変更して"),
            },
            HistoryMessage {
                role: "assistant".to_owned(),
                content: format!("中間応答 {}", "あ".repeat(900)),
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: "[ToolResult apply_draft_edit_command status=OK]\n{\"ok\":true}"
                    .to_owned(),
            },
        ];

        let section = format_conversation_history_section(&history, max_bytes);
        assert!(
            section.len() <= max_bytes,
            "section len {} exceeded max_bytes {}",
            section.len(),
            max_bytes
        );
        assert!(section.contains("字幕の縁取りを赤に変更して"));
        assert!(section.contains("[ToolResult apply_draft_edit_command"));
        assert_eq!(section.matches(OMITTED_MARKER).count(), 2);
    }

    #[test]
    fn huge_latest_tool_result_is_compressed_rather_than_dropped() {
        let max_bytes = 12 * 1024;
        let history = vec![
            HistoryMessage {
                role: "user".to_owned(),
                content: format!("{ORIGINAL_INSTRUCTION_MARKER}\n最初の依頼"),
            },
            HistoryMessage {
                role: "user".to_owned(),
                content: format!(
                    "[ToolResult inspect status=OK]\n{{\"head\":1,\"body\":\"{}\",\"tail\":99}}",
                    "デ".repeat(20_000)
                ),
            },
        ];

        let section = format_conversation_history_section(&history, max_bytes);
        assert!(section.len() <= max_bytes);
        assert!(section.contains("最初の依頼"));
        assert!(section.contains("[ToolResult inspect status=OK]"));
        assert!(section.contains("\"tail\":99"));
    }

    #[test]
    fn tiny_budget_with_multibyte_chars_is_utf8_safe_and_bounded() {
        let history = vec![
            HistoryMessage {
                role: "user".to_owned(),
                content: format!("{ORIGINAL_INSTRUCTION_MARKER}\n{}", "あ".repeat(500)),
            },
            HistoryMessage {
                role: "assistant".to_owned(),
                content: "お".repeat(500),
            },
        ];
        for cap in [120, 256, 512] {
            let section = format_conversation_history_section(&history, cap);
            assert!(section.len() <= cap);
        }
    }

    #[test]
    fn extract_acp_update_message_delta_filters_non_message_chunks() {
        let thought = json!({
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking" }
            }
        });
        assert_eq!(extract_acp_update_message_delta(&thought), None);

        let tool = json!({
            "update": {
                "sessionUpdate": "tool_call",
                "content": { "type": "text", "text": "running tool" }
            }
        });
        assert_eq!(extract_acp_update_message_delta(&tool), None);

        let msg = json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "完了しました。" }
            }
        });
        assert_eq!(
            extract_acp_update_message_delta(&msg).as_deref(),
            Some("完了しました。")
        );
        assert_eq!(
            extract_acp_update_event(&thought),
            Some(AcpSessionUpdateEvent::ThoughtDelta("thinking".to_owned()))
        );
        assert_eq!(
            extract_acp_update_event(&tool),
            Some(AcpSessionUpdateEvent::ToolActivity {
                title: "running tool".to_owned(),
                status: None,
            })
        );
        let named_tool = json!({
            "update": {
                "sessionUpdate": "tool_call_update",
                "title": "Inspecting timeline",
                "status": "in_progress"
            }
        });
        assert_eq!(
            extract_acp_update_event(&named_tool),
            Some(AcpSessionUpdateEvent::ToolActivity {
                title: "Inspecting timeline".to_owned(),
                status: Some("in_progress".to_owned()),
            })
        );
    }
}
