//! Phase A-5: メディアインポート、プレビュー合成転送、再生/シーク/分割/Undo、欠損メディア検知のヘッドレス結合検証。

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{ImageFormat, Rgba, RgbaImage};
use rav1e::prelude::{
    ChromaSampling, Config, Context, EncoderConfig, EncoderStatus, Frame, PixelRange, Rational,
};
use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::time::{FrameIndex, PlaybackRate};
use sea_movie_creator::timeline::document::{AssetStatus, ClipKind};
use sea_movie_creator::ui::app_controller::{
    AppController, PlaybackState, clamp_right_panel_width, clamp_timeline_height,
};
use sea_movie_creator::ui::surface_bridge::PreviewSurfaceBridge;

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("sea_mc_phase_a5_{prefix}_{pid}_{id}"));
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

fn write_solid_png(
    path: &Path,
    width: u32,
    height: u32,
    rgba: Rgba<u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    let img = RgbaImage::from_pixel(width, height, rgba);
    img.save_with_format(path, ImageFormat::Png)?;
    Ok(())
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

fn write_synthetic_av1_ivf(
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

    let mut file = File::create(out_path)?;
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

    for pkt in &packets {
        let len_u32 = u32::try_from(pkt.data.len())?;
        file.write_all(&len_u32.to_le_bytes())?;
        file.write_all(&pkt.input_frameno.to_le_bytes())?;
        file.write_all(&pkt.data)?;
    }
    Ok(())
}

#[test]
fn import_png_and_av1_ivf_renders_preview_and_advances_playback_clock()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempTestDir::create("import_play")?;
    let png_path = dir.child("green.png");
    let ivf_path = dir.child("clip.ivf");
    write_solid_png(&png_path, 64, 64, Rgba([20, 220, 60, 255]))?;
    write_synthetic_av1_ivf(&ivf_path, 64, 64, &[32, 80, 140, 200, 240, 250])?;

    let mut controller = AppController::new();
    let _png_asset = controller.import_media_file(&png_path)?;
    assert_eq!(controller.document.assets.len(), 1);
    assert!(controller.selected_clip_id.is_some());
    assert!(controller.surface_bridge.front_image().is_some());
    let first_frame_id = controller.surface_bridge.current_frame_id();
    assert!(first_frame_id >= 1);
    assert!(controller.surface_bridge.current_dimensions().is_some());

    let _vid_asset = controller.import_media_file(&ivf_path)?;
    assert_eq!(controller.document.assets.len(), 2);
    assert_eq!(controller.document.layers.len(), 1);
    assert_eq!(controller.document.layers[0].clips.len(), 2);

    let video_clip_start = controller.document.layers[0].clips[1].timeline_start;
    let total_frames = controller.document.total_duration_frames();
    assert!(total_frames > video_clip_start.get());

    // 再生状態遷移とフレーム進行・ブリッジ世代更新
    controller.play();
    assert_eq!(controller.playback_state, PlaybackState::Playing);
    controller.tick_playback(0.5);
    assert_eq!(controller.clock.current_frame(), FrameIndex::new(30));
    assert!(controller.surface_bridge.current_frame_id() > first_frame_id);

    controller.pause();
    assert_eq!(controller.playback_state, PlaybackState::Paused);

    // 2本目の AV1 動画クリップ領域へシークしてフレーム描画を更新
    let before_seek_id = controller.surface_bridge.current_frame_id();
    controller.seek_to_frame(FrameIndex::new(video_clip_start.get() + 2));
    assert_eq!(
        controller.clock.current_frame(),
        FrameIndex::new(video_clip_start.get() + 2)
    );
    assert!(controller.surface_bridge.current_frame_id() > before_seek_id);

    controller.stop();
    assert_eq!(controller.playback_state, PlaybackState::Stopped);
    assert_eq!(controller.clock.current_frame(), FrameIndex::ZERO);
    Ok(())
}

#[test]
fn split_opacity_playback_rate_and_undo_redo_work_through_app_controller()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempTestDir::create("edit_undo")?;
    let png_path = dir.child("blue.png");
    let ivf_path = dir.child("video.ivf");
    write_solid_png(&png_path, 64, 64, Rgba([40, 100, 230, 255]))?;
    write_synthetic_av1_ivf(&ivf_path, 64, 64, &[40, 80, 120, 160, 200, 240, 250, 255])?;

    let mut controller = AppController::new();
    let _ = controller.import_media_file(&png_path)?;

    // 静止画クリップをフレーム 60 で分割し、不透明度変更・Undo/Redo を検証
    controller.seek_to_frame(FrameIndex::new(60));
    controller.split_selected_clip_at_playhead()?;
    assert_eq!(controller.document.layers[0].clips.len(), 2);

    controller.set_selected_clip_opacity(0.4)?;
    {
        let (_, selected) = controller
            .selected_clip()
            .ok_or("分割後の選択クリップが存在するはず")?;
        if let ClipKind::Image { visual, .. } = &selected.kind
            && let AnimatedParameter::Constant { value } = visual.opacity
        {
            assert!((value - 0.4).abs() < 1e-6);
        } else {
            return Err("選択クリップが定数不透明度の Image ではありません".into());
        }
    }

    assert!(controller.undo()?);
    {
        let (_, selected) = controller
            .selected_clip()
            .ok_or("Undo 後もクリップが存在するはず")?;
        if let ClipKind::Image { visual, .. } = &selected.kind
            && let AnimatedParameter::Constant { value } = visual.opacity
        {
            assert!((value - 1.0).abs() < 1e-6);
        } else {
            return Err("Undo 後の選択クリップが定数不透明度の Image ではありません".into());
        }
    }

    // 分割の Undo で 1 本へ戻る
    assert!(controller.undo()?);
    assert_eq!(controller.document.layers[0].clips.len(), 1);

    // 2 段階 Redo で分割と不透明度が再適用される
    assert!(controller.redo()?);
    assert_eq!(controller.document.layers[0].clips.len(), 2);
    assert!(controller.redo()?);
    {
        let (_, selected) = controller
            .selected_clip()
            .ok_or("Redo 後の選択クリップが存在するはず")?;
        if let ClipKind::Image { visual, .. } = &selected.kind
            && let AnimatedParameter::Constant { value } = visual.opacity
        {
            assert!((value - 0.4).abs() < 1e-6);
        } else {
            return Err("Redo 後の選択クリップが定数不透明度の Image ではありません".into());
        }
    }

    // AV1 動画クリップに対する再生速度変更 (0.5x スロー) も AppController 経由で反映
    let _ = controller.import_media_file(&ivf_path)?;
    controller.set_selected_clip_playback_rate(1, 2)?;
    let (_, video_clip) = controller
        .selected_clip()
        .ok_or("動画クリップが選択されているはず")?;
    if let ClipKind::Video { playback_rate, .. } = &video_clip.kind {
        assert_eq!(*playback_rate, PlaybackRate::new(1, 2)?);
    } else {
        return Err("選択クリップが Video ではありません".into());
    }
    Ok(())
}

#[test]
fn reopen_after_deleting_asset_marks_missing_and_toggles_media_offline_flag()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempTestDir::create("missing_reopen")?;
    let missing_png = dir.child("will_be_deleted.png");
    let valid_png = dir.child("still_present.png");
    let project_path = dir.child("project.smc.json");
    write_solid_png(&missing_png, 64, 64, Rgba([220, 40, 40, 255]))?;
    write_solid_png(&valid_png, 64, 64, Rgba([30, 180, 90, 255]))?;

    let mut controller = AppController::new();
    let _ = controller.import_media_file(&missing_png)?;
    let _ = controller.import_media_file(&valid_png)?;
    assert!(!controller.has_media_offline_at_playhead);
    controller.save_project_file(&project_path)?;

    // 1 本目の素材ファイルをディスクから削除してから別コントローラで開く
    fs::remove_file(&missing_png)?;

    let mut reopened = AppController::new();
    reopened.open_project_file(&project_path)?;
    assert_eq!(reopened.document.assets.len(), 2);
    assert!(matches!(
        reopened.document.assets[0].status,
        AssetStatus::Missing { .. }
    ));
    assert_eq!(reopened.document.assets[1].status, AssetStatus::Available);

    // フレーム 0 は欠損クリップ区間のため Media Offline が true
    reopened.seek_to_frame(FrameIndex::ZERO);
    assert!(reopened.has_media_offline_at_playhead);

    // 2 本目の有効クリップ区間へ移動すると Media Offline は false へ戻る
    let valid_start = reopened.document.layers[0].clips[1].timeline_start;
    reopened.seek_to_frame(FrameIndex::new(valid_start.get() + 10));
    assert!(!reopened.has_media_offline_at_playhead);

    // 1/5 右パネル幅とタイムライン高さのレイアウトクランプ境界も確認
    assert!((clamp_right_panel_width(1920.0, None) - 384.0).abs() < 1e-4);
    let timeline_h = clamp_timeline_height(1000.0, None);
    assert!((140.0..=650.0).contains(&timeline_h));
    Ok(())
}
