//! 双方向 NDJSON エージェントプラグイン（`plugins/agent/*`）の検出、マニフェスト検証、
//! 常駐セッション実行ホスト（レート制限・承認ゲート・Draft Sandbox・キャンセル制御）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sea_agent_protocol::{
    AgentAskChoice, AgentAskField, AgentLlmMessage, AgentLlmToolCall, AgentToHostMessage,
    AgentToolSpec, AgentUserAnswerPayload, HostToAgentMessage, PROTOCOL_VERSION, read_ndjson_line,
    write_ndjson_line,
};
use sea_export_protocol::apply_background_process_flags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::ai::capabilities::{InstalledExtensionSummary, collect_host_capabilities};
use crate::ai::media_inspector::{AllowedFolderScope, encode_thumbnail_jpeg_base64};
use crate::ai::planner::{AiEditPlan, dry_run_validate};
use crate::ai::provider::{
    AgentRuntimeSettings, AiAgentTurnRequest, AiApprovalMode, AiContentPart, AiError, AiProvider,
    AiProviderKind, AiThinkingLevel, AiTokenUsage, AiToolCall, AiToolDefinition, AiTurnMessage,
};

const MAX_CHAT_IMAGE_PATHS: usize = 16;
const MAX_STDERR_LINE_BYTES: usize = 4096;
use crate::ai::toolbox::{
    AgentToolDescriptor, DraftSandbox, HostToolRegistry, ToolExecutionContext, ToolSafetyClass,
};
use crate::core::PluginAiHints;
use crate::export::registry::resolve_plugin_executable;
use crate::timeline::command::EditCommand;
use crate::timeline::document::ProjectDocument;

/// エージェントプラグインが採用する双方向通信プロトコル種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPluginProtocol {
    /// `sea-agent-protocol` による `stdin`/`stdout` 双方向 NDJSON (`sea_agent_v1`)。
    SeaAgentV1,
}

/// `plugins/agent/<id>/agent-plugin.json` のマニフェスト定義。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPluginManifest {
    /// マニフェスト形式バージョン（現在は `1`）。
    pub schema_version: u32,
    /// グローバルに一意なプラグイン識別子（例: `"sea.agent.director"`）。
    pub id: String,
    /// UI 表示名。
    pub name: String,
    /// セマンティックバージョン。
    pub version: String,
    /// プラグインフォルダ基準の実行バイナリ名（パストラバーサル不可）。
    pub entrypoint: String,
    /// 起動時固定 CLI 引数一覧。
    #[serde(default)]
    pub args: Vec<String>,
    /// 双方向通信プロトコル種別。
    pub protocol: AgentPluginProtocol,
    /// AI エージェント・ルーター向けの能力自己申告メタデータ（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hints: Option<PluginAiHints>,
}

impl AgentPluginManifest {
    /// マニフェスト JSON ファイルを読み込み、スキーマと安全性を検証する。
    ///
    /// # Errors
    ///
    /// ファイル読取失敗、JSON パースエラー、または [`Self::validate`] 違反時に [`AgentPluginHostError`] を返す。
    pub fn from_file(path: &Path) -> Result<Self, AgentPluginHostError> {
        let raw = fs::read_to_string(path).map_err(|err| AgentPluginHostError::Io {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        let manifest: Self =
            serde_json::from_str(&raw).map_err(|err| AgentPluginHostError::InvalidManifest {
                path: path.display().to_string(),
                reason: err.to_string(),
            })?;
        manifest.validate(path)?;
        Ok(manifest)
    }

    /// 必須フィールドおよび `entrypoint` のパストラバーサル禁止を検証する。
    ///
    /// # Errors
    ///
    /// スキーマ不正や危険な `entrypoint` を検出した場合に [`AgentPluginHostError::InvalidManifest`] を返す。
    pub fn validate(&self, manifest_path: &Path) -> Result<(), AgentPluginHostError> {
        let invalid = |reason: &str| AgentPluginHostError::InvalidManifest {
            path: manifest_path.display().to_string(),
            reason: reason.to_owned(),
        };
        if self.schema_version != 1 {
            return Err(invalid("unsupported agent plugin schema_version"));
        }
        if self.id.trim().is_empty() {
            return Err(invalid("plugin id must not be empty"));
        }
        if self.name.trim().is_empty() {
            return Err(invalid("plugin name must not be empty"));
        }
        if self.version.trim().is_empty() {
            return Err(invalid("plugin version must not be empty"));
        }
        if self.entrypoint.is_empty()
            || self.entrypoint != self.entrypoint.trim()
            || self.entrypoint == "."
            || self.entrypoint.contains('/')
            || self.entrypoint.contains('\\')
            || self.entrypoint.contains(':')
            || self.entrypoint.contains("..")
            || self.entrypoint.contains('\0')
        {
            return Err(invalid("entrypoint must be a plain binary name"));
        }
        Ok(())
    }
}

/// 検出済みエージェントプラグイン情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredAgentPlugin {
    /// パース＆検証済みマニフェスト。
    pub manifest: AgentPluginManifest,
    /// `agent-plugin.json` のパス。
    pub manifest_path: PathBuf,
    /// プラグインディレクトリ。
    pub plugin_dir: PathBuf,
    /// 解決された実行バイナリパス（未ビルド時は `None`）。
    pub executable_path: Option<PathBuf>,
}

/// エージェントプラグイン探索・ホスト起動時のエラー型。
#[derive(Debug, Error)]
pub enum AgentPluginHostError {
    /// ファイル I/O 失敗。
    #[error("agent plugin I/O error at {path}: {reason}")]
    Io {
        /// 失敗対象パス。
        path: String,
        /// 原因詳細。
        reason: String,
    },
    /// マニフェスト形式または検証エラー。
    #[error("invalid agent plugin manifest at {path}: {reason}")]
    InvalidManifest {
        /// 対象マニフェストパス。
        path: String,
        /// 原因詳細。
        reason: String,
    },
    /// プラグイン実行バイナリが見つからない。
    #[error("agent plugin executable `{entrypoint}` not found for `{plugin_id}`")]
    ExecutableMissing {
        /// プラグイン ID。
        plugin_id: String,
        /// マニフェスト指定エントリポイント。
        entrypoint: String,
    },
    /// プロセス起動・IPC チャネル通信失敗。
    #[error("agent process transport failed: {0}")]
    Transport(String),
}

/// 検出済みエージェントプラグイン一覧と警告を保持するレジストリ。
#[derive(Debug, Clone, Default)]
pub struct AgentPluginRegistry {
    plugins: Vec<DiscoveredAgentPlugin>,
    warnings: Vec<String>,
}

impl AgentPluginRegistry {
    /// 既定のエージェントプラグイン検索ディレクトリ一覧を返す。
    #[must_use]
    pub fn default_search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = env::var("SEA_AGENT_PLUGIN_DIR")
            && !env_dir.trim().is_empty()
        {
            dirs.push(PathBuf::from(env_dir));
        }
        if let Ok(exe) = env::current_exe()
            && let Some(exe_dir) = exe.parent()
        {
            dirs.push(exe_dir.join("plugins").join("agent"));
            let mut cur = Some(exe_dir);
            while let Some(dir) = cur {
                if dir.join("Cargo.toml").is_file() && dir.join("plugins").is_dir() {
                    dirs.push(dir.join("plugins").join("agent"));
                    break;
                }
                cur = dir.parent();
            }
        }
        if let Ok(cwd) = env::current_dir() {
            dirs.push(cwd.join("plugins").join("agent"));
        }
        dedup_paths(dirs)
    }

    /// 既定ディレクトリ群からすべてのエージェントプラグインを検出する。
    #[must_use]
    pub fn discover_default() -> Self {
        Self::discover_in_dirs(&Self::default_search_dirs())
    }

    /// 指定ディレクトリ群から `<dir>/<sub >/agent-plugin.json`（および直下）を走査する。
    #[must_use]
    pub fn discover_in_dirs(search_dirs: &[PathBuf]) -> Self {
        let mut reg = Self::default();
        let mut seen_ids = HashSet::new();
        for dir in search_dirs {
            if !dir.is_dir() {
                continue;
            }
            let direct = dir.join("agent-plugin.json");
            if direct.is_file() {
                reg.try_register(dir, &direct, &mut seen_ids);
            }
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut subdirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            subdirs.sort();
            for sub in subdirs {
                let manifest_path = sub.join("agent-plugin.json");
                if manifest_path.is_file() {
                    reg.try_register(&sub, &manifest_path, &mut seen_ids);
                }
            }
        }
        reg.plugins
            .sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        reg
    }

    fn try_register(
        &mut self,
        plugin_dir: &Path,
        manifest_path: &Path,
        seen_ids: &mut HashSet<String>,
    ) {
        match AgentPluginManifest::from_file(manifest_path) {
            Ok(manifest) => {
                if !seen_ids.insert(manifest.id.clone()) {
                    return;
                }
                let executable_path = resolve_plugin_executable(plugin_dir, &manifest.entrypoint);
                if executable_path.is_none() {
                    self.warnings.push(format!(
                        "executable '{}' missing for '{}'",
                        manifest.entrypoint, manifest.id
                    ));
                }
                self.plugins.push(DiscoveredAgentPlugin {
                    manifest,
                    manifest_path: manifest_path.to_path_buf(),
                    plugin_dir: plugin_dir.to_path_buf(),
                    executable_path,
                });
            }
            Err(err) => self.warnings.push(err.to_string()),
        }
    }

    /// 検出されたすべてのエージェントプラグインを返す。
    #[must_use]
    pub fn plugins(&self) -> &[DiscoveredAgentPlugin] {
        &self.plugins
    }

    /// ID 指定でエージェントプラグインを検索する。
    #[must_use]
    pub fn find_by_id(&self, id: &str) -> Option<&DiscoveredAgentPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == id)
    }

    /// 検出時の警告メッセージ一覧を返す。
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// ホスト側のツール記述子をワイヤー型へ変換する。
#[must_use]
pub fn descriptor_to_wire_spec(desc: &AgentToolDescriptor) -> AgentToolSpec {
    AgentToolSpec {
        name: desc.name.clone(),
        description: desc.description.clone(),
        input_schema: desc.input_schema.clone(),
        safety_class: match desc.safety_class {
            ToolSafetyClass::ReadOnly => "read_only",
            ToolSafetyClass::DraftMutation => "draft_mutation",
            ToolSafetyClass::ExternalSideEffect => "external_side_effect",
        }
        .to_owned(),
        source: desc.category.clone(),
    }
}

/// レジストリ内の全ツールをワイヤー仕様一覧へ変換する。
#[must_use]
pub fn wire_specs_from_registry(registry: &HostToolRegistry) -> Vec<AgentToolSpec> {
    registry
        .list_descriptors()
        .iter()
        .map(descriptor_to_wire_spec)
        .collect()
}

/// 実行中の常駐エージェントセッションから UI 側へ通知されるイベント。
#[derive(Debug, Clone)]
pub enum AgentHostEvent {
    /// 進捗ステータス通知。
    Status {
        /// 進捗テキスト。
        message: String,
        /// ステップ番号。
        step: Option<u32>,
        /// 想定総ステップ数。
        total_steps: Option<u32>,
    },
    /// LLM が逐次生成している部分テキストチャンク。
    StreamDelta {
        /// 追記テキスト。
        text: String,
    },
    /// ホストツール実行結果のアクティビティログ。
    ToolActivity {
        /// 実行されたツール名。
        tool_name: String,
        /// 成功フラグ。
        ok: bool,
        /// 短い結果サマリ。
        summary: String,
    },
    /// エージェントからのリッチ対話質問（UI 上の入力カード表示を要求）。
    AskUser {
        /// 回答時に返送するリクエスト ID。
        request_id: String,
        /// カード見出し。
        title: String,
        /// 質問本文。
        message: String,
        /// 入力フィールド定義一覧。
        fields: Vec<AgentAskField>,
        /// クイック選択ボタン定義一覧。
        choices: Vec<AgentAskChoice>,
        /// スキップボタン許可フラグ。
        allow_skip: bool,
    },
    /// `Manual` モード時に外部副作用ツール実行前の承認を要求する通知。
    ToolApprovalRequired {
        /// 承認結果返送に使うリクエスト ID。
        request_id: String,
        /// 対象ツール名。
        tool_name: String,
        /// 引数 JSON。
        arguments: Value,
        /// ツール説明文。
        description: String,
    },
    /// エージェントが構成し、ホストの `dry_run_validate` を通過した編集提案。
    Proposal {
        /// 構成要約 Markdown。
        summary_markdown: String,
        /// `EditCommand` 本体。
        command: Box<EditCommand>,
        /// 検証済み編集計画（適用詳細要約付き）。
        plan: Box<AiEditPlan>,
        /// 更新後の永続化対象 `agent_memory`。
        agent_memory: Value,
        /// 高レベル設計（`MovieBlueprint` 等）の保持 JSON。
        blueprint: Option<Value>,
    },
    /// セッション記憶が更新された通知。
    MemoryUpdated {
        /// 最新 `agent_memory`。
        agent_memory: Value,
    },
    /// 1 回のエージェント LLM 呼び出し完了時に報告されるトークン使用量。
    UsageReported {
        /// 抽出または推定された使用量メタデータ。
        usage: AiTokenUsage,
    },
    /// エージェントまたはホスト側ブリッジで発生したエラー。
    AgentError {
        /// エラーコード。
        code: String,
        /// エラー内容。
        message: String,
        /// 同一セッション上で回復・再試行可能か。
        recoverable: bool,
    },
    /// 子プロセス終了通知。
    Exited {
        /// 終了理由および診断抜粋。
        reason: String,
    },
}

/// 常駐エージェントセッション起動パラメータ。
pub struct AgentSessionStartConfig {
    /// 起動対象プラグイン。
    pub plugin: DiscoveredAgentPlugin,
    /// セッション識別子。
    pub session_id: String,
    /// 初期タスク指示文（`resume == false` 時に使用）。
    pub goal: String,
    /// 既存セッションの復元起動かどうか。
    pub resume: bool,
    /// ユーザーが明示添付した許可メディアフォルダ一覧。
    pub attached_folders: Vec<PathBuf>,
    /// 添付クリップ ID / 名前一覧。
    pub attached_clips: Vec<String>,
    /// 現在のタイムラインスナップショット（Draft Sandbox の土台）。
    pub base_document: ProjectDocument,
    /// 現在のタイムライン要約テキスト。
    pub timeline_summary: String,
    /// 永続化されていたまたは初期のセッション記憶。
    pub agent_memory: Value,
    /// バックエンド推論を実行する AI プロバイダ（ホスト側認証・設定を共用）。
    pub provider: Arc<dyn AiProvider>,
    /// プロバイダ種別メタデータ（診断ログ用）。
    pub provider_kind: AiProviderKind,
    /// 使用モデル ID。
    pub model_name: String,
    /// 思考レベル設定。
    pub thinking_level: AiThinkingLevel,
    /// 承認モード（`Manual` / `Autopilot`）。
    pub approval_mode: AiApprovalMode,
    /// ステップ上限・呼出間隔・Vision 設定。
    pub runtime_settings: AgentRuntimeSettings,
    /// 使用するツールレジストリ（`None` の場合は組み込みツールセットを使用）。
    pub tool_registry: Option<HostToolRegistry>,
    /// 能力スナップショットへ含める追加拡張情報。
    pub extra_extensions: Vec<InstalledExtensionSummary>,
}

enum CoordinatorMsg {
    ChildMessage(AgentToHostMessage),
    ChildEof,
    ChildReadError(String),
    FollowUp {
        prompt: String,
        attached_folders: Vec<PathBuf>,
        attached_clips: Vec<String>,
        base_document: ProjectDocument,
        timeline_summary: String,
    },
    UserAnswer {
        request_id: String,
        answer: AgentUserAnswerPayload,
    },
    ToolApprovalDecision {
        request_id: String,
        approved: bool,
        reason: Option<String>,
    },
    UpdateRuntime {
        provider: Arc<dyn AiProvider>,
        model_name: String,
        thinking_level: AiThinkingLevel,
        approval_mode: AiApprovalMode,
        settings: AgentRuntimeSettings,
    },
    Shutdown,
}

struct PendingApprovalCall {
    request_id: String,
    tool_name: String,
    arguments: Value,
}

struct LlmChatPayload {
    request_id: String,
    system_prompt: Option<String>,
    messages: Vec<AgentLlmMessage>,
    tools: Vec<AgentToolSpec>,
    response_json_schema: Option<Value>,
    prefer_json_object: bool,
}

fn spawn_agent_child(
    plugin: &DiscoveredAgentPlugin,
) -> Result<
    (
        Child,
        ChildStdin,
        std::process::ChildStdout,
        std::process::ChildStderr,
    ),
    AgentPluginHostError,
> {
    let exe_path =
        plugin
            .executable_path
            .clone()
            .ok_or_else(|| AgentPluginHostError::ExecutableMissing {
                plugin_id: plugin.manifest.id.clone(),
                entrypoint: plugin.manifest.entrypoint.clone(),
            })?;
    if !exe_path.is_file() {
        return Err(AgentPluginHostError::ExecutableMissing {
            plugin_id: plugin.manifest.id.clone(),
            entrypoint: plugin.manifest.entrypoint.clone(),
        });
    }

    let mut cmd = Command::new(&exe_path);
    cmd.args(&plugin.manifest.args)
        .current_dir(&plugin.plugin_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_background_process_flags(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|err| AgentPluginHostError::Transport(format!("spawn failed: {err}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentPluginHostError::Transport("missing piped stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentPluginHostError::Transport("missing piped stdout".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentPluginHostError::Transport("missing piped stderr".to_owned()))?;
    Ok((child, stdin, stdout, stderr))
}

#[allow(clippy::too_many_arguments)]
fn build_initial_host_message(
    session_id: &str,
    goal: &str,
    resume: bool,
    attached_folders: &[PathBuf],
    attached_clips: &[String],
    timeline_summary: &str,
    agent_memory: &Value,
    tool_registry: &HostToolRegistry,
    extra_extensions: &[InstalledExtensionSummary],
) -> HostToAgentMessage {
    let wire_tools = wire_specs_from_registry(tool_registry);
    let mut caps = collect_host_capabilities(false);
    caps.installed_extensions
        .extend(extra_extensions.iter().cloned());
    let caps_value = serde_json::to_value(&caps).unwrap_or_else(|_| json!({}));

    if resume {
        HostToAgentMessage::ResumeSession {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            session_id: session_id.to_owned(),
            capabilities: caps_value,
            tools: wire_tools,
            timeline_summary: timeline_summary.to_owned(),
            agent_memory: agent_memory.clone(),
        }
    } else {
        HostToAgentMessage::StartTask {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            session_id: session_id.to_owned(),
            goal: goal.to_owned(),
            attached_folders: attached_folders
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            attached_clips: attached_clips.to_vec(),
            capabilities: caps_value,
            tools: wire_tools,
            timeline_summary: timeline_summary.to_owned(),
            agent_memory: agent_memory.clone(),
        }
    }
}

/// 別プロセスとして常駐するエージェントプラグインと通信・安全制御を行うホストハンドル。
pub struct AgentSessionRunner {
    session_id: String,
    plugin_id: String,
    cmd_tx: mpsc::Sender<CoordinatorMsg>,
    event_rx: Mutex<mpsc::Receiver<AgentHostEvent>>,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl AgentSessionRunner {
    /// エージェントプラグイン子プロセスを起動し、初期タスクまたはセッション復元指示を送信する。
    ///
    /// # Errors
    ///
    /// バイナリ未ビルド、または子プロセス起動失敗時に [`AgentPluginHostError`] を返す。
    pub fn start(config: AgentSessionStartConfig) -> Result<Self, AgentPluginHostError> {
        let (child, stdin, stdout, stderr) = spawn_agent_child(&config.plugin)?;

        let tool_registry = config
            .tool_registry
            .unwrap_or_else(HostToolRegistry::with_builtin_tools);
        let initial_msg = build_initial_host_message(
            &config.session_id,
            &config.goal,
            config.resume,
            &config.attached_folders,
            &config.attached_clips,
            &config.timeline_summary,
            &config.agent_memory,
            &tool_registry,
            &config.extra_extensions,
        );

        let (coord_tx, coord_rx) = mpsc::channel::<CoordinatorMsg>();
        let (event_tx, event_rx) = mpsc::channel::<AgentHostEvent>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let child_handle = Arc::new(Mutex::new(Some(child)));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));

        spawn_stdout_reader(stdout, coord_tx.clone());
        spawn_stderr_drain(stderr, Arc::clone(&stderr_tail));

        let state = CoordinatorState {
            child: Arc::clone(&child_handle),
            stdin: Some(stdin),
            initial_msg: Some(initial_msg),
            tool_registry,
            folder_scope: AllowedFolderScope::from_folders(&config.attached_folders),
            attached_folders_raw: config.attached_folders,
            sandbox: DraftSandbox::new(config.base_document),
            agent_memory: config.agent_memory,
            provider: config.provider,
            provider_kind: config.provider_kind,
            model_name: config.model_name,
            thinking_level: config.thinking_level,
            approval_mode: config.approval_mode,
            runtime_settings: config.runtime_settings,
            extra_extensions: config.extra_extensions,
            last_llm_call_at: None,
            turn_step_count: 0,
            pending_approvals: HashMap::new(),
            cancelled: Arc::clone(&cancelled),
            alive: Arc::clone(&alive),
            stderr_tail: Arc::clone(&stderr_tail),
        };
        std::thread::spawn(move || run_coordinator_loop(state, &coord_rx, &event_tx));

        Ok(Self {
            session_id: config.session_id,
            plugin_id: config.plugin.manifest.id,
            cmd_tx: coord_tx,
            event_rx: Mutex::new(event_rx),
            child: child_handle,
            cancelled,
            alive,
            stderr_tail,
        })
    }

    /// セッション ID を返す。
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 起動中のエージェントプラグイン ID を返す。
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// エージェントプロセスが生存中かどうかを返す。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// 届いているすべてのホストイベントをノンブロッキングで取り出す。
    #[must_use]
    pub fn poll_events(&self) -> Vec<AgentHostEvent> {
        let mut events = Vec::new();
        if let Ok(rx) = self.event_rx.lock() {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        events
    }

    /// 指定タイムアウト以内に次のイベントを受信する（テストや同期待機向け）。
    ///
    /// # Errors
    ///
    /// タイムアウトまたはチャネル切断時に `RecvTimeoutError` を返す。
    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AgentHostEvent, mpsc::RecvTimeoutError> {
        let rx = self
            .event_rx
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        rx.recv_timeout(timeout)
    }

    /// 直近の `stderr` 診断ログ行を返す。
    #[must_use]
    pub fn recent_stderr_lines(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 同一常駐セッションに対してフォローアップ調整指示を送信する。
    ///
    /// # Errors
    ///
    /// セッションが既に終了している場合に [`AgentPluginHostError::Transport`] を返す。
    pub fn send_follow_up(
        &self,
        prompt: String,
        attached_folders: Vec<PathBuf>,
        attached_clips: Vec<String>,
        base_document: ProjectDocument,
        timeline_summary: String,
    ) -> Result<(), AgentPluginHostError> {
        self.cancelled.store(false, Ordering::Release);
        self.cmd_tx
            .send(CoordinatorMsg::FollowUp {
                prompt,
                attached_folders,
                attached_clips,
                base_document,
                timeline_summary,
            })
            .map_err(|_| AgentPluginHostError::Transport("agent coordinator stopped".to_owned()))
    }

    /// `AskUser` 質問カードに対するユーザー回答を返送する。
    ///
    /// # Errors
    ///
    /// セッション終了時に [`AgentPluginHostError::Transport`] を返す。
    pub fn send_user_answer(
        &self,
        request_id: String,
        answer: AgentUserAnswerPayload,
    ) -> Result<(), AgentPluginHostError> {
        self.cmd_tx
            .send(CoordinatorMsg::UserAnswer { request_id, answer })
            .map_err(|_| AgentPluginHostError::Transport("agent coordinator stopped".to_owned()))
    }

    /// `ToolApprovalRequired` に対するユーザー許可／拒否判断を返送する。
    ///
    /// # Errors
    ///
    /// セッション終了時に [`AgentPluginHostError::Transport`] を返す。
    pub fn send_tool_approval(
        &self,
        request_id: String,
        approved: bool,
        reason: Option<String>,
    ) -> Result<(), AgentPluginHostError> {
        self.cmd_tx
            .send(CoordinatorMsg::ToolApprovalDecision {
                request_id,
                approved,
                reason,
            })
            .map_err(|_| AgentPluginHostError::Transport("agent coordinator stopped".to_owned()))
    }

    /// 稼働中セッションのプロバイダ・承認モード・ランタイム設定を動的に更新する。
    ///
    /// # Errors
    ///
    /// セッション終了時に [`AgentPluginHostError::Transport`] を返す。
    pub fn update_runtime(
        &self,
        provider: Arc<dyn AiProvider>,
        model_name: String,
        thinking_level: AiThinkingLevel,
        approval_mode: AiApprovalMode,
        settings: AgentRuntimeSettings,
    ) -> Result<(), AgentPluginHostError> {
        self.cmd_tx
            .send(CoordinatorMsg::UpdateRuntime {
                provider,
                model_name,
                thinking_level,
                approval_mode,
                settings,
            })
            .map_err(|_| AgentPluginHostError::Transport("agent coordinator stopped".to_owned()))
    }

    /// 実行中エージェントを停止・終了させる。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(CoordinatorMsg::Shutdown);
    }

    /// 終了を短時間待機し、停止しない場合は子プロセスを強制回収して同期終了する（アプリ終了時向け）。
    pub fn shutdown(&self) {
        self.cancel();
        for _ in 0..15 {
            if !self.alive.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let child = self.child.lock().ok().and_then(|mut guard| guard.take());
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
            self.alive.store(false, Ordering::Release);
        }
    }
}

impl Drop for AgentSessionRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct CoordinatorState {
    child: Arc<Mutex<Option<Child>>>,
    stdin: Option<ChildStdin>,
    initial_msg: Option<HostToAgentMessage>,
    tool_registry: HostToolRegistry,
    folder_scope: AllowedFolderScope,
    attached_folders_raw: Vec<PathBuf>,
    sandbox: DraftSandbox,
    agent_memory: Value,
    provider: Arc<dyn AiProvider>,
    provider_kind: AiProviderKind,
    model_name: String,
    thinking_level: AiThinkingLevel,
    approval_mode: AiApprovalMode,
    runtime_settings: AgentRuntimeSettings,
    extra_extensions: Vec<InstalledExtensionSummary>,
    last_llm_call_at: Option<Instant>,
    turn_step_count: u32,
    pending_approvals: HashMap<String, PendingApprovalCall>,
    cancelled: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Drop for CoordinatorState {
    fn drop(&mut self) {
        self.shutdown_child(Some("coordinator_dropped"));
    }
}

impl CoordinatorState {
    fn send_to_child(&mut self, msg: &HostToAgentMessage) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return false;
        };
        write_ndjson_line(stdin, msg).is_ok()
    }

    fn take_child(&self) -> Option<Child> {
        self.child.lock().ok().and_then(|mut g| g.take())
    }

    fn reap_child_bounded(&mut self) {
        let Some(mut child) = self.take_child() else {
            self.alive.store(false, Ordering::Release);
            return;
        };
        for _ in 0..10 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.alive.store(false, Ordering::Release);
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        self.alive.store(false, Ordering::Release);
    }

    fn shutdown_child(&mut self, reason: Option<&str>) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = write_ndjson_line(
                &mut stdin,
                &HostToAgentMessage::Cancel {
                    reason: reason.map(str::to_owned),
                },
            );
            drop(stdin);
        }
        self.reap_child_bounded();
    }

    fn check_step_budget(&mut self) -> Result<(), String> {
        self.turn_step_count = self.turn_step_count.saturating_add(1);
        let max_steps = self.runtime_settings.max_steps_per_turn;
        if max_steps > 0 && self.turn_step_count > max_steps {
            return Err(format!(
                "max_steps_per_turn ({max_steps}) exceeded in this turn"
            ));
        }
        Ok(())
    }

    fn throttle_with_cancel(&mut self) -> bool {
        let min_ms = self.runtime_settings.min_llm_interval_ms;
        if min_ms > 0
            && let Some(prev) = self.last_llm_call_at
        {
            let target = Duration::from_millis(min_ms);
            while let Some(remaining) = target.checked_sub(prev.elapsed()) {
                if self.cancelled.load(Ordering::Acquire) {
                    return false;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(50)));
            }
        }
        self.last_llm_call_at = Some(Instant::now());
        !self.cancelled.load(Ordering::Acquire)
    }

    fn execute_registered_tool(
        &mut self,
        request_id: &str,
        tool_name: &str,
        arguments: &Value,
        event_tx: &mpsc::Sender<AgentHostEvent>,
    ) {
        let prev_count = self.tool_registry.list_descriptors().len();
        let result = {
            let mut on_usage = |usage| {
                let _ = event_tx.send(AgentHostEvent::UsageReported { usage });
            };
            let mut ctx = ToolExecutionContext {
                folder_scope: &self.folder_scope,
                sandbox: &mut self.sandbox,
                llm_provider: Some(self.provider.as_ref()),
                runtime_settings: &self.runtime_settings,
                last_llm_call_at: &mut self.last_llm_call_at,
                cancelled: Some(&self.cancelled),
                on_progress: &mut |msg| {
                    let _ = event_tx.send(AgentHostEvent::Status {
                        message: msg.to_owned(),
                        step: None,
                        total_steps: None,
                    });
                },
                on_usage: Some(&mut on_usage),
                extra_extensions: self.extra_extensions.clone(),
            };
            self.tool_registry.execute(tool_name, arguments, &mut ctx)
        };

        let new_count = self.tool_registry.list_descriptors().len();
        let updated_tools =
            (new_count != prev_count).then(|| wire_specs_from_registry(&self.tool_registry));

        match result {
            Ok(val) => {
                let _ = event_tx.send(AgentHostEvent::ToolActivity {
                    tool_name: tool_name.to_owned(),
                    ok: true,
                    summary: "ok".to_owned(),
                });
                let _ = self.send_to_child(&HostToAgentMessage::ToolResult {
                    request_id: request_id.to_owned(),
                    ok: true,
                    result: Some(val),
                    error: None,
                    updated_tools,
                });
            }
            Err(err) => {
                let _ = event_tx.send(AgentHostEvent::ToolActivity {
                    tool_name: tool_name.to_owned(),
                    ok: false,
                    summary: err.clone(),
                });
                let _ = self.send_to_child(&HostToAgentMessage::ToolResult {
                    request_id: request_id.to_owned(),
                    ok: false,
                    result: None,
                    error: Some(err),
                    updated_tools,
                });
            }
        }
    }
}

fn run_coordinator_loop(
    mut state: CoordinatorState,
    coord_rx: &mpsc::Receiver<CoordinatorMsg>,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    if let Some(initial_msg) = state.initial_msg.take()
        && !state.send_to_child(&initial_msg)
    {
        let err = "initial NDJSON handshake write failed".to_owned();
        let _ = event_tx.send(AgentHostEvent::AgentError {
            code: "transport_error".to_owned(),
            message: err.clone(),
            recoverable: false,
        });
        state.shutdown_child(Some("initial_write_failed"));
        let _ = event_tx.send(AgentHostEvent::Exited { reason: err });
        return;
    }

    while let Ok(msg) = coord_rx.recv() {
        match msg {
            CoordinatorMsg::Shutdown => {
                state.shutdown_child(Some("cancelled_by_host"));
                let _ = event_tx.send(AgentHostEvent::Exited {
                    reason: "cancelled".to_owned(),
                });
                return;
            }
            CoordinatorMsg::ChildEof => {
                drop(state.stdin.take());
                state.reap_child_bounded();
                let tail = state
                    .stderr_tail
                    .lock()
                    .map(|g| g.iter().cloned().collect::<Vec<_>>().join(" | "))
                    .unwrap_or_default();
                let reason = if tail.is_empty() {
                    "plugin exited".to_owned()
                } else {
                    format!("plugin exited ({tail})")
                };
                let _ = event_tx.send(AgentHostEvent::Exited { reason });
                return;
            }
            CoordinatorMsg::ChildReadError(err) => {
                let _ = event_tx.send(AgentHostEvent::AgentError {
                    code: "protocol_read_error".to_owned(),
                    message: err.clone(),
                    recoverable: false,
                });
                state.shutdown_child(Some("protocol_read_error"));
                let _ = event_tx.send(AgentHostEvent::Exited { reason: err });
                return;
            }
            CoordinatorMsg::FollowUp {
                prompt,
                attached_folders,
                attached_clips,
                base_document,
                timeline_summary,
            } => handle_follow_up_msg(
                &mut state,
                prompt,
                &attached_folders,
                attached_clips,
                base_document,
                timeline_summary,
            ),
            CoordinatorMsg::UserAnswer { request_id, answer } => {
                let _ = state.send_to_child(&HostToAgentMessage::UserAnswer { request_id, answer });
            }
            CoordinatorMsg::ToolApprovalDecision {
                request_id,
                approved,
                reason,
            } => handle_tool_approval_msg(&mut state, &request_id, approved, reason, event_tx),
            CoordinatorMsg::UpdateRuntime {
                provider,
                model_name,
                thinking_level,
                approval_mode,
                settings,
            } => {
                state.provider = provider;
                state.model_name = model_name;
                state.thinking_level = thinking_level;
                state.approval_mode = approval_mode;
                state.runtime_settings = settings;
            }
            CoordinatorMsg::ChildMessage(agent_msg) => {
                handle_agent_message(&mut state, agent_msg, event_tx);
            }
        }
    }
}

fn handle_follow_up_msg(
    state: &mut CoordinatorState,
    prompt: String,
    attached_folders: &[PathBuf],
    attached_clips: Vec<String>,
    base_document: ProjectDocument,
    timeline_summary: String,
) {
    state.turn_step_count = 0;
    state.pending_approvals.clear();
    state.sandbox = DraftSandbox::new(base_document);
    if !attached_folders.is_empty() {
        let mut combined = state.attached_folders_raw.clone();
        combined.extend(attached_folders.iter().cloned());
        state.attached_folders_raw.clone_from(&combined);
        state.folder_scope = AllowedFolderScope::from_folders(combined);
    }
    let folder_strings: Vec<String> = attached_folders
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let _ = state.send_to_child(&HostToAgentMessage::FollowUp {
        prompt,
        attached_folders: folder_strings,
        attached_clips,
        timeline_summary,
    });
}

fn handle_tool_approval_msg(
    state: &mut CoordinatorState,
    request_id: &str,
    approved: bool,
    reason: Option<String>,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    let Some(pending) = state.pending_approvals.remove(request_id) else {
        return;
    };
    if approved {
        state.execute_registered_tool(
            &pending.request_id,
            &pending.tool_name,
            &pending.arguments,
            event_tx,
        );
    } else {
        let _ = state.send_to_child(&HostToAgentMessage::ToolResult {
            request_id: pending.request_id,
            ok: false,
            result: None,
            error: Some(
                reason
                    .unwrap_or_else(|| "Tool execution denied in Manual approval mode".to_owned()),
            ),
            updated_tools: None,
        });
    }
}

fn handle_agent_message(
    state: &mut CoordinatorState,
    msg: AgentToHostMessage,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    match msg {
        AgentToHostMessage::Status {
            message,
            step,
            total_steps,
        } => {
            let _ = event_tx.send(AgentHostEvent::Status {
                message,
                step,
                total_steps,
            });
        }
        AgentToHostMessage::CallTool {
            request_id,
            tool_name,
            arguments,
        } => handle_call_tool(state, request_id, tool_name, arguments, event_tx),
        AgentToHostMessage::LlmChat {
            request_id,
            system_prompt,
            messages,
            tools,
            response_json_schema,
            prefer_json_object,
        } => {
            let payload = LlmChatPayload {
                request_id,
                system_prompt,
                messages,
                tools,
                response_json_schema,
                prefer_json_object,
            };
            handle_llm_chat(state, payload, event_tx);
        }
        AgentToHostMessage::AskUser {
            request_id,
            title,
            message,
            fields,
            choices,
            allow_skip,
        } => {
            let _ = event_tx.send(AgentHostEvent::AskUser {
                request_id,
                title,
                message,
                fields,
                choices,
                allow_skip,
            });
        }
        AgentToHostMessage::Proposal {
            summary_markdown,
            command,
            agent_memory_patch,
            blueprint,
        } => {
            handle_agent_proposal(
                state,
                summary_markdown,
                &command,
                agent_memory_patch,
                blueprint,
                event_tx,
            );
        }
        AgentToHostMessage::Error {
            code,
            message,
            recoverable,
            agent_memory_patch,
        } => {
            if let Some(patch) = agent_memory_patch {
                merge_json_patch(&mut state.agent_memory, patch);
                let _ = event_tx.send(AgentHostEvent::MemoryUpdated {
                    agent_memory: state.agent_memory.clone(),
                });
            }
            let _ = event_tx.send(AgentHostEvent::AgentError {
                code,
                message,
                recoverable,
            });
        }
    }
}

fn handle_call_tool(
    state: &mut CoordinatorState,
    request_id: String,
    tool_name: String,
    arguments: Value,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    if let Err(err) = state.check_step_budget() {
        let _ = state.send_to_child(&HostToAgentMessage::ToolResult {
            request_id,
            ok: false,
            result: None,
            error: Some(err.clone()),
            updated_tools: None,
        });
        let _ = event_tx.send(AgentHostEvent::AgentError {
            code: "max_steps_exceeded".to_owned(),
            message: err,
            recoverable: true,
        });
        return;
    }
    let safety_and_desc = state.tool_registry.find_descriptor(&tool_name);
    if let Some(desc) = safety_and_desc
        && desc.safety_class == ToolSafetyClass::ExternalSideEffect
        && state.approval_mode == AiApprovalMode::Manual
    {
        state.pending_approvals.insert(
            request_id.clone(),
            PendingApprovalCall {
                request_id: request_id.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
            },
        );
        let _ = event_tx.send(AgentHostEvent::ToolApprovalRequired {
            request_id,
            tool_name,
            arguments,
            description: desc.description,
        });
        return;
    }
    state.execute_registered_tool(&request_id, &tool_name, &arguments, event_tx);
}

fn handle_llm_chat(
    state: &mut CoordinatorState,
    payload: LlmChatPayload,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    let request_id = payload.request_id;
    if let Err(err) = state.check_step_budget() {
        send_llm_error(state, request_id, err.clone());
        let _ = event_tx.send(AgentHostEvent::AgentError {
            code: "max_steps_exceeded".to_owned(),
            message: err,
            recoverable: true,
        });
        return;
    }

    if !state.throttle_with_cancel() {
        send_llm_error(state, request_id, "cancelled by user".to_owned());
        return;
    }

    let converted_msgs = match convert_wire_messages_to_turn_messages(
        &payload.messages,
        &state.folder_scope,
        state.runtime_settings.vision_max_edge_px,
    ) {
        Ok(m) => m,
        Err(err) => {
            send_llm_error(state, request_id, err);
            return;
        }
    };

    let req = AiAgentTurnRequest {
        system_prompt: payload.system_prompt.unwrap_or_else(|| {
            format!(
                "You are an intelligent editing assistant running on {} ({}). Respond carefully.",
                state.provider_kind.ui_combo_label(),
                state.model_name
            )
        }),
        messages: converted_msgs,
        tools: payload
            .tools
            .into_iter()
            .map(|t| AiToolDefinition {
                name: t.name,
                description: t.description,
                parameters_schema: t.input_schema,
            })
            .collect(),
        response_json_schema: if payload.response_json_schema.is_some() {
            payload.response_json_schema
        } else if payload.prefer_json_object {
            Some(json!({ "type": "object" }))
        } else {
            None
        },
        temperature: Some(0.2),
    };

    send_agent_llm_turn_and_reply(state, request_id, &req, event_tx);
}

fn forward_agent_stream_event(
    ev: crate::ai::provider::AiStreamEvent,
    suppress_raw_delta: bool,
    structured_chars: &mut usize,
    last_reported_chars: &mut usize,
    tx: &mpsc::Sender<AgentHostEvent>,
) {
    match ev {
        crate::ai::provider::AiStreamEvent::Status(msg) => {
            let _ = tx.send(AgentHostEvent::Status {
                message: msg,
                step: None,
                total_steps: None,
            });
        }
        crate::ai::provider::AiStreamEvent::DeltaText(delta) => {
            if suppress_raw_delta {
                *structured_chars = structured_chars.saturating_add(delta.chars().count());
                if structured_chars.saturating_sub(*last_reported_chars) >= 200 {
                    *last_reported_chars = *structured_chars;
                    let _ = tx.send(AgentHostEvent::Status {
                        message: format!("構成データを生成中... ({structured_chars}文字)"),
                        step: None,
                        total_steps: None,
                    });
                }
            } else {
                let _ = tx.send(AgentHostEvent::StreamDelta { text: delta });
            }
        }
        crate::ai::provider::AiStreamEvent::ActivityHint(label) => {
            let _ = tx.send(AgentHostEvent::Status {
                message: label,
                step: None,
                total_steps: None,
            });
        }
        crate::ai::provider::AiStreamEvent::ThoughtDelta(_)
        | crate::ai::provider::AiStreamEvent::Completed(_)
        | crate::ai::provider::AiStreamEvent::Failed(_) => {}
    }
}

fn send_agent_llm_turn_and_reply(
    state: &mut CoordinatorState,
    request_id: String,
    req: &AiAgentTurnRequest,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    let cancelled_ref = Arc::clone(&state.cancelled);
    let tx_clone = event_tx.clone();
    let suppress_raw_delta = req.response_json_schema.is_some();
    let mut structured_chars = 0_usize;
    let mut last_reported_chars = 0_usize;
    let mut on_stream_event = |ev: crate::ai::provider::AiStreamEvent| {
        if cancelled_ref.load(Ordering::Acquire) {
            return;
        }
        forward_agent_stream_event(
            ev,
            suppress_raw_delta,
            &mut structured_chars,
            &mut last_reported_chars,
            &tx_clone,
        );
    };

    match state.provider.send_agent_turn(req, &mut on_stream_event) {
        Ok(resp) => {
            let reported_usage = resp.usage.clone().unwrap_or_else(|| {
                AiTokenUsage::estimate_from_chars(
                    req.estimated_input_chars(),
                    resp.estimated_output_chars(),
                )
            });
            let _ = event_tx.send(AgentHostEvent::UsageReported {
                usage: reported_usage,
            });
            let tool_calls = resp
                .tool_calls
                .into_iter()
                .map(|c| AgentLlmToolCall {
                    id: c.id,
                    name: c.name,
                    arguments: c.arguments,
                    thought_signature: c.thought_signature,
                })
                .collect();
            let thinking_blocks = resp
                .thinking_blocks
                .into_iter()
                .map(|b| sea_agent_protocol::AgentThinkingBlock {
                    thinking: b.thinking,
                    signature: b.signature,
                })
                .collect();
            let _ = state.send_to_child(&HostToAgentMessage::LlmResponse {
                request_id,
                ok: true,
                text: resp.text.unwrap_or_default(),
                thinking_blocks,
                tool_calls,
                finish_reason: resp.finish_reason,
                error: None,
            });
        }
        Err(err) => {
            if let AiError::HttpStatus { status, .. } = &err {
                if *status == 429 {
                    let _ = event_tx.send(AgentHostEvent::AgentError {
                        code: "rate_limited".to_owned(),
                        message: err.to_string(),
                        recoverable: true,
                    });
                } else if *status == 403 {
                    let _ = event_tx.send(AgentHostEvent::AgentError {
                        code: "forbidden".to_owned(),
                        message: err.to_string(),
                        recoverable: true,
                    });
                }
            }
            send_llm_error(state, request_id, err.to_string());
        }
    }
}

fn send_llm_error(state: &mut CoordinatorState, request_id: String, error: String) {
    let _ = state.send_to_child(&HostToAgentMessage::LlmResponse {
        request_id,
        ok: false,
        text: String::new(),
        thinking_blocks: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: None,
        error: Some(error),
    });
}

fn convert_wire_messages_to_turn_messages(
    messages: &[AgentLlmMessage],
    folder_scope: &AllowedFolderScope,
    max_edge_px: u32,
) -> Result<Vec<AiTurnMessage>, String> {
    let clamped_edge = max_edge_px.clamp(128, 1024);
    let mut call_id_to_name: HashMap<String, String> = HashMap::new();
    let mut remaining_images = MAX_CHAT_IMAGE_PATHS;
    let mut out = Vec::with_capacity(messages.len());

    for msg in messages {
        let role_lower = msg.role.trim().to_ascii_lowercase();
        let turn_msg = match role_lower.as_str() {
            "system" => AiTurnMessage::System {
                text: msg.text.clone(),
            },
            "assistant" | "model" => {
                for c in &msg.tool_calls {
                    call_id_to_name.insert(c.id.clone(), c.name.clone());
                }
                AiTurnMessage::Assistant {
                    text: (!msg.text.is_empty()).then(|| msg.text.clone()),
                    thinking_blocks: msg
                        .thinking_blocks
                        .iter()
                        .map(|b| crate::ai::provider::AiThinkingBlock {
                            thinking: b.thinking.clone(),
                            signature: b.signature.clone(),
                        })
                        .collect(),
                    tool_calls: msg
                        .tool_calls
                        .iter()
                        .map(|c| AiToolCall {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.clone(),
                            thought_signature: c.thought_signature.clone(),
                        })
                        .collect(),
                }
            }
            "tool" => {
                let tool_call_id = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "tool_call".to_owned());
                let resolved_name = call_id_to_name
                    .get(&tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_owned());
                AiTurnMessage::ToolResult {
                    tool_call_id,
                    name: resolved_name,
                    content_json: msg.text.clone(),
                    is_error: false,
                }
            }
            _ => {
                if msg.image_paths.len() > remaining_images {
                    return Err(format!(
                        "too many images in LlmChat request (max {MAX_CHAT_IMAGE_PATHS})"
                    ));
                }
                let mut parts = Vec::new();
                if !msg.text.is_empty() {
                    parts.push(AiContentPart::text(&msg.text));
                }
                for img_path in &msg.image_paths {
                    remaining_images = remaining_images.saturating_sub(1);
                    let thumb = encode_thumbnail_jpeg_base64(
                        folder_scope,
                        Path::new(img_path),
                        clamped_edge,
                    )
                    .map_err(|err| format!("thumbnail encoding failed `{img_path}`: {err}"))?;
                    parts.push(AiContentPart::ImageData {
                        mime_type: thumb.mime_type,
                        base64_data: thumb.base64_data,
                    });
                }
                AiTurnMessage::User { parts }
            }
        };
        out.push(turn_msg);
    }
    Ok(out)
}

fn handle_agent_proposal(
    state: &mut CoordinatorState,
    summary_markdown: String,
    command_value: &Value,
    agent_memory_patch: Option<Value>,
    blueprint: Option<Value>,
    event_tx: &mpsc::Sender<AgentHostEvent>,
) {
    let raw_json = match serde_json::to_string(command_value) {
        Ok(s) => s,
        Err(err) => {
            let _ = event_tx.send(AgentHostEvent::AgentError {
                code: "invalid_proposal_command".to_owned(),
                message: err.to_string(),
                recoverable: true,
            });
            return;
        }
    };

    let plan = match dry_run_validate(state.sandbox.base_document(), &raw_json) {
        Ok(plan) => plan,
        Err(err) => {
            let _ = event_tx.send(AgentHostEvent::AgentError {
                code: "proposal_validation_failed".to_owned(),
                message: err.to_string(),
                recoverable: true,
            });
            return;
        }
    };
    let command: EditCommand = match serde_json::from_value(command_value.clone()) {
        Ok(cmd) => cmd,
        Err(err) => {
            let _ = event_tx.send(AgentHostEvent::AgentError {
                code: "invalid_proposal_command".to_owned(),
                message: err.to_string(),
                recoverable: true,
            });
            return;
        }
    };

    if let Some(patch) = agent_memory_patch {
        merge_json_patch(&mut state.agent_memory, patch);
    }
    if let Some(ref bp) = blueprint {
        merge_json_patch(&mut state.agent_memory, json!({ "last_blueprint": bp }));
    }
    let _ = event_tx.send(AgentHostEvent::MemoryUpdated {
        agent_memory: state.agent_memory.clone(),
    });
    let _ = event_tx.send(AgentHostEvent::Proposal {
        summary_markdown,
        command: Box::new(command),
        plan: Box::new(plan),
        agent_memory: state.agent_memory.clone(),
        blueprint,
    });
}

fn merge_json_patch(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(t_map), Value::Object(p_map)) => {
            for (k, v) in p_map {
                if v.is_null() {
                    t_map.remove(&k);
                } else {
                    match t_map.get_mut(&k) {
                        Some(existing) if existing.is_object() && v.is_object() => {
                            merge_json_patch(existing, v);
                        }
                        _ => {
                            t_map.insert(k, v);
                        }
                    }
                }
            }
        }
        (target_slot, new_val) => {
            *target_slot = new_val;
        }
    }
}

fn spawn_stdout_reader(stdout: std::process::ChildStdout, tx: mpsc::Sender<CoordinatorMsg>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_ndjson_line::<_, AgentToHostMessage>(&mut reader) {
                Ok(Some(msg)) => {
                    if tx.send(CoordinatorMsg::ChildMessage(msg)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(CoordinatorMsg::ChildEof);
                    break;
                }
                Err(err) => {
                    let _ = tx.send(CoordinatorMsg::ChildReadError(err.to_string()));
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    use std::io::{BufRead as _, Read as _};

    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::with_capacity(512);
        loop {
            buf.clear();
            let limit = u64::try_from(MAX_STDERR_LINE_BYTES.saturating_add(1)).unwrap_or(u64::MAX);
            let Ok(read) = (&mut reader).take(limit).read_until(b'\n', &mut buf) else {
                break;
            };
            if read == 0 {
                break;
            }
            // 改行なし・長大行の後続バイトは次行境界まで読み飛ばす
            if read > MAX_STDERR_LINE_BYTES && !buf.ends_with(b"\n") {
                while let Ok(avail) = reader.fill_buf() {
                    if avail.is_empty() {
                        break;
                    }
                    if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                        reader.consume(pos + 1);
                        break;
                    }
                    let len = avail.len();
                    reader.consume(len);
                }
            }
            let text = String::from_utf8_lossy(&buf);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(mut guard) = tail.lock() {
                if guard.len() >= 40 {
                    guard.pop_front();
                }
                guard.push_back(trimmed.chars().take(MAX_STDERR_LINE_BYTES).collect());
            }
        }
    });
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for p in paths {
        let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn agent_manifest_validates_fields_and_rejects_traversal()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = std::env::temp_dir().join(format!(
            "sea_agent_manifest_test_{}_{}",
            std::process::id(),
            crate::ai::session_store::AiSessionStore::generate_session_id()
        ));
        let _guard = TempDirGuard(base.clone());
        let p_dir = base.join("director");
        fs::create_dir_all(&p_dir)?;
        let manifest_path = p_dir.join("agent-plugin.json");

        fs::write(
            &manifest_path,
            r#"{
              "schema_version": 1,
              "id": "sea.agent.director",
              "name": "Director Agent",
              "version": "0.1.0",
              "entrypoint": "sea-agent-director",
              "args": [],
              "protocol": "sea_agent_v1",
              "ai_hints": {
                "purpose": "Build wedding movies",
                "capabilities": ["vision_batch", "movie_blueprint"],
                "recommended_scenarios": ["wedding profile"]
              }
            }"#,
        )?;

        let reg = AgentPluginRegistry::discover_in_dirs(std::slice::from_ref(&base));
        assert_eq!(reg.plugins().len(), 1);
        let entry = &reg.plugins()[0];
        assert_eq!(entry.manifest.id, "sea.agent.director");
        assert_eq!(
            entry
                .manifest
                .ai_hints
                .as_ref()
                .map(|h| h.capabilities.clone()),
            Some(vec![
                "vision_batch".to_owned(),
                "movie_blueprint".to_owned()
            ])
        );

        let bad_path = p_dir.join("bad-agent-plugin.json");
        fs::write(
            &bad_path,
            r#"{
              "schema_version": 1,
              "id": "sea.agent.bad",
              "name": "Bad",
              "version": "0.1.0",
              "entrypoint": "../evil",
              "protocol": "sea_agent_v1"
            }"#,
        )?;
        assert!(matches!(
            AgentPluginManifest::from_file(&bad_path),
            Err(AgentPluginHostError::InvalidManifest { .. })
        ));
        Ok(())
    }

    #[test]
    fn json_merge_patch_updates_nested_memory_and_deletes_nulls() {
        let mut mem = json!({
            "couple": { "groom": "Taro", "bride": "Hanako" },
            "temp_flag": true,
            "captions": ["a", "b"]
        });
        merge_json_patch(
            &mut mem,
            json!({
                "couple": { "date": "2026-11-22" },
                "temp_flag": null,
                "style": "wedding_warm"
            }),
        );
        assert_eq!(mem["couple"]["groom"], "Taro");
        assert_eq!(mem["couple"]["date"], "2026-11-22");
        assert!(mem.get("temp_flag").is_none());
        assert_eq!(mem["style"], "wedding_warm");
    }

    #[test]
    fn wire_conversion_resolves_tool_name_and_validates_entrypoint_and_image_cap()
    -> Result<(), String> {
        let mut manifest = AgentPluginManifest {
            schema_version: 1,
            id: "sea.agent.test".to_owned(),
            name: "Test".to_owned(),
            version: "0.1.0".to_owned(),
            entrypoint: "C:evil.exe".to_owned(),
            args: Vec::new(),
            protocol: AgentPluginProtocol::SeaAgentV1,
            ai_hints: None,
        };
        assert!(manifest.validate(Path::new("agent-plugin.json")).is_err());
        manifest.entrypoint = " sea-agent ".to_owned();
        assert!(manifest.validate(Path::new("agent-plugin.json")).is_err());
        manifest.entrypoint = ".".to_owned();
        assert!(manifest.validate(Path::new("agent-plugin.json")).is_err());

        let wire_msgs = vec![
            AgentLlmMessage {
                role: "assistant".to_owned(),
                text: String::new(),
                image_paths: Vec::new(),
                thinking_blocks: Vec::new(),
                tool_calls: vec![AgentLlmToolCall {
                    id: "call_abc".to_owned(),
                    name: "inspect_media_folder".to_owned(),
                    arguments: json!({"path":"C:/w"}),
                    thought_signature: None,
                }],
                tool_call_id: None,
            },
            AgentLlmMessage {
                role: "tool".to_owned(),
                text: "{\"ok\":true}".to_owned(),
                image_paths: Vec::new(),
                thinking_blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: Some("call_abc".to_owned()),
            },
        ];
        let scope = AllowedFolderScope::default();
        let converted = convert_wire_messages_to_turn_messages(&wire_msgs, &scope, 512)?;
        assert!(matches!(
            &converted[1],
            AiTurnMessage::ToolResult { name, .. } if name == "inspect_media_folder"
        ));

        let too_many_imgs = vec![AgentLlmMessage {
            role: "user".to_owned(),
            text: "hi".to_owned(),
            image_paths: (0..=MAX_CHAT_IMAGE_PATHS)
                .map(|i| format!("img_{i}.png"))
                .collect(),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        assert!(convert_wire_messages_to_turn_messages(&too_many_imgs, &scope, 512).is_err());
        Ok(())
    }

    #[test]
    fn stream_delta_forwards_plain_turns_and_suppresses_structured_schema_turns() {
        let (tx, rx) = mpsc::channel();
        let mut structured = 0_usize;
        let mut reported = 0_usize;

        // response_json_schema なし: StreamDelta をそのまま転送
        forward_agent_stream_event(
            crate::ai::provider::AiStreamEvent::DeltaText("作業中です".to_owned()),
            false,
            &mut structured,
            &mut reported,
            &tx,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentHostEvent::StreamDelta { text }) if text == "作業中です"
        ));

        // response_json_schema あり: StreamDelta を抑止し 200 文字ごとに Status 通知
        let chunk = "a".repeat(220);
        forward_agent_stream_event(
            crate::ai::provider::AiStreamEvent::DeltaText(chunk),
            true,
            &mut structured,
            &mut reported,
            &tx,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentHostEvent::Status { message, .. }) if message.contains("220文字")
        ));
        assert!(rx.try_recv().is_err());
    }
}
