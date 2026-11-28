//! ユーザー許可フォルダ内の画像・音声・動画・メモ走査、EXIF 撮影日時抽出、Vision 用軽量サムネイル生成。

use std::cmp::Ordering;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::UNIX_EPOCH;

use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, DynamicImage, ImageEncoder as _, Limits, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ai::provider::encode_base64;
use crate::media::decoder::{
    BackendPreference, decode_oriented_dynamic_image_file, open_audio_decoder, probe_media,
};
use crate::media::image_loader::inspect_image_file;

const MAX_SCAN_DEPTH: usize = 4;
const MAX_SCAN_FILES: usize = 500;
const MAX_NOTE_FILES: usize = 10;
const MAX_NOTE_BYTES: usize = 64 * 1024;
const EXIF_HEADER_READ_LIMIT: usize = 64 * 1024;
const MAX_THUMB_DECODE_DIM: u32 = 16_384;
const MAX_THUMB_DECODE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// フォルダ走査・サムネイル生成時のエラー。
#[derive(Debug, Error)]
pub enum MediaInspectorError {
    /// 許可されていないパス、またはディレクトリトラバーサルが検出された。
    #[error("path access denied (outside allowed folder scope): {0}")]
    AccessDenied(String),
    /// 対象ディレクトリまたはファイルが存在しない。
    #[error("target path does not exist or is not valid: {0}")]
    NotFound(String),
    /// ファイル I/O エラー。
    #[error("media inspector I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// 画像デコードまたはサムネイルエンコードに失敗した。
    #[error("image processing failed: {0}")]
    ImageFailed(String),
}

/// ユーザーがチャットへ添付／明示したフォルダ群、およびプロジェクト登録済みファイルのみへ読み取りを制限するアクセス境界。
#[derive(Debug, Clone, Default)]
pub struct AllowedFolderScope {
    roots: Vec<PathBuf>,
    allowed_files: Vec<PathBuf>,
}

impl AllowedFolderScope {
    /// 許可するルートディレクトリ群を正規化して構築する。
    #[must_use]
    pub fn from_folders<I, P>(folders: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::from_folders_and_files(folders, std::iter::empty::<PathBuf>())
    }

    /// 許可するルートディレクトリ群および個別許可ファイル群を正規化して構築する。
    ///
    /// `files` に登録された項目は同一ディレクトリ内の兄弟ファイルへ波及せず、完全一致のファイルのみ読み取りを許可する。
    #[must_use]
    pub fn from_folders_and_files<IF, PF, IFile, PFile>(folders: IF, files: IFile) -> Self
    where
        IF: IntoIterator<Item = PF>,
        PF: AsRef<Path>,
        IFile: IntoIterator<Item = PFile>,
        PFile: AsRef<Path>,
    {
        let mut roots = Vec::new();
        for folder in folders {
            if let Ok(canon) = fs::canonicalize(folder.as_ref())
                && canon.is_dir()
                && !roots.contains(&canon)
            {
                roots.push(canon);
            }
        }
        let mut allowed_files = Vec::new();
        for file in files {
            if let Ok(canon) = fs::canonicalize(file.as_ref())
                && canon.is_file()
                && !allowed_files.contains(&canon)
            {
                allowed_files.push(canon);
            }
        }
        Self {
            roots,
            allowed_files,
        }
    }

    /// 登録されている正規化済みルート一覧を返す。
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 登録されている正規化済み個別許可ファイル一覧を返す。
    #[must_use]
    pub fn allowed_files(&self) -> &[PathBuf] {
        &self.allowed_files
    }

    /// 対象パスが許可ルート配下または個別許可ファイルに実在するか検証し、正規化済みパスを返す。
    ///
    /// # Errors
    ///
    /// 対象が存在しない場合、または許可されたいずれのルート・ファイルにも含まれない場合。
    pub fn validate_path(&self, target: &Path) -> Result<PathBuf, MediaInspectorError> {
        if self.roots.is_empty() && self.allowed_files.is_empty() {
            return Err(MediaInspectorError::AccessDenied(
                target.display().to_string(),
            ));
        }
        // Windows 環境で許可外 UNC やデバイス/Verbatim 名前空間へ fs::canonicalize 前にアクセスするのを防ぐ
        if let Some(Component::Prefix(prefix_comp)) = target.components().next() {
            match prefix_comp.kind() {
                Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => {
                    let target_unc = unc_server_share(target);
                    let same_unc_allowed = target_unc.is_some_and(|t_unc| {
                        self.roots
                            .iter()
                            .chain(self.allowed_files.iter())
                            .filter_map(|allowed| unc_server_share(allowed))
                            .any(|allowed_unc| allowed_unc == t_unc)
                    });
                    if !same_unc_allowed {
                        return Err(MediaInspectorError::AccessDenied(
                            target.display().to_string(),
                        ));
                    }
                }
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                    return Err(MediaInspectorError::AccessDenied(
                        target.display().to_string(),
                    ));
                }
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
            }
        }
        let canon = fs::canonicalize(target)
            .map_err(|_| MediaInspectorError::NotFound(target.display().to_string()))?;
        if self.roots.iter().any(|root| canon.starts_with(root))
            || self.allowed_files.iter().any(|file| file == &canon)
        {
            return Ok(canon);
        }
        Err(MediaInspectorError::AccessDenied(
            target.display().to_string(),
        ))
    }
}

fn unc_server_share(path: &Path) -> Option<(String, String)> {
    match path.components().next() {
        Some(Component::Prefix(prefix_comp)) => match prefix_comp.kind() {
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some((
                server.to_string_lossy().to_ascii_lowercase(),
                share.to_string_lossy().to_ascii_lowercase(),
            )),
            _ => None,
        },
        _ => None,
    }
}

/// 画像のアスペクト向き分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageOrientation {
    /// 横長（`width > height`）。
    Landscape,
    /// 縦長（`height > width`）。
    Portrait,
    /// 正方形（`width == height`）。
    Square,
}

/// フォルダ内で検出された静止画メタ情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectedImageItem {
    /// 実体ファイルパス（正規化済み文字列）。
    pub path: String,
    /// 探索ルートからの相対パス（章推定用サブフォルダ情報を含む）。
    pub relative_path: String,
    /// ファイル名。
    pub file_name: String,
    /// 所属親サブフォルダ名（ルート直下なら `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subfolder: Option<String>,
    /// 画像幅 (px)。
    pub width: u32,
    /// 画像高さ (px)。
    pub height: u32,
    /// 向き分類。
    pub orientation: ImageOrientation,
    /// アスペクト比 (`width / height`)。
    pub aspect_ratio: f64,
    /// ファイルサイズ（バイト）。
    pub file_size_bytes: u64,
    /// EXIF から抽出された撮影日時（例: `"2022:08:15 14:30:00"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exif_captured_at: Option<String>,
    /// ファイルシステム上の更新日時（UNIX エポック秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_epoch_secs: Option<u64>,
}

/// フォルダ内で検出された音声ファイルメタ情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectedAudioItem {
    /// 実体ファイルパス。
    pub path: String,
    /// 相対パス。
    pub relative_path: String,
    /// ファイル名。
    pub file_name: String,
    /// 再生時間（秒、未確定時は `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    /// サンプルレート (Hz)。
    pub sample_rate_hz: u32,
    /// チャンネル数。
    pub channels: u16,
}

/// フォルダ内で検出された動画ファイルメタ情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectedVideoItem {
    /// 実体ファイルパス。
    pub path: String,
    /// 相対パス。
    pub relative_path: String,
    /// ファイル名。
    pub file_name: String,
    /// 再生時間（秒、未確定時は `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    /// 映像幅 (px)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// 映像高さ (px)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// 音声トラックを含むか。
    pub has_audio: bool,
}

/// 同フォルダ内のテキストメモ（タイトル案や写真説明等の補助情報）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectedTextNote {
    /// 相対パス。
    pub relative_path: String,
    /// テキスト内容（最大 64KB・先頭抜粋）。
    pub content: String,
}

/// フォルダ走査の統合結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderInspectionResult {
    /// 正規化済みフォルダパス。
    pub root_path: String,
    /// 検出された画像一覧（EXIF 撮影日時 → 更新時刻 → パス順にソート済み）。
    pub images: Vec<InspectedImageItem>,
    /// 検出された音声一覧。
    pub audios: Vec<InspectedAudioItem>,
    /// 検出された動画一覧。
    pub videos: Vec<InspectedVideoItem>,
    /// 検出されたメモテキスト一覧。
    pub notes: Vec<InspectedTextNote>,
    /// 走査件数上限到達等でスキップされた件数。
    pub skipped_count: usize,
}

/// Vision 解析へ渡す縮小済みサムネイルデータ（使い捨て用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedThumbnail {
    /// 元画像パス。
    pub path: String,
    /// サムネイル幅 (px)。
    pub width: u32,
    /// サムネイル高さ (px)。
    pub height: u32,
    /// MIME 種別（`"image/jpeg"`）。
    pub mime_type: String,
    /// Base64 エンコード済み JPEG データ。
    pub base64_data: String,
}

/// 許可フォルダ配下の画像・音声・動画・テキストメモを安全に走査してメタ情報を収集する。
///
/// # Errors
///
/// 対象が許可境界外、非存在、またはディレクトリでない場合に [`MediaInspectorError`] を返す。
pub fn inspect_media_folder(
    scope: &AllowedFolderScope,
    folder: &Path,
    recursive: bool,
) -> Result<FolderInspectionResult, MediaInspectorError> {
    let root = scope.validate_path(folder)?;
    if !root.is_dir() {
        return Err(MediaInspectorError::NotFound(format!(
            "{} is not a directory",
            folder.display()
        )));
    }

    let mut files = Vec::new();
    let mut skipped_count = 0_usize;
    let max_depth = if recursive { MAX_SCAN_DEPTH } else { 1 };
    collect_files_bounded(&root, 0, max_depth, &mut files, &mut skipped_count)?;
    files.sort();

    let mut images = Vec::new();
    let mut audios = Vec::new();
    let mut videos = Vec::new();
    let mut notes = Vec::new();

    for path in files {
        let Ok(canon) = scope.validate_path(&path) else {
            skipped_count += 1;
            continue;
        };
        let rel = canon
            .strip_prefix(&root)
            .unwrap_or(&canon)
            .to_string_lossy()
            .replace('\\', "/");
        let file_name = canon
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_owned();
        let ext = canon
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

        match classify_extension(&ext) {
            Some(FileMediaClass::Image) => {
                if let Some(item) = inspect_single_image(&canon, &rel, &file_name) {
                    images.push(item);
                } else {
                    skipped_count += 1;
                }
            }
            Some(FileMediaClass::Audio) => {
                if let Some(item) = inspect_single_audio(&canon, &rel, &file_name) {
                    audios.push(item);
                } else {
                    skipped_count += 1;
                }
            }
            Some(FileMediaClass::Video) => {
                if let Some(item) = inspect_single_video(&canon, &rel, &file_name) {
                    videos.push(item);
                } else {
                    skipped_count += 1;
                }
            }
            Some(FileMediaClass::Note) => {
                if notes.len() < MAX_NOTE_FILES
                    && let Some(note) = read_small_text_note(&canon, &rel)
                {
                    notes.push(note);
                }
            }
            None => {}
        }
    }

    images.sort_by(compare_images_chronologically);

    Ok(FolderInspectionResult {
        root_path: root.to_string_lossy().into_owned(),
        images,
        audios,
        videos,
        notes,
        skipped_count,
    })
}

/// Vision 用サムネイル生成時の縮小・回転・圧縮オプション。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailEncodeOptions {
    /// 長辺の上限ピクセル数（`64..=2048` へクランプ）。
    pub max_edge_px: u32,
    /// JPEG エンコード品質（`40..=90` へクランプ、既定 `68`）。
    pub jpeg_quality: u8,
    /// EXIF 正立後に適用する追加直交回転（時計回り度：`0`, `90`, `180`, `270` 等）。
    pub extra_rotation_deg: i32,
}

impl Default for ThumbnailEncodeOptions {
    fn default() -> Self {
        Self {
            max_edge_px: 512,
            jpeg_quality: 68,
            extra_rotation_deg: 0,
        }
    }
}

/// コンタクトシート生成時のグリッド・品質設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactSheetOptions {
    /// 横列数 (`1..=4`)。
    pub cols: u32,
    /// 縦行数 (`1..=4`)。
    pub rows: u32,
    /// 各タイルの長辺上限ピクセル数 (`128..=1024`)。
    pub tile_max_edge_px: u32,
    /// JPEG エンコード品質 (`40..=90`)。
    pub jpeg_quality: u8,
}

impl Default for ContactSheetOptions {
    fn default() -> Self {
        Self {
            cols: 3,
            rows: 2,
            tile_max_edge_px: 320,
            jpeg_quality: 68,
        }
    }
}

/// コンタクトシート内の 1 タイルに配置する画像入力指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSheetInput {
    /// 画像ファイルパス（[`AllowedFolderScope`] 内であること）。
    pub path: PathBuf,
    /// 画像左上に描画する 1-based 番号バッジ（例: `1` -> `#1`）。
    pub badge_index: usize,
    /// EXIF 正立後に適用する事前直交回転角（時計回り度）。
    pub extra_rotation_deg: i32,
}

/// 生成された番号バッジ付きコンタクトシートのエンコード結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedContactSheet {
    /// シート全体の幅 (px)。
    pub width: u32,
    /// シート全体の高さ (px)。
    pub height: u32,
    /// MIME 種別（`"image/jpeg"`）。
    pub mime_type: String,
    /// Base64 エンコード済み JPEG データ。
    pub base64_data: String,
    /// 含まれるタイル枚数。
    pub tile_count: usize,
    /// 各タイルに対応する正規化済み実ファイルパス一覧。
    pub paths: Vec<String>,
}

/// 指定画像ファイルを長辺 `max_edge_px` 以下へ縮小し、Vision 送信用 JPEG Base64 として生成する。
///
/// # Errors
///
/// パスが許可境界外、画像読込失敗、または JPEG エンコード失敗時に [`MediaInspectorError`] を返す。
pub fn encode_thumbnail_jpeg_base64(
    scope: &AllowedFolderScope,
    image_path: &Path,
    max_edge_px: u32,
) -> Result<EncodedThumbnail, MediaInspectorError> {
    encode_thumbnail_jpeg_base64_with_options(
        scope,
        image_path,
        ThumbnailEncodeOptions {
            max_edge_px,
            ..ThumbnailEncodeOptions::default()
        },
    )
}

/// オプション（JPEG 品質・EXIF 正立後追加回転）を指定して Vision 用サムネイル JPEG Base64 を生成する。
///
/// # Errors
///
/// パスが許可境界外、画像読込失敗、または JPEG エンコード失敗時に [`MediaInspectorError`] を返す。
pub fn encode_thumbnail_jpeg_base64_with_options(
    scope: &AllowedFolderScope,
    image_path: &Path,
    options: ThumbnailEncodeOptions,
) -> Result<EncodedThumbnail, MediaInspectorError> {
    let canon = scope.validate_path(image_path)?;
    let clamped_edge = options.max_edge_px.clamp(64, 2048);
    let quality = options.jpeg_quality.clamp(40, 90);

    let rotated = decode_oriented_and_rotated(&canon, options.extra_rotation_deg)?;
    let resized = if rotated.width().max(rotated.height()) > clamped_edge {
        rotated.thumbnail(clamped_edge, clamped_edge)
    } else {
        rotated
    };

    let width = resized.width();
    let height = resized.height();
    let rgb_img = dynamic_image_to_rgb8_white_bg(resized)?;
    let base64_data = encode_rgb_image_to_jpeg_base64(&rgb_img, quality)?;

    Ok(EncodedThumbnail {
        path: canon.to_string_lossy().into_owned(),
        width,
        height,
        mime_type: "image/jpeg".to_owned(),
        base64_data,
    })
}

/// 単一パス・コンタクトシート生成で個別にスキップされた画像エラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSheetFailure {
    /// 元の `inputs` スライス上の 0-based インデックス。
    pub input_index: usize,
    /// エラーメッセージ。
    pub reason: String,
}

/// 単一パス・部分許容型のコンタクトシート合成結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSheetSkipResult {
    /// 合成済みコンタクトシート。
    pub sheet: EncodedContactSheet,
    /// 成功してシートに含められた `inputs` 上の 0-based インデックス一覧。
    pub accepted_input_indices: Vec<usize>,
    /// 読込・デコード失敗等でスキップされた要素一覧。
    pub skipped: Vec<ContactSheetFailure>,
}

/// 複数画像を単一パスでデコード・縮小し、破損画像を個別隔離したうえでコンタクトシートへ合成する。
///
/// # Errors
///
/// 入力が空またはグリッド上限超過、全画像デコード失敗、または JPEG エンコード失敗時に [`MediaInspectorError`] を返す。
pub fn build_contact_sheet_jpeg_base64_skipping_errors(
    scope: &AllowedFolderScope,
    inputs: &[ContactSheetInput],
    options: ContactSheetOptions,
    renumber_badges_sequentially: bool,
) -> Result<ContactSheetSkipResult, MediaInspectorError> {
    if inputs.is_empty() {
        return Err(MediaInspectorError::ImageFailed(
            "contact sheet requires at least 1 input image".to_owned(),
        ));
    }
    let cols = options.cols.clamp(1, 4);
    let rows = options.rows.clamp(1, 4);
    let capacity = usize::try_from(cols.saturating_mul(rows)).unwrap_or(16);
    if inputs.len() > capacity {
        return Err(MediaInspectorError::ImageFailed(format!(
            "contact sheet tile count {} exceeds {cols}x{rows} capacity ({capacity})",
            inputs.len()
        )));
    }
    let tile_px = options.tile_max_edge_px.clamp(128, 1024);
    let quality = options.jpeg_quality.clamp(40, 90);

    let mut accepted = Vec::with_capacity(inputs.len());
    let mut skipped = Vec::new();
    for (idx, item) in inputs.iter().enumerate() {
        match load_contact_sheet_tile(scope, item, tile_px) {
            Ok((canon, rgb_tile)) => accepted.push((idx, item.badge_index, canon, rgb_tile)),
            Err(err) => skipped.push(ContactSheetFailure {
                input_index: idx,
                reason: err.to_string(),
            }),
        }
    }
    if accepted.is_empty() {
        let first_reason = skipped.first().map_or_else(
            || "no readable images in batch".to_owned(),
            |f| f.reason.clone(),
        );
        return Err(MediaInspectorError::ImageFailed(first_reason));
    }

    let active_cols = cols.min(u32::try_from(accepted.len()).unwrap_or(1).max(1));
    let active_rows = u32::try_from(accepted.len().div_ceil(active_cols as usize)).unwrap_or(1);
    let sheet_w = active_cols.saturating_mul(tile_px);
    let sheet_h = active_rows.saturating_mul(tile_px);

    let mut sheet = RgbImage::from_pixel(sheet_w, sheet_h, Rgb([24, 28, 36]));
    let mut paths = Vec::with_capacity(accepted.len());
    let mut accepted_input_indices = Vec::with_capacity(accepted.len());

    for (slot_idx, (input_idx, badge_index, canon, rgb_tile)) in accepted.into_iter().enumerate() {
        let slot_u32 = u32::try_from(slot_idx).unwrap_or(0);
        let tile_origin_x = (slot_u32 % active_cols).saturating_mul(tile_px);
        let tile_origin_y = (slot_u32 / active_cols).saturating_mul(tile_px);
        blit_centered_tile(&mut sheet, &rgb_tile, tile_origin_x, tile_origin_y, tile_px);

        let effective_badge = if renumber_badges_sequentially {
            slot_idx + 1
        } else {
            badge_index
        };
        let label = format!("#{effective_badge}");
        draw_badge_label(&mut sheet, tile_origin_x + 4, tile_origin_y + 4, &label);
        paths.push(canon.to_string_lossy().into_owned());
        accepted_input_indices.push(input_idx);
    }

    let base64_data = encode_rgb_image_to_jpeg_base64(&sheet, quality)?;
    Ok(ContactSheetSkipResult {
        sheet: EncodedContactSheet {
            width: sheet_w,
            height: sheet_h,
            mime_type: "image/jpeg".to_owned(),
            base64_data,
            tile_count: paths.len(),
            paths,
        },
        accepted_input_indices,
        skipped,
    })
}

fn load_contact_sheet_tile(
    scope: &AllowedFolderScope,
    item: &ContactSheetInput,
    tile_px: u32,
) -> Result<(PathBuf, RgbImage), MediaInspectorError> {
    let canon = scope.validate_path(&item.path)?;
    let oriented = decode_oriented_and_rotated(&canon, item.extra_rotation_deg)?;
    let thumb = if oriented.width().max(oriented.height()) > tile_px {
        oriented.thumbnail(tile_px, tile_px)
    } else {
        oriented
    };
    let rgb_tile = dynamic_image_to_rgb8_white_bg(thumb)?;
    Ok((canon, rgb_tile))
}

/// 複数画像を番号バッジ付きコンタクトシート 1 枚へ合成し、JPEG Base64 で返す。
///
/// # Errors
///
/// 入力が空またはグリッド上限超過、パスが許可境界外、画像デコード／JPEG エンコード失敗時に [`MediaInspectorError`] を返す。
pub fn build_contact_sheet_jpeg_base64(
    scope: &AllowedFolderScope,
    inputs: &[ContactSheetInput],
    options: ContactSheetOptions,
) -> Result<EncodedContactSheet, MediaInspectorError> {
    if inputs.is_empty() {
        return Err(MediaInspectorError::ImageFailed(
            "contact sheet requires at least 1 input image".to_owned(),
        ));
    }
    let cols = options.cols.clamp(1, 4);
    let rows = options.rows.clamp(1, 4);
    let capacity = usize::try_from(cols.saturating_mul(rows)).unwrap_or(16);
    if inputs.len() > capacity {
        return Err(MediaInspectorError::ImageFailed(format!(
            "contact sheet tile count {} exceeds {cols}x{rows} capacity ({capacity})",
            inputs.len()
        )));
    }
    let tile_px = options.tile_max_edge_px.clamp(128, 1024);
    let quality = options.jpeg_quality.clamp(40, 90);
    let active_cols = cols.min(u32::try_from(inputs.len()).unwrap_or(1).max(1));
    let active_rows = u32::try_from(inputs.len().div_ceil(active_cols as usize)).unwrap_or(1);
    let sheet_w = active_cols.saturating_mul(tile_px);
    let sheet_h = active_rows.saturating_mul(tile_px);

    let mut sheet = RgbImage::from_pixel(sheet_w, sheet_h, Rgb([24, 28, 36]));
    let mut paths = Vec::with_capacity(inputs.len());

    for (slot_idx, item) in inputs.iter().enumerate() {
        let canon = scope.validate_path(&item.path)?;
        let oriented = decode_oriented_and_rotated(&canon, item.extra_rotation_deg)?;
        let thumb = if oriented.width().max(oriented.height()) > tile_px {
            oriented.thumbnail(tile_px, tile_px)
        } else {
            oriented
        };
        let rgb_tile = dynamic_image_to_rgb8_white_bg(thumb)?;

        let slot_u32 = u32::try_from(slot_idx).unwrap_or(0);
        let tile_origin_x = (slot_u32 % active_cols).saturating_mul(tile_px);
        let tile_origin_y = (slot_u32 / active_cols).saturating_mul(tile_px);
        blit_centered_tile(&mut sheet, &rgb_tile, tile_origin_x, tile_origin_y, tile_px);

        let label = format!("#{}", item.badge_index);
        draw_badge_label(&mut sheet, tile_origin_x + 4, tile_origin_y + 4, &label);
        paths.push(canon.to_string_lossy().into_owned());
    }

    let base64_data = encode_rgb_image_to_jpeg_base64(&sheet, quality)?;
    Ok(EncodedContactSheet {
        width: sheet_w,
        height: sheet_h,
        mime_type: "image/jpeg".to_owned(),
        base64_data,
        tile_count: paths.len(),
        paths,
    })
}

fn decode_oriented_and_rotated(
    canon: &Path,
    extra_rotation_deg: i32,
) -> Result<DynamicImage, MediaInspectorError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_THUMB_DECODE_DIM);
    limits.max_image_height = Some(MAX_THUMB_DECODE_DIM);
    limits.max_alloc = Some(MAX_THUMB_DECODE_ALLOC_BYTES);

    let decoded = decode_oriented_dynamic_image_file(canon, Some(limits))
        .map_err(|err| MediaInspectorError::ImageFailed(format!("image decode failed: {err}")))?;
    Ok(apply_extra_orthogonal_rotation(decoded, extra_rotation_deg))
}

fn apply_extra_orthogonal_rotation(image: DynamicImage, deg: i32) -> DynamicImage {
    let normalized = deg.rem_euclid(360);
    match normalized {
        90 => image.rotate90(),
        180 => image.rotate180(),
        270 => image.rotate270(),
        _ => image,
    }
}

fn dynamic_image_to_rgb8_white_bg(image: DynamicImage) -> Result<RgbImage, MediaInspectorError> {
    let width = image.width();
    let height = image.height();
    if !image.color().has_alpha() {
        return Ok(image.into_rgb8());
    }
    let rgba = image.into_rgba8();
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for px in rgba.as_chunks::<4>().0 {
        let a = u16::from(px[3]);
        if a == 255 {
            rgb.extend_from_slice(&px[..3]);
        } else {
            // 透過 PNG 等は白背景へ合成して JPEG 時の黒潰れを防止する
            let inv = 255 - a;
            let r = u8::try_from((u16::from(px[0]) * a + 255 * inv) / 255).unwrap_or(255);
            let g = u8::try_from((u16::from(px[1]) * a + 255 * inv) / 255).unwrap_or(255);
            let b = u8::try_from((u16::from(px[2]) * a + 255 * inv) / 255).unwrap_or(255);
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
    }
    RgbImage::from_raw(width, height, rgb).ok_or_else(|| {
        MediaInspectorError::ImageFailed("failed to construct RGB image buffer".to_owned())
    })
}

fn encode_rgb_image_to_jpeg_base64(
    rgb_img: &RgbImage,
    quality: u8,
) -> Result<String, MediaInspectorError> {
    let mut jpeg_buf = Cursor::new(Vec::new());
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
    encoder
        .write_image(
            rgb_img.as_raw(),
            rgb_img.width(),
            rgb_img.height(),
            ColorType::Rgb8.into(),
        )
        .map_err(|err| MediaInspectorError::ImageFailed(format!("JPEG encode failed: {err}")))?;
    Ok(encode_base64(&jpeg_buf.into_inner()))
}

fn blit_centered_tile(
    sheet: &mut RgbImage,
    tile: &RgbImage,
    origin_x: u32,
    origin_y: u32,
    cell_px: u32,
) {
    let pad_x = cell_px.saturating_sub(tile.width()) / 2;
    let pad_y = cell_px.saturating_sub(tile.height()) / 2;
    for y in 0..tile.height() {
        for x in 0..tile.width() {
            let dst_x = origin_x + pad_x + x;
            let dst_y = origin_y + pad_y + y;
            if dst_x < sheet.width() && dst_y < sheet.height() {
                sheet.put_pixel(dst_x, dst_y, *tile.get_pixel(x, y));
            }
        }
    }
}

fn draw_badge_label(sheet: &mut RgbImage, start_x: u32, start_y: u32, text: &str) {
    let scale = 2_u32;
    let char_w = 5 * scale;
    let char_h = 7 * scale;
    let spacing = 2_u32;
    let pad = 4_u32;
    let char_count = u32::try_from(text.chars().count()).unwrap_or(1).max(1);
    let box_w = pad * 2 + char_count * char_w + (char_count.saturating_sub(1)) * spacing;
    let box_h = pad * 2 + char_h;

    for dy in 0..box_h {
        for dx in 0..box_w {
            let px_x = start_x + dx;
            let px_y = start_y + dy;
            if px_x < sheet.width() && px_y < sheet.height() {
                let is_border = dx == 0 || dy == 0 || dx + 1 == box_w || dy + 1 == box_h;
                let color = if is_border {
                    Rgb([255, 255, 255])
                } else {
                    Rgb([15, 23, 42])
                };
                sheet.put_pixel(px_x, px_y, color);
            }
        }
    }

    for (idx, ch) in text.chars().enumerate() {
        let idx_u32 = u32::try_from(idx).unwrap_or(0);
        let glyph = bitmap_glyph_5x7(ch);
        let gx = start_x + pad + idx_u32 * (char_w + spacing);
        let gy = start_y + pad;
        for (row_idx, &row_bits) in glyph.iter().enumerate() {
            let row_u32 = u32::try_from(row_idx).unwrap_or(0);
            for col in 0..5_u32 {
                if (row_bits & (1 << (4 - col))) != 0 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px_x = gx + col * scale + sx;
                            let px_y = gy + row_u32 * scale + sy;
                            if px_x < sheet.width() && px_y < sheet.height() {
                                sheet.put_pixel(px_x, px_y, Rgb([250, 204, 21]));
                            }
                        }
                    }
                }
            }
        }
    }
}

const fn bitmap_glyph_5x7(ch: char) -> [u8; 7] {
    match ch {
        '#' => [
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        _ => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
    }
}

enum FileMediaClass {
    Image,
    Audio,
    Video,
    Note,
}

fn classify_extension(ext: &str) -> Option<FileMediaClass> {
    match ext {
        "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" => Some(FileMediaClass::Image),
        "wav" | "flac" | "ogg" | "mp3" | "m4a" | "aac" => Some(FileMediaClass::Audio),
        "mp4" | "mkv" | "webm" | "mov" | "avi" => Some(FileMediaClass::Video),
        "txt" | "md" | "csv" => Some(FileMediaClass::Note),
        _ => None,
    }
}

fn collect_files_bounded(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<PathBuf>,
    skipped_count: &mut usize,
) -> Result<(), MediaInspectorError> {
    if depth >= max_depth {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) if depth > 0 => {
            *skipped_count += 1;
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *skipped_count += 1;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            *skipped_count += 1;
            continue;
        };
        if file_type.is_symlink() {
            *skipped_count += 1;
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_files_bounded(&path, depth + 1, max_depth, out, skipped_count)?;
        } else if file_type.is_file() {
            if out.len() >= MAX_SCAN_FILES {
                *skipped_count += 1;
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

fn inspect_single_image(path: &Path, rel: &str, file_name: &str) -> Option<InspectedImageItem> {
    let info = inspect_image_file(path).ok()?;
    let meta = fs::metadata(path).ok();
    let file_size_bytes = meta.as_ref().map_or(0, fs::Metadata::len);
    let modified_epoch_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let orientation = match info.width.cmp(&info.height) {
        Ordering::Greater => ImageOrientation::Landscape,
        Ordering::Less => ImageOrientation::Portrait,
        Ordering::Equal => ImageOrientation::Square,
    };
    let aspect_ratio = f64::from(info.width.max(1)) / f64::from(info.height.max(1));
    let subfolder = rel
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_owned())
        .filter(|s| !s.is_empty());
    let exif_captured_at = (info.format == "Jpeg")
        .then(|| extract_jpeg_exif_datetime(path))
        .flatten();

    Some(InspectedImageItem {
        path: path.to_string_lossy().into_owned(),
        relative_path: rel.to_owned(),
        file_name: file_name.to_owned(),
        subfolder,
        width: info.width,
        height: info.height,
        orientation,
        aspect_ratio,
        file_size_bytes,
        exif_captured_at,
        modified_epoch_secs,
    })
}

fn inspect_single_audio(path: &Path, rel: &str, file_name: &str) -> Option<InspectedAudioItem> {
    let dec = open_audio_decoder(path, BackendPreference::Auto).ok()?;
    Some(InspectedAudioItem {
        path: path.to_string_lossy().into_owned(),
        relative_path: rel.to_owned(),
        file_name: file_name.to_owned(),
        duration_seconds: dec.duration_seconds(),
        sample_rate_hz: dec.sample_rate(),
        channels: dec.channels(),
    })
}

fn inspect_single_video(path: &Path, rel: &str, file_name: &str) -> Option<InspectedVideoItem> {
    let info = probe_media(path, BackendPreference::Auto).ok()?;
    Some(InspectedVideoItem {
        path: path.to_string_lossy().into_owned(),
        relative_path: rel.to_owned(),
        file_name: file_name.to_owned(),
        duration_seconds: info.duration_seconds,
        width: info.width,
        height: info.height,
        has_audio: info.has_audio,
    })
}

fn read_small_text_note(path: &Path, rel: &str) -> Option<InspectedTextNote> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > u64::try_from(MAX_NOTE_BYTES).unwrap_or(65536) {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(InspectedTextNote {
        relative_path: rel.to_owned(),
        content: trimmed.chars().take(4000).collect(),
    })
}

fn compare_images_chronologically(a: &InspectedImageItem, b: &InspectedImageItem) -> Ordering {
    match (&a.exif_captured_at, &b.exif_captured_at) {
        (Some(ea), Some(eb)) => ea
            .cmp(eb)
            .then_with(|| a.relative_path.cmp(&b.relative_path)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a
            .modified_epoch_secs
            .cmp(&b.modified_epoch_secs)
            .then_with(|| a.relative_path.cmp(&b.relative_path)),
    }
}

/// JPEG ファイルの先頭 APP1 セグメントから EXIF 撮影日時 (`DateTimeOriginal` 等) を安全抽出する。
fn extract_jpeg_exif_datetime(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let limit = u64::try_from(EXIF_HEADER_READ_LIMIT).unwrap_or(65_536);
    let mut buf = Vec::with_capacity(EXIF_HEADER_READ_LIMIT.min(16 * 1024));
    file.take(limit).read_to_end(&mut buf).ok()?;
    parse_exif_datetime_from_jpeg_bytes(&buf)
}

/// JPEG バイト列の APP1 (`Exif\0\0`) 内 TIFF IFD から撮影日時を読み取る純粋 Rust パーサ。
#[must_use]
pub fn parse_exif_datetime_from_jpeg_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2_usize;
    while pos < bytes.len() {
        if bytes[pos] != 0xFF {
            break;
        }
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let marker = bytes[pos];
        pos += 1;
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        if marker == 0x00 || (0xD0..=0xD8).contains(&marker) {
            continue;
        }
        let Some(len_end) = pos.checked_add(2) else {
            break;
        };
        let Some(len_slice) = bytes.get(pos..len_end) else {
            break;
        };
        let seg_len = usize::from(u16::from_be_bytes([len_slice[0], len_slice[1]]));
        let Some(end) = pos.checked_add(seg_len) else {
            break;
        };
        if seg_len < 2 || end > bytes.len() {
            break;
        }
        let Some(payload) = bytes.get(len_end..end) else {
            break;
        };
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let tiff = &payload[6..];
            if let Some(dt) = parse_tiff_exif_datetime(tiff) {
                return Some(dt);
            }
        }
        pos = end;
    }
    None
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn read_u16(self, b: &[u8], off: usize) -> Option<u16> {
        let end = off.checked_add(2)?;
        let slice = b.get(off..end)?;
        let arr = [slice[0], slice[1]];
        Some(match self {
            Self::Little => u16::from_le_bytes(arr),
            Self::Big => u16::from_be_bytes(arr),
        })
    }

    fn read_u32(self, b: &[u8], off: usize) -> Option<u32> {
        let end = off.checked_add(4)?;
        let slice = b.get(off..end)?;
        let arr = [slice[0], slice[1], slice[2], slice[3]];
        Some(match self {
            Self::Little => u32::from_le_bytes(arr),
            Self::Big => u32::from_be_bytes(arr),
        })
    }
}

fn parse_tiff_exif_datetime(tiff: &[u8]) -> Option<String> {
    if tiff.len() < 8 {
        return None;
    }
    let endian = match &tiff[0..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };
    if endian.read_u16(tiff, 2)? != 42 {
        return None;
    }
    let ifd0_off = usize::try_from(endian.read_u32(tiff, 4)?).ok()?;
    let (ifd0_dt, exif_ifd_off) = scan_ifd_for_datetime_and_subifd(tiff, endian, ifd0_off);
    if let Some(sub_off) = exif_ifd_off {
        let (sub_dt, _) = scan_ifd_for_datetime_and_subifd(tiff, endian, sub_off);
        if sub_dt.is_some() {
            return sub_dt;
        }
    }
    ifd0_dt
}

fn scan_ifd_for_datetime_and_subifd(
    tiff: &[u8],
    endian: Endian,
    ifd_off: usize,
) -> (Option<String>, Option<usize>) {
    let Some(count) = endian.read_u16(tiff, ifd_off) else {
        return (None, None);
    };
    let entry_count = usize::from(count).min(256);
    let mut best_original: Option<String> = None;
    let mut best_digitized: Option<String> = None;
    let mut best_modify: Option<String> = None;
    let mut exif_subifd_off: Option<usize> = None;

    for i in 0..entry_count {
        let Some(base) = ifd_off.checked_add(2) else {
            break;
        };
        let Some(stride) = i.checked_mul(12) else {
            break;
        };
        let Some(entry_off) = base.checked_add(stride) else {
            break;
        };
        let Some(tag) = endian.read_u16(tiff, entry_off) else {
            break;
        };
        let Some(off_typ) = entry_off.checked_add(2) else {
            break;
        };
        let Some(typ) = endian.read_u16(tiff, off_typ) else {
            break;
        };
        let Some(off_cnt) = entry_off.checked_add(4) else {
            break;
        };
        let Some(val_count) = endian.read_u32(tiff, off_cnt) else {
            break;
        };
        let Some(off_val) = entry_off.checked_add(8) else {
            break;
        };
        let Some(val_or_off) = endian.read_u32(tiff, off_val) else {
            break;
        };

        if tag == 0x8769 {
            exif_subifd_off = usize::try_from(val_or_off).ok();
            continue;
        }
        // ASCII 型 (2) で十分な長さを持つ日時タグのみを読む
        if typ == 2 && (10..=64).contains(&val_count) && matches!(tag, 0x9003 | 0x9004 | 0x0132) {
            let Ok(str_off) = usize::try_from(val_or_off) else {
                continue;
            };
            let len = usize::try_from(val_count).unwrap_or(0);
            let Some(str_end) = str_off.checked_add(len) else {
                continue;
            };
            if let Some(raw) = tiff.get(str_off..str_end) {
                let cleaned = String::from_utf8_lossy(raw)
                    .trim_end_matches('\0')
                    .trim()
                    .to_owned();
                if !cleaned.is_empty() {
                    match tag {
                        0x9003 => best_original = Some(cleaned),
                        0x9004 => best_digitized = Some(cleaned),
                        0x0132 => best_modify = Some(cleaned),
                        _ => {}
                    }
                }
            }
        }
    }
    (
        best_original.or(best_digitized).or(best_modify),
        exif_subifd_off,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let id = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "sea_media_inspector_{label}_{}_{id}",
                std::process::id()
            ));
            fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_sample_image(
        path: &Path,
        width: u32,
        height: u32,
        fmt: ImageFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut img = RgbaImage::new(width, height);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = Rgba([220, 120, 80, 180]);
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, fmt)?;
        fs::write(path, buf.into_inner())?;
        Ok(())
    }

    #[test]
    fn inspect_folder_discovers_images_notes_and_enforces_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("scope")?;
        let allowed_dir = guard.0.join("wedding_photos");
        let outside_dir = guard.0.join("secret_outside");
        fs::create_dir_all(&allowed_dir)?;
        fs::create_dir_all(&outside_dir)?;

        write_sample_image(
            &allowed_dir.join("01_groom/childhood.png"),
            800,
            600,
            ImageFormat::Png,
        )?;
        write_sample_image(
            &allowed_dir.join("02_bride/portrait.jpg"),
            600,
            1000,
            ImageFormat::Jpeg,
        )?;
        fs::write(
            allowed_dir.join("memo.txt"),
            "新郎: 太郎 / 新婦: 花子\n出会い: 2020年春",
        )?;
        write_sample_image(
            &outside_dir.join("private.jpg"),
            400,
            300,
            ImageFormat::Jpeg,
        )?;

        let scope = AllowedFolderScope::from_folders([&allowed_dir]);
        let res = inspect_media_folder(&scope, &allowed_dir, true)?;
        assert_eq!(res.images.len(), 2);
        assert_eq!(res.notes.len(), 1);
        assert!(res.notes[0].content.contains("太郎"));

        let portrait = res
            .images
            .iter()
            .find(|i| i.file_name == "portrait.jpg")
            .ok_or("missing portrait")?;
        assert_eq!(portrait.orientation, ImageOrientation::Portrait);
        assert_eq!(portrait.subfolder.as_deref(), Some("02_bride"));

        // 許可スコープ外へのパストラバーサルは拒否される
        let outside_attempt = allowed_dir.join("..").join("secret_outside");
        assert!(matches!(
            inspect_media_folder(&scope, &outside_attempt, true),
            Err(MediaInspectorError::AccessDenied(_))
        ));

        // 512px サムネイル JPEG Base64 エンコード
        let thumb =
            encode_thumbnail_jpeg_base64(&scope, &allowed_dir.join("02_bride/portrait.jpg"), 512)?;
        assert_eq!(thumb.mime_type, "image/jpeg");
        assert!(thumb.width <= 512 && thumb.height <= 512);
        assert_eq!(thumb.height, 512);
        assert_ne!(thumb.base64_data, "");
        Ok(())
    }

    #[test]
    fn exif_parser_extracts_datetime_original_from_minimal_jpeg() {
        // 最小限の Little-Endian TIFF (IFD0 -> ExifIFD 0x8769 -> DateTimeOriginal 0x9003)
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II"); // Little Endian
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes()); // IFD0 offset = 8
        // IFD0: 1 entry at offset 8 (size 2 + 12 + 4 = 18 -> next offset 26)
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x8769_u16.to_le_bytes()); // ExifIFD tag
        tiff.extend_from_slice(&4_u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&26_u32.to_le_bytes()); // ExifIFD offset = 26
        tiff.extend_from_slice(&0_u32.to_le_bytes()); // Next IFD = 0

        // ExifIFD at offset 26: 1 entry (size 2 + 12 + 4 = 18 -> string at 44)
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x9003_u16.to_le_bytes()); // DateTimeOriginal
        tiff.extend_from_slice(&2_u16.to_le_bytes()); // ASCII
        let dt = b"2023:05:20 11:22:33\0";
        tiff.extend_from_slice(&u32::try_from(dt.len()).unwrap_or(20).to_le_bytes());
        tiff.extend_from_slice(&44_u32.to_le_bytes()); // string offset = 44
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        tiff.extend_from_slice(dt);

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let seg_len = u16::try_from(app1.len() + 2).unwrap_or(0);

        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);

        assert_eq!(
            parse_exif_datetime_from_jpeg_bytes(&jpeg).as_deref(),
            Some("2023:05:20 11:22:33")
        );

        // 連続 0xFF フィルバイトを含む JPEG でも抽出できること
        let mut fill_jpeg = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF, 0xE1];
        fill_jpeg.extend_from_slice(&seg_len.to_be_bytes());
        fill_jpeg.extend_from_slice(&app1);
        fill_jpeg.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(
            parse_exif_datetime_from_jpeg_bytes(&fill_jpeg).as_deref(),
            Some("2023:05:20 11:22:33")
        );

        // オーバーフローを引き起こす巨大オフセット/セグメント長でもパニックしないこと
        let mut crafted = vec![0xFF, 0xD8, 0xFF, 0xE1];
        crafted.extend_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(parse_exif_datetime_from_jpeg_bytes(&crafted), None);
        assert_eq!(Endian::Little.read_u32(&[1, 2, 3, 4], usize::MAX - 1), None);
    }

    #[test]
    fn allowed_files_scope_allows_exact_match_and_denies_siblings()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("file_scope")?;
        let dir = guard.0.join("photos");
        fs::create_dir_all(&dir)?;
        let allowed_img = dir.join("registered.jpg");
        let sibling_img = dir.join("secret_sibling.jpg");
        write_sample_image(&allowed_img, 120, 80, ImageFormat::Jpeg)?;
        write_sample_image(&sibling_img, 120, 80, ImageFormat::Jpeg)?;

        let scope = AllowedFolderScope::from_folders_and_files(
            std::iter::empty::<PathBuf>(),
            [&allowed_img],
        );
        assert!(scope.validate_path(&allowed_img).is_ok());
        assert!(matches!(
            scope.validate_path(&sibling_img),
            Err(MediaInspectorError::AccessDenied(_))
        ));
        Ok(())
    }

    #[test]
    fn contact_sheet_and_extra_rotation_produce_compact_valid_jpegs()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("contact_sheet")?;
        let dir = guard.0.join("sheet");
        fs::create_dir_all(&dir)?;
        let p1 = dir.join("a.png");
        let p2 = dir.join("b.png");
        let p3 = dir.join("c.png");
        write_sample_image(&p1, 640, 360, ImageFormat::Png)?;
        write_sample_image(&p2, 360, 640, ImageFormat::Png)?;
        write_sample_image(&p3, 500, 500, ImageFormat::Png)?;

        let scope = AllowedFolderScope::from_folders([&dir]);
        let thumb = encode_thumbnail_jpeg_base64_with_options(
            &scope,
            &p1,
            ThumbnailEncodeOptions {
                max_edge_px: 320,
                jpeg_quality: 68,
                extra_rotation_deg: 90,
            },
        )?;
        assert_eq!((thumb.width, thumb.height), (180, 320));

        let sheet = build_contact_sheet_jpeg_base64(
            &scope,
            &[
                ContactSheetInput {
                    path: p1,
                    badge_index: 1,
                    extra_rotation_deg: 0,
                },
                ContactSheetInput {
                    path: p2,
                    badge_index: 2,
                    extra_rotation_deg: 0,
                },
                ContactSheetInput {
                    path: p3,
                    badge_index: 3,
                    extra_rotation_deg: 0,
                },
            ],
            ContactSheetOptions {
                cols: 3,
                rows: 2,
                tile_max_edge_px: 256,
                jpeg_quality: 68,
            },
        )?;
        assert_eq!(sheet.width, 256 * 3);
        assert_eq!(sheet.height, 256);
        assert_eq!(sheet.tile_count, 3);
        assert_eq!(sheet.mime_type, "image/jpeg");
        assert_ne!(sheet.base64_data, "");

        // 端数枚（例: cols=3 で 2 枚）時はシート幅が 2 列分へ縮小されること
        let shrunk = build_contact_sheet_jpeg_base64(
            &scope,
            &[
                ContactSheetInput {
                    path: dir.join("a.png"),
                    badge_index: 1,
                    extra_rotation_deg: 0,
                },
                ContactSheetInput {
                    path: dir.join("b.png"),
                    badge_index: 2,
                    extra_rotation_deg: 0,
                },
            ],
            ContactSheetOptions {
                cols: 3,
                rows: 2,
                tile_max_edge_px: 256,
                jpeg_quality: 68,
            },
        )?;
        assert_eq!(shrunk.width, 256 * 2);
        assert_eq!(shrunk.height, 256);
        Ok(())
    }

    #[test]
    fn empty_scope_and_unallowed_unc_paths_return_access_denied_before_fs_probe() {
        let empty = AllowedFolderScope::default();
        assert!(matches!(
            empty.validate_path(Path::new("/definitely/missing/secret.jpg")),
            Err(MediaInspectorError::AccessDenied(_))
        ));

        let guard_res = TempDirGuard::new("unc_scope");
        if let Ok(guard) = guard_res {
            let scope = AllowedFolderScope::from_folders([&guard.0]);
            if cfg!(windows) {
                for blocked in [
                    r"\\attacker.invalid\share\probe.jpg",
                    r"\\.\UNC\attacker.invalid\share\probe.jpg",
                    r"\\?\unc\attacker.invalid\share\probe.jpg",
                    r"\\?\GLOBALROOT\Device\Mup\probe.jpg",
                ] {
                    assert!(
                        matches!(
                            scope.validate_path(Path::new(blocked)),
                            Err(MediaInspectorError::AccessDenied(_))
                        ),
                        "expected AccessDenied for {blocked}"
                    );
                }
            }
        }
    }
}
