//! AI 出力テキストから `EditCommand` を安全抽出・事前ドライラン検証・差分要約するモジュール。

use serde_json::Value;
use thiserror::Error;

use crate::core::ids::{ClipRef, LayerRef};
use crate::timeline::command::{CommandError, EditCommand, ParameterTarget};
use crate::timeline::dispatcher::CommandDispatcher;
use crate::timeline::document::ProjectDocument;

/// ドライラン検証を通過した適用待ちの編集計画。
#[derive(Debug, Clone, PartialEq)]
pub struct AiEditPlan {
    /// 抽出・正規化されたコマンド JSON 文字列。
    pub raw_json: String,
    /// パース済み統一編集コマンド。
    pub command: EditCommand,
    /// モデル応答に含まれるテキスト補足（`"message"` / `"reply"` フィールドや JSON 外テキスト）。
    pub reply_note: Option<String>,
    /// UI の差分カードへ表示する人間向け要約行一覧。
    pub summary_lines: Vec<String>,
    /// 変更対象となる識別子一覧。
    pub affected_targets: Vec<String>,
}

impl AiEditPlan {
    /// タイムライン変更を伴わない空バッチ（応答専用）計画かどうかを返す。
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        matches!(&self.command, EditCommand::Batch { commands } if commands.is_empty())
    }
}

/// AI 出力のパースまたは事前検証失敗エラー。
#[derive(Debug, Error)]
pub enum AiPlanError {
    /// AI 応答から JSON コマンドが見つからなかった。
    #[error("AI response did not contain a JSON command: {0}")]
    NoJsonCommand(String),
    /// JSON は存在したが [`EditCommand`] として解釈できなかった。
    #[error("invalid EditCommand JSON: {0}")]
    InvalidJsonCommand(String),
    /// 複製ドキュメントへのドライラン検証で拒否された。
    #[error("dry-run validation failed: {0}")]
    ValidationFailed(#[from] CommandError),
}

/// AI 応答テキスト（生 JSON・Markdown コードブロック・説明文混在）から [`EditCommand`] を抽出する。
///
/// # Errors
///
/// JSON ブロックが見つからない場合や `EditCommand` スキーマに適合しない場合。
pub fn extract_edit_command_from_text(raw: &str) -> Result<(String, EditCommand), AiPlanError> {
    let (raw_json, command, _) = extract_edit_command_and_note(raw)?;
    Ok((raw_json, command))
}

fn extract_edit_command_and_note(
    raw: &str,
) -> Result<(String, EditCommand, Option<String>), AiPlanError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AiPlanError::NoJsonCommand(
            "empty model response".to_owned(),
        ));
    }

    let fenced_blocks = scan_fenced_blocks(trimmed);
    let mut candidates = Vec::new();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        candidates.push(trimmed.to_owned());
    }
    for (_, _, json_body) in &fenced_blocks {
        if let Some(body) = json_body {
            candidates.push(body.clone());
        }
    }
    if candidates.is_empty()
        && let Some((start, end)) = find_first_balanced_json_span(trimmed)
    {
        candidates.push(trimmed[start..end].to_owned());
    }

    if candidates.is_empty() {
        let preview: String = trimmed.chars().take(120).collect();
        return Err(AiPlanError::NoJsonCommand(preview));
    }

    let mut last_err = String::new();
    for cand in candidates {
        match parse_command_candidate(&cand) {
            Ok((normalized, cmd, json_note)) => {
                let reply_note =
                    json_note.or_else(|| strip_outside_json_text(trimmed, &fenced_blocks));
                return Ok((normalized, cmd, reply_note));
            }
            Err(err) => last_err = err,
        }
    }

    Err(AiPlanError::InvalidJsonCommand(last_err))
}

/// すでにデシリアライズ済みの [`EditCommand`] を複製ドキュメント上で直接ドライラン検証する。
///
/// 不要な JSON 再シリアライズ／再パースを行わずに検証済み [`AiEditPlan`] を構築したい場合に利用します。
///
/// # Errors
///
/// ドキュメント不変条件・存在チェック違反時に [`AiPlanError`] を返す。
pub fn dry_run_validate_command(
    doc: &ProjectDocument,
    command: EditCommand,
    reply_note: Option<String>,
) -> Result<AiEditPlan, AiPlanError> {
    let is_no_op = matches!(&command, EditCommand::Batch { commands } if commands.is_empty());
    if is_no_op {
        doc.validate().map_err(CommandError::from)?;
        return Ok(AiEditPlan {
            raw_json: String::new(),
            command,
            reply_note,
            summary_lines: vec!["タイムライン変更なし（0 ステップ）".to_owned()],
            affected_targets: Vec::new(),
        });
    }

    let mut trial_doc = doc.clone();
    let mut trial_dispatcher = CommandDispatcher::new();
    trial_dispatcher.dispatch(&mut trial_doc, &command)?;

    let mut summary_lines = Vec::new();
    let mut affected_targets = Vec::new();
    summarize_command(&command, &mut summary_lines, &mut affected_targets);

    Ok(AiEditPlan {
        raw_json: String::new(),
        command,
        reply_note,
        summary_lines,
        affected_targets,
    })
}

/// モデル出力をパースしたうえで複製ドキュメント上でドライランし、安全な [`AiEditPlan`] を生成する。
///
/// 呼び出し元の `doc` および履歴には一切の副作用を残さない。
/// 空のバッチ (`{"type":"batch","commands":[]}`) は変更なしの有効な応答（no-op 計画）として受理する。
///
/// # Errors
///
/// JSON 不正またはドキュメント不変条件・存在チェック違反時に [`AiPlanError`] を返す。
pub fn dry_run_validate(
    doc: &ProjectDocument,
    raw_response: &str,
) -> Result<AiEditPlan, AiPlanError> {
    let (raw_json, command, reply_note) = extract_edit_command_and_note(raw_response)?;
    let mut plan = dry_run_validate_command(doc, command, reply_note)?;
    plan.raw_json = raw_json;
    Ok(plan)
}

/// 検証済み計画を実際のドキュメントと [`CommandDispatcher`] へ単一ステップ（単一 Undo 単位）で適用する。
///
/// # Errors
///
/// 適用時点でドキュメントが事前状態から変化しており整合しない場合。
pub fn apply_plan(
    doc: &mut ProjectDocument,
    dispatcher: &mut CommandDispatcher,
    plan: &AiEditPlan,
) -> Result<(), CommandError> {
    if plan.is_no_op() {
        return Ok(());
    }
    dispatcher.dispatch(doc, &plan.command)?;
    Ok(())
}

fn extract_json_reply_note(val: &Value) -> Option<String> {
    ["message", "reply", "answer"]
        .into_iter()
        .find_map(|key| val.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_command_candidate(
    candidate: &str,
) -> Result<(String, EditCommand, Option<String>), String> {
    let val: Value =
        serde_json::from_str(candidate).map_err(|err| format!("JSON syntax error: {err}"))?;
    let json_note = extract_json_reply_note(&val);

    if let Ok(cmd) = serde_json::from_value::<EditCommand>(val.clone()) {
        let pretty = serde_json::to_string_pretty(&cmd).unwrap_or_else(|_| candidate.to_owned());
        return Ok((pretty, cmd, json_note));
    }

    // `"commands": [...]` をルートに持つオブジェクトもバッチに昇格許容する
    if let Some(commands_val) = val.get("commands")
        && let Ok(commands) = serde_json::from_value::<Vec<EditCommand>>(commands_val.clone())
    {
        let batch = EditCommand::Batch { commands };
        let pretty = serde_json::to_string_pretty(&batch).unwrap_or_else(|_| candidate.to_owned());
        return Ok((pretty, batch, json_note));
    }

    Err(format!(
        "does not match EditCommand schema: {}",
        serde_json::from_value::<EditCommand>(val)
            .err()
            .map_or_else(|| "unknown schema error".to_owned(), |e| e.to_string())
    ))
}

/// テキスト（素 JSON・コードフェンス内・説明文混在）から最初の有効な JSON オブジェクトを抽出する。
#[must_use]
pub(crate) fn extract_json_object_from_text(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(val) = serde_json::from_str::<Value>(trimmed)
        && val.is_object()
    {
        return Some(val);
    }
    for (_, _, json_body) in scan_fenced_blocks(trimmed) {
        if let Some(body) = json_body
            && let Some(val) = parse_first_balanced_json_object(&body)
        {
            return Some(val);
        }
    }
    parse_first_balanced_json_object(trimmed)
}

fn parse_first_balanced_json_object(raw: &str) -> Option<Value> {
    let mut offset = 0_usize;
    while let Some(rel_brace) = raw[offset..].find('{') {
        let brace_pos = offset + rel_brace;
        if let Some((0, rel_end)) = find_first_balanced_json_span(&raw[brace_pos..]) {
            let candidate = &raw[brace_pos..brace_pos + rel_end];
            if let Ok(val) = serde_json::from_str::<Value>(candidate)
                && val.is_object()
            {
                return Some(val);
            }
        }
        offset = brace_pos + 1;
    }
    None
}

fn scan_fenced_blocks(raw: &str) -> Vec<(usize, usize, Option<String>)> {
    let mut blocks = Vec::new();
    let mut offset = 0_usize;
    while let Some(rel_start) = raw[offset..].find("```") {
        let fence_start = offset + rel_start;
        let after_ticks = fence_start + 3;
        let Some(rel_newline) = raw[after_ticks..].find('\n') else {
            break;
        };
        let header_end = after_ticks + rel_newline;
        let lang = raw[after_ticks..header_end].trim().to_ascii_lowercase();
        let body_start = header_end + 1;
        let Some(rel_end) = raw[body_start..].find("```") else {
            break;
        };
        let body_end = body_start + rel_end;
        let fence_end = body_end + 3;
        let body = raw[body_start..body_end].trim();
        let json_candidate =
            ((lang.is_empty() || lang == "json") && !body.is_empty()).then(|| body.to_owned());
        blocks.push((fence_start, fence_end, json_candidate));
        offset = fence_end;
    }
    blocks
}

fn strip_outside_json_text(
    trimmed: &str,
    fenced_blocks: &[(usize, usize, Option<String>)],
) -> Option<String> {
    let mut parts = Vec::new();
    if fenced_blocks.is_empty() {
        let (start, end) = find_first_balanced_json_span(trimmed)?;
        let before = trimmed[..start].trim();
        if !before.is_empty() {
            parts.push(before);
        }
        let after = trimmed[end..].trim();
        if !after.is_empty() {
            parts.push(after);
        }
    } else {
        let mut cursor = 0_usize;
        for &(start, end, _) in fenced_blocks {
            let segment = trimmed[cursor..start].trim();
            if !segment.is_empty() {
                parts.push(segment);
            }
            cursor = end;
        }
        let tail = trimmed[cursor..].trim();
        if !tail.is_empty() {
            parts.push(tail);
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn find_first_balanced_json_span(raw: &str) -> Option<(usize, usize)> {
    let start_byte = raw.find('{')?;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (rel_idx, ch) in raw[start_byte..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start_byte + rel_idx + 1;
                    return Some((start_byte, end));
                }
            }
            _ => {}
        }
    }
    None
}

fn summarize_command(
    cmd: &EditCommand,
    summary_lines: &mut Vec<String>,
    targets: &mut Vec<String>,
) {
    match cmd {
        EditCommand::Batch { commands } => {
            summary_lines.push(format!("バッチ実行（{} ステップ）:", commands.len()));
            for inner in commands {
                summarize_command(inner, summary_lines, targets);
            }
        }
        EditCommand::AddClip {
            temp_id,
            id,
            layer_id,
            clip,
        } => {
            let clip_ref = temp_id
                .clone()
                .or_else(|| id.as_ref().map(|v| v.as_str().to_owned()))
                .unwrap_or_else(|| "<auto>".to_owned());
            let layer_lbl = format_layer_ref(layer_id);
            targets.push(clip_ref.clone());
            summary_lines.push(format!(
                "  + クリップ追加 `{}` ({}) -> layer={}, frame={}..{}",
                clip.name,
                clip_ref,
                layer_lbl,
                clip.timeline_start.get(),
                clip.timeline_start
                    .get()
                    .saturating_add(clip.duration.get())
            ));
        }
        EditCommand::SplitClip {
            clip_id,
            split_at_timeline_frame,
            ..
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!(
                "  / クリップ分割 {lbl} @ frame {}",
                split_at_timeline_frame.get()
            ));
        }
        EditCommand::SetParameter { target, path, .. } => {
            let lbl = match target {
                ParameterTarget::Clip { clip_id } => format_clip_ref(clip_id),
                ParameterTarget::AudioPlugin { clip_id, slot_id } => {
                    format!("{}[{}]", format_clip_ref(clip_id), slot_id)
                }
            };
            targets.push(lbl.clone());
            summary_lines.push(format!("  * パラメータ変更 {lbl}.{path:?}"));
        }
        EditCommand::SetPlaybackRate {
            clip_id, new_rate, ..
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!(
                "  * 再生速度変更 {lbl} -> {}/{}",
                new_rate.numerator(),
                new_rate.denominator()
            ));
        }
        other => summarize_other_command(other, summary_lines, targets),
    }
}

fn summarize_other_command(
    cmd: &EditCommand,
    summary_lines: &mut Vec<String>,
    targets: &mut Vec<String>,
) {
    match cmd {
        EditCommand::SetProjectMeta { name, .. } => {
            summary_lines.push(format!("  * プロジェクト設定更新 (name={name:?})"));
        }
        EditCommand::AddAsset { name, .. } => {
            summary_lines.push(format!("  + 素材追加 `{name}`"));
        }
        EditCommand::RemoveAsset { asset_id } => {
            targets.push(asset_id.as_str().to_owned());
            summary_lines.push(format!("  - 素材削除 {}", asset_id.as_str()));
        }
        EditCommand::RelinkAsset { asset_id, .. } => {
            targets.push(asset_id.as_str().to_owned());
            summary_lines.push(format!("  ~ 素材再リンク {}", asset_id.as_str()));
        }
        EditCommand::AddLayer { name, temp_id, .. } => {
            if let Some(t) = temp_id {
                targets.push(t.clone());
            }
            summary_lines.push(format!("  + トラック追加 `{name}`"));
        }
        EditCommand::RemoveLayer { layer_id } => {
            targets.push(layer_id.as_str().to_owned());
            summary_lines.push(format!("  - トラック削除 {}", layer_id.as_str()));
        }
        EditCommand::SetLayerFlags { layer_id, .. } => {
            let lbl = format_layer_ref(layer_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  * トラック設定変更 {lbl}"));
        }
        EditCommand::RemoveClip { clip_id } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  - クリップ削除 {lbl}"));
        }
        EditCommand::MoveClip {
            clip_id,
            new_timeline_start,
            ..
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!(
                "  ~ クリップ移動 {lbl} -> frame {}",
                new_timeline_start.get()
            ));
        }
        EditCommand::TrimLeft {
            clip_id,
            delta_frames,
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  |<- 左トリム {lbl} ({delta_frames:+}f)"));
        }
        EditCommand::TrimRight {
            clip_id,
            delta_frames,
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  ->| 右トリム {lbl} ({delta_frames:+}f)"));
        }
        EditCommand::DuplicateClip {
            clip_id,
            new_timeline_start,
            ..
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!(
                "  + クリップ複製 {lbl} @ frame {}",
                new_timeline_start.get()
            ));
        }
        other => summarize_timeline_and_midpoint_command(other, summary_lines, targets),
    }
}

fn summarize_timeline_and_midpoint_command(
    cmd: &EditCommand,
    summary_lines: &mut Vec<String>,
    targets: &mut Vec<String>,
) {
    match cmd {
        EditCommand::SetTimelineLength { mode } => {
            summary_lines.push(format!("  * タイムライン総尺設定 ({mode:?})"));
        }
        EditCommand::SetSelectionRange { range } => {
            summary_lines.push(format!("  * 選択範囲変更 ({range:?})"));
        }
        EditCommand::AddMidpoint {
            clip_id,
            effect_frame,
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  + 中間点追加 {lbl} @ {effect_frame}"));
        }
        EditCommand::MoveMidpoint {
            clip_id,
            old_effect_frame,
            new_effect_frame,
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!(
                "  ~ 中間点移動 {lbl} ({old_effect_frame} -> {new_effect_frame})"
            ));
        }
        EditCommand::RemoveMidpoint {
            clip_id,
            effect_frame,
        } => {
            let lbl = format_clip_ref(clip_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  - 中間点削除 {lbl} @ {effect_frame}"));
        }
        _ => {}
    }
}

fn format_clip_ref(r: &ClipRef) -> String {
    match r {
        ClipRef::Id(id) => id.as_str().to_owned(),
        ClipRef::Temp(temp) => temp.clone(),
    }
}

fn format_layer_ref(r: &LayerRef) -> String {
    match r {
        LayerRef::Id(id) => id.as_str().to_owned(),
        LayerRef::Temp(temp) => temp.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::LayerId;
    use crate::timeline::document::Layer;

    #[test]
    fn ai_plan_keyframes_sync_and_align_clip_midpoints() -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("AI Midpoints");
        let l1 = LayerId::new("l1")?;
        let c1 = crate::core::ids::ClipId::new("c1")?;
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::Batch {
                commands: vec![
                    EditCommand::AddLayer {
                        temp_id: None,
                        id: Some(l1.clone()),
                        name: "Sub".to_owned(),
                        insert_index: None,
                    },
                    EditCommand::AddClip {
                        temp_id: None,
                        id: Some(c1.clone()),
                        layer_id: LayerRef::Id(l1),
                        clip: Box::new(crate::timeline::command::NewClipSpec {
                            name: "AI Sub".to_owned(),
                            timeline_start: crate::core::time::FrameIndex::ZERO,
                            duration: crate::core::time::FrameCount::new(120)?,
                            effect_offset_frames: 0,
                            effect_duration_frames: None,
                            midpoints: Vec::new(),
                            kind: crate::timeline::command::NewClipKind::Subtitle(
                                crate::media::visual_properties::SubtitleProperties::new("AI"),
                            ),
                        }),
                    },
                ],
            },
        )?;

        let cmd_val = EditCommand::Batch {
            commands: vec![EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(c1.clone()),
                },
                path: crate::timeline::command::ParameterPath::X,
                value: crate::timeline::command::ParameterValue::Animated {
                    param: crate::core::animation::AnimatedParameter::Keyframes {
                        keyframes: vec![
                            crate::core::animation::Keyframe::new(0, -100.0),
                            crate::core::animation::Keyframe::new(45, 50.0),
                            crate::core::animation::Keyframe::new(119, 100.0),
                        ],
                        extrapolation: crate::core::animation::ExtrapolationMode::Clamp,
                    },
                },
            }],
        };
        let raw_response = format!(
            "中間点付きキーフレームを設定します。\n```json\n{}\n```",
            serde_json::to_string_pretty(&cmd_val)?
        );
        let plan = dry_run_validate(&doc, &raw_response)?;
        assert_eq!(plan.summary_lines.len(), 2);
        assert!(plan.affected_targets.iter().any(|t| t.contains("c1")));
        dispatcher.dispatch(&mut doc, &plan.command)?;
        let (_, clip) = doc.find_clip(&c1).ok_or("missing clip")?;
        assert_eq!(clip.midpoints, vec![45]);
        Ok(())
    }

    #[test]
    fn dry_run_accepts_fenced_batch_with_temp_id_without_mutating_doc()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("PlanTest");
        doc.layers.push(Layer::new(LayerId::new("L1")?, "Main"));

        let raw_ai_output = r#"了解しました。以下のバッチを実行します。
```json
{
  "type": "batch",
  "commands": [
    {
      "type": "add_clip",
      "temp_id": "$c1",
      "layer_id": { "id": "L1" },
      "clip": {
        "name": "Subtitle1",
        "timeline_start": 0,
        "duration": 60,
        "kind": {
          "kind": "subtitle",
          "text": "字幕テスト"
        }
      }
    },
    {
      "type": "split_clip",
      "clip_id": { "temp": "$c1" },
      "split_at_timeline_frame": 30
    }
  ]
}
```"#;

        let plan = dry_run_validate(&doc, raw_ai_output)?;
        assert!(
            doc.layers[0].clips.is_empty(),
            "ドライランは元 doc を変更しない"
        );
        assert!(plan.summary_lines.len() >= 3);
        assert!(plan.affected_targets.iter().any(|t| t == "$c1"));

        let mut dispatcher = CommandDispatcher::new();
        apply_plan(&mut doc, &mut dispatcher, &plan)?;
        assert_eq!(doc.layers[0].clips.len(), 2);

        dispatcher.undo(&mut doc)?;
        assert_eq!(doc.layers[0].clips.len(), 0);
        Ok(())
    }

    #[test]
    fn dry_run_rejects_malformed_json_and_nonexistent_ids() {
        let doc = ProjectDocument::new("RejectTest");
        assert!(matches!(
            dry_run_validate(&doc, "not json at all"),
            Err(AiPlanError::NoJsonCommand(_))
        ));
        assert!(matches!(
            dry_run_validate(&doc, "{\"type\":\"unknown_cmd\"}"),
            Err(AiPlanError::InvalidJsonCommand(_))
        ));
        let bad_id = r#"{"type":"remove_clip","clip_id":{"id":"missing-clip-99"}}"#;
        assert!(matches!(
            dry_run_validate(&doc, bad_id),
            Err(AiPlanError::ValidationFailed(_))
        ));
    }

    #[test]
    fn dry_run_accepts_empty_batch_no_op_and_extracts_reply_note()
    -> Result<(), Box<dyn std::error::Error>> {
        let doc = ProjectDocument::new("NoOpTest");

        let bare = dry_run_validate(&doc, r#"{"type":"batch","commands":[]}"#)?;
        assert!(bare.is_no_op());
        assert!(bare.reply_note.is_none());

        let with_prose = dry_run_validate(
            &doc,
            "pong! 応答テスト OK です。\n```json\n{\"type\":\"batch\",\"commands\":[]}\n```",
        )?;
        assert!(with_prose.is_no_op());
        assert_eq!(
            with_prose.reply_note.as_deref(),
            Some("pong! 応答テスト OK です。")
        );

        let with_json_message = dry_run_validate(
            &doc,
            r#"{"type":"batch","commands":[],"message":"現在の長さは 0 フレームです。"}"#,
        )?;
        assert!(with_json_message.is_no_op());
        assert_eq!(
            with_json_message.reply_note.as_deref(),
            Some("現在の長さは 0 フレームです。")
        );

        let mut doc_copy = doc;
        let mut dispatcher = CommandDispatcher::new();
        apply_plan(&mut doc_copy, &mut dispatcher, &bare)?;
        Ok(())
    }
}
