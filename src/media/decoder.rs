//! メディアデコーダの共通トレイト・ピクセルバッファ・バックエンド選択。

use std::path::Path;

use image::ImageReader;
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
    /// Windows では OS ネイティブ (Media Foundation) とオープン SW (rav1d/symphonia) を形式に応じて自動選択し、失敗時にフォールバックする。
    #[default]
    Auto,
    /// 常にクロスプラットフォームなオープンソースデコーダ (`rav1d` / `symphonia` / `matroska-demuxer`) を使用する。
    OpenSoftware,
    /// 常に Windows Media Foundation バックエンドを使用する（Windows 以外では利用不可）。
    WindowsMediaFoundation,
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

/// 静止画ファイルを読み込み、BGRA8 の `VideoFrameBuffer` に変換する。
///
/// # Errors
///
/// ファイル読み込みまたは画像デコードに失敗した場合。
pub fn decode_image_file(path: &Path) -> Result<VideoFrameBuffer, MediaError> {
    let reader = ImageReader::open(path)?;
    let reader = reader
        .with_guessed_format()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    let image = reader
        .decode()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    dynamic_image_to_bgra8(&image)
}

/// メモリ上の画像バイト列から静止画をデコードし、BGRA8 `VideoFrameBuffer` を返す。
///
/// # Errors
///
/// フォーマット判定またはデコードに失敗した場合。
pub fn decode_image_bytes(bytes: &[u8]) -> Result<VideoFrameBuffer, MediaError> {
    let cursor = std::io::Cursor::new(bytes);
    let reader = ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
    let image = reader
        .decode()
        .map_err(|err| MediaError::ImageDecode(err.to_string()))?;
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
