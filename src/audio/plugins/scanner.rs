//! OS 標準パスおよび指定ディレクトリからの `.vst3` / `.clap` プラグイン静的スキャナ。

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::audio::plugins::descriptor::{
    AudioPluginDescriptor, WireAudioPluginFormat, builtin_test_gain_descriptor,
};
use crate::audio::properties::AudioPluginFormat;

/// プラグインスキャン時に検出された破損・非対応ファイルの警告情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanWarning {
    /// 問題が検出されたファイルまたはバンドルのパス。
    pub path: PathBuf,
    /// 除外された理由の説明。
    pub reason: String,
}

/// ディレクトリスキャンによって得られた有効プラグイン一覧と警告一覧。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginScanResult {
    /// 利用可能と判定されたプラグイン記述一覧。
    pub plugins: Vec<AudioPluginDescriptor>,
    /// 破損・不正フォーマット等で無視されたエントリーの警告一覧。
    pub warnings: Vec<ScanWarning>,
}

/// 現在の OS における標準 `.vst3` 検索パス一覧を返す。
#[must_use]
pub fn default_vst3_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(custom) = env::var("SEA_VST3_PATH") {
        dirs.extend(env::split_paths(&custom));
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = env::var("ProgramFiles") {
            dirs.push(PathBuf::from(pf).join("Common Files").join("VST3"));
        } else {
            dirs.push(PathBuf::from(r"C:\Program Files\Common Files\VST3"));
        }
        if let Ok(local) = env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("Common")
                    .join("VST3"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".vst3"));
        }
        dirs.push(PathBuf::from("/usr/lib/vst3"));
        dirs.push(PathBuf::from("/usr/local/lib/vst3"));
    }
    dirs
}

/// 現在の OS における標準 `.clap` 検索パス一覧を返す。
#[must_use]
pub fn default_clap_search_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(custom) = env::var("SEA_CLAP_PATH") {
        dirs.extend(env::split_paths(&custom));
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(pf) = env::var("ProgramFiles") {
            dirs.push(PathBuf::from(pf).join("Common Files").join("CLAP"));
        } else {
            dirs.push(PathBuf::from(r"C:\Program Files\Common Files\CLAP"));
        }
        if let Ok(local) = env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Programs")
                    .join("Common")
                    .join("CLAP"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/CLAP"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".clap"));
        }
        dirs.push(PathBuf::from("/usr/lib/clap"));
        dirs.push(PathBuf::from("/usr/local/lib/clap"));
    }
    dirs
}

/// OS 標準ディレクトリおよびテスト用組み込みプラグインを含むスキャン結果を返す。
#[must_use]
pub fn scan_default_plugin_locations() -> PluginScanResult {
    let vst3_dirs = default_vst3_search_paths();
    let clap_dirs = default_clap_search_paths();
    let mut result = scan_plugin_directories(&vst3_dirs, &clap_dirs);
    result.plugins.push(builtin_test_gain_descriptor());
    result
}

/// 指定された `VST3` / `CLAP` ディレクトリ群を静的検査して有効プラグイン一覧を構築する。
///
/// 動的ライブラリをメインプロセスへロードせず、拡張子・バンドル構造・実行バイナリの
/// マジックバイト (`MZ` / `\x7fELF` / `Mach-O`) を検証するため、破損ファイルでも安全に失敗する。
#[must_use]
pub fn scan_plugin_directories(vst3_dirs: &[PathBuf], clap_dirs: &[PathBuf]) -> PluginScanResult {
    let mut result = PluginScanResult::default();
    for dir in vst3_dirs {
        scan_single_directory(dir, AudioPluginFormat::Vst3, &mut result);
    }
    for dir in clap_dirs {
        scan_single_directory(dir, AudioPluginFormat::Clap, &mut result);
    }
    result
}

fn scan_single_directory(
    dir: &Path,
    expected_format: AudioPluginFormat,
    result: &mut PluginScanResult,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let expected_ext = match expected_format {
        AudioPluginFormat::Vst3 => "vst3",
        AudioPluginFormat::Clap => "clap",
        AudioPluginFormat::BuiltinTest => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let ext_matches = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case(expected_ext));
        if !ext_matches {
            continue;
        }
        match inspect_plugin_bundle(&path, expected_format) {
            Ok(desc) => result.plugins.push(desc),
            Err(reason) => result.warnings.push(ScanWarning { path, reason }),
        }
    }
}

/// 単一の `.vst3` / `.clap` ファイルまたはバンドルディレクトリを静的検証する。
///
/// # Errors
///
/// 存在しない・拡張子不一致・バンドル内バイナリ不在・バイナリマジック不正時に説明文を返す。
pub fn inspect_plugin_bundle(
    path: &Path,
    expected_format: AudioPluginFormat,
) -> Result<AudioPluginDescriptor, String> {
    let binary_path = resolve_bundle_binary_path(path)?;
    validate_native_binary_header(&binary_path)?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let wire_format = match expected_format {
        AudioPluginFormat::Vst3 => WireAudioPluginFormat::Vst3,
        AudioPluginFormat::Clap => WireAudioPluginFormat::Clap,
        AudioPluginFormat::BuiltinTest => {
            return Err("BuiltinTest plugins do not live on filesystem".to_owned());
        }
    };

    Ok(AudioPluginDescriptor {
        plugin_id: format!("{}.{stem}", expected_ext_prefix(expected_format)),
        name: stem.to_owned(),
        vendor: "External Bundle".to_owned(),
        format: wire_format,
        bundle_path: Some(path.to_string_lossy().into_owned()),
        parameters: Vec::new(),
    })
}

const fn expected_ext_prefix(format: AudioPluginFormat) -> &'static str {
    match format {
        AudioPluginFormat::Vst3 => "vst3",
        AudioPluginFormat::Clap => "clap",
        AudioPluginFormat::BuiltinTest => "builtin",
    }
}

fn resolve_bundle_binary_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.is_dir() {
        return Err("plugin path is neither a file nor a bundle directory".to_owned());
    }

    let contents = path.join("Contents");
    let candidate_dirs = [
        contents.join("x86_64-win"),
        contents.join("x86_64-linux"),
        contents.join("MacOS"),
    ];
    for sub in candidate_dirs {
        let Ok(entries) = std::fs::read_dir(&sub) else {
            continue;
        };
        for item in entries.flatten() {
            let p = item.path();
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err("bundle directory contains no native plugin binary".to_owned())
}

fn validate_native_binary_header(binary_path: &Path) -> Result<(), String> {
    let mut file =
        File::open(binary_path).map_err(|e| format!("failed to read plugin binary: {e}"))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(|_| "plugin binary is truncated or empty (< 4 bytes)".to_owned())?;

    let is_pe = magic[0] == b'M' && magic[1] == b'Z';
    let is_elf = magic == [0x7F, b'E', b'L', b'F'];
    let is_macho = matches!(
        magic,
        [0xCF | 0xCE, 0xFA, 0xED, 0xFE] | [0xFE, 0xED, 0xFA, 0xCF] | [0xCA, 0xFE, 0xBA, 0xBE]
    );
    if is_pe || is_elf || is_macho {
        Ok(())
    } else {
        Err(format!(
            "unsupported or corrupt binary magic bytes: {magic:02X?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_rejects_corrupt_binary_and_accepts_pe_header()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("sea-plugin-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;

        let corrupt_clap = dir.join("broken.clap");
        std::fs::write(&corrupt_clap, b"not-a-valid-clap-binary")?;

        let valid_vst3 = dir.join("mock_pe.vst3");
        std::fs::write(&valid_vst3, b"MZ\x90\x00dummy-pe-header")?;

        let empty_bundle = dir.join("empty_bundle.vst3");
        std::fs::create_dir_all(&empty_bundle)?;

        let res = scan_plugin_directories(std::slice::from_ref(&dir), std::slice::from_ref(&dir));
        assert_eq!(res.plugins.len(), 1);
        assert_eq!(res.plugins[0].name, "mock_pe");
        assert!(res.warnings.len() >= 2);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
