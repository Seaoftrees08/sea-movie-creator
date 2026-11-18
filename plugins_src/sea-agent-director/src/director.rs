//! ムービー構成エージェント（`sea-agent-director`）の自律ワークフロー実装。
//!
//! 1. フォルダ走査＋EXIF 時系列＋Vision バッチ解析による画像理解
//! 2. `ask_user` リッチ対話カードによる希望確認
//! 3. `MovieBlueprint` 生成・自己修復リトライ付き検証・`Proposal` 送出
//! 4. フォローアップ調整および動的拡張ツールを利用した汎用ツールループ

use std::collections::HashMap;

use sea_agent_protocol::{
    AgentAskChoice, AgentAskField, AgentAskFieldKind, AgentLlmMessage, AgentLlmToolCall,
    AgentToHostMessage, AgentUserAnswerPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol_io::{AgentTransport, RpcError};

/// `agent_memory` に永続化されるディレクターの作業記憶。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirectorMemory {
    /// 初回タスクの目的・プロンプト。
    #[serde(default)]
    pub goal: String,
    /// これまでに走査したフォルダパス一覧。
    #[serde(default)]
    pub inspected_folders: Vec<String>,
    /// 発見された BGM 等音声アセット情報一覧。
    #[serde(default)]
    pub discovered_audios: Vec<DiscoveredAudioMeta>,
    /// 同フォルダ内から読み取ったメモテキスト一覧。
    #[serde(default)]
    pub discovered_notes: Vec<Value>,
    /// 解析済み画像の一覧（パス・解像度・Vision 字幕案など）。
    #[serde(default)]
    pub analyzed_images: Vec<AnalyzedImageMeta>,
    /// `ask_user` で確定したユーザー希望値。
    #[serde(default)]
    pub user_preferences: Option<Value>,
    /// 最新コンパイル済みの `MovieBlueprint` JSON。
    #[serde(default)]
    pub last_blueprint: Option<Value>,
}

/// 発見された音声ファイルメタデータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredAudioMeta {
    /// 正規化済み絶対パス。
    pub path: String,
    /// ファイル名。
    pub file_name: String,
    /// 再生時間（秒）。
    #[serde(default)]
    pub duration_seconds: Option<f64>,
}

/// 走査＆Vision 解析済み 1 枚分の写真情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzedImageMeta {
    /// 正規化済み絶対パス。
    pub path: String,
    /// 相対パス。
    #[serde(default)]
    pub relative_path: String,
    /// ファイル名。
    #[serde(default)]
    pub file_name: String,
    /// 幅 (px)。
    #[serde(default)]
    pub width: u32,
    /// 高さ (px)。
    #[serde(default)]
    pub height: u32,
    /// EXIF 撮影日時（存在する場合）。
    #[serde(default)]
    pub exif_datetime: Option<String>,
    /// 情景説明。
    #[serde(default)]
    pub scene_description: String,
    /// 提案章見出し。
    #[serde(default)]
    pub suggested_chapter: String,
    /// 提案日本語字幕。
    #[serde(default)]
    pub suggested_subtitle: String,
    /// 雰囲気タグ。
    #[serde(default)]
    pub mood: String,
    /// 推奨 Ken Burns モーション。
    #[serde(default)]
    pub recommended_motion: String,
    /// Vision モデルによる解析が完了しているか。
    #[serde(default)]
    pub vision_analyzed: bool,
}

/// ホストとの常駐セッション中に保持するランタイムコンテキスト。
pub struct DirectorSession {
    /// ホストが申告した能力スナップショット。
    pub capabilities: Value,
    /// 永続記憶。
    pub memory: DirectorMemory,
    /// 最新のタイムライン要約。
    pub timeline_summary: String,
}

impl DirectorSession {
    /// `StartTask` / `ResumeSession` の初期状態から構築する。
    #[must_use]
    pub fn from_host_init(
        capabilities: Value,
        memory_val: Value,
        timeline_summary: String,
    ) -> Self {
        let memory = serde_json::from_value(memory_val).unwrap_or_default();
        Self {
            capabilities,
            memory,
            timeline_summary,
        }
    }

    /// 初回タスクを実行する（フォルダ添付がある場合はストーリー構成フロー、無ければ汎用ツールループ）。
    pub fn handle_start_task(
        &mut self,
        transport: &mut AgentTransport<'_>,
        goal: &str,
        attached_folders: &[String],
    ) {
        goal.clone_into(&mut self.memory.goal);
        if !attached_folders.is_empty() {
            if let Err(err) = run_folder_story_workflow(self, transport, goal, attached_folders) {
                handle_rpc_failure(transport, err);
            }
            return;
        }

        // フォルダ未添付時は汎用ツール呼び出しで応答
        if let Err(err) = run_dynamic_tool_loop(self, transport, goal, &[]) {
            handle_rpc_failure(transport, err);
        }
    }

    /// フォローアップ指示（構成微調整や追加フォルダ読み込み等）を処理する。
    pub fn handle_follow_up(
        &mut self,
        transport: &mut AgentTransport<'_>,
        prompt: &str,
        attached_folders: &[String],
        timeline_summary: String,
    ) {
        self.timeline_summary = timeline_summary;
        if !attached_folders.is_empty() {
            // 追加フォルダがあればまず走査＆Vision 解析を取り込む
            if let Err(err) = ingest_folders_and_vision(self, transport, prompt, attached_folders) {
                handle_rpc_failure(transport, err);
                return;
            }
        }
        match run_dynamic_tool_loop(self, transport, prompt, attached_folders) {
            Ok(()) => {}
            Err(err) => handle_rpc_failure(transport, err),
        }
    }
}

fn handle_rpc_failure(transport: &mut AgentTransport<'_>, err: RpcError) {
    match err {
        RpcError::Cancelled(_) => {}
        RpcError::Io(msg) => transport.send_error("io_error", msg, false),
        RpcError::Remote(msg) => transport.send_error(classify_error_code(&msg), msg, true),
    }
}

fn classify_error_code(msg: &str) -> &'static str {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("quota") {
        "rate_limited"
    } else if lower.contains("403") || lower.contains("401") || lower.contains("api key") {
        "auth_or_permission"
    } else if lower.contains("max_steps_per_turn") {
        "max_steps_exceeded"
    } else {
        "agent_failed"
    }
}

fn run_folder_story_workflow(
    session: &mut DirectorSession,
    transport: &mut AgentTransport<'_>,
    goal: &str,
    attached_folders: &[String],
) -> Result<(), RpcError> {
    ingest_folders_and_vision(session, transport, goal, attached_folders)?;

    if session.memory.analyzed_images.is_empty() {
        transport.send_error(
            "no_media",
            "添付フォルダ内に利用可能な画像ファイルが見つかりませんでした。画像入りのフォルダを指定してください。",
            true,
        );
        return Ok(());
    }

    // ステップ 3: リッチ対話質問カードでタイトル・尺・雰囲気を確認
    transport.status("構成と字幕の希望を確認中...", Some(3), Some(4));
    let suggested_duration = parse_target_duration_seconds(goal);
    let user_answer = ask_movie_preferences_card(session, transport, suggested_duration)?;
    session.memory.user_preferences = Some(json!({
        "choice_id": user_answer.choice_id,
        "field_values": user_answer.field_values,
        "free_text": user_answer.free_text,
        "skipped": user_answer.skipped,
        "requested_duration_seconds": extract_preferred_duration(&user_answer, suggested_duration),
    }));

    // ステップ 4: MovieBlueprint 生成＋最大 3 回自律修正＋Proposal 送出
    transport.status("タイムライン構成案を設計・検証中...", Some(4), Some(4));
    let (blueprint_val, compile_summary, command_val, model_summary) =
        synthesize_and_compile_blueprint(session, transport, goal)?;

    session.memory.last_blueprint = Some(blueprint_val.clone());
    let summary_markdown = format_blueprint_proposal_markdown(
        &blueprint_val,
        &compile_summary,
        model_summary.as_deref(),
        &session.memory,
    );
    let memory_patch = serde_json::to_value(&session.memory).unwrap_or(Value::Null);

    transport
        .send_message(&AgentToHostMessage::Proposal {
            summary_markdown,
            command: command_val,
            agent_memory_patch: Some(memory_patch),
            blueprint: Some(blueprint_val),
        })
        .map_err(RpcError::Io)?;
    Ok(())
}

fn ingest_folders_and_vision(
    session: &mut DirectorSession,
    transport: &mut AgentTransport<'_>,
    goal_hint: &str,
    attached_folders: &[String],
) -> Result<(), RpcError> {
    transport.status(
        "1/4 フォルダ内の写真・BGM・メモを調査中...",
        Some(1),
        Some(4),
    );

    let mut newly_discovered: Vec<AnalyzedImageMeta> = Vec::new();
    for folder in attached_folders {
        let inspected = transport.call_tool(
            "inspect_media_folder",
            json!({ "path": folder, "recursive": true }),
        )?;
        record_inspected_folder(session, folder, &inspected, &mut newly_discovered);
    }

    if newly_discovered.is_empty() && session.memory.analyzed_images.is_empty() {
        return Ok(());
    }

    // 100 枚超の大規模フォルダでは間引き確認ダイアログを提示
    let mut selected_new = newly_discovered;
    if selected_new.len() > 100 {
        selected_new = confirm_large_folder_selection(transport, selected_new)?;
    }

    for item in selected_new {
        if let Some(existing) = session
            .memory
            .analyzed_images
            .iter_mut()
            .find(|e| e.path == item.path)
        {
            existing.width = item.width;
            existing.height = item.height;
        } else {
            session.memory.analyzed_images.push(item);
        }
    }

    // 未 Vision 解析画像のみをバッチ解析してセッション記憶へ蓄積
    let pending_indices: Vec<usize> = session
        .memory
        .analyzed_images
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.vision_analyzed)
        .map(|(idx, _)| idx)
        .collect();

    let total_pending = pending_indices.len();
    let chunk_size = 6_usize;
    for (chunk_idx, idx_chunk) in pending_indices.chunks(chunk_size).enumerate() {
        let done_count = (chunk_idx * chunk_size).min(total_pending);
        transport.status(
            format!("2/4 写真を解析して情景・字幕案を作成中 ({done_count}/{total_pending})..."),
            Some(2),
            Some(4),
        );
        let paths: Vec<String> = idx_chunk
            .iter()
            .map(|&i| session.memory.analyzed_images[i].path.clone())
            .collect();
        let res = transport.call_tool(
            "inspect_images_vision",
            json!({
                "paths": paths,
                "theme_hint": goal_hint,
                "language": "ja",
            }),
        )?;
        apply_vision_results_to_memory(&mut session.memory.analyzed_images, idx_chunk, &res);
    }
    Ok(())
}

fn record_inspected_folder(
    session: &mut DirectorSession,
    folder: &str,
    inspected: &Value,
    out_images: &mut Vec<AnalyzedImageMeta>,
) {
    if !session.memory.inspected_folders.iter().any(|f| f == folder) {
        session.memory.inspected_folders.push(folder.to_owned());
    }
    if let Some(imgs) = inspected.get("images").and_then(Value::as_array) {
        for img in imgs {
            let Some(path) = img.get("path").and_then(Value::as_str) else {
                continue;
            };
            let file_name = img
                .get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("photo")
                .to_owned();
            let width = u32::try_from(img.get("width").and_then(Value::as_u64).unwrap_or(1920))
                .unwrap_or(1920);
            let height = u32::try_from(img.get("height").and_then(Value::as_u64).unwrap_or(1080))
                .unwrap_or(1080);
            out_images.push(AnalyzedImageMeta {
                path: path.to_owned(),
                relative_path: img
                    .get("relative_path")
                    .and_then(Value::as_str)
                    .unwrap_or(&file_name)
                    .to_owned(),
                file_name,
                width,
                height,
                exif_datetime: img
                    .get("exif_datetime")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                scene_description: String::new(),
                suggested_chapter: String::new(),
                suggested_subtitle: String::new(),
                mood: "warm".to_owned(),
                recommended_motion: "zoom_in".to_owned(),
                vision_analyzed: false,
            });
        }
    }
    if let Some(audios) = inspected.get("audios").and_then(Value::as_array) {
        for audio in audios {
            let Some(path) = audio.get("path").and_then(Value::as_str) else {
                continue;
            };
            if !session
                .memory
                .discovered_audios
                .iter()
                .any(|a| a.path == path)
            {
                session.memory.discovered_audios.push(DiscoveredAudioMeta {
                    path: path.to_owned(),
                    file_name: audio
                        .get("file_name")
                        .and_then(Value::as_str)
                        .unwrap_or("bgm")
                        .to_owned(),
                    duration_seconds: audio.get("duration_seconds").and_then(Value::as_f64),
                });
            }
        }
    }
    if let Some(notes) = inspected.get("notes").and_then(Value::as_array) {
        session
            .memory
            .discovered_notes
            .extend(notes.iter().cloned());
    }
}

fn confirm_large_folder_selection(
    transport: &mut AgentTransport<'_>,
    all_images: Vec<AnalyzedImageMeta>,
) -> Result<Vec<AnalyzedImageMeta>, RpcError> {
    let total = all_images.len();
    let ans = transport.ask_user(
        format!("写真が {total} 枚見つかりました"),
        "目標尺（例：3分）に見やすく収めるため、代表写真 36 枚を均等ピックアップして解析するか、全枚数を対象にするか選択してください。",
        Vec::new(),
        vec![
            AgentAskChoice {
                id: "sample_36".to_owned(),
                label: "代表 36 枚に厳選する（推奨）".to_owned(),
                description: Some("時系列に沿って均等に 36 枚を抽出し、1枚約5秒でゆったり見せます".to_owned()),
                is_primary: true,
            },
            AgentAskChoice {
                id: "use_all".to_owned(),
                label: format!("全 {total} 枚をすべて使う"),
                description: Some("解析ステップとスライド枚数が増えます".to_owned()),
                is_primary: false,
            },
        ],
        true,
    )?;

    if ans.choice_id.as_deref() == Some("use_all") {
        return Ok(all_images);
    }
    let target = 36_usize;
    let mut picked = Vec::with_capacity(target);
    for i in 0..target {
        let idx = (i * (total - 1)) / (target - 1);
        if let Some(item) = all_images.get(idx) {
            picked.push(item.clone());
        }
    }
    Ok(picked)
}

fn apply_vision_results_to_memory(
    images: &mut [AnalyzedImageMeta],
    target_indices: &[usize],
    tool_res: &Value,
) {
    let Some(arr) = tool_res.get("analyses").and_then(Value::as_array) else {
        for &idx in target_indices {
            if let Some(slot) = images.get_mut(idx) {
                slot.vision_analyzed = true;
            }
        }
        return;
    };
    for (pos, &idx) in target_indices.iter().enumerate() {
        let Some(slot) = images.get_mut(idx) else {
            continue;
        };
        let matched = arr
            .iter()
            .find(|v| v.get("path").and_then(Value::as_str) == Some(slot.path.as_str()))
            .or_else(|| arr.get(pos));
        if let Some(item) = matched {
            if let Some(s) = item.get("scene_description").and_then(Value::as_str) {
                s.clone_into(&mut slot.scene_description);
            }
            if let Some(s) = item.get("suggested_chapter").and_then(Value::as_str) {
                s.clone_into(&mut slot.suggested_chapter);
            }
            if let Some(s) = item.get("suggested_subtitle").and_then(Value::as_str) {
                s.clone_into(&mut slot.suggested_subtitle);
            }
            if let Some(s) = item.get("mood").and_then(Value::as_str) {
                s.clone_into(&mut slot.mood);
            }
            if let Some(s) = item.get("recommended_motion").and_then(Value::as_str) {
                s.clone_into(&mut slot.recommended_motion);
            }
        }
        slot.vision_analyzed = true;
    }
}

fn ask_movie_preferences_card(
    session: &DirectorSession,
    transport: &mut AgentTransport<'_>,
    suggested_duration_sec: f64,
) -> Result<AgentUserAnswerPayload, RpcError> {
    let img_count = session.memory.analyzed_images.len();
    let bgm_count = session.memory.discovered_audios.len();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let default_sec = suggested_duration_sec.round().max(10.0) as u64;

    let msg = format!(
        "写真 {img_count} 枚の解析と BGM 候補 {bgm_count} 曲の確認が完了しました。\nお名前や全体の雰囲気など希望があれば入力してください（「おまかせ」またはスキップも可能です）。"
    );
    transport.ask_user(
        "ムービー構成の希望を確認",
        msg,
        vec![
            AgentAskField {
                id: "title".to_owned(),
                label: "タイトル・お名前（例: 太郎 & 花子 Profile Movie）".to_owned(),
                placeholder: Some("太郎 & 花子 プロフィールムービー".to_owned()),
                default_value: None,
                kind: AgentAskFieldKind::Text,
            },
            AgentAskField {
                id: "duration_seconds".to_owned(),
                label: "目標再生時間（秒）".to_owned(),
                placeholder: Some("180".to_owned()),
                default_value: Some(default_sec.to_string()),
                kind: AgentAskFieldKind::Number,
            },
            AgentAskField {
                id: "tone_or_notes".to_owned(),
                label: "雰囲気・章構成・入れたいメッセージ".to_owned(),
                placeholder: Some("例: 温かく感動的に / 新郎→新婦→二人の思い出の順".to_owned()),
                default_value: None,
                kind: AgentAskFieldKind::Textarea,
            },
        ],
        vec![
            AgentAskChoice {
                id: "customize_apply".to_owned(),
                label: "この設定でタイムラインを作成".to_owned(),
                description: Some("入力内容をタイトル・字幕・構成へ反映します".to_owned()),
                is_primary: true,
            },
            AgentAskChoice {
                id: "auto".to_owned(),
                label: "おまかせで自動構成".to_owned(),
                description: Some("写真の解析結果から最適な構成を自動生成します".to_owned()),
                is_primary: false,
            },
        ],
        true,
    )
}

fn extract_preferred_duration(ans: &AgentUserAnswerPayload, fallback_sec: f64) -> f64 {
    if !ans.skipped
        && let Some(raw) = ans.field_values.get("duration_seconds")
        && let Ok(parsed) = raw.trim().parse::<f64>()
        && parsed.is_finite()
        && parsed >= 3.0
    {
        return parsed;
    }
    fallback_sec
}

fn synthesize_and_compile_blueprint(
    session: &DirectorSession,
    transport: &mut AgentTransport<'_>,
    goal: &str,
) -> Result<(Value, Value, Value, Option<String>), RpcError> {
    let pref = session
        .memory
        .user_preferences
        .clone()
        .unwrap_or(Value::Null);
    let target_duration = pref
        .get("requested_duration_seconds")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| parse_target_duration_seconds(goal));

    let blueprint_schema = session.capabilities.get("blueprint_schema").cloned();
    let media_summary = build_blueprint_synthesis_prompt(session, goal, target_duration, &pref);
    let system_prompt = blueprint_system_prompt();

    let mut messages = vec![AgentLlmMessage {
        role: "user".to_owned(),
        text: media_summary,
        image_paths: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }];

    // 最大 3 回まで検証エラーを LLM へフィードバックし自律修正する
    let mut last_error = String::new();
    for attempt in 0..4_u8 {
        let (raw_text, _) = transport.llm_chat(
            Some(system_prompt.to_owned()),
            messages.clone(),
            Vec::new(),
            blueprint_schema.clone(),
            true,
        )?;
        match try_compile_llm_blueprint_reply(
            session,
            transport,
            target_duration,
            attempt,
            raw_text,
            &mut messages,
        ) {
            Ok(res) => return Ok(res),
            Err(RpcError::Remote(err_msg)) => {
                last_error.clone_from(&err_msg);
            }
            Err(other) => return Err(other),
        }
    }

    Err(RpcError::Remote(format!(
        "MovieBlueprint compilation failed after self-correction retries: {last_error}"
    )))
}

const fn blueprint_system_prompt() -> &'static str {
    concat!(
        "You are an expert video director designing a structured `MovieBlueprint` JSON for sea-movie-creator.\n",
        "Output ONLY a valid JSON object matching this shape:\n",
        "{\n",
        "  \"project_name\": string,\n",
        "  \"target_duration_seconds\": number,\n",
        "  \"transition_overlap_seconds\": number (e.g. 0.7),\n",
        "  \"style\": \"wedding_warm\" | \"classic_mincho\" | \"modern_clean\",\n",
        "  \"title_card\": { \"title\": string, \"subtitle\": string|null, \"duration_seconds\": number } | null,\n",
        "  \"slides\": [{ \"image_path\": string, \"subtitle\": string|null, \"chapter\": string|null, \"motion\": \"zoom_in\"|\"zoom_out\"|\"pan_left\"|\"pan_right\"|\"static\", \"weight\": number, \"portrait_backdrop\": boolean }],\n",
        "  \"ending_card\": { \"title\": string, \"subtitle\": string|null, \"duration_seconds\": number } | null,\n",
        "  \"bgm\": { \"audio_path\": string, \"gain_db\": number, \"fade_in_seconds\": number, \"fade_out_seconds\": number, \"loop_if_shorter\": boolean } | null\n",
        "}\n",
        "Use exact `path` strings from the inspected image/audio list. Create warm, natural Japanese subtitles tailored to the photos and user notes."
    )
}

#[allow(clippy::type_complexity)]
fn try_compile_llm_blueprint_reply(
    session: &DirectorSession,
    transport: &mut AgentTransport<'_>,
    target_duration: f64,
    attempt: u8,
    raw_text: String,
    messages: &mut Vec<AgentLlmMessage>,
) -> Result<(Value, Value, Value, Option<String>), RpcError> {
    let candidate_raw = extract_first_json_object(&raw_text)
        .and_then(|slice| serde_json::from_str::<Value>(slice).ok());

    let Some(mut blueprint_val) = candidate_raw else {
        let preview: String = raw_text.chars().take(180).collect();
        let err_msg = format!(
            "Model did not return valid MovieBlueprint JSON (attempt {}): {preview}",
            attempt + 1
        );
        append_correction_feedback(
            messages,
            raw_text,
            "Your previous reply was not a valid JSON object. Return ONLY a raw JSON object matching the MovieBlueprint schema.".to_owned(),
        );
        return Err(RpcError::Remote(err_msg));
    };

    let model_summary = blueprint_val
        .get("director_notes")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(obj) = blueprint_val.as_object_mut() {
        obj.remove("director_notes");
    }
    normalize_blueprint_candidate(&mut blueprint_val, session, target_duration);

    match compile_and_validate_blueprint(transport, &blueprint_val) {
        Ok((compile_res, command_val)) => {
            Ok((blueprint_val, compile_res, command_val, model_summary))
        }
        Err(RpcError::Remote(err_msg)) => {
            append_correction_feedback(
                messages,
                serde_json::to_string(&blueprint_val).unwrap_or_default(),
                format!(
                    "Validation failed when compiling your MovieBlueprint: {err_msg}\nFix the JSON and output the corrected MovieBlueprint JSON only."
                ),
            );
            Err(RpcError::Remote(err_msg))
        }
        Err(other) => Err(other),
    }
}

fn append_correction_feedback(
    messages: &mut Vec<AgentLlmMessage>,
    assistant_text: String,
    feedback_text: String,
) {
    messages.push(AgentLlmMessage {
        role: "assistant".to_owned(),
        text: assistant_text,
        image_paths: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    });
    messages.push(AgentLlmMessage {
        role: "user".to_owned(),
        text: feedback_text,
        image_paths: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    });
}

fn compile_and_validate_blueprint(
    transport: &mut AgentTransport<'_>,
    blueprint_val: &Value,
) -> Result<(Value, Value), RpcError> {
    let compile_res = transport.call_tool(
        "compile_movie_blueprint",
        json!({ "blueprint": blueprint_val, "replace_draft": true }),
    )?;
    let validation_res =
        transport.call_tool("validate_draft_plan", json!({ "include_command": true }))?;
    let command_val = validation_res.get("command").cloned().ok_or_else(|| {
        RpcError::Remote("validate_draft_plan did not return `command`".to_owned())
    })?;
    Ok((compile_res, command_val))
}

fn normalize_blueprint_candidate(
    bp: &mut Value,
    session: &DirectorSession,
    target_duration_sec: f64,
) {
    let Some(obj) = bp.as_object_mut() else {
        return;
    };
    obj.entry("project_name".to_owned())
        .or_insert_with(|| Value::String("Profile Movie".to_owned()));
    obj.entry("target_duration_seconds".to_owned())
        .or_insert_with(|| json!(target_duration_sec));
    if obj
        .get("target_duration_seconds")
        .and_then(Value::as_f64)
        .is_none_or(|d| !d.is_finite() || d <= 0.0)
    {
        obj.insert(
            "target_duration_seconds".to_owned(),
            json!(target_duration_sec),
        );
    }
    obj.entry("transition_overlap_seconds".to_owned())
        .or_insert_with(|| json!(0.7));
    obj.entry("style".to_owned())
        .or_insert_with(|| json!("wedding_warm"));

    normalize_blueprint_slides(obj, session);
    normalize_blueprint_bgm(obj, session);
}

fn normalize_blueprint_slides(obj: &mut serde_json::Map<String, Value>, session: &DirectorSession) {
    // 相対パスやファイル名のみ返したモデルのパスを正規パスへ補正
    let mut path_lookup: HashMap<String, String> = HashMap::new();
    for img in &session.memory.analyzed_images {
        path_lookup.insert(img.path.clone(), img.path.clone());
        path_lookup.insert(img.relative_path.clone(), img.path.clone());
        path_lookup.insert(img.file_name.clone(), img.path.clone());
    }

    let motions = ["zoom_in", "pan_right", "zoom_out", "pan_left"];
    if let Some(slides) = obj.get_mut("slides").and_then(Value::as_array_mut) {
        for (idx, slide) in slides.iter_mut().enumerate() {
            if let Some(s_obj) = slide.as_object_mut() {
                if let Some(raw_path) = s_obj.get("image_path").and_then(Value::as_str)
                    && let Some(canon) = path_lookup.get(raw_path)
                {
                    s_obj.insert("image_path".to_owned(), Value::String(canon.clone()));
                }
                s_obj
                    .entry("motion".to_owned())
                    .or_insert_with(|| json!(motions[idx % motions.len()]));
                s_obj
                    .entry("portrait_backdrop".to_owned())
                    .or_insert_with(|| json!(true));
                s_obj
                    .entry("weight".to_owned())
                    .or_insert_with(|| json!(1.0));
            }
        }
    }

    // slides が空配列または欠落している場合は解析済み画像から安全補完
    let slides_empty = obj
        .get("slides")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if slides_empty && !session.memory.analyzed_images.is_empty() {
        let built: Vec<Value> = session
            .memory
            .analyzed_images
            .iter()
            .enumerate()
            .map(|(idx, img)| {
                let motion = if img.recommended_motion.is_empty() {
                    motions[idx % motions.len()]
                } else {
                    img.recommended_motion.as_str()
                };
                json!({
                    "image_path": img.path,
                    "subtitle": if img.suggested_subtitle.is_empty() {
                        Value::Null
                    } else {
                        Value::String(img.suggested_subtitle.clone())
                    },
                    "chapter": if img.suggested_chapter.is_empty() {
                        Value::Null
                    } else {
                        Value::String(img.suggested_chapter.clone())
                    },
                    "motion": motion,
                    "weight": 1.0,
                    "portrait_backdrop": true,
                })
            })
            .collect();
        obj.insert("slides".to_owned(), Value::Array(built));
    }
}

fn normalize_blueprint_bgm(obj: &mut serde_json::Map<String, Value>, session: &DirectorSession) {
    // BGM パス補正または既定 BGM 付与
    if let Some(bgm_obj) = obj.get_mut("bgm").and_then(Value::as_object_mut)
        && let Some(raw_audio) = bgm_obj.get("audio_path").and_then(Value::as_str)
        && let Some(found) = session
            .memory
            .discovered_audios
            .iter()
            .find(|a| a.path == raw_audio || a.file_name == raw_audio)
    {
        bgm_obj.insert("audio_path".to_owned(), Value::String(found.path.clone()));
    } else if (!obj.contains_key("bgm") || obj.get("bgm").is_some_and(Value::is_null))
        && let Some(first_bgm) = session.memory.discovered_audios.first()
    {
        obj.insert(
            "bgm".to_owned(),
            json!({
                "audio_path": first_bgm.path,
                "gain_db": -4.0,
                "fade_in_seconds": 1.5,
                "fade_out_seconds": 3.0,
                "loop_if_shorter": true,
            }),
        );
    }
}

fn build_blueprint_synthesis_prompt(
    session: &DirectorSession,
    goal: &str,
    target_duration: f64,
    user_prefs: &Value,
) -> String {
    let available_fonts: String = session
        .capabilities
        .get("available_fonts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .take(16)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    json!({
        "goal": goal,
        "target_duration_seconds": target_duration,
        "user_preferences": user_prefs,
        "images": session.memory.analyzed_images,
        "audios": session.memory.discovered_audios,
        "notes": session.memory.discovered_notes,
        "available_fonts_sample": available_fonts,
    })
    .to_string()
}

fn format_blueprint_proposal_markdown(
    blueprint: &Value,
    compile_res: &Value,
    model_notes: Option<&str>,
    memory: &DirectorMemory,
) -> String {
    let summary = compile_res.get("summary").unwrap_or(&Value::Null);
    let secs = summary
        .get("total_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let frames = summary
        .get("total_frames")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let slides = summary
        .get("slide_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let subtitles = summary
        .get("subtitle_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let bgm_segments = summary
        .get("bgm_segment_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let backdrops = summary
        .get("portrait_backdrop_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let title = blueprint
        .get("project_name")
        .and_then(Value::as_str)
        .unwrap_or("ムービー構成案");

    let mut md = format!(
        "### {title}\n\
         - **総尺**: {secs:.2} 秒（{frames} フレーム）\n\
         - **写真スライド**: {slides} 枚（A/B クロスフェード＋縦長背面額縁 {backdrops} 枚）\n\
         - **字幕・タイトル**: {subtitles} 件 / **BGM セグメント**: {bgm_segments} 件\n\
         - **解析キャッシュ済み画像**: {} 枚",
        memory.analyzed_images.len()
    );
    if let Some(notes) = model_notes.map(str::trim).filter(|s| !s.is_empty()) {
        md.push_str("\n\n");
        md.push_str(notes);
    }
    md
}

/// フォローアップ微調整・フォルダなし依頼・将来追加ツール呼び出しを統一処理する自律ツールループ。
fn run_dynamic_tool_loop(
    session: &mut DirectorSession,
    transport: &mut AgentTransport<'_>,
    prompt: &str,
    _attached_folders: &[String],
) -> Result<(), RpcError> {
    transport.status("指示内容を解析して調整プランを実行中...", None, None);

    let system_prompt = build_followup_system_prompt(session);
    let initial_context = json!({
        "user_instruction": prompt,
        "timeline_summary": session.timeline_summary,
        "last_blueprint": session.memory.last_blueprint,
        "analyzed_images_count": session.memory.analyzed_images.len(),
        "discovered_audios": session.memory.discovered_audios,
    })
    .to_string();

    let mut messages = vec![AgentLlmMessage {
        role: "user".to_owned(),
        text: initial_context,
        image_paths: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }];

    let mut final_text = String::new();
    let mut updated_blueprint: Option<Value> = None;

    for step in 0..12_u32 {
        let tools_snapshot = transport.available_tools.clone();
        let (text, tool_calls) = transport.llm_chat(
            Some(system_prompt.clone()),
            messages.clone(),
            tools_snapshot,
            None,
            false,
        )?;

        if tool_calls.is_empty() {
            final_text = text;
            break;
        }

        messages.push(AgentLlmMessage {
            role: "assistant".to_owned(),
            text,
            image_paths: Vec::new(),
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
        });

        for AgentLlmToolCall {
            id,
            name,
            arguments,
        } in tool_calls
        {
            transport.status(format!("ツール実行中: {name} ..."), Some(step + 1), None);
            if name == "compile_movie_blueprint"
                && let Some(bp) = arguments.get("blueprint")
            {
                let mut bp_clone = bp.clone();
                let fallback_sec = session
                    .memory
                    .last_blueprint
                    .as_ref()
                    .and_then(|v| v.get("target_duration_seconds"))
                    .and_then(Value::as_f64)
                    .unwrap_or(180.0);
                normalize_blueprint_candidate(&mut bp_clone, session, fallback_sec);
                updated_blueprint = Some(bp_clone);
            }

            let tool_reply_json = match transport.call_tool(&name, arguments) {
                Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| "{}".to_owned()),
                Err(RpcError::Remote(err)) => json!({ "error": err }).to_string(),
                Err(other) => return Err(other),
            };
            messages.push(AgentLlmMessage {
                role: "tool".to_owned(),
                text: tool_reply_json,
                image_paths: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: Some(id),
            });
        }
    }

    // DraftSandbox に変更が蓄積されているか検証し Proposal として送出
    finalize_dynamic_proposal(session, transport, &final_text, updated_blueprint)
}

fn finalize_dynamic_proposal(
    session: &mut DirectorSession,
    transport: &mut AgentTransport<'_>,
    final_text: &str,
    updated_blueprint: Option<Value>,
) -> Result<(), RpcError> {
    let val_res = transport.call_tool("validate_draft_plan", json!({ "include_command": true }))?;
    let is_no_op = val_res
        .get("is_no_op")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let command_val = val_res
        .get("command")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "batch", "commands": [] }));

    if let Some(ref bp) = updated_blueprint {
        session.memory.last_blueprint = Some(bp.clone());
    }
    let summary_markdown = if final_text.trim().is_empty() {
        let lines = val_res
            .get("summary_lines")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(|l| format!("- {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if is_no_op {
            "タイムラインへの変更はありませんでした。".to_owned()
        } else {
            format!("調整プランを作成しました:\n{lines}")
        }
    } else {
        final_text.to_owned()
    };

    let patch = serde_json::to_value(&session.memory).unwrap_or(Value::Null);
    transport
        .send_message(&AgentToHostMessage::Proposal {
            summary_markdown,
            command: command_val,
            agent_memory_patch: Some(patch),
            blueprint: updated_blueprint,
        })
        .map_err(RpcError::Io)?;
    Ok(())
}

fn build_followup_system_prompt(session: &DirectorSession) -> String {
    let ext_summary = session
        .capabilities
        .get("installed_extensions")
        .cloned()
        .unwrap_or(Value::Null);
    format!(
        "You are sea-agent-director, an autonomous editing agent for sea-movie-creator.\n\
         Use the provided host tools to inspect the timeline, call newly installed extension tools if relevant, \
         modify existing clips/subtitles via `apply_draft_edit_command`, or recompile the overall structure via `compile_movie_blueprint`.\n\
         Keep exact target duration constraints when adjusting individual slides/endings.\n\
         Installed extensions & capabilities snapshot: {ext_summary}\n\
         When you have finished updating the draft timeline, call `validate_draft_plan` or conclude with a concise Japanese summary."
    )
}

/// ユーザーの目標文から希望再生時間（秒）を抽出する（既定 180.0 秒）。
#[must_use]
pub fn parse_target_duration_seconds(goal: &str) -> f64 {
    let mut mins: Option<f64> = None;
    let mut secs: Option<f64> = None;

    let chars: Vec<char> = goal.chars().collect();
    let mut i = 0_usize;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            let rest: String = chars[i..].iter().take(4).collect();
            if let Ok(val) = num_str.parse::<f64>() {
                let rest_lower = rest.to_ascii_lowercase();
                if rest_lower.starts_with("分")
                    || rest_lower.starts_with("min")
                    || rest_lower.starts_with('m') && !rest_lower.starts_with("ms")
                {
                    mins = Some(val);
                } else if rest_lower.starts_with("秒")
                    || rest_lower.starts_with("sec")
                    || rest_lower.starts_with('s')
                {
                    secs = Some(val);
                }
            }
        } else {
            i += 1;
        }
    }

    match (mins, secs) {
        (Some(m), Some(s)) => (m * 60.0 + s).clamp(5.0, 7200.0),
        (Some(m), None) => (m * 60.0).clamp(5.0, 7200.0),
        (None, Some(s)) => s.clamp(5.0, 7200.0),
        (None, None) => 180.0,
    }
}

fn extract_first_json_object(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end >= start).then_some(&trimmed[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_duration_parser_extracts_minutes_and_seconds() {
        assert!(
            (parse_target_duration_seconds("結婚式プロフィールムービーを3分でお願いします")
                - 180.0)
                .abs()
                < 1e-6
        );
        assert!((parse_target_duration_seconds("2分30秒のダイジェスト") - 150.0).abs() < 1e-6);
        assert!((parse_target_duration_seconds("90秒のVlog") - 90.0).abs() < 1e-6);
        assert!((parse_target_duration_seconds("4.5min highlight") - 270.0).abs() < 1e-6);
        assert!((parse_target_duration_seconds("楽しくまとめて") - 180.0).abs() < 1e-6);
    }

    #[test]
    fn normalize_blueprint_resolves_filenames_and_attaches_bgm() {
        let session = DirectorSession {
            capabilities: Value::Null,
            memory: DirectorMemory {
                analyzed_images: vec![AnalyzedImageMeta {
                    path: "D:/wedding/01_baby.jpg".to_owned(),
                    relative_path: "01_baby.jpg".to_owned(),
                    file_name: "01_baby.jpg".to_owned(),
                    width: 1200,
                    height: 1600,
                    exif_datetime: None,
                    scene_description: String::new(),
                    suggested_chapter: String::new(),
                    suggested_subtitle: "幼少期の思い出".to_owned(),
                    mood: "warm".to_owned(),
                    recommended_motion: "zoom_in".to_owned(),
                    vision_analyzed: true,
                }],
                discovered_audios: vec![DiscoveredAudioMeta {
                    path: "D:/wedding/bgm.wav".to_owned(),
                    file_name: "bgm.wav".to_owned(),
                    duration_seconds: Some(120.0),
                }],
                ..DirectorMemory::default()
            },
            timeline_summary: String::new(),
        };

        let mut bp = json!({
            "project_name": "Test Wedding",
            "target_duration_seconds": 180.0,
            "slides": [
                { "image_path": "01_baby.jpg", "subtitle": "ここから始まった物語" }
            ]
        });
        normalize_blueprint_candidate(&mut bp, &session, 180.0);
        assert_eq!(bp["slides"][0]["image_path"], "D:/wedding/01_baby.jpg");
        assert_eq!(bp["bgm"]["audio_path"], "D:/wedding/bgm.wav");
    }
}
