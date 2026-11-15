//! 下部タイムラインパネル: 仮想トラック一覧・クリップ選択・再生ヘッドシーク。

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, Context, ExternalPaths, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Size, Styled as _,
    Window, div, px, rgba, size,
};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};

use crate::core::ids::ClipId;
use crate::core::time::{FrameCount, FrameIndex};
use crate::timeline::document::{AssetStatus, Clip, ClipKind, ProjectDocument};
use crate::timeline::snap::{
    SnapConfig, clip_intersects_window, collect_snap_anchors, snap_clip_move, snap_frame_edge,
};
use crate::ui::app_controller::AppController;
use crate::ui::window::SeaEditorWindow;

const TRACK_HEADER_WIDTH: f32 = 136.0;
const TRACK_ROW_HEIGHT: f32 = 44.0;

/// 下部タイムライン領域（ルーラー＋仮想スクロールトラック＋再生ヘッド＋外部ファイルドロップ）を描画する。
pub fn render_timeline_panel(
    controller: &AppController,
    scroll_handle: &VirtualListScrollHandle,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let layer_count = controller.document.layers.len();
    let item_sizes: Rc<Vec<Size<Pixels>>> =
        Rc::new(vec![size(px(1000.), px(TRACK_ROW_HEIGHT)); layer_count]);
    let view_entity = cx.entity();

    v_flex()
        .size_full()
        .bg(theme.background)
        .border_t_1()
        .border_color(theme.border)
        .drag_over::<ExternalPaths>(|style, _, _, _| {
            style
                .bg(rgba(0x0f17_2a33))
                .border_1()
                .border_color(rgba(0x38bd_f899))
        })
        .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            handle_timeline_file_drop(this, paths, None, window, cx);
        }))
        .child(render_timeline_ruler(controller, cx))
        .child(render_track_area(
            controller,
            layer_count,
            view_entity,
            scroll_handle,
            item_sizes,
            cx,
        ))
}

/// ウィンドウ座標 `X` からタイムラインレーン上のドロップ先フレーム位置を算出する。
///
/// 左端トラックヘッダ領域 (`x < TRACK_HEADER_WIDTH`) 上に落とされた場合は `None` を返し、
/// 呼び出し側で対象トラック末尾への追加として扱えるようにする。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn frame_at_timeline_window_x(
    window_x: f32,
    scroll_x_frames: u64,
    px_per_frame: f32,
) -> Option<FrameIndex> {
    if window_x < TRACK_HEADER_WIDTH {
        return None;
    }
    let local_x = window_x - TRACK_HEADER_WIDTH;
    let safe_ppf = px_per_frame.max(0.5);
    let delta = (local_x / safe_ppf).round() as u64;
    Some(FrameIndex::new(scroll_x_frames.saturating_add(delta)))
}

fn handle_timeline_file_drop(
    view: &mut SeaEditorWindow,
    paths: &ExternalPaths,
    target_layer: Option<&crate::core::ids::LayerId>,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let cursor_x = f32::from(window.mouse_position().x);
    let drop_frame = frame_at_timeline_window_x(
        cursor_x,
        view.controller.timeline_scroll_x_frames,
        view.controller.timeline_px_per_frame,
    );
    match view
        .controller
        .import_media_files_at_drop(paths.paths(), target_layer, drop_frame)
    {
        Ok(_) => {
            view.controller.inspector.invalidate_sync();
            view.sync_sliders_now(window, cx);
        }
        Err(err) => {
            view.controller.status_message = format!("ドロップ追加失敗: {err}");
        }
    }
    cx.notify();
}

fn render_timeline_ruler(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let playhead = controller.clock.current_frame().get();
    let px_per_frame = controller.timeline_px_per_frame.max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let head_x = (playhead as f32) * px_per_frame;

    h_flex()
        .h(px(28.))
        .w_full()
        .border_b_1()
        .border_color(theme.border)
        .items_center()
        .child(
            div()
                .w(px(TRACK_HEADER_WIDTH))
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .border_r_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Timeline"),
        )
        .child(
            div()
                .id("timeline-ruler-seek")
                .flex_1()
                .h_full()
                .relative()
                .bg(rgba(0x1619_20ff))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, evt: &MouseDownEvent, _, cx| {
                        let raw_x = f32::from(evt.position.x) - TRACK_HEADER_WIDTH;
                        let scale = this.controller.timeline_px_per_frame.max(1.0);
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let target = (raw_x / scale).max(0.0).round() as u64;
                        this.controller.seek_to_frame(FrameIndex::new(target));
                        this.mark_sliders_dirty();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(head_x))
                        .w(px(2.))
                        .bg(rgba(0x38bd_f8ff)),
                ),
        )
}

/// タイムライン上のクリップドラッグ編集種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragMode {
    /// クリップの時間位置・レイヤー移動。
    Move,
    /// 左端を動かして開始時刻と長さをトリム・延長。
    TrimLeft,
    /// 右端を動かして長さをトリム・延長。
    TrimRight,
}

/// ドラッグ中（`MouseDown`〜`MouseUp`）のリアルタイムプレビュー状態。
#[derive(Debug, Clone)]
pub struct TimelineDragState {
    /// ドラッグ中のクリップ ID。
    pub clip_id: ClipId,
    /// 元レイヤー位置。
    pub origin_layer_idx: usize,
    /// 元開始フレーム。
    pub origin_start_frame: u64,
    /// 元クリップ長（フレーム）。
    pub origin_duration_frames: u64,
    /// ドラッグ開始時マウス X (px)。
    pub start_mouse_x: f32,
    /// ドラッグ開始時マウス Y (px)。
    pub start_mouse_y: f32,
    /// 現在の編集モード。
    pub mode: DragMode,
    /// 現在のプレビュー先レイヤーインデックス。
    pub current_layer_idx: usize,
    /// 現在のプレビュー開始フレーム。
    pub current_start_frame: u64,
    /// 現在のプレビュー長（フレーム）。
    pub current_duration_frames: u64,
}

impl TimelineDragState {
    /// クリップ現在状態とマウス位置から新しいドラッグ状態を作成する。
    #[must_use]
    pub fn begin(
        clip: &Clip,
        origin_layer_idx: usize,
        mouse_x: f32,
        mouse_y: f32,
        mode: DragMode,
    ) -> Self {
        let start = clip.timeline_start.get();
        let dur = clip.duration.get();
        Self {
            clip_id: clip.id.clone(),
            origin_layer_idx,
            origin_start_frame: start,
            origin_duration_frames: dur,
            start_mouse_x: mouse_x,
            start_mouse_y: mouse_y,
            mode,
            current_layer_idx: origin_layer_idx,
            current_start_frame: start,
            current_duration_frames: dur,
        }
    }

    /// マウス移動量からプレビュー位置・長さ・スナップ縦ガイド線を再計算する。
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn update_from_cursor(
        &mut self,
        cursor: (f32, f32),
        px_per_frame: f32,
        doc: &ProjectDocument,
        playhead: FrameIndex,
        snap_cfg: &SnapConfig,
    ) -> Option<u64> {
        let (mouse_x, mouse_y) = cursor;
        let safe_ppf = px_per_frame.max(0.5);
        let delta_frames = ((mouse_x - self.start_mouse_x) / safe_ppf).round() as i64;
        let excluded = [self.clip_id.clone()];

        match self.mode {
            DragMode::Move => {
                let row_delta =
                    ((mouse_y - self.start_mouse_y) / TRACK_ROW_HEIGHT).round() as isize;
                let max_layer = doc.layers.len().saturating_sub(1);
                let target_layer = (isize::try_from(self.origin_layer_idx).unwrap_or(0)
                    + row_delta)
                    .clamp(0, isize::try_from(max_layer).unwrap_or(0));
                self.current_layer_idx = usize::try_from(target_layer).unwrap_or(0);

                let raw_start = (i64::try_from(self.origin_start_frame).unwrap_or(0) + delta_frames)
                    .max(0) as u64;
                let dur =
                    FrameCount::new(self.origin_duration_frames.max(1)).unwrap_or(FrameCount::ONE);
                let snap = snap_clip_move(
                    doc,
                    playhead,
                    dur,
                    FrameIndex::new(raw_start),
                    &excluded,
                    snap_cfg,
                );
                self.current_start_frame = snap.snapped_start.get();
                self.current_duration_frames = self.origin_duration_frames;
                snap.guide_frame
            }
            DragMode::TrimLeft => {
                let orig_end = self
                    .origin_start_frame
                    .saturating_add(self.origin_duration_frames);
                let max_delta =
                    i64::try_from(self.origin_duration_frames.saturating_sub(1)).unwrap_or(0);
                let min_delta = -i64::try_from(self.origin_start_frame).unwrap_or(0);
                let clamped = delta_frames.clamp(min_delta, max_delta);
                let raw_left =
                    (i64::try_from(self.origin_start_frame).unwrap_or(0) + clamped).max(0) as u64;
                let mut guide = None;
                let final_left = if snap_cfg.enabled {
                    let anchors = collect_snap_anchors(doc, playhead, &excluded, snap_cfg);
                    if let Some(hit) =
                        snap_frame_edge(raw_left, &anchors, snap_cfg.threshold_frames)
                        && hit.frame < orig_end
                    {
                        guide = Some(hit.frame);
                        hit.frame
                    } else {
                        raw_left
                    }
                } else {
                    raw_left
                };
                self.current_start_frame = final_left;
                self.current_duration_frames = orig_end.saturating_sub(final_left).max(1);
                guide
            }
            DragMode::TrimRight => {
                let min_delta =
                    -i64::try_from(self.origin_duration_frames.saturating_sub(1)).unwrap_or(0);
                let clamped = delta_frames.max(min_delta);
                let raw_dur = (i64::try_from(self.origin_duration_frames).unwrap_or(1) + clamped)
                    .max(1) as u64;
                let raw_right = self.origin_start_frame.saturating_add(raw_dur);
                let mut guide = None;
                let final_right = if snap_cfg.enabled {
                    let anchors = collect_snap_anchors(doc, playhead, &excluded, snap_cfg);
                    if let Some(hit) =
                        snap_frame_edge(raw_right, &anchors, snap_cfg.threshold_frames)
                        && hit.frame > self.origin_start_frame
                    {
                        guide = Some(hit.frame);
                        hit.frame
                    } else {
                        raw_right
                    }
                } else {
                    raw_right
                };
                self.current_start_frame = self.origin_start_frame;
                self.current_duration_frames =
                    final_right.saturating_sub(self.origin_start_frame).max(1);
                guide
            }
        }
    }

    /// ドラッグ完了時に単一 Undo 可能な実コマンドとして `AppController` へ確定適用する。
    ///
    /// # Errors
    ///
    /// ドメイン制約違反（重複・ソース範囲超過等）が発生した場合にエラー文字列を返す。
    #[allow(clippy::cast_possible_wrap)]
    pub fn commit(&self, controller: &mut AppController) -> Result<(), String> {
        match self.mode {
            DragMode::Move => {
                if self.current_layer_idx == self.origin_layer_idx
                    && self.current_start_frame == self.origin_start_frame
                {
                    return Ok(());
                }
                let target_layer_id = controller
                    .document
                    .layers
                    .get(self.current_layer_idx)
                    .map(|l| l.id.clone())
                    .ok_or_else(|| "移動先レイヤーが存在しません".to_owned())?;
                let cmd = crate::timeline::command::EditCommand::MoveClip {
                    clip_id: crate::core::ids::ClipRef::Id(self.clip_id.clone()),
                    target_layer_id: Some(crate::core::ids::LayerRef::Id(target_layer_id.clone())),
                    new_timeline_start: FrameIndex::new(self.current_start_frame),
                };
                let _ = controller.dispatch_command(&cmd)?;
                controller.selected_layer_id = Some(target_layer_id);
                Ok(())
            }
            DragMode::TrimLeft => {
                let delta = i64::try_from(self.current_start_frame).unwrap_or(0)
                    - i64::try_from(self.origin_start_frame).unwrap_or(0);
                if delta == 0 {
                    return Ok(());
                }
                controller.trim_clip_left_delta(&self.clip_id, delta)
            }
            DragMode::TrimRight => {
                let delta = i64::try_from(self.origin_duration_frames).unwrap_or(0)
                    - i64::try_from(self.current_duration_frames).unwrap_or(0);
                if delta == 0 {
                    return Ok(());
                }
                controller.trim_clip_right_delta(&self.clip_id, delta)
            }
        }
    }
}

fn render_track_area(
    controller: &AppController,
    layer_count: usize,
    view_entity: gpui_kit::Entity<SeaEditorWindow>,
    scroll_handle: &VirtualListScrollHandle,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let theme = cx.theme().clone();
    if layer_count == 0 {
        return v_flex()
            .flex_1()
            .w_full()
            .child(render_timeline_toolbar(controller, cx))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("タイムラインは空です。「ファイル > メディアを読み込む...」またはここへドラッグ＆ドロップで素材を追加できます。"),
            )
            .into_any_element();
    }

    v_flex()
        .flex_1()
        .w_full()
        .overflow_hidden()
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            let x = f32::from(ev.position.x);
            let y = f32::from(ev.position.y);
            let px_per_frame = this.controller.timeline_px_per_frame;
            let head = this.controller.clock.current_frame();
            let snap_cfg = this.controller.snap_config;
            if let Some(mut drag) = this.controller.timeline_drag.take() {
                let guide = drag.update_from_cursor(
                    (x, y),
                    px_per_frame,
                    &this.controller.document,
                    head,
                    &snap_cfg,
                );
                this.controller.snap_guide_frame = guide;
                this.controller.timeline_drag = Some(drag);
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _: &MouseUpEvent, window, cx| {
                this.controller.snap_guide_frame = None;
                if let Some(drag) = this.controller.timeline_drag.take() {
                    if let Err(err) = drag.commit(&mut this.controller) {
                        this.controller.status_message = format!("編集制約: {err}");
                    }
                    this.controller.inspector.invalidate_sync();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                }
            }),
        )
        .child(render_timeline_toolbar(controller, cx))
        .child(
            div().flex_1().w_full().overflow_hidden().child(
                v_virtual_list(
                    view_entity,
                    "timeline-track-list",
                    item_sizes,
                    |view, visible_range, _window, cx| {
                        visible_range
                            .filter_map(|ix| render_track_row(&view.controller, ix, cx))
                            .collect()
                    },
                )
                .track_scroll(scroll_handle),
            ),
        )
        .into_any_element()
}

fn render_timeline_toolbar(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let msg = controller.status_message.as_str();
    let (status_text, status_color) = if msg.starts_with("ドロップ") || msg.starts_with("編集制約")
    {
        (msg.to_owned(), theme.danger)
    } else if msg.contains("素材を追加") || msg.contains("タイムラインに追加") {
        (msg.to_owned(), theme.foreground)
    } else {
        (
            "ファイル D&D で素材追加可".to_owned(),
            theme.muted_foreground,
        )
    };

    h_flex()
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .child(toolbar_btn("tl-add-track", "+Track", cx, |this, _, cx| {
            let _ = this.controller.add_empty_layer();
            cx.notify();
        }))
        .child(toolbar_btn("tl-zoom-out", "Zoom-", cx, |this, _, cx| {
            this.controller.zoom_timeline(0.75);
            cx.notify();
        }))
        .child(toolbar_btn("tl-zoom-in", "Zoom+", cx, |this, _, cx| {
            this.controller.zoom_timeline(1.33);
            cx.notify();
        }))
        .child(toolbar_btn("tl-scroll-left", "< 60f", cx, |this, _, cx| {
            this.controller.timeline_scroll_x_frames =
                this.controller.timeline_scroll_x_frames.saturating_sub(60);
            cx.notify();
        }))
        .child(toolbar_btn(
            "tl-scroll-right",
            "60f >",
            cx,
            |this, _, cx| {
                this.controller.timeline_scroll_x_frames =
                    this.controller.timeline_scroll_x_frames.saturating_add(60);
                cx.notify();
            },
        ))
        .child(toolbar_btn("tl-snap-toggle", "Snap", cx, |this, _, cx| {
            this.controller.snap_config.enabled = !this.controller.snap_config.enabled;
            this.controller.status_message = if this.controller.snap_config.enabled {
                "フレームスナップ ON".to_owned()
            } else {
                "フレームスナップ OFF".to_owned()
            };
            cx.notify();
        }))
        .child(toolbar_btn("tl-copy", "コピー", cx, |this, _, cx| {
            let _ = this.controller.copy_selected_clip();
            cx.notify();
        }))
        .child(toolbar_btn("tl-paste", "貼付", cx, |this, window, cx| {
            let _ = this.controller.paste_clipboard_at_playhead();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .child(toolbar_btn("tl-dup", "複製", cx, |this, window, cx| {
            let _ = this.controller.duplicate_selected_clip();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .child(div().flex_1())
        .child(div().text_color(status_color).child(status_text))
}

fn toolbar_btn(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<SeaEditorWindow>,
    handler: impl Fn(&mut SeaEditorWindow, &mut gpui_kit::Window, &mut Context<SeaEditorWindow>)
    + 'static,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .small()
        .label(label)
        .on_click(cx.listener(move |this, _, window, cx| {
            handler(this, window, cx);
        }))
}

#[derive(Clone, Copy)]
struct ClipDisplayRect {
    start_frame: u64,
    duration_frames: u64,
    clip_ix: usize,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_track_row(
    controller: &AppController,
    layer_ix: usize,
    cx: &mut Context<SeaEditorWindow>,
) -> Option<AnyElement> {
    let layer = controller.document.layers.get(layer_ix)?;
    let border_color = cx.theme().border;
    let fg_color = cx.theme().foreground;
    let px_per_frame = controller.timeline_px_per_frame.max(0.5);
    let scroll_x = controller.timeline_scroll_x_frames;
    let head_frame = controller.clock.current_frame().get();
    let head_x = (head_frame as f32 - scroll_x as f32) * px_per_frame;
    let clip_views = collect_visible_clips_for_row(controller, layer, layer_ix, cx);

    let mut lane = div()
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .children(clip_views);

    if let Some(guide_f) = controller.snap_guide_frame {
        let guide_x = (guide_f as f32 - scroll_x as f32) * px_per_frame;
        if guide_x >= -2.0 {
            lane = lane.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(guide_x))
                    .w(px(1.5))
                    .bg(rgba(0xfacc_15dd)),
            );
        }
    }

    if head_x >= -2.0 {
        lane = lane.child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(head_x))
                .w(px(2.))
                .bg(rgba(0x38bd_f8ff)),
        );
    }

    let target_layer_id = layer.id.clone();
    Some(
        h_flex()
            .h(px(TRACK_ROW_HEIGHT))
            .w_full()
            .border_b_1()
            .border_color(border_color)
            .drag_over::<ExternalPaths>(|style, _, _, _| {
                style
                    .bg(rgba(0x1d4e_d833))
                    .border_1()
                    .border_color(rgba(0x38bd_f8ee))
            })
            .on_drop::<ExternalPaths>(cx.listener(
                move |this, paths: &ExternalPaths, window, cx| {
                    handle_timeline_file_drop(this, paths, Some(&target_layer_id), window, cx);
                },
            ))
            .child(track_row_header(layer, border_color, fg_color))
            .child(lane)
            .into_any_element(),
    )
}

fn track_row_header(
    layer: &crate::timeline::document::Layer,
    border_color: gpui_kit::Hsla,
    fg_color: gpui_kit::Hsla,
) -> impl IntoElement {
    div()
        .w(px(TRACK_HEADER_WIDTH))
        .h_full()
        .px_2()
        .flex()
        .items_center()
        .border_r_1()
        .border_color(border_color)
        .text_xs()
        .text_color(fg_color)
        .child(if layer.muted {
            format!("{} [M]", layer.name)
        } else {
            layer.name.clone()
        })
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn collect_visible_clips_for_row(
    controller: &AppController,
    layer: &crate::timeline::document::Layer,
    layer_ix: usize,
    cx: &mut Context<SeaEditorWindow>,
) -> Vec<AnyElement> {
    let px_per_frame = controller.timeline_px_per_frame.max(0.5);
    let scroll_x = controller.timeline_scroll_x_frames;
    let visible_span = ((2600.0 / px_per_frame).ceil() as u64).max(120);
    let visible_end = scroll_x.saturating_add(visible_span);
    let drag = controller.timeline_drag.as_ref();

    let mut out: Vec<AnyElement> = layer
        .clips
        .iter()
        .enumerate()
        .filter_map(|(clip_ix, clip)| {
            let (start_f, dur_f) = if let Some(d) = drag
                && d.clip_id == clip.id
            {
                if d.current_layer_idx != layer_ix {
                    return None;
                }
                (d.current_start_frame, d.current_duration_frames)
            } else {
                (clip.timeline_start.get(), clip.duration.get())
            };
            if !clip_intersects_window(start_f, dur_f, scroll_x, visible_end) {
                return None;
            }
            let rect = ClipDisplayRect {
                start_frame: start_f,
                duration_frames: dur_f,
                clip_ix,
            };
            Some(render_clip_block(controller, layer_ix, clip, rect, cx))
        })
        .collect();

    if let Some(d) = drag
        && d.current_layer_idx == layer_ix
        && d.origin_layer_idx != layer_ix
        && let Some((_, dragged_clip)) = controller.document.find_clip(&d.clip_id)
    {
        let rect = ClipDisplayRect {
            start_frame: d.current_start_frame,
            duration_frames: d.current_duration_frames,
            clip_ix: 999_999,
        };
        out.push(render_clip_block(
            controller,
            layer_ix,
            dragged_clip,
            rect,
            cx,
        ));
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn render_clip_block(
    controller: &AppController,
    layer_ix: usize,
    clip: &Clip,
    rect: ClipDisplayRect,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let px_per_frame = controller.timeline_px_per_frame.max(0.5);
    let scroll_x = controller.timeline_scroll_x_frames;
    let left_px = (rect.start_frame as f32 - scroll_x as f32) * px_per_frame;
    let width_px = ((rect.duration_frames as f32) * px_per_frame).max(16.0);
    let is_selected = controller.selected_clip_id.as_ref() == Some(&clip.id);
    let bg = clip_block_color(controller, clip, is_selected);
    let border_col = if is_selected {
        rgba(0xfacc_15ff)
    } else {
        rgba(0x4755_69ff)
    };
    let owner_layer_id = controller
        .document
        .layers
        .get(layer_ix)
        .map(|l| l.id.clone());

    h_flex()
        .id(("timeline-clip", (rect.clip_ix as u64) + rect.start_frame))
        .absolute()
        .top(px(4.))
        .bottom(px(4.))
        .left(px(left_px))
        .w(px(width_px))
        .rounded_sm()
        .bg(bg)
        .border_1()
        .border_color(border_col)
        .overflow_hidden()
        .child(bind_clip_drag_segment(
            div().w(px(6.)).h_full().bg(rgba(0xf8fa_fc33)),
            clip,
            owner_layer_id.as_ref(),
            layer_ix,
            DragMode::TrimLeft,
            cx,
        ))
        .child(bind_clip_drag_segment(
            div()
                .flex_1()
                .h_full()
                .px_1()
                .flex()
                .items_center()
                .text_xs()
                .text_color(rgba(0xf8fa_fcff))
                .overflow_hidden()
                .child(clip.name.clone()),
            clip,
            owner_layer_id.as_ref(),
            layer_ix,
            DragMode::Move,
            cx,
        ))
        .child(bind_clip_drag_segment(
            div().w(px(6.)).h_full().bg(rgba(0xf8fa_fc33)),
            clip,
            owner_layer_id.as_ref(),
            layer_ix,
            DragMode::TrimRight,
            cx,
        ))
        .into_any_element()
}

fn clip_block_color(controller: &AppController, clip: &Clip, is_selected: bool) -> gpui_kit::Rgba {
    let is_missing = match &clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => controller
            .document
            .find_asset(asset_id)
            .is_some_and(|a| matches!(a.status, AssetStatus::Missing { .. })),
        ClipKind::Subtitle(_) => false,
    };
    if is_missing {
        rgba(0x991b_1bff)
    } else if is_selected {
        rgba(0x2563_ebff)
    } else {
        match &clip.kind {
            ClipKind::Video { .. } => rgba(0x1d4e_d8cc),
            ClipKind::Audio { .. } => rgba(0x0596_69cc),
            ClipKind::Image { .. } => rgba(0x7c3a_edcc),
            ClipKind::Subtitle(_) => rgba(0xd977_06cc),
        }
    }
}

fn bind_clip_drag_segment(
    segment: gpui_kit::Div,
    clip: &Clip,
    owner_layer_id: Option<&crate::core::ids::LayerId>,
    layer_ix: usize,
    mode: DragMode,
    cx: &mut Context<SeaEditorWindow>,
) -> gpui_kit::Div {
    let clip_clone = clip.clone();
    let lid_clone = owner_layer_id.cloned();
    segment.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
            this.controller.selected_layer_id.clone_from(&lid_clone);
            this.controller.selected_clip_id = Some(clip_clone.id.clone());
            this.controller.timeline_drag = Some(TimelineDragState::begin(
                &clip_clone,
                layer_ix,
                f32::from(ev.position.x),
                f32::from(ev.position.y),
                mode,
            ));
            this.sync_sliders_now(window, cx);
            cx.notify();
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::LayerId;
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::Layer;

    #[test]
    fn timeline_drag_move_and_trims_commit_accurately() -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = AppController::new();
        let l1 = LayerId::new("v1")?;
        let l2 = LayerId::new("v2")?;
        let c1 = ClipId::new("clip_drag")?;
        let mut layer1 = Layer::new(l1, "Track 1");
        layer1.clips.push(Clip {
            id: c1.clone(),
            name: "Sub".to_owned(),
            timeline_start: FrameIndex::new(30),
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Subtitle(SubtitleProperties::new("Drag Test")),
        });
        let layer2 = Layer::new(l2, "Track 2");
        controller.document.layers.push(layer1);
        controller.document.layers.push(layer2);
        controller.timeline_px_per_frame = 2.0;

        let snap = controller
            .document
            .find_clip(&c1)
            .ok_or("missing clip")?
            .1
            .clone();
        let mut drag = TimelineDragState::begin(&snap, 0, 100.0, 20.0, DragMode::Move);
        // dx = +80px @ 2px/frame = +40f -> 70f, dy = +44px = +1 row -> layer 1
        let _ = drag.update_from_cursor(
            (180.0, 64.0),
            controller.timeline_px_per_frame,
            &controller.document,
            FrameIndex::ZERO,
            &SnapConfig {
                enabled: false,
                ..SnapConfig::default()
            },
        );
        assert_eq!(drag.current_layer_idx, 1);
        assert_eq!(drag.current_start_frame, 70);
        drag.commit(&mut controller)
            .map_err(std::io::Error::other)?;
        assert_eq!(
            controller.document.layers[1].clips[0].timeline_start.get(),
            70
        );

        // TrimRight: -20px @ 2px/frame = -10f -> duration 60 -> 50
        let snap2 = controller
            .document
            .find_clip(&c1)
            .ok_or("missing moved clip")?
            .1
            .clone();
        let mut trim = TimelineDragState::begin(&snap2, 1, 200.0, 60.0, DragMode::TrimRight);
        let _ = trim.update_from_cursor(
            (180.0, 60.0),
            controller.timeline_px_per_frame,
            &controller.document,
            FrameIndex::ZERO,
            &SnapConfig::default(),
        );
        assert_eq!(trim.current_duration_frames, 50);
        trim.commit(&mut controller)
            .map_err(std::io::Error::other)?;
        assert_eq!(controller.document.layers[1].clips[0].duration.get(), 50);
        Ok(())
    }

    #[test]
    fn frame_at_timeline_window_x_handles_header_scroll_and_zoom() {
        assert_eq!(frame_at_timeline_window_x(0.0, 0, 2.0), None);
        assert_eq!(frame_at_timeline_window_x(135.9, 100, 2.0), None);
        assert_eq!(
            frame_at_timeline_window_x(TRACK_HEADER_WIDTH, 100, 2.0),
            Some(FrameIndex::new(100))
        );
        assert_eq!(
            frame_at_timeline_window_x(TRACK_HEADER_WIDTH + 60.0, 120, 2.0),
            Some(FrameIndex::new(150))
        );
    }
}
