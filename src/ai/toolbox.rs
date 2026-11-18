//! 動的自己記述ツールレジストリ (`HostToolRegistry`) および軽量 `DraftSandbox` 実行環境。

use std::time::{Duration, Instant};

use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ai::blueprint::{MovieBlueprint, compile_blueprint_to_edit_command};
use crate::ai::capabilities::{InstalledExtensionSummary, collect_host_capabilities};
use crate::ai::context_builder::build_timeline_context;
use crate::ai::media_inspector::{
    AllowedFolderScope, encode_thumbnail_jpeg_base64, inspect_media_folder,
};
use crate::ai::planner::dry_run_validate;
use crate::ai::provider::{
    AgentRuntimeSettings, AiAgentTurnRequest, AiContentPart, AiProvider, AiToolDefinition,
    AiTurnMessage,
};
use crate::core::time::FrameIndex;
use crate::timeline::command::EditCommand;
use crate::timeline::dispatcher::CommandDispatcher;
use crate::timeline::document::ProjectDocument;

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
    /// 前回 LLM 呼び出し完了時刻（`min_llm_interval_ms` 制御用）。
    pub last_llm_call_at: Option<Instant>,
    /// 進捗通知コールバック。
    pub on_progress: &'a mut dyn FnMut(&str),
    /// 登録済み外部拡張を自己申告一覧に含めるための追加リスト（任意）。
    pub extra_extensions: Vec<InstalledExtensionSummary>,
}

impl ToolExecutionContext<'_> {
    /// 設定された `min_llm_interval_ms` を満たすよう必要に応じて待機し、最終呼出時刻を更新する。
    pub fn throttle_before_llm_call(&mut self) {
        let min_ms = self.runtime_settings.min_llm_interval_ms;
        if min_ms > 0
            && let Some(prev) = self.last_llm_call_at
        {
            let target = Duration::from_millis(min_ms);
            if let Some(remaining) = target.checked_sub(prev.elapsed()) {
                std::thread::sleep(remaining);
            }
        }
        self.last_llm_call_at = Some(Instant::now());
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
    tools: Vec<Box<dyn AgentTool>>,
}

impl std::fmt::Debug for HostToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.tools.iter().map(|t| t.descriptor().name).collect();
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
        reg.register(Box::new(CompileMovieBlueprintTool));
        reg.register(Box::new(ApplyDraftEditCommandTool));
        reg.register(Box::new(ResetDraftToLiveTool));
        reg.register(Box::new(ValidateDraftPlanTool));
        reg
    }

    /// 新しいツールを登録する（同名ツールが存在する場合は上書き置換）。
    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
        let name = tool.descriptor().name;
        if let Some(pos) = self.tools.iter().position(|t| t.descriptor().name == name) {
            self.tools[pos] = tool;
        } else {
            self.tools.push(tool);
        }
    }

    /// 登録済み全ツールの自己記述仕様一覧を返す。
    #[must_use]
    pub fn list_descriptors(&self) -> Vec<AgentToolDescriptor> {
        self.tools.iter().map(|t| t.descriptor()).collect()
    }

    /// 指定名ツールの自己記述仕様を検索する。
    #[must_use]
    pub fn find_descriptor(&self, name: &str) -> Option<AgentToolDescriptor> {
        self.tools
            .iter()
            .map(|t| t.descriptor())
            .find(|d| d.name == name)
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
        let tool = self
            .tools
            .iter()
            .find(|t| t.descriptor().name == name)
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

struct InspectImagesVisionTool;

impl AgentTool for InspectImagesVisionTool {
    fn descriptor(&self) -> AgentToolDescriptor {
        AgentToolDescriptor {
            name: "inspect_images_vision".to_owned(),
            category: "vision".to_owned(),
            description: "画像パス一覧を長辺512px相当へ軽量縮小し、Vision 対応 LLM で情景説明・章候補・適切な日本語字幕案をバッチ生成します。".to_owned(),
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
        if paths.is_empty() {
            return Ok(json!({ "analyses": [], "used_vision": false }));
        }

        let theme_hint = args["theme_hint"]
            .as_str()
            .unwrap_or("思い出フォトムービー");
        let max_edge = ctx.runtime_settings.vision_max_edge_px.clamp(128, 1024);
        let batch_size = ctx.runtime_settings.vision_batch_size.clamp(1, 12);

        let Some(provider) = ctx.llm_provider.filter(|p| p.supports_vision()) else {
            // Vision 非対応時はファイル名ベースの安全フォールバックを返す
            let fallback: Vec<Value> = paths
                .iter()
                .map(|p| build_filename_fallback_analysis(p))
                .collect();
            return Ok(json!({
                "analyses": fallback,
                "used_vision": false,
                "note": "selected provider does not support Vision; generated metadata-only fallback",
            }));
        };

        let mut all_analyses = Vec::with_capacity(paths.len());
        let total = paths.len();
        for (chunk_idx, chunk) in paths.chunks(batch_size).enumerate() {
            let done_so_far = (chunk_idx * batch_size).min(total);
            (ctx.on_progress)(&format!(
                "写真を Vision 解析中 ({done_so_far}/{total} 枚)..."
            ));
            ctx.throttle_before_llm_call();
            let batch_items =
                run_single_vision_batch(provider, ctx.folder_scope, chunk, max_edge, theme_hint)?;
            all_analyses.extend(batch_items);
        }

        Ok(json!({
            "analyses": all_analyses,
            "used_vision": true,
        }))
    }
}

fn run_single_vision_batch(
    provider: &dyn AiProvider,
    scope: &AllowedFolderScope,
    paths: &[String],
    max_edge: u32,
    theme_hint: &str,
) -> Result<Vec<Value>, String> {
    let mut parts = Vec::new();
    let mut manifest_lines = Vec::new();
    let mut valid_paths = Vec::new();

    for (idx, p) in paths.iter().enumerate() {
        let thumb = encode_thumbnail_jpeg_base64(scope, std::path::Path::new(p), max_edge)
            .map_err(|e| e.to_string())?;
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

    let prompt_head = format!(
        "Analyze the attached photos for the video theme: \"{theme_hint}\".\n\
         Return ONLY a valid JSON object of the form:\n\
         {{\"analyses\":[{{\"index\":0,\"path\":\"...\",\"scene_description\":\"...\",\"suggested_chapter\":\"...\",\"suggested_subtitle\":\"...\",\"mood\":\"warm|joyful|nostalgic|calm\",\"recommended_motion\":\"zoom_in|zoom_out|pan_left|pan_right\"}}]}}\n\
         Keep `suggested_subtitle` warm, natural Japanese (12-26 chars) matching what is actually visible in the photo.\n\
         Images:\n{}",
        manifest_lines.join("\n")
    );
    parts.insert(0, AiContentPart::text(prompt_head));

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

    let resp = provider
        .send_agent_turn(&req, &mut |_| {})
        .map_err(|e| e.to_string())?;
    let raw_text = resp.text.unwrap_or_default();
    if let Some(parsed) = parse_vision_analyses_json(&raw_text, &valid_paths) {
        return Ok(parsed);
    }
    Ok(valid_paths
        .iter()
        .map(|p| build_filename_fallback_analysis(p))
        .collect())
}

fn parse_vision_analyses_json(raw: &str, valid_paths: &[String]) -> Option<Vec<Value>> {
    let trimmed = raw.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let val: Value = serde_json::from_str(&trimmed[start..=end]).ok()?;
    let arr = val.get("analyses")?.as_array()?;
    let mut out = Vec::with_capacity(valid_paths.len());
    for (idx, default_path) in valid_paths.iter().enumerate() {
        let matched = arr
            .iter()
            .find(|item| item["index"].as_u64() == Some(idx as u64))
            .or_else(|| arr.get(idx));
        if let Some(item) = matched {
            let mut obj = item.clone();
            if obj.get("path").and_then(Value::as_str).is_none() {
                obj["path"] = Value::String(default_path.clone());
            }
            out.push(obj);
        } else {
            out.push(build_filename_fallback_analysis(default_path));
        }
    }
    Some(out)
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
        let blueprint_schema = serde_json::to_value(schema_for!(MovieBlueprint))
            .unwrap_or_else(|_| json!({ "type": "object" }));
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
        let batch = ctx.sandbox.to_batch_command();
        let raw = serde_json::to_string(&batch).map_err(|e| e.to_string())?;
        let plan =
            dry_run_validate(ctx.sandbox.base_document(), &raw).map_err(|e| e.to_string())?;
        let include_command = args["include_command"].as_bool().unwrap_or(false);
        let mut resp = json!({
            "valid": true,
            "is_no_op": plan.is_no_op(),
            "summary_lines": plan.summary_lines,
            "affected_targets_count": plan.affected_targets.len(),
            "draft_total_duration_frames": ctx.sandbox.draft_document().total_duration_frames(),
        });
        if include_command && let Ok(cmd_val) = serde_json::to_value(&batch) {
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
        let mock_vision = MockAiProvider::with_agent_turns([AiAgentTurnResponse {
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
            tool_calls: Vec::new(),
            model: "mock-vision".to_owned(),
            finish_reason: Some("stop".to_owned()),
        }]);

        let mut logs = Vec::new();
        let mut progress = |msg: &str| logs.push(msg.to_owned());
        let mut ctx = ToolExecutionContext {
            folder_scope: &scope,
            sandbox: &mut sandbox,
            llm_provider: Some(&mock_vision),
            runtime_settings: &settings,
            last_llm_call_at: None,
            on_progress: &mut progress,
            extra_extensions: Vec::new(),
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
        assert_ne!(logs.len(), 0);
        Ok(())
    }
}
