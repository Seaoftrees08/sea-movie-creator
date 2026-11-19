//! ホスト側の機能・コマンド・フォント・導入済み拡張プラグインを自己記述 JSON として公開する契約モジュール。

use std::sync::OnceLock;

use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::agent_host::AgentPluginRegistry;
use crate::ai::blueprint::MovieBlueprint;
use crate::ai::context_builder::build_command_schema_value;
use crate::ai::plugin_host::AiPluginRegistry;
use crate::core::PluginAiHints;
use crate::export::registry::PluginRegistry;
use crate::media::subtitle_renderer::list_available_font_families;
use crate::media::visual_properties::BlendMode;
use crate::timeline::command::ParameterPath;

/// ホストに登録されている外部プラグイン・拡張モジュールの自己記述要約。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledExtensionSummary {
    /// プラグイン識別子。
    pub id: String,
    /// 表示名。
    pub name: String,
    /// 種別カテゴリ（`"export"`, `"ai"`, `"agent"` 等）。
    pub category: String,
    /// 実行バイナリが解決でき即座に利用可能かどうか。
    pub available: bool,
    /// 未ビルドや外部依存未設定などの補足状態。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_note: Option<String>,
    /// プラグインマニフェストが宣言した AI 向けヒント（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hints: Option<PluginAiHints>,
}

/// エージェント起動時およびツール問い合わせ時にホストが提供する能力スナップショット。
///
/// 新しい `EditCommand`・`ParameterPath`・フォント・外部プラグインが追加された場合、
/// このスナップショットを通じてエージェントプラグイン側がコード変更なしで自動認識できます。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCapabilitySnapshot {
    /// 契約スキーマバージョン（現在は `1`）。
    pub schema_version: u32,
    /// プロジェクト編集に利用可能なブレンドモード名一覧。
    pub supported_blend_modes: Vec<String>,
    /// `EditCommand::SetParameter` で操作可能な代表パラメータパス一覧。
    pub supported_parameter_paths: Vec<String>,
    /// 現在ローカル描画に利用可能な代表フォントファミリ名一覧。
    pub available_fonts: Vec<String>,
    /// 導入済みプラグインと状態・`ai_hints` の一覧。
    pub installed_extensions: Vec<InstalledExtensionSummary>,
    /// `schemars` 駆動の `EditCommand` 完全 JSON Schema（`include_schemas = true` 時のみ付与）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_command_schema: Option<Value>,
    /// `MovieBlueprint` 構成仕様の完全 JSON Schema（`include_schemas = true` 時のみ付与）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blueprint_schema: Option<Value>,
}

/// 現在の本体機能および導入済みプラグイン群を走査し、[`HostCapabilitySnapshot`] を構築する。
///
/// `include_schemas` を `true` にすると、`EditCommand` および `MovieBlueprint` の完全な
/// JSON Schema を同梱します（初回トークン節約時は `false` を指定可能）。
#[must_use]
pub fn collect_host_capabilities(include_schemas: bool) -> HostCapabilitySnapshot {
    let available_fonts: Vec<String> = list_available_font_families()
        .into_iter()
        .filter(|entry| entry.is_local)
        .map(|entry| entry.family)
        .take(48)
        .collect();

    let installed_extensions = collect_installed_extensions();

    HostCapabilitySnapshot {
        schema_version: 1,
        supported_blend_modes: BlendMode::ALL
            .iter()
            .map(|mode| mode.as_str().to_owned())
            .collect(),
        supported_parameter_paths: cached_parameter_path_names().clone(),
        available_fonts,
        installed_extensions,
        edit_command_schema: include_schemas
            .then(|| cached_edit_command_schema().cloned())
            .flatten(),
        blueprint_schema: include_schemas
            .then(|| cached_blueprint_schema().cloned())
            .flatten(),
    }
}

fn cached_edit_command_schema() -> Option<&'static Value> {
    static SCHEMA: OnceLock<Option<Value>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| build_command_schema_value().ok())
        .as_ref()
}

fn cached_blueprint_schema() -> Option<&'static Value> {
    static SCHEMA: OnceLock<Option<Value>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| serde_json::to_value(schema_for!(MovieBlueprint)).ok())
        .as_ref()
}

fn cached_parameter_path_names() -> &'static Vec<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        serde_json::to_value(schema_for!(ParameterPath))
            .map(|val| extract_enum_variant_names(&val))
            .unwrap_or_default()
    })
}

fn extract_enum_variant_names(schema: &Value) -> Vec<String> {
    let mut names = Vec::new();
    collect_variant_names_from_node(schema, &mut names);
    names
}

fn collect_variant_names_from_node(node: &Value, out: &mut Vec<String>) {
    let Some(obj) = node.as_object() else {
        return;
    };
    if let Some(s) = obj.get("const").and_then(Value::as_str)
        && !out.iter().any(|existing| existing == s)
    {
        out.push(s.to_owned());
    }
    if let Some(arr) = obj.get("enum").and_then(Value::as_array) {
        for item in arr {
            if let Some(s) = item.as_str()
                && !out.iter().any(|existing| existing == s)
            {
                out.push(s.to_owned());
            }
        }
    }
    if let Some(req) = obj.get("required").and_then(Value::as_array)
        && req.len() == 1
        && let Some(props) = obj.get("properties").and_then(Value::as_object)
        && let Some(key) = req[0].as_str()
        && props.contains_key(key)
        && !out.iter().any(|existing| existing == key)
    {
        out.push(key.to_owned());
    }
    for branch_key in ["oneOf", "anyOf"] {
        if let Some(branches) = obj.get(branch_key).and_then(Value::as_array) {
            for branch in branches {
                collect_variant_names_from_node(branch, out);
            }
        }
    }
}

fn collect_installed_extensions() -> Vec<InstalledExtensionSummary> {
    let mut installed = Vec::new();
    for plugin in PluginRegistry::discover_default().plugins() {
        installed.push(InstalledExtensionSummary {
            id: plugin.manifest.id.clone(),
            name: plugin.manifest.name.clone(),
            category: "export".to_owned(),
            available: plugin.executable_path.is_some(),
            status_note: plugin
                .executable_path
                .is_none()
                .then(|| format!("executable `{}` not found", plugin.manifest.entrypoint)),
            ai_hints: plugin.manifest.ai_hints.clone(),
        });
    }
    for plugin in AiPluginRegistry::discover_default().plugins() {
        installed.push(InstalledExtensionSummary {
            id: plugin.manifest.id.clone(),
            name: plugin.manifest.name.clone(),
            category: "ai".to_owned(),
            available: plugin.executable_path.is_some(),
            status_note: plugin
                .executable_path
                .is_none()
                .then(|| format!("executable `{}` not found", plugin.manifest.entrypoint)),
            ai_hints: plugin.manifest.ai_hints.clone(),
        });
    }
    for plugin in AgentPluginRegistry::discover_default().plugins() {
        installed.push(InstalledExtensionSummary {
            id: plugin.manifest.id.clone(),
            name: plugin.manifest.name.clone(),
            category: "agent".to_owned(),
            available: plugin.executable_path.is_some(),
            status_note: plugin
                .executable_path
                .is_none()
                .then(|| format!("executable `{}` not found", plugin.manifest.entrypoint)),
            ai_hints: plugin.manifest.ai_hints.clone(),
        });
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_capabilities_collects_schemas_fonts_and_extensions() {
        let light = collect_host_capabilities(false);
        assert_eq!(light.schema_version, 1);
        assert!(light.edit_command_schema.is_none());
        assert!(light.blueprint_schema.is_none());
        assert_ne!(light.available_fonts.len(), 0);
        assert!(light.supported_blend_modes.iter().any(|m| m == "overlay"));
        assert!(
            light
                .supported_parameter_paths
                .iter()
                .any(|p| p == "subtitle_text")
        );
        assert!(
            light
                .supported_parameter_paths
                .iter()
                .any(|p| p == "plugin_param")
        );

        let full = collect_host_capabilities(true);
        assert!(full.edit_command_schema.is_some());
        assert!(full.blueprint_schema.is_some());
        let bp_str = full
            .blueprint_schema
            .as_ref()
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        assert!(bp_str.contains("target_duration_seconds"));
    }
}
