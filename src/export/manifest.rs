//! 書き出しプラグインの `plugin.json` マニフェスト定義・検証・読み込みモジュール。

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::export::protocol::{AudioSampleFormat, PixelFormat};

/// 現在のプラグインマニフェスト仕様バージョン。
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// プラグインが担う機能種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// 動画ファイルへの書き出し（エンコード・マルチプレクス）プラグイン。
    Export,
}

/// プラグインおよび関連コンテナ／コーデックのライセンス・外部依存開示情報。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLicenseInfo {
    /// SPDX ライセンス識別子（例: `"MIT OR Apache-2.0"`, `"MIT"`）。
    pub spdx: String,
    /// ダイアログ上へ表示するライセンスまたは外部バイナリ利用上の注意書き。
    #[serde(default)]
    pub notice: Option<String>,
    /// ユーザー環境上の外部コマンド（例: `ffmpeg`）等を要求するかどうか。
    #[serde(default)]
    pub requires_external_binary: bool,
}

/// プラグインが処理可能な入出力形式やハードウェア加速能力の宣言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCapabilities {
    /// 透過（アルファチャンネル付き映像）出力に対応するか。
    #[serde(default)]
    pub supports_alpha: bool,
    /// 音声トラックのエンコード・多重化に対応するか。
    #[serde(default = "default_true")]
    pub supports_audio: bool,
    /// ホストから受け取る共有メモリピクセル形式。
    #[serde(default = "default_pixel_format")]
    pub pixel_format: PixelFormat,
    /// ホストから受け取る音声 PCM 形式。
    #[serde(default = "default_audio_format")]
    pub audio_sample_format: AudioSampleFormat,
    /// OS 標準 API 等によるハードウェア支援エンコードを利用するか。
    #[serde(default)]
    pub hw_accelerated: bool,
}

const fn default_true() -> bool {
    true
}

const fn default_pixel_format() -> PixelFormat {
    PixelFormat::Bgra8
}

const fn default_audio_format() -> AudioSampleFormat {
    AudioSampleFormat::F32Interleaved
}

/// プラグインが提供する書き出しプリセット定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetSchema {
    /// 一意なプリセット識別子（例: `"av1_balanced"`）。
    pub id: String,
    /// UI 表示用ラベル。
    pub label: String,
    /// プリセットの用途説明。
    #[serde(default)]
    pub description: String,
    /// 映像コーデック名（表示用、例: `"AV1 (rav1e)"`）。
    pub video_codec: String,
    /// 音声コーデック名（表示用、例: `"FLAC"`）。
    pub audio_codec: String,
    /// 推奨出力ファイル拡張子（ドットなし小文字、例: `"mkv"`）。
    pub container_ext: String,
    /// プリセット選択時に適用される既定 JSON オプション値。
    #[serde(default)]
    pub default_options: serde_json::Value,
}

/// 選択選択肢 1 項目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionChoice {
    /// JSON オプションに格納される内部値。
    pub value: String,
    /// UI 表示用ラベル。
    pub label: String,
}

/// UI ダイアログで調整可能なプラグイン固有オプション項目の型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExportOptionKind {
    /// 真偽値トグル。
    Boolean,
    /// 整数スライダ／入力。
    Integer {
        /// 最小値。
        min: i64,
        /// 最大値。
        max: i64,
    },
    /// 定義済み候補からの選択。
    Select {
        /// 選択候補一覧。
        choices: Vec<OptionChoice>,
    },
    /// 任意文字列入力（外部コマンドパス指定など）。
    String,
}

/// UI ダイアログに動的に表示するためのオプション項目メタデータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportOptionField {
    /// `options` JSON オブジェクト上のキー名。
    pub key: String,
    /// UI 表示用ラベル。
    pub label: String,
    /// 補足説明。
    #[serde(default)]
    pub description: String,
    /// 入力 UI 種別。
    pub kind: ExportOptionKind,
    /// 既定値。
    #[serde(default)]
    pub default_value: serde_json::Value,
}

/// 書き出しプラグインのマニフェスト (`plugin.json`) ルート構造。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportPluginManifest {
    /// マニフェスト形式バージョン（`1` 固定）。
    pub schema_version: u32,
    /// 一意なプラグイン識別子（例: `"sea.export.av1"`）。
    pub id: String,
    /// 表示名。
    pub name: String,
    /// セマンティックバージョン文字列。
    pub version: String,
    /// プラグイン種別（現在は `export` のみ）。
    pub kind: PluginKind,
    /// 実行ファイル名またはプラグインディレクトリ内相対パス。
    pub entrypoint: String,
    /// プラグイン起動時の固定追加引数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 対応コンテナ拡張子一覧（例: `["mkv"]`）。
    pub containers: Vec<String>,
    /// ライセンス・特許／外部依存注意表示情報。
    pub license: PluginLicenseInfo,
    /// 入出力フォーマットおよびハードウェア対応宣言。
    pub capabilities: PluginCapabilities,
    /// 利用可能なプリセット一覧。
    pub presets: Vec<PresetSchema>,
    /// ユーザーが設定変更可能な追加オプション項目定義。
    #[serde(default)]
    pub option_fields: Vec<ExportOptionField>,
}

/// マニフェスト解析・検証エラー。
#[derive(Debug, Error)]
pub enum ManifestError {
    /// ファイル読み込み I/O エラー。
    #[error("failed to read plugin manifest '{path}': {source}")]
    Io {
        /// エラーが発生したパス。
        path: PathBuf,
        /// 元の I/O エラー。
        source: std::io::Error,
    },
    /// JSON デコードエラー。
    #[error("failed to parse plugin manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// 必須項目不足・不正パスなどの仕様違反。
    #[error("invalid plugin manifest: {0}")]
    Validation(String),
}

impl ExportPluginManifest {
    /// JSON 文字列からマニフェストをパースし、整合性検証を行って返す。
    ///
    /// # Errors
    ///
    /// JSON 不正または必須フィールド検証違反時に [`ManifestError`] を返す。
    pub fn parse_str(json: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_str(json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// ファイルシステム上の `plugin.json` を読み込んで検証する。
    ///
    /// # Errors
    ///
    /// ファイル読み込み・JSON パース・検証に失敗した場合 [`ManifestError`] を返す。
    pub fn from_file(path: &Path) -> Result<Self, ManifestError> {
        let raw = fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_str(&raw)
    }

    /// マニフェストの各フィールドが期待仕様を満たしているか検証する。
    ///
    /// # Errors
    ///
    /// 仕様違反が見つかった場合 [`ManifestError::Validation`] を返す。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Validation(format!(
                "unsupported schema_version {}, expected {MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err(ManifestError::Validation(
                "plugin id and name must not be empty".to_owned(),
            ));
        }
        validate_safe_entrypoint(&self.entrypoint)?;

        if self.containers.is_empty() || self.containers.iter().any(|ext| ext.trim().is_empty()) {
            return Err(ManifestError::Validation(
                "plugin must declare at least one non-empty container extension".to_owned(),
            ));
        }
        if self.presets.is_empty() {
            return Err(ManifestError::Validation(
                "plugin must declare at least one preset".to_owned(),
            ));
        }
        let mut seen_presets = HashSet::new();
        for preset in &self.presets {
            if preset.id.trim().is_empty() || preset.container_ext.trim().is_empty() {
                return Err(ManifestError::Validation(
                    "preset id and container_ext must not be empty".to_owned(),
                ));
            }
            if !seen_presets.insert(preset.id.as_str()) {
                return Err(ManifestError::Validation(format!(
                    "duplicate preset id '{}'",
                    preset.id
                )));
            }
        }
        Ok(())
    }

    /// プリセット ID に一致する定義を返す。
    #[must_use]
    pub fn find_preset(&self, preset_id: &str) -> Option<&PresetSchema> {
        self.presets.iter().find(|p| p.id == preset_id)
    }

    /// 先頭の既定プリセットを返す。
    #[must_use]
    pub fn default_preset(&self) -> Option<&PresetSchema> {
        self.presets.first()
    }
}

fn validate_safe_entrypoint(entrypoint: &str) -> Result<(), ManifestError> {
    let trimmed = entrypoint.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::Validation(
            "entrypoint must not be empty".to_owned(),
        ));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(ManifestError::Validation(format!(
            "entrypoint '{trimmed}' must be relative, not absolute"
        )));
    }
    for comp in path.components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(ManifestError::Validation(format!(
                "entrypoint '{trimmed}' cannot traverse outside plugin directory"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest_json(entrypoint: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "id": "sea.export.av1",
            "name": "AV1 + FLAC (Pure Rust)",
            "version": "0.1.0",
            "kind": "export",
            "entrypoint": entrypoint,
            "containers": ["mkv"],
            "license": {
                "spdx": "BSD-2-Clause / Apache-2.0",
                "notice": null,
                "requires_external_binary": false
            },
            "capabilities": {
                "supports_alpha": false,
                "supports_audio": true,
                "pixel_format": "bgra8",
                "audio_sample_format": "f32_interleaved",
                "hw_accelerated": false
            },
            "presets": [
                {
                    "id": "av1_fast",
                    "label": "AV1 高速",
                    "description": "速度優先の SW エンコード",
                    "video_codec": "AV1 (rav1e)",
                    "audio_codec": "FLAC",
                    "container_ext": "mkv",
                    "default_options": { "speed": 9, "quantizer": 110 }
                }
            ]
        })
        .to_string()
    }

    #[test]
    fn valid_manifest_parses_and_rejects_traversal_and_bad_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let parsed = ExportPluginManifest::parse_str(&sample_manifest_json("sea-export-av1"))?;
        assert_eq!(parsed.id, "sea.export.av1");
        assert_eq!(
            parsed.default_preset().map(|p| p.id.as_str()),
            Some("av1_fast")
        );

        assert!(ExportPluginManifest::parse_str(&sample_manifest_json("../evil")).is_err());

        let bad_version = sample_manifest_json("sea-export-av1")
            .replace("\"schema_version\":1", "\"schema_version\":99");
        assert!(ExportPluginManifest::parse_str(&bad_version).is_err());
        Ok(())
    }
}
