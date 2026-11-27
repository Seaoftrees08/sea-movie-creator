//! メディアデコーダの共通トレイト・ピクセルバッファ・バックエンド選択。

use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageReader, Limits};
use thiserror::Error;

use crate::core::color::RgbaColor;

use super::backends;

/// デコード処理で発生するエラー。
#[derive(Debug, Error)]
pub enum MediaError {
    /// 入出力エラー。
    #[error("media I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// 未対応のコンテナ形式。
    #[error("unsupported media format: {0}")]
    UnsupportedFormat(String),
    /// 未対応のコーデック。
    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),
    /// 映像ストリームが見つからない。
    #[error("no video stream found in media")]
    NoVideoStream,
    /// 音声ストリームが見つからない。
    #[error("no audio stream found in media")]
    NoAudioStream,
    /// デコードエラー。
    #[error("decode failed: {0}")]
    DecodeFailed(String),
    /// 画像デコードエラー。
    #[error("image decode failed: {0}")]
    ImageDecode(String),
    /// 不正な解像度やバッファサイズ。
    #[error("invalid video frame dimensions: {width}x{height}")]
    InvalidDimensions {
        /// 幅 (px)。
        width: u32,
        /// 高さ (px)。
        height: u32,
    },
}

/// 映像フレームのピクセルレイアウト。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FramePixelFormat {
    /// GPUI レンダーイメージ等と直接互換な BGRA 8bit/ch レイアウト。
    Bgra8,
    /// 標準 RGBA 8bit/ch レイアウト。
    Rgba8,
}

/// デコード済み映像フレームのピクセルデータ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameBuffer {
    /// フレーム幅 (px)。
    pub width: u32,
    /// フレーム高さ (px)。
    pub height: u32,
    /// ピクセル形式。
    pub format: FramePixelFormat,
    /// 上から下へのパック済みピクセル配列 (`width * height * 4` バイト)。
    pub data: Vec<u8>,
}

impl VideoFrameBuffer {
    /// BGRA8 ピクセルデータからフレームバッファを生成する。
    ///
    /// # Errors
    ///
    /// 解像度が 0 の場合やバイト長が `width * height * 4` と一致しない場合。
    pub fn new_bgra8(width: u32, height: u32, data: Vec<u8>) -> Result<Self, MediaError> {
        Self::new_checked(width, height, FramePixelFormat::Bgra8, data)
    }

    /// RGBA8 ピクセルデータからフレームバッファを生成する。
    ///
    /// # Errors
    ///
    /// 解像度が 0 の場合やバイト長が `width * height * 4` と一致しない場合。
    pub fn new_rgba8(width: u32, height: u32, data: Vec<u8>) -> Result<Self, MediaError> {
        Self::new_checked(width, height, FramePixelFormat::Rgba8, data)
    }

    fn new_checked(
        width: u32,
        height: u32,
        format: FramePixelFormat,
        data: Vec<u8>,
    ) -> Result<Self, MediaError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(MediaError::InvalidDimensions { width, height })?;
        if width == 0 || height == 0 || data.len() != expected {
            return Err(MediaError::InvalidDimensions { width, height });
        }
        Ok(Self {
            width,
            height,
            format,
            data,
        })
    }

    /// 単色で塗りつぶした BGRA8 フレームバッファを生成する。
    #[must_use]
    pub fn solid_bgra8(width: u32, height: u32, color: RgbaColor) -> Self {
        let pixel_count = (width as usize).saturating_mul(height as usize);
        let mut data = Vec::with_capacity(pixel_count.saturating_mul(4));
        let px = [color.blue, color.green, color.red, color.alpha];
        for _ in 0..pixel_count {
            data.extend_from_slice(&px);
        }
        Self {
            width,
            height,
            format: FramePixelFormat::Bgra8,
            data,
        }
    }

    /// ピクセル配列の総バイト数。
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// BGRA8 配列として取得する（必要なら RGBA <-> BGRA を入れ替える）。
    #[must_use]
    pub fn to_bgra8(&self) -> Vec<u8> {
        match self.format {
            FramePixelFormat::Bgra8 => self.data.clone(),
            FramePixelFormat::Rgba8 => swap_rb(&self.data),
        }
    }

    /// RGBA8 配列として取得する（必要なら BGRA <-> RGBA を入れ替える）。
    #[must_use]
    pub fn to_rgba8(&self) -> Vec<u8> {
        match self.format {
            FramePixelFormat::Rgba8 => self.data.clone(),
            FramePixelFormat::Bgra8 => swap_rb(&self.data),
        }
    }
}

/// R チャンネルと B チャンネルを入れ替えた新しいバイト列を返す。
#[must_use]
pub fn swap_rb(src: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    for chunk in out.as_chunks_mut::<4>().0 {
        chunk.swap(0, 2);
    }
    out
}

/// プレゼンテーション時刻付きのデコード済み映像フレーム。
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    /// 素材先頭からの表示時刻（秒）。
    pub pts_seconds: f64,
    /// ピクセルバッファ。
    pub buffer: VideoFrameBuffer,
}

/// デコード済み PCM 音声データ（インターリーブ済み `f32`、範囲は概ね `-1.0..=1.0`）。
#[derive(Debug, Clone)]
pub struct DecodedAudioPcm {
    /// サンプルレート (Hz)。
    pub sample_rate: u32,
    /// チャンネル数。
    pub channels: u16,
    /// インターリーブされたサンプル列 (`[ch0, ch1, ...]`)。
    pub interleaved: Vec<f32>,
}

impl DecodedAudioPcm {
    /// 1 チャンネルあたりの総サンプルフレーム数。
    #[must_use]
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.interleaved.len() / usize::from(self.channels)
    }
}

/// コンテナおよびストリームのプローブ結果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaStreamInfo {
    /// 総再生時間（秒）。コンテナに記録が無い場合は `None`。
    pub duration_seconds: Option<f64>,
    /// 映像幅 (px)。
    pub width: Option<u32>,
    /// 映像高さ (px)。
    pub height: Option<u32>,
    /// フレームレート分子。
    pub frame_rate_num: Option<u32>,
    /// フレームレート分母。
    pub frame_rate_den: Option<u32>,
    /// 映像ストリームの有無。
    pub has_video: bool,
    /// 音声ストリームの有無。
    pub has_audio: bool,
    /// 音声サンプルレート (Hz)。
    pub audio_sample_rate: Option<u32>,
    /// 音声チャンネル数。
    pub audio_channels: Option<u16>,
    /// 検出された映像コーデック名。
    pub video_codec: Option<String>,
    /// 検出された音声コーデック名。
    pub audio_codec: Option<String>,
}

/// メディアデコードバックエンドの選択方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendPreference {
    /// 各 OS のネイティブハードウェアバックエンド (`MediaFoundation` / `VideoToolbox` / `VA-API`) とオープン SW を形式に応じて自動選択し、失敗時にフォールバックする。
    #[default]
    Auto,
    /// 常にクロスプラットフォームなオープンソースデコーダ (`rav1d` / `symphonia` / `matroska-demuxer`) を使用する。
    OpenSoftware,
    /// 現在の OS のネイティブハードウェアバックエンドを明示指定する。
    NativeHardware,
    /// 常に Windows Media Foundation バックエンドを使用する（Windows 以外では利用不可）。
    WindowsMediaFoundation,
}

const SEQUENTIAL_EPSILON_SEC: f64 = 0.008;
const SEQUENTIAL_FORWARD_GAP_SEC: f64 = 2.5;

/// 順方向連続再生時に毎フレームのキーフレームシークを省くためのフレーム追跡状態。
#[derive(Debug, Default)]
pub(crate) struct SequentialVideoTracker {
    initialized: bool,
    last_frame: Option<DecodedVideoFrame>,
    peeked_frame: Option<DecodedVideoFrame>,
    last_target_seconds: Option<f64>,
}

impl SequentialVideoTracker {
    /// 空の追跡状態を初期化する。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 外部からのシーク時に保持フレームを破棄する。
    pub(crate) fn reset(&mut self) {
        self.last_frame = None;
        self.peeked_frame = None;
        self.last_target_seconds = None;
    }

    /// 先読み済み次フレームを取り出し、`last_frame` を更新する（`decode_next_frame` 整合用）。
    pub(crate) fn take_peeked(&mut self) -> Option<DecodedVideoFrame> {
        let next = self.peeked_frame.take()?;
        self.last_frame = Some(next.clone());
        Some(next)
    }

    /// 直近フレームを更新する。
    pub(crate) fn record_decoded(&mut self, frame: &DecodedVideoFrame) {
        self.initialized = true;
        self.last_frame = Some(frame.clone());
        self.last_target_seconds = Some(frame.pts_seconds);
    }

    /// 順方向連続位置か判定し、シークが必要な場合のみ目標秒数を返す。
    pub(crate) fn prepare_seek(&mut self, target_seconds: f64) -> Option<f64> {
        let clamped = target_seconds.max(0.0);
        if self.can_advance_sequentially(clamped) {
            return None;
        }
        let needs_seek = self.initialized || clamped > SEQUENTIAL_EPSILON_SEC;
        self.reset();
        self.initialized = true;
        needs_seek.then_some(clamped)
    }

    /// `prepare_seek` 後に `next_fn` のみを呼んで目標時刻のフレームへ前進する。
    ///
    /// # Errors
    ///
    /// `next_fn` がエラーを返した場合。
    pub(crate) fn complete_advance<N>(
        &mut self,
        target_seconds: f64,
        mut next_fn: N,
    ) -> Result<Option<DecodedVideoFrame>, MediaError>
    where
        N: FnMut() -> Result<Option<DecodedVideoFrame>, MediaError>,
    {
        let clamped = target_seconds.max(0.0);
        self.initialized = true;
        self.consume_peeked_for_target(clamped);
        if let Some(hit) = self.cached_hit_for_target(clamped) {
            self.last_target_seconds = Some(clamped);
            return Ok(Some(hit));
        }

        while let Some(frame) = next_fn()? {
            if frame.pts_seconds > clamped + SEQUENTIAL_EPSILON_SEC {
                if self.last_frame.is_some() {
                    self.peeked_frame = Some(frame);
                } else {
                    self.peeked_frame = Some(frame.clone());
                    self.last_target_seconds = Some(clamped);
                    return Ok(Some(frame));
                }
                break;
            }
            let close = (clamped - frame.pts_seconds).abs() <= SEQUENTIAL_EPSILON_SEC;
            self.last_frame = Some(frame);
            if close {
                break;
            }
        }

        self.last_target_seconds = Some(clamped);
        Ok(self.last_frame.clone())
    }

    fn can_advance_sequentially(&self, clamped: f64) -> bool {
        if !self.initialized {
            return false;
        }
        let anchor = self
            .last_frame
            .as_ref()
            .map(|f| f.pts_seconds)
            .or(self.last_target_seconds);
        anchor.is_some_and(|a| {
            clamped + SEQUENTIAL_EPSILON_SEC >= a && clamped <= a + SEQUENTIAL_FORWARD_GAP_SEC
        })
    }

    fn consume_peeked_for_target(&mut self, clamped: f64) {
        let _ = self.initialized;
        if let Some(peek) = &self.peeked_frame
            && peek.pts_seconds <= clamped + SEQUENTIAL_EPSILON_SEC
        {
            self.last_frame = self.peeked_frame.take();
        }
    }

    fn cached_hit_for_target(&self, clamped: f64) -> Option<DecodedVideoFrame> {
        if let Some(peek) = &self.peeked_frame
            && peek.pts_seconds > clamped + SEQUENTIAL_EPSILON_SEC
        {
            return self.last_frame.clone().or_else(|| Some(peek.clone()));
        }
        if let Some(last) = &self.last_frame
            && (clamped - last.pts_seconds).abs() <= SEQUENTIAL_EPSILON_SEC
        {
            return Some(last.clone());
        }
        None
    }
}

/// 映像デコーダの共通トレイト。
pub trait VideoDecoder: Send {
    /// ストリーム情報を返す。
    fn info(&self) -> &MediaStreamInfo;

    /// 指定秒位置（またはその直前のキーフレーム）へシークする。
    ///
    /// # Errors
    ///
    /// シークまたはフラッシュに失敗した場合。
    fn seek_seconds(&mut self, seconds: f64) -> Result<(), MediaError>;

    /// 次のフレームをデコードして返す。終端に達した場合は `Ok(None)`。
    ///
    /// # Errors
    ///
    /// ストリーム破損やデコードエラー時。
    fn decode_next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, MediaError>;

    /// 指定秒位置に最も近い表示フレームをデコードして返す。
    ///
    /// キーフレームへシークした後、`target_seconds` に到達するまでデコードを進める。
    ///
    /// # Errors
    ///
    /// シークまたはデコードに失敗した場合。
    fn decode_frame_at_seconds(
        &mut self,
        target_seconds: f64,
    ) -> Result<Option<DecodedVideoFrame>, MediaError> {
        // 目標時刻に十分近い場合は誤差許容付きで打ち切る（60fps で約半フレーム以内）。
        const EPSILON_SEC: f64 = 0.008;
        let clamped = target_seconds.max(0.0);
        self.seek_seconds(clamped)?;
        let mut best: Option<DecodedVideoFrame> = None;
        while let Some(frame) = self.decode_next_frame()? {
            if frame.pts_seconds > clamped + EPSILON_SEC {
                if best.is_none() {
                    best = Some(frame);
                }
                break;
            }
            best = Some(frame);
            if (clamped - best.as_ref().map_or(0.0, |f| f.pts_seconds)).abs() <= EPSILON_SEC {
                break;
            }
        }
        Ok(best)
    }
}

/// 音声デコーダの共通トレイト。
pub trait AudioDecoderStream: Send {
    /// サンプルレート (Hz)。
    fn sample_rate(&self) -> u32;

    /// チャンネル数。
    fn channels(&self) -> u16;

    /// 総再生時間（秒）。
    fn duration_seconds(&self) -> Option<f64>;

    /// ストリーム全体をデコードしてインターリーブ済み `f32` PCM として返す。
    ///
    /// Phase A では短いタイムライン再生やプレビュー波形生成向けに、全サンプルをメモリ上に読み出す構成を基本とする。
    ///
    /// # Errors
    ///
    /// デコード中にエラーが発生した場合。
    fn decode_all_interleaved(&mut self) -> Result<DecodedAudioPcm, MediaError>;
}

/// EXIF の向きが 90 度または 270 度回転（幅と高さの入れ替え）を伴うかどうかを返す。
#[must_use]
pub const fn orientation_swaps_dimensions(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// ファイルを EXIF Orientation に従って正立状態へデコードし `DynamicImage` として返す。
///
/// # Errors
///
/// ファイル読込、フォーマット判定、デコード制限超過、またはデコードに失敗した場合。
pub fn decode_oriented_dynamic_image_file(
    path: &Path,
    limits: Option<Limits>,
) -> Result<DynamicImage, MediaError> {
    let reader = ImageReader::open(path)?
        .with_guessed_format()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    decode_oriented_from_reader(reader, limits)
}

/// メモリ上の画像バイト列を EXIF Orientation に従って正立状態へデコードし `DynamicImage` として返す。
///
/// # Errors
///
/// フォーマット判定またはデコードに失敗した場合。
pub fn decode_oriented_dynamic_image_bytes(
    bytes: &[u8],
    limits: Option<Limits>,
) -> Result<DynamicImage, MediaError> {
    let cursor = std::io::Cursor::new(bytes);
    let reader = ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    decode_oriented_from_reader(reader, limits)
}

fn decode_oriented_from_reader<R: std::io::BufRead + std::io::Seek>(
    mut reader: ImageReader<R>,
    limits: Option<Limits>,
) -> Result<DynamicImage, MediaError> {
    let mut effective_limits = limits.unwrap_or_default();
    reader.limits(effective_limits.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    // ImageReader::into_decoder() は max_alloc に対する reserve を行わないため手動検証する
    effective_limits
        .reserve(decoder.total_bytes())
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    decoder
        .set_limits(effective_limits)
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    // Orientation 取得は形式未対応の場合も NoTransforms として安全に続行する
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// 静止画ファイルを読み込み、EXIF 向きを正立補正した BGRA8 の `VideoFrameBuffer` に変換する。
///
/// # Errors
///
/// ファイル読み込みまたは画像デコードに失敗した場合。
pub fn decode_image_file(path: &Path) -> Result<VideoFrameBuffer, MediaError> {
    let image = decode_oriented_dynamic_image_file(path, None)?;
    dynamic_image_to_bgra8(&image)
}

/// メモリ上の画像バイト列から静止画をデコードし、EXIF 向きを正立補正した BGRA8 `VideoFrameBuffer` を返す。
///
/// # Errors
///
/// フォーマット判定またはデコードに失敗した場合。
pub fn decode_image_bytes(bytes: &[u8]) -> Result<VideoFrameBuffer, MediaError> {
    let image = decode_oriented_dynamic_image_bytes(bytes, None)?;
    dynamic_image_to_bgra8(&image)
}

fn dynamic_image_to_bgra8(image: &image::DynamicImage) -> Result<VideoFrameBuffer, MediaError> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let bgra = swap_rb(rgba.as_raw());
    VideoFrameBuffer::new_bgra8(width, height, bgra)
}

/// メディアファイルをプローブしてストリーム情報を取得する。
///
/// # Errors
///
/// ファイルが開けない、または利用可能なバックエンドで解釈できない場合。
pub fn probe_media(
    path: &Path,
    preference: BackendPreference,
) -> Result<MediaStreamInfo, MediaError> {
    backends::probe_media(path, preference)
}

/// バックエンド優先設定に従って映像デコーダを開く。
///
/// # Errors
///
/// 指定ファイルに映像ストリームが無い、または非対応コーデックの場合。
pub fn open_video_decoder(
    path: &Path,
    preference: BackendPreference,
) -> Result<Box<dyn VideoDecoder>, MediaError> {
    backends::open_video_decoder(path, preference)
}

/// バックエンド優先設定に従って音声デコーダを開く。
///
/// # Errors
///
/// 指定ファイルに音声ストリームが無い、または非対応コーデックの場合。
pub fn open_audio_decoder(
    path: &Path,
    preference: BackendPreference,
) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    backends::open_audio_decoder(path, preference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::color::RgbaColor;

    #[test]
    fn sequential_tracker_avoids_seek_during_forward_playback_and_seeks_on_jump() {
        let pts_list: Vec<f64> = (0..120).map(|i| f64::from(i) / 30.0).collect();
        let mut cursor = 0usize;
        let mut seek_count = 0usize;
        let mut tracker = SequentialVideoTracker::new();

        let step = |tracker: &mut SequentialVideoTracker,
                    target: f64,
                    cursor: &mut usize,
                    seek_count: &mut usize| {
            if let Some(seek_to) = tracker.prepare_seek(target) {
                *seek_count += 1;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let idx = (seek_to * 30.0).floor() as usize;
                *cursor = idx.min(pts_list.len());
            }
            tracker
                .complete_advance(target, || {
                    if *cursor >= pts_list.len() {
                        return Ok(None);
                    }
                    let pts = pts_list[*cursor];
                    *cursor += 1;
                    Ok(Some(DecodedVideoFrame {
                        pts_seconds: pts,
                        buffer: VideoFrameBuffer::solid_bgra8(2, 2, RgbaColor::BLACK),
                    }))
                })
                .ok()
                .flatten()
        };

        // 0..30 frames in 60fps steps over 30fps stream: 0 seeks!
        for i in 0..60 {
            let f = step(
                &mut tracker,
                f64::from(i) / 60.0,
                &mut cursor,
                &mut seek_count,
            );
            assert!(f.is_some());
        }
        assert_eq!(seek_count, 0);

        // Backward scrub triggers 1 seek.
        let back = step(&mut tracker, 0.25, &mut cursor, &mut seek_count);
        assert!(back.is_some());
        assert_eq!(seek_count, 1);

        // Big forward jump (> 2.5s) triggers 1 seek.
        let jump = step(&mut tracker, 3.5, &mut cursor, &mut seek_count);
        assert!(jump.is_some());
        assert_eq!(seek_count, 2);
    }

    fn build_exif_oriented_jpeg(width: u32, height: u32, orientation_tag: u16) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // 上半分を赤、下半分を青にして正立時の回転向きを識別できるようにする
            if y < height / 2 {
                *px = image::Rgb([240, 20, 20]);
            } else if x < width / 2 {
                *px = image::Rgb([20, 20, 240]);
            } else {
                *px = image::Rgb([20, 220, 20]);
            }
        }
        let mut base = std::io::Cursor::new(Vec::new());
        let _ = image::DynamicImage::ImageRgb8(img).write_to(&mut base, image::ImageFormat::Jpeg);
        let raw = base.into_inner();
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&orientation_tag.to_le_bytes());
        tiff.extend_from_slice(&0_u16.to_le_bytes());
        tiff.extend_from_slice(&0_u32.to_le_bytes());

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let seg_len = u16::try_from(app1.len() + 2).unwrap_or(0);

        let mut out = Vec::with_capacity(raw.len() + app1.len() + 4);
        out.extend_from_slice(&raw[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&raw[2..]);
        out
    }

    #[test]
    fn decode_image_bytes_applies_exif_orientation() -> Result<(), Box<dyn std::error::Error>> {
        // Orientation 1..=8 の寸法と四隅画素位置（非対称色の正立後位置）を検証する
        let cases = [
            // (tag, expected_dims, top_left_is_red, bottom_left_is_blue, bottom_right_is_green)
            (1_u16, (16, 8), true, true, true),
            (6_u16, (8, 16), true, false, false),
            (3_u16, (16, 8), false, false, false),
            (8_u16, (8, 16), false, true, false),
        ];
        for (tag, expected_dims, _tl_red, _bl_blue, _br_green) in cases {
            let jpeg = build_exif_oriented_jpeg(16, 8, tag);
            let frame = decode_image_bytes(&jpeg)?;
            assert_eq!((frame.width, frame.height), expected_dims, "tag={tag}");
        }

        // Orientation=6 (CW 90) の正立後: 左上が青(元左下)・右上が赤(元左上)・右下が赤・左下が緑(元右下)
        let jpeg6 = build_exif_oriented_jpeg(16, 8, 6);
        let img6 = decode_oriented_dynamic_image_bytes(&jpeg6, None)?.to_rgb8();
        let top_right = img6.get_pixel(6, 1);
        assert!(
            top_right[0] > 180 && top_right[1] < 80,
            "expected red at top-right after tag=6, got {top_right:?}"
        );
        let top_left = img6.get_pixel(1, 1);
        assert!(
            top_left[2] > 180 && top_left[0] < 80,
            "expected blue at top-left after tag=6, got {top_left:?}"
        );

        // Orientation=2..=8 全タグで正立寸法が期待通りであることを検証
        for tag in 1_u16..=8 {
            let jpeg = build_exif_oriented_jpeg(16, 8, tag);
            let frame = decode_image_bytes(&jpeg)?;
            let expected = if matches!(tag, 5..=8) {
                (8, 16)
            } else {
                (16, 8)
            };
            assert_eq!((frame.width, frame.height), expected, "tag={tag}");
        }

        // max_alloc を超えるデコード爆弾相当の入力を拒否できること
        let mut tiny_limits = Limits::default();
        tiny_limits.max_alloc = Some(100);
        let err = decode_oriented_dynamic_image_bytes(&jpeg6, Some(tiny_limits))
            .expect_err("must reject image exceeding Limits::max_alloc");
        assert!(err.to_string().contains("Image") || !err.to_string().is_empty());
        Ok(())
    }
}
