//! 静止画（PNG / JPEG / WebP）の検査・デコード・縮小・フレームキャッシュ登録を行うローダー。

use std::path::Path;
use std::sync::Arc;

use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{ImageBuffer, ImageDecoder as _, ImageReader, Rgba};

use crate::core::ids::AssetId;

use super::decoder::{
    MediaError, VideoFrameBuffer, decode_image_bytes, decode_image_file,
    orientation_swaps_dimensions,
};
use super::frame_cache::{FrameCache, FrameCacheKey};

/// 静止画ファイルのメタ情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// 画像の幅 (px)。
    pub width: u32,
    /// 画像の高さ (px)。
    pub height: u32,
    /// 推定・検出されたフォーマット名（例: `"Png"`, `"Jpeg"`, `"WebP"`）。
    pub format: String,
}

/// 画像ファイルを全画素デコードせずにヘッダから EXIF 補正後の解像度と形式を取得する。
///
/// # Errors
///
/// ファイルを開けない、またはヘッダ解析に失敗した場合。
pub fn inspect_image_file(path: &Path) -> Result<ImageInfo, MediaError> {
    let reader = ImageReader::open(path)?
        .with_guessed_format()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    let format = reader
        .format()
        .map_or_else(|| "Unknown".to_owned(), |fmt| format!("{fmt:?}"));
    let mut decoder = reader
        .into_decoder()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    let (raw_width, raw_height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = if orientation_swaps_dimensions(orientation) {
        (raw_height, raw_width)
    } else {
        (raw_width, raw_height)
    };
    Ok(ImageInfo {
        width,
        height,
        format,
    })
}

/// 静止画ファイルを読み込み、必要に応じて長辺 `max_edge` 以内に縮小して BGRA8 で返す。
///
/// # Errors
///
/// 画像読み込み・デコード・縮小変換に失敗した場合。
pub fn load_image_file(path: &Path, max_edge: Option<u32>) -> Result<VideoFrameBuffer, MediaError> {
    let frame = decode_image_file(path)?;
    downscale_bgra8_if_needed(frame, max_edge)
}

/// 静止画ファイルを読み込み、`FrameCache` に登録した `Arc<VideoFrameBuffer>` を返す。
///
/// 既に `(asset_id, frame=0, scale_tag)` がキャッシュにあればディスクアクセスせず再利用する。
///
/// # Errors
///
/// 画像ファイルの読み込み・デコードに失敗した場合。
pub fn load_image_into_cache(
    path: &Path,
    asset_id: &AssetId,
    max_edge: Option<u32>,
    cache: &mut FrameCache,
) -> Result<Arc<VideoFrameBuffer>, MediaError> {
    let scale_tag = max_edge.unwrap_or(0);
    let key = FrameCacheKey::new(asset_id.clone(), 0, scale_tag);
    if let Some(hit) = cache.get(&key) {
        return Ok(hit);
    }
    let frame = Arc::new(load_image_file(path, max_edge)?);
    cache.insert(key, Arc::clone(&frame));
    Ok(frame)
}

/// メモリ上の画像バイト列をデコード・任意縮小し、`FrameCache` に登録して返す。
///
/// # Errors
///
/// 画像フォーマット判定またはデコードに失敗した場合。
pub fn load_image_bytes_into_cache(
    bytes: &[u8],
    asset_id: &AssetId,
    max_edge: Option<u32>,
    cache: &mut FrameCache,
) -> Result<Arc<VideoFrameBuffer>, MediaError> {
    let scale_tag = max_edge.unwrap_or(0);
    let key = FrameCacheKey::new(asset_id.clone(), 0, scale_tag);
    if let Some(hit) = cache.get(&key) {
        return Ok(hit);
    }
    let decoded = decode_image_bytes(bytes)?;
    let frame = Arc::new(downscale_bgra8_if_needed(decoded, max_edge)?);
    cache.insert(key, Arc::clone(&frame));
    Ok(frame)
}

/// 長辺が `max_edge` を超える場合のみ縦横比を保って高品質フィルタで縮小する。
///
/// # Errors
///
/// バッファサイズ不整合時。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn downscale_bgra8_if_needed(
    frame: VideoFrameBuffer,
    max_edge: Option<u32>,
) -> Result<VideoFrameBuffer, MediaError> {
    let Some(limit) = max_edge.filter(|&edge| edge > 0) else {
        return Ok(frame);
    };
    let longest = frame.width.max(frame.height);
    if longest <= limit {
        return Ok(frame);
    }

    let ratio = f64::from(limit) / f64::from(longest);
    let target_w = ((f64::from(frame.width) * ratio).round() as u32).max(1);
    let target_h = ((f64::from(frame.height) * ratio).round() as u32).max(1);

    let bgra = frame.to_bgra8();
    let img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(frame.width, frame.height, bgra).ok_or(
        MediaError::InvalidDimensions {
            width: frame.width,
            height: frame.height,
        },
    )?;
    let resized = image::imageops::resize(&img, target_w, target_h, FilterType::Lanczos3);
    VideoFrameBuffer::new_bgra8(target_w, target_h, resized.into_raw())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;
    use crate::media::frame_cache::DEFAULT_FRAME_CACHE_BUDGET_BYTES;

    fn encode_test_image(
        width: u32,
        height: u32,
        format: ImageFormat,
        alpha: u8,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut img = RgbaImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = u8::try_from((x * 13) % 256).unwrap_or(255);
            let g = u8::try_from((y * 17) % 256).unwrap_or(255);
            *px = Rgba([r, g, 200, alpha]);
        }
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut cursor, format)?;
        Ok(cursor.into_inner())
    }

    struct TestTempDir(std::path::PathBuf);

    impl TestTempDir {
        fn new(tag: &str) -> Result<Self, std::io::Error> {
            let unique = format!("sea_img_{tag}_{}", std::process::id());
            let dir = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_png_jpeg_webp_and_caches_frames() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TestTempDir::new("loaders")?;
        let png_path = dir.path().join("sample.png");
        let jpg_path = dir.path().join("sample.jpg");
        let webp_path = dir.path().join("sample.webp");

        std::fs::write(
            &png_path,
            encode_test_image(120, 60, ImageFormat::Png, 128)?,
        )?;
        std::fs::write(
            &jpg_path,
            encode_test_image(90, 45, ImageFormat::Jpeg, 255)?,
        )?;
        std::fs::write(
            &webp_path,
            encode_test_image(80, 40, ImageFormat::WebP, 180)?,
        )?;

        let png_info = inspect_image_file(&png_path)?;
        assert_eq!((png_info.width, png_info.height), (120, 60));

        let mut cache = FrameCache::new(DEFAULT_FRAME_CACHE_BUDGET_BYTES);
        let a_png = AssetId::new("img_png")?;
        let a_jpg = AssetId::new("img_jpg")?;
        let a_webp = AssetId::new("img_webp")?;

        let f_png = load_image_into_cache(&png_path, &a_png, None, &mut cache)?;
        assert_eq!((f_png.width, f_png.height), (120, 60));
        // アルファチャンネルが BGRA8 の [3] に維持されることを確認
        assert_eq!(f_png.data[3], 128);

        // 2 回目はキャッシュヒット
        let f_png_hit = load_image_into_cache(&png_path, &a_png, None, &mut cache)?;
        assert!(Arc::ptr_eq(&f_png, &f_png_hit));

        let f_jpg = load_image_into_cache(&jpg_path, &a_jpg, Some(30), &mut cache)?;
        assert_eq!((f_jpg.width, f_jpg.height), (30, 15));

        let f_webp = load_image_into_cache(&webp_path, &a_webp, None, &mut cache)?;
        assert_eq!((f_webp.width, f_webp.height), (80, 40));
        assert!(f_webp.data[3] > 0);
        Ok(())
    }

    #[test]
    fn inspect_image_file_swaps_dimensions_for_exif_orientation_6()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TestTempDir::new("exif_orient6")?;
        let jpg_path = dir.path().join("rotated.jpg");
        let base_jpeg = encode_test_image(24, 12, ImageFormat::Jpeg, 255)?;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&6_u16.to_le_bytes());
        tiff.extend_from_slice(&0_u16.to_le_bytes());
        tiff.extend_from_slice(&0_u32.to_le_bytes());

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let seg_len = u16::try_from(app1.len() + 2).unwrap_or(0);

        let mut oriented_jpeg = Vec::with_capacity(base_jpeg.len() + app1.len() + 4);
        oriented_jpeg.extend_from_slice(&base_jpeg[..2]);
        oriented_jpeg.extend_from_slice(&[0xFF, 0xE1]);
        oriented_jpeg.extend_from_slice(&seg_len.to_be_bytes());
        oriented_jpeg.extend_from_slice(&app1);
        oriented_jpeg.extend_from_slice(&base_jpeg[2..]);
        std::fs::write(&jpg_path, oriented_jpeg)?;

        let info = inspect_image_file(&jpg_path)?;
        assert_eq!((info.width, info.height), (12, 24));
        Ok(())
    }
}
