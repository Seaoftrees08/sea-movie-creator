//! AI チャット (`AppController` + `MockAiProvider` + `dry_run_validate`) の承認フローおよび一括 Undo 検証。

use sea_movie_creator::ai::provider::{AiError, AiRole};
use sea_movie_creator::ai::providers::mock::MockAiProvider;
use sea_movie_creator::core::animation::{AnimatedParameter, EvaluationContext};
use sea_movie_creator::core::ids::{ClipRef, LayerRef};
use sea_movie_creator::core::time::{FrameCount, FrameIndex};
use sea_movie_creator::media::visual_properties::SubtitleProperties;
use sea_movie_creator::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use sea_movie_creator::timeline::document::ClipKind;
use sea_movie_creator::ui::app_controller::AppController;

fn build_sample_ai_batch_json() -> Result<String, Box<dyn std::error::Error>> {
    let cmd = EditCommand::Batch {
        commands: vec![
            EditCommand::AddLayer {
                temp_id: Some("ai_sub_layer".to_owned()),
                id: None,
                name: "AI Subtitles".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: Some("c_sub".to_owned()),
                id: None,
                layer_id: LayerRef::Temp("ai_sub_layer".to_owned()),
                clip: Box::new(NewClipSpec {
                    name: "Title Banner".to_owned(),
                    timeline_start: FrameIndex::new(0),
                    duration: FrameCount::new(120)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("Draft")),
                }),
            },
            EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Temp("c_sub".to_owned()),
                },
                path: ParameterPath::SubtitleText,
                value: ParameterValue::Text {
                    value: "AI 自動字幕テスト".to_owned(),
                },
            },
            EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Temp("c_sub".to_owned()),
                },
                path: ParameterPath::X,
                value: ParameterValue::Animated {
                    param: AnimatedParameter::expression("sin(t * 2) * 120"),
                },
            },
            EditCommand::SplitClip {
                clip_id: ClipRef::Temp("c_sub".to_owned()),
                split_at_timeline_frame: FrameIndex::new(60),
                right_temp_id: Some("c_sub_r".to_owned()),
                right_id: None,
            },
        ],
    };
    let raw_json = serde_json::to_string_pretty(&cmd)?;
    Ok(format!("以下を提案します。\n```json\n{raw_json}\n```"))
}

#[test]
fn ai_chat_dry_run_preview_approval_and_atomic_one_click_undo()
-> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let _ = controller.dispatch_command(&EditCommand::Batch {
        commands: vec![
            EditCommand::AddLayer {
                temp_id: Some("seed_layer".to_owned()),
                id: None,
                name: "Base Layer".to_owned(),
                insert_index: None,
            },
            EditCommand::AddClip {
                temp_id: Some("seed".to_owned()),
                id: None,
                layer_id: LayerRef::Temp("seed_layer".to_owned()),
                clip: Box::new(NewClipSpec {
                    name: "Base Clip".to_owned(),
                    timeline_start: FrameIndex::new(0),
                    duration: FrameCount::new(180)?,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(SubtitleProperties::new("Base")),
                }),
            },
        ],
    })?;
    controller.selected_clip_id = controller.document.layers[0]
        .clips
        .first()
        .map(|clip| clip.id.clone());

    let initial_layers = controller.document.layers.len();
    controller.seek_to_frame(FrameIndex::new(30));

    let fenced_batch = build_sample_ai_batch_json()?;
    let mock = MockAiProvider::with_handler(move |req| {
        let schema_str = serde_json::to_string(&req.command_schema_json)
            .map_err(|err| AiError::ParseFailed(format!("schema serialize failed: {err}")))?;
        if !schema_str.contains("set_parameter") || !schema_str.contains("batch") {
            return Err(AiError::ParseFailed(
                "AiRequest is missing EditCommand schema definitions".to_owned(),
            ));
        }
        if req.context_summary["playhead_frame"].as_u64() != Some(30) {
            return Err(AiError::ParseFailed(
                "AiRequest playhead_frame does not match controller state".to_owned(),
            ));
        }
        Ok(fenced_batch.clone())
    });

    controller.submit_ai_prompt_with_provider(
        "字幕を追加して 60f で分割し、X 座標を sin 波で動かして",
        &mock,
    )?;

    // 承認前: ドライラン差分カードは生成されるがドキュメント本体は無変更
    assert_eq!(controller.document.layers.len(), initial_layers);
    assert!(!controller.ai_chat.can_one_click_undo);
    let proposal = controller
        .ai_chat
        .pending_proposal
        .as_ref()
        .ok_or("pending_proposal should exist after dry-run")?;
    assert!(proposal.plan.summary_lines.len() >= 4);
    assert!(proposal.plan.affected_targets.iter().any(|t| t == "c_sub"));
    assert_eq!(mock.recorded_requests().len(), 1);

    // 承認して適用: 単一コマンド / Batch がタイムラインへ反映される
    controller.approve_pending_ai_proposal()?;
    assert!(controller.ai_chat.pending_proposal.is_none());
    assert!(controller.ai_chat.can_one_click_undo);
    assert_eq!(controller.document.layers.len(), initial_layers + 1);

    let sub_layer = controller
        .document
        .layers
        .last()
        .ok_or("new subtitle layer should exist")?;
    assert_eq!(sub_layer.clips.len(), 2);
    let left = &sub_layer.clips[0];
    let right = &sub_layer.clips[1];
    assert_eq!(left.effect_offset_frames, 0);
    assert_eq!(right.effect_offset_frames, 60);

    let (ClipKind::Subtitle(sub_l), ClipKind::Subtitle(sub_r)) = (&left.kind, &right.kind) else {
        return Err("split clips must remain Subtitle clips".into());
    };
    assert_eq!(sub_l.text, "AI 自動字幕テスト");
    assert_eq!(sub_r.text, "AI 自動字幕テスト");

    let left_boundary_x = sub_l.visual.x.evaluate(EvaluationContext::new(
        60,
        left.effect_offset_frames,
        left.effect_duration_frames.unwrap_or(left.duration),
        controller.document.fps,
    ))?;
    let right_start_x = sub_r.visual.x.evaluate(EvaluationContext::new(
        0,
        right.effect_offset_frames,
        right.effect_duration_frames.unwrap_or(right.duration),
        controller.document.fps,
    ))?;
    assert!((left_boundary_x - right_start_x).abs() < 1e-9);

    // 1 クリック Undo で Batch 全体を 1 ステップで原状回復
    controller.undo_last_ai_plan()?;
    assert!(!controller.ai_chat.can_one_click_undo);
    assert_eq!(controller.document.layers.len(), initial_layers);
    Ok(())
}

#[test]
fn ai_chat_rejects_invalid_output_and_keeps_document_untouched() {
    let mut controller = AppController::new();
    let snapshot_before = controller.document.clone();
    let mock = MockAiProvider::with_responses([
        "申し訳ありませんが JSON は生成できません",
        "{\"type\":\"delete_clip\",\"clip_id\":{\"id\":\"nonexistent_clip_999\"}}",
    ]);

    assert!(
        controller
            .submit_ai_prompt_with_provider("不正文字列テスト", &mock)
            .is_err()
    );
    assert!(controller.ai_chat.pending_proposal.is_none());
    assert!(controller.ai_chat.last_error.is_some());
    assert_eq!(controller.document, snapshot_before);

    assert!(
        controller
            .submit_ai_prompt_with_provider("存在しないID削除テスト", &mock)
            .is_err()
    );
    assert!(controller.ai_chat.pending_proposal.is_none());
    assert_eq!(controller.document, snapshot_before);
}

#[test]
fn ai_chat_accepts_empty_batch_no_op_without_error() -> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AppController::new();
    let snapshot_before = controller.document.clone();
    let mock = MockAiProvider::with_responses([
        "pong!\n```json\n{\"type\":\"batch\",\"commands\":[]}\n```",
        "{\"type\":\"batch\",\"commands\":[]}",
    ]);

    controller.submit_ai_prompt_with_provider("ping", &mock)?;
    assert!(controller.ai_chat.last_error.is_none());
    assert!(controller.ai_chat.pending_proposal.is_none());
    assert_eq!(controller.document, snapshot_before);
    let last_turn = controller
        .ai_chat
        .turns
        .last()
        .ok_or("assistant reply turn should be recorded")?;
    assert_eq!(last_turn.role, AiRole::Assistant);
    assert_eq!(last_turn.text, "pong!");

    controller.submit_ai_prompt_with_provider("ping again", &mock)?;
    assert!(controller.ai_chat.last_error.is_none());
    assert!(controller.ai_chat.pending_proposal.is_none());
    assert_eq!(controller.document, snapshot_before);
    let bare_turn = controller
        .ai_chat
        .turns
        .last()
        .ok_or("assistant fallback reply turn should be recorded")?;
    assert_eq!(bare_turn.role, AiRole::Assistant);
    assert_eq!(bare_turn.text, "応答を受信しました（タイムライン変更なし）");
    Ok(())
}
