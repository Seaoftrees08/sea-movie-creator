//! `lang/*.json` 多言語ファイルの構文・非空値・プレースホルダ整合性テスト。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::workspace_root;

fn parse_lang_strings(path: &Path) -> Result<(String, BTreeMap<String, String>), String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let trimmed = raw.trim_start_matches('\u{FEFF}');
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|err| format!("invalid JSON in {}: {err}", path.display()))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| format!("{} root must be a JSON object", path.display()))?;

    let fallback_code = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("invalid file name {}", path.display()))?
        .to_ascii_lowercase();

    let code = obj
        .get("code")
        .and_then(Value::as_str)
        .map_or(fallback_code, str::to_ascii_lowercase);

    let mut out = BTreeMap::new();
    if let Some(strings) = obj.get("strings") {
        let map = strings
            .as_object()
            .ok_or_else(|| format!("{} 'strings' must be an object", path.display()))?;
        for (key, val) in map {
            let text = val
                .as_str()
                .ok_or_else(|| format!("{} key '{key}' must map to a string", path.display()))?;
            out.insert(key.clone(), text.to_owned());
        }
        return Ok((code, out));
    }

    for (key, val) in obj {
        if matches!(key.as_str(), "code" | "name" | "$schema") {
            continue;
        }
        if let Some(text) = val.as_str() {
            out.insert(key.clone(), text.to_owned());
        }
    }
    Ok((code, out))
}

fn extract_placeholders(text: &str) -> BTreeSet<String> {
    let mut placeholders = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'{' {
            let start = idx + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b'}' {
                if let Ok(token) = std::str::from_utf8(&bytes[start..end]) {
                    placeholders.insert(token.to_owned());
                }
                idx = end + 1;
                continue;
            }
        }
        idx += 1;
    }
    placeholders
}

#[test]
fn lang_packs_have_valid_entries_and_matching_placeholders() -> Result<(), String> {
    let lang_dir = workspace_root().join("lang");
    let entries = fs::read_dir(&lang_dir)
        .map_err(|err| format!("failed to read {}: {err}", lang_dir.display()))?;

    let mut packs = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry error: {err}"))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let (code, strings) = parse_lang_strings(&path)?;
        assert!(
            !strings.is_empty(),
            "{} must contain at least one translation entry",
            path.display()
        );
        for (key, text) in &strings {
            assert!(
                !text.trim().is_empty(),
                "empty translation value for '{key}' in {}",
                path.display()
            );
        }
        assert!(
            packs.insert(code.clone(), strings).is_none(),
            "duplicate language code '{code}' in lang/"
        );
    }

    let en = packs
        .get("en")
        .ok_or_else(|| "missing lang/en.json".to_owned())?;
    let ja = packs
        .get("ja")
        .ok_or_else(|| "missing lang/ja.json".to_owned())?;

    let en_keys: BTreeSet<&String> = en.keys().collect();
    let ja_keys: BTreeSet<&String> = ja.keys().collect();
    assert_eq!(
        ja_keys, en_keys,
        "lang/ja.json and lang/en.json keys must stay identical"
    );

    for (code, strings) in &packs {
        for (key, text) in strings {
            let Some(en_text) = en.get(key) else {
                continue;
            };
            let actual = extract_placeholders(text);
            let expected = extract_placeholders(en_text);
            assert_eq!(
                actual, expected,
                "placeholder mismatch for key '{key}' in lang/{code}.json"
            );
        }
    }

    Ok(())
}
