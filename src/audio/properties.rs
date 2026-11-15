//! クリップ別オーディオパラメータ、外部エフェクトスロット定義、およびソフトリミッタ。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::core::animation::{AnimatedParameter, AnimationError, EvaluationContext};
use crate::timeline::document::AudioProperties;

/// クリップやトラックに装着される外部オーディオプラグインの規格種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AudioPluginFormat {
    /// Steinberg VST3 (`MIT / Steinberg 3.8+`)。
    Vst3,
    /// CLAP (`MIT`, `clack-host`)。
    #[default]
    Clap,
    /// 自動テストおよびローカル検証用の内蔵テストエフェクトモード。
    BuiltinTest,
}

/// オーディオプラグインのパラメータスロット値。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "value_kind", rename_all = "snake_case")]
pub enum AudioPluginParamValue {
    /// 時刻に応じて変化するアニメーションパラメータ。
    Animated {
        /// アニメーション定義。
        param: AnimatedParameter,
    },
    /// 数値定数（プラグイン正規化値または単位値）。
    Number {
        /// 定数値。
        value: f64,
    },
    /// 真偽スイッチ（例: `bypass`, `invert`）。
    Bool {
        /// 真偽値。
        value: bool,
    },
}

/// クリップへ直列装着される 1 基のオーディオプラグインエフェクトスロット。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioPluginSlot {
    /// スロット一意識別子（タイムラインコマンド・Undo での対象指定用）。
    pub slot_id: String,
    /// 検出されたプラグイン ID（スキャナの識別子またはテスト ID）。
    pub plugin_id: String,
    /// UI 表示名。
    pub name: String,
    /// プラグイン形式 (`vst3` / `clap` / `builtin_test`)。
    #[serde(default)]
    pub format: AudioPluginFormat,
    /// エフェクトが有効かどうか（`false` またはクラッシュ時は自動バイパス）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 外部 `.vst3` / `.clap` バンドルへの明示パス（`BuiltinTest` では省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    /// プラグイン固有パラメータ名と値のマッピング。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, AudioPluginParamValue>,
}

const fn default_true() -> bool {
    true
}

impl AudioPluginSlot {
    /// 基本設定で新しい有効なオーディオプラグインスロットを作成する。
    #[must_use]
    pub fn new(
        slot_id: impl Into<String>,
        plugin_id: impl Into<String>,
        name: impl Into<String>,
        format: AudioPluginFormat,
    ) -> Self {
        Self {
            slot_id: slot_id.into(),
            plugin_id: plugin_id.into(),
            name: name.into(),
            format,
            enabled: true,
            bundle_path: None,
            parameters: BTreeMap::new(),
        }
    }
}

/// ある時刻において評価済みのクリップ音量・パン係数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvaluatedAudioParams {
    /// 評価済みデシベルゲイン (`dB`)。
    pub gain_db: f64,
    /// 評価済みステレオパン (`-1.0..=1.0`)。
    pub pan: f64,
    /// 左チャンネル最終リニアゲイン。
    pub gain_l: f64,
    /// 右チャンネル最終リニアゲイン。
    pub gain_r: f64,
}

/// オーディオプロパティを指定コンテキストで評価し、ステレオ L/R リニアゲインを返す。
///
/// # Errors
///
/// `AnimatedParameter` の評価に失敗した場合に [`AnimationError`] を返す。
pub fn evaluate_audio_params(
    props: &AudioProperties,
    ctx: EvaluationContext,
) -> Result<EvaluatedAudioParams, AnimationError> {
    if props.muted {
        return Ok(EvaluatedAudioParams {
            gain_db: f64::NEG_INFINITY,
            pan: 0.0,
            gain_l: 0.0,
            gain_r: 0.0,
        });
    }
    let gain_db = props.gain_db.evaluate(ctx)?;
    let pan = props.pan.evaluate(ctx)?.clamp(-1.0, 1.0);
    let linear = db_to_linear_f64(gain_db);
    let (pan_l, pan_r) = pan_multipliers(pan);
    Ok(EvaluatedAudioParams {
        gain_db,
        pan,
        gain_l: linear * pan_l,
        gain_r: linear * pan_r,
    })
}

/// `dB` 値をリニア振幅倍率へ変換する（`-96 dB` 以下は無音 `0.0`）。
#[must_use]
pub fn db_to_linear_f64(db: f64) -> f64 {
    if !db.is_finite() || db <= -96.0 {
        0.0
    } else {
        10.0_f64.powf(db / 20.0)
    }
}

/// `-1.0..=1.0` のパン値から左右チャンネルのバランス倍率を計算する。
#[must_use]
pub fn pan_multipliers(pan: f64) -> (f64, f64) {
    let clamped = pan.clamp(-1.0, 1.0);
    let l = (1.0 - clamped).min(1.0);
    let r = (1.0 + clamped).min(1.0);
    (l, r)
}

/// ピーククリップを滑らかに抑えるソフトリミッタ（常に有限かつ `|x| <= 1.0`）。
#[must_use]
pub fn soft_limit_sample(sample: f32) -> f32 {
    if !sample.is_finite() {
        return 0.0;
    }
    let threshold = 0.9_f32;
    let abs_x = sample.abs();
    if abs_x <= threshold {
        return sample;
    }
    // 0.9 を超えた分だけ滑らかに tanh 圧縮して [-1.0, 1.0] へ収める
    let excess = (abs_x - threshold) / (1.0 - threshold);
    let compressed = threshold + (1.0 - threshold) * excess.tanh();
    sample.signum() * compressed.min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::time::{FrameCount, FrameRate};

    #[test]
    fn audio_plugin_slot_roundtrips_serde() -> Result<(), Box<dyn std::error::Error>> {
        let mut slot = AudioPluginSlot::new(
            "slot_eq",
            "org.example.eq",
            "Parametric EQ",
            AudioPluginFormat::Clap,
        );
        slot.parameters.insert(
            "mix".to_owned(),
            AudioPluginParamValue::Animated {
                param: AnimatedParameter::constant(0.75),
            },
        );
        slot.parameters.insert(
            "bypass".to_owned(),
            AudioPluginParamValue::Bool { value: false },
        );

        let json = serde_json::to_string(&slot)?;
        let decoded: AudioPluginSlot = serde_json::from_str(&json)?;
        assert_eq!(decoded, slot);
        Ok(())
    }

    #[test]
    fn soft_limiter_preserves_normal_range_and_clamps_peaks()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!((soft_limit_sample(0.5) - 0.5).abs() < 1e-6);
        assert!(soft_limit_sample(5.0) <= 1.0);
        assert!(soft_limit_sample(5.0) >= 0.9);
        assert!(soft_limit_sample(-5.0) >= -1.0);
        assert_eq!(soft_limit_sample(f32::NAN), 0.0);

        let props = AudioProperties::default();
        let ctx = EvaluationContext::new(0, 0, FrameCount::new(30)?, FrameRate::FPS_30);
        let eval = evaluate_audio_params(&props, ctx)?;
        assert!((eval.gain_l - 1.0).abs() < 1e-9);
        assert!((eval.gain_r - 1.0).abs() < 1e-9);
        Ok(())
    }
}
