//! `AviUtl` 風 `TrackBarRow`・`InspectorWidgetBank`・移動方法ドロップダウン・スライダ連続ドラッグ。

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::{ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::select::{SearchableVec, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, Focusable as _, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Rgba, SharedString, Styled as _,
    Subscription, Window, div, px, relative, rgba,
};

use crate::core::animation::EasingCurve;
use crate::core::color::RgbaColor;
use crate::core::ids::{ClipId, ClipRef};
use crate::media::google_fonts::{FontSourceKind, fetch_google_fonts_catalog};
use crate::media::subtitle_renderer::{
    install_google_font_for_subtitles, list_available_font_families,
};
use crate::timeline::command::{EditCommand, ParameterPath, ParameterTarget, ParameterValue};
use crate::timeline::document::{Clip, ClipKind};
use crate::ui::app_controller::{AppController, MIN_RIGHT_PANEL_WIDTH};
use crate::ui::i18n::tr;
use crate::ui::inspector::segment_ops::{
    InspectorMode, InspectorParameterSlot, MovementModeEdit, MovementModePreset,
    SegmentEndpointEdit, TrackBarDragStart, build_apply_bezier_curve_command,
    build_apply_movement_mode_with_options, build_expression_command,
    build_set_segment_endpoint_with_lock, read_trackbar_segment_view, set_animated_command,
    validate_expression_and_tuning,
};
use crate::ui::window::SeaEditorWindow;

const WIDE_ROW_BREAKPOINT_PX: f32 = 380.0;

/// 字幕フォントコンボボックスに表示するフォント候補アイテム。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontSelectItem {
    /// コンボボックス一覧に表示するラベル（標準 / OS導入済 / GF取得可）。
    pub title: SharedString,
    /// `SubtitleProperties::font_family` へ保存される実フォントファミリ名。
    pub family: String,
    /// ローカル環境に導入済みでオフライン即時描画できるか。
    pub is_local: bool,
}

impl SelectItem for FontSelectItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.family
    }
}

/// 標準プリセット・OS導入フォント・Google Fonts 検索結果を統合したコンボボックス候補を生成する。
#[must_use]
pub fn build_font_select_items(
    extra_families: &[String],
    current_font: Option<&str>,
) -> SearchableVec<FontSelectItem> {
    let catalog = list_available_font_families();
    let mut items = Vec::with_capacity(catalog.len() + extra_families.len() + 1);
    let mut seen = std::collections::HashSet::new();

    for entry in catalog {
        seen.insert(entry.family.to_ascii_lowercase());
        let suffix = match entry.source {
            FontSourceKind::Standard => "  [標準]",
            FontSourceKind::Installed => "",
            FontSourceKind::GoogleCached => "  [GF導入済]",
            FontSourceKind::GoogleAvailable => "  [GF取得可]",
        };
        items.push(FontSelectItem {
            title: SharedString::from(format!("{}{suffix}", entry.family)),
            family: entry.family,
            is_local: entry.is_local,
        });
    }

    for extra in extra_families {
        let trimmed = extra.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_ascii_lowercase()) {
            continue;
        }
        items.push(FontSelectItem {
            title: SharedString::from(format!("{trimmed}  [GF検索]")),
            family: trimmed.to_owned(),
            is_local: false,
        });
    }

    if let Some(cur) = current_font.map(str::trim).filter(|s| !s.is_empty())
        && seen.insert(cur.to_ascii_lowercase())
    {
        items.insert(
            0,
            FontSelectItem {
                title: SharedString::from(format!("{cur}  [カスタム]")),
                family: cur.to_owned(),
                is_local: false,
            },
        );
    }

    SearchableVec::new(items)
}

/// インスペクタ全体で共有する入力・カラーピッカーエンティティ群。
#[derive(Clone)]
pub struct InspectorWidgetBank {
    /// 各パラメータの `(開始=false / 終了=true)` 数値入力欄。
    pub track_inputs: HashMap<(InspectorParameterSlot, bool), Entity<InputState>>,
    /// ベジェ制御点 `(x1, y1, x2, y2)` 入力欄。
    pub bezier_inputs: [Entity<InputState>; 4],
    /// 字幕複数行本文 `Textarea`。
    pub subtitle_textarea: Entity<TextareaState>,
    /// 字幕フォントファミリ検索可能コンボボックス状態。
    pub font_family_select: Entity<SelectState<SearchableVec<FontSelectItem>>>,
    /// 字幕フォントファミリ任意入力・Google Fonts 検索欄。
    pub font_family_input: Entity<InputState>,
    /// 字幕縁取り幅入力欄。
    pub outline_width_input: Entity<InputState>,
    /// 字幕文字色ピッカー。
    pub text_color_picker: Entity<ColorPickerState>,
    /// 字幕縁色ピッカー。
    pub outline_color_picker: Entity<ColorPickerState>,
    /// `f(t)` 数式入力欄。
    pub expr_input: Entity<InputState>,
    /// `f(t)` オフセット微調整欄。
    pub expr_offset_input: Entity<InputState>,
    /// `f(t)` 倍率微調整欄。
    pub expr_scale_input: Entity<InputState>,
    /// 複数選択一括相対移動 `ΔX` 欄。
    pub multi_dx_input: Entity<InputState>,
    /// 複数選択一括相対移動 `ΔY` 欄。
    pub multi_dy_input: Entity<InputState>,
}

impl InspectorWidgetBank {
    /// インスペクタ用エンティティを一括生成しイベント購読を登録する。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<SeaEditorWindow>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let track_inputs = build_track_input_map(window, cx, subscriptions);
        let bezier_inputs = build_bezier_inputs(window, cx, subscriptions);
        let subtitle_textarea = cx.new(|cx| TextareaState::new(window, cx).rows(3).auto_grow(2, 6));
        subscriptions.push(subscribe_subtitle_textarea(&subtitle_textarea, window, cx));

        let font_family_select = cx.new(|cx| {
            SelectState::new(
                build_font_select_items(&[], Some("sans-serif")),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        subscriptions.push(subscribe_font_family_select(
            &font_family_select,
            window,
            cx,
        ));

        let font_family_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("sans-serif")
                .placeholder(tr("inspector.font_custom_placeholder"))
        });
        subscriptions.push(subscribe_font_family_input(&font_family_input, window, cx));

        let outline_width_input = cx.new(|cx| InputState::new(window, cx).default_value("2.0"));
        subscriptions.push(subscribe_outline_width_input(
            &outline_width_input,
            window,
            cx,
        ));

        let text_color_picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(rgba_to_hsla(RgbaColor::WHITE))
        });
        subscriptions.push(subscribe_color_picker(
            &text_color_picker,
            ParameterPath::SubtitleTextColor,
            window,
            cx,
        ));

        let outline_color_picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(rgba_to_hsla(RgbaColor::BLACK))
        });
        subscriptions.push(subscribe_color_picker(
            &outline_color_picker,
            ParameterPath::SubtitleOutlineColor,
            window,
            cx,
        ));

        let expr_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("数式 f(t) 例: 0.5 + 0.5 * sin(t * 2.0)")
                .default_value("1.0")
        });
        subscriptions.push(subscribe_expr_input(&expr_input, window, cx));

        let expr_offset_input = cx.new(|cx| InputState::new(window, cx).default_value("0.0"));
        let expr_scale_input = cx.new(|cx| InputState::new(window, cx).default_value("1.0"));
        subscriptions.push(subscribe_expr_tuning_input(
            &expr_offset_input,
            true,
            window,
            cx,
        ));
        subscriptions.push(subscribe_expr_tuning_input(
            &expr_scale_input,
            false,
            window,
            cx,
        ));

        let multi_horiz_input = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let multi_vert_input = cx.new(|cx| InputState::new(window, cx).default_value("0"));

        Self {
            track_inputs,
            bezier_inputs,
            subtitle_textarea,
            font_family_select,
            font_family_input,
            outline_width_input,
            text_color_picker,
            outline_color_picker,
            expr_input,
            expr_offset_input,
            expr_scale_input,
            multi_dx_input: multi_horiz_input,
            multi_dy_input: multi_vert_input,
        }
    }

    /// 指定スロットと開始/終了側の入力欄エンティティを返す。
    #[must_use]
    pub fn track_input(
        &self,
        slot: InspectorParameterSlot,
        is_end: bool,
    ) -> Option<&Entity<InputState>> {
        self.track_inputs.get(&(slot, is_end))
    }

    /// インスペクタ内の全単一行テキスト・数値入力欄をイテレートする。
    pub fn all_text_inputs(&self) -> impl Iterator<Item = &Entity<InputState>> {
        self.track_inputs
            .values()
            .chain(self.bezier_inputs.iter())
            .chain([
                &self.font_family_input,
                &self.outline_width_input,
                &self.expr_input,
                &self.expr_offset_input,
                &self.expr_scale_input,
                &self.multi_dx_input,
                &self.multi_dy_input,
            ])
    }

    /// インスペクタ内のいずれかのテキスト・数値入力欄がフォーカス中かを判定する。
    #[must_use]
    pub fn is_any_input_focused(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        self.subtitle_textarea
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
            || self
                .all_text_inputs()
                .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// フォーカス保護付きでコントローラ状態から全ウィジェット表示を同期する。
    pub fn sync_from_controller(
        &self,
        controller: &mut AppController,
        extra_fonts: &[String],
        fonts_dirty: bool,
        window: &mut Window,
        cx: &mut Context<SeaEditorWindow>,
    ) {
        let Some((_, clip)) = controller.selected_clip() else {
            return;
        };
        let clip_owned = clip.clone();
        let doc_owned = controller.document.clone();
        let head = controller.clock.current_frame();
        let seg_opt = controller.selected_segment_index;

        let prev_cid = controller.inspector.last_synced_clip_id().cloned();
        let force_context_changed = prev_cid.as_ref() != Some(&clip_owned.id);
        let synced =
            controller
                .inspector
                .sync_from_clip_and_segment(&doc_owned, &clip_owned, head, seg_opt);

        self.sync_track_values(controller, &clip_owned, force_context_changed, window, cx);
        self.sync_bezier_and_expr(controller, force_context_changed || synced, window, cx);
        self.sync_subtitle_widgets(
            &clip_owned,
            (force_context_changed || synced, fonts_dirty),
            extra_fonts,
            window,
            cx,
        );
    }

    fn sync_track_values(
        &self,
        controller: &AppController,
        clip: &Clip,
        force_overwrite: bool,
        window: &mut Window,
        cx: &mut Context<SeaEditorWindow>,
    ) {
        let doc = &controller.document;
        let segment_index = controller.selected_segment_index.unwrap_or(0);
        for slot in InspectorParameterSlot::available_for(clip) {
            let Some(view) = read_trackbar_segment_view(doc, clip, slot, segment_index) else {
                continue;
            };
            for (is_end, internal) in [(false, view.start_value), (true, view.end_value)] {
                let Some(entity) = self.track_inputs.get(&(slot, is_end)) else {
                    continue;
                };
                if !force_overwrite && entity.read(cx).focus_handle(cx).is_focused(window) {
                    continue;
                }
                let formatted = format_ui_value(slot, slot.internal_to_ui(internal));
                let unchanged = entity.read(cx).value().as_ref() == formatted;
                if !unchanged {
                    entity.update(cx, |input, cx| input.set_value(&formatted, window, cx));
                }
            }
        }
    }

    fn sync_bezier_and_expr(
        &self,
        controller: &AppController,
        force_overwrite: bool,
        window: &mut Window,
        cx: &mut Context<SeaEditorWindow>,
    ) {
        let (x1, y1, x2, y2) = controller.inspector.bezier_draft;
        let bez_strs = [
            format!("{x1:.2}"),
            format!("{y1:.2}"),
            format!("{x2:.2}"),
            format!("{y2:.2}"),
        ];
        for (entity, text) in self.bezier_inputs.iter().zip(bez_strs) {
            if (force_overwrite || !entity.read(cx).focus_handle(cx).is_focused(window))
                && entity.read(cx).value().as_ref() != text
            {
                entity.update(cx, |input, cx| input.set_value(&text, window, cx));
            }
        }

        let expr_text = controller.inspector.expr_draft.clone();
        if (force_overwrite || !self.expr_input.read(cx).focus_handle(cx).is_focused(window))
            && self.expr_input.read(cx).value().as_ref() != expr_text
        {
            self.expr_input
                .update(cx, |input, cx| input.set_value(&expr_text, window, cx));
        }

        let off_text = format!("{:.2}", controller.inspector.expr_base_offset);
        if (force_overwrite
            || !self
                .expr_offset_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            && self.expr_offset_input.read(cx).value().as_ref() != off_text
        {
            self.expr_offset_input
                .update(cx, |input, cx| input.set_value(&off_text, window, cx));
        }

        let scl_text = format!("{:.2}", controller.inspector.expr_scale);
        if (force_overwrite
            || !self
                .expr_scale_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            && self.expr_scale_input.read(cx).value().as_ref() != scl_text
        {
            self.expr_scale_input
                .update(cx, |input, cx| input.set_value(&scl_text, window, cx));
        }
    }

    fn sync_subtitle_widgets(
        &self,
        clip: &Clip,
        (force_overwrite, fonts_dirty): (bool, bool),
        extra_fonts: &[String],
        window: &mut Window,
        cx: &mut Context<SeaEditorWindow>,
    ) {
        let ClipKind::Subtitle(sub) = &clip.kind else {
            return;
        };
        if (force_overwrite
            || !self
                .subtitle_textarea
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            && self.subtitle_textarea.read(cx).value().as_ref() != sub.text
        {
            let text = sub.text.clone();
            self.subtitle_textarea
                .update(cx, |ta, cx| ta.set_value(&text, window, cx));
        }
        let current_selected = self.font_family_select.read(cx).selected_value().cloned();
        if fonts_dirty || current_selected.as_deref() != Some(sub.font_family.as_str()) {
            let items = build_font_select_items(extra_fonts, Some(&sub.font_family));
            let target_family = sub.font_family.clone();
            self.font_family_select.update(cx, |sel, cx| {
                sel.set_items(items, window, cx);
                sel.set_selected_value(&target_family, window, cx);
            });
        }
        if (force_overwrite
            || !self
                .font_family_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            && self.font_family_input.read(cx).value().as_ref() != sub.font_family
        {
            let family = sub.font_family.clone();
            self.font_family_input
                .update(cx, |input, cx| input.set_value(&family, window, cx));
        }
        let ow_str = format!("{:.1}", sub.outline_width);
        if (force_overwrite
            || !self
                .outline_width_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            && self.outline_width_input.read(cx).value().as_ref() != ow_str
        {
            self.outline_width_input
                .update(cx, |input, cx| input.set_value(&ow_str, window, cx));
        }
        let text_hsla = rgba_to_hsla(sub.text_color);
        if self.text_color_picker.read(cx).value() != Some(text_hsla) {
            self.text_color_picker
                .update(cx, |cp, cx| cp.set_value(text_hsla, window, cx));
        }
        let outline_hsla = rgba_to_hsla(sub.outline_color);
        if self.outline_color_picker.read(cx).value() != Some(outline_hsla) {
            self.outline_color_picker
                .update(cx, |cp, cx| cp.set_value(outline_hsla, window, cx));
        }
    }
}

/// `AviUtl` スタイルの 1 パラメータトラックバー行を構築する。
///
/// 右パネル幅が `380px` 以上では 1 行レイアウト、`240..379px` では 2 行コンパクト配置へ自動適応する。
pub fn render_trackbar_row(
    controller: &AppController,
    clip: &Clip,
    slot: InspectorParameterSlot,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let seg_idx = controller.selected_segment_index.unwrap_or(0);
    let Some(view) = read_trackbar_segment_view(&controller.document, clip, slot, seg_idx) else {
        return div().into_any_element();
    };
    let panel_width = controller
        .right_panel_width
        .unwrap_or(320.0)
        .max(MIN_RIGHT_PANEL_WIDTH);
    let is_wide = panel_width >= WIDE_ROW_BREAKPOINT_PX;
    let is_active = controller.inspector.active_slot == slot;

    let mut wrapper = v_flex()
        .gap_0p5()
        .px_1p5()
        .py_1()
        .rounded_sm()
        .bg(if is_active {
            rgba(0x1e29_3b88)
        } else {
            rgba(0x0000_0000)
        })
        .border_1()
        .border_color(if is_active {
            rgba(0x3b82_f688)
        } else {
            rgba(0x3341_5533)
        });

    if is_wide {
        wrapper = wrapper.child(render_wide_trackbar_line(
            controller, clip, &view, bank, theme, cx,
        ));
    } else {
        wrapper = wrapper
            .child(render_compact_top_line(controller, clip, &view, theme, cx))
            .child(render_compact_bottom_line(
                controller, clip, &view, bank, theme, cx,
            ));
    }

    if view.is_bezier() && is_active {
        wrapper = wrapper.child(render_inline_bezier_row(clip, &view, bank, theme, cx));
    }

    attach_trackbar_row_context_menu(wrapper, &clip.id, slot, cx).into_any_element()
}

fn attach_trackbar_row_context_menu(
    wrapper: gpui_kit::Div,
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let win_entity = cx.entity().clone();
    let cid = clip_id.clone();
    let row_key = row_base_id(slot);

    wrapper
        .id(("tb-row", row_key))
        .context_menu(move |menu: PopupMenu, window, cx| {
            let snap = win_entity.read(cx);
            let Some((layer, clip)) = snap.controller.document.find_clip(&cid) else {
                return menu;
            };
            let locked = layer.locked;
            let other_slots: Vec<InspectorParameterSlot> =
                InspectorParameterSlot::available_for(clip)
                    .into_iter()
                    .filter(|&s| s != slot)
                    .collect();

            build_trackbar_context_popup(
                menu,
                &cid,
                slot,
                locked,
                &other_slots,
                &win_entity,
                window,
                cx,
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn build_trackbar_context_popup(
    menu: PopupMenu,
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    locked: bool,
    other_slots: &[InspectorParameterSlot],
    win: &Entity<SeaEditorWindow>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let cid_reset = clip_id.clone();
    let cid_flat = clip_id.clone();
    let cid_expr = clip_id.clone();
    let cid_copy = clip_id.clone();
    let w_reset = win.clone();
    let w_flat = win.clone();
    let w_expr = win.clone();
    let w_copy = win.clone();
    let targets = other_slots.to_vec();

    menu.item(
        PopupMenuItem::new("既定値に戻す")
            .disabled(locked)
            .on_click(move |_, window, cx| {
                w_reset.update(cx, |this, cx| {
                    let _ = this.controller.reset_clip_slot_to_default(&cid_reset, slot);
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                });
            }),
    )
    .item(
        PopupMenuItem::new("全区間の値を同じにする")
            .disabled(locked)
            .on_click(move |_, window, cx| {
                w_flat.update(cx, |this, cx| {
                    let _ = this
                        .controller
                        .flatten_clip_slot_to_constant(&cid_flat, slot);
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                });
            }),
    )
    .item(
        PopupMenuItem::new("数式 f(t) で編集...")
            .disabled(locked)
            .on_click(move |_, window, cx| {
                w_expr.update(cx, |this, cx| {
                    let _ = this
                        .controller
                        .activate_slot_expression_editor(&cid_expr, slot);
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                });
            }),
    )
    .separator()
    .submenu(
        "他スロットへカーブをコピー",
        window,
        cx,
        move |mut sub, _, _| {
            if targets.is_empty() {
                return sub.item(PopupMenuItem::new("コピー先なし").disabled(true));
            }
            for &target_slot in &targets {
                let cid_c = cid_copy.clone();
                let w_c = w_copy.clone();
                sub = sub.item(
                    PopupMenuItem::new(target_slot.label())
                        .disabled(locked)
                        .on_click(move |_, window, cx| {
                            w_c.update(cx, |this, cx| {
                                let _ =
                                    this.controller
                                        .copy_clip_slot_curve(&cid_c, slot, target_slot);
                                this.sync_sliders_now(window, cx);
                                cx.notify();
                            });
                        }),
                );
            }
            sub
        },
    )
}

#[allow(clippy::cast_precision_loss)]
fn render_wide_trackbar_line(
    controller: &AppController,
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let slot = view.slot;
    let is_constant = view.mode == InspectorMode::Constant;
    let row_key = row_base_id(slot);

    h_flex()
        .items_center()
        .gap_1()
        .child(
            Button::new(("tb-sel", row_key))
                .ghost()
                .small()
                .label(slot.short_label())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.controller.inspector.active_slot = slot;
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        )
        .child(render_endpoint_control(
            controller, clip, view, bank, false, theme, cx,
        ))
        .child(render_micro_nudge_pair(
            controller, &clip.id, view, false, cx,
        ))
        .child(render_movement_mode_button(clip, view, theme, cx))
        .child(
            div()
                .opacity(if is_constant { 0.72 } else { 1.0 })
                .child(render_endpoint_control(
                    controller, clip, view, bank, true, theme, cx,
                )),
        )
        .children(scale_lock_button(slot, controller.scale_aspect_locked, cx))
}

fn render_compact_top_line(
    controller: &AppController,
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let slot = view.slot;
    let row_key = row_base_id(slot);
    let unit = slot.ui_unit_label();
    let title = if unit.is_empty() {
        slot.short_label().to_owned()
    } else {
        format!("{}({unit})", slot.short_label())
    };

    h_flex()
        .items_center()
        .justify_between()
        .gap_1()
        .child(
            Button::new(("tb-csel", row_key))
                .ghost()
                .small()
                .label(title)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.controller.inspector.active_slot = slot;
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(render_micro_nudge_pair(
                    controller, &clip.id, view, false, cx,
                ))
                .child(render_movement_mode_button(clip, view, theme, cx))
                .children(scale_lock_button(slot, controller.scale_aspect_locked, cx)),
        )
}

fn render_compact_bottom_line(
    controller: &AppController,
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let is_constant = view.mode == InspectorMode::Constant;
    h_flex()
        .items_center()
        .gap_1p5()
        .child(div().flex_1().child(render_endpoint_control(
            controller, clip, view, bank, false, theme, cx,
        )))
        .child(
            div()
                .flex_1()
                .opacity(if is_constant { 0.65 } else { 1.0 })
                .child(render_endpoint_control(
                    controller, clip, view, bank, true, theme, cx,
                )),
        )
}

#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
fn render_endpoint_control(
    controller: &AppController,
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    bank: &InspectorWidgetBank,
    is_end: bool,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let slot = view.slot;
    let internal = if is_end {
        view.end_value
    } else {
        view.start_value
    };
    let ui_val = slot.internal_to_ui(internal);
    let (ui_min, ui_max, _) = slot.ui_bounds();
    let frac = ((ui_val - ui_min) / (ui_max - ui_min).max(1e-6)).clamp(0.0, 1.0) as f32;
    let cid = clip.id.clone();
    let seg_idx = view.segment_index;
    let locked = controller.scale_aspect_locked;
    let bar_id = row_base_id(slot) * 2 + usize::from(is_end);

    let slider_strip = div()
        .id(("tb-drag", bar_id))
        .flex_1()
        .min_w(px(36.0))
        .h(px(14.0))
        .relative()
        .rounded_xs()
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .cursor_ew_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                let px_x: f32 = ev.position.x / px(1.0);
                this.controller.begin_trackbar_drag(TrackBarDragStart {
                    clip_id: cid.clone(),
                    slot,
                    segment_index: seg_idx,
                    is_end,
                    pointer_x_px: px_x,
                    track_width_px: 120.0,
                });
                this.controller.inspector.active_slot = slot;
                let _ = locked;
                cx.notify();
            }),
        )
        .child(div().h_full().w(relative(frac)).rounded_xs().bg(
            if view.mode == InspectorMode::Constant && is_end {
                rgba(0x4755_6988)
            } else {
                rgba(0x3b82_f699)
            },
        ));

    let input_box = div()
        .w(px(52.0))
        .flex_shrink_0()
        .children(bank.track_input(slot, is_end).map(Input::new));

    h_flex()
        .items_center()
        .gap_1()
        .child(input_box)
        .child(slider_strip)
        .into_any_element()
}

fn render_micro_nudge_pair(
    _controller: &AppController,
    clip_id: &ClipId,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    is_end: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let slot = view.slot;
    let seg_idx = view.segment_index;
    let base_id = row_base_id(slot) * 4 + usize::from(is_end);
    let cid_dec = clip_id.clone();
    let cid_inc = clip_id.clone();

    h_flex()
        .gap_0p5()
        .child(
            Button::new(("tb-dec", base_id))
                .ghost()
                .small()
                .label("<")
                .on_click(
                    cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                        apply_micro_nudge(
                            this,
                            &cid_dec,
                            slot,
                            seg_idx,
                            is_end,
                            -1.0,
                            ev.modifiers().shift,
                            window,
                            cx,
                        );
                    }),
                ),
        )
        .child(
            Button::new(("tb-inc", base_id))
                .ghost()
                .small()
                .label(">")
                .on_click(
                    cx.listener(move |this, ev: &gpui_kit::ClickEvent, window, cx| {
                        apply_micro_nudge(
                            this,
                            &cid_inc,
                            slot,
                            seg_idx,
                            is_end,
                            1.0,
                            ev.modifiers().shift,
                            window,
                            cx,
                        );
                    }),
                ),
        )
}

#[allow(clippy::too_many_arguments)]
fn apply_micro_nudge(
    win: &mut SeaEditorWindow,
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    segment_index: usize,
    is_end: bool,
    sign: f64,
    shift: bool,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let Some((_, clip)) = win.controller.document.find_clip(clip_id) else {
        return;
    };
    let clip_owned = clip.clone();
    let doc = &win.controller.document;
    let Some(view) = read_trackbar_segment_view(doc, &clip_owned, slot, segment_index) else {
        return;
    };
    let current_ui = slot.internal_to_ui(if is_end {
        view.end_value
    } else {
        view.start_value
    });
    let (ui_min, ui_max, ui_step) = slot.ui_bounds();
    let delta = ui_step * sign * if shift { 0.1 } else { 1.0 };
    let next_ui = (current_ui + delta).clamp(ui_min, ui_max);
    let (b_min, b_max, _) = slot.bounds();
    let next_internal = slot.ui_to_internal(next_ui).clamp(b_min, b_max);
    let edit = SegmentEndpointEdit {
        slot,
        segment_index,
        is_end,
        new_value: next_internal,
        scale_aspect_locked: win.controller.scale_aspect_locked,
        aspect_ratio_override: None,
    };
    if let Ok(cmd) = build_set_segment_endpoint_with_lock(doc, &clip_owned, &edit)
        && win.controller.dispatch_command(&cmd).is_ok()
    {
        win.controller.inspector.active_slot = slot;
        win.controller.inspector.invalidate_sync();
        win.sync_sliders_now(window, cx);
    }
    cx.notify();
}

fn render_movement_mode_button(
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let slot = view.slot;
    let key = row_base_id(slot);
    let is_animated = view.mode != InspectorMode::Constant;
    let label = format_mode_button_label(view);
    let cid = clip.id.clone();
    let active_preset = view.active_preset;
    let is_mixed = view.is_mixed_segments;
    let win_entity = cx.entity().clone();

    let mut btn = Button::new(("tb-mode", key)).small().label(label);
    btn = if is_animated {
        btn.primary()
    } else {
        btn.ghost()
    };
    let _ = theme;

    btn.dropdown_menu(move |mut menu: PopupMenu, _window, _cx| {
        for &preset in MovementModePreset::all() {
            let checked = active_preset == Some(preset) && !is_mixed;
            let ent = win_entity.clone();
            let cid_opt = cid.clone();
            menu = menu.item(
                PopupMenuItem::new(preset.label())
                    .checked(checked)
                    .on_click(move |_, window, cx| {
                        ent.update(cx, |this, cx| {
                            apply_preset_selection(this, &cid_opt, slot, preset, window, cx);
                        });
                    }),
            );
        }
        let ent_reset = win_entity.clone();
        let cid_reset = cid.clone();
        menu = menu
            .separator()
            .item(
                PopupMenuItem::new("この項目をリセット").on_click(move |_, window, cx| {
                    ent_reset.update(cx, |this, cx| {
                        if let Some((_, c)) = this.controller.document.find_clip(&cid_reset) {
                            let c_owned = c.clone();
                            let cmd = set_animated_command(
                                &c_owned,
                                slot,
                                crate::core::animation::AnimatedParameter::constant(
                                    slot.default_internal_value(),
                                ),
                            );
                            let _ = this.controller.dispatch_command(&cmd);
                            this.controller.inspector.invalidate_sync();
                            this.sync_sliders_now(window, cx);
                            cx.notify();
                        }
                    });
                }),
            );
        menu
    })
}

fn apply_preset_selection(
    win: &mut SeaEditorWindow,
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    preset: MovementModePreset,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let Some((_, clip)) = win.controller.document.find_clip(clip_id) else {
        return;
    };
    let clip_owned = clip.clone();
    let head = win.controller.clock.current_frame();
    let (x1, y1, x2, y2) = win.controller.inspector.bezier_draft;
    let edit = MovementModeEdit {
        slot,
        preset,
        playhead: head,
        bezier_curve: EasingCurve::CubicBezier { x1, y1, x2, y2 },
        scale_aspect_locked: win.controller.scale_aspect_locked,
    };
    if let Ok(cmd) =
        build_apply_movement_mode_with_options(&win.controller.document, &clip_owned, &edit)
        && win.controller.dispatch_command(&cmd).is_ok()
    {
        win.controller.inspector.active_slot = slot;
        if preset == MovementModePreset::Expression {
            win.controller
                .inspector_sections
                .insert("expression".to_owned(), true);
        }
        win.controller.inspector.invalidate_sync();
        win.sync_sliders_now(window, cx);
    }
    cx.notify();
}

fn scale_lock_button(
    slot: InspectorParameterSlot,
    locked: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<impl IntoElement> {
    if slot != InspectorParameterSlot::ScaleX {
        return None;
    }
    let mut btn =
        Button::new("scale-aspect-lock")
            .small()
            .label(if locked { "🔗" } else { "×" });
    btn = if locked { btn.primary() } else { btn.ghost() };
    Some(btn.on_click(cx.listener(|this, _, _, cx| {
        this.controller.scale_aspect_locked = !this.controller.scale_aspect_locked;
        cx.notify();
    })))
}

fn render_inline_bezier_row(
    clip: &Clip,
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
    bank: &InspectorWidgetBank,
    theme: &Theme,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let cid = clip.id.clone();
    let slot = view.slot;
    h_flex()
        .items_center()
        .gap_1()
        .px_1()
        .py_0p5()
        .rounded_xs()
        .bg(theme.secondary)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("曲線:"),
        )
        .children(
            ["x1", "y1", "x2", "y2"]
                .iter()
                .zip(bank.bezier_inputs.iter())
                .map(|(lbl, input)| {
                    h_flex()
                        .items_center()
                        .gap_0p5()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(*lbl),
                        )
                        .child(div().w(px(42.0)).child(Input::new(input)))
                }),
        )
        .child(
            Button::new("apply-bezier-btn")
                .ghost()
                .small()
                .label("適用")
                .on_click(cx.listener(move |this, _, window, cx| {
                    apply_bezier_inputs(this, &cid, slot, window, cx);
                })),
        )
}

fn apply_bezier_inputs(
    win: &mut SeaEditorWindow,
    clip_id: &ClipId,
    slot: InspectorParameterSlot,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let vals: Vec<f64> = win
        .inspector_widgets
        .bezier_inputs
        .iter()
        .map(|i| i.read(cx).value().trim().parse::<f64>().unwrap_or(0.5))
        .collect();
    if vals.len() != 4 {
        return;
    }
    let curve = EasingCurve::CubicBezier {
        x1: vals[0].clamp(0.0, 1.0),
        y1: vals[1].clamp(-2.0, 2.0),
        x2: vals[2].clamp(0.0, 1.0),
        y2: vals[3].clamp(-2.0, 2.0),
    };
    win.controller.inspector.bezier_draft = (
        vals[0].clamp(0.0, 1.0),
        vals[1].clamp(-2.0, 2.0),
        vals[2].clamp(0.0, 1.0),
        vals[3].clamp(-2.0, 2.0),
    );
    if let Some((_, clip)) = win.controller.document.find_clip(clip_id) {
        let clip_owned = clip.clone();
        let head = win.controller.clock.current_frame();
        if let Ok(cmd) = build_apply_bezier_curve_command(
            &win.controller.document,
            &clip_owned,
            slot,
            curve,
            head,
        ) {
            let _ = win.controller.dispatch_command(&cmd);
            win.controller.inspector.invalidate_sync();
            win.sync_sliders_now(window, cx);
        }
    }
    cx.notify();
}

fn build_track_input_map(
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
    subscriptions: &mut Vec<Subscription>,
) -> HashMap<(InspectorParameterSlot, bool), Entity<InputState>> {
    let mut map = HashMap::with_capacity(InspectorParameterSlot::ALL.len() * 2);
    for &slot in InspectorParameterSlot::ALL {
        for is_end in [false, true] {
            let entity = cx.new(|cx| InputState::new(window, cx).default_value("0"));
            subscriptions.push(subscribe_track_input(&entity, slot, is_end, window, cx));
            map.insert((slot, is_end), entity);
        }
    }
    map
}

fn build_bezier_inputs(
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
    subscriptions: &mut Vec<Subscription>,
) -> [Entity<InputState>; 4] {
    ["0.42", "0.00", "0.58", "1.00"].map(|def| {
        let e = cx.new(|cx| InputState::new(window, cx).default_value(def));
        subscriptions.push(subscribe_bezier_input(&e, window, cx));
        e
    })
}

fn subscribe_track_input(
    entity: &Entity<InputState>,
    slot: InspectorParameterSlot,
    is_end: bool,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(
        entity,
        window,
        move |this, input, ev: &InputEvent, window, cx| {
            if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                return;
            }
            if this.syncing_widgets {
                return;
            }
            let Ok(ui_val) = input.read(cx).value().trim().parse::<f64>() else {
                this.sync_sliders_now(window, cx);
                return;
            };
            if !ui_val.is_finite() {
                return;
            }
            let Some((_, clip)) = this.controller.selected_clip() else {
                return;
            };
            let clip_owned = clip.clone();
            let seg_idx = this.controller.selected_segment_index.unwrap_or(0);
            let Some(view) =
                read_trackbar_segment_view(&this.controller.document, &clip_owned, slot, seg_idx)
            else {
                return;
            };
            let internal_val = slot.ui_to_internal(ui_val);
            let current_internal = if is_end {
                view.end_value
            } else {
                view.start_value
            };
            if (internal_val - current_internal).abs() <= 1e-5
                && !(is_end && view.mode == InspectorMode::Constant)
            {
                return;
            }
            let edit = SegmentEndpointEdit {
                slot,
                segment_index: seg_idx,
                is_end,
                new_value: internal_val,
                scale_aspect_locked: this.controller.scale_aspect_locked,
                aspect_ratio_override: None,
            };
            if let Ok(cmd) =
                build_set_segment_endpoint_with_lock(&this.controller.document, &clip_owned, &edit)
                && this.controller.dispatch_command(&cmd).is_ok()
            {
                this.controller.inspector.active_slot = slot;
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
            }
            cx.notify();
        },
    )
}

fn subscribe_bezier_input(
    entity: &Entity<InputState>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(entity, window, |this, _, ev: &InputEvent, window, cx| {
        if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) || this.syncing_widgets {
            return;
        }
        let Some((_, clip)) = this.controller.selected_clip() else {
            return;
        };
        let cid = clip.id.clone();
        let slot = this.controller.inspector.active_slot;
        apply_bezier_inputs(this, &cid, slot, window, cx);
    })
}

fn subscribe_subtitle_textarea(
    entity: &Entity<TextareaState>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(entity, window, |this, ta, ev: &InputEvent, _, cx| {
        if this.syncing_widgets {
            return;
        }
        let raw = ta.read(cx).value().to_string();
        match ev {
            InputEvent::Change => {
                this.controller.inspector.subtitle_text_draft = raw;
            }
            InputEvent::Blur => {
                if let Some((_, clip)) = this.controller.selected_clip()
                    && let ClipKind::Subtitle(sub) = &clip.kind
                    && sub.text != raw
                {
                    let cmd = EditCommand::SetParameter {
                        target: ParameterTarget::Clip {
                            clip_id: ClipRef::Id(clip.id.clone()),
                        },
                        path: ParameterPath::SubtitleText,
                        value: ParameterValue::Text { value: raw },
                    };
                    let _ = this.controller.dispatch_command(&cmd);
                    this.controller.inspector.invalidate_sync();
                    cx.notify();
                }
            }
            _ => {}
        }
    })
}

fn subscribe_font_family_select(
    entity: &Entity<SelectState<SearchableVec<FontSelectItem>>>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(
        entity,
        window,
        |this, _, ev: &SelectEvent<SearchableVec<FontSelectItem>>, _, cx| {
            if this.syncing_widgets {
                return;
            }
            let SelectEvent::Confirm(Some(family)) = ev else {
                return;
            };
            let is_local = list_available_font_families()
                .iter()
                .any(|e| e.family.eq_ignore_ascii_case(family) && e.is_local);
            let target_clip_id = this
                .controller
                .selected_clip()
                .and_then(|(_, c)| matches!(c.kind, ClipKind::Subtitle(_)).then(|| c.id.clone()));
            if let Some(cid) = target_clip_id.as_ref() {
                apply_subtitle_font_family_command(this, cid, family);
            }
            if is_local {
                this.mark_sliders_dirty();
            } else {
                spawn_google_font_install(family, target_clip_id, cx);
            }
            cx.notify();
        },
    )
}

/// 字幕クリップへフォントファミリ名変更コマンドを適用する。
pub(crate) fn apply_subtitle_font_family_command(
    this: &mut SeaEditorWindow,
    clip_id: &ClipId,
    family: &str,
) {
    let trimmed = family.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some((_, clip)) = this.controller.document.find_clip(clip_id)
        && let ClipKind::Subtitle(sub) = &clip.kind
        && sub.font_family != trimmed
    {
        let cmd = EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(clip_id.clone()),
            },
            path: ParameterPath::SubtitleFontFamily,
            value: ParameterValue::Text {
                value: trimmed.to_owned(),
            },
        };
        let _ = this.controller.dispatch_command(&cmd);
        this.controller.inspector.invalidate_sync();
    }
}

/// ユーザーの明示指示に基づき Google Fonts から TTF/OTF を非同期取得し字幕レンダラへ登録する。
pub(crate) fn spawn_google_font_install(
    family: &str,
    clip_id: Option<ClipId>,
    cx: &mut Context<SeaEditorWindow>,
) {
    let trimmed = family.trim().to_owned();
    if trimmed.is_empty() {
        return;
    }
    cx.entity().update(cx, |this, cx| {
        this.controller.status_message = format!("Google Fonts から '{trimmed}' を取得中...");
        cx.notify();
    });
    cx.spawn(async move |this, cx| {
        let fam_bg = trimmed.clone();
        let res = cx
            .background_executor()
            .spawn(async move { install_google_font_for_subtitles(&fam_bg) })
            .await;
        let _ = this.update(cx, |view, cx| {
            match res {
                Ok(downloaded) => {
                    if let Some(cid) = clip_id {
                        apply_subtitle_font_family_command(view, &cid, &downloaded.family);
                    }
                    view.controller.invalidate_and_refresh_preview();
                    view.font_select_needs_refresh = true;
                    view.mark_sliders_dirty();
                    view.controller.status_message = format!(
                        "Google Font '{}' を導入・適用しました ({} files)",
                        downloaded.family,
                        downloaded.saved_paths.len()
                    );
                }
                Err(err) => {
                    view.controller.status_message =
                        format!("Google Font 取得失敗 ({trimmed}): {err}");
                }
            }
            cx.notify();
        });
    })
    .detach();
}

/// ユーザーの検索操作に応じて Google Fonts メタデータを非同期検索しコンボボックス候補を更新する。
pub(crate) fn spawn_google_font_search(query: &str, cx: &mut Context<SeaEditorWindow>) {
    let trimmed = query.trim().to_owned();
    cx.entity().update(cx, |this, cx| {
        this.controller.status_message = format!("Google Fonts を検索中: '{trimmed}'...");
        cx.notify();
    });
    cx.spawn(async move |this, cx| {
        let q_bg = trimmed.clone();
        let res = cx
            .background_executor()
            .spawn(async move { fetch_google_fonts_catalog(&q_bg, 25) })
            .await;
        let _ = this.update(cx, |view, cx| {
            match res {
                Ok(families) => {
                    let count = families.len();
                    view.extra_google_font_results = families;
                    view.font_select_needs_refresh = true;
                    view.mark_sliders_dirty();
                    view.controller.status_message =
                        format!("Google Fonts 検索結果: {count} 件をフォント一覧に追加しました");
                }
                Err(err) => {
                    view.controller.status_message =
                        format!("Google Fonts 検索失敗 (オフラインまたは接続不可): {err}");
                }
            }
            cx.notify();
        });
    })
    .detach();
}

fn subscribe_font_family_input(
    entity: &Entity<InputState>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(entity, window, |this, input, ev: &InputEvent, _, cx| {
        if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) || this.syncing_widgets {
            return;
        }
        let raw = input.read(cx).value().trim().to_owned();
        if raw.is_empty() {
            return;
        }
        if let Some((_, clip)) = this.controller.selected_clip()
            && let ClipKind::Subtitle(_) = &clip.kind
        {
            let cid = clip.id.clone();
            apply_subtitle_font_family_command(this, &cid, &raw);
            this.font_select_needs_refresh = true;
            this.mark_sliders_dirty();
            cx.notify();
        }
    })
}

fn subscribe_outline_width_input(
    entity: &Entity<InputState>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(entity, window, |this, input, ev: &InputEvent, _, cx| {
        if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) || this.syncing_widgets {
            return;
        }
        let Ok(val) = input.read(cx).value().trim().parse::<f64>() else {
            return;
        };
        if !val.is_finite() || val < 0.0 {
            return;
        }
        if let Some((_, clip)) = this.controller.selected_clip()
            && let ClipKind::Subtitle(sub) = &clip.kind
            && (f64::from(sub.outline_width) - val).abs() > 1e-3
        {
            let cmd = EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: ParameterPath::SubtitleOutlineWidth,
                value: ParameterValue::Number {
                    value: val.clamp(0.0, 64.0),
                },
            };
            let _ = this.controller.dispatch_command(&cmd);
            this.controller.inspector.invalidate_sync();
            cx.notify();
        }
    })
}

fn subscribe_color_picker(
    entity: &Entity<ColorPickerState>,
    path: ParameterPath,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(
        entity,
        window,
        move |this, _, ev: &ColorPickerEvent, _, cx| {
            if this.syncing_widgets {
                return;
            }
            let ColorPickerEvent::Change(Some(hsla)) = ev else {
                return;
            };
            let next_color = hsla_to_rgba(*hsla);
            let Some((_, clip)) = this.controller.selected_clip() else {
                return;
            };
            let ClipKind::Subtitle(sub) = &clip.kind else {
                return;
            };
            let current = if path == ParameterPath::SubtitleTextColor {
                sub.text_color
            } else {
                sub.outline_color
            };
            if current == next_color {
                return;
            }
            let cmd = EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(clip.id.clone()),
                },
                path: path.clone(),
                value: ParameterValue::Color { value: next_color },
            };
            let _ = this.controller.dispatch_command(&cmd);
            cx.notify();
        },
    )
}

fn subscribe_expr_input(
    entity: &Entity<InputState>,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(
        entity,
        window,
        |this, input, ev: &InputEvent, window, cx| {
            if this.syncing_widgets {
                return;
            }
            let raw = input.read(cx).value().to_string();
            match ev {
                InputEvent::Change => {
                    this.controller.inspector.expr_draft.clone_from(&raw);
                    let off = this.controller.inspector.expr_base_offset;
                    let scl = this.controller.inspector.expr_scale;
                    this.controller.inspector.expr_error =
                        validate_expression_and_tuning(&raw, off, scl).err();
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.controller.inspector.expr_draft.clone_from(&raw);
                    let off = this.controller.inspector.expr_base_offset;
                    let scl = this.controller.inspector.expr_scale;
                    let slot = this.controller.inspector.active_slot;
                    if let Some((_, clip)) = this.controller.selected_clip() {
                        let clip_owned = clip.clone();
                        match build_expression_command(&clip_owned, slot, &raw, off, scl) {
                            Ok(cmd) => {
                                this.controller.inspector.expr_error = None;
                                let _ = this.controller.dispatch_command(&cmd);
                                this.controller.inspector.invalidate_sync();
                                this.sync_sliders_now(window, cx);
                            }
                            Err(err) => this.controller.inspector.expr_error = Some(err),
                        }
                        cx.notify();
                    }
                }
                _ => {}
            }
        },
    )
}

fn subscribe_expr_tuning_input(
    entity: &Entity<InputState>,
    is_offset: bool,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Subscription {
    cx.subscribe_in(
        entity,
        window,
        move |this, input, ev: &InputEvent, window, cx| {
            if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur)
                || this.syncing_widgets
            {
                return;
            }
            let Ok(val) = input.read(cx).value().trim().parse::<f64>() else {
                return;
            };
            if !val.is_finite() {
                return;
            }
            if is_offset {
                this.controller.inspector.expr_base_offset = val;
            } else {
                this.controller.inspector.expr_scale = val;
            }
            let slot = this.controller.inspector.active_slot;
            let off = this.controller.inspector.expr_base_offset;
            let scl = this.controller.inspector.expr_scale;
            let raw = this
                .inspector_widgets
                .expr_input
                .read(cx)
                .value()
                .to_string();
            if let Some((_, clip)) = this.controller.selected_clip() {
                let clip_owned = clip.clone();
                if let Ok(cmd) = build_expression_command(&clip_owned, slot, &raw, off, scl) {
                    let _ = this.controller.dispatch_command(&cmd);
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                }
            }
        },
    )
}

fn format_mode_button_label(
    view: &crate::ui::inspector::segment_ops::TrackBarSegmentView,
) -> String {
    if view.is_mixed_segments {
        return "[混在]".to_owned();
    }
    let base = view
        .active_preset
        .map_or("直線", MovementModePreset::short_badge);
    if view.is_cut_easing {
        format!("{base}[継続]")
    } else if view.unaligned_keyframes {
        format!("{base}*")
    } else {
        base.to_owned()
    }
}

fn format_ui_value(slot: InspectorParameterSlot, ui_val: f64) -> String {
    match slot {
        InspectorParameterSlot::PositionX
        | InspectorParameterSlot::PositionY
        | InspectorParameterSlot::AnchorX
        | InspectorParameterSlot::AnchorY
        | InspectorParameterSlot::ScaleX
        | InspectorParameterSlot::ScaleY
        | InspectorParameterSlot::Opacity
        | InspectorParameterSlot::AudioPan
        | InspectorParameterSlot::SubtitleFontSize
        | InspectorParameterSlot::RotationDeg
        | InspectorParameterSlot::HueDeg
        | InspectorParameterSlot::AudioVolume => format!("{ui_val:.1}"),
        InspectorParameterSlot::Brightness
        | InspectorParameterSlot::Contrast
        | InspectorParameterSlot::Saturation
        | InspectorParameterSlot::Gamma => format!("{ui_val:.2}"),
    }
}

const fn row_base_id(slot: InspectorParameterSlot) -> usize {
    match slot {
        InspectorParameterSlot::PositionX => 0,
        InspectorParameterSlot::PositionY => 1,
        InspectorParameterSlot::ScaleX => 2,
        InspectorParameterSlot::ScaleY => 3,
        InspectorParameterSlot::RotationDeg => 4,
        InspectorParameterSlot::AnchorX => 5,
        InspectorParameterSlot::AnchorY => 6,
        InspectorParameterSlot::Opacity => 7,
        InspectorParameterSlot::Brightness => 8,
        InspectorParameterSlot::Contrast => 9,
        InspectorParameterSlot::Saturation => 10,
        InspectorParameterSlot::HueDeg => 11,
        InspectorParameterSlot::Gamma => 12,
        InspectorParameterSlot::SubtitleFontSize => 13,
        InspectorParameterSlot::AudioVolume => 14,
        InspectorParameterSlot::AudioPan => 15,
    }
}

/// `RgbaColor` を `Hsla` へ変換する。
#[must_use]
pub fn rgba_to_hsla(c: RgbaColor) -> Hsla {
    Rgba {
        r: f32::from(c.red) / 255.0,
        g: f32::from(c.green) / 255.0,
        b: f32::from(c.blue) / 255.0,
        a: f32::from(c.alpha) / 255.0,
    }
    .into()
}

/// `Hsla` を `RgbaColor` へ変換する。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn hsla_to_rgba(h: Hsla) -> RgbaColor {
    let rgb = h.to_rgb();
    RgbaColor::new(
        (rgb.r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb.g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb.b.clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}
