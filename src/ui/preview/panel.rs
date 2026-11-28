//! プレビューパネル描画（28px ヘッダ・インタラクティブキャンバス・34px マスター音声波形・36px トランスポート）。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::{Disableable as _, StyledExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, ThemeMode, h_flex, v_flex};
use gpui_kit::{
    AnyElement, BorderStyle, Bounds, ContentMask, Context, Corners, Edges, Entity,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _,
    PathBuilder, Pixels, RenderImage, Rgba, ScrollDelta, ScrollEvent, Styled as _, Window, canvas,
    div, fill, point, px, quad, rgba, size,
};

use crate::core::ids::ClipId;
use crate::core::time::FrameIndex;
use crate::ui::app_controller::{AppController, PreviewZoomMode};
use crate::ui::i18n::tr;
use crate::ui::preview::canvas_ops::{
    PREVIEW_GRID_STEP_PX, PreviewHandleKind, PreviewHoverKind, PreviewHoverState,
    PreviewLayerHitQuad, PreviewViewportTransform, ResizeCursorAxis, build_preview_hit_quads,
    find_missing_asset_clip_at_playhead, handle_screen_positions, step_zoom_preset,
    zoom_around_cursor,
};
use crate::ui::preview::master_waveform::sample_waveform_bars_for_timeline;
use crate::ui::timeline::format_timeline_timestamp;
use crate::ui::window::SeaEditorWindow;

/// プレビュー上部ヘッダ高さ (px)。
const PREVIEW_HEADER_HEIGHT_PX: f32 = 28.0;
/// マスター音声波形ストリップ高さ (px)。
const MASTER_WAVEFORM_HEIGHT_PX: f32 = 34.0;
/// 下部トランスポートバー高さ (px)。
const TRANSPORT_BAR_HEIGHT_PX: f32 = 36.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PreviewPalette {
    pub(crate) backdrop_bg: Rgba,
    pub(crate) canvas_inner_bg: Rgba,
    pub(crate) canvas_border: Rgba,
}

pub(crate) fn preview_palette(is_dark: bool) -> PreviewPalette {
    if is_dark {
        PreviewPalette {
            backdrop_bg: rgba(0x2b30_3bff),
            canvas_inner_bg: rgba(0x0000_00ff),
            canvas_border: rgba(0x6474_8bcc),
        }
    } else {
        PreviewPalette {
            backdrop_bg: rgba(0xd4d8_e0ff),
            canvas_inner_bg: rgba(0x0000_00ff),
            canvas_border: rgba(0x4755_69cc),
        }
    }
}

pub(crate) fn outer_canvas_border_bounds(canvas_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            canvas_bounds.origin.x - px(1.0),
            canvas_bounds.origin.y - px(1.0),
        ),
        size: size(
            canvas_bounds.size.width + px(2.0),
            canvas_bounds.size.height + px(2.0),
        ),
    }
}

#[derive(Clone)]
struct PreviewCanvasDrawPayload {
    bounds_cell: Rc<Cell<(f32, f32, f32, f32)>>,
    proj_size: (u32, u32),
    zoom: PreviewZoomMode,
    pan_px: (f32, f32),
    image: Option<Arc<RenderImage>>,
    is_dark: bool,
    show_grid: bool,
    show_guides: bool,
    selected_ids: Vec<ClipId>,
    primary_clip_id: Option<ClipId>,
    hovered_handle: Option<PreviewHandleKind>,
    quads: Vec<PreviewQuadTemplate>,
}

#[derive(Clone)]
struct PreviewQuadTemplate {
    clip_id: ClipId,
    layer_id: crate::core::ids::LayerId,
    is_locked: bool,
    canvas_corners: [(f64, f64); 4],
    anchor_canvas: (f64, f64),
}

/// プレビューパネル全体を構築する（ヘッダ + 直接操作キャンバス + 任意波形 + トランスポート）。
pub fn render_preview_panel(
    controller: &mut AppController,
    scrub_slider: &Entity<SliderState>,
    space_pan_held: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let playhead = controller.clock.current_frame();
    let total_frames = controller.document.total_duration_frames().max(1);
    let fps_f64 = controller.document.fps.as_f64();
    let waveform_bars = if controller.show_preview_waveform {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let est_w = controller.preview_viewport_bounds.get().2.max(160.0) as usize;
        let bar_count = (est_w / 3).clamp(64, 360);
        sample_waveform_bars_for_timeline(
            &mut controller.master_waveform,
            total_frames,
            fps_f64,
            bar_count,
        )
    } else {
        Vec::new()
    };

    let is_dark = matches!(cx.theme().mode, ThemeMode::Dark);
    let palette = preview_palette(is_dark);
    let header = render_preview_header(controller, cx);
    let viewport = render_preview_viewport(controller, playhead, space_pan_held, cx);
    let transport = render_transport_bar(controller, scrub_slider, cx);

    let mut root = v_flex()
        .size_full()
        .min_h(px(160.0))
        .bg(palette.backdrop_bg)
        .child(header)
        .child(viewport);

    if controller.show_preview_waveform {
        root = root.child(render_master_waveform_strip(controller, &waveform_bars, cx));
    }

    root.child(transport)
}

fn render_preview_header(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let left_group = render_preview_zoom_group(controller, cx);
    let res_label = format!(
        "{}x{}",
        controller.document.resolution.width.get(),
        controller.document.resolution.height.get()
    );

    let right_group = h_flex()
        .gap_1()
        .items_center()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgba(0x94a3_b8ff))
                .pr_1()
                .child(res_label),
        )
        .child(render_header_toggle(
            "toggle-preview-guides",
            tr("preview.guides"),
            controller.show_preview_guides,
            cx.listener(|this, _, window, cx| {
                this.controller.show_preview_guides = !this.controller.show_preview_guides;
                this.reload_app_menus(cx);
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            }),
        ))
        .child(render_header_toggle(
            "toggle-preview-grid",
            "#",
            controller.show_preview_grid,
            cx.listener(|this, _, window, cx| {
                this.controller.show_preview_grid = !this.controller.show_preview_grid;
                this.reload_app_menus(cx);
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            }),
        ))
        .child(render_header_toggle(
            "toggle-preview-waveform",
            tr("preview.waveform"),
            controller.show_preview_waveform,
            cx.listener(|this, _, window, cx| {
                this.controller.show_preview_waveform = !this.controller.show_preview_waveform;
                this.reload_app_menus(cx);
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            }),
        ));

    h_flex()
        .h(px(PREVIEW_HEADER_HEIGHT_PX))
        .w_full()
        .px_2()
        .items_center()
        .justify_between()
        .bg(rgba(0x161a_22ff))
        .border_b_1()
        .border_color(rgba(0x2a31_40ff))
        .child(left_group)
        .child(right_group)
        .into_any_element()
}

fn render_preview_zoom_group(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let presets = [
        ("Fit", PreviewZoomMode::Fit),
        ("25%", PreviewZoomMode::Percent(25)),
        ("50%", PreviewZoomMode::Percent(50)),
        ("100%", PreviewZoomMode::Percent(100)),
        ("200%", PreviewZoomMode::Percent(200)),
    ];
    let has_pan = controller.preview_pan_offset_px.0.abs() > 0.5
        || controller.preview_pan_offset_px.1.abs() > 0.5;

    let mut left_group =
        h_flex()
            .gap_1()
            .items_center()
            .children(presets.into_iter().enumerate().map(|(ix, (label, mode))| {
                let active = controller.preview_zoom == mode;
                let mut btn = Button::new(("preview-zoom-preset", ix))
                    .xsmall()
                    .label(label);
                btn = if active { btn.primary() } else { btn.ghost() };
                btn.on_click(cx.listener(move |this, _, window, cx| {
                    this.controller.preview_zoom = mode;
                    if mode == PreviewZoomMode::Fit {
                        this.controller.preview_pan_offset_px = (0.0, 0.0);
                    }
                    window.focus(&this.workspace_focus, cx);
                    cx.notify();
                }))
            }));

    if has_pan {
        left_group = left_group.child(
            Button::new("preview-pan-reset")
                .xsmall()
                .ghost()
                .label(tr("preview.center"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.controller.preview_zoom = PreviewZoomMode::Fit;
                    this.controller.preview_pan_offset_px = (0.0, 0.0);
                    window.focus(&this.workspace_focus, cx);
                    cx.notify();
                })),
        );
    }

    left_group.into_any_element()
}

fn render_header_toggle(
    id: &'static str,
    label: impl Into<gpui_kit::SharedString>,
    active: bool,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> Button {
    let mut btn = Button::new(id).xsmall().label(label);
    btn = if active { btn.primary() } else { btn.ghost() };
    btn.on_click(on_click)
}

fn collect_preview_quad_templates(
    controller: &AppController,
    playhead: FrameIndex,
) -> Vec<PreviewQuadTemplate> {
    let proj_w = controller.document.resolution.width.get().max(1);
    let proj_h = controller.document.resolution.height.get().max(1);
    let nominal_tf = PreviewViewportTransform {
        bounds: controller.preview_viewport_bounds.get(),
        proj_size: (proj_w, proj_h),
        zoom: controller.preview_zoom,
        pan_px: controller.preview_pan_offset_px,
    };
    build_preview_hit_quads(&controller.document, playhead, &nominal_tf)
        .into_iter()
        .map(|q| PreviewQuadTemplate {
            clip_id: q.clip_id,
            layer_id: q.layer_id,
            is_locked: q.is_locked,
            canvas_corners: q.canvas_corners,
            anchor_canvas: q.anchor_canvas,
        })
        .collect()
}

fn render_preview_viewport(
    controller: &AppController,
    playhead: FrameIndex,
    space_pan_held: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let proj_w = controller.document.resolution.width.get().max(1);
    let proj_h = controller.document.resolution.height.get().max(1);
    let quads = collect_preview_quad_templates(controller, playhead);

    let hovered_handle = controller.preview_hover.and_then(|h| match h.kind {
        PreviewHoverKind::Handle { handle, .. } => Some(handle),
        _ => None,
    });
    let is_dark = cx.theme().mode == ThemeMode::Dark;
    let palette = preview_palette(is_dark);
    let payload = PreviewCanvasDrawPayload {
        bounds_cell: Rc::clone(&controller.preview_viewport_bounds),
        proj_size: (proj_w, proj_h),
        zoom: controller.preview_zoom,
        pan_px: controller.preview_pan_offset_px,
        image: controller.surface_bridge.front_image(),
        is_dark,
        show_grid: controller.show_preview_grid,
        show_guides: controller.show_preview_guides,
        selected_ids: controller.effective_selected_clip_ids(),
        primary_clip_id: controller.selected_clip_id.clone(),
        hovered_handle,
        quads,
    };

    let interactive_canvas = canvas(
        move |bounds, _, _| {
            let rect = (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            );
            payload.bounds_cell.set(rect);
            (rect, payload)
        },
        move |_, (rect, data), window, _| {
            paint_preview_viewport_canvas(rect, &data, window);
        },
    )
    .size_full();

    let hover_cursor = cursor_style_for_preview_hover(controller.preview_hover.as_ref());
    let mut container = div()
        .id("preview-viewport")
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(120.0))
        .overflow_hidden()
        .bg(palette.backdrop_bg)
        .cursor(hover_cursor)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.workspace_focus, cx);
                this.controller.handle_preview_left_mouse_down(
                    (f32::from(ev.position.x), f32::from(ev.position.y)),
                    space_pan_held,
                );
                this.mark_sliders_dirty();
                cx.notify();
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.workspace_focus, cx);
                this.controller
                    .begin_preview_pan((f32::from(ev.position.x), f32::from(ev.position.y)));
                cx.notify();
            }),
        )
        .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
            handle_preview_wheel(&mut this.controller, ev);
            cx.notify();
        }))
        .child(interactive_canvas);

    if let Some((asset_id, name)) =
        find_missing_asset_clip_at_playhead(&controller.document, playhead)
    {
        container = container.child(render_media_offline_overlay(asset_id, name, cx));
    }

    if let Some(notice) = controller.preview_notice.as_ref() {
        container = container.child(render_preview_notice_banner(notice.clone(), cx));
    }

    if let Some(hover) = controller.preview_hover {
        container = container.child(render_preview_hover_badge(hover));
    }

    container.into_any_element()
}

fn cursor_style_for_preview_hover(hover: Option<&PreviewHoverState>) -> gpui_kit::CursorStyle {
    match hover.map(|h| h.kind) {
        None => gpui_kit::CursorStyle::Arrow,
        Some(PreviewHoverKind::Pan) => gpui_kit::CursorStyle::PointingHand,
        Some(PreviewHoverKind::MoveClip) => gpui_kit::CursorStyle::Crosshair,
        Some(PreviewHoverKind::LockedClip) => gpui_kit::CursorStyle::OperationNotAllowed,
        Some(PreviewHoverKind::Handle { handle, axis }) => match handle {
            PreviewHandleKind::Anchor | PreviewHandleKind::Move => gpui_kit::CursorStyle::Crosshair,
            PreviewHandleKind::Rotate => gpui_kit::CursorStyle::PointingHand,
            PreviewHandleKind::ScaleTL
            | PreviewHandleKind::ScaleT
            | PreviewHandleKind::ScaleTR
            | PreviewHandleKind::ScaleR
            | PreviewHandleKind::ScaleBR
            | PreviewHandleKind::ScaleB
            | PreviewHandleKind::ScaleBL
            | PreviewHandleKind::ScaleL => match axis {
                ResizeCursorAxis::Horizontal => gpui_kit::CursorStyle::ResizeLeftRight,
                ResizeCursorAxis::Vertical => gpui_kit::CursorStyle::ResizeUpDown,
                ResizeCursorAxis::DiagNwSe => gpui_kit::CursorStyle::ResizeUpLeftDownRight,
                ResizeCursorAxis::DiagNeSw => gpui_kit::CursorStyle::ResizeUpRightDownLeft,
            },
        },
    }
}

fn render_preview_hover_badge(hover: PreviewHoverState) -> AnyElement {
    let key = match hover.kind {
        PreviewHoverKind::Pan => "preview.hover.pan",
        PreviewHoverKind::MoveClip => "preview.hover.move",
        PreviewHoverKind::LockedClip => "preview.hover.locked",
        PreviewHoverKind::Handle { handle, .. } => match handle {
            PreviewHandleKind::Rotate => "preview.hover.rotate",
            PreviewHandleKind::Anchor => "preview.hover.anchor",
            PreviewHandleKind::Move => "preview.hover.move",
            _ => "preview.hover.scale",
        },
    };
    div()
        .absolute()
        .bottom(px(8.0))
        .left(px(8.0))
        .px_2()
        .py_0p5()
        .rounded_sm()
        .bg(rgba(0x0f17_2acc))
        .border_1()
        .border_color(rgba(0x38bd_f866))
        .text_size(px(10.0))
        .text_color(rgba(0xe2e8_f0ff))
        .child(tr(key))
        .into_any_element()
}

fn handle_preview_wheel(controller: &mut AppController, ev: &ScrollEvent) {
    let dy = match ev.delta {
        ScrollDelta::Pixels(pt) => f32::from(pt.y),
        ScrollDelta::Lines(pt) => pt.y * 24.0,
    };
    if dy.abs() < 1e-3 {
        return;
    }
    let bounds = controller.preview_viewport_bounds.get();
    let proj_size = (
        controller.document.resolution.width.get().max(1),
        controller.document.resolution.height.get().max(1),
    );
    let tf = PreviewViewportTransform {
        bounds,
        proj_size,
        zoom: controller.preview_zoom,
        pan_px: controller.preview_pan_offset_px,
    };
    let sign = if dy > 0.0 { 1 } else { -1 };
    let next_zoom = step_zoom_preset(controller.preview_zoom, tf.fit_scale(), sign);
    let cursor = (f32::from(ev.position.x), f32::from(ev.position.y));
    let next_pan = zoom_around_cursor(
        bounds,
        proj_size,
        controller.preview_zoom,
        controller.preview_pan_offset_px,
        next_zoom,
        cursor,
    );
    controller.preview_zoom = next_zoom;
    controller.preview_pan_offset_px = next_pan;
}

fn paint_preview_viewport_canvas(
    rect: (f32, f32, f32, f32),
    data: &PreviewCanvasDrawPayload,
    window: &mut Window,
) {
    let tf = PreviewViewportTransform {
        bounds: rect,
        proj_size: data.proj_size,
        zoom: data.zoom,
        pan_px: data.pan_px,
    };
    let (rx, ry, rw, rh) = tf.rendered_rect();
    let viewport_bounds = Bounds {
        origin: point(px(rect.0), px(rect.1)),
        size: size(px(rect.2), px(rect.3)),
    };
    let canvas_bounds = Bounds {
        origin: point(px(rx), px(ry)),
        size: size(px(rw), px(rh)),
    };
    let palette = preview_palette(data.is_dark);

    window.with_content_mask(
        Some(ContentMask {
            bounds: viewport_bounds,
        }),
        |window| {
            window.paint_quad(fill(canvas_bounds, palette.canvas_inner_bg));

            if let Some(image) = &data.image {
                let _ = window.paint_image(
                    viewport_bounds,
                    canvas_bounds,
                    Corners::all(px(0.0)),
                    Arc::clone(image),
                    0,
                    false,
                );
            }

            // 動画ピクセル端を潰さず、かつ拡大時は viewport_bounds クリップ内に収まるよう外周 1px 枠線を後描画
            window.paint_quad(quad(
                outer_canvas_border_bounds(canvas_bounds),
                Corners::all(px(0.0)),
                rgba(0x0000_0000),
                Edges::all(px(1.0)),
                palette.canvas_border,
                BorderStyle::Solid,
            ));

            if data.show_grid {
                paint_grid_lines(viewport_bounds, &tf, data.proj_size, window);
            }
            if data.show_guides {
                paint_selection_wireframes(&tf, data, window);
            }
        },
    );
}

fn paint_grid_lines(
    clip_bounds: Bounds<Pixels>,
    tf: &PreviewViewportTransform,
    proj_size: (u32, u32),
    window: &mut Window,
) {
    let half_w = f64::from(proj_size.0) * 0.5;
    let half_h = f64::from(proj_size.1) * 0.5;
    let (left, top) = tf.canvas_px_to_screen(-half_w, -half_h);
    let (right, bottom) = tf.canvas_px_to_screen(half_w, half_h);
    let vx0 = left.max(f32::from(clip_bounds.origin.x));
    let vx1 = right.min(f32::from(clip_bounds.origin.x) + f32::from(clip_bounds.size.width));
    let vy0 = top.max(f32::from(clip_bounds.origin.y));
    let vy1 = bottom.min(f32::from(clip_bounds.origin.y) + f32::from(clip_bounds.size.height));
    if vx1 <= vx0 || vy1 <= vy0 {
        return;
    }

    let step_max_x = (half_w / PREVIEW_GRID_STEP_PX).floor();
    #[allow(clippy::cast_possible_truncation)]
    let x_step_limit = step_max_x as i32;
    for ix in -x_step_limit..=x_step_limit {
        let cx = f64::from(ix) * PREVIEW_GRID_STEP_PX;
        let (sx, _) = tf.canvas_px_to_screen(cx, 0.0);
        if sx < vx0 || sx > vx1 {
            continue;
        }
        let color = if ix == 0 {
            rgba(0x38bd_f866)
        } else {
            rgba(0x94a3_b833)
        };
        window.paint_quad(fill(
            Bounds {
                origin: point(px(sx), px(vy0)),
                size: size(px(1.0), px(vy1 - vy0)),
            },
            color,
        ));
    }

    let step_max_y = (half_h / PREVIEW_GRID_STEP_PX).floor();
    #[allow(clippy::cast_possible_truncation)]
    let y_step_limit = step_max_y as i32;
    for iy in -y_step_limit..=y_step_limit {
        let cy = f64::from(iy) * PREVIEW_GRID_STEP_PX;
        let (_, sy) = tf.canvas_px_to_screen(0.0, cy);
        if sy < vy0 || sy > vy1 {
            continue;
        }
        let color = if iy == 0 {
            rgba(0x38bd_f866)
        } else {
            rgba(0x94a3_b833)
        };
        window.paint_quad(fill(
            Bounds {
                origin: point(px(vx0), px(sy)),
                size: size(px(vx1 - vx0), px(1.0)),
            },
            color,
        ));
    }
}

fn paint_selection_wireframes(
    tf: &PreviewViewportTransform,
    data: &PreviewCanvasDrawPayload,
    window: &mut Window,
) {
    for q in &data.quads {
        if !data.selected_ids.iter().any(|id| id == &q.clip_id) {
            continue;
        }
        let sc = [
            tf.canvas_px_to_screen(q.canvas_corners[0].0, q.canvas_corners[0].1),
            tf.canvas_px_to_screen(q.canvas_corners[1].0, q.canvas_corners[1].1),
            tf.canvas_px_to_screen(q.canvas_corners[2].0, q.canvas_corners[2].1),
            tf.canvas_px_to_screen(q.canvas_corners[3].0, q.canvas_corners[3].1),
        ];
        let anchor_sc = tf.canvas_px_to_screen(q.anchor_canvas.0, q.anchor_canvas.1);
        let stroke_color = if q.is_locked {
            rgba(0xf59e_0bcc)
        } else {
            rgba(0x38bd_f8ff)
        };

        let mut outline = PathBuilder::stroke(px(1.5));
        outline.move_to(point(px(sc[0].0), px(sc[0].1)));
        outline.line_to(point(px(sc[1].0), px(sc[1].1)));
        outline.line_to(point(px(sc[2].0), px(sc[2].1)));
        outline.line_to(point(px(sc[3].0), px(sc[3].1)));
        outline.line_to(point(px(sc[0].0), px(sc[0].1)));
        if let Ok(path) = outline.build() {
            window.paint_path(path, stroke_color);
        }

        let is_primary = data.primary_clip_id.as_ref() == Some(&q.clip_id);
        if is_primary && !q.is_locked {
            let synth_quad = build_overlay_hit_quad(q, sc, anchor_sc);
            paint_primary_handles(&synth_quad, data.hovered_handle, window);
        }
    }
}

fn build_overlay_hit_quad(
    tpl: &PreviewQuadTemplate,
    screen_corners: [(f32, f32); 4],
    anchor_screen: (f32, f32),
) -> PreviewLayerHitQuad {
    let top_mid = (
        f32::midpoint(screen_corners[0].0, screen_corners[1].0),
        f32::midpoint(screen_corners[0].1, screen_corners[1].1),
    );
    let bot_mid = (
        f32::midpoint(screen_corners[3].0, screen_corners[2].0),
        f32::midpoint(screen_corners[3].1, screen_corners[2].1),
    );
    let dx = top_mid.0 - bot_mid.0;
    let dy = top_mid.1 - bot_mid.1;
    let len = dx.hypot(dy);
    let (nx, ny) = if len > 1e-3 {
        (dx / len, dy / len)
    } else {
        (0.0, -1.0)
    };
    let rotate_handle_screen = (
        top_mid.0 + nx * super::canvas_ops::ROTATE_HANDLE_OFFSET_PX,
        top_mid.1 + ny * super::canvas_ops::ROTATE_HANDLE_OFFSET_PX,
    );
    PreviewLayerHitQuad {
        clip_id: tpl.clip_id.clone(),
        layer_id: tpl.layer_id.clone(),
        track_index: 0,
        z_index: 0,
        is_locked: tpl.is_locked,
        source_size: (1, 1),
        visual: crate::media::visual_properties::EvaluatedVisual::from_components(
            tpl.anchor_canvas.0,
            tpl.anchor_canvas.1,
            1.0,
            1.0,
            0.0,
            0.0,
            0.0,
            1.0,
            crate::media::visual_properties::BlendMode::Normal,
        ),
        canvas_corners: tpl.canvas_corners,
        screen_corners,
        anchor_canvas: tpl.anchor_canvas,
        anchor_screen,
        rotate_handle_screen,
    }
}

fn paint_primary_handles(
    q: &PreviewLayerHitQuad,
    hovered: Option<PreviewHandleKind>,
    window: &mut Window,
) {
    let top_mid = (
        f32::midpoint(q.screen_corners[0].0, q.screen_corners[1].0),
        f32::midpoint(q.screen_corners[0].1, q.screen_corners[1].1),
    );
    let mut stem = PathBuilder::stroke(px(1.5));
    stem.move_to(point(px(top_mid.0), px(top_mid.1)));
    stem.line_to(point(
        px(q.rotate_handle_screen.0),
        px(q.rotate_handle_screen.1),
    ));
    if let Ok(path) = stem.build() {
        window.paint_path(path, rgba(0x38bd_f8ff));
    }

    for (kind, pos) in handle_screen_positions(q) {
        let is_hovered = hovered == Some(kind);
        match kind {
            PreviewHandleKind::Anchor => paint_anchor_handle(pos, is_hovered, window),
            PreviewHandleKind::Rotate => paint_rotate_handle(pos, is_hovered, window),
            _ => paint_scale_handle(pos, is_hovered, window),
        }
    }
}

fn paint_anchor_handle(pos: (f32, f32), is_hovered: bool, window: &mut Window) {
    if is_hovered {
        let hr = 9.0_f32;
        window.paint_quad(quad(
            Bounds {
                origin: point(px(pos.0 - hr), px(pos.1 - hr)),
                size: size(px(hr * 2.0), px(hr * 2.0)),
            },
            Corners::all(px(hr)),
            rgba(0xf59e_0b33),
            Edges::all(px(1.5)),
            rgba(0xfef0_8aff),
            BorderStyle::Solid,
        ));
    }
    let r = if is_hovered { 6.5_f32 } else { 5.5_f32 };
    window.paint_quad(quad(
        Bounds {
            origin: point(px(pos.0 - r), px(pos.1 - r)),
            size: size(px(r * 2.0), px(r * 2.0)),
        },
        Corners::all(px(r)),
        rgba(0xf59e_0bcc),
        Edges::all(px(1.5)),
        rgba(0xffff_ffff),
        BorderStyle::Solid,
    ));
    window.paint_quad(fill(
        Bounds {
            origin: point(px(pos.0 - 4.0), px(pos.1 - 0.5)),
            size: size(px(8.0), px(1.0)),
        },
        rgba(0xffff_ffff),
    ));
    window.paint_quad(fill(
        Bounds {
            origin: point(px(pos.0 - 0.5), px(pos.1 - 4.0)),
            size: size(px(1.0), px(8.0)),
        },
        rgba(0xffff_ffff),
    ));
}

fn paint_rotate_handle(pos: (f32, f32), is_hovered: bool, window: &mut Window) {
    if is_hovered {
        paint_rotate_hover_ring(pos, window);
    }
    let r = if is_hovered { 5.5_f32 } else { 4.5_f32 };
    window.paint_quad(quad(
        Bounds {
            origin: point(px(pos.0 - r), px(pos.1 - r)),
            size: size(px(r * 2.0), px(r * 2.0)),
        },
        Corners::all(px(r)),
        rgba(0x10b9_81ff),
        Edges::all(px(1.0)),
        rgba(0xffff_ffff),
        BorderStyle::Solid,
    ));
}

fn paint_scale_handle(pos: (f32, f32), is_hovered: bool, window: &mut Window) {
    if is_hovered {
        let hh = 6.0_f32;
        window.paint_quad(quad(
            Bounds {
                origin: point(px(pos.0 - hh), px(pos.1 - hh)),
                size: size(px(hh * 2.0), px(hh * 2.0)),
            },
            Corners::all(px(2.0)),
            rgba(0x38bd_f844),
            Edges::all(px(1.5)),
            rgba(0x38bd_f8ff),
            BorderStyle::Solid,
        ));
    }
    let half = if is_hovered { 4.5_f32 } else { 3.5_f32 };
    window.paint_quad(quad(
        Bounds {
            origin: point(px(pos.0 - half), px(pos.1 - half)),
            size: size(px(half * 2.0), px(half * 2.0)),
        },
        Corners::all(px(1.0)),
        rgba(0xffff_ffff),
        Edges::all(px(1.0)),
        rgba(0x0284_c7ff),
        BorderStyle::Solid,
    ));
}

fn paint_rotate_hover_ring(center: (f32, f32), window: &mut Window) {
    let outer_r = 10.5_f32;
    let steps = 16;
    let start_deg = -45.0_f32;
    let sweep_deg = 270.0_f32;
    let mut arc = PathBuilder::stroke(px(1.75));
    for idx in 0..=steps {
        #[allow(clippy::cast_precision_loss)]
        let frac = (idx as f32) / (steps as f32);
        let rad = (start_deg + sweep_deg * frac).to_radians();
        let x = center.0 + outer_r * rad.cos();
        let y = center.1 + outer_r * rad.sin();
        if idx == 0 {
            arc.move_to(point(px(x), px(y)));
        } else {
            arc.line_to(point(px(x), px(y)));
        }
    }
    let end_rad = (start_deg + sweep_deg).to_radians();
    let tip = (
        center.0 + outer_r * end_rad.cos(),
        center.1 + outer_r * end_rad.sin(),
    );
    arc.line_to(point(px(tip.0 - 4.0), px(tip.1 - 3.0)));
    arc.move_to(point(px(tip.0), px(tip.1)));
    arc.line_to(point(px(tip.0 + 3.0), px(tip.1 - 4.0)));
    if let Ok(path) = arc.build() {
        window.paint_path(path, rgba(0x34d3_99ff));
    }
}

fn render_media_offline_overlay(
    asset_id: crate::core::ids::AssetId,
    asset_name: String,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    h_flex()
        .id("media-offline-overlay")
        .absolute()
        .bottom(px(10.0))
        .left(px(10.0))
        .gap_2()
        .px_2p5()
        .py_1p5()
        .rounded(px(6.0))
        .bg(rgba(0x991b_1be6))
        .border_1()
        .border_color(rgba(0xfca5_a5ff))
        .items_center()
        .child(
            v_flex()
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_bold()
                        .text_color(rgba(0xfef2_f2ff))
                        .child("MEDIA OFFLINE"),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgba(0xfeca_caff))
                        .child(asset_name),
                ),
        )
        .child(
            Button::new("relink-offline-media-btn")
                .xsmall()
                .primary()
                .label("メディア再リンク...")
                .on_click(cx.listener(move |_, _, _, cx| {
                    SeaEditorWindow::prompt_relink_asset(asset_id.clone(), cx);
                })),
        )
        .into_any_element()
}

fn render_preview_notice_banner(notice: String, cx: &mut Context<SeaEditorWindow>) -> AnyElement {
    h_flex()
        .id("preview-notice-banner")
        .absolute()
        .top(px(8.0))
        .left(px(12.0))
        .gap_2()
        .px_2p5()
        .py_1()
        .rounded(px(4.0))
        .bg(rgba(0x7835_0fe6))
        .border_1()
        .border_color(rgba(0xf59e_0bff))
        .items_center()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgba(0xfef3_c7ff))
                .child(notice),
        )
        .child(
            Button::new("dismiss-preview-notice")
                .xsmall()
                .ghost()
                .label("×")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.preview_notice = None;
                    cx.notify();
                })),
        )
        .into_any_element()
}

#[allow(clippy::cast_precision_loss)]
fn render_master_waveform_strip(
    controller: &AppController,
    bars: &[f32],
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let total = controller.document.total_duration_frames().max(1);
    let head = controller.clock.current_frame().get().min(total);
    let head_frac = (head as f32) / (total as f32);
    let sel_range = controller.document.selection_range.map(|r| {
        (
            (r.start.get().min(total) as f32) / (total as f32),
            (r.end.get().min(total) as f32) / (total as f32),
        )
    });
    let bounds_cell = Rc::clone(&controller.preview_waveform_bounds);
    let bars_owned = bars.to_vec();

    let strip = canvas(
        move |bounds, _, _| {
            let rect = (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            );
            bounds_cell.set(rect);
            (rect, bars_owned)
        },
        move |_, (rect, bars_data), window, _| {
            paint_master_waveform(rect, &bars_data, head_frac, sel_range, window);
        },
    )
    .size_full();

    div()
        .id("preview-master-waveform")
        .w_full()
        .h(px(MASTER_WAVEFORM_HEIGHT_PX))
        .bg(rgba(0x0f14_1eff))
        .border_t_1()
        .border_color(rgba(0x2730_3fff))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.workspace_focus, cx);
                this.preview_pointer_mode =
                    crate::ui::window::PreviewPointerInputMode::WaveformScrubbing;
                this.controller.seek_by_waveform_x(f32::from(ev.position.x));
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        )
        .child(strip)
        .into_any_element()
}

#[allow(clippy::cast_precision_loss)]
fn paint_master_waveform(
    rect: (f32, f32, f32, f32),
    bars: &[f32],
    head_frac: f32,
    sel_range: Option<(f32, f32)>,
    window: &mut Window,
) {
    let (rx, ry, rw, rh) = rect;
    if rw <= 2.0 || rh <= 2.0 {
        return;
    }

    if let Some((s0, s1)) = sel_range
        && s1 > s0
    {
        window.paint_quad(fill(
            Bounds {
                origin: point(px(rx + rw * s0), px(ry)),
                size: size(px(rw * (s1 - s0)), px(rh)),
            },
            rgba(0x38bd_f828),
        ));
    }

    let cy = ry + rh * 0.5;
    window.paint_quad(fill(
        Bounds {
            origin: point(px(rx), px(cy)),
            size: size(px(rw), px(1.0)),
        },
        rgba(0x3341_5588),
    ));

    let count = bars.len().max(1);
    let slot_w = rw / (count as f32);
    let bar_w = (slot_w - 0.5).max(1.0);
    let half_h = (rh * 0.44).max(1.0);
    for (idx, &peak) in bars.iter().enumerate() {
        let amp = (peak * half_h).max(if peak > 0.002 { 1.5 } else { 0.5 });
        let bx = rx + (idx as f32) * slot_w;
        window.paint_quad(fill(
            Bounds {
                origin: point(px(bx), px(cy - amp)),
                size: size(px(bar_w), px(amp * 2.0)),
            },
            rgba(0x22c5_5ed8),
        ));
    }

    let hx = rx + rw * head_frac.clamp(0.0, 1.0);
    window.paint_quad(fill(
        Bounds {
            origin: point(px(hx - 1.0), px(ry)),
            size: size(px(2.0), px(rh)),
        },
        rgba(0xef44_44ee),
    ));
}

#[allow(clippy::cast_precision_loss)]
fn render_transport_bar(
    controller: &AppController,
    scrub_slider: &Entity<SliderState>,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let current = controller.clock.current_frame();
    let total = controller.document.total_duration_frames();
    let playing = controller.clock.is_playing();
    let return_to_start = controller.return_playhead_to_start_on_stop;
    let has_sel_clip = !controller.effective_selected_clip_ids().is_empty();
    let tc = format!(
        "{} ({}f / {}f)",
        format_timeline_timestamp(
            current.get(),
            controller.document.fps,
            controller.time_display_unit,
        ),
        current.get(),
        total
    );

    let sel_band = controller.document.selection_range.and_then(|r| {
        let span = total.max(1) as f32;
        let left_pct = ((r.start.get().min(total) as f32) / span).clamp(0.0, 1.0) * 100.0;
        let width_pct = ((r.end.get().min(total).saturating_sub(r.start.get()) as f32) / span)
            .clamp(0.0, 1.0)
            * 100.0;
        if width_pct <= 0.1 {
            None
        } else {
            Some(
                div()
                    .absolute()
                    .top(px(4.0))
                    .bottom(px(4.0))
                    .left(gpui_kit::relative(left_pct / 100.0))
                    .w(gpui_kit::relative(width_pct / 100.0))
                    .rounded(px(2.0))
                    .bg(rgba(0x38bd_f833)),
            )
        }
    });

    let base = h_flex()
        .h(px(TRANSPORT_BAR_HEIGHT_PX))
        .w_full()
        .px_2()
        .gap_1p5()
        .items_center()
        .bg(theme.tab_bar)
        .border_t_1()
        .border_color(theme.border);
    add_transport_buttons(base, playing, cx)
        .child(
            div()
                .min_w(px(145.0))
                .text_size(px(10.0))
                .font_family("monospace")
                .text_color(rgba(0xe2e8_f0ff))
                .child(tc),
        )
        .child(
            div()
                .relative()
                .flex_1()
                .min_w(px(80.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .children(sel_band)
                .child(Slider::new(scrub_slider)),
        )
        .child({
            let mut btn = Button::new("transport-return-start-btn")
                .xsmall()
                .label("停止時↩");
            btn = if return_to_start {
                btn.primary()
            } else {
                btn.ghost()
            };
            btn.on_click(cx.listener(|this, _, window, cx| {
                this.controller.toggle_return_playhead_to_start_on_stop();
                this.reload_app_menus(cx);
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            }))
        })
        .child(
            Button::new("transport-split-btn")
                .xsmall()
                .ghost()
                .label("S:分割")
                .disabled(!has_sel_clip)
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Err(err) = this.controller.split_selected_clip_at_playhead() {
                        this.controller.status_message = err;
                    }
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn add_transport_buttons(
    bar: gpui_kit::Div,
    playing: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Div {
    bar.child(
        Button::new("transport-start-btn")
            .xsmall()
            .ghost()
            .label("|<")
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.seek_to_frame(FrameIndex::ZERO);
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            })),
    )
    .child(
        Button::new("transport-prev-frame-btn")
            .xsmall()
            .ghost()
            .label("◀")
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.step_frames(-1);
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            })),
    )
    .child(
        Button::new("transport-play-btn")
            .xsmall()
            .primary()
            .label(if playing { "⏸" } else { "▶" })
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.toggle_play_pause();
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                window.focus(&this.workspace_focus, cx);
                cx.notify();
            })),
    )
    .child(
        Button::new("transport-stop-btn")
            .xsmall()
            .ghost()
            .label("■")
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.stop();
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            })),
    )
    .child(
        Button::new("transport-next-frame-btn")
            .xsmall()
            .ghost()
            .label("▶|")
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.step_frames(1);
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            })),
    )
    .child(
        Button::new("transport-end-btn")
            .xsmall()
            .ghost()
            .label(">|")
            .on_click(cx.listener(|this, _, window, cx| {
                this.controller.seek_to_end();
                this.mark_sliders_dirty();
                this.sync_sliders_now(window, cx);
                cx.notify();
            })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_palette_distinguishes_backdrop_from_black_canvas() {
        let dark = preview_palette(true);
        assert_eq!(dark.backdrop_bg, rgba(0x2b30_3bff));
        assert_eq!(dark.canvas_inner_bg, rgba(0x0000_00ff));
        assert_eq!(dark.canvas_border, rgba(0x6474_8bcc));
        assert_ne!(dark.backdrop_bg, dark.canvas_inner_bg);

        let light = preview_palette(false);
        assert_eq!(light.backdrop_bg, rgba(0xd4d8_e0ff));
        assert_eq!(light.canvas_inner_bg, rgba(0x0000_00ff));
        assert_eq!(light.canvas_border, rgba(0x4755_69cc));
        assert_ne!(light.backdrop_bg, light.canvas_inner_bg);
    }

    #[test]
    fn outer_canvas_border_bounds_expands_by_one_pixel_on_each_side() {
        let inner = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: size(px(640.0), px(360.0)),
        };
        let outer = outer_canvas_border_bounds(inner);
        assert!((f32::from(outer.origin.x) - 99.0).abs() < 1e-4);
        assert!((f32::from(outer.origin.y) - 49.0).abs() < 1e-4);
        assert!((f32::from(outer.size.width) - 642.0).abs() < 1e-4);
        assert!((f32::from(outer.size.height) - 362.0).abs() < 1e-4);
    }
}
