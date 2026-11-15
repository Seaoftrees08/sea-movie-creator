//! `sea-export-av1`: 純粋 Rust (`rav1e` + `flacenc` + `webm-iterable`) による AV1 書き出しプラグインバイナリ。

#![doc = "AV1 + FLAC Matroska export plugin implementing sea-export-protocol v1."]

mod color_yuv;
mod mkv_muxer;

use std::io::{self, BufReader, BufWriter, Write};
use std::path::PathBuf;

use rav1e::color::{
    ColorDescription, ColorPrimaries, MatrixCoefficients, PixelRange, TransferCharacteristics,
};
use rav1e::prelude::{
    ChromaSamplePosition, ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, Rational,
};
use sea_export_protocol::backends::mf_sink::{MfSinkConfig, MfSinkWriter, MfVideoCodec};
use sea_export_protocol::backends::shm_mmap::ShmPluginRegion;
use sea_export_protocol::{
    EXPORT_PROTOCOL_VERSION, ExportInitPayload, HostToPluginMessage, PixelFormat,
    PluginToHostMessage, SlotKind, read_json_line, write_json_line,
};

use color_yuv::{
    Av1BackendMode, Av1EncodeSettings, fill_rav1e_frame_from_bgra, validate_video_geometry,
};
use mkv_muxer::{EncodedPacket, MkvTrackConfig, encode_pcm_to_flac, write_mkv_file};

struct Rav1eSession {
    init: ExportInitPayload,
    shm: ShmPluginRegion,
    width: usize,
    height: usize,
    ctx: Context<u8>,
    video_packets: Vec<EncodedPacket>,
    pcm_samples_i32: Vec<i32>,
    frames_submitted: u64,
    audio_samples_received: u64,
}

impl Rav1eSession {
    fn create(init: ExportInitPayload, settings: &Av1EncodeSettings) -> Result<Self, String> {
        if init.pixel_format != PixelFormat::Bgra8 {
            return Err(format!(
                "未対応のピクセル形式です (bgra8 のみ対応): {:?}",
                init.pixel_format
            ));
        }
        let (width, height) = validate_video_geometry(init.width, init.height)?;
        let shm =
            ShmPluginRegion::open_readonly(std::path::Path::new(&init.shm_path), init.shm_layout)
                .map_err(|err| format!("共有メモリを開けません: {err}"))?;

        let mut enc = EncoderConfig::with_speed_preset(settings.speed);
        enc.width = width;
        enc.height = height;
        enc.quantizer = settings.quantizer;
        enc.bit_depth = 8;
        enc.chroma_sampling = ChromaSampling::Cs420;
        enc.chroma_sample_position = ChromaSamplePosition::Colocated;
        enc.pixel_range = PixelRange::Limited;
        enc.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::BT709,
            matrix_coefficients: MatrixCoefficients::BT709,
        });
        enc.time_base = Rational {
            num: u64::from(init.fps_den.max(1)),
            den: u64::from(init.fps_num.max(1)),
        };

        let mut cfg = Config::new().with_encoder_config(enc);
        if settings.threads > 0 {
            cfg = cfg.with_threads(settings.threads);
        }
        let ctx: Context<u8> = cfg
            .new_context()
            .map_err(|err| format!("rav1e コンテキスト初期化に失敗しました: {err}"))?;

        Ok(Self {
            init,
            shm,
            width,
            height,
            ctx,
            video_packets: Vec::new(),
            pcm_samples_i32: Vec::new(),
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
            .map_err(|err| format!("映像共有メモリ読み取りエラー: {err}"))?;

        let mut frame = self.ctx.new_frame();
        fill_rav1e_frame_from_bgra(
            bgra,
            self.width,
            self.height,
            PixelRange::Limited,
            &mut frame,
        )?;

        self.ctx
            .send_frame(frame)
            .map_err(|err| format!("rav1e フレーム投入エラー (frame={frame_index}): {err}"))?;
        self.drain_available_packets(false)?;
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
        let raw_bytes = self
            .shm
            .read_audio_slot(slot_index, len_usize)
            .map_err(|err| format!("音声共有メモリ読み取りエラー: {err}"))?;

        self.pcm_samples_i32.reserve(raw_bytes.len() / 4);
        for chunk in raw_bytes.as_chunks::<4>().0 {
            let sample_f32 = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let sanitized = if sample_f32.is_finite() {
                sample_f32.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            #[allow(clippy::cast_possible_truncation)]
            let quantized = (sanitized * 32767.0).round() as i32;
            self.pcm_samples_i32.push(quantized);
        }
        self.audio_samples_received = self
            .audio_samples_received
            .saturating_add(u64::from(sample_frame_count));
        Ok(())
    }

    fn finish_and_write_mkv(mut self) -> Result<(u64, u64), String> {
        self.ctx.flush();
        self.drain_available_packets(true)?;

        let audio_track = encode_pcm_to_flac(
            &self.pcm_samples_i32,
            self.init.sample_rate,
            self.init.audio_channels,
        )?;
        let mkv_config = MkvTrackConfig {
            width: self.init.width,
            height: self.init.height,
            fps_num: self.init.fps_num,
            fps_den: self.init.fps_den,
            total_video_frames: self.frames_submitted,
            av1_codec_private: self.ctx.container_sequence_header(),
            audio: audio_track,
        };

        let out_path = PathBuf::from(&self.init.output_path);
        let file_size = write_mkv_file(&out_path, &mkv_config, &self.video_packets)?;
        let encoded_frames =
            u64::try_from(self.video_packets.len()).unwrap_or(self.frames_submitted);
        Ok((file_size, encoded_frames))
    }

    fn drain_available_packets(&mut self, wait_until_limit: bool) -> Result<(), String> {
        let fps_num = u128::from(self.init.fps_num.max(1));
        let fps_den = u128::from(self.init.fps_den.max(1));

        loop {
            match self.ctx.receive_packet() {
                Ok(packet) => {
                    let pts_ms_u128 = (u128::from(packet.input_frameno) * 1000 * fps_den) / fps_num;
                    let pts_ms = u64::try_from(pts_ms_u128).unwrap_or(u64::MAX);
                    self.video_packets.push(EncodedPacket {
                        track_number: 1,
                        pts_ms,
                        is_keyframe: packet.frame_type.all_intra(),
                        data: packet.data,
                    });
                }
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData) => {
                    if !wait_until_limit {
                        break;
                    }
                    return Err(
                        "rav1e フラッシュ中に予期しない NeedMoreData が発生しました".to_owned()
                    );
                }
                Err(EncoderStatus::LimitReached) => break,
                Err(err) => return Err(format!("rav1e エンコードエラー: {err}")),
            }
        }
        Ok(())
    }
}

struct MfAv1Session {
    init: ExportInitPayload,
    shm: ShmPluginRegion,
    writer: MfSinkWriter,
    frames_submitted: u64,
    audio_samples_received: u64,
}

impl MfAv1Session {
    fn create(init: ExportInitPayload, settings: &Av1EncodeSettings) -> Result<Self, String> {
        if init.pixel_format != PixelFormat::Bgra8 {
            return Err(format!(
                "未対応のピクセル形式です (bgra8 のみ対応): {:?}",
                init.pixel_format
            ));
        }
        let shm =
            ShmPluginRegion::open_readonly(std::path::Path::new(&init.shm_path), init.shm_layout)
                .map_err(|err| format!("共有メモリを開けません: {err}"))?;
        let mf_config = MfSinkConfig {
            video_codec: MfVideoCodec::Av1,
            width: init.width,
            height: init.height,
            fps_num: init.fps_num.max(1),
            fps_den: init.fps_den.max(1),
            bitrate_kbps: settings.bitrate_kbps,
            sample_rate: init.sample_rate,
            audio_channels: init.audio_channels,
            enable_audio: init.audio_channels > 0,
            prefer_hardware: true,
        };
        let writer = MfSinkWriter::open(std::path::Path::new(&init.output_path), mf_config)?;
        Ok(Self {
            init,
            shm,
            writer,
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
            .map_err(|err| format!("映像共有メモリ読み取りエラー: {err}"))?;
        self.writer.write_video_bgra8(frame_index, bgra)?;
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
            .map_err(|err| format!("音声共有メモリ読み取りエラー: {err}"))?;
        self.writer.write_audio_f32(sample_frame_count, raw)?;
        self.audio_samples_received = self
            .audio_samples_received
            .saturating_add(u64::from(sample_frame_count));
        Ok(())
    }

    fn finish(self) -> Result<(u64, u64), String> {
        let frames = self.frames_submitted;
        let bytes = self.writer.finalize()?;
        Ok((bytes, frames))
    }
}

enum Av1EncoderSession {
    SoftwareRav1e(Box<Rav1eSession>),
    HardwareMf(Box<MfAv1Session>),
}

impl Av1EncoderSession {
    fn create(init: ExportInitPayload, settings: &Av1EncodeSettings) -> Result<Self, String> {
        match settings.backend_mode {
            Av1BackendMode::SoftwareRav1e => Ok(Self::SoftwareRav1e(Box::new(
                Rav1eSession::create(init, settings)?,
            ))),
            Av1BackendMode::OsHardwareMf => Ok(Self::HardwareMf(Box::new(MfAv1Session::create(
                init, settings,
            )?))),
        }
    }

    fn output_path(&self) -> &str {
        match self {
            Self::SoftwareRav1e(s) => &s.init.output_path,
            Self::HardwareMf(s) => &s.init.output_path,
        }
    }

    fn stats(&self) -> (u64, u64, u64) {
        match self {
            Self::SoftwareRav1e(s) => (
                s.frames_submitted,
                s.init.total_video_frames,
                s.audio_samples_received,
            ),
            Self::HardwareMf(s) => (
                s.frames_submitted,
                s.init.total_video_frames,
                s.audio_samples_received,
            ),
        }
    }

    fn process_video_slot(
        &mut self,
        slot_index: u32,
        frame_index: u64,
        data_len_bytes: u64,
    ) -> Result<(), String> {
        match self {
            Self::SoftwareRav1e(s) => s.process_video_slot(slot_index, frame_index, data_len_bytes),
            Self::HardwareMf(s) => s.process_video_slot(slot_index, frame_index, data_len_bytes),
        }
    }

    fn process_audio_slot(
        &mut self,
        slot_index: u32,
        sample_frame_count: u32,
        data_len_bytes: u64,
    ) -> Result<(), String> {
        match self {
            Self::SoftwareRav1e(s) => {
                s.process_audio_slot(slot_index, sample_frame_count, data_len_bytes)
            }
            Self::HardwareMf(s) => {
                s.process_audio_slot(slot_index, sample_frame_count, data_len_bytes)
            }
        }
    }

    fn finish(self) -> Result<(u64, u64), String> {
        match self {
            Self::SoftwareRav1e(s) => s.finish_and_write_mkv(),
            Self::HardwareMf(s) => s.finish(),
        }
    }
}

fn send_error_and_exit<W: Write>(writer: &mut W, code: &str, message: &str) {
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
        read_json_line(&mut reader).map_err(|err| format!("Init メッセージ受信失敗: {err}"))?;
    let init = match init_msg {
        HostToPluginMessage::Init(payload) => *payload,
        HostToPluginMessage::Abort => return Ok(()),
        other => {
            return Err(format!(
                "初回コマンドとして Init が必要ですが別コマンドを受信しました: {other:?}"
            ));
        }
    };

    if init.protocol_version != EXPORT_PROTOCOL_VERSION {
        let msg = format!(
            "プロトコルバージョン不一致: host={}, plugin={EXPORT_PROTOCOL_VERSION}",
            init.protocol_version
        );
        send_error_and_exit(&mut writer, "protocol_mismatch", &msg);
        return Err(msg);
    }

    let settings = match Av1EncodeSettings::from_init(&init.preset_id, &init.options) {
        Ok(s) => s,
        Err(msg) => {
            send_error_and_exit(&mut writer, "invalid_options", &msg);
            return Err(msg);
        }
    };

    let session = match Av1EncoderSession::create(init, &settings) {
        Ok(s) => s,
        Err(msg) => {
            send_error_and_exit(&mut writer, "init_failed", &msg);
            return Err(msg);
        }
    };

    write_json_line(
        &mut writer,
        &PluginToHostMessage::Ready {
            protocol_version: EXPORT_PROTOCOL_VERSION,
        },
    )
    .map_err(|err| format!("Ready 応答送信失敗: {err}"))?;

    process_commands(&mut reader, &mut writer, session)
}

fn process_commands<R: io::BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut session: Av1EncoderSession,
) -> Result<(), String> {
    loop {
        let msg: HostToPluginMessage =
            read_json_line(reader).map_err(|err| format!("コマンド読み取り失敗: {err}"))?;
        match msg {
            HostToPluginMessage::ProcessVideoSlot {
                slot_index,
                frame_index,
                data_len_bytes,
            } => {
                if let Err(err) =
                    session.process_video_slot(slot_index, frame_index, data_len_bytes)
                {
                    send_error_and_exit(writer, "video_encode_failed", &err);
                    return Err(err);
                }
                send_slot_ack_and_progress(writer, SlotKind::Video, slot_index, &session)?;
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
                    send_error_and_exit(writer, "audio_encode_failed", &err);
                    return Err(err);
                }
                send_slot_ack_and_progress(writer, SlotKind::Audio, slot_index, &session)?;
            }
            HostToPluginMessage::Finish => {
                let output_path = session.output_path().to_owned();
                let (file_size_bytes, frames_encoded) = match session.finish() {
                    Ok(res) => res,
                    Err(err) => {
                        send_error_and_exit(writer, "mux_failed", &err);
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
                let msg = "セッション中に重複した Init を受信しました";
                send_error_and_exit(writer, "duplicate_init", msg);
                return Err(msg.to_owned());
            }
        }
    }
}

fn send_slot_ack_and_progress<W: Write>(
    writer: &mut W,
    kind: SlotKind,
    slot_index: u32,
    session: &Av1EncoderSession,
) -> Result<(), String> {
    write_json_line(writer, &PluginToHostMessage::SlotAck { kind, slot_index })
        .map_err(|err| format!("SlotAck 送信失敗: {err}"))?;

    let (frames_encoded, total_frames, audio_samples_encoded) = session.stats();
    if matches!(kind, SlotKind::Video) && frames_encoded.is_multiple_of(5) {
        write_json_line(
            writer,
            &PluginToHostMessage::Progress {
                frames_encoded,
                total_frames,
                audio_samples_encoded,
                message: Some("AV1 エンコード中".to_owned()),
            },
        )
        .map_err(|err| format!("Progress 送信失敗: {err}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(err) = run_plugin_loop() {
        eprintln!("sea-export-av1 error: {err}");
        std::process::exit(1);
    }
}
