//! プロジェクト JSON の永続化・読込およびアセットパス解決（相対優先＋絶対フォールバック）。

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::timeline::document::{
    AssetKind, AssetPath, AssetStatus, DocumentValidationError, ProjectDocument,
};

/// プロジェクトの保存・読込時に発生し得るエラー。
#[derive(Debug, Error)]
pub enum SerializationError {
    /// ファイル I/O エラー。
    #[error("project file I/O failed: {0}")]
    Io(#[from] io::Error),
    /// JSON の直列化・逆直列化エラー。
    #[error("project JSON codec failed: {0}")]
    Json(#[from] serde_json::Error),
    /// 読込または保存時のドキュメント検証エラー。
    #[error("project validation failed: {0}")]
    Validation(#[from] DocumentValidationError),
}

/// [`ProjectDocument`] を検証し、整形済み JSON 文字列へ直列化する。
///
/// # Errors
///
/// ドキュメントの不変条件に違反している場合、または JSON 変換に失敗した場合はエラーを返す。
pub fn serialize_project_to_string(
    document: &ProjectDocument,
) -> Result<String, SerializationError> {
    document.validate()?;
    Ok(serde_json::to_string_pretty(document)?)
}

/// JSON 文字列から [`ProjectDocument`] を読み込み、整合性検証を行う。
///
/// # Errors
///
/// JSON 構文不正またはドキュメントの不変条件違反がある場合はエラーを返す。
pub fn deserialize_project_from_str(json: &str) -> Result<ProjectDocument, SerializationError> {
    let document: ProjectDocument = serde_json::from_str(json)?;
    document.validate()?;
    Ok(document)
}

/// [`ProjectDocument`] を指定ファイルパスへ保存する（親ディレクトリが必要なら作成）。
///
/// # Errors
///
/// 検証エラー、ディレクトリ作成/書き込み失敗時にエラーを返す。
pub fn save_project_file(
    document: &ProjectDocument,
    path: &Path,
) -> Result<(), SerializationError> {
    let json = serialize_project_to_string(document)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)?;
    Ok(())
}

/// 指定ファイルパスから [`ProjectDocument`] を読み込み、全素材のリンク状態を再解決する。
///
/// 読込時はプロジェクトファイルのあるディレクトリからの相対パスを優先確認し、
/// 見つからない場合は絶対パスへフォールバックする。どちらも見つからない場合でも
/// 読込自体は失敗させず [`AssetStatus::Missing`] を設定してグレースフルに開く。
///
/// # Errors
///
/// ファイル読込・JSON パース・ドキュメント構造検証に失敗した場合にエラーを返す。
pub fn load_project_file(path: &Path) -> Result<ProjectDocument, SerializationError> {
    let content = fs::read_to_string(path)?;
    let mut document = deserialize_project_from_str(&content)?;
    let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    resolve_document_assets(&mut document, project_dir);
    Ok(document)
}

/// ドキュメント内の全アセットの解決先を判定し、`status` および画像の実寸を最新化する。
pub fn resolve_document_assets(document: &mut ProjectDocument, project_dir: Option<&Path>) {
    for asset in &mut document.assets {
        match resolve_existing_asset_path(&asset.path, project_dir) {
            Some(resolved_path) => {
                asset.status = AssetStatus::Available;
                // 旧仕様等で EXIF 回転前の寸法が保存されたままの .seaproj を読込時に正立寸法へ同期する
                if let AssetKind::Image { width, height } = &mut asset.kind
                    && let Ok(info) = crate::media::image_loader::inspect_image_file(&resolved_path)
                    && info.width > 0
                    && info.height > 0
                    && (*width != info.width || *height != info.height)
                {
                    *width = info.width;
                    *height = info.height;
                }
            }
            None => {
                asset.status = AssetStatus::Missing {
                    expected_path: preferred_display_path(&asset.path, project_dir),
                };
            }
        }
    }
}

/// 単一素材について「プロジェクト基準の相対パス -> 絶対パス」の順で存在確認し、見つかった実体パスを返す。
#[must_use]
pub fn resolve_existing_asset_path(
    asset_path: &AssetPath,
    project_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let (Some(dir), Some(rel)) = (project_dir, asset_path.relative.as_deref())
        && !rel.trim().is_empty()
    {
        let candidate = dir.join(rel);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    if let Some(abs) = asset_path.absolute.as_deref()
        && !abs.trim().is_empty()
    {
        let candidate = PathBuf::from(abs);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// 対象ファイルとプロジェクトディレクトリから、`relative` 優先・`absolute` 保持の [`AssetPath`] を構築する。
#[must_use]
pub fn build_asset_path(target_file: &Path, project_dir: Option<&Path>) -> AssetPath {
    let absolute = Some(target_file.to_string_lossy().into_owned());
    let relative = project_dir.and_then(|dir| compute_relative_path(dir, target_file));
    AssetPath::new(relative, absolute)
}

fn preferred_display_path(asset_path: &AssetPath, project_dir: Option<&Path>) -> String {
    if let (Some(dir), Some(rel)) = (project_dir, asset_path.relative.as_deref())
        && !rel.trim().is_empty()
    {
        return dir.join(rel).to_string_lossy().into_owned();
    }
    asset_path
        .absolute
        .clone()
        .or_else(|| asset_path.relative.clone())
        .unwrap_or_default()
}

fn compute_relative_path(base_dir: &Path, target: &Path) -> Option<String> {
    let base_parts: Vec<Component<'_>> = base_dir.components().collect();
    let target_parts: Vec<Component<'_>> = target.components().collect();

    // プレフィックス（Windows ドライブ文字や RootDir）が異なる場合は相対表現不可
    let common_len = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(a, b)| a == b)
        .count();
    if common_len == 0 && (base_dir.is_absolute() || target.is_absolute()) {
        return None;
    }

    let mut rel = PathBuf::new();
    for component in &base_parts[common_len..] {
        if matches!(component, Component::Normal(_)) {
            rel.push("..");
        }
    }
    for component in &target_parts[common_len..] {
        rel.push(component.as_os_str());
    }

    Some(rel.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::AssetId;
    use crate::timeline::document::{Asset, AssetKind};

    #[test]
    fn compute_relative_path_builds_portable_forward_slashes() {
        let project_dir = Path::new("C:/workspace/proj_a");
        let media = Path::new("C:/workspace/proj_a/assets/clip.mkv");
        assert_eq!(
            compute_relative_path(project_dir, media).as_deref(),
            Some("assets/clip.mkv")
        );

        let shared = Path::new("C:/workspace/shared/clip.mkv");
        assert_eq!(
            compute_relative_path(project_dir, shared).as_deref(),
            Some("../shared/clip.mkv")
        );
    }

    #[test]
    fn missing_asset_is_marked_gracefully_without_failing_load()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Missing Asset Check");
        doc.assets.push(Asset {
            id: AssetId::new("a1")?,
            name: "missing.png".to_owned(),
            path: AssetPath::new(Some("nonexistent_rel.png".to_owned()), None),
            kind: AssetKind::Image {
                width: 64,
                height: 64,
            },
            status: AssetStatus::Available,
        });

        resolve_document_assets(&mut doc, Some(Path::new(".")));
        assert!(matches!(doc.assets[0].status, AssetStatus::Missing { .. }));
        Ok(())
    }

    #[test]
    fn resolve_document_assets_syncs_exif_oriented_image_dimensions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("sea_ser_exif_{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let jpg_path = dir.join("portrait_exif.jpg");

        let img = image::DynamicImage::new_rgb8(20, 10);
        let mut cursor = io::Cursor::new(Vec::new());
        img.write_to(&mut cursor, image::ImageFormat::Jpeg)
            .map_err(io::Error::other)?;
        let raw = cursor.into_inner();
        let tiff: [u8; 26] = [
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let seg_len = u16::try_from(6 + tiff.len() + 2).unwrap_or(0);
        let mut out = Vec::new();
        out.extend_from_slice(&raw[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff);
        out.extend_from_slice(&raw[2..]);
        fs::write(&jpg_path, out)?;

        let mut doc = ProjectDocument::new("EXIF Sync");
        doc.assets.push(Asset {
            id: AssetId::new("a1")?,
            name: "portrait_exif.jpg".to_owned(),
            path: AssetPath::new(Some("portrait_exif.jpg".to_owned()), None),
            kind: AssetKind::Image {
                width: 20,
                height: 10,
            },
            status: AssetStatus::Missing {
                expected_path: String::new(),
            },
        });
        let proj_path = dir.join("proj.seaproj");
        save_project_file(&doc, &proj_path)?;

        let loaded = load_project_file(&proj_path)?;
        assert_eq!(
            loaded.assets[0].kind,
            AssetKind::Image {
                width: 10,
                height: 20,
            }
        );
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
