//! 意味的構成計画 (`MovieBlueprint`) から正確なタイムライン `EditCommand::Batch` へ展開する決定論的コンパイラ。

use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ai::media_inspector::AllowedFolderScope;
use crate::core::animation::{AnimatedParameter, EasingCurve, ExtrapolationMode, Keyframe};
use crate::core::color::RgbaColor;
use crate::core::ids::{AssetRef, LayerRef};
use crate::core::time::{FrameCount, FrameIndex, PlaybackRate, RationalDuration};
use crate::media::decoder::{BackendPreference, open_audio_decoder};
use crate::media::image_loader::inspect_image_file;
use crate::media::visual_properties::{
    BlendMode, SubtitleProperties, TextAlignment, VisualProperties,
};
use crate::timeline::command::{EditCommand, NewClipKind, NewClipSpec};
use crate::timeline::document::{
    AssetKind, AssetPath, AudioProperties, ProjectDocument, TimelineLengthMode,
};

/// 構成コンパイル時に発生し得るエラー。
#[derive(Debug, Error)]
pub enum BlueprintError {
    /// スライドもタイトルカードも 1 つも指定されていない。
    #[error("blueprint must include at least one title card or slide")]
    EmptyBlueprint,
    /// 目標総尺が不正（0 以下または非有限値）。
    #[error("target_duration_seconds must be positive and finite (got {0})")]
    InvalidDuration(f64),
    /// 指定画像または音声が許可フォルダ外、または存在しない。
    #[error("media access error for `{path}`: {reason}")]
    MediaAccess {
        /// 対象パス。
        path: String,
        /// エラー理由。
        reason: String,
    },
    /// 時刻またはフレーム数の変換エラー。
    #[error("time calculation failed: {0}")]
    Time(String),
}

/// ムービー全体のフォント・配色・背景テイストを表すスタイルプリセット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovieStylePreset {
    /// 結婚式・記念日向けの温かみのある明朝系・暗め背景スタイル。
    #[default]
    WeddingWarm,
    /// クラシックで上品なフォーマル明朝スタイル。
    ClassicMincho,
    /// 旅行・日常 Vlog 向けのすっきりしたサンセリフスタイル。
    ModernClean,
}

impl MovieStylePreset {
    fn background_color(self) -> RgbaColor {
        match self {
            Self::WeddingWarm => RgbaColor::new(16, 14, 18, 255),
            Self::ClassicMincho => RgbaColor::new(10, 10, 14, 255),
            Self::ModernClean => RgbaColor::new(18, 20, 24, 255),
        }
    }

    fn default_font_family(self) -> &'static str {
        match self {
            Self::WeddingWarm | Self::ClassicMincho => "serif",
            Self::ModernClean => "sans-serif",
        }
    }

    fn text_color(self) -> RgbaColor {
        match self {
            Self::WeddingWarm => RgbaColor::new(255, 250, 244, 255),
            Self::ClassicMincho | Self::ModernClean => RgbaColor::WHITE,
        }
    }

    fn outline_color() -> RgbaColor {
        RgbaColor::new(18, 16, 20, 220)
    }
}

/// 写真に適用する Ken Burns モーションプリセット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KenBurnsMotion {
    /// ゆっくり拡大（注目・余韻）。
    #[default]
    ZoomIn,
    /// 少し引いた状態へズームアウト（広がり）。
    ZoomOut,
    /// 右から左へ緩やかにパン。
    PanLeft,
    /// 左から右へ緩やかにパン。
    PanRight,
    /// 静止表示（クロスフェードのみ）。
    Static,
}

/// オープニング／エンディング用のタイトルカード定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TitleCardSpec {
    /// メインタイトル文字列（改行可）。
    pub title: String,
    /// 補足サブタイトル文字列（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// 希望表示秒数（総尺配分時の重みとして利用。既定 `8.0`）。
    #[serde(default = "default_title_seconds")]
    pub duration_seconds: f64,
}

const fn default_title_seconds() -> f64 {
    8.0
}

/// 1 枚の写真スライドの構成仕様。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlideBlueprintSpec {
    /// 画像ファイルの絶対または正規化済みパス。
    pub image_path: String,
    /// 表示する字幕テキスト（省略・空時は字幕なし）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// 章ラベル（例: `"新郎 生い立ち"`, `"おふたりの歩み"`。レイヤー要約/クリップ名に反映）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter: Option<String>,
    /// 写真へ与える Ken Burns モーション。
    #[serde(default)]
    pub motion: KenBurnsMotion,
    /// 他スライドに対する表示尺の重み倍率（`1.0` が標準）。
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// 縦長画像を配置する際に背面へ暗転拡大画像（額縁）を敷くか（既定 `true`）。
    #[serde(default = "default_true")]
    pub portrait_backdrop: bool,
}

const fn default_weight() -> f64 {
    1.0
}

const fn default_true() -> bool {
    true
}

/// BGM 音声の配置仕様。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BgmBlueprintSpec {
    /// 音声ファイルのパス。
    pub audio_path: String,
    /// 基本音量ゲイン (dB。既定 `-4.0`)。
    #[serde(default = "default_bgm_gain_db")]
    pub gain_db: f64,
    /// 先頭フェードイン秒数（既定 `1.5`）。
    #[serde(default = "default_bgm_fade_in")]
    pub fade_in_seconds: f64,
    /// 末尾フェードアウト秒数（既定 `3.0`）。
    #[serde(default = "default_bgm_fade_out")]
    pub fade_out_seconds: f64,
    /// 曲が目標総尺より短い場合に自動ループするか（既定 `true`）。
    #[serde(default = "default_true")]
    pub loop_if_shorter: bool,
}

const fn default_bgm_gain_db() -> f64 {
    -4.0
}

const fn default_bgm_fade_in() -> f64 {
    1.5
}

const fn default_bgm_fade_out() -> f64 {
    3.0
}

const fn default_overlap_seconds() -> f64 {
    0.8
}

/// エージェント／LLM が生成する高レベルムービー構成設計図。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MovieBlueprint {
    /// プロジェクト名として設定するタイトル（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// 目標総尺（秒。例: 3 分 = `180.0`）。
    pub target_duration_seconds: f64,
    /// 連続する写真・カード間のクロスフェード重複秒数（既定 `0.8`）。
    #[serde(default = "default_overlap_seconds")]
    pub transition_overlap_seconds: f64,
    /// テロップ・背景の雰囲気プリセット。
    #[serde(default)]
    pub style: MovieStylePreset,
    /// フォントファミリの明示上書き（省略時はスタイル既定）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// オープニングタイトルカード（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_card: Option<TitleCardSpec>,
    /// 本編スライド一覧。
    #[serde(default)]
    pub slides: Vec<SlideBlueprintSpec>,
    /// エンディングタイトルカード（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending_card: Option<TitleCardSpec>,
    /// 背景 BGM 指定（任意）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgm: Option<BgmBlueprintSpec>,
}

/// コンパイル後のタイムライン構成要約統計。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlueprintCompileSummary {
    /// 最終タイムライン総フレーム数。
    pub total_frames: u64,
    /// 最終タイムライン総尺（秒）。
    pub total_seconds: f64,
    /// 配置された写真スライド数。
    pub slide_count: usize,
    /// 配置された字幕クリップ数（タイトル含む）。
    pub subtitle_count: usize,
    /// 配置された BGM 音声クリップ数。
    pub bgm_segment_count: usize,
    /// 縦長写真向けに背面暗転額縁を付与した枚数。
    pub portrait_backdrop_count: usize,
}

/// 決定論的コンパイラが生成したコマンドと要約。
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledBlueprintResult {
    /// そのまま `dry_run_validate` / `CommandDispatcher` へ渡せる `EditCommand::Batch`。
    pub command: EditCommand,
    /// 生成された構成の統計情報。
    pub summary: BlueprintCompileSummary,
}

enum VisualCardKind<'a> {
    Title(&'a TitleCardSpec),
    Slide {
        spec: &'a SlideBlueprintSpec,
        asset_temp_id: String,
        img_width: u32,
        img_height: u32,
    },
    Ending(&'a TitleCardSpec),
}

struct ScheduledCard<'a> {
    kind: VisualCardKind<'a>,
    start_frame: u64,
    slot_frames: u64,
    duration_frames: u64,
    fade_in_frames: u64,
    fade_out_frames: u64,
}

/// [`MovieBlueprint`] を現在のプロジェクト設定（解像度・FPS）に合わせて厳密な [`EditCommand::Batch`] へコンパイルする。
///
/// `scope` を渡した場合、すべての画像・音声パスが許可フォルダ境界内に存在することを強制検証します。
///
/// # Errors
///
/// 目標尺不正、カード空、許可外パス・画像/音声メタ読取失敗・時間計算エラー時に [`BlueprintError`] を返す。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn compile_blueprint_to_edit_command(
    doc: &ProjectDocument,
    blueprint: &MovieBlueprint,
    scope: Option<&AllowedFolderScope>,
) -> Result<CompiledBlueprintResult, BlueprintError> {
    if !blueprint.target_duration_seconds.is_finite() || blueprint.target_duration_seconds <= 0.0 {
        return Err(BlueprintError::InvalidDuration(
            blueprint.target_duration_seconds,
        ));
    }
    let fps = doc.fps.as_f64();
    let canvas_w = f64::from(doc.resolution.width.get());
    let canvas_h = f64::from(doc.resolution.height.get());

    let mut commands = Vec::new();
    commands.push(EditCommand::SetProjectMeta {
        name: blueprint.project_name.clone(),
        resolution: None,
        background_color: Some(blueprint.style.background_color()),
    });

    let raw_cards = collect_blueprint_cards(blueprint, scope, &mut commands)?;
    if raw_cards.is_empty() {
        return Err(BlueprintError::EmptyBlueprint);
    }

    let min_frames = u64::try_from(raw_cards.len()).unwrap_or(1);
    let target_total_frames =
        ((blueprint.target_duration_seconds * fps).round() as u64).max(min_frames);

    // スライド重みとタイトル希望秒数を同一スケールの重みとして合計 target_total_frames へ配分
    let scheduled = schedule_cards(
        raw_cards,
        target_total_frames,
        blueprint.transition_overlap_seconds,
        fps,
    );

    let has_backdrops = scheduled.iter().any(|card| {
        matches!(
            card.kind,
            VisualCardKind::Slide {
                spec,
                img_width,
                img_height,
                ..
            } if spec.portrait_backdrop && is_portrait_relative_to_canvas(img_width, img_height, canvas_w, canvas_h)
        )
    });

    let layer_ids = BlueprintLayerIds::default();
    append_blueprint_layers(
        &layer_ids,
        blueprint.bgm.is_some(),
        has_backdrops,
        &mut commands,
    );

    let font_family = blueprint
        .font_family
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| blueprint.style.default_font_family())
        .to_owned();

    let card_counts = append_scheduled_cards_commands(
        &scheduled,
        &layer_ids,
        &font_family,
        blueprint.style,
        (canvas_w, canvas_h),
        &mut commands,
    )?;

    let bgm_segment_count = if let Some(bgm) = &blueprint.bgm {
        append_bgm_commands(
            bgm,
            layer_ids.bgm,
            target_total_frames,
            fps,
            scope,
            &mut commands,
        )?
    } else {
        0
    };

    let manual_len =
        FrameCount::new(target_total_frames).map_err(|e| BlueprintError::Time(e.to_string()))?;
    commands.push(EditCommand::SetTimelineLength {
        mode: TimelineLengthMode::Manual {
            end_frame: manual_len,
        },
    });

    Ok(CompiledBlueprintResult {
        command: EditCommand::Batch { commands },
        summary: BlueprintCompileSummary {
            total_frames: target_total_frames,
            #[allow(clippy::cast_precision_loss)]
            total_seconds: (target_total_frames as f64) / fps,
            slide_count: card_counts.slides,
            subtitle_count: card_counts.subtitles,
            bgm_segment_count,
            portrait_backdrop_count: card_counts.portrait_backdrops,
        },
    })
}

struct BlueprintLayerIds {
    bgm: &'static str,
    bg_a: &'static str,
    bg_b: &'static str,
    photo_a: &'static str,
    photo_b: &'static str,
    sub: &'static str,
}

impl Default for BlueprintLayerIds {
    fn default() -> Self {
        Self {
            bgm: "$bp_layer_bgm",
            bg_a: "$bp_layer_backdrop_a",
            bg_b: "$bp_layer_backdrop_b",
            photo_a: "$bp_layer_photo_a",
            photo_b: "$bp_layer_photo_b",
            sub: "$bp_layer_subtitles",
        }
    }
}

struct ScheduledCardCounts {
    slides: usize,
    subtitles: usize,
    portrait_backdrops: usize,
}

fn collect_blueprint_cards<'a>(
    blueprint: &'a MovieBlueprint,
    scope: Option<&AllowedFolderScope>,
    commands: &mut Vec<EditCommand>,
) -> Result<Vec<(VisualCardKind<'a>, f64)>, BlueprintError> {
    let mut raw_cards: Vec<(VisualCardKind<'a>, f64)> = Vec::new();
    if let Some(title) = &blueprint.title_card {
        raw_cards.push((
            VisualCardKind::Title(title),
            title.duration_seconds.max(1.0),
        ));
    }

    let mut registered_images: HashMap<String, (String, u32, u32)> = HashMap::new();
    for (idx, slide) in blueprint.slides.iter().enumerate() {
        let resolved_path = resolve_allowed_file(Path::new(&slide.image_path), scope)?;
        let key = resolved_path.to_string_lossy().into_owned();
        let (asset_temp_id, img_w, img_h) = if let Some(existing) = registered_images.get(&key) {
            existing.clone()
        } else {
            let info =
                inspect_image_file(&resolved_path).map_err(|err| BlueprintError::MediaAccess {
                    path: slide.image_path.clone(),
                    reason: err.to_string(),
                })?;
            let temp_id = format!("$bp_img_{}", registered_images.len() + 1);
            let file_name = resolved_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image")
                .to_owned();
            commands.push(EditCommand::AddAsset {
                temp_id: Some(temp_id.clone()),
                id: None,
                name: format!("{file_name}#{idx}"),
                path: AssetPath::from_absolute(key.clone()),
                kind: AssetKind::Image {
                    width: info.width,
                    height: info.height,
                },
            });
            let entry = (temp_id, info.width.max(1), info.height.max(1));
            registered_images.insert(key, entry.clone());
            entry
        };
        let weight = if slide.weight.is_finite() && slide.weight > 0.0 {
            slide.weight
        } else {
            1.0
        };
        raw_cards.push((
            VisualCardKind::Slide {
                spec: slide,
                asset_temp_id,
                img_width: img_w,
                img_height: img_h,
            },
            weight,
        ));
    }

    if let Some(ending) = &blueprint.ending_card {
        raw_cards.push((
            VisualCardKind::Ending(ending),
            ending.duration_seconds.max(1.0),
        ));
    }
    Ok(raw_cards)
}

fn append_blueprint_layers(
    layers: &BlueprintLayerIds,
    has_bgm: bool,
    has_backdrops: bool,
    commands: &mut Vec<EditCommand>,
) {
    // レイヤー作成順（最背面 BGM -> Backdrop A/B -> Photo A/B -> Subtitles 最前面）
    let mut push_layer = |temp_id: &str, name: &str| {
        commands.push(EditCommand::AddLayer {
            temp_id: Some(temp_id.to_owned()),
            id: None,
            name: name.to_owned(),
            insert_index: None,
        });
    };
    if has_bgm {
        push_layer(layers.bgm, "BGM");
    }
    if has_backdrops {
        push_layer(layers.bg_a, "Backdrop A");
        push_layer(layers.bg_b, "Backdrop B");
    }
    push_layer(layers.photo_a, "Photos A");
    push_layer(layers.photo_b, "Photos B");
    push_layer(layers.sub, "Subtitles");
}

fn append_scheduled_cards_commands(
    scheduled: &[ScheduledCard<'_>],
    layers: &BlueprintLayerIds,
    font_family: &str,
    style: MovieStylePreset,
    canvas_size: (f64, f64),
    commands: &mut Vec<EditCommand>,
) -> Result<ScheduledCardCounts, BlueprintError> {
    let mut counts = ScheduledCardCounts {
        slides: 0,
        subtitles: 0,
        portrait_backdrops: 0,
    };

    for (idx, card) in scheduled.iter().enumerate() {
        let is_even = idx % 2 == 0;
        let photo_layer = if is_even {
            layers.photo_a
        } else {
            layers.photo_b
        };
        let bg_layer = if is_even { layers.bg_a } else { layers.bg_b };

        match &card.kind {
            VisualCardKind::Title(title) | VisualCardKind::Ending(title) => {
                let is_title = matches!(card.kind, VisualCardKind::Title(_));
                append_title_card_command(
                    title,
                    is_title,
                    card,
                    layers.sub,
                    font_family,
                    style,
                    canvas_size.1,
                    commands,
                    &mut counts,
                )?;
            }
            VisualCardKind::Slide {
                spec,
                asset_temp_id,
                img_width,
                img_height,
            } => {
                append_slide_card_commands(
                    spec,
                    asset_temp_id,
                    (*img_width, *img_height),
                    card,
                    (photo_layer, bg_layer, layers.sub),
                    font_family,
                    style,
                    canvas_size,
                    commands,
                    &mut counts,
                )?;
            }
        }
    }
    Ok(counts)
}

#[allow(clippy::too_many_arguments)]
fn append_title_card_command(
    title: &TitleCardSpec,
    is_title: bool,
    card: &ScheduledCard<'_>,
    sub_layer: &str,
    font_family: &str,
    style: MovieStylePreset,
    canvas_h: f64,
    commands: &mut Vec<EditCommand>,
    counts: &mut ScheduledCardCounts,
) -> Result<(), BlueprintError> {
    let full_text = match &title.subtitle {
        Some(sub) if !sub.trim().is_empty() => {
            format!("{}\n{}", title.title.trim(), sub.trim())
        }
        _ => title.title.trim().to_owned(),
    };
    if full_text.is_empty() {
        return Ok(());
    }
    let sub_dur = FrameCount::new(card.slot_frames.max(1))
        .map_err(|e| BlueprintError::Time(e.to_string()))?;
    let sub_props = build_title_subtitle_props(
        &full_text,
        font_family,
        style,
        canvas_h,
        (sub_dur.get(), card.fade_in_frames, card.fade_out_frames),
    );
    commands.push(EditCommand::AddClip {
        temp_id: None,
        id: None,
        layer_id: LayerRef::Temp(sub_layer.to_owned()),
        clip: Box::new(NewClipSpec {
            name: if is_title { "Opening" } else { "Ending" }.to_owned(),
            timeline_start: FrameIndex::new(card.start_frame),
            duration: sub_dur,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: NewClipKind::Subtitle(sub_props),
        }),
    });
    counts.subtitles += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_slide_card_commands(
    spec: &SlideBlueprintSpec,
    asset_temp_id: &str,
    img_size: (u32, u32),
    card: &ScheduledCard<'_>,
    target_layers: (&str, &str, &str),
    font_family: &str,
    style: MovieStylePreset,
    canvas_size: (f64, f64),
    commands: &mut Vec<EditCommand>,
    counts: &mut ScheduledCardCounts,
) -> Result<(), BlueprintError> {
    let (img_width, img_height) = img_size;
    let (canvas_w, canvas_h) = canvas_size;
    let (photo_layer, bg_layer, sub_layer) = target_layers;
    let dur =
        FrameCount::new(card.duration_frames).map_err(|e| BlueprintError::Time(e.to_string()))?;

    counts.slides += 1;
    let slide_num = counts.slides;
    let is_portrait = is_portrait_relative_to_canvas(img_width, img_height, canvas_w, canvas_h);
    if spec.portrait_backdrop && is_portrait {
        counts.portrait_backdrops += 1;
        let bg_visual = build_backdrop_visual(img_width, img_height, canvas_size, card);
        commands.push(EditCommand::AddClip {
            temp_id: None,
            id: None,
            layer_id: LayerRef::Temp(bg_layer.to_owned()),
            clip: Box::new(NewClipSpec {
                name: format!("Backdrop {slide_num}"),
                timeline_start: FrameIndex::new(card.start_frame),
                duration: dur,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: NewClipKind::Image {
                    asset_id: AssetRef::Temp(asset_temp_id.to_owned()),
                    visual: bg_visual,
                },
            }),
        });
    }

    let fg_visual = build_slide_visual(img_size, canvas_size, is_portrait, spec.motion, card);
    let clip_label = match &spec.chapter {
        Some(ch) if !ch.trim().is_empty() => format!("[{}] #{slide_num}", ch.trim()),
        _ => format!("Slide #{slide_num}"),
    };
    commands.push(EditCommand::AddClip {
        temp_id: None,
        id: None,
        layer_id: LayerRef::Temp(photo_layer.to_owned()),
        clip: Box::new(NewClipSpec {
            name: clip_label,
            timeline_start: FrameIndex::new(card.start_frame),
            duration: dur,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: NewClipKind::Image {
                asset_id: AssetRef::Temp(asset_temp_id.to_owned()),
                visual: fg_visual,
            },
        }),
    });

    if let Some(sub_raw) = spec.subtitle.as_deref().map(str::trim)
        && !sub_raw.is_empty()
    {
        let (sub_start, sub_dur) = compute_subtitle_window(card);
        if let Ok(sub_dur_fc) = FrameCount::new(sub_dur) {
            let sub_props =
                build_slide_subtitle_props(sub_raw, font_family, style, canvas_h, sub_dur_fc.get());
            commands.push(EditCommand::AddClip {
                temp_id: None,
                id: None,
                layer_id: LayerRef::Temp(sub_layer.to_owned()),
                clip: Box::new(NewClipSpec {
                    name: format!("Sub #{slide_num}"),
                    timeline_start: FrameIndex::new(sub_start),
                    duration: sub_dur_fc,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: NewClipKind::Subtitle(sub_props),
                }),
            });
            counts.subtitles += 1;
        }
    }
    Ok(())
}

fn resolve_allowed_file(
    raw: &Path,
    scope: Option<&AllowedFolderScope>,
) -> Result<std::path::PathBuf, BlueprintError> {
    if let Some(guard) = scope {
        guard
            .validate_path(raw)
            .map_err(|err| BlueprintError::MediaAccess {
                path: raw.display().to_string(),
                reason: err.to_string(),
            })
    } else if raw.is_file() {
        Ok(raw.to_path_buf())
    } else {
        Err(BlueprintError::MediaAccess {
            path: raw.display().to_string(),
            reason: "file does not exist".to_owned(),
        })
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn schedule_cards(
    raw_cards: Vec<(VisualCardKind<'_>, f64)>,
    target_total_frames: u64,
    overlap_seconds: f64,
    fps: f64,
) -> Vec<ScheduledCard<'_>> {
    let n = raw_cards.len();
    let weights: Vec<f64> = raw_cards
        .iter()
        .map(|(kind, w)| match kind {
            // Title/Ending の秒数指定はスライド全体の標準尺に対して合理的な比率へ変換
            VisualCardKind::Title(_) | VisualCardKind::Ending(_) => {
                let avg_slide_sec = ((target_total_frames as f64) / fps) / (n.max(1) as f64);
                (*w / avg_slide_sec.max(1.0)).clamp(0.4, 3.0)
            }
            VisualCardKind::Slide { .. } => *w,
        })
        .collect();

    let total_weight: f64 = weights.iter().sum::<f64>().max(1e-6);
    let mut slots: Vec<u64> = weights
        .iter()
        .map(|w| (((target_total_frames as f64) * (*w / total_weight)).round() as u64).max(1))
        .collect();

    // 合計を target_total_frames へ厳密一致させる（過剰は長い要素から削減、不足は末尾へ加算）
    let mut sum: u64 = slots.iter().sum();
    while sum > target_total_frames {
        if let Some((idx, _)) = slots
            .iter()
            .enumerate()
            .filter(|(_, val)| **val > 1)
            .max_by_key(|(_, val)| **val)
        {
            slots[idx] -= 1;
            sum -= 1;
        } else {
            break;
        }
    }
    if sum < target_total_frames
        && let Some(last) = slots.last_mut()
    {
        *last += target_total_frames - sum;
    }

    let desired_overlap = if overlap_seconds.is_finite() && overlap_seconds > 0.0 {
        (overlap_seconds * fps).round() as u64
    } else {
        0
    };

    let mut out = Vec::with_capacity(n);
    let mut cursor = 0_u64;
    let mut prev_overlap = 0_u64;

    for (idx, (kind, _)) in raw_cards.into_iter().enumerate() {
        let slot = slots[idx];
        let next_overlap = if idx + 1 < n {
            desired_overlap.min(slot / 2).min(slots[idx + 1] / 2)
        } else {
            0
        };
        let duration = slot.saturating_add(next_overlap);
        let fade_in = if idx == 0 {
            desired_overlap.min(slot / 3)
        } else {
            prev_overlap
        };
        let fade_out = if idx + 1 == n {
            desired_overlap.min(slot / 3)
        } else {
            next_overlap
        };

        out.push(ScheduledCard {
            kind,
            start_frame: cursor,
            slot_frames: slot,
            duration_frames: duration,
            fade_in_frames: fade_in,
            fade_out_frames: fade_out,
        });
        cursor = cursor.saturating_add(slot);
        prev_overlap = next_overlap;
    }
    out
}

fn is_portrait_relative_to_canvas(img_w: u32, img_h: u32, canvas_w: f64, canvas_h: f64) -> bool {
    let img_ar = f64::from(img_w.max(1)) / f64::from(img_h.max(1));
    let canvas_ar = canvas_w / canvas_h.max(1.0);
    img_ar < canvas_ar * 0.85
}

fn build_fade_opacity(
    dur: u64,
    fade_in: u64,
    fade_out: u64,
    peak_opacity: f64,
) -> AnimatedParameter {
    if dur <= 2 || (fade_in == 0 && fade_out == 0) {
        return AnimatedParameter::constant(peak_opacity);
    }
    let last = i64::try_from(dur.saturating_sub(1)).unwrap_or(0);
    let in_end = i64::try_from(fade_in.min(dur / 2)).unwrap_or(0);
    let out_start = last.saturating_sub(i64::try_from(fade_out.min(dur / 2)).unwrap_or(0));

    let mut kfs = Vec::new();
    if in_end > 0 {
        kfs.push(Keyframe::with_easing(0, 0.0, EasingCurve::EaseInOut));
        kfs.push(Keyframe::new(in_end, peak_opacity));
    } else {
        kfs.push(Keyframe::new(0, peak_opacity));
    }
    if out_start > in_end && fade_out > 0 {
        kfs.push(Keyframe::with_easing(
            out_start,
            peak_opacity,
            EasingCurve::EaseInOut,
        ));
        kfs.push(Keyframe::new(last, 0.0));
    } else if fade_out > 0 && last > in_end {
        kfs.push(Keyframe::new(last, 0.0));
    }
    AnimatedParameter::Keyframes {
        keyframes: kfs,
        extrapolation: ExtrapolationMode::Clamp,
    }
}

fn build_backdrop_visual(
    img_w: u32,
    img_h: u32,
    canvas_size: (f64, f64),
    card: &ScheduledCard<'_>,
) -> VisualProperties {
    let (canvas_w, canvas_h) = canvas_size;
    let cover_scale =
        (canvas_w / f64::from(img_w.max(1))).max(canvas_h / f64::from(img_h.max(1))) * 1.06;
    VisualProperties {
        scale_x: AnimatedParameter::constant(cover_scale),
        scale_y: AnimatedParameter::constant(cover_scale),
        opacity: build_fade_opacity(
            card.duration_frames,
            card.fade_in_frames,
            card.fade_out_frames,
            0.38,
        ),
        brightness: AnimatedParameter::constant(-0.42),
        saturation: AnimatedParameter::constant(0.75),
        ..VisualProperties::default()
    }
}

fn build_slide_visual(
    img_size: (u32, u32),
    canvas_size: (f64, f64),
    is_portrait: bool,
    motion: KenBurnsMotion,
    card: &ScheduledCard<'_>,
) -> VisualProperties {
    let (img_w, img_h) = img_size;
    let (canvas_w, canvas_h) = canvas_size;
    let dur = card.duration_frames;
    let fit_scale = (canvas_w / f64::from(img_w.max(1))).min(canvas_h / f64::from(img_h.max(1)));
    let base_scale = if is_portrait {
        fit_scale * 0.94
    } else {
        fit_scale * 1.04
    };
    let last = i64::try_from(dur.saturating_sub(1)).unwrap_or(0);
    let zoom_hi = base_scale * 1.08;
    let pan_shift = (canvas_w * 0.035).min(64.0);

    let (scale_param, x_param) = match motion {
        KenBurnsMotion::ZoomIn if last > 0 => (
            AnimatedParameter::Keyframes {
                keyframes: vec![
                    Keyframe::with_easing(0, base_scale, EasingCurve::Linear),
                    Keyframe::new(last, zoom_hi),
                ],
                extrapolation: ExtrapolationMode::Clamp,
            },
            AnimatedParameter::constant(0.0),
        ),
        KenBurnsMotion::ZoomOut if last > 0 => (
            AnimatedParameter::Keyframes {
                keyframes: vec![
                    Keyframe::with_easing(0, zoom_hi, EasingCurve::Linear),
                    Keyframe::new(last, base_scale),
                ],
                extrapolation: ExtrapolationMode::Clamp,
            },
            AnimatedParameter::constant(0.0),
        ),
        KenBurnsMotion::PanLeft if last > 0 => (
            AnimatedParameter::constant(zoom_hi),
            AnimatedParameter::Keyframes {
                keyframes: vec![
                    Keyframe::with_easing(0, pan_shift, EasingCurve::Linear),
                    Keyframe::new(last, -pan_shift),
                ],
                extrapolation: ExtrapolationMode::Clamp,
            },
        ),
        KenBurnsMotion::PanRight if last > 0 => (
            AnimatedParameter::constant(zoom_hi),
            AnimatedParameter::Keyframes {
                keyframes: vec![
                    Keyframe::with_easing(0, -pan_shift, EasingCurve::Linear),
                    Keyframe::new(last, pan_shift),
                ],
                extrapolation: ExtrapolationMode::Clamp,
            },
        ),
        // KenBurnsMotion::Static および 1 フレームのみのカードはキーフレームを張らず静止表示
        KenBurnsMotion::Static
        | KenBurnsMotion::ZoomIn
        | KenBurnsMotion::ZoomOut
        | KenBurnsMotion::PanLeft
        | KenBurnsMotion::PanRight => (
            AnimatedParameter::constant(base_scale),
            AnimatedParameter::constant(0.0),
        ),
    };

    VisualProperties {
        x: x_param,
        scale_x: scale_param.clone(),
        scale_y: scale_param,
        opacity: build_fade_opacity(dur, card.fade_in_frames, card.fade_out_frames, 1.0),
        blend_mode: BlendMode::Normal,
        ..VisualProperties::default()
    }
}

fn compute_subtitle_window(card: &ScheduledCard<'_>) -> (u64, u64) {
    let pad_in = card.fade_in_frames.min(card.slot_frames / 4);
    let pad_out = (card.slot_frames / 10).min(8);
    let start = card.start_frame.saturating_add(pad_in);
    let available = card
        .slot_frames
        .saturating_sub(pad_in.saturating_add(pad_out));
    (start, available.max(1))
}

fn build_title_subtitle_props(
    text: &str,
    font_family: &str,
    style: MovieStylePreset,
    canvas_h: f64,
    fade_window: (u64, u64, u64),
) -> SubtitleProperties {
    let (dur, fade_in, fade_out) = fade_window;
    SubtitleProperties {
        text: text.to_owned(),
        font_family: font_family.to_owned(),
        font_size: AnimatedParameter::constant((canvas_h * 0.064).clamp(24.0, 120.0)),
        text_color: style.text_color(),
        outline_color: MovieStylePreset::outline_color(),
        outline_width: 3.0,
        alignment: TextAlignment::Center,
        visual: VisualProperties {
            opacity: build_fade_opacity(dur, fade_in, fade_out, 1.0),
            ..VisualProperties::default()
        },
    }
}

fn build_slide_subtitle_props(
    text: &str,
    font_family: &str,
    style: MovieStylePreset,
    canvas_h: f64,
    dur: u64,
) -> SubtitleProperties {
    let fade = (dur / 6).min(12);
    SubtitleProperties {
        text: text.to_owned(),
        font_family: font_family.to_owned(),
        font_size: AnimatedParameter::constant((canvas_h * 0.040).clamp(18.0, 80.0)),
        text_color: style.text_color(),
        outline_color: MovieStylePreset::outline_color(),
        outline_width: 2.5,
        alignment: TextAlignment::Center,
        visual: VisualProperties {
            y: AnimatedParameter::constant(canvas_h * 0.36),
            opacity: build_fade_opacity(dur, fade, fade, 1.0),
            ..VisualProperties::default()
        },
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn append_bgm_commands(
    bgm: &BgmBlueprintSpec,
    layer_temp_id: &str,
    target_total_frames: u64,
    fps: f64,
    scope: Option<&AllowedFolderScope>,
    commands: &mut Vec<EditCommand>,
) -> Result<usize, BlueprintError> {
    let resolved = resolve_allowed_file(Path::new(&bgm.audio_path), scope)?;
    let dec = open_audio_decoder(&resolved, BackendPreference::Auto).map_err(|err| {
        BlueprintError::MediaAccess {
            path: bgm.audio_path.clone(),
            reason: err.to_string(),
        }
    })?;
    let sample_rate_hz = dec.sample_rate().max(1);
    let channels = dec.channels().max(1);
    let duration_sec = dec.duration_seconds().unwrap_or(300.0).max(0.1);
    let source_duration = RationalDuration::from_millis((duration_sec * 1000.0).round() as u64);
    let max_asset_frames = ((duration_sec * fps).floor() as u64).max(1);

    let asset_temp_id = "$bp_bgm_1".to_owned();
    let file_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bgm")
        .to_owned();
    commands.push(EditCommand::AddAsset {
        temp_id: Some(asset_temp_id.clone()),
        id: None,
        name: file_name,
        path: AssetPath::from_absolute(resolved.to_string_lossy().into_owned()),
        kind: AssetKind::Audio {
            sample_rate_hz,
            channels,
            source_duration,
        },
    });

    let base_gain = bgm.gain_db.clamp(-40.0, 12.0);
    let fade_in_frames = (bgm.fade_in_seconds.max(0.0) * fps).round() as u64;
    let fade_out_frames = (bgm.fade_out_seconds.max(0.0) * fps).round() as u64;

    let mut cursor = 0_u64;
    let mut segments = 0_usize;
    while cursor < target_total_frames {
        let remaining = target_total_frames - cursor;
        let seg_frames = remaining.min(max_asset_frames);
        let is_first = segments == 0;
        let is_last = cursor + seg_frames >= target_total_frames || !bgm.loop_if_shorter;
        let gain_param = build_bgm_gain_curve(
            seg_frames,
            base_gain,
            if is_first { fade_in_frames } else { 0 },
            if is_last { fade_out_frames } else { 0 },
        );
        let dur_fc =
            FrameCount::new(seg_frames).map_err(|e| BlueprintError::Time(e.to_string()))?;
        commands.push(EditCommand::AddClip {
            temp_id: None,
            id: None,
            layer_id: LayerRef::Temp(layer_temp_id.to_owned()),
            clip: Box::new(NewClipSpec {
                name: format!("BGM #{}", segments + 1),
                timeline_start: FrameIndex::new(cursor),
                duration: dur_fc,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: NewClipKind::Audio {
                    asset_id: AssetRef::Temp(asset_temp_id.clone()),
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::NORMAL,
                    preserve_pitch: true,
                    audio: AudioProperties {
                        gain_db: gain_param,
                        ..AudioProperties::default()
                    },
                },
            }),
        });
        segments += 1;
        cursor += seg_frames;
        if !bgm.loop_if_shorter {
            break;
        }
    }
    Ok(segments)
}

fn build_bgm_gain_curve(
    dur: u64,
    base_gain_db: f64,
    fade_in: u64,
    fade_out: u64,
) -> AnimatedParameter {
    if dur <= 2 || (fade_in == 0 && fade_out == 0) {
        return AnimatedParameter::constant(base_gain_db);
    }
    let last = i64::try_from(dur.saturating_sub(1)).unwrap_or(0);
    let in_end = i64::try_from(fade_in.min(dur / 3)).unwrap_or(0);
    let out_start = last.saturating_sub(i64::try_from(fade_out.min(dur / 3)).unwrap_or(0));

    let mut kfs = Vec::new();
    if in_end > 0 {
        kfs.push(Keyframe::with_easing(0, -40.0, EasingCurve::EaseOut));
        kfs.push(Keyframe::new(in_end, base_gain_db));
    } else {
        kfs.push(Keyframe::new(0, base_gain_db));
    }
    if out_start > in_end && fade_out > 0 {
        kfs.push(Keyframe::with_easing(
            out_start,
            base_gain_db,
            EasingCurve::EaseIn,
        ));
        kfs.push(Keyframe::new(last, -45.0));
    }
    AnimatedParameter::Keyframes {
        keyframes: kfs,
        extrapolation: ExtrapolationMode::Clamp,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use image::{ImageFormat, Rgba, RgbaImage};

    use super::*;
    use crate::core::time::FrameRate;
    use crate::timeline::dispatcher::CommandDispatcher;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("sea_blueprint_{label}_{}_{id}", std::process::id()));
            fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_img(path: &Path, w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
        let mut img = RgbaImage::new(w, h);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = Rgba([100, 150, 200, 255]);
        }
        let mut buf = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, ImageFormat::Png)?;
        fs::write(path, buf.into_inner())?;
        Ok(())
    }

    fn write_wav(path: &Path, sample_rate: u32, samples: usize) -> Result<(), std::io::Error> {
        let data_bytes = u32::try_from(samples * 2).unwrap_or(0);
        let riff_size = data_bytes + 36;
        let mut out = Vec::with_capacity((riff_size + 8) as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_bytes.to_le_bytes());
        out.resize((riff_size + 8) as usize, 0);
        fs::write(path, out)
    }

    #[test]
    fn wedding_three_minute_blueprint_compiles_with_exact_frames_and_passes_validation()
    -> Result<(), Box<dyn std::error::Error>> {
        let guard = TempDirGuard::new("3min")?;
        let landscape = guard.0.join("landscape.png");
        let portrait = guard.0.join("portrait.png");
        let bgm = guard.0.join("wedding.wav");
        write_img(&landscape, 1600, 900)?;
        write_img(&portrait, 720, 1280)?;
        write_wav(&bgm, 48_000, 48_000 * 60)?; // 60秒のBGMを3分にループ配置させる

        let mut slides = Vec::new();
        for i in 0..10 {
            let p = if i % 3 == 0 { &portrait } else { &landscape };
            slides.push(SlideBlueprintSpec {
                image_path: p.to_string_lossy().into_owned(),
                subtitle: Some(format!("思い出のシーン {}", i + 1)),
                chapter: Some(if i < 5 { "新郎" } else { "ふたり" }.to_owned()),
                motion: if i % 2 == 0 {
                    KenBurnsMotion::ZoomIn
                } else {
                    KenBurnsMotion::PanLeft
                },
                weight: 1.0,
                portrait_backdrop: true,
            });
        }

        let blueprint = MovieBlueprint {
            project_name: Some("太郎 & 花子 Profile Movie".to_owned()),
            target_duration_seconds: 180.0,
            transition_overlap_seconds: 0.8,
            style: MovieStylePreset::WeddingWarm,
            font_family: None,
            title_card: Some(TitleCardSpec {
                title: "Taro & Hanako".to_owned(),
                subtitle: Some("Profile Movie".to_owned()),
                duration_seconds: 8.0,
            }),
            slides,
            ending_card: Some(TitleCardSpec {
                title: "Thank you!".to_owned(),
                subtitle: None,
                duration_seconds: 10.0,
            }),
            bgm: Some(BgmBlueprintSpec {
                audio_path: bgm.to_string_lossy().into_owned(),
                gain_db: -4.0,
                fade_in_seconds: 1.5,
                fade_out_seconds: 3.0,
                loop_if_shorter: true,
            }),
        };

        let scope = AllowedFolderScope::from_folders([&guard.0]);
        for fps in [FrameRate::FPS_30, FrameRate::FPS_60, FrameRate::FPS_29_97] {
            let mut doc = ProjectDocument::new("Test");
            doc.fps = fps;
            let res = compile_blueprint_to_edit_command(&doc, &blueprint, Some(&scope))?;
            // テスト用に 180 秒を各フレームレートの正の整数フレームへ丸める
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let expected_frames = (180.0 * fps.as_f64()).round() as u64;
            assert_eq!(res.summary.total_frames, expected_frames);
            assert_eq!(res.summary.slide_count, 10);
            assert_eq!(res.summary.subtitle_count, 12); // title + 10 slides + ending
            assert_eq!(res.summary.portrait_backdrop_count, 4); // indices 0,3,6,9
            // 29.97fps では floor(60*29.97)=1798f * 3 = 5394f に対し 180s が 5395f のため 1f 分の補完ループが追加される
            let expected_bgm_segments = if fps == FrameRate::FPS_29_97 { 4 } else { 3 };
            assert_eq!(res.summary.bgm_segment_count, expected_bgm_segments);

            let mut dispatcher = CommandDispatcher::new();
            dispatcher.dispatch(&mut doc, &res.command)?;
            assert_eq!(doc.total_duration_frames(), expected_frames);
            doc.validate()?;
        }
        Ok(())
    }
}
