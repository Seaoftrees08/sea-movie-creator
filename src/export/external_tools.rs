//! 外部ツール（ユーザー導入 `ffmpeg` 等）のパス設定・検証・永続化モジュール。
//!
//! 外部 `ffmpeg` CLI はユーザーが明示的に有効・設定した場合のみ書き出しダイアログに露出し、
//! 未設定状態で実行時 `program not found` エラーが発生することを防ぐ。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const EXTERNAL_TOOLS_FILE_NAME: &str = "external_tools.json";

/// 外部ツール設定（`%APPDATA%\sea-movie-creator\external_tools.json` 等へ保存）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalToolsConfig {
    /// ユーザーが設定した外部 `ffmpeg` 実行ファイルパスまたはコマンド名。
    #[serde(default)]
    pub ffmpeg_path: Option<String>,
}

impl ExternalToolsConfig {
    /// 既定の設定保存パス (`%APPDATA%\sea-movie-creator\external_tools.json` 等) を返す。
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        crate::core::user_config_file_path(EXTERNAL_TOOLS_FILE_NAME)
    }

    /// 指定 JSON ファイルから外部ツール設定を読み込む。
    ///
    /// # Errors
    ///
    /// ファイル読み込みまたは JSON 解析に失敗した場合。
    pub fn load_from_path(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("外部ツール設定読込失敗: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("外部ツール設定 JSON 解析失敗: {e}"))
    }

    /// 既定パスが存在すれば読み込み、未作成・破損時は空設定を返す。
    #[must_use]
    pub fn load_or_default() -> Self {
        Self::default_path()
            .and_then(|p| Self::load_from_path(&p).ok())
            .unwrap_or_default()
    }

    /// 指定ファイルパスへ JSON 形式で保存する。
    ///
    /// # Errors
    ///
    /// 親ディレクトリ作成・JSON 生成・書き込みに失敗した場合。
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("外部ツール設定フォルダ作成失敗: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("外部ツール設定 JSON 生成失敗: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("外部ツール設定保存失敗: {e}"))
    }

    /// 空文字を除いた設定済み `ffmpeg` 指定文字列を返す。
    #[must_use]
    pub fn configured_ffmpeg_spec(&self) -> Option<&str> {
        self.ffmpeg_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 設定済みの `ffmpeg` 実行ファイルが実際に解決できる場合、そのフルパスを返す。
    #[must_use]
    pub fn resolved_ffmpeg_path(&self) -> Option<PathBuf> {
        let spec = self.configured_ffmpeg_spec()?;
        resolve_executable_path(spec)
    }

    /// 設定済みの `ffmpeg` が有効かつ存在する場合、その UTF-8 表現を返す。
    #[must_use]
    pub fn resolved_ffmpeg_path_string(&self) -> Option<String> {
        self.resolved_ffmpeg_path()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// ユーザーが有効な `ffmpeg` を設定済みかどうかを返す。
    #[must_use]
    pub fn is_ffmpeg_configured(&self) -> bool {
        self.resolved_ffmpeg_path().is_some()
    }
}

/// ユーザー入力値から `ffmpeg` パス設定を検証・正規化する。
///
/// 空欄は設定解除（`Ok(None)`）として扱い、非空欄時は実在する実行ファイルを要求する。
///
/// # Errors
///
/// 指定パス／コマンドに該当する実行ファイルが見つからない場合。
pub fn validate_ffmpeg_input(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let resolved = resolve_executable_path(trimmed)
        .ok_or_else(|| format!("ffmpeg 実行ファイルが見つかりません: {trimmed}"))?;
    Ok(Some(resolved.to_string_lossy().into_owned()))
}

/// パスまたはコマンド名から実在する実行ファイルを解決する（未検出時は `None`）。
#[must_use]
pub fn resolve_executable_path(spec: &str) -> Option<PathBuf> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let direct = Path::new(trimmed);
    if direct.is_file() {
        return Some(direct.to_path_buf());
    }
    #[cfg(windows)]
    if !trimmed.to_ascii_lowercase().ends_with(".exe") {
        let with_exe = direct.with_extension("exe");
        if with_exe.is_file() {
            return Some(with_exe);
        }
    }

    if direct.components().count() == 1
        && let Some(path_var) = std::env::var_os("PATH")
    {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(trimmed);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            if !trimmed.to_ascii_lowercase().ends_with(".exe") {
                let exe_candidate = dir.join(format!("{trimmed}.exe"));
                if exe_candidate.is_file() {
                    return Some(exe_candidate);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ExternalToolsConfig, resolve_executable_path, validate_ffmpeg_input};

    #[test]
    fn unconfigured_or_missing_ffmpeg_reports_not_configured() {
        let empty = ExternalToolsConfig::default();
        assert!(!empty.is_ffmpeg_configured());
        assert_eq!(empty.resolved_ffmpeg_path(), None);

        let missing = ExternalToolsConfig {
            ffmpeg_path: Some("Z:\\nonexistent_tool_dir_9999\\ffmpeg.exe".to_owned()),
        };
        assert!(!missing.is_ffmpeg_configured());
        assert!(validate_ffmpeg_input("Z:\\nonexistent_tool_dir_9999\\ffmpeg.exe").is_err());
    }

    #[test]
    fn existing_file_roundtrips_json_and_resolves_as_configured()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "sea_ext_tools_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir)?;
        let exe_path: PathBuf = dir.join("ffmpeg.exe");
        std::fs::write(&exe_path, b"dummy")?;

        let raw = exe_path.to_string_lossy().into_owned();
        let validated = validate_ffmpeg_input(&raw)?;
        assert_eq!(validated.as_deref(), Some(raw.as_str()));
        assert!(resolve_executable_path(&raw).is_some());

        let cfg = ExternalToolsConfig {
            ffmpeg_path: validated,
        };
        assert!(cfg.is_ffmpeg_configured());

        let json_path = dir.join("external_tools.json");
        cfg.save_to_path(&json_path)?;
        let reloaded = ExternalToolsConfig::load_from_path(&json_path)?;
        assert_eq!(reloaded, cfg);

        assert_eq!(validate_ffmpeg_input("   ")?, None);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
