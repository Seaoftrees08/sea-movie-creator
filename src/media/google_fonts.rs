//! 標準フォント・OS 導入フォントカタログ管理およびユーザー明示操作による Google Fonts 取得ヘルパー。

use std::collections::HashSet;
use std::fs;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::core::user_config_file_path;

/// コンボボックス上位へ優先表示する標準フォント・総称ファミリ一覧。
pub const CURATED_STANDARD_FONTS: &[&str] = &[
    "sans-serif",
    "serif",
    "monospace",
    "Yu Gothic UI",
    "Yu Gothic",
    "Meiryo",
    "Meiryo UI",
    "MS PGothic",
    "MS Gothic",
    "BIZ UDPGothic",
    "BIZ UDGothic",
    "Yu Mincho",
    "MS PMincho",
    "MS Mincho",
    "UD Digi Kyokasho N-R",
    "Segoe UI",
    "Arial",
    "Georgia",
    "Times New Roman",
    "Consolas",
];

/// ワンクリック導入・補完候補として内蔵する代表的なオープンソース Google Fonts (SIL OFL / Apache-2.0) 一覧。
pub const CURATED_GOOGLE_FONTS: &[&str] = &[
    "Noto Sans JP",
    "Noto Serif JP",
    "M PLUS 1p",
    "M PLUS Rounded 1c",
    "Zen Kaku Gothic New",
    "Zen Maru Gothic",
    "Kosugi Maru",
    "Kaisei Decol",
    "Klee One",
    "Shippori Mincho",
    "Dela Gothic One",
    "DotGothic16",
    "Yusei Magic",
    "Hachi Maru Pop",
    "Reggae One",
    "Stick",
    "RocknRoll One",
    "Potta One",
    "Roboto",
    "Open Sans",
    "Inter",
    "Montserrat",
    "Oswald",
];

/// コンボボックスに表示するフォント候補の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSourceKind {
    /// OS 標準・汎用ファミリ（かつローカル利用可能）。
    Standard,
    /// OS へ導入済みのローカルフォント。
    Installed,
    /// アプリ内のローカル Google Fonts キャッシュ (`fonts/`) へ導入済み。
    GoogleCached,
    /// まだローカル未導入の Google Fonts 候補（ユーザー操作で取得可能）。
    GoogleAvailable,
}

/// フォント選択コンボボックスへ表示する 1 件の候補情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontCatalogEntry {
    /// `SubtitleProperties::font_family` に保存されるファミリ名。
    pub family: String,
    /// 出所区分。
    pub source: FontSourceKind,
    /// ローカルで現在即座に描画可能か。
    pub is_local: bool,
}

/// Google Fonts から取得・保存したフォントファイル群の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedGoogleFont {
    /// 正規化後のファミリ名。
    pub family: String,
    /// 保存した `.ttf`/`.otf` パス一覧。
    pub saved_paths: Vec<PathBuf>,
    /// `FontSystem` へ即座に読み込むための各フォントバイナリ。
    pub font_bytes: Vec<Vec<u8>>,
}

/// ユーザー導入済みダウンロードフォントを保存するディレクトリ (`%APPDATA%/sea-movie-creator/fonts`) を返す。
#[must_use]
pub fn user_downloaded_fonts_dir() -> Option<PathBuf> {
    user_config_file_path("fonts")
}

/// OS 導入済みフォント名一覧と標準プリセット・Google Fonts 候補を統合して重複なく整列する。
#[must_use]
pub fn build_font_catalog<S: BuildHasher>(
    installed_families: &[String],
    cached_families: &HashSet<String, S>,
) -> Vec<FontCatalogEntry> {
    let installed_lower: HashSet<String> = installed_families
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    let cached_lower: HashSet<String> = cached_families
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();

    let mut seen_lower = HashSet::new();
    let mut entries = Vec::new();

    for &std_name in CURATED_STANDARD_FONTS {
        let lower = std_name.to_ascii_lowercase();
        let is_generic = matches!(lower.as_str(), "sans-serif" | "serif" | "monospace");
        let is_installed = is_generic || installed_lower.contains(&lower);
        let is_cached = cached_lower.contains(&lower);
        if is_installed || is_cached {
            seen_lower.insert(lower);
            entries.push(FontCatalogEntry {
                family: std_name.to_owned(),
                source: if is_cached {
                    FontSourceKind::GoogleCached
                } else {
                    FontSourceKind::Standard
                },
                is_local: true,
            });
        }
    }

    let mut extra_installed: Vec<String> = installed_families
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    extra_installed.sort_by_key(|a| a.to_ascii_lowercase());
    for fam in extra_installed {
        let lower = fam.to_ascii_lowercase();
        if !seen_lower.insert(lower.clone()) {
            continue;
        }
        let source = if cached_lower.contains(&lower) {
            FontSourceKind::GoogleCached
        } else {
            FontSourceKind::Installed
        };
        entries.push(FontCatalogEntry {
            family: fam,
            source,
            is_local: true,
        });
    }

    for &gf_name in CURATED_GOOGLE_FONTS {
        let lower = gf_name.to_ascii_lowercase();
        if !seen_lower.insert(lower) {
            continue;
        }
        entries.push(FontCatalogEntry {
            family: gf_name.to_owned(),
            source: FontSourceKind::GoogleAvailable,
            is_local: false,
        });
    }

    entries
}

/// CSS2 レスポンス文字列から安全な Google Fonts の TTF/OTF `url(...)` を抽出する。
#[must_use]
pub fn extract_css2_font_urls(css: &str) -> Vec<String> {
    let mut ttf_or_otf = Vec::new();
    let mut rest = css;
    while let Some(start) = rest.find("url(") {
        let after = &rest[start + 4..];
        let Some(end) = after.find(')') else {
            break;
        };
        let raw = after[..end].trim().trim_matches(|c| c == '\'' || c == '"');
        let lower = raw.to_ascii_lowercase();
        let is_font_ext = Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"));
        if lower.starts_with("https://fonts.gstatic.com/")
            && is_font_ext
            && !ttf_or_otf.iter().any(|u| u == raw)
        {
            ttf_or_otf.push(raw.to_owned());
        }
        rest = &after[end + 1..];
    }
    ttf_or_otf
}

/// Google Fonts 公開メタデータ JSON（先頭 XSSI 防御行 `)]}'` 対応）からファミリ名一覧を取り出す。
#[must_use]
pub fn parse_google_fonts_metadata_families(body: &str) -> Vec<String> {
    let trimmed = body.trim_start();
    let json_text = trimmed
        .strip_prefix(")]}'")
        .map_or(trimmed, |after| after.trim_start());
    let Ok(val) = serde_json::from_str::<Value>(json_text) else {
        return Vec::new();
    };
    let Some(list) = val.get("familyMetadataList").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in list {
        if let Some(fam) = item.get("family").and_then(Value::as_str) {
            let clean = fam.trim();
            if !clean.is_empty() {
                out.push(clean.to_owned());
            }
        }
    }
    out
}

/// ユーザーの明示操作に応じて Google Fonts メタデータを検索し、一致するファミリ名一覧を返す。
///
/// オフラインや通信失敗時はエラー詳細を返し、内蔵リストで補完可能な旨を呼び出し元へ通知する。
///
/// # Errors
///
/// 検索クエリが空、または HTTP リクエスト・応答解析に失敗した場合。
pub fn fetch_google_fonts_catalog(query: &str, limit: usize) -> Result<Vec<String>, String> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(CURATED_GOOGLE_FONTS
            .iter()
            .take(limit.max(1))
            .map(|s| (*s).to_owned())
            .collect());
    }
    let agent = build_http_agent(6);
    let mut resp = agent
        .get("https://fonts.google.com/metadata/fonts")
        .header("User-Agent", "sea-movie-creator/0.1")
        .call()
        .map_err(|e| format!("Google Fonts カタログ取得失敗: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!("Google Fonts カタログ HTTP {status}"));
    }
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Google Fonts 応答読込失敗: {e}"))?;
    let all = parse_google_fonts_metadata_families(&text);
    if all.is_empty() {
        return Err("Google Fonts メタデータの解析結果が空でした".to_owned());
    }
    let mut matches: Vec<String> = all
        .into_iter()
        .filter(|fam| fam.to_ascii_lowercase().contains(&needle))
        .collect();
    matches.truncate(limit.max(1));
    Ok(matches)
}

/// 指定 Google Font ファミリをユーザー明示指示のもと TTF/OTF ダウンロードしてローカル保存する。
///
/// # Errors
///
/// フォント名不正、HTTP 失敗、TTF/OTF URL 不在、または保存ディレクトリ書き込みエラー時。
pub fn download_google_font_ttf(
    family: &str,
    dest_dir: &Path,
) -> Result<DownloadedGoogleFont, String> {
    let trimmed = family.trim();
    if trimmed.is_empty() {
        return Err("フォント名が空です".to_owned());
    }
    let encoded = encode_google_font_query(trimmed);
    let agent = build_http_agent(12);
    let css = fetch_css2_with_fallback(&agent, &encoded)?;
    let urls = extract_css2_font_urls(&css);
    if urls.is_empty() {
        return Err(format!(
            "Google Fonts CSS にダウンロード可能な TTF/OTF URL が見つかりませんでした: {trimmed}"
        ));
    }

    fs::create_dir_all(dest_dir).map_err(|e| {
        format!(
            "フォント保存ディレクトリ作成失敗 {}: {e}",
            dest_dir.display()
        )
    })?;

    let stem = sanitize_font_file_stem(trimmed);
    let mut saved_paths = Vec::new();
    let mut font_bytes = Vec::new();
    for (idx, url) in urls.into_iter().take(2).enumerate() {
        let mut font_resp = agent
            .get(&url)
            .header("User-Agent", "curl/8.0")
            .call()
            .map_err(|e| format!("フォントファイル取得失敗: {e}"))?;
        if font_resp.status().as_u16() >= 400 {
            return Err(format!(
                "フォントファイル取得 HTTP {}",
                font_resp.status().as_u16()
            ));
        }
        let bytes = font_resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| format!("フォントデータ読込失敗: {e}"))?;
        if !looks_like_sfnt_font(&bytes) {
            return Err("取得したバイナリが有効な TTF/OTF フォントではありません".to_owned());
        }
        let ext = if url.to_ascii_lowercase().ends_with(".otf") {
            "otf"
        } else {
            "ttf"
        };
        let path = dest_dir.join(format!("{stem}-{idx}.{ext}"));
        fs::write(&path, &bytes)
            .map_err(|e| format!("フォント保存失敗 {}: {e}", path.display()))?;
        saved_paths.push(path);
        font_bytes.push(bytes);
    }

    Ok(DownloadedGoogleFont {
        family: trimmed.to_owned(),
        saved_paths,
        font_bytes,
    })
}

fn fetch_css2_with_fallback(agent: &ureq::Agent, encoded_family: &str) -> Result<String, String> {
    let urls = [
        format!("https://fonts.googleapis.com/css2?family={encoded_family}:wght@400;700"),
        format!("https://fonts.googleapis.com/css2?family={encoded_family}"),
    ];
    let mut last_err = String::new();
    for css_url in urls {
        match agent.get(&css_url).header("User-Agent", "curl/8.0").call() {
            Ok(mut resp) if resp.status().as_u16() < 400 => {
                return resp
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| format!("Google Fonts CSS 読込失敗: {e}"));
            }
            Ok(resp) => {
                last_err = format!(
                    "Google Fonts CSS HTTP {} ({css_url})",
                    resp.status().as_u16()
                );
            }
            Err(e) => {
                last_err = format!("Google Fonts CSS 通信失敗: {e}");
            }
        }
    }
    Err(last_err)
}

fn build_http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs.max(3))))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Google Fonts CSS2 クエリパラメータ用に空白を `+`、非 ASCII・記号を `%XX` へ変換する。
#[must_use]
pub fn encode_google_font_query(family: &str) -> String {
    let mut out = String::with_capacity(family.len());
    for byte in family.trim().bytes() {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(char::from(byte));
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// パストラバーサルを防止した安全なフォントファイル名ベースを生成する。
#[must_use]
pub fn sanitize_font_file_stem(family: &str) -> String {
    let cleaned: String = family
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "google_font".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[must_use]
fn looks_like_sfnt_font(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    matches!(
        &bytes[..4],
        [0x00, 0x01, 0x00, 0x00] | b"OTTO" | b"true" | b"ttcf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css2_parser_extracts_gstatic_ttf_urls_only() {
        let css = r"
@font-face {
  font-family: 'Noto Sans JP';
  src: url(https://fonts.gstatic.com/s/notosansjp/v52/test.ttf) format('truetype');
}
@font-face {
  font-family: 'Other';
  src: url(https://evil.example.com/fake.ttf);
}
@font-face {
  font-family: 'WoffOnly';
  src: url(https://fonts.gstatic.com/s/test/font.woff2) format('woff2');
}
";
        let urls = extract_css2_font_urls(css);
        assert_eq!(
            urls,
            vec!["https://fonts.gstatic.com/s/notosansjp/v52/test.ttf".to_owned()]
        );
    }

    #[test]
    fn metadata_parser_strips_xssi_prefix_and_reads_families() {
        let raw = ")]}'\n{\"familyMetadataList\":[{\"family\":\"Noto Sans JP\"},{\"family\":\"Zen Maru Gothic\"}]}";
        let families = parse_google_fonts_metadata_families(raw);
        assert_eq!(families, vec!["Noto Sans JP", "Zen Maru Gothic"]);
    }

    #[test]
    fn sanitize_font_stem_blocks_traversal() {
        assert_eq!(sanitize_font_file_stem("../../evil font/x"), "evil_font_x");
        assert_eq!(encode_google_font_query("Noto Sans JP"), "Noto+Sans+JP");
    }

    #[test]
    fn build_catalog_prioritizes_standard_then_installed_then_google() {
        let installed = vec!["Yu Gothic UI".to_owned(), "Custom Studio Font".to_owned()];
        let cached = HashSet::from(["Zen Maru Gothic".to_owned()]);
        let entries = build_font_catalog(&installed, &cached);
        assert!(entries.iter().any(|e| {
            e.family == "sans-serif" && e.source == FontSourceKind::Standard && e.is_local
        }));
        assert!(entries.iter().any(|e| {
            e.family == "Custom Studio Font" && e.source == FontSourceKind::Installed && e.is_local
        }));
        assert!(entries.iter().any(|e| {
            e.family == "Noto Sans JP" && e.source == FontSourceKind::GoogleAvailable && !e.is_local
        }));
    }
}
