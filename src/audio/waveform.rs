//! タイムライン波形描画用のマルチチャンネル最小・最大ピークキャッシュ。

use std::collections::HashMap;
use std::sync::Arc;

use crate::core::ids::AssetId;
use crate::media::decoder::DecodedAudioPcm;

/// 1 ビンあたりの音声波形ピーク値 (`[-1.0, 1.0]` に正規化済み)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveformPeakBin {
    /// ビン内の最小振幅。
    pub min: f32,
    /// ビン内の最大振幅。
    pub max: f32,
}

/// 特定ビン幅で事前計算された素材の波形ピーク列。
#[derive(Debug, Clone, PartialEq)]
pub struct WaveformPeaks {
    /// 対象アセット ID。
    pub asset_id: AssetId,
    /// 入力サンプルレート (Hz)。
    pub sample_rate: u32,
    /// 1 ピークビンに集約したサンプルフレーム数。
    pub samples_per_peak: usize,
    /// 元 PCM の総フレーム数。
    pub total_frames: usize,
    /// ピークビン列。
    pub bins: Vec<WaveformPeakBin>,
}

impl WaveformPeaks {
    /// PCM データから指定フレーム幅ごとの波形ピーク列を生成する。
    #[must_use]
    pub fn from_pcm(asset_id: AssetId, pcm: &DecodedAudioPcm, samples_per_peak: usize) -> Self {
        let step = samples_per_peak.max(1);
        let channels = usize::from(pcm.channels).max(1);
        let total_frames = pcm.frame_count();
        #[allow(clippy::cast_precision_loss)]
        let inv_ch = 1.0 / (channels as f32);

        let mut bins = Vec::with_capacity(total_frames.div_ceil(step));
        for bin_frames in pcm.interleaved.chunks(step.saturating_mul(channels)) {
            let mut min_v = f32::INFINITY;
            let mut max_v = f32::NEG_INFINITY;
            for frame in bin_frames.chunks_exact(channels) {
                let mono: f32 = frame.iter().copied().sum::<f32>() * inv_ch;
                if mono.is_finite() {
                    min_v = min_v.min(mono);
                    max_v = max_v.max(mono);
                }
            }
            if !min_v.is_finite() || !max_v.is_finite() {
                bins.push(WaveformPeakBin { min: 0.0, max: 0.0 });
            } else {
                bins.push(WaveformPeakBin {
                    min: min_v.clamp(-1.0, 1.0),
                    max: max_v.clamp(-1.0, 1.0),
                });
            }
        }

        Self {
            asset_id,
            sample_rate: pcm.sample_rate,
            samples_per_peak: step,
            total_frames,
            bins,
        }
    }
}

/// `(AssetId, samples_per_peak)` ごとに波形ピークを保持・再利用するキャッシュ。
#[derive(Debug, Default)]
pub struct WaveformCache {
    entries: HashMap<(AssetId, usize), Arc<WaveformPeaks>>,
}

impl WaveformCache {
    /// 空の波形キャッシュを生成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// キャッシュ済みピークがあれば返し、未生成なら PCM から計算・登録して返す。
    pub fn get_or_build(
        &mut self,
        asset_id: &AssetId,
        pcm: &DecodedAudioPcm,
        samples_per_peak: usize,
    ) -> Arc<WaveformPeaks> {
        let step = samples_per_peak.max(1);
        let key = (asset_id.clone(), step);
        if let Some(existing) = self.entries.get(&key) {
            return Arc::clone(existing);
        }
        let peaks = Arc::new(WaveformPeaks::from_pcm(asset_id.clone(), pcm, step));
        self.entries.insert(key, Arc::clone(&peaks));
        peaks
    }

    /// 指定アセットに関連する全解像度の波形キャッシュを破棄する。
    pub fn invalidate_asset(&mut self, asset_id: &AssetId) {
        self.entries.retain(|(id, _), _| id != asset_id);
    }

    /// 現在キャッシュされているエントリ数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// キャッシュが空かどうかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_cache_computes_accurate_peaks_and_invalidates_asset()
    -> Result<(), Box<dyn std::error::Error>> {
        let id = AssetId::new("a_wave")?;
        let pcm = DecodedAudioPcm {
            sample_rate: 48_000,
            channels: 2,
            interleaved: vec![
                0.25, 0.25, -0.5, -0.5, // bin 0 -> min -0.5, max 0.25
                0.8, 0.8, -0.1, -0.1, // bin 1 -> min -0.1, max 0.8
            ],
        };
        let mut cache = WaveformCache::new();
        let peaks = cache.get_or_build(&id, &pcm, 2);
        assert_eq!(peaks.bins.len(), 2);
        assert!((peaks.bins[0].min - (-0.5)).abs() < 1e-6);
        assert!((peaks.bins[0].max - 0.25).abs() < 1e-6);
        assert!((peaks.bins[1].max - 0.8).abs() < 1e-6);

        let cached = cache.get_or_build(&id, &pcm, 2);
        assert!(Arc::ptr_eq(&peaks, &cached));
        cache.invalidate_asset(&id);
        assert!(cache.is_empty());
        Ok(())
    }
}
