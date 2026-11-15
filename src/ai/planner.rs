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
    /// UI の差分カードへ表示する人間向け要約行一覧。
    pub summary_lines: Vec<String>,
    /// 変更対象となる識別子一覧。
    pub affected_targets: Vec<String>,
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
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AiPlanError::NoJsonCommand(
            "empty model response".to_owned(),
        ));
    }

    let mut candidates = Vec::new();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        candidates.push(trimmed.to_owned());
    }
    collect_fenced_json_blocks(trimmed, &mut candidates);
    if candidates.is_empty()
        && let Some(obj) = find_first_balanced_json_object(trimmed)
    {
        candidates.push(obj);
    }

    if candidates.is_empty() {
        let preview: String = trimmed.chars().take(120).collect();
        return Err(AiPlanError::NoJsonCommand(preview));
    }

    let mut last_err = String::new();
    for cand in candidates {
        match parse_command_candidate(&cand) {
            Ok((normalized, cmd)) => return Ok((normalized, cmd)),
            Err(err) => last_err = err,
        }
    }

    Err(AiPlanError::InvalidJsonCommand(last_err))
}

/// モデル出力をパースしたうえで複製ドキュメント上でドライランし、安全な [`AiEditPlan`] を生成する。
///
/// 呼び出し元の `doc` および履歴には一切の副作用を残さない。
///
/// # Errors
///
/// JSON 不正またはドキュメント不変条件・存在チェック違反時に [`AiPlanError`] を返す。
pub fn dry_run_validate(
    doc: &ProjectDocument,
    raw_response: &str,
) -> Result<AiEditPlan, AiPlanError> {
    let (raw_json, command) = extract_edit_command_from_text(raw_response)?;
    let mut trial_doc = doc.clone();
    let mut trial_dispatcher = CommandDispatcher::new();
    trial_dispatcher.dispatch(&mut trial_doc, &command)?;

    let mut summary_lines = Vec::new();
    let mut affected_targets = Vec::new();
    summarize_command(&command, &mut summary_lines, &mut affected_targets);

    Ok(AiEditPlan {
        raw_json,
        command,
        summary_lines,
        affected_targets,
    })
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
    dispatcher.dispatch(doc, &plan.command)?;
    Ok(())
}

fn parse_command_candidate(candidate: &str) -> Result<(String, EditCommand), String> {
    let val: Value =
        serde_json::from_str(candidate).map_err(|err| format!("JSON syntax error: {err}"))?;

    if let Ok(cmd) = serde_json::from_value::<EditCommand>(val.clone()) {
        let pretty = serde_json::to_string_pretty(&cmd).unwrap_or_else(|_| candidate.to_owned());
        return Ok((pretty, cmd));
    }

    // `"commands": [...]` をルートに持つオブジェクトもバッチに昇格許容する
    if let Some(commands_val) = val.get("commands")
        && let Ok(commands) = serde_json::from_value::<Vec<EditCommand>>(commands_val.clone())
    {
        let batch = EditCommand::Batch { commands };
        let pretty = serde_json::to_string_pretty(&batch).unwrap_or_else(|_| candidate.to_owned());
        return Ok((pretty, batch));
    }

    Err(format!(
        "does not match EditCommand schema: {}",
        serde_json::from_value::<EditCommand>(val)
            .err()
            .map_or_else(|| "unknown schema error".to_owned(), |e| e.to_string())
    ))
}

fn collect_fenced_json_blocks(raw: &str, out: &mut Vec<String>) {
    let mut rest = raw;
    while let Some(start) = rest.find("```") {
        let after_ticks = &rest[start + 3..];
        let Some(newline) = after_ticks.find('\n') else {
            break;
        };
        let lang = after_ticks[..newline].trim().to_ascii_lowercase();
        let body_start = &after_ticks[newline + 1..];
        let Some(end) = body_start.find("```") else {
            break;
        };
        let body = body_start[..end].trim();
        if (lang.is_empty() || lang == "json") && !body.is_empty() {
            out.push(body.to_owned());
        }
        rest = &body_start[end + 3..];
    }
}

fn find_first_balanced_json_object(raw: &str) -> Option<String> {
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
                    return Some(raw[start_byte..end].to_owned());
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
            summary_lines.push(format!("  + レイヤー追加 `{name}`"));
        }
        EditCommand::RemoveLayer { layer_id } => {
            targets.push(layer_id.as_str().to_owned());
            summary_lines.push(format!("  - レイヤー削除 {}", layer_id.as_str()));
        }
        EditCommand::SetLayerFlags { layer_id, .. } => {
            let lbl = format_layer_ref(layer_id);
            targets.push(lbl.clone());
            summary_lines.push(format!("  * レイヤー設定変更 {lbl}"));
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
}
