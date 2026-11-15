//! フレーム境界・有理数再生速度・既約有理数秒を扱うOS非依存の時間表現モジュール。
//!
//! 29.97fps (`30000/1001`) 等の非整数 FPS で丸め誤差やクリップ間の隙間を防ぐため、
//! タイムライン配置は整数フレームを唯一の正とし、元メディア時刻は `u128` 中間精度の
//! 既約有理数演算で厳密に計算する。

use std::cmp::Ordering;
use std::num::NonZeroU64;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 時間表現および有理数計算で発生し得るエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TimeError {
    /// 分子に 0 が指定された（正の値のみ許可される文脈）。
    #[error("numerator must be greater than 0")]
    ZeroNumerator,
    /// 分母に 0 が指定された。
    #[error("denominator must be greater than 0")]
    ZeroDenominator,
    /// フレーム長（クリップ長・エフェクト基準長）に 0 が指定された。
    #[error("frame count must be at least 1")]
    ZeroFrameCount,
    /// 減算結果が負になり非負不変条件を満たさない。
    #[error("duration or frame position underflowed below 0")]
    Underflow,
    /// 64bit / 128bit 境界を超えるオーバーフローが発生した。
    #[error("time calculation overflowed")]
    Overflow,
}

const fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let rem = a % b;
        a = b;
        b = rem;
    }
    if a == 0 { 1 } else { a }
}

const fn gcd_u32(a: u32, b: u32) -> u32 {
    let mut x = a;
    let mut y = b;
    while y != 0 {
        let rem = x % y;
        x = y;
        y = rem;
    }
    if x == 0 { 1 } else { x }
}

/// プロジェクトのフレームレート（正の既約有理数 FPS）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema)]
pub struct FrameRate {
    #[schemars(range(min = 1))]
    numerator: u32,
    #[schemars(range(min = 1))]
    denominator: u32,
}

impl FrameRate {
    /// 既定の 60fps (`60/1`)。
    pub const FPS_60: Self = Self {
        numerator: 60,
        denominator: 1,
    };

    /// 既定の 30fps (`30/1`)。
    pub const FPS_30: Self = Self {
        numerator: 30,
        denominator: 1,
    };

    /// NTSC 29.97fps (`30000/1001`)。
    pub const FPS_29_97: Self = Self {
        numerator: 30_000,
        denominator: 1_001,
    };

    /// 正の分子・分母から既約 `FrameRate` を生成する。
    ///
    /// # Errors
    ///
    /// - `numerator == 0` の場合 [`TimeError::ZeroNumerator`] を返す。
    /// - `denominator == 0` の場合 [`TimeError::ZeroDenominator`] を返す。
    pub const fn new(numerator: u32, denominator: u32) -> Result<Self, TimeError> {
        if numerator == 0 {
            return Err(TimeError::ZeroNumerator);
        }
        if denominator == 0 {
            return Err(TimeError::ZeroDenominator);
        }
        let divisor = gcd_u32(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// 分子を返す。
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    /// 分母を返す。
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    /// 表示・近似計算向けの `f64` FPS 値を返す。
    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::FPS_60
    }
}

#[derive(Deserialize)]
struct RawRational32 {
    numerator: u32,
    denominator: u32,
}

impl<'de> Deserialize<'de> for FrameRate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawRational32::deserialize(deserializer)?;
        Self::new(raw.numerator, raw.denominator).map_err(serde::de::Error::custom)
    }
}

/// タイムライン上の非負整数フレーム位置。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
pub struct FrameIndex(pub u64);

impl FrameIndex {
    /// 先頭フレーム (`0`)。
    pub const ZERO: Self = Self(0);

    /// フレーム番号から [`FrameIndex`] を生成する。
    #[must_use]
    pub const fn new(frame: u64) -> Self {
        Self(frame)
    }

    /// 生のフレーム番号を返す。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// フレーム長を加算した終了フレーム位置（排他的上端）を返す。
    ///
    /// # Errors
    ///
    /// `u64` 範囲を超える場合 [`TimeError::Overflow`] を返す。
    pub const fn checked_add_count(self, count: FrameCount) -> Result<Self, TimeError> {
        match self.0.checked_add(count.get()) {
            Some(sum) => Ok(Self(sum)),
            None => Err(TimeError::Overflow),
        }
    }

    /// 符号付きオフセットを加算する（負になる場合はアンダーフローエラー）。
    ///
    /// # Errors
    ///
    /// 結果が負または `u64::MAX` を超える場合、[`TimeError::Underflow`] または [`TimeError::Overflow`] を返す。
    pub const fn checked_add_signed(self, delta: i64) -> Result<Self, TimeError> {
        if delta >= 0 {
            let add = delta.cast_unsigned();
            match self.0.checked_add(add) {
                Some(sum) => Ok(Self(sum)),
                None => Err(TimeError::Overflow),
            }
        } else {
            let sub = delta.unsigned_abs();
            match self.0.checked_sub(sub) {
                Some(diff) => Ok(Self(diff)),
                None => Err(TimeError::Underflow),
            }
        }
    }
}

/// 1 以上のフレーム長（0 フレームクリップを型レベルで禁止）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct FrameCount(NonZeroU64);

impl FrameCount {
    /// `frames >= 1` から [`FrameCount`] を生成する。
    ///
    /// # Errors
    ///
    /// `frames == 0` の場合 [`TimeError::ZeroFrameCount`] を返す。
    pub const fn new(frames: u64) -> Result<Self, TimeError> {
        match NonZeroU64::new(frames) {
            Some(nz) => Ok(Self(nz)),
            None => Err(TimeError::ZeroFrameCount),
        }
    }

    /// 1 フレーム。
    pub const ONE: Self = match NonZeroU64::new(1) {
        Some(nz) => Self(nz),
        None => unreachable!(),
    };

    /// フレーム数を `u64` で返す（常に `>= 1`）。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// 符号付き差分を減算する（トリム量 `delta` が正のとき縮み、負のとき伸びる）。
    ///
    /// # Errors
    ///
    /// 結果が 1 未満または `u64` オーバーフローの場合エラーを返す。
    pub const fn checked_sub_signed(self, delta: i64) -> Result<Self, TimeError> {
        let next = if delta >= 0 {
            match self.0.get().checked_sub(delta.cast_unsigned()) {
                Some(val) => val,
                None => return Err(TimeError::ZeroFrameCount),
            }
        } else {
            match self.0.get().checked_add(delta.unsigned_abs()) {
                Some(val) => val,
                None => return Err(TimeError::Overflow),
            }
        };
        Self::new(next)
    }
}

/// クリップ単位の再生速度倍率（正の既約有理数、既定 `1/1`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema)]
pub struct PlaybackRate {
    #[schemars(range(min = 1))]
    numerator: u32,
    #[schemars(range(min = 1))]
    denominator: u32,
}

impl PlaybackRate {
    /// 等倍速 (`1/1`)。
    pub const NORMAL: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    /// 正の分子・分母から既約 `PlaybackRate` を生成する。
    ///
    /// # Errors
    ///
    /// - `numerator == 0` の場合 [`TimeError::ZeroNumerator`] を返す。
    /// - `denominator == 0` の場合 [`TimeError::ZeroDenominator`] を返す。
    pub const fn new(numerator: u32, denominator: u32) -> Result<Self, TimeError> {
        if numerator == 0 {
            return Err(TimeError::ZeroNumerator);
        }
        if denominator == 0 {
            return Err(TimeError::ZeroDenominator);
        }
        let divisor = gcd_u32(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// 分子を返す。
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    /// 分母を返す。
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    /// 浮動小数点の倍速表示値を返す。
    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

impl Default for PlaybackRate {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl<'de> Deserialize<'de> for PlaybackRate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawRational32::deserialize(deserializer)?;
        Self::new(raw.numerator, raw.denominator).map_err(serde::de::Error::custom)
    }
}

/// 秒単位の非負既約有理数（`source_offset` や `asset.duration` に使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema)]
pub struct RationalDuration {
    numerator: u64,
    #[schemars(range(min = 1))]
    denominator: u64,
}

impl RationalDuration {
    /// `0` 秒 (`0/1`)。
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    /// 分子・分母から既約 [`RationalDuration`] を生成する。
    ///
    /// # Errors
    ///
    /// `denominator == 0` の場合 [`TimeError::ZeroDenominator`] を返す。
    pub const fn new(numerator: u64, denominator: u64) -> Result<Self, TimeError> {
        Self::from_u128(numerator as u128, denominator as u128)
    }

    /// `u128` 分数から約分後に `u64` へ安全に収める。
    ///
    /// 中間積が `u64::MAX` を一時的に超えても約分後に収まるケースを許容する。
    ///
    /// # Errors
    ///
    /// `denominator == 0` または約分後の分子/分母が `u64` を超える場合にエラーを返す。
    pub const fn from_u128(numerator: u128, denominator: u128) -> Result<Self, TimeError> {
        if denominator == 0 {
            return Err(TimeError::ZeroDenominator);
        }
        if numerator == 0 {
            return Ok(Self::ZERO);
        }
        let divisor = gcd_u128(numerator, denominator);
        let reduced_num = numerator / divisor;
        let reduced_den = denominator / divisor;
        if reduced_num > u64::MAX as u128 || reduced_den > u64::MAX as u128 {
            return Err(TimeError::Overflow);
        }
        #[allow(clippy::cast_possible_truncation)]
        // 上の条件分岐で u64::MAX 以下であることを検証済みのため切り捨ては起きない
        Ok(Self {
            numerator: reduced_num as u64,
            denominator: reduced_den as u64,
        })
    }

    /// ミリ秒整数から有理数秒を生成する。
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        match Self::from_u128(millis as u128, 1_000) {
            Ok(dur) => dur,
            Err(_) => unreachable!(),
        }
    }

    /// 分子を返す。
    #[must_use]
    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    /// 分母を返す（常に `>= 1`）。
    #[must_use]
    pub const fn denominator(self) -> u64 {
        self.denominator
    }

    /// 近似秒 (`f64`) を返す。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    // 表示・補間時刻用途の浮動小数点変換（タイムライン演算では既約分数を正とする）
    pub const fn as_seconds_f64(self) -> f64 {
        (self.numerator as f64) / (self.denominator as f64)
    }

    /// 2 つの既約有理数を厳密加算し既約化して返す。
    ///
    /// # Errors
    ///
    /// 通分・約分結果が `u64` に収まらない場合 [`TimeError::Overflow`] を返す。
    pub const fn checked_add(self, rhs: Self) -> Result<Self, TimeError> {
        let g = gcd_u128(self.denominator as u128, rhs.denominator as u128);
        let rhs_scale = (rhs.denominator as u128) / g;
        let lhs_scale = (self.denominator as u128) / g;
        let Some(lhs_term) = (self.numerator as u128).checked_mul(rhs_scale) else {
            return Err(TimeError::Overflow);
        };
        let Some(rhs_term) = (rhs.numerator as u128).checked_mul(lhs_scale) else {
            return Err(TimeError::Overflow);
        };
        let Some(num) = lhs_term.checked_add(rhs_term) else {
            return Err(TimeError::Overflow);
        };
        let Some(den) = (self.denominator as u128).checked_mul(rhs_scale) else {
            return Err(TimeError::Overflow);
        };
        Self::from_u128(num, den)
    }

    /// 2 つの既約有理数を厳密減算する（負になる場合は [`TimeError::Underflow`]）。
    ///
    /// # Errors
    ///
    /// `self < rhs` のとき [`TimeError::Underflow`]、通分積超過時は [`TimeError::Overflow`] を返す。
    pub const fn checked_sub(self, rhs: Self) -> Result<Self, TimeError> {
        let g = gcd_u128(self.denominator as u128, rhs.denominator as u128);
        let rhs_scale = (rhs.denominator as u128) / g;
        let lhs_scale = (self.denominator as u128) / g;
        let Some(lhs_term) = (self.numerator as u128).checked_mul(rhs_scale) else {
            return Err(TimeError::Overflow);
        };
        let Some(rhs_term) = (rhs.numerator as u128).checked_mul(lhs_scale) else {
            return Err(TimeError::Overflow);
        };
        let Some(num) = lhs_term.checked_sub(rhs_term) else {
            return Err(TimeError::Underflow);
        };
        let Some(den) = (self.denominator as u128).checked_mul(rhs_scale) else {
            return Err(TimeError::Overflow);
        };
        Self::from_u128(num, den)
    }

    /// 符号付きオフセットの有理数加算を行う。
    ///
    /// # Errors
    ///
    /// 負結果またはオーバーフロー時にエラーを返す。
    pub const fn checked_add_signed_frames(
        self,
        delta_frames: i64,
        fps: FrameRate,
        rate: PlaybackRate,
    ) -> Result<Self, TimeError> {
        if delta_frames >= 0 {
            match frames_to_source_seconds(delta_frames.cast_unsigned(), fps, rate) {
                Ok(span) => self.checked_add(span),
                Err(err) => Err(err),
            }
        } else {
            match frames_to_source_seconds(delta_frames.unsigned_abs(), fps, rate) {
                Ok(span) => self.checked_sub(span),
                Err(err) => Err(err),
            }
        }
    }

    /// 指定サンプルレートにおける切り捨てサンプル番号を厳密整数計算で返す。
    ///
    /// # Errors
    ///
    /// 中間計算が `u128` に収まらない場合 [`TimeError::Overflow`] を返す。
    pub const fn to_sample_index_floor(self, sample_rate: u32) -> Result<u64, TimeError> {
        let Some(scaled) = (self.numerator as u128).checked_mul(sample_rate as u128) else {
            return Err(TimeError::Overflow);
        };
        let samples = scaled / (self.denominator as u128);
        if samples > u64::MAX as u128 {
            return Err(TimeError::Overflow);
        }
        #[allow(clippy::cast_possible_truncation)]
        // 上の分岐で u64::MAX 以下を確認済み
        Ok(samples as u64)
    }
}

impl Default for RationalDuration {
    fn default() -> Self {
        Self::ZERO
    }
}

impl Ord for RationalDuration {
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = u128::from(self.numerator) * u128::from(other.denominator);
        let rhs = u128::from(other.numerator) * u128::from(self.denominator);
        lhs.cmp(&rhs)
    }
}

impl PartialOrd for RationalDuration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Deserialize)]
struct RawRational64 {
    numerator: u64,
    denominator: u64,
}

impl<'de> Deserialize<'de> for RationalDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawRational64::deserialize(deserializer)?;
        Self::new(raw.numerator, raw.denominator).map_err(serde::de::Error::custom)
    }
}

/// タイムライン上のフレーム数と再生速度から、元メディア上の厳密な進行時間（秒・既約分数）を算出する。
///
/// 式: `frames * fps.denominator * rate.numerator / (fps.numerator * rate.denominator)`
///
/// # Errors
///
/// 中間積・約分結果が範囲を超える場合 [`TimeError::Overflow`] を返す。
pub const fn frames_to_source_seconds(
    frames: u64,
    fps: FrameRate,
    rate: PlaybackRate,
) -> Result<RationalDuration, TimeError> {
    if frames == 0 {
        return Ok(RationalDuration::ZERO);
    }
    // 128bit 演算上で分母・分子の因数ごと先に整理してから積を取り、約分後に u64 へ収める。
    let Some(fps_den_rate_num) = (fps.denominator() as u128).checked_mul(rate.numerator() as u128)
    else {
        return Err(TimeError::Overflow);
    };
    let Some(num) = (frames as u128).checked_mul(fps_den_rate_num) else {
        return Err(TimeError::Overflow);
    };
    let den = (fps.numerator() as u128) * (rate.denominator() as u128);
    RationalDuration::from_u128(num, den)
}

/// プロジェクトフレーム番号をプロジェクト経過時間（等倍 `1/1` 秒・既約分数）へ変換する。
///
/// # Errors
///
/// 約分結果が `u64` を超える場合 [`TimeError::Overflow`] を返す。
pub const fn frames_to_project_seconds(
    frames: u64,
    fps: FrameRate,
) -> Result<RationalDuration, TimeError> {
    frames_to_source_seconds(frames, fps, PlaybackRate::NORMAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntsc_2997_with_playback_rate_3_over_2_has_zero_rounding_error() {
        let fps = FrameRate::FPS_29_97;
        let rate = PlaybackRate::new(3, 2).expect("valid rate");
        // 30000 frames at 30000/1001 fps = 1001 seconds timeline, x 3/2 = 3003/2 seconds source.
        let dur = frames_to_source_seconds(30_000, fps, rate).expect("exact rational");
        assert_eq!(dur, RationalDuration::new(3_003, 2).expect("expected"));
        assert_eq!(dur.numerator(), 3_003);
        assert_eq!(dur.denominator(), 2);
    }

    #[test]
    fn sequential_splits_sum_exactly_to_original_duration() {
        let fps = FrameRate::FPS_29_97;
        let rate = PlaybackRate::new(3, 2).expect("valid rate");
        let part_a = frames_to_source_seconds(17, fps, rate).expect("part a");
        let part_b = frames_to_source_seconds(43, fps, rate).expect("part b");
        let part_c = frames_to_source_seconds(40, fps, rate).expect("part c");
        let total = frames_to_source_seconds(100, fps, rate).expect("total");

        let sum = part_a
            .checked_add(part_b)
            .and_then(|s| s.checked_add(part_c))
            .expect("exact add");
        assert_eq!(sum, total);
    }

    #[test]
    fn deserialization_and_constructors_reject_zero_frame_count_and_zero_rate() {
        assert_eq!(FrameCount::new(0), Err(TimeError::ZeroFrameCount));
        assert_eq!(PlaybackRate::new(0, 1), Err(TimeError::ZeroNumerator));
        assert_eq!(PlaybackRate::new(1, 0), Err(TimeError::ZeroDenominator));
        assert_eq!(FrameRate::new(0, 1), Err(TimeError::ZeroNumerator));

        assert!(serde_json::from_str::<FrameCount>("0").is_err());
        assert!(
            serde_json::from_str::<PlaybackRate>(r#"{"numerator":0,"denominator":1}"#).is_err()
        );
        assert!(serde_json::from_str::<FrameRate>(r#"{"numerator":60,"denominator":0}"#).is_err());
    }
}
