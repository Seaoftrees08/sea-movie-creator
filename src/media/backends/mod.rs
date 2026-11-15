//! OS ネイティブおよびオープン SW メディアバックエンドの統合エントリポイント。
//!
//! `unsafe` を伴う FFI / COM 呼び出しはこの `backends` サブモジュール内に隔離される。

use std::path::Path;

use super::decoder::{
    AudioDecoderStream, BackendPreference, MediaError, MediaStreamInfo, VideoDecoder,
};

pub mod open_sw;
#[cfg(target_os = "windows")]
pub mod windows_mf;

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
        BackendPreference::Auto => {
            if prefers_open_sw_first(path) {
                match open_sw::probe(path) {
                    Ok(info) => Ok(info),
                    Err(sw_err) => probe_windows_mf(path).or(Err(sw_err)),
                }
            } else {
                match probe_windows_mf(path) {
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
        BackendPreference::Auto => {
            if prefers_open_sw_first(path) {
                match open_sw::open_video(path) {
                    Ok(dec) => Ok(dec),
                    Err(sw_err) => open_video_windows_mf(path).or(Err(sw_err)),
                }
            } else {
                match open_video_windows_mf(path) {
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
        BackendPreference::OpenSoftware | BackendPreference::Auto => open_sw::open_audio(path),
        BackendPreference::WindowsMediaFoundation => Err(MediaError::UnsupportedCodec(
            "Windows Media Foundation audio decoder is deferred to Phase B; use OpenSoftware"
                .to_owned(),
        )),
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
