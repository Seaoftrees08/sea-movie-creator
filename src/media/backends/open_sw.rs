//! オープンソースデコーダ (`matroska-demuxer` + `rav1d` + `symphonia`) バックエンド。
//!
//! `rav1d` の `dav1d` 互換 C ABI 呼び出しを安全に扱うため、このモジュール内に限り
//! `unsafe` を許可し、すべての呼び出し箇所に `// SAFETY:` コメントを明記する。

#![allow(unsafe_code)]

use std::ffi::c_int;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use matroska_demuxer::{MatroskaFile, TrackType as MkvTrackType};
use rav1d::Dav1dResult;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_MC_BT709, DAV1D_MC_IDENTITY, DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420,
    DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::media::decoder::{
    AudioDecoderStream, DecodedAudioPcm, DecodedVideoFrame, MediaError, MediaStreamInfo,
    VideoDecoder, VideoFrameBuffer,
};

#[cfg(target_os = "macos")]
const DAV1D_ERR_EAGAIN: c_int = -35;
#[cfg(not(target_os = "macos"))]
const DAV1D_ERR_EAGAIN: c_int = -11;

const DECODER_STACK_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_DURATION_SEC_TO_NS: f64 = 1_000_000_000.0;

/// オープンソフトウェアバックエンドでメディアファイルをプローブする。
///
/// # Errors
///
/// サポート対象コンテナとして解釈できない場合。
pub fn probe(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    if is_ivf_extension(path) {
        let header = read_ivf_header(path)?;
        return Ok(MediaStreamInfo {
            duration_seconds: header.duration_seconds(),
            width: Some(u32::from(header.width)),
            height: Some(u32::from(header.height)),
            frame_rate_num: Some(header.timebase_den),
            frame_rate_den: Some(header.timebase_num.max(1)),
            has_video: true,
            has_audio: false,
            audio_sample_rate: None,
            audio_channels: None,
            video_codec: Some("av1".to_owned()),
            audio_codec: None,
        });
    }

    let mut info = MediaStreamInfo::default();
    if is_mkv_extension(path)
        && let Ok(mkv) = File::open(path)
            .and_then(|f| MatroskaFile::open(f).map_err(|e| std::io::Error::other(e.to_string())))
    {
        populate_info_from_mkv(&mkv, &mut info);
    }

    if let Ok(symphonia_info) = probe_audio_with_symphonia(path) {
        if info.duration_seconds.is_none() {
            info.duration_seconds = symphonia_info.duration_seconds;
        }
        info.has_audio = symphonia_info.has_audio;
        info.audio_sample_rate = symphonia_info.audio_sample_rate;
        info.audio_channels = symphonia_info.audio_channels;
        info.audio_codec = symphonia_info.audio_codec;
    }

    if !info.has_video && !info.has_audio {
        return Err(MediaError::UnsupportedFormat(format!(
            "cannot probe {} with OpenSoftware backend",
            path.display()
        )));
    }
    Ok(info)
}

/// オープンソフトウェア映像デコーダ (AV1 over Matroska / IVF) を開く。
///
/// # Errors
///
/// 非対応コンテナ・非対応コーデック・映像トラック不在の場合。
pub fn open_video(path: &Path) -> Result<Box<dyn VideoDecoder>, MediaError> {
    if is_ivf_extension(path) {
        let source = IvfPacketSource::open(path)?;
        let info = source.stream_info();
        let worker = Rav1dWorkerHandle::spawn()?;
        return Ok(Box::new(OpenSwVideoDecoder {
            info,
            source: VideoPacketSource::Ivf(source),
            worker,
            eof_reached: false,
        }));
    }

    let file = File::open(path)?;
    let mkv = MatroskaFile::open(file)
        .map_err(|err| MediaError::UnsupportedFormat(format!("matroska open failed: {err}")))?;
    let source = MkvPacketSource::from_file(mkv, path)?;
    let info = source.stream_info();
    let worker = Rav1dWorkerHandle::spawn()?;

    Ok(Box::new(OpenSwVideoDecoder {
        info,
        source: VideoPacketSource::Mkv(Box::new(source)),
        worker,
        eof_reached: false,
    }))
}

/// オープンソフトウェア音声デコーダ (`symphonia`) を開く。
///
/// # Errors
///
/// 音声トラックが見つからない、またはコーデック非対応の場合。
pub fn open_audio(path: &Path) -> Result<Box<dyn AudioDecoderStream>, MediaError> {
    let dec = SymphoniaAudioDecoder::open(path)?;
    Ok(Box::new(dec))
}

fn is_ivf_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ivf"))
}

fn is_mkv_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv") || e.eq_ignore_ascii_case("webm"))
}

fn populate_info_from_mkv(mkv: &MatroskaFile<File>, info: &mut MediaStreamInfo) {
    let ts_scale = mkv.info().timestamp_scale().get();
    #[allow(clippy::cast_precision_loss)]
    if let Some(dur_ticks) = mkv.info().duration() {
        info.duration_seconds = Some(dur_ticks * (ts_scale as f64) / DEFAULT_DURATION_SEC_TO_NS);
    }

    for track in mkv.tracks() {
        match track.track_type() {
            MkvTrackType::Video if !info.has_video => {
                info.has_video = true;
                info.video_codec = Some(track.codec_id().to_owned());
                if let Some(video) = track.video() {
                    info.width = u32::try_from(video.pixel_width().get()).ok();
                    info.height = u32::try_from(video.pixel_height().get()).ok();
                }
                if let Some(frame_ns) = track.default_duration() {
                    let (num, den) = ns_to_rational_fps(frame_ns.get());
                    info.frame_rate_num = Some(num);
                    info.frame_rate_den = Some(den);
                }
            }
            MkvTrackType::Audio if !info.has_audio => {
                info.has_audio = true;
                info.audio_codec = Some(track.codec_id().to_owned());
                if let Some(audio) = track.audio() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let rate = audio.sampling_frequency().round() as u32;
                    info.audio_sample_rate = Some(rate);
                    info.audio_channels = u16::try_from(audio.channels().get()).ok();
                }
            }
            _ => {}
        }
    }
}

fn ns_to_rational_fps(frame_ns: u64) -> (u32, u32) {
    if frame_ns == 0 {
        return (30, 1);
    }
    // 一般的な標準フレームレートへスナップしつつ計算可能な分数へ丸める。
    #[allow(clippy::cast_precision_loss)]
    let fps = DEFAULT_DURATION_SEC_TO_NS / (frame_ns as f64);
    let rounded = fps.round();
    if (fps - rounded).abs() < 0.05 && (1.0..=1000.0).contains(&rounded) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return (rounded as u32, 1);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled_num = (fps * 1001.0).round().clamp(1.0, 240_000.0) as u32;
    (scaled_num, 1001)
}

struct OpenSwVideoDecoder {
    info: MediaStreamInfo,
    source: VideoPacketSource,
    worker: Rav1dWorkerHandle,
    eof_reached: bool,
}

impl VideoDecoder for OpenSwVideoDecoder {
    fn info(&self) -> &MediaStreamInfo {
        &self.info
    }

    fn seek_seconds(&mut self, seconds: f64) -> Result<(), MediaError> {
        self.eof_reached = false;
        self.source.seek_seconds(seconds)?;
        self.worker.flush()
    }

    fn decode_next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, MediaError> {
        loop {
            if let Some(frame) = self.worker.receive_picture()? {
                return Ok(Some(frame));
            }
            if self.eof_reached {
                return Ok(None);
            }
            match self.source.next_packet()? {
                Some(packet) => {
                    self.worker.send_packet(packet.payload, packet.pts_ns)?;
                }
                None => {
                    self.eof_reached = true;
                }
            }
        }
    }
}

struct DemuxedVideoPacket {
    pts_ns: i64,
    payload: Vec<u8>,
}

enum VideoPacketSource {
    Mkv(Box<MkvPacketSource>),
    Ivf(IvfPacketSource),
}

impl VideoPacketSource {
    fn seek_seconds(&mut self, seconds: f64) -> Result<(), MediaError> {
        match self {
            Self::Mkv(mkv) => mkv.seek_seconds(seconds),
            Self::Ivf(ivf) => ivf.seek_seconds(seconds),
        }
    }

    fn next_packet(&mut self) -> Result<Option<DemuxedVideoPacket>, MediaError> {
        match self {
            Self::Mkv(mkv) => mkv.next_packet(),
            Self::Ivf(ivf) => ivf.next_packet(),
        }
    }
}

struct MkvPacketSource {
    file: MatroskaFile<File>,
    video_track_id: u64,
    timestamp_scale_ns: u64,
    frame_buf: matroska_demuxer::Frame,
    info: MediaStreamInfo,
}

impl MkvPacketSource {
    fn from_file(file: MatroskaFile<File>, path: &Path) -> Result<Self, MediaError> {
        let track = file
            .tracks()
            .iter()
            .find(|t| t.track_type() == MkvTrackType::Video)
            .ok_or(MediaError::NoVideoStream)?;
        if track.codec_id() != "V_AV1" {
            return Err(MediaError::UnsupportedCodec(format!(
                "OpenSoftware video decoder requires AV1 (V_AV1), got {} in {}",
                track.codec_id(),
                path.display()
            )));
        }

        let video_track_id = track.track_number().get();
        let timestamp_scale_ns = file.info().timestamp_scale().get();
        let mut info = MediaStreamInfo::default();
        populate_info_from_mkv(&file, &mut info);

        Ok(Self {
            file,
            video_track_id,
            timestamp_scale_ns,
            frame_buf: matroska_demuxer::Frame::default(),
            info,
        })
    }

    fn stream_info(&self) -> MediaStreamInfo {
        self.info.clone()
    }

    fn seek_seconds(&mut self, seconds: f64) -> Result<(), MediaError> {
        let ns = (seconds.max(0.0) * DEFAULT_DURATION_SEC_TO_NS).round();
        #[allow(clippy::cast_precision_loss)]
        let ticks_f64 = ns / (self.timestamp_scale_ns as f64);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ticks = ticks_f64.max(0.0) as u64;
        self.file
            .seek(ticks)
            .map_err(|err| MediaError::DecodeFailed(format!("matroska seek failed: {err}")))
    }

    fn next_packet(&mut self) -> Result<Option<DemuxedVideoPacket>, MediaError> {
        loop {
            let has_more = self
                .file
                .next_frame(&mut self.frame_buf)
                .map_err(|err| MediaError::DecodeFailed(format!("matroska read failed: {err}")))?;
            if !has_more {
                return Ok(None);
            }
            if self.frame_buf.track != self.video_track_id {
                continue;
            }
            let pts_ns_u64 = self
                .frame_buf
                .timestamp
                .saturating_mul(self.timestamp_scale_ns);
            let pts_ns = i64::try_from(pts_ns_u64).unwrap_or(i64::MAX);
            return Ok(Some(DemuxedVideoPacket {
                pts_ns,
                payload: std::mem::take(&mut self.frame_buf.data),
            }));
        }
    }
}

#[derive(Clone, Copy)]
struct IvfHeader {
    width: u16,
    height: u16,
    timebase_den: u32,
    timebase_num: u32,
    frame_count: u32,
}

impl IvfHeader {
    fn duration_seconds(self) -> Option<f64> {
        if self.frame_count == 0 || self.timebase_den == 0 {
            return None;
        }
        let sec = f64::from(self.frame_count) * f64::from(self.timebase_num.max(1))
            / f64::from(self.timebase_den);
        Some(sec)
    }
}

fn read_ivf_header(path: &Path) -> Result<IvfHeader, MediaError> {
    let mut file = File::open(path)?;
    let mut hdr = [0u8; 32];
    file.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"DKIF" || &hdr[8..12] != b"AV01" {
        return Err(MediaError::UnsupportedFormat(
            "expected DKIF AV01 IVF header".to_owned(),
        ));
    }
    Ok(IvfHeader {
        width: u16::from_le_bytes([hdr[12], hdr[13]]),
        height: u16::from_le_bytes([hdr[14], hdr[15]]),
        timebase_den: u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]).max(1),
        timebase_num: u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]).max(1),
        frame_count: u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]),
    })
}

struct IvfPacketSource {
    reader: BufReader<File>,
    header: IvfHeader,
}

impl IvfPacketSource {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let header = read_ivf_header(path)?;
        let mut reader = BufReader::new(File::open(path)?);
        reader.seek(SeekFrom::Start(32))?;
        Ok(Self { reader, header })
    }

    fn stream_info(&self) -> MediaStreamInfo {
        MediaStreamInfo {
            duration_seconds: self.header.duration_seconds(),
            width: Some(u32::from(self.header.width)),
            height: Some(u32::from(self.header.height)),
            frame_rate_num: Some(self.header.timebase_den),
            frame_rate_den: Some(self.header.timebase_num),
            has_video: true,
            has_audio: false,
            audio_sample_rate: None,
            audio_channels: None,
            video_codec: Some("av1".to_owned()),
            audio_codec: None,
        }
    }

    fn seek_seconds(&mut self, _seconds: f64) -> Result<(), MediaError> {
        // IVF にはキーフレーム索引が無いため先頭へ巻き戻し、上位の decode_frame_at_seconds に送出を委ねる。
        self.reader.seek(SeekFrom::Start(32))?;
        Ok(())
    }

    fn next_packet(&mut self) -> Result<Option<DemuxedVideoPacket>, MediaError> {
        let mut frame_hdr = [0u8; 12];
        match self.reader.read_exact(&mut frame_hdr) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(MediaError::Io(err)),
        }
        let sz = u32::from_le_bytes([frame_hdr[0], frame_hdr[1], frame_hdr[2], frame_hdr[3]]);
        let pts_ticks = u64::from_le_bytes([
            frame_hdr[4],
            frame_hdr[5],
            frame_hdr[6],
            frame_hdr[7],
            frame_hdr[8],
            frame_hdr[9],
            frame_hdr[10],
            frame_hdr[11],
        ]);
        let payload_len = usize::try_from(sz)
            .map_err(|_| MediaError::DecodeFailed("IVF frame size overflow".to_owned()))?;
        let mut payload = vec![0u8; payload_len];
        self.reader.read_exact(&mut payload)?;

        #[allow(clippy::cast_precision_loss)]
        let pts_sec = (pts_ticks as f64) * f64::from(self.header.timebase_num)
            / f64::from(self.header.timebase_den);
        #[allow(clippy::cast_possible_truncation)]
        let pts_ns = (pts_sec * DEFAULT_DURATION_SEC_TO_NS).round() as i64;
        Ok(Some(DemuxedVideoPacket { pts_ns, payload }))
    }
}

enum WorkerCommand {
    SendPacket {
        payload: Vec<u8>,
        pts_ns: i64,
        reply: SyncSender<Result<(), MediaError>>,
    },
    ReceivePicture {
        reply: SyncSender<Result<Option<DecodedVideoFrame>, MediaError>>,
    },
    Flush {
        reply: SyncSender<Result<(), MediaError>>,
    },
    Shutdown,
}

/// 8 MiB スタック専用スレッド上で `rav1d` コンテキストを安全に運用するアクター。
struct Rav1dWorkerHandle {
    tx: SyncSender<WorkerCommand>,
    join: Option<JoinHandle<()>>,
}

impl Rav1dWorkerHandle {
    fn spawn() -> Result<Self, MediaError> {
        let (tx, rx) = mpsc::sync_channel::<WorkerCommand>(4);
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), MediaError>>(1);
        let join = thread::Builder::new()
            .name("sea-rav1d-decoder".to_owned())
            .stack_size(DECODER_STACK_BYTES)
            .spawn(move || run_rav1d_worker(&rx, &init_tx))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Some(join),
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                let _ = join.join();
                Err(MediaError::DecodeFailed(
                    "rav1d worker exited during startup".to_owned(),
                ))
            }
        }
    }

    fn send_packet(&self, payload: Vec<u8>, pts_ns: i64) -> Result<(), MediaError> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(WorkerCommand::SendPacket {
                payload,
                pts_ns,
                reply,
            })
            .map_err(|_| MediaError::DecodeFailed("rav1d worker channel closed".to_owned()))?;
        rx.recv()
            .map_err(|_| MediaError::DecodeFailed("rav1d worker reply dropped".to_owned()))?
    }

    fn receive_picture(&self) -> Result<Option<DecodedVideoFrame>, MediaError> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(WorkerCommand::ReceivePicture { reply })
            .map_err(|_| MediaError::DecodeFailed("rav1d worker channel closed".to_owned()))?;
        rx.recv()
            .map_err(|_| MediaError::DecodeFailed("rav1d worker reply dropped".to_owned()))?
    }

    fn flush(&self) -> Result<(), MediaError> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(WorkerCommand::Flush { reply })
            .map_err(|_| MediaError::DecodeFailed("rav1d worker channel closed".to_owned()))?;
        rx.recv()
            .map_err(|_| MediaError::DecodeFailed("rav1d worker reply dropped".to_owned()))?
    }
}

impl Drop for Rav1dWorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_rav1d_worker(
    rx: &mpsc::Receiver<WorkerCommand>,
    init_tx: &SyncSender<Result<(), MediaError>>,
) {
    let mut decoder = match Rav1dDecoder::open(2) {
        Ok(dec) => {
            let _ = init_tx.send(Ok(()));
            dec
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCommand::SendPacket {
                payload,
                pts_ns,
                reply,
            } => {
                let _ = reply.send(decoder.send_packet(&payload, pts_ns));
            }
            WorkerCommand::ReceivePicture { reply } => {
                let _ = reply.send(decoder.receive_picture());
            }
            WorkerCommand::Flush { reply } => {
                decoder.flush();
                let _ = reply.send(Ok(()));
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

struct Rav1dDecoder {
    ctx: Option<Dav1dContext>,
}

impl Rav1dDecoder {
    fn open(n_threads: c_int) -> Result<Self, MediaError> {
        let mut settings_uninit = MaybeUninit::<Dav1dSettings>::uninit();
        let settings_ptr = NonNull::new(settings_uninit.as_mut_ptr())
            .ok_or_else(|| MediaError::DecodeFailed("null Dav1dSettings pointer".to_owned()))?;

        // SAFETY: `settings_ptr` は書き込み可能な未初期化 `Dav1dSettings` を指している。
        unsafe {
            dav1d_default_settings(settings_ptr);
        }
        // SAFETY: `dav1d_default_settings` が全フィールドを初期化済み。
        let mut settings = unsafe { settings_uninit.assume_init() };
        settings.n_threads = n_threads.max(1);
        settings.max_frame_delay = 1;
        settings.apply_grain = 1;

        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: `ctx` と `settings` は有効な参照であり、`dav1d_open` のみで使用される。
        let res = unsafe { dav1d_open(NonNull::new(&raw mut ctx), Some(NonNull::from(&settings))) };
        if res.0 != 0 || ctx.is_none() {
            return Err(MediaError::DecodeFailed(format!(
                "dav1d_open failed with code {}",
                res.0
            )));
        }
        Ok(Self { ctx })
    }

    fn flush(&self) {
        if let Some(ctx) = self.ctx {
            // SAFETY: `ctx` は `dav1d_open` で生成され、まだ close されていない。
            unsafe {
                dav1d_flush(ctx);
            }
        }
    }

    fn send_packet(&mut self, payload: &[u8], pts_ns: i64) -> Result<(), MediaError> {
        if payload.is_empty() {
            return Ok(());
        }
        let mut data = Dav1dData::default();
        // SAFETY: `data` は有効なローカル変数を指し、`payload.len()` バイト確保される。
        let dst = unsafe { dav1d_data_create(NonNull::new(&raw mut data), payload.len()) };
        if dst.is_null() {
            return Err(MediaError::DecodeFailed(
                "dav1d_data_create returned null".to_owned(),
            ));
        }
        // SAFETY: `dst` は `dav1d_data_create` が確保した `payload.len()` バイトであり重複しない。
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), dst, payload.len());
        }
        data.m.timestamp = pts_ns;

        loop {
            // SAFETY: `self.ctx` と `data` は共に有効。`dav1d_send_data` 成功時は `data` 所有権が移動する。
            let res: Dav1dResult =
                unsafe { dav1d_send_data(self.ctx, NonNull::new(&raw mut data)) };
            if res.0 == 0 {
                return Ok(());
            }
            if res.0 == DAV1D_ERR_EAGAIN {
                // 内部キューが満杯の場合は 1 フレーム排出を促してから再送する。
                let _ = self.receive_picture()?;
                continue;
            }
            // SAFETY: エラー時に未消費の `Dav1dData` が残っている場合に解放する。
            unsafe {
                dav1d_data_unref(NonNull::new(&raw mut data));
            }
            return Err(MediaError::DecodeFailed(format!(
                "dav1d_send_data failed with code {}",
                res.0
            )));
        }
    }

    fn receive_picture(&mut self) -> Result<Option<DecodedVideoFrame>, MediaError> {
        let mut guard = PictureGuard(Dav1dPicture::default());
        // SAFETY: `self.ctx` は有効、かつ `&raw mut guard.0` は書き込み可能。
        let res = unsafe { dav1d_get_picture(self.ctx, NonNull::new(&raw mut guard.0)) };
        if res.0 == DAV1D_ERR_EAGAIN {
            return Ok(None);
        }
        if res.0 != 0 {
            return Err(MediaError::DecodeFailed(format!(
                "dav1d_get_picture failed with code {}",
                res.0
            )));
        }

        let frame = extract_bgra8_from_picture(&guard.0)?;
        Ok(Some(frame))
    }
}

impl Drop for Rav1dDecoder {
    fn drop(&mut self) {
        if self.ctx.is_some() {
            // SAFETY: `self.ctx` は `dav1d_open` 由来で 1 回のみ close される。
            unsafe {
                dav1d_close(NonNull::new(&raw mut self.ctx));
            }
        }
    }
}

struct PictureGuard(Dav1dPicture);

impl Drop for PictureGuard {
    fn drop(&mut self) {
        // SAFETY: `Dav1dPicture` がデータ参照を持つ場合のみ `dav1d_picture_unref` が安全に解放する。
        unsafe {
            dav1d_picture_unref(NonNull::new(&raw mut self.0));
        }
    }
}

struct YuvConversionParams {
    width: usize,
    height: usize,
    bpc: u8,
    layout: u32,
    use_bt709: bool,
    full_range: bool,
}

fn extract_bgra8_from_picture(pic: &Dav1dPicture) -> Result<DecodedVideoFrame, MediaError> {
    let width = u32::try_from(pic.p.w).map_err(|_| MediaError::InvalidDimensions {
        width: 0,
        height: 0,
    })?;
    let height =
        u32::try_from(pic.p.h).map_err(|_| MediaError::InvalidDimensions { width, height: 0 })?;
    if width == 0 || height == 0 {
        return Err(MediaError::InvalidDimensions { width, height });
    }
    let bpc = u8::try_from(pic.p.bpc).unwrap_or(8);

    let (use_bt709, full_range) = match pic.seq_hdr {
        Some(seq_ptr) => {
            // SAFETY: `pic.seq_hdr` は `pic` 生存期間中 `rav1d` が保持する有効な配列ヘッダを指す。
            let seq = unsafe { seq_ptr.as_ref() };
            let bt709 = seq.mtrx == DAV1D_MC_BT709;
            (bt709, seq.color_range != 0 || seq.mtrx == DAV1D_MC_IDENTITY)
        }
        None => (height >= 720, false),
    };

    let y_plane = PlaneView::from_picture(pic, 0, pic.stride[0])?;
    let u_plane = if pic.p.layout == DAV1D_PIXEL_LAYOUT_I400 {
        None
    } else {
        Some(PlaneView::from_picture(pic, 1, pic.stride[1])?)
    };
    let v_plane = if pic.p.layout == DAV1D_PIXEL_LAYOUT_I400 {
        None
    } else {
        Some(PlaneView::from_picture(pic, 2, pic.stride[1])?)
    };

    let params = YuvConversionParams {
        width: width as usize,
        height: height as usize,
        bpc,
        layout: pic.p.layout,
        use_bt709,
        full_range,
    };

    let mut bgra = vec![0u8; params.width * params.height * 4];
    yuv_to_bgra8(
        &params,
        &y_plane,
        u_plane.as_ref(),
        v_plane.as_ref(),
        &mut bgra,
    );
    let buffer = VideoFrameBuffer::new_bgra8(width, height, bgra)?;

    let pts_ns = if pic.m.timestamp == i64::MIN {
        0
    } else {
        pic.m.timestamp.max(0)
    };
    #[allow(clippy::cast_precision_loss)]
    let pts_seconds = (pts_ns as f64) / DEFAULT_DURATION_SEC_TO_NS;
    Ok(DecodedVideoFrame {
        pts_seconds,
        buffer,
    })
}

struct PlaneView<'a> {
    base: *const u8,
    stride: isize,
    _marker: std::marker::PhantomData<&'a Dav1dPicture>,
}

impl<'a> PlaneView<'a> {
    fn from_picture(
        pic: &'a Dav1dPicture,
        plane_idx: usize,
        stride: isize,
    ) -> Result<Self, MediaError> {
        let ptr = pic.data[plane_idx].ok_or_else(|| {
            MediaError::DecodeFailed(format!("missing YUV plane {plane_idx} in Dav1dPicture"))
        })?;
        Ok(Self {
            base: ptr.as_ptr().cast::<u8>(),
            stride,
            _marker: std::marker::PhantomData,
        })
    }

    fn read_8bit(&self, row: usize, col: usize, bpc: u8) -> u8 {
        let row_isize = isize::try_from(row).unwrap_or(0);
        // SAFETY: 呼び出し側が `row < plane_height` と `col < plane_width` を保証し、`pic` 生存期間中のみ読み取る。
        let row_ptr = unsafe { self.base.offset(row_isize * self.stride) };
        if bpc <= 8 {
            // SAFETY: `col` は有効なピクセル列範囲内。
            return unsafe { *row_ptr.add(col) };
        }
        // SAFETY: 高ビット深度はリトルエンディアン 2 バイト格納であり、`col * 2 + 1` が行範囲内。
        let val = unsafe {
            let b0 = *row_ptr.add(col * 2);
            let b1 = *row_ptr.add(col * 2 + 1);
            u16::from_le_bytes([b0, b1])
        };
        let shift = bpc.saturating_sub(8).min(8);
        u8::try_from((val >> shift).min(255)).unwrap_or(255)
    }
}

fn yuv_to_bgra8(
    params: &YuvConversionParams,
    y_plane: &PlaneView<'_>,
    u_plane: Option<&PlaneView<'_>>,
    v_plane: Option<&PlaneView<'_>>,
    dst_bgra: &mut [u8],
) {
    let shift_x = usize::from(
        params.layout == DAV1D_PIXEL_LAYOUT_I420 || params.layout == DAV1D_PIXEL_LAYOUT_I422,
    );
    let shift_y = usize::from(params.layout == DAV1D_PIXEL_LAYOUT_I420);

    for row in 0..params.height {
        let c_row = row >> shift_y;
        let row_out = &mut dst_bgra[row * params.width * 4..(row + 1) * params.width * 4];
        for col in 0..params.width {
            let y_raw = y_plane.read_8bit(row, col, params.bpc);
            let (u_raw, v_raw) = match (u_plane, v_plane) {
                (Some(up), Some(vp)) => {
                    let c_col = col >> shift_x;
                    (
                        up.read_8bit(c_row, c_col, params.bpc),
                        vp.read_8bit(c_row, c_col, params.bpc),
                    )
                }
                _ => (128, 128),
            };

            let (r, g, b) =
                yuv_sample_to_rgb(y_raw, u_raw, v_raw, params.use_bt709, params.full_range);
            let off = col * 4;
            row_out[off] = b;
            row_out[off + 1] = g;
            row_out[off + 2] = r;
            row_out[off + 3] = 255;
        }
    }
}

fn yuv_sample_to_rgb(
    y_val: u8,
    u_val: u8,
    v_val: u8,
    use_bt709: bool,
    full_range: bool,
) -> (u8, u8, u8) {
    let (luma_raw, scale_y) = if full_range {
        (f32::from(y_val), 1.0_f32)
    } else {
        ((f32::from(y_val) - 16.0).max(0.0), 255.0 / 219.0)
    };
    let scale_uv = if full_range { 1.0_f32 } else { 255.0 / 224.0 };
    let cb = (f32::from(u_val) - 128.0) * scale_uv;
    let cr = (f32::from(v_val) - 128.0) * scale_uv;
    let luma = luma_raw * scale_y;

    let (r_coef, g_for_cb, cr_for_g, b_coef) = if use_bt709 {
        (1.5748_f32, -0.1873_f32, -0.4681_f32, 1.8556_f32)
    } else {
        (1.402_f32, -0.344_136_f32, -0.714_136_f32, 1.772_f32)
    };

    let red = clamp_f32_to_u8(luma + r_coef * cr);
    let green = clamp_f32_to_u8(luma + g_for_cb * cb + cr_for_g * cr);
    let blue = clamp_f32_to_u8(luma + b_coef * cb);
    (red, green, blue)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_f32_to_u8(val: f32) -> u8 {
    val.round().clamp(0.0, 255.0) as u8
}

fn probe_audio_with_symphonia(path: &Path) -> Result<MediaStreamInfo, MediaError> {
    let dec = SymphoniaAudioDecoder::open(path)?;
    Ok(MediaStreamInfo {
        duration_seconds: dec.duration_seconds(),
        has_audio: true,
        audio_sample_rate: Some(dec.sample_rate()),
        audio_channels: Some(dec.channels()),
        audio_codec: Some(dec.codec_name.clone()),
        ..MediaStreamInfo::default()
    })
}

struct SymphoniaAudioDecoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    duration_seconds: Option<f64>,
    codec_name: String,
}

impl SymphoniaAudioDecoder {
    fn open(path: &Path) -> Result<Self, MediaError> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|err| {
                MediaError::UnsupportedFormat(format!("symphonia probe failed: {err}"))
            })?;

        let track = format
            .first_track_known_codec(symphonia::core::formats::TrackType::Audio)
            .ok_or(MediaError::NoAudioStream)?;

        let track_id = track.id;
        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or(MediaError::NoAudioStream)?
            .clone();

        let sample_rate = audio_params.sample_rate.ok_or_else(|| {
            MediaError::UnsupportedCodec("audio track missing sample rate".to_owned())
        })?;
        let channels_count = audio_params
            .channels
            .as_ref()
            .map_or(2, |c| u16::try_from(c.count()).unwrap_or(2));

        let duration_seconds = match (track.num_frames, track.time_base, track.duration) {
            (Some(frames), _, _) if sample_rate > 0 =>
            {
                #[allow(clippy::cast_precision_loss)]
                Some((frames as f64) / f64::from(sample_rate))
            }
            (_, Some(tb), Some(dur)) => tb.calc_duration(dur).map(|t| t.as_secs_f64()),
            _ => None,
        };

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
            .map_err(|err| {
                MediaError::UnsupportedCodec(format!("symphonia decoder init failed: {err}"))
            })?;
        let codec_name = decoder.codec_info().short_name.to_owned();

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels: channels_count,
            duration_seconds,
            codec_name,
        })
    }
}

impl AudioDecoderStream for SymphoniaAudioDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.duration_seconds
    }

    fn decode_all_interleaved(&mut self) -> Result<DecodedAudioPcm, MediaError> {
        let mut interleaved = Vec::new();
        let mut scratch = Vec::new();

        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(pkt)) => pkt,
                Ok(None) => break,
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(err) => {
                    return Err(MediaError::DecodeFailed(format!(
                        "symphonia next_packet failed: {err}"
                    )));
                }
            };

            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    decoded.copy_to_vec_interleaved::<f32>(&mut scratch);
                    interleaved.extend_from_slice(&scratch);
                }
                Err(SymphoniaError::DecodeError(_)) => {
                    // 軽微な破損フレームはスキップして再生継続する。
                }
                Err(err) => {
                    return Err(MediaError::DecodeFailed(format!(
                        "symphonia decode failed: {err}"
                    )));
                }
            }
        }

        Ok(DecodedAudioPcm {
            sample_rate: self.sample_rate,
            channels: self.channels,
            interleaved,
        })
    }
}
