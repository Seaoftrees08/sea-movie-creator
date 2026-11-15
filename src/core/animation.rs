//! キーフレーム・イージング・数式に基づくアニメーションパラメータ定義と評価エンジン。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::expression::{ExprContext, ExpressionError, evaluate_expression};
use crate::core::time::{FrameCount, FrameRate};

/// アニメーション評価時に渡す時刻コンテキスト。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationContext {
    /// クリップ内ローカルフレーム位置（`0 .. clip.duration`）。
    pub local_frame: u64,
    /// カット・左トリム後も原点を維持するためのオフセット。
    pub effect_offset_frames: i64,
    /// イージング・正規化進捗 `p` / 総時間 `d` に使う基準フレーム長（常に `>= 1`）。
    pub effect_duration_frames: FrameCount,
    /// プロジェクト FPS。
    pub fps: FrameRate,
}

impl EvaluationContext {
    /// 評価用コンテキストを生成する。
    #[must_use]
    pub const fn new(
        local_frame: u64,
        effect_offset_frames: i64,
        effect_duration_frames: FrameCount,
        fps: FrameRate,
    ) -> Self {
        Self {
            local_frame,
            effect_offset_frames,
            effect_duration_frames,
            fps,
        }
    }

    /// カット後も保たれるエフェクト基準フレーム番号（`effect_offset_frames + local_frame`）を返す。
    ///
    /// # Errors
    ///
    /// フレーム加算が `i64` を超える場合に [`AnimationError::FrameOverflow`] を返す。
    pub fn effect_frame(&self) -> Result<i64, AnimationError> {
        let local = i64::try_from(self.local_frame).map_err(|_| AnimationError::FrameOverflow)?;
        self.effect_offset_frames
            .checked_add(local)
            .ok_or(AnimationError::FrameOverflow)
    }

    #[allow(clippy::cast_precision_loss)]
    fn normalized_progress(self, effect_frame: i64) -> f64 {
        let span = self.effect_duration_frames.get();
        if span <= 1 {
            // 1 フレームクリップでの 0/0 除算を防ぐ
            0.0
        } else {
            (effect_frame as f64) / ((span - 1) as f64)
        }
    }
}

/// アニメーションパラメータ評価時に発生し得るエラー。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AnimationError {
    /// エフェクト基準フレームが `i64` 範囲を超えた。
    #[error("effect frame index overflowed i64")]
    FrameOverflow,
    /// キーフレーム配列が空であるため評価できない。
    #[error("keyframe list cannot be empty")]
    EmptyKeyframes,
    /// 数式パラメータの解析・評価に失敗した。
    #[error("expression evaluation failed: {0}")]
    Expression(#[from] ExpressionError),
    /// 評価結果またはパラメータ値が非有限数値 (`NaN`/`Infinity`) になった。
    #[error("animated parameter produced non-finite value")]
    NonFiniteValue,
}

/// クリップの有効範囲やキーフレーム区間の外側における補外モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtrapolationMode {
    /// 区間外の値を端点の値で固定する。
    #[default]
    Clamp,
    /// 端点の傾きや変化率をそのまま外挿する。
    Extrapolate,
    /// 先頭から末尾への変化を繰り返す。
    Loop,
    /// 往復するように反転しながら繰り返す。
    PingPong,
}

/// 2点間または開始値と終了値の間を結ぶ補間カーブ。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EasingCurve {
    /// 一定速度の線形補間。
    #[default]
    Linear,
    /// ゆっくり始まり徐々に加速する補間。
    EaseIn,
    /// 速く始まり徐々に減速する補間。
    EaseOut,
    /// 両端が滑らかに加減速する補間。
    EaseInOut,
    /// 制御点 `(x1, y1)`, `(x2, y2)` を持つ 3 次ベジェ補間。
    CubicBezier {
        /// 最初の制御点の X 座標（通常 `0.0..=1.0`）。
        x1: f64,
        /// 最初の制御点の Y 座標。
        y1: f64,
        /// 2 番目の制御点の X 座標（通常 `0.0..=1.0`）。
        x2: f64,
        /// 2 番目の制御点の Y 座標。
        y2: f64,
    },
}

impl EasingCurve {
    /// 正規化進行率 `u` (`0.0..=1.0`) に対するカーブ値 `y` を評価する。
    #[must_use]
    pub fn sample(self, u: f64) -> f64 {
        let t = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => {
                let inv = 1.0 - t;
                1.0 - inv * inv
            }
            Self::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    let inv = 2.0f64.mul_add(-t, 2.0);
                    1.0 - (inv * inv) * 0.5
                }
            }
            Self::CubicBezier { x1, y1, x2, y2 } => {
                sample_cubic_bezier(t, x1.clamp(0.0, 1.0), y1, x2.clamp(0.0, 1.0), y2)
            }
        }
    }

    fn slope_at_start(self) -> f64 {
        match self {
            Self::Linear => 1.0,
            Self::EaseIn | Self::EaseInOut => 0.0,
            Self::EaseOut => 2.0,
            Self::CubicBezier { x1, y1, .. } => {
                if x1 > 1e-6 {
                    y1 / x1
                } else {
                    0.0
                }
            }
        }
    }

    fn slope_at_end(self) -> f64 {
        match self {
            Self::Linear => 1.0,
            Self::EaseIn => 2.0,
            Self::EaseOut | Self::EaseInOut => 0.0,
            Self::CubicBezier { x2, y2, .. } => {
                let dx = 1.0 - x2;
                if dx > 1e-6 { (1.0 - y2) / dx } else { 0.0 }
            }
        }
    }
}

/// エフェクト原点からの相対フレーム位置と値の組を表すキーフレーム。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Keyframe {
    /// エフェクト基準フレーム（`effect_offset_frames + local_frame`）上の位置。
    pub frame: i64,
    /// そのフレームにおけるパラメータ値。
    pub value: f64,
    /// このキーフレームから次のキーフレームへ向かう際の補間カーブ。
    #[serde(default)]
    pub easing: EasingCurve,
}

impl Keyframe {
    /// 線形補間を既定として新しいキーフレームを生成する。
    #[must_use]
    pub const fn new(frame: i64, value: f64) -> Self {
        Self {
            frame,
            value,
            easing: EasingCurve::Linear,
        }
    }

    /// 指定したイージングカーブ付きの新しいキーフレームを生成する。
    #[must_use]
    pub const fn with_easing(frame: i64, value: f64, easing: EasingCurve) -> Self {
        Self {
            frame,
            value,
            easing,
        }
    }
}

/// クリップおよびエフェクト上で時刻に応じて変化する数値パラメータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnimatedParameter {
    /// 時間変化しない固定値。
    Constant {
        /// 固定値。
        value: f64,
    },
    /// エフェクト基準時間全体を使って開始値から終了値へ補間する設定。
    Easing {
        /// 開始値。
        start_value: f64,
        /// 終了値。
        end_value: f64,
        /// 補間カーブ。
        #[serde(default)]
        curve: EasingCurve,
        /// 範囲外時刻での外挿挙動。
        #[serde(default)]
        extrapolation: ExtrapolationMode,
    },
    /// 複数のキーフレーム列で値を制御する設定。
    Keyframes {
        /// キーフレーム一覧。評価時にはフレーム昇順で扱われる。
        keyframes: Vec<Keyframe>,
        /// キーフレーム範囲外での外挿挙動。
        #[serde(default)]
        extrapolation: ExtrapolationMode,
    },
    /// サンドボックス化された純粋 Rust 数式 `f(t)` によって値を計算する設定。
    Expression {
        /// 評価する数式文字列（利用可能変数: `t`, `p`, `d`, `pi`, `e`）。
        expr: String,
    },
}

impl AnimatedParameter {
    /// 時間変化しない固定値パラメータを作成する。
    #[must_use]
    pub const fn constant(value: f64) -> Self {
        Self::Constant { value }
    }

    /// 指定された時刻コンテキストにおけるパラメータ値を評価する。
    ///
    /// # Errors
    ///
    /// 空キーフレーム・数式エラー・オーバーフロー・非有限値発生時に [`AnimationError`] を返す。
    #[allow(clippy::cast_precision_loss)]
    pub fn evaluate(&self, ctx: EvaluationContext) -> Result<f64, AnimationError> {
        let effect_frame = ctx.effect_frame()?;
        let val = match self {
            Self::Constant { value } => *value,
            Self::Easing {
                start_value,
                end_value,
                curve,
                extrapolation,
            } => evaluate_easing(
                *start_value,
                *end_value,
                *curve,
                *extrapolation,
                ctx.normalized_progress(effect_frame),
            ),
            Self::Keyframes {
                keyframes,
                extrapolation,
            } => evaluate_keyframes(keyframes, *extrapolation, effect_frame as f64)?,
            Self::Expression { expr } => {
                let fps_f64 = ctx.fps.as_f64();
                let t = (effect_frame as f64) / fps_f64;
                let p = ctx.normalized_progress(effect_frame);
                let d = (ctx.effect_duration_frames.get() as f64) / fps_f64;
                evaluate_expression(expr, ExprContext { t, p, d })?
            }
        };

        if val.is_finite() {
            Ok(val)
        } else {
            Err(AnimationError::NonFiniteValue)
        }
    }
}

impl Default for AnimatedParameter {
    fn default() -> Self {
        Self::constant(0.0)
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    (b - a).mul_add(t, a)
}

fn evaluate_easing(
    start_value: f64,
    end_value: f64,
    curve: EasingCurve,
    extrapolation: ExtrapolationMode,
    raw_u: f64,
) -> f64 {
    let factor = match extrapolation {
        ExtrapolationMode::Clamp => curve.sample(raw_u.clamp(0.0, 1.0)),
        ExtrapolationMode::Extrapolate => {
            if raw_u < 0.0 {
                curve.slope_at_start() * raw_u
            } else if raw_u > 1.0 {
                curve.slope_at_end().mul_add(raw_u - 1.0, 1.0)
            } else {
                curve.sample(raw_u)
            }
        }
        ExtrapolationMode::Loop => {
            let wrapped = if (0.0..=1.0).contains(&raw_u) {
                raw_u
            } else {
                raw_u.rem_euclid(1.0)
            };
            curve.sample(wrapped)
        }
        ExtrapolationMode::PingPong => {
            let folded = if (0.0..=1.0).contains(&raw_u) {
                raw_u
            } else {
                let cycle = raw_u.rem_euclid(2.0);
                if cycle <= 1.0 { cycle } else { 2.0 - cycle }
            };
            curve.sample(folded)
        }
    };
    lerp(start_value, end_value, factor)
}

#[allow(clippy::cast_precision_loss)]
fn evaluate_keyframes(
    keyframes: &[Keyframe],
    extrapolation: ExtrapolationMode,
    t: f64,
) -> Result<f64, AnimationError> {
    if keyframes.is_empty() {
        return Err(AnimationError::EmptyKeyframes);
    }

    let mut ordered: Vec<&Keyframe> = keyframes.iter().collect();
    ordered.sort_by_key(|kf| kf.frame);

    if ordered.len() == 1 {
        return Ok(ordered[0].value);
    }

    let first = ordered[0];
    let last = ordered[ordered.len() - 1];
    let first_t = first.frame as f64;
    let last_t = last.frame as f64;
    let total_span = last_t - first_t;

    if total_span <= 0.0 {
        return Ok(last.value);
    }

    if t < first_t || t > last_t {
        match extrapolation {
            ExtrapolationMode::Clamp => {
                return Ok(if t < first_t { first.value } else { last.value });
            }
            ExtrapolationMode::Extrapolate => {
                return Ok(extrapolate_keyframe_boundary(&ordered, t, first_t));
            }
            ExtrapolationMode::Loop => {
                let wrapped = first_t + (t - first_t).rem_euclid(total_span);
                return Ok(sample_ordered_keyframes(&ordered, wrapped));
            }
            ExtrapolationMode::PingPong => {
                let cycle = (t - first_t).rem_euclid(2.0 * total_span);
                let mirrored = if cycle <= total_span {
                    first_t + cycle
                } else {
                    last_t - (cycle - total_span)
                };
                return Ok(sample_ordered_keyframes(&ordered, mirrored));
            }
        }
    }

    Ok(sample_ordered_keyframes(&ordered, t))
}

#[allow(clippy::cast_precision_loss)]
fn extrapolate_keyframe_boundary(ordered: &[&Keyframe], t: f64, first_t: f64) -> f64 {
    if t < first_t {
        let k0 = ordered[0];
        let k1 = ordered[1];
        let dt = (k1.frame - k0.frame) as f64;
        if dt <= 0.0 {
            k0.value
        } else {
            let slope = (k1.value - k0.value) / dt;
            slope.mul_add(t - (k0.frame as f64), k0.value)
        }
    } else {
        let n = ordered.len();
        let prev = ordered[n - 2];
        let last = ordered[n - 1];
        let dt = (last.frame - prev.frame) as f64;
        if dt <= 0.0 {
            last.value
        } else {
            let slope = (last.value - prev.value) / dt;
            slope.mul_add(t - (last.frame as f64), last.value)
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn sample_ordered_keyframes(ordered: &[&Keyframe], t: f64) -> f64 {
    if t <= ordered[0].frame as f64 {
        return ordered[0].value;
    }
    let last = ordered[ordered.len() - 1];
    if t >= last.frame as f64 {
        return last.value;
    }

    for pair in ordered.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let a_t = a.frame as f64;
        let b_t = b.frame as f64;
        if t <= b_t {
            let dt = b_t - a_t;
            if dt <= 0.0 {
                return b.value;
            }
            let u = (t - a_t) / dt;
            let eased = a.easing.sample(u);
            return lerp(a.value, b.value, eased);
        }
    }
    last.value
}

fn sample_cubic_bezier(target_x: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    if target_x <= 0.0 {
        return 0.0;
    }
    if target_x >= 1.0 {
        return 1.0;
    }

    // 二分法で区間 [lo, hi] を確実に狭めつつ、ニュートン法で高速収束させる
    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;
    let mut s = target_x;

    for _ in 0..16 {
        let x = cubic_bezier_coord(s, x1, x2);
        let err = x - target_x;
        if err.abs() < 1e-7 {
            break;
        }
        if err < 0.0 {
            lo = s;
        } else {
            hi = s;
        }
        let dx = cubic_bezier_derivative(s, x1, x2);
        let newton = if dx.abs() > 1e-6 {
            s - err / dx
        } else {
            f64::NAN
        };
        if newton > lo && newton < hi {
            s = newton;
        } else {
            s = f64::midpoint(lo, hi);
        }
    }

    cubic_bezier_coord(s, y1, y2)
}

fn cubic_bezier_coord(t: f64, p1: f64, p2: f64) -> f64 {
    let inv = 1.0 - t;
    let t2 = t * t;
    3.0 * inv * inv * t * p1 + 3.0 * inv * t2 * p2 + t2 * t
}

fn cubic_bezier_derivative(t: f64, p1: f64, p2: f64) -> f64 {
    let inv = 1.0 - t;
    3.0 * inv * inv * p1 + 6.0 * inv * t * (p2 - p1) + 3.0 * t * t * (1.0 - p2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_parameter_serde_roundtrip() -> Result<(), serde_json::Error> {
        let param = AnimatedParameter::Keyframes {
            keyframes: vec![
                Keyframe::new(0, 100.0),
                Keyframe::with_easing(
                    30,
                    250.0,
                    EasingCurve::CubicBezier {
                        x1: 0.42,
                        y1: 0.0,
                        x2: 0.58,
                        y2: 1.0,
                    },
                ),
            ],
            extrapolation: ExtrapolationMode::Clamp,
        };

        let json = serde_json::to_string(&param)?;
        let restored: AnimatedParameter = serde_json::from_str(&json)?;
        assert_eq!(restored, param);
        Ok(())
    }

    #[test]
    fn one_frame_easing_does_not_divide_by_zero() -> Result<(), Box<dyn std::error::Error>> {
        let param = AnimatedParameter::Easing {
            start_value: 25.0,
            end_value: 100.0,
            curve: EasingCurve::EaseInOut,
            extrapolation: ExtrapolationMode::Clamp,
        };
        let ctx = EvaluationContext::new(0, 0, FrameCount::ONE, FrameRate::FPS_60);
        assert!((param.evaluate(ctx)? - 25.0).abs() < 1e-9);
        Ok(())
    }

    #[test]
    fn keyframe_extrapolation_modes_behave_accurately() -> Result<(), Box<dyn std::error::Error>> {
        let make = |mode| AnimatedParameter::Keyframes {
            keyframes: vec![Keyframe::new(10, 0.0), Keyframe::new(20, 100.0)],
            extrapolation: mode,
        };
        let dur = FrameCount::new(40)?;
        let ctx_at = |eff: i64| EvaluationContext::new(0, eff, dur, FrameRate::FPS_60);

        assert!((make(ExtrapolationMode::Clamp).evaluate(ctx_at(25))? - 100.0).abs() < 1e-9);
        assert!((make(ExtrapolationMode::Extrapolate).evaluate(ctx_at(25))? - 150.0).abs() < 1e-9);
        assert!((make(ExtrapolationMode::Loop).evaluate(ctx_at(25))? - 50.0).abs() < 1e-9);
        assert!((make(ExtrapolationMode::PingPong).evaluate(ctx_at(23))? - 70.0).abs() < 1e-9);
        Ok(())
    }
}
