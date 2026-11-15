use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sea_movie_creator::core::animation::{
    AnimatedParameter, EasingCurve, ExtrapolationMode, Keyframe,
};
use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
use sea_movie_creator::core::schema::project_document_schema_json;
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::media::visual_properties::{SubtitleProperties, VisualProperties};
use sea_movie_creator::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind,
    DocumentValidationError, Layer, ProjectDocument,
};
use sea_movie_creator::timeline::serialization::{
    build_asset_path, load_project_file, save_project_file,
};

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn create(prefix: &str) -> Result<Self, std::io::Error> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "sea_movie_creator_{prefix}_{}_{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        Ok(Self { path: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn populate_sample_assets(doc: &mut ProjectDocument) -> Result<(), Box<dyn std::error::Error>> {
    doc.assets.push(Asset {
        id: AssetId::new("asset_v1")?,
        name: "intro.mkv".to_owned(),
        path: AssetPath::new(Some("media/intro.mkv".to_owned()), None),
        kind: AssetKind::Video {
            has_audio: true,
            default_fps: Some(FrameRate::FPS_29_97),
            duration_frames: Some(FrameCount::new(600)?),
            width: Some(1920),
            height: Some(1080),
            source_duration: RationalDuration::new(20, 1)?,
        },
        status: AssetStatus::Available,
    });
    doc.assets.push(Asset {
        id: AssetId::new("asset_a1")?,
        name: "bgm.flac".to_owned(),
        path: AssetPath::new(Some("media/bgm.flac".to_owned()), None),
        kind: AssetKind::Audio {
            sample_rate_hz: 48_000,
            channels: 2,
            source_duration: RationalDuration::new(30, 1)?,
        },
        status: AssetStatus::Available,
    });
    doc.assets.push(Asset {
        id: AssetId::new("asset_i1")?,
        name: "logo.png".to_owned(),
        path: AssetPath::new(Some("media/logo.png".to_owned()), None),
        kind: AssetKind::Image {
            width: 512,
            height: 512,
        },
        status: AssetStatus::Available,
    });
    Ok(())
}

fn build_sample_document() -> Result<ProjectDocument, Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("PhaseA Roundtrip Project");
    doc.fps = FrameRate::FPS_29_97;
    populate_sample_assets(&mut doc)?;

    let mut layer_video = Layer::new(LayerId::new("layer_video")?, "Video Track");
    layer_video.clips.push(Clip {
        id: ClipId::new("clip_video")?,
        name: "Intro Clip".to_owned(),
        timeline_start: FrameIndex::new(0),
        duration: FrameCount::new(150)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Video {
            asset_id: AssetId::new("asset_v1")?,
            source_in: RationalDuration::new(1, 1)?,
            playback_rate: PlaybackRate::new(3, 2)?,
            preserve_pitch: true,
            visual: VisualProperties {
                x: AnimatedParameter::Keyframes {
                    keyframes: vec![
                        Keyframe::new(0, -100.0),
                        Keyframe::with_easing(120, 100.0, EasingCurve::EaseInOut),
                    ],
                    extrapolation: ExtrapolationMode::Clamp,
                },
                ..VisualProperties::default()
            },
            audio: Some(AudioProperties::default()),
        },
    });
    layer_video.clips.push(Clip {
        id: ClipId::new("clip_logo")?,
        name: "Logo Overlay".to_owned(),
        timeline_start: FrameIndex::new(150),
        duration: FrameCount::new(90)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Image {
            asset_id: AssetId::new("asset_i1")?,
            visual: VisualProperties::default(),
        },
    });

    let mut layer_audio = Layer::new(LayerId::new("layer_audio")?, "BGM Track");
    layer_audio.clips.push(Clip {
        id: ClipId::new("clip_bgm")?,
        name: "BGM".to_owned(),
        timeline_start: FrameIndex::new(0),
        duration: FrameCount::new(240)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Audio {
            asset_id: AssetId::new("asset_a1")?,
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties::default(),
        },
    });

    let mut layer_text = Layer::new(LayerId::new("layer_text")?, "Subtitle Track");
    layer_text.clips.push(Clip {
        id: ClipId::new("clip_sub")?,
        name: "Title".to_owned(),
        timeline_start: FrameIndex::new(30),
        duration: FrameCount::new(120)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Subtitle(SubtitleProperties::new("Hello Sea Movie Creator")),
    });

    doc.layers = vec![layer_video, layer_audio, layer_text];
    doc.validate()?;
    Ok(doc)
}

#[test]
fn project_save_and_load_roundtrip_resolves_relative_and_missing_assets()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDirGuard::create("roundtrip")?;
    let media_dir = temp.path().join("media");
    fs::create_dir_all(&media_dir)?;

    let intro_file = media_dir.join("intro.mkv");
    let logo_file = media_dir.join("logo.png");
    fs::write(&intro_file, b"dummy mkv")?;
    fs::write(&logo_file, b"dummy png")?;
    // bgm.flac は意図的に作成せず Missing 状態を検証する

    let mut doc = build_sample_document()?;
    doc.assets[0].path = build_asset_path(&intro_file, Some(temp.path()));
    doc.assets[2].path = build_asset_path(&logo_file, Some(temp.path()));

    let project_file = temp.path().join("project.sea.json");
    save_project_file(&doc, &project_file)?;

    let loaded = load_project_file(&project_file)?;
    assert_eq!(loaded.name, doc.name);
    assert_eq!(loaded.fps, FrameRate::FPS_29_97);
    assert_eq!(loaded.layers.len(), 3);
    assert_eq!(loaded.assets[0].status, AssetStatus::Available);
    assert!(matches!(
        loaded.assets[1].status,
        AssetStatus::Missing { .. }
    ));
    assert_eq!(loaded.assets[2].status, AssetStatus::Available);
    assert_eq!(loaded.total_duration_frames(), 240);
    Ok(())
}

#[test]
fn moved_directory_prefers_relative_path_and_falls_back_to_absolute()
-> Result<(), Box<dyn std::error::Error>> {
    let orig_dir = TempDirGuard::create("orig")?;
    let moved_dir = TempDirGuard::create("moved")?;
    let fallback_dir = TempDirGuard::create("fallback")?;

    fs::create_dir_all(orig_dir.path().join("media"))?;
    fs::create_dir_all(moved_dir.path().join("media"))?;

    // rel_ok.png は moved_dir/media/rel_ok.png が存在する（移動先相対パス優先を確認）
    let rel_orig = orig_dir.path().join("media").join("rel_ok.png");
    let rel_moved = moved_dir.path().join("media").join("rel_ok.png");
    fs::write(&rel_orig, b"orig")?;
    fs::write(&rel_moved, b"moved")?;

    // abs_fallback.png は moved_dir には無いが絶対パスには存在する
    let abs_only = fallback_dir.path().join("abs_fallback.png");
    fs::write(&abs_only, b"fallback")?;

    let mut doc = ProjectDocument::new("Relocation Check");
    doc.assets.push(Asset {
        id: AssetId::new("a_rel")?,
        name: "rel_ok.png".to_owned(),
        path: AssetPath::new(
            Some("media/rel_ok.png".to_owned()),
            Some(rel_orig.to_string_lossy().into_owned()),
        ),
        kind: AssetKind::Image {
            width: 32,
            height: 32,
        },
        status: AssetStatus::Missing {
            expected_path: "stale".to_owned(),
        },
    });
    doc.assets.push(Asset {
        id: AssetId::new("a_abs")?,
        name: "abs_fallback.png".to_owned(),
        path: AssetPath::new(
            Some("media/abs_fallback.png".to_owned()),
            Some(abs_only.to_string_lossy().into_owned()),
        ),
        kind: AssetKind::Image {
            width: 32,
            height: 32,
        },
        status: AssetStatus::Missing {
            expected_path: "stale".to_owned(),
        },
    });

    let moved_project_file = moved_dir.path().join("moved.sea.json");
    save_project_file(&doc, &moved_project_file)?;
    fs::remove_file(&rel_orig)?;

    let loaded = load_project_file(&moved_project_file)?;
    assert_eq!(loaded.assets[0].status, AssetStatus::Available);
    assert_eq!(loaded.assets[1].status, AssetStatus::Available);
    Ok(())
}

#[test]
fn validation_rejects_playback_rate_exceeding_asset_duration()
-> Result<(), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("Rate Bound");
    doc.fps = FrameRate::FPS_30;
    doc.assets.push(Asset {
        id: AssetId::new("v1")?,
        name: "short.mkv".to_owned(),
        path: AssetPath::from_absolute("C:/short.mkv"),
        kind: AssetKind::Video {
            has_audio: false,
            default_fps: Some(FrameRate::FPS_30),
            duration_frames: Some(FrameCount::new(60)?),
            width: Some(1280),
            height: Some(720),
            source_duration: RationalDuration::new(2, 1)?,
        },
        status: AssetStatus::Available,
    });

    let mut layer = Layer::new(LayerId::new("l1")?, "V1");
    // 30fps で 40 フレームを 2.0x 再生するとソース消費時間は 40/30 * 2 = 2.667秒 > 2秒
    layer.clips.push(Clip {
        id: ClipId::new("c1")?,
        name: "Fast Clip".to_owned(),
        timeline_start: FrameIndex::new(0),
        duration: FrameCount::new(40)?,
        effect_offset_frames: 0,
        effect_duration_frames: None,
        kind: ClipKind::Video {
            asset_id: AssetId::new("v1")?,
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::new(2, 1)?,
            preserve_pitch: true,
            visual: VisualProperties::default(),
            audio: None,
        },
    });
    doc.layers.push(layer);

    assert!(matches!(
        doc.validate(),
        Err(DocumentValidationError::SourceRangeExceedsAsset { .. })
    ));
    Ok(())
}

#[test]
fn document_schema_exports_expected_definitions() -> Result<(), Box<dyn std::error::Error>> {
    let schema_json = project_document_schema_json()?;
    let parsed: serde_json::Value = serde_json::from_str(&schema_json)?;
    assert!(parsed.is_object());
    assert!(schema_json.contains("ProjectDocument"));
    assert!(schema_json.contains("AnimatedParameter"));
    assert!(schema_json.contains("AssetPath"));
    Ok(())
}
