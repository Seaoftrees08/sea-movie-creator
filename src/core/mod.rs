//! 共通基盤・基本型・時刻やフレーム計算などの基礎モジュール。

pub mod animation;
pub mod color;
pub mod expression;
pub mod frame_rate;
pub mod ids;
pub mod schema;
pub mod time;

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 各種プラグインマニフェスト（`plugin.json` / `ai-plugin.json` / `agent-plugin.json`）が任意で宣言できる AI 向け自己記述ヒント。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct PluginAiHints {
    /// プラグインの目的・得意分野の要約文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// 提供する主要能力タグ一覧（例: `["vision_batch", "movie_blueprint"]`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// エージェントが利用を検討すべき推奨ユースケース一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended_scenarios: Vec<String>,
}

/// アプリ共通のユーザー設定ディレクトリ配下のファイルパス（`%APPDATA%\sea-movie-creator\<file_name>` 等）を返す。
#[must_use]
pub fn user_config_file_path(file_name: &str) -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA")
        && !appdata.trim().is_empty()
    {
        return Some(
            PathBuf::from(appdata)
                .join("sea-movie-creator")
                .join(file_name),
        );
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        return Some(PathBuf::from(xdg).join("sea-movie-creator").join(file_name));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("sea-movie-creator")
                .join(file_name)
        })
}
