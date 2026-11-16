//! `sea-audio-host` 外部ワーカープロセス隔離ホストおよびクラッシュ時バイパス制御。

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use sea_export_protocol::backends::shm_mmap::ShmHostRegion;
use sea_export_protocol::{
    ProtocolError, ShmLayout, ShmLayoutSpec, read_json_line, write_json_line,
};

use crate::audio::plugins::descriptor::{AudioWorkerRequest, AudioWorkerResponse, to_wire_format};
use crate::audio::properties::{AudioPluginFormat, AudioPluginParamValue, AudioPluginSlot};
use crate::core::animation::EvaluationContext;
use crate::core::ids::ClipId;
use crate::export::registry::{platform_exe_candidates, resolve_plugin_executable};

const DEFAULT_MAX_BLOCK_FRAMES: u32 = 4096;
const STEREO_CHANNELS: u16 = 2;

#[derive(Debug)]
struct ActiveWorkerProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    shm: ShmHostRegion,
    worker_pid: u32,
}

impl Drop for ActiveWorkerProcess {
    fn drop(&mut self) {
        let _ = write_json_line(&mut self.stdin, &AudioWorkerRequest::Shutdown);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 単一プラグインスロットに対応し、別プロセス＋共有メモリで音声を処理する隔離ホスト。
#[derive(Debug)]
pub struct IsolatedAudioPluginHost {
    plugin_id: String,
    format: AudioPluginFormat,
    bundle_path: Option<String>,
    sample_rate: u32,
    max_block_frames: u32,
    bypassed: bool,
    last_error: Option<String>,
    cached_params: BTreeMap<String, f64>,
    worker: Option<ActiveWorkerProcess>,
}

impl IsolatedAudioPluginHost {
    /// 指定プラグイン用の外部ワーカープロセス (`sea-audio-host`) を起動し初期ロードする。
    #[must_use]
    pub fn spawn(
        slot: &AudioPluginSlot,
        sample_rate: u32,
        host_executable_override: Option<&Path>,
    ) -> Self {
        let mut host = Self {
            plugin_id: slot.plugin_id.clone(),
            format: slot.format,
            bundle_path: slot.bundle_path.clone(),
            sample_rate,
            max_block_frames: DEFAULT_MAX_BLOCK_FRAMES,
            bypassed: false,
            last_error: None,
            cached_params: BTreeMap::new(),
            worker: None,
        };
        if let Err(err) = host.connect_worker(slot, host_executable_override) {
            host.mark_bypassed(err);
        }
        host
    }

    /// 外部ワーカーへの通信・異常検知後に安全バイパス状態へ入ったか。
    #[must_use]
    pub const fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    /// 最後に記録された失敗理由を返す。
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 稼働中ワーカーの OS プロセス ID を返す。
    #[must_use]
    pub fn worker_pid(&self) -> Option<u32> {
        self.worker.as_ref().map(|w| w.worker_pid)
    }

    fn mark_bypassed(&mut self, reason: String) {
        self.bypassed = true;
        self.last_error = Some(reason);
        self.worker.take();
    }

    fn connect_worker(
        &mut self,
        slot: &AudioPluginSlot,
        host_executable_override: Option<&Path>,
    ) -> Result<(), String> {
        let exe_path = resolve_audio_host_binary(host_executable_override)?;
        let layout = ShmLayout::from_spec(ShmLayoutSpec {
            width: 1,
            height: 1,
            pixel_bytes: 4,
            video_slot_count: 1,
            audio_channels: STEREO_CHANNELS,
            audio_slot_capacity_frames: self.max_block_frames,
            audio_slot_count: 2,
        })
        .map_err(|e| format!("failed to compute audio SHM layout: {e}"))?;
        let shm = ShmHostRegion::create_temp("sea-audio-shm", layout)
            .map_err(|e| format!("failed to allocate audio SHM: {e}"))?;
        let shm_path_str = shm.path().to_string_lossy().into_owned();

        let mut child = Command::new(&exe_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", exe_path.display()))?;
        let worker_pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "missing worker stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing worker stdout".to_owned())?;

        let mut worker = ActiveWorkerProcess {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            shm,
            worker_pid,
        };

        let load_req = AudioWorkerRequest::LoadPlugin {
            format: to_wire_format(slot.format),
            plugin_id: slot.plugin_id.clone(),
            bundle_path: self.bundle_path.clone(),
            sample_rate: self.sample_rate,
            max_block_frames: self.max_block_frames,
            shm_path: shm_path_str,
            shm_layout: layout,
        };
        let resp = exchange_ipc(&mut worker, &load_req)?;
        match resp {
            AudioWorkerResponse::Loaded { .. } => {
                self.worker = Some(worker);
                Ok(())
            }
            AudioWorkerResponse::Error { message } => Err(message),
            other => Err(format!("unexpected LoadPlugin response: {other:?}")),
        }
    }

    /// クリップ上のスロットパラメータを評価し、前回値から変化した項目のみワーカーへ送る。
    pub fn sync_parameters(&mut self, slot: &AudioPluginSlot, ctx: EvaluationContext) {
        if self.bypassed || self.worker.is_none() {
            return;
        }
        for (name, param_val) in &slot.parameters {
            let evaluated = evaluate_plugin_param(param_val, ctx);
            let unchanged = self
                .cached_params
                .get(name)
                .is_some_and(|prev| (prev - evaluated).abs() <= 1e-7);
            if unchanged {
                continue;
            }
            let Some(worker) = self.worker.as_mut() else {
                return;
            };
            let req = AudioWorkerRequest::SetParameter {
                name: name.clone(),
                value: evaluated,
            };
            match exchange_ipc(worker, &req) {
                Ok(AudioWorkerResponse::ParameterAck { applied_value, .. }) => {
                    self.cached_params.insert(name.clone(), applied_value);
                }
                Ok(AudioWorkerResponse::Error { message }) => {
                    self.mark_bypassed(format!("SetParameter error: {message}"));
                    return;
                }
                Ok(other) => {
                    self.mark_bypassed(format!("unexpected SetParameter response: {other:?}"));
                    return;
                }
                Err(err) => {
                    self.mark_bypassed(err);
                    return;
                }
            }
        }
    }

    /// テストまたは故障シミュレーション用: ワーカープロセスに強制異常終了を指示する。
    pub fn simulate_worker_crash(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            let _ = write_json_line(&mut worker.stdin, &AudioWorkerRequest::SimulateCrash);
            let _ = worker.child.wait();
        }
    }

    /// ステレオ `f32` インプレースバッファをワーカー側へ送って処理する。
    ///
    /// ワーカーがクラッシュ・切断された場合は、入力のドライ音を破壊せず安全にバイパスへ移行し
    /// `false` を返す。
    pub fn process_stereo_block(&mut self, stereo_samples: &mut [f32]) -> bool {
        if self.bypassed || stereo_samples.is_empty() || self.worker.is_none() {
            return false;
        }
        let chunk_floats =
            (self.max_block_frames as usize).saturating_mul(STEREO_CHANNELS as usize);
        let mut scratch_out = Vec::new();
        for chunk in stereo_samples.chunks_mut(chunk_floats) {
            let frame_count_usize = chunk.len() / (STEREO_CHANNELS as usize);
            let Ok(frame_count) = u32::try_from(frame_count_usize) else {
                self.mark_bypassed("frame_count overflows u32".to_owned());
                return false;
            };
            if frame_count == 0 {
                continue;
            }
            if let Err(reason) = self.process_single_chunk(chunk, frame_count, &mut scratch_out) {
                self.mark_bypassed(reason);
                return false;
            }
        }
        true
    }

    fn process_single_chunk(
        &mut self,
        chunk: &mut [f32],
        frame_count: u32,
        scratch_out: &mut Vec<f32>,
    ) -> Result<(), String> {
        let Some(worker) = self.worker.as_mut() else {
            return Err("worker process not connected".to_owned());
        };
        worker
            .shm
            .write_audio_slot_f32(0, chunk)
            .map_err(|e| format!("failed to write audio SHM input slot: {e}"))?;
        let req = AudioWorkerRequest::ProcessBlock {
            input_slot: 0,
            output_slot: 1,
            frame_count,
            channels: STEREO_CHANNELS,
        };
        match exchange_ipc(worker, &req)? {
            AudioWorkerResponse::BlockAck { .. } => {}
            AudioWorkerResponse::Error { message } => return Err(message),
            other => return Err(format!("unexpected ProcessBlock response: {other:?}")),
        }
        worker
            .shm
            .read_audio_slot_f32(1, chunk.len(), scratch_out)
            .map_err(|e| format!("failed to read audio SHM output slot: {e}"))?;
        chunk.copy_from_slice(scratch_out);
        Ok(())
    }
}

/// クリップ単位のプラグインチェイン状態を保持し、`AudioMixer` から呼ばれる隔離ホスト管理ラック。
#[derive(Debug, Default)]
pub struct IsolatedAudioPluginRack {
    hosts: BTreeMap<(String, String), IsolatedAudioPluginHost>,
    host_executable_override: Option<PathBuf>,
}

impl IsolatedAudioPluginRack {
    /// 空のプラグインラックを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// テスト用に `sea-audio-host` 実行ファイルパスを明示上書きする。
    pub fn set_host_executable_override(&mut self, path: Option<PathBuf>) {
        self.host_executable_override = path;
    }

    /// クリップ内の有効プラグインスロットを順番に直列適用する。
    pub fn process_clip_slots(
        &mut self,
        clip_id: &ClipId,
        slots: &[AudioPluginSlot],
        eval_ctx: EvaluationContext,
        sample_rate: u32,
        stereo_samples: &mut [f32],
    ) {
        if slots.is_empty() || stereo_samples.is_empty() {
            return;
        }
        let override_path = self
            .host_executable_override
            .as_deref()
            .map(Path::to_path_buf);
        for slot in slots {
            if !slot.enabled {
                continue;
            }
            let key = (clip_id.as_str().to_owned(), slot.slot_id.clone());
            let needs_recreate = self.hosts.get(&key).is_none_or(|existing| {
                existing.plugin_id != slot.plugin_id
                    || existing.format != slot.format
                    || existing.bundle_path != slot.bundle_path
                    || existing.sample_rate != sample_rate
            });
            if needs_recreate {
                let host =
                    IsolatedAudioPluginHost::spawn(slot, sample_rate, override_path.as_deref());
                self.hosts.insert(key.clone(), host);
            }
            if let Some(host) = self.hosts.get_mut(&key) {
                if host.is_bypassed() {
                    continue;
                }
                host.sync_parameters(slot, eval_ctx);
                let _ = host.process_stereo_block(stereo_samples);
            }
        }
    }

    /// 指定クリップ・スロットで現在稼働中のワーカー `PID` を返す。
    #[must_use]
    pub fn worker_pid(&self, clip_id: &ClipId, slot_id: &str) -> Option<u32> {
        self.hosts
            .get(&(clip_id.as_str().to_owned(), slot_id.to_owned()))
            .and_then(IsolatedAudioPluginHost::worker_pid)
    }

    /// 指定クリップ・スロットがクラッシュ等によりバイパス状態へ遷移しているか返す。
    #[must_use]
    pub fn is_slot_bypassed(&self, clip_id: &ClipId, slot_id: &str) -> bool {
        self.hosts
            .get(&(clip_id.as_str().to_owned(), slot_id.to_owned()))
            .is_some_and(IsolatedAudioPluginHost::is_bypassed)
    }

    /// 指定クリップ・スロットの最新エラー文を返す。
    #[must_use]
    pub fn slot_last_error(&self, clip_id: &ClipId, slot_id: &str) -> Option<&str> {
        self.hosts
            .get(&(clip_id.as_str().to_owned(), slot_id.to_owned()))
            .and_then(IsolatedAudioPluginHost::last_error)
    }

    /// 手動復旧用に、バイパス済みの全ホストエントリーを削除して次回再起動可能にする。
    pub fn reset_bypassed_slots(&mut self) {
        self.hosts.retain(|_, host| !host.is_bypassed());
    }
}

fn evaluate_plugin_param(value: &AudioPluginParamValue, ctx: EvaluationContext) -> f64 {
    match value {
        AudioPluginParamValue::Animated { param } => param.evaluate(ctx).unwrap_or(0.0),
        AudioPluginParamValue::Number { value } => *value,
        AudioPluginParamValue::Bool { value } => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
    }
}

fn resolve_audio_host_binary(override_path: Option<&Path>) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().ok();
    let cwd = std::env::current_dir().ok();
    resolve_audio_host_binary_in(override_path, exe.as_deref(), cwd.as_deref())
}

fn resolve_audio_host_binary_in(
    override_path: Option<&Path>,
    exe_path: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        return Err(format!(
            "explicit audio host executable not found: {}",
            p.display()
        ));
    }

    let names = platform_exe_candidates("sea-audio-host");
    let mut audio_plugin_dirs = Vec::new();
    if let Some(exe_dir) = exe_path.and_then(Path::parent) {
        for ancestor in exe_dir.ancestors().take(6) {
            audio_plugin_dirs.push(ancestor.join("plugins").join("audio"));
        }
    }
    if let Some(cwd_dir) = cwd {
        audio_plugin_dirs.push(cwd_dir.join("plugins").join("audio"));
    }

    for dir in &audio_plugin_dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let primary_dir = audio_plugin_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("plugins").join("audio"));
    resolve_plugin_executable(&primary_dir, "sea-audio-host").ok_or_else(|| {
        "sea-audio-host executable not found in plugins/audio/ or build target; run `cargo build --workspace`"
            .to_owned()
    })
}

fn exchange_ipc(
    worker: &mut ActiveWorkerProcess,
    request: &AudioWorkerRequest,
) -> Result<AudioWorkerResponse, String> {
    write_json_line(&mut worker.stdin, request)
        .map_err(|e| format!("failed to send IPC request to worker: {e}"))?;
    match read_json_line(&mut worker.stdout) {
        Ok(resp) => Ok(resp),
        Err(ProtocolError::UnexpectedEof) => {
            Err("audio plugin worker closed IPC pipe unexpectedly (EOF)".to_owned())
        }
        Err(e) => Err(format!("failed to read IPC response from worker: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_audio_host_inside_exe_plugins_audio_before_cwd()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("sea-audio-host-res-{}", std::process::id()));
        let exe_dir = root.join("dist");
        let audio_dir = exe_dir.join("plugins").join("audio");
        let other_cwd = root.join("other_cwd");
        std::fs::create_dir_all(&audio_dir)?;
        std::fs::create_dir_all(other_cwd.join("plugins").join("audio"))?;

        let exe_name = platform_exe_candidates("sea-audio-host")
            .into_iter()
            .next_back()
            .ok_or("missing exe candidate")?;
        let dist_host = audio_dir.join(&exe_name);
        let cwd_host = other_cwd.join("plugins").join("audio").join(&exe_name);
        std::fs::write(&dist_host, b"dist-host")?;
        std::fs::write(&cwd_host, b"cwd-host")?;

        let app_exe = exe_dir.join("sea-movie-creator.exe");
        let resolved = resolve_audio_host_binary_in(None, Some(&app_exe), Some(&other_cwd))?;
        assert_eq!(resolved, dist_host);

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
