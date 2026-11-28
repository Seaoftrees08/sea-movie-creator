//! 動的自己記述ツールレジストリ (`HostToolRegistry`) および軽量 `DraftSandbox` 実行環境。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ai::blueprint::{MovieBlueprint, compile_blueprint_to_edit_command};
use crate::ai::capabilities::{InstalledExtensionSummary, collect_host_capabilities};
use crate::ai::context_builder::build_timeline_context;
use crate::ai::media_inspector::{
    AllowedFolderScope, ContactSheetInput, ContactSheetOptions, ThumbnailEncodeOptions,
    build_contact_sheet_jpeg_base64_skipping_errors, encode_thumbnail_jpeg_base64_with_options,
    inspect_media_folder,
};
use crate::ai::planner::dry_run_validate_command;
use crate::ai::provider::{
    AgentRuntimeSettings, AiAgentTurnRequest, AiContentPart, AiError, AiProvider, AiTokenUsage,
    AiToolDefinition, AiTurnMessage, VisionPackingMode,
};
use crate::ai::vision_audit::execute_audit_timeline_slides;
use crate::core::time::FrameIndex;
use crate::timeline::command::EditCommand;
use crate::timeline::dispatcher::CommandDispatcher;
use crate::timeline::document::ProjectDocument;

const MAX_VISION_INPUT_PATHS: usize = 100;

/// ツールが引き起こす副作用と承認方針の安全分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSafetyClass {
    /// 読み取り・解析・検証のみ（確認不要で自律実行可）。
    ReadOnly,
    /// 軽量 `DraftSandbox` 上のタイムライン編集（本番非汚染・最後に差分カードで承認）。
    DraftMutation,
    /// 外部ファイル生成・書き出し等の外部副作用を伴う操作（`Manual` 時は事前承認）。
    ExternalSideEffect,
}

/// エージェントおよび LLM へ公開する自己記述ツール仕様。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolDescriptor {
    /// 一意なツール識別名。
    pub name: String,
    /// 機能カテゴリ（`"inspection"`, `"vision"`, `"composition"`, `"timeline"`, `"extension"` 等）。
    pub category: String,
    /// LLM が用途と引数を判断するための詳細説明文。
    pub description: String,
    /// 安全クラス分類。
    pub safety_class: ToolSafetyClass,
    /// 実行中に進捗テキストを通知するかどうか。
    pub supports_progress: bool,
    /// 引数の JSON Schema。
    pub input_schema: Value,
}

impl AgentToolDescriptor {
    /// LLM のツール呼び出し定義型 (`AiToolDefinition`) へ変換する。
    #[must_use]
    pub fn to_llm_tool_definition(&self) -> AiToolDefinition {
        AiToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters_schema: self.input_schema.clone(),
        }
    }
}

/// 本番 `ProjectDocument` を汚さずに複数ステップの編集を積み上げて検証する軽量サンドボックス。
#[derive(Debug, Clone)]
pub struct DraftSandbox {
    base_doc: ProjectDocument,
    draft_doc: ProjectDocument,
    dispatcher: CommandDispatcher,
    accumulated_commands: Vec<EditCommand>,
}

impl DraftSandbox {
    /// 現在のライブドキュメントを基点に新しい [`DraftSandbox`] を構築する。
    #[must_use]
    pub fn new(base_doc: ProjectDocument) -> Self {
        let draft_doc = base_doc.clone();
        Self {
            base_doc,
            draft_doc,
            dispatcher: CommandDispatcher::new(),
            accumulated_commands: Vec::new(),
        }
    }

    /// ライブドキュメントへの参照を返す。
    #[must_use]
    pub const fn base_document(&self) -> &ProjectDocument {
        &self.base_doc
    }

    /// 現在のドラフト状態ドキュメントへの参照を返す。
    #[must_use]
    pub const fn draft_document(&self) -> &ProjectDocument {
        &self.draft_doc
    }

    /// ドラフト状態を現在のライブドキュメント状態へリセットし、蓄積コマンドも破棄する。
    pub fn reset(&mut self) {
        self.draft_doc = self.base_doc.clone();
        self.dispatcher.clear_history();
        self.accumulated_commands.clear();
    }

    /// 外部でライブドキュメントが更新された場合（承認適用後など）に新しい基点へ同期リセットする。
    pub fn rebase_to_live(&mut self, live_doc: ProjectDocument) {
        self.base_doc = live_doc.clone();
        self.draft_doc = live_doc;
        self.dispatcher.clear_history();
        self.accumulated_commands.clear();
    }

    /// 編集コマンドをドラフトへ適用し、整合性が取れた場合のみ蓄積リストへ記録する。
    ///
    /// # Errors
    ///
    /// 検証・実行エラー時は自動でロールバックし、エラーメッセージを返す。
    pub fn apply_command(&mut self, cmd: EditCommand) -> Result<(), String> {
        self.dispatcher
            .dispatch(&mut self.draft_doc, &cmd)
            .map_err(|e| e.to_string())?;
        flatten_append_commands(&mut self.accumulated_commands, cmd);
        Ok(())
    }

    /// 蓄積されたすべての変更を単一 Undo 単位の [`EditCommand::Batch`] として返す。
    #[must_use]
    pub fn to_batch_command(&self) -> EditCommand {
        EditCommand::Batch {
            commands: self.accumulated_commands.clone(),
        }
    }

    /// 蓄積されているコマンド数を返す。
    #[must_use]
    pub fn pending_command_count(&self) -> usize {
        self.accumulated_commands.len()
    }
}

fn flatten_append_commands(out: &mut Vec<EditCommand>, cmd: EditCommand) {
    match cmd {
        EditCommand::Batch { commands } => {
            for inner in commands {
                flatten_append_commands(out, inner);
            }
        }
        other => out.push(other),
    }
}

/// ツール実行時にホストから渡されるコンテキスト（許可フォルダ境界・Draft・LLM・レート制御）。
pub struct ToolExecutionContext<'a> {
    /// ユーザーが許可・添付したフォルダ範囲。
    pub folder_scope: &'a AllowedFolderScope,
    /// 現在の軽量編集サンドボックス。
    pub sandbox: &'a mut DraftSandbox,
    /// Vision 解析ツール等が利用する選択中 LLM プロバイダ（任意）。
    pub llm_provider: Option<&'a dyn AiProvider>,
    /// ランタイム設定（スロットリング・バッチ上限等）。
    pub runtime_settings: &'a AgentRuntimeSettings,
    /// 前回 LLM 呼び出し完了時刻への可変参照（`min_llm_interval_ms` 制御用）。
    pub last_llm_call_at: &'a mut Option<Instant>,
    /// 中断・キャンセル要求フラグ（任意）。
    pub cancelled: Option<&'a Arc<AtomicBool>>,
    /// 進捗通知コールバック。
    pub on_progress: &'a mut dyn FnMut(&str),
    /// ツール内部で行われた LLM 呼び出しのトークン使用量通知コールバック（任意）。
    pub on_usage: Option<&'a mut dyn FnMut(AiTokenUsage)>,
    /// 登録済み外部拡張を自己申告一覧に含めるための追加リスト（任意）。
    pub extra_extensions: Vec<InstalledExtensionSummary>,
    /// 現在開いているプロジェクトファイルのあるディレクトリ（相対アセット解決用）。
    pub project_dir: Option<&'a Path>,
}

impl ToolExecutionContext<'_> {
    /// キャンセル要求が立っているか確認する。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// 設定された `min_llm_interval_ms` を満たすよう必要に応じて待機し、最終呼出時刻を更新する。
    ///
    /// # Errors
    ///
    /// 待機中にキャンセルが要求された場合はエラーを返す。
    pub fn throttle_before_llm_call(&mut self) -> Result<(), String> {
        let min_ms = self.runtime_settings.min_llm_interval_ms;
        if min_ms > 0
            && let Some(prev) = *self.last_llm_call_at
        {
            let target = Duration::from_millis(min_ms);
            while let Some(remaining) = target.checked_sub(prev.elapsed()) {
                if self.is_cancelled() {
                    return Err("tool execution cancelled".to_owned());
                }
                std::thread::sleep(remaining.min(Duration::from_millis(25)));
            }
        }
        if self.is_cancelled() {
            return Err("tool execution cancelled".to_owned());
        }
        *self.last_llm_call_at = Some(Instant::now());
        Ok(())
    }
}

/// ホスト上で実行可能な拡張ツールの統一トレイト。
pub trait AgentTool: Send + Sync {
    /// 自己記述メタデータと引数 JSON Schema を返す。
    fn descriptor(&self) -> AgentToolDescriptor;

    /// ツールを実行し、JSON 形式の結果を返す。
    ///
    /// # Errors
    ///
    /// 引数不正・アクセス境界違反・処理失敗時にエラー説明文字列を返す。
    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String>;
}

/// 本体の標準ツールおよび将来拡張ツールを動的管理するレジストリ。
#[derive(Default)]
pub struct HostToolRegistry {
    tools: Vec<(AgentToolDescriptor, Box<dyn AgentTool>)>,
}

impl std::fmt::Debug for HostToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.tools.iter().map(|(d, _)| d.name.as_str()).collect();
        f.debug_struct("HostToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

impl HostToolRegistry {
    /// 空のレジストリを作成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// すべての標準ホストツールを登録したレジストリを構築する。
    #[must_use]
    pub fn with_builtin_tools() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(GetCapabilitiesTool));
        reg.register(Box::new(GetTimelineSummaryTool));
        reg.register(Box::new(InspectMediaFolderTool));
        reg.register(Box::new(InspectImagesVisionTool));
        reg.register(Box::new(AuditTimelineSlidesTool));
        reg.register(Box::new(CompileMovieBlueprintTool));
        reg.register(Box::new(ApplyDraftEditCommandTool));
        reg.register(Box::new(ResetDraftToLiveTool));
        reg.register(Box::new(ValidateDraftPlanTool));
        reg
    }

    /// 新しいツールを登録する（同名ツールが存在する場合は上書き置換）。
    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
        let desc = tool.descriptor();
        if let Some(pos) = self.tools.iter().position(|(d, _)| d.name == desc.name) {
            self.tools[pos] = (desc, tool);
        } else {
            self.tools.push((desc, tool));
        }
    }

    /// 登録済み全ツールの自己記述仕様一覧を返す。
    #[must_use]
    pub fn list_descriptors(&self) -> Vec<AgentToolDescriptor> {
        self.tools.iter().map(|(d, _)| d.clone()).collect()
    }

    /// 指定名ツールの自己記述仕様を検索する。
    #[must_use]
    pub fn find_descriptor(&self, name: &str) -> Option<AgentToolDescriptor> {
        self.tools
            .iter()
            .find(|(d, _)| d.name == name)
            .map(|(d, _)| d.clone())
    }

    /// 指定ツールを実行して JSON 結果を返す。
    ///
    /// # Errors
    ///
    /// 指定名ツールが未登録、またはツール実行自体が失敗した場合。
    pub fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &mut ToolExecutionContext<'_>,
    ) -> Result<Value, String> {
        let (_, tool) = self
            .tools
            .iter()
            .find(|(d, _)| d.name == name)
            .ok_or_else(|| format!("unknown host tool `{name}`"))?;
        tool.execute(args, ctx)
    }
}

struct GetCapabilitiesTool;

impl AgentTool for GetCapabilitiesTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "get_capabilities".to_owned(),
            category: "discovery".to_owned(),
            description: "本体がサポートする EditCommand・パラメータ・利用可能フォント・導入済みプラグイン状態を返します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "include_schemas": { "type": "boolean", "description": "true で EditCommand / MovieBlueprint の完全 JSON Schema も同梱" }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let include_schemas = args["include_schemas"].as_bool().unwrap_or(false);
        let mut snap = collect_host_capabilities(include_schemas);
        for ext in &ctx.extra_extensions {
            if !snap.installed_extensions.iter().any(|e| e.id == ext.id) {
                snap.installed_extensions.push(ext.clone());
            }
        }
        serde_json::to_value(snap).map_err(|e| e.to_string())
    }
}

struct GetTimelineSummaryTool;

impl AgentTool for GetTimelineSummaryTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "get_timeline_summary".to_owned(),
            category: "timeline".to_owned(),
            description: "現在タイムライン（既定では編集中の Draft、指定時は Live）のトラック・クリップ・素材要約を返します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "enum": ["draft", "live"] },
                    "playhead_frame": { "type": "integer", "minimum": 0 }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let playhead = FrameIndex::new(args["playhead_frame"].as_u64().unwrap_or(0));
        let doc = if args["source"].as_str() == Some("live") {
            ctx.sandbox.base_document()
        } else {
            ctx.sandbox.draft_document()
        };
        let summary = build_timeline_context(doc, playhead, None, None);
        serde_json::to_value(summary).map_err(|e| e.to_string())
    }
}

struct InspectMediaFolderTool;

impl AgentTool for InspectMediaFolderTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "inspect_media_folder".to_owned(),
            category: "inspection".to_owned(),
            description: "許可フォルダ内の画像（解像度・縦横向き・EXIF撮影日時）・音声（尺）・動画・テキストメモを走査します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: true,
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "走査対象フォルダパス" },
                    "recursive": { "type": "boolean", "default": true }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let path_str = args["path"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "inspect_media_folder requires non-empty `path`".to_owned())?;
        let recursive = args["recursive"].as_bool().unwrap_or(true);
        (ctx.on_progress)(&format!("フォルダ内素材を走査中: {path_str}"));
        let res = inspect_media_folder(ctx.folder_scope, std::path::Path::new(path_str), recursive)
            .map_err(|e| e.to_string())?;
        serde_json::to_value(res).map_err(|e| e.to_string())
    }
}

const VISION_UNSUPPORTED_WARNING: &str =
    "⚠ Vision 非対応のためファイル名・EXIF 情報のみで字幕を生成しました";

struct AuditTimelineSlidesTool;

impl AgentTool for AuditTimelineSlidesTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "audit_timeline_slides".to_owned(),
            category: "vision".to_owned(),
            description: "タイムライン上の画像スライドと重なっている全字幕を、実画像（現行回転適用済みコンタクトシート＋低信頼時の単体再確認）でクロス監査し、内容不一致と回転補正＋スケール再フィット案（および `companion_backdrop_updates` の背景クリップ `RotationDeg`/`ScaleX`/`ScaleY` 更新案）を返します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: true,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "slide_indices": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 1 },
                        "description": "監査する 1-based スライド番号一覧（省略時は全スライド）"
                    },
                    "check_subtitle_mismatch": { "type": "boolean", "default": true },
                    "check_orientation": { "type": "boolean", "default": true },
                    "focus_hint": { "type": "string", "description": "ユーザーが特に気にしている点" }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        execute_audit_timeline_slides(args, ctx)
    }
}

struct InspectImagesVisionTool;

impl AgentTool for InspectImagesVisionTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "inspect_images_vision".to_owned(),
            category: "vision".to_owned(),
            description: "画像パス一覧を軽量縮小／コンタクトシート圧縮し、Vision 対応 LLM で情景説明・章候補・適切な日本語字幕案をバッチ生成します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: true,
            input_schema: json!({
                "type": "object",
                "required": ["paths"],
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "theme_hint": { "type": "string", "description": "動画テーマ（例: 結婚式プロフィールムービー）" },
                    "language": { "type": "string", "default": "ja" }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let paths: Vec<String> = args["paths"]
            .as_array()
            .ok_or_else(|| "inspect_images_vision requires `paths` array".to_owned())?
            .iter()
            .filter_map(|v| v.as_str().map(ToOwned::to_owned))
            .collect();
        if paths.len() > MAX_VISION_INPUT_PATHS {
            return Err(format!(
                "inspect_images_vision paths limit exceeded ({}; max {MAX_VISION_INPUT_PATHS})",
                paths.len()
            ));
        }
        if paths.is_empty() {
            return Ok(json!({ "analyses": [], "used_vision": false }));
        }

        let theme_hint = args["theme_hint"]
            .as_str()
            .unwrap_or("思い出フォトムービー");
        let language = args["language"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("ja");
        let Some(provider) = ctx.llm_provider.filter(|p| p.supports_vision()) else {
            // Vision 非対応時はファイル名ベースの安全フォールバックを返し、UI/ユーザー向け警告を添える
            let fallback: Vec<Value> = paths
                .iter()
                .map(|p| build_filename_fallback_analysis(p))
                .collect();
            return Ok(json!({
                "analyses": fallback,
                "used_vision": false,
                "warning": VISION_UNSUPPORTED_WARNING,
                "note": "selected provider does not support Vision; generated metadata-only fallback",
            }));
        };

        execute_vision_batches(provider, &paths, theme_hint, language, ctx)
    }
}

struct VisionBatchConfig<'a> {
    packing_mode: VisionPackingMode,
    max_edge: u32,
    jpeg_quality: u8,
    sheet_opts: ContactSheetOptions,
    batch_size: usize,
    theme_hint: &'a str,
    language: &'a str,
}

fn build_vision_batch_config<'a>(
    settings: &AgentRuntimeSettings,
    theme_hint: &'a str,
    language: &'a str,
) -> VisionBatchConfig<'a> {
    let s = settings.sanitized();
    let packing_mode = s.vision_packing_mode;
    let cols = u32::from(s.vision_contact_sheet_cols);
    let rows = u32::from(s.vision_contact_sheet_rows);
    let batch_size = match packing_mode {
        VisionPackingMode::ContactSheet => usize::try_from(cols * rows).unwrap_or(6).max(1),
        VisionPackingMode::Individual => s.vision_batch_size,
    };
    VisionBatchConfig {
        packing_mode,
        max_edge: s.vision_max_edge_px,
        jpeg_quality: s.vision_jpeg_quality,
        sheet_opts: ContactSheetOptions {
            cols,
            rows,
            tile_max_edge_px: s.vision_contact_sheet_tile_px,
            jpeg_quality: s.vision_jpeg_quality,
        },
        batch_size,
        theme_hint,
        language,
    }
}

fn execute_vision_batches(
    provider: &dyn AiProvider,
    paths: &[String],
    theme_hint: &str,
    language: &str,
    ctx: &mut ToolExecutionContext<'_>,
) -> Result<Value, String> {
    let cfg = build_vision_batch_config(ctx.runtime_settings, theme_hint, language);
    let mut all_analyses = Vec::with_capacity(paths.len());
    let total = paths.len();
    let mut used_vision_ok = true;
    let mut fallback_note: Option<String> = None;
    let mut vision_warning: Option<&'static str> = None;

    for (chunk_idx, chunk) in paths.chunks(cfg.batch_size).enumerate() {
        let done_so_far = (chunk_idx * cfg.batch_size).min(total);
        if ctx.is_cancelled() {
            return Ok(partial_vision_json(
                &all_analyses,
                used_vision_ok && !all_analyses.is_empty(),
                total,
                "cancelled",
                "vision analysis cancelled by user",
            ));
        }
        (ctx.on_progress)(&format!(
            "写真を Vision 解析中 ({done_so_far}/{total} 枚)..."
        ));
        if let Err(cancel_msg) = ctx.throttle_before_llm_call() {
            return Ok(partial_vision_json(
                &all_analyses,
                used_vision_ok && !all_analyses.is_empty(),
                total,
                "cancelled",
                &cancel_msg,
            ));
        }

        let (outcome, batch_usage) = match cfg.packing_mode {
            VisionPackingMode::ContactSheet => run_single_contact_sheet_vision_batch(
                provider,
                ctx.folder_scope,
                chunk,
                &cfg,
                ctx.cancelled,
            ),
            VisionPackingMode::Individual => {
                run_single_vision_batch(provider, ctx.folder_scope, chunk, &cfg, ctx.cancelled)
            }
        };
        if let (Some(cb), Some(usage)) = (ctx.on_usage.as_deref_mut(), batch_usage) {
            cb(usage);
        }
        match outcome {
            VisionBatchOutcome::Parsed(items) => all_analyses.extend(items),
            VisionBatchOutcome::ParseFallback(items) => {
                used_vision_ok = false;
                fallback_note.get_or_insert_with(|| "vision JSON parse fallback used".to_owned());
                all_analyses.extend(items);
            }
            VisionBatchOutcome::VisionUnsupported(reason) => {
                used_vision_ok = false;
                fallback_note = Some(reason);
                vision_warning = Some(VISION_UNSUPPORTED_WARNING);
                for remaining_path in &paths[done_so_far..] {
                    all_analyses.push(build_filename_fallback_analysis(remaining_path));
                }
                break;
            }
            VisionBatchOutcome::Failed { code, message } => {
                if !all_analyses.is_empty()
                    || matches!(code, "rate_limited" | "forbidden" | "cancelled")
                {
                    return Ok(partial_vision_json(
                        &all_analyses,
                        used_vision_ok && !all_analyses.is_empty(),
                        total,
                        code,
                        &message,
                    ));
                }
                return Err(message);
            }
        }
    }

    let mut out = json!({
        "analyses": all_analyses,
        "used_vision": used_vision_ok,
    });
    if let Some(warn) = vision_warning {
        out["warning"] = Value::String(warn.to_owned());
    }
    if let Some(note) = fallback_note {
        out["note"] = Value::String(note);
    }
    Ok(out)
}

fn partial_vision_json(
    analyses: &[Value],
    used_vision: bool,
    total_count: usize,
    error_code: &str,
    error: &str,
) -> Value {
    json!({
        "analyses": analyses,
        "used_vision": used_vision,
        "partial": true,
        "processed_count": analyses.len(),
        "total_count": total_count,
        "error_code": error_code,
        "error": error,
    })
}

enum VisionBatchOutcome {
    Parsed(Vec<Value>),
    ParseFallback(Vec<Value>),
    VisionUnsupported(String),
    Failed { code: &'static str, message: String },
}

fn is_vision_unsupported_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if [
        "vision_unsupported",
        "does not support vision",
        "vision not supported",
        "vision is not supported",
        "does not support image",
        "image input not supported",
        "multimodal not supported",
        "not a vision",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        return true;
    }
    let mentions_vision_media = [
        "image",
        "vision",
        "multimodal",
        "image_url",
        "inline_data",
        "media_type",
    ]
    .iter()
    .any(|kw| lower.contains(kw));
    mentions_vision_media
        && ["does not support", "not supported", "unknown variant"]
            .iter()
            .any(|kw| lower.contains(kw))
}

pub(crate) fn classify_vision_api_error(err: &AiError) -> (&'static str, bool, String) {
    let msg = err.to_string();
    match err {
        AiError::Cancelled => ("cancelled", false, msg),
        AiError::HttpStatus { status: 429, .. } => ("rate_limited", false, msg),
        AiError::HttpStatus { status: 403, .. } => ("forbidden", false, msg),
        AiError::HttpStatus {
            status: 400 | 404 | 422,
            body,
        }
        | AiError::PluginError(body) => {
            if is_vision_unsupported_text(body) {
                ("vision_unsupported", true, msg)
            } else {
                ("llm_error", false, msg)
            }
        }
        _ => ("llm_error", false, msg),
    }
}

fn describe_target_language(code: &str) -> String {
    match code.trim().to_ascii_lowercase().as_str() {
        "ja" | "jpn" | "japanese" => "natural Japanese".to_owned(),
        "en" | "eng" | "english" => "natural English".to_owned(),
        other => other.chars().take(24).collect(),
    }
}

fn merge_failed_vision_items(
    outcome: VisionBatchOutcome,
    failed_items: Vec<Value>,
) -> VisionBatchOutcome {
    if failed_items.is_empty() {
        return outcome;
    }
    match outcome {
        VisionBatchOutcome::Parsed(mut items) => {
            items.extend(failed_items);
            VisionBatchOutcome::Parsed(items)
        }
        VisionBatchOutcome::ParseFallback(mut items) => {
            items.extend(failed_items);
            VisionBatchOutcome::ParseFallback(items)
        }
        other => other,
    }
}

fn build_failed_vision_item(path: &str, err: &dyn std::fmt::Display) -> Value {
    let mut item = build_filename_fallback_analysis(path);
    item["media_error"] = json!(err.to_string());
    item
}

fn run_single_contact_sheet_vision_batch(
    provider: &dyn AiProvider,
    scope: &AllowedFolderScope,
    paths: &[String],
    cfg: &VisionBatchConfig<'_>,
    cancelled: Option<&Arc<AtomicBool>>,
) -> (VisionBatchOutcome, Option<AiTokenUsage>) {
    let inputs: Vec<ContactSheetInput> = paths
        .iter()
        .enumerate()
        .map(|(idx, p)| ContactSheetInput {
            path: std::path::PathBuf::from(p),
            badge_index: idx + 1,
            extra_rotation_deg: 0,
        })
        .collect();
    let res =
        match build_contact_sheet_jpeg_base64_skipping_errors(scope, &inputs, cfg.sheet_opts, true)
        {
            Ok(r) => r,
            Err(err) => {
                return (
                    VisionBatchOutcome::Failed {
                        code: "media_error",
                        message: err.to_string(),
                    },
                    None,
                );
            }
        };
    let failed_items: Vec<Value> = res
        .skipped
        .iter()
        .filter_map(|f| {
            paths
                .get(f.input_index)
                .map(|p| build_failed_vision_item(p, &f.reason))
        })
        .collect();
    let manifest_lines: Vec<String> = res
        .accepted_input_indices
        .iter()
        .enumerate()
        .map(|(idx, &orig_idx)| {
            let badge = idx + 1;
            let fname = paths
                .get(orig_idx)
                .and_then(|p| Path::new(p).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("photo");
            format!("[index={idx}, badge=#{badge}] file={fname}")
        })
        .collect();
    let sheet = res.sheet;
    let lang_desc = describe_target_language(cfg.language);
    let prompt = format!(
        "Analyze every tile in the attached contact sheet for the video theme: \"{}\".\n\
         Each tile has a yellow badge `#1`, `#2`, ... corresponding to `index = badge - 1` below.\n\
         Return ONLY a valid JSON object:\n\
         {{\"analyses\":[{{\"index\":0,\"path\":\"...\",\"scene_description\":\"...\",\"suggested_chapter\":\"...\",\"suggested_subtitle\":\"...\",\"mood\":\"warm|joyful|nostalgic|calm\",\"recommended_motion\":\"zoom_in|zoom_out|pan_left|pan_right\"}}]}}\n\
         Keep `suggested_subtitle` warm, {lang_desc} (12-26 chars) matching what is actually visible in that tile.\n\
         Tiles:\n{}",
        cfg.theme_hint,
        manifest_lines.join("\n")
    );
    let parts = vec![
        AiContentPart::text(prompt),
        AiContentPart::ImageData {
            mime_type: sheet.mime_type,
            base64_data: sheet.base64_data,
        },
    ];
    let (outcome, usage) = execute_vision_analysis_turn(provider, parts, &sheet.paths, cancelled);
    (merge_failed_vision_items(outcome, failed_items), usage)
}

fn run_single_vision_batch(
    provider: &dyn AiProvider,
    scope: &AllowedFolderScope,
    paths: &[String],
    cfg: &VisionBatchConfig<'_>,
    cancelled: Option<&Arc<AtomicBool>>,
) -> (VisionBatchOutcome, Option<AiTokenUsage>) {
    let mut parts = Vec::new();
    let mut manifest_lines = Vec::new();
    let mut valid_paths = Vec::new();
    let mut failed_items = Vec::new();
    let mut first_err: Option<String> = None;

    for p in paths {
        let thumb = match encode_thumbnail_jpeg_base64_with_options(
            scope,
            std::path::Path::new(p),
            ThumbnailEncodeOptions {
                max_edge_px: cfg.max_edge,
                jpeg_quality: cfg.jpeg_quality,
                extra_rotation_deg: 0,
            },
        ) {
            Ok(t) => t,
            Err(err) => {
                if first_err.is_none() {
                    first_err = Some(err.to_string());
                }
                failed_items.push(build_failed_vision_item(p, &err));
                continue;
            }
        };
        let idx = valid_paths.len();
        let fname = std::path::Path::new(&thumb.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo");
        manifest_lines.push(format!("[{idx}] path={} file={fname}", thumb.path));
        parts.push(AiContentPart::text(format!("Image [{idx}]: {fname}")));
        parts.push(AiContentPart::ImageData {
            mime_type: thumb.mime_type,
            base64_data: thumb.base64_data,
        });
        valid_paths.push(thumb.path);
    }
    if valid_paths.is_empty() {
        return (
            VisionBatchOutcome::Failed {
                code: "media_error",
                message: first_err.unwrap_or_else(|| "no readable images in batch".to_owned()),
            },
            None,
        );
    }

    let lang_desc = describe_target_language(cfg.language);
    let prompt_head = format!(
        "Analyze the attached photos for the video theme: \"{}\".\n\
         Return ONLY a valid JSON object of the form:\n\
         {{\"analyses\":[{{\"index\":0,\"path\":\"...\",\"scene_description\":\"...\",\"suggested_chapter\":\"...\",\"suggested_subtitle\":\"...\",\"mood\":\"warm|joyful|nostalgic|calm\",\"recommended_motion\":\"zoom_in|zoom_out|pan_left|pan_right\"}}]}}\n\
         Keep `suggested_subtitle` warm, {lang_desc} (12-26 chars) matching what is actually visible in the photo.\n\
         Images:\n{}",
        cfg.theme_hint,
        manifest_lines.join("\n")
    );
    parts.insert(0, AiContentPart::text(prompt_head));
    let (outcome, usage) = execute_vision_analysis_turn(provider, parts, &valid_paths, cancelled);
    (merge_failed_vision_items(outcome, failed_items), usage)
}

fn execute_vision_analysis_turn(
    provider: &dyn AiProvider,
    parts: Vec<AiContentPart>,
    valid_paths: &[String],
    cancelled: Option<&Arc<AtomicBool>>,
) -> (VisionBatchOutcome, Option<AiTokenUsage>) {
    let req = AiAgentTurnRequest {
        system_prompt:
            "You are a sensitive photo curator for video storytelling. Always output strict JSON."
                .to_owned(),
        messages: vec![AiTurnMessage::User { parts }],
        tools: Vec::new(),
        response_json_schema: Some(json!({
            "type": "object",
            "required": ["analyses"],
            "properties": {
                "analyses": { "type": "array" }
            }
        })),
        temperature: Some(0.2),
    };

    let resp = match provider.send_agent_turn_cancellable(&req, cancelled, &mut |_| {}) {
        Ok(r) => r,
        Err(err) => {
            let (code, is_unsupported, message) = classify_vision_api_error(&err);
            let outcome = if is_unsupported {
                VisionBatchOutcome::VisionUnsupported(message)
            } else {
                VisionBatchOutcome::Failed { code, message }
            };
            return (outcome, None);
        }
    };
    let in_chars = req.estimated_input_chars();
    let out_chars = resp.estimated_output_chars();
    let usage = resp
        .usage
        .unwrap_or_else(|| AiTokenUsage::estimate_from_chars(in_chars, out_chars));
    let raw_text = resp.text.unwrap_or_default();
    if let Some(parsed) = parse_vision_analyses_json(&raw_text, valid_paths) {
        return (VisionBatchOutcome::Parsed(parsed), Some(usage));
    }
    let fallback = valid_paths
        .iter()
        .map(|p| build_filename_fallback_analysis(p))
        .collect();
    (VisionBatchOutcome::ParseFallback(fallback), Some(usage))
}

fn parse_vision_analyses_json(raw: &str, valid_paths: &[String]) -> Option<Vec<Value>> {
    let val = crate::ai::planner::extract_json_object_from_text(raw)?;
    let arr = val.get("analyses")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    // コンタクトシートの黄色バッジ `#1..#N` につられて LLM が 1-based で返した場合を自動吸収する
    let has_zero_index = arr.iter().any(|item| item["index"].as_u64() == Some(0));
    let max_one_based = u64::try_from(valid_paths.len()).unwrap_or(u64::MAX);
    let one_based_hint = !has_zero_index
        && arr.iter().any(|item| {
            item["index"]
                .as_u64()
                .is_some_and(|n| (1..=max_one_based).contains(&n))
        });
    let mut used = vec![false; arr.len()];
    let mut resolved: Vec<Option<usize>> = vec![None; valid_paths.len()];
    let mut matched_count = 0_usize;

    // 1 巡目: 明示的な index が一致する要素のみを先に割り当てる（後続要素のインデックス横取りを防止）
    for (idx, item_slot) in resolved.iter_mut().enumerate() {
        let expected_index = if one_based_hint {
            u64::try_from(idx + 1).ok()
        } else {
            u64::try_from(idx).ok()
        };
        if let Some(target) = expected_index
            && let Some(arr_idx) = arr.iter().enumerate().position(|(i, item)| {
                !used[i] && item.is_object() && item["index"].as_u64() == Some(target)
            })
        {
            used[arr_idx] = true;
            *item_slot = Some(arr_idx);
            matched_count += 1;
        }
    }

    // 2 巡目: index を欠落した要素のみ、未割当の同一順序スロットへフォールバックする
    for (idx, item_slot) in resolved.iter_mut().enumerate() {
        if item_slot.is_some() {
            continue;
        }
        let unindexed = arr
            .get(idx)
            .is_some_and(|item| item.is_object() && item.get("index").is_none_or(Value::is_null));
        if unindexed && !used.get(idx).copied().unwrap_or(true) {
            used[idx] = true;
            *item_slot = Some(idx);
            matched_count += 1;
        }
    }

    let out = valid_paths
        .iter()
        .enumerate()
        .map(|(idx, default_path)| {
            if let Some(arr_idx) = resolved[idx]
                && let Some(item_map) = arr.get(arr_idx).and_then(Value::as_object)
            {
                let mut obj_map = item_map.clone();
                // LLM がハルシネーションやインジェクションで別パスを返しても正規実パスで上書きする
                obj_map.insert("path".to_owned(), Value::String(default_path.clone()));
                obj_map.insert(
                    "index".to_owned(),
                    Value::Number(serde_json::Number::from(idx)),
                );
                Value::Object(obj_map)
            } else {
                build_filename_fallback_analysis(default_path)
            }
        })
        .collect();
    (matched_count > 0).then_some(out)
}

fn build_filename_fallback_analysis(path: &str) -> Value {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("思い出");
    json!({
        "path": path,
        "scene_description": format!("ファイル名 {stem} の写真"),
        "suggested_chapter": "思い出",
        "suggested_subtitle": format!("大切なひととき ({stem})"),
        "mood": "warm",
        "recommended_motion": "zoom_in",
    })
}

struct CompileMovieBlueprintTool;

impl AgentTool for CompileMovieBlueprintTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        static BLUEPRINT_SCHEMA: OnceLock<Value> = OnceLock::new();
        let blueprint_schema = BLUEPRINT_SCHEMA
            .get_or_init(|| {
                serde_json::to_value(schema_for!(MovieBlueprint))
                    .unwrap_or_else(|_| json!({ "type": "object" }))
            })
            .clone();
        AgentToolDescriptor {
            name: "compile_movie_blueprint".to_owned(),
            category: "composition".to_owned(),
            description: "MovieBlueprint（目標総尺・タイトル・スライド字幕・モーション・BGM）から厳密なタイムライン EditCommand::Batch を生成して DraftSandbox へ展開・検証します。".to_owned(),
            safety_class: ToolSafetyClass::DraftMutation,
            supports_progress: true,
            input_schema: json!({
                "type": "object",
                "required": ["blueprint"],
                "properties": {
                    "blueprint": blueprint_schema,
                    "replace_draft": { "type": "boolean", "default": true }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let bp_val = args
            .get("blueprint")
            .ok_or_else(|| "compile_movie_blueprint requires `blueprint`".to_owned())?;
        let blueprint: MovieBlueprint = serde_json::from_value(bp_val.clone())
            .map_err(|e| format!("invalid blueprint: {e}"))?;
        let replace_draft = args["replace_draft"].as_bool().unwrap_or(true);
        (ctx.on_progress)("構成プランをタイムラインへコンパイル中...");

        if replace_draft {
            ctx.sandbox.reset();
        }
        let compiled = compile_blueprint_to_edit_command(
            ctx.sandbox.draft_document(),
            &blueprint,
            Some(ctx.folder_scope),
        )
        .map_err(|e| e.to_string())?;

        ctx.sandbox.apply_command(compiled.command)?;
        Ok(json!({
            "compiled": true,
            "summary": compiled.summary,
            "pending_command_count": ctx.sandbox.pending_command_count(),
        }))
    }
}

struct ApplyDraftEditCommandTool;

impl AgentTool for ApplyDraftEditCommandTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "apply_draft_edit_command".to_owned(),
            category: "timeline".to_owned(),
            description: "任意の EditCommand（クリップ追加・移動・パラメータ変更・字幕修正など）を DraftSandbox へ適用し整合性検証します。".to_owned(),
            safety_class: ToolSafetyClass::DraftMutation,
            supports_progress: false,
            input_schema: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "object", "description": "EditCommand JSON" }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let cmd: EditCommand = serde_json::from_value(args["command"].clone())
            .map_err(|e| format!("invalid EditCommand: {e}"))?;
        ctx.sandbox.apply_command(cmd)?;
        Ok(json!({
            "applied_to_draft": true,
            "pending_command_count": ctx.sandbox.pending_command_count(),
            "draft_total_duration_frames": ctx.sandbox.draft_document().total_duration_frames(),
        }))
    }
}

struct ResetDraftToLiveTool;

impl AgentTool for ResetDraftToLiveTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "reset_draft_to_live".to_owned(),
            category: "timeline".to_owned(),
            description:
                "DraftSandbox の未適用変更を破棄し、現在のライブプロジェクト状態へ戻します。"
                    .to_owned(),
            safety_class: ToolSafetyClass::DraftMutation,
            supports_progress: false,
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn execute(&self, _args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        ctx.sandbox.reset();
        Ok(json!({
            "reset": true,
            "draft_total_duration_frames": ctx.sandbox.draft_document().total_duration_frames(),
        }))
    }
}

struct ValidateDraftPlanTool;

impl AgentTool for ValidateDraftPlanTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "validate_draft_plan".to_owned(),
            category: "timeline".to_owned(),
            description: "現在 DraftSandbox に蓄積された編集バッチがライブタイムライン上で安全に適用できるか dry_run_validate で検証し差分要約を返します。".to_owned(),
            safety_class: ToolSafetyClass::ReadOnly,
            supports_progress: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "include_command": {
                        "type": "boolean",
                        "default": false,
                        "description": "true の場合、Proposal 送信用に蓄積された EditCommand::Batch JSON (`command`) も返却する"
                    }
                }
            }),
        }
    }

    fn execute(&self, args: &Value, ctx: &mut ToolExecutionContext<'_>) -> Result<Value, String> {
        let include_command = args["include_command"].as_bool().unwrap_or(false);
        let batch = ctx.sandbox.to_batch_command();
        let command_json = if include_command {
            Some(serde_json::to_value(&batch).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let plan = dry_run_validate_command(ctx.sandbox.base_document(), batch, None)
            .map_err(|e| e.to_string())?;
        let mut resp = json!({
            "valid": true,
            "is_no_op": plan.is_no_op(),
            "summary_lines": plan.summary_lines,
            "affected_targets_count": plan.affected_targets.len(),
            "draft_total_duration_frames": ctx.sandbox.draft_document().total_duration_frames(),
        });
        if let Some(cmd_val) = command_json {
            resp["command"] = cmd_val;
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;
    use crate::ai::provider::AiAgentTurnResponse;
    use crate::ai::providers::mock::MockAiProvider;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("sea_toolbox_{label}_{}_{id}", std::process::id()));
            fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_png(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
        let mut img = RgbaImage::new(800, 600);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = Rgba([200, 180, 160, 255]);
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, ImageFormat::Png)?;
        fs::write(path, buf.into_inner())?;
        Ok(())
    }

    struct DummyCustomExtensionTool;

    impl AgentTool for DummyCustomExtensionTool {
        fn descriptor(&self) -> AgentToolDescriptor {
            AgentToolDescriptor {
                name: "future_silence_cut".to_owned(),
                category: "extension".to_owned(),
                description: "将来追加される無音カットツール".to_owned(),
                safety_class: ToolSafetyClass::DraftMutation,
                supports_progress: false,
                input_schema: json!({ "type": "object" }),
            }
        }

        fn execute(
            &self,
            _args: &Value,
            _ctx: &mut ToolExecutionContext<'_>,
        ) -> Result<Value, String> {
            Ok(json!({ "cut_segments": 3 }))
        }
    }

    fn sample_vision_mock_response() -> AiAgentTurnResponse {
        AiAgentTurnResponse {
            text: Some(
                json!({
                    "analyses": [{
                        "index": 0,
                        "scene_description": "仲良く並ぶ二人の写真",
                        "suggested_chapter": "ふたり",
                        "suggested_subtitle": "出会った頃の大切な思い出",
                        "mood": "warm",
                        "recommended_motion": "zoom_in"
                    }]
                })
                .to_string(),
            ),
            thinking_blocks: Vec::new(),
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
            usage: None,
        }
    }

    #[test]
    fn host_toolbox_supports_dynamic_registration_vision_and_draft_sandbox()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("toolbox")?;
        let p1 = guard.0.join("memory1.png");
        write_png(&p1)?;

        let mut registry = HostToolRegistry::with_builtin_tools();
        // 将来ツールの動的登録をテスト
        registry.register(Box::new(DummyCustomExtensionTool));
        assert!(registry.find_descriptor("future_silence_cut").is_some());

        let live_doc = ProjectDocument::new("ToolboxTest");
        let mut sandbox = DraftSandbox::new(live_doc.clone());
        let scope = AllowedFolderScope::from_folders([&guard.0]);
        let settings = AgentRuntimeSettings::default();
        let mock_vision = MockAiProvider::with_agent_turns([sample_vision_mock_response()]);

        let mut logs = Vec::new();
        let mut progress = |msg: &str| logs.push(msg.to_owned());
        let mut reported_usages = Vec::new();
        let mut on_usage = |usage| reported_usages.push(usage);
        let mut last_llm_call_at = None;
        let mut ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut sandbox,
            llm_provider: Some(&mock_vision),
            runtime_settings: &settings,
            last_llm_call_at: &mut last_llm_call_at,
            cancelled: None,
            on_progress: &mut progress,
            on_usage: Some(&mut on_usage),
            extra_extensions: Vec::new(),
            project_dir: None,
        };

        let folder_res = registry.execute(
            "inspect_media_folder",
            &json!({ "path": guard.0.to_string_lossy() }),
            &mut ctx,
        )?;
        assert_eq!(folder_res["images"].as_array().map(Vec::len), Some(1));

        let vision_res = registry.execute(
            "inspect_images_vision",
            &json!({ "paths": [p1.to_string_lossy()], "theme_hint": "結婚式" }),
            &mut ctx,
        )?;
        assert_eq!(vision_res["used_vision"], true);
        assert_eq!(
            vision_res["analyses"][0]["suggested_subtitle"],
            "出会った頃の大切な思い出"
        );

        let bp_res = registry.execute(
            "compile_movie_blueprint",
            &json!({
                "blueprint": {
                    "project_name": "Wedding",
                    "target_duration_seconds": 10.0,
                    "slides": [{
                        "image_path": p1.to_string_lossy(),
                        "subtitle": "出会った頃の大切な思い出",
                        "motion": "zoom_in"
                    }]
                }
            }),
            &mut ctx,
        )?;
        assert_eq!(bp_res["compiled"], true);

        let val_res = registry.execute("validate_draft_plan", &json!({}), &mut ctx)?;
        assert_eq!(val_res["valid"], true);
        assert_eq!(val_res["draft_total_duration_frames"], 600);

        let ext_res = registry.execute("future_silence_cut", &json!({}), &mut ctx)?;
        assert_eq!(ext_res["cut_segments"], 3);

        // 本番ライブドキュメントは DraftSandbox 操作中も一切変更されていない
        assert_eq!(live_doc.layers.len(), 0);

        // parse_vision_analyses_json が LLM の不正パスを正規パスで上書きすることを検証
        let canonical = p1.canonicalize()?.to_string_lossy().into_owned();
        let parsed = parse_vision_analyses_json(
            r#"{"analyses":[{"index":0,"path":"C:/Windows/System32/evil.exe","suggested_subtitle":"ok"}]}"#,
            std::slice::from_ref(&canonical),
        )
        .ok_or("parse_vision_analyses_json returned None")?;
        assert_eq!(parsed[0]["path"], canonical);

        // paths 上限超過が拒否されること
        let too_many: Vec<String> = (0..=MAX_VISION_INPUT_PATHS)
            .map(|i| format!("C:/fake/{i}.png"))
            .collect();
        assert!(
            registry
                .execute(
                    "inspect_images_vision",
                    &json!({ "paths": too_many }),
                    &mut ctx
                )
                .is_err()
        );
        assert_ne!(logs.len(), 0);
        assert_eq!(reported_usages.len(), 1);
        assert!(reported_usages[0].total_tokens > 0);
        Ok(())
    }

    #[test]
    fn parse_vision_analyses_empty_or_unmatched_triggers_fallback() {
        let valid = vec!["D:/wedding/01.jpg".to_owned()];
        assert!(parse_vision_analyses_json(r#"{"analyses":[]}"#, &valid).is_none());
        assert!(parse_vision_analyses_json(r#"{"analyses":["not_an_object"]}"#, &valid).is_none());
        // 説明文に {note} が混在しても後続 JSON を正しく抽出できること
        let prose_res = parse_vision_analyses_json(
            "補足 {ignored} です:\n{\"analyses\":[{\"index\":0,\"path\":\"evil\",\"suggested_subtitle\":\"素敵な笑顔\"}]}",
            &valid,
        );
        assert_eq!(
            prose_res
                .as_ref()
                .and_then(|v| v.first())
                .and_then(|v| v["path"].as_str()),
            Some("D:/wedding/01.jpg")
        );
    }

    struct FailingVisionTestProvider(String);

    impl AiProvider for FailingVisionTestProvider {
        fn id(&self) -> &'static str {
            "failing-vision"
        }

        fn display_name(&self) -> &'static str {
            "Failing Vision"
        }

        fn supports_vision(&self) -> bool {
            true
        }

        fn send_streaming(
            &self,
            _request: &crate::ai::provider::AiRequest,
            _on_event: &mut dyn FnMut(crate::ai::provider::AiStreamEvent),
        ) -> Result<crate::ai::provider::AiResponse, AiError> {
            Err(AiError::PluginError(self.0.clone()))
        }

        fn send_agent_turn(
            &self,
            _request: &AiAgentTurnRequest,
            _on_event: &mut dyn FnMut(crate::ai::provider::AiStreamEvent),
        ) -> Result<crate::ai::provider::AiAgentTurnResponse, AiError> {
            Err(AiError::PluginError(self.0.clone()))
        }
    }

    #[test]
    fn inspect_images_vision_emits_explicit_warning_on_plugin_vision_unsupported()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("vision_warn")?;
        let p1 = guard.0.join("sakura.png");
        write_png(&p1)?;

        let registry = HostToolRegistry::with_builtin_tools();
        assert!(registry.find_descriptor("audit_timeline_slides").is_some());

        let mut sandbox = DraftSandbox::new(ProjectDocument::new("WarnTest"));
        let scope = AllowedFolderScope::from_folders([&guard.0]);
        let settings = AgentRuntimeSettings::default();
        let mock_err = FailingVisionTestProvider(
            "vision_unsupported: promptCapabilities.image = false".to_owned(),
        );
        let mut last_at = None;
        let mut progress = |_: &str| {};
        let mut ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut sandbox,
            llm_provider: Some(&mock_err),
            runtime_settings: &settings,
            last_llm_call_at: &mut last_at,
            cancelled: None,
            on_progress: &mut progress,
            on_usage: None,
            extra_extensions: Vec::new(),
            project_dir: None,
        };

        let res = registry.execute(
            "inspect_images_vision",
            &json!({ "paths": [p1.to_string_lossy()] }),
            &mut ctx,
        )?;
        assert_eq!(res["used_vision"], false);
        assert_eq!(res["warning"], VISION_UNSUPPORTED_WARNING);

        // 1-based バッジ応答でも重複や 1 枚ズレが起きず 0-based index と実パスへ整合すること
        let paths_3 = vec![
            "D:/p/01.jpg".to_owned(),
            "D:/p/02.jpg".to_owned(),
            "D:/p/03.jpg".to_owned(),
        ];
        let one_based = r#"{"analyses":[
            {"index":1,"suggested_subtitle":"桜の並木道"},
            {"index":2,"suggested_subtitle":"海辺の夕暮れ"},
            {"index":3,"suggested_subtitle":"笑顔の記念写真"}
        ]}"#;
        let parsed_3 = parse_vision_analyses_json(one_based, &paths_3)
            .ok_or("1-based analyses parse failed")?;
        assert_eq!(parsed_3[0]["suggested_subtitle"], "桜の並木道");
        assert_eq!(parsed_3[1]["suggested_subtitle"], "海辺の夕暮れ");
        assert_eq!(parsed_3[2]["suggested_subtitle"], "笑顔の記念写真");
        assert_eq!(parsed_3[1]["path"], "D:/p/02.jpg");

        // 1-based 部分欠落応答（index=1 が抜けて index=2 から始まる）でも後続要素を slot 0 へ横取りしないこと
        let partial_one_based = r#"{"analyses":[
            {"index":2,"suggested_subtitle":"海辺の夕暮れ"},
            {"index":3,"suggested_subtitle":"笑顔の記念写真"}
        ]}"#;
        let parsed_partial = parse_vision_analyses_json(partial_one_based, &paths_3)
            .ok_or("partial 1-based analyses parse failed")?;
        assert_eq!(parsed_partial[0]["path"], "D:/p/01.jpg");
        assert_ne!(parsed_partial[0]["suggested_subtitle"], "海辺の夕暮れ");
        assert_eq!(parsed_partial[1]["suggested_subtitle"], "海辺の夕暮れ");
        assert_eq!(parsed_partial[2]["suggested_subtitle"], "笑顔の記念写真");

        // 無関係なパラメータエラーを vision_unsupported へ誤分類しないこと
        assert!(!is_vision_unsupported_text(
            "Model does not support temperature parameter"
        ));
        assert!(is_vision_unsupported_text(
            "This model does not support image input"
        ));
        Ok(())
    }
}
