//! 外部音声プラグインホスト (`sea-audio-host`) と本体間の共有 IPC スキーマ定義。

use serde::{Deserialize, Serialize};

use crate::ShmLayout;

/// 外部プロセスホストで扱う音声プラグイン形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAudioPluginFormat {
    /// Steinberg `VST3` プラグイン（外部 `.vst3` バンドル）。
    Vst3,
    /// `CLAP` プラグイン（外部 `.clap` プラグイン）。
    Clap,
    /// テスト・オフライン検証用の内蔵決定論的 `DSP` プラグイン。
    BuiltinTest,
}

/// 音声プラグインが公開する単一パラメータ定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPluginParameterDescriptor {
    /// JSON／コマンド指定に用いるパラメータ識別子。
    pub name: String,
    /// UI 表示用ラベル。
    pub display_name: String,
    /// 最小値。
    pub min_value: f64,
    /// 最大値。
    pub max_value: f64,
    /// 既定値。
    pub default_value: f64,
    /// 真偽値トグルとして扱うか。
    #[serde(default)]
    pub is_boolean: bool,
}

/// 検出またはロードされた音声プラグインのメタ情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPluginDescriptor {
    /// 一意なプラグイン識別子（例: `com.example.eq`, `sea.test.gain`）。
    pub plugin_id: String,
    /// プラグイン表示名。
    pub name: String,
    /// ベンダー／開発元名。
    pub vendor: String,
    /// プラグイン形式。
    pub format: WireAudioPluginFormat,
    /// 外部バンドルまたは動的ライブラリへのパス。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    /// 利用可能なパラメータ一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<AudioPluginParameterDescriptor>,
}

/// ホストプロセスから `sea-audio-host` ワーカーへ送る `NDJSON` 指令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AudioWorkerRequest {
    /// 指定パスのプラグインをロードし、対象共有メモリ領域へ接続する。
    LoadPlugin {
        /// プラグイン形式。
        format: WireAudioPluginFormat,
        /// プラグイン識別子（テスト用 `BuiltinTest` では `"sea.test.gain"` 等）。
        plugin_id: String,
        /// 外部 `.vst3` / `.clap` へのパス（`BuiltinTest` 時は `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle_path: Option<String>,
        /// 動作サンプリングレート（Hz）。
        sample_rate: u32,
        /// 1 ブロックあたりの最大フレーム数。
        max_block_frames: u32,
        /// ホスト側で確保済みの共有メモリファイルパス。
        shm_path: String,
        /// 共有メモリのレイアウト記述。
        shm_layout: ShmLayout,
    },
    /// 外部プラグインバンドルから公開メタ情報のみを読み出す。
    DescribePlugin {
        /// プラグイン形式。
        format: WireAudioPluginFormat,
        /// 外部 `.vst3` / `.clap` へのパス。
        bundle_path: String,
    },
    /// プラグインの指定パラメータ値を更新する。
    SetParameter {
        /// パラメータ名または識別子。
        name: String,
        /// 設定値。
        value: f64,
    },
    /// 共有メモリの `input_slot` にある `f32` `PCM` を処理し `output_slot` へ書き出す。
    ProcessBlock {
        /// 入力スロット番号（通常 `0`）。
        input_slot: u32,
        /// 出力スロット番号（通常 `1`）。
        output_slot: u32,
        /// 今回のブロックの有効フレーム数。
        frame_count: u32,
        /// チャンネル数（通常 `2`）。
        channels: u16,
    },
    /// 異常終了・クラッシュ分離テスト用にワーカーを即時エラー終了させる。
    SimulateCrash,
    /// ワーカーを安全に停止させる。
    Shutdown,
}

/// `sea-audio-host` ワーカーからホスト本体へ返す `NDJSON` 応答。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioWorkerResponse {
    /// プラグインのロードおよび共有メモリ接続が完了した。
    Loaded {
        /// ロードされたプラグインのメタ情報。
        descriptor: AudioPluginDescriptor,
    },
    /// バンドルスキャン結果の一覧。
    Described {
        /// バンドルに含まれるプラグイン記述群。
        plugins: Vec<AudioPluginDescriptor>,
    },
    /// パラメータ変更を受理した。
    ParameterAck {
        /// パラメータ名。
        name: String,
        /// 実際に適用された値。
        applied_value: f64,
    },
    /// 1 ブロック分の音声処理および `SHM` 書き出しが完了した。
    BlockAck {
        /// 処理済みフレーム数。
        frame_count: u32,
    },
    /// シャットダウン要求を受理した。
    ShutdownAck,
    /// プラグイン操作または読み込み中にエラーが発生した。
    Error {
        /// エラー詳細メッセージ。
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ShmLayoutSpec, read_json_line, write_json_line};
    use std::io::Cursor;

    #[test]
    fn audio_ipc_ndjson_roundtrip_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let layout = ShmLayout::from_spec(ShmLayoutSpec {
            width: 1,
            height: 1,
            pixel_bytes: 4,
            video_slot_count: 1,
            audio_channels: 2,
            audio_slot_capacity_frames: 512,
            audio_slot_count: 2,
        })?;
        let req = AudioWorkerRequest::LoadPlugin {
            format: WireAudioPluginFormat::BuiltinTest,
            plugin_id: "sea.test.gain".to_owned(),
            bundle_path: None,
            sample_rate: 48_000,
            max_block_frames: 512,
            shm_path: "dummy.shm".to_owned(),
            shm_layout: layout,
        };
        let mut buf = Vec::new();
        write_json_line(&mut buf, &req)?;
        let mut cursor = Cursor::new(buf);
        let decoded: AudioWorkerRequest = read_json_line(&mut cursor)?;
        assert_eq!(decoded, req);
        Ok(())
    }
}
