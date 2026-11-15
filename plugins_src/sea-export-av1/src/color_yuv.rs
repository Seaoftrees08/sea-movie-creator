//! `sea-export-av1` のエンコード設定解析および BGRA8 -> BT.709 `YUV420p` 色空間変換。

use rav1e::prelude::{Frame, PixelRange};
use serde_json::Value;

/// AV1 エンコーダの動作バックエンド種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1BackendMode {
    /// 純粋 Rust (`rav1e` + `flacenc` + `webm-iterable`) によるソフトウェア MKV 書き出し。
    SoftwareRav1e,
    /// OS 標準 Media Foundation ハードウェア AV1 エンコーダによる MP4 書き出し。
    OsHardwareMf,
}

/// プラグイン `options` JSON から解決された AV1 エンコード設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1EncodeSettings {
    /// 利用するバックエンド種別。
    pub backend_mode: Av1BackendMode,
    /// `rav1e` 速度プリセット (`0..=10`、数値が大きいほど高速)。
    pub speed: u8,
    /// `rav1e` 基本量子化パラメータ (`20..=200`、数値が小さいほど高画質)。
    pub quantizer: usize,
    /// `rav1e` スレッド数 (`0` は自動)。
    pub threads: usize,
    /// ハードウェア出力モード時の目標映像ビットレート (kbps)。
    pub bitrate_kbps: u32,
}

impl Default for Av1EncodeSettings {
    fn default() -> Self {
        Self {
            backend_mode: Av1BackendMode::SoftwareRav1e,
            speed: 8,
            quantizer: 100,
            threads: 0,
            bitrate_kbps: 8000,
        }
    }
}

impl Av1EncodeSettings {
    /// プラグイン初期化時の JSON オブジェクトおよびプリセット名から設定値を構築する。
    ///
    /// # Errors
    ///
    /// 未知の `backend_mode` が指定された場合にエラー文字列を返す。
    pub fn from_init(preset_id: &str, options: &Value) -> Result<Self, String> {
        let mut settings = match preset_id {
            "av1_quality" => Self {
                backend_mode: Av1BackendMode::SoftwareRav1e,
                speed: 5,
                quantizer: 75,
                threads: 0,
                bitrate_kbps: 10_000,
            },
            "av1_hw" => Self {
                backend_mode: Av1BackendMode::OsHardwareMf,
                bitrate_kbps: 8000,
                ..Self::default()
            },
            _ => Self {
                backend_mode: Av1BackendMode::SoftwareRav1e,
                speed: 9,
                quantizer: 110,
                threads: 0,
                bitrate_kbps: 6000,
            },
        };

        if let Some(mode_str) = options.get("backend_mode").and_then(Value::as_str) {
            settings.backend_mode = match mode_str {
                "software_rav1e" => Av1BackendMode::SoftwareRav1e,
                "os_hardware_mf" => Av1BackendMode::OsHardwareMf,
                other => {
                    return Err(format!(
                        "未知の AV1 バックエンドモードです: '{other}' (software_rav1e / os_hardware_mf)"
                    ));
                }
            };
        }
        if let Some(speed) = options.get("speed").and_then(Value::as_u64) {
            settings.speed = u8::try_from(speed.min(10)).unwrap_or(10);
        }
        if let Some(quantizer) = options.get("quantizer").and_then(Value::as_u64) {
            let clamped = quantizer.clamp(20, 200);
            settings.quantizer = usize::try_from(clamped).unwrap_or(100);
        }
        if let Some(threads) = options.get("threads").and_then(Value::as_u64) {
            let clamped = threads.min(32);
            settings.threads = usize::try_from(clamped).unwrap_or(0);
        }
        if let Some(bitrate) = options.get("bitrate_kbps").and_then(Value::as_u64) {
            settings.bitrate_kbps = u32::try_from(bitrate.clamp(200, 200_000)).unwrap_or(8000);
        }
        Ok(settings)
    }
}

/// 映像解像度が AV1 (`rav1e`) で処理可能な制約を満たしているか検証する。
///
/// # Errors
///
/// 幅または高さが 16px 未満・65535px 超の場合にエラー文字列を返す。
pub fn validate_video_geometry(width: u32, height: u32) -> Result<(usize, usize), String> {
    if width < 16 || height < 16 {
        return Err(format!(
            "AV1 エンコードは 16x16 以上の解像度が必要です (入力: {width}x{height})"
        ));
    }
    if width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
        return Err(format!(
            "AV1 エンコードの上限解像度 (65535x65535) を超えています (入力: {width}x{height})"
        ));
    }
    let w = usize::try_from(width).map_err(|_| "解像度幅が範囲外です".to_owned())?;
    let h = usize::try_from(height).map_err(|_| "解像度高さが範囲外です".to_owned())?;
    Ok((w, h))
}

/// 1 画素の BGRA8 (B, G, R) を ITU-R BT.709 Studio Swing (`Limited` 範囲) の (Y, Cb, Cr) へ変換する。
#[inline]
#[must_use]
pub fn bgra_pixel_to_bt709_limited(blue: u8, green: u8, red: u8) -> (u8, u8, u8) {
    let r_i = i32::from(red);
    let g_i = i32::from(green);
    let b_i = i32::from(blue);

    let luma = ((187 * r_i + 629 * g_i + 63 * b_i + 512) >> 10) + 16;
    let cb = ((-103 * r_i - 347 * g_i + 450 * b_i + 512) >> 10) + 128;
    let cr = ((450 * r_i - 409 * g_i - 41 * b_i + 512) >> 10) + 128;

    let clamp_u8 =
        |val: i32, lo: i32, hi: i32| -> u8 { u8::try_from(val.clamp(lo, hi)).unwrap_or(128) };
    (
        clamp_u8(luma, 16, 235),
        clamp_u8(cb, 16, 240),
        clamp_u8(cr, 16, 240),
    )
}

/// BGRA8 画像バッファを `rav1e::Frame<u8>` (`YUV420p` / BT.709 Limited Range) へ書き込む。
///
/// 呼び出し側のアルファ前提に依存せず、透明部分はそのまま RGB 値として扱う。
///
/// # Errors
///
/// 入力バイト長が `width * height * 4` と一致しない場合にエラー文字列を返す。
pub fn fill_rav1e_frame_from_bgra(
    bgra: &[u8],
    width: usize,
    height: usize,
    pixel_range: PixelRange,
    frame: &mut Frame<u8>,
) -> Result<(), String> {
    let expected_len = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "BGRA フレームサイズ計算がオーバーフローしました".to_owned())?;
    if bgra.len() != expected_len {
        return Err(format!(
            "BGRA フレームサイズが不正です: expected={expected_len}, actual={}",
            bgra.len()
        ));
    }

    let mut y_plane = vec![16u8; width * height];
    let chroma_w = frame.planes[1].cfg.width;
    let chroma_h = frame.planes[1].cfg.height;
    let mut u_plane = vec![128u8; chroma_w * chroma_h];
    let mut v_plane = vec![128u8; chroma_w * chroma_h];

    convert_luma_plane(bgra, width, height, pixel_range, &mut y_plane);
    let dims = PlaneGeometry {
        luma_w: width,
        luma_h: height,
        chroma_w,
        chroma_h,
    };
    convert_chroma_planes(bgra, dims, &mut u_plane, &mut v_plane);

    frame.planes[0].copy_from_raw_u8(&y_plane, width, 1);
    frame.planes[1].copy_from_raw_u8(&u_plane, chroma_w, 1);
    frame.planes[2].copy_from_raw_u8(&v_plane, chroma_w, 1);
    Ok(())
}

#[derive(Clone, Copy)]
struct PlaneGeometry {
    luma_w: usize,
    luma_h: usize,
    chroma_w: usize,
    chroma_h: usize,
}

fn convert_luma_plane(
    bgra: &[u8],
    width: usize,
    height: usize,
    pixel_range: PixelRange,
    y_out: &mut [u8],
) {
    for row in 0..height {
        let row_offset = row * width;
        for col in 0..width {
            let src_idx = (row_offset + col) * 4;
            let (b, g, r) = (bgra[src_idx], bgra[src_idx + 1], bgra[src_idx + 2]);
            let y_val = match pixel_range {
                PixelRange::Limited => bgra_pixel_to_bt709_limited(b, g, r).0,
                PixelRange::Full => {
                    let y =
                        (218 * i32::from(r) + 732 * i32::from(g) + 74 * i32::from(b) + 512) >> 10;
                    u8::try_from(y.clamp(0, 255)).unwrap_or(0)
                }
            };
            y_out[row_offset + col] = y_val;
        }
    }
}

fn convert_chroma_planes(bgra: &[u8], dims: PlaneGeometry, u_out: &mut [u8], v_out: &mut [u8]) {
    // 4:2:0 サブサンプリングは 2x2 ブロック平均を取り、色滲みや位相ずれを抑える。
    for cy in 0..dims.chroma_h {
        let y0 = (cy * 2).min(dims.luma_h.saturating_sub(1));
        let y1 = (cy * 2 + 1).min(dims.luma_h.saturating_sub(1));
        for cx in 0..dims.chroma_w {
            let x0 = (cx * 2).min(dims.luma_w.saturating_sub(1));
            let x1 = (cx * 2 + 1).min(dims.luma_w.saturating_sub(1));

            let coords = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
            let mut sum_r = 0i32;
            let mut sum_g = 0i32;
            let mut sum_b = 0i32;
            for (px, py) in coords {
                let idx = (py * dims.luma_w + px) * 4;
                sum_b += i32::from(bgra[idx]);
                sum_g += i32::from(bgra[idx + 1]);
                sum_r += i32::from(bgra[idx + 2]);
            }
            let avg_b = u8::try_from((sum_b + 2) / 4).unwrap_or(0);
            let avg_g = u8::try_from((sum_g + 2) / 4).unwrap_or(0);
            let avg_r = u8::try_from((sum_r + 2) / 4).unwrap_or(0);
            let (_, u_val, v_val) = bgra_pixel_to_bt709_limited(avg_b, avg_g, avg_r);

            let dst_idx = cy * dims.chroma_w + cx;
            u_out[dst_idx] = u_val;
            v_out[dst_idx] = v_val;
        }
    }
}

#[cfg(test)]
mod tests {
    use rav1e::prelude::*;

    use super::*;

    #[test]
    fn settings_from_presets_and_overrides_work() {
        let fast = Av1EncodeSettings::from_init("av1_fast", &Value::Null).unwrap();
        assert_eq!(fast.backend_mode, Av1BackendMode::SoftwareRav1e);
        assert_eq!(fast.speed, 9);

        let quality = Av1EncodeSettings::from_init("av1_quality", &Value::Null).unwrap();
        assert_eq!(quality.speed, 5);
        assert_eq!(quality.quantizer, 75);

        let overridden = Av1EncodeSettings::from_init(
            "av1_fast",
            &serde_json::json!({
                "speed": 6,
                "quantizer": 88,
                "threads": 4
            }),
        )
        .unwrap();
        assert_eq!(overridden.speed, 6);
        assert_eq!(overridden.quantizer, 88);
        assert_eq!(overridden.threads, 4);

        let invalid = Av1EncodeSettings::from_init(
            "av1_fast",
            &serde_json::json!({ "backend_mode": "unknown_mode" }),
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn bt709_limited_primaries_match_standard_levels() {
        assert_eq!(bgra_pixel_to_bt709_limited(0, 0, 0), (16, 128, 128));
        assert_eq!(bgra_pixel_to_bt709_limited(255, 255, 255), (235, 128, 128));

        let (y_r, u_r, v_r) = bgra_pixel_to_bt709_limited(0, 0, 255);
        assert!((61..=65).contains(&y_r));
        assert!(v_r >= 235);
        assert!(u_r < 120);
    }

    #[test]
    fn validate_geometry_rejects_too_small_frames() {
        assert!(validate_video_geometry(15, 16).is_err());
        assert!(validate_video_geometry(16, 15).is_err());
        assert_eq!(validate_video_geometry(64, 48).unwrap(), (64, 48));
    }

    #[test]
    fn fill_rav1e_frame_writes_y_u_v_planes() {
        let mut enc = EncoderConfig::with_speed_preset(9);
        enc.width = 32;
        enc.height = 32;
        let ctx: Context<u8> = Config::new()
            .with_encoder_config(enc)
            .new_context()
            .unwrap();
        let mut frame = ctx.new_frame();

        let mut bgra = vec![0u8; 32 * 32 * 4];
        for px in bgra.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[255, 255, 255, 255]);
        }
        fill_rav1e_frame_from_bgra(&bgra, 32, 32, PixelRange::Limited, &mut frame).unwrap();

        assert_eq!(frame.planes[0].p(0, 0), 235);
        assert_eq!(frame.planes[1].p(0, 0), 128);
        assert_eq!(frame.planes[2].p(0, 0), 128);
    }
}
