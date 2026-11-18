//! リポジトリ規約（lint 継承、`unsafe` 隔離、ルール上限、機密混入防止）の静的ガードテスト。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::{collect_files_with_extensions, workspace_root};

const ANTIGRAVITY_RULE_BYTE_LIMIT: usize = 24_000;

fn cargo_toml_inherits_workspace_lints(content: &str) -> bool {
    let mut in_lints = false;
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_lints = line == "[lints]";
            continue;
        }
        if !in_lints {
            continue;
        }
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact == "workspace=true" {
            return true;
        }
    }
    false
}

#[test]
fn all_plugin_crates_inherit_workspace_lints() -> Result<(), String> {
    let plugins_src = workspace_root().join("plugins_src");
    let entries = fs::read_dir(&plugins_src)
        .map_err(|err| format!("failed to read {}: {err}", plugins_src.display()))?;

    let mut checked = 0usize;
    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry error: {err}"))?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let cargo_path = dir.join("Cargo.toml");
        assert!(
            cargo_path.is_file(),
            "missing Cargo.toml in {}",
            dir.display()
        );
        let raw = fs::read_to_string(&cargo_path)
            .map_err(|err| format!("failed to read {}: {err}", cargo_path.display()))?;
        assert!(
            cargo_toml_inherits_workspace_lints(&raw),
            "{} must set `[lints] workspace = true`",
            cargo_path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no crates found under plugins_src/");
    Ok(())
}

#[test]
fn allow_unsafe_code_is_confined_to_backends_modules() -> Result<(), String> {
    let root = workspace_root();
    let needle = concat!("allow(", "unsafe_code)");
    let mut violations = Vec::new();

    for sub in ["src", "plugins_src", "tests", "examples"] {
        let files = collect_files_with_extensions(&root.join(sub), &["rs"])?;
        for path in files {
            let raw = fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            if !raw.contains(needle) {
                continue;
            }
            let in_backends = path
                .components()
                .any(|component| component.as_os_str() == "backends");
            if !in_backends {
                violations.push(path.display().to_string());
            }
        }
    }

    assert!(
        violations.is_empty(),
        "unsafe_code allowance found outside `backends/` modules: {violations:?}"
    );
    Ok(())
}

fn strip_yaml_frontmatter(markdown: &str) -> &str {
    let Some(rest) = markdown
        .strip_prefix("---\n")
        .or_else(|| markdown.strip_prefix("---\r\n"))
    else {
        return markdown;
    };
    if let Some(end_lf) = rest.find("\n---\n") {
        return &rest[end_lf + 5..];
    }
    if let Some(end_crlf) = rest.find("\r\n---\r\n") {
        return &rest[end_crlf + 7..];
    }
    markdown
}

fn expand_antigravity_includes(
    file_path: &Path,
    visited: &mut HashSet<std::path::PathBuf>,
) -> Result<String, String> {
    let canonical = fs::canonicalize(file_path)
        .map_err(|err| format!("failed to canonicalize {}: {err}", file_path.display()))?;
    if !visited.insert(canonical) {
        return Err(format!("cyclic rule include at {}", file_path.display()));
    }

    let raw = fs::read_to_string(file_path)
        .map_err(|err| format!("failed to read {}: {err}", file_path.display()))?;
    let body = strip_yaml_frontmatter(&raw);
    let base_dir = file_path
        .parent()
        .ok_or_else(|| format!("missing parent dir for {}", file_path.display()))?;

    let mut expanded = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(after_at) = trimmed.strip_prefix("@[")
            && let Some((_, rest)) = after_at.split_once("](")
            && let Some(rel_path) = rest.strip_suffix(')')
        {
            let target = base_dir.join(rel_path);
            assert!(
                target.is_file(),
                "rule include target not found: {} (from {})",
                target.display(),
                file_path.display()
            );
            expanded.push_str(&expand_antigravity_includes(&target, visited)?);
            expanded.push('\n');
            continue;
        }
        expanded.push_str(line);
    }
    Ok(expanded)
}

#[test]
fn antigravity_rules_stay_within_per_file_byte_budget() -> Result<(), String> {
    let root = workspace_root();
    let rule_entrypoints = [
        root.join("AGENTS.md"),
        root.join(".agents").join("rules").join("general.md"),
    ];

    for entry in rule_entrypoints {
        assert!(entry.is_file(), "missing rule file: {}", entry.display());
        let mut visited = HashSet::new();
        let expanded = expand_antigravity_includes(&entry, &mut visited)?;
        assert!(
            expanded.len() <= ANTIGRAVITY_RULE_BYTE_LIMIT,
            "{} expanded size {} exceeds Antigravity 24,000-byte cap",
            entry.display(),
            expanded.len()
        );
    }
    Ok(())
}

fn contains_real_secret_token(line: &str) -> Option<&'static str> {
    if line.contains("secret-scan-ignore") {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    if [
        "example",
        "placeholder",
        "dummy",
        "your-api-key",
        "your_api_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return None;
    }

    let pem_headers = [
        concat!("-----BEGIN ", "PRIVATE KEY-----"),
        concat!("-----BEGIN RSA ", "PRIVATE KEY-----"),
        concat!("-----BEGIN OPENSSH ", "PRIVATE KEY-----"),
    ];
    if pem_headers.iter().any(|hdr| line.contains(hdr)) {
        return Some("PEM private key header");
    }

    if has_prefixed_token(line, concat!("AI", "za"), 35, is_url_safe_token_char) {
        return Some("Google API key");
    }
    if has_prefixed_token(line, concat!("gh", "p_"), 30, is_alnum_char)
        || has_prefixed_token(line, concat!("github_", "pat_"), 30, is_ident_token_char)
    {
        return Some("GitHub personal access token");
    }
    if has_prefixed_token(line, concat!("AK", "IA"), 16, is_upper_alnum_char) {
        return Some("AWS access key ID");
    }
    if has_prefixed_token(line, concat!("sk-", "proj-"), 24, is_url_safe_token_char)
        || has_prefixed_token(line, concat!("sk-", "ant-"), 24, is_url_safe_token_char)
        || has_prefixed_token(line, concat!("sk", "-"), 36, is_alnum_char)
    {
        return Some("LLM provider API secret key");
    }

    None
}

fn has_prefixed_token<F>(line: &str, prefix: &str, min_tail: usize, is_valid: F) -> bool
where
    F: Fn(u8) -> bool,
{
    let bytes = line.as_bytes();
    let mut search_from = 0usize;
    while let Some(offset) = line[search_from..].find(prefix) {
        let idx = search_from + offset;
        let preceded_by_token = idx > 0 && is_ident_token_char(bytes[idx - 1]);
        let tail_start = idx + prefix.len();
        let tail_len = bytes[tail_start..]
            .iter()
            .take_while(|&&b| is_valid(b))
            .count();
        if !preceded_by_token && tail_len >= min_tail {
            return true;
        }
        search_from = tail_start;
    }
    false
}

const fn is_alnum_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

const fn is_upper_alnum_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

const fn is_ident_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

const fn is_url_safe_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

#[test]
fn tracked_source_and_config_files_contain_no_secrets() -> Result<(), String> {
    let root = workspace_root();
    let scan_dirs = [
        "src",
        "plugins_src",
        "plugins",
        "tests",
        "lang",
        "dev-doc",
        "docs",
        ".github",
        "scripts",
    ];
    let exts = ["rs", "toml", "json", "md", "yml", "yaml", "ps1", "sh"];
    let mut hits = Vec::new();

    for dir_name in scan_dirs {
        let files = collect_files_with_extensions(&root.join(dir_name), &exts)?;
        for path in files {
            let raw = fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            for (line_idx, line) in raw.lines().enumerate() {
                if let Some(kind) = contains_real_secret_token(line) {
                    hits.push(format!("{}:{}: {kind}", path.display(), line_idx + 1));
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "potential hardcoded secrets detected:\n{}",
        hits.join("\n")
    );
    Ok(())
}
