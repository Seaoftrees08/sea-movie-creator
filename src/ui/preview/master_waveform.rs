//! プレビュー下部マスター音声波形（`4kHz / 10ms ビン / 10秒チャンク / 最大16チャンク LRU / 非同期生成`）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::audio::mixer::AudioMixer;
use crate::timeline::document::ProjectDocument;

/// マスター波形用の低レートサンプルレート (Hz)。
pub const MASTER_WAVEFORM_SAMPLE_RATE: u32 = 4_000;
/// 1 ビンあたりの時間幅 (ms)。
pub const MASTER_WAVEFORM_BIN_MS: u64 = 10;
/// 1 チャンクあたりの秒数。
pub const MASTER_WAVEFORM_CHUNK_SECS: u64 = 10;
/// LRU キャッシュで保持する最大チャンク数。
pub const MASTER_WAVEFORM_MAX_CHUNKS: usize = 16;

/// 1 ビンあたりの 4kHz サンプルフレーム数 (`4000 * 10ms = 40`)。
#[must_use]
pub const fn samples_per_bin() -> usize {
    ((MASTER_WAVEFORM_SAMPLE_RATE as u64 * MASTER_WAVEFORM_BIN_MS) / 1_000) as usize
}

/// 1 チャンク（10 秒）あたりのビン数 (`10s / 10ms = 1000`)。
#[must_use]
pub const fn bins_per_chunk() -> usize {
    ((MASTER_WAVEFORM_CHUNK_SECS * 1_000) / MASTER_WAVEFORM_BIN_MS) as usize
}

/// 10 秒単位で計算済みのマスター波形チャンク。
#[derive(Debug, Clone, PartialEq)]
pub struct MasterWaveformChunk {
    /// このチャンクが属する `doc_audio_revision`。
    pub revision: u64,
    /// チャンク番号 (`0 = 0..10s`, `1 = 10..20s`, ...)。
    pub chunk_index: u64,
    /// 10ms ビンごとの正規化ピーク振幅 (`0.0..=1.0`)。
    pub peaks: Vec<f32>,
}

struct WorkerRequest {
    revision: u64,
    chunk_index: u64,
    document: Arc<ProjectDocument>,
    mixer: AudioMixer,
}

struct WorkerResponse {
    chunk: MasterWaveformChunk,
}

/// `AppController` が保持するマスター音声波形 LRU キャッシュ兼非同期ビルダー。
pub struct MasterWaveformCache {
    cached_revision: u64,
    chunks: HashMap<u64, MasterWaveformChunk>,
    lru_order: VecDeque<u64>,
    in_flight: HashSet<u64>,
    req_tx: Option<Sender<WorkerRequest>>,
    res_rx: Receiver<WorkerResponse>,
}

impl Default for MasterWaveformCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterWaveformCache {
    /// 専用バックグラウンドスレッド付きの空キャッシュを生成する。
    #[must_use]
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<WorkerRequest>();
        let (res_tx, res_rx) = mpsc::channel::<WorkerResponse>();
        let spawned = thread::Builder::new()
            .name("sea-master-waveform".to_owned())
            .spawn(move || run_worker_loop(&req_rx, &res_tx));
        Self {
            cached_revision: 0,
            chunks: HashMap::new(),
            lru_order: VecDeque::new(),
            in_flight: HashSet::new(),
            req_tx: spawned.ok().map(|_| req_tx),
            res_rx,
        }
    }

    /// ドキュメント音声リビジョン変更を検知し、古いキャッシュを破棄する。
    pub fn sync_revision(&mut self, revision: u64) {
        if self.cached_revision != revision {
            self.cached_revision = revision;
            self.chunks.clear();
            self.lru_order.clear();
            self.in_flight.clear();
        }
    }

    /// 計算完了済みチャンクを非ブロッキングで回収し、1 件でも更新があれば `true` を返す。
    pub fn poll_completed(&mut self) -> bool {
        let mut updated = false;
        while let Ok(msg) = self.res_rx.try_recv() {
            let chunk = msg.chunk;
            if chunk.revision != self.cached_revision {
                continue;
            }
            self.in_flight.remove(&chunk.chunk_index);
            self.insert_chunk_lru(chunk);
            updated = true;
        }
        updated
    }

    /// 指定チャンクがキャッシュにあれば参照を返し、LRU 順位を最新へ更新する。
    pub fn get_chunk(&mut self, chunk_index: u64) -> Option<&MasterWaveformChunk> {
        if self.chunks.contains_key(&chunk_index) {
            self.touch_lru(chunk_index);
        }
        self.chunks.get(&chunk_index)
    }

    /// キャッシュ済みチャンク数（最大 [`MASTER_WAVEFORM_MAX_CHUNKS`]）を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// キャッシュが空かどうかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// 現在のタイムライン総尺と再生位置に必要な最大 [`MASTER_WAVEFORM_MAX_CHUNKS`] チャンクを非同期キューへ投入する。
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn schedule_timeline_chunks(
        &mut self,
        revision: u64,
        doc: &ProjectDocument,
        mixer: &AudioMixer,
        playhead_sec: f64,
    ) {
        self.sync_revision(revision);
        let _ = self.poll_completed();

        let total_secs = (doc.total_duration_frames().max(1) as f64) / doc.fps.as_f64();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max_chunk = (total_secs / (MASTER_WAVEFORM_CHUNK_SECS as f64))
            .floor()
            .max(0.0) as u64;
        let indices = select_priority_chunk_indices(max_chunk, playhead_sec);
        let missing: Vec<u64> = indices
            .into_iter()
            .filter(|idx| !self.chunks.contains_key(idx) && !self.in_flight.contains(idx))
            .collect();
        if missing.is_empty() {
            return;
        }

        let Some(tx) = &self.req_tx else {
            return;
        };
        let doc_arc = Arc::new(doc.clone());
        let mixer_snap = mixer.snapshot_sources();
        for chunk_index in missing {
            self.in_flight.insert(chunk_index);
            if tx
                .send(WorkerRequest {
                    revision,
                    chunk_index,
                    document: Arc::clone(&doc_arc),
                    mixer: mixer_snap.snapshot_sources(),
                })
                .is_err()
            {
                self.in_flight.remove(&chunk_index);
                break;
            }
        }
    }

    /// テストや即時評価向けに指定チャンクを同期計算してキャッシュへ格納し、参照を返す。
    pub fn ensure_chunk_sync(
        &mut self,
        revision: u64,
        chunk_index: u64,
        doc: &ProjectDocument,
        mixer: &AudioMixer,
    ) -> &MasterWaveformChunk {
        self.sync_revision(revision);
        if self.chunks.contains_key(&chunk_index) {
            self.touch_lru(chunk_index);
        } else {
            let peaks = compute_master_waveform_chunk_peaks(doc, mixer, chunk_index);
            self.insert_chunk_lru(MasterWaveformChunk {
                revision,
                chunk_index,
                peaks,
            });
        }
        &self.chunks[&chunk_index]
    }

    fn insert_chunk_lru(&mut self, chunk: MasterWaveformChunk) {
        let idx = chunk.chunk_index;
        self.chunks.insert(idx, chunk);
        self.touch_lru(idx);
        while self.chunks.len() > MASTER_WAVEFORM_MAX_CHUNKS {
            if let Some(evicted) = self.lru_order.pop_front() {
                self.chunks.remove(&evicted);
            } else {
                break;
            }
        }
    }

    fn touch_lru(&mut self, chunk_index: u64) {
        self.lru_order.retain(|&c| c != chunk_index);
        self.lru_order.push_back(chunk_index);
    }
}

fn run_worker_loop(rx: &Receiver<WorkerRequest>, tx: &Sender<WorkerResponse>) {
    while let Ok(req) = rx.recv() {
        let peaks = compute_master_waveform_chunk_peaks(&req.document, &req.mixer, req.chunk_index);
        let chunk = MasterWaveformChunk {
            revision: req.revision,
            chunk_index: req.chunk_index,
            peaks,
        };
        if tx.send(WorkerResponse { chunk }).is_err() {
            break;
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn select_priority_chunk_indices(max_chunk: u64, playhead_sec: f64) -> Vec<u64> {
    if max_chunk < MASTER_WAVEFORM_MAX_CHUNKS as u64 {
        return (0..=max_chunk).collect();
    }
    let center = (playhead_sec / (MASTER_WAVEFORM_CHUNK_SECS as f64))
        .floor()
        .clamp(0.0, max_chunk as f64) as u64;
    let half = (MASTER_WAVEFORM_MAX_CHUNKS as u64) / 2;
    let start = center.saturating_sub(half);
    let end = (start + MASTER_WAVEFORM_MAX_CHUNKS as u64 - 1).min(max_chunk);
    let start_adj = end.saturating_sub(MASTER_WAVEFORM_MAX_CHUNKS as u64 - 1);
    (start_adj..=end).collect()
}

/// 1 チャンク (`chunk_index * 10s`) 分のマスターミキシングを実行し、`10ms` ビン配列 (`len = 1000`) を返す。
#[must_use]
pub fn compute_master_waveform_chunk_peaks(
    doc: &ProjectDocument,
    mixer: &AudioMixer,
    chunk_index: u64,
) -> Vec<f32> {
    let spb = samples_per_bin();
    let bin_count = bins_per_chunk();
    let output_frames = spb.saturating_mul(bin_count);
    let start_sample = chunk_index
        .saturating_mul(MASTER_WAVEFORM_CHUNK_SECS)
        .saturating_mul(u64::from(MASTER_WAVEFORM_SAMPLE_RATE));

    let mut stereo = Vec::new();
    mixer.mix_timeline_window(
        doc,
        start_sample,
        output_frames,
        MASTER_WAVEFORM_SAMPLE_RATE,
        &mut stereo,
    );

    let mut peaks = Vec::with_capacity(bin_count);
    for bin_idx in 0..bin_count {
        let base_frame = bin_idx * spb;
        let end_frame = base_frame + spb;
        let mut peak = 0.0_f32;
        for f in base_frame..end_frame {
            let i = f * 2;
            if i + 1 < stereo.len() {
                let sample_abs = stereo[i].abs().max(stereo[i + 1].abs());
                if sample_abs > peak {
                    peak = sample_abs;
                }
            }
        }
        peaks.push(peak.clamp(0.0, 1.0));
    }
    peaks
}

/// 現在のキャッシュ済みチャンクからタイムライン全幅に対応する棒グラフ高さ配列 (`0.0..=1.0`) をリサンプルする。
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn sample_waveform_bars_for_timeline(
    cache: &mut MasterWaveformCache,
    total_duration_frames: u64,
    fps_f64: f64,
    bar_count: usize,
) -> Vec<f32> {
    let count = bar_count.max(1);
    let total_secs = (total_duration_frames.max(1) as f64) / fps_f64.max(1.0);
    let bin_sec = (MASTER_WAVEFORM_BIN_MS as f64) / 1_000.0;
    let per_chunk = bins_per_chunk() as u64;
    let mut bars = Vec::with_capacity(count);

    for idx in 0..count {
        let t0 = (idx as f64 / count as f64) * total_secs;
        let t1 = ((idx + 1) as f64 / count as f64) * total_secs;
        let b0 = (t0 / bin_sec).floor().max(0.0) as u64;
        let b1 = ((t1 / bin_sec).ceil().max(0.0) as u64).max(b0 + 1);
        let mut peak = 0.0_f32;
        // 過剰サンプリングを抑えるため最大 16 ビン刻みでピーク抽出
        let step = ((b1 - b0) / 16).max(1);
        let mut bin = b0;
        while bin < b1 {
            let chunk_idx = bin / per_chunk;
            let in_chunk = (bin % per_chunk) as usize;
            if let Some(chunk) = cache.get_chunk(chunk_idx)
                && let Some(&v) = chunk.peaks.get(in_chunk)
                && v > peak
            {
                peak = v;
            }
            bin = bin.saturating_add(step);
        }
        bars.push(peak);
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::{AssetId, ClipId, LayerId};
    use crate::core::time::{FrameCount, FrameIndex, FrameRate, RationalDuration};
    use crate::media::decoder::DecodedAudioPcm;
    use crate::timeline::document::{
        Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind, Layer,
    };

    fn make_tone_doc(
        amplitude: f32,
    ) -> Result<(ProjectDocument, AudioMixer), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("wf-test");
        doc.fps = FrameRate::FPS_30;
        let asset_id = AssetId::new("tone-1")?;
        doc.assets.push(Asset {
            id: asset_id.clone(),
            name: "tone.wav".to_owned(),
            path: AssetPath::new(Some("tone.wav".to_owned()), None),
            kind: AssetKind::Audio {
                sample_rate_hz: 8_000,
                channels: 1,
                source_duration: RationalDuration::from_millis(2_000),
            },
            status: AssetStatus::Available,
        });
        let mut layer = Layer::new(LayerId::new("layer-a")?, "Audio");
        layer.clips.push(Clip {
            id: ClipId::new("clip-a")?,
            name: "Tone".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Audio {
                asset_id: asset_id.clone(),
                source_in: RationalDuration::ZERO,
                playback_rate: crate::core::time::PlaybackRate::NORMAL,
                preserve_pitch: true,
                audio: AudioProperties::default(),
            },
        });
        doc.layers.push(layer);

        let mut mixer = AudioMixer::new();
        mixer.register_pcm(
            asset_id,
            Arc::new(DecodedAudioPcm {
                sample_rate: 8_000,
                channels: 1,
                interleaved: vec![amplitude; 16_000],
            }),
        );
        Ok((doc, mixer))
    }

    #[test]
    fn master_waveform_chunk_computes_peaks_and_invalidates_on_revision()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, mixer) = make_tone_doc(0.6)?;
        let mut cache = MasterWaveformCache::new();
        let c0 = cache.ensure_chunk_sync(1, 0, &doc, &mixer);
        assert_eq!(c0.peaks.len(), 1_000);
        // 先頭 1.0 秒（100 ビン）あたりには約 0.6 の振幅が出る（パン法則適用後非ゼロ）
        assert!(c0.peaks[10] > 0.3);
        // クリップ終端 (2.0s = 200 ビン) 以降は無音
        assert!(c0.peaks[250] < 1e-4);

        // レイヤーをミュートして revision を上げると再計算され無音になる
        doc.layers[0].muted = true;
        let c0_muted = cache.ensure_chunk_sync(2, 0, &doc, &mixer);
        assert!(c0_muted.peaks[10] < 1e-4);
        Ok(())
    }

    #[test]
    fn lru_cache_evicts_oldest_beyond_16_chunks() -> Result<(), Box<dyn std::error::Error>> {
        let (doc, mixer) = make_tone_doc(0.2)?;
        let mut cache = MasterWaveformCache::new();
        for idx in 0..20_u64 {
            let _ = cache.ensure_chunk_sync(1, idx, &doc, &mixer);
        }
        assert_eq!(cache.len(), MASTER_WAVEFORM_MAX_CHUNKS);
        assert!(cache.get_chunk(0).is_none());
        assert!(cache.get_chunk(3).is_none());
        assert!(cache.get_chunk(19).is_some());
        Ok(())
    }
}
