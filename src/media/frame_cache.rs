//! デコード済み映像フレームのメモリ予算付き LRU キャッシュ。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::core::ids::AssetId;

use super::decoder::VideoFrameBuffer;

/// デフォルトのフレームキャッシュ予算（512 MiB）。
pub const DEFAULT_FRAME_CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// フレームキャッシュの一意キー。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameCacheKey {
    /// アセット識別子。
    pub asset_id: AssetId,
    /// ソースメディア基準のフレーム番号。
    pub frame_index: u64,
    /// スケール識別タグ（`0` は原寸、その他はターゲット幅など）。
    pub scale_tag: u32,
}

impl FrameCacheKey {
    /// キーを生成する。
    #[must_use]
    pub const fn new(asset_id: AssetId, frame_index: u64, scale_tag: u32) -> Self {
        Self {
            asset_id,
            frame_index,
            scale_tag,
        }
    }
}

struct CacheEntry {
    frame: Arc<VideoFrameBuffer>,
    byte_size: usize,
    last_used_seq: u64,
}

/// 指定メモリ上限内でデコード済みフレームを保持する LRU キャッシュ。
pub struct FrameCache {
    budget_bytes: usize,
    used_bytes: usize,
    next_seq: u64,
    entries: HashMap<FrameCacheKey, CacheEntry>,
    /// `(last_used_seq, key)` で最古エントリを O(log N) で取り出すための順序索引。
    lru_order: BTreeMap<u64, FrameCacheKey>,
    hits: u64,
    misses: u64,
}

impl Default for FrameCache {
    fn default() -> Self {
        Self::new(DEFAULT_FRAME_CACHE_BUDGET_BYTES)
    }
}

impl FrameCache {
    /// メモリ予算（バイト）を指定して空のキャッシュを生成する。
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            used_bytes: 0,
            next_seq: 0,
            entries: HashMap::new(),
            lru_order: BTreeMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// キャッシュからフレームを取得し、ヒット時は LRU 順位を最新に更新する。
    pub fn get(&mut self, key: &FrameCacheKey) -> Option<Arc<VideoFrameBuffer>> {
        let new_seq = self.next_seq;
        let Some(entry) = self.entries.get_mut(key) else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };

        let old_seq = entry.last_used_seq;
        entry.last_used_seq = new_seq;
        let frame = Arc::clone(&entry.frame);

        self.next_seq = self.next_seq.saturating_add(1);
        self.lru_order.remove(&old_seq);
        self.lru_order.insert(new_seq, key.clone());
        self.hits = self.hits.saturating_add(1);
        Some(frame)
    }

    /// フレームをキャッシュに挿入し、予算を超える場合は古いエントリから破棄する。
    ///
    /// 単一フレームだけで `budget_bytes` を上回る場合は他のフレームをすべて追い出した上で
    /// そのフレームも保持しない（予算不変条件を厳密に維持する）。
    pub fn insert(&mut self, key: FrameCacheKey, frame: Arc<VideoFrameBuffer>) {
        let byte_size = frame.byte_len();
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(old.byte_size);
            self.lru_order.remove(&old.last_used_seq);
        }

        if byte_size > self.budget_bytes {
            return;
        }

        while self.used_bytes.saturating_add(byte_size) > self.budget_bytes {
            if !self.evict_oldest() {
                break;
            }
        }

        let seq = self.alloc_seq();
        self.used_bytes = self.used_bytes.saturating_add(byte_size);
        self.lru_order.insert(seq, key.clone());
        self.entries.insert(
            key,
            CacheEntry {
                frame,
                byte_size,
                last_used_seq: seq,
            },
        );
    }

    /// 指定アセットに紐づく全キャッシュを無効化する（メディア再リンク時などに使用）。
    pub fn invalidate_asset(&mut self, asset_id: &AssetId) {
        let keys: Vec<FrameCacheKey> = self
            .entries
            .keys()
            .filter(|k| &k.asset_id == asset_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(entry) = self.entries.remove(&key) {
                self.used_bytes = self.used_bytes.saturating_sub(entry.byte_size);
                self.lru_order.remove(&entry.last_used_seq);
            }
        }
    }

    /// すべてのエントリを破棄する。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru_order.clear();
        self.used_bytes = 0;
    }

    /// 現在使用中のバイト数。
    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// 設定されている上限予算（バイト）。
    #[must_use]
    pub const fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// 保持中のフレーム数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// キャッシュが空かどうか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 累積ヒット数。
    #[must_use]
    pub const fn hit_count(&self) -> u64 {
        self.hits
    }

    /// 累積ミス数。
    #[must_use]
    pub const fn miss_count(&self) -> u64 {
        self.misses
    }

    /// 現在キャッシュされている全キーとフレーム参照のスナップショットを取得する。
    #[must_use]
    pub fn snapshot_entries(&self) -> Vec<(FrameCacheKey, Arc<VideoFrameBuffer>)> {
        self.entries
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(&v.frame)))
            .collect()
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn evict_oldest(&mut self) -> bool {
        let Some((seq, key)) = self
            .lru_order
            .iter()
            .next()
            .map(|(&seq, key)| (seq, key.clone()))
        else {
            return false;
        };
        self.lru_order.remove(&seq);
        if let Some(removed) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(removed.byte_size);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::color::RgbaColor;

    fn dummy_frame(w: u32, h: u32) -> Arc<VideoFrameBuffer> {
        Arc::new(VideoFrameBuffer::solid_bgra8(w, h, RgbaColor::BLACK))
    }

    #[test]
    fn lru_eviction_and_refresh_respect_budget() -> Result<(), Box<dyn std::error::Error>> {
        // 1 フレーム = 4x4*4 = 64 bytes、予算 = 128 bytes (最大 2 フレーム)
        let asset = AssetId::new("asset-lru")?;
        let mut cache = FrameCache::new(128);
        let k0 = FrameCacheKey::new(asset.clone(), 0, 0);
        let k1 = FrameCacheKey::new(asset.clone(), 1, 0);
        let k2 = FrameCacheKey::new(asset, 2, 0);

        cache.insert(k0.clone(), dummy_frame(4, 4));
        cache.insert(k1.clone(), dummy_frame(4, 4));
        assert_eq!(cache.used_bytes(), 128);
        assert_eq!(cache.len(), 2);

        // k0 を参照して k1 を最古にする。
        assert!(cache.get(&k0).is_some());
        cache.insert(k2.clone(), dummy_frame(4, 4));

        assert_eq!(cache.used_bytes(), 128);
        assert!(cache.get(&k0).is_some());
        assert!(cache.get(&k2).is_some());
        assert!(cache.get(&k1).is_none());
        assert_eq!(cache.hit_count(), 3);
        assert_eq!(cache.miss_count(), 1);
        Ok(())
    }

    #[test]
    fn invalidate_asset_only_removes_matching_asset() -> Result<(), Box<dyn std::error::Error>> {
        let a1 = AssetId::new("asset-a1")?;
        let a2 = AssetId::new("asset-a2")?;
        let mut cache = FrameCache::new(1024);

        cache.insert(FrameCacheKey::new(a1.clone(), 0, 0), dummy_frame(4, 4));
        cache.insert(FrameCacheKey::new(a1.clone(), 1, 0), dummy_frame(4, 4));
        cache.insert(FrameCacheKey::new(a2.clone(), 0, 0), dummy_frame(4, 4));

        cache.invalidate_asset(&a1);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 64);
        assert!(cache.get(&FrameCacheKey::new(a2, 0, 0)).is_some());
        Ok(())
    }
}
