//! プロジェクトドキュメント本体と整合性検証（不変条件チェック）の定義。

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::animation::{AnimatedParameter, EvaluationContext};
use crate::core::color::RgbaColor;
use crate::core::ids::{AssetId, ClipId, LayerId};
use crate::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration, TimeError,
    frames_to_source_seconds,
};
use crate::media::visual_properties::{SubtitleProperties, VisualProperties};

/// 現在サポートしているプロジェクトファイルのスキーマバージョン。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// ドキュメントの不変条件検証時に検出されるエラー。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DocumentValidationError {
    /// 未対応のスキーマバージョンが指定された。
    #[error("unsupported schema version {found} (supported: {supported})")]
    UnsupportedSchemaVersion {
        /// 読み込まれたバージョン。
        found: u32,
        /// サポート対象バージョン。
        supported: u32,
    },
    /// 素材 ID が重複している。
    #[error("duplicate asset id `{0}`")]
    DuplicateAssetId(AssetId),
    /// レイヤー ID が重複している。
    #[error("duplicate layer id `{0}`")]
    DuplicateLayerId(LayerId),
    /// クリップ ID がプロジェクト全体で重複している。
    #[error("duplicate clip id `{0}`")]
    DuplicateClipId(ClipId),
    /// 素材の相対パス・絶対パスの両方が空である。
    #[error("asset `{0}` must have at least one non-empty path")]
    InvalidAssetPath(AssetId),
    /// クリップが参照する素材が存在しない。
    #[error("clip `{clip_id}` references unknown asset `{asset_id}`")]
    MissingAsset {
        /// 該当クリップ ID。
        clip_id: ClipId,
        /// 見つからなかった素材 ID。
        asset_id: AssetId,
    },
    /// クリップ種別と参照先素材の種別（または音声有無）が一致しない。
    #[error("clip `{clip_id}` is incompatible with asset `{asset_id}` (expected {expected})")]
    IncompatibleAssetKind {
        /// 該当クリップ ID。
        clip_id: ClipId,
        /// 参照先素材 ID。
        asset_id: AssetId,
        /// 期待された素材条件。
        expected: &'static str,
    },
    /// クリップの終了フレーム計算が `u64` を超えた。
    #[error("clip `{clip_id}` exceeds representable frame range")]
    ClipFrameRangeOverflow {
        /// 該当クリップ ID。
        clip_id: ClipId,
    },
    /// 同一レイヤー内で 2 つのクリップのフレーム区間が重なっている。
    #[error("layer `{layer_id}` has overlapping clips `{first}` and `{second}`")]
    OverlappingClips {
        /// 重複が発生したレイヤー ID。
        layer_id: LayerId,
        /// 先行クリップ ID。
        first: ClipId,
        /// 重複したクリップ ID。
        second: ClipId,
    },
    /// クリップのソース参照区間が素材アセットの総時間を超えている。
    #[error(
        "clip `{clip_id}` source end ({required:?}) exceeds asset `{asset_id}` duration ({available:?})"
    )]
    SourceRangeExceedsAsset {
        /// 該当クリップ ID。
        clip_id: ClipId,
        /// 参照先素材 ID。
        asset_id: AssetId,
        /// クリップが必要とするソース終端時刻。
        required: RationalDuration,
        /// 素材アセットの総時間。
        available: RationalDuration,
    },
    /// ソース時間計算中に時刻エラーが発生した。
    #[error("time calculation failed for clip `{clip_id}`: {source}")]
    TimeCalculation {
        /// 該当クリップ ID。
        clip_id: ClipId,
        /// 発生した時間計算エラー。
        #[source]
        source: TimeError,
    },
    /// タイムライン選択範囲の開始が終了以上になっている。
    #[error("selection range start ({:?}) must be strictly less than end ({:?})", .start.get(), .end.get())]
    InvalidSelectionRange {
        /// 範囲開始フレーム。
        start: FrameIndex,
        /// 範囲終了フレーム。
        end: FrameIndex,
    },
    /// クリップ中間点配列が狭義単調増加ではない。
    #[error("clip `{clip_id}` midpoints must be strictly increasing")]
    InvalidMidpoints {
        /// 該当クリップ ID。
        clip_id: ClipId,
    },
}

/// タイムライン全体の総尺算出モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TimelineLengthMode {
    /// 全レイヤーの最終クリップ末尾に追従する（既定）。
    #[default]
    AutoToLastClip,
    /// 手動指定したフレーム数をプロジェクト尺として扱う。
    Manual {
        /// 手動尺の総フレーム数（1 以上）。
        end_frame: FrameCount,
    },
}

/// タイムライン上の半開選択範囲 `[start, end)`（単位: フレーム）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FrameRange {
    /// 選択範囲の開始フレーム（含む）。
    pub start: FrameIndex,
    /// 選択範囲の終了フレーム（含まない）。
    pub end: FrameIndex,
}

impl FrameRange {
    /// 選択範囲のフレーム長を返す。
    #[must_use]
    pub const fn len_frames(self) -> u64 {
        self.end.get().saturating_sub(self.start.get())
    }

    /// 指定フレームがこの選択範囲 `[start, end)` に含まれるか返す。
    #[must_use]
    pub const fn contains(self, frame: FrameIndex) -> bool {
        self.start.get() <= frame.get() && frame.get() < self.end.get()
    }
}

/// キャンバス解像度（幅・高さとも 1px 以上）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectResolution {
    /// 水平解像度（px）。
    #[schemars(range(min = 1))]
    pub width: NonZeroU32,
    /// 垂直解像度（px）。
    #[schemars(range(min = 1))]
    pub height: NonZeroU32,
}

impl ProjectResolution {
    /// 1920x1080（Full HD）。
    pub const HD_1080P: Self = match (NonZeroU32::new(1920), NonZeroU32::new(1080)) {
        (Some(w), Some(h)) => Self {
            width: w,
            height: h,
        },
        _ => unreachable!(),
    };

    /// 正の幅と高さから解像度を生成する。
    #[must_use]
    pub const fn new(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self { width, height }
    }
}

impl Default for ProjectResolution {
    fn default() -> Self {
        Self::HD_1080P
    }
}

/// 素材ファイルの参照パス（プロジェクト基準相対パス優先＋絶対パスフォールバック）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct AssetPath {
    /// プロジェクトファイルのあるディレクトリからの相対パス（推奨、`/` 区切り）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative: Option<String>,
    /// 絶対パス（移動耐性のためのフォールバック）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute: Option<String>,
}

impl AssetPath {
    /// 相対パスと絶対パスを指定して生成する。
    #[must_use]
    pub const fn new(relative: Option<String>, absolute: Option<String>) -> Self {
        Self { relative, absolute }
    }

    /// 絶対パスのみを指定して生成する。
    #[must_use]
    pub fn from_absolute(absolute: impl Into<String>) -> Self {
        Self {
            relative: None,
            absolute: Some(absolute.into()),
        }
    }

    /// 少なくとも 1 つの非空パスが設定されているかを返す。
    #[must_use]
    pub fn is_non_empty(&self) -> bool {
        let has_rel = self
            .relative
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_abs = self
            .absolute
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        has_rel || has_abs
    }
}

/// 読込時のファイル解決状態（未保存の一時状態も表現可能）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AssetStatus {
    /// 実体ファイルが参照可能。
    #[default]
    Available,
    /// 実体ファイルが見つからずリンク切れ状態。
    Missing {
        /// 期待された解決先パス文字列表現。
        expected_path: String,
    },
}

/// 素材アセットのメディア種別とメタデータ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssetKind {
    /// 動画ファイル（音声トラックの有無を保持）。
    Video {
        /// 音声ストリームを含むかどうか。
        has_audio: bool,
        /// 素材固有フレームレート（可変長や未取得時は `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_fps: Option<FrameRate>,
        /// 素材フレーム数（既知の場合）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_frames: Option<FrameCount>,
        /// 映像幅（px）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        /// 映像高さ（px）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        /// 素材の総再生時間（厳密有理数秒）。
        source_duration: RationalDuration,
    },
    /// 音声専用ファイル。
    Audio {
        /// サンプルレート（Hz）。
        sample_rate_hz: u32,
        /// チャンネル数。
        channels: u16,
        /// 素材の総再生時間（厳密有理数秒）。
        source_duration: RationalDuration,
    },
    /// 静止画ファイル（タイムライン上で任意長配置可能）。
    Image {
        /// 画像幅（px）。
        width: u32,
        /// 画像高さ（px）。
        height: u32,
    },
}

/// プロジェクトに登録された外部メディアアセット。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Asset {
    /// 一意な素材 ID。
    pub id: AssetId,
    /// UI 表示用の素材名。
    pub name: String,
    /// 相対優先＋絶対フォールバックのファイルパス情報。
    pub path: AssetPath,
    /// メディア種別・尺・解像度などの属性。
    pub kind: AssetKind,
    /// ファイル解決状態。
    #[serde(default)]
    pub status: AssetStatus,
}

/// クリップの音声ミキシングパラメータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioProperties {
    /// ゲイン（dB）。`0.0` で等倍。
    #[serde(default = "default_gain_db")]
    pub gain_db: AnimatedParameter,
    /// パン（`-1.0` 左 ～ `0.0` 中央 ～ `1.0` 右）。
    #[serde(default = "default_pan")]
    pub pan: AnimatedParameter,
    /// 個別ミュート状態。
    #[serde(default)]
    pub muted: bool,
    /// クリップ単位で直列適用する外部オーディオプラグインスロット一覧。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_slots: Vec<crate::audio::properties::AudioPluginSlot>,
}

impl Default for AudioProperties {
    fn default() -> Self {
        Self {
            gain_db: default_gain_db(),
            pan: default_pan(),
            muted: false,
            plugin_slots: Vec::new(),
        }
    }
}

const fn default_gain_db() -> AnimatedParameter {
    AnimatedParameter::constant(0.0)
}

const fn default_pan() -> AnimatedParameter {
    AnimatedParameter::constant(0.0)
}

const fn default_preserve_pitch() -> bool {
    true
}

/// クリップの種類と固有パラメータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipKind {
    /// 動画クリップ。
    Video {
        /// 参照する素材 ID。
        asset_id: AssetId,
        /// 元素材上の開始位置（秒・有理数）。
        #[serde(default)]
        source_in: RationalDuration,
        /// 再生速度倍率（正の有理数、既定 `1/1`）。
        #[serde(default)]
        playback_rate: PlaybackRate,
        /// ピッチ維持の有効/無効。
        #[serde(default = "default_preserve_pitch")]
        preserve_pitch: bool,
        /// 視覚変換プロパティ。
        #[serde(default)]
        visual: VisualProperties,
        /// 動画内音声の設定（無音または音声トラックなしの場合は `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<AudioProperties>,
    },
    /// 音声クリップ。
    Audio {
        /// 参照する素材 ID。
        asset_id: AssetId,
        /// 元素材上の開始位置（秒・有理数）。
        #[serde(default)]
        source_in: RationalDuration,
        /// 再生速度倍率（正の有理数、既定 `1/1`）。
        #[serde(default)]
        playback_rate: PlaybackRate,
        /// ピッチ維持の有効/無効。
        #[serde(default = "default_preserve_pitch")]
        preserve_pitch: bool,
        /// 音声プロパティ。
        #[serde(default)]
        audio: AudioProperties,
    },
    /// 静止画クリップ。
    Image {
        /// 参照する素材 ID。
        asset_id: AssetId,
        /// 視覚変換プロパティ。
        #[serde(default)]
        visual: VisualProperties,
    },
    /// 字幕テキストクリップ。
    Subtitle(SubtitleProperties),
}

/// タイムライン上の単一クリップ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Clip {
    /// プロジェクト内で一意なクリップ ID。
    pub id: ClipId,
    /// 表示名。
    pub name: String,
    /// タイムライン上の開始フレーム。
    pub timeline_start: FrameIndex,
    /// タイムライン上の長さ（常に 1 フレーム以上）。
    pub duration: FrameCount,
    /// カット・左トリム後もアニメーション原点の一貫性を保つためのエフェクト基準オフセット。
    ///
    /// エフェクト評価時の基準フレームは `effect_offset_frames + local_frame` で求める。
    #[serde(default)]
    pub effect_offset_frames: i64,
    /// カット後もイージングや数式 `p`/`d` の時間尺度を保つための基準尺（省略時はクリップ長）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_duration_frames: Option<FrameCount>,
    /// クリップ内エフェクト基準座標の中間点配列（狭義単調増加）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midpoints: Vec<i64>,
    /// クリップ種別ごとのペイロード。
    pub kind: ClipKind,
}

impl Clip {
    /// クリップ可視範囲のエフェクト基準境界 `(eff_start, eff_last)` を返す。
    #[must_use]
    pub fn effect_bounds(&self) -> (i64, i64) {
        let eff_start = self.effect_offset_frames;
        let span = i64::try_from(self.duration.get().saturating_sub(1)).unwrap_or(i64::MAX);
        let eff_last = eff_start.saturating_add(span);
        (eff_start, eff_last)
    }

    /// 現在の可視範囲 `(eff_start, eff_last)` に厳密に含まれる中間点一覧を昇順で返す。
    #[must_use]
    pub fn visible_midpoints(&self) -> Vec<i64> {
        let (eff_start, eff_last) = self.effect_bounds();
        self.midpoints
            .iter()
            .copied()
            .filter(|&m| eff_start < m && m < eff_last)
            .collect()
    }

    /// セグメント境界フレーム列 `[eff_start, ...visible_midpoints..., eff_last]` を返す。
    ///
    /// クリップ長が 1 フレームのときは `[eff_start]` を返す。
    #[must_use]
    pub fn grid_frames(&self) -> Vec<i64> {
        let (eff_start, eff_last) = self.effect_bounds();
        if self.duration.get() <= 1 || eff_last <= eff_start {
            return vec![eff_start];
        }
        let mut grid = Vec::with_capacity(self.midpoints.len() + 2);
        grid.push(eff_start);
        grid.extend(self.visible_midpoints());
        grid.push(eff_last);
        grid
    }

    /// クリップ内の全 [`AnimatedParameter`] に対して変更コールバックを適用する。
    pub fn visit_animated_params_mut<F>(&mut self, mut visit: F)
    where
        F: FnMut(&mut AnimatedParameter),
    {
        match &mut self.kind {
            ClipKind::Video { visual, audio, .. } => {
                visit_visual_params_mut(visual, &mut visit);
                if let Some(a) = audio {
                    visit(&mut a.gain_db);
                    visit(&mut a.pan);
                }
            }
            ClipKind::Image { visual, .. } => {
                visit_visual_params_mut(visual, &mut visit);
            }
            ClipKind::Audio { audio, .. } => {
                visit(&mut audio.gain_db);
                visit(&mut audio.pan);
            }
            ClipKind::Subtitle(sub) => {
                visit(&mut sub.font_size);
                visit_visual_params_mut(&mut sub.visual, &mut visit);
            }
        }
    }
}

fn visit_visual_params_mut<F>(visual: &mut VisualProperties, visit: &mut F)
where
    F: FnMut(&mut AnimatedParameter),
{
    visit(&mut visual.x);
    visit(&mut visual.y);
    visit(&mut visual.scale_x);
    visit(&mut visual.scale_y);
    visit(&mut visual.rotation_deg);
    visit(&mut visual.anchor_x);
    visit(&mut visual.anchor_y);
    visit(&mut visual.opacity);
    visit(&mut visual.brightness);
    visit(&mut visual.contrast);
    visit(&mut visual.saturation);
    visit(&mut visual.hue_deg);
    visit(&mut visual.gamma);
}

impl Clip {
    /// いずれかの視覚・音声・字幕パラメータに時間依存（`Constant` 以外）の設定があるか返す。
    #[must_use]
    pub fn has_time_varying_animation(&self) -> bool {
        let is_dyn = |p: &AnimatedParameter| !matches!(p, AnimatedParameter::Constant { .. });
        let vis_dyn = |v: &VisualProperties| {
            is_dyn(&v.x)
                || is_dyn(&v.y)
                || is_dyn(&v.scale_x)
                || is_dyn(&v.scale_y)
                || is_dyn(&v.rotation_deg)
                || is_dyn(&v.anchor_x)
                || is_dyn(&v.anchor_y)
                || is_dyn(&v.opacity)
                || is_dyn(&v.brightness)
                || is_dyn(&v.contrast)
                || is_dyn(&v.saturation)
                || is_dyn(&v.hue_deg)
                || is_dyn(&v.gamma)
        };
        let aud_dyn = |a: &AudioProperties| is_dyn(&a.gain_db) || is_dyn(&a.pan);
        match &self.kind {
            ClipKind::Video { visual, audio, .. } => {
                vis_dyn(visual) || audio.as_ref().is_some_and(aud_dyn)
            }
            ClipKind::Image { visual, .. } => vis_dyn(visual),
            ClipKind::Audio { audio, .. } => aud_dyn(audio),
            ClipKind::Subtitle(sub) => is_dyn(&sub.font_size) || vis_dyn(&sub.visual),
        }
    }
}

impl Clip {
    /// タイムライン上の終了フレーム（排他的上端）を返す。
    ///
    /// # Errors
    ///
    /// 終了フレームが `u64::MAX` を超える場合は [`TimeError::Overflow`] を返す。
    pub const fn timeline_end(&self) -> Result<FrameIndex, TimeError> {
        self.timeline_start.checked_add_count(self.duration)
    }

    /// エフェクト（イージング・数式 `p`/`d`）評価時に基準とする有効フレーム尺を返す。
    #[must_use]
    pub const fn effective_effect_duration(&self) -> FrameCount {
        match self.effect_duration_frames {
            Some(duration) => duration,
            None => self.duration,
        }
    }

    /// 指定したタイムラインフレーム位置がこのクリップ範囲内であれば評価コンテキストを返す。
    #[must_use]
    pub fn evaluation_context(
        &self,
        timeline_frame: FrameIndex,
        fps: FrameRate,
    ) -> Option<EvaluationContext> {
        let start = self.timeline_start.get();
        let end = self.timeline_end().ok()?.get();
        let frame = timeline_frame.get();
        if frame < start || frame >= end {
            return None;
        }
        Some(EvaluationContext::new(
            frame - start,
            self.effect_offset_frames,
            self.effective_effect_duration(),
            fps,
        ))
    }
}

const fn default_visible() -> bool {
    true
}

/// タイムライン上の 1 レイヤー（配列順序がそのまま上下スタック順、末尾ほど前面）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Layer {
    /// 一意なレイヤー ID。
    pub id: LayerId,
    /// レイヤー名。
    pub name: String,
    /// レイヤーの映像可視状態。
    #[serde(default = "default_visible")]
    pub visible: bool,
    /// レイヤー全体のミュート状態。
    #[serde(default)]
    pub muted: bool,
    /// ロック状態（ロック時の操作制限に利用）。
    #[serde(default)]
    pub locked: bool,
    /// このレイヤーに配置されたクリップ群（同一レイヤー内で重なり不可）。
    #[serde(default)]
    pub clips: Vec<Clip>,
}

impl Layer {
    /// 新しい空のレイヤーを作成する。
    #[must_use]
    pub fn new(id: LayerId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            visible: true,
            muted: false,
            locked: false,
            clips: Vec::new(),
        }
    }

    /// レイヤー内クリップを開始フレーム順にソートする。
    pub fn sort_clips(&mut self) {
        self.clips.sort_by_key(|clip| clip.timeline_start.get());
    }
}

const fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// `sea-movie-creator` のプロジェクト全体を表すルートドキュメント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectDocument {
    /// JSON スキーマバージョン（現行 `1`）。
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// プロジェクト名。
    pub name: String,
    /// プロジェクト固定フレームレート。
    #[serde(default)]
    pub fps: FrameRate,
    /// キャンバス解像度。
    #[serde(default)]
    pub resolution: ProjectResolution,
    /// キャンバス背景色。
    #[serde(default)]
    pub background_color: RgbaColor,
    /// タイムライン総尺の算出モード。
    #[serde(default)]
    pub timeline_length: TimelineLengthMode,
    /// タイムライン上の選択範囲（`[`〜`]` マーカー、半開区間）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_range: Option<FrameRange>,
    /// 登録済み素材一覧。
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// レイヤー一覧（先頭が背面、末尾が前面）。
    #[serde(default)]
    pub layers: Vec<Layer>,
}

impl ProjectDocument {
    /// 空の標準 1080p60 プロジェクトを生成する。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            name: name.into(),
            fps: FrameRate::FPS_60,
            resolution: ProjectResolution::HD_1080P,
            background_color: RgbaColor::BLACK,
            timeline_length: TimelineLengthMode::AutoToLastClip,
            selection_range: None,
            assets: Vec::new(),
            layers: Vec::new(),
        }
    }

    /// 素材 ID から素材を検索する。
    #[must_use]
    pub fn find_asset(&self, asset_id: &AssetId) -> Option<&Asset> {
        self.assets.iter().find(|asset| &asset.id == asset_id)
    }

    /// 素材 ID から素材を可変参照で検索する。
    #[must_use]
    pub fn find_asset_mut(&mut self, asset_id: &AssetId) -> Option<&mut Asset> {
        self.assets.iter_mut().find(|asset| &asset.id == asset_id)
    }

    /// レイヤー ID からレイヤーを検索する。
    #[must_use]
    pub fn find_layer(&self, layer_id: &LayerId) -> Option<&Layer> {
        self.layers.iter().find(|layer| &layer.id == layer_id)
    }

    /// レイヤー ID からレイヤーを可変参照で検索する。
    #[must_use]
    pub fn find_layer_mut(&mut self, layer_id: &LayerId) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|layer| &layer.id == layer_id)
    }

    /// クリップ ID から所属レイヤーとクリップを検索する。
    #[must_use]
    pub fn find_clip(&self, clip_id: &ClipId) -> Option<(&Layer, &Clip)> {
        for layer in &self.layers {
            if let Some(clip) = layer.clips.iter().find(|clip| &clip.id == clip_id) {
                return Some((layer, clip));
            }
        }
        None
    }

    /// 全レイヤーの最終クリップ末尾位置（最大 `timeline_start + duration`）を返す。
    #[must_use]
    pub fn clips_end_frame(&self) -> u64 {
        self.layers
            .iter()
            .flat_map(|layer| layer.clips.iter())
            .filter_map(|clip| clip.timeline_end().ok().map(FrameIndex::get))
            .max()
            .unwrap_or(0)
    }

    /// プロジェクト全体の総尺フレーム数を返す。
    ///
    /// [`TimelineLengthMode::AutoToLastClip`] の場合は [`Self::clips_end_frame`]、
    /// [`TimelineLengthMode::Manual`] の場合は指定された `end_frame` を返す。
    #[must_use]
    pub fn total_duration_frames(&self) -> u64 {
        match self.timeline_length {
            TimelineLengthMode::AutoToLastClip => self.clips_end_frame(),
            TimelineLengthMode::Manual { end_frame } => end_frame.get(),
        }
    }

    /// ルーラー表示用の仮想終端フレーム位置（`total_duration_frames + 10秒`、最小 10 秒）を返す。
    #[must_use]
    pub fn ruler_display_span_frames(&self) -> u64 {
        let num = u64::from(self.fps.numerator());
        let den = u64::from(self.fps.denominator()).max(1);
        let ten_sec = (num.saturating_mul(10).saturating_add(den / 2)) / den;
        self.total_duration_frames()
            .saturating_add(ten_sec)
            .max(ten_sec.max(1))
    }

    /// プレイヘッドシーク許容上限フレームを返す。
    #[must_use]
    pub fn seek_upper_bound_frames(&self) -> u64 {
        match self.timeline_length {
            TimelineLengthMode::AutoToLastClip => self.ruler_display_span_frames(),
            TimelineLengthMode::Manual { end_frame } => end_frame.get(),
        }
    }

    /// ドキュメント全体の不変条件（ID 一意性・非重複区間・参照整合・ソース境界・選択範囲・中間点）を検証する。
    ///
    /// # Errors
    ///
    /// 整合性違反が見つかった場合、最初の違反内容を [`DocumentValidationError`] として返す。
    pub fn validate(&self) -> Result<(), DocumentValidationError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(DocumentValidationError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        if let Some(range) = self.selection_range
            && range.start >= range.end
        {
            return Err(DocumentValidationError::InvalidSelectionRange {
                start: range.start,
                end: range.end,
            });
        }

        let asset_map = self.validate_assets()?;
        let mut seen_layer_ids = HashSet::with_capacity(self.layers.len());
        let mut seen_clip_ids = HashSet::new();

        for layer in &self.layers {
            if !seen_layer_ids.insert(&layer.id) {
                return Err(DocumentValidationError::DuplicateLayerId(layer.id.clone()));
            }
            for clip in &layer.clips {
                if !clip.midpoints.windows(2).all(|w| w[0] < w[1]) {
                    return Err(DocumentValidationError::InvalidMidpoints {
                        clip_id: clip.id.clone(),
                    });
                }
            }
            self.validate_single_layer(layer, &asset_map, &mut seen_clip_ids)?;
        }

        Ok(())
    }

    fn validate_assets(&self) -> Result<HashMap<&AssetId, &Asset>, DocumentValidationError> {
        let mut asset_map = HashMap::with_capacity(self.assets.len());
        for asset in &self.assets {
            if asset_map.insert(&asset.id, asset).is_some() {
                return Err(DocumentValidationError::DuplicateAssetId(asset.id.clone()));
            }
            if !asset.path.is_non_empty() {
                return Err(DocumentValidationError::InvalidAssetPath(asset.id.clone()));
            }
        }
        Ok(asset_map)
    }

    fn validate_single_layer<'doc>(
        &'doc self,
        layer: &'doc Layer,
        asset_map: &HashMap<&'doc AssetId, &'doc Asset>,
        seen_clip_ids: &mut HashSet<&'doc ClipId>,
    ) -> Result<(), DocumentValidationError> {
        let mut intervals: Vec<(u64, u64, &ClipId)> = Vec::with_capacity(layer.clips.len());

        for clip in &layer.clips {
            if !seen_clip_ids.insert(&clip.id) {
                return Err(DocumentValidationError::DuplicateClipId(clip.id.clone()));
            }
            let end = clip.timeline_end().map_err(|_| {
                DocumentValidationError::ClipFrameRangeOverflow {
                    clip_id: clip.id.clone(),
                }
            })?;
            self.validate_clip_asset_reference(clip, asset_map)?;
            intervals.push((clip.timeline_start.get(), end.get(), &clip.id));
        }

        // レイヤー配列内の並び順に依存せず重なりを O(N log N) で厳密検出する
        intervals.sort_by_key(|(start, end, _)| (*start, *end));
        for pair in intervals.windows(2) {
            let first = pair[0];
            let second = pair[1];
            if first.1 > second.0 {
                return Err(DocumentValidationError::OverlappingClips {
                    layer_id: layer.id.clone(),
                    first: first.2.clone(),
                    second: second.2.clone(),
                });
            }
        }

        Ok(())
    }

    fn validate_clip_asset_reference(
        &self,
        clip: &Clip,
        asset_map: &HashMap<&AssetId, &Asset>,
    ) -> Result<(), DocumentValidationError> {
        match &clip.kind {
            ClipKind::Video {
                asset_id,
                source_in,
                playback_rate,
                ..
            } => {
                let asset = lookup_asset(clip, asset_id, asset_map)?;
                let AssetKind::Video {
                    source_duration, ..
                } = &asset.kind
                else {
                    return Err(DocumentValidationError::IncompatibleAssetKind {
                        clip_id: clip.id.clone(),
                        asset_id: asset_id.clone(),
                        expected: "video asset",
                    });
                };
                validate_source_bounds(
                    clip,
                    asset_id,
                    self.fps,
                    *source_in,
                    *playback_rate,
                    *source_duration,
                )
            }
            ClipKind::Audio {
                asset_id,
                source_in,
                playback_rate,
                ..
            } => {
                let asset = lookup_asset(clip, asset_id, asset_map)?;
                let available = match &asset.kind {
                    AssetKind::Audio {
                        source_duration, ..
                    }
                    | AssetKind::Video {
                        has_audio: true,
                        source_duration,
                        ..
                    } => *source_duration,
                    AssetKind::Video {
                        has_audio: false, ..
                    }
                    | AssetKind::Image { .. } => {
                        return Err(DocumentValidationError::IncompatibleAssetKind {
                            clip_id: clip.id.clone(),
                            asset_id: asset_id.clone(),
                            expected: "audio asset or video asset with audio stream",
                        });
                    }
                };
                validate_source_bounds(
                    clip,
                    asset_id,
                    self.fps,
                    *source_in,
                    *playback_rate,
                    available,
                )
            }
            ClipKind::Image { asset_id, .. } => {
                let asset = lookup_asset(clip, asset_id, asset_map)?;
                if !matches!(asset.kind, AssetKind::Image { .. }) {
                    return Err(DocumentValidationError::IncompatibleAssetKind {
                        clip_id: clip.id.clone(),
                        asset_id: asset_id.clone(),
                        expected: "image asset",
                    });
                }
                Ok(())
            }
            ClipKind::Subtitle(_) => Ok(()),
        }
    }
}

impl Default for ProjectDocument {
    fn default() -> Self {
        Self::new("Untitled Project")
    }
}

fn lookup_asset<'doc>(
    clip: &Clip,
    asset_id: &AssetId,
    asset_map: &HashMap<&'doc AssetId, &'doc Asset>,
) -> Result<&'doc Asset, DocumentValidationError> {
    asset_map
        .get(asset_id)
        .copied()
        .ok_or_else(|| DocumentValidationError::MissingAsset {
            clip_id: clip.id.clone(),
            asset_id: asset_id.clone(),
        })
}

fn validate_source_bounds(
    clip: &Clip,
    asset_id: &AssetId,
    fps: FrameRate,
    source_in: RationalDuration,
    playback_rate: PlaybackRate,
    available: RationalDuration,
) -> Result<(), DocumentValidationError> {
    let span =
        frames_to_source_seconds(clip.duration.get(), fps, playback_rate).map_err(|source| {
            DocumentValidationError::TimeCalculation {
                clip_id: clip.id.clone(),
                source,
            }
        })?;
    let required =
        source_in
            .checked_add(span)
            .map_err(|source| DocumentValidationError::TimeCalculation {
                clip_id: clip.id.clone(),
                source,
            })?;
    if required > available {
        return Err(DocumentValidationError::SourceRangeExceedsAsset {
            clip_id: clip.id.clone(),
            asset_id: asset_id.clone(),
            required,
            available,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_video_asset(
        id: &str,
        duration_secs: u64,
    ) -> Result<Asset, Box<dyn std::error::Error>> {
        Ok(Asset {
            id: AssetId::new(id)?,
            name: "sample.mp4".to_owned(),
            path: AssetPath::new(
                Some("media/sample.mp4".to_owned()),
                Some("C:/media/sample.mp4".to_owned()),
            ),
            kind: AssetKind::Video {
                has_audio: true,
                default_fps: Some(FrameRate::FPS_60),
                duration_frames: FrameCount::new(duration_secs * 60).ok(),
                width: Some(1920),
                height: Some(1080),
                source_duration: RationalDuration::new(duration_secs, 1)?,
            },
            status: AssetStatus::Available,
        })
    }

    #[test]
    fn valid_document_with_120_layers_passes_validation() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut doc = ProjectDocument::new("Large Layer Test");
        let asset = sample_video_asset("asset_video", 60)?;
        doc.assets.push(asset);

        for idx in 0..120_u32 {
            let layer_id = LayerId::new(format!("layer_{idx}"))?;
            let mut layer = Layer::new(layer_id, format!("Track {idx}"));
            layer.clips.push(Clip {
                id: ClipId::new(format!("clip_{idx}"))?,
                name: format!("Clip {idx}"),
                timeline_start: FrameIndex::new(u64::from(idx) * 10),
                duration: FrameCount::new(60)?,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: ClipKind::Video {
                    asset_id: AssetId::new("asset_video")?,
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::NORMAL,
                    preserve_pitch: true,
                    visual: VisualProperties::default(),
                    audio: Some(AudioProperties::default()),
                },
            });
            doc.layers.push(layer);
        }

        doc.validate()?;
        assert_eq!(doc.layers.len(), 120);
        assert_eq!(doc.total_duration_frames(), 119 * 10 + 60);
        Ok(())
    }

    #[test]
    fn overlapping_clips_on_same_layer_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Overlap Test");
        doc.assets.push(sample_video_asset("asset_video", 30)?);

        let mut layer = Layer::new(LayerId::new("layer_1")?, "Video 1");
        layer.clips.push(Clip {
            id: ClipId::new("clip_a")?,
            name: "A".to_owned(),
            timeline_start: FrameIndex::new(0),
            duration: FrameCount::new(100)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("Hello")),
        });
        layer.clips.push(Clip {
            id: ClipId::new("clip_b")?,
            name: "B".to_owned(),
            timeline_start: FrameIndex::new(99),
            duration: FrameCount::new(50)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("Overlap")),
        });
        doc.layers.push(layer);

        let err = doc.validate();
        assert!(matches!(
            err,
            Err(DocumentValidationError::OverlappingClips { .. })
        ));
        Ok(())
    }

    #[test]
    fn clip_exceeding_asset_source_duration_is_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut doc = ProjectDocument::new("Source Bound Test");
        doc.assets.push(sample_video_asset("asset_video", 2)?);

        let mut layer = Layer::new(LayerId::new("layer_1")?, "Video 1");
        layer.clips.push(Clip {
            id: ClipId::new("clip_too_long")?,
            name: "Too Long".to_owned(),
            timeline_start: FrameIndex::new(0),
            duration: FrameCount::new(121)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Video {
                asset_id: AssetId::new("asset_video")?,
                source_in: RationalDuration::ZERO,
                playback_rate: PlaybackRate::NORMAL,
                preserve_pitch: true,
                visual: VisualProperties::default(),
                audio: None,
            },
        });
        doc.layers.push(layer);

        let err = doc.validate();
        assert!(matches!(
            err,
            Err(DocumentValidationError::SourceRangeExceedsAsset { .. })
        ));
        Ok(())
    }

    #[test]
    fn timeline_length_ruler_span_and_seek_upper_bound_behave_per_spec()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Len");
        doc.fps = FrameRate::FPS_29_97;
        assert_eq!(doc.total_duration_frames(), 0);
        assert_eq!(doc.ruler_display_span_frames(), 300);
        assert_eq!(doc.seek_upper_bound_frames(), 300);

        let mut layer = Layer::new(LayerId::new("l1")?, "V1");
        layer.clips.push(Clip {
            id: ClipId::new("c1")?,
            name: "Sub".to_owned(),
            timeline_start: FrameIndex::new(100),
            duration: FrameCount::new(200)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: vec![20, 100],
            kind: ClipKind::Subtitle(SubtitleProperties::new("Hi")),
        });
        doc.layers.push(layer);
        assert_eq!(doc.clips_end_frame(), 300);
        assert_eq!(doc.total_duration_frames(), 300);
        assert_eq!(doc.ruler_display_span_frames(), 600);
        assert_eq!(doc.seek_upper_bound_frames(), 600);

        doc.timeline_length = TimelineLengthMode::Manual {
            end_frame: FrameCount::new(900)?,
        };
        assert_eq!(doc.total_duration_frames(), 900);
        assert_eq!(doc.ruler_display_span_frames(), 1200);
        assert_eq!(doc.seek_upper_bound_frames(), 900);

        let clip = &doc.layers[0].clips[0];
        assert_eq!(clip.effect_bounds(), (0, 199));
        assert_eq!(clip.visible_midpoints(), vec![20, 100]);
        assert_eq!(clip.grid_frames(), vec![0, 20, 100, 199]);
        Ok(())
    }

    #[test]
    fn invalid_selection_range_and_non_monotonic_midpoints_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Validate");
        doc.selection_range = Some(FrameRange {
            start: FrameIndex::new(10),
            end: FrameIndex::new(10),
        });
        assert!(matches!(
            doc.validate(),
            Err(DocumentValidationError::InvalidSelectionRange { .. })
        ));

        doc.selection_range = Some(FrameRange {
            start: FrameIndex::new(10),
            end: FrameIndex::new(20),
        });
        let mut layer = Layer::new(LayerId::new("l1")?, "V1");
        layer.clips.push(Clip {
            id: ClipId::new("c1")?,
            name: "Bad".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(100)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: vec![30, 30],
            kind: ClipKind::Subtitle(SubtitleProperties::new("Bad")),
        });
        doc.layers.push(layer);
        assert!(matches!(
            doc.validate(),
            Err(DocumentValidationError::InvalidMidpoints { .. })
        ));
        Ok(())
    }

    #[test]
    fn old_project_json_without_new_fields_deserializes_with_defaults()
    -> Result<(), Box<dyn std::error::Error>> {
        let doc = ProjectDocument::new("Compat");
        let mut val = serde_json::to_value(&doc)?;
        if let Some(obj) = val.as_object_mut() {
            let _ = obj.remove("timeline_length");
            let _ = obj.remove("selection_range");
        }
        let restored: ProjectDocument = serde_json::from_value(val)?;
        assert_eq!(restored.timeline_length, TimelineLengthMode::AutoToLastClip);
        assert_eq!(restored.selection_range, None);
        restored.validate()?;
        Ok(())
    }
}
