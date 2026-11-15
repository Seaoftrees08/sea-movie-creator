//! 外部音声プラグインの記述型・内蔵テストプラグイン定義・`IPC` 型再エクスポート。

pub use sea_export_protocol::audio_ipc::{
    AudioPluginDescriptor, AudioPluginParameterDescriptor, AudioWorkerRequest, AudioWorkerResponse,
    WireAudioPluginFormat,
};

use crate::audio::properties::AudioPluginFormat;

/// 決定論的テスト・検証用の内蔵ゲインエフェクト識別子。
pub const SEA_TEST_GAIN_PLUGIN_ID: &str = "sea.test.gain";

/// テスト用プラグインのゲインパラメータ名。
pub const SEA_TEST_GAIN_PARAM_NAME: &str = "gain";

/// アプリ内の [`AudioPluginFormat`] を `IPC` 転送用 [`WireAudioPluginFormat`] へ変換する。
#[must_use]
pub const fn to_wire_format(format: AudioPluginFormat) -> WireAudioPluginFormat {
    match format {
        AudioPluginFormat::Vst3 => WireAudioPluginFormat::Vst3,
        AudioPluginFormat::Clap => WireAudioPluginFormat::Clap,
        AudioPluginFormat::BuiltinTest => WireAudioPluginFormat::BuiltinTest,
    }
}

/// `IPC` 転送用 [`WireAudioPluginFormat`] をアプリ内の [`AudioPluginFormat`] へ変換する。
#[must_use]
pub const fn from_wire_format(format: WireAudioPluginFormat) -> AudioPluginFormat {
    match format {
        WireAudioPluginFormat::Vst3 => AudioPluginFormat::Vst3,
        WireAudioPluginFormat::Clap => AudioPluginFormat::Clap,
        WireAudioPluginFormat::BuiltinTest => AudioPluginFormat::BuiltinTest,
    }
}

/// 内蔵テストゲインプラグインの標準メタ情報を返す。
#[must_use]
pub fn builtin_test_gain_descriptor() -> AudioPluginDescriptor {
    AudioPluginDescriptor {
        plugin_id: SEA_TEST_GAIN_PLUGIN_ID.to_owned(),
        name: "Sea Test Gain (DSP)".to_owned(),
        vendor: "sea-movie-creator".to_owned(),
        format: WireAudioPluginFormat::BuiltinTest,
        bundle_path: None,
        parameters: vec![AudioPluginParameterDescriptor {
            name: SEA_TEST_GAIN_PARAM_NAME.to_owned(),
            display_name: "Gain".to_owned(),
            min_value: 0.0,
            max_value: 4.0,
            default_value: 1.0,
            is_boolean: false,
        }],
    }
}
