//! `sea-movie-creator` ホスト本体と外部プロセス書き出しプラグイン間の共通 IPC プロトコル。

pub mod audio_ipc;
pub mod backends;
pub mod external_ffmpeg;

use std::io::{BufRead, Write};
use std::ops::Range;
use std::process::Command;

/// Windows 上で GUI プロセスから子プロセスを起動した際にコンソールウィンドウが一瞬開くのを防ぐ (`CREATE_NO_WINDOW`)。
pub fn apply_background_process_flags(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

use serde::{Deserialize, Serialize};
use thiserror::Error;

use backends::shm_mmap::ShmError;

/// 本プロトコル仕様の現在のバージョン番号。
pub const EXPORT_PROTOCOL_VERSION: u32 = 1;

const SLOT_ALIGNMENT_BYTES: u64 = 64;

/// 共有メモリ内で扱う映像ピクセル配列形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// Windows / GPUI 共通の 8bit BGRA リトルエンディアン配列。
    Bgra8,
    /// 標準 8bit RGBA 配列。
    Rgba8,
}

impl PixelFormat {
    /// 1 ピクセルあたりのバイト数を返す。
    #[must_use]
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Bgra8 | Self::Rgba8 => 4,
        }
    }
}

/// 共有メモリ内で扱う音声サンプル形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSampleFormat {
    /// チャンネルインターリーブ済み 32bit 浮動小数点 PCM（`-1.0..=1.0`）。
    F32Interleaved,
    /// チャンネルインターリーブ済み符号付き 16bit 整数 PCM。
    I16Interleaved,
}

/// 共有メモリスロットの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    /// 映像フレーム用スロット。
    Video,
    /// 音声チャンク用スロット。
    Audio,
}

/// 共有メモリのオフセット・スロットサイズ計算済みレイアウト。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmLayout {
    /// 映像スロット 1 つあたりの最大容量（バイト単位、64B アライン済み）。
    pub video_slot_stride_bytes: u64,
    /// 映像スロット個数。
    pub video_slot_count: u32,
    /// 音声領域開始オフセット（バイト単位、64B アライン済み）。
    pub audio_region_offset_bytes: u64,
    /// 音声スロット 1 つあたりの最大容量（バイト単位、64B アライン済み）。
    pub audio_slot_stride_bytes: u64,
    /// 音声スロット個数。
    pub audio_slot_count: u32,
    /// 共有メモリ全体のサイズ（バイト単位）。
    pub total_bytes: u64,
}

/// 共有メモリレイアウト計算用の入力パラメータ組。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmLayoutSpec {
    /// 出力映像幅（ピクセル）。
    pub width: u32,
    /// 出力映像高さ（ピクセル）。
    pub height: u32,
    /// 1 ピクセルあたりのバイト数（BGRA8/RGBA8 なら 4）。
    pub pixel_bytes: u32,
    /// 確保する映像スロット数。
    pub video_slot_count: u32,
    /// 音声チャンネル数（ステレオなら 2）。
    pub audio_channels: u16,
    /// 1 音声スロットあたりの最大サンプルフレーム数。
    pub audio_slot_capacity_frames: u32,
    /// 確保する音声スロット数。
    pub audio_slot_count: u32,
}

impl ShmLayout {
    /// 映像解像度・チャネル数・スロット数から 64 バイト境界整列済みの [`ShmLayout`] を構築する。
    ///
    /// # Errors
    ///
    /// 各種カウントが 0 または `u64` オーバーフローする場合 [`ShmError::InvalidLayout`] を返す。
    pub fn from_spec(spec: ShmLayoutSpec) -> Result<Self, ShmError> {
        if spec.width == 0
            || spec.height == 0
            || spec.pixel_bytes == 0
            || spec.video_slot_count == 0
            || spec.audio_channels == 0
            || spec.audio_slot_capacity_frames == 0
            || spec.audio_slot_count == 0
        {
            return Err(ShmError::InvalidLayout(
                "dimension or slot count must be non-zero".to_owned(),
            ));
        }

        let raw_video = u64::from(spec.width)
            .checked_mul(u64::from(spec.height))
            .and_then(|v| v.checked_mul(u64::from(spec.pixel_bytes)))
            .ok_or_else(|| ShmError::InvalidLayout("video frame bytes overflow".to_owned()))?;
        let video_slot_stride_bytes = align_up(raw_video, SLOT_ALIGNMENT_BYTES)?;

        let video_region_bytes = video_slot_stride_bytes
            .checked_mul(u64::from(spec.video_slot_count))
            .ok_or_else(|| ShmError::InvalidLayout("video region size overflow".to_owned()))?;
        let audio_region_offset_bytes = align_up(video_region_bytes, SLOT_ALIGNMENT_BYTES)?;

        let raw_audio = u64::from(spec.audio_slot_capacity_frames)
            .checked_mul(u64::from(spec.audio_channels))
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| ShmError::InvalidLayout("audio slot bytes overflow".to_owned()))?;
        let audio_slot_stride_bytes = align_up(raw_audio, SLOT_ALIGNMENT_BYTES)?;

        let audio_region_bytes = audio_slot_stride_bytes
            .checked_mul(u64::from(spec.audio_slot_count))
            .ok_or_else(|| ShmError::InvalidLayout("audio region size overflow".to_owned()))?;

        let total_bytes = audio_region_offset_bytes
            .checked_add(audio_region_bytes)
            .ok_or_else(|| ShmError::InvalidLayout("total shm size overflow".to_owned()))?;

        Ok(Self {
            video_slot_stride_bytes,
            video_slot_count: spec.video_slot_count,
            audio_region_offset_bytes,
            audio_slot_stride_bytes,
            audio_slot_count: spec.audio_slot_count,
            total_bytes,
        })
    }

    /// 指定映像スロットに対する `mmap` バイト範囲を計算する。
    ///
    /// # Errors
    ///
    /// スロット番号または要求バイト長が範囲外の場合 [`ShmError::OutOfBounds`] を返す。
    pub fn video_slot_byte_range(
        &self,
        slot_index: u32,
        byte_len: usize,
    ) -> Result<Range<usize>, ShmError> {
        if slot_index >= self.video_slot_count {
            return Err(ShmError::OutOfBounds(format!(
                "video slot {slot_index} >= {}",
                self.video_slot_count
            )));
        }
        let len_u64 = u64::try_from(byte_len)
            .map_err(|_| ShmError::OutOfBounds("byte_len overflows u64".to_owned()))?;
        if len_u64 > self.video_slot_stride_bytes {
            return Err(ShmError::OutOfBounds(format!(
                "video payload {len_u64} > stride {}",
                self.video_slot_stride_bytes
            )));
        }
        let start_u64 = self
            .video_slot_stride_bytes
            .checked_mul(u64::from(slot_index))
            .ok_or_else(|| ShmError::OutOfBounds("video slot offset overflow".to_owned()))?;
        let end_u64 = start_u64
            .checked_add(len_u64)
            .ok_or_else(|| ShmError::OutOfBounds("video slot end overflow".to_owned()))?;
        to_usize_range(start_u64, end_u64)
    }

    /// 指定音声スロットに対する `mmap` バイト範囲を計算する。
    ///
    /// # Errors
    ///
    /// スロット番号または要求バイト長が範囲外の場合 [`ShmError::OutOfBounds`] を返す。
    pub fn audio_slot_byte_range(
        &self,
        slot_index: u32,
        byte_len: usize,
    ) -> Result<Range<usize>, ShmError> {
        if slot_index >= self.audio_slot_count {
            return Err(ShmError::OutOfBounds(format!(
                "audio slot {slot_index} >= {}",
                self.audio_slot_count
            )));
        }
        let len_u64 = u64::try_from(byte_len)
            .map_err(|_| ShmError::OutOfBounds("byte_len overflows u64".to_owned()))?;
        if len_u64 > self.audio_slot_stride_bytes {
            return Err(ShmError::OutOfBounds(format!(
                "audio payload {len_u64} > stride {}",
                self.audio_slot_stride_bytes
            )));
        }
        let start_u64 = self
            .audio_slot_stride_bytes
            .checked_mul(u64::from(slot_index))
            .and_then(|offset| self.audio_region_offset_bytes.checked_add(offset))
            .ok_or_else(|| ShmError::OutOfBounds("audio slot offset overflow".to_owned()))?;
        let end_u64 = start_u64
            .checked_add(len_u64)
            .ok_or_else(|| ShmError::OutOfBounds("audio slot end overflow".to_owned()))?;
        to_usize_range(start_u64, end_u64)
    }
}

fn align_up(value: u64, align: u64) -> Result<u64, ShmError> {
    let rem = value % align;
    if rem == 0 {
        Ok(value)
    } else {
        value
            .checked_add(align - rem)
            .ok_or_else(|| ShmError::InvalidLayout("alignment overflow".to_owned()))
    }
}

fn to_usize_range(start_u64: u64, end_u64: u64) -> Result<Range<usize>, ShmError> {
    let start = usize::try_from(start_u64)
        .map_err(|_| ShmError::OutOfBounds("range start overflows usize".to_owned()))?;
    let end = usize::try_from(end_u64)
        .map_err(|_| ShmError::OutOfBounds("range end overflows usize".to_owned()))?;
    Ok(start..end)
}

/// 書き出しジョブ初期化時にホストからプラグインへ渡すセッション設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportInitPayload {
    /// プロトコルバージョン。
    pub protocol_version: u32,
    /// 出力映像幅（ピクセル）。
    pub width: u32,
    /// 出力映像高さ（ピクセル）。
    pub height: u32,
    /// フレームレート分子。
    pub fps_num: u32,
    /// フレームレート分母。
    pub fps_den: u32,
    /// 書き出し対象の総映像フレーム数。
    pub total_video_frames: u64,
    /// 音声サンプルレート（Hz）。
    pub sample_rate: u32,
    /// 音声チャンネル数。
    pub audio_channels: u16,
    /// 映像ピクセル配列形式。
    pub pixel_format: PixelFormat,
    /// 音声サンプル配列形式。
    pub audio_sample_format: AudioSampleFormat,
    /// 共有メモリバッキングファイルのパス。
    pub shm_path: String,
    /// 共有メモリレイアウト。
    pub shm_layout: ShmLayout,
    /// 出力先ファイルパス。
    pub output_path: String,
    /// 選択プリセット ID。
    pub preset_id: String,
    /// プリセットごとの任意 JSON オプション。
    #[serde(default)]
    pub options: serde_json::Value,
}

/// ホストからプラグイン標準入力 (`stdin`) へ送られる JSON Lines コマンド。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum HostToPluginMessage {
    /// 書き出しセッションと共有メモリ接続の初期化指令。
    Init(Box<ExportInitPayload>),
    /// 指定映像スロットのフレームをエンコードキューへ投入する指令。
    ProcessVideoSlot {
        /// 読み取り対象の映像スロット番号。
        slot_index: u32,
        /// タイムライン先頭を 0 とする出力連番フレーム番号。
        frame_index: u64,
        /// スロット内に書かれた有効データ長（バイト単位）。
        data_len_bytes: u64,
    },
    /// 指定音声スロットの PCM チャンクをエンコードキューへ投入する指令。
    ProcessAudioSlot {
        /// 読み取り対象の音声スロット番号。
        slot_index: u32,
        /// 先頭サンプル番号。
        start_sample: u64,
        /// サンプルフレーム数（ステレオなら 2ch をまとめて 1 フレームと数える）。
        sample_frame_count: u32,
        /// スロット内に書かれた有効データ長（バイト単位）。
        data_len_bytes: u64,
    },
    /// 全フレーム・音声の投入完了通知（フラッシュおよびコンテナファイナライズ指示）。
    Finish,
    /// キャンセルまたはエラーによる即時中断指示。
    Abort,
}

/// プラグイン標準出力 (`stdout`) からホストへ返される JSON Lines イベント。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PluginToHostMessage {
    /// プロセス起動および `Init` 受信後の準備完了通知。
    Ready {
        /// プラグインが対応しているプロトコルバージョン。
        protocol_version: u32,
    },
    /// 指定スロットの読み取りが完了しホスト側で再利用可能になった通知。
    SlotAck {
        /// 再利用可能になったスロット種別。
        kind: SlotKind,
        /// スロット番号。
        slot_index: u32,
    },
    /// エンコード進捗通知。
    Progress {
        /// 現時点でエンコード完了した映像フレーム数。
        frames_encoded: u64,
        /// 総映像フレーム数。
        total_frames: u64,
        /// 現時点で処理済みの音声サンプルフレーム数。
        audio_samples_encoded: u64,
        /// UI 表示用ステータスメッセージ。
        #[serde(default)]
        message: Option<String>,
    },
    /// 全ストリームのコンテナ出力およびクリーン終了完了通知。
    Completed {
        /// 実際に書き出されたファイルパス。
        output_path: String,
        /// 出力ファイルサイズ（バイト単位）。
        file_size_bytes: u64,
        /// 出力映像フレーム数。
        frames_encoded: u64,
    },
    /// エンコード中の致命的または設定起因のエラー通知。
    Error {
        /// 機械判別用エラーコード。
        code: String,
        /// 人間および AI 向けのエラー詳細説明。
        message: String,
    },
}

/// JSON Lines IPC 通信時のシリアライズ／入出力エラー。
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// I/O エラー。
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON パース／シリアライズ失敗。
    #[error("IPC JSON codec failed: {0}")]
    Json(#[from] serde_json::Error),
    /// ピアプロセスが応答前にストリームを閉じた（クラッシュ等）。
    #[error("IPC peer closed stream unexpectedly (EOF)")]
    UnexpectedEof,
}

/// 1 メッセージを 1 行の JSON として書き込み即座にフラッシュする。
///
/// # Errors
///
/// JSON 化またはパイプ書き込みに失敗した場合 [`ProtocolError`] を返す。
pub fn write_json_line<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), ProtocolError> {
    let serialized = serde_json::to_string(message)?;
    writer.write_all(serialized.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// `BufRead` から次の非空行を読み込み JSON としてデシリアライズする。
///
/// # Errors
///
/// EOF 到達時 [`ProtocolError::UnexpectedEof`]、不正な JSON 時 [`ProtocolError::Json`] を返す。
pub fn read_json_line<R: BufRead, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, ProtocolError> {
    let mut line = String::new();
    loop {
        line.clear();
        let read_bytes = reader.read_line(&mut line)?;
        if read_bytes == 0 {
            return Err(ProtocolError::UnexpectedEof);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(serde_json::from_str(trimmed)?);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn json_lines_roundtrip_handles_blank_lines_and_tags() -> Result<(), Box<dyn std::error::Error>>
    {
        let layout = ShmLayout::from_spec(ShmLayoutSpec {
            width: 1920,
            height: 1080,
            pixel_bytes: 4,
            video_slot_count: 2,
            audio_channels: 2,
            audio_slot_capacity_frames: 4096,
            audio_slot_count: 2,
        })?;
        let msg = HostToPluginMessage::Init(Box::new(ExportInitPayload {
            protocol_version: EXPORT_PROTOCOL_VERSION,
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            total_video_frames: 120,
            sample_rate: 48_000,
            audio_channels: 2,
            pixel_format: PixelFormat::Bgra8,
            audio_sample_format: AudioSampleFormat::F32Interleaved,
            shm_path: "test.shm".to_owned(),
            shm_layout: layout,
            output_path: "out.mkv".to_owned(),
            preset_id: "default".to_owned(),
            options: serde_json::json!({ "speed": 8 }),
        }));

        let mut buf = Vec::new();
        buf.extend_from_slice(b"\n");
        write_json_line(&mut buf, &msg)?;
        let mut cursor = Cursor::new(buf);
        let decoded: HostToPluginMessage = read_json_line(&mut cursor)?;
        assert_eq!(decoded, msg);

        let evt = PluginToHostMessage::SlotAck {
            kind: SlotKind::Video,
            slot_index: 1,
        };
        let mut evt_buf = Vec::new();
        write_json_line(&mut evt_buf, &evt)?;
        let mut evt_cursor = Cursor::new(evt_buf);
        let decoded_evt: PluginToHostMessage = read_json_line(&mut evt_cursor)?;
        assert_eq!(decoded_evt, evt);
        Ok(())
    }

    #[test]
    fn background_process_flags_configure_command_cleanly() {
        let mut cmd = Command::new("cmd");
        apply_background_process_flags(&mut cmd);
        assert_eq!(cmd.get_program(), "cmd");
    }
}
