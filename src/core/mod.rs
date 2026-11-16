//! 共通基盤・基本型・時刻やフレーム計算などの基礎モジュール。

pub mod animation;
pub mod color;
pub mod expression;
pub mod frame_rate;
pub mod ids;
pub mod schema;
pub mod time;

use std::path::PathBuf;

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
