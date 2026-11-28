//! エディタ UI のユーザー設定（タイムライン再生位置追随モード等）の永続化モジュール。

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::{user_config_file_path, user_config_read_path};
use serde::{Deserialize, Serialize};

const EDITOR_PREFS_FILE_NAME: &str = "editor_prefs.json";
static PREFS_TEMP_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 再生中・シーク時のタイムライン水平スクロール追随モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelinePlayheadFollowMode {
    /// 自動スクロール追随を行わない。
    Off,
    /// 右端 (`90%`) 到達時または表示範囲外に出たときページ送り（`12%` 位置へ配置、既定）。
    #[default]
    Page,
    /// 再生バーを常に表示領域の中央 (`50%`) 付近に維持して連続スクロールする。
    Center,
}

impl TimelinePlayheadFollowMode {
    /// 次モードへ順送りする (`Page -> Center -> Off -> Page`)。
    #[must_use]
    pub const fn cycle(self) -> Self {
        match self {
            Self::Page => Self::Center,
            Self::Center => Self::Off,
            Self::Off => Self::Page,
        }
    }
}

/// エディタ UI プリファレンス（`settings/editor_prefs.json` へ保存）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPreferences {
    /// タイムライン再生位置追随モード。
    pub playhead_follow_mode: TimelinePlayheadFollowMode,
}

impl EditorPreferences {
    /// 設定ディレクトリからプリファレンスを読み込む（欠如・破損時は既定値へフォールバック）。
    #[must_use]
    pub fn load_or_default() -> Self {
        let Some(path) = user_config_read_path(EDITOR_PREFS_FILE_NAME) else {
            return Self::default();
        };
        Self::load_from_path(&path)
    }

    /// 指定ファイルパスからプリファレンスを読み込む（欠如・破損時は既定値へフォールバック）。
    #[must_use]
    pub fn load_from_path(path: &Path) -> Self {
        let raw = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                eprintln!("[editor_prefs] failed to read {}: {err}", path.display());
                return Self::default();
            }
        };
        let stripped = raw.trim_start_matches('\u{feff}');
        serde_json::from_str::<Self>(stripped).unwrap_or_else(|err| {
            eprintln!(
                "[editor_prefs] invalid JSON in {}; falling back to defaults: {err}",
                path.display()
            );
            Self::default()
        })
    }

    /// ユーザー設定ディレクトリへ保存する。
    ///
    /// # Errors
    ///
    /// 設定パス解決不能、シリアライズ失敗、または書き込みエラー時に文字列を返す。
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = user_config_file_path(EDITOR_PREFS_FILE_NAME) else {
            return Err("settings directory is unavailable".to_owned());
        };
        self.save_to_path(&path)
    }

    /// 指定ファイルパスへ整形 JSON として原子的に保存する（一時ファイル書込→リネーム）。
    ///
    /// # Errors
    ///
    /// ディレクトリ作成、JSON 変換、または書き込み失敗時にエラー文字列を返す。
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| format!("create settings dir failed: {e}"))?;
        }
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize editor_prefs failed: {e}"))?;
        let seq = PREFS_TEMP_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("editor_prefs");
        let tmp_path = path.with_file_name(format!("{file_name}.tmp.{pid}.{seq}"));
        let mut open_opts = fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        let mut file = open_opts
            .open(&tmp_path)
            .map_err(|e| format!("open temp editor_prefs failed: {e}"))?;
        if let Err(err) = file.write_all(body.as_bytes()).and_then(|()| file.flush()) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("write temp editor_prefs failed: {err}"));
        }
        drop(file);
        if let Err(err) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("rename editor_prefs failed: {err}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playhead_follow_mode_cycles_in_expected_order() {
        assert_eq!(
            TimelinePlayheadFollowMode::default(),
            TimelinePlayheadFollowMode::Page
        );
        assert_eq!(
            TimelinePlayheadFollowMode::Page.cycle(),
            TimelinePlayheadFollowMode::Center
        );
        assert_eq!(
            TimelinePlayheadFollowMode::Center.cycle(),
            TimelinePlayheadFollowMode::Off
        );
        assert_eq!(
            TimelinePlayheadFollowMode::Off.cycle(),
            TimelinePlayheadFollowMode::Page
        );
    }

    #[test]
    fn editor_preferences_roundtrip_and_corrupted_json_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("sea_prefs_test_{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let path = dir.join("editor_prefs.json");

        let prefs = EditorPreferences {
            playhead_follow_mode: TimelinePlayheadFollowMode::Center,
        };
        prefs.save_to_path(&path)?;
        let loaded = EditorPreferences::load_from_path(&path);
        assert_eq!(loaded, prefs);

        // UTF-8 BOM 付き JSON でも正しく読み込めること
        let bom_body = format!("\u{feff}{}", fs::read_to_string(&path)?);
        fs::write(&path, bom_body)?;
        assert_eq!(EditorPreferences::load_from_path(&path), prefs);

        // 破損 JSON でもパニックせず Page (既定値) にフォールバックすること
        fs::write(&path, "{ invalid json")?;
        let fallback = EditorPreferences::load_from_path(&path);
        assert_eq!(
            fallback.playhead_follow_mode,
            TimelinePlayheadFollowMode::Page
        );

        // 存在しないファイルは既定値へフォールバックすること
        assert_eq!(
            EditorPreferences::load_from_path(&dir.join("missing.json")).playhead_follow_mode,
            TimelinePlayheadFollowMode::Page
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
