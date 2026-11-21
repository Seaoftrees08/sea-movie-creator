//! `lang/*.json` ファイルの自動検出・読み込み・フォールバック解決を担う多言語 (i18n) カタログ。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::{user_config_file_path, user_config_read_path};

const BUILTIN_JA_JSON: &str = include_str!("../../lang/ja.json");
const BUILTIN_EN_JSON: &str = include_str!("../../lang/en.json");
const DEFAULT_LANGUAGE_CODE: &str = "ja";

static GLOBAL_I18N: LazyLock<RwLock<I18nCatalog>> =
    LazyLock::new(|| RwLock::new(I18nCatalog::load_default()));

/// 利用可能な言語パックの概要メタデータ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageInfo {
    /// 言語コード（例: `"ja"`, `"en"`, `"ko"`）。
    pub code: String,
    /// メニューや設定欄に表示する言語名（例: `"日本語"`, `"English"`）。
    pub name: String,
    /// 外部 `lang/*.json` から読み込まれた場合のファイルパス。
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct LoadedPack {
    info: LanguageInfo,
    strings: BTreeMap<String, String>,
}

/// 組み込み言語と `lang/` ディレクトリ群を統合管理する多言語カタログ。
#[derive(Debug, Clone)]
pub struct I18nCatalog {
    packs: BTreeMap<String, LoadedPack>,
    active_code: String,
    search_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedLanguagePreference {
    active_code: String,
}

impl Default for I18nCatalog {
    fn default() -> Self {
        Self::from_dirs(&default_lang_search_dirs())
    }
}

impl I18nCatalog {
    /// 既定の `lang/` 検索ディレクトリ群および保存済み設定を読み込んで初期化する。
    #[must_use]
    pub fn load_default() -> Self {
        let mut catalog = Self::from_dirs(&default_lang_search_dirs());
        if let Some(pref_code) = load_saved_language_preference()
            && catalog.packs.contains_key(&pref_code)
        {
            catalog.active_code = pref_code;
        }
        catalog
    }

    /// 指定された `lang/` ディレクトリ群を優先順位付きで走査しカタログを構築する。
    #[must_use]
    pub fn from_dirs(dirs: &[PathBuf]) -> Self {
        let mut packs = BTreeMap::new();
        for raw in [BUILTIN_JA_JSON, BUILTIN_EN_JSON] {
            if let Ok(pack) = parse_language_pack_str(raw, "builtin", None) {
                packs.insert(pack.info.code.clone(), pack);
            }
        }

        for dir in dirs {
            load_directory_into(&mut packs, dir);
        }

        Self {
            packs,
            active_code: DEFAULT_LANGUAGE_CODE.to_owned(),
            search_dirs: dirs.to_vec(),
        }
    }

    /// 登録済みディレクトリを再走査し、追加・編集された `lang/*.json` を反映する。
    pub fn reload(&mut self) -> usize {
        let dirs = self.search_dirs.clone();
        let prev = self.active_code.clone();
        *self = Self::from_dirs(&dirs);
        if self.packs.contains_key(&prev) {
            self.active_code = prev;
        }
        self.packs.len()
    }

    /// アクティブ言語を切り替える（存在しない言語コードの場合はエラー）。
    ///
    /// # Errors
    ///
    /// 指定された言語コードに対応する言語パックが存在しない場合。
    pub fn set_active(&mut self, code: &str) -> Result<LanguageInfo, String> {
        let norm = normalize_lang_code(code);
        let Some(pack) = self.packs.get(&norm) else {
            return Err(format!("未知の言語コードです: {code}"));
        };
        self.active_code = norm;
        Ok(pack.info.clone())
    }

    /// 現在のアクティブ言語コードを返す。
    #[must_use]
    pub fn active_code(&self) -> &str {
        &self.active_code
    }

    /// 利用可能な全言語の一覧を返す（`ja` / `en` を先頭に維持し、追加言語をコード順に並べる）。
    #[must_use]
    pub fn languages(&self) -> Vec<LanguageInfo> {
        let mut out = Vec::new();
        for pinned in ["ja", "en"] {
            if let Some(p) = self.packs.get(pinned) {
                out.push(p.info.clone());
            }
        }
        for (code, pack) in &self.packs {
            if code != "ja" && code != "en" {
                out.push(pack.info.clone());
            }
        }
        out
    }

    /// キーに対応する翻訳文を返す（アクティブ言語 -> `en` -> `ja` -> キー文字列 の順でフォールバック）。
    #[must_use]
    pub fn translate<'a>(&'a self, key: &'a str) -> &'a str {
        if let Some(pack) = self.packs.get(&self.active_code)
            && let Some(val) = pack.strings.get(key)
        {
            return val.as_str();
        }
        for fallback in ["en", DEFAULT_LANGUAGE_CODE] {
            if fallback != self.active_code
                && let Some(pack) = self.packs.get(fallback)
                && let Some(val) = pack.strings.get(key)
            {
                return val.as_str();
            }
        }
        key
    }

    /// プレースホルダ `{name}` を展開して翻訳文を返す。
    #[must_use]
    pub fn format(&self, key: &str, vars: &[(&str, &str)]) -> String {
        let mut out = self.translate(key).to_owned();
        for (name, value) in vars {
            let token = format!("{{{name}}}");
            out = out.replace(&token, value);
        }
        out
    }
}

/// 現在の設定言語でキーに対応する UI 文字列を取得する。
#[must_use]
pub fn tr(key: &str) -> String {
    match GLOBAL_I18N.read() {
        Ok(guard) => guard.translate(key).to_owned(),
        Err(poisoned) => poisoned.into_inner().translate(key).to_owned(),
    }
}

/// 現在の設定言語で `{name}` プレースホルダを展開した UI 文字列を取得する。
#[must_use]
pub fn tr_fmt(key: &str, vars: &[(&str, &str)]) -> String {
    match GLOBAL_I18N.read() {
        Ok(guard) => guard.format(key, vars),
        Err(poisoned) => poisoned.into_inner().format(key, vars),
    }
}

/// 現在アクティブな言語コードを返す。
#[must_use]
pub fn active_language_code() -> String {
    match GLOBAL_I18N.read() {
        Ok(guard) => guard.active_code().to_owned(),
        Err(poisoned) => poisoned.into_inner().active_code().to_owned(),
    }
}

/// 検出済み（組み込み＋外部 `lang/*.json`）の全言語リストを返す。
#[must_use]
pub fn available_languages() -> Vec<LanguageInfo> {
    match GLOBAL_I18N.read() {
        Ok(guard) => guard.languages(),
        Err(poisoned) => poisoned.into_inner().languages(),
    }
}

/// アクティブ言語を切り替え、設定ファイルへ永続化する。
///
/// # Errors
///
/// 未登録の言語コードが指定された場合。
pub fn set_active_language(code: &str) -> Result<LanguageInfo, String> {
    let info = match GLOBAL_I18N.write() {
        Ok(mut guard) => guard.set_active(code)?,
        Err(poisoned) => poisoned.into_inner().set_active(code)?,
    };
    save_language_preference(&info.code);
    Ok(info)
}

/// `lang/` フォルダを再走査して新規追加や手動編集を即座に反映し、合計言語数を返す。
pub fn reload_languages() -> usize {
    match GLOBAL_I18N.write() {
        Ok(mut guard) => guard.reload(),
        Err(poisoned) => poisoned.into_inner().reload(),
    }
}

/// ユーザーが `lang/*.json` を追加する目安ディレクトリパスを返す。
#[must_use]
pub fn primary_lang_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let exe_lang = exe_dir.join("lang");
        if exe_lang.is_dir() {
            return exe_lang;
        }
        for anc in exe_dir.ancestors().take(6) {
            let candidate = anc.join("lang");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    std::env::current_dir().map_or_else(|_| PathBuf::from("lang"), |cwd| cwd.join("lang"))
}

fn default_lang_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("lang"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        for anc in exe_dir.ancestors().take(6) {
            let candidate = anc.join("lang");
            if candidate.is_dir() {
                dirs.push(candidate);
                break;
            }
        }
        dirs.push(exe_dir.join("lang"));
    }
    if let Some(user_lang) = user_config_file_path("lang") {
        dirs.push(user_lang);
    }

    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for dir in dirs {
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            deduped.push(dir);
        }
    }
    deduped
}

fn load_directory_into(packs: &mut BTreeMap<String, LoadedPack>, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut json_files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    json_files.sort();

    for path in json_files {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("custom");
        if let Ok(parsed) = parse_language_pack_str(&raw, stem, Some(path.clone())) {
            match packs.get_mut(&parsed.info.code) {
                Some(existing) => {
                    if !parsed.info.name.is_empty() {
                        existing.info.name = parsed.info.name;
                    }
                    existing.info.source_path = parsed.info.source_path;
                    for (k, v) in parsed.strings {
                        existing.strings.insert(k, v);
                    }
                }
                None => {
                    packs.insert(parsed.info.code.clone(), parsed);
                }
            }
        }
    }
}

fn parse_language_pack_str(
    raw: &str,
    fallback_code: &str,
    source_path: Option<PathBuf>,
) -> Result<LoadedPack, String> {
    let val: Value =
        serde_json::from_str(raw.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    let Some(obj) = val.as_object() else {
        return Err("language file root must be a JSON object".to_owned());
    };

    let code = obj
        .get("code")
        .and_then(Value::as_str)
        .map_or_else(|| normalize_lang_code(fallback_code), normalize_lang_code);
    if code.is_empty() {
        return Err("language code cannot be empty".to_owned());
    }
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| code.clone(), ToOwned::to_owned);

    let mut strings = BTreeMap::new();
    if let Some(nested) = obj.get("strings").and_then(Value::as_object) {
        for (k, v) in nested {
            if let Some(s) = v.as_str() {
                strings.insert(k.clone(), s.to_owned());
            }
        }
    } else {
        for (k, v) in obj {
            if matches!(k.as_str(), "code" | "name" | "$schema") {
                continue;
            }
            if let Some(s) = v.as_str() {
                strings.insert(k.clone(), s.to_owned());
            }
        }
    }

    Ok(LoadedPack {
        info: LanguageInfo {
            code,
            name,
            source_path,
        },
        strings,
    })
}

fn normalize_lang_code(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn language_preference_path() -> Option<PathBuf> {
    user_config_file_path("language.json")
}

fn load_saved_language_preference() -> Option<String> {
    let path = user_config_read_path("language.json")?;
    let raw = fs::read_to_string(path).ok()?;
    let pref: SavedLanguagePreference = serde_json::from_str(&raw).ok()?;
    let code = normalize_lang_code(&pref.active_code);
    (!code.is_empty()).then_some(code)
}

fn save_language_preference(code: &str) {
    let Some(path) = language_preference_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&SavedLanguagePreference {
        active_code: code.to_owned(),
    }) {
        let _ = fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ja_and_en_have_matching_key_sets() -> Result<(), Box<dyn std::error::Error>> {
        let ja = parse_language_pack_str(BUILTIN_JA_JSON, "ja", None)?;
        let en = parse_language_pack_str(BUILTIN_EN_JSON, "en", None)?;
        let ja_keys: Vec<&String> = ja.strings.keys().collect();
        let en_keys: Vec<&String> = en.strings.keys().collect();
        assert_eq!(
            ja_keys, en_keys,
            "ja.json and en.json keys must stay aligned"
        );
        Ok(())
    }

    #[test]
    fn dropping_custom_lang_file_registers_new_language_with_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("sea-i18n-test-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let custom_path = dir.join("ko.json");
        fs::write(
            &custom_path,
            r#"{"name":"한국어","strings":{"menu.file":"파일"}}"#,
        )?;

        let mut catalog = I18nCatalog::from_dirs(std::slice::from_ref(&dir));
        assert!(
            catalog
                .languages()
                .iter()
                .any(|l| l.code == "ko" && l.name == "한국어")
        );
        catalog.set_active("ko")?;
        assert_eq!(catalog.translate("menu.file"), "파일");
        // 未翻訳キーは en へ安全フォールバックする
        assert_eq!(catalog.translate("menu.edit"), "Edit");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
