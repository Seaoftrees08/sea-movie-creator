//! ユーザー AI 設定（`settings/ai_settings.json`）の永続化、モデル解決、シークレットマスク管理。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::ai::provider::{
    AgentRuntimeSettings, AiApprovalMode, AiModelPreset, AiProviderKind, AiThinkingLevel,
};
use crate::core::{
    has_explicit_settings_dir_override, legacy_user_config_file_path, user_config_file_path,
};

/// ユーザー AI 設定 JSON ファイル名。
pub const AI_SETTINGS_FILE_NAME: &str = "ai_settings.json";

/// 旧エージェントランタイム設定 JSON ファイル名（移行フォールバック対象）。
pub const LEGACY_AGENT_SETTINGS_FILE_NAME: &str = "agent_settings.json";

/// 現行の AI 設定 JSON スキーマ世代番号。
pub const CURRENT_AI_SETTINGS_SCHEMA_VERSION: u32 = 1;

static TEMP_WRITE_SEQ: AtomicU64 = AtomicU64::new(1);

/// チャット下部フッタのトークン・価格・残量表示形式プリファレンス。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDisplayPreference {
    /// プロバイダ特性に応じて最適な指標（残クレジット / Ctx残量 / 利用額）を自動選択する（既定）。
    #[default]
    Auto,
    /// トークン数優先で表示する。
    Tokens,
    /// 価格・クレジット残高（USD）優先で表示する。
    Cost,
    /// パーセント（残量% / 予算%）優先で表示する。
    Percent,
}

impl UsageDisplayPreference {
    /// 選択可能な全表示モードを返す。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Auto, Self::Tokens, Self::Cost, Self::Percent]
    }

    /// UI 表示用ラベルを返す。
    #[must_use]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Auto => "自動 (Auto)",
            Self::Tokens => "トークン数 (Tokens)",
            Self::Cost => "価格 / 残高 ($ USD)",
            Self::Percent => "パーセント (%)",
        }
    }
}

/// アプリ起動時に復元する「最後に開いていた接続 AI・モデル・モード」一式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiLastSelection {
    /// 最後に選択していた AI プロバイダ種別。
    pub provider: AiProviderKind,
    /// 最後に選択していたモデル ID。
    pub model_name: String,
    /// 最後に選択していた思考レベル。
    pub thinking_level: AiThinkingLevel,
    /// 最後に選択していた承認モード (`Manual` / `Autopilot`)。
    pub approval_mode: AiApprovalMode,
    /// 最後に選択していた自律エージェントプラグイン ID（単発チャット時は `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_agent_id: Option<String>,
    /// プロバイダ (`settings_key`) ごとのカスタムベース URL 上書きマップ。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_base_urls: BTreeMap<String, String>,
}

impl Default for AiLastSelection {
    fn default() -> Self {
        let default_provider = AiProviderKind::OpenAi;
        Self {
            provider: default_provider.clone(),
            model_name: default_provider.preferred_default_model().to_owned(),
            thinking_level: default_provider.default_thinking_level(),
            approval_mode: AiApprovalMode::Manual,
            selected_agent_id: None,
            custom_base_urls: BTreeMap::new(),
        }
    }
}

/// ユーザーが GUI または `settings/ai_settings.json` から自由に追加・上書きできるカスタムモデル定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiCustomModelConfig {
    /// プロバイダ API へ送信するモデル識別子（必須。例: `"deepseek/deepseek-r1"`）。
    pub id: String,
    /// UI 表示名（省略時は `id` をそのまま表示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 切り替え可能な思考レベル一覧（空時は思考レベル非対応扱い）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<AiThinkingLevel>,
    /// このモデルを選択したときの既定思考レベル。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking: Option<AiThinkingLevel>,
    /// コンテキストウィンドウ上限トークン数（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    /// 100 万入力トークンあたりの単価（USD、任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_1m_usd: Option<f64>,
    /// 100 万出力トークンあたりの単価（USD、任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_1m_usd: Option<f64>,
}

/// 組み込みモデル・カスタムモデル・外部 CLI 検出結果を統合した UI 解決済みモデル情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAiModel {
    /// API / CLI へ渡すモデル識別子。
    pub id: String,
    /// UI 上の表示名。
    pub display_name: String,
    /// 補助バッジ（`"New"`, `"Custom"` 等）。
    pub badge: Option<String>,
    /// 選択可能な思考レベル一覧。
    pub thinking_levels: Vec<AiThinkingLevel>,
    /// 既定思考レベル。
    pub default_thinking: AiThinkingLevel,
    /// コンテキストウィンドウ上限トークン数。
    pub context_window_tokens: Option<u64>,
    /// 100 万入力トークンあたりの単価（USD）。
    pub input_price_per_1m_usd: Option<f64>,
    /// 100 万出力トークンあたりの単価（USD）。
    pub output_price_per_1m_usd: Option<f64>,
    /// ユーザー定義カスタムモデル由来の場合は `true`。
    pub is_custom: bool,
    /// 現在環境で利用可能と判定されているか（CLI プローブ結果反映用）。
    pub available: bool,
    /// 状態補足メモ（例: `"CLI 検出済み"` / `"未導入"`）。
    pub availability_note: Option<String>,
}

impl ResolvedAiModel {
    /// 組み込みプリセット [`AiModelPreset`] から変換する。
    #[must_use]
    pub fn from_preset(preset: &AiModelPreset) -> Self {
        Self {
            id: preset.id.to_owned(),
            display_name: preset.display_name.to_owned(),
            badge: preset.badge.map(ToOwned::to_owned),
            thinking_levels: preset.thinking_levels.to_vec(),
            default_thinking: preset.default_thinking,
            context_window_tokens: preset.context_window_tokens,
            input_price_per_1m_usd: preset.input_price_per_1m_usd,
            output_price_per_1m_usd: preset.output_price_per_1m_usd,
            is_custom: false,
            available: true,
            availability_note: None,
        }
    }

    /// ユーザー定義 [`AiCustomModelConfig`] から変換する。
    #[must_use]
    pub fn from_custom(cfg: &AiCustomModelConfig) -> Self {
        let display_name = cfg
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(cfg.id.as_str())
            .to_owned();
        let default_thinking = cfg.default_thinking.unwrap_or_else(|| {
            if cfg.thinking_levels.is_empty() {
                AiThinkingLevel::Auto
            } else if cfg.thinking_levels.contains(&AiThinkingLevel::Medium) {
                AiThinkingLevel::Medium
            } else {
                cfg.thinking_levels[0]
            }
        });
        Self {
            id: cfg.id.trim().to_owned(),
            display_name,
            badge: Some("Custom".to_owned()),
            thinking_levels: cfg.thinking_levels.clone(),
            default_thinking,
            context_window_tokens: cfg.context_window_tokens,
            input_price_per_1m_usd: cfg.input_price_per_1m_usd,
            output_price_per_1m_usd: cfg.output_price_per_1m_usd,
            is_custom: true,
            available: true,
            availability_note: None,
        }
    }

    /// モデルメニュー行に表示するラベルを生成する。
    #[must_use]
    pub fn format_menu_label(
        &self,
        is_selected: bool,
        current_thinking: AiThinkingLevel,
    ) -> String {
        let mut parts = vec![self.display_name.clone()];
        if !self.thinking_levels.is_empty() {
            let displayed_level = if is_selected {
                current_thinking
            } else if self.thinking_levels.contains(&AiThinkingLevel::Medium) {
                AiThinkingLevel::Medium
            } else {
                self.default_thinking
            };
            parts.push(displayed_level.display_label().to_owned());
        }
        if let Some(badge) = &self.badge {
            parts.push(format!("[{badge}]"));
        }
        if !self.available {
            parts.push("[未検出]".to_owned());
        }
        parts.join("  ")
    }
}

/// CLI プローブ等から得られた利用可能モデル 1 件分の簡易記述（`settings.rs` 用の軽量構造体）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedModelEntry {
    /// モデルまたは CLI プロファイル識別子。
    pub id: String,
    /// 表示名。
    pub display_name: String,
    /// 補助バッジ。
    pub badge: Option<String>,
    /// 対応思考レベル一覧。
    pub thinking_levels: Vec<AiThinkingLevel>,
    /// 既定思考レベル。
    pub default_thinking: AiThinkingLevel,
    /// コンテキストウィンドウ上限トークン数。
    pub context_window_tokens: Option<u64>,
    /// 利用可能かどうか。
    pub available: bool,
    /// 状態補足メモ。
    pub note: Option<String>,
}

/// ユーザー AI 設定全体（`settings/ai_settings.json` に対応）。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiUserSettings {
    /// スキーマバージョン（現在は `1`）。
    pub schema_version: u32,
    /// 前回使用していた AI プロバイダ・モデル等の選択状態。
    pub last_selection: AiLastSelection,
    /// `settings/ai_settings.json` 内へ任意で保持するプロバイダ別 API キー（環境変数が優先）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub api_keys: BTreeMap<String, String>,
    /// プロバイダごとのユーザー自由追加・上書きモデル一覧。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_models: BTreeMap<String, Vec<AiCustomModelConfig>>,
    /// ユーザーが任意で設定するセッション / 利用予算上限（USD）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// 送信ボタン下フッタの表示形式優先設定。
    pub usage_display_mode: UsageDisplayPreference,
    /// エージェント実行時の安全制御・レート制限・Vision バッチ設定。
    pub runtime: AgentRuntimeSettings,
}

impl Default for AiUserSettings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_AI_SETTINGS_SCHEMA_VERSION,
            last_selection: AiLastSelection::default(),
            api_keys: BTreeMap::new(),
            custom_models: BTreeMap::new(),
            budget_usd: None,
            usage_display_mode: UsageDisplayPreference::Auto,
            runtime: AgentRuntimeSettings::default(),
        }
    }
}

impl fmt::Debug for AiUserSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let masked_keys: BTreeMap<&str, &str> = self
            .api_keys
            .keys()
            .map(|k| (k.as_str(), "***REDACTED***"))
            .collect();
        f.debug_struct("AiUserSettings")
            .field("schema_version", &self.schema_version)
            .field("last_selection", &self.last_selection)
            .field("api_keys", &masked_keys)
            .field("custom_models", &self.custom_models)
            .field("budget_usd", &self.budget_usd)
            .field("usage_display_mode", &self.usage_display_mode)
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl AiUserSettings {
    /// 既定の設定ファイル保存パス（`<settings_dir>/ai_settings.json`）を返す。
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        user_config_file_path(AI_SETTINGS_FILE_NAME)
    }

    /// 既定パスが存在する場合のみ `settings/ai_settings.json` を読み込む（認証リゾルバ等向け軽量読込）。
    #[must_use]
    pub fn load_default_if_exists() -> Option<Self> {
        let path = Self::default_path()?;
        if !path.is_file() {
            return None;
        }
        Self::load_from_path(&path).ok()
    }

    /// 指定パスの JSON ファイルを読み込み、正規化・検証して返す。
    ///
    /// # Errors
    ///
    /// ファイル読込失敗、JSON 解析失敗、または値検証エラー時にエラー文言を返す。
    pub fn load_from_path(path: &Path) -> Result<Self, String> {
        let raw = fs::read_to_string(path)
            .map_err(|err| format!("AI 設定ファイル読込失敗 ({}): {err}", path.display()))?;
        Self::parse_and_validate_json(&raw)
    }

    /// 指定された `settings_dir` から設定を読み込み、未作成なら旧 `agent_settings.json` からの引き継ぎを行う。
    ///
    /// パースエラー時は起動を止めず既定値へフォールバックし、第 2 要素に警告文言を返す。
    #[must_use]
    pub fn load_or_default_at(
        settings_dir: &Path,
        allow_legacy_fallback: bool,
    ) -> (Self, Option<String>) {
        let settings_path = settings_dir.join(AI_SETTINGS_FILE_NAME);
        if settings_path.is_file() {
            match Self::load_from_path(&settings_path) {
                Ok(parsed) => return (parsed, None),
                Err(err) => {
                    let mut fallback = Self::default();
                    if let Some(rt) = load_legacy_runtime(settings_dir, allow_legacy_fallback) {
                        fallback.runtime = rt;
                    }
                    return (
                        fallback,
                        Some(format!(
                            "ai_settings.json の読込に失敗したため既定値で起動しました（手動編集ファイル保護のため自動上書きを停止中）: {err}"
                        )),
                    );
                }
            }
        }

        let mut initial = Self::default();
        if let Some(rt) = load_legacy_runtime(settings_dir, allow_legacy_fallback) {
            initial.runtime = rt;
        }
        (initial, None)
    }

    /// 既定設定ディレクトリ基準で `AiUserSettings` を読み込む（ライブ起動用）。
    #[must_use]
    pub fn load_or_default_user() -> (Self, Option<PathBuf>, Option<String>) {
        let Some(path) = Self::default_path() else {
            return (Self::default(), None, None);
        };
        let Some(dir) = path.parent() else {
            return (Self::default(), Some(path), None);
        };
        let allow_legacy = !has_explicit_settings_dir_override();
        let (settings, warn) = Self::load_or_default_at(dir, allow_legacy);
        (settings, Some(path), warn)
    }

    /// JSON 文字列をパースし、エイリアスキー正規化とスキーマ検証を行って返す。
    ///
    /// 最小手書き JSON（例: `{"api_keys":{"open_router":"..."}}`）も受け付ける。
    ///
    /// # Errors
    ///
    /// JSON 構文不正、非対応 `schema_version`、空モデル ID、または負数／非有限の価格・予算時にエラーを返す。
    pub fn parse_and_validate_json(raw: &str) -> Result<Self, String> {
        let cleaned = raw.trim_start_matches('\u{feff}');
        let mut settings: Self =
            serde_json::from_str(cleaned).map_err(|err| format!("AI 設定 JSON 解析失敗: {err}"))?;
        settings.normalize_in_place();
        settings.validate()?;
        Ok(settings)
    }

    /// 人間が読みやすい整形済み JSON 文字列を生成する。
    ///
    /// # Errors
    ///
    /// JSON シリアライズ失敗時にエラー文言を返す。
    pub fn to_pretty_json(&self) -> Result<String, String> {
        let mut normalized = self.clone();
        normalized.normalize_in_place();
        serde_json::to_string_pretty(&normalized)
            .map_err(|err| format!("AI 設定 JSON 生成失敗: {err}"))
    }

    /// 指定パスへ原子的（一時ファイル経由）に JSON を書き出す。
    ///
    /// # Errors
    ///
    /// 親ディレクトリ作成、JSON シリアライズ、または書き込み失敗時にエラー文言を返す。
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|err| format!("AI 設定フォルダ作成失敗 ({}): {err}", parent.display()))?;
        }
        let json = self.to_pretty_json()?;
        atomic_write_text(path, &json)
            .map_err(|err| format!("AI 設定ファイル保存失敗 ({}): {err}", path.display()))
    }

    /// マップキーの正規化（`"openrouter"` -> `"open_router"` 等）と前後空白の除去を行う。
    pub fn normalize_in_place(&mut self) {
        let mut next_urls = BTreeMap::new();
        for (k, v) in std::mem::take(&mut self.last_selection.custom_base_urls) {
            let nk = AiProviderKind::normalize_settings_key(&k);
            let trimmed = v.trim().to_owned();
            if !nk.is_empty() && !trimmed.is_empty() {
                next_urls.insert(nk, trimmed);
            }
        }
        self.last_selection.custom_base_urls = next_urls;

        let mut next_keys = BTreeMap::new();
        for (k, v) in std::mem::take(&mut self.api_keys) {
            let nk = AiProviderKind::normalize_settings_key(&k);
            let trimmed = v.trim().to_owned();
            if !nk.is_empty() && !trimmed.is_empty() {
                next_keys.insert(nk, trimmed);
            }
        }
        self.api_keys = next_keys;

        let mut next_models: BTreeMap<String, Vec<AiCustomModelConfig>> = BTreeMap::new();
        for (k, list) in std::mem::take(&mut self.custom_models) {
            let nk = AiProviderKind::normalize_settings_key(&k);
            if nk.is_empty() {
                continue;
            }
            let target = next_models.entry(nk).or_default();
            for mut item in list {
                let trimmed_id = item.id.trim().to_owned();
                item.id = trimmed_id;
                if let Some(name) = item.display_name.take() {
                    let t = name.trim().to_owned();
                    item.display_name = (!t.is_empty()).then_some(t);
                }
                if let Some(pos) = target.iter().position(|m| m.id == item.id) {
                    target[pos] = item;
                } else {
                    target.push(item);
                }
            }
        }
        self.custom_models = next_models;
    }

    /// 値の整合性を検証する。
    ///
    /// # Errors
    ///
    /// `schema_version` 不一致、カスタムモデル ID 空欄、または不正な負数／NaN 値が含まれる場合。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CURRENT_AI_SETTINGS_SCHEMA_VERSION {
            return Err(format!(
                "非対応の schema_version `{}` です（現在対応は {}）",
                self.schema_version, CURRENT_AI_SETTINGS_SCHEMA_VERSION
            ));
        }
        if let Some(budget) = self.budget_usd
            && (!budget.is_finite() || budget < 0.0)
        {
            return Err("budget_usd は 0 以上の有限値である必要があります".to_owned());
        }
        for (provider_key, models) in &self.custom_models {
            for model in models {
                if model.id.trim().is_empty() {
                    return Err(format!(
                        "custom_models.{provider_key} に空の model id が含まれています"
                    ));
                }
                validate_context_window_tokens(model.context_window_tokens)?;
                validate_optional_price(model.input_price_per_1m_usd, "input_price_per_1m_usd")?;
                validate_optional_price(model.output_price_per_1m_usd, "output_price_per_1m_usd")?;
            }
        }
        self.runtime.validate()?;
        Ok(())
    }

    /// 指定プロバイダ向けに `settings/ai_settings.json` に保存されている API キーを返す。
    #[must_use]
    pub fn api_key_for(&self, kind: &AiProviderKind) -> Option<&str> {
        let canonical = kind.settings_key();
        if let Some(found) = self
            .api_keys
            .get(&canonical)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(found);
        }
        self.api_keys
            .iter()
            .find(|(k, _)| AiProviderKind::normalize_settings_key(k) == canonical)
            .map(|(_, v)| v.trim())
            .filter(|s| !s.is_empty())
    }

    /// 指定プロバイダ向けに JSON 保存 API キーを設定（空なら削除）する。
    pub fn set_api_key(&mut self, kind: &AiProviderKind, raw_key: &str) {
        let key = kind.settings_key();
        self.remove_aliased_keys_from_map(&key);
        let trimmed = raw_key.trim();
        if !trimmed.is_empty() {
            self.api_keys.insert(key, trimmed.to_owned());
        }
    }

    /// 指定プロバイダ向けに JSON 保存 API キーを削除する。
    pub fn remove_api_key(&mut self, kind: &AiProviderKind) {
        let key = kind.settings_key();
        self.remove_aliased_keys_from_map(&key);
    }

    fn remove_aliased_keys_from_map(&mut self, canonical: &str) {
        self.api_keys
            .retain(|k, _| AiProviderKind::normalize_settings_key(k) != canonical);
    }

    /// 指定プロバイダに保存されたカスタムベース URL を返す（未保存時は `None`）。
    #[must_use]
    pub fn base_url_for(&self, kind: &AiProviderKind) -> Option<&str> {
        let canonical = kind.settings_key();
        if let Some(url) = self
            .last_selection
            .custom_base_urls
            .get(&canonical)
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(url);
        }
        self.last_selection
            .custom_base_urls
            .iter()
            .find(|(k, _)| AiProviderKind::normalize_settings_key(k) == canonical)
            .map(|(_, v)| v.trim())
            .filter(|s| !s.is_empty())
    }

    /// 指定プロバイダのカスタムベース URL を保存または規定値へリセット（削除）する。
    pub fn set_base_url(&mut self, kind: &AiProviderKind, raw_url: &str) {
        let canonical = kind.settings_key();
        self.last_selection
            .custom_base_urls
            .retain(|k, _| AiProviderKind::normalize_settings_key(k) != canonical);
        let trimmed = raw_url.trim();
        if !trimmed.is_empty() && trimmed != kind.default_base_url() {
            self.last_selection
                .custom_base_urls
                .insert(canonical, trimmed.to_owned());
        }
    }

    /// 指定プロバイダのカスタムモデル定義スライスを返す。
    #[must_use]
    pub fn custom_models_for(&self, kind: &AiProviderKind) -> &[AiCustomModelConfig] {
        let canonical = kind.settings_key();
        if let Some(models) = self.custom_models.get(&canonical) {
            return models.as_slice();
        }
        self.custom_models
            .iter()
            .find(|(k, _)| AiProviderKind::normalize_settings_key(k) == canonical)
            .map_or(&[], |(_, v)| v.as_slice())
    }

    fn consolidate_custom_models_key(&mut self, canonical: &str) {
        let matching: Vec<String> = self
            .custom_models
            .keys()
            .filter(|k| AiProviderKind::normalize_settings_key(k) == canonical)
            .cloned()
            .collect();
        let mut combined: Vec<AiCustomModelConfig> = Vec::new();
        for key in matching {
            if let Some(items) = self.custom_models.remove(&key) {
                for item in items {
                    if let Some(pos) = combined.iter().position(|existing| existing.id == item.id) {
                        combined[pos] = item;
                    } else {
                        combined.push(item);
                    }
                }
            }
        }
        if !combined.is_empty() {
            self.custom_models.insert(canonical.to_owned(), combined);
        }
    }

    /// カスタマイズ可能プロバイダへカスタムモデルを追加または上書き更新する。
    ///
    /// # Errors
    ///
    /// 固定モデル CLI プロバイダ指定時、モデル ID 空欄時、またはコンテキスト長・単価が不正な場合にエラーを返す。
    pub fn upsert_custom_model(
        &mut self,
        kind: &AiProviderKind,
        mut model: AiCustomModelConfig,
    ) -> Result<(), String> {
        if !kind.supports_custom_models() {
            return Err(format!(
                "{} は接続先側でモデルが決まるためカスタムモデルを手動追加できません",
                kind.ui_combo_label()
            ));
        }
        let trimmed_id = model.id.trim().to_owned();
        model.id = trimmed_id;
        if model.id.is_empty() {
            return Err("モデル ID を入力してください".to_owned());
        }
        validate_context_window_tokens(model.context_window_tokens)?;
        validate_optional_price(model.input_price_per_1m_usd, "input_price_per_1m_usd")?;
        validate_optional_price(model.output_price_per_1m_usd, "output_price_per_1m_usd")?;
        model.display_name = model
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());

        let canonical = kind.settings_key();
        self.consolidate_custom_models_key(&canonical);
        let entry = self.custom_models.entry(canonical).or_default();
        if let Some(existing) = entry.iter_mut().find(|m| m.id == model.id) {
            *existing = model;
        } else {
            entry.push(model);
        }
        Ok(())
    }

    /// 指定プロバイダのカスタムモデルを削除する（削除できたら `true`）。
    pub fn remove_custom_model(&mut self, kind: &AiProviderKind, model_id: &str) -> bool {
        let canonical = kind.settings_key();
        self.consolidate_custom_models_key(&canonical);
        let trimmed = model_id.trim();
        let Some(list) = self.custom_models.get_mut(&canonical) else {
            return false;
        };
        let before = list.len();
        list.retain(|m| m.id != trimmed);
        let removed = list.len() < before;
        if list.is_empty() {
            self.custom_models.remove(&canonical);
        }
        removed
    }

    /// プロバイダの標準カタログ・カスタムモデル・外部 CLI 検出結果を統合した一覧を返す。
    #[must_use]
    pub fn resolve_models_with_discovered(
        &self,
        kind: &AiProviderKind,
        discovered: &[ProbedModelEntry],
    ) -> Vec<ResolvedAiModel> {
        if !kind.supports_custom_models() && !discovered.is_empty() {
            return discovered
                .iter()
                .map(|entry| {
                    let preset = kind.model_preset(&entry.id);
                    ResolvedAiModel {
                        id: entry.id.clone(),
                        display_name: entry.display_name.clone(),
                        badge: entry.badge.clone(),
                        thinking_levels: entry.thinking_levels.clone(),
                        default_thinking: entry.default_thinking,
                        context_window_tokens: entry
                            .context_window_tokens
                            .or_else(|| preset.and_then(|p| p.context_window_tokens)),
                        input_price_per_1m_usd: None,
                        output_price_per_1m_usd: None,
                        is_custom: false,
                        available: entry.available,
                        availability_note: entry.note.clone(),
                    }
                })
                .collect();
        }

        let mut merged: Vec<ResolvedAiModel> = kind
            .model_catalog()
            .iter()
            .map(ResolvedAiModel::from_preset)
            .collect();

        if kind.supports_custom_models() {
            for cfg in self.custom_models_for(kind) {
                let resolved = ResolvedAiModel::from_custom(cfg);
                if let Some(pos) = merged.iter().position(|m| m.id == resolved.id) {
                    merged[pos] = resolved;
                } else {
                    merged.push(resolved);
                }
            }
        }
        merged
    }
}

fn validate_context_window_tokens(value: Option<u64>) -> Result<(), String> {
    if value == Some(0) {
        return Err("context_window_tokens は 1 以上の正の整数である必要があります".to_owned());
    }
    Ok(())
}

fn validate_optional_price(value: Option<f64>, field: &str) -> Result<(), String> {
    if let Some(v) = value
        && (!v.is_finite() || v < 0.0)
    {
        return Err(format!("{field} は 0 以上の有限値である必要があります"));
    }
    Ok(())
}

fn load_legacy_runtime(
    settings_dir: &Path,
    allow_legacy_fallback: bool,
) -> Option<AgentRuntimeSettings> {
    let local_agent_settings = settings_dir.join(LEGACY_AGENT_SETTINGS_FILE_NAME);
    if let Some(rt) = read_runtime_file(&local_agent_settings) {
        return Some(rt);
    }
    if allow_legacy_fallback
        && let Some(legacy_path) = legacy_user_config_file_path(LEGACY_AGENT_SETTINGS_FILE_NAME)
        && let Some(rt) = read_runtime_file(&legacy_path)
    {
        return Some(rt);
    }
    None
}

fn read_runtime_file(path: &Path) -> Option<AgentRuntimeSettings> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<AgentRuntimeSettings>(&raw)
        .ok()
        .map(|s| s.sanitized())
}

fn atomic_write_text(path: &Path, text: &str) -> Result<(), std::io::Error> {
    use std::io::Write as _;

    let seq = TEMP_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ai_settings");
    let tmp_path = path.with_file_name(format!("{file_name}.tmp.{pid}.{seq}"));
    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open_opts.mode(0o600);
    }
    let mut file = open_opts.open(&tmp_path)?;
    if let Err(err) = file.write_all(text.as_bytes()).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    drop(file);
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        fn create(label: &str) -> Result<Self, std::io::Error> {
            let seq = TEMP_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "sea_ai_settings_{label}_{}_{seq}",
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

    #[test]
    fn minimal_handwritten_json_and_aliases_load_cleanly() -> Result<(), Box<dyn std::error::Error>>
    {
        let raw = r#"{
            "api_keys": { "openrouter": "demo-openrouter-key-123" },
            "custom_models": {
                "openai": [{ "id": "gpt-5.2-custom" }]
            }
        }"#;
        let parsed = AiUserSettings::parse_and_validate_json(raw)?;
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.last_selection.provider, AiProviderKind::OpenAi);
        assert_eq!(
            parsed.api_key_for(&AiProviderKind::OpenRouter),
            Some("demo-openrouter-key-123")
        );
        assert!(parsed.api_keys.contains_key("open_router"));
        assert_eq!(parsed.custom_models_for(&AiProviderKind::OpenAi).len(), 1);
        assert_eq!(
            parsed.custom_models_for(&AiProviderKind::OpenAi)[0].id,
            "gpt-5.2-custom"
        );

        let dbg = format!("{parsed:?}");
        assert!(!dbg.contains("demo-openrouter-key-123"));
        assert!(dbg.contains("***REDACTED***"));
        Ok(())
    }

    #[test]
    fn save_roundtrip_and_invalid_json_error_work() -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::create("roundtrip")?;
        let path = guard.0.join(AI_SETTINGS_FILE_NAME);

        let mut settings = AiUserSettings::default();
        settings.last_selection.provider = AiProviderKind::OpenRouter;
        settings.last_selection.model_name = "deepseek/deepseek-r1".to_owned();
        settings.set_base_url(&AiProviderKind::OllamaCompat, "http://127.0.0.1:11435/v1");
        settings.set_api_key(&AiProviderKind::OpenRouter, "demo-key-abc");
        settings.budget_usd = Some(15.0);
        settings.usage_display_mode = UsageDisplayPreference::Cost;
        settings.upsert_custom_model(
            &AiProviderKind::OpenRouter,
            AiCustomModelConfig {
                id: "deepseek/deepseek-r1".to_owned(),
                display_name: Some("DeepSeek R1".to_owned()),
                thinking_levels: vec![AiThinkingLevel::Low, AiThinkingLevel::High],
                default_thinking: Some(AiThinkingLevel::High),
                context_window_tokens: Some(64_000),
                input_price_per_1m_usd: Some(0.55),
                output_price_per_1m_usd: Some(2.19),
            },
        )?;
        settings.save_to_path(&path)?;

        let loaded = AiUserSettings::load_from_path(&path)?;
        assert_eq!(loaded, settings);
        assert_eq!(
            loaded.base_url_for(&AiProviderKind::OllamaCompat),
            Some("http://127.0.0.1:11435/v1")
        );

        let models = loaded.resolve_models_with_discovered(&AiProviderKind::OpenRouter, &[]);
        assert!(
            models
                .iter()
                .any(|m| m.id == "deepseek/deepseek-r1" && m.is_custom)
        );

        assert!(
            loaded
                .clone()
                .remove_custom_model(&AiProviderKind::OpenRouter, "deepseek/deepseek-r1")
        );

        // 不正 JSON は分かりやすいエラーになり、load_or_default_at は既定値＋警告へフォールバック
        fs::write(&path, "{ invalid json")?;
        assert!(AiUserSettings::load_from_path(&path).is_err());
        let (fallback, warning) = AiUserSettings::load_or_default_at(&guard.0, false);
        assert!(warning.is_some());
        assert_eq!(fallback.schema_version, 1);
        Ok(())
    }

    #[test]
    fn fixed_model_cli_provider_rejects_custom_models_and_inherits_legacy_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut settings = AiUserSettings::default();
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        let err = settings.upsert_custom_model(
            &agy,
            AiCustomModelConfig {
                id: "custom-model".to_owned(),
                display_name: None,
                thinking_levels: Vec::new(),
                default_thinking: None,
                context_window_tokens: None,
                input_price_per_1m_usd: None,
                output_price_per_1m_usd: None,
            },
        );
        assert!(err.is_err());

        let guard = TempDirGuard::create("legacy_rt")?;
        fs::write(
            guard.0.join(LEGACY_AGENT_SETTINGS_FILE_NAME),
            r#"{"max_steps_per_turn":42,"min_llm_interval_ms":2000}"#,
        )?;
        let (loaded, warn) = AiUserSettings::load_or_default_at(&guard.0, false);
        assert_eq!(warn, None);
        assert_eq!(loaded.runtime.max_steps_per_turn, 42);
        assert_eq!(loaded.runtime.min_llm_interval_ms, 2000);
        Ok(())
    }

    #[test]
    fn cli_probe_inherits_preset_context_window_and_custom_models_handle_aliases_and_zero_ctx() {
        let agy = AiProviderKind::Plugin {
            plugin_id: "sea.ai.antigravity".to_owned(),
        };
        let resolved = AiUserSettings::default().resolve_models_with_discovered(
            &agy,
            &[ProbedModelEntry {
                id: "gemini-4-hydrogen".to_owned(),
                display_name: "Gemini 4 Hydrogen".to_owned(),
                badge: Some("Live CLI".to_owned()),
                thinking_levels: vec![AiThinkingLevel::Low, AiThinkingLevel::Medium],
                default_thinking: AiThinkingLevel::Medium,
                context_window_tokens: None,
                available: true,
                note: Some("検出済み".to_owned()),
            }],
        );
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].context_window_tokens, Some(1_000_000));

        // 0 コンテキスト長は validate と upsert の両方で拒否される
        let zero_json = r#"{"schema_version":1,"custom_models":{"openrouter":[{"id":"x/zero","context_window_tokens":0}]}}"#;
        assert!(AiUserSettings::parse_and_validate_json(zero_json).is_err());

        let mut raw_settings = AiUserSettings::default();
        assert!(
            raw_settings
                .upsert_custom_model(
                    &AiProviderKind::OpenRouter,
                    AiCustomModelConfig {
                        id: "x/zero".to_owned(),
                        display_name: None,
                        thinking_levels: Vec::new(),
                        default_thinking: None,
                        context_window_tokens: Some(0),
                        input_price_per_1m_usd: None,
                        output_price_per_1m_usd: None,
                    },
                )
                .is_err()
        );

        // エイリアスキー ("openrouter") をそのまま持つ未正規化構造体からも統合削除できる
        raw_settings.custom_models.insert(
            "openrouter".to_owned(),
            vec![AiCustomModelConfig {
                id: "alias/model".to_owned(),
                display_name: None,
                thinking_levels: Vec::new(),
                default_thinking: None,
                context_window_tokens: Some(64_000),
                input_price_per_1m_usd: None,
                output_price_per_1m_usd: None,
            }],
        );
        assert!(raw_settings.remove_custom_model(&AiProviderKind::OpenRouter, "alias/model"));
        assert!(raw_settings.custom_models.is_empty());
    }
}
