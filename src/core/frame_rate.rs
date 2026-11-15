//! フレーム数や再生時間に関する基本的な計算処理を提供するモジュール。
//!
//! Phase 0 の環境・自動チェック・テスト運用を確認するための仮実装を含む。

use std::fmt;

/// フレーム計算時に発生し得るエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCalculationError {
    /// フレームレートに 0 が指定されたため計算できない。
    ZeroFrameRate,
    /// 計算結果が `u64` の上限を超えた。
    Overflow,
}

impl fmt::Display for FrameCalculationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFrameRate => write!(formatter, "frames_per_second must be greater than 0"),
            Self::Overflow => write!(formatter, "calculated frame count overflows u64"),
        }
    }
}

impl std::error::Error for FrameCalculationError {}

/// ミリ秒単位の再生時間とフレームレート（FPS）から、総フレーム数を計算する。
///
/// 1 フレーム未満の端数は切り捨てる。
///
/// # Errors
///
/// - `frames_per_second` が 0 の場合、[`FrameCalculationError::ZeroFrameRate`] を返す。
/// - 計算結果が `u64` の最大値を超える場合、[`FrameCalculationError::Overflow`] を返す。
pub fn calculate_total_frames(
    duration_millis: u64,
    frames_per_second: u32,
) -> Result<u64, FrameCalculationError> {
    if frames_per_second == 0 {
        return Err(FrameCalculationError::ZeroFrameRate);
    }

    // 浮動小数点数の丸め誤差によるフレームずれを防ぐため、u128 の整数演算へ拡張して
    // 「ミリ秒 × FPS ÷ 1000」を計算してから u64 へ安全に変換する。
    let scaled_frames = u128::from(duration_millis) * u128::from(frames_per_second);
    let total_frames = scaled_frames / 1_000;

    u64::try_from(total_frames).map_err(|_| FrameCalculationError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_duration_millis_and_fps_to_total_frames() {
        assert_eq!(calculate_total_frames(2_500, 60), Ok(150));
        assert_eq!(calculate_total_frames(0, 30), Ok(0));
    }

    #[test]
    fn returns_error_when_fps_is_zero_or_result_overflows() {
        assert_eq!(
            calculate_total_frames(1_000, 0),
            Err(FrameCalculationError::ZeroFrameRate),
        );
        assert_eq!(
            calculate_total_frames(u64::MAX, 2_000),
            Err(FrameCalculationError::Overflow),
        );
    }
}
