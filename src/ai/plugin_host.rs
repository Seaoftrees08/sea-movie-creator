//! `plugins/ai/<id>/ai-plugin.json` の検出、マニフェスト検証、外部プロセス型 AI プラグイン実行ホスト。

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::ai::provider::{
    AiChatMessage, AiError, AiProvider, AiRequest, AiResponse, AiRole, AiStreamEvent,
    AiThinkingLevel, AiTokenUsage,
};
use crate::ai::settings::ProbedModelEntry;
use crate::core::PluginAiHints;
use crate::export::registry::resolve_plugin_executable;

/// `ai-plugin.json` が宣言できる通信プロトコル種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPluginProtocol {
    /// `sea-movie-creator` 標準の NDJSON stdio エンベロープ (`sea_ai_v1`)。
    #[default]
    SeaAiV1,
    /// Agent Client Protocol (`ACP`) の直接 JSON-RPC stdio。
    AcpStdio,
}

/// 1 つの AI プラグインが提供するサブエージェントプロファイル（例: `claude`, `codex`, `copilot`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiPluginProfile {
    /// プロファイル識別子。
    pub id: String,
    /// UI 表示名。
    pub label: String,
    /// 実行する外部 CLI 名または既定コマンド。
    pub cli: String,
    /// 追加 CLI 引数。
    #[serde(default)]
    pub args: Vec<String>,
}

/// `plugins/ai/<id>/ai-plugin.json` のマニフェスト定義。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiPluginManifest {
    /// スキーマバージョン（現在は `1` のみ対応）。
    pub schema_version: u32,
    /// 一意なプラグイン ID（例: `"sea.ai.antigravity"`, `"sea.ai.acp"`）。
    pub id: String,
    /// UI 表示名。
    pub name: String,
    /// セマンティックバージョン。
    pub version: String,
    /// 起動する実行ファイル名（パストラバーサル不可）。
    pub entrypoint: String,
    /// 起動時固定引数。
    #[serde(default)]
    pub args: Vec<String>,
    /// プロトコル種別。
    #[serde(default)]
    pub protocol: AiPluginProtocol,
    /// プラグインが内部で呼び出す既定外部 CLI ヒント（任意）。
    #[serde(default)]
    pub requires_external_cli: Option<String>,
    /// 選択可能なプロファイル一覧（`ACP` 用など）。
    #[serde(default)]
    pub profiles: Vec<AiPluginProfile>,
    /// AI エージェント向けの任意自己記述ヒント（後方互換）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hints: Option<PluginAiHints>,
}

/// AI プラグイン検出・実行時のエラー。
#[derive(Debug, Error)]
pub enum AiPluginHostError {
    /// マニフェストの形式・値検証エラー。
    #[error("invalid ai-plugin.json: {0}")]
    InvalidManifest(String),
    /// ファイル走査・読み込み I/O エラー。
    #[error("AI plugin discovery I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON パースエラー。
    #[error("AI plugin manifest JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    /// 指定されたプラグイン ID が未登録。
    #[error("AI plugin '{0}' is not registered")]
    PluginNotFound(String),
    /// マニフェストはあるがワークスペースまたは同梱バイナリが見つからない。
    #[error(
        "executable '{entrypoint}' for AI plugin '{plugin_id}' not found (run `cargo build --workspace --all-targets`)"
    )]
    MissingExecutable {
        /// プラグイン ID。
        plugin_id: String,
        /// エントリポイント名。
        entrypoint: String,
    },
}

/// 検出された AI プラグイン 1 件の情報。
#[derive(Debug, Clone)]
pub struct DiscoveredAiPlugin {
    /// 検証済みマニフェスト。
    pub manifest: AiPluginManifest,
    /// `ai-plugin.json` のパス。
    pub manifest_path: PathBuf,
    /// プラグインディレクトリ。
    pub plugin_dir: PathBuf,
    /// 解決された実行バイナリパス（未ビルドなら `None`）。
    pub executable_path: Option<PathBuf>,
}

/// プラグイン検出中の非致命的警告。
#[derive(Debug, Clone)]
pub struct AiPluginWarning {
    /// 問題が起きたパス。
    pub path: PathBuf,
    /// 警告内容。
    pub message: String,
}

/// AI プラグイン (`sea_ai_v1`) へ 1 行 JSON として送るリクエスト。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPluginRequestEnvelope {
    /// プロトコル識別子（`"sea_ai_v1"`）。
    pub protocol: String,
    /// メソッド名（`"generate_command"`）。
    pub method: String,
    /// 呼び出しパラメータ本体。
    pub params: AiPluginCallParams,
}

/// AI プラグイン呼び出しパラメータ。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiPluginCallParams {
    /// システムプロンプト。
    pub system_prompt: String,
    /// `schemars` 駆動の `EditCommand` JSON Schema。
    pub command_schema_json: Value,
    /// タイムライン状態要約 JSON。
    pub context_summary: Value,
    /// 直近会話履歴。
    pub history: Vec<AiChatMessage>,
    /// ユーザー入力プロンプト。
    pub user_prompt: String,
    /// ストリーミング要求フラグ。
    pub stream: bool,
    /// 使用モデルまたはプロファイル指定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 思考強度レベル指定（`low` / `medium` / `high` / `xhigh` 等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// テストやユーザー指定による外部 CLI バイナリ上書きパス。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_cli: Option<String>,
    /// 外部 CLI 向けの追加引数上書き。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_args: Vec<String>,
}

/// `"method": "list_models"` により AI プラグインから取得された利用可能モデル／CLI 情報。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredAiModel {
    /// モデルまたはプロファイル識別子。
    pub id: String,
    /// 人間向け表示名。
    pub display_name: String,
    /// 補助バッジ（`"New"` 等、任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// サポートする思考レベル一覧。
    #[serde(default)]
    pub thinking_levels: Vec<AiThinkingLevel>,
    /// 既定思考レベル。
    #[serde(default)]
    pub default_thinking: AiThinkingLevel,
    /// 現在の環境で実行・選択可能か。
    #[serde(default = "default_true")]
    pub available: bool,
    /// 状態補足メモ（例: `"ACP利用可能"` / `"CLI未検出"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// コンテキストウィンドウ上限トークン数（既知の場合）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
}

const fn default_true() -> bool {
    true
}

impl DiscoveredAiModel {
    /// 設定・UI 側の共通プローブモデル項目 [`ProbedModelEntry`] へ変換する。
    #[must_use]
    pub fn to_probed_entry(&self) -> ProbedModelEntry {
        ProbedModelEntry {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            badge: self.badge.clone(),
            thinking_levels: self.thinking_levels.clone(),
            default_thinking: self.default_thinking,
            context_window_tokens: self.context_window_tokens,
            available: self.available,
            note: self.note.clone(),
        }
    }
}

/// AI プラグイン (`sea_ai_v1`) が標準出力へ 1 行ずつ返す NDJSON イベント。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AiPluginHostEvent {
    /// 進捗ステータスメッセージ。
    Status {
        /// 表示用状態メッセージ。
        message: String,
    },
    /// 部分生成テキストチャンク。
    Delta {
        /// 追記テキスト。
        text: String,
    },
    /// 利用可能モデル・プロファイル一覧応答 (`"method": "list_models"` 用)。
    Models {
        /// 検出されたモデル一覧。
        #[serde(default)]
        models: Vec<DiscoveredAiModel>,
    },
    /// 生成完了と最終テキスト。
    Completed {
        /// 最終応答生テキスト。
        raw_text: String,
        /// 応答モデル・アダプタ名。
        model: String,
        /// トークン使用量・コストメタデータ（外部 CLI が返却した場合）。
        #[serde(default)]
        usage: Option<AiTokenUsage>,
    },
    /// エラー通知。
    Error {
        /// エラー分類コード（例: `"missing_external_cli"`, `"execution_failed"`）。
        code: String,
        /// 人間向け詳細メッセージ。
        message: String,
    },
}

/// `plugins/ai/` 配下の全プラグインを保持するレジストリ。
#[derive(Debug, Clone, Default)]
pub struct AiPluginRegistry {
    plugins: Vec<DiscoveredAiPlugin>,
    warnings: Vec<AiPluginWarning>,
}

impl AiPluginRegistry {
    /// 既定検索ディレクトリから `ai-plugin.json` を自動検出する。
    #[must_use]
    pub fn discover_default() -> Self {
        Self::discover_in_dirs(&Self::default_search_dirs())
    }

    /// 既定の AI プラグイン検索ディレクトリ一覧を返す。
    #[must_use]
    pub fn default_search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = env::var("SEA_AI_PLUGIN_DIR")
            && !env_dir.trim().is_empty()
        {
            dirs.push(PathBuf::from(env_dir));
        }
        if let Ok(exe) = env::current_exe()
            && let Some(exe_dir) = exe.parent()
        {
            dirs.push(exe_dir.join("plugins").join("ai"));
            for ancestor in exe_dir.ancestors().take(6) {
                let candidate = ancestor.join("plugins").join("ai");
                if candidate.is_dir() {
                    dirs.push(candidate);
                    break;
                }
            }
        }
        if let Ok(cwd) = env::current_dir() {
            dirs.push(cwd.join("plugins").join("ai"));
        }
        dedup_paths(dirs)
    }

    /// 指定ディレクトリ群を走査して検証済み AI プラグインを登録する。
    #[must_use]
    pub fn discover_in_dirs(search_dirs: &[PathBuf]) -> Self {
        let mut registry = Self::default();
        let mut seen_ids = HashSet::new();

        for root in search_dirs {
            if !root.is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();

            for plugin_dir in dirs {
                let manifest_path = plugin_dir.join("ai-plugin.json");
                if !manifest_path.is_file() {
                    continue;
                }
                match load_and_validate_manifest(&manifest_path) {
                    Ok(manifest) => {
                        if !seen_ids.insert(manifest.id.clone()) {
                            continue;
                        }
                        let executable_path =
                            resolve_plugin_executable(&plugin_dir, &manifest.entrypoint);
                        registry.plugins.push(DiscoveredAiPlugin {
                            manifest,
                            manifest_path,
                            plugin_dir,
                            executable_path,
                        });
                    }
                    Err(err) => registry.warnings.push(AiPluginWarning {
                        path: manifest_path,
                        message: err.to_string(),
                    }),
                }
            }
        }
        registry
    }

    /// 検出された AI プラグイン一覧を返す。
    #[must_use]
    pub fn plugins(&self) -> &[DiscoveredAiPlugin] {
        &self.plugins
    }

    /// 走査中の警告一覧を返す。
    #[must_use]
    pub fn warnings(&self) -> &[AiPluginWarning] {
        &self.warnings
    }

    /// ID でプラグインを検索する。
    #[must_use]
    pub fn find(&self, plugin_id: &str) -> Option<&DiscoveredAiPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == plugin_id)
    }

    /// 指定プラグインの実行可能バイナリを確認し [`AiPluginProvider`] を構築する。
    ///
    /// # Errors
    ///
    /// プラグイン未検出、またはバイナリ未ビルド時に [`AiPluginHostError`] を返す。
    pub fn resolve_provider(&self, plugin_id: &str) -> Result<AiPluginProvider, AiPluginHostError> {
        let plugin = self
            .find(plugin_id)
            .ok_or_else(|| AiPluginHostError::PluginNotFound(plugin_id.to_owned()))?;
        let exe = plugin
            .executable_path
            .as_ref()
            .filter(|p| p.is_file())
            .cloned()
            .or_else(|| resolve_plugin_executable(&plugin.plugin_dir, &plugin.manifest.entrypoint))
            .ok_or_else(|| AiPluginHostError::MissingExecutable {
                plugin_id: plugin.manifest.id.clone(),
                entrypoint: plugin.manifest.entrypoint.clone(),
            })?;
        Ok(AiPluginProvider::from_discovered(plugin, exe))
    }
}

/// 外部 AI プラグインプロセスを駆動して [`AiProvider`] として動作するプロバイダ。
#[derive(Debug, Clone)]
pub struct AiPluginProvider {
    plugin_id: String,
    display_name: String,
    executable_path: PathBuf,
    args: Vec<String>,
    protocol: AiPluginProtocol,
    model_override: Option<String>,
    thinking_level_override: Option<String>,
    external_cli_override: Option<String>,
    external_args_override: Vec<String>,
    extra_env: Vec<(String, String)>,
}

impl AiPluginProvider {
    /// 検出済みプラグイン情報と解決済み実行ファイルから生成する。
    #[must_use]
    pub fn from_discovered(plugin: &DiscoveredAiPlugin, executable_path: PathBuf) -> Self {
        Self {
            plugin_id: plugin.manifest.id.clone(),
            display_name: plugin.manifest.name.clone(),
            executable_path,
            args: plugin.manifest.args.clone(),
            protocol: plugin.manifest.protocol,
            model_override: None,
            thinking_level_override: None,
            external_cli_override: None,
            external_args_override: Vec::new(),
            extra_env: Vec::new(),
        }
    }

    /// 使用モデル名（または ACP プロファイル名）を設定する。
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }

    /// 思考強度レベル（`low` / `medium` / `high` / `xhigh`）を設定する。
    #[must_use]
    pub fn with_thinking_level(mut self, level: Option<String>) -> Self {
        self.thinking_level_override = level;
        self
    }

    /// プラグイン内部で起動する外部 CLI 実行ファイルを上書きする（テスト・ユーザー独自パス向け）。
    #[must_use]
    pub fn with_external_cli(mut self, cli: impl Into<String>) -> Self {
        self.external_cli_override = Some(cli.into());
        self
    }

    /// 外部 CLI への追加引数を上書きする。
    #[must_use]
    pub fn with_external_args(mut self, args: Vec<String>) -> Self {
        self.external_args_override = args;
        self
    }

    /// 子プロセスへ引き継ぐ環境変数を追加する。
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// プラグインへ `"method": "list_models"` を送信し、現在利用可能なモデル・CLI 一覧を取得する。
    ///
    /// # Errors
    ///
    /// プロセス起動失敗、プロトコル不正、または外部 CLI 未検出・認証エラー時に [`AiError`] を返す。
    pub fn probe_available_models(&self) -> Result<Vec<DiscoveredAiModel>, AiError> {
        if self.protocol != AiPluginProtocol::SeaAiV1 {
            return Ok(Vec::new());
        }
        let mut cmd = Command::new(&self.executable_path);
        sea_export_protocol::apply_background_process_flags(&mut cmd);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|err| {
            AiError::PluginError(format!(
                "failed to spawn AI plugin `{}` for list_models ({}): {err}",
                self.plugin_id,
                self.executable_path.display()
            ))
        })?;

        let envelope = AiPluginRequestEnvelope {
            protocol: "sea_ai_v1".to_owned(),
            method: "list_models".to_owned(),
            params: AiPluginCallParams {
                external_cli: self.external_cli_override.clone(),
                external_args: self.external_args_override.clone(),
                ..AiPluginCallParams::default()
            },
        };
        let payload = serde_json::to_string(&envelope).map_err(|err| {
            AiError::ParseFailed(format!("list_models request serialize error: {err}"))
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{payload}")?;
            stdin.flush()?;
        }

        let Some(stdout) = child.stdout.take() else {
            return Err(AiError::PluginError(
                "AI plugin stdout pipe unavailable".to_owned(),
            ));
        };
        let mut noop = |_ev: AiStreamEvent| {};
        let output = read_sea_ai_v1_events(&self.plugin_id, stdout, &mut child, &mut noop)?;

        let status = child.wait_with_output()?;
        if !status.status.success() {
            let stderr_text = String::from_utf8_lossy(&status.stderr).trim().to_owned();
            let msg = if stderr_text.is_empty() {
                format!(
                    "AI plugin `{}` list_models exited with {}",
                    self.plugin_id, status.status
                )
            } else {
                format!(
                    "AI plugin `{}` list_models exited with {}: {stderr_text}",
                    self.plugin_id, status.status
                )
            };
            return Err(AiError::PluginError(msg));
        }

        Ok(output.discovered_models)
    }

    fn run_sea_ai_v1(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        let mut cmd = Command::new(&self.executable_path);
        sea_export_protocol::apply_background_process_flags(&mut cmd);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|err| {
            AiError::PluginError(format!(
                "failed to spawn AI plugin `{}` ({}): {err}",
                self.plugin_id,
                self.executable_path.display()
            ))
        })?;

        let envelope = AiPluginRequestEnvelope {
            protocol: "sea_ai_v1".to_owned(),
            method: "generate_command".to_owned(),
            params: AiPluginCallParams {
                system_prompt: request.system_prompt.clone(),
                command_schema_json: request.command_schema_json.clone(),
                context_summary: request.context_summary.clone(),
                history: request.history.clone(),
                user_prompt: request.user_prompt.clone(),
                stream: request.stream,
                model: self.model_override.clone(),
                thinking_level: self.thinking_level_override.clone(),
                external_cli: self.external_cli_override.clone(),
                external_args: self.external_args_override.clone(),
            },
        };
        let payload = serde_json::to_string(&envelope).map_err(|err| {
            AiError::ParseFailed(format!("plugin request serialize error: {err}"))
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{payload}")?;
            stdin.flush()?;
        }

        let Some(stdout) = child.stdout.take() else {
            return Err(AiError::PluginError(
                "AI plugin stdout pipe unavailable".to_owned(),
            ));
        };
        let sea_out = read_sea_ai_v1_events(&self.plugin_id, stdout, &mut child, on_event)?;
        let streamed_text = sea_out.streamed_text;
        let final_response = sea_out.final_response;

        let output_status = child.wait_with_output()?;
        if !output_status.status.success() {
            let stderr_text = String::from_utf8_lossy(&output_status.stderr)
                .trim()
                .to_owned();
            let msg = if stderr_text.is_empty() {
                format!(
                    "AI plugin `{}` exited with {}",
                    self.plugin_id, output_status.status
                )
            } else {
                format!(
                    "AI plugin `{}` exited with {}: {stderr_text}",
                    self.plugin_id, output_status.status
                )
            };
            on_event(AiStreamEvent::Failed(msg.clone()));
            return Err(AiError::PluginError(msg));
        }

        let response = final_response.unwrap_or_else(|| AiResponse {
            raw_text: streamed_text,
            model: self
                .model_override
                .clone()
                .unwrap_or_else(|| self.plugin_id.clone()),
            usage: None,
        });
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
    }

    fn run_direct_acp_stdio(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        let mut cmd = Command::new(&self.executable_path);
        sea_export_protocol::apply_background_process_flags(&mut cmd);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|err| {
            AiError::PluginError(format!(
                "failed to spawn ACP agent `{}`: {err}",
                self.plugin_id
            ))
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AiError::PluginError("ACP stdin unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AiError::PluginError("ACP stdout unavailable".to_owned()))?;
        let mut reader = BufReader::new(stdout);

        send_acp_jsonrpc_line(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": { "name": "sea-movie-creator", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
        )?;
        let _init_res = read_acp_until_id(&mut reader, 1, on_event)?;

        let cwd = env::current_dir().map_or_else(|_| ".".to_owned(), |p| p.display().to_string());
        send_acp_jsonrpc_line(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": { "cwd": cwd, "mcpServers": [] }
            }),
        )?;
        let sess_res = read_acp_until_id(&mut reader, 2, on_event)?;
        let session_id = sess_res["result"]["sessionId"]
            .as_str()
            .unwrap_or("default-session")
            .to_owned();

        let prompt_body = build_direct_acp_prompt_body(request);
        send_acp_jsonrpc_line(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": prompt_body }]
                }
            }),
        )?;
        drop(stdin);

        let prompt_res = read_acp_until_id_collecting(&mut reader, 3, on_event)?;
        let _ = child.wait();
        let final_text = if prompt_res.collected_text.is_empty() {
            prompt_res.response["result"]["text"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        } else {
            prompt_res.collected_text
        };
        let response = AiResponse {
            raw_text: final_text,
            model: self.plugin_id.clone(),
            usage: None,
        };
        on_event(AiStreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

impl AiProvider for AiPluginProvider {
    fn id(&self) -> &str {
        &self.plugin_id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn send_streaming(
        &self,
        request: &AiRequest,
        on_event: &mut dyn FnMut(AiStreamEvent),
    ) -> Result<AiResponse, AiError> {
        match self.protocol {
            AiPluginProtocol::SeaAiV1 => self.run_sea_ai_v1(request, on_event),
            AiPluginProtocol::AcpStdio => self.run_direct_acp_stdio(request, on_event),
        }
    }
}

struct SeaAiV1EventsOutput {
    streamed_text: String,
    final_response: Option<AiResponse>,
    discovered_models: Vec<DiscoveredAiModel>,
}

fn read_sea_ai_v1_events(
    plugin_id: &str,
    stdout: std::process::ChildStdout,
    child: &mut std::process::Child,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<SeaAiV1EventsOutput, AiError> {
    let reader = BufReader::new(stdout);
    let mut streamed_text = String::new();
    let mut final_response: Option<AiResponse> = None;
    let mut discovered_models: Vec<DiscoveredAiModel> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: AiPluginHostEvent = serde_json::from_str(trimmed).map_err(|err| {
            AiError::ParseFailed(format!(
                "AI plugin `{plugin_id}` emitted invalid NDJSON: {err} (line: {trimmed})"
            ))
        })?;
        match event {
            AiPluginHostEvent::Status { message } => {
                on_event(AiStreamEvent::Status(message));
            }
            AiPluginHostEvent::Delta { text } => {
                streamed_text.push_str(&text);
                on_event(AiStreamEvent::DeltaText(text));
            }
            AiPluginHostEvent::Models { models } => {
                discovered_models = models;
            }
            AiPluginHostEvent::Completed {
                raw_text,
                model,
                usage,
            } => {
                let chosen = if raw_text.is_empty() {
                    streamed_text.clone()
                } else {
                    raw_text
                };
                final_response = Some(AiResponse {
                    raw_text: chosen,
                    model,
                    usage,
                });
            }
            AiPluginHostEvent::Error { code, message } => {
                let _ = child.wait();
                let err_text = format!("[{code}] {message}");
                on_event(AiStreamEvent::Failed(err_text.clone()));
                return Err(AiError::PluginError(err_text));
            }
        }
    }
    Ok(SeaAiV1EventsOutput {
        streamed_text,
        final_response,
        discovered_models,
    })
}

struct AcpCollectedResult {
    response: Value,
    collected_text: String,
}

fn send_acp_jsonrpc_line(stdin: &mut std::process::ChildStdin, msg: &Value) -> Result<(), AiError> {
    let encoded = serde_json::to_string(msg)
        .map_err(|err| AiError::ParseFailed(format!("ACP encode error: {err}")))?;
    writeln!(stdin, "{encoded}")?;
    stdin.flush()?;
    Ok(())
}

fn read_acp_until_id<R: BufRead>(
    reader: &mut R,
    expected_id: u64,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<Value, AiError> {
    Ok(read_acp_until_id_collecting(reader, expected_id, on_event)?.response)
}

fn read_acp_until_id_collecting<R: BufRead>(
    reader: &mut R,
    expected_id: u64,
    on_event: &mut dyn FnMut(AiStreamEvent),
) -> Result<AcpCollectedResult, AiError> {
    let mut line = String::new();
    let mut collected = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(AiError::PluginError(format!(
                "ACP agent closed stream before response id={expected_id}"
            )));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(trimmed)
            .map_err(|err| AiError::ParseFailed(format!("ACP JSON parse error: {err}")))?;
        if msg["method"].as_str() == Some("session/update")
            && let Some(delta) = extract_acp_update_delta(&msg["params"])
        {
            collected.push_str(&delta);
            on_event(AiStreamEvent::DeltaText(delta));
            continue;
        }
        if msg["id"].as_u64() == Some(expected_id) {
            if let Some(err_obj) = msg.get("error").filter(|v| !v.is_null()) {
                return Err(AiError::PluginError(format!("ACP error: {err_obj}")));
            }
            return Ok(AcpCollectedResult {
                response: msg,
                collected_text: collected,
            });
        }
    }
}

fn extract_acp_update_delta(params: &Value) -> Option<String> {
    let update = params.get("update").unwrap_or(params);
    update["content"]["text"]
        .as_str()
        .or_else(|| update["text"].as_str())
        .or_else(|| update["delta"].as_str())
        .map(ToOwned::to_owned)
}

/// マニフェストファイルを読み込み、必須制約・パストラバーサル禁止を検証する。
///
/// # Errors
///
/// ファイル読み込み、JSON 解析、または検証失敗時に [`AiPluginHostError`] を返す。
pub fn load_and_validate_manifest(path: &Path) -> Result<AiPluginManifest, AiPluginHostError> {
    let raw = fs::read_to_string(path)?;
    let manifest: AiPluginManifest = serde_json::from_str(&raw)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

const MAX_HISTORY_PROMPT_BYTES: usize = 128 * 1024;
const PINNED_INSTRUCTION_MAX_BYTES: usize = 64 * 1024;
const ORIGINAL_INSTRUCTION_MARKER: &str = "# Original User Instruction";

fn is_empty_json_value(val: &Value) -> bool {
    val.is_null() || val.as_object().is_some_and(serde_json::Map::is_empty)
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

const fn role_label(role: AiRole) -> &'static str {
    match role {
        AiRole::System => "system",
        AiRole::User => "user",
        AiRole::Assistant => "assistant",
    }
}

fn compress_acp_history_entries(
    history: &[AiChatMessage],
    entries: &mut [String],
    pinned_idx: Option<usize>,
    session_cap: usize,
    tool_cap: usize,
) {
    let last_tool_idx = history
        .iter()
        .rposition(|m| m.content.trim_start().starts_with("[ToolResult "));
    for (idx, msg) in history.iter().enumerate() {
        if pinned_idx == Some(idx) {
            continue;
        }
        let trimmed = msg.content.trim_start();
        let role = role_label(msg.role);
        if trimmed.starts_with("# Session Context") {
            let compressed = excerpt_head_tail(
                &msg.content,
                session_cap,
                "Session Context 圧縮済み: 必要なら get_timeline_summary を呼び出してください",
            );
            entries[idx] = format!("[{role}]\n{compressed}");
        } else if trimmed.starts_with("[ToolResult ") && last_tool_idx != Some(idx) {
            let compressed = excerpt_head_tail(&msg.content, tool_cap, "ToolResult 省略");
            entries[idx] = format!("[{role}]\n{compressed}");
        }
    }
}

fn joined_acp_history_bytes(entries: &[String]) -> usize {
    const HEADER: &str = "# Conversation History\n";
    let body: usize = entries.iter().map(String::len).sum();
    let seps = entries.len().saturating_sub(1) * 2;
    HEADER.len() + body + seps
}

fn assemble_omitted_acp_history(
    entries: &[String],
    pinned_idx: Option<usize>,
    max_bytes: usize,
) -> String {
    const HEADER: &str = "# Conversation History\n";
    const OMITTED: &str = "...(earlier messages omitted)...";
    let mut keep = vec![false; entries.len()];
    let mut used = HEADER.len() + OMITTED.len() + 4;
    if let Some(pin) = pinned_idx
        && let Some(entry) = entries.get(pin)
    {
        keep[pin] = true;
        used += entry.len() + 2;
    }
    for idx in (0..entries.len()).rev() {
        if keep[idx] {
            continue;
        }
        let needed = entries[idx].len() + 2;
        if used + needed <= max_bytes {
            keep[idx] = true;
            used += needed;
        } else {
            break;
        }
    }
    if !keep.iter().any(|&k| k)
        && let Some(last) = entries.last()
    {
        let budget = max_bytes.saturating_sub(HEADER.len() + OMITTED.len() + 4);
        let fallback = truncate_utf8_prefix(last, budget);
        return format!("{HEADER}{OMITTED}\n\n{fallback}");
    }
    let mut blocks = Vec::new();
    let mut in_omitted = false;
    for (idx, entry) in entries.iter().enumerate() {
        if keep[idx] {
            in_omitted = false;
            blocks.push(entry.as_str());
        } else if !in_omitted {
            in_omitted = true;
            blocks.push(OMITTED);
        }
    }
    format!("{HEADER}{}", blocks.join("\n\n"))
}

fn format_acp_conversation_history(history: &[AiChatMessage], max_bytes: usize) -> String {
    if history.is_empty() {
        return String::new();
    }
    let pinned_idx = history.iter().rposition(|m| {
        m.content
            .trim_start()
            .starts_with(ORIGINAL_INSTRUCTION_MARKER)
    });
    let mut entries: Vec<String> = history
        .iter()
        .enumerate()
        .map(|(idx, msg)| {
            let role = role_label(msg.role);
            if pinned_idx == Some(idx) && msg.content.len() > PINNED_INSTRUCTION_MAX_BYTES {
                format!(
                    "[{role}]\n{}...(truncated)",
                    truncate_utf8_prefix(&msg.content, PINNED_INSTRUCTION_MAX_BYTES)
                )
            } else {
                format!("[{role}]\n{}", msg.content)
            }
        })
        .collect();

    if joined_acp_history_bytes(&entries) > max_bytes {
        compress_acp_history_entries(history, &mut entries, pinned_idx, 24 * 1024, 16 * 1024);
    }
    if joined_acp_history_bytes(&entries) > max_bytes {
        compress_acp_history_entries(history, &mut entries, pinned_idx, 8 * 1024, 4 * 1024);
    }
    if joined_acp_history_bytes(&entries) <= max_bytes {
        return format!("# Conversation History\n{}", entries.join("\n\n"));
    }
    assemble_omitted_acp_history(&entries, pinned_idx, max_bytes)
}

fn build_direct_acp_prompt_body(request: &AiRequest) -> String {
    let mut sections = vec![request.system_prompt.clone()];
    if !is_empty_json_value(&request.command_schema_json) {
        sections.push(format!(
            "# EditCommand JSON Schema\n{}",
            request.command_schema_json
        ));
    }
    if !is_empty_json_value(&request.context_summary) {
        sections.push(format!(
            "# Current Timeline Context\n{}",
            request.context_summary
        ));
    }
    let history_section =
        format_acp_conversation_history(&request.history, MAX_HISTORY_PROMPT_BYTES);
    if !history_section.is_empty() {
        sections.push(history_section);
    }
    sections.push(format!("# Instruction\n{}", request.user_prompt));
    sections.join("\n\n")
}

fn validate_manifest(m: &AiPluginManifest) -> Result<(), AiPluginHostError> {
    if m.schema_version != 1 {
        return Err(AiPluginHostError::InvalidManifest(format!(
            "unsupported schema_version {} (expected 1)",
            m.schema_version
        )));
    }
    if m.id.trim().is_empty() || m.name.trim().is_empty() || m.version.trim().is_empty() {
        return Err(AiPluginHostError::InvalidManifest(
            "id, name, and version must be non-empty".to_owned(),
        ));
    }
    if is_unsafe_relative_name(&m.entrypoint) {
        return Err(AiPluginHostError::InvalidManifest(format!(
            "unsafe entrypoint path `{}`",
            m.entrypoint
        )));
    }
    Ok(())
}

fn is_unsafe_relative_name(name: &str) -> bool {
    let trimmed = name.trim();
    trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || Path::new(trimmed).is_absolute()
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in paths {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn create(label: &str) -> Result<Self, std::io::Error> {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("sea_mc_ai_host_{label}_{}_{n}", std::process::id()));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn manifest_discovery_discovers_valid_and_rejects_traversal()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDirGuard::create("manifest")?;
        let good = dir.0.join("good_agent");
        let bad = dir.0.join("bad_agent");
        fs::create_dir_all(&good)?;
        fs::create_dir_all(&bad)?;

        fs::write(
            good.join("ai-plugin.json"),
            r#"{"schema_version":1,"id":"test.ai.good","name":"Good","version":"0.1.0","entrypoint":"good-bin"}"#,
        )?;
        fs::write(
            bad.join("ai-plugin.json"),
            r#"{"schema_version":1,"id":"test.ai.bad","name":"Bad","version":"0.1.0","entrypoint":"../escape"}"#,
        )?;

        let reg = AiPluginRegistry::discover_in_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(reg.plugins().len(), 1);
        assert_eq!(reg.plugins()[0].manifest.id, "test.ai.good");
        assert_eq!(reg.warnings().len(), 1);
        assert!(matches!(
            reg.resolve_provider("test.ai.good"),
            Err(AiPluginHostError::MissingExecutable { .. })
        ));
        Ok(())
    }

    #[test]
    fn resolves_ai_executable_inside_plugin_subdir() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDirGuard::create("plugin-subdir")?;
        let ai_dir = dir.0.join("acp");
        fs::create_dir_all(&ai_dir)?;
        let exe_name = if std::env::consts::EXE_SUFFIX.is_empty() {
            "custom-ai-bin".to_owned()
        } else {
            format!("custom-ai-bin{}", std::env::consts::EXE_SUFFIX)
        };
        let exe_path = ai_dir.join(&exe_name);
        fs::write(&exe_path, b"stub")?;
        fs::write(
            ai_dir.join("ai-plugin.json"),
            r#"{"schema_version":1,"id":"test.ai.local","name":"Local AI","version":"0.1.0","entrypoint":"custom-ai-bin"}"#,
        )?;

        let reg = AiPluginRegistry::discover_in_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(
            reg.find("test.ai.local")
                .and_then(|p| p.executable_path.as_deref()),
            Some(exe_path.as_path())
        );
        let _provider = reg.resolve_provider("test.ai.local")?;
        Ok(())
    }

    #[test]
    fn sea_ai_v1_models_event_and_completed_usage_deserialize_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let list_req_json = r#"{"protocol":"sea_ai_v1","method":"list_models","params":{}}"#;
        let env: AiPluginRequestEnvelope = serde_json::from_str(list_req_json)?;
        assert_eq!(env.method, "list_models");
        assert_eq!(env.params.user_prompt, "");

        let models_line = r#"{"event":"models","models":[{"id":"gemini-4-argon","display_name":"Gemini 4 Argon","thinking_levels":["low","medium","high"],"default_thinking":"medium","available":true,"note":"ACP利用可能"}]}"#;
        let parsed_models: AiPluginHostEvent = serde_json::from_str(models_line)?;
        let AiPluginHostEvent::Models { models } = parsed_models else {
            return Err("expected models event".into());
        };
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-4-argon");
        assert!(models[0].available);
        assert_eq!(
            models[0].to_probed_entry().note.as_deref(),
            Some("ACP利用可能")
        );

        let completed_line = r#"{"event":"completed","raw_text":"{}","model":"antigravity:gemini-4-argon","usage":{"input_tokens":120,"output_tokens":45,"total_tokens":165,"is_estimated":false}}"#;
        let parsed_done: AiPluginHostEvent = serde_json::from_str(completed_line)?;
        let AiPluginHostEvent::Completed { usage, .. } = parsed_done else {
            return Err("expected completed event".into());
        };
        let u = usage.ok_or("expected usage payload")?;
        assert_eq!(u.total_tokens, 165);
        assert!(!u.is_estimated);
        Ok(())
    }

    #[test]
    fn direct_acp_prompt_expands_history_pins_instruction_and_omits_empty_sections() {
        let req = AiRequest {
            system_prompt: "SYS".to_owned(),
            command_schema_json: json!({}),
            context_summary: Value::Null,
            history: vec![
                AiChatMessage {
                    role: AiRole::User,
                    content: format!("# Session Context\n{}", "あ".repeat(40_000)),
                },
                AiChatMessage {
                    role: AiRole::User,
                    content: format!("{ORIGINAL_INSTRUCTION_MARKER}\n字幕色を白に変更"),
                },
                AiChatMessage {
                    role: AiRole::User,
                    content: format!("[ToolResult old status=OK]\n{}", "い".repeat(30_000)),
                },
            ],
            user_prompt: "[ToolResult latest status=OK]\n{\"ok\":true}".to_owned(),
            stream: true,
        };
        let prompt = build_direct_acp_prompt_body(&req);
        assert!(!prompt.contains("# EditCommand JSON Schema"));
        assert!(!prompt.contains("# Current Timeline Context"));
        assert!(prompt.contains("# Conversation History"));
        assert!(prompt.contains("字幕色を白に変更"));
        assert!(prompt.contains("# Instruction\n[ToolResult latest"));
    }
}
