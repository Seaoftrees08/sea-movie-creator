//! 結合テストおよび決定論的エンドツーエンド検証用の組み込み `DSP` エフェクト。

use sea_export_protocol::audio_ipc::{
    AudioPluginDescriptor, AudioPluginParameterDescriptor, WireAudioPluginFormat,
};

/// テスト用ゲインエフェクト (`sea.test.gain`)。
#[derive(Debug, Clone)]
pub struct BuiltinGainEffect {
    descriptor: AudioPluginDescriptor,
    gain: f64,
}

impl BuiltinGainEffect {
    /// 指定された `plugin_id` に応じた組み込みテストエフェクトを生成する。
    ///
    /// # Errors
    ///
    /// 未知の組み込みテスト `plugin_id` が指定された場合にエラー文を返す。
    pub fn load(plugin_id: &str) -> Result<Self, String> {
        if plugin_id != "sea.test.gain" {
            return Err(format!("unknown builtin test plugin_id: {plugin_id}"));
        }
        Ok(Self {
            descriptor: AudioPluginDescriptor {
                plugin_id: plugin_id.to_owned(),
                name: "Sea Test Gain (DSP)".to_owned(),
                vendor: "sea-movie-creator".to_owned(),
                format: WireAudioPluginFormat::BuiltinTest,
                bundle_path: None,
                parameters: vec![AudioPluginParameterDescriptor {
                    name: "gain".to_owned(),
                    display_name: "Gain".to_owned(),
                    min_value: 0.0,
                    max_value: 4.0,
                    default_value: 1.0,
                    is_boolean: false,
                }],
            },
            gain: 1.0,
        })
    }

    /// プラグインのメタ情報を返す。
    #[must_use]
    pub const fn descriptor(&self) -> &AudioPluginDescriptor {
        &self.descriptor
    }

    /// ゲイン等のパラメータを更新する。
    ///
    /// # Errors
    ///
    /// 未知パラメータ名、または非有限値の場合にエラーを返す。
    pub fn set_parameter(&mut self, name: &str, value: f64) -> Result<f64, String> {
        if name != "gain" {
            return Err(format!("unknown builtin parameter: {name}"));
        }
        if !value.is_finite() {
            return Err("gain must be finite".to_owned());
        }
        let clamped = value.clamp(0.0, 4.0);
        self.gain = clamped;
        Ok(clamped)
    }

    /// インターリーブステレオ `f32` サンプル列へ線形ゲインを適用する。
    #[allow(clippy::cast_possible_truncation)]
    pub fn process_stereo(&self, interleaved_in: &[f32], interleaved_out: &mut [f32]) {
        let g = self.gain as f32;
        for (dst, src) in interleaved_out
            .iter_mut()
            .zip(interleaved_in.iter().copied())
        {
            *dst = src * g;
        }
    }
}
