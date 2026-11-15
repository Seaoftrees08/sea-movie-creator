use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sea_movie_creator::core::animation::AnimatedParameter;
use sea_movie_creator::core::ids::{AssetId, ClipId, LayerId};
use sea_movie_creator::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration,
};
use sea_movie_creator::export::registry::{PluginRegistry, RegistryError};
use sea_movie_creator::export::runner::{
    ExportCancelToken, ExportError, ExportJobRequest, ExportMediaSources, ExportPhase,
    run_export_job,
};
use sea_movie_creator::media::decoder::{DecodedAudioPcm, FramePixelFormat, VideoFrameBuffer};
use sea_movie_creator::media::visual_properties::{SubtitleProperties, VisualProperties};
use sea_movie_creator::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind, Layer,
    ProjectDocument, ProjectResolution,
};

fn setup_dummy_plugin_dir(tag: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("sea-plugin-host-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let plugin_dir = root.join("dummy");
    fs::create_dir_all(&plugin_dir)?;
    let manifest = serde_json::json!({
        "schema_version": 1,
        "id": "sea.export.dummy",
        "name": "Dummy Protocol Plugin",
        "version": "0.1.0",
        "kind": "export",
        "entrypoint": "sea-export-dummy",
        "containers": ["json"],
        "license": {
            "spdx": "MIT OR Apache-2.0",
            "notice": "Protocol integration test dummy plugin.",
            "requires_external_binary": false
        },
        "capabilities": {
            "supports_alpha": true,
            "supports_audio": true,
            "pixel_format": "bgra8",
            "audio_sample_format": "f32_interleaved",
            "hw_accelerated": false
        },
        "presets": [
            {
                "id": "dummy_verify",
                "label": "Dummy Verify",
                "description": "IPC checksum verifier",
                "video_codec": "raw-xor",
                "audio_codec": "raw-f32",
                "container_ext": "json",
                "default_options": {}
            }
        ]
    });
    fs::write(
        plugin_dir.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(root)
}

fn build_synthetic_project()
-> Result<(ProjectDocument, AssetId, AssetId), Box<dyn std::error::Error>> {
    let mut doc = ProjectDocument::new("ExportHostTest");
    doc.fps = FrameRate::new(30, 1)?;
    doc.resolution = ProjectResolution::new(
        NonZeroU32::new(320).ok_or("non-zero")?,
        NonZeroU32::new(180).ok_or("non-zero")?,
    );

    let img_id = AssetId::new("asset-img")?;
    let bgm_id = AssetId::new("asset-bgm")?;
    doc.assets.push(Asset {
        id: img_id.clone(),
        name: "SyntheticImg".to_owned(),
        path: AssetPath::from_absolute("assets/synthetic.png"),
        kind: AssetKind::Image {
            width: 160,
            height: 90,
        },
        status: AssetStatus::Available,
    });
    doc.assets.push(Asset {
        id: bgm_id.clone(),
        name: "SyntheticTone".to_owned(),
        path: AssetPath::from_absolute("assets/tone.wav"),
        kind: AssetKind::Audio {
            sample_rate_hz: 48_000,
            channels: 2,
            source_duration: RationalDuration::from_millis(2_000),
        },
        status: AssetStatus::Available,
    });

    let visual = VisualProperties {
        opacity: AnimatedParameter::constant(0.85),
        x: AnimatedParameter::constant(12.0),
        ..VisualProperties::default()
    };
    let mut subtitle_props = SubtitleProperties::new("字幕ホストテスト");
    subtitle_props.font_size = AnimatedParameter::constant(28.0);

    doc.layers = vec![
        Layer {
            id: LayerId::new("layer-visual")?,
            name: "Visual".to_owned(),
            visible: true,
            locked: false,
            muted: false,
            clips: vec![Clip {
                id: ClipId::new("clip-img")?,
                name: "ImgClip".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: FrameCount::new(8)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: ClipKind::Image {
                    asset_id: img_id.clone(),
                    visual,
                },
            }],
        },
        Layer {
            id: LayerId::new("layer-sub")?,
            name: "Sub".to_owned(),
            visible: true,
            locked: false,
            muted: false,
            clips: vec![Clip {
                id: ClipId::new("clip-sub")?,
                name: "Title".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: FrameCount::new(8)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: ClipKind::Subtitle(subtitle_props),
            }],
        },
        Layer {
            id: LayerId::new("layer-audio")?,
            name: "Audio".to_owned(),
            visible: true,
            locked: false,
            muted: false,
            clips: vec![Clip {
                id: ClipId::new("clip-bgm")?,
                name: "Bgm".to_owned(),
                timeline_start: FrameIndex::ZERO,
                duration: FrameCount::new(8)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: ClipKind::Audio {
                    asset_id: bgm_id.clone(),
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::NORMAL,
                    preserve_pitch: true,
                    audio: AudioProperties::default(),
                },
            }],
        },
    ];
    doc.validate()?;
    Ok((doc, img_id, bgm_id))
}

fn build_synthetic_sources(img_id: AssetId, bgm_id: AssetId) -> ExportMediaSources {
    let mut sources = ExportMediaSources::new(None);
    sources.seed_frame(
        img_id,
        0,
        Arc::new(VideoFrameBuffer {
            width: 160,
            height: 90,
            format: FramePixelFormat::Bgra8,
            data: vec![0x7f; 160 * 90 * 4],
        }),
    );
    #[allow(clippy::cast_precision_loss)]
    let tone_samples: Vec<f32> = (0..(48_000 / 2))
        .flat_map(|i| {
            let phase = (i as f32) * 0.05;
            [phase.sin() * 0.4, phase.cos() * 0.4]
        })
        .collect();
    sources.seed_audio(
        bgm_id,
        Arc::new(DecodedAudioPcm {
            sample_rate: 48_000,
            channels: 2,
            interleaved: tone_samples,
        }),
    );
    sources
}

fn require_built_dummy(registry: &PluginRegistry) -> Result<PathBuf, Box<dyn std::error::Error>> {
    match registry.resolve_runnable("sea.export.dummy", "dummy_verify") {
        Ok((_, path)) => Ok(path),
        Err(RegistryError::MissingExecutable { plugin_id, entrypoint }) => Err(format!(
            "workspace plugin binary missing ({plugin_id} -> {entrypoint}); run `cargo build --workspace --all-targets` before tests"
        )
        .into()),
        Err(err) => Err(err.into()),
    }
}

#[test]
fn export_host_ipc_streams_frames_handles_cancel_and_detects_crash()
-> Result<(), Box<dyn std::error::Error>> {
    let plugin_root = setup_dummy_plugin_dir("ipc")?;
    let registry = PluginRegistry::discover_in_dirs(std::slice::from_ref(&plugin_root));
    assert_eq!(registry.plugins().len(), 1);
    let _exe = require_built_dummy(&registry)?;

    let (doc, img_id, bgm_id) = build_synthetic_project()?;
    let mut sources = build_synthetic_sources(img_id, bgm_id);
    let out_dir = plugin_root.join("out");
    let ok_path = out_dir.join("summary.json");
    let request = ExportJobRequest::full_project(
        &doc,
        "sea.export.dummy".to_owned(),
        "dummy_verify".to_owned(),
        ok_path.clone(),
        serde_json::json!({}),
    );

    let mut phases = Vec::new();
    let cancel = ExportCancelToken::new();
    let outcome = run_export_job(&doc, &registry, &request, &mut sources, &cancel, |prog| {
        phases.push(prog.phase);
    })?;
    assert_eq!(outcome.frames_encoded, 8);
    assert!(phases.contains(&ExportPhase::Preparing));
    assert!(phases.contains(&ExportPhase::Encoding));
    assert!(phases.contains(&ExportPhase::Completed));

    let summary_val: serde_json::Value = serde_json::from_str(&fs::read_to_string(&ok_path)?)?;
    assert_eq!(summary_val["frames_received"].as_u64(), Some(8));
    assert!(summary_val["audio_samples_received"].as_u64().unwrap_or(0) > 10_000);
    assert!(summary_val["audio_abs_sum"].as_f64().unwrap_or(0.0) > 1.0);
    assert!(summary_val["frame_digest"].as_u64().unwrap_or(0) != 0);

    // 中断テスト: スリープ付き実行を別スレッドからキャンセルし、残骸ファイルが残らないことを確認
    let cancel_path = out_dir.join("cancelled.json");
    let slow_req = ExportJobRequest::full_project(
        &doc,
        "sea.export.dummy".to_owned(),
        "dummy_verify".to_owned(),
        cancel_path.clone(),
        serde_json::json!({ "sleep_per_frame_ms": 35 }),
    );
    let cancel_token = ExportCancelToken::new();
    let cancel_trigger = cancel_token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        cancel_trigger.cancel();
    });
    let cancel_res = run_export_job(
        &doc,
        &registry,
        &slow_req,
        &mut sources,
        &cancel_token,
        |_| {},
    );
    assert!(matches!(cancel_res, Err(ExportError::Cancelled)));
    assert!(!cancel_path.exists());

    // 異常終了テスト: プラグインクラッシュ時にハングせずエラーを返して残骸を消すこと
    let crash_path = out_dir.join("crashed.json");
    let crash_req = ExportJobRequest::full_project(
        &doc,
        "sea.export.dummy".to_owned(),
        "dummy_verify".to_owned(),
        crash_path.clone(),
        serde_json::json!({ "simulate_crash_at_frame": 2 }),
    );
    let crash_started = Instant::now();
    let crash_outcome = run_export_job(
        &doc,
        &registry,
        &crash_req,
        &mut sources,
        &ExportCancelToken::new(),
        |_| {},
    );
    assert!(matches!(crash_outcome, Err(ExportError::Protocol { .. })));
    assert!(crash_started.elapsed() < Duration::from_secs(5));
    assert!(!crash_path.exists());

    let _ = fs::remove_dir_all(&plugin_root);
    Ok(())
}
