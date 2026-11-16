//! インスペクタ `Collapsible` セクション・未選択/複数選択・字幕/音声/数式 UI。

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::color_picker::ColorPicker;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Window, div, px, rgba,
};

use crate::core::animation::{AnimatedParameter, ExtrapolationMode, Keyframe};
use crate::core::ids::{ClipId, ClipRef};
use crate::core::time::FrameIndex;
use crate::media::visual_properties::{BlendMode, SubtitleProperties, TextAlignment};
use crate::timeline::command::{EditCommand, ParameterPath, ParameterTarget, ParameterValue};
use crate::timeline::document::{AudioProperties, Clip, ClipKind, Layer};
use crate::ui::app_controller::{AppController, RightPanelTab};
use crate::ui::i18n::tr;
use crate::ui::inspector::midpoint_bar::render_midpoint_bar;
use crate::ui::inspector::segment_ops::{
    InspectorParameterSlot, build_batch_relative_xy_offset_command, build_expression_command,
    build_remove_keyframe_command, build_reset_clip_parameters_command,
    build_reset_color_adjustments_command, clip_has_non_default_color_adjustments,
    effect_frame_at_playhead, evaluation_context_for_clip, sample_parameter_curve,
};
use crate::ui::inspector::trackbar_row::{
    InspectorWidgetBank, apply_subtitle_font_family_command, render_trackbar_row,
    spawn_google_font_install, spawn_google_font_search,
};
use crate::ui::window::SeaEditorWindow;

/// インスペクタ描画に必要な UI ウィジェット群への参照。
#[derive(Clone, Copy)]
pub struct InspectorWidgets<'a> {
    /// 全インスペクタ入力・カラーピッカー保持バンク。
    pub bank: &'a InspectorWidgetBank,
}

/// 選択状態に応じたクリップインスペクタ全体を構築する。
pub fn render_clip_inspector(
    controller: &AppController,
    widgets: InspectorWidgets<'_>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement + use<> {
    let theme = cx.theme().clone();
    let selected_ids = controller.effective_selected_clip_ids();

    if selected_ids.is_empty() {
        return render_empty_inspector(&theme, cx);
    }
    if selected_ids.len() >= 2 {
        return render_multi_select_inspector(controller, &selected_ids, widgets, &theme, cx);
    }

    let Some((layer, clip)) = controller
        .document
        .find_clip(&selected_ids[0])
        .or_else(|| controller.selected_clip())
    else {
        return render_empty_inspector(&theme, cx);
    };

    render_single_clip_inspector(controller, layer, clip, widgets, &theme, cx)
}

fn render_empty_inspector(theme: &Theme, _cx: &mut Context<SeaEditorWindow>) -> gpui_kit::Div {
    v_flex()
        .gap_2()
        .p_3()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_sm()
                .font_semibold()
                .text_color(theme.foreground)
                .child(tr("inspector.no_selection")),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(tr("inspector.no_selection_hint")),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(tr("inspector.multi_select_hint")),
        )
}

fn render_multi_select_inspector(
    controller: &AppController,
    selected_ids: &[ClipId],
    widgets: InspectorWidgets<'_>,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Div {
    let bank = widgets.bank;
    let count = selected_ids.len();
    let names: Vec<String> = selected_ids
        .iter()
        .filter_map(|id| {
            controller
                .document
                .find_clip(id)
                .map(|(_, c)| c.name.clone())
        })
        .take(6)
        .collect();

    v_flex()
        .gap_2()
        .w_full()
        .child(
            v_flex()
                .gap_1()
                .p_2()
                .rounded_sm()
                .bg(rgba(0x1d4e_d833))
                .border_1()
                .border_color(rgba(0x3b82_f6aa))
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(crate::ui::i18n::tr_fmt(
                            "inspector.multi_select_title",
                            &[("count", &count.to_string())],
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(names.join(", ")),
                ),
        )
        .child(render_multi_select_actions(selected_ids, cx))
        .child(render_multi_select_offset_box(
            selected_ids,
            bank,
            theme,
            cx,
        ))
}

fn render_multi_select_actions(
    selected_ids: &[ClipId],
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let ids_dup = selected_ids.to_vec();
    let ids_del = selected_ids.to_vec();
    h_flex()
        .gap_1()
        .flex_wrap()
        .child(
            Button::new("multi-dup-btn")
                .ghost()
                .small()
                .label(tr("inspector.bulk_duplicate"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    duplicate_selected_clips(this, &ids_dup, window, cx);
                })),
        )
        .child(
            Button::new("multi-del-btn")
                .ghost()
                .small()
                .label(tr("inspector.bulk_delete"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    delete_selected_clips(this, &ids_del, window, cx);
                })),
        )
        .child(
            Button::new("multi-clear-btn")
                .ghost()
                .small()
                .label("選択解除")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.clear_clip_selection();
                    cx.notify();
                })),
        )
}

fn render_multi_select_offset_box(
    selected_ids: &[ClipId],
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let ids_move = selected_ids.to_vec();
    v_flex()
        .gap_1p5()
        .p_2()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("相対移動 (ΔX / ΔY px)"),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(div().text_xs().child("ΔX"))
                .child(div().w(px(56.0)).child(Input::new(&bank.multi_dx_input)))
                .child(div().text_xs().child("ΔY"))
                .child(div().w(px(56.0)).child(Input::new(&bank.multi_dy_input)))
                .child(
                    Button::new("multi-apply-xy-btn")
                        .primary()
                        .small()
                        .label("適用")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            apply_multi_xy_offset(this, &ids_move, window, cx);
                        })),
                ),
        )
}

fn duplicate_selected_clips(
    win: &mut SeaEditorWindow,
    ids: &[ClipId],
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let mut commands = Vec::new();
    for cid in ids {
        if let Some((layer, clip)) = win.controller.document.find_clip(cid) {
            if layer.locked {
                continue;
            }
            let target_start = clip
                .timeline_start
                .get()
                .saturating_add(clip.duration.get());
            commands.push(EditCommand::DuplicateClip {
                clip_id: ClipRef::Id(cid.clone()),
                target_layer_id: Some(crate::core::ids::LayerRef::Id(layer.id.clone())),
                new_timeline_start: FrameIndex::new(target_start),
                new_temp_id: None,
                new_id: None,
            });
        }
    }
    if !commands.is_empty() {
        let _ = win
            .controller
            .dispatch_command(&EditCommand::Batch { commands });
        win.controller.inspector.invalidate_sync();
        win.sync_sliders_now(window, cx);
        cx.notify();
    }
}

fn delete_selected_clips(
    win: &mut SeaEditorWindow,
    ids: &[ClipId],
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let commands: Vec<EditCommand> = ids
        .iter()
        .filter(|cid| {
            win.controller
                .document
                .find_clip(cid)
                .is_some_and(|(layer, _)| !layer.locked)
        })
        .map(|cid| EditCommand::RemoveClip {
            clip_id: ClipRef::Id(cid.clone()),
        })
        .collect();
    if !commands.is_empty() {
        let _ = win
            .controller
            .dispatch_command(&EditCommand::Batch { commands });
        win.controller.inspector.invalidate_sync();
        win.sync_sliders_now(window, cx);
        cx.notify();
    }
}

fn apply_multi_xy_offset(
    win: &mut SeaEditorWindow,
    ids: &[ClipId],
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let dx = win
        .inspector_widgets
        .multi_dx_input
        .read(cx)
        .value()
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0);
    let dy = win
        .inspector_widgets
        .multi_dy_input
        .read(cx)
        .value()
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0);
    if let Ok(cmd) = build_batch_relative_xy_offset_command(&win.controller.document, ids, dx, dy)
        && win.controller.dispatch_command(&cmd).is_ok()
    {
        win.controller.inspector.invalidate_sync();
        win.sync_sliders_now(window, cx);
        cx.notify();
    }
}

fn render_single_clip_inspector(
    controller: &AppController,
    layer: &Layer,
    clip: &Clip,
    widgets: InspectorWidgets<'_>,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Div {
    let bank = widgets.bank;
    let mut root = v_flex()
        .gap_2()
        .w_full()
        .child(render_object_header(controller, layer, clip, theme, cx))
        .child(render_midpoint_bar(controller, clip, theme, cx));

    if has_visual_section(&clip.kind) {
        let std_open = controller.is_inspector_section_open("standard");
        root = root.child(render_collapsible_section(
            "standard",
            tr("inspector.section_standard"),
            false,
            std_open,
            render_standard_section_body(controller, clip, bank, theme, cx),
            theme,
            cx,
        ));

        let color_active = clip_has_non_default_color_adjustments(clip);
        let color_open = controller.is_inspector_section_open("color");
        root = root.child(render_collapsible_section(
            "color",
            "色調補正",
            color_active,
            color_open,
            render_color_section_body(controller, clip, bank, theme, cx),
            theme,
            cx,
        ));
    }

    if let ClipKind::Subtitle(sub) = &clip.kind {
        let sub_open = controller.is_inspector_section_open("subtitle");
        root = root.child(render_collapsible_section(
            "subtitle",
            tr("inspector.section_subtitle"),
            false,
            sub_open,
            render_subtitle_section_body(controller, clip, sub, bank, theme, cx),
            theme,
            cx,
        ));
    }

    if let Some((audio, preserve_pitch)) = clip_audio_info(&clip.kind) {
        let audio_open = controller.is_inspector_section_open("audio");
        root = root.child(render_collapsible_section(
            "audio",
            tr("inspector.section_audio"),
            false,
            audio_open,
            render_audio_section_body(controller, clip, audio, preserve_pitch, bank, theme, cx),
            theme,
            cx,
        ));
    }

    let expr_open = controller.is_inspector_section_open("expression");
    root = root.child(render_collapsible_section(
        "expression",
        tr("inspector.section_expr"),
        false,
        expr_open,
        render_curve_and_expr_section_body(controller, clip, bank, theme, cx),
        theme,
        cx,
    ));

    root
}

fn render_object_header(
    controller: &AppController,
    layer: &Layer,
    clip: &Clip,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let kind_badge = match clip.kind {
        ClipKind::Video { .. } => "動画",
        ClipKind::Audio { .. } => "音声",
        ClipKind::Image { .. } => "静止画",
        ClipKind::Subtitle(_) => "字幕",
    };
    let layer_idx = controller
        .document
        .layers
        .iter()
        .position(|l| l.id == layer.id)
        .unwrap_or(0);
    let start_f = clip.timeline_start.get();
    let end_f = start_f.saturating_add(clip.duration.get());
    let cid_reset = clip.id.clone();

    v_flex()
        .gap_1()
        .p_2()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_1()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .px_1()
                                .py_0p5()
                                .rounded_xs()
                                .bg(rgba(0x3b82_f644))
                                .text_xs()
                                .child(kind_badge),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_semibold()
                                .text_color(theme.foreground)
                                .child(clip.name.clone()),
                        ),
                )
                .child(
                    Button::new("clip-reset-all-btn")
                        .ghost()
                        .small()
                        .label(tr("inspector.reset"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some((_, c)) = this.controller.document.find_clip(&cid_reset)
                                && let Some(cmd) = build_reset_clip_parameters_command(c)
                            {
                                let _ = this.controller.dispatch_command(&cmd);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
                                cx.notify();
                            }
                        })),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_1()
                .child(
                    Button::new("clip-layer-jump-btn")
                        .ghost()
                        .small()
                        .label(format!(
                            "L{} ({}) · {start_f}f..{end_f}f",
                            layer_idx + 1,
                            layer.name
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.controller.seek_to_frame(FrameIndex::new(start_f));
                            this.controller.right_tab = RightPanelTab::LayerList;
                            this.sync_sliders_now(window, cx);
                            cx.notify();
                        })),
                )
                .children(if layer.locked {
                    Some(
                        div()
                            .px_1()
                            .rounded_xs()
                            .bg(rgba(0xd977_0644))
                            .text_xs()
                            .child("🔒ロック"),
                    )
                } else {
                    None
                }),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_collapsible_section(
    key: &'static str,
    title: impl AsRef<str>,
    blue_dot: bool,
    open: bool,
    content: AnyElement,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let caret = if open { "▾" } else { "▸" };
    let header = h_flex()
        .items_center()
        .justify_between()
        .px_1p5()
        .py_1()
        .rounded_sm()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            Button::new(format!("insp-sec-btn-{key}"))
                .ghost()
                .small()
                .label(format!("{caret} {}", title.as_ref()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.toggle_inspector_section(key);
                    cx.notify();
                })),
        )
        .children(if blue_dot {
            Some(
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded_full()
                    .bg(rgba(0x3b82_f6ff)),
            )
        } else {
            None
        });

    Collapsible::new()
        .motion_id(format!("insp-sec-coll-{key}"))
        .open(open)
        .child(header)
        .content(div().pt_1().child(content))
}

fn render_standard_section_body(
    controller: &AppController,
    clip: &Clip,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let mut body = v_flex().gap_1().w_full();
    body = body.child(render_blend_mode_row(clip, theme, cx));
    for &slot in InspectorParameterSlot::standard_slots() {
        body = body.child(render_trackbar_row(controller, clip, slot, bank, theme, cx));
    }

    let anchor_open = controller.is_inspector_section_open("anchor");
    let mut anchor_content = v_flex().gap_1();
    for &slot in InspectorParameterSlot::anchor_slots() {
        anchor_content =
            anchor_content.child(render_trackbar_row(controller, clip, slot, bank, theme, cx));
    }
    body = body.child(render_collapsible_section(
        "anchor",
        tr("inspector.section_anchor"),
        false,
        anchor_open,
        anchor_content.into_any_element(),
        theme,
        cx,
    ));

    body.into_any_element()
}

fn render_color_section_body(
    controller: &AppController,
    clip: &Clip,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let cid_reset = clip.id.clone();
    let mut body = v_flex().gap_1().w_full().child(
        h_flex().justify_end().child(
            Button::new("color-reset-btn")
                .ghost()
                .small()
                .label("色調リセット")
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some((_, c)) = this.controller.document.find_clip(&cid_reset)
                        && let Some(cmd) = build_reset_color_adjustments_command(c)
                    {
                        let _ = this.controller.dispatch_command(&cmd);
                        this.controller.inspector.invalidate_sync();
                        this.sync_sliders_now(window, cx);
                        cx.notify();
                    }
                })),
        ),
    );
    for &slot in InspectorParameterSlot::color_slots() {
        body = body.child(render_trackbar_row(controller, clip, slot, bank, theme, cx));
    }
    body.into_any_element()
}

fn render_subtitle_section_body(
    controller: &AppController,
    clip: &Clip,
    sub: &SubtitleProperties,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    v_flex()
        .gap_1p5()
        .w_full()
        .child(render_subtitle_text_box(&clip.id, bank, theme, cx))
        .child(render_subtitle_font_align_row(
            &clip.id, sub, bank, theme, cx,
        ))
        .child(render_subtitle_color_outline_row(bank, theme))
        .child(render_trackbar_row(
            controller,
            clip,
            InspectorParameterSlot::SubtitleFontSize,
            bank,
            theme,
            cx,
        ))
        .into_any_element()
}

fn render_subtitle_text_box(
    clip_id: &ClipId,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid_txt = clip_id.clone();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(tr("inspector.subtitle_text")),
        )
        .child(Textarea::new(&bank.subtitle_textarea).h(px(64.0)))
        .child(
            h_flex().justify_end().child(
                Button::new("apply-sub-text-btn")
                    .primary()
                    .small()
                    .label(tr("inspector.subtitle_apply"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let text = this
                            .inspector_widgets
                            .subtitle_textarea
                            .read(cx)
                            .value()
                            .to_string();
                        let cmd = EditCommand::SetParameter {
                            target: ParameterTarget::Clip {
                                clip_id: ClipRef::Id(cid_txt.clone()),
                            },
                            path: ParameterPath::SubtitleText,
                            value: ParameterValue::Text { value: text },
                        };
                        let _ = this.controller.dispatch_command(&cmd);
                        cx.notify();
                    })),
            ),
        )
}

fn render_subtitle_font_align_row(
    clip_id: &ClipId,
    sub: &SubtitleProperties,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid_align = clip_id.clone();
    let alignments = [
        (tr("inspector.align_left"), TextAlignment::Left),
        (tr("inspector.align_center"), TextAlignment::Center),
        (tr("inspector.align_right"), TextAlignment::Right),
    ];

    let top_row = h_flex()
        .items_center()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(tr("inspector.font")),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(120.0))
                .child(Select::new(&bank.font_family_select).small()),
        )
        .children(
            alignments
                .into_iter()
                .enumerate()
                .map(|(ix, (lbl, align))| {
                    let cid_btn = cid_align.clone();
                    let mut b = Button::new(("sub-align-btn", ix)).small().label(lbl);
                    b = if sub.alignment == align {
                        b.primary()
                    } else {
                        b.ghost()
                    };
                    b.on_click(cx.listener(move |this, _, _, cx| {
                        let cmd = EditCommand::SetParameter {
                            target: ParameterTarget::Clip {
                                clip_id: ClipRef::Id(cid_btn.clone()),
                            },
                            path: ParameterPath::SubtitleAlignment,
                            value: ParameterValue::TextAlignment { value: align },
                        };
                        let _ = this.controller.dispatch_command(&cmd);
                        cx.notify();
                    }))
                }),
        );

    v_flex()
        .gap_1()
        .w_full()
        .child(top_row)
        .child(render_subtitle_custom_gf_row(clip_id, bank, theme, cx))
}

fn render_subtitle_custom_gf_row(
    clip_id: &ClipId,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid_manual = clip_id.clone();
    let cid_gf = clip_id.clone();
    v_flex()
        .gap_0p5()
        .w_full()
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .w_full()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.muted_foreground)
                        .child(tr("inspector.font_custom_label")),
                )
                .child(div().flex_1().child(Input::new(&bank.font_family_input)))
                .child(
                    Button::new("sub-font-apply-btn")
                        .ghost()
                        .xsmall()
                        .label(tr("inspector.font_apply"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let val = this
                                .inspector_widgets
                                .font_family_input
                                .read(cx)
                                .value()
                                .to_string();
                            apply_subtitle_font_family_command(this, &cid_manual, &val);
                            this.font_select_needs_refresh = true;
                            this.mark_sliders_dirty();
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("sub-font-gf-search-btn")
                        .ghost()
                        .xsmall()
                        .label(tr("inspector.font_gf_search"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let q = this
                                .inspector_widgets
                                .font_family_input
                                .read(cx)
                                .value()
                                .to_string();
                            spawn_google_font_search(&q, cx);
                        })),
                )
                .child(
                    Button::new("sub-font-gf-fetch-btn")
                        .ghost()
                        .xsmall()
                        .label(tr("inspector.font_gf_fetch"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let fam = this
                                .inspector_widgets
                                .font_family_input
                                .read(cx)
                                .value()
                                .to_string();
                            spawn_google_font_install(&fam, Some(cid_gf.clone()), cx);
                        })),
                ),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.muted_foreground)
                .child(tr("inspector.font_gf_hint")),
        )
}

fn render_subtitle_color_outline_row(
    bank: &InspectorWidgetBank,
    theme: &Theme,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_2()
        .flex_wrap()
        .child(
            ColorPicker::new(&bank.text_color_picker)
                .small()
                .label(tr("inspector.text_color")),
        )
        .child(
            ColorPicker::new(&bank.outline_color_picker)
                .small()
                .label(tr("inspector.outline_color")),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr("inspector.outline_width")),
                )
                .child(
                    div()
                        .w(px(46.0))
                        .child(Input::new(&bank.outline_width_input)),
                ),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_audio_section_body(
    controller: &AppController,
    clip: &Clip,
    audio: &AudioProperties,
    preserve_pitch: Option<bool>,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let cid_mute = clip.id.clone();
    let cid_pitch = clip.id.clone();
    let current_rate = match &clip.kind {
        ClipKind::Video { playback_rate, .. } | ClipKind::Audio { playback_rate, .. } => {
            Some(*playback_rate)
        }
        _ => None,
    };

    let mut body = v_flex().gap_1p5().w_full();
    for &slot in InspectorParameterSlot::audio_slots() {
        body = body.child(render_trackbar_row(controller, clip, slot, bank, theme, cx));
    }

    let mut flags = h_flex().items_center().gap_3().child(
        Checkbox::new("audio-mute-checkbox")
            .small()
            .label("ミュート")
            .checked(audio.muted)
            .on_click(cx.listener(move |this, next_checked: &bool, _, cx| {
                let cmd = EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Id(cid_mute.clone()),
                    },
                    path: ParameterPath::AudioMuted,
                    value: ParameterValue::Bool {
                        value: *next_checked,
                    },
                };
                let _ = this.controller.dispatch_command(&cmd);
                cx.notify();
            })),
    );
    if let (Some(pitch), Some(rate)) = (preserve_pitch, current_rate) {
        flags = flags.child(
            Checkbox::new("audio-pitch-checkbox")
                .small()
                .label("ピッチ維持")
                .checked(pitch)
                .on_click(cx.listener(move |this, next_checked: &bool, _, cx| {
                    let cmd = EditCommand::SetPlaybackRate {
                        clip_id: ClipRef::Id(cid_pitch.clone()),
                        new_rate: rate,
                        preserve_pitch: Some(*next_checked),
                    };
                    let _ = this.controller.dispatch_command(&cmd);
                    cx.notify();
                })),
        );
    }
    body = body.child(flags);

    if !audio.plugin_slots.is_empty() {
        body = body.child(render_audio_plugins_list(clip, audio, theme, cx));
    }

    body.into_any_element()
}

fn render_audio_plugins_list(
    clip: &Clip,
    audio: &AudioProperties,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let mut list = v_flex()
        .gap_1()
        .p_1p5()
        .rounded_sm()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("オーディオプラグイン"),
        );

    for (ix, slot) in audio.plugin_slots.iter().enumerate() {
        let cid = clip.id.clone();
        let sid = slot.slot_id.clone();
        list = list.child(
            Checkbox::new(("audio-plugin-chk", ix))
                .small()
                .label(slot.name.clone())
                .checked(slot.enabled)
                .on_click(cx.listener(move |this, next: &bool, _, cx| {
                    let cmd = EditCommand::SetParameter {
                        target: ParameterTarget::AudioPlugin {
                            clip_id: ClipRef::Id(cid.clone()),
                            slot_id: sid.clone(),
                        },
                        path: ParameterPath::PluginEnabled,
                        value: ParameterValue::Bool { value: *next },
                    };
                    let _ = this.controller.dispatch_command(&cmd);
                    cx.notify();
                })),
        );
    }
    list
}

fn render_blend_mode_row(
    clip: &Clip,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let current = match &clip.kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => visual.blend_mode,
        ClipKind::Subtitle(sub) => sub.visual.blend_mode,
        ClipKind::Audio { .. } => BlendMode::Normal,
    };
    let modes = [
        ("通常", BlendMode::Normal),
        ("加算", BlendMode::Add),
        ("乗算", BlendMode::Multiply),
        ("ｽｸﾘｰﾝ", BlendMode::Screen),
        ("ｵｰﾊﾞｰﾚｲ", BlendMode::Overlay),
    ];

    h_flex()
        .items_center()
        .gap_1()
        .flex_wrap()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("合成:"),
        )
        .children(modes.into_iter().enumerate().map(|(ix, (lbl, mode))| {
            let cid = clip.id.clone();
            let mut btn = Button::new(("blend-mode-btn", ix)).small().label(lbl);
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
                    path: ParameterPath::BlendMode,
                    value: ParameterValue::BlendMode { value: mode },
                };
                let _ = this.controller.dispatch_command(&cmd);
                cx.notify();
            }))
        }))
}

fn render_curve_and_expr_section_body(
    controller: &AppController,
    clip: &Clip,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let active_slot = controller.inspector.active_slot;
    let available = InspectorParameterSlot::available_for(clip);
    let slot = if available.contains(&active_slot) {
        active_slot
    } else {
        available.first().copied().unwrap_or(active_slot)
    };
    let param = slot
        .extract_param(clip)
        .cloned()
        .unwrap_or_else(|| AnimatedParameter::constant(0.0));
    let head = controller.clock.current_frame();
    let ctx_head = evaluation_context_for_clip(&controller.document, clip, head);
    let eval_now = param.evaluate(ctx_head).unwrap_or(0.0);
    let samples = sample_parameter_curve(clip, controller.document.fps, slot, 28);

    let mut body = v_flex()
        .gap_1p5()
        .w_full()
        .child(div().text_xs().text_color(theme.foreground).child(format!(
            "対象: {} | 現在値: {:.2}{}",
            slot.label(),
            slot.internal_to_ui(eval_now),
            slot.ui_unit_label()
        )))
        .child(render_curve_bar_graph(&samples, theme));

    match &param {
        AnimatedParameter::Expression { .. } | AnimatedParameter::Constant { .. } => {
            body = body.child(render_expression_editor(
                clip, slot, controller, bank, theme, cx,
            ));
        }
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => {
            body = body
                .child(render_extrapolation_row(
                    &clip.id,
                    slot,
                    keyframes,
                    *extrapolation,
                    cx,
                ))
                .child(render_keyframes_list(clip, slot, keyframes, head, cx));
        }
        AnimatedParameter::Easing { curve, .. } => {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("補間カーブ: {curve:?}")),
            );
        }
    }

    body.into_any_element()
}

#[allow(clippy::cast_possible_truncation)]
fn render_curve_bar_graph(samples: &[f64], theme: &Theme) -> impl IntoElement {
    let min_v = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let max_v = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = (max_v - min_v).abs();

    h_flex()
        .items_end()
        .h(px(36.0))
        .gap_0p5()
        .p_1()
        .rounded_sm()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .children(samples.iter().map(|&val| {
            let ratio = if span > 1e-6 {
                ((val - min_v) / span).clamp(0.08, 1.0)
            } else {
                0.5
            };
            div()
                .flex_1()
                .h(px((ratio * 26.0) as f32))
                .rounded_xs()
                .bg(rgba(0x3b82_f6cc))
        }))
}

fn render_expression_editor(
    clip: &Clip,
    slot: InspectorParameterSlot,
    controller: &AppController,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid = clip.id.clone();
    let status = match &controller.inspector.expr_error {
        Some(err) => div()
            .p_1()
            .rounded_xs()
            .bg(rgba(0x991b_1b44))
            .text_xs()
            .text_color(rgba(0xfeca_caff))
            .child(format!("構文エラー: {err}")),
        None => div()
            .px_1()
            .py_0p5()
            .rounded_xs()
            .bg(rgba(0x1453_2d44))
            .text_xs()
            .text_color(rgba(0x86ef_acff))
            .child("構文 OK (変数: t, p, d, pi, e)"),
    };

    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_1()
                .child(div().flex_1().child(Input::new(&bank.expr_input)))
                .child(
                    Button::new("expr-apply-btn")
                        .primary()
                        .small()
                        .label("f(t)適用")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let raw = this
                                .inspector_widgets
                                .expr_input
                                .read(cx)
                                .value()
                                .to_string();
                            let off = this
                                .inspector_widgets
                                .expr_offset_input
                                .read(cx)
                                .value()
                                .trim()
                                .parse::<f64>()
                                .unwrap_or(this.controller.inspector.expr_base_offset);
                            let scl = this
                                .inspector_widgets
                                .expr_scale_input
                                .read(cx)
                                .value()
                                .trim()
                                .parse::<f64>()
                                .unwrap_or(this.controller.inspector.expr_scale);
                            if let Some((_, c)) = this.controller.document.find_clip(&cid) {
                                let c_owned = c.clone();
                                match build_expression_command(&c_owned, slot, &raw, off, scl) {
                                    Ok(cmd) => {
                                        this.controller.inspector.expr_error = None;
                                        let _ = this.controller.dispatch_command(&cmd);
                                        this.controller.inspector.invalidate_sync();
                                        this.sync_sliders_now(window, cx);
                                    }
                                    Err(err) => this.controller.inspector.expr_error = Some(err),
                                }
                            }
                            cx.notify();
                        })),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("offset:"),
                )
                .child(div().w(px(56.0)).child(Input::new(&bank.expr_offset_input)))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("scale:"),
                )
                .child(div().w(px(56.0)).child(Input::new(&bank.expr_scale_input))),
        )
        .child(status)
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

fn render_keyframes_list(
    clip: &Clip,
    slot: InspectorParameterSlot,
    keyframes: &[Keyframe],
    head: FrameIndex,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let head_eff = effect_frame_at_playhead(clip, head);
    let mut list = v_flex().gap_1();

    for (ix, kf) in keyframes.iter().enumerate() {
        let kf_frame = kf.frame;
        let is_current = kf_frame == head_eff;
        let cid_del = clip.id.clone();
        let tl_start = clip.timeline_start.get();
        let eff_off = clip.effect_offset_frames;

        let mut row = h_flex()
            .items_center()
            .justify_between()
            .px_1()
            .py_0p5()
            .rounded_xs()
            .border_1()
            .border_color(if is_current {
                rgba(0x3b82_f6ff)
            } else {
                rgba(0x4755_69ff)
            })
            .child(
                Button::new(("kf-seek-btn", ix))
                    .ghost()
                    .small()
                    .label(format!(
                        "@{kf_frame}f = {:.2}{}",
                        slot.internal_to_ui(kf.value),
                        slot.ui_unit_label()
                    ))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
                        let tl = (tl_start as i64 + (kf_frame - eff_off)).max(0) as u64;
                        this.controller.seek_to_frame(FrameIndex::new(tl));
                        this.sync_sliders_now(window, cx);
                        cx.notify();
                    })),
            );
        if keyframes.len() > 1 {
            row = row.child(
                Button::new(("kf-del-btn", ix))
                    .ghost()
                    .small()
                    .label("削除")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some((_, c)) = this.controller.document.find_clip(&cid_del) {
                            let c_owned = c.clone();
                            if let Ok(cmd) = build_remove_keyframe_command(&c_owned, slot, kf_frame)
                            {
                                let _ = this.controller.dispatch_command(&cmd);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
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

const fn has_visual_section(kind: &ClipKind) -> bool {
    matches!(
        kind,
        ClipKind::Video { .. } | ClipKind::Image { .. } | ClipKind::Subtitle(_)
    )
}

fn clip_audio_info(kind: &ClipKind) -> Option<(&AudioProperties, Option<bool>)> {
    match kind {
        ClipKind::Video {
            audio: Some(a),
            preserve_pitch,
            ..
        } => Some((a, Some(*preserve_pitch))),
        ClipKind::Audio {
            audio,
            preserve_pitch,
            ..
        } => Some((audio, Some(*preserve_pitch))),
        _ => None,
    }
}
