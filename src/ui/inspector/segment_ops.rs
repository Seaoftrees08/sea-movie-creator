//! インスペクタ セグメント読み書き・移動モード切替・UI 単位変換・連続ドラッグ状態管理。

use crate::core::animation::{
    AnimatedParameter, EasingCurve, EvaluationContext, ExtrapolationMode, Keyframe,
};
use crate::core::color::RgbaColor;
use crate::core::expression::CompiledExpression;
use crate::core::ids::{ClipId, ClipRef};
use crate::core::time::{FrameIndex, FrameRate};
use crate::media::visual_properties::{BlendMode, TextAlignment};
use crate::timeline::command::{EditCommand, ParameterPath, ParameterTarget, ParameterValue};
use crate::timeline::document::{Clip, ClipKind, ProjectDocument};
use crate::timeline::operations::sync_and_align_clip_keyframes;

/// インスペクタで編集対象にするパラメータモード（4 モード）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorMode {
    /// 時間変化しない固定値。
    Constant,
    /// 開始値から終了値への 2 点イージング。
    Easing,
    /// エフェクト基準フレームベースのキーフレーム列。
    Keyframes,
    /// 純粋 Rust 数式 `f(t)` と微調整オフセット・倍率。
    Expression,
}

impl InspectorMode {
    /// 全 4 モード一覧。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Constant,
            Self::Easing,
            Self::Keyframes,
            Self::Expression,
        ]
    }

    /// UI 表示用ラベル。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Constant => "固定値",
            Self::Easing => "イージング",
            Self::Keyframes => "キーフレーム",
            Self::Expression => "数式 f(t)",
        }
    }

    /// `AnimatedParameter` に対応するモードを返す。
    #[must_use]
    pub const fn from_param(param: &AnimatedParameter) -> Self {
        match param {
            AnimatedParameter::Constant { .. } => Self::Constant,
            AnimatedParameter::Easing { .. } => Self::Easing,
            AnimatedParameter::Keyframes { .. } => Self::Keyframes,
            AnimatedParameter::Expression { .. } => Self::Expression,
        }
    }
}

/// `AviUtl` 風中央移動方法ボタンから選択できる 8 プリセット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovementModePreset {
    /// 時間変化しない固定値。
    Constant,
    /// 一定速度の線形補間。
    Linear,
    /// 両端が滑らかに加減速する補間。
    EaseInOut,
    /// ゆっくり始まり徐々に加速する補間。
    EaseIn,
    /// 速く始まり徐々に減速する補間。
    EaseOut,
    /// 制御点指定の 3 次ベジェ補間。
    Bezier,
    /// キーフレーム列を直接編集するモード。
    Keyframes,
    /// 数式 `f(t)` によって評価するモード。
    Expression,
}

impl MovementModePreset {
    /// メニュー表示順に並べた全 8 プリセット。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Constant,
            Self::Linear,
            Self::EaseInOut,
            Self::EaseIn,
            Self::EaseOut,
            Self::Bezier,
            Self::Keyframes,
            Self::Expression,
        ]
    }

    /// ポップアップメニュー内の日本語ラベルを返す。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Constant => "移動なし",
            Self::Linear => "直線移動",
            Self::EaseInOut => "加減速移動",
            Self::EaseIn => "加速移動",
            Self::EaseOut => "減速移動",
            Self::Bezier => "ベジェ曲線",
            Self::Keyframes => "キーフレーム",
            Self::Expression => "数式 f(t)",
        }
    }

    /// トラックバー中央ボタンに収まる短い表示バッジを返す。
    #[must_use]
    pub const fn short_badge(self) -> &'static str {
        match self {
            Self::Constant => "固定",
            Self::Linear => "直線",
            Self::EaseInOut => "加減速",
            Self::EaseIn => "加速",
            Self::EaseOut => "減速",
            Self::Bezier => "ベジェ",
            Self::Keyframes => "ｷｰﾌﾚｰﾑ",
            Self::Expression => "数式",
        }
    }

    /// イージングカーブ種別を対応プリセットへ変換する。
    #[must_use]
    pub const fn from_curve(curve: EasingCurve) -> Self {
        match curve {
            EasingCurve::Linear => Self::Linear,
            EasingCurve::EaseIn => Self::EaseIn,
            EasingCurve::EaseOut => Self::EaseOut,
            EasingCurve::EaseInOut => Self::EaseInOut,
            EasingCurve::CubicBezier { .. } => Self::Bezier,
        }
    }
}

/// インスペクタで操作できるアニメーション可能パラメータスロット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum InspectorParameterSlot {
    /// X 座標 (px)。
    PositionX,
    /// Y 座標 (px)。
    PositionY,
    /// X 拡大率 (`0.05..=5.0`, UI 表示 `5..=500%`)。
    ScaleX,
    /// Y 拡大率 (`0.05..=5.0`, UI 表示 `5..=500%`)。
    ScaleY,
    /// 回転角度 (度)。
    RotationDeg,
    /// アンカー X (px)。
    AnchorX,
    /// アンカー Y (px)。
    AnchorY,
    /// 不透明度 (`0.0..=1.0`, UI は透明度 `0..=100%`)。
    #[default]
    Opacity,
    /// 明度補正 (`-1.0..=1.0`)。
    Brightness,
    /// コントラスト倍率 (`0.0..=4.0`)。
    Contrast,
    /// 彩度倍率 (`0.0..=4.0`)。
    Saturation,
    /// 色相回転 (度)。
    HueDeg,
    /// ガンマ (`0.05..=5.0`)。
    Gamma,
    /// 字幕フォントサイズ (px)。
    SubtitleFontSize,
    /// 音量倍率 (dB)。
    AudioVolume,
    /// パン (`-1.0..=1.0`, UI 表示 `-100..=100`)。
    AudioPan,
}

impl InspectorParameterSlot {
    /// 全 16 スロットの一覧。
    pub const ALL: &'static [Self] = &[
        Self::PositionX,
        Self::PositionY,
        Self::ScaleX,
        Self::ScaleY,
        Self::RotationDeg,
        Self::AnchorX,
        Self::AnchorY,
        Self::Opacity,
        Self::Brightness,
        Self::Contrast,
        Self::Saturation,
        Self::HueDeg,
        Self::Gamma,
        Self::SubtitleFontSize,
        Self::AudioVolume,
        Self::AudioPan,
    ];

    /// 標準描画セクションの主要スロット。
    #[must_use]
    pub const fn standard_slots() -> &'static [Self] {
        &[
            Self::PositionX,
            Self::PositionY,
            Self::ScaleX,
            Self::ScaleY,
            Self::RotationDeg,
            Self::Opacity,
        ]
    }

    /// 基準点拡張セクションのスロット。
    #[must_use]
    pub const fn anchor_slots() -> &'static [Self] {
        &[Self::AnchorX, Self::AnchorY]
    }

    /// 色調補正セクションのスロット。
    #[must_use]
    pub const fn color_slots() -> &'static [Self] {
        &[
            Self::Brightness,
            Self::Contrast,
            Self::Saturation,
            Self::HueDeg,
            Self::Gamma,
        ]
    }

    /// 音声再生セクションのスロット。
    #[must_use]
    pub const fn audio_slots() -> &'static [Self] {
        &[Self::AudioVolume, Self::AudioPan]
    }

    /// 対象クリップで編集可能なスロット一覧を返す。
    #[must_use]
    pub fn available_for(clip: &Clip) -> Vec<Self> {
        let mut slots = Vec::new();
        match &clip.kind {
            ClipKind::Video { audio, .. } => {
                slots.extend_from_slice(Self::visual_slots());
                if audio.is_some() {
                    slots.extend_from_slice(&[Self::AudioVolume, Self::AudioPan]);
                }
            }
            ClipKind::Image { .. } => {
                slots.extend_from_slice(Self::visual_slots());
            }
            ClipKind::Subtitle(_) => {
                slots.push(Self::SubtitleFontSize);
                slots.extend_from_slice(Self::visual_slots());
            }
            ClipKind::Audio { .. } => {
                slots.extend_from_slice(&[Self::AudioVolume, Self::AudioPan]);
            }
        }
        slots
    }

    const fn visual_slots() -> &'static [Self] {
        &[
            Self::Opacity,
            Self::PositionX,
            Self::PositionY,
            Self::ScaleX,
            Self::ScaleY,
            Self::RotationDeg,
            Self::AnchorX,
            Self::AnchorY,
            Self::Brightness,
            Self::Contrast,
            Self::Saturation,
            Self::HueDeg,
            Self::Gamma,
        ]
    }

    /// UI 表示用短縮ラベル。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PositionX => "位置 X",
            Self::PositionY => "位置 Y",
            Self::ScaleX => "拡大 X",
            Self::ScaleY => "拡大 Y",
            Self::RotationDeg => "回転°",
            Self::AnchorX => "アンカーX",
            Self::AnchorY => "アンカーY",
            Self::Opacity => "不透明度",
            Self::Brightness => "明度",
            Self::Contrast => "コントラスト",
            Self::Saturation => "彩度",
            Self::HueDeg => "色相°",
            Self::Gamma => "ガンマ",
            Self::SubtitleFontSize => "文字サイズ",
            Self::AudioVolume => "音量 (dB)",
            Self::AudioPan => "パン",
        }
    }

    /// `TrackBarRow` 用のさらに簡潔なラベル（`AviUtl` 風表記）。
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::PositionX => "X",
            Self::PositionY => "Y",
            Self::ScaleX => "拡大X",
            Self::ScaleY => "拡大Y",
            Self::RotationDeg => "回転",
            Self::AnchorX => "中心X",
            Self::AnchorY => "中心Y",
            Self::Opacity => "透明度",
            Self::Brightness => "明るさ",
            Self::Contrast => "ｺﾝﾄﾗｽﾄ",
            Self::Saturation => "彩度",
            Self::HueDeg => "色相",
            Self::Gamma => "ガンマ",
            Self::SubtitleFontSize => "サイズ",
            Self::AudioVolume => "音量",
            Self::AudioPan => "左右",
        }
    }

    /// UI 表示用の単位文字列を返す。
    #[must_use]
    pub const fn ui_unit_label(self) -> &'static str {
        match self {
            Self::PositionX
            | Self::PositionY
            | Self::AnchorX
            | Self::AnchorY
            | Self::SubtitleFontSize => "px",
            Self::ScaleX | Self::ScaleY | Self::Opacity => "%",
            Self::RotationDeg | Self::HueDeg => "°",
            Self::AudioVolume => "dB",
            Self::Brightness | Self::Contrast | Self::Saturation | Self::Gamma | Self::AudioPan => {
                ""
            }
        }
    }

    /// 内部モデルのデフォルト値を返す。
    #[must_use]
    pub const fn default_internal_value(self) -> f64 {
        match self {
            Self::ScaleX
            | Self::ScaleY
            | Self::Opacity
            | Self::Contrast
            | Self::Saturation
            | Self::Gamma => 1.0,
            Self::SubtitleFontSize => 48.0,
            Self::PositionX
            | Self::PositionY
            | Self::RotationDeg
            | Self::AnchorX
            | Self::AnchorY
            | Self::Brightness
            | Self::HueDeg
            | Self::AudioVolume
            | Self::AudioPan => 0.0,
        }
    }

    /// 対応する `EditCommand::SetParameter` パスへ変換する。
    #[must_use]
    pub const fn to_parameter_path(self) -> ParameterPath {
        match self {
            Self::PositionX => ParameterPath::X,
            Self::PositionY => ParameterPath::Y,
            Self::ScaleX => ParameterPath::ScaleX,
            Self::ScaleY => ParameterPath::ScaleY,
            Self::RotationDeg => ParameterPath::RotationDeg,
            Self::AnchorX => ParameterPath::AnchorX,
            Self::AnchorY => ParameterPath::AnchorY,
            Self::Opacity => ParameterPath::Opacity,
            Self::Brightness => ParameterPath::Brightness,
            Self::Contrast => ParameterPath::Contrast,
            Self::Saturation => ParameterPath::Saturation,
            Self::HueDeg => ParameterPath::HueDeg,
            Self::Gamma => ParameterPath::Gamma,
            Self::SubtitleFontSize => ParameterPath::SubtitleFontSize,
            Self::AudioVolume => ParameterPath::GainDb,
            Self::AudioPan => ParameterPath::Pan,
        }
    }

    /// 推奨内部値域 `(min, max, step)` を返す。
    #[must_use]
    pub const fn bounds(self) -> (f64, f64, f64) {
        match self {
            Self::PositionX | Self::PositionY | Self::AnchorX | Self::AnchorY => {
                (-1920.0, 1920.0, 10.0)
            }
            Self::ScaleX | Self::ScaleY | Self::Gamma => (0.05, 5.0, 0.1),
            Self::RotationDeg => (-1080.0, 1080.0, 15.0),
            Self::Opacity => (0.0, 1.0, 0.05),
            Self::Brightness => (-1.0, 1.0, 0.05),
            Self::Contrast | Self::Saturation => (0.0, 4.0, 0.1),
            Self::HueDeg => (-180.0, 180.0, 15.0),
            Self::SubtitleFontSize => (8.0, 200.0, 4.0),
            Self::AudioVolume => (-60.0, 24.0, 1.5),
            Self::AudioPan => (-1.0, 1.0, 0.1),
        }
    }

    /// 内部値を UI 表示単位へ変換する（拡大率 `%`・透明度 `%`・パン `%`）。
    #[must_use]
    pub fn internal_to_ui(self, internal: f64) -> f64 {
        match self {
            Self::ScaleX | Self::ScaleY | Self::AudioPan => internal * 100.0,
            Self::Opacity => (1.0 - internal) * 100.0,
            _ => internal,
        }
    }

    /// UI 入力値を内部モデル単位へ逆変換する。
    #[must_use]
    pub fn ui_to_internal(self, ui: f64) -> f64 {
        match self {
            Self::ScaleX | Self::ScaleY | Self::AudioPan => ui / 100.0,
            Self::Opacity => 1.0 - (ui / 100.0),
            _ => ui,
        }
    }

    /// UI 表示単位での推奨値域 `(min_ui, max_ui, step_ui)` を返す（常に `min_ui <= max_ui`）。
    #[must_use]
    pub const fn ui_bounds(self) -> (f64, f64, f64) {
        let (min_i, max_i, step_i) = self.bounds();
        match self {
            Self::ScaleX | Self::ScaleY | Self::AudioPan => {
                (min_i * 100.0, max_i * 100.0, step_i * 100.0)
            }
            Self::Opacity => (0.0, 100.0, step_i * 100.0),
            _ => (min_i, max_i, step_i),
        }
    }

    /// クリップから該当スロットの `AnimatedParameter` を取り出す。
    #[must_use]
    pub fn extract_param(self, clip: &Clip) -> Option<&AnimatedParameter> {
        let visual = match &clip.kind {
            ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Some(visual),
            ClipKind::Subtitle(sub) => {
                if self == Self::SubtitleFontSize {
                    return Some(&sub.font_size);
                }
                Some(&sub.visual)
            }
            ClipKind::Audio { .. } => None,
        };
        let audio = match &clip.kind {
            ClipKind::Video { audio, .. } => audio.as_ref(),
            ClipKind::Audio { audio, .. } => Some(audio),
            ClipKind::Image { .. } | ClipKind::Subtitle(_) => None,
        };

        match self {
            Self::PositionX => visual.map(|v| &v.x),
            Self::PositionY => visual.map(|v| &v.y),
            Self::ScaleX => visual.map(|v| &v.scale_x),
            Self::ScaleY => visual.map(|v| &v.scale_y),
            Self::RotationDeg => visual.map(|v| &v.rotation_deg),
            Self::AnchorX => visual.map(|v| &v.anchor_x),
            Self::AnchorY => visual.map(|v| &v.anchor_y),
            Self::Opacity => visual.map(|v| &v.opacity),
            Self::Brightness => visual.map(|v| &v.brightness),
            Self::Contrast => visual.map(|v| &v.contrast),
            Self::Saturation => visual.map(|v| &v.saturation),
            Self::HueDeg => visual.map(|v| &v.hue_deg),
            Self::Gamma => visual.map(|v| &v.gamma),
            Self::SubtitleFontSize => match &clip.kind {
                ClipKind::Subtitle(sub) => Some(&sub.font_size),
                _ => None,
            },
            Self::AudioVolume => audio.map(|a| &a.gain_db),
            Self::AudioPan => audio.map(|a| &a.pan),
        }
    }

    /// クリップから該当スロットの `AnimatedParameter` への可変参照を取り出す。
    pub fn extract_param_mut(self, clip: &mut Clip) -> Option<&mut AnimatedParameter> {
        let (visual, audio, font_size) = match &mut clip.kind {
            ClipKind::Video { visual, audio, .. } => (Some(visual), audio.as_mut(), None),
            ClipKind::Image { visual, .. } => (Some(visual), None, None),
            ClipKind::Subtitle(sub) => (Some(&mut sub.visual), None, Some(&mut sub.font_size)),
            ClipKind::Audio { audio, .. } => (None, Some(audio), None),
        };
        match self {
            Self::PositionX => visual.map(|v| &mut v.x),
            Self::PositionY => visual.map(|v| &mut v.y),
            Self::ScaleX => visual.map(|v| &mut v.scale_x),
            Self::ScaleY => visual.map(|v| &mut v.scale_y),
            Self::RotationDeg => visual.map(|v| &mut v.rotation_deg),
            Self::AnchorX => visual.map(|v| &mut v.anchor_x),
            Self::AnchorY => visual.map(|v| &mut v.anchor_y),
            Self::Opacity => visual.map(|v| &mut v.opacity),
            Self::Brightness => visual.map(|v| &mut v.brightness),
            Self::Contrast => visual.map(|v| &mut v.contrast),
            Self::Saturation => visual.map(|v| &mut v.saturation),
            Self::HueDeg => visual.map(|v| &mut v.hue_deg),
            Self::Gamma => visual.map(|v| &mut v.gamma),
            Self::SubtitleFontSize => font_size,
            Self::AudioVolume => audio.map(|a| &mut a.gain_db),
            Self::AudioPan => audio.map(|a| &mut a.pan),
        }
    }
}

/// インスペクタパネルの編集用下書き状態（`AppController` に保持）。
#[derive(Debug, Clone)]
pub struct InspectorState {
    /// 現在選択中のアニメーション対象スロット。
    pub active_slot: InspectorParameterSlot,
    /// `f(t)` 入力欄の現在のドラフト文字列。
    pub expr_draft: String,
    /// `f(t)` 数式検証エラー（正常時は `None`）。
    pub expr_error: Option<String>,
    /// `Expression` モードの微調整基準オフセット。
    pub expr_base_offset: f64,
    /// `Expression` モードの微調整倍率スケール。
    pub expr_scale: f64,
    /// 字幕テキスト入力欄のドラフト。
    pub subtitle_text_draft: String,
    /// 字幕フォントファミリのドラフト。
    pub font_family_draft: String,
    /// 字幕縁取り幅のドラフト。
    pub outline_width_draft: String,
    /// ベジェ制御点下書き `(x1, y1, x2, y2)`。
    pub bezier_draft: (f64, f64, f64, f64),
    /// 複数選択一括相対オフセット X 下書き。
    pub multi_dx_draft: String,
    /// 複数選択一括相対オフセット Y 下書き。
    pub multi_dy_draft: String,
    last_synced_clip_id: Option<ClipId>,
    last_synced_slot: Option<InspectorParameterSlot>,
    last_synced_segment_index: Option<usize>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            active_slot: InspectorParameterSlot::Opacity,
            expr_draft: "1.0".to_owned(),
            expr_error: None,
            expr_base_offset: 0.0,
            expr_scale: 1.0,
            subtitle_text_draft: String::new(),
            font_family_draft: "sans-serif".to_owned(),
            outline_width_draft: "2.0".to_owned(),
            bezier_draft: (0.42, 0.0, 0.58, 1.0),
            multi_dx_draft: "0".to_owned(),
            multi_dy_draft: "0".to_owned(),
            last_synced_clip_id: None,
            last_synced_slot: None,
            last_synced_segment_index: None,
        }
    }
}

impl InspectorState {
    /// 選択クリップ・アクティブスロット・セグメントに合わせて UI 下書き値を同期する。
    pub fn sync_from_clip(
        &mut self,
        doc: &ProjectDocument,
        clip: &Clip,
        playhead: FrameIndex,
    ) -> bool {
        self.sync_from_clip_and_segment(doc, clip, playhead, self.last_synced_segment_index)
    }

    /// セグメント切替も含めて下書き値を同期する（クリップ/スロット/セグメント変化時に `true`）。
    pub fn sync_from_clip_and_segment(
        &mut self,
        doc: &ProjectDocument,
        clip: &Clip,
        playhead: FrameIndex,
        segment_index: Option<usize>,
    ) -> bool {
        let available = InspectorParameterSlot::available_for(clip);
        if !available.contains(&self.active_slot)
            && let Some(&first) = available.first()
        {
            self.active_slot = first;
        }

        let clip_changed = self.last_synced_clip_id.as_ref() != Some(&clip.id);
        let slot_changed = self.last_synced_slot != Some(self.active_slot);
        let seg_changed = self.last_synced_segment_index != segment_index;
        if !clip_changed && !slot_changed && !seg_changed {
            return false;
        }

        self.last_synced_clip_id = Some(clip.id.clone());
        self.last_synced_slot = Some(self.active_slot);
        self.last_synced_segment_index = segment_index;

        if let ClipKind::Subtitle(sub) = &clip.kind {
            self.subtitle_text_draft.clone_from(&sub.text);
            self.font_family_draft.clone_from(&sub.font_family);
            self.outline_width_draft = format!("{:.1}", sub.outline_width);
        } else {
            self.subtitle_text_draft.clear();
        }

        let ctx = evaluation_context_for_clip(doc, clip, playhead);
        if let Some(param) = self.active_slot.extract_param(clip) {
            if let Some(view) =
                read_trackbar_segment_view(doc, clip, self.active_slot, segment_index.unwrap_or(0))
                && let EasingCurve::CubicBezier { x1, y1, x2, y2 } = view.segment_curve
            {
                self.bezier_draft = (x1, y1, x2, y2);
            }
            match param {
                AnimatedParameter::Expression {
                    expr,
                    base_offset,
                    scale,
                } => {
                    self.expr_draft.clone_from(expr);
                    self.expr_base_offset = *base_offset;
                    self.expr_scale = *scale;
                    self.expr_error =
                        validate_expression_and_tuning(expr, *base_offset, *scale).err();
                }
                other => {
                    let current = other.evaluate(ctx).unwrap_or(0.0);
                    self.expr_draft = format!("{current:.3}");
                    self.expr_base_offset = 0.0;
                    self.expr_scale = 1.0;
                    self.expr_error = None;
                }
            }
        }
        true
    }

    /// 強制的に次回同期を発火させる（モード切替や Undo 後など）。
    pub fn invalidate_sync(&mut self) {
        self.last_synced_slot = None;
    }

    /// 直前に同期したクリップ ID を返す。
    #[must_use]
    pub const fn last_synced_clip_id(&self) -> Option<&ClipId> {
        self.last_synced_clip_id.as_ref()
    }
}

/// 選択セグメント単位で読み出した `TrackBarRow` 表示データ。
#[derive(Debug, Clone, PartialEq)]
pub struct TrackBarSegmentView {
    /// パラメータスロット。
    pub slot: InspectorParameterSlot,
    /// 実セグメント番号 (`0..segment_count`)。
    pub segment_index: usize,
    /// 総セグメント数 (`>= 1`)。
    pub segment_count: usize,
    /// セグメント開始エフェクト基準フレーム。
    pub seg_start_eff: i64,
    /// セグメント終了エフェクト基準フレーム。
    pub seg_end_eff: i64,
    /// セグメント開始値（内部単位）。
    pub start_value: f64,
    /// セグメント終了値（内部単位）。
    pub end_value: f64,
    /// 基本パラメータモード。
    pub mode: InspectorMode,
    /// 全区間一致時の移動モードプリセット（混在時は `None`）。
    pub active_preset: Option<MovementModePreset>,
    /// 当該セグメントの補間カーブ。
    pub segment_curve: EasingCurve,
    /// カットされた連続 `Easing` クリップか (`[分割継続]`)。
    pub is_cut_easing: bool,
    /// セグメントごとに異なるカーブが混在しているか (`[混在]`)。
    pub is_mixed_segments: bool,
    /// グリッドに整列していない任意キーフレームが含まれるか。
    pub unaligned_keyframes: bool,
}

impl TrackBarSegmentView {
    /// 当該セグメントが 3 次ベジェ曲線補間か。
    #[must_use]
    pub const fn is_bezier(&self) -> bool {
        !matches!(
            self.mode,
            InspectorMode::Constant | InspectorMode::Expression
        ) && matches!(self.segment_curve, EasingCurve::CubicBezier { .. })
    }

    /// 当該パラメータが数式 `f(t)` モードか。
    #[must_use]
    pub const fn is_expression(&self) -> bool {
        matches!(self.mode, InspectorMode::Expression)
    }
}

/// クリップとアクティブセグメントからトラックバー 1 行分の表示情報を読み取る。
#[must_use]
pub fn read_trackbar_segment_view(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    segment_index: usize,
) -> Option<TrackBarSegmentView> {
    let param = slot.extract_param(clip)?;
    let grid = clip.grid_frames();
    let seg_count = grid.len().saturating_sub(1).max(1);
    let seg_idx = segment_index.min(seg_count - 1);
    let seg_start_eff = grid[seg_idx];
    let seg_end_eff = grid.get(seg_idx + 1).copied().unwrap_or(seg_start_eff);
    let is_cut = is_clip_effect_cut(clip);

    let start_value = eval_param_at_eff(doc, clip, param, seg_start_eff);
    let end_value = eval_param_at_eff(doc, clip, param, seg_end_eff);
    let mode = InspectorMode::from_param(param);

    let (active_preset, segment_curve, is_cut_easing, is_mixed_segments, unaligned_keyframes) =
        classify_segment_mode(param, &grid, seg_start_eff, is_cut);

    Some(TrackBarSegmentView {
        slot,
        segment_index: seg_idx,
        segment_count: seg_count,
        seg_start_eff,
        seg_end_eff,
        start_value,
        end_value,
        mode,
        active_preset,
        segment_curve,
        is_cut_easing,
        is_mixed_segments,
        unaligned_keyframes,
    })
}

fn classify_segment_mode(
    param: &AnimatedParameter,
    grid: &[i64],
    seg_start_eff: i64,
    is_cut: bool,
) -> (Option<MovementModePreset>, EasingCurve, bool, bool, bool) {
    match param {
        AnimatedParameter::Constant { .. } => (
            Some(MovementModePreset::Constant),
            EasingCurve::Linear,
            false,
            false,
            false,
        ),
        AnimatedParameter::Easing { curve, .. } => (
            Some(MovementModePreset::from_curve(*curve)),
            *curve,
            is_cut,
            false,
            false,
        ),
        AnimatedParameter::Keyframes { keyframes, .. } => {
            let unaligned = grid
                .iter()
                .any(|g| keyframes.iter().all(|kf| kf.frame != *g))
                || keyframes.iter().any(|kf| !grid.contains(&kf.frame));
            let seg_curve = curve_at_or_before(keyframes, seg_start_eff);
            let mixed = has_mixed_segment_curves(keyframes, grid);
            let preset = if mixed {
                None
            } else {
                Some(MovementModePreset::from_curve(seg_curve))
            };
            (preset, seg_curve, false, mixed, unaligned)
        }
        AnimatedParameter::Expression { .. } => (
            Some(MovementModePreset::Expression),
            EasingCurve::Linear,
            false,
            false,
            false,
        ),
    }
}

fn curve_at_or_before(keyframes: &[Keyframe], frame: i64) -> EasingCurve {
    keyframes
        .iter()
        .filter(|kf| kf.frame <= frame)
        .max_by_key(|kf| kf.frame)
        .map_or(EasingCurve::Linear, |kf| kf.easing)
}

fn has_mixed_segment_curves(keyframes: &[Keyframe], grid: &[i64]) -> bool {
    if grid.len() < 3 {
        return false;
    }
    let first = curve_at_or_before(keyframes, grid[0]);
    grid[..grid.len() - 1]
        .iter()
        .any(|&g| curve_at_or_before(keyframes, g) != first)
}

#[must_use]
fn is_clip_effect_cut(clip: &Clip) -> bool {
    clip.effect_offset_frames != 0
        || clip
            .effect_duration_frames
            .is_some_and(|ed| ed != clip.duration)
}

#[allow(clippy::cast_sign_loss)]
fn eval_param_at_eff(
    doc: &ProjectDocument,
    clip: &Clip,
    param: &AnimatedParameter,
    eff_frame: i64,
) -> f64 {
    let local = eff_frame.saturating_sub(clip.effect_offset_frames).max(0) as u64;
    let effect_dur = clip.effect_duration_frames.unwrap_or(clip.duration);
    let ctx = EvaluationContext::new(local, clip.effect_offset_frames, effect_dur, doc.fps);
    param.evaluate(ctx).unwrap_or(0.0)
}

/// 再生ヘッド位置におけるクリップのエフェクト基準フレーム番号を計算する（分割後も曲線連続）。
#[must_use]
#[allow(clippy::cast_possible_wrap)]
pub const fn effect_frame_at_playhead(clip: &Clip, playhead: FrameIndex) -> i64 {
    let local = (playhead.get() as i64) - (clip.timeline_start.get() as i64);
    local + clip.effect_offset_frames
}

/// クリップと再生ヘッドから `EvaluationContext` を組み立てる。
#[must_use]
pub fn evaluation_context_for_clip(
    doc: &ProjectDocument,
    clip: &Clip,
    playhead: FrameIndex,
) -> EvaluationContext {
    let local_frame = playhead.get().saturating_sub(clip.timeline_start.get());
    EvaluationContext::new(
        local_frame,
        clip.effect_offset_frames,
        clip.effect_duration_frames.unwrap_or(clip.duration),
        doc.fps,
    )
}

/// 数式 `expr` と微調整係数 `base_offset`/`scale` の構文・有限値チェックを行う。
///
/// # Errors
///
/// 未許可変数・構文エラー・非有限値が含まれる場合にエラー説明文字列を返す。
pub fn validate_expression_and_tuning(
    expr: &str,
    base_offset: f64,
    scale: f64,
) -> Result<(), String> {
    if !base_offset.is_finite() || !scale.is_finite() {
        return Err("base_offset と scale は有限値である必要があります".to_owned());
    }
    CompiledExpression::parse(expr)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// パラメータを別モードへ切り替える [`EditCommand`] を構築する（現在値を可能な限り維持）。
///
/// # Errors
///
/// 指定スロットがクリップ種別に適合しない場合。
#[allow(clippy::cast_possible_wrap)]
pub fn build_switch_mode_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    target_mode: InspectorMode,
    playhead: FrameIndex,
) -> Result<EditCommand, String> {
    let param = slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", slot.label()))?;
    let ctx = evaluation_context_for_clip(doc, clip, playhead);
    let current = param.evaluate(ctx).unwrap_or(0.0);

    let next_param = match target_mode {
        InspectorMode::Constant => AnimatedParameter::constant(current),
        InspectorMode::Easing => {
            if let AnimatedParameter::Easing { .. } = param {
                param.clone()
            } else {
                let (_, max_b, step) = slot.bounds();
                let delta = step.min((max_b - current).abs());
                AnimatedParameter::Easing {
                    start_value: current,
                    end_value: (current + delta).min(max_b),
                    curve: EasingCurve::EaseInOut,
                    extrapolation: ExtrapolationMode::Clamp,
                }
            }
        }
        InspectorMode::Keyframes => {
            if let AnimatedParameter::Keyframes { .. } = param {
                param.clone()
            } else {
                let first_eff = clip.effect_offset_frames;
                let span = clip.effect_duration_frames.unwrap_or(clip.duration).get();
                let last_eff = first_eff + (span.saturating_sub(1) as i64);
                let mut keyframes = vec![Keyframe::new(first_eff, current)];
                if last_eff > first_eff {
                    keyframes.push(Keyframe::new(last_eff, current));
                }
                AnimatedParameter::Keyframes {
                    keyframes,
                    extrapolation: ExtrapolationMode::Clamp,
                }
            }
        }
        InspectorMode::Expression => match param {
            AnimatedParameter::Expression { .. } => param.clone(),
            _ => AnimatedParameter::expression_with_tuning(format!("{current:.3}"), 0.0, 1.0),
        },
    };

    Ok(set_animated_command(clip, slot, next_param))
}

/// セグメント端点変更に必要な入力パラメータ群。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentEndpointEdit {
    /// 対象パラメータスロット。
    pub slot: InspectorParameterSlot,
    /// セグメント番号。
    pub segment_index: usize,
    /// `false` で左端（開始点）、`true` で右端（終了点）。
    pub is_end: bool,
    /// 設定する新しい内部値。
    pub new_value: f64,
    /// `ScaleX`/`ScaleY` アスペクト比連動ロック。
    pub scale_aspect_locked: bool,
    /// ドラッグ開始時比率固定用の任意の明示比率 (`paired / primary`)。
    pub aspect_ratio_override: Option<f64>,
}

/// 移動モード切替に必要な入力パラメータ群。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementModeEdit {
    /// 対象パラメータスロット。
    pub slot: InspectorParameterSlot,
    /// 適用する移動モードプリセット。
    pub preset: MovementModePreset,
    /// 現在再生フレーム位置。
    pub playhead: FrameIndex,
    /// `MovementModePreset::Bezier` 時に適用する制御点カーブ。
    pub bezier_curve: EasingCurve,
    /// `ScaleX`/`ScaleY` アスペクト比連動ロック。
    pub scale_aspect_locked: bool,
}

/// 単一セグメント端点（開始値または終了値）を変更する [`EditCommand`] を構築する。
///
/// カット済み `Easing` やグリッド未整列 `Keyframes` は現在カーブ値を保持したまま自動整列・昇格する。
///
/// # Errors
///
/// 非有限値、スロット未存在、または数式モードへの直接端点編集時。
pub fn build_set_segment_endpoint_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    segment_index: usize,
    is_end: bool,
    new_value: f64,
) -> Result<EditCommand, String> {
    if !new_value.is_finite() {
        return Err("パラメータ値は有限値である必要があります".to_owned());
    }
    let param = slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", slot.label()))?;
    let next = compute_endpoint_updated_param(doc, clip, param, segment_index, is_end, new_value)?;
    Ok(set_animated_command(clip, slot, next))
}

/// アスペクト比連動を考慮してセグメント端点を変更する [`EditCommand`] を構築する。
///
/// # Errors
///
/// 非有限値・スロット未存在・数式モード編集時。
pub fn build_set_segment_endpoint_with_lock(
    doc: &ProjectDocument,
    clip: &Clip,
    edit: &SegmentEndpointEdit,
) -> Result<EditCommand, String> {
    let primary_cmd = build_set_segment_endpoint_command(
        doc,
        clip,
        edit.slot,
        edit.segment_index,
        edit.is_end,
        edit.new_value,
    )?;
    if !edit.scale_aspect_locked {
        return Ok(primary_cmd);
    }
    let Some(paired_slot) = paired_scale_slot(edit.slot) else {
        return Ok(primary_cmd);
    };
    let ratio = edit.aspect_ratio_override.unwrap_or_else(|| {
        compute_scale_aspect_ratio(
            doc,
            clip,
            edit.slot,
            paired_slot,
            edit.segment_index,
            edit.is_end,
        )
    });
    let (p_min, p_max, _) = paired_slot.bounds();
    let paired_val = (edit.new_value * ratio).clamp(p_min, p_max);
    let paired_cmd = build_set_segment_endpoint_command(
        doc,
        clip,
        paired_slot,
        edit.segment_index,
        edit.is_end,
        paired_val,
    )?;
    Ok(EditCommand::Batch {
        commands: vec![primary_cmd, paired_cmd],
    })
}

/// 拡大率 `ScaleX` ⇔ `ScaleY` の対になるスロットを返す。
#[must_use]
pub const fn paired_scale_slot(slot: InspectorParameterSlot) -> Option<InspectorParameterSlot> {
    match slot {
        InspectorParameterSlot::ScaleX => Some(InspectorParameterSlot::ScaleY),
        InspectorParameterSlot::ScaleY => Some(InspectorParameterSlot::ScaleX),
        _ => None,
    }
}

/// 拡大率ロック時に維持する `paired / primary` アスペクト比率を計算する。
#[must_use]
pub fn compute_scale_aspect_ratio(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    paired_slot: InspectorParameterSlot,
    segment_index: usize,
    is_end: bool,
) -> f64 {
    let primary_old = read_trackbar_segment_view(doc, clip, slot, segment_index)
        .map_or(1.0, |v| if is_end { v.end_value } else { v.start_value });
    let paired_old = read_trackbar_segment_view(doc, clip, paired_slot, segment_index)
        .map_or(1.0, |v| if is_end { v.end_value } else { v.start_value });
    if primary_old.abs() > 1e-6 {
        paired_old / primary_old
    } else {
        1.0
    }
}

fn compute_endpoint_updated_param(
    doc: &ProjectDocument,
    clip: &Clip,
    param: &AnimatedParameter,
    segment_index: usize,
    is_end: bool,
    new_value: f64,
) -> Result<AnimatedParameter, String> {
    let grid = clip.grid_frames();
    let seg_count = grid.len().saturating_sub(1).max(1);
    let seg_idx = segment_index.min(seg_count - 1);
    let target_frame = if grid.len() >= 2 && is_end {
        grid[seg_idx + 1]
    } else {
        grid[seg_idx]
    };
    let is_cut = is_clip_effect_cut(clip);

    match param {
        AnimatedParameter::Constant { value } => {
            if !is_end || grid.len() < 2 {
                Ok(AnimatedParameter::constant(new_value))
            } else if grid.len() == 2 && !is_cut {
                Ok(AnimatedParameter::Easing {
                    start_value: *value,
                    end_value: new_value,
                    curve: EasingCurve::Linear,
                    extrapolation: ExtrapolationMode::Clamp,
                })
            } else {
                let keyframes = grid
                    .iter()
                    .map(|&f| {
                        let val = if f == target_frame { new_value } else { *value };
                        Keyframe::with_easing(f, val, EasingCurve::Linear)
                    })
                    .collect();
                Ok(AnimatedParameter::Keyframes {
                    keyframes,
                    extrapolation: ExtrapolationMode::Clamp,
                })
            }
        }
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => {
            if grid.len() <= 2 && !is_cut {
                Ok(AnimatedParameter::Easing {
                    start_value: if is_end { *start_value } else { new_value },
                    end_value: if is_end { new_value } else { *end_value },
                    curve: *curve,
                    extrapolation: *extrapolation,
                })
            } else {
                let keyframes = grid
                    .iter()
                    .map(|&f| {
                        let val = if f == target_frame {
                            new_value
                        } else {
                            eval_param_at_eff(doc, clip, param, f)
                        };
                        Keyframe::with_easing(f, val, *curve)
                    })
                    .collect();
                Ok(AnimatedParameter::Keyframes {
                    keyframes,
                    extrapolation: *extrapolation,
                })
            }
        }
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => {
            let mut next = harvest_aligned_keyframes(doc, clip, param, keyframes, &grid);
            if let Some(found) = next.iter_mut().find(|kf| kf.frame == target_frame) {
                found.value = new_value;
            } else {
                let c = curve_at_or_before(&next, target_frame);
                next.push(Keyframe::with_easing(target_frame, new_value, c));
                next.sort_by_key(|kf| kf.frame);
            }
            Ok(AnimatedParameter::Keyframes {
                keyframes: next,
                extrapolation: *extrapolation,
            })
        }
        AnimatedParameter::Expression { .. } => {
            Err("数式 f(t) モードでは端点スライダではなく数式欄から編集してください".to_owned())
        }
    }
}

fn harvest_aligned_keyframes(
    doc: &ProjectDocument,
    clip: &Clip,
    param: &AnimatedParameter,
    existing: &[Keyframe],
    grid: &[i64],
) -> Vec<Keyframe> {
    let mut out = existing.to_vec();
    for &g in grid {
        if !out.iter().any(|kf| kf.frame == g) {
            let val = eval_param_at_eff(doc, clip, param, g);
            let curve = curve_at_or_before(existing, g);
            out.push(Keyframe::with_easing(g, val, curve));
        }
    }
    out.sort_by_key(|kf| kf.frame);
    out
}

/// 選択スロットに全区間共通の移動モードプリセットを適用する [`EditCommand`] を構築する。
///
/// # Errors
///
/// 指定スロットがクリップ種別に適合しない場合。
pub fn build_apply_movement_mode_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    preset: MovementModePreset,
    playhead: FrameIndex,
) -> Result<EditCommand, String> {
    let default_bezier = slot
        .extract_param(clip)
        .and_then(|p| match p {
            AnimatedParameter::Easing {
                curve: c @ EasingCurve::CubicBezier { .. },
                ..
            } => Some(*c),
            AnimatedParameter::Keyframes { keyframes, .. } => keyframes
                .iter()
                .find(|kf| matches!(kf.easing, EasingCurve::CubicBezier { .. }))
                .map(|kf| kf.easing),
            _ => None,
        })
        .unwrap_or(EasingCurve::CubicBezier {
            x1: 0.42,
            y1: 0.0,
            x2: 0.58,
            y2: 1.0,
        });
    build_apply_movement_mode_with_options(
        doc,
        clip,
        &MovementModeEdit {
            slot,
            preset,
            playhead,
            bezier_curve: default_bezier,
            scale_aspect_locked: false,
        },
    )
}

/// オプション付き（ベジェ制御点・拡大率アスペクトロック）移動モード変更コマンドを構築する。
///
/// # Errors
///
/// 指定スロットがクリップ種別に適合しない場合。
pub fn build_apply_movement_mode_with_options(
    doc: &ProjectDocument,
    clip: &Clip,
    edit: &MovementModeEdit,
) -> Result<EditCommand, String> {
    let primary_param = edit
        .slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", edit.slot.label()))?;
    let next_primary = apply_movement_mode_param(doc, clip, primary_param, edit);
    let primary_cmd = set_animated_command(clip, edit.slot, next_primary);

    if !edit.scale_aspect_locked {
        return Ok(primary_cmd);
    }
    let Some(paired_slot) = paired_scale_slot(edit.slot) else {
        return Ok(primary_cmd);
    };
    let Some(paired_param) = paired_slot.extract_param(clip) else {
        return Ok(primary_cmd);
    };
    let next_paired = apply_movement_mode_param(doc, clip, paired_param, edit);
    Ok(EditCommand::Batch {
        commands: vec![
            primary_cmd,
            set_animated_command(clip, paired_slot, next_paired),
        ],
    })
}

/// ベジェ制御点を更新して現在パラメータへ適用する [`EditCommand`] を構築する。
///
/// # Errors
///
/// 非有限値または対象スロット不在時。
pub fn build_apply_bezier_curve_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    curve: EasingCurve,
    playhead: FrameIndex,
) -> Result<EditCommand, String> {
    if let EasingCurve::CubicBezier { x1, y1, x2, y2 } = curve
        && (!x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite())
    {
        return Err("ベジェ制御点は有限値である必要があります".to_owned());
    }
    build_apply_movement_mode_with_options(
        doc,
        clip,
        &MovementModeEdit {
            slot,
            preset: MovementModePreset::Bezier,
            playhead,
            bezier_curve: curve,
            scale_aspect_locked: false,
        },
    )
}

fn apply_movement_mode_param(
    doc: &ProjectDocument,
    clip: &Clip,
    param: &AnimatedParameter,
    edit: &MovementModeEdit,
) -> AnimatedParameter {
    let grid = clip.grid_frames();
    let ctx_head = evaluation_context_for_clip(doc, clip, edit.playhead);
    let current = param.evaluate(ctx_head).unwrap_or(0.0);

    match edit.preset {
        MovementModePreset::Constant => AnimatedParameter::constant(current),
        MovementModePreset::Expression => match param {
            AnimatedParameter::Expression { .. } => param.clone(),
            _ => AnimatedParameter::expression_with_tuning(format!("{current:.3}"), 0.0, 1.0),
        },
        MovementModePreset::Keyframes => {
            let (base_kfs, extrap, default_curve) = match param {
                AnimatedParameter::Keyframes {
                    keyframes,
                    extrapolation,
                } => (keyframes.clone(), *extrapolation, EasingCurve::Linear),
                AnimatedParameter::Easing {
                    curve,
                    extrapolation,
                    ..
                } => (Vec::new(), *extrapolation, *curve),
                _ => (Vec::new(), ExtrapolationMode::Clamp, EasingCurve::Linear),
            };
            let mut out = harvest_aligned_keyframes(doc, clip, param, &base_kfs, &grid);
            if !matches!(param, AnimatedParameter::Keyframes { .. }) {
                for kf in &mut out {
                    kf.easing = default_curve;
                }
            }
            AnimatedParameter::Keyframes {
                keyframes: out,
                extrapolation: extrap,
            }
        }
        MovementModePreset::Linear
        | MovementModePreset::EaseInOut
        | MovementModePreset::EaseIn
        | MovementModePreset::EaseOut
        | MovementModePreset::Bezier => {
            let target_curve = preset_to_curve(edit.preset, edit.bezier_curve);
            apply_curve_to_all_segments(doc, clip, param, &grid, target_curve)
        }
    }
}

const fn preset_to_curve(preset: MovementModePreset, bezier: EasingCurve) -> EasingCurve {
    match preset {
        MovementModePreset::EaseIn => EasingCurve::EaseIn,
        MovementModePreset::EaseOut => EasingCurve::EaseOut,
        MovementModePreset::EaseInOut => EasingCurve::EaseInOut,
        MovementModePreset::Bezier => match bezier {
            c @ EasingCurve::CubicBezier { .. } => c,
            _ => EasingCurve::CubicBezier {
                x1: 0.42,
                y1: 0.0,
                x2: 0.58,
                y2: 1.0,
            },
        },
        MovementModePreset::Linear
        | MovementModePreset::Constant
        | MovementModePreset::Keyframes
        | MovementModePreset::Expression => EasingCurve::Linear,
    }
}

fn apply_curve_to_all_segments(
    doc: &ProjectDocument,
    clip: &Clip,
    param: &AnimatedParameter,
    grid: &[i64],
    target_curve: EasingCurve,
) -> AnimatedParameter {
    let is_cut = is_clip_effect_cut(clip);
    let has_midpoints = grid.len() >= 3;
    let has_extra_internal = matches!(
        param,
        AnimatedParameter::Keyframes { keyframes, .. }
            if keyframes.iter().any(|kf| !grid.contains(&kf.frame))
    );

    if !has_midpoints && !has_extra_internal {
        if let AnimatedParameter::Easing {
            start_value,
            end_value,
            extrapolation,
            ..
        } = param
        {
            return AnimatedParameter::Easing {
                start_value: *start_value,
                end_value: *end_value,
                curve: target_curve,
                extrapolation: *extrapolation,
            };
        }
        if !is_cut {
            let start_v = eval_param_at_eff(doc, clip, param, grid[0]);
            let end_v = eval_param_at_eff(doc, clip, param, *grid.last().unwrap_or(&grid[0]));
            return AnimatedParameter::Easing {
                start_value: start_v,
                end_value: end_v,
                curve: target_curve,
                extrapolation: ExtrapolationMode::Clamp,
            };
        }
    }

    let (base_kfs, extrap) = match param {
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => (keyframes.clone(), *extrapolation),
        AnimatedParameter::Easing { extrapolation, .. } => (Vec::new(), *extrapolation),
        _ => (Vec::new(), ExtrapolationMode::Clamp),
    };
    let mut aligned = harvest_aligned_keyframes(doc, clip, param, &base_kfs, grid);
    for kf in &mut aligned {
        kf.easing = target_curve;
    }
    AnimatedParameter::Keyframes {
        keyframes: aligned,
        extrapolation: extrap,
    }
}

/// ドラッグ中のライブプレビュー用パラメータ対 `(主スロット, 連動スケール対)` を算出する。
#[must_use]
pub fn build_transient_drag_param_pair(
    doc: &ProjectDocument,
    clip: &Clip,
    orig_param: &AnimatedParameter,
    orig_paired: Option<&AnimatedParameter>,
    edit: &SegmentEndpointEdit,
) -> Option<(AnimatedParameter, Option<AnimatedParameter>)> {
    if !edit.new_value.is_finite() {
        return None;
    }
    let next_primary = compute_endpoint_updated_param(
        doc,
        clip,
        orig_param,
        edit.segment_index,
        edit.is_end,
        edit.new_value,
    )
    .ok()?;

    let next_paired = if edit.scale_aspect_locked
        && let (Some(paired_orig), Some(paired_slot)) = (orig_paired, paired_scale_slot(edit.slot))
    {
        let ratio = edit.aspect_ratio_override.unwrap_or(1.0);
        let (p_min, p_max, _) = paired_slot.bounds();
        let paired_val = (edit.new_value * ratio).clamp(p_min, p_max);
        compute_endpoint_updated_param(
            doc,
            clip,
            paired_orig,
            edit.segment_index,
            edit.is_end,
            paired_val,
        )
        .ok()
    } else {
        None
    };

    Some((next_primary, next_paired))
}

/// トラックバー水平ドラッグ開始時のパラメータ群。
#[derive(Debug, Clone)]
pub struct TrackBarDragStart {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// 操作対象スロット。
    pub slot: InspectorParameterSlot,
    /// 編集中のセグメント番号。
    pub segment_index: usize,
    /// `false` で左端（開始点）、`true` で右端（終了点）。
    pub is_end: bool,
    /// 押下時のポインタ X 座標 (px)。
    pub pointer_x_px: f32,
    /// スライダ仮想幅 (px)。
    pub track_width_px: f32,
}

/// トラックバー水平ドラッグの継続状態（プレビュー即時反映 + 終了時単一 Undo コミット）。
#[derive(Debug, Clone)]
pub struct ActiveTrackBarDrag {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// 操作中スロット。
    pub slot: InspectorParameterSlot,
    /// 編集中のセグメント番号。
    pub segment_index: usize,
    /// `false` で左端（開始点）、`true` で右端（終了点）。
    pub is_end: bool,
    /// アスペクト比連動ロック中か。
    pub scale_aspect_locked: bool,
    /// ドラッグ開始時に確定した `paired / primary` 比率。
    pub aspect_ratio: Option<f64>,
    /// 直近ポインタ X 座標 (px)。
    pub last_pointer_x_px: f32,
    /// 現在の UI 単位値。
    pub current_ui: f64,
    /// スライダ仮想幅 (px)。
    pub track_width_px: f32,
    /// ドラッグ開始前の元の `AnimatedParameter`。
    pub original_param: AnimatedParameter,
    /// ドラッグ開始前の連動側（`ScaleX`/`ScaleY`）の元の `AnimatedParameter`。
    pub original_paired_param: Option<AnimatedParameter>,
    /// ドラッグ開始前のクリップ中間点配列（不変性保証）。
    pub original_clip_midpoints: Vec<i64>,
}

/// 指定エフェクトフレームのキーフレームを追加または更新するコマンドを構築する。
///
/// # Errors
///
/// スロット非対応または値が非有限の場合。
pub fn build_upsert_keyframe_command(
    clip: &Clip,
    slot: InspectorParameterSlot,
    effect_frame: i64,
    value: f64,
    interpolation: EasingCurve,
) -> Result<EditCommand, String> {
    if !value.is_finite() {
        return Err("キーフレーム値は有限値である必要があります".to_owned());
    }
    let existing = slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", slot.label()))?;

    let (mut keyframes, extrapolation) = match existing {
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => (keyframes.clone(), *extrapolation),
        _ => (Vec::new(), ExtrapolationMode::Clamp),
    };

    if let Some(found) = keyframes.iter_mut().find(|kf| kf.frame == effect_frame) {
        found.value = value;
        found.easing = interpolation;
    } else {
        keyframes.push(Keyframe::with_easing(effect_frame, value, interpolation));
    }
    keyframes.sort_by_key(|kf| kf.frame);

    Ok(set_animated_command(
        clip,
        slot,
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        },
    ))
}

/// 指定エフェクトフレームのキーフレームを削除するコマンドを構築する。
///
/// # Errors
///
/// 対象キーフレームが見つからない、または最後の 1 個を削除しようとした場合。
pub fn build_remove_keyframe_command(
    clip: &Clip,
    slot: InspectorParameterSlot,
    effect_frame: i64,
) -> Result<EditCommand, String> {
    let existing = slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", slot.label()))?;
    let AnimatedParameter::Keyframes {
        keyframes,
        extrapolation,
    } = existing
    else {
        return Err("現在はキーフレームモードではありません".to_owned());
    };

    if keyframes.len() <= 1 {
        return Err("最後のキーフレームは削除できません".to_owned());
    }
    let next: Vec<Keyframe> = keyframes
        .iter()
        .filter(|kf| kf.frame != effect_frame)
        .cloned()
        .collect();
    if next.len() == keyframes.len() {
        return Err(format!(
            "フレーム {effect_frame} のキーフレームが見つかりません"
        ));
    }

    Ok(set_animated_command(
        clip,
        slot,
        AnimatedParameter::Keyframes {
            keyframes: next,
            extrapolation: *extrapolation,
        },
    ))
}

/// `Expression` モードの数式と `base_offset`/`scale` を適用するコマンドを構築する。
///
/// # Errors
///
/// 数式エラーまたは非有限値の場合。
pub fn build_expression_command(
    clip: &Clip,
    slot: InspectorParameterSlot,
    expr: &str,
    base_offset: f64,
    scale: f64,
) -> Result<EditCommand, String> {
    validate_expression_and_tuning(expr, base_offset, scale)?;
    Ok(set_animated_command(
        clip,
        slot,
        AnimatedParameter::expression_with_tuning(expr, base_offset, scale),
    ))
}

/// クリップの色調補正値が既定値から変化しているかを返す（セクション青ドット表示用）。
#[must_use]
pub fn clip_has_non_default_color_adjustments(clip: &Clip) -> bool {
    InspectorParameterSlot::color_slots()
        .iter()
        .any(|&slot| match slot.extract_param(clip) {
            Some(AnimatedParameter::Constant { value }) => {
                (*value - slot.default_internal_value()).abs() > 1e-4
            }
            Some(_) => true,
            None => false,
        })
}

/// クリップの色調補正 5 項目を既定値へリセットする [`EditCommand::Batch`] を構築する。
#[must_use]
pub fn build_reset_color_adjustments_command(clip: &Clip) -> Option<EditCommand> {
    let mut commands = Vec::new();
    for &slot in InspectorParameterSlot::color_slots() {
        if slot.extract_param(clip).is_some() {
            commands.push(set_animated_command(
                clip,
                slot,
                AnimatedParameter::constant(slot.default_internal_value()),
            ));
        }
    }
    if commands.is_empty() {
        None
    } else {
        Some(EditCommand::Batch { commands })
    }
}

/// クリップの全パラメータ（標準描画・基準点・色調・音声・字幕スタイル）を既定値に戻す。
#[must_use]
pub fn build_reset_clip_parameters_command(clip: &Clip) -> Option<EditCommand> {
    let mut commands = Vec::new();
    for slot in InspectorParameterSlot::available_for(clip) {
        commands.push(set_animated_command(
            clip,
            slot,
            AnimatedParameter::constant(slot.default_internal_value()),
        ));
    }
    match &clip.kind {
        ClipKind::Video { audio, .. } => {
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::BlendMode,
                value: ParameterValue::BlendMode {
                    value: BlendMode::Normal,
                },
            });
            if audio.is_some() {
                commands.push(EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Id(clip.id.clone()),
                    },
                    path: ParameterPath::AudioMuted,
                    value: ParameterValue::Bool { value: false },
                });
            }
        }
        ClipKind::Image { .. } => {
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::BlendMode,
                value: ParameterValue::BlendMode {
                    value: BlendMode::Normal,
                },
            });
        }
        ClipKind::Subtitle(_) => {
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::BlendMode,
                value: ParameterValue::BlendMode {
                    value: BlendMode::Normal,
                },
            });
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::SubtitleTextColor,
                value: ParameterValue::Color {
                    value: RgbaColor::WHITE,
                },
            });
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::SubtitleOutlineColor,
                value: ParameterValue::Color {
                    value: RgbaColor::BLACK,
                },
            });
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::SubtitleOutlineWidth,
                value: ParameterValue::Number { value: 2.0 },
            });
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::SubtitleAlignment,
                value: ParameterValue::TextAlignment {
                    value: TextAlignment::Center,
                },
            });
        }
        ClipKind::Audio { .. } => {
            commands.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::AudioMuted,
                value: ParameterValue::Bool { value: false },
            });
        }
    }
    if commands.is_empty() {
        None
    } else {
        Some(EditCommand::Batch { commands })
    }
}

/// 選択中の複数クリップに対して `X`/`Y` 相対移動を一括適用する `Batch` コマンドを構築する。
///
/// # Errors
///
/// `dx` または `dy` が非有限、あるいは移動対象となる映像クリップが 1 つもない場合。
pub fn build_batch_relative_xy_offset_command(
    doc: &ProjectDocument,
    clip_ids: &[ClipId],
    dx: f64,
    dy: f64,
) -> Result<EditCommand, String> {
    if !dx.is_finite() || !dy.is_finite() {
        return Err("相対オフセット値は有限値である必要があります".to_owned());
    }
    let mut commands = Vec::new();
    for cid in clip_ids {
        let Some((layer, clip)) = doc.find_clip(cid) else {
            continue;
        };
        if layer.locked {
            continue;
        }
        if dx.abs() > 1e-6
            && let Some(x_param) = InspectorParameterSlot::PositionX.extract_param(clip)
        {
            commands.push(set_animated_command(
                clip,
                InspectorParameterSlot::PositionX,
                shift_param_by_delta(x_param, dx),
            ));
        }
        if dy.abs() > 1e-6
            && let Some(y_param) = InspectorParameterSlot::PositionY.extract_param(clip)
        {
            commands.push(set_animated_command(
                clip,
                InspectorParameterSlot::PositionY,
                shift_param_by_delta(y_param, dy),
            ));
        }
    }
    if commands.is_empty() {
        return Err("相対移動できる選択クリップまたは変更量がありません".to_owned());
    }
    Ok(EditCommand::Batch { commands })
}

fn shift_param_by_delta(param: &AnimatedParameter, delta: f64) -> AnimatedParameter {
    match param {
        AnimatedParameter::Constant { value } => AnimatedParameter::constant(value + delta),
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => AnimatedParameter::Easing {
            start_value: start_value + delta,
            end_value: end_value + delta,
            curve: *curve,
            extrapolation: *extrapolation,
        },
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => AnimatedParameter::Keyframes {
            keyframes: keyframes
                .iter()
                .map(|kf| Keyframe::with_easing(kf.frame, kf.value + delta, kf.easing))
                .collect(),
            extrapolation: *extrapolation,
        },
        AnimatedParameter::Expression {
            expr,
            base_offset,
            scale,
        } => AnimatedParameter::expression_with_tuning(expr.clone(), base_offset + delta, *scale),
    }
}

/// クリップの全期間にわたって指定スロットのアニメーション曲線を等間隔サンプリングする。
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn sample_parameter_curve(
    clip: &Clip,
    fps: FrameRate,
    slot: InspectorParameterSlot,
    samples: usize,
) -> Vec<f64> {
    let Some(param) = slot.extract_param(clip) else {
        return Vec::new();
    };
    let count = samples.max(2);
    let max_local = clip.duration.get().saturating_sub(1);
    let effect_dur = clip.effect_duration_frames.unwrap_or(clip.duration);
    (0..count)
        .map(|ix| {
            let frac = (ix as f64) / ((count - 1) as f64);
            let local_frame = ((max_local as f64) * frac).round() as u64;
            let ctx =
                EvaluationContext::new(local_frame, clip.effect_offset_frames, effect_dur, fps);
            param.evaluate(ctx).unwrap_or(0.0)
        })
        .collect()
}

pub(crate) fn set_animated_command(
    clip: &Clip,
    slot: InspectorParameterSlot,
    param: AnimatedParameter,
) -> EditCommand {
    EditCommand::SetParameter {
        target: ParameterTarget::Clip {
            clip_id: ClipRef::Id(clip.id.clone()),
        },
        path: slot.to_parameter_path(),
        value: ParameterValue::Animated { param },
    }
}

/// 指定スロットを既定値の固定値へ戻すコマンドを構築する。
///
/// # Errors
///
/// 指定クリップに該当スロットが存在しない場合。
pub fn build_reset_slot_command(
    clip: &Clip,
    slot: InspectorParameterSlot,
) -> Result<EditCommand, String> {
    if slot.extract_param(clip).is_none() {
        return Err(format!(
            "クリップに `{}` スロットがありません",
            slot.label()
        ));
    }
    Ok(set_animated_command(
        clip,
        slot,
        AnimatedParameter::constant(slot.default_internal_value()),
    ))
}

/// 指定スロットの現在フレーム評価値を使い、全区間（中間点含む）を同じ固定値にするコマンドを構築する。
///
/// # Errors
///
/// 指定クリップに該当スロットが存在しないか、現在値評価が非有限になった場合。
pub fn build_flatten_slot_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    playhead: FrameIndex,
) -> Result<EditCommand, String> {
    let param = slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", slot.label()))?;
    let local_frame = playhead.get().saturating_sub(clip.timeline_start.get());
    let max_local = clip.duration.get().saturating_sub(1);
    let effect_dur = clip.effect_duration_frames.unwrap_or(clip.duration);
    let ctx = EvaluationContext::new(
        local_frame.min(max_local),
        clip.effect_offset_frames,
        effect_dur,
        doc.fps,
    );
    let evaluated = param
        .evaluate(ctx)
        .unwrap_or_else(|_| slot.default_internal_value());
    if !evaluated.is_finite() {
        return Err("固定化する評価値が有限値ではありません".to_owned());
    }
    Ok(set_animated_command(
        clip,
        slot,
        AnimatedParameter::constant(evaluated),
    ))
}

fn remap_param_between_slots(
    param: &AnimatedParameter,
    from: InspectorParameterSlot,
    to: InspectorParameterSlot,
) -> AnimatedParameter {
    if from == to {
        return param.clone();
    }
    let convert = |internal: f64| -> f64 {
        let ui = from.internal_to_ui(internal);
        let mapped = to.ui_to_internal(ui);
        if mapped.is_finite() {
            mapped
        } else {
            to.default_internal_value()
        }
    };
    match param {
        AnimatedParameter::Constant { value } => AnimatedParameter::constant(convert(*value)),
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => AnimatedParameter::Easing {
            start_value: convert(*start_value),
            end_value: convert(*end_value),
            curve: *curve,
            extrapolation: *extrapolation,
        },
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => AnimatedParameter::Keyframes {
            keyframes: keyframes
                .iter()
                .map(|kf| Keyframe::with_easing(kf.frame, convert(kf.value), kf.easing))
                .collect(),
            extrapolation: *extrapolation,
        },
        AnimatedParameter::Expression {
            expr,
            base_offset,
            scale,
        } => AnimatedParameter::expression_with_tuning(expr.clone(), *base_offset, *scale),
    }
}

/// クリップ内の別スロットへカーブ（キーフレーム／イージング／数式）をコピーするコマンドを構築する。
///
/// # Errors
///
/// コピー元またはコピー先スロットが存在しない、または同一スロットが指定された場合。
pub fn build_copy_slot_curve_command(
    clip: &Clip,
    from_slot: InspectorParameterSlot,
    to_slot: InspectorParameterSlot,
) -> Result<EditCommand, String> {
    if from_slot == to_slot {
        return Err("同一スロットへはコピーできません".to_owned());
    }
    let source = from_slot
        .extract_param(clip)
        .ok_or_else(|| format!("クリップに `{}` スロットがありません", from_slot.label()))?;
    if to_slot.extract_param(clip).is_none() {
        return Err(format!(
            "クリップに `{}` スロットがありません",
            to_slot.label()
        ));
    }
    let mapped = remap_param_between_slots(source, from_slot, to_slot);
    Ok(set_animated_command(clip, to_slot, mapped))
}

/// ドラッグ開始前に元の状態から `Keyframes` を含む整合済みドキュメントへ復元するための補助。
pub fn align_clip_after_transient_edit(clip: &mut Clip, fps: FrameRate) {
    sync_and_align_clip_keyframes(clip, fps);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::command::{NewClipKind, NewClipSpec};
    use crate::timeline::dispatcher::CommandDispatcher;

    fn setup_subtitle_doc(
        duration: u64,
    ) -> Result<(ProjectDocument, ClipId), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Test");
        let mut dispatcher = CommandDispatcher::new();
        let layer_id = crate::core::ids::LayerId::new("layer_1")?;
        let clip_id = ClipId::new("clip_1")?;
        let batch = EditCommand::Batch {
            commands: vec![
                EditCommand::AddLayer {
                    temp_id: None,
                    id: Some(layer_id.clone()),
                    name: "L1".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: None,
                    id: Some(clip_id.clone()),
                    layer_id: crate::core::ids::LayerRef::Id(layer_id),
                    clip: Box::new(NewClipSpec {
                        name: "Sub".to_owned(),
                        timeline_start: FrameIndex::ZERO,
                        duration: FrameCount::new(duration)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        midpoints: Vec::new(),
                        kind: NewClipKind::Subtitle(SubtitleProperties::new("Hello")),
                    }),
                },
            ],
        };
        dispatcher.dispatch(&mut doc, &batch)?;
        Ok((doc, clip_id))
    }

    #[test]
    fn ui_unit_roundtrips_and_bounds_match_spec() {
        for (slot, internal, expected_ui) in [
            (InspectorParameterSlot::ScaleX, 1.5, 150.0),
            (InspectorParameterSlot::ScaleY, 0.25, 25.0),
            (InspectorParameterSlot::Opacity, 1.0, 0.0),
            (InspectorParameterSlot::Opacity, 0.25, 75.0),
            (InspectorParameterSlot::AudioPan, -0.5, -50.0),
            (InspectorParameterSlot::PositionX, 320.0, 320.0),
        ] {
            let ui = slot.internal_to_ui(internal);
            assert!((ui - expected_ui).abs() < 1e-6);
            assert!((slot.ui_to_internal(ui) - internal).abs() < 1e-6);
        }
        assert_eq!(InspectorParameterSlot::Saturation.bounds(), (0.0, 4.0, 0.1));
        assert_eq!(InspectorParameterSlot::Gamma.bounds(), (0.05, 5.0, 0.1));
    }

    #[test]
    fn constant_right_edit_promotes_and_cut_easing_preserves_curve()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, cid) = setup_subtitle_doc(120)?;
        let mut dispatcher = CommandDispatcher::new();

        // Constant -> Right box edit promotes to Linear Easing
        let cmd = {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            build_set_segment_endpoint_command(
                &doc,
                clip,
                InspectorParameterSlot::PositionX,
                0,
                true,
                120.0,
            )?
        };
        dispatcher.dispatch(&mut doc, &cmd)?;

        let right_id = ClipId::new("clip_right")?;
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::SplitClip {
                clip_id: ClipRef::Id(cid),
                split_at_timeline_frame: FrameIndex::new(40),
                right_temp_id: None,
                right_id: Some(right_id.clone()),
            },
        )?;

        // Right cut clip should indicate [分割継続] and evaluate X at grid bounds
        let right_view = {
            let (_, rclip) = doc.find_clip(&right_id).ok_or("right clip")?;
            read_trackbar_segment_view(&doc, rclip, InspectorParameterSlot::PositionX, 0)
                .ok_or("view")?
        };
        assert!(right_view.is_cut_easing);
        assert!(right_view.start_value > 39.0);
        assert!((right_view.end_value - 120.0).abs() < 1e-2);

        // Editing start value on cut Easing promotes to aligned Keyframes preserving end_value
        let edit_cut_cmd = {
            let (_, rclip) = doc.find_clip(&right_id).ok_or("right clip")?;
            build_set_segment_endpoint_command(
                &doc,
                rclip,
                InspectorParameterSlot::PositionX,
                0,
                false,
                10.0,
            )?
        };
        dispatcher.dispatch(&mut doc, &edit_cut_cmd)?;
        let (_, rclip_after) = doc.find_clip(&right_id).ok_or("right clip")?;
        let after_view =
            read_trackbar_segment_view(&doc, rclip_after, InspectorParameterSlot::PositionX, 0)
                .ok_or("view2")?;
        assert_eq!(after_view.mode, InspectorMode::Keyframes);
        assert!((after_view.start_value - 10.0).abs() < 1e-6);
        assert!((after_view.end_value - 120.0).abs() < 1e-2);
        Ok(())
    }

    #[test]
    fn scale_aspect_lock_and_all_presets_across_segments() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut doc, cid) = setup_subtitle_doc(100)?;
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::AddMidpoint {
                clip_id: ClipRef::Id(cid.clone()),
                effect_frame: 50,
            },
        )?;

        let edit_lock = SegmentEndpointEdit {
            slot: InspectorParameterSlot::ScaleX,
            segment_index: 1,
            is_end: true,
            new_value: 2.0,
            scale_aspect_locked: true,
            aspect_ratio_override: None,
        };
        let cmd = {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            build_set_segment_endpoint_with_lock(&doc, clip, &edit_lock)?
        };
        dispatcher.dispatch(&mut doc, &cmd)?;

        for &preset in MovementModePreset::all() {
            let mcmd = {
                let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
                build_apply_movement_mode_with_options(
                    &doc,
                    clip,
                    &MovementModeEdit {
                        slot: InspectorParameterSlot::ScaleX,
                        preset,
                        playhead: FrameIndex::new(25),
                        bezier_curve: EasingCurve::CubicBezier {
                            x1: 0.25,
                            y1: 0.1,
                            x2: 0.25,
                            y2: 1.0,
                        },
                        scale_aspect_locked: true,
                    },
                )?
            };
            dispatcher.dispatch(&mut doc, &mcmd)?;
            let (_, clip_now) = doc.find_clip(&cid).ok_or("clip")?;
            let vx = read_trackbar_segment_view(&doc, clip_now, InspectorParameterSlot::ScaleX, 1)
                .ok_or("vx")?;
            let vy = read_trackbar_segment_view(&doc, clip_now, InspectorParameterSlot::ScaleY, 1)
                .ok_or("vy")?;
            assert_eq!(vx.active_preset, vy.active_preset);
        }
        Ok(())
    }

    #[test]
    fn color_badge_and_batch_xy_offset() -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, cid) = setup_subtitle_doc(60)?;
        let mut dispatcher = CommandDispatcher::new();
        {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            assert!(!clip_has_non_default_color_adjustments(clip));
        }
        let sat_cmd = {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            build_set_segment_endpoint_command(
                &doc,
                clip,
                InspectorParameterSlot::Saturation,
                0,
                false,
                2.5,
            )?
        };
        dispatcher.dispatch(&mut doc, &sat_cmd)?;
        {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            assert!(clip_has_non_default_color_adjustments(clip));
        }
        let reset_cmd = {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            build_reset_color_adjustments_command(clip).ok_or("reset")?
        };
        dispatcher.dispatch(&mut doc, &reset_cmd)?;
        {
            let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
            assert!(!clip_has_non_default_color_adjustments(clip));
        }

        let offset_cmd =
            build_batch_relative_xy_offset_command(&doc, std::slice::from_ref(&cid), 40.0, -20.0)?;
        dispatcher.dispatch(&mut doc, &offset_cmd)?;
        let (_, clip) = doc.find_clip(&cid).ok_or("clip")?;
        let vx = read_trackbar_segment_view(&doc, clip, InspectorParameterSlot::PositionX, 0)
            .ok_or("vx")?;
        let vy = read_trackbar_segment_view(&doc, clip, InspectorParameterSlot::PositionY, 0)
            .ok_or("vy")?;
        assert!((vx.start_value - 40.0).abs() < 1e-6);
        assert!((vy.start_value + 20.0).abs() < 1e-6);
        Ok(())
    }
}
