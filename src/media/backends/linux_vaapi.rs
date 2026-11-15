//! Linux `VA-API` (`/dev/dri/renderD*` + `libva.so.2`) ハードウェア支援検出バックエンド。
//!
//! コンパイル時 C ヘッダ依存なしに実行時 `dlopen` で `libva` を検査するため、
//! `unsafe` FFI を本モジュール内に隔離し `// SAFETY:` コメントを明記する。

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::media::decoder::{MediaError, MediaStreamInfo, VideoDecoder};

const RTLD_LAZY: c_int = 0x1;
const VA_STATUS_SUCCESS: c_int = 0;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

/// 検出された `VA-API` ハードウェアデバイスのバージョン情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaapiCapabilities {
    /// 開けた DRM レンダーノードパス（例: `/dev/dri/renderD128`）。
    pub render_node: PathBuf,
    /// `libva` メジャーバージョン。
    pub major_version: i32,
    /// `libva` マイナーバージョン。
    pub minor_version: i32,
}

/// 実行環境の `/dev/dri/renderD*` と `libva` / `libva-drm` の利用可否を安全に検査する。
///
/// # Errors
///
/// DRM ノード不在、または `libva.so.2` / `libva-drm.so.2` 未導入・初期化失敗時にエラーを返す。
pub fn detect_vaapi_device() -> Result<VaapiCapabilities, MediaError> {
    let render_node = find_render_node().ok_or_else(|| {
        MediaError::UnsupportedFormat("no /dev/dri/renderD* DRM node found".to_owned())
    })?;
    let fd_file = File::open(&render_node)?;
    probe_va_display(&render_node, fd_file.as_raw_fd())
}

fn find_render_node() -> Option<PathBuf> {
    (128..136)
        .map(|num| PathBuf::from(format!("/dev/dri/renderD{num}")))
        .find(|path| path.exists())
}

fn probe_va_display(render_node: &Path, raw_fd: c_int) -> Result<VaapiCapabilities, MediaError> {
    let libva_name = c"libva.so.2";
    let libva_drm_name = c"libva-drm.so.2";

    // SAFETY:
    // 共有ライブラリ `libva.so.2` と `libva-drm.so.2` を `RTLD_LAZY` で開き、
    // 必要なシンボルを解決・初期化した後、`vaTerminate` と `dlclose` で確実に後始末する。
    unsafe {
        let libva = dlopen(libva_name.as_ptr(), RTLD_LAZY);
        if libva.is_null() {
            return Err(MediaError::UnsupportedFormat(
                "libva.so.2 not found on Linux host".to_owned(),
            ));
        }
        let libva_drm = dlopen(libva_drm_name.as_ptr(), RTLD_LAZY);
        if libva_drm.is_null() {
            let _ = dlclose(libva);
            return Err(MediaError::UnsupportedFormat(
                "libva-drm.so.2 not found on Linux host".to_owned(),
            ));
        }

        let res = probe_va_symbols(libva, libva_drm, render_node, raw_fd);
        let _ = dlclose(libva_drm);
        let _ = dlclose(libva);
        res
    }
}

unsafe fn probe_va_symbols(
    libva: *mut c_void,
    libva_drm: *mut c_void,
    render_node: &Path,
    raw_fd: c_int,
) -> Result<VaapiCapabilities, MediaError> {
    type VaGetDisplayDrmFn = unsafe extern "C" fn(c_int) -> *mut c_void;
    type VaInitializeFn = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
    type VaTerminateFn = unsafe extern "C" fn(*mut c_void) -> c_int;

    let sym_get: &CStr = c"vaGetDisplayDRM";
    let sym_init: &CStr = c"vaInitialize";
    let sym_term: &CStr = c"vaTerminate";

    // SAFETY: 呼び出し元 `probe_va_display` により `libva` と `libva_drm` は有効なハンドル。
    unsafe {
        let p_get = dlsym(libva_drm, sym_get.as_ptr());
        let p_init = dlsym(libva, sym_init.as_ptr());
        let p_term = dlsym(libva, sym_term.as_ptr());
        if p_get.is_null() || p_init.is_null() || p_term.is_null() {
            return Err(MediaError::UnsupportedFormat(
                "required VA-API entrypoints missing".to_owned(),
            ));
        }
        let va_get_display = std::mem::transmute::<*mut c_void, VaGetDisplayDrmFn>(p_get);
        let va_initialize = std::mem::transmute::<*mut c_void, VaInitializeFn>(p_init);
        let va_terminate = std::mem::transmute::<*mut c_void, VaTerminateFn>(p_term);

        let display = va_get_display(raw_fd);
        if display.is_null() {
            return Err(MediaError::DecodeFailed(
                "vaGetDisplayDRM returned null".to_owned(),
            ));
        }
        let mut major: c_int = 0;
        let mut minor: c_int = 0;
        let status = va_initialize(display, &raw mut major, &raw mut minor);
        let _ = va_terminate(display);
        if status != VA_STATUS_SUCCESS {
            return Err(MediaError::DecodeFailed(format!(
                "vaInitialize failed with status {status}"
            )));
        }
        Ok(VaapiCapabilities {
            render_node: render_node.to_path_buf(),
            major_version: major,
            minor_version: minor,
        })
    }
}

/// Linux `VA-API` バックエンドでメディア対応を検査する。
///
/// # Errors
///
/// `VA-API` デバイス不在または未初期化時に [`MediaError`] を返す（`Auto` 時は SW デコーダへフォールバックする）。
pub fn probe(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    if !path.is_file() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("media file not found: {}", path.display()),
        )));
    }
    let _caps = detect_vaapi_device()?;
    Err(MediaError::UnsupportedFormat(
        "Linux VA-API bitstream parser delegates container demuxing to OpenSoftware".to_owned(),
    ))
}

/// Linux `VA-API` バックエンドで映像デコーダを開く。
///
/// # Errors
///
/// `VA-API` デコード経路が利用できない場合に [`MediaError`] を返す。
pub fn open_video(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    let _ = probe(path)?;
    Err(MediaError::UnsupportedFormat(
        "VA-API video decode unavailable; fallback to OpenSoftware".to_owned(),
    ))
}
