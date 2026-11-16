//! OS ネイティブおよびオープン SW メディアバックエンドの統合エントリポイント。
//!
//! `unsafe` を伴う FFI / COM 呼び出しはこの `backends` サブモジュール内に隔離される。

use std::path::Path;

use super::decoder::{
    AudioDecoderStream, BackendPreference, MediaError, MediaStreamInfo, VideoDecoder,
};

#[cfg(target_os = "linux")]
pub mod linux_vaapi;
#[cfg(target_os = "macos")]
pub mod macos_vt;
pub mod open_sw;
#[cfg(target_os = "windows")]
pub mod windows_mf;

/// 実行 OS で利用可能なネイティブハードウェアメディアバックエンド種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMediaBackendKind {
    /// Windows Media Foundation (`D3D11VA` / MFT)。
    WindowsMediaFoundation,
    /// macOS `VideoToolbox`。
    MacOsVideoToolbox,
    /// Linux `VA-API`。
    LinuxVaapi,
}

/// 現在のターゲット OS に対応するネイティブハードウェアバックエンドを返す。
#[must_use]
pub const fn current_os_native_backend() -> Option<NativeMediaBackendKind> {
    #[cfg(target_os = "windows")]
    {
        Some(NativeMediaBackendKind::WindowsMediaFoundation)
    }
    #[cfg(target_os = "macos")]
    {
        Some(NativeMediaBackendKind::MacOsVideoToolbox)
    }
    #[cfg(target_os = "linux")]
    {
        Some(NativeMediaBackendKind::LinuxVaapi)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// 各 OS ハードウェアバックエンドが対象とする映像コーデック名かを判定する。
#[must_use]
pub fn supports_hardware_decode_codec(backend: NativeMediaBackendKind, codec: &str) -> bool {
    let normalized = codec.trim().to_ascii_lowercase();
    match backend {
        NativeMediaBackendKind::WindowsMediaFoundation
        | NativeMediaBackendKind::MacOsVideoToolbox => {
            matches!(
                normalized.as_str(),
                "h264" | "avc1" | "hevc" | "h265" | "hvc1" | "av1" | "av01"
            )
        }
        NativeMediaBackendKind::LinuxVaapi => matches!(
            normalized.as_str(),
            "h264" | "avc1" | "hevc" | "h265" | "hvc1" | "vp9" | "av1" | "av01"
        ),
    }
}

/// メディアファイルのストリーム情報をプローブする。
///
/// # Errors
///
/// いずれのバックエンドでもコンテナを解釈できなかった場合。
pub fn probe_media(
    path: &Path,
    preference: BackendPreference,
) -> Result<MediaStreamInfo, MediaError> {
    if !path.exists() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("media file not found: {}", path.display()),
        )));
    }

    match preference {
        BackendPreference::OpenSoftware => open_sw::probe(path),
        BackendPreference::WindowsMediaFoundation => probe_windows_mf(path),
        BackendPreference::NativeHardware => probe_native_os(path),
        BackendPreference::Auto => {
            if prefers_open_sw_first(path) {
                match open_sw::probe(path) {
                    Ok(info) => Ok(info),
                    Err(sw_err) => probe_native_os(path).or(Err(sw_err)),
                }
            } else {
                match probe_native_os(path) {
                    Ok(info) => Ok(info),
                    Err(_) => open_sw::probe(path),
                }
            }
        }
    }
}

/// 優先方針に従って映像デコーダを開く。
///
/// # Errors
///
/// 映像ストリームが見つからない、またはデコーダ初期化に失敗した場合。
pub fn open_video_decoder(
    path: &Path,
    preference: BackendPreference,
) -> Result<Box<dyn VideoDecoder>, MediaError> {
    if !path.exists() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("media file not found: {}", path.display()),
        )));
    }

    match preference {
        BackendPreference::OpenSoftware => open_sw::open_video(path),
        BackendPreference::WindowsMediaFoundation => open_video_windows_mf(path),
        BackendPreference::NativeHardware => open_video_native_os(path),
        BackendPreference::Auto => {
            if prefers_open_sw_first(path) {
                match open_sw::open_video(path) {
                    Ok(dec) => Ok(dec),
                    Err(sw_err) => open_video_native_os(path).or(Err(sw_err)),
                }
            } else {
                match open_video_native_os(path) {
                    Ok(dec) => Ok(dec),
                    Err(_) => open_sw::open_video(path),
                }
            }
        }
    }
}

/// 優先方針に従って音声デコーダを開く。
///
/// # Errors
///
/// 音声ストリームが見つからない、または非対応形式の場合。
pub fn open_audio_decoder(
    path: &Path,
    preference: BackendPreference,
) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    if !path.exists() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("media file not found: {}", path.display()),
        )));
    }

    match preference {
        BackendPreference::OpenSoftware => open_sw::open_audio(path),
        BackendPreference::WindowsMediaFoundation => open_audio_windows_mf(path),
        BackendPreference::NativeHardware => open_audio_native_os(path),
        BackendPreference::Auto => {
            if prefers_open_sw_first(path) {
                match open_sw::open_audio(path) {
                    Ok(dec) => Ok(dec),
                    Err(sw_err) => open_audio_native_os(path).or(Err(sw_err)),
                }
            } else {
                match open_audio_native_os(path) {
                    Ok(dec) => Ok(dec),
                    Err(_) => open_sw::open_audio(path),
                }
            }
        }
    }
}

fn prefers_open_sw_first(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        ext.as_deref(),
        Some("mkv" | "webm" | "ivf" | "flac" | "wav" | "ogg" | "opus" | "mp3")
    )
}

fn probe_native_os(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::probe(path)
    }
    #[cfg(target_os = "macos")]
    {
        macos_vt::probe(path)
    }
    #[cfg(target_os = "linux")]
    {
        linux_vaapi::probe(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "no native hardware media backend on this target OS".to_owned(),
        ))
    }
}

fn open_video_native_os(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::open_video(path)
    }
    #[cfg(target_os = "macos")]
    {
        macos_vt::open_video(path)
    }
    #[cfg(target_os = "linux")]
    {
        linux_vaapi::open_video(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "no native hardware media backend on this target OS".to_owned(),
        ))
    }
}

fn probe_windows_mf(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::probe(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "Windows Media Foundation is only available on Windows".to_owned(),
        ))
    }
}

fn open_video_windows_mf(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::open_video(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "Windows Media Foundation is only available on Windows".to_owned(),
        ))
    }
}

fn open_audio_native_os(path: &Path) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::open_audio(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "native OS audio decoder is currently only implemented via Windows Media Foundation"
                .to_owned(),
        ))
    }
}

fn open_audio_windows_mf(path: &Path) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    #[cfg(target_os = "windows")]
    {
        windows_mf::open_audio(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(MediaError::UnsupportedFormat(
            "Windows Media Foundation is only available on Windows".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_backend_detection_and_codec_table_work() {
        let current = current_os_native_backend();
        assert!(current.is_some());
        assert!(supports_hardware_decode_codec(
            NativeMediaBackendKind::MacOsVideoToolbox,
            "h265"
        ));
        assert!(supports_hardware_decode_codec(
            NativeMediaBackendKind::LinuxVaapi,
            "vp9"
        ));
        assert!(!supports_hardware_decode_codec(
            NativeMediaBackendKind::WindowsMediaFoundation,
            "vp9"
        ));
    }
}
