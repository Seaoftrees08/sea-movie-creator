//! `memmap2` を用いたホスト〜プラグイン間フレーム／音声共有メモリ領域管理。
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{Mmap, MmapMut, MmapOptions};
use thiserror::Error;

use crate::ShmLayout;

static SHM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// 共有メモリ確保および読み書き時に発生するエラー。
#[derive(Debug, Error)]
pub enum ShmError {
    /// 共有メモリファイルの作成・オープン・拡張に失敗した。
    #[error("shared memory file I/O failed: {0}")]
    Io(#[from] io::Error),
    /// レイアウトサイズが不正（0 バイトや `usize` 変換オーバーフロー等）である。
    #[error("invalid shared memory layout size: {0}")]
    InvalidLayout(String),
    /// 指定されたスロット番号またはデータ長が確保領域を超過している。
    #[error("shared memory slot out of bounds: {0}")]
    OutOfBounds(String),
}

/// ホストプロセス側が所有・書き込みを行う共有メモリ領域。
#[derive(Debug)]
pub struct ShmHostRegion {
    path: PathBuf,
    layout: ShmLayout,
    mmap: Option<MmapMut>,
    remove_on_drop: bool,
}

impl ShmHostRegion {
    /// 一時ディレクトリ上に共有メモリ用バッキングファイルを確保し、書き込み可能にマップする。
    ///
    /// # Errors
    ///
    /// ファイル作成・サイズ拡張・メモリマップに失敗した場合 [`ShmError`] を返す。
    pub fn create_temp(prefix: &str, layout: ShmLayout) -> Result<Self, ShmError> {
        let total_len = usize::try_from(layout.total_bytes)
            .map_err(|_| ShmError::InvalidLayout("total_bytes overflows usize".to_owned()))?;
        if total_len == 0 {
            return Err(ShmError::InvalidLayout(
                "shared memory size must be positive".to_owned(),
            ));
        }

        let seq = SHM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let file_name = format!("{prefix}-{pid}-{seq}.shm");
        let path = std::env::temp_dir().join(file_name);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(layout.total_bytes)?;

        // SAFETY:
        // 1. 同一スレッド上で `file.set_len(layout.total_bytes)` により拡張完了後にマップする。
        // 2. ホスト側は排他スロット番号ごとにのみ書き込み、別プロセスは `SlotAck` による
        //    完了通知と直列化された `Process*Slot` 指令に従ってしか対象スロットへ触れないため
        //    データ競合は起きない。
        // 3. 当該一時ファイルは外部プログラムによってマップ中に切り詰められない。
        let mmap = unsafe { MmapOptions::new().len(total_len).map_mut(&file)? };

        Ok(Self {
            path,
            layout,
            mmap: Some(mmap),
            remove_on_drop: true,
        })
    }

    /// 共有メモリバッキングファイルのパスを返す。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 計算済みレイアウト情報を返す。
    #[must_use]
    pub const fn layout(&self) -> &ShmLayout {
        &self.layout
    }

    /// 指定された映像スロットへ BGRA8 / RGBA8 ピクセル列を書き込む。
    ///
    /// # Errors
    ///
    /// スロット番号またはバイト長がスロット容量を超える場合 [`ShmError::OutOfBounds`] を返す。
    pub fn write_video_slot(&mut self, slot_index: u32, data: &[u8]) -> Result<(), ShmError> {
        let range = self.layout.video_slot_byte_range(slot_index, data.len())?;
        let mmap = self.mmap.as_mut().ok_or_else(|| {
            ShmError::InvalidLayout("host shared memory already closed".to_owned())
        })?;
        mmap[range].copy_from_slice(data);
        Ok(())
    }

    /// 指定された音声スロットへインターリーブ済みバイト列（`f32` または `i16`）を書き込む。
    ///
    /// # Errors
    ///
    /// スロット番号またはバイト長がスロット容量を超える場合 [`ShmError::OutOfBounds`] を返す。
    pub fn write_audio_slot_bytes(&mut self, slot_index: u32, data: &[u8]) -> Result<(), ShmError> {
        let range = self.layout.audio_slot_byte_range(slot_index, data.len())?;
        let mmap = self.mmap.as_mut().ok_or_else(|| {
            ShmError::InvalidLayout("host shared memory already closed".to_owned())
        })?;
        mmap[range].copy_from_slice(data);
        Ok(())
    }

    /// インターリーブ `f32` サンプルスライスをリトルエンディアンバイト列として書き込む。
    ///
    /// # Errors
    ///
    /// スロット番号またはサンプル数が容量を超える場合 [`ShmError::OutOfBounds`] を返す。
    pub fn write_audio_slot_f32(
        &mut self,
        slot_index: u32,
        samples: &[f32],
    ) -> Result<(), ShmError> {
        let byte_len = samples.len().saturating_mul(std::mem::size_of::<f32>());
        let range = self.layout.audio_slot_byte_range(slot_index, byte_len)?;
        let mmap = self.mmap.as_mut().ok_or_else(|| {
            ShmError::InvalidLayout("host shared memory already closed".to_owned())
        })?;
        for (dst, sample) in mmap[range]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(samples.iter().copied())
        {
            *dst = sample.to_le_bytes();
        }
        Ok(())
    }

    /// マッピングを明示解除したうえで一時ファイルを削除する（子プロセス終了後に呼ぶ）。
    pub fn cleanup(mut self) {
        self.mmap.take();
        let _ = std::fs::remove_file(&self.path);
        self.remove_on_drop = false;
    }
}

impl Drop for ShmHostRegion {
    fn drop(&mut self) {
        self.mmap.take();
        if self.remove_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// プラグイン子プロセス側が読み取り専用で参照する共有メモリ領域。
#[derive(Debug)]
pub struct ShmPluginRegion {
    layout: ShmLayout,
    mmap: Mmap,
}

impl ShmPluginRegion {
    /// ホストから指定された共有メモリファイルを開き、読み取り専用でマップする。
    ///
    /// # Errors
    ///
    /// ファイルオープン失敗、サイズ不一致、メモリマップ失敗時に [`ShmError`] を返す。
    pub fn open_readonly(path: &Path, layout: ShmLayout) -> Result<Self, ShmError> {
        let expected_len = usize::try_from(layout.total_bytes)
            .map_err(|_| ShmError::InvalidLayout("total_bytes overflows usize".to_owned()))?;
        if expected_len == 0 {
            return Err(ShmError::InvalidLayout(
                "shared memory size must be positive".to_owned(),
            ));
        }
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() < layout.total_bytes {
            return Err(ShmError::InvalidLayout(format!(
                "backing file smaller than expected: {} < {}",
                metadata.len(),
                layout.total_bytes
            )));
        }

        // SAFETY:
        // 1. バッキングファイルの物理長が `layout.total_bytes` 以上であることを直前に検証済み。
        // 2. 読み取り専用マッピング (`map`) であり、ホストプロセスが該当スロットへ書き込み完了後に
        //    `Process*Slot` 指令を受信してから `SlotAck` を返すまでの間のみそのスロットを読む。
        // 3. マップ中に他プロセスがファイルを縮小しない契約の下でホストと協調動作する。
        let mmap = unsafe { MmapOptions::new().len(expected_len).map(&file)? };

        Ok(Self { layout, mmap })
    }

    /// 指定映像スロットのバイトスライスを読み取る。
    ///
    /// # Errors
    ///
    /// 範囲外アクセスの場合 [`ShmError::OutOfBounds`] を返す。
    pub fn read_video_slot(&self, slot_index: u32, byte_len: usize) -> Result<&[u8], ShmError> {
        let range = self.layout.video_slot_byte_range(slot_index, byte_len)?;
        Ok(&self.mmap[range])
    }

    /// 指定音声スロットのバイトスライスを読み取る。
    ///
    /// # Errors
    ///
    /// 範囲外アクセスの場合 [`ShmError::OutOfBounds`] を返す。
    pub fn read_audio_slot(&self, slot_index: u32, byte_len: usize) -> Result<&[u8], ShmError> {
        let range = self.layout.audio_slot_byte_range(slot_index, byte_len)?;
        Ok(&self.mmap[range])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ShmLayoutSpec;

    #[test]
    fn host_write_and_plugin_read_roundtrip_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let layout = ShmLayout::from_spec(ShmLayoutSpec {
            width: 4,
            height: 2,
            pixel_bytes: 4,
            video_slot_count: 2,
            audio_channels: 2,
            audio_slot_capacity_frames: 16,
            audio_slot_count: 2,
        })?;
        let mut host = ShmHostRegion::create_temp("sea-shm-test", layout)?;
        let path = host.path().to_path_buf();

        let frame_bytes = vec![0xAA_u8; 4 * 2 * 4];
        host.write_video_slot(1, &frame_bytes)?;
        let samples = vec![0.25_f32, -0.5_f32, 0.75_f32, -0.125_f32];
        host.write_audio_slot_f32(0, &samples)?;

        let plugin = ShmPluginRegion::open_readonly(&path, layout)?;
        assert_eq!(plugin.read_video_slot(1, frame_bytes.len())?, &frame_bytes);
        let audio_bytes = plugin.read_audio_slot(0, samples.len() * 4)?;
        let decoded: Vec<f32> = audio_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        assert_eq!(decoded, samples);

        assert!(host.write_video_slot(2, &frame_bytes).is_err());
        drop(plugin);
        host.cleanup();
        assert!(!path.exists());
        Ok(())
    }
}
