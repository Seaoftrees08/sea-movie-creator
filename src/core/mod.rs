//! 共通基盤・基本型・時刻やフレーム計算などの基礎モジュール。

pub mod animation;
pub mod color;
pub mod expression;
pub mod frame_rate;
pub mod ids;
pub mod schema;
pub mod time;

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// ポータブル設定ディレクトリ位置を上書きする環境変数名。
pub const ENV_SETTINGS_DIR: &str = "SEA_SETTINGS_DIR";

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

/// 優先順位（環境変数 `SEA_SETTINGS_DIR` -> `<exe_dir>/settings` -> `<cwd>/settings`）で設定ディレクトリを解決する純粋関数。
#[must_use]
pub fn resolve_settings_dir_from(
    env_override: Option<&str>,
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(raw) = env_override
        && !raw.trim().is_empty()
    {
        return Some(PathBuf::from(raw.trim()));
    }
    if let Some(dir) = exe_dir {
        return Some(dir.join("settings"));
    }
    cwd.map(|dir| dir.join("settings"))
}

/// 環境変数 `SEA_SETTINGS_DIR` が明示設定されているか（非空）を返す。
#[must_use]
pub fn has_explicit_settings_dir_override() -> bool {
    std::env::var(ENV_SETTINGS_DIR).is_ok_and(|v| !v.trim().is_empty())
}

/// 実行ファイル配置位置基準のポータブル設定フォルダ（`<exe_dir>/settings` 等）を返す。
#[must_use]
pub fn app_settings_dir() -> Option<PathBuf> {
    let env_override = std::env::var(ENV_SETTINGS_DIR).ok();
    let exe_path = std::env::current_exe().ok();
    let exe_dir = exe_path.as_deref().and_then(Path::parent);
    let cwd = std::env::current_dir().ok();
    resolve_settings_dir_from(env_override.as_deref(), exe_dir, cwd.as_deref())
}

/// 書き込み・保存先のユーザー設定ファイルパス（`<settings_dir>/<file_name>`）を返す。
#[must_use]
pub fn user_config_file_path(file_name: &str) -> Option<PathBuf> {
    app_settings_dir().map(|dir| dir.join(file_name))
}

/// 旧保存先（`%APPDATA%\sea-movie-creator\<file_name>` 等）のパスを返す（初回移行フォールバック用）。
#[must_use]
pub fn legacy_user_config_file_path(file_name: &str) -> Option<PathBuf> {
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

/// 単一設定 JSON の初回フォールバックを考慮した読込パスを解決する純粋関数。
///
/// 新パスが存在する場合は常に新パスを返す。新パスが存在せず、`has_env_override` が `false`
/// （テストやプロファイル分離のために `SEA_SETTINGS_DIR` を指定していない）かつ旧パスが存在する場合のみ
/// 旧パスへフォールバックする。
#[must_use]
pub fn user_config_read_path_with(
    new_path: Option<PathBuf>,
    legacy_path: Option<PathBuf>,
    has_env_override: bool,
) -> Option<PathBuf> {
    if let Some(ref candidate) = new_path
        && candidate.exists()
    {
        return new_path;
    }
    if !has_env_override
        && let Some(ref legacy) = legacy_path
        && legacy.exists()
    {
        return legacy_path;
    }
    new_path
}

/// 単一設定 JSON の読込パス（新 `settings/` 優先、未作成かつ非上書き時のみ旧 `%APPDATA%` フォールバック）を返す。
#[must_use]
pub fn user_config_read_path(file_name: &str) -> Option<PathBuf> {
    user_config_read_path_with(
        user_config_file_path(file_name),
        legacy_user_config_file_path(file_name),
        has_explicit_settings_dir_override(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn resolve_settings_dir_respects_env_exe_and_cwd_priority() {
        let exe = Path::new("/opt/sea/bin");
        let cwd = Path::new("/work/project");

        assert_eq!(
            resolve_settings_dir_from(Some(" /custom/settings "), Some(exe), Some(cwd)),
            Some(PathBuf::from("/custom/settings"))
        );
        assert_eq!(
            resolve_settings_dir_from(Some("   "), Some(exe), Some(cwd)),
            Some(exe.join("settings"))
        );
        assert_eq!(
            resolve_settings_dir_from(None, None, Some(cwd)),
            Some(cwd.join("settings"))
        );
        assert_eq!(resolve_settings_dir_from(None, None, None), None);
    }

    #[test]
    fn user_config_read_path_falls_back_only_when_new_missing_and_env_unset()
    -> Result<(), Box<dyn std::error::Error>> {
        let root =
            std::env::temp_dir().join(format!("sea_core_read_fallback_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("new"))?;
        fs::create_dir_all(root.join("legacy"))?;

        let new_file = root.join("new").join("key_config.json");
        let legacy_file = root.join("legacy").join("key_config.json");
        fs::write(&legacy_file, "{}")?;

        // 1. 新未作成 + SEA_SETTINGS_DIR 未設定 -> 旧パスへフォールバック
        assert_eq!(
            user_config_read_path_with(Some(new_file.clone()), Some(legacy_file.clone()), false),
            Some(legacy_file.clone())
        );

        // 2. 新未作成 + SEA_SETTINGS_DIR 設定あり -> 隔離のため旧パスを返さない
        assert_eq!(
            user_config_read_path_with(Some(new_file.clone()), Some(legacy_file.clone()), true),
            Some(new_file.clone())
        );

        // 3. 新パスが作成済みなら常に新パスを優先
        fs::write(&new_file, "{}")?;
        assert_eq!(
            user_config_read_path_with(Some(new_file.clone()), Some(legacy_file), false),
            Some(new_file)
        );

        let _ = fs::remove_dir_all(&root);
        Ok(())
    }
}
