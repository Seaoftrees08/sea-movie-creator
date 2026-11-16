//! 書き出しプラグインプロセス起動・共有メモリフレーム送出・進捗通知・中断管理ランナー。

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::audio::mixer::AudioMixer;
use crate::audio::playback::resample_interleaved_pcm;
use crate::core::ids::AssetId;
use crate::core::time::FrameIndex;
use crate::export::cpu_fallback::render_scene_cpu_bgra8;
use crate::export::protocol::{
    AudioSampleFormat, EXPORT_PROTOCOL_VERSION, ExportInitPayload, HostToPluginMessage,
    PixelFormat, PluginToHostMessage, ProtocolError, ShmError, ShmHostRegion, ShmLayout,
    ShmLayoutSpec, SlotKind, read_json_line, write_json_line,
};
use crate::export::registry::{PluginRegistry, RegistryError};
use crate::media::compositor::{CompositorError, RenderScene, WgpuCompositor, build_render_scene};
use crate::media::decoder::{
    BackendPreference, DecodedAudioPcm, VideoDecoder, VideoFrameBuffer, open_audio_decoder,
    open_video_decoder,
};
use crate::media::frame_cache::{FrameCache, FrameCacheKey};
use crate::media::image_loader::load_image_file;
use crate::timeline::document::{AssetKind, AssetStatus, ClipKind, ProjectDocument};
use crate::timeline::serialization::resolve_existing_asset_path;

const VIDEO_SLOT_COUNT: u32 = 2;
const AUDIO_SLOT_COUNT: u32 = 2;
const AUDIO_CHUNK_FRAMES: u32 = 4096;
const AUDIO_CHANNELS: u16 = 2;
const SLOT_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 書き出し処理の進行フェーズ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPhase {
    /// プラグインプロセス起動と共有メモリ初期化中。
    Preparing,
    /// フレーム・音声をプラグインへ送出しながらエンコード中。
    Encoding,
    /// コンテナファイナライズと完了待機中。
    Finalizing,
    /// 正常完了。
    Completed,
    /// ユーザー操作等により中断完了。
    Cancelled,
    /// エラーにより失敗。
    Failed,
}

/// UI や呼び出し元へリアルタイム通知する書き出し進捗スナップショット。
#[derive(Debug, Clone)]
pub struct ExportProgress {
    /// 現在の処理フェーズ。
    pub phase: ExportPhase,
    /// 完了フレーム数。
    pub frames_completed: u64,
    /// 総予定フレーム数。
    pub total_frames: u64,
    /// 経過時間（ミリ秒）。
    pub elapsed_ms: u64,
    /// 表示用詳細メッセージ。
    pub message: String,
}

/// 書き出し正常終了時の成果サマリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    /// 実際に出力されたファイルパス。
    pub output_path: PathBuf,
    /// 出力ファイルサイズ（バイト）。
    pub file_size_bytes: u64,
    /// エンコードされた総映像フレーム数。
    pub frames_encoded: u64,
}

/// 書き出し実行を安全に中断するための協調キャンセルトークン。
#[derive(Debug, Clone, Default)]
pub struct ExportCancelToken(Arc<AtomicBool>);

impl ExportCancelToken {
    /// 新しい未キャンセル状態のトークンを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 中断要求を発行する。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// 中断要求が出されているか判定する。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 書き出しジョブ 1 件分のパラメータ要求仕様。
#[derive(Debug, Clone)]
pub struct ExportJobRequest {
    /// 使用する書き出しプラグイン ID。
    pub plugin_id: String,
    /// 選択されたプリセット ID。
    pub preset_id: String,
    /// 出力ファイルパス。
    pub output_path: PathBuf,
    /// 出力映像幅（ピクセル）。
    pub width: u32,
    /// 出力映像高さ（ピクセル）。
    pub height: u32,
    /// 書き出し開始タイムラインフレーム。
    pub start_frame: FrameIndex,
    /// 書き出し終了タイムラインフレーム（排他的上限）。
    pub end_frame: FrameIndex,
    /// 出力音声サンプルレート（Hz）。
    pub sample_rate: u32,
    /// プリセット由来およびユーザー設定のオプション JSON。
    pub options: serde_json::Value,
}

impl ExportJobRequest {
    /// プロジェクトの総尺・既定解像度を丸ごと書き出す標準リクエストを作成する。
    #[must_use]
    pub fn full_project(
        doc: &ProjectDocument,
        plugin_id: String,
        preset_id: String,
        output_path: PathBuf,
        options: serde_json::Value,
    ) -> Self {
        Self::for_document_range(doc, plugin_id, preset_id, output_path, options, false)
    }

    /// 選択範囲限定フラグに応じて `[start_frame, end_frame)` を解決した書き出しリクエストを作成する。
    #[must_use]
    pub fn for_document_range(
        doc: &ProjectDocument,
        plugin_id: String,
        preset_id: String,
        output_path: PathBuf,
        options: serde_json::Value,
        selection_only: bool,
    ) -> Self {
        let (start_frame, end_frame) = resolve_export_frame_range(doc, selection_only);
        Self {
            plugin_id,
            preset_id,
            output_path,
            width: doc.resolution.width.get(),
            height: doc.resolution.height.get(),
            start_frame,
            end_frame,
            sample_rate: 48_000,
            options,
        }
    }
}

/// プロジェクトの総尺および `selection_range` から書き出しフレーム範囲 `[start, end)` を決定する。
#[must_use]
pub fn resolve_export_frame_range(
    doc: &ProjectDocument,
    selection_only: bool,
) -> (FrameIndex, FrameIndex) {
    let total = doc.total_duration_frames().max(1);
    if selection_only && let Some(range) = doc.selection_range {
        let clamped_start = range.start.get().min(total.saturating_sub(1));
        let clamped_end = range
            .end
            .get()
            .max(clamped_start.saturating_add(1))
            .min(total);
        return (FrameIndex::new(clamped_start), FrameIndex::new(clamped_end));
    }
    (FrameIndex::ZERO, FrameIndex::new(total))
}

/// 書き出しランナー実行時のエラー型。
#[derive(Debug, Error)]
pub enum ExportError {
    /// 出力解像度・フレーム範囲・パス等の指定不正。
    #[error("invalid export request: {0}")]
    InvalidRequest(String),
    /// プラグイン登録またはバイナリ解決失敗。
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// 共有メモリ確保・読み書き失敗。
    #[error(transparent)]
    Shm(#[from] ShmError),
    /// IPC 通信失敗（異常終了や不正 JSON 含む）。
    #[error("plugin IPC failed ({details}): {source}")]
    Protocol {
        /// エラー原因。
        source: ProtocolError,
        /// 子プロセス標準エラー出力等の補足診断。
        details: String,
    },
    /// プラグイン側が明示エラーイベントを返した。
    #[error("export plugin failed [{code}]: {message}")]
    PluginFailed {
        /// プラグイン返却エラーコード。
        code: String,
        /// エラー詳細文言。
        message: String,
    },
    /// 合成処理失敗。
    #[error("frame composition failed: {0}")]
    Compositor(#[from] CompositorError),
    /// ファイルまたはプロセス I/O 失敗。
    #[error("export I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// ユーザーまたは呼び出し元によりキャンセルされた。
    #[error("export cancelled")]
    Cancelled,
}

/// 書き出し中に各フレームの素材・PCM を供給するローカルリソース解決器。
pub struct ExportMediaSources {
    project_dir: Option<PathBuf>,
    frame_cache: FrameCache,
    audio_mixer: AudioMixer,
    video_decoders: HashMap<AssetId, Box<dyn VideoDecoder>>,
}

impl Default for ExportMediaSources {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ExportMediaSources {
    /// 空のリソース解決器を作成する。
    #[must_use]
    pub fn new(project_dir: Option<PathBuf>) -> Self {
        Self {
            project_dir,
            frame_cache: FrameCache::new(512 * 1024 * 1024),
            audio_mixer: AudioMixer::new(),
            video_decoders: HashMap::new(),
        }
    }

    /// 既存アプリコントローラのキャッシュ済みフレームおよび音声 PCM を引き継ぐ。
    pub fn seed_from_cache_and_mixer(&mut self, cache: &FrameCache, mixer: &AudioMixer) {
        for (key, frame) in cache.snapshot_entries() {
            self.frame_cache.insert(key, frame);
        }
        self.audio_mixer = mixer.clone();
    }

    /// 単一フレーム画像をテスト・事前登録用途でシードする。
    pub fn seed_frame(&mut self, asset_id: AssetId, frame_idx: u64, frame: Arc<VideoFrameBuffer>) {
        self.frame_cache
            .insert(FrameCacheKey::new(asset_id, frame_idx, 100), frame);
    }

    /// 単一音声 PCM をテスト・事前登録用途でシードする。
    pub fn seed_audio(&mut self, asset_id: AssetId, pcm: Arc<DecodedAudioPcm>) {
        self.audio_mixer.register_pcm(asset_id, pcm);
    }

    /// ドキュメント内の利用可能アセット（画像・音声・動画デコーダ）を事前読込する。
    pub fn preload_document_assets(&mut self, doc: &ProjectDocument, target_sample_rate: u32) {
        let project_dir = self.project_dir.clone();
        for asset in &doc.assets {
            if asset.status != AssetStatus::Available {
                continue;
            }
            let Some(path) = resolve_existing_asset_path(&asset.path, project_dir.as_deref())
            else {
                continue;
            };
            match asset.kind {
                AssetKind::Image { .. } => {
                    let existing = FrameCacheKey::new(asset.id.clone(), 0, 100);
                    if self.frame_cache.get(&existing).is_none()
                        && let Ok(buf) = load_image_file(&path, None)
                    {
                        self.frame_cache.insert(existing, Arc::new(buf));
                    }
                }
                AssetKind::Audio { .. } => {
                    self.preload_audio_asset(&asset.id, &path, target_sample_rate);
                }
                AssetKind::Video { .. } => {
                    if !self.video_decoders.contains_key(&asset.id)
                        && let Ok(dec) = open_video_decoder(&path, BackendPreference::Auto)
                    {
                        self.video_decoders.insert(asset.id.clone(), dec);
                    }
                    self.preload_audio_asset(&asset.id, &path, target_sample_rate);
                }
            }
        }
    }

    fn preload_audio_asset(&mut self, asset_id: &AssetId, path: &Path, target_sample_rate: u32) {
        if let Ok(mut dec) = open_audio_decoder(path, BackendPreference::Auto)
            && let Ok(pcm) = dec.decode_all_interleaved()
            && let Ok(resampled) = resample_interleaved_pcm(&pcm, target_sample_rate)
        {
            self.audio_mixer
                .register_pcm(asset_id.clone(), Arc::new(resampled));
        }
    }

    fn ensure_frames_for_playhead(&mut self, doc: &ProjectDocument, playhead: FrameIndex) {
        let needed = collect_video_source_frames(doc, playhead);
        for (asset_id, src_idx) in needed {
            let key = FrameCacheKey::new(asset_id.clone(), src_idx, 100);
            if self.frame_cache.get(&key).is_some() {
                continue;
            }
            if !self.video_decoders.contains_key(&asset_id)
                && let Some(asset) = doc.find_asset(&asset_id)
                && asset.status == AssetStatus::Available
                && let Some(path) =
                    resolve_existing_asset_path(&asset.path, self.project_dir.as_deref())
                && let Ok(dec) = open_video_decoder(&path, BackendPreference::Auto)
            {
                self.video_decoders.insert(asset_id.clone(), dec);
            }
            if let Some(dec) = self.video_decoders.get_mut(&asset_id) {
                #[allow(clippy::cast_precision_loss)]
                let target_sec = (src_idx as f64) / doc.fps.as_f64();
                if let Ok(Some(decoded)) = dec.decode_frame_at_seconds(target_sec) {
                    self.frame_cache.insert(key, Arc::new(decoded.buffer));
                }
            }
        }
    }

    fn lookup_frame(
        &mut self,
        asset_id: &AssetId,
        frame_idx: u64,
    ) -> Option<Arc<VideoFrameBuffer>> {
        let exact = FrameCacheKey::new(asset_id.clone(), frame_idx, 100);
        if let Some(hit) = self.frame_cache.get(&exact) {
            return Some(hit);
        }
        let still = FrameCacheKey::new(asset_id.clone(), 0, 100);
        self.frame_cache.get(&still)
    }
}

fn collect_video_source_frames(doc: &ProjectDocument, playhead: FrameIndex) -> Vec<(AssetId, u64)> {
    let mut out = Vec::new();
    for layer in &doc.layers {
        if !layer.visible {
            continue;
        }
        for clip in &layer.clips {
            let start = clip.timeline_start.get();
            let end = start.saturating_add(clip.duration.get());
            if playhead.get() < start || playhead.get() >= end {
                continue;
            }
            if let ClipKind::Video {
                asset_id,
                source_in,
                playback_rate,
                ..
            } = &clip.kind
            {
                let local = playhead.get().saturating_sub(start);
                #[allow(clippy::cast_precision_loss)]
                let local_sec = (local as f64) / doc.fps.as_f64();
                let media_sec = source_in.as_seconds_f64() + local_sec * playback_rate.as_f64();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let src_idx = (media_sec * doc.fps.as_f64()).max(0.0).round() as u64;
                out.push((asset_id.clone(), src_idx));
            }
        }
    }
    out
}

enum ExportCompositor {
    Gpu(Box<WgpuCompositor>),
    Cpu,
}

impl ExportCompositor {
    fn try_best() -> Self {
        match WgpuCompositor::try_new() {
            Ok(gpu) => Self::Gpu(Box::new(gpu)),
            Err(_) => Self::Cpu,
        }
    }

    fn render_scene(
        &mut self,
        scene: &RenderScene,
        out_width: u32,
        out_height: u32,
    ) -> Result<VideoFrameBuffer, CompositorError> {
        match self {
            Self::Gpu(gpu) => gpu.render_scene_to_bgra8(scene, out_width, out_height),
            Self::Cpu => Ok(render_scene_cpu_bgra8(scene, out_width, out_height)),
        }
    }
}

struct PluginChildSession {
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout_rx: mpsc::Receiver<Result<PluginToHostMessage, ProtocolError>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
    shm: Option<ShmHostRegion>,
    pending_video_acks: VecDeque<u32>,
    pending_audio_acks: VecDeque<u32>,
    video_in_flight: [bool; VIDEO_SLOT_COUNT as usize],
    audio_in_flight: [bool; AUDIO_SLOT_COUNT as usize],
    completed_outcome: Option<ExportOutcome>,
    last_plugin_frames: u64,
}

impl PluginChildSession {
    fn spawn(
        exe_path: &Path,
        extra_args: &[String],
        shm: ShmHostRegion,
        init_payload: ExportInitPayload,
    ) -> Result<Self, ExportError> {
        let mut cmd = Command::new(exe_path);
        sea_export_protocol::apply_background_process_flags(&mut cmd);
        let mut child = cmd
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExportError::Io(std::io::Error::other("missing plugin stdin pipe")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExportError::Io(std::io::Error::other("missing plugin stdout pipe")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ExportError::Io(std::io::Error::other("missing plugin stderr pipe")))?;

        let (stdout_tx, stdout_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_json_line::<_, PluginToHostMessage>(&mut reader) {
                    Ok(msg) => {
                        if stdout_tx.send(Ok(msg)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = stdout_tx.send(Err(err));
                        break;
                    }
                }
            }
        });

        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::clone(&stderr_lines);
        let stderr_thread = thread::spawn(move || {
            use std::io::BufRead as _;
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut guard) = stderr_buf.lock() {
                    if guard.len() >= 24 {
                        guard.remove(0);
                    }
                    guard.push(line);
                }
            }
        });

        let mut session = Self {
            child: Some(child),
            stdin: Some(BufWriter::new(stdin)),
            stdout_rx,
            stdout_thread: Some(stdout_thread),
            stderr_lines,
            stderr_thread: Some(stderr_thread),
            shm: Some(shm),
            pending_video_acks: VecDeque::new(),
            pending_audio_acks: VecDeque::new(),
            video_in_flight: [false; VIDEO_SLOT_COUNT as usize],
            audio_in_flight: [false; AUDIO_SLOT_COUNT as usize],
            completed_outcome: None,
            last_plugin_frames: 0,
        };

        session.send_cmd(&HostToPluginMessage::Init(Box::new(init_payload)))?;
        session.wait_until_ready()?;
        Ok(session)
    }

    fn stderr_summary(&self) -> String {
        self.stderr_lines
            .lock()
            .ok()
            .filter(|lines| !lines.is_empty())
            .map_or_else(|| "no stderr output".to_owned(), |lines| lines.join(" | "))
    }

    fn send_cmd(&mut self, msg: &HostToPluginMessage) -> Result<(), ExportError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(ExportError::Protocol {
                source: ProtocolError::UnexpectedEof,
                details: "plugin stdin already closed".to_owned(),
            });
        };
        write_json_line(stdin, msg).map_err(|source| ExportError::Protocol {
            source,
            details: self.stderr_summary(),
        })
    }

    fn wait_until_ready(&mut self) -> Result<(), ExportError> {
        let deadline = Instant::now() + SLOT_ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ExportError::PluginFailed {
                    code: "READY_TIMEOUT".to_owned(),
                    message: "timed out waiting for plugin Ready event".to_owned(),
                });
            }
            match self.stdout_rx.recv_timeout(remaining) {
                Ok(Ok(PluginToHostMessage::Ready { protocol_version })) => {
                    if protocol_version != EXPORT_PROTOCOL_VERSION {
                        return Err(ExportError::PluginFailed {
                            code: "PROTOCOL_MISMATCH".to_owned(),
                            message: format!(
                                "plugin protocol {protocol_version} != host {EXPORT_PROTOCOL_VERSION}"
                            ),
                        });
                    }
                    return Ok(());
                }
                Ok(Ok(PluginToHostMessage::Error { code, message })) => {
                    return Err(ExportError::PluginFailed { code, message });
                }
                Ok(Ok(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Ok(Err(source)) => {
                    return Err(ExportError::Protocol {
                        source,
                        details: self.stderr_summary(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ExportError::Protocol {
                        source: ProtocolError::UnexpectedEof,
                        details: self.stderr_summary(),
                    });
                }
            }
        }
    }

    fn handle_event(&mut self, evt: PluginToHostMessage) -> Result<(), ExportError> {
        match evt {
            PluginToHostMessage::Ready { .. } => Ok(()),
            PluginToHostMessage::SlotAck { kind, slot_index } => {
                match kind {
                    SlotKind::Video => {
                        if let Some(flag) = self.video_in_flight.get_mut(slot_index as usize) {
                            *flag = false;
                        }
                        self.pending_video_acks.push_back(slot_index);
                    }
                    SlotKind::Audio => {
                        if let Some(flag) = self.audio_in_flight.get_mut(slot_index as usize) {
                            *flag = false;
                        }
                        self.pending_audio_acks.push_back(slot_index);
                    }
                }
                Ok(())
            }
            PluginToHostMessage::Progress { frames_encoded, .. } => {
                self.last_plugin_frames = frames_encoded;
                Ok(())
            }
            PluginToHostMessage::Completed {
                output_path,
                file_size_bytes,
                frames_encoded,
            } => {
                self.completed_outcome = Some(ExportOutcome {
                    output_path: PathBuf::from(output_path),
                    file_size_bytes,
                    frames_encoded,
                });
                Ok(())
            }
            PluginToHostMessage::Error { code, message } => {
                Err(ExportError::PluginFailed { code, message })
            }
        }
    }

    fn drain_nonblocking(&mut self) -> Result<(), ExportError> {
        loop {
            match self.stdout_rx.try_recv() {
                Ok(Ok(evt)) => self.handle_event(evt)?,
                Ok(Err(source)) => {
                    return Err(ExportError::Protocol {
                        source,
                        details: self.stderr_summary(),
                    });
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(ExportError::Protocol {
                        source: ProtocolError::UnexpectedEof,
                        details: self.stderr_summary(),
                    });
                }
            }
        }
    }

    fn wait_slot_free(
        &mut self,
        kind: SlotKind,
        slot_index: u32,
        cancel: &ExportCancelToken,
    ) -> Result<(), ExportError> {
        let deadline = Instant::now() + SLOT_ACK_TIMEOUT;
        loop {
            if cancel.is_cancelled() {
                return Err(ExportError::Cancelled);
            }
            let busy = match kind {
                SlotKind::Video => self.video_in_flight[slot_index as usize],
                SlotKind::Audio => self.audio_in_flight[slot_index as usize],
            };
            if !busy {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ExportError::PluginFailed {
                    code: "SLOT_ACK_TIMEOUT".to_owned(),
                    message: format!("timed out waiting for slot ack {kind:?} #{slot_index}"),
                });
            }
            match self.stdout_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(evt)) => self.handle_event(evt)?,
                Ok(Err(source)) => {
                    return Err(ExportError::Protocol {
                        source,
                        details: self.stderr_summary(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ExportError::Protocol {
                        source: ProtocolError::UnexpectedEof,
                        details: self.stderr_summary(),
                    });
                }
            }
        }
    }

    fn wait_completion(
        &mut self,
        cancel: &ExportCancelToken,
    ) -> Result<ExportOutcome, ExportError> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(outcome) = self.completed_outcome.take() {
                return Ok(outcome);
            }
            if cancel.is_cancelled() {
                return Err(ExportError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(ExportError::PluginFailed {
                    code: "FINALIZE_TIMEOUT".to_owned(),
                    message: "timed out waiting for plugin Completed event".to_owned(),
                });
            }
            match self.stdout_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(evt)) => self.handle_event(evt)?,
                Ok(Err(source)) => {
                    if let Some(outcome) = self.completed_outcome.take() {
                        return Ok(outcome);
                    }
                    return Err(ExportError::Protocol {
                        source,
                        details: self.stderr_summary(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(outcome) = self.completed_outcome.take() {
                        return Ok(outcome);
                    }
                    return Err(ExportError::Protocol {
                        source: ProtocolError::UnexpectedEof,
                        details: self.stderr_summary(),
                    });
                }
            }
        }
    }

    fn submit_video_frame(
        &mut self,
        slot_index: u32,
        frame_index: u64,
        pixels: &[u8],
        cancel: &ExportCancelToken,
    ) -> Result<(), ExportError> {
        self.wait_slot_free(SlotKind::Video, slot_index, cancel)?;
        let shm = self.shm.as_mut().ok_or_else(|| {
            ExportError::Shm(ShmError::InvalidLayout(
                "host shm already closed".to_owned(),
            ))
        })?;
        shm.write_video_slot(slot_index, pixels)?;
        self.video_in_flight[slot_index as usize] = true;
        self.send_cmd(&HostToPluginMessage::ProcessVideoSlot {
            slot_index,
            frame_index,
            data_len_bytes: u64::try_from(pixels.len()).unwrap_or(0),
        })?;
        self.drain_nonblocking()
    }

    fn submit_audio_chunk(
        &mut self,
        slot_index: u32,
        start_sample: u64,
        samples: &[f32],
        channels: u16,
        cancel: &ExportCancelToken,
    ) -> Result<(), ExportError> {
        let ch = usize::from(channels.max(1));
        let sample_frames = u32::try_from(samples.len() / ch).unwrap_or(0);
        if sample_frames == 0 {
            return Ok(());
        }
        self.wait_slot_free(SlotKind::Audio, slot_index, cancel)?;
        let shm = self.shm.as_mut().ok_or_else(|| {
            ExportError::Shm(ShmError::InvalidLayout(
                "host shm already closed".to_owned(),
            ))
        })?;
        shm.write_audio_slot_f32(slot_index, samples)?;
        self.audio_in_flight[slot_index as usize] = true;
        self.send_cmd(&HostToPluginMessage::ProcessAudioSlot {
            slot_index,
            start_sample,
            sample_frame_count: sample_frames,
            data_len_bytes: u64::try_from(samples.len().saturating_mul(4)).unwrap_or(0),
        })?;
        self.drain_nonblocking()
    }

    fn shutdown(mut self, send_abort: bool) -> Result<(), std::io::Error> {
        if send_abort && let Some(mut stdin) = self.stdin.take() {
            let _ = write_json_line(&mut stdin, &HostToPluginMessage::Abort);
        } else {
            self.stdin.take();
        }
        let mut status_ok = true;
        if let Some(mut child) = self.child.take() {
            let wait_deadline = Instant::now() + Duration::from_secs(2);
            let mut exited = false;
            while Instant::now() < wait_deadline {
                if let Ok(Some(status)) = child.try_wait() {
                    status_ok = status.success();
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if !exited {
                let _ = child.kill();
                let _ = child.wait();
                status_ok = false;
            }
        }
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        if let Some(shm) = self.shm.take() {
            shm.cleanup();
        }
        if send_abort || status_ok {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "plugin process exited with non-zero status ({})",
                self.stderr_summary()
            )))
        }
    }
}

impl Drop for PluginChildSession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        if let Some(shm) = self.shm.take() {
            shm.cleanup();
        }
    }
}

/// タイムライン映像・音声をプラグイン別プロセスへ送り出し、成果ファイルを生成する。
///
/// # Errors
///
/// プラグイン不在、設定不備、IPC 異常、中断、出力エラー時に [`ExportError`] を返す。
pub fn run_export_job(
    doc: &ProjectDocument,
    registry: &PluginRegistry,
    request: &ExportJobRequest,
    sources: &mut ExportMediaSources,
    cancel: &ExportCancelToken,
    mut on_progress: impl FnMut(&ExportProgress),
) -> Result<ExportOutcome, ExportError> {
    validate_export_request(request)?;
    let (plugin, exe_path) = registry.resolve_runnable(&request.plugin_id, &request.preset_id)?;
    let total_frames = request
        .end_frame
        .get()
        .saturating_sub(request.start_frame.get());
    let started = Instant::now();

    emit_progress(
        &mut on_progress,
        ExportPhase::Preparing,
        0,
        total_frames,
        started,
        "メディア・共有メモリを準備しています...",
    );

    sources.preload_document_assets(doc, request.sample_rate);
    if let Some(parent) = request.output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let target_pixel_fmt = plugin.manifest.capabilities.pixel_format;
    let shm_layout = ShmLayout::from_spec(ShmLayoutSpec {
        width: request.width,
        height: request.height,
        pixel_bytes: target_pixel_fmt.bytes_per_pixel(),
        video_slot_count: VIDEO_SLOT_COUNT,
        audio_channels: AUDIO_CHANNELS,
        audio_slot_capacity_frames: AUDIO_CHUNK_FRAMES,
        audio_slot_count: AUDIO_SLOT_COUNT,
    })?;
    let shm = ShmHostRegion::create_temp("sea-export", shm_layout)?;
    let shm_path_str = shm.path().to_string_lossy().into_owned();

    let init_payload = build_init_payload(doc, request, shm_path_str, shm_layout, target_pixel_fmt);
    let mut session =
        PluginChildSession::spawn(&exe_path, &plugin.manifest.args, shm, init_payload)?;

    let supports_audio = plugin.manifest.capabilities.supports_audio;
    let mut ctx = StreamRunCtx {
        doc,
        request,
        sources,
        session: &mut session,
        pixel_format: target_pixel_fmt,
        supports_audio,
        cancel,
        started,
    };
    let result = stream_timeline_to_plugin(&mut ctx, &mut on_progress);

    match result {
        Ok(outcome) => {
            session.shutdown(false)?;
            emit_progress(
                &mut on_progress,
                ExportPhase::Completed,
                total_frames,
                total_frames,
                started,
                "書き出しが完了しました",
            );
            Ok(outcome)
        }
        Err(err) => {
            let is_cancelled = matches!(err, ExportError::Cancelled);
            let _ = session.shutdown(is_cancelled);
            if request.output_path.exists() {
                let _ = fs::remove_file(&request.output_path);
            }
            let phase = if is_cancelled {
                ExportPhase::Cancelled
            } else {
                ExportPhase::Failed
            };
            emit_progress(
                &mut on_progress,
                phase,
                0,
                total_frames,
                started,
                &err.to_string(),
            );
            Err(err)
        }
    }
}

fn validate_export_request(request: &ExportJobRequest) -> Result<(), ExportError> {
    if request.width == 0 || request.height == 0 {
        return Err(ExportError::InvalidRequest(
            "width and height must be non-zero".to_owned(),
        ));
    }
    if !request.width.is_multiple_of(2) || !request.height.is_multiple_of(2) {
        return Err(ExportError::InvalidRequest(
            "width and height must be even numbers".to_owned(),
        ));
    }
    if request.end_frame <= request.start_frame {
        return Err(ExportError::InvalidRequest(
            "end_frame must be greater than start_frame".to_owned(),
        ));
    }
    if request.sample_rate == 0 {
        return Err(ExportError::InvalidRequest(
            "sample_rate must be positive".to_owned(),
        ));
    }
    if request.output_path.as_os_str().is_empty() {
        return Err(ExportError::InvalidRequest(
            "output_path must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn build_init_payload(
    doc: &ProjectDocument,
    request: &ExportJobRequest,
    shm_path: String,
    shm_layout: ShmLayout,
    pixel_format: PixelFormat,
) -> ExportInitPayload {
    ExportInitPayload {
        protocol_version: EXPORT_PROTOCOL_VERSION,
        width: request.width,
        height: request.height,
        fps_num: doc.fps.numerator(),
        fps_den: doc.fps.denominator(),
        total_video_frames: request
            .end_frame
            .get()
            .saturating_sub(request.start_frame.get()),
        sample_rate: request.sample_rate,
        audio_channels: AUDIO_CHANNELS,
        pixel_format,
        audio_sample_format: AudioSampleFormat::F32Interleaved,
        shm_path,
        shm_layout,
        output_path: request.output_path.to_string_lossy().into_owned(),
        preset_id: request.preset_id.clone(),
        options: request.options.clone(),
    }
}

struct StreamRunCtx<'a> {
    doc: &'a ProjectDocument,
    request: &'a ExportJobRequest,
    sources: &'a mut ExportMediaSources,
    session: &'a mut PluginChildSession,
    pixel_format: PixelFormat,
    supports_audio: bool,
    cancel: &'a ExportCancelToken,
    started: Instant,
}

fn stream_timeline_to_plugin(
    ctx: &mut StreamRunCtx<'_>,
    on_progress: &mut impl FnMut(&ExportProgress),
) -> Result<ExportOutcome, ExportError> {
    let mut compositor = ExportCompositor::try_best();
    let total_frames = ctx
        .request
        .end_frame
        .get()
        .saturating_sub(ctx.request.start_frame.get());
    let audio_start_base = timeline_frame_to_sample(
        ctx.request.start_frame.get(),
        ctx.doc.fps.as_f64(),
        ctx.request.sample_rate,
    );
    let audio_end_target = timeline_frame_to_sample(
        ctx.request.end_frame.get(),
        ctx.doc.fps.as_f64(),
        ctx.request.sample_rate,
    );
    let mut audio_state = AudioFlushState {
        base_sample: audio_start_base,
        cursor_sample: audio_start_base,
        slot_cursor: 0,
        scratch: Vec::new(),
    };

    for offset in 0..total_frames {
        if ctx.cancel.is_cancelled() {
            return Err(ExportError::Cancelled);
        }
        let abs_frame = FrameIndex::new(ctx.request.start_frame.get().saturating_add(offset));
        ctx.sources.ensure_frames_for_playhead(ctx.doc, abs_frame);
        let scene = build_render_scene(ctx.doc, abs_frame, &mut |id, idx| {
            ctx.sources.lookup_frame(id, idx)
        });
        let mut rendered =
            compositor.render_scene(&scene, ctx.request.width, ctx.request.height)?;
        if ctx.pixel_format == PixelFormat::Rgba8 {
            convert_bgra_to_rgba_in_place(&mut rendered.data);
        }
        let slot_idx = u32::try_from(offset % u64::from(VIDEO_SLOT_COUNT)).unwrap_or(0);
        ctx.session
            .submit_video_frame(slot_idx, offset, &rendered.data, ctx.cancel)?;

        if ctx.supports_audio {
            let next_target = timeline_frame_to_sample(
                abs_frame.get().saturating_add(1),
                ctx.doc.fps.as_f64(),
                ctx.request.sample_rate,
            );
            flush_audio_until(ctx, &mut audio_state, next_target.min(audio_end_target))?;
        }

        emit_progress(
            on_progress,
            ExportPhase::Encoding,
            offset.saturating_add(1),
            total_frames,
            ctx.started,
            &format!("フレーム {}/{total_frames} エンコード中...", offset + 1),
        );
    }

    if ctx.supports_audio {
        flush_audio_until(ctx, &mut audio_state, audio_end_target)?;
    }

    emit_progress(
        on_progress,
        ExportPhase::Finalizing,
        total_frames,
        total_frames,
        ctx.started,
        "ファイルをファイナライズしています...",
    );
    ctx.session.send_cmd(&HostToPluginMessage::Finish)?;
    ctx.session.wait_completion(ctx.cancel)
}

struct AudioFlushState {
    base_sample: u64,
    cursor_sample: u64,
    slot_cursor: u32,
    scratch: Vec<f32>,
}

fn flush_audio_until(
    ctx: &mut StreamRunCtx<'_>,
    audio: &mut AudioFlushState,
    target_sample: u64,
) -> Result<(), ExportError> {
    while audio.cursor_sample < target_sample {
        let remaining = target_sample.saturating_sub(audio.cursor_sample);
        let chunk = usize::try_from(remaining.min(u64::from(AUDIO_CHUNK_FRAMES))).unwrap_or(0);
        if chunk == 0 {
            break;
        }
        ctx.sources.audio_mixer.mix_timeline_window(
            ctx.doc,
            audio.cursor_sample,
            chunk,
            ctx.request.sample_rate,
            &mut audio.scratch,
        );
        let slot_idx = audio.slot_cursor % AUDIO_SLOT_COUNT;
        audio.slot_cursor = audio.slot_cursor.wrapping_add(1);
        let rel_start_sample = audio.cursor_sample.saturating_sub(audio.base_sample);
        ctx.session.submit_audio_chunk(
            slot_idx,
            rel_start_sample,
            &audio.scratch,
            AUDIO_CHANNELS,
            ctx.cancel,
        )?;
        audio.cursor_sample = audio.cursor_sample.saturating_add(chunk as u64);
    }
    Ok(())
}

fn convert_bgra_to_rgba_in_place(bgra: &mut [u8]) {
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

/// バックグラウンドスレッドで実行中の非同期書き出しジョブハンドル。
pub struct ExportJobHandle {
    /// 進捗更新の受信チャネル。
    pub progress_rx: mpsc::Receiver<ExportProgress>,
    /// 最終完了／失敗結果の受信チャネル。
    pub result_rx: mpsc::Receiver<Result<ExportOutcome, String>>,
    /// キャンセル要求トークン。
    pub cancel_token: ExportCancelToken,
}

/// タイムラインスナップショットを別スレッドへ移譲し非同期で書き出しを開始する。
#[must_use]
pub fn spawn_export_job(
    doc: ProjectDocument,
    registry: PluginRegistry,
    request: ExportJobRequest,
    mut sources: ExportMediaSources,
) -> ExportJobHandle {
    let cancel_token = ExportCancelToken::new();
    let thread_cancel = cancel_token.clone();
    let (progress_tx, progress_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();

    thread::spawn(move || {
        let res = run_export_job(
            &doc,
            &registry,
            &request,
            &mut sources,
            &thread_cancel,
            |prog| {
                let _ = progress_tx.send(prog.clone());
            },
        )
        .map_err(|err| err.to_string());
        let _ = result_tx.send(res);
    });

    ExportJobHandle {
        progress_rx,
        result_rx,
        cancel_token,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn timeline_frame_to_sample(frame: u64, fps: f64, sample_rate: u32) -> u64 {
    if fps <= 0.0 {
        return 0;
    }
    (((frame as f64) / fps) * f64::from(sample_rate))
        .round()
        .max(0.0) as u64
}

fn emit_progress(
    cb: &mut impl FnMut(&ExportProgress),
    phase: ExportPhase,
    frames_completed: u64,
    total_frames: u64,
    started: Instant,
    message: &str,
) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    cb(&ExportProgress {
        phase,
        frames_completed,
        total_frames,
        elapsed_ms,
        message: message.to_owned(),
    });
}
