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
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_SOURCE_READER_CONSTANTS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFAudioFormat_Float,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
    PropVariantToInt64,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::media::decoder::{
    AudioDecoderStream, DecodedAudioPcm, DecodedVideoFrame, MediaError, MediaStreamInfo,
    VideoDecoder, VideoFrameBuffer,
};

const HNS_PER_SECOND: f64 = 10_000_000.0;
const SEQUENTIAL_EPSILON_SEC: f64 = 0.008;
const SEQUENTIAL_FORWARD_GAP_SEC: f64 = 2.5;

/// Windows Media Foundation でファイルをプローブする。
///
/// # Errors
///
/// COM / MF 初期化またはソースリーダー構築に失敗した場合。
pub fn probe(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    let (_com, _mf, reader) = open_source_reader(path, true)?;
    let (info, _stride) = probe_streams(&reader)?;
    Ok(info)
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

/// Windows Media Foundation を用いた音声デコーダを開く。
///
/// # Errors
///
/// 音声ストリームを開けない場合、または Float PCM 出力設定に失敗した場合。
pub fn open_audio(path: &Path) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    let reader = WindowsMfAudioDecoder::open(path)?;
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
    initialized: bool,
    last_frame: Option<DecodedVideoFrame>,
    peeked_frame: Option<DecodedVideoFrame>,
    last_target_seconds: Option<f64>,
}

// `IMFSourceReader` へのアクセスは本アプリ側の排他ミューテックス/`&mut self` 上で行われ、COM MTA または単一デコーダスレッド所有とする。
// SAFETY: `WindowsMfReader` は外部へ生ポインタを公開せず `&mut self` で直列化されるため `Send` を安全に満たす。
unsafe impl Send for WindowsMfReader {}

impl WindowsMfReader {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let (com, mf, reader) = open_source_reader(path, true)?;
        let (info, stride) = probe_streams(&reader)?;
        if !info.has_video {
            return Err(MediaError::NoVideoStream);
        }
        // 映像デコーダ稼働中に未消費の音声サンプルが滞留しないよう音声ストリーム選択を解除する。
        let audio_stream = stream_index(MF_SOURCE_READER_FIRST_AUDIO_STREAM);
        // SAFETY: `audio_stream` が存在しない場合も MF 側が安全に無視/エラーを返すだけなので問題ない。
        let _ = unsafe { reader.SetStreamSelection(audio_stream, false) };

        Ok(Self {
            _com: com,
            _mf: mf,
            reader,
            info,
            stride,
            initialized: false,
            last_frame: None,
            peeked_frame: None,
            last_target_seconds: None,
        })
    }

    fn seek_internal(&mut self, seconds: f64) -> Result<(), MediaError> {
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

    fn read_next_raw_sample(&mut self) -> Result<Option<(f64, IMFSample)>, MediaError> {
        let video_stream = stream_index(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
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
                && let Ok((width, height, fps_num, fps_den, stride)) =
                    read_video_metadata(&self.reader, video_stream)
            {
                self.info.width = Some(width);
                self.info.height = Some(height);
                self.info.frame_rate_num = Some(fps_num.max(1));
                self.info.frame_rate_den = Some(fps_den.max(1));
                self.stride = stride;
            }
            if (flags_i32 & MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
                return Ok(None);
            }
            let Some(sample) = sample else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)]
            let pts_seconds = (timestamp_100ns.max(0) as f64) / HNS_PER_SECOND;
            return Ok(Some((pts_seconds, sample)));
        }
    }

    fn sample_to_decoded_frame(
        &self,
        pts_seconds: f64,
        sample: &IMFSample,
    ) -> Result<DecodedVideoFrame, MediaError> {
        let width = self.info.width.ok_or(MediaError::NoVideoStream)?;
        let height = self.info.height.ok_or(MediaError::NoVideoStream)?;
        let buffer = copy_sample_to_bgra8(sample, width, height, self.stride)?;
        Ok(DecodedVideoFrame {
            pts_seconds,
            buffer,
        })
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
}

const fn stream_index(constant: MF_SOURCE_READER_CONSTANTS) -> u32 {
    u32::from_ne_bytes(constant.0.to_ne_bytes())
}

fn open_source_reader(
    path: &Path,
    enable_video_processing: bool,
) -> Result<(ComGuard, MfGuard, IMFSourceReader), MediaError> {
    let com = ComGuard::initialize()?;
    let mf = MfGuard::startup()?;
    let wide_url = path_to_hstring(path);

    let reader = if enable_video_processing {
        let attrs = create_reader_attributes()?;
        // SAFETY: 初期化済み `wide_url` (`HSTRING`) と `attrs` を渡して `IMFSourceReader` を生成する。
        unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide_url.as_ptr()), &attrs) }
    } else {
        // SAFETY: 初期化済み `wide_url` (`HSTRING`) で音声用 `IMFSourceReader` を生成する。
        unsafe { MFCreateSourceReaderFromURL(PCWSTR::from_raw(wide_url.as_ptr()), None) }
    }
    .map_err(|err| {
        MediaError::UnsupportedFormat(format!(
            "MFCreateSourceReaderFromURL failed for {}: {err}",
            path.display()
        ))
    })?;

    Ok((com, mf, reader))
}

#[derive(Clone, Copy)]
struct AudioFormatMeta {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u32,
}

fn probe_audio_stream(reader: &IMFSourceReader) -> Result<Option<AudioFormatMeta>, MediaError> {
    let audio_stream = stream_index(MF_SOURCE_READER_FIRST_AUDIO_STREAM);
    // SAFETY: 音声ストリームの有無確認のため先頭ネイティブメディアタイプを照会する。
    if unsafe { reader.GetNativeMediaType(audio_stream, 0) }.is_err() {
        return Ok(None);
    }

    let out_type: IMFMediaType = unsafe { MFCreateMediaType() }
        .map_err(|err| MediaError::DecodeFailed(format!("MFCreateMediaType failed: {err}")))?;
    // SAFETY: 音声ストリームの出力形式として非圧縮 32-bit float PCM (`MFAudioFormat_Float`) を要求する。
    let set_res = unsafe {
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .and_then(|()| out_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float))
            .and_then(|()| reader.SetCurrentMediaType(audio_stream, None, &out_type))
    };
    if set_res.is_err() {
        return Ok(None);
    }

    Ok(read_audio_metadata(reader, audio_stream))
}

fn read_audio_metadata(reader: &IMFSourceReader, audio_stream: u32) -> Option<AudioFormatMeta> {
    // SAFETY: `SetCurrentMediaType` 後の確定音声フォーマットを取得する。
    let current = unsafe { reader.GetCurrentMediaType(audio_stream) }.ok()?;
    let sample_rate = unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.ok()?;
    let channels_u32 = unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.ok()?;
    let bits_per_sample = unsafe { current.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE) }.unwrap_or(32);
    let channels = u16::try_from(channels_u32).ok()?;
    if sample_rate == 0 || channels == 0 {
        return None;
    }
    Some(AudioFormatMeta {
        sample_rate,
        channels,
        bits_per_sample,
    })
}

fn configure_video_rgb32(reader: &IMFSourceReader, video_stream: u32) -> Result<bool, MediaError> {
    // SAFETY: 先頭映像ストリームの存在確認。不在時は音声専用ファイルとして扱う。
    if unsafe { reader.GetNativeMediaType(video_stream, 0) }.is_err() {
        return Ok(false);
    }

    let out_type: IMFMediaType = unsafe { MFCreateMediaType() }
        .map_err(|err| MediaError::DecodeFailed(format!("MFCreateMediaType failed: {err}")))?;
    // SAFETY: 先頭映像ストリームのデコード出力形式に非圧縮 `MFVideoFormat_RGB32` (BGRx) を要求する。
    unsafe {
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32))
            .and_then(|()| reader.SetCurrentMediaType(video_stream, None, &out_type))
    }
    .map_err(|err| MediaError::UnsupportedCodec(format!("MF RGB32 negotiation failed: {err}")))?;
    Ok(true)
}

fn probe_streams(reader: &IMFSourceReader) -> Result<(MediaStreamInfo, i32), MediaError> {
    let video_stream = stream_index(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
    let has_video = configure_video_rgb32(reader, video_stream)?;
    let video_meta = if has_video {
        Some(read_video_metadata(reader, video_stream)?)
    } else {
        None
    };

    let audio_meta = probe_audio_stream(reader)?;
    if video_meta.is_none() && audio_meta.is_none() {
        return Err(MediaError::NoVideoStream);
    }

    let duration_seconds = read_duration_seconds(reader);
    let (width, height, frame_rate_num, frame_rate_den, stride) = match video_meta {
        Some((w, h, num, den, s)) => (Some(w), Some(h), Some(num.max(1)), Some(den.max(1)), s),
        None => (None, None, None, None, 0),
    };

    let info = MediaStreamInfo {
        duration_seconds,
        width,
        height,
        frame_rate_num,
        frame_rate_den,
        has_video,
        has_audio: audio_meta.is_some(),
        audio_sample_rate: audio_meta.map(|a| a.sample_rate),
        audio_channels: audio_meta.map(|a| a.channels),
        video_codec: has_video.then(|| "media-foundation".to_owned()),
        audio_codec: audio_meta.map(|_| "media-foundation".to_owned()),
    };
    Ok((info, stride))
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

fn read_video_metadata(
    reader: &IMFSourceReader,
    video_stream: u32,
) -> Result<(u32, u32, u32, u32, i32), MediaError> {
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

    Ok((width, height, fps_num, fps_den, stride))
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
        self.last_frame = None;
        self.peeked_frame = None;
        self.last_target_seconds = None;
        self.initialized = true;
        self.seek_internal(seconds)
    }

    fn decode_next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, MediaError> {
        if let Some(peek) = self.peeked_frame.take() {
            self.initialized = true;
            self.last_target_seconds = Some(peek.pts_seconds);
            self.last_frame = Some(peek.clone());
            return Ok(Some(peek));
        }
        let Some((pts, sample)) = self.read_next_raw_sample()? else {
            return Ok(None);
        };
        let frame = self.sample_to_decoded_frame(pts, &sample)?;
        self.initialized = true;
        self.last_target_seconds = Some(frame.pts_seconds);
        self.last_frame = Some(frame.clone());
        Ok(Some(frame))
    }

    fn decode_frame_at_seconds(
        &mut self,
        target_seconds: f64,
    ) -> Result<Option<DecodedVideoFrame>, MediaError> {
        let clamped = target_seconds.max(0.0);
        if !self.can_advance_sequentially(clamped) {
            let needs_seek = self.initialized || clamped > SEQUENTIAL_EPSILON_SEC;
            self.last_frame = None;
            self.peeked_frame = None;
            self.last_target_seconds = None;
            if needs_seek {
                self.seek_internal(clamped)?;
            }
            self.initialized = true;
        }

        if let Some(peek) = &self.peeked_frame
            && peek.pts_seconds <= clamped + SEQUENTIAL_EPSILON_SEC
        {
            self.last_frame = self.peeked_frame.take();
        }
        if let Some(peek) = &self.peeked_frame
            && peek.pts_seconds > clamped + SEQUENTIAL_EPSILON_SEC
        {
            self.last_target_seconds = Some(clamped);
            return Ok(self.last_frame.clone().or_else(|| Some(peek.clone())));
        }
        if let Some(last) = &self.last_frame
            && (clamped - last.pts_seconds).abs() <= SEQUENTIAL_EPSILON_SEC
        {
            self.last_target_seconds = Some(clamped);
            return Ok(Some(last.clone()));
        }

        let mut pending_best: Option<(f64, IMFSample)> = None;
        while let Some((pts, sample)) = self.read_next_raw_sample()? {
            if pts > clamped + SEQUENTIAL_EPSILON_SEC {
                if let Some((best_pts, best_sample)) = pending_best.take() {
                    self.last_frame = Some(self.sample_to_decoded_frame(best_pts, &best_sample)?);
                }
                let peek_frame = self.sample_to_decoded_frame(pts, &sample)?;
                if self.last_frame.is_some() {
                    self.peeked_frame = Some(peek_frame);
                    break;
                }
                self.peeked_frame = Some(peek_frame.clone());
                self.last_target_seconds = Some(clamped);
                return Ok(Some(peek_frame));
            }
            if (clamped - pts).abs() <= SEQUENTIAL_EPSILON_SEC {
                pending_best = None;
                self.last_frame = Some(self.sample_to_decoded_frame(pts, &sample)?);
                break;
            }
            pending_best = Some((pts, sample));
        }

        if let Some((best_pts, best_sample)) = pending_best {
            self.last_frame = Some(self.sample_to_decoded_frame(best_pts, &best_sample)?);
        }
        self.last_target_seconds = Some(clamped);
        Ok(self.last_frame.clone())
    }
}

struct WindowsMfAudioDecoder {
    _com: ComGuard,
    _mf: MfGuard,
    reader: IMFSourceReader,
    meta: AudioFormatMeta,
    duration_seconds: Option<f64>,
}

// SAFETY: `WindowsMfAudioDecoder` は排他アクセス (`&mut self`) 前提で外部へ生ポインタを公開しない。
unsafe impl Send for WindowsMfAudioDecoder {}

impl WindowsMfAudioDecoder {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let (com, mf, reader) = open_source_reader(path, false)?;
        let video_stream = stream_index(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
        let audio_stream = stream_index(MF_SOURCE_READER_FIRST_AUDIO_STREAM);

        // SAFETY: 音声デコード時に映像フレームの不要なデコード・滞留を抑止する。
        let _ = unsafe { reader.SetStreamSelection(video_stream, false) };
        // SAFETY: 先頭音声ストリームを選択有効化する。
        let _ = unsafe { reader.SetStreamSelection(audio_stream, true) };

        let meta = probe_audio_stream(&reader)?.ok_or(MediaError::NoAudioStream)?;
        let duration_seconds = read_duration_seconds(&reader);
        Ok(Self {
            _com: com,
            _mf: mf,
            reader,
            meta,
            duration_seconds,
        })
    }
}

impl AudioDecoderStream for WindowsMfAudioDecoder {
    fn sample_rate(&self) -> u32 {
        self.meta.sample_rate
    }

    fn channels(&self) -> u16 {
        self.meta.channels
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.duration_seconds
    }

    fn decode_all_interleaved(&mut self) -> Result<DecodedAudioPcm, MediaError> {
        let audio_stream = stream_index(MF_SOURCE_READER_FIRST_AUDIO_STREAM);
        let mut interleaved = Vec::<f32>::new();

        loop {
            let mut flags: u32 = 0;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: 先頭音声ストリームから次の PCM サンプルを同期読み出しする。
            unsafe {
                self.reader.ReadSample(
                    audio_stream,
                    0,
                    None,
                    Some(&raw mut flags),
                    None,
                    Some(&raw mut sample),
                )
            }
            .map_err(|err| {
                MediaError::DecodeFailed(format!("MF audio ReadSample failed: {err}"))
            })?;

            let flags_i32 = i32::from_ne_bytes(flags.to_ne_bytes());
            if (flags_i32 & MF_SOURCE_READERF_ERROR.0) != 0 {
                return Err(MediaError::DecodeFailed(
                    "Media Foundation reported audio stream read error".to_owned(),
                ));
            }
            if (flags_i32 & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) != 0
                && let Some(updated) = read_audio_metadata(&self.reader, audio_stream)
            {
                self.meta = updated;
            }
            if (flags_i32 & MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
                break;
            }
            let Some(sample) = sample else {
                continue;
            };
            append_audio_sample(&sample, self.meta.bits_per_sample, &mut interleaved)?;
        }

        if interleaved.is_empty() {
            return Err(MediaError::NoAudioStream);
        }

        Ok(DecodedAudioPcm {
            sample_rate: self.meta.sample_rate,
            channels: self.meta.channels,
            interleaved,
        })
    }
}

fn append_audio_sample(
    sample: &IMFSample,
    bits_per_sample: u32,
    out: &mut Vec<f32>,
) -> Result<(), MediaError> {
    // SAFETY: 音声サンプルの連続バッファを取得して読み出す。
    let media_buf = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|err| {
        MediaError::DecodeFailed(format!(
            "IMFSample::ConvertToContiguousBuffer failed: {err}"
        ))
    })?;
    let locked = LockedBufferGuard::lock(&media_buf)?;
    append_pcm_bytes(locked.as_slice(), bits_per_sample, out);
    Ok(())
}

fn append_pcm_bytes(bytes: &[u8], bits_per_sample: u32, out: &mut Vec<f32>) {
    if bits_per_sample == 16 {
        let (chunks, _) = bytes.as_chunks::<2>();
        out.reserve(chunks.len());
        for &chunk in chunks {
            let v = i16::from_le_bytes(chunk);
            out.push((f32::from(v) / 32_768.0).clamp(-1.0, 1.0));
        }
        return;
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    out.reserve(chunks.len());
    for &chunk in chunks {
        let s = f32::from_le_bytes(chunk);
        out.push(if s.is_finite() {
            s.clamp(-1.0, 1.0)
        } else {
            0.0
        });
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

#[cfg(test)]
mod tests {
    use super::append_pcm_bytes;

    #[test]
    fn append_pcm_bytes_decodes_f32_and_i16_samples() {
        let mut float_samples = Vec::new();
        let mut float_bytes = Vec::new();
        float_bytes.extend_from_slice(&0.5_f32.to_le_bytes());
        float_bytes.extend_from_slice(&(-1.5_f32).to_le_bytes());
        float_bytes.extend_from_slice(&f32::NAN.to_le_bytes());
        append_pcm_bytes(&float_bytes, 32, &mut float_samples);
        assert_eq!(float_samples.len(), 3);
        assert!((float_samples[0] - 0.5).abs() < 1e-6);
        assert!((float_samples[1] - (-1.0)).abs() < 1e-6);
        assert!((float_samples[2] - 0.0).abs() < 1e-6);

        let mut int_samples = Vec::new();
        let mut int_bytes = Vec::new();
        int_bytes.extend_from_slice(&16_384_i16.to_le_bytes());
        int_bytes.extend_from_slice(&i16::MIN.to_le_bytes());
        append_pcm_bytes(&int_bytes, 16, &mut int_samples);
        assert_eq!(int_samples.len(), 2);
        assert!((int_samples[0] - 0.5).abs() < 1e-4);
        assert!((int_samples[1] - (-1.0)).abs() < 1e-4);
    }
}
