//! 実リポジトリ上のプラグインマニフェストと `plugins_src/` バイナリ定義の整合性検証。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use sea_movie_creator::ai::plugin_host::{AiPluginManifest, AiPluginProtocol};
use sea_movie_creator::export::manifest::ExportPluginManifest;

use super::workspace_root;

fn discover_plugin_binary_crates(root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let plugins_src = root.join("plugins_src");
    let entries = fs::read_dir(&plugins_src)
        .map_err(|err| format!("failed to read {}: {err}", plugins_src.display()))?;
    let mut binaries = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry error: {err}"))?;
        let crate_dir = entry.path();
        if !crate_dir.is_dir() || !crate_dir.join("src").join("main.rs").is_file() {
            continue;
        }
        let cargo_path = crate_dir.join("Cargo.toml");
        let raw = fs::read_to_string(&cargo_path)
            .map_err(|err| format!("failed to read {}: {err}", cargo_path.display()))?;
        if let Some(pkg_name) = parse_package_name(&raw) {
            binaries.insert(pkg_name, crate_dir);
        }
    }
    Ok(binaries)
}

fn parse_package_name(cargo_toml: &str) -> Option<String> {
    let mut in_package = false;
    for raw_line in cargo_toml.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let trimmed = rest.trim_start();
            if let Some(after_eq) = trimmed.strip_prefix('=') {
                let value = after_eq.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

fn validate_clean_spdx_declaration(spdx: &str, manifest_path: &Path) -> Result<(), String> {
    let upper = spdx.to_ascii_uppercase();
    if upper.trim().is_empty() {
        return Err(format!("empty license.spdx in {}", manifest_path.display()));
    }
    for forbidden in ["GPL-2.0-ONLY", "GPL-3.0", "AGPL"] {
        if upper.contains(forbidden) {
            return Err(format!(
                "forbidden license token '{forbidden}' in {}",
                manifest_path.display()
            ));
        }
    }
    Ok(())
}

#[test]
fn export_plugin_manifests_parse_and_match_workspace_binaries() -> Result<(), String> {
    let root = workspace_root();
    let binaries = discover_plugin_binary_crates(&root)?;
    let export_root = root.join("plugins").join("export");
    let entries = fs::read_dir(&export_root)
        .map_err(|err| format!("failed to read {}: {err}", export_root.display()))?;

    let mut seen_ids = HashSet::new();
    let mut manifest_count = 0usize;

    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry error: {err}"))?;
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let manifest_path = plugin_dir.join("plugin.json");
        assert!(
            manifest_path.is_file(),
            "missing plugin.json in {}",
            plugin_dir.display()
        );
        let manifest = ExportPluginManifest::from_file(&manifest_path)
            .map_err(|err| format!("manifest error in {}: {err}", manifest_path.display()))?;
        assert!(
            seen_ids.insert(manifest.id.clone()),
            "duplicate export plugin id: {}",
            manifest.id
        );
        assert!(
            binaries.contains_key(&manifest.entrypoint),
            "export plugin '{}' entrypoint '{}' has no matching binary crate in plugins_src/",
            manifest.id,
            manifest.entrypoint
        );
        validate_clean_spdx_declaration(&manifest.license.spdx, &manifest_path)?;
        manifest_count += 1;
    }

    assert!(
        manifest_count >= 3,
        "expected at least 3 export plugins (av1, h264, h265), found {manifest_count}"
    );
    Ok(())
}

#[test]
fn ai_plugin_manifests_and_audio_host_match_workspace_binaries() -> Result<(), String> {
    let root = workspace_root();
    let binaries = discover_plugin_binary_crates(&root)?;
    assert!(
        binaries.contains_key("sea-audio-host"),
        "missing sea-audio-host binary crate in plugins_src/"
    );

    let ai_root = root.join("plugins").join("ai");
    let entries = fs::read_dir(&ai_root)
        .map_err(|err| format!("failed to read {}: {err}", ai_root.display()))?;

    let mut seen_ids = HashSet::new();
    let mut manifest_count = 0usize;

    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry error: {err}"))?;
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let manifest_path = plugin_dir.join("ai-plugin.json");
        assert!(
            manifest_path.is_file(),
            "missing ai-plugin.json in {}",
            plugin_dir.display()
        );
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|err| format!("failed to read {}: {err}", manifest_path.display()))?;
        let manifest: AiPluginManifest = serde_json::from_str(&raw)
            .map_err(|err| format!("invalid {}: {err}", manifest_path.display()))?;

        assert_eq!(manifest.schema_version, 1, "unexpected schema_version");
        assert!(!manifest.id.trim().is_empty(), "empty AI plugin id");
        assert!(!manifest.name.trim().is_empty(), "empty AI plugin name");
        assert!(
            !manifest.version.trim().is_empty(),
            "empty AI plugin version"
        );
        assert!(
            seen_ids.insert(manifest.id.clone()),
            "duplicate AI plugin id: {}",
            manifest.id
        );
        assert!(
            binaries.contains_key(&manifest.entrypoint),
            "AI plugin '{}' entrypoint '{}' has no matching binary crate in plugins_src/",
            manifest.id,
            manifest.entrypoint
        );
        assert!(
            matches!(
                manifest.protocol,
                AiPluginProtocol::SeaAiV1 | AiPluginProtocol::AcpStdio
            ),
            "unexpected protocol in {}",
            manifest_path.display()
        );

        let mut profile_ids = HashSet::new();
        for profile in &manifest.profiles {
            assert!(
                !profile.id.trim().is_empty() && !profile.cli.trim().is_empty(),
                "invalid AI profile in {}",
                manifest_path.display()
            );
            assert!(
                profile_ids.insert(profile.id.clone()),
                "duplicate profile id '{}' in {}",
                profile.id,
                manifest_path.display()
            );
        }
        manifest_count += 1;
    }

    assert!(
        manifest_count >= 2,
        "expected at least 2 AI plugins (acp, antigravity), found {manifest_count}"
    );
    Ok(())
}
