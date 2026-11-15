//! 映像・画像・字幕クリップの視覚プロパティおよび配置座標・変換行列評価。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::core::animation::{AnimatedParameter, AnimationError, EvaluationContext};
use crate::core::color::RgbaColor;

/// 合成時のブレンドモード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    /// 通常のアルファブレンド。
    #[default]
    Normal,
    /// 加算合成。
    Add,
    /// 乗算合成。
    Multiply,
    /// スクリーン合成。
    Screen,
    /// オーバーレイ合成。
    Overlay,
}

/// キャンバス中心 `(0, 0)`（+X 右, +Y 下）座標系での 2D アフィン変換行列。
///
/// 行優先レイアウト `[ [a, b, tx], [c, d, ty], [0, 0, 1] ]` に対応する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix2D {
    /// 行列要素 `[a, b, tx, c, d, ty]`。
    pub coeffs: [f64; 6],
}

impl Matrix2D {
    /// 恒等変換行列。
    pub const IDENTITY: Self = Self {
        coeffs: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    };

    /// ローカル座標 `(local_x, local_y)` へアフィン変換を適用した座標を返す。
    #[must_use]
    pub fn transform_point(self, local_x: f64, local_y: f64) -> (f64, f64) {
        let [m00, m01, tx, m10, m11, ty] = self.coeffs;
        (
            m00 * local_x + m01 * local_y + tx,
            m10 * local_x + m11 * local_y + ty,
        )
    }

    /// キャンバス左上原点のピクセル座標系へ平行移動（`+w/2, +h/2`）した行列を返す。
    #[must_use]
    pub fn to_canvas_pixels(self, canvas_width: u32, canvas_height: u32) -> Self {
        let [m00, m01, tx, m10, m11, ty] = self.coeffs;
        Self {
            coeffs: [
                m00,
                m01,
                tx + f64::from(canvas_width) * 0.5,
                m10,
                m11,
                ty + f64::from(canvas_height) * 0.5,
            ],
        }
    }

    /// 位置 `(x, y)`・拡大率・アンカー・時計回り回転角（度）から 2D 変換行列を構築する。
    #[must_use]
    pub fn from_transform(
        position: (f64, f64),
        scale: (f64, f64),
        anchor: (f64, f64),
        rotation_deg: f64,
    ) -> Self {
        let (sin_r, cos_r) = rotation_deg.to_radians().sin_cos();
        // 順序: (-anchor_x, -anchor_y) -> scale -> clock-wise rotate (+Y down) -> translate(x, y)
        let m00 = cos_r * scale.0;
        let m01 = -sin_r * scale.1;
        let m10 = sin_r * scale.0;
        let m11 = cos_r * scale.1;
        let tx = position.0 - (m00 * anchor.0 + m01 * anchor.1);
        let ty = position.1 - (m10 * anchor.0 + m11 * anchor.1);
        Self {
            coeffs: [m00, m01, tx, m10, m11, ty],
        }
    }
}

impl Default for Matrix2D {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// ある 1 フレーム時点で評価済みの視覚パラメータおよび変換行列。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvaluatedVisual {
    /// キャンバス中心からの X オフセット。
    pub x: f64,
    /// キャンバス中心からの Y オフセット。
    pub y: f64,
    /// X 方向拡大率。
    pub scale_x: f64,
    /// Y 方向拡大率。
    pub scale_y: f64,
    /// 時計回り回転角（度）。
    pub rotation_deg: f64,
    /// アンカーポイント X オフセット。
    pub anchor_x: f64,
    /// アンカーポイント Y オフセット。
    pub anchor_y: f64,
    /// クランプ済み不透明度 (`0.0..=1.0`)。
    pub opacity: f64,
    /// ブレンドモード。
    pub blend_mode: BlendMode,
    /// `translate(-anchor) -> scale -> rotate_cw -> translate(x, y)` の合成 2D 変換行列。
    pub matrix: Matrix2D,
}

impl EvaluatedVisual {
    /// 各評価値から 2D 変換行列を組み立てて生成する。
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_components(
        x: f64,
        y: f64,
        scale_x: f64,
        scale_y: f64,
        rotation_deg: f64,
        anchor_x: f64,
        anchor_y: f64,
        opacity: f64,
        blend_mode: BlendMode,
    ) -> Self {
        Self {
            x,
            y,
            scale_x,
            scale_y,
            rotation_deg,
            anchor_x,
            anchor_y,
            opacity: opacity.clamp(0.0, 1.0),
            blend_mode,
            matrix: Matrix2D::from_transform(
                (x, y),
                (scale_x, scale_y),
                (anchor_x, anchor_y),
                rotation_deg,
            ),
        }
    }
}

/// キャンバス中心 `(0, 0)`（+X 右, +Y 下, +回転は時計回り）を基準とする視覚プロパティ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VisualProperties {
    /// キャンバス中心からの X 方向オフセット（ピクセル）。
    #[serde(default = "default_zero")]
    pub x: AnimatedParameter,
    /// キャンバス中心からの Y 方向オフセット（ピクセル）。
    #[serde(default = "default_zero")]
    pub y: AnimatedParameter,
    /// X 方向拡大率（`1.0` が等倍）。
    #[serde(default = "default_one")]
    pub scale_x: AnimatedParameter,
    /// Y 方向拡大率（`1.0` が等倍）。
    #[serde(default = "default_one")]
    pub scale_y: AnimatedParameter,
    /// 時計回りの回転角（度）。
    #[serde(default = "default_zero")]
    pub rotation_deg: AnimatedParameter,
    /// コンテンツ中心を `(0, 0)` としたアンカーポイントの X オフセット（ピクセル）。
    #[serde(default = "default_zero")]
    pub anchor_x: AnimatedParameter,
    /// コンテンツ中心を `(0, 0)` としたアンカーポイントの Y オフセット（ピクセル）。
    #[serde(default = "default_zero")]
    pub anchor_y: AnimatedParameter,
    /// 不透明度（`0.0..=1.0`）。
    #[serde(default = "default_one")]
    pub opacity: AnimatedParameter,
    /// 合成時のブレンドモード。
    #[serde(default)]
    pub blend_mode: BlendMode,
}

impl VisualProperties {
    /// 指定された評価コンテキストにおいて各視覚パラメータと 2D 変換行列を計算する。
    ///
    /// # Errors
    ///
    /// いずれかの [`AnimatedParameter`] 評価が失敗した場合は [`AnimationError`] を返す。
    pub fn evaluate(&self, ctx: EvaluationContext) -> Result<EvaluatedVisual, AnimationError> {
        let x = self.x.evaluate(ctx)?;
        let y = self.y.evaluate(ctx)?;
        let scale_x = self.scale_x.evaluate(ctx)?;
        let scale_y = self.scale_y.evaluate(ctx)?;
        let rotation_deg = self.rotation_deg.evaluate(ctx)?;
        let anchor_x = self.anchor_x.evaluate(ctx)?;
        let anchor_y = self.anchor_y.evaluate(ctx)?;
        let opacity = self.opacity.evaluate(ctx)?;

        Ok(EvaluatedVisual::from_components(
            x,
            y,
            scale_x,
            scale_y,
            rotation_deg,
            anchor_x,
            anchor_y,
            opacity,
            self.blend_mode,
        ))
    }
}

impl Default for VisualProperties {
    fn default() -> Self {
        Self {
            x: default_zero(),
            y: default_zero(),
            scale_x: default_one(),
            scale_y: default_one(),
            rotation_deg: default_zero(),
            anchor_x: default_zero(),
            anchor_y: default_zero(),
            opacity: default_one(),
            blend_mode: BlendMode::Normal,
        }
    }
}

/// 字幕テキストの水平揃え位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextAlignment {
    /// 左揃え。
    Left,
    /// 中央揃え。
    #[default]
    Center,
    /// 右揃え。
    Right,
}

/// 字幕クリップ固有の表示プロパティ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubtitleProperties {
    /// 表示する字幕テキスト本文。
    pub text: String,
    /// 使用するフォントファミリ名。
    #[serde(default = "default_font_family")]
    pub font_family: String,
    /// フォントサイズ（論理ピクセル）。
    #[serde(default = "default_font_size")]
    pub font_size: AnimatedParameter,
    /// 文字色（RGBA）。
    #[serde(default = "default_text_color")]
    pub text_color: RgbaColor,
    /// 縁取り色（RGBA）。
    #[serde(default = "default_outline_color")]
    pub outline_color: RgbaColor,
    /// 縁取りの太さ（ピクセル）。
    #[serde(default = "default_outline_width")]
    pub outline_width: f32,
    /// 水平方向の揃え位置。
    #[serde(default)]
    pub alignment: TextAlignment,
    /// 字幕全体の位置・拡大率・回転・不透明度などの共通視覚プロパティ。
    #[serde(default)]
    pub visual: VisualProperties,
}

impl SubtitleProperties {
    /// 既定スタイルで新しい字幕プロパティを生成する。
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            text_color: default_text_color(),
            outline_color: default_outline_color(),
            outline_width: default_outline_width(),
            alignment: TextAlignment::Center,
            visual: VisualProperties::default(),
        }
    }
}

const fn default_zero() -> AnimatedParameter {
    AnimatedParameter::constant(0.0)
}

const fn default_one() -> AnimatedParameter {
    AnimatedParameter::constant(1.0)
}

fn default_font_family() -> String {
    "sans-serif".to_owned()
}

const fn default_font_size() -> AnimatedParameter {
    AnimatedParameter::constant(48.0)
}

const fn default_text_color() -> RgbaColor {
    RgbaColor::WHITE
}

const fn default_outline_color() -> RgbaColor {
    RgbaColor::BLACK
}

const fn default_outline_width() -> f32 {
    2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_defaults_and_serde_roundtrip() -> Result<(), serde_json::Error> {
        let subtitle = SubtitleProperties::new("テスト字幕");
        let json = serde_json::to_string(&subtitle)?;
        let restored: SubtitleProperties = serde_json::from_str(&json)?;
        assert_eq!(restored, subtitle);
        Ok(())
    }

    #[test]
    fn anchor_scale_clockwise_rotate_and_translate_transform_order() {
        // アンカー (10, 0) を固定し 2倍・時計回り90度回転・(+100, +50) 移動:
        // ローカル点 (20, 0) -> アンカー差し引き (10, 0) -> 2倍 (20, 0)
        // -> +90deg 時計回り (+X右,+Y下系) で (0, +20) -> 平行移動後 (100, 70)
        let eval = EvaluatedVisual::from_components(
            100.0,
            50.0,
            2.0,
            2.0,
            90.0,
            10.0,
            0.0,
            1.5,
            BlendMode::Normal,
        );
        let (px, py) = eval.matrix.transform_point(20.0, 0.0);
        assert!((px - 100.0).abs() < 1e-9);
        assert!((py - 70.0).abs() < 1e-9);
        // アンカー自身 (10, 0) は常に (x, y) = (100, 50) に配置される
        let (ax, ay) = eval.matrix.transform_point(10.0, 0.0);
        assert!((ax - 100.0).abs() < 1e-9);
        assert!((ay - 50.0).abs() < 1e-9);
        // 不透明度は 0..=1 にクランプされる
        assert!((eval.opacity - 1.0).abs() < 1e-9);
    }
}
