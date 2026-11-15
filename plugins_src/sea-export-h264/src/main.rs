//! `sea-export-h264`: OS ネイティブ Media Foundation HW/SW および任意外部 `ffmpeg` CLI による H.264 書き出しプラグイン。

#![doc = "H.264/AAC MP4 export plugin implementing sea-export-protocol v1 without bundling GPL/patented encoders."]

mod ffmpeg_cli;

use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;

use sea_export_protocol::backends::mf_sink::{MfSinkConfig, MfSinkWriter, MfVideoCodec};
use sea_export_protocol::backends::shm_mmap::ShmPluginRegion;
use sea_export_protocol::{
    EXPORT_PROTOCOL_VERSION, ExportInitPayload, HostToPluginMessage, PixelFormat,
    PluginToHostMessage, SlotKind, read_json_line, write_json_line,
};
use serde_json::Value;

use ffmpeg_cli::ExternalFfmpegSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264BackendMode {
    WindowsMf,
    ExternalFfmpeg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct H264Settings {
    backend_mode: H264BackendMode,
    bitrate_kbps: u32,
    prefer_hardware: bool,
    ffmpeg_path: String,
}

impl H264Settings {
    fn from_init(preset_id: &str, options: &Value) -> Result<Self, String> {
        let mut settings = match preset_id {
            "h264_external_ffmpeg" => Self {
                backend_mode: H264BackendMode::ExternalFfmpeg,
                bitrate_kbps: 10_000,
                prefer_hardware: true,
                ffmpeg_path: "ffmpeg".to_owned(),
            },
            _ => Self {
                backend_mode: H264BackendMode::WindowsMf,
                bitrate_kbps: 8000,
                prefer_hardware: true,
                ffmpeg_path: "ffmpeg".to_owned(),
            },
        };

        if let Some(mode) = options.get("backend_mode").and_then(Value::as_str) {
            settings.backend_mode = match mode {
                "windows_mf" => H264BackendMode::WindowsMf,
                "external_ffmpeg" => H264BackendMode::ExternalFfmpeg,
                other => {
                    return Err(format!(
                        "未知の H.264 バックエンドモードです: '{other}' (windows_mf / external_ffmpeg)"
                    ));
                }
            };
        }
        if let Some(bitrate) = options.get("bitrate_kbps").and_then(Value::as_u64) {
            settings.bitrate_kbps = u32::try_from(bitrate.clamp(200, 200_000)).unwrap_or(8000);
        }
        if let Some(pref_hw) = options.get("prefer_hardware").and_then(Value::as_bool) {
            settings.prefer_hardware = pref_hw;
        }
        if let Some(path) = options.get("ffmpeg_path").and_then(Value::as_str) {
            path.clone_into(&mut settings.ffmpeg_path);
        }
        Ok(settings)
    }
}

enum H264Backend {
    WindowsMf(MfSinkWriter),
    ExternalFfmpeg(Box<ExternalFfmpegSession>),
}

struct H264Session {
    init: ExportInitPayload,
    shm: ShmPluginRegion,
    backend: H264Backend,
    frames_submitted: u64,
    audio_samples_received: u64,
}

impl H264Session {
    fn create(init: ExportInitPayload, settings: &H264Settings) -> Result<Self, String> {
        if init.pixel_format != PixelFormat::Bgra8 {
            return Err(format!(
                "未対応のピクセル形式です (bgra8 のみ対応): {:?}",
                init.pixel_format
            ));
        }
        let shm = ShmPluginRegion::open_readonly(Path::new(&init.shm_path), init.shm_layout)
            .map_err(|err| format!("共有メモリを開けません: {err}"))?;

        let backend = match settings.backend_mode {
            H264BackendMode::WindowsMf => {
                let mf_config = MfSinkConfig {
                    video_codec: MfVideoCodec::H264,
                    width: init.width,
                    height: init.height,
                    fps_num: init.fps_num.max(1),
                    fps_den: init.fps_den.max(1),
                    bitrate_kbps: settings.bitrate_kbps,
                    sample_rate: init.sample_rate,
                    audio_channels: init.audio_channels,
                    enable_audio: init.audio_channels > 0,
                    prefer_hardware: settings.prefer_hardware,
                };
                let writer = MfSinkWriter::open(Path::new(&init.output_path), mf_config)?;
                H264Backend::WindowsMf(writer)
            }
            H264BackendMode::ExternalFfmpeg => {
                let ext = ExternalFfmpegSession::start(
                    &init,
                    &settings.ffmpeg_path,
                    settings.bitrate_kbps,
                )?;
                H264Backend::ExternalFfmpeg(Box::new(ext))
            }
        };

        Ok(Self {
            init,
            shm,
            backend,
            frames_submitted: 0,
            audio_samples_received: 0,
        })
    }

    fn process_video_slot(
        &mut self,
        slot_index: u32,
        frame_index: u64,
        data_len_bytes: u64,
    ) -> Result<(), String> {
        let len_usize = usize::try_from(data_len_bytes)
            .map_err(|_| "映像スロット長が usize を超えています".to_owned())?;
        let bgra = self
            .shm
            .read_video_slot(slot_index, len_usize)
            .map_err(|err| format!("映像共有メモリ読み取り失敗: {err}"))?;

        match &mut self.backend {
            H264Backend::WindowsMf(writer) => writer.write_video_bgra8(frame_index, bgra)?,
            H264Backend::ExternalFfmpeg(ext) => ext.write_video_bgra8(bgra)?,
        }
        self.frames_submitted = self.frames_submitted.saturating_add(1);
        Ok(())
    }

    fn process_audio_slot(
        &mut self,
        slot_index: u32,
        sample_frame_count: u32,
        data_len_bytes: u64,
    ) -> Result<(), String> {
        let len_usize = usize::try_from(data_len_bytes)
            .map_err(|_| "音声スロット長が usize を超えています".to_owned())?;
        let raw = self
            .shm
            .read_audio_slot(slot_index, len_usize)
            .map_err(|err| format!("音声共有メモリ読み取り失敗: {err}"))?;

        match &mut self.backend {
            H264Backend::WindowsMf(writer) => writer.write_audio_f32(sample_frame_count, raw)?,
            H264Backend::ExternalFfmpeg(ext) => ext.write_audio_f32(sample_frame_count, raw)?,
        }
        self.audio_samples_received = self
            .audio_samples_received
            .saturating_add(u64::from(sample_frame_count));
        Ok(())
    }

    fn finish(self) -> Result<(u64, u64), String> {
        let frames_submitted = self.frames_submitted;
        match self.backend {
            H264Backend::WindowsMf(writer) => {
                let bytes = writer.finalize()?;
                Ok((bytes, frames_submitted))
            }
            H264Backend::ExternalFfmpeg(ext) => {
                let _unused_frames = (ext.frames_submitted(), ext.audio_samples_received());
                ext.finalize()
            }
        }
    }
}

fn send_error<W: Write>(writer: &mut W, code: &str, message: &str) {
    let _ = write_json_line(
        writer,
        &PluginToHostMessage::Error {
            code: code.to_owned(),
            message: message.to_owned(),
        },
    );
}

fn run_plugin_loop() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let init_msg: HostToPluginMessage =
        read_json_line(&mut reader).map_err(|err| format!("Init 受信失敗: {err}"))?;
    let init = match init_msg {
        HostToPluginMessage::Init(payload) => *payload,
        HostToPluginMessage::Abort => return Ok(()),
        other => return Err(format!("初回コマンドに Init が必要です: {other:?}")),
    };

    if init.protocol_version != EXPORT_PROTOCOL_VERSION {
        let msg = format!(
            "プロトコルバージョン不一致: host={}, plugin={EXPORT_PROTOCOL_VERSION}",
            init.protocol_version
        );
        send_error(&mut writer, "protocol_mismatch", &msg);
        return Err(msg);
    }

    let settings = match H264Settings::from_init(&init.preset_id, &init.options) {
        Ok(s) => s,
        Err(msg) => {
            send_error(&mut writer, "invalid_options", &msg);
            return Err(msg);
        }
    };

    let session = match H264Session::create(init, &settings) {
        Ok(s) => s,
        Err(msg) => {
            send_error(&mut writer, "init_failed", &msg);
            return Err(msg);
        }
    };

    write_json_line(
        &mut writer,
        &PluginToHostMessage::Ready {
            protocol_version: EXPORT_PROTOCOL_VERSION,
        },
    )
    .map_err(|err| format!("Ready 送信失敗: {err}"))?;

    process_commands(&mut reader, &mut writer, session)
}

fn process_commands<R: io::BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut session: H264Session,
) -> Result<(), String> {
    loop {
        let msg: HostToPluginMessage =
            read_json_line(reader).map_err(|err| format!("コマンド受信失敗: {err}"))?;
        match msg {
            HostToPluginMessage::ProcessVideoSlot {
                slot_index,
                frame_index,
                data_len_bytes,
            } => {
                if let Err(err) =
                    session.process_video_slot(slot_index, frame_index, data_len_bytes)
                {
                    send_error(writer, "video_encode_failed", &err);
                    return Err(err);
                }
                ack_and_progress(writer, SlotKind::Video, slot_index, &session)?;
            }
            HostToPluginMessage::ProcessAudioSlot {
                slot_index,
                start_sample: _,
                sample_frame_count,
                data_len_bytes,
            } => {
                if let Err(err) =
                    session.process_audio_slot(slot_index, sample_frame_count, data_len_bytes)
                {
                    send_error(writer, "audio_encode_failed", &err);
                    return Err(err);
                }
                ack_and_progress(writer, SlotKind::Audio, slot_index, &session)?;
            }
            HostToPluginMessage::Finish => {
                let output_path = session.init.output_path.clone();
                let (file_size_bytes, frames_encoded) = match session.finish() {
                    Ok(res) => res,
                    Err(err) => {
                        send_error(writer, "finalize_failed", &err);
                        return Err(err);
                    }
                };
                write_json_line(
                    writer,
                    &PluginToHostMessage::Completed {
                        output_path,
                        file_size_bytes,
                        frames_encoded,
                    },
                )
                .map_err(|err| format!("Completed 送信失敗: {err}"))?;
                return Ok(());
            }
            HostToPluginMessage::Abort => return Ok(()),
            HostToPluginMessage::Init(_) => {
                let msg = "重複した Init コマンドを受信しました";
                send_error(writer, "duplicate_init", msg);
                return Err(msg.to_owned());
            }
        }
    }
}

fn ack_and_progress<W: Write>(
    writer: &mut W,
    kind: SlotKind,
    slot_index: u32,
    session: &H264Session,
) -> Result<(), String> {
    write_json_line(writer, &PluginToHostMessage::SlotAck { kind, slot_index })
        .map_err(|err| format!("SlotAck 送信失敗: {err}"))?;

    if matches!(kind, SlotKind::Video) && session.frames_submitted.is_multiple_of(5) {
        write_json_line(
            writer,
            &PluginToHostMessage::Progress {
                frames_encoded: session.frames_submitted,
                total_frames: session.init.total_video_frames,
                audio_samples_encoded: session.audio_samples_received,
                message: Some("H.264 / MP4 エンコード中".to_owned()),
            },
        )
        .map_err(|err| format!("Progress 送信失敗: {err}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(err) = run_plugin_loop() {
        eprintln!("sea-export-h264 error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_settings_parse_presets_and_overrides() {
        let mf = H264Settings::from_init("h264_mf_hw", &Value::Null).unwrap();
        assert_eq!(mf.backend_mode, H264BackendMode::WindowsMf);
        assert!(mf.prefer_hardware);
        assert_eq!(mf.bitrate_kbps, 8000);

        let ext = H264Settings::from_init(
            "h264_external_ffmpeg",
            &serde_json::json!({ "bitrate_kbps": 15_000, "ffmpeg_path": "C:/tools/ffmpeg.exe" }),
        )
        .unwrap();
        assert_eq!(ext.backend_mode, H264BackendMode::ExternalFfmpeg);
        assert_eq!(ext.bitrate_kbps, 15_000);
        assert_eq!(ext.ffmpeg_path, "C:/tools/ffmpeg.exe");

        assert!(
            H264Settings::from_init("h264_mf_hw", &serde_json::json!({ "backend_mode": "bad" }))
                .is_err()
        );
    }
}
