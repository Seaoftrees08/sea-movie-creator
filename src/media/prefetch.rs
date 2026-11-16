//! 再生中映像の非同期バックグラウンド先読みデコードワーカー。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::core::ids::AssetId;
use crate::media::backends::open_video_decoder;
use crate::media::decoder::{BackendPreference, VideoDecoder, VideoFrameBuffer};
use crate::media::frame_cache::{FrameCache, FrameCacheKey};

const MAX_PENDING_JOBS: usize = 48;
const MAX_COMPLETED_FRAMES: usize = 64;

/// ワーカースレッドへ投入する単一フレーム先読み要求。
#[derive(Debug, Clone)]
pub struct PrefetchRequest {
    /// キャッシュ格納用キー。
    pub key: FrameCacheKey,
    /// 実メディアファイルパス。
    pub path: PathBuf,
    /// デコード対象のソースメディア時刻（秒）。
    pub target_seconds: f64,
}

#[derive(Debug, Clone)]
struct InternalPrefetchJob {
    generation: u64,
    key: FrameCacheKey,
    path: PathBuf,
    target_seconds: f64,
}

struct PrefetchOutcome {
    generation: u64,
    key: FrameCacheKey,
    frame: Option<Arc<VideoFrameBuffer>>,
}

#[derive(Default)]
struct PrefetchSharedState {
    generation: u64,
    shutdown: bool,
    clear_decoders: bool,
    pending: Vec<InternalPrefetchJob>,
    completed: Vec<PrefetchOutcome>,
}

/// 映像デコードを専用スレッドへオフロードし、連続再生時の UI スレッドブロックを防ぐ先読みマネージャ。
pub struct VideoPrefetchWorker {
    generation: u64,
    queued_keys: HashMap<FrameCacheKey, u64>,
    shared: Arc<(Mutex<PrefetchSharedState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Default for VideoPrefetchWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoPrefetchWorker {
    /// 未起動（初回投入時に遅延起動）の空マネージャを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self {
            generation: 0,
            queued_keys: HashMap::new(),
            shared: Arc::new((Mutex::new(PrefetchSharedState::default()), Condvar::new())),
            thread: None,
        }
    }

    /// 現在のリクエスト世代を返し、先読みジョブが進行中かどうか確認する。
    #[must_use]
    pub fn is_pending(&self, key: &FrameCacheKey) -> bool {
        self.queued_keys.get(key).copied() == Some(self.generation)
    }

    /// 進行中の先読み世代を進め、未処理・完了済み結果を即座に破棄する。
    ///
    /// `reset_decoders` が `true` の場合はワーカースレッド側が保持するデコーダハンドルも破棄する。
    pub fn invalidate_generation(&mut self, reset_decoders: bool) {
        self.generation = self.generation.wrapping_add(1);
        self.queued_keys.clear();
        let (lock, cvar) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            state.generation = self.generation;
            state.pending.clear();
            state.completed.clear();
            if reset_decoders {
                state.clear_decoders = true;
            }
            cvar.notify_all();
        }
    }

    /// 先読み要求群をキューへ積む（同世代で既にキュー済みのキーは重複投入しない）。
    pub fn enqueue_batch(&mut self, requests: &[PrefetchRequest]) {
        if requests.is_empty() {
            return;
        }
        self.ensure_started();
        let (lock, cvar) = &*self.shared;
        let Ok(mut state) = lock.lock() else {
            return;
        };
        let mut added = false;
        for req in requests {
            if self.queued_keys.get(&req.key).copied() == Some(self.generation) {
                continue;
            }
            if state.pending.len() >= MAX_PENDING_JOBS {
                break;
            }
            self.queued_keys.insert(req.key.clone(), self.generation);
            state.pending.push(InternalPrefetchJob {
                generation: self.generation,
                key: req.key.clone(),
                path: req.path.clone(),
                target_seconds: req.target_seconds,
            });
            added = true;
        }
        if added {
            cvar.notify_one();
        }
    }

    /// 完了した先読みフレームを `FrameCache` へ格納し、回収済みリストを返す。
    pub fn drain_completed(
        &mut self,
        cache: &mut FrameCache,
    ) -> Vec<(AssetId, u64, Arc<VideoFrameBuffer>)> {
        let outcomes = {
            let (lock, _) = &*self.shared;
            let Ok(mut state) = lock.lock() else {
                return Vec::new();
            };
            if state.completed.is_empty() {
                return Vec::new();
            }
            std::mem::take(&mut state.completed)
        };

        let mut drained = Vec::new();
        for outcome in outcomes {
            if outcome.generation != self.generation {
                continue;
            }
            self.queued_keys.remove(&outcome.key);
            if let Some(buf) = outcome.frame {
                cache.insert(outcome.key.clone(), Arc::clone(&buf));
                drained.push((outcome.key.asset_id, outcome.key.frame_index, buf));
            }
        }
        drained
    }

    fn ensure_started(&mut self) {
        if self.thread.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        if let Ok(handle) = thread::Builder::new()
            .name("sea-video-prefetch".to_owned())
            .spawn(move || run_prefetch_worker_loop(&shared))
        {
            self.thread = Some(handle);
        }
    }
}

impl Drop for VideoPrefetchWorker {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            state.shutdown = true;
            state.pending.clear();
            cvar.notify_all();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn run_prefetch_worker_loop(shared: &(Mutex<PrefetchSharedState>, Condvar)) {
    let mut decoders: HashMap<AssetId, (PathBuf, Box<dyn VideoDecoder>)> = HashMap::new();
    let (lock, cvar) = shared;

    loop {
        let next_job = {
            let Ok(mut guard) = lock.lock() else {
                break;
            };
            while !guard.shutdown && !guard.clear_decoders && guard.pending.is_empty() {
                let Ok(wait_guard) = cvar.wait(guard) else {
                    return;
                };
                guard = wait_guard;
            }
            if guard.shutdown {
                break;
            }
            if guard.clear_decoders {
                decoders.clear();
                guard.clear_decoders = false;
            }
            pop_earliest_current_job(&mut guard)
        };

        let Some(job) = next_job else {
            continue;
        };

        let frame_opt = decode_job_with_cache(&mut decoders, &job);

        let Ok(mut guard) = lock.lock() else {
            break;
        };
        if guard.generation == job.generation && guard.completed.len() < MAX_COMPLETED_FRAMES {
            guard.completed.push(PrefetchOutcome {
                generation: job.generation,
                key: job.key,
                frame: frame_opt.map(Arc::new),
            });
        }
    }
}

fn pop_earliest_current_job(state: &mut PrefetchSharedState) -> Option<InternalPrefetchJob> {
    let active_gen = state.generation;
    state.pending.retain(|job| job.generation == active_gen);
    let (best_idx, _) = state.pending.iter().enumerate().min_by(|(_, a), (_, b)| {
        a.target_seconds
            .total_cmp(&b.target_seconds)
            .then_with(|| a.key.frame_index.cmp(&b.key.frame_index))
    })?;
    Some(state.pending.swap_remove(best_idx))
}

fn decode_job_with_cache(
    decoders: &mut HashMap<AssetId, (PathBuf, Box<dyn VideoDecoder>)>,
    job: &InternalPrefetchJob,
) -> Option<VideoFrameBuffer> {
    let needs_open = decoders
        .get(&job.key.asset_id)
        .is_none_or(|(cached_path, _)| cached_path != &job.path);
    if needs_open {
        let dec = open_video_decoder(&job.path, BackendPreference::Auto).ok()?;
        decoders.insert(job.key.asset_id.clone(), (job.path.clone(), dec));
    }
    let (_, decoder) = decoders.get_mut(&job.key.asset_id)?;
    decoder
        .decode_frame_at_seconds(job.target_seconds)
        .ok()
        .flatten()
        .map(|f| f.buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::frame_cache::DEFAULT_FRAME_CACHE_BUDGET_BYTES;

    #[test]
    fn invalidate_generation_drops_pending_jobs_and_tracks_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut worker = VideoPrefetchWorker::new();
        let asset_id = AssetId::new("missing_asset")?;
        let key0 = FrameCacheKey::new(asset_id.clone(), 0, 100);
        assert!(!worker.is_pending(&key0));

        worker.invalidate_generation(true);
        let key1 = FrameCacheKey::new(asset_id, 1, 100);
        worker.enqueue_batch(&[PrefetchRequest {
            key: key1.clone(),
            path: PathBuf::from("nonexistent_prefetch_test.mp4"),
            target_seconds: 0.033,
        }]);
        assert!(worker.is_pending(&key1));

        // 世代無効化により即座に破棄され pending ではなくなる。
        worker.invalidate_generation(false);
        assert!(!worker.is_pending(&key1));

        let mut cache = FrameCache::new(DEFAULT_FRAME_CACHE_BUDGET_BYTES);
        assert_eq!(worker.drain_completed(&mut cache).len(), 0);
        Ok(())
    }
}
