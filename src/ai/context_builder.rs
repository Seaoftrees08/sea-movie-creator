//! 現在のタイムライン状態からコンパクトな JSON 要約と `schemars` 駆動スキーマを構築するモジュール。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ai::provider::{AiChatMessage, AiError, AiRequest};
use crate::core::animation::{AnimatedParameter, EvaluationContext};
use crate::core::ids::{ClipId, LayerId};
use crate::core::schema::edit_command_schema_json;
use crate::core::time::FrameIndex;
use crate::media::visual_properties::{BlendMode, VisualProperties};
use crate::timeline::document::{
    Asset, AssetKind, AssetStatus, AudioProperties, Clip, ClipKind, ProjectDocument,
    TimelineLengthMode,
};

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

/// クリップの現在再生位置における視覚パラメータ要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipVisualSnapshot {
    /// キャンバス中心基準 X 座標 (px)。
    pub x: f64,
    /// キャンバス中心基準 Y 座標 (px)。
    pub y: f64,
    /// X 方向拡大率。
    pub scale_x: f64,
    /// Y 方向拡大率。
    pub scale_y: f64,
    /// 時計回り回転角（度）。
    pub rotation_deg: f64,
    /// アンカーポイント X (px)。
    pub anchor_x: f64,
    /// アンカーポイント Y (px)。
    pub anchor_y: f64,
    /// 不透明度 (`0.0..=1.0`)。
    pub opacity: f64,
    /// ブレンドモード。
    pub blend_mode: BlendMode,
    /// 現在倍率を掛けたキャンバス上の描画サイズ `[width_px, height_px]`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered_size_px: Option<[f64; 2]>,
    /// 時間変化しているパラメータ一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animated_fields: Vec<String>,
}

/// クリップの現在再生位置における音声パラメータ要約。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipAudioSnapshot {
    /// ゲイン (dB)。`0.0` が原音 (`1.0x`)、`-9.54` が約 `1/3`。
    pub gain_db: f64,
    /// パン (`-1.0..=1.0`)。
    pub pan: f64,
    /// ミュート状態。
    pub muted: bool,
    /// 時間変化している音声パラメータ一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animated_fields: Vec<String>,
}

/// `@ファイル名` メンションまたはタイムラインからの添付で明示指定された対象詳細。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferencedTargetDetail {
    /// 指定元 (`"attached"` / `"mentioned"` / `"attached+mentioned"`)。
    pub source: String,
    /// マッチしたクエリやファイル名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_token: Option<String>,
    /// 対象素材 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// 対象素材名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,
    /// 素材ファイルパス文字列。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_path: Option<String>,
    /// 素材解像度 `[width, height]`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_dimensions: Option<[u32; 2]>,
    /// 所属トラック ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_id: Option<String>,
    /// 所属トラック名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// 対象クリップの要約＋評価済みプロパティ。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<ClipSummary>,
    /// 添付された生の視覚アニメーション構造（詳細プロパティ確認用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_visual: Option<VisualProperties>,
    /// 添付された生の音声アニメーション構造（詳細プロパティ確認用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_audio: Option<AudioProperties>,
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
    /// キーフレーム指定用のクリップ内可視境界 `[eff_start, eff_last]`。
    pub effect_bounds_frames: [i64; 2],
    /// イージング／数式 `p`/`d` が基準とするフレーム尺（トリム・分割後は `duration_frames` と異なる場合あり）。
    pub effect_duration_frames: u64,
    /// クリップ内中間点のエフェクト基準フレーム一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midpoints: Vec<i64>,
    /// 参照素材 ID（字幕は `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// 字幕本文の先頭抜粋（字幕クリップのみ）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_excerpt: Option<String>,
    /// 字幕フォントサイズ (px)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_font_size: Option<f64>,
    /// 現在時刻における視覚パラメータ要約（映像・画像・字幕）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visual: Option<ClipVisualSnapshot>,
    /// 現在時刻における音声パラメータ要約（音声・音声付き動画）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<ClipAudioSnapshot>,
}

/// タイムライン総尺設定の AI 要約表現。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TimelineLengthSummary {
    /// 最終クリップ末尾へ自動追従。
    AutoToLastClip,
    /// 手動指定終了フレーム。
    Manual {
        /// 手動終了フレーム番号。
        end_frame: u64,
    },
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
    /// タイムライン総尺設定 (`auto_to_last_clip` / `manual`)。
    pub timeline_length: TimelineLengthSummary,
    /// タイムライン上の選択範囲 `[start, end)`（未設定時は `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_range_frames: Option<[u64; 2]>,
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
    /// トラック・クリップ一覧。
    pub layers: Vec<LayerSummary>,
    /// `@ファイル名` メンションおよびタイムライン添付で明示された注目対象詳細。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_targets: Vec<ReferencedTargetDetail>,
}

/// ドキュメントから AI 向けのコンパクト要約構造体を生成する。
#[must_use]
pub fn build_timeline_context(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    selected_clip: Option<&ClipId>,
    selected_layer: Option<&LayerId>,
) -> TimelineContextSummary {
    build_timeline_context_with_targets(doc, playhead, selected_clip, selected_layer, &[], "")
}

/// `@<ファイル名>` メンションと添付クリップ ID を解決した拡張 AI コンテキスト要約を生成する。
#[must_use]
pub fn build_timeline_context_with_targets(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    selected_clip: Option<&ClipId>,
    selected_layer: Option<&LayerId>,
    attached_clip_ids: &[ClipId],
    user_prompt: &str,
) -> TimelineContextSummary {
    let assets: Vec<AssetSummary> = doc.assets.iter().map(summarize_asset).collect();
    let asset_dims: HashMap<&str, [u32; 2]> = assets
        .iter()
        .filter_map(|a| a.dimensions.map(|dims| (a.id.as_str(), dims)))
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
            clips: layer
                .clips
                .iter()
                .map(|clip| summarize_clip(clip, doc, playhead, &asset_dims))
                .collect(),
        })
        .collect();

    let referenced_targets =
        collect_referenced_targets(doc, playhead, &asset_dims, attached_clip_ids, user_prompt);

    #[allow(clippy::cast_precision_loss)]
    let playhead_seconds = (playhead.get() as f64) / doc.fps.as_f64();

    let timeline_length = match doc.timeline_length {
        TimelineLengthMode::AutoToLastClip => TimelineLengthSummary::AutoToLastClip,
        TimelineLengthMode::Manual { end_frame } => TimelineLengthSummary::Manual {
            end_frame: end_frame.get(),
        },
    };
    let selection_range_frames = doc
        .selection_range
        .map(|range| [range.start.get(), range.end.get()]);

    TimelineContextSummary {
        project_name: doc.name.clone(),
        resolution: [doc.resolution.width.get(), doc.resolution.height.get()],
        fps: [doc.fps.numerator(), doc.fps.denominator()],
        total_duration_frames: doc.total_duration_frames(),
        timeline_length,
        selection_range_frames,
        playhead_frame: playhead.get(),
        playhead_seconds,
        selected_layer_id: selected_layer.map(|id| id.as_str().to_owned()),
        selected_clip_id: selected_clip.map(|id| id.as_str().to_owned()),
        assets,
        layers,
        referenced_targets,
    }
}

fn summarize_asset(asset: &Asset) -> AssetSummary {
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
}

fn eval_context_clamped(
    doc: &ProjectDocument,
    clip: &Clip,
    playhead: FrameIndex,
) -> EvaluationContext {
    let end = clip
        .timeline_start
        .get()
        .saturating_add(clip.duration.get());
    let sample_frame = if playhead.get() < clip.timeline_start.get() {
        clip.timeline_start
    } else if playhead.get() >= end {
        FrameIndex::new(end.saturating_sub(1))
    } else {
        playhead
    };
    let local_frame = sample_frame.get().saturating_sub(clip.timeline_start.get());
    EvaluationContext::new(
        local_frame,
        clip.effect_offset_frames,
        clip.effective_effect_duration(),
        doc.fps,
    )
}

fn describe_animated_param(name: &str, param: &AnimatedParameter) -> Option<String> {
    match param {
        AnimatedParameter::Constant { .. } => None,
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            ..
        } => Some(format!(
            "{name}: easing({curve:?} {start_value:.1}->{end_value:.1})"
        )),
        AnimatedParameter::Keyframes { keyframes, .. } => {
            Some(format!("{name}: keyframes({}pts)", keyframes.len()))
        }
        AnimatedParameter::Expression { expr, .. } => Some(format!("{name}: expr({expr})")),
    }
}

fn build_visual_snapshot(
    visual: &VisualProperties,
    ctx: EvaluationContext,
    asset_dims: Option<[u32; 2]>,
) -> ClipVisualSnapshot {
    let eval = visual.evaluate(ctx).ok();
    let x = eval.map_or(0.0, |e| e.x);
    let y = eval.map_or(0.0, |e| e.y);
    let scale_x = eval.map_or(1.0, |e| e.scale_x);
    let scale_y = eval.map_or(1.0, |e| e.scale_y);
    let rotation_deg = eval.map_or(0.0, |e| e.rotation_deg);
    let anchor_x = eval.map_or(0.0, |e| e.anchor_x);
    let anchor_y = eval.map_or(0.0, |e| e.anchor_y);
    let opacity = eval.map_or(1.0, |e| e.opacity);
    let rendered_size_px =
        asset_dims.map(|[w, h]| [f64::from(w) * scale_x.abs(), f64::from(h) * scale_y.abs()]);
    let animated_fields = [
        describe_animated_param("x", &visual.x),
        describe_animated_param("y", &visual.y),
        describe_animated_param("scale_x", &visual.scale_x),
        describe_animated_param("scale_y", &visual.scale_y),
        describe_animated_param("rotation_deg", &visual.rotation_deg),
        describe_animated_param("anchor_x", &visual.anchor_x),
        describe_animated_param("anchor_y", &visual.anchor_y),
        describe_animated_param("opacity", &visual.opacity),
    ]
    .into_iter()
    .flatten()
    .collect();

    ClipVisualSnapshot {
        x,
        y,
        scale_x,
        scale_y,
        rotation_deg,
        anchor_x,
        anchor_y,
        opacity,
        blend_mode: visual.blend_mode,
        rendered_size_px,
        animated_fields,
    }
}

fn build_audio_snapshot(audio: &AudioProperties, ctx: EvaluationContext) -> ClipAudioSnapshot {
    let gain_db = audio.gain_db.evaluate(ctx).unwrap_or(0.0);
    let pan = audio.pan.evaluate(ctx).unwrap_or(0.0);
    let animated_fields = [
        describe_animated_param("gain_db", &audio.gain_db),
        describe_animated_param("pan", &audio.pan),
    ]
    .into_iter()
    .flatten()
    .collect();
    ClipAudioSnapshot {
        gain_db,
        pan,
        muted: audio.muted,
        animated_fields,
    }
}

fn summarize_clip(
    clip: &Clip,
    doc: &ProjectDocument,
    playhead: FrameIndex,
    asset_dims: &HashMap<&str, [u32; 2]>,
) -> ClipSummary {
    let ctx = eval_context_clamped(doc, clip, playhead);
    let (kind, asset_id, subtitle_excerpt, subtitle_font_size, visual, audio) = match &clip.kind {
        ClipKind::Video {
            asset_id,
            visual,
            audio,
            ..
        } => {
            let dims = asset_dims.get(asset_id.as_str()).copied();
            (
                "video",
                Some(asset_id.as_str().to_owned()),
                None,
                None,
                Some(build_visual_snapshot(visual, ctx, dims)),
                audio.as_ref().map(|a| build_audio_snapshot(a, ctx)),
            )
        }
        ClipKind::Audio {
            asset_id, audio, ..
        } => (
            "audio",
            Some(asset_id.as_str().to_owned()),
            None,
            None,
            None,
            Some(build_audio_snapshot(audio, ctx)),
        ),
        ClipKind::Image { asset_id, visual } => {
            let dims = asset_dims.get(asset_id.as_str()).copied();
            (
                "image",
                Some(asset_id.as_str().to_owned()),
                None,
                None,
                Some(build_visual_snapshot(visual, ctx, dims)),
                None,
            )
        }
        ClipKind::Subtitle(sub) => {
            let excerpt: String = sub.text.chars().take(40).collect();
            let font_px = sub.font_size.evaluate(ctx).ok();
            (
                "subtitle",
                None,
                Some(excerpt),
                font_px,
                Some(build_visual_snapshot(&sub.visual, ctx, None)),
                None,
            )
        }
    };
    let start = clip.timeline_start.get();
    let dur = clip.duration.get();
    let (eff_start, eff_last) = clip.effect_bounds();
    ClipSummary {
        id: clip.id.as_str().to_owned(),
        name: clip.name.clone(),
        kind: kind.to_owned(),
        timeline_start_frame: start,
        timeline_end_frame: start.saturating_add(dur),
        duration_frames: dur,
        effect_offset_frames: clip.effect_offset_frames,
        effect_bounds_frames: [eff_start, eff_last],
        effect_duration_frames: clip.effective_effect_duration().get(),
        midpoints: clip.midpoints.clone(),
        asset_id,
        subtitle_excerpt,
        subtitle_font_size,
        visual,
        audio,
    }
}

fn asset_display_path(asset: &Asset) -> Option<&str> {
    asset
        .path
        .relative
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            asset
                .path
                .absolute
                .as_deref()
                .filter(|s| !s.trim().is_empty())
        })
}

/// ユーザー入力中の `@<ファイル名>` を抽出し、プロジェクト内ファイル名と最長一致優先で切り出す。
#[must_use]
pub fn extract_at_mentions(doc: &ProjectDocument, prompt: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    for asset in &doc.assets {
        if !asset.name.trim().is_empty() {
            candidates.push(asset.name.trim().to_owned());
        }
        if let Some(p) = asset_display_path(asset)
            && let Some(fname) = std::path::Path::new(p).file_name().and_then(|n| n.to_str())
        {
            candidates.push(fname.to_owned());
        }
    }
    for layer in &doc.layers {
        for clip in &layer.clips {
            if !clip.name.trim().is_empty() {
                candidates.push(clip.name.trim().to_owned());
            }
        }
    }
    candidates.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    candidates.dedup();

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let bytes = prompt.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] != b'@' {
            idx += prompt[idx..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let rest = &prompt[idx + 1..];
        let (matched, advance) = match_single_at_token(rest, &candidates);
        if !matched.is_empty() && seen.insert(matched.to_ascii_lowercase()) {
            out.push(matched);
        }
        idx += 1 + advance;
    }
    out
}

fn match_single_at_token(rest: &str, known_names: &[String]) -> (String, usize) {
    if let Some(quote) = rest.chars().next().filter(|&c| c == '"' || c == '\'') {
        let after_q = &rest[quote.len_utf8()..];
        if let Some(end_q) = after_q.find(quote) {
            return (
                after_q[..end_q].trim().to_owned(),
                quote.len_utf8() + end_q + quote.len_utf8(),
            );
        }
    }
    let lower_rest = rest.to_ascii_lowercase();
    for cand in known_names {
        let lower_cand = cand.to_ascii_lowercase();
        if lower_rest.starts_with(&lower_cand) {
            return (cand.clone(), cand.len());
        }
    }
    let stop_chars = [
        ' ', '\t', '\r', '\n', '、', '。', ',', ';', ':', '!', '?', 'を', 'の', 'に', 'へ', 'で',
        'が', 'は', 'も', 'と',
    ];
    let token_end = rest
        .char_indices()
        .find(|(_, ch)| stop_chars.contains(ch))
        .map_or(rest.len(), |(pos, _)| pos);
    (rest[..token_end].to_owned(), token_end)
}

struct ClipTargetInput<'a> {
    doc: &'a ProjectDocument,
    layer: &'a crate::timeline::document::Layer,
    clip: &'a Clip,
    playhead: FrameIndex,
    asset_dims: &'a HashMap<&'a str, [u32; 2]>,
}

fn collect_referenced_targets(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    asset_dims: &HashMap<&str, [u32; 2]>,
    attached_clip_ids: &[ClipId],
    user_prompt: &str,
) -> Vec<ReferencedTargetDetail> {
    let mentions = extract_at_mentions(doc, user_prompt);
    let mut details: Vec<ReferencedTargetDetail> = Vec::new();
    let mut seen_clip_idx: HashMap<String, usize> = HashMap::new();

    for cid in attached_clip_ids {
        if let Some((layer, clip)) = doc.find_clip(cid) {
            let entry = build_clip_target_detail(
                &ClipTargetInput {
                    doc,
                    layer,
                    clip,
                    playhead,
                    asset_dims,
                },
                "attached".to_owned(),
                None,
            );
            seen_clip_idx.insert(clip.id.as_str().to_owned(), details.len());
            details.push(entry);
        }
    }

    for token in mentions {
        let lower = token.to_ascii_lowercase();
        let mut matched_any = false;
        for layer in &doc.layers {
            for clip in &layer.clips {
                if clip_matches_mention(doc, clip, &lower) {
                    matched_any = true;
                    if let Some(&existing_ix) = seen_clip_idx.get(clip.id.as_str()) {
                        "attached+mentioned".clone_into(&mut details[existing_ix].source);
                        details[existing_ix].matched_token = Some(token.clone());
                    } else {
                        let entry = build_clip_target_detail(
                            &ClipTargetInput {
                                doc,
                                layer,
                                clip,
                                playhead,
                                asset_dims,
                            },
                            "mentioned".to_owned(),
                            Some(token.clone()),
                        );
                        seen_clip_idx.insert(clip.id.as_str().to_owned(), details.len());
                        details.push(entry);
                    }
                }
            }
        }
        if !matched_any
            && let Some(asset) = doc.assets.iter().find(|a| asset_matches_mention(a, &lower))
        {
            details.push(ReferencedTargetDetail {
                source: "mentioned".to_owned(),
                matched_token: Some(token),
                asset_id: Some(asset.id.as_str().to_owned()),
                asset_name: Some(asset.name.clone()),
                asset_path: asset_display_path(asset).map(ToOwned::to_owned),
                asset_dimensions: asset_dims.get(asset.id.as_str()).copied(),
                layer_id: None,
                layer_name: None,
                clip: None,
                raw_visual: None,
                raw_audio: None,
            });
        }
    }

    details
}

fn asset_matches_mention(asset: &Asset, lower_query: &str) -> bool {
    if asset.name.to_ascii_lowercase() == lower_query
        || asset.id.as_str().to_ascii_lowercase() == lower_query
    {
        return true;
    }
    let Some(raw_path) = asset_display_path(asset) else {
        return false;
    };
    let path_obj = std::path::Path::new(raw_path);
    let file_eq = path_obj
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase() == lower_query);
    let stem_eq = path_obj
        .file_stem()
        .and_then(|n| n.to_str())
        .is_some_and(|s| s.to_ascii_lowercase() == lower_query);
    file_eq || stem_eq
}

fn clip_matches_mention(doc: &ProjectDocument, clip: &Clip, lower_query: &str) -> bool {
    if clip.name.to_ascii_lowercase() == lower_query
        || clip.id.as_str().to_ascii_lowercase() == lower_query
    {
        return true;
    }
    let aid = match &clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => Some(asset_id),
        ClipKind::Subtitle(_) => None,
    };
    aid.and_then(|id| doc.find_asset(id))
        .is_some_and(|asset| asset_matches_mention(asset, lower_query))
}

fn build_clip_target_detail(
    input: &ClipTargetInput<'_>,
    source: String,
    matched_token: Option<String>,
) -> ReferencedTargetDetail {
    let summary = summarize_clip(input.clip, input.doc, input.playhead, input.asset_dims);
    let asset_id_opt = match &input.clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => Some(asset_id),
        ClipKind::Subtitle(_) => None,
    };
    let asset = asset_id_opt.and_then(|id| input.doc.find_asset(id));
    let (raw_visual, raw_audio) = match &input.clip.kind {
        ClipKind::Video { visual, audio, .. } => (Some(visual.clone()), audio.clone()),
        ClipKind::Image { visual, .. } => (Some(visual.clone()), None),
        ClipKind::Audio { audio, .. } => (None, Some(audio.clone())),
        ClipKind::Subtitle(sub) => (Some(sub.visual.clone()), None),
    };
    ReferencedTargetDetail {
        source,
        matched_token,
        asset_id: asset.map(|a| a.id.as_str().to_owned()),
        asset_name: asset.map(|a| a.name.clone()),
        asset_path: asset.and_then(asset_display_path).map(ToOwned::to_owned),
        asset_dimensions: asset.and_then(|a| input.asset_dims.get(a.id.as_str()).copied()),
        layer_id: Some(input.layer.id.as_str().to_owned()),
        layer_name: Some(input.layer.name.clone()),
        clip: Some(summary),
        raw_visual,
        raw_audio,
    }
}

/// 選択中のクリップ・トラックおよび添付クリップ ID 群をまとめた AI コンテキスト指定。
#[derive(Debug, Clone, Copy, Default)]
pub struct AiSelectionContext<'a> {
    /// タイムライン上で選択中の主クリップ ID。
    pub selected_clip: Option<&'a ClipId>,
    /// タイムライン上で選択中のトラック ID。
    pub selected_layer: Option<&'a LayerId>,
    /// チャットコンポーザへ明示添付されたクリップ ID 一覧。
    pub attached_clips: &'a [ClipId],
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
        "1. Canvas coordinate origin (0, 0) is the center; +X is right ([-W/2, +W/2]), +Y is down ([-H/2, +H/2]), in canvas pixels. ",
        "For an item whose displayed size is rendered_size_px=[dw, dh] (asset_dimensions * scale), placing it flush inside canvas corners uses ",
        "right X = +(W/2 - dw/2), left X = -(W/2 - dw/2), bottom Y = +(H/2 - dh/2), top Y = -(H/2 - dh/2) (never stop at X=0 for left/right edges).\n",
        "2. Timeline clip ranges use integer frames [timeline_start_frame, timeline_end_frame). Inside a clip, animations evaluate in effect-local frame coordinates ",
        "effect_bounds_frames=[eff_start, eff_last]. For linear start-to-end clip motion, PREFER AnimatedParameter::Keyframes ",
        "with [{\"frame\": eff_start, \"value\": start_val, \"easing\": \"linear\"}, {\"frame\": eff_last, \"value\": end_val}] and extrapolation=\"clamp\" ",
        "so the animation always finishes at the exact last frame of the clip even after trimming. ",
        "Note that AnimatedParameter::Easing normalizes across [0, effect_duration_frames-1], which differs from duration_frames when preserved across cuts/trims.\n",
        "3. Audio gain uses gain_db (0.0 dB = 1.0x). Multiplying current volume by factor k adds 20*log10(k) dB to current clip.audio.gain_db (e.g. 1/3 -> -9.54 dB, 1/2 -> -6.02 dB).\n",
        "4. When creating items and referencing them in the same turn, wrap steps in {\"type\":\"batch\",\"commands\":[...]} ",
        "using temp_id (e.g. \"$c1\") with ClipRef::Temp ({\"temp\":\"$c1\"}) / LayerRef::Temp.\n",
        "5. Use SetParameter with typed ParameterValue (animated, text, bool, number, color, text_alignment, blend_mode), ",
        "SetPlaybackRate to change playback speed safely, SetTimelineLength / SetSelectionRange for project range management, ",
        "and AddMidpoint / MoveMidpoint / RemoveMidpoint (with interior effect_frame) for AviUtl-style clip segment midpoints.\n",
        "6. Only reference asset/layer/clip IDs that exist in the Current Timeline Context or were defined earlier in the same batch. ",
        "When referenced_targets is non-empty (from @<filename> mentions or user-attached timeline clips), prioritize editing those exact targets using their current visual/audio values.\n",
        "7. For questions, greetings, or checks requiring no timeline edits (such as \"ping\"), ",
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
    build_ai_request_with_attachments(
        doc,
        playhead,
        AiSelectionContext {
            selected_clip,
            selected_layer,
            attached_clips: &[],
        },
        history,
        user_prompt,
    )
}

/// 添付クリップ ID 群および `@<ファイル名>` メンション解決を含む [`AiRequest`] を組み立てる。
///
/// # Errors
///
/// コンテキストや `EditCommand` スキーマの JSON 生成に失敗した場合。
pub fn build_ai_request_with_attachments(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    selection: AiSelectionContext<'_>,
    history: &[AiChatMessage],
    user_prompt: &str,
) -> Result<AiRequest, AiError> {
    let summary = build_timeline_context_with_targets(
        doc,
        playhead,
        selection.selected_clip,
        selection.selected_layer,
        selection.attached_clips,
        user_prompt,
    );
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

    fn setup_ref_test_doc()
    -> Result<(ProjectDocument, LayerId, ClipId, ClipId), Box<dyn std::error::Error>> {
        use crate::core::ids::AssetId;
        use crate::timeline::document::{Asset, AssetPath};

        let mut doc = ProjectDocument::new("RefTest");
        let a_img = AssetId::new("a_img")?;
        let a_aud = AssetId::new("a_aud")?;
        doc.assets.push(Asset {
            id: a_img.clone(),
            name: "kaitosai.jpg".to_owned(),
            path: AssetPath::from_absolute("C:/media/kaitosai.jpg"),
            status: AssetStatus::Available,
            kind: AssetKind::Image {
                width: 800,
                height: 450,
            },
        });
        doc.assets.push(Asset {
            id: a_aud.clone(),
            name: "bgm track.wav".to_owned(),
            path: AssetPath::from_absolute("C:/media/bgm track.wav"),
            status: AssetStatus::Available,
            kind: AssetKind::Audio {
                sample_rate_hz: 48_000,
                channels: 2,
                source_duration: crate::core::time::RationalDuration::from_millis(3_000),
            },
        });

        let l1 = LayerId::new("L1")?;
        let c_img = ClipId::new("C_IMG")?;
        let c_aud = ClipId::new("C_AUD")?;
        let mut layer = Layer::new(l1.clone(), "Track 1");
        let vis = VisualProperties {
            scale_x: AnimatedParameter::constant(0.5),
            scale_y: AnimatedParameter::constant(0.5),
            ..VisualProperties::default()
        };
        layer.clips.push(Clip {
            id: c_img.clone(),
            name: "kaitosai.jpg".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(163)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: a_img,
                visual: vis,
            },
        });
        layer.clips.push(Clip {
            id: c_aud.clone(),
            name: "bgm track.wav".to_owned(),
            timeline_start: FrameIndex::new(200),
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Audio {
                asset_id: a_aud,
                audio: AudioProperties {
                    gain_db: AnimatedParameter::constant(-6.0),
                    ..AudioProperties::default()
                },
                source_in: crate::core::time::RationalDuration::ZERO,
                playback_rate: crate::core::time::PlaybackRate::NORMAL,
                preserve_pitch: true,
            },
        });
        doc.layers.push(layer);
        Ok((doc, l1, c_img, c_aud))
    }

    #[test]
    fn at_mentions_and_attached_clips_resolve_targets_and_visual_geometry()
    -> Result<(), Box<dyn std::error::Error>> {
        let (doc, l1, c_img, c_aud) = setup_ref_test_doc()?;

        let prompt = "@kaitosai.jpgを右下から左下へ。さらに@\"bgm track.wav\"を1/3音量に";
        let mentions = extract_at_mentions(&doc, prompt);
        assert_eq!(
            mentions,
            vec!["kaitosai.jpg".to_owned(), "bgm track.wav".to_owned()]
        );

        let ctx = build_timeline_context_with_targets(
            &doc,
            FrameIndex::new(10),
            Some(&c_img),
            Some(&l1),
            std::slice::from_ref(&c_aud),
            prompt,
        );
        assert_eq!(ctx.referenced_targets.len(), 2);
        let aud_target = &ctx.referenced_targets[0];
        assert_eq!(aud_target.source, "attached+mentioned");
        let img_target = &ctx.referenced_targets[1];
        assert_eq!(img_target.source, "mentioned");
        let img_clip = img_target.clip.as_ref().ok_or("missing img clip")?;
        assert_eq!(img_clip.effect_bounds_frames, [0, 162]);
        let vis_snap = img_clip.visual.as_ref().ok_or("missing visual")?;
        let rendered = vis_snap.rendered_size_px.ok_or("missing rendered size")?;
        assert!((rendered[0] - 400.0).abs() < 1e-9);
        assert!((rendered[1] - 225.0).abs() < 1e-9);
        let gain = aud_target
            .clip
            .as_ref()
            .and_then(|c| c.audio.as_ref())
            .map(|a| a.gain_db)
            .ok_or("missing audio")?;
        assert!((gain - (-6.0)).abs() < 1e-9);
        Ok(())
    }
}
