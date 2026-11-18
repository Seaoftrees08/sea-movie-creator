//! OS ネイティブ Media Foundation (`IMFSinkWriter`) を使った HW/OS 動画書き出しバックエンド。
//!
//! COM / Media Foundation FFI 呼び出しを本モジュール内に隔離し、プラグイン本体側での `unsafe` を不要にする。

#![allow(unsafe_code)]

/// Media Foundation シンクライターで出力する映像コーデック種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfVideoCodec {
    /// AVC / H.264 (`MFVideoFormat_H264`)。
    H264,
    /// HEVC / H.265 (`MFVideoFormat_HEVC`)。
    Hevc,
    /// AV1 (`MFVideoFormat_AV1`、OS / GPU ハードウェアエンコーダ MFT 利用)。
    Av1,
}

/// [`MfSinkWriter`] の初期化構成パラメータ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MfSinkConfig {
    /// 出力映像コーデック。
    pub video_codec: MfVideoCodec,
    /// 出力映像幅 (px)。
    pub width: u32,
    /// 出力映像高さ (px)。
    pub height: u32,
    /// フレームレート分子。
    pub fps_num: u32,
    /// フレームレート分母。
    pub fps_den: u32,
    /// 目標映像ビットレート (kbps)。
    pub bitrate_kbps: u32,
    /// 音声サンプルレート (Hz)。
    pub sample_rate: u32,
    /// 音声チャンネル数。
    pub audio_channels: u16,
    /// 音声トラック (`AAC`) を含めるか。
    pub enable_audio: bool,
    /// GPU / ハードウェア MFT を優先有効化するか (`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`)。
    pub prefer_hardware: bool,
}

/// Media Foundation AAC エンコーダが許容する音声仕様と入力値の整合性を検証する。
///
/// # Errors
///
/// サンプルレートが `44100` / `48000` 以外、またはチャンネル数が `1..=2` 外の場合にエラー文字列を返す。
pub fn validate_mf_audio_params(sample_rate: u32, audio_channels: u16) -> Result<(), String> {
    if sample_rate != 44_100 && sample_rate != 48_000 {
        return Err(format!(
            "OS Media Foundation AAC エンコーダは 44100 Hz / 48000 Hz のみ対応しています (入力: {sample_rate} Hz)"
        ));
    }
    if audio_channels == 0 || audio_channels > 2 {
        return Err(format!(
            "OS Media Foundation AAC エンコーダはモノラル (1ch) またはステレオ (2ch) のみ対応しています (入力: {audio_channels}ch)"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
mod imp {
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Media::MediaFoundation::{
        IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSinkWriter,
        MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
        MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
        MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
        MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
        MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING, MF_VERSION,
        MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes, MFCreateMediaType,
        MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio,
        MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_AV1,
        MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_RGB32, MFVideoInterlace_Progressive,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::{GUID, PCWSTR};

    use super::{MfSinkConfig, MfVideoCodec, validate_mf_audio_params};

    const HNS_PER_SECOND: i128 = 10_000_000;

    struct ComGuard {
        should_uninit: bool,
    }

    impl ComGuard {
        fn init() -> Result<Self, String> {
            // SAFETY: プラグインワーカースレッド上でマルチスレッドアパートメントとして COM を初期化する。
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_ok() {
                return Ok(Self {
                    should_uninit: true,
                });
            }
            if hr == RPC_E_CHANGED_MODE {
                return Ok(Self {
                    should_uninit: false,
                });
            }
            Err(format!(
                "COM の初期化 (CoInitializeEx) に失敗しました: {hr:?}"
            ))
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.should_uninit {
                // SAFETY: 同一スレッド上で `CoInitializeEx` が成功した場合のみ対で呼ぶ。
                unsafe { CoUninitialize() };
            }
        }
    }

    struct MfSessionGuard;

    impl MfSessionGuard {
        fn startup() -> Result<Self, String> {
            // SAFETY: COM 初期化済みプロセス上で Media Foundation プラットフォームを起動する。
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|err| {
                format!("Media Foundation 起動 (MFStartup) に失敗しました: {err}")
            })?;
            Ok(Self)
        }
    }

    impl Drop for MfSessionGuard {
        fn drop(&mut self) {
            // SAFETY: `MFStartup` 成功後に必ず 1 回 `MFShutdown` で参照カウンタを戻す。
            let _ = unsafe { MFShutdown() };
        }
    }

    /// Windows Media Foundation (`IMFSinkWriter`) を使った MP4 出力セッション。
    ///
    /// フィールド宣言順により、必ず `writer` を先に解放してから `MFShutdown` / `CoUninitialize` を行う。
    pub struct MfSinkWriter {
        writer: Option<IMFSinkWriter>,
        video_stream_index: u32,
        audio_stream_index: Option<u32>,
        config: MfSinkConfig,
        output_path: PathBuf,
        audio_frames_written: u64,
        _mf_session: MfSessionGuard,
        _com: ComGuard,
    }

    impl MfSinkWriter {
        /// 出力 MP4 ファイルと映像・音声ストリームを開き、`BeginWriting` を完了する。
        ///
        /// # Errors
        ///
        /// 解像度不正・非対応音声設定・OS エンコーダ MFT 不在・出力作成失敗時にエラー文字列を返す。
        pub fn open(output_path: &Path, config: MfSinkConfig) -> Result<Self, String> {
            validate_sink_config(&config)?;
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("出力フォルダを作成できません: {err}"))?;
            }

            let com = ComGuard::init()?;
            let mf_session = MfSessionGuard::startup()?;

            let writer = create_sink_writer(output_path, config.prefer_hardware)?;
            let video_stream_index = setup_video_stream(&writer, &config)?;
            let audio_stream_index = if config.enable_audio && config.audio_channels > 0 {
                validate_mf_audio_params(config.sample_rate, config.audio_channels)?;
                Some(setup_audio_stream(&writer, &config)?)
            } else {
                None
            };

            // SAFETY: すべてのストリームへ入出力メディア型を設定済み。
            unsafe { writer.BeginWriting() }.map_err(|err| {
                format!("IMFSinkWriter::BeginWriting に失敗しました (対応エンコーダ MFT 不在の可能性): {err}")
            })?;

            Ok(Self {
                writer: Some(writer),
                video_stream_index,
                audio_stream_index,
                config,
                output_path: output_path.to_path_buf(),
                audio_frames_written: 0,
                _mf_session: mf_session,
                _com: com,
            })
        }

        /// `BGRA8` 生フレームを `MFVideoFormat_RGB32` サンプルとしてエンコーダへ投入する。
        ///
        /// # Errors
        ///
        /// バッファ長不一致・サンプル作成失敗・`WriteSample` 失敗時にエラー文字列を返す。
        pub fn write_video_bgra8(&mut self, frame_index: u64, bgra: &[u8]) -> Result<(), String> {
            let expected_len = usize::try_from(self.config.width)
                .ok()
                .and_then(|w| {
                    usize::try_from(self.config.height)
                        .ok()
                        .and_then(|h| w.checked_mul(h))
                })
                .and_then(|px| px.checked_mul(4))
                .ok_or_else(|| "映像フレームバイト長計算がオーバーフローしました".to_owned())?;
            if bgra.len() != expected_len {
                return Err(format!(
                    "BGRA フレームサイズが一致しません: expected={expected_len}, actual={}",
                    bgra.len()
                ));
            }
            let writer = self
                .writer
                .as_ref()
                .ok_or_else(|| "SinkWriter は既に終了しています".to_owned())?;

            let fps_num = i128::from(self.config.fps_num.max(1));
            let fps_den = i128::from(self.config.fps_den.max(1));
            let pts_hns =
                i64::try_from((i128::from(frame_index) * HNS_PER_SECOND * fps_den) / fps_num)
                    .map_err(|_| "映像タイムスタンプがオーバーフローしました".to_owned())?;
            let dur_hns = i64::try_from((HNS_PER_SECOND * fps_den) / fps_num).unwrap_or(333_333);

            let sample = build_mf_sample(bgra, pts_hns, dur_hns)?;
            // SAFETY: 有効な映像ストリーム番号と初期化済み `IMFSample` を渡す。
            unsafe { writer.WriteSample(self.video_stream_index, &sample) }.map_err(|err| {
                format!("映像フレーム書き出しに失敗しました (frame={frame_index}): {err}")
            })
        }

        /// インターリーブ `f32` LE バイト列を 16bit PCM へ変換し音声トラックへ投入する。
        ///
        /// # Errors
        ///
        /// データ長不正・`WriteSample` 失敗時にエラー文字列を返す。
        pub fn write_audio_f32(
            &mut self,
            sample_frame_count: u32,
            raw_f32_le_bytes: &[u8],
        ) -> Result<(), String> {
            let Some(audio_stream_index) = self.audio_stream_index else {
                return Ok(());
            };
            if sample_frame_count == 0 || raw_f32_le_bytes.is_empty() {
                return Ok(());
            }
            let writer = self
                .writer
                .as_ref()
                .ok_or_else(|| "SinkWriter は既に終了しています".to_owned())?;

            let pcm_bytes = f32_le_bytes_to_i16_le_bytes(raw_f32_le_bytes);
            let sr = i128::from(self.config.sample_rate.max(1));
            let pts_hns =
                i64::try_from((i128::from(self.audio_frames_written) * HNS_PER_SECOND) / sr)
                    .map_err(|_| "音声タイムスタンプがオーバーフローしました".to_owned())?;
            let dur_hns =
                i64::try_from((i128::from(sample_frame_count) * HNS_PER_SECOND) / sr).unwrap_or(0);

            let sample = build_mf_sample(&pcm_bytes, pts_hns, dur_hns)?;
            // SAFETY: 有効な音声ストリーム番号と初期化済み `IMFSample` を渡す。
            unsafe { writer.WriteSample(audio_stream_index, &sample) }
                .map_err(|err| format!("音声フレーム書き出しに失敗しました: {err}"))?;
            self.audio_frames_written = self
                .audio_frames_written
                .saturating_add(u64::from(sample_frame_count));
            Ok(())
        }

        /// エンコーダキューをフラッシュし、MP4 コンテナヘッダを確定してファイルバイト数を返す。
        ///
        /// # Errors
        ///
        /// `IMFSinkWriter::Finalize` または出力サイズ取得に失敗した場合にエラー文字列を返す。
        pub fn finalize(mut self) -> Result<u64, String> {
            let writer = self
                .writer
                .take()
                .ok_or_else(|| "SinkWriter は既にファイナライズ済みです".to_owned())?;
            // SAFETY: `BeginWriting` 完了後の `IMFSinkWriter` を正常終了させる。
            let res = unsafe { writer.Finalize() };
            drop(writer);
            res.map_err(|err| format!("IMFSinkWriter::Finalize に失敗しました: {err}"))?;

            std::fs::metadata(&self.output_path)
                .map(|m| m.len())
                .map_err(|err| format!("書き出し済み MP4 サイズ取得に失敗しました: {err}"))
        }
    }

    fn validate_sink_config(config: &MfSinkConfig) -> Result<(), String> {
        if config.width < 16
            || config.height < 16
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(format!(
                "Media Foundation 書き出しは 16x16 以上かつ偶数の解像度を推奨・要求します (入力: {}x{})",
                config.width, config.height
            ));
        }
        if config.fps_num == 0 || config.fps_den == 0 {
            return Err("FPS の分子・分母は 1 以上である必要があります".to_owned());
        }
        Ok(())
    }

    fn create_sink_writer(
        output_path: &Path,
        prefer_hardware: bool,
    ) -> Result<IMFSinkWriter, String> {
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: 空の属性ストアを確保して SinkWriter オプションを設定する。
        unsafe { MFCreateAttributes(&raw mut attrs, 2) }
            .map_err(|err| format!("MFCreateAttributes に失敗しました: {err}"))?;
        let attrs =
            attrs.ok_or_else(|| "MFCreateAttributes が空ポインタを返しました".to_owned())?;

        (|| -> windows::core::Result<()> {
            // SAFETY: `attrs` は直前の `MFCreateAttributes` で確保した有効な COM オブジェクトであり、
            // 既知の属性 GUID 参照と `u32` 値を渡している。
            unsafe {
                attrs.SetUINT32(
                    &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                    u32::from(prefer_hardware),
                )?;
                attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            }
            Ok(())
        })()
        .map_err(|err| format!("SinkWriter 属性設定に失敗しました: {err}"))?;

        let mut wide_path: Vec<u16> = output_path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .collect();
        wide_path.push(0);

        // SAFETY: NULL 終端 UTF-16 パスと有効な `IMFAttributes` を渡してファイルシンクを開く。
        unsafe { MFCreateSinkWriterFromURL(PCWSTR(wide_path.as_ptr()), None, &attrs) }.map_err(
            |err| {
                format!(
                    "MFCreateSinkWriterFromURL に失敗しました ({}): {err}",
                    output_path.display()
                )
            },
        )
    }

    fn setup_video_stream(writer: &IMFSinkWriter, config: &MfSinkConfig) -> Result<u32, String> {
        let out_subtype = match config.video_codec {
            MfVideoCodec::H264 => MFVideoFormat_H264,
            MfVideoCodec::Hevc => MFVideoFormat_HEVC,
            MfVideoCodec::Av1 => MFVideoFormat_AV1,
        };
        let frame_size = pack_u64_pair(config.width, config.height);
        let frame_rate = pack_u64_pair(config.fps_num, config.fps_den);
        let aspect_ratio = pack_u64_pair(1, 1);

        // SAFETY: 出力側映像メディア型 (`H264` または `AV1`) を生成・設定する。
        let out_type = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("出力映像 IMFMediaType 作成失敗: {err}"))?;
        set_video_common_attrs(
            &out_type,
            &out_subtype,
            frame_size,
            frame_rate,
            aspect_ratio,
        )?;
        let bitrate_bps = config.bitrate_kbps.saturating_mul(1000).max(200_000);
        // SAFETY: ビットレート属性を設定する。
        unsafe { out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps) }
            .map_err(|err| format!("映像ビットレート設定失敗: {err}"))?;

        // SAFETY: 出力ストリームを追加する。
        let stream_index = unsafe { writer.AddStream(&out_type) }.map_err(|err| {
            format!(
                "IMFSinkWriter::AddStream (映像 {:?}) に失敗しました: {err}",
                config.video_codec
            )
        })?;

        // SAFETY: 入力側映像メディア型 (`MFVideoFormat_RGB32`, トップダウン正ストライド) を作成する。
        let in_type = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("入力映像 IMFMediaType 作成失敗: {err}"))?;
        set_video_common_attrs(
            &in_type,
            &MFVideoFormat_RGB32,
            frame_size,
            frame_rate,
            aspect_ratio,
        )?;
        let top_down_stride = config.width.saturating_mul(4);
        // SAFETY: `BGRA8` と同一レイアウトの正値ストライドを明示し上下反転を防ぐ。
        unsafe { in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, top_down_stride) }
            .map_err(|err| format!("映像ストライド設定失敗: {err}"))?;

        // SAFETY: 追加済み映像ストリームへ入力メディア型を接続する。
        unsafe { writer.SetInputMediaType(stream_index, &in_type, None) }.map_err(|err| {
            format!(
                "映像入力形式接続 (SetInputMediaType, codec={:?}) に失敗しました: {err}",
                config.video_codec
            )
        })?;

        Ok(stream_index)
    }

    fn set_video_common_attrs(
        media_type: &IMFMediaType,
        subtype: &GUID,
        frame_size: u64,
        frame_rate: u64,
        aspect_ratio: u64,
    ) -> Result<(), String> {
        (|| -> windows::core::Result<()> {
            // SAFETY: `media_type` と `subtype` は呼び出し元から渡された有効な COM/GUID 参照であり、
            // 映像フォーマット GUID とパック済み解像度・FPS の値を設定する。
            unsafe {
                media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                #[allow(clippy::cast_sign_loss)]
                media_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                media_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
                media_type.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
                media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, aspect_ratio)?;
            }
            Ok(())
        })()
        .map_err(|err| format!("映像メディア属性設定失敗: {err}"))
    }

    fn setup_audio_stream(writer: &IMFSinkWriter, config: &MfSinkConfig) -> Result<u32, String> {
        let channels = u32::from(config.audio_channels);
        let sample_rate = config.sample_rate;

        // SAFETY: 出力音声型 (`MFAudioFormat_AAC`) を生成する。
        let out_type = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("出力音声 IMFMediaType 作成失敗: {err}"))?;
        (|| -> windows::core::Result<()> {
            // SAFETY: `out_type` は直前の `MFCreateMediaType` で確保した有効な COM オブジェクトであり、
            // Windows AAC MFT が要求する 16bit / raw payload / ビットレート属性を設定する。
            unsafe {
                out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                out_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                out_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                out_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
                out_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
                out_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
                out_type.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            }
            Ok(())
        })()
        .map_err(|err| format!("出力音声 (AAC) 属性設定失敗: {err}"))?;

        // SAFETY: 音声出力ストリームを登録する。
        let stream_index = unsafe { writer.AddStream(&out_type) }
            .map_err(|err| format!("IMFSinkWriter::AddStream (AAC 音声) 失敗: {err}"))?;

        // SAFETY: 入力 PCM 16bit インターリーブ音声型を生成する。
        let in_type = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("入力音声 IMFMediaType 作成失敗: {err}"))?;
        let block_align = channels.saturating_mul(2);
        let avg_bytes_per_sec = sample_rate.saturating_mul(block_align);
        (|| -> windows::core::Result<()> {
            // SAFETY: `in_type` は直前の `MFCreateMediaType` で確保した有効な COM オブジェクトであり、
            // 16bit signed PCM 入力属性を設定する。
            unsafe {
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                in_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                in_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
                in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
                in_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
                in_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes_per_sec)?;
            }
            Ok(())
        })()
        .map_err(|err| format!("入力音声 (PCM) 属性設定失敗: {err}"))?;

        // SAFETY: 音声ストリームへ入力 PCM 形式を接続する。
        unsafe { writer.SetInputMediaType(stream_index, &in_type, None) }
            .map_err(|err| format!("音声入力形式接続 (SetInputMediaType) 失敗: {err}"))?;

        Ok(stream_index)
    }

    fn build_mf_sample(bytes: &[u8], pts_hns: i64, dur_hns: i64) -> Result<IMFSample, String> {
        let len_u32 = u32::try_from(bytes.len())
            .map_err(|_| "Media Foundation バッファ長が u32 を超えました".to_owned())?;
        // SAFETY: `len_u32` バイトの連続 `IMFMediaBuffer` を確保する。
        let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(len_u32) }
            .map_err(|err| format!("MFCreateMemoryBuffer 失敗: {err}"))?;

        let mut locked_ptr: *mut u8 = std::ptr::null_mut();
        // SAFETY: ロック後に `bytes.len()` 分だけコピーし、結果に関わらず `Unlock` する。
        unsafe { buffer.Lock(&raw mut locked_ptr, None, None) }
            .map_err(|err| format!("IMFMediaBuffer::Lock 失敗: {err}"))?;
        if locked_ptr.is_null() {
            // SAFETY: ロック解除。
            let _ = unsafe { buffer.Unlock() };
            return Err("IMFMediaBuffer::Lock が NULL ポインタを返しました".to_owned());
        }
        // SAFETY: `MFCreateMemoryBuffer(len_u32)` により `len_u32` バイト以上が確保されている。
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), locked_ptr, bytes.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(len_u32)
                .map_err(|err| format!("IMFMediaBuffer::SetCurrentLength 失敗: {err}"))?;
        }

        // SAFETY: `IMFSample` を作成し、バッファ・時刻・継続時間を割り当てる。
        let sample =
            unsafe { MFCreateSample() }.map_err(|err| format!("MFCreateSample 失敗: {err}"))?;
        (|| -> windows::core::Result<()> {
            // SAFETY: `sample` と `buffer` はそれぞれ直前の `MFCreateSample` / `MFCreateMemoryBuffer` で
            // 確保した有効な COM オブジェクトであり、バッファと時刻・継続時間を割り当てる。
            unsafe {
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(pts_hns)?;
                sample.SetSampleDuration(dur_hns)?;
            }
            Ok(())
        })()
        .map_err(|err| format!("IMFSample 設定失敗: {err}"))?;

        Ok(sample)
    }

    fn f32_le_bytes_to_i16_le_bytes(raw_f32_le_bytes: &[u8]) -> Vec<u8> {
        let chunks = raw_f32_le_bytes.as_chunks::<4>().0;
        let mut out = Vec::with_capacity(chunks.len() * 2);
        for chunk in chunks {
            let sample = f32::from_le_bytes(*chunk);
            let sanitized = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            #[allow(clippy::cast_possible_truncation)]
            let pcm_i16 = (sanitized * 32767.0).round() as i16;
            out.extend_from_slice(&pcm_i16.to_le_bytes());
        }
        out
    }

    #[inline]
    fn pack_u64_pair(high: u32, low: u32) -> u64 {
        (u64::from(high) << 32) | u64::from(low)
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::path::Path;

    use super::MfSinkConfig;

    /// 非 Windows 環境向けスタブ `MfSinkWriter`（コンパイル・型互換維持用）。
    pub struct MfSinkWriter;

    impl MfSinkWriter {
        /// 非 Windows 環境では利用不可エラーを返す。
        ///
        /// # Errors
        ///
        /// 常に非対応エラー文字列を返す。
        pub fn open(_output_path: &Path, _config: MfSinkConfig) -> Result<Self, String> {
            Err("Windows Media Foundation は Windows 環境でのみ利用可能です".to_owned())
        }

        /// 非 Windows 環境では呼び出されないスタブメソッド。
        ///
        /// # Errors
        ///
        /// 常に非対応エラー文字列を返す。
        pub fn write_video_bgra8(&mut self, _frame_index: u64, _bgra: &[u8]) -> Result<(), String> {
            Err("Windows Media Foundation は Windows 環境でのみ利用可能です".to_owned())
        }

        /// 非 Windows 環境では呼び出されないスタブメソッド。
        ///
        /// # Errors
        ///
        /// 常に非対応エラー文字列を返す。
        pub fn write_audio_f32(
            &mut self,
            _sample_frame_count: u32,
            _raw_f32_le_bytes: &[u8],
        ) -> Result<(), String> {
            Err("Windows Media Foundation は Windows 環境でのみ利用可能です".to_owned())
        }

        /// 非 Windows 環境では呼び出されないスタブメソッド。
        ///
        /// # Errors
        ///
        /// 常に非対応エラー文字列を返す。
        pub fn finalize(self) -> Result<u64, String> {
            Err("Windows Media Foundation は Windows 環境でのみ利用可能です".to_owned())
        }
    }
}

pub use imp::MfSinkWriter;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_validation_rejects_unsupported_sample_rate_and_channels() {
        assert!(validate_mf_audio_params(48_000, 2).is_ok());
        assert!(validate_mf_audio_params(44_100, 1).is_ok());
        assert!(validate_mf_audio_params(22_050, 2).is_err());
        assert!(validate_mf_audio_params(48_000, 0).is_err());
        assert!(validate_mf_audio_params(48_000, 8).is_err());
    }

    #[test]
    fn odd_or_tiny_resolution_fails_gracefully() {
        let cfg = MfSinkConfig {
            video_codec: MfVideoCodec::H264,
            width: 8,
            height: 8,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 2000,
            sample_rate: 48_000,
            audio_channels: 2,
            enable_audio: true,
            prefer_hardware: true,
        };
        let out = std::env::temp_dir().join("sea-mf-invalid-dim-test.mp4");
        assert!(MfSinkWriter::open(&out, cfg).is_err());
        assert!(!out.exists());
    }
}
