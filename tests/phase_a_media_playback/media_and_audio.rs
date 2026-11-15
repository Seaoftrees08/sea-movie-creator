//! Phase A-4: 画像・AV1映像デコード、FrameCache、AudioMixer、rubato リサンプリング、PlaybackClock 結合検証。

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use image::{ImageFormat, Rgba, RgbaImage};
use rav1e::prelude::{
    ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, Frame, PixelRange, Rational,
};
use sea_movie_creator::audio::mixer::AudioMixer;
use sea_movie_creator::audio::playback::{PlaybackClock, resample_interleaved_pcm};
use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::decoder::{
    BackendPreference, FramePixelFormat, decode_image_bytes, decode_image_file, open_audio_decoder,
    open_video_decoder, probe_media,
};
use sea_movie_creator::media::frame_cache::{FrameCache, FrameCacheKey};
use sea_movie_creator::timeline::document::{
    AudioProperties, Clip, ClipKind, Layer, ProjectDocument,
};

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("sea_mc_phase_a4_{prefix}_{pid}_{id}"));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn fill_const_yuv_frame(frame: &mut Frame<u8>, y: u8, u: u8, v: u8) {
    let values = [y, u, v];
    for (idx, plane) in frame.planes.iter_mut().enumerate() {
        let val = values[idx];
        let stride = plane.cfg.stride;
        for row in plane.data.chunks_mut(stride) {
            row.fill(val);
        }
    }
}

fn build_rav1e_av1_ivf(
    out_path: &Path,
    width: usize,
    height: usize,
    luma_frames: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut enc = EncoderConfig::with_speed_preset(10);
    enc.width = width;
    enc.height = height;
    enc.bit_depth = 8;
    enc.chroma_sampling = ChromaSampling::Cs420;
    enc.pixel_range = PixelRange::Full;
    enc.time_base = Rational::new(1, 30);
    enc.low_latency = true;
    enc.quantizer = 64;
    enc.min_key_frame_interval = 1;
    enc.max_key_frame_interval = 8;

    let cfg = Config::new().with_encoder_config(enc).with_threads(1);
    let mut ctx: Context<u8> = cfg.new_context()?;

    for &y in luma_frames {
        let mut frame = ctx.new_frame();
        fill_const_yuv_frame(&mut frame, y, 128, 128);
        ctx.send_frame(frame)?;
    }
    ctx.flush();

    let mut packets = Vec::new();
    loop {
        match ctx.receive_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(EncoderStatus::Encoded) => {}
            Err(EncoderStatus::LimitReached) => break,
            Err(status) => {
                return Err(format!("rav1e encode failure: {status:?}").into());
            }
        }
    }

    write_ivf_file(out_path, width, height, &packets)
}

fn write_ivf_file(
    path: &Path,
    width: usize,
    height: usize,
    packets: &[rav1e::data::Packet<u8>],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::create(path)?;
    let w_u16 = u16::try_from(width)?;
    let h_u16 = u16::try_from(height)?;
    let count_u32 = u32::try_from(packets.len())?;

    file.write_all(b"DKIF")?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"AV01")?;
    file.write_all(&w_u16.to_le_bytes())?;
    file.write_all(&h_u16.to_le_bytes())?;
    file.write_all(&30u32.to_le_bytes())?;
    file.write_all(&1u32.to_le_bytes())?;
    file.write_all(&count_u32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;

    for pkt in packets {
        let len_u32 = u32::try_from(pkt.data.len())?;
        file.write_all(&len_u32.to_le_bytes())?;
        file.write_all(&pkt.input_frameno.to_le_bytes())?;
        file.write_all(&pkt.data)?;
    }
    Ok(())
}

fn write_pcm16_wav(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    samples_i16: &[i16],
) -> Result<(), Box<dyn std::error::Error>> {
    let data_bytes = u32::try_from(samples_i16.len() * 2)?;
    let riff_size = data_bytes + 36;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;

    let mut file = File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    for &sample in samples_i16 {
        file.write_all(&sample.to_le_bytes())?;
    }
    Ok(())
}

#[test]
fn image_decode_file_and_bytes_convert_to_bgra8_and_integrate_with_frame_cache()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempTestDir::create("img")?;
    let png_path = dir.child("sample.png");

    // 左上を純赤 (255, 0, 0, 255)、右下を半透明青 (0, 0, 200, 128) とする。
    let img = RgbaImage::from_fn(2, 2, |x, y| {
        if x == 0 && y == 0 {
            Rgba([255, 10, 20, 255])
        } else {
            Rgba([0, 30, 200, 128])
        }
    });
    img.save_with_format(&png_path, ImageFormat::Png)?;

    let decoded_from_file = decode_image_file(&png_path)?;
    assert_eq!(decoded_from_file.format, FramePixelFormat::Bgra8);
    assert_eq!(decoded_from_file.width, 2);
    assert_eq!(decoded_from_file.height, 2);
    // BGRA8 順: [B=20, G=10, R=255, A=255]
    assert_eq!(&decoded_from_file.data[0..4], &[20, 10, 255, 255]);
    // 4 番目のピクセル (x=1, y=1): [B=200, G=30, R=0, A=128]
    assert_eq!(&decoded_from_file.data[12..16], &[200, 30, 0, 128]);

    let png_bytes = fs::read(&png_path)?;
    let decoded_from_bytes = decode_image_bytes(&png_bytes)?;
    assert_eq!(decoded_from_bytes.data, decoded_from_file.data);

    let asset = AssetId::new("png-asset")?;
    let key = FrameCacheKey::new(asset, 0, 100);
    let mut cache = FrameCache::new(4096);
    cache.insert(key.clone(), Arc::new(decoded_from_file));
    let cached = cache.get(&key).ok_or("expected cached frame")?;
    assert_eq!(cached.to_rgba8()[0..4], [255, 10, 20, 255]);
    Ok(())
}

#[test]
fn open_sw_decodes_synthetic_rav1e_av1_stream_and_seeks() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = TempTestDir::create("av1")?;
    let ivf_path = dir.child("synthetic.ivf");

    let luma_levels = [32_u8, 128_u8, 224_u8];
    build_rav1e_av1_ivf(&ivf_path, 64, 64, &luma_levels)?;

    let info = probe_media(&ivf_path, BackendPreference::OpenSoftware)?;
    assert!(info.has_video);
    assert_eq!(info.width, Some(64));
    assert_eq!(info.height, Some(64));
    assert_eq!(info.video_codec.as_deref(), Some("av1"));

    let mut decoder = open_video_decoder(&ivf_path, BackendPreference::OpenSoftware)?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.decode_next_frame()? {
        frames.push(frame);
    }
    assert_eq!(frames.len(), 3);

    // 輝度値が段階的に増加していることを確認する (BGRA の R チャンネルで確認)。
    let r0 = frames[0].buffer.data[2];
    let r1 = frames[1].buffer.data[2];
    let r2 = frames[2].buffer.data[2];
    assert!(
        r0 < r1 && r1 < r2,
        "expected increasing luma: {r0} < {r1} < {r2}"
    );

    // 3 番目のフレーム時刻 (2 / 30 秒 ≈ 0.0667s) 付近へシークデコードできること。
    let seeked = decoder
        .decode_frame_at_seconds(2.0 / 30.0)?
        .ok_or("seeked frame missing")?;
    assert!(
        seeked.buffer.data[2] >= 180,
        "expected bright third frame after seek, got R={}",
        seeked.buffer.data[2]
    );
    Ok(())
}

#[test]
fn wav_decode_rubato_resample_and_audio_mixer_synchronize_with_clock()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempTestDir::create("audio")?;
    let wav_path = dir.child("sine_44100.wav");

    // 44.1 kHz・0.5 秒分の一定振幅トーンを生成。
    let frames_44k = 22_050_usize;
    let amplitude_i16 = 16_384_i16; // ≈ 0.5
    let samples = vec![amplitude_i16; frames_44k];
    write_pcm16_wav(&wav_path, 44_100, 1, &samples)?;

    let mut audio_dec = open_audio_decoder(&wav_path, BackendPreference::OpenSoftware)?;
    assert_eq!(audio_dec.sample_rate(), 44_100);
    assert_eq!(audio_dec.channels(), 1);
    let pcm_44k = audio_dec.decode_all_interleaved()?;
    assert_eq!(pcm_44k.frame_count(), frames_44k);

    // 44.1 kHz -> 48 kHz へ rubato リサンプリング。
    let pcm_48k = resample_interleaved_pcm(&pcm_44k, 48_000)?;
    assert_eq!(pcm_48k.sample_rate, 48_000);
    assert!(pcm_48k.frame_count() >= 23_500);

    // AudioMixer で 0 dB・中央パン・再生速度 1.0x でタイムラインへミックス。
    let mut doc = ProjectDocument::new("PhaseA4Audio");
    doc.fps = FrameRate::FPS_30;
    let asset_id = AssetId::new("wav-asset")?;
    let mut layer = Layer::new(LayerId::new("a1")?, "BGM");
    layer.clips.push(Clip {
        id: ClipId::new("clip-wav")?,
        name: "WAV".to_owned(),
        timeline_start: FrameIndex::ZERO,
        duration: FrameCount::new(15)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        midpoints: Vec::new(),
        kind: ClipKind::Audio {
            asset_id: asset_id.clone(),
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties {
                gain_db: AnimatedParameter::constant(0.0),
                pan: AnimatedParameter::constant(0.0),
                muted: false,
                ..AudioProperties::default()
            },
        },
    });
    doc.layers.push(layer);

    let mut mixer = AudioMixer::new();
    mixer.register_pcm(asset_id, Arc::new(pcm_48k));

    let mut out_stereo_pcm = Vec::new();
    mixer.mix_timeline_window(&doc, 4_800, 1_024, 48_000, &mut out_stereo_pcm);
    assert_eq!(out_stereo_pcm.len(), 2_048);
    // rubato 出力中央部は約 0.5 付近の定常値になる。
    assert!((out_stereo_pcm[100] - 0.5).abs() < 0.05);
    assert!((out_stereo_pcm[101] - 0.5).abs() < 0.05);

    // PlaybackClock がサンプル進行とタイムラインフレームを同期できること。
    let clock = PlaybackClock::new(FrameRate::FPS_30, 48_000);
    clock.update_timeline_metrics(FrameRate::FPS_30, 100);
    clock.set_playing(true);
    clock.advance_samples(16_000); // 16000 / 48000 = 1/3 秒 = 10 フレーム @ 30fps
    assert_eq!(clock.current_frame(), FrameIndex(10));
    Ok(())
}

#[test]
#[ignore = "requires real OS Windows Media Foundation decoder and default audio device"]
fn smoke_windows_mf_and_cpal_hardware_output() -> Result<(), Box<dyn std::error::Error>> {
    use sea_movie_creator::audio::playback::{SharedPlaybackState, start_audio_output};
    use std::sync::Mutex;

    let clock = Arc::new(PlaybackClock::new(FrameRate::FPS_60, 48_000));
    clock.update_timeline_metrics(FrameRate::FPS_60, 300);
    let shared = SharedPlaybackState {
        document: Arc::new(Mutex::new(ProjectDocument::new("HardwareSmoke"))),
        mixer: Arc::new(Mutex::new(AudioMixer::new())),
        clock,
    };
    let handle = start_audio_output(&shared)?;
    assert!(handle.sample_rate() >= 8_000);
    Ok(())
}
