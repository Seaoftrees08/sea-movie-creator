//! 品質ゲート結合テスト群のモジュール定義および共通ファイル走査ヘルパー。

use std::fs;
use std::path::{Path, PathBuf};

mod localization_assets;
mod plugin_manifests;
mod project_compat;
mod repository_rules;

pub(super) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub(super) fn collect_files_with_extensions(
    root: &Path,
    extensions: &[&str],
) -> Result<Vec<PathBuf>, String> {
    let mut collected = Vec::new();
    collect_recursive(root, extensions, &mut collected)?;
    collected.sort();
    Ok(collected)
}

fn collect_recursive(
    dir: &Path,
    extensions: &[&str],
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("failed to read entry in {}: {err}", dir.display()))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if path.is_dir() {
            // ビルド生成物や一時 worktree は決して走査しない
            if matches!(name.as_ref(), "target" | ".git" | ".worktrees" | "dist") {
                continue;
            }
            collect_recursive(&path, extensions, out)?;
            continue;
        }
        let matches_ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                extensions
                    .iter()
                    .any(|allowed| ext.eq_ignore_ascii_case(allowed))
            });
        if matches_ext {
            out.push(path);
        }
    }
    Ok(())
}
