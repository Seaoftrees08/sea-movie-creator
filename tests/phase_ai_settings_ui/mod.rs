use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sea_movie_creator::ai::provider::{
    AgentRuntimeSettings, AiApprovalMode, AiProviderKind, AiThinkingLevel, AiTokenUsage,
};
use sea_movie_creator::ai::providers::openai_compat::OpenRouterCreditsSnapshot;
use sea_movie_creator::ai::settings::{
    AI_SETTINGS_FILE_NAME, AiCustomModelConfig, AiUserSettings, ProbedModelEntry,
    UsageDisplayPreference,
};
use sea_movie_creator::ui::ai_chat_panel::{AiChatState, format_usage_footer_parts};
use sea_movie_creator::ui::app_controller::AppController;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

fn make_temp_settings_dir(prefix: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sea_{prefix}_{}_{}", std::process::id(), seq));
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[test]
fn startup_restores_last_selection_custom_models_and_runtime_settings()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_settings_dir("restore")?;
    let custom_model = AiCustomModelConfig {
        id: "deepseek/deepseek-r1".to_owned(),
        display_name: Some("DeepSeek R1".to_owned()),
        thinking_levels: vec![AiThinkingLevel::Low, AiThinkingLevel::High],
        default_thinking: Some(AiThinkingLevel::High),
        context_window_tokens: Some(128_000),
        input_price_per_1m_usd: Some(0.55),
        output_price_per_1m_usd: Some(2.19),
    };

    {
        let mut chat = AiChatState::default();
        chat.enable_settings_in_dir(&dir);
        chat.select_provider(AiProviderKind::OpenRouter);

        let mut next = chat.user_settings.clone();
        next.upsert_custom_model(&AiProviderKind::OpenRouter, custom_model)?;
        next.set_base_url(
            &AiProviderKind::OpenRouter,
            "https://custom.openrouter.example/v1",
        );
        next.budget_usd = Some(15.0);
        next.usage_display_mode = UsageDisplayPreference::Cost;
        chat.save_explicit_user_settings(next)?;

        chat.select_model("deepseek/deepseek-r1", AiThinkingLevel::High);
        chat.set_approval_mode(AiApprovalMode::Autopilot);
        chat.select_agent(Some("sea.agent.director".to_owned()));
        chat.apply_runtime_settings(AgentRuntimeSettings {
            max_steps_per_turn: 25,
            min_llm_interval_ms: 1500,
            vision_batch_size: 4,
            vision_max_edge_px: 512,
        });
    }

    let mut restarted = AiChatState::default();
    restarted.enable_settings_in_dir(&dir);

    assert_eq!(restarted.selected_provider, AiProviderKind::OpenRouter);
    assert_eq!(restarted.model_name, "deepseek/deepseek-r1");
    assert_eq!(restarted.thinking_level, AiThinkingLevel::High);
    assert_eq!(restarted.approval_mode, AiApprovalMode::Autopilot);
    assert_eq!(
        restarted.selected_agent_id.as_deref(),
        Some("sea.agent.director")
    );
    assert_eq!(
        restarted.custom_base_url,
        "https://custom.openrouter.example/v1"
    );
    assert_eq!(restarted.runtime_settings.max_steps_per_turn, 25);
    assert_eq!(restarted.runtime_settings.min_llm_interval_ms, 1500);
    assert_eq!(restarted.user_settings.budget_usd, Some(15.0));
    assert_eq!(
        restarted.user_settings.usage_display_mode,
        UsageDisplayPreference::Cost
    );
    assert!(
        restarted
            .resolved_models_for_current_provider()
            .iter()
            .any(|m| m.id == "deepseek/deepseek-r1" && m.is_custom)
    );

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn custom_model_pricing_and_fixed_cli_rejection_behave_as_designed()
-> Result<(), Box<dyn std::error::Error>> {
    let mut settings = AiUserSettings::default();
    settings.upsert_custom_model(
        &AiProviderKind::OpenRouter,
        AiCustomModelConfig {
            id: "vendor/custom-pro".to_owned(),
            display_name: Some("Custom Pro".to_owned()),
            thinking_levels: Vec::new(),
            default_thinking: None,
            context_window_tokens: Some(64_000),
            input_price_per_1m_usd: Some(2.0),
            output_price_per_1m_usd: Some(8.0),
        },
    )?;

    let mut chat = AiChatState::default();
    chat.apply_user_settings_snapshot(settings);
    chat.select_provider(AiProviderKind::OpenRouter);
    chat.select_model("vendor/custom-pro", AiThinkingLevel::Auto);
    chat.record_turn_usage(&AiTokenUsage {
        input_tokens: 500_000,
        output_tokens: 125_000,
        total_tokens: 625_000,
        cost_usd: None,
        is_estimated: false,
    });
    assert!(chat.session_usage.has_known_cost);
    // 0.5M * $2.0 + 0.125M * $8.0 = $2.00
    assert!((chat.session_usage.cost_usd - 2.0).abs() < 1e-6);

    // 固定モデル CLI プロバイダ (Antigravity) への自由追加は拒否される
    let agy = AiProviderKind::Plugin {
        plugin_id: "sea.ai.antigravity".to_owned(),
    };
    let reject = chat.user_settings.upsert_custom_model(
        &agy,
        AiCustomModelConfig {
            id: "unauthorized-model".to_owned(),
            display_name: None,
            thinking_levels: Vec::new(),
            default_thinking: None,
            context_window_tokens: None,
            input_price_per_1m_usd: None,
            output_price_per_1m_usd: None,
        },
    );
    assert!(reject.is_err());

    // CLI プローブ結果があればその一覧が優先表示される
    chat.discovered_plugin_models.insert(
        "sea.ai.antigravity".to_owned(),
        vec![ProbedModelEntry {
            id: "gemini-4-hydrogen".to_owned(),
            display_name: "Gemini 4 Hydrogen".to_owned(),
            badge: Some("Live CLI".to_owned()),
            thinking_levels: vec![AiThinkingLevel::Low, AiThinkingLevel::High],
            default_thinking: AiThinkingLevel::Medium,
            context_window_tokens: Some(1_000_000),
            available: true,
            note: Some("検出済み".to_owned()),
        }],
    );
    let agy_models = chat.resolved_models_for_provider(&agy);
    assert_eq!(agy_models.len(), 1);
    assert_eq!(agy_models[0].badge.as_deref(), Some("Live CLI"));
    Ok(())
}

#[test]
fn project_file_never_contains_api_keys_from_settings_json()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_settings_dir("secret_iso")?;
    let dummy_key = "demo-openrouter-token-12345";

    let mut controller = AppController::new();
    controller.ai_chat.enable_settings_in_dir(&dir);
    let mut next = controller.ai_chat.user_settings.clone();
    next.set_api_key(&AiProviderKind::OpenRouter, dummy_key);
    controller.ai_chat.save_explicit_user_settings(next)?;
    controller
        .ai_chat
        .select_provider(AiProviderKind::OpenRouter);
    assert!(controller.ai_chat.has_resolved_credentials());

    let project_file = dir.join("test_project.smc.json");
    controller.save_project_file(&project_file)?;
    let saved_project = fs::read_to_string(&project_file)?;
    assert!(!saved_project.contains(dummy_key));
    assert!(!saved_project.contains("api_keys"));

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn corrupted_settings_json_falls_back_and_is_not_clobbered_automatically()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = make_temp_settings_dir("corrupt")?;
    let settings_file = dir.join(AI_SETTINGS_FILE_NAME);
    let broken_text = "{ invalid json -- manual edit in progress ";
    fs::write(&settings_file, broken_text)?;

    let mut chat = AiChatState::default();
    chat.enable_settings_in_dir(&dir);
    assert!(chat.settings_load_warning.is_some());

    // 通常操作では破損ファイルを上書きしない
    chat.select_provider(AiProviderKind::Gemini);
    assert_eq!(fs::read_to_string(&settings_file)?, broken_text);

    // 明示保存時のみ上書き修復・警告解除される
    chat.save_and_apply_user_settings(AiUserSettings::default())?;
    assert!(chat.settings_load_warning.is_none());
    assert!(AiUserSettings::load_from_path(&settings_file).is_ok());

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn session_usage_persists_and_footer_adapts_per_provider() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = make_temp_settings_dir("usage_footer")?;
    let mut controller = AppController::new();
    controller.ai_chat.enable_settings_in_dir(&dir);
    controller
        .ai_chat
        .select_provider(AiProviderKind::OpenRouter);
    controller.ai_chat.openrouter_credits = Some(OpenRouterCreditsSnapshot {
        total_credits_usd: 10.0,
        total_usage_usd: 5.18,
        remaining_usd: 4.82,
        remaining_percent: Some(48.2),
    });
    controller.ai_chat.record_turn_usage(&AiTokenUsage {
        input_tokens: 1_000,
        output_tokens: 500,
        total_tokens: 1_500,
        cost_usd: Some(0.025),
        is_estimated: false,
    });
    controller
        .ai_chat
        .turns
        .push(sea_movie_creator::ui::ai_chat_panel::AiChatTurn::new(
            1,
            sea_movie_creator::ai::provider::AiRole::User,
            "テスト".to_owned(),
            None,
        ));
    controller.ai_chat.persist_session();
    let sid = controller.ai_chat.session_id.clone();

    let (or_left, or_right) = format_usage_footer_parts(&controller.ai_chat);
    assert!(or_left.contains("1.5k tok") && or_left.contains("$0.025"));
    assert!(or_right.contains("$4.82") && or_right.contains("48%"));

    // セッション再読込後に利用量累計が復元される
    controller.ai_chat.new_session();
    assert_eq!(controller.ai_chat.session_usage.total_tokens, 0);
    controller.open_saved_ai_session(&sid)?;
    assert_eq!(controller.ai_chat.session_usage.total_tokens, 1_500);

    // Ollama / Local 表示
    controller
        .ai_chat
        .select_provider(AiProviderKind::OllamaCompat);
    let (local_left, local_right) = format_usage_footer_parts(&controller.ai_chat);
    assert!(local_left.contains("Local"));
    assert!(local_right.contains("Local"));

    // Antigravity CLI 表示
    controller.ai_chat.select_provider(AiProviderKind::Plugin {
        plugin_id: "sea.ai.antigravity".to_owned(),
    });
    let (_cli_left, cli_right) = format_usage_footer_parts(&controller.ai_chat);
    assert!(cli_right.contains("CLI管理"));

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[test]
fn session_restore_preserves_custom_base_url_and_footer_shows_credit_error()
-> Result<(), Box<dyn std::error::Error>> {
    use sea_movie_creator::ai::session_store::{AiSessionRecord, AiSessionStore, AiSessionUsage};

    let dir = make_temp_settings_dir("restore_base_url")?;
    let mut controller = AppController::new();
    controller.ai_chat.enable_settings_in_dir(&dir);

    let mut next = controller.ai_chat.user_settings.clone();
    next.set_base_url(&AiProviderKind::OllamaCompat, "http://127.0.0.1:22434/v1");
    controller.ai_chat.save_explicit_user_settings(next)?;

    let store =
        AiSessionStore::with_paths(dir.join("ai_sessions"), dir.join("agent_settings.json"));
    let rec = AiSessionRecord {
        schema_version: 1,
        session_id: "sess_custom_ollama".to_owned(),
        title: "Ollama Session".to_owned(),
        created_at_unix_secs: 1,
        updated_at_unix_secs: 2,
        provider: Some(AiProviderKind::OllamaCompat),
        agent_plugin_id: None,
        model_name: "llama3.2".to_owned(),
        thinking_level: AiThinkingLevel::Auto,
        approval_mode: AiApprovalMode::Manual,
        attached_folders: Vec::new(),
        turns: Vec::new(),
        pending_question: None,
        agent_memory: serde_json::Value::Null,
        session_usage: AiSessionUsage::default(),
    };
    store.save_session(&rec)?;

    controller.open_saved_ai_session("sess_custom_ollama")?;
    assert_eq!(
        controller.ai_chat.selected_provider,
        AiProviderKind::OllamaCompat
    );
    assert_eq!(
        controller.ai_chat.custom_base_url,
        "http://127.0.0.1:22434/v1"
    );

    // OpenRouter クレジット取得エラー時でもフッタが明確にエラー状態を表示する
    controller
        .ai_chat
        .select_provider(AiProviderKind::OpenRouter);
    controller.ai_chat.openrouter_credits = None;
    controller.ai_chat.openrouter_credits_error = Some("401 unauthorized".to_owned());
    let (_left, err_right) = format_usage_footer_parts(&controller.ai_chat);
    assert!(err_right.contains("残高取得失敗"));

    let _ = fs::remove_dir_all(&dir);
    Ok(())
}
