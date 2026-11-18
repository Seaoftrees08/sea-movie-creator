//! ユーザー許可フォルダ内の画像・音声・動画・メモ走査、EXIF 撮影日時抽出、Vision 用軽量サムネイル生成。

use std::cmp::Ordering;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, ImageEncoder as _, RgbImage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ai::provider::encode_base64;
use crate::media::decoder::{BackendPreference, open_audio_decoder, probe_media};
use crate::media::image_loader::{inspect_image_file, load_image_file};

const MAX_SCAN_DEPTH: usize = 4;
const MAX_SCAN_FILES: usize = 500;
const MAX_NOTE_FILES: usize = 10;
const MAX_NOTE_BYTES: usize = 64 * 1024;
const EXIF_HEADER_READ_LIMIT: usize = 64 * 1024;

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

/// ユーザーがチャットへ添付／明示したフォルダ群のみへ読み取りを制限するアクセス境界。
#[derive(Debug, Clone, Default)]
pub struct AllowedFolderScope {
    roots: Vec<PathBuf>,
}

impl AllowedFolderScope {
    /// 許可するルートディレクトリ群を正規化して構築する。
    #[must_use]
    pub fn from_folders<I, P>(folders: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
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
        Self { roots }
    }

    /// 登録されている正規化済みルート一覧を返す。
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 対象パスが許可ルート配下に実在するか検証し、正規化済みパスを返す。
    ///
    /// # Errors
    ///
    /// 対象が存在しない場合、または許可されたいずれのルート配下にも含まれない場合。
    pub fn validate_path(&self, target: &Path) -> Result<PathBuf, MediaInspectorError> {
        let canon = fs::canonicalize(target)
            .map_err(|_| MediaInspectorError::NotFound(target.display().to_string()))?;
        if self.roots.iter().any(|root| canon.starts_with(root)) {
            return Ok(canon);
        }
        Err(MediaInspectorError::AccessDenied(
            target.display().to_string(),
        ))
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
    let canon = scope.validate_path(image_path)?;
    let clamped_edge = max_edge_px.clamp(64, 2048);
    let frame = load_image_file(&canon, Some(clamped_edge))
        .map_err(|err| MediaInspectorError::ImageFailed(err.to_string()))?;

    let rgba = frame.to_rgba8();
    let mut rgb = Vec::with_capacity((frame.width as usize) * (frame.height as usize) * 3);
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

    let rgb_img = RgbImage::from_raw(frame.width, frame.height, rgb).ok_or_else(|| {
        MediaInspectorError::ImageFailed("failed to construct RGB image buffer".to_owned())
    })?;
    let mut jpeg_buf = Cursor::new(Vec::new());
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, 78);
    encoder
        .write_image(
            rgb_img.as_raw(),
            frame.width,
            frame.height,
            ColorType::Rgb8.into(),
        )
        .map_err(|err| MediaInspectorError::ImageFailed(format!("JPEG encode failed: {err}")))?;

    Ok(EncodedThumbnail {
        path: canon.to_string_lossy().into_owned(),
        width: frame.width,
        height: frame.height,
        mime_type: "image/jpeg".to_owned(),
        base64_data: encode_base64(&jpeg_buf.into_inner()),
    })
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
    let entries = fs::read_dir(dir)?;
    for entry in entries {
        let Ok(entry) = entry else {
            *skipped_count += 1;
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            collect_files_bounded(&path, depth + 1, max_depth, out, skipped_count)?;
        } else if path.is_file() {
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
    let exif_captured_at = extract_jpeg_exif_datetime(path);

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
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0_u8; EXIF_HEADER_READ_LIMIT];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    parse_exif_datetime_from_jpeg_bytes(&buf)
}

/// JPEG バイト列の APP1 (`Exif\0\0`) 内 TIFF IFD から撮影日時を読み取る純粋 Rust パーサ。
#[must_use]
pub fn parse_exif_datetime_from_jpeg_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2_usize;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF {
            break;
        }
        let marker = bytes[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        if marker == 0x00 || (0xD0..=0xD8).contains(&marker) {
            pos += 2;
            continue;
        }
        let seg_len = usize::from(u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]));
        if seg_len < 2 || pos + 2 + seg_len > bytes.len() {
            break;
        }
        let payload = &bytes[pos + 4..pos + 2 + seg_len];
        if marker == 0xE1 && payload.starts_with(b"Exif\0\0") {
            let tiff = &payload[6..];
            if let Some(dt) = parse_tiff_exif_datetime(tiff) {
                return Some(dt);
            }
        }
        pos += 2 + seg_len;
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
        let slice = b.get(off..off + 2)?;
        let arr = [slice[0], slice[1]];
        Some(match self {
            Self::Little => u16::from_le_bytes(arr),
            Self::Big => u16::from_be_bytes(arr),
        })
    }

    fn read_u32(self, b: &[u8], off: usize) -> Option<u32> {
        let slice = b.get(off..off + 4)?;
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
        let entry_off = ifd_off + 2 + i * 12;
        let Some(tag) = endian.read_u16(tiff, entry_off) else {
            break;
        };
        let Some(typ) = endian.read_u16(tiff, entry_off + 2) else {
            break;
        };
        let Some(val_count) = endian.read_u32(tiff, entry_off + 4) else {
            break;
        };
        let Some(val_or_off) = endian.read_u32(tiff, entry_off + 8) else {
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
            if let Some(raw) = tiff.get(str_off..str_off + len) {
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
    }
}
