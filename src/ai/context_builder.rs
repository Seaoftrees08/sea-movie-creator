//! 現在のタイムライン状態からコンパクトな JSON 要約と `schemars` 駆動スキーマを構築するモジュール。

use serde::{Deserialize, Serialize};

use crate::ai::provider::{AiChatMessage, AiError, AiRequest};
use crate::core::ids::{ClipId, LayerId};
use crate::core::schema::edit_command_schema_json;
use crate::core::time::FrameIndex;
use crate::timeline::document::{AssetKind, AssetStatus, ClipKind, ProjectDocument};

/// AI プロンプトへ渡す素材要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetSummary {
    /// 素材 ID。
    pub id: String,
    /// 表示名。
    pub name: String,
    /// 種別（`"video"` / `"audio"` / `"image"`）。
    pub kind: String,
    /// 利用可能かどうか。
    pub available: bool,
    /// 長さ（秒、静止画は `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    /// 映像・画像の解像度 `[width, height]`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<[u32; 2]>,
}

/// AI プロンプトへ渡すクリップ要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipSummary {
    /// クリップ ID。
    pub id: String,
    /// 表示名。
    pub name: String,
    /// 種別（`"video"` / `"audio"` / `"image"` / `"subtitle"`）。
    pub kind: String,
    /// 開始フレーム番号。
    pub timeline_start_frame: u64,
    /// 終了フレーム番号（排他的）。
    pub timeline_end_frame: u64,
    /// 長さ（フレーム）。
    pub duration_frames: u64,
    /// エフェクト基準フレームオフセット。
    pub effect_offset_frames: i64,
    /// 参照素材 ID（字幕は `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// 字幕本文の先頭抜粋（字幕クリップのみ）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_excerpt: Option<String>,
}

/// AI プロンプトへ渡すレイヤー要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerSummary {
    /// レイヤー ID。
    pub id: String,
    /// レイヤー名。
    pub name: String,
    /// レイヤーインデックス（`0` が最背面）。
    pub index: usize,
    /// 可視状態。
    pub visible: bool,
    /// ロック状態。
    pub locked: bool,
    /// ミュート状態。
    pub muted: bool,
    /// レイヤー内のクリップ一覧。
    pub clips: Vec<ClipSummary>,
}

/// AI コンテキストとして渡すプロジェクト全体のコンパクト要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineContextSummary {
    /// プロジェクト名。
    pub project_name: String,
    /// キャンバス解像度 `[width, height]`。
    pub resolution: [u32; 2],
    /// FPS `[numerator, denominator]`。
    pub fps: [u32; 2],
    /// タイムライン総フレーム数。
    pub total_duration_frames: u64,
    /// 現在再生位置（フレーム）。
    pub playhead_frame: u64,
    /// 現在再生位置（秒）。
    pub playhead_seconds: f64,
    /// 選択中レイヤー ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_layer_id: Option<String>,
    /// 選択中クリップ ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_clip_id: Option<String>,
    /// 登録済み素材一覧。
    pub assets: Vec<AssetSummary>,
    /// レイヤー・クリップ一覧。
    pub layers: Vec<LayerSummary>,
}

/// ドキュメントから AI 向けのコンパクト要約構造体を生成する。
#[must_use]
pub fn build_timeline_context(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    selected_clip: Option<&ClipId>,
    selected_layer: Option<&LayerId>,
) -> TimelineContextSummary {
    let assets = doc
        .assets
        .iter()
        .map(|asset| {
            let (kind, duration_seconds, dimensions) = match &asset.kind {
                AssetKind::Video {
                    width,
                    height,
                    source_duration,
                    ..
                } => (
                    "video",
                    Some(source_duration.as_seconds_f64()),
                    width.zip(*height).map(|(w, h)| [w, h]),
                ),
                AssetKind::Audio {
                    source_duration, ..
                } => ("audio", Some(source_duration.as_seconds_f64()), None),
                AssetKind::Image { width, height } => ("image", None, Some([*width, *height])),
            };
            AssetSummary {
                id: asset.id.as_str().to_owned(),
                name: asset.name.clone(),
                kind: kind.to_owned(),
                available: asset.status == AssetStatus::Available,
                duration_seconds,
                dimensions,
            }
        })
        .collect();

    let layers = doc
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| LayerSummary {
            id: layer.id.as_str().to_owned(),
            name: layer.name.clone(),
            index,
            visible: layer.visible,
            locked: layer.locked,
            muted: layer.muted,
            clips: layer.clips.iter().map(summarize_clip).collect(),
        })
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let playhead_seconds = (playhead.get() as f64) / doc.fps.as_f64();

    TimelineContextSummary {
        project_name: doc.name.clone(),
        resolution: [doc.resolution.width.get(), doc.resolution.height.get()],
        fps: [doc.fps.numerator(), doc.fps.denominator()],
        total_duration_frames: doc.total_duration_frames(),
        playhead_frame: playhead.get(),
        playhead_seconds,
        selected_layer_id: selected_layer.map(|id| id.as_str().to_owned()),
        selected_clip_id: selected_clip.map(|id| id.as_str().to_owned()),
        assets,
        layers,
    }
}

fn summarize_clip(clip: &crate::timeline::document::Clip) -> ClipSummary {
    let (kind, asset_id, subtitle_excerpt) = match &clip.kind {
        ClipKind::Video { asset_id, .. } => ("video", Some(asset_id.as_str().to_owned()), None),
        ClipKind::Audio { asset_id, .. } => ("audio", Some(asset_id.as_str().to_owned()), None),
        ClipKind::Image { asset_id, .. } => ("image", Some(asset_id.as_str().to_owned()), None),
        ClipKind::Subtitle(sub) => {
            let excerpt: String = sub.text.chars().take(40).collect();
            ("subtitle", None, Some(excerpt))
        }
    };
    let start = clip.timeline_start.get();
    let dur = clip.duration.get();
    ClipSummary {
        id: clip.id.as_str().to_owned(),
        name: clip.name.clone(),
        kind: kind.to_owned(),
        timeline_start_frame: start,
        timeline_end_frame: start.saturating_add(dur),
        duration_frames: dur,
        effect_offset_frames: clip.effect_offset_frames,
        asset_id,
        subtitle_excerpt,
    }
}

/// `schemars` により生成される `EditCommand` の JSON Schema オブジェクトを返す。
///
/// # Errors
///
/// スキーマのシリアライズ／再パースに失敗した場合。
pub fn build_command_schema_value() -> Result<serde_json::Value, AiError> {
    let raw = edit_command_schema_json()
        .map_err(|err| AiError::ParseFailed(format!("EditCommand schema failed: {err}")))?;
    serde_json::from_str(&raw)
        .map_err(|err| AiError::ParseFailed(format!("Schema JSON re-parse failed: {err}")))
}

/// システムインストラクション文面を生成する。
#[must_use]
pub fn default_editor_system_prompt() -> String {
    concat!(
        "You are an AI assistant inside sea-movie-creator, a Rust video editor.\n",
        "Always include a valid JSON object matching the provided EditCommand JSON Schema.\n",
        "Rules:\n",
        "1. Canvas coordinate origin (0, 0) is the center; +X is right, +Y is down, in canvas pixels.\n",
        "2. Timeline positions and durations use integer frame numbers (timeline_start >= 0, duration >= 1).\n",
        "3. When creating items and referencing them in the same turn, wrap steps in {\"type\":\"batch\",\"commands\":[...]} ",
        "using temp_id (e.g. \"$c1\") with ClipRef::Temp ({\"temp\":\"$c1\"}) / LayerRef::Temp.\n",
        "4. Use SetParameter with typed ParameterValue (animated, text, bool, number, color, text_alignment, blend_mode), ",
        "and use SetPlaybackRate to change playback speed safely.\n",
        "5. Only reference asset/layer/clip IDs that exist in the Current Timeline Context or were defined earlier in the same batch.\n",
        "6. For questions, greetings, or checks requiring no timeline edits (such as \"ping\"), ",
        "give a brief answer either before a ```json code block or inside a \"message\" field, paired with {\"type\":\"batch\",\"commands\":[]}."
    )
    .to_owned()
}

/// ドキュメント状態・選択状態・会話履歴・プロンプトから [`AiRequest`] を組み立てる。
///
/// # Errors
///
/// コンテキストや `EditCommand` スキーマの JSON 生成に失敗した場合。
pub fn build_ai_request(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    selected_clip: Option<&ClipId>,
    selected_layer: Option<&LayerId>,
    history: &[AiChatMessage],
    user_prompt: &str,
) -> Result<AiRequest, AiError> {
    let summary = build_timeline_context(doc, playhead, selected_clip, selected_layer);
    let context_summary = serde_json::to_value(summary)
        .map_err(|err| AiError::ParseFailed(format!("context serialization failed: {err}")))?;
    let command_schema_json = build_command_schema_value()?;

    Ok(AiRequest {
        system_prompt: default_editor_system_prompt(),
        command_schema_json,
        context_summary,
        history: history.to_vec(),
        user_prompt: user_prompt.to_owned(),
        stream: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::{Clip, Layer};

    #[test]
    fn command_schema_includes_batch_set_parameter_and_temp_ref()
    -> Result<(), Box<dyn std::error::Error>> {
        let schema = build_command_schema_value()?;
        let schema_str = serde_json::to_string(&schema)?;
        assert!(schema_str.contains("batch"));
        assert!(schema_str.contains("set_parameter"));
        assert!(schema_str.contains("set_playback_rate"));
        assert!(schema_str.contains("temp"));
        Ok(())
    }

    #[test]
    fn timeline_context_captures_playhead_selection_and_clips()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("CtxTest");
        let layer_id = LayerId::new("L1")?;
        let clip_id = ClipId::new("C1")?;
        let mut layer = Layer::new(layer_id.clone(), "Subtitles");
        layer.clips.push(Clip {
            id: clip_id.clone(),
            name: "Intro".to_owned(),
            timeline_start: FrameIndex::new(15),
            duration: FrameCount::new(45)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("開始テロップ")),
        });
        doc.layers.push(layer);

        let ctx =
            build_timeline_context(&doc, FrameIndex::new(30), Some(&clip_id), Some(&layer_id));
        assert_eq!(ctx.playhead_frame, 30);
        assert_eq!(ctx.selected_clip_id.as_deref(), Some("C1"));
        assert_eq!(
            ctx.layers[0].clips[0].subtitle_excerpt.as_deref(),
            Some("開始テロップ")
        );
        Ok(())
    }
}
