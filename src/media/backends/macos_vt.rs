//! macOS `VideoToolbox` ハードウェア支援プローブおよびデコードバックエンド。
//!
//! POSIX `dlopen` / `dlsym` により `VideoToolbox.framework` のエントリポイントを解決するため、
//! `unsafe` FFI を本モジュール内に隔離しすべての呼び出しに `// SAFETY:` を付記する。

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;

use crate::media::decoder::{MediaError, MediaStreamInfo, VideoDecoder};

const RTLD_LAZY: c_int = 0x1;
const VIDEOTOOLBOX_FRAMEWORK_PATH: &CStr =
    c"/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox";
const SYM_VT_HW_DECODE: &CStr = c"VTIsHardwareDecodeSupported";

/// H.264 (`avc1`) の `VideoToolbox` `FourCC` 識別子。
pub const FOURCC_H264: u32 = u32::from_be_bytes(*b"avc1");
/// HEVC / H.265 (`hvc1`) の `VideoToolbox` `FourCC` 識別子。
pub const FOURCC_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
/// AV1 (`av01`) の `VideoToolbox` `FourCC` 識別子。
pub const FOURCC_AV1: u32 = u32::from_be_bytes(*b"av01");

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

/// 指定した `FourCC` コーデックについて macOS `VideoToolbox` の HW デコード対応有無を照会する。
#[must_use]
pub fn is_hardware_decode_supported(codec_type: u32) -> bool {
    type VtHwFn = unsafe extern "C" fn(u32) -> u8;

    // SAFETY:
    // macOS 標準フレームワークパスを `RTLD_LAZY` で開き、`VTIsHardwareDecodeSupported`
    // の有無を確認したうえで必ず `dlclose` で解放する。
    unsafe {
        let handle = dlopen(VIDEOTOOLBOX_FRAMEWORK_PATH.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            return false;
        }
        let sym = dlsym(handle, SYM_VT_HW_DECODE.as_ptr());
        let supported = if sym.is_null() {
            false
        } else {
            let func = std::mem::transmute::<*mut c_void, VtHwFn>(sym);
            func(codec_type) != 0
        };
        let _ = dlclose(handle);
        supported
    }
}

/// macOS `VideoToolbox` を用いてメディアの対応可能性を確認する。
///
/// # Errors
///
/// 対象外コンテナ、またはヘッドレス環境等でハードウェア支援が利用できない場合に
/// [`MediaError`] を返す（[`BackendPreference::Auto`](crate::media::decoder::BackendPreference::Auto) 時は呼び出し元がオープン SW へフォールバックする）。
pub fn probe(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    if !path.is_file() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("media file not found: {}", path.display()),
        )));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(ext.as_deref(), Some("mp4" | "mov" | "m4v")) {
        return Err(MediaError::UnsupportedFormat(format!(
            "VideoToolbox probe skips non-ISOBMFF container: {}",
            path.display()
        )));
    }
    let hw_ready = is_hardware_decode_supported(FOURCC_H264)
        || is_hardware_decode_supported(FOURCC_HEVC)
        || is_hardware_decode_supported(FOURCC_AV1);
    if !hw_ready {
        return Err(MediaError::DecodeFailed(
            "VideoToolbox hardware decode is unavailable on this host".to_owned(),
        ));
    }
    Err(MediaError::UnsupportedFormat(
        "macOS VideoToolbox frame reader requires AVFoundation CMSampleBuffer bridge; falling back to OpenSoftware"
            .to_owned(),
    ))
}

/// macOS `VideoToolbox` を用いた映像デコーダを開く。
///
/// # Errors
///
/// 非対応環境・コンテナ時は [`MediaError`] を返す。
pub fn open_video(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    let _ = probe(path)?;
    Err(MediaError::UnsupportedFormat(
        "VideoToolbox video decode unavailable; fallback to OpenSoftware".to_owned(),
    ))
}
