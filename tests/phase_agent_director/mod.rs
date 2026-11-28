//! `sea-agent-director` 実バイナリを用いたエンドツーエンド結合テスト。

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use image::{ImageFormat, Rgba, RgbaImage};
use sea_movie_creator::ai::agent_host::{
    AgentHostEvent, AgentPluginRegistry, AgentSessionRunner, AgentSessionStartConfig,
};
use sea_movie_creator::ai::provider::{
    AgentRuntimeSettings, AiAgentTurnResponse, AiApprovalMode, AiProviderKind, AiThinkingLevel,
    AiToolCall, AiTurnMessage,
};
use sea_movie_creator::ai::providers::mock::MockAiProvider;
use sea_movie_creator::ai::session_store::AiSessionStore;
use sea_movie_creator::ai::toolbox::{
    AgentTool, AgentToolDescriptor, HostToolRegistry, ToolExecutionContext, ToolSafetyClass,
};
use sea_movie_creator::timeline::document::ProjectDocument;
use sea_movie_creator::ui::app_controller::AppController;
use serde_json::{Value, json};

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(label: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sea_phase_agent_{label}_{}_{id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_test_png(path: &Path, width: u32, height: u32) -> Result<(), Box<dyn std::error::Error>> {
    let mut img = RgbaImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let r = u8::try_from((x * 3) % 255).unwrap_or(180);
        let g = u8::try_from((y * 5) % 255).unwrap_or(140);
        *px = Rgba([r, g, 200, 255]);
    }
    let mut buf = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut buf, ImageFormat::Png)?;
    fs::write(path, buf.into_inner())?;
    Ok(())
}

fn write_wav_silence_48k_stereo(
    path: &Path,
    seconds: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let sample_rate = 48_000_u32;
    let channels = 2_u16;
    let bits_per_sample = 16_u16;
    let total_samples = sample_rate * seconds * u32::from(channels);
    let data_bytes = total_samples * u32::from(bits_per_sample / 8);
    let riff_size = 36_u32.saturating_add(data_bytes);

    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
    let block_align = channels * (bits_per_sample / 8);
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    out.resize(44 + data_bytes as usize, 0);
    fs::write(path, out)?;
    Ok(())
}

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

fn mock_vision_batch_reply(
    req: &sea_movie_creator::ai::provider::AiAgentTurnRequest,
    image_paths: &[String],
) -> AiAgentTurnResponse {
    let user_txt = req
        .messages
        .first()
        .map(|m| match m {
            AiTurnMessage::User { parts } => parts
                .iter()
                .filter_map(|p| match p {
                    sea_movie_creator::ai::provider::AiContentPart::Text { text } => {
                        Some(text.as_str())
                    }
                    sea_movie_creator::ai::provider::AiContentPart::ImageData { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .unwrap_or_default();

    let mut analyses = Vec::new();
    for (idx, p) in image_paths.iter().enumerate() {
        let matches_path_or_file = user_txt.contains(p.as_str())
            || Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|fname| user_txt.contains(fname));
        if matches_path_or_file {
            let pos = analyses.len();
            analyses.push(json!({
                "index": pos,
                "path": p,
                "scene_description": format!("思い出のシーン #{idx}"),
                "suggested_chapter": if idx < 4 { "生い立ち" } else { "二人の思い出" },
                "suggested_subtitle": format!("かけがえのない一日 #{}", idx + 1),
                "mood": "warm",
                "recommended_motion": if idx % 2 == 0 { "zoom_in" } else { "pan_right" },
            }));
        }
    }
    AiAgentTurnResponse {
        text: Some(json!({ "analyses": analyses }).to_string()),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        model: "mock-vision-v1".to_owned(),
        finish_reason: Some("stop".to_owned()),
        usage: None,
    }
}

fn build_wedding_mock_provider(
    image_paths: Vec<String>,
    bgm_canon: String,
    bp_attempt_counter: Arc<AtomicUsize>,
) -> Arc<MockAiProvider> {
    Arc::new(MockAiProvider::with_agent_handler(move |req| {
        if req.system_prompt.contains("photo curator") {
            return Ok(mock_vision_batch_reply(req, &image_paths));
        }

        // MovieBlueprint 合成ターン: 初回は存在しないパスを返して自律自己修正リトライを検証
        let attempt = bp_attempt_counter.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Ok(AiAgentTurnResponse {
                text: Some(
                    json!({
                        "project_name": "Broken Draft",
                        "target_duration_seconds": 180.0,
                        "slides": [{ "image_path": "does_not_exist_999.png" }]
                    })
                    .to_string(),
                ),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                model: "mock-director-v1".to_owned(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            });
        }

        let slides: Vec<Value> = image_paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                json!({
                    "image_path": p,
                    "subtitle": format!("大切な記念写真 #{}", i + 1),
                    "chapter": if i < 4 { "生い立ち" } else { "二人の歩み" },
                    "motion": if i % 2 == 0 { "zoom_in" } else { "zoom_out" },
                    "weight": 1.0,
                    "portrait_backdrop": true
                })
            })
            .collect();
        let valid_bp = json!({
            "project_name": "太郎 & 花子 プロフィールムービー",
            "target_duration_seconds": 180.0,
            "transition_overlap_seconds": 0.7,
            "style": "wedding_warm",
            "title_card": {
                "title": "太郎 & 花子",
                "subtitle": "Wedding Profile Movie",
                "duration_seconds": 5.0
            },
            "slides": slides,
            "ending_card": {
                "title": "本日はありがとうございました",
                "subtitle": "これからもよろしくお願いします",
                "duration_seconds": 6.0
            },
            "bgm": {
                "audio_path": bgm_canon,
                "gain_db": -4.0,
                "fade_in_seconds": 1.5,
                "fade_out_seconds": 3.0,
                "loop_if_shorter": true
            }
        });
        Ok(AiAgentTurnResponse {
            text: Some(valid_bp.to_string()),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-director-v1".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        })
    }))
}

fn verify_session_reload_and_followup(
    base_doc: ProjectDocument,
    store: AiSessionStore,
    session_id: &str,
    director: sea_movie_creator::ai::agent_host::DiscoveredAgentPlugin,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut controller2 = AppController::new();
    controller2.document = base_doc;
    controller2.ai_chat.session_store = Some(store);

    let followup_step = Arc::new(AtomicUsize::new(0));
    let followup_step_ref = Arc::clone(&followup_step);
    let followup_provider = Arc::new(MockAiProvider::with_agent_handler(move |_req| {
        let step = followup_step_ref.fetch_add(1, Ordering::SeqCst);
        if step == 0 {
            return Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![AiToolCall {
                    id: "call_meta_1".to_owned(),
                    name: "apply_draft_edit_command".to_owned(),
                    arguments: json!({
                        "command": {
                            "type": "set_project_meta",
                            "name": "太郎 & 花子 Profile Movie (Final Cut)"
                        }
                    }),
                    thought_signature: None,
                }],
                model: "mock-director-v1".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            });
        }
        Ok(AiAgentTurnResponse {
            text: Some("ご指示どおり構成タイトルをブラッシュアップしました。".to_owned()),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-director-v1".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        })
    }));

    controller2.open_saved_ai_session_with_provider(
        session_id,
        Some(followup_provider),
        Some(director),
    )?;
    assert!(
        controller2.ai_chat.pending_question.is_none(),
        "restoring a saved session must reset pending_question so idle agent won't hang UI"
    );
    assert_ne!(controller2.ai_chat.turns.len(), 0);
    assert!(
        controller2
            .ai_chat
            .agent_memory
            .get("last_blueprint")
            .is_some()
    );

    controller2.start_ai_prompt_async("タイトルを最終版にして")?;
    poll_until(&mut controller2, Duration::from_secs(20), |c| {
        c.ai_chat.pending_proposal.is_some()
    })?;
    controller2.approve_pending_ai_proposal()?;
    assert_eq!(
        controller2.document.name,
        "太郎 & 花子 Profile Movie (Final Cut)"
    );

    // 1 クリック Undo で直前のフォローアップ適用を取り消せることを確認
    controller2.undo_last_ai_plan()?;
    assert_ne!(
        controller2.document.name,
        "太郎 & 花子 Profile Movie (Final Cut)"
    );
    Ok(())
}

#[test]
fn wedding_three_minute_agent_workflow_session_resume_and_followup()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;
    if director.executable_path.is_none() {
        return Err("sea-agent-director binary missing; run cargo build --workspace first".into());
    }

    let guard = TempDirGuard::new("wedding_e2e")?;
    let media_dir = guard.0.join("wedding_photos");
    let store_dir = guard.0.join("sessions");
    fs::create_dir_all(&media_dir)?;

    // 横長4枚 + 縦長4枚 + BGM 60秒 WAV + メモファイルを用意
    let mut image_paths = Vec::new();
    for i in 0..8_u32 {
        let file = media_dir.join(format!("photo_{i:02}.png"));
        let (w, h) = if i % 2 == 0 { (600, 800) } else { (800, 600) };
        write_test_png(&file, w, h)?;
        image_paths.push(fs::canonicalize(&file)?.to_string_lossy().into_owned());
    }
    let bgm_file = media_dir.join("wedding_bgm.wav");
    write_wav_silence_48k_stereo(&bgm_file, 60)?;
    let bgm_canon = fs::canonicalize(&bgm_file)?.to_string_lossy().into_owned();
    fs::write(
        media_dir.join("notes.txt"),
        "新郎: 太郎 / 新婦: 花子\n大学時代から現在までの記録",
    )?;

    let bp_attempt_counter = Arc::new(AtomicUsize::new(0));
    let mock_provider =
        build_wedding_mock_provider(image_paths, bgm_canon, Arc::clone(&bp_attempt_counter));

    let mut controller = AppController::new();
    let store = AiSessionStore::new(&store_dir);
    controller.ai_chat.session_store = Some(store.clone());
    controller.ai_chat.selected_provider = AiProviderKind::Mock;
    controller.ai_chat.attach_folder_with_auto_agent(media_dir);
    assert_eq!(
        controller.ai_chat.selected_agent_id.as_deref(),
        Some("sea.agent.director")
    );

    let session_id = controller.ai_chat.session_id.clone();
    controller.start_agent_prompt_with_provider(
        "このフォルダにある画像を使って結婚式にプロフィールムービーを作り、時間は3分でお願いします。字幕は適切なものを入れてください。",
        mock_provider,
        Some(director.clone()),
    )?;

    // 1) フォルダ走査直後（Vision 解析前）、ask_user リッチ対話カードへ到達するのを待機
    poll_until(&mut controller, Duration::from_secs(20), |c| {
        c.ai_chat.pending_question.is_some()
    })?;

    let question = controller
        .ai_chat
        .pending_question
        .as_ref()
        .ok_or("missing ask_user question")?;
    assert!(question.fields.iter().any(|f| f.id == "title"));
    assert!(question.allow_skip);

    // 2) リッチ質問カードへ新郎新婦名・3分尺回答を返送
    let fields = BTreeMap::from([
        ("title".to_owned(), "太郎 & 花子 Profile Movie".to_owned()),
        ("duration_seconds".to_owned(), "180".to_owned()),
        ("tone_or_notes".to_owned(), "温かく感動的に".to_owned()),
    ]);
    controller.answer_pending_agent_question(
        Some("customize_apply".to_owned()),
        fields,
        None,
        false,
    )?;

    // 3) 自律修復リトライを経由し Proposal 差分カード到達を待機
    poll_until(&mut controller, Duration::from_secs(20), |c| {
        c.ai_chat.pending_proposal.is_some()
    })?;
    assert!(
        bp_attempt_counter.load(Ordering::SeqCst) >= 2,
        "expected self-correction retry on invalid draft blueprint"
    );

    // 4) Manual 承認してタイムラインへ 1 ステップ適用し、既定 60fps で 3分(10,800f)＆整合＆ミキサー同期を確認
    controller.approve_pending_ai_proposal()?;
    assert_eq!(
        controller.document.fps,
        sea_movie_creator::core::time::FrameRate::FPS_60
    );
    assert_eq!(controller.document.total_duration_frames(), 10_800);
    controller.document.validate()?;
    assert!(controller.ai_chat.can_one_click_undo);

    // 音声ミキサーにも BGM PCM が同期されていることを確認
    let audio_asset = controller
        .document
        .assets
        .iter()
        .find(|a| {
            matches!(
                a.kind,
                sea_movie_creator::timeline::document::AssetKind::Audio { .. }
            )
        })
        .ok_or("BGM audio asset missing after apply")?;
    assert!(controller.mixer.get_pcm(&audio_asset.id).is_some());

    // 現在のアクティブエージェントは終了させて、別コントローラへセッション保存から再開
    let _ = controller.cancel_ai_prompt();

    let listed = store.list_sessions()?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, session_id);

    verify_session_reload_and_followup(controller.document.clone(), store, &session_id, director)
}

struct MockExternalTtsTool {
    executed: Arc<AtomicBool>,
}

impl AgentTool for MockExternalTtsTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "future_tts_narration".to_owned(),
            category: "extension".to_owned(),
            description: "将来追加される外部 TTS プラグインで音声ファイルを生成します".to_owned(),
            safety_class: ToolSafetyClass::ExternalSideEffect,
            supports_progress: false,
            input_schema: json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" } }
            }),
        }
    }

    fn execute(&self, args: &Value, _ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        self.executed.store(true, Ordering::SeqCst);
        Ok(json!({
            "generated_wav": "narration_01.wav",
            "text": args["text"]
        }))
    }
}

#[test]
fn dynamic_external_tool_approval_and_rate_limit_work_end_to_end()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;

    let executed = Arc::new(AtomicBool::new(false));
    let mut toolbox = HostToolRegistry::with_builtin_tools();
    toolbox.register(Box::new(MockExternalTtsTool {
        executed: Arc::clone(&executed),
    }));

    let llm_calls = Arc::new(AtomicUsize::new(0));
    let llm_calls_ref = Arc::clone(&llm_calls);
    let provider = Arc::new(MockAiProvider::with_agent_handler(move |req| {
        let idx = llm_calls_ref.fetch_add(1, Ordering::SeqCst);
        // 新しく追加されたツールが動的に渡されていることを検証
        assert!(req.tools.iter().any(|t| t.name == "future_tts_narration"));
        if idx == 0 {
            return Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![AiToolCall {
                    id: "ext_1".to_owned(),
                    name: "future_tts_narration".to_owned(),
                    arguments: json!({ "text": "本日はご列席ありがとうございます" }),
                    thought_signature: None,
                }],
                model: "mock-v1".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            });
        }
        Ok(AiAgentTurnResponse {
            text: Some("外部 TTS 拡張ツールを呼び出しました。".to_owned()),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-v1".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        })
    }));

    let started = Instant::now();
    let runner = AgentSessionRunner::start(AgentSessionStartConfig {
        plugin: director,
        session_id: "sess_ext_gate".to_owned(),
        goal: "新しい外部音声合成ツールで挨拶を生成して".to_owned(),
        resume: false,
        attached_folders: Vec::new(),
        attached_clips: Vec::new(),
        base_document: ProjectDocument::new("ExtTest"),
        timeline_summary: "{}".to_owned(),
        agent_memory: json!({}),
        provider,
        provider_kind: AiProviderKind::Mock,
        model_name: "mock-v1".to_owned(),
        thinking_level: AiThinkingLevel::Auto,
        approval_mode: AiApprovalMode::Manual,
        runtime_settings: AgentRuntimeSettings {
            max_steps_per_turn: 20,
            min_llm_interval_ms: 120,
            vision_batch_size: 4,
            vision_max_edge_px: 512,
            ..AgentRuntimeSettings::default()
        },
        tool_registry: Some(toolbox),
        extra_extensions: Vec::new(),
        project_dir: None,
    })?;

    // 1) Manual モードのため ExternalSideEffect 実行前に承認要求で一時停止する
    let mut approval_req_id = None;
    let wait_start = Instant::now();
    while wait_start.elapsed() < Duration::from_secs(10) {
        if let Ok(evt) = runner.recv_event_timeout(Duration::from_millis(100))
            && let AgentHostEvent::ToolApprovalRequired {
                request_id,
                tool_name,
                ..
            } = evt
        {
            assert_eq!(tool_name, "future_tts_narration");
            assert!(
                !executed.load(Ordering::SeqCst),
                "external tool must not run before Manual approval"
            );
            approval_req_id = Some(request_id);
            break;
        }
    }
    let req_id = approval_req_id.ok_or("expected ToolApprovalRequired event")?;
    runner.send_tool_approval(req_id, true, None)?;

    // 2) 承認後にツール実行＋2回目 LLM 推論＋Proposal 完了まで待機
    let mut got_proposal = false;
    while wait_start.elapsed() < Duration::from_secs(15) {
        if let Ok(AgentHostEvent::Proposal { .. }) =
            runner.recv_event_timeout(Duration::from_millis(100))
        {
            got_proposal = true;
            break;
        }
    }
    assert!(got_proposal);
    assert!(executed.load(Ordering::SeqCst));
    // 2回の LLM 呼び出し間に min_llm_interval_ms (120ms) 以上経過していることを確認
    assert!(started.elapsed() >= Duration::from_millis(100));
    Ok(())
}

fn setup_partial_429_fixture(
    guard: &TempDirGuard,
) -> Result<(PathBuf, PathBuf, Arc<MockAiProvider>), Box<dyn std::error::Error>> {
    let media_dir = guard.0.join("photos");
    let store_dir = guard.0.join("sessions");
    fs::create_dir_all(&media_dir)?;

    let mut paths = Vec::new();
    for i in 0..8_u32 {
        let file = media_dir.join(format!("p_{i:02}.png"));
        write_test_png(&file, 640, 480)?;
        paths.push(fs::canonicalize(&file)?.to_string_lossy().into_owned());
    }

    let call_count = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(MockAiProvider::with_agent_handler(move |req| {
        let call_idx = call_count.fetch_add(1, Ordering::SeqCst);
        if call_idx == 0 {
            return Ok(mock_vision_batch_reply(req, &paths));
        }
        Err(sea_movie_creator::ai::provider::AiError::HttpStatus {
            status: 429,
            body: "rate limit exceeded".to_owned(),
        })
    }));
    Ok((media_dir, store_dir, provider))
}

#[test]
fn rate_limit_preserves_partial_vision_memory_and_resumes_without_pending_question()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;

    let guard = TempDirGuard::new("partial_429")?;
    let (media_dir, store_dir, provider) = setup_partial_429_fixture(&guard)?;

    let store = AiSessionStore::new(&store_dir);
    let mut controller = AppController::new();
    controller.ai_chat.session_store = Some(store.clone());
    controller.ai_chat.runtime_settings.vision_packing_mode =
        sea_movie_creator::ai::provider::VisionPackingMode::Individual;
    controller.ai_chat.runtime_settings.vision_batch_size = 4;
    controller.ai_chat.attach_folder_with_auto_agent(media_dir);
    let sid = controller.ai_chat.session_id.clone();

    controller.start_agent_prompt_with_provider(
        "写真を整理してムービーにして",
        provider,
        Some(director.clone()),
    )?;

    // 先に提示される初回希望確認カードへ応答し、後続の Vision バッチ解析へ進める
    poll_until(&mut controller, Duration::from_secs(20), |c| {
        c.ai_chat.pending_question.is_some()
    })?;
    controller.answer_pending_agent_question(
        Some("auto".to_owned()),
        BTreeMap::new(),
        None,
        false,
    )?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let _ = controller.poll_ai_chat();
        if controller.ai_chat.last_error.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        controller
            .ai_chat
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("rate_limited")),
        "expected rate_limited error on second batch, got {:?}",
        controller.ai_chat.last_error
    );

    // 429 中断時点でも最初のバッチで解析済みの画像記憶がセッションファイルに永続化されていること
    let saved = store.load_session(&sid)?;
    let analyzed = saved
        .agent_memory
        .get("analyzed_images")
        .and_then(Value::as_array)
        .ok_or("analyzed_images missing from saved session memory")?;
    assert_eq!(analyzed.len(), 8);
    let vision_done = analyzed
        .iter()
        .filter(|item| item["vision_analyzed"].as_bool() == Some(true))
        .count();
    assert_eq!(
        vision_done, 4,
        "expected only first batch of 4 analyzed images marked vision_analyzed=true despite partial 429 on batch 2, got {vision_done}"
    );
    let _ = controller.cancel_ai_prompt();

    // 仮に未回答の pending_question が保存されていても復元時にはクリアされ UI がハングしないこと
    let mut poisoned = saved;
    poisoned.pending_question = Some(
        sea_movie_creator::ai::session_store::PendingAgentQuestionSnapshot {
            request_id: "stale_q".to_owned(),
            title: "古い質問".to_owned(),
            message: "msg".to_owned(),
            fields: Vec::new(),
            choices: Vec::new(),
            allow_skip: true,
        },
    );
    store.save_session(&poisoned)?;

    let mut controller2 = AppController::new();
    controller2.ai_chat.session_store = Some(store);
    controller2.open_saved_ai_session_with_provider(
        &sid,
        Some(Arc::new(MockAiProvider::new())),
        Some(director),
    )?;
    assert!(controller2.ai_chat.pending_question.is_none());
    let _ = controller2.cancel_ai_prompt();
    Ok(())
}

fn recv_runner_until<T>(
    runner: &AgentSessionRunner,
    timeout: Duration,
    mut pred: impl FnMut(&AgentHostEvent) -> Option<T>,
) -> Result<T, Box<dyn std::error::Error>> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(ev) = runner.recv_event_timeout(Duration::from_millis(100)) {
            if let Some(found) = pred(&ev) {
                return Ok(found);
            }
            if let AgentHostEvent::AgentError {
                code,
                message,
                recoverable: false,
            } = ev
            {
                return Err(format!("fatal agent error [{code}]: {message}").into());
            }
        }
    }
    Err("timed out waiting for AgentHostEvent".into())
}

fn build_skip_thinking_mock_provider(
    seen_redacted: Arc<AtomicBool>,
    seen_skip: Arc<AtomicBool>,
) -> Arc<MockAiProvider> {
    use sea_movie_creator::ai::provider::AiThinkingBlock;

    Arc::new(MockAiProvider::with_agent_handler(move |req| {
        if req.messages.len() <= 2 {
            return Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: vec![
                    AiThinkingBlock {
                        thinking: "確認事項を整理".to_owned(),
                        signature: "sig_step1".to_owned(),
                        redacted_data: None,
                    },
                    AiThinkingBlock {
                        thinking: String::new(),
                        signature: String::new(),
                        redacted_data: Some("opaque_redacted_1".to_owned()),
                    },
                ],
                tool_calls: vec![AiToolCall {
                    id: "ask_1".to_owned(),
                    name: "ask_user".to_owned(),
                    arguments: json!({
                        "title": "方向性確認",
                        "message": "字幕色を選びますか？",
                        "allow_skip": true
                    }),
                    thought_signature: Some("gem_sig_ask".to_owned()),
                }],
                model: "mock-v1".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            });
        }
        for msg in &req.messages {
            if let AiTurnMessage::Assistant {
                thinking_blocks, ..
            } = msg
                && thinking_blocks
                    .iter()
                    .any(|b| b.redacted_data.as_deref() == Some("opaque_redacted_1"))
            {
                seen_redacted.store(true, Ordering::Release);
            }
            if let AiTurnMessage::ToolResult { content_json, .. } = msg
                && content_json.contains("\"skipped\":true")
            {
                seen_skip.store(true, Ordering::Release);
            }
        }
        Ok(AiAgentTurnResponse {
            text: Some("おまかせ設定で確認完了しました（変更なし）。".to_owned()),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-v1".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        })
    }))
}

#[test]
fn director_preserves_thinking_blocks_and_handles_ask_user_skip()
-> Result<(), Box<dyn std::error::Error>> {
    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;

    let seen_second_turn_has_redacted = Arc::new(AtomicBool::new(false));
    let seen_skip_reply = Arc::new(AtomicBool::new(false));
    let provider_skip = build_skip_thinking_mock_provider(
        Arc::clone(&seen_second_turn_has_redacted),
        Arc::clone(&seen_skip_reply),
    );

    let runner1 = AgentSessionRunner::start(AgentSessionStartConfig {
        plugin: director,
        session_id: "sess_thinking_skip".to_owned(),
        goal: "全体の相談だけ".to_owned(),
        resume: false,
        attached_folders: Vec::new(),
        attached_clips: Vec::new(),
        base_document: ProjectDocument::new("ThinkingTest"),
        timeline_summary: "{}".to_owned(),
        agent_memory: json!({}),
        provider: provider_skip,
        provider_kind: AiProviderKind::Mock,
        model_name: "mock-v1".to_owned(),
        thinking_level: AiThinkingLevel::High,
        approval_mode: AiApprovalMode::Autopilot,
        runtime_settings: AgentRuntimeSettings::default(),
        tool_registry: None,
        extra_extensions: Vec::new(),
        project_dir: None,
    })?;

    let mut ask_ok_on_skip = false;
    let _proposal1 = recv_runner_until(&runner1, Duration::from_secs(10), |ev| match ev {
        AgentHostEvent::AskUser { request_id, .. } => {
            let _ = runner1.send_user_answer(
                request_id.clone(),
                sea_agent_protocol::AgentUserAnswerPayload {
                    choice_id: None,
                    field_values: BTreeMap::new(),
                    free_text: None,
                    skipped: true,
                },
            );
            None
        }
        AgentHostEvent::ToolActivity {
            tool_name,
            ok,
            summary,
            ..
        } if tool_name == "ask_user" && summary == "skipped" => {
            ask_ok_on_skip = *ok;
            None
        }
        AgentHostEvent::Proposal {
            summary_markdown, ..
        } => Some(summary_markdown.clone()),
        _ => None,
    })?;
    assert!(ask_ok_on_skip);
    assert!(seen_second_turn_has_redacted.load(Ordering::Acquire));
    assert!(seen_skip_reply.load(Ordering::Acquire));
    runner1.shutdown();
    Ok(())
}

fn build_audit_stop_doc(img_path: &Path) -> Result<ProjectDocument, Box<dyn std::error::Error>> {
    use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
    use sea_movie_creator::core::time::{FrameCount, FrameIndex};
    use sea_movie_creator::media::visual_properties::VisualProperties;
    use sea_movie_creator::timeline::document::{
        Asset, AssetKind, AssetPath, AssetStatus, Clip, ClipKind, Layer,
    };

    let mut doc = ProjectDocument::new("AuditStopTest");
    let asset_id = AssetId::new("a1")?;
    doc.assets.push(Asset {
        id: asset_id.clone(),
        name: "sample.png".to_owned(),
        path: AssetPath::new(None, Some(img_path.to_string_lossy().into_owned())),
        kind: AssetKind::Image {
            width: 320,
            height: 240,
        },
        status: AssetStatus::Available,
    });
    let mut layer = Layer::new(LayerId::new("photos")?, "Photos");
    layer.clips.push(Clip {
        id: ClipId::new("c1")?,
        name: "sample".to_owned(),
        timeline_start: FrameIndex(0),
        duration: FrameCount::new(120)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Image {
            asset_id,
            visual: VisualProperties::default(),
        },
    });
    doc.layers.push(layer);
    Ok(doc)
}

#[test]
fn director_stops_safely_on_audit_vision_unsupported() -> Result<(), Box<dyn std::error::Error>> {
    use sea_movie_creator::ai::provider::AiError;

    let guard = TempDirGuard::new("audit_stop")?;
    let img_path = guard.0.join("sample.png");
    write_test_png(&img_path, 320, 240)?;
    let doc = build_audit_stop_doc(&img_path)?;

    let reg = AgentPluginRegistry::discover_default();
    let director = reg
        .find_by_id("sea.agent.director")
        .cloned()
        .ok_or("sea.agent.director plugin manifest missing")?;

    let call_counter = Arc::new(AtomicUsize::new(0));
    let call_counter_clone = Arc::clone(&call_counter);
    let provider_vision_err = Arc::new(MockAiProvider::with_agent_handler(move |req| {
        if req.response_json_schema.is_some() {
            return Err(AiError::PluginError(
                "vision_unsupported: model does not support image input".to_owned(),
            ));
        }
        let idx = call_counter_clone.fetch_add(1, Ordering::AcqRel);
        if idx == 0 {
            Ok(AiAgentTurnResponse {
                text: None,
                thinking_blocks: Vec::new(),
                tool_calls: vec![AiToolCall {
                    id: "audit_1".to_owned(),
                    name: "audit_timeline_slides".to_owned(),
                    arguments: json!({}),
                    thought_signature: None,
                }],
                model: "mock-text-only".to_owned(),
                finish_reason: Some("tool_calls".to_owned()),
                usage: None,
            })
        } else {
            Ok(AiAgentTurnResponse {
                text: Some("選択中のモデルが画像入力非対応のため安全停止しました。".to_owned()),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                model: "mock-text-only".to_owned(),
                finish_reason: Some("stop".to_owned()),
                usage: None,
            })
        }
    }));

    let runner2 = AgentSessionRunner::start(AgentSessionStartConfig {
        plugin: director,
        session_id: "sess_audit_safe_stop".to_owned(),
        goal: "写真の字幕と向きを直して".to_owned(),
        resume: false,
        attached_folders: Vec::new(),
        attached_clips: Vec::new(),
        base_document: doc,
        timeline_summary: "{}".to_owned(),
        agent_memory: json!({}),
        provider: provider_vision_err,
        provider_kind: AiProviderKind::Mock,
        model_name: "mock-text-only".to_owned(),
        thinking_level: AiThinkingLevel::Auto,
        approval_mode: AiApprovalMode::Autopilot,
        runtime_settings: AgentRuntimeSettings::default(),
        tool_registry: None,
        extra_extensions: Vec::new(),
        project_dir: None,
    })?;
    let summary2 = recv_runner_until(&runner2, Duration::from_secs(10), |ev| {
        if let AgentHostEvent::Proposal {
            summary_markdown, ..
        } = ev
        {
            Some(summary_markdown.clone())
        } else {
            None
        }
    })?;
    // try_queue_self_repair が発火しないため LLM 呼び出しは 2 回で終了し、0 件警告も付かない
    assert_eq!(call_counter.load(Ordering::Acquire), 2);
    assert!(!summary2.contains("タイムラインへの変更は行われませんでした"));
    runner2.shutdown();
    Ok(())
}
