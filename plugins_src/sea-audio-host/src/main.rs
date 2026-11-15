//! `sea-movie-creator` 外部音声プラグイン隔離ワーカープロセス (`sea-audio-host`)。
//!
//! 親プロセスとは標準入出力 `NDJSON` と `ShmHostRegion` 共有メモリリングバッファ経由でのみ通信し、
//! サードパーティ製 `.vst3` / `.clap` プラグインのクラッシュやメモリ破損を本体から隔離する。

mod backends;
mod builtin_dsp;

use std::io::{self, BufReader, BufWriter};
use std::path::Path;
use std::process::ExitCode;

use sea_export_protocol::audio_ipc::{
    AudioPluginDescriptor, AudioWorkerRequest, AudioWorkerResponse, WireAudioPluginFormat,
};
use sea_export_protocol::backends::shm_mmap::ShmHostRegion;
use sea_export_protocol::{ProtocolError, ShmLayout, read_json_line, write_json_line};

use crate::backends::clap_host::{ClapHostedPlugin, describe_clap_bundle};
use crate::backends::vst3_host::{Vst3HostedPlugin, describe_vst3_bundle};
use crate::builtin_dsp::BuiltinGainEffect;

enum LoadedPluginBackend {
    Builtin(BuiltinGainEffect),
    Clap(ClapHostedPlugin),
    Vst3(Vst3HostedPlugin),
}

impl LoadedPluginBackend {
    const fn descriptor(&self) -> &AudioPluginDescriptor {
        match self {
            Self::Builtin(b) => b.descriptor(),
            Self::Clap(c) => c.descriptor(),
            Self::Vst3(v) => v.descriptor(),
        }
    }

    fn set_parameter(&mut self, name: &str, value: f64) -> Result<f64, String> {
        match self {
            Self::Builtin(b) => b.set_parameter(name, value),
            Self::Clap(c) => c.set_parameter(name, value),
            Self::Vst3(v) => v.set_parameter(name, value),
        }
    }

    fn process_stereo(
        &mut self,
        interleaved_in: &[f32],
        interleaved_out: &mut [f32],
    ) -> Result<(), String> {
        match self {
            Self::Builtin(b) => {
                b.process_stereo(interleaved_in, interleaved_out);
                Ok(())
            }
            Self::Clap(c) => c.process_stereo(interleaved_in, interleaved_out),
            Self::Vst3(v) => v.process_stereo(interleaved_in, interleaved_out),
        }
    }
}

struct WorkerState {
    plugin: Option<LoadedPluginBackend>,
    shm: Option<ShmHostRegion>,
    in_scratch: Vec<f32>,
    out_scratch: Vec<f32>,
}

struct LoadRequest<'a> {
    format: WireAudioPluginFormat,
    plugin_id: &'a str,
    bundle_path: Option<&'a str>,
    sample_rate: u32,
    max_block_frames: u32,
    shm_path: &'a str,
    shm_layout: ShmLayout,
}

impl WorkerState {
    const fn new() -> Self {
        Self {
            plugin: None,
            shm: None,
            in_scratch: Vec::new(),
            out_scratch: Vec::new(),
        }
    }

    fn handle_load(&mut self, req: &LoadRequest<'_>) -> AudioWorkerResponse {
        let shm = match ShmHostRegion::open_existing_rw(Path::new(req.shm_path), req.shm_layout) {
            Ok(region) => region,
            Err(e) => {
                return AudioWorkerResponse::Error {
                    message: format!("failed to map worker SHM: {e}"),
                };
            }
        };

        let loaded = match req.format {
            WireAudioPluginFormat::BuiltinTest => {
                BuiltinGainEffect::load(req.plugin_id).map(LoadedPluginBackend::Builtin)
            }
            WireAudioPluginFormat::Clap => match req.bundle_path {
                Some(path) => ClapHostedPlugin::load(
                    Path::new(path),
                    req.plugin_id,
                    req.sample_rate,
                    req.max_block_frames,
                )
                .map(LoadedPluginBackend::Clap),
                None => Err("CLAP plugin requires bundle_path".to_owned()),
            },
            WireAudioPluginFormat::Vst3 => match req.bundle_path {
                Some(path) => Vst3HostedPlugin::load(
                    Path::new(path),
                    req.plugin_id,
                    req.sample_rate,
                    req.max_block_frames,
                )
                .map(LoadedPluginBackend::Vst3),
                None => Err("VST3 plugin requires bundle_path".to_owned()),
            },
        };

        match loaded {
            Ok(backend) => {
                let descriptor = backend.descriptor().clone();
                self.plugin = Some(backend);
                self.shm = Some(shm);
                AudioWorkerResponse::Loaded { descriptor }
            }
            Err(message) => AudioWorkerResponse::Error { message },
        }
    }

    fn handle_process(
        &mut self,
        input_slot: u32,
        output_slot: u32,
        frame_count: u32,
        channels: u16,
    ) -> AudioWorkerResponse {
        let Some(plugin) = self.plugin.as_mut() else {
            return AudioWorkerResponse::Error {
                message: "ProcessBlock received before LoadPlugin".to_owned(),
            };
        };
        let Some(shm) = self.shm.as_mut() else {
            return AudioWorkerResponse::Error {
                message: "SHM region is not mapped".to_owned(),
            };
        };
        let sample_count = (frame_count as usize).saturating_mul(channels as usize);
        if let Err(e) = shm.read_audio_slot_f32(input_slot, sample_count, &mut self.in_scratch) {
            return AudioWorkerResponse::Error {
                message: format!("failed to read SHM input slot: {e}"),
            };
        }
        self.out_scratch.resize(sample_count, 0.0);
        if let Err(message) = plugin.process_stereo(&self.in_scratch, &mut self.out_scratch) {
            return AudioWorkerResponse::Error { message };
        }
        if let Err(e) = shm.write_audio_slot_f32(output_slot, &self.out_scratch) {
            return AudioWorkerResponse::Error {
                message: format!("failed to write SHM output slot: {e}"),
            };
        }
        AudioWorkerResponse::BlockAck { frame_count }
    }
}

fn handle_describe(format: WireAudioPluginFormat, bundle_path: &str) -> AudioWorkerResponse {
    let path = Path::new(bundle_path);
    let res = match format {
        WireAudioPluginFormat::BuiltinTest => {
            BuiltinGainEffect::load("sea.test.gain").map(|b| vec![b.descriptor().clone()])
        }
        WireAudioPluginFormat::Clap => describe_clap_bundle(path),
        WireAudioPluginFormat::Vst3 => describe_vst3_bundle(path),
    };
    match res {
        Ok(plugins) => AudioWorkerResponse::Described { plugins },
        Err(message) => AudioWorkerResponse::Error { message },
    }
}

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut state = WorkerState::new();

    loop {
        let req: AudioWorkerRequest = match read_json_line(&mut reader) {
            Ok(req) => req,
            Err(ProtocolError::UnexpectedEof) => return ExitCode::SUCCESS,
            Err(e) => {
                let _ = write_json_line(
                    &mut writer,
                    &AudioWorkerResponse::Error {
                        message: format!("invalid JSON IPC request: {e}"),
                    },
                );
                return ExitCode::from(1);
            }
        };

        let resp = match req {
            AudioWorkerRequest::LoadPlugin {
                format,
                plugin_id,
                bundle_path,
                sample_rate,
                max_block_frames,
                shm_path,
                shm_layout,
            } => state.handle_load(&LoadRequest {
                format,
                plugin_id: &plugin_id,
                bundle_path: bundle_path.as_deref(),
                sample_rate,
                max_block_frames,
                shm_path: &shm_path,
                shm_layout,
            }),
            AudioWorkerRequest::DescribePlugin {
                format,
                bundle_path,
            } => handle_describe(format, &bundle_path),
            AudioWorkerRequest::SetParameter { name, value } => match state.plugin.as_mut() {
                Some(plugin) => match plugin.set_parameter(&name, value) {
                    Ok(applied_value) => AudioWorkerResponse::ParameterAck {
                        name,
                        applied_value,
                    },
                    Err(message) => AudioWorkerResponse::Error { message },
                },
                None => AudioWorkerResponse::Error {
                    message: "SetParameter received before LoadPlugin".to_owned(),
                },
            },
            AudioWorkerRequest::ProcessBlock {
                input_slot,
                output_slot,
                frame_count,
                channels,
            } => state.handle_process(input_slot, output_slot, frame_count, channels),
            AudioWorkerRequest::SimulateCrash => return ExitCode::from(101),
            AudioWorkerRequest::Shutdown => {
                let _ = write_json_line(&mut writer, &AudioWorkerResponse::ShutdownAck);
                return ExitCode::SUCCESS;
            }
        };

        if write_json_line(&mut writer, &resp).is_err() {
            return ExitCode::from(1);
        }
    }
}
