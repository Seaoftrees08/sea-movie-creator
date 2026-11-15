//! 字幕・背景・プレースホルダ描画等で共有する RGBA カラー定義。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 非プリマルチプライド 8bit RGBA カラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RgbaColor {
    /// 赤成分 (`0..=255`)。
    pub red: u8,
    /// 緑成分 (`0..=255`)。
    pub green: u8,
    /// 青成分 (`0..=255`)。
    pub blue: u8,
    /// 不透明度アルファ成分 (`0..=255`)。
    pub alpha: u8,
}

impl RgbaColor {
    /// 不透明な白 (`255, 255, 255, 255`)。
    pub const WHITE: Self = Self {
        red: 255,
        green: 255,
        blue: 255,
        alpha: 255,
    };

    /// 不透明な黒 (`0, 0, 0, 255`)。
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 255,
    };

    /// 完全透明 (`0, 0, 0, 0`)。
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };

    /// 各チャンネルを指定して色を生成する。
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

impl Default for RgbaColor {
    fn default() -> Self {
        Self::BLACK
    }
}
