//! 純粋 Rust (`flacenc` + `webm-iterable`) による FLAC 音声エンコードおよび Matroska (`.mkv`) 多重化モジュール。

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder as FlacEncoderConfig;
use flacenc::error::Verify;
use flacenc::source::MemSource;
use webm_iterable::WebmWriter;
use webm_iterable::matroska_spec::{Master, MatroskaSpec, SimpleBlock};

/// クラスタを分割する最大時間間隔 (ms)。`SimpleBlock` の相対時刻 (`i16`) 超過を防ぐ。
const MAX_CLUSTER_SPAN_MS: u64 = 2000;

/// Matroska トラックへ書き込む 1 パケット分のデータとメタ情報。
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Matroska トラック番号 (`1`: 映像, `2`: 音声)。
    pub track_number: u64,
    /// 表示時刻 (ミリ秒単位、`TimestampScale = 1_000_000`)。
    pub pts_ms: u64,
    /// キーフレームであるか。
    pub is_keyframe: bool,
    /// トラック符号化後の生フレームバイト列。
    pub data: Vec<u8>,
}

/// FLAC エンコード済み音声トラック情報 (`A_FLAC`)。
#[derive(Debug, Clone)]
pub struct FlacAudioTrackData {
    /// サンプリング周波数 (Hz)。
    pub sample_rate: u32,
    /// チャンネル数。
    pub channels: u16,
    /// Matroska `CodecPrivate` に格納する 42 バイト (`"fLaC"` + `STREAMINFO`)。
    pub codec_private: Vec<u8>,
    /// エンコード済み FLAC フレーム群。
    pub blocks: Vec<EncodedPacket>,
}

/// MKV 書き出しに必要なトラックおよび時間情報。
#[derive(Debug, Clone)]
pub struct MkvTrackConfig {
    /// 出力映像幅 (px)。
    pub width: u32,
    /// 出力映像高さ (px)。
    pub height: u32,
    /// フレームレート分子。
    pub fps_num: u32,
    /// フレームレート分母。
    pub fps_den: u32,
    /// 総映像フレーム数。
    pub total_video_frames: u64,
    /// AV1 `CodecPrivate` (`av1C` 設定バイト列)。
    pub av1_codec_private: Vec<u8>,
    /// 音声トラック（音声データが存在しない場合は `None`）。
    pub audio: Option<FlacAudioTrackData>,
}

/// 16bit 整数化済みのインターリーブ PCM を純粋 Rust `flacenc` で FLAC エンコードする。
///
/// サンプル列が空の場合は `Ok(None)` を返し、音声トラック無しとして扱う。
///
/// # Errors
///
/// チャンネル数・サンプルレート不正、または FLAC エンコード／ビットストリーム直列化失敗時にエラーを返す。
pub fn encode_pcm_to_flac(
    samples_i32: &[i32],
    sample_rate: u32,
    channels: u16,
) -> Result<Option<FlacAudioTrackData>, String> {
    if samples_i32.is_empty() {
        return Ok(None);
    }
    if channels == 0 || channels > 8 || sample_rate == 0 {
        return Err(format!(
            "FLAC 非対応の音声パラメータです: sample_rate={sample_rate}, channels={channels}"
        ));
    }

    let config = FlacEncoderConfig::default()
        .into_verified()
        .map_err(|(_cfg, err)| format!("FLAC エンコーダ設定の検証に失敗しました: {err}"))?;

    let source = MemSource::from_samples(
        samples_i32,
        usize::from(channels),
        16,
        usize::try_from(sample_rate).map_err(|_| "サンプルレートが範囲外です".to_owned())?,
    );

    let flac_stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|err| format!("FLAC エンコードに失敗しました: {err}"))?;

    let codec_private = extract_flac_codec_private(&flac_stream)?;
    let blocks = collect_flac_frame_packets(&flac_stream, sample_rate)?;

    Ok(Some(FlacAudioTrackData {
        sample_rate,
        channels,
        codec_private,
        blocks,
    }))
}

fn extract_flac_codec_private(flac_stream: &flacenc::component::Stream) -> Result<Vec<u8>, String> {
    let mut header_sink = ByteSink::new();
    flac_stream
        .write(&mut header_sink)
        .map_err(|err| format!("FLAC ストリームヘッダ出力に失敗しました: {err:?}"))?;

    let bytes = header_sink.as_slice();
    if !bytes.starts_with(b"fLaC") || bytes.len() < 42 {
        return Err("生成された FLAC ストリームの STREAMINFO ヘッダ長が不足しています".to_owned());
    }
    Ok(bytes[..42].to_vec())
}

fn collect_flac_frame_packets(
    flac_stream: &flacenc::component::Stream,
    sample_rate: u32,
) -> Result<Vec<EncodedPacket>, String> {
    let mut packets = Vec::with_capacity(flac_stream.frame_count());
    let mut cumulative_samples = 0u64;
    let sr_u128 = u128::from(sample_rate.max(1));

    for idx in 0..flac_stream.frame_count() {
        let Some(frame) = flac_stream.frame(idx) else {
            continue;
        };
        let mut frame_sink = ByteSink::new();
        frame
            .write(&mut frame_sink)
            .map_err(|err| format!("FLAC フレーム書き出しに失敗しました (index={idx}): {err:?}"))?;

        let pts_ms_u128 = (u128::from(cumulative_samples) * 1000) / sr_u128;
        let pts_ms = u64::try_from(pts_ms_u128).unwrap_or(u64::MAX);
        cumulative_samples = cumulative_samples.saturating_add(frame.block_size() as u64);

        packets.push(EncodedPacket {
            track_number: 2,
            pts_ms,
            is_keyframe: true,
            data: frame_sink.into_inner(),
        });
    }
    Ok(packets)
}

/// エンコード済み映像・音声パケットを Matroska (`.mkv`) ファイルへ書き出し、総バイト数を返す。
///
/// # Errors
///
/// ファイル作成・コンテナタグ書き出し・フラッシュに失敗した場合にエラーを返す。
pub fn write_mkv_file(
    output_path: &Path,
    config: &MkvTrackConfig,
    video_packets: &[EncodedPacket],
) -> Result<u64, String> {
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("出力先フォルダを作成できません: {err}"))?;
    }

    let file = File::create(output_path).map_err(|err| {
        format!(
            "MKV 出力ファイルを作成できません ({}): {err}",
            output_path.display()
        )
    })?;
    let mut writer = WebmWriter::new(BufWriter::new(file));

    write_ebml_and_segment_header(&mut writer, config)?;
    write_tracks_section(&mut writer, config)?;
    write_interleaved_clusters(&mut writer, video_packets, config.audio.as_ref())?;

    writer
        .write(&MatroskaSpec::Segment(Master::End))
        .map_err(|err| format!("MKV セグメント終端書き出しに失敗しました: {err}"))?;
    drop(writer);

    let len = std::fs::metadata(output_path)
        .map_err(|err| format!("MKV ファイルサイズ取得に失敗しました: {err}"))?
        .len();
    Ok(len)
}

fn write_ebml_and_segment_header<W: std::io::Write>(
    writer: &mut WebmWriter<W>,
    config: &MkvTrackConfig,
) -> Result<(), String> {
    #[allow(clippy::cast_precision_loss)]
    let total_duration_ms = if config.fps_num == 0 {
        0.0
    } else {
        (config.total_video_frames as f64) * 1000.0 * f64::from(config.fps_den)
            / f64::from(config.fps_num)
    };

    let tags = [
        MatroskaSpec::Ebml(Master::Start),
        MatroskaSpec::EbmlVersion(1),
        MatroskaSpec::EbmlReadVersion(1),
        MatroskaSpec::EbmlMaxIdLength(4),
        MatroskaSpec::EbmlMaxSizeLength(8),
        MatroskaSpec::DocType("matroska".to_owned()),
        MatroskaSpec::DocTypeVersion(4),
        MatroskaSpec::DocTypeReadVersion(2),
        MatroskaSpec::Ebml(Master::End),
        MatroskaSpec::Segment(Master::Start),
        MatroskaSpec::Info(Master::Start),
        MatroskaSpec::TimestampScale(1_000_000),
        MatroskaSpec::Duration(total_duration_ms),
        MatroskaSpec::MuxingApp("sea-export-av1".to_owned()),
        MatroskaSpec::WritingApp("sea-movie-creator".to_owned()),
        MatroskaSpec::Info(Master::End),
    ];
    write_tag_slice(writer, &tags)
}

fn write_tracks_section<W: std::io::Write>(
    writer: &mut WebmWriter<W>,
    config: &MkvTrackConfig,
) -> Result<(), String> {
    let default_duration_ns = if config.fps_num == 0 {
        33_333_333
    } else {
        (u64::from(config.fps_den) * 1_000_000_000) / u64::from(config.fps_num)
    };

    let mut tags = vec![
        MatroskaSpec::Tracks(Master::Start),
        MatroskaSpec::TrackEntry(Master::Start),
        MatroskaSpec::TrackNumber(1),
        MatroskaSpec::TrackUID(1),
        MatroskaSpec::TrackType(1),
        MatroskaSpec::FlagEnabled(1),
        MatroskaSpec::FlagDefault(1),
        MatroskaSpec::FlagLacing(0),
        MatroskaSpec::DefaultDuration(default_duration_ns),
        MatroskaSpec::CodecID("V_AV1".to_owned()),
        MatroskaSpec::CodecPrivate(config.av1_codec_private.clone()),
        MatroskaSpec::Video(Master::Start),
        MatroskaSpec::PixelWidth(u64::from(config.width)),
        MatroskaSpec::PixelHeight(u64::from(config.height)),
        MatroskaSpec::Colour(Master::Start),
        MatroskaSpec::MatrixCoefficients(1),
        MatroskaSpec::TransferCharacteristics(1),
        MatroskaSpec::Primaries(1),
        MatroskaSpec::Range(1),
        MatroskaSpec::Colour(Master::End),
        MatroskaSpec::Video(Master::End),
        MatroskaSpec::TrackEntry(Master::End),
    ];

    if let Some(audio) = &config.audio {
        tags.extend([
            MatroskaSpec::TrackEntry(Master::Start),
            MatroskaSpec::TrackNumber(2),
            MatroskaSpec::TrackUID(2),
            MatroskaSpec::TrackType(2),
            MatroskaSpec::FlagEnabled(1),
            MatroskaSpec::FlagDefault(1),
            MatroskaSpec::FlagLacing(0),
            MatroskaSpec::CodecID("A_FLAC".to_owned()),
            MatroskaSpec::CodecPrivate(audio.codec_private.clone()),
            MatroskaSpec::Audio(Master::Start),
            MatroskaSpec::SamplingFrequency(f64::from(audio.sample_rate)),
            MatroskaSpec::Channels(u64::from(audio.channels)),
            MatroskaSpec::BitDepth(16),
            MatroskaSpec::Audio(Master::End),
            MatroskaSpec::TrackEntry(Master::End),
        ]);
    }
    tags.push(MatroskaSpec::Tracks(Master::End));
    write_tag_slice(writer, &tags)
}

fn write_interleaved_clusters<W: std::io::Write>(
    writer: &mut WebmWriter<W>,
    video_packets: &[EncodedPacket],
    audio: Option<&FlacAudioTrackData>,
) -> Result<(), String> {
    let audio_blocks = audio.map_or(&[][..], |a| a.blocks.as_slice());
    let mut merged: Vec<&EncodedPacket> =
        Vec::with_capacity(video_packets.len() + audio_blocks.len());
    merged.extend(video_packets.iter());
    merged.extend(audio_blocks.iter());
    merged.sort_by_key(|pkt| (pkt.pts_ms, pkt.track_number));

    let mut cluster_start_ms: Option<u64> = None;
    for pkt in merged {
        let need_new_cluster = match cluster_start_ms {
            None => true,
            Some(base_ms) => pkt.pts_ms.saturating_sub(base_ms) >= MAX_CLUSTER_SPAN_MS,
        };
        if need_new_cluster {
            if cluster_start_ms.is_some() {
                writer
                    .write(&MatroskaSpec::Cluster(Master::End))
                    .map_err(|err| format!("MKV Cluster 終端書き出し失敗: {err}"))?;
            }
            cluster_start_ms = Some(pkt.pts_ms);
            writer
                .write(&MatroskaSpec::Cluster(Master::Start))
                .map_err(|err| format!("MKV Cluster 開始書き出し失敗: {err}"))?;
            writer
                .write(&MatroskaSpec::Timestamp(pkt.pts_ms))
                .map_err(|err| format!("MKV Cluster 時刻書き出し失敗: {err}"))?;
        }

        let base_ms = cluster_start_ms.unwrap_or(0);
        let rel_ms_u64 = pkt.pts_ms.saturating_sub(base_ms);
        let rel_ms = i16::try_from(rel_ms_u64).unwrap_or(i16::MAX);
        let block = SimpleBlock::new_uncheked(
            &pkt.data,
            pkt.track_number,
            rel_ms,
            false,
            None,
            false,
            pkt.is_keyframe,
        );
        let tag: MatroskaSpec = block.into();
        writer
            .write(&tag)
            .map_err(|err| format!("MKV SimpleBlock 書き出し失敗: {err}"))?;
    }

    if cluster_start_ms.is_some() {
        writer
            .write(&MatroskaSpec::Cluster(Master::End))
            .map_err(|err| format!("MKV 最終 Cluster 終端書き出し失敗: {err}"))?;
    }
    Ok(())
}

fn write_tag_slice<W: std::io::Write>(
    writer: &mut WebmWriter<W>,
    tags: &[MatroskaSpec],
) -> Result<(), String> {
    for tag in tags {
        writer
            .write(tag)
            .map_err(|err| format!("MKV ヘッダタグ書き出し失敗: {err}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use matroska_demuxer::{MatroskaFile, TrackType};

    use super::*;

    #[test]
    fn flac_encode_and_mkv_muxer_produce_demuxable_container() {
        let pcm: Vec<i32> = (0..4096)
            .flat_map(|i| {
                let sample = ((i % 64) - 32) * 512;
                [sample, -sample]
            })
            .collect();
        let audio = encode_pcm_to_flac(&pcm, 48_000, 2).unwrap().unwrap();
        assert!(audio.codec_private.starts_with(b"fLaC"));
        assert_eq!(audio.codec_private.len(), 42);
        assert!(!audio.blocks.is_empty());

        let temp_dir = std::env::temp_dir();
        let out_path = temp_dir.join(format!("sea-mkv-mux-test-{}.mkv", std::process::id()));
        let cfg = MkvTrackConfig {
            width: 32,
            height: 32,
            fps_num: 30,
            fps_den: 1,
            total_video_frames: 2,
            av1_codec_private: vec![0x81, 0x00, 0x0C, 0x00],
            audio: Some(audio),
        };
        let dummy_video = vec![
            EncodedPacket {
                track_number: 1,
                pts_ms: 0,
                is_keyframe: true,
                data: vec![0x12, 0x00, 0x0A, 0x00],
            },
            EncodedPacket {
                track_number: 1,
                pts_ms: 33,
                is_keyframe: false,
                data: vec![0x12, 0x00, 0x0B, 0x00],
            },
        ];

        let size = write_mkv_file(&out_path, &cfg, &dummy_video).unwrap();
        assert!(size > 100);

        let file = File::open(&out_path).unwrap();
        let mut mkv = MatroskaFile::open(file).unwrap();
        let tracks = mkv.tracks();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_type(), TrackType::Video);
        assert_eq!(tracks[0].codec_id(), "V_AV1");
        assert_eq!(tracks[1].track_type(), TrackType::Audio);
        assert_eq!(tracks[1].codec_id(), "A_FLAC");

        let mut frame = matroska_demuxer::Frame::default();
        let mut block_count = 0usize;
        while mkv.next_frame(&mut frame).unwrap() {
            block_count += 1;
        }
        assert!(block_count >= 3);
        drop(mkv);
        let _ = std::fs::remove_file(&out_path);
    }
}
