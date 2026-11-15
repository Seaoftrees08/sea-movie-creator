//! `plugins/export/<id>/plugin.json` の自動検出および実行ファイル解決レジストリ。

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::export::manifest::{ExportPluginManifest, ManifestError};

/// 検出に成功した書き出しプラグイン 1 件の情報。
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// 検証済みマニフェスト定義。
    pub manifest: ExportPluginManifest,
    /// `plugin.json` の絶対または基準パス。
    pub manifest_path: PathBuf,
    /// プラグインディレクトリパス。
    pub plugin_dir: PathBuf,
    /// 解決された実行バイナリパス（未ビルドの場合は `None`）。
    pub executable_path: Option<PathBuf>,
}

/// 一部プラグインの検出・検証失敗を記録する非致命的警告。
#[derive(Debug, Clone)]
pub struct DiscoveryWarning {
    /// 問題が起きたマニフェストまたは検索対象パス。
    pub manifest_path: PathBuf,
    /// 警告内容。
    pub message: String,
}

/// プラグイン解決・要求時のエラー。
#[derive(Debug, Error)]
pub enum RegistryError {
    /// 指定 ID のプラグインが登録されていない。
    #[error("export plugin '{0}' is not registered")]
    PluginNotFound(String),
    /// プラグインのマニフェストはあるが実行バイナリが見つからない。
    #[error(
        "executable '{entrypoint}' for plugin '{plugin_id}' not found (checked plugin dir and Cargo target dirs; run `cargo build --workspace --all-targets`)"
    )]
    MissingExecutable {
        /// プラグイン ID。
        plugin_id: String,
        /// マニフェスト記載のエントリポイント名。
        entrypoint: String,
    },
    /// 指定 ID のプリセットがマニフェスト内に存在しない。
    #[error("preset '{preset_id}' does not exist in plugin '{plugin_id}'")]
    PresetNotFound {
        /// プラグイン ID。
        plugin_id: String,
        /// プリセット ID。
        preset_id: String,
    },
    /// マニフェスト解析失敗。
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// ディレクトリ走査 I/O エラー。
    #[error("plugin discovery I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// 書き出しプラグインの一覧・警告を保持するレジストリ。
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: Vec<DiscoveredPlugin>,
    warnings: Vec<DiscoveryWarning>,
}

impl PluginRegistry {
    /// 空のレジストリを作成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 既定のプラグイン検索パス群（実行ファイル隣接・環境変数・カレント作業ディレクトリ）を返す。
    #[must_use]
    pub fn default_search_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = env::var("SEA_EXPORT_PLUGIN_DIR")
            && !env_dir.trim().is_empty()
        {
            dirs.push(PathBuf::from(env_dir));
        }
        if let Ok(exe) = env::current_exe()
            && let Some(exe_dir) = exe.parent()
        {
            dirs.push(exe_dir.join("plugins").join("export"));
            if let Some(root) = find_workspace_root_from(exe_dir) {
                dirs.push(root.join("plugins").join("export"));
            }
        }
        if let Ok(cwd) = env::current_dir() {
            dirs.push(cwd.join("plugins").join("export"));
        }
        dedup_paths(dirs)
    }

    /// 既定検索パスを走査して [`PluginRegistry`] を構築する。
    #[must_use]
    pub fn discover_default() -> Self {
        let dirs = Self::default_search_dirs();
        Self::discover_in_dirs(&dirs)
    }

    /// 指定ディレクトリ群から `<dir>/<sub >/plugin.json`（および直下 `plugin.json`）を検出する。
    #[must_use]
    pub fn discover_in_dirs(search_dirs: &[PathBuf]) -> Self {
        let mut registry = Self::new();
        let mut seen_ids = HashSet::new();
        for dir in search_dirs {
            if !dir.is_dir() {
                continue;
            }
            let direct_manifest = dir.join("plugin.json");
            if direct_manifest.is_file() {
                registry.try_register_manifest(dir, &direct_manifest, &mut seen_ids);
            }
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut subdirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            subdirs.sort();
            for sub in subdirs {
                let manifest_path = sub.join("plugin.json");
                if manifest_path.is_file() {
                    registry.try_register_manifest(&sub, &manifest_path, &mut seen_ids);
                }
            }
        }
        registry
    }

    fn try_register_manifest(
        &mut self,
        plugin_dir: &Path,
        manifest_path: &Path,
        seen_ids: &mut HashSet<String>,
    ) {
        match ExportPluginManifest::from_file(manifest_path) {
            Ok(manifest) => {
                if !seen_ids.insert(manifest.id.clone()) {
                    return;
                }
                let executable_path = resolve_plugin_executable(plugin_dir, &manifest.entrypoint);
                if executable_path.is_none() {
                    self.warnings.push(DiscoveryWarning {
                        manifest_path: manifest_path.to_path_buf(),
                        message: format!(
                            "executable '{}' not built or missing for '{}'",
                            manifest.entrypoint, manifest.id
                        ),
                    });
                }
                self.plugins.push(DiscoveredPlugin {
                    manifest,
                    manifest_path: manifest_path.to_path_buf(),
                    plugin_dir: plugin_dir.to_path_buf(),
                    executable_path,
                });
            }
            Err(err) => {
                self.warnings.push(DiscoveryWarning {
                    manifest_path: manifest_path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        }
    }

    /// 検出されたプラグイン一覧を返す。
    #[must_use]
    pub fn plugins(&self) -> &[DiscoveredPlugin] {
        &self.plugins
    }

    /// 走査時に発生した警告一覧を返す。
    #[must_use]
    pub fn warnings(&self) -> &[DiscoveryWarning] {
        &self.warnings
    }

    /// 指定 ID のプラグインを検索する。
    #[must_use]
    pub fn find(&self, plugin_id: &str) -> Option<&DiscoveredPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == plugin_id)
    }

    /// プラグイン・プリセット・実行バイナリの存在を必須検証し、呼び出し可能な状態を返す。
    ///
    /// # Errors
    ///
    /// プラグイン未検出、プリセット不正、または実行バイナリ未ビルド時に [`RegistryError`] を返す。
    pub fn resolve_runnable(
        &self,
        plugin_id: &str,
        preset_id: &str,
    ) -> Result<(&DiscoveredPlugin, PathBuf), RegistryError> {
        let plugin = self
            .find(plugin_id)
            .ok_or_else(|| RegistryError::PluginNotFound(plugin_id.to_owned()))?;
        if plugin.manifest.find_preset(preset_id).is_none() {
            return Err(RegistryError::PresetNotFound {
                plugin_id: plugin_id.to_owned(),
                preset_id: preset_id.to_owned(),
            });
        }
        let exe = plugin
            .executable_path
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
            .or_else(|| resolve_plugin_executable(&plugin.plugin_dir, &plugin.manifest.entrypoint))
            .ok_or_else(|| RegistryError::MissingExecutable {
                plugin_id: plugin.manifest.id.clone(),
                entrypoint: plugin.manifest.entrypoint.clone(),
            })?;
        Ok((plugin, exe))
    }
}

/// プラグインディレクトリおよび Cargo ワークスペースビルド出力から実行ファイルを探索する。
#[must_use]
pub fn resolve_plugin_executable(plugin_dir: &Path, entrypoint: &str) -> Option<PathBuf> {
    let candidate_names = platform_exe_candidates(entrypoint);
    for name in &candidate_names {
        let local = plugin_dir.join(name);
        if local.is_file() {
            return Some(local);
        }
    }

    let mut search_roots = Vec::new();
    if let Ok(target_dir) = env::var("CARGO_TARGET_DIR")
        && !target_dir.trim().is_empty()
    {
        let root = PathBuf::from(target_dir);
        search_roots.push(root.join("debug"));
        search_roots.push(root.join("release"));
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for ancestor in dir.ancestors().take(6) {
            search_roots.push(ancestor.to_path_buf());
            search_roots.push(ancestor.join("target").join("debug"));
            search_roots.push(ancestor.join("target").join("release"));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        search_roots.push(cwd.join("target").join("debug"));
        search_roots.push(cwd.join("target").join("release"));
    }

    for dir in search_roots {
        for name in &candidate_names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn platform_exe_candidates(entrypoint: &str) -> Vec<String> {
    let mut out = vec![entrypoint.to_owned()];
    let suffix = env::consts::EXE_SUFFIX;
    if !suffix.is_empty() && !entrypoint.ends_with(suffix) {
        out.push(format!("{entrypoint}{suffix}"));
    }
    out
}

fn find_workspace_root_from(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("plugins").is_dir())
        .map(Path::to_path_buf)
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for path in paths {
        let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            result.push(path);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_plugin_root(tag: &str) -> PathBuf {
        env::temp_dir().join(format!("sea-reg-test-{}-{tag}", std::process::id()))
    }

    #[test]
    fn missing_executable_fails_explicitly_without_silent_skip()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_plugin_root("missing-exe");
        let plugin_dir = root.join("broken");
        fs::create_dir_all(&plugin_dir)?;
        let manifest = serde_json::json!({
            "schema_version": 1,
            "id": "sea.export.nonexistent",
            "name": "Missing Binary Test",
            "version": "0.1.0",
            "kind": "export",
            "entrypoint": "definitely-missing-plugin-binary",
            "containers": ["mkv"],
            "license": { "spdx": "MIT", "notice": null, "requires_external_binary": false },
            "capabilities": { "supports_alpha": false, "supports_audio": true },
            "presets": [{
                "id": "default",
                "label": "Default",
                "video_codec": "none",
                "audio_codec": "none",
                "container_ext": "mkv"
            }]
        });
        fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let registry = PluginRegistry::discover_in_dirs(std::slice::from_ref(&root));
        assert_eq!(registry.plugins().len(), 1);
        assert!(!registry.warnings().is_empty());

        let Err(err) = registry.resolve_runnable("sea.export.nonexistent", "default") else {
            return Err("expected MissingExecutable error".into());
        };
        assert!(matches!(err, RegistryError::MissingExecutable { .. }));
        let _ = fs::remove_dir_all(&root);
        Ok(())
    }
}
