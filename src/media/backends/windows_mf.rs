//! Windows Media Foundation 映像デコードバックエンド。
//!
//! COM / Win32 Media Foundation API を直接呼び出すため、このモジュール内に限り
//! `unsafe` を許可し、すべての呼び出しに `// SAFETY:` コメントを明記する。

#![allow(unsafe_code)]

use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown,
    MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
    PropVariantToInt64,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::media::decoder::{
    DecodedVideoFrame, MediaError, MediaStreamInfo, VideoDecoder, VideoFrameBuffer,
};

const HNS_PER_SECOND: f64 = 10_000_000.0;

/// Windows Media Foundation でファイルをプローブする。
///
/// # Errors
///
/// COM / MF 初期化またはソースリーダー構築に失敗した場合。
pub fn probe(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    let reader = WindowsMfReader::open(path)?;
    Ok(reader.info.clone())
}

/// Windows Media Foundation を用いた映像デコーダを開く。
///
/// # Errors
///
/// 映像ストリームを開けない場合、または RGB32 出力設定に失敗した場合。
pub fn open_video(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    let reader = WindowsMfReader::open(path)?;
    Ok(Box::new(reader))
}

struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn initialize() -> Result<Self, MediaError> {
        // SAFETY: スレッドの COM アパートメントを MTA として初期化する。既に STA 等で初期化済みなら `RPC_E_CHANGED_MODE` を許容する。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            return Ok(Self {
                should_uninit: true,
            });
        }
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self {
                should_uninit: false,
            });
        }
        Err(MediaError::DecodeFailed(format!(
            "CoInitializeEx failed: {hr:?}"
        )))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: このガードが `CoInitializeEx` に成功した回数と対になるよう 1 回だけ `CoUninitialize` を呼ぶ。
            unsafe {
                CoUninitialize();
            }
        }
    }
}

struct MfGuard;

impl MfGuard {
    fn startup() -> Result<Self, MediaError> {
        // SAFETY: `MFStartup` はスレッドセーフな参照カウント管理を持ち、`Drop` で `MFShutdown` と対になる。
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|err| MediaError::DecodeFailed(format!("MFStartup failed: {err}")))?;
        Ok(Self)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        // SAFETY: `MfGuard::startup` の成功と 1 対 1 で対応してシャットダウンする。
        let _ = unsafe { MFShutdown() };
    }
}

struct LockedBufferGuard<'a> {
    buffer: &'a IMFMediaBuffer,
    data_ptr: *const u8,
    current_len: usize,
}

impl<'a> LockedBufferGuard<'a> {
    fn lock(buffer: &'a IMFMediaBuffer) -> Result<Self, MediaError> {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut current_len: u32 = 0;
        // SAFETY: 有効な `IMFMediaBuffer` をロックし、取得されたポインタ長さと共に管理する。
        unsafe { buffer.Lock(&raw mut ptr, None, Some(&raw mut current_len)) }.map_err(|err| {
            MediaError::DecodeFailed(format!("IMFMediaBuffer::Lock failed: {err}"))
        })?;
        if ptr.is_null() {
            // SAFETY: ロック成功後に異常ポインタだった場合も確実にアンロックする。
            let _ = unsafe { buffer.Unlock() };
            return Err(MediaError::DecodeFailed(
                "IMFMediaBuffer::Lock returned null pointer".to_owned(),
            ));
        }
        Ok(Self {
            buffer,
            data_ptr: ptr,
            current_len: current_len as usize,
        })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `Unlock` 前かつ `Lock` が返した `current_len` バイト範囲に限定してスライス化する。
        unsafe { std::slice::from_raw_parts(self.data_ptr, self.current_len) }
    }
}

impl Drop for LockedBufferGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: `Lock` 成功と対で必ず `Unlock` する。
        let _ = unsafe { self.buffer.Unlock() };
    }
}

struct WindowsMfReader {
    _com: ComGuard,
    _mf: MfGuard,
    reader: IMFSourceReader,
    info: MediaStreamInfo,
    stride: i32,
}

// `IMFSourceReader` へのアクセスは本アプリ側の排他ミューテックス/`&mut self` 上で行われ、COM MTA または単一デコーダスレッド所有とする。
// SAFETY: `WindowsMfReader` は外部へ生ポインタを公開せず `&mut self` で直列化されるため `Send` を安全に満たす。
unsafe impl Send for WindowsMfReader {}

impl WindowsMfReader {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let com = ComGuard::initialize()?;
        let mf = MfGuard::startup()?;

        let attrs = create_reader_attributes()?;
        let wide_url = path_to_hstring(path);

        // SAFETY: 初期化済み `wide_url` (`HSTRING`) と `attrs` を渡してローカルファイル用の `IMFSourceReader` を生成する。
        let reader =
            unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide_url.as_ptr()), &attrs) }
                .map_err(|err| {
                    MediaError::UnsupportedFormat(format!(
                        "MFCreateSourceReaderFromURL failed for {}: {err}",
                        path.display()
                    ))
                })?;

        let video_stream = u32::from_ne_bytes(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0.to_ne_bytes());
        let out_type: IMFMediaType = unsafe { MFCreateMediaType() }
            .map_err(|err| MediaError::DecodeFailed(format!("MFCreateMediaType failed: {err}")))?;
        // SAFETY: 先頭映像ストリームのデコード出力形式に非圧縮 `MFVideoFormat_RGB32` (BGRx) を要求する。
        unsafe {
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32))
                .and_then(|()| reader.SetCurrentMediaType(video_stream, None, &out_type))
        }
        .map_err(|err| {
            MediaError::UnsupportedCodec(format!("MF RGB32 negotiation failed: {err}"))
        })?;

        let (info, stride) = read_stream_metadata(&reader, video_stream)?;
        Ok(Self {
            _com: com,
            _mf: mf,
            reader,
            info,
            stride,
        })
    }
}

fn create_reader_attributes() -> Result<IMFAttributes, MediaError> {
    let mut attrs: Option<IMFAttributes> = None;
    // SAFETY: `attrs` は書き込み可能なローカル `Option<IMFAttributes>` へのポインタ。
    unsafe { MFCreateAttributes(&raw mut attrs, 1) }
        .map_err(|err| MediaError::DecodeFailed(format!("MFCreateAttributes failed: {err}")))?;
    let attrs = attrs.ok_or_else(|| {
        MediaError::DecodeFailed("MFCreateAttributes returned empty attributes".to_owned())
    })?;
    // SAFETY: `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING` を有効化し YUV->RGB 変換を利用可能にする。
    unsafe { attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }.map_err(|err| {
        MediaError::DecodeFailed(format!("IMFAttributes::SetUINT32 failed: {err}"))
    })?;
    Ok(attrs)
}

fn path_to_hstring(path: &Path) -> HSTRING {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    HSTRING::from_wide(&wide)
}

fn unpack_u64_pair(packed: u64) -> (u32, u32) {
    let hi = u32::try_from(packed >> 32).unwrap_or(0);
    let lo = u32::try_from(packed & 0xFFFF_FFFF).unwrap_or(0);
    (hi, lo)
}

fn read_stream_metadata(
    reader: &IMFSourceReader,
    video_stream: u32,
) -> Result<(MediaStreamInfo, i32), MediaError> {
    // SAFETY: `SetCurrentMediaType` 後の現在のメディアタイプから解像度・FPS・ストライドを取得する。
    let current = unsafe { reader.GetCurrentMediaType(video_stream) }
        .map_err(|_err| MediaError::NoVideoStream)?;

    let packed_size = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|err| MediaError::DecodeFailed(format!("MF_MT_FRAME_SIZE missing: {err}")))?;
    let (width, height) = unpack_u64_pair(packed_size);
    if width == 0 || height == 0 {
        return Err(MediaError::InvalidDimensions { width, height });
    }

    let (fps_num, fps_den) = unsafe { current.GetUINT64(&MF_MT_FRAME_RATE) }
        .ok()
        .map_or((30, 1), unpack_u64_pair);

    let stride_u32 = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.ok();
    let default_stride = i32::try_from(width.saturating_mul(4)).unwrap_or(i32::MAX);
    let stride = stride_u32
        .map(|v| i32::from_ne_bytes(v.to_ne_bytes()))
        .filter(|&s| s != 0)
        .unwrap_or(default_stride);

    let duration_seconds = read_duration_seconds(reader);
    let info = MediaStreamInfo {
        duration_seconds,
        width: Some(width),
        height: Some(height),
        frame_rate_num: Some(fps_num.max(1)),
        frame_rate_den: Some(fps_den.max(1)),
        has_video: true,
        has_audio: false,
        audio_sample_rate: None,
        audio_channels: None,
        video_codec: Some("media-foundation".to_owned()),
        audio_codec: None,
    };
    Ok((info, stride))
}

fn read_duration_seconds(reader: &IMFSourceReader) -> Option<f64> {
    let media_source_idx = u32::from_ne_bytes(MF_SOURCE_READER_MEDIASOURCE.0.to_ne_bytes());
    // SAFETY: `MF_PD_DURATION` を `IMFSourceReader` 経由で取得し、必ず `PropVariantClear` で解放する。
    let mut prop =
        unsafe { reader.GetPresentationAttribute(media_source_idx, &MF_PD_DURATION) }.ok()?;
    let duration_100ns = unsafe { PropVariantToInt64(&raw const prop) }.ok();
    unsafe {
        let _ = PropVariantClear(&raw mut prop);
    }
    #[allow(clippy::cast_precision_loss)]
    duration_100ns.map(|d| (d.max(0) as f64) / HNS_PER_SECOND)
}

impl VideoDecoder for WindowsMfReader {
    fn info(&self) -> &MediaStreamInfo {
        &self.info
    }

    fn seek_seconds(&mut self, seconds: f64) -> Result<(), MediaError> {
        #[allow(clippy::cast_possible_truncation)]
        let pos_100ns = (seconds.max(0.0) * HNS_PER_SECOND).round() as i64;
        let var = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_I8,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { hVal: pos_100ns },
                }),
            },
        };
        let time_format = GUID::zeroed();
        // SAFETY: `GUID::zeroed()` は `GUID_NULL` (100ns 単位) を表し、`var` は `VT_I8` (`i64`) である。
        unsafe {
            self.reader
                .SetCurrentPosition(&raw const time_format, &raw const var)
        }
        .map_err(|err| MediaError::DecodeFailed(format!("MF SetCurrentPosition failed: {err}")))
    }

    fn decode_next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, MediaError> {
        let video_stream = u32::from_ne_bytes(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0.to_ne_bytes());
        let width = self.info.width.ok_or(MediaError::NoVideoStream)?;
        let height = self.info.height.ok_or(MediaError::NoVideoStream)?;

        loop {
            let mut flags: u32 = 0;
            let mut timestamp_100ns: i64 = 0;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: 先頭映像ストリームから 1 サンプルを同期読み出しする。
            unsafe {
                self.reader.ReadSample(
                    video_stream,
                    0,
                    None,
                    Some(&raw mut flags),
                    Some(&raw mut timestamp_100ns),
                    Some(&raw mut sample),
                )
            }
            .map_err(|err| MediaError::DecodeFailed(format!("MF ReadSample failed: {err}")))?;

            let flags_i32 = i32::from_ne_bytes(flags.to_ne_bytes());
            if (flags_i32 & MF_SOURCE_READERF_ERROR.0) != 0 {
                return Err(MediaError::DecodeFailed(
                    "Media Foundation reported stream read error".to_owned(),
                ));
            }
            if (flags_i32 & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) != 0
                && let Ok((updated, stride)) = read_stream_metadata(&self.reader, video_stream)
            {
                self.info = updated;
                self.stride = stride;
            }
            if (flags_i32 & MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
                return Ok(None);
            }
            let Some(sample) = sample else {
                continue;
            };

            let frame_buf = copy_sample_to_bgra8(&sample, width, height, self.stride)?;
            #[allow(clippy::cast_precision_loss)]
            let pts_seconds = (timestamp_100ns.max(0) as f64) / HNS_PER_SECOND;
            return Ok(Some(DecodedVideoFrame {
                pts_seconds,
                buffer: frame_buf,
            }));
        }
    }
}

fn copy_sample_to_bgra8(
    sample: &IMFSample,
    width: u32,
    height: u32,
    stride: i32,
) -> Result<VideoFrameBuffer, MediaError> {
    // SAFETY: `ConvertToContiguousBuffer` は単一の連続メモリバッファとしてフレームを返す。
    let media_buf = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|err| {
        MediaError::DecodeFailed(format!(
            "IMFSample::ConvertToContiguousBuffer failed: {err}"
        ))
    })?;
    let locked = LockedBufferGuard::lock(&media_buf)?;
    let src = locked.as_slice();

    let w = width as usize;
    let h = height as usize;
    let row_bytes = w * 4;
    let abs_stride = usize::try_from(stride.unsigned_abs())
        .unwrap_or(row_bytes)
        .max(row_bytes);
    if src.len() < abs_stride.saturating_mul(h) {
        return Err(MediaError::DecodeFailed(
            "Media Foundation sample buffer smaller than expected frame size".to_owned(),
        ));
    }

    let mut dst = vec![0u8; row_bytes * h];
    let bottom_up = stride < 0;
    for y in 0..h {
        let src_row_idx = if bottom_up { h - 1 - y } else { y };
        let src_off = src_row_idx * abs_stride;
        let dst_off = y * row_bytes;
        dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
        // RGB32 はアルファバイトが未定義(0)のため完全不透明(255)に正規化する。
        for px in dst[dst_off..dst_off + row_bytes].as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    VideoFrameBuffer::new_bgra8(width, height, dst)
}
