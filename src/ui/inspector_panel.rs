//! インスペクタパネル: 4 モード切替・キーフレーム編集・`f(t)` 数式エディタ・カーブ可視化。

use std::fmt::Write as _;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{Context, Entity, IntoElement, ParentElement as _, Styled as _, div, px, rgba};

use crate::core::animation::{
    AnimatedParameter, EasingCurve, EvaluationContext, ExtrapolationMode, Keyframe,
};
use crate::core::expression::CompiledExpression;
use crate::core::ids::ClipId;
use crate::core::ids::ClipRef;
use crate::core::time::{FrameIndex, FrameRate};
use crate::media::visual_properties::BlendMode;
use crate::timeline::command::{EditCommand, ParameterPath, ParameterTarget, ParameterValue};
use crate::timeline::document::{Clip, ClipKind, ProjectDocument};
use crate::ui::app_controller::AppController;
use crate::ui::window::SeaEditorWindow;

/// インスペクタで編集対象にするパラメータモード（設計 §5 準拠の 4 モード）。
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

/// インスペクタで操作できるアニメーション可能パラメータスロット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum InspectorParameterSlot {
    /// X 座標 (px)。
    PositionX,
    /// Y 座標 (px)。
    PositionY,
    /// X 拡大率。
    ScaleX,
    /// Y 拡大率。
    ScaleY,
    /// 回転角度 (度)。
    RotationDeg,
    /// アンカー X (px)。
    AnchorX,
    /// アンカー Y (px)。
    AnchorY,
    /// 不透明度 (`0.0..=1.0`)。
    #[default]
    Opacity,
    /// 明度補正 (`-1.0..=1.0`)。
    Brightness,
    /// コントラスト倍率 (`0.0..=4.0`)。
    Contrast,
    /// 彩度倍率 (`0.0..=3.0`)。
    Saturation,
    /// 色相回転 (度)。
    HueDeg,
    /// ガンマ (`0.1..=4.0`)。
    Gamma,
    /// 字幕フォントサイズ (px)。
    SubtitleFontSize,
    /// 音量倍率。
    AudioVolume,
    /// パン (`-1.0..=1.0`)。
    AudioPan,
}

impl InspectorParameterSlot {
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

    /// 推奨値域 `(min, max, step)` を返す。
    #[must_use]
    pub const fn bounds(self) -> (f64, f64, f64) {
        match self {
            Self::PositionX | Self::PositionY | Self::AnchorX | Self::AnchorY => {
                (-1920.0, 1920.0, 10.0)
            }
            Self::ScaleX | Self::ScaleY => (0.05, 5.0, 0.1),
            Self::RotationDeg => (-1080.0, 1080.0, 15.0),
            Self::Opacity => (0.0, 1.0, 0.05),
            Self::Brightness => (-1.0, 1.0, 0.05),
            Self::Contrast => (0.0, 4.0, 0.1),
            Self::Saturation => (0.0, 3.0, 0.1),
            Self::HueDeg => (-180.0, 180.0, 15.0),
            Self::Gamma => (0.1, 4.0, 0.1),
            Self::SubtitleFontSize => (8.0, 200.0, 4.0),
            Self::AudioVolume => (-60.0, 24.0, 1.5),
            Self::AudioPan => (-1.0, 1.0, 0.1),
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
    last_synced_clip_id: Option<ClipId>,
    last_synced_slot: Option<InspectorParameterSlot>,
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
            last_synced_clip_id: None,
            last_synced_slot: None,
        }
    }
}

impl InspectorState {
    /// 選択クリップとアクティブスロットに合わせて UI 下書き値を同期する（更新時 `true`）。
    pub fn sync_from_clip(
        &mut self,
        doc: &ProjectDocument,
        clip: &Clip,
        playhead: FrameIndex,
    ) -> bool {
        let available = InspectorParameterSlot::available_for(clip);
        if !available.contains(&self.active_slot)
            && let Some(&first) = available.first()
        {
            self.active_slot = first;
        }

        let clip_changed = self.last_synced_clip_id.as_ref() != Some(&clip.id);
        let slot_changed = self.last_synced_slot != Some(self.active_slot);
        if !clip_changed && !slot_changed {
            return false;
        }

        self.last_synced_clip_id = Some(clip.id.clone());
        self.last_synced_slot = Some(self.active_slot);
        if let ClipKind::Subtitle(sub) = &clip.kind {
            self.subtitle_text_draft.clone_from(&sub.text);
        } else {
            self.subtitle_text_draft.clear();
        }

        let ctx = evaluation_context_for_clip(doc, clip, playhead);
        if let Some(param) = self.active_slot.extract_param(clip) {
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

fn set_animated_command(
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

/// インスペクタ描画に必要な UI ウィジェット群への参照。
#[derive(Clone, Copy)]
pub struct InspectorWidgets<'a> {
    /// 不透明度用連続スライダ。
    pub opacity_slider: &'a Entity<SliderState>,
    /// 数式 `f(t)` テキスト入力欄。
    pub expr_input: &'a Entity<InputState>,
    /// 字幕本文入力欄。
    pub subtitle_text_input: &'a Entity<InputState>,
}

/// クリップ選択中の詳細インスペクタ UI を構築する。
pub fn render_clip_inspector(
    controller: &AppController,
    widgets: InspectorWidgets<'_>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let Some((_, clip)) = controller.selected_clip() else {
        return v_flex()
            .p_3()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                "クリップを選択すると 4 モード・キーフレーム・数式 f(t) インスペクタを表示します。",
            )
            .into_any_element();
    };

    let clip_id = clip.id.clone();
    let playhead = controller.clock.current_frame();
    let active_slot = controller.inspector.active_slot;
    let available = InspectorParameterSlot::available_for(clip);
    let eff_frame = effect_frame_at_playhead(clip, playhead);
    let ctx = evaluation_context_for_clip(&controller.document, clip, playhead);
    let param = active_slot.extract_param(clip).cloned().unwrap_or_default();
    let mode = InspectorMode::from_param(&param);
    let current_val = param.evaluate(ctx).unwrap_or(0.0);
    let curve_samples = sample_parameter_curve(clip, controller.document.fps, active_slot, 24);

    let mut root = v_flex()
        .p_3()
        .gap_2()
        .text_xs()
        .text_color(theme.foreground)
        .child(render_clip_header(clip, playhead, eff_frame, &theme))
        .child(render_slot_selector(&available, active_slot, cx));

    if matches!(clip.kind, ClipKind::Subtitle(_)) {
        root = root.child(render_subtitle_editor(
            clip_id.clone(),
            widgets.subtitle_text_input,
            cx,
        ));
    }

    if let Some(blend) = clip_blend_mode(clip) {
        root = root.child(render_blend_mode_row(&clip_id, blend, cx));
    }

    root = root
        .child(render_mode_selector(mode, active_slot, &theme, cx))
        .child(render_curve_preview(
            &curve_samples,
            active_slot,
            current_val,
            &theme,
        ))
        .child(render_active_mode_body(
            clip,
            active_slot,
            &param,
            current_val,
            eff_frame,
            &widgets,
            &controller.inspector,
            cx,
        ));

    root.into_any_element()
}

fn render_clip_header(
    clip: &Clip,
    playhead: FrameIndex,
    eff_frame: i64,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let start = clip.timeline_start.get();
    let end = start + clip.duration.get();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_color(theme.foreground)
                .child(format!("クリップ: {}", clip.name)),
        )
        .child(div().text_color(theme.muted_foreground).child(format!(
            "範囲: {start}..{end}f / 再生: {}f / 効果枠: {eff_frame}f",
            playhead.get()
        )))
}

fn render_slot_selector(
    slots: &[InspectorParameterSlot],
    active: InspectorParameterSlot,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(slots.iter().enumerate().map(|(ix, &slot)| {
            let mut btn = Button::new(("slot-btn", ix)).small().label(slot.label());
            btn = if slot == active {
                btn.primary()
            } else {
                btn.ghost()
            };
            btn.on_click(cx.listener(move |this, _, window, cx| {
                this.controller.inspector.active_slot = slot;
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
        }))
}

fn render_subtitle_editor(
    clip_id: ClipId,
    subtitle_input: &Entity<InputState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(div().flex_1().child(Input::new(subtitle_input)))
        .child(
            Button::new("apply-subtitle-text")
                .small()
                .primary()
                .label("字幕更新")
                .on_click(cx.listener(move |this, _, _, cx| {
                    let text = this.subtitle_text_input.read(cx).value().to_string();
                    let cmd = EditCommand::SetParameter {
                        target: ParameterTarget::Clip {
                            clip_id: ClipRef::Id(clip_id.clone()),
                        },
                        path: ParameterPath::SubtitleText,
                        value: ParameterValue::Text { value: text },
                    };
                    if let Err(err) = this.controller.dispatch_command(&cmd) {
                        this.controller.status_message = format!("字幕更新失敗: {err}");
                    }
                    cx.notify();
                })),
        )
}

fn clip_blend_mode(clip: &Clip) -> Option<BlendMode> {
    match &clip.kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Some(visual.blend_mode),
        ClipKind::Subtitle(sub) => Some(sub.visual.blend_mode),
        ClipKind::Audio { .. } => None,
    }
}

fn render_blend_mode_row(
    clip_id: &ClipId,
    current: BlendMode,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    const MODES: [(&str, BlendMode); 5] = [
        ("通常", BlendMode::Normal),
        ("加算", BlendMode::Add),
        ("乗算", BlendMode::Multiply),
        ("スクリーン", BlendMode::Screen),
        ("オーバーレイ", BlendMode::Overlay),
    ];
    h_flex()
        .gap_1()
        .flex_wrap()
        .items_center()
        .child(div().child("合成:"))
        .children(MODES.iter().enumerate().map(|(ix, &(label, bm))| {
            let cid = clip_id.clone();
            let mut btn = Button::new(("blend-btn", ix)).small().label(label);
            btn = if bm == current {
                btn.primary()
            } else {
                btn.ghost()
            };
            btn.on_click(cx.listener(move |this, _, _, cx| {
                let cmd = EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Id(cid.clone()),
                    },
                    path: ParameterPath::BlendMode,
                    value: ParameterValue::BlendMode { value: bm },
                };
                let _ = this.controller.dispatch_command(&cmd);
                cx.notify();
            }))
        }))
}

fn render_mode_selector(
    current_mode: InspectorMode,
    slot: InspectorParameterSlot,
    theme: &gpui_kit::component::Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(format!("補間モード ({})", slot.label())),
        )
        .child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(InspectorMode::all().iter().enumerate().map(|(ix, &mode)| {
                    let mut btn = Button::new(("mode-btn", ix)).small().label(mode.label());
                    btn = if mode == current_mode {
                        btn.primary()
                    } else {
                        btn.ghost()
                    };
                    btn.on_click(cx.listener(move |this, _, window, cx| {
                        if let Some((_, clip)) = this.controller.selected_clip() {
                            let clip_owned = clip.clone();
                            let head = this.controller.clock.current_frame();
                            if let Ok(cmd) = build_switch_mode_command(
                                &this.controller.document,
                                &clip_owned,
                                slot,
                                mode,
                                head,
                            ) {
                                let _ = this.controller.dispatch_command(&cmd);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
                            }
                        }
                        cx.notify();
                    }))
                })),
        )
}

fn render_curve_preview(
    samples: &[f64],
    slot: InspectorParameterSlot,
    current_val: f64,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let (bound_min, bound_max, _) = slot.bounds();
    let sample_min = samples.iter().copied().fold(bound_min, f64::min);
    let sample_max = samples.iter().copied().fold(bound_max, f64::max);
    let span = (sample_max - sample_min).max(1e-6);

    v_flex()
        .p_2()
        .gap_1()
        .border_1()
        .border_color(theme.border)
        .rounded_sm()
        .child(
            div()
                .flex()
                .justify_between()
                .text_color(theme.muted_foreground)
                .child(format!("曲線 ({}): {:.3}", slot.label(), current_val)),
        )
        .child(
            h_flex()
                .h(px(36.))
                .w_full()
                .items_end()
                .gap(px(1.))
                .children(samples.iter().map(|&v| {
                    let norm = ((v - sample_min) / span).clamp(0.05, 1.0);
                    #[allow(clippy::cast_possible_truncation)]
                    let h_px = (norm * 32.0) as f32;
                    div().flex_1().h(px(h_px)).bg(rgba(0x38bd_f8cc))
                })),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_active_mode_body(
    clip: &Clip,
    slot: InspectorParameterSlot,
    param: &AnimatedParameter,
    current_val: f64,
    eff_frame: i64,
    widgets: &InspectorWidgets<'_>,
    state: &InspectorState,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::AnyElement {
    let theme = cx.theme().clone();
    match param {
        AnimatedParameter::Constant { value } => {
            render_constant_controls(clip, slot, *value, widgets.opacity_slider, cx)
                .into_any_element()
        }
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => render_easing_controls(
            &clip.id,
            slot,
            EasingSpec {
                start_value: *start_value,
                end_value: *end_value,
                curve: *curve,
                extrapolation: *extrapolation,
            },
            cx,
        )
        .into_any_element(),
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => render_keyframe_controls(
            clip,
            slot,
            keyframes,
            *extrapolation,
            eff_frame,
            current_val,
            &theme,
            cx,
        )
        .into_any_element(),
        AnimatedParameter::Expression { .. } => {
            render_expression_controls(clip.id.clone(), slot, widgets.expr_input, state, &theme, cx)
                .into_any_element()
        }
    }
}

#[derive(Clone, Copy)]
struct EasingSpec {
    start_value: f64,
    end_value: f64,
    curve: EasingCurve,
    extrapolation: ExtrapolationMode,
}

fn render_constant_controls(
    clip: &Clip,
    slot: InspectorParameterSlot,
    value: f64,
    opacity_slider: &Entity<SliderState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let (_, _, step) = slot.bounds();
    let mut box_el = v_flex()
        .gap_2()
        .child(div().child(format!("定数値: {value:.3}")));

    if slot == InspectorParameterSlot::Opacity {
        box_el = box_el.child(Slider::new(opacity_slider));
    }

    box_el.child(render_numeric_nudge_row(
        "const-nudge",
        value,
        step,
        &clip.id,
        AnimatedParameter::constant,
        cx,
    ))
}

fn render_easing_controls(
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    spec: EasingSpec,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let (_, _, step) = slot.bounds();
    let EasingSpec {
        start_value,
        end_value,
        curve,
        extrapolation,
    } = spec;
    v_flex()
        .gap_2()
        .child(div().child(format!("開始値: {start_value:.3}")))
        .child(render_numeric_nudge_row(
            "ease-start",
            start_value,
            step,
            clip_id,
            move |next| AnimatedParameter::Easing {
                start_value: next,
                end_value,
                curve,
                extrapolation,
            },
            cx,
        ))
        .child(div().child(format!("終了値: {end_value:.3}")))
        .child(render_numeric_nudge_row(
            "ease-end",
            end_value,
            step,
            clip_id,
            move |next| AnimatedParameter::Easing {
                start_value,
                end_value: next,
                curve,
                extrapolation,
            },
            cx,
        ))
        .child(render_curve_preset_buttons(clip_id, slot, spec, cx))
}

fn render_curve_preset_buttons(
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    spec: EasingSpec,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let presets = [
        ("Linear", EasingCurve::Linear),
        ("EaseIn", EasingCurve::EaseIn),
        ("EaseOut", EasingCurve::EaseOut),
        ("EaseInOut", EasingCurve::EaseInOut),
    ];
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(
            presets
                .into_iter()
                .enumerate()
                .map(|(ix, (label, preset))| {
                    let cid = clip_id.clone();
                    let mut btn = Button::new(("easing-curve-btn", ix)).small().label(label);
                    btn = if preset == spec.curve {
                        btn.primary()
                    } else {
                        btn.ghost()
                    };
                    btn.on_click(cx.listener(move |this, _, _, cx| {
                        let cmd = EditCommand::SetParameter {
                            target: ParameterTarget::Clip {
                                clip_id: ClipRef::Id(cid.clone()),
                            },
                            path: slot.to_parameter_path(),
                            value: ParameterValue::Animated {
                                param: AnimatedParameter::Easing {
                                    start_value: spec.start_value,
                                    end_value: spec.end_value,
                                    curve: preset,
                                    extrapolation: spec.extrapolation,
                                },
                            },
                        };
                        let _ = this.controller.dispatch_command(&cmd);
                        cx.notify();
                    }))
                }),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_keyframe_controls(
    clip: &Clip,
    slot: InspectorParameterSlot,
    keyframes: &[Keyframe],
    extrapolation: ExtrapolationMode,
    eff_frame: i64,
    current_val: f64,
    theme: &gpui_kit::component::Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let (_, _, step) = slot.bounds();
    v_flex()
        .gap_2()
        .child(render_keyframe_action_bar(
            slot,
            eff_frame,
            current_val,
            step,
            cx,
        ))
        .child(render_extrapolation_row(
            &clip.id,
            slot,
            keyframes,
            extrapolation,
            cx,
        ))
        .child(render_keyframe_rows(
            clip, slot, keyframes, eff_frame, theme, cx,
        ))
}

fn render_keyframe_action_bar(
    slot: InspectorParameterSlot,
    eff_frame: i64,
    current_val: f64,
    step: f64,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let add_label = format!("現在枠 {eff_frame}f に KF 設定 ({current_val:.2})");
    h_flex()
        .gap_1()
        .child(keyframe_upsert_button(
            "kf-add-current",
            add_label,
            slot,
            0.0,
            true,
            cx,
        ))
        .child(keyframe_upsert_button(
            "kf-step-up",
            format!("+{step:.2} @ {eff_frame}f"),
            slot,
            step,
            false,
            cx,
        ))
}

fn keyframe_upsert_button(
    id: &'static str,
    label: String,
    slot: InspectorParameterSlot,
    delta: f64,
    primary: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut btn = Button::new(id).small().label(label);
    btn = if primary { btn.primary() } else { btn.ghost() };
    btn.on_click(cx.listener(move |this, _, _, cx| {
        if let Some((_, c)) = this.controller.selected_clip() {
            let c_owned = c.clone();
            let head = this.controller.clock.current_frame();
            let eff = effect_frame_at_playhead(&c_owned, head);
            let ctx_eval = evaluation_context_for_clip(&this.controller.document, &c_owned, head);
            let val = slot
                .extract_param(&c_owned)
                .and_then(|p| p.evaluate(ctx_eval).ok())
                .unwrap_or(0.0)
                + delta;
            if let Ok(cmd) =
                build_upsert_keyframe_command(&c_owned, slot, eff, val, EasingCurve::Linear)
            {
                let _ = this.controller.dispatch_command(&cmd);
            }
        }
        cx.notify();
    }))
}

fn render_keyframe_rows(
    clip: &Clip,
    slot: InspectorParameterSlot,
    keyframes: &[Keyframe],
    eff_frame: i64,
    _theme: &gpui_kit::component::Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let clip_id = clip.id.clone();
    let mut list = v_flex().gap_1();

    for (ix, kf) in keyframes.iter().take(12).enumerate() {
        let kf_frame = kf.frame;
        let clip_timeline_start = clip.timeline_start.get();
        let clip_eff_off = clip.effect_offset_frames;
        let is_current = kf_frame == eff_frame;
        let cid_del = clip_id.clone();
        let mut row = h_flex()
            .gap_1()
            .items_center()
            .justify_between()
            .p_1()
            .border_1()
            .border_color(if is_current {
                rgba(0x38bd_f8ff)
            } else {
                rgba(0x4755_69ff)
            })
            .rounded_sm()
            .child(
                Button::new(("kf-seek", ix))
                    .ghost()
                    .small()
                    .label(format!("@{kf_frame}f = {:.2}", kf.value))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
                        let target_tl =
                            (clip_timeline_start as i64 + (kf_frame - clip_eff_off)).max(0) as u64;
                        this.controller.seek_to_frame(FrameIndex::new(target_tl));
                        this.sync_sliders_now(window, cx);
                        cx.notify();
                    })),
            );
        if keyframes.len() > 1 {
            row = row.child(
                Button::new(("kf-del", ix))
                    .ghost()
                    .small()
                    .label("削除")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some((_, c)) = this.controller.selected_clip()
                            && c.id == cid_del
                        {
                            let c_owned = c.clone();
                            if let Ok(cmd) = build_remove_keyframe_command(&c_owned, slot, kf_frame)
                            {
                                let _ = this.controller.dispatch_command(&cmd);
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        list = list.child(row);
    }
    list
}

fn render_extrapolation_row(
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    keyframes: &[Keyframe],
    current: ExtrapolationMode,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    const EXTS: [(&str, ExtrapolationMode); 4] = [
        ("Clamp", ExtrapolationMode::Clamp),
        ("Loop", ExtrapolationMode::Loop),
        ("PingPong", ExtrapolationMode::PingPong),
        ("Extrap", ExtrapolationMode::Extrapolate),
    ];
    let kfs = keyframes.to_vec();
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(EXTS.iter().enumerate().map(|(ix, &(label, mode))| {
            let cid = clip_id.clone();
            let kf_vec = kfs.clone();
            let mut btn = Button::new(("kf-ext-btn", ix)).small().label(label);
            btn = if mode == current {
                btn.primary()
            } else {
                btn.ghost()
            };
            btn.on_click(cx.listener(move |this, _, _, cx| {
                let cmd = EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Id(cid.clone()),
                    },
                    path: slot.to_parameter_path(),
                    value: ParameterValue::Animated {
                        param: AnimatedParameter::Keyframes {
                            keyframes: kf_vec.clone(),
                            extrapolation: mode,
                        },
                    },
                };
                let _ = this.controller.dispatch_command(&cmd);
                cx.notify();
            }))
        }))
}

fn render_expression_controls(
    clip_id: ClipId,
    slot: InspectorParameterSlot,
    expr_input: &Entity<InputState>,
    state: &InspectorState,
    theme: &gpui_kit::component::Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut container = v_flex()
        .gap_2()
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child("使用可変数: t (秒), p (0..1進行率), d (尺秒), pi, e"),
        )
        .child(
            h_flex()
                .gap_1()
                .child(div().flex_1().child(Input::new(expr_input)))
                .child(
                    Button::new("apply-expr-btn")
                        .primary()
                        .small()
                        .label("数式適用")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let raw = this.expr_input.read(cx).value().to_string();
                            this.controller.inspector.expr_draft.clone_from(&raw);
                            let base_offset = this.controller.inspector.expr_base_offset;
                            let scale = this.controller.inspector.expr_scale;
                            if let Some((_, c)) = this.controller.selected_clip()
                                && c.id == clip_id
                            {
                                let c_owned = c.clone();
                                match build_expression_command(
                                    &c_owned,
                                    slot,
                                    &raw,
                                    base_offset,
                                    scale,
                                ) {
                                    Ok(cmd) => {
                                        this.controller.inspector.expr_error = None;
                                        let _ = this.controller.dispatch_command(&cmd);
                                    }
                                    Err(err) => {
                                        this.controller.inspector.expr_error = Some(err);
                                    }
                                }
                            }
                            cx.notify();
                        })),
                ),
        );

    if let Some(err) = &state.expr_error {
        container = container.child(
            div()
                .p_1()
                .rounded_sm()
                .bg(rgba(0x991b_1b44))
                .text_color(rgba(0xfeca_caff))
                .child(format!("数式エラー: {err}")),
        );
    }

    container = container
        .child(div().child(format!(
            "微調整: base_offset = {:.2}, scale = {:.2}",
            state.expr_base_offset, state.expr_scale
        )))
        .child(render_expr_tuning_buttons(slot, state, cx));

    container
}

fn render_expr_tuning_buttons(
    slot: InspectorParameterSlot,
    state: &InspectorState,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let (_, _, step) = slot.bounds();
    let offset_steps = [("off-", -step), ("off+", step)];
    let scale_steps = [("scl-", -0.25), ("scl+", 0.25)];
    let expr_now = state.expr_draft.clone();

    let mut row = h_flex().gap_1().flex_wrap();
    for (ix, (label, delta)) in offset_steps.into_iter().enumerate() {
        let e = expr_now.clone();
        row = row.child(
            Button::new(("expr-off-btn", ix))
                .ghost()
                .small()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.inspector.expr_base_offset += delta;
                    if let Some((_, c)) = this.controller.selected_clip() {
                        let c_owned = c.clone();
                        let off = this.controller.inspector.expr_base_offset;
                        let scl = this.controller.inspector.expr_scale;
                        let raw = this.expr_input.read(cx).value().to_string();
                        let src = if raw.is_empty() {
                            e.as_str()
                        } else {
                            raw.as_str()
                        };
                        if let Ok(cmd) = build_expression_command(&c_owned, slot, src, off, scl) {
                            let _ = this.controller.dispatch_command(&cmd);
                        }
                    }
                    cx.notify();
                })),
        );
    }
    for (ix, (label, delta)) in scale_steps.into_iter().enumerate() {
        let e = expr_now.clone();
        row = row.child(
            Button::new(("expr-scl-btn", ix))
                .ghost()
                .small()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.inspector.expr_scale += delta;
                    if let Some((_, c)) = this.controller.selected_clip() {
                        let c_owned = c.clone();
                        let off = this.controller.inspector.expr_base_offset;
                        let scl = this.controller.inspector.expr_scale;
                        let raw = this.expr_input.read(cx).value().to_string();
                        let src = if raw.is_empty() {
                            e.as_str()
                        } else {
                            raw.as_str()
                        };
                        if let Ok(cmd) = build_expression_command(&c_owned, slot, src, off, scl) {
                            let _ = this.controller.dispatch_command(&cmd);
                        }
                    }
                    cx.notify();
                })),
        );
    }
    row
}

fn render_numeric_nudge_row<F>(
    id_prefix: &'static str,
    current_value: f64,
    step: f64,
    clip_id: &ClipId,
    make_param: F,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement
where
    F: Fn(f64) -> AnimatedParameter + Clone + 'static,
{
    let deltas = [-step * 5.0, -step, step, step * 5.0];
    h_flex()
        .gap_1()
        .flex_wrap()
        .children(deltas.into_iter().enumerate().map(|(ix, delta)| {
            let mut label = String::new();
            let _ = if delta >= 0.0 {
                write!(&mut label, "+{delta:.2}")
            } else {
                write!(&mut label, "{delta:.2}")
            };
            let cid = clip_id.clone();
            let make = make_param.clone();
            Button::new((id_prefix, ix))
                .ghost()
                .small()
                .label(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let slot = this.controller.inspector.active_slot;
                    let (b_min, b_max, _) = slot.bounds();
                    let next = (current_value + delta).clamp(b_min, b_max);
                    let cmd = EditCommand::SetParameter {
                        target: ParameterTarget::Clip {
                            clip_id: ClipRef::Id(cid.clone()),
                        },
                        path: slot.to_parameter_path(),
                        value: ParameterValue::Animated { param: make(next) },
                    };
                    let _ = this.controller.dispatch_command(&cmd);
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                }))
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::LayerId;
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::{SubtitleProperties, VisualProperties};
    use crate::timeline::dispatcher::CommandDispatcher;
    use crate::timeline::document::Layer;

    fn sample_doc_with_visual_clip() -> Result<(ProjectDocument, ClipId), Box<dyn std::error::Error>>
    {
        let mut doc = ProjectDocument::new("inspector_test");
        let layer_id = LayerId::new("layer1")?;
        let clip_id = ClipId::new("clip1")?;
        let mut layer = Layer::new(layer_id, "V1");
        let mut sub = SubtitleProperties::new("Hello");
        sub.visual = VisualProperties::default();
        layer.clips.push(Clip {
            id: clip_id.clone(),
            name: "Title".to_owned(),
            timeline_start: FrameIndex::new(30),
            duration: FrameCount::new(60)?,
            effect_offset_frames: 15,
            effect_duration_frames: Some(FrameCount::new(120)?),
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(sub),
        });
        doc.layers.push(layer);
        Ok((doc, clip_id))
    }

    #[test]
    fn mode_switching_keyframe_ops_and_expression_validation_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, clip_id) = sample_doc_with_visual_clip()?;
        let mut dispatcher = CommandDispatcher::new();
        let playhead = FrameIndex::new(50);

        let clip_snap = doc.find_clip(&clip_id).ok_or("clip missing")?.1.clone();
        assert_eq!(effect_frame_at_playhead(&clip_snap, playhead), 35);

        // Constant(1.0) -> Keyframes へ切替
        let to_kf = build_switch_mode_command(
            &doc,
            &clip_snap,
            InspectorParameterSlot::PositionX,
            InspectorMode::Keyframes,
            playhead,
        )?;
        dispatcher.dispatch(&mut doc, &to_kf)?;

        // キーフレーム追加・削除テスト
        let clip_snap = doc.find_clip(&clip_id).ok_or("clip missing")?.1.clone();
        let upsert = build_upsert_keyframe_command(
            &clip_snap,
            InspectorParameterSlot::PositionX,
            35,
            240.0,
            EasingCurve::EaseOut,
        )?;
        dispatcher.dispatch(&mut doc, &upsert)?;

        let clip_snap = doc.find_clip(&clip_id).ok_or("clip missing")?.1.clone();
        let ctx = evaluation_context_for_clip(&doc, &clip_snap, playhead);
        let pos_x = InspectorParameterSlot::PositionX
            .extract_param(&clip_snap)
            .ok_or("slot missing")?;
        assert!((pos_x.evaluate(ctx)? - 240.0).abs() < 1e-6);

        let remove_cmd =
            build_remove_keyframe_command(&clip_snap, InspectorParameterSlot::PositionX, 35)?;
        dispatcher.dispatch(&mut doc, &remove_cmd)?;

        // 数式バリデーションと微調整オフセット/倍率
        assert!(validate_expression_and_tuning("unknown_id + 1", 0.0, 1.0).is_err());
        let expr_cmd = build_expression_command(
            &clip_snap,
            InspectorParameterSlot::RotationDeg,
            "p * 100",
            10.0,
            2.0,
        )?;
        dispatcher.dispatch(&mut doc, &expr_cmd)?;

        let clip_snap = doc.find_clip(&clip_id).ok_or("clip missing")?.1.clone();
        let samples =
            sample_parameter_curve(&clip_snap, doc.fps, InspectorParameterSlot::RotationDeg, 5);
        assert_eq!(samples.len(), 5);
        assert!(samples[4] > samples[0]);
        Ok(())
    }
}
