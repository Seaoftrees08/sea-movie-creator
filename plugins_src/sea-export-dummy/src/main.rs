//! 書き出しプラグインホストの IPC／共有メモリ／中断・異常終了テスト用ダミープラグイン。

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use sea_export_protocol::backends::shm_mmap::ShmPluginRegion;
use sea_export_protocol::{
    EXPORT_PROTOCOL_VERSION, ExportInitPayload, HostToPluginMessage, PluginToHostMessage, SlotKind,
    read_json_line, write_json_line,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct DummyExportSummary {
    preset_id: String,
    width: u32,
    height: u32,
    frames_received: u64,
    audio_samples_received: u64,
    frame_digest: u64,
    audio_abs_sum: f64,
}

struct SessionState {
    init: ExportInitPayload,
    shm: ShmPluginRegion,
    frames_received: u64,
    audio_samples_received: u64,
    frame_digest: u64,
    audio_abs_sum: f64,
    sleep_per_frame_ms: u64,
    simulate_crash_at_frame: Option<u64>,
}

fn main() -> ExitCode {
    match run_plugin_loop() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let mut out = io::stdout().lock();
            let _ = write_json_line(
                &mut out,
                &PluginToHostMessage::Error {
                    code: "DUMMY_PLUGIN_ERROR".to_owned(),
                    message: err,
                },
            );
            ExitCode::from(1)
        }
    }
}

fn run_plugin_loop() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    let first_msg: HostToPluginMessage =
        read_json_line(&mut reader).map_err(|err| format!("failed reading Init: {err}"))?;
    let init = match first_msg {
        HostToPluginMessage::Init(payload) => *payload,
        _ => return Err("expected Init as first host command".to_owned()),
    };
    let mut session = create_session(init)?;

    write_json_line(
        &mut writer,
        &PluginToHostMessage::Ready {
            protocol_version: EXPORT_PROTOCOL_VERSION,
        },
    )
    .map_err(|err| err.to_string())?;

    loop {
        let msg: HostToPluginMessage =
            read_json_line(&mut reader).map_err(|err| format!("failed reading command: {err}"))?;
        match msg {
            HostToPluginMessage::Init(_) => {
                return Err("received duplicate Init command".to_owned());
            }
            HostToPluginMessage::ProcessVideoSlot {
                slot_index,
                frame_index,
                data_len_bytes,
            } => {
                if handle_video_slot(
                    &mut session,
                    &mut writer,
                    slot_index,
                    frame_index,
                    data_len_bytes,
                )? {
                    return Ok(());
                }
            }
            HostToPluginMessage::ProcessAudioSlot {
                slot_index,
                start_sample: _,
                sample_frame_count,
                data_len_bytes,
            } => {
                handle_audio_slot(
                    &mut session,
                    &mut writer,
                    slot_index,
                    sample_frame_count,
                    data_len_bytes,
                )?;
            }
            HostToPluginMessage::Finish => {
                finalize_summary(&session, &mut writer)?;
                return Ok(());
            }
            HostToPluginMessage::Abort => {
                let _ = write_json_line(
                    &mut writer,
                    &PluginToHostMessage::Error {
                        code: "CANCELLED".to_owned(),
                        message: "export aborted by host".to_owned(),
                    },
                );
                return Ok(());
            }
        }
    }
}

fn create_session(init: ExportInitPayload) -> Result<SessionState, String> {
    let shm = ShmPluginRegion::open_readonly(Path::new(&init.shm_path), init.shm_layout)
        .map_err(|err| format!("failed to map shm: {err}"))?;
    let sleep_per_frame_ms = init
        .options
        .get("sleep_per_frame_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let simulate_crash_at_frame = init
        .options
        .get("simulate_crash_at_frame")
        .and_then(serde_json::Value::as_u64);
    Ok(SessionState {
        init,
        shm,
        frames_received: 0,
        audio_samples_received: 0,
        frame_digest: 0xcbf2_9ce4_8422_2325,
        audio_abs_sum: 0.0,
        sleep_per_frame_ms,
        simulate_crash_at_frame,
    })
}

fn handle_video_slot<W: Write>(
    session: &mut SessionState,
    writer: &mut W,
    slot_index: u32,
    frame_index: u64,
    data_len_bytes: u64,
) -> Result<bool, String> {
    if session.simulate_crash_at_frame == Some(frame_index) {
        std::process::exit(42);
    }
    if session.sleep_per_frame_ms > 0 {
        std::thread::sleep(Duration::from_millis(session.sleep_per_frame_ms));
    }

    let len_usize =
        usize::try_from(data_len_bytes).map_err(|_| "video byte count exceeds usize".to_owned())?;
    let bytes = session
        .shm
        .read_video_slot(slot_index, len_usize)
        .map_err(|err| err.to_string())?;
    let mut hash = session.frame_digest ^ frame_index;
    hash = hash.wrapping_mul(0x0100_0000_01b3);
    for &b in bytes.iter().step_by(97) {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    session.frame_digest = hash;
    session.frames_received = session.frames_received.saturating_add(1);

    write_json_line(
        writer,
        &PluginToHostMessage::SlotAck {
            kind: SlotKind::Video,
            slot_index,
        },
    )
    .map_err(|err| err.to_string())?;
    write_json_line(
        writer,
        &PluginToHostMessage::Progress {
            frames_encoded: session.frames_received,
            total_frames: session.init.total_video_frames,
            audio_samples_encoded: session.audio_samples_received,
            message: Some(format!("dummy frame {frame_index}")),
        },
    )
    .map_err(|err| err.to_string())?;
    Ok(false)
}

fn handle_audio_slot<W: Write>(
    session: &mut SessionState,
    writer: &mut W,
    slot_index: u32,
    sample_frame_count: u32,
    data_len_bytes: u64,
) -> Result<(), String> {
    let len_usize =
        usize::try_from(data_len_bytes).map_err(|_| "audio byte count exceeds usize".to_owned())?;
    let bytes = session
        .shm
        .read_audio_slot(slot_index, len_usize)
        .map_err(|err| err.to_string())?;
    for chunk in bytes.as_chunks::<4>().0 {
        let sample = f32::from_le_bytes(*chunk);
        session.audio_abs_sum += f64::from(sample.abs());
    }
    session.audio_samples_received = session
        .audio_samples_received
        .saturating_add(u64::from(sample_frame_count));

    write_json_line(
        writer,
        &PluginToHostMessage::SlotAck {
            kind: SlotKind::Audio,
            slot_index,
        },
    )
    .map_err(|err| err.to_string())
}

fn finalize_summary<W: Write>(session: &SessionState, writer: &mut W) -> Result<(), String> {
    let out_path = Path::new(&session.init.output_path);
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let summary = DummyExportSummary {
        preset_id: session.init.preset_id.clone(),
        width: session.init.width,
        height: session.init.height,
        frames_received: session.frames_received,
        audio_samples_received: session.audio_samples_received,
        frame_digest: session.frame_digest,
        audio_abs_sum: session.audio_abs_sum,
    };
    let file = File::create(out_path).map_err(|err| err.to_string())?;
    let mut bw = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut bw, &summary).map_err(|err| err.to_string())?;
    bw.flush().map_err(|err| err.to_string())?;
    let file_size_bytes = std::fs::metadata(out_path).map_or(0, |meta| meta.len());

    write_json_line(
        writer,
        &PluginToHostMessage::Completed {
            output_path: session.init.output_path.clone(),
            file_size_bytes,
            frames_encoded: session.frames_received,
        },
    )
    .map_err(|err| err.to_string())
}
