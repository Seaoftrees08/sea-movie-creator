//! タイムライン領域（空きトラック・クリップ・中間点・ルーラー・ヘッダ）の右クリックメニュー定義。

use gpui_kit::component::menu::{ContextMenu, ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::{Context, Entity, Window};

use crate::core::ids::{ClipId, LayerId};
use crate::core::time::FrameIndex;
use crate::timeline::document::{Clip, ProjectDocument, TimelineLengthMode};
use crate::ui::app_controller::{
    AppController, RightPanelTab, TimeDisplayUnit, TimelineContextClick,
};
use crate::ui::timeline::state::frame_at_timeline_window_x;
use crate::ui::window::SeaEditorWindow;

/// トラック空き領域右クリック時のメニュー可否フラグ群。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneMenuState {
    /// 対象レイヤーがロックされているか。
    pub layer_locked: bool,
    /// クリップボードに貼り付け可能なデータがあるか。
    pub has_clipboard: bool,
    /// タイムラインに選択範囲 `[start, end)` が設定されているか。
    pub has_selection_range: bool,
}

/// タイムライン動作モードのトグル反映状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneToggleState {
    /// スナップ有効状態。
    pub snap_enabled: bool,
    /// シーク時のクリップ選択追従。
    pub follow_selected_on_seek: bool,
    /// ルーラー・ヘッダ時間をフレーム番号表示にするか。
    pub show_frames: bool,
}

/// トラックレーン右クリックメニュー全体の評価結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneMenuAvailability {
    /// 右クリックされたタイムラインフレーム位置。
    pub clicked_frame: FrameIndex,
    /// アクション可否フラグ。
    pub flags: LaneMenuState,
    /// チェック状態フラグ。
    pub toggles: LaneToggleState,
}

/// クリップ分割・中間点操作の可否フラグ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitMidpointAvailability {
    /// クリック位置でクリップを分割できるか。
    pub can_split_at_click: bool,
    /// 再生位置でクリップを分割できるか。
    pub can_split_at_playhead: bool,
    /// クリック位置へ中間点を挿入できるか。
    pub can_add_midpoint_at_click: bool,
}

/// クリップ移動・変更操作の可否フラグ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipMutationAvailability {
    /// 再生位置へ中間点を挿入できるか。
    pub can_add_midpoint_at_playhead: bool,
    /// 末尾を再生位置へ揃える操作が有効か。
    pub can_align_end_to_playhead: bool,
    /// 所属レイヤーがロックされているか。
    pub layer_locked: bool,
}

/// クリップ右クリックメニュー全体の評価結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipMenuAvailability {
    /// 分割およびクリック位置中間点の可否。
    pub split: SplitMidpointAvailability,
    /// 再生位置中間点・アライン・ロック可否。
    pub mutation: ClipMutationAvailability,
}

/// トラック空き領域右クリック時のメニュー可否を算出する。
#[must_use]
pub fn evaluate_lane_menu_availability(
    controller: &AppController,
    layer_id: &LayerId,
    clicked_frame: FrameIndex,
) -> LaneMenuAvailability {
    let layer_locked = controller
        .document
        .find_layer(layer_id)
        .is_some_and(|l| l.locked);
    LaneMenuAvailability {
        clicked_frame,
        flags: LaneMenuState {
            layer_locked,
            has_clipboard: controller.clipboard.is_some(),
            has_selection_range: controller.document.selection_range.is_some(),
        },
        toggles: LaneToggleState {
            snap_enabled: controller.snap_config.enabled,
            follow_selected_on_seek: controller.follow_selected_on_seek,
            show_frames: controller.time_display_unit == TimeDisplayUnit::Frames,
        },
    }
}

/// 指定タイムライン位置がクリップ内部の有効な中間点追加候補ならエフェクト基準フレームを返す。
#[must_use]
pub fn midpoint_effect_frame_for_timeline_frame(clip: &Clip, frame: FrameIndex) -> Option<i64> {
    if clip.duration.get() < 3 {
        return None;
    }
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    let f = frame.get();
    if !(start < f && f + 1 < end) {
        return None;
    }
    let local_i64 = i64::try_from(f - start).ok()?;
    let eff = clip.effect_offset_frames.checked_add(local_i64)?;
    let (eff_start, eff_last) = clip.effect_bounds();
    if eff_start < eff && eff < eff_last && !clip.midpoints.contains(&eff) {
        Some(eff)
    } else {
        None
    }
}

/// クリップ上のタイムライン座標から該当セグメントインデックス (`0..`) を求める。
#[must_use]
pub fn segment_index_at_frame(clip: &Clip, frame: FrameIndex) -> usize {
    let grid = clip.grid_frames();
    let seg_count = grid.len().saturating_sub(1).max(1);
    let local_i64 =
        i64::try_from(frame.get().saturating_sub(clip.timeline_start.get())).unwrap_or(i64::MAX);
    let eff = clip.effect_offset_frames.saturating_add(local_i64);
    for (idx, win) in grid.windows(2).enumerate() {
        if eff >= win[0] && (eff < win[1] || idx + 1 == seg_count) {
            return idx;
        }
    }
    0
}

/// 中間点エフェクト基準フレームからタイムライン絶対フレーム位置を求める。
#[must_use]
pub fn midpoint_timeline_frame(clip: &Clip, effect_frame: i64) -> FrameIndex {
    let rel = effect_frame.saturating_sub(clip.effect_offset_frames);
    let offset = u64::try_from(rel.max(0)).unwrap_or(0);
    let clamped = offset.min(clip.duration.get().saturating_sub(1));
    FrameIndex::new(clip.timeline_start.get().saturating_add(clamped))
}

/// クリップ右クリック時のメニュー可否を算出する。
#[must_use]
pub fn evaluate_clip_menu_availability(
    _doc: &ProjectDocument,
    clip: &Clip,
    layer_locked: bool,
    clicked_frame: FrameIndex,
    playhead: FrameIndex,
) -> ClipMenuAvailability {
    let start = clip.timeline_start.get();
    let end = start.saturating_add(clip.duration.get());
    let click_f = clicked_frame.get();
    let head_f = playhead.get();

    ClipMenuAvailability {
        split: SplitMidpointAvailability {
            can_split_at_click: !layer_locked && start < click_f && click_f < end,
            can_split_at_playhead: !layer_locked && start < head_f && head_f < end,
            can_add_midpoint_at_click: !layer_locked
                && midpoint_effect_frame_for_timeline_frame(clip, clicked_frame).is_some(),
        },
        mutation: ClipMutationAvailability {
            can_add_midpoint_at_playhead: !layer_locked
                && midpoint_effect_frame_for_timeline_frame(clip, playhead).is_some(),
            can_align_end_to_playhead: !layer_locked && head_f >= clip.duration.get(),
            layer_locked,
        },
    }
}

/// トラックレーン右クリック位置をコントローラへ記録する。
pub(crate) fn record_lane_right_click(
    view: &mut SeaEditorWindow,
    layer_id: LayerId,
    window_x: f32,
) {
    let frame = frame_at_timeline_window_x(
        window_x,
        view.controller.timeline_scroll_x_frames,
        view.controller.timeline_px_per_frame,
    )
    .unwrap_or_else(|| view.controller.clock.current_frame());
    view.controller.selected_layer_id = Some(layer_id.clone());
    view.controller.timeline_context_click = Some(TimelineContextClick {
        frame,
        layer_id: Some(layer_id),
        clip_id: None,
        segment_index: None,
        midpoint_effect_frame: None,
    });
}

/// クリップ右クリック時に位置記録と単一/グループ選択保持を行う。
pub(crate) fn record_clip_right_click(
    view: &mut SeaEditorWindow,
    clip_id: ClipId,
    window_x: f32,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    let frame = frame_at_timeline_window_x(
        window_x,
        view.controller.timeline_scroll_x_frames,
        view.controller.timeline_px_per_frame,
    )
    .unwrap_or_else(|| view.controller.clock.current_frame());

    let (layer_id, seg_idx) = view
        .controller
        .document
        .find_clip(&clip_id)
        .map_or((None, 0), |(layer, clip)| {
            (Some(layer.id.clone()), segment_index_at_frame(clip, frame))
        });

    let active_ids = view.controller.effective_selected_clip_ids();
    if !active_ids.contains(&clip_id) || active_ids.len() <= 1 {
        view.controller.select_clip_segment(&clip_id, seg_idx);
        view.controller.inspector.invalidate_sync();
        view.sync_sliders_now(window, cx);
    }

    view.controller.timeline_context_click = Some(TimelineContextClick {
        frame,
        layer_id,
        clip_id: Some(clip_id),
        segment_index: Some(seg_idx),
        midpoint_effect_frame: None,
    });
}

/// 中間点ひし形右クリック位置をコントローラへ記録する。
pub(crate) fn record_midpoint_right_click(
    view: &mut SeaEditorWindow,
    clip_id: ClipId,
    effect_frame: i64,
) {
    let (layer_id, timeline_frame) = view
        .controller
        .document
        .find_clip(&clip_id)
        .map_or((None, FrameIndex::ZERO), |(l, c)| {
            (Some(l.id.clone()), midpoint_timeline_frame(c, effect_frame))
        });
    view.controller.timeline_context_click = Some(TimelineContextClick {
        frame: timeline_frame,
        layer_id,
        clip_id: Some(clip_id),
        segment_index: None,
        midpoint_effect_frame: Some(effect_frame),
    });
}

/// ルーラー右クリック位置をコントローラへ記録する。
pub(crate) fn record_ruler_right_click(view: &mut SeaEditorWindow, window_x: f32) {
    let frame = frame_at_timeline_window_x(
        window_x,
        view.controller.timeline_scroll_x_frames,
        view.controller.timeline_px_per_frame,
    )
    .unwrap_or_else(|| view.controller.clock.current_frame());
    view.controller.timeline_context_click = Some(TimelineContextClick {
        frame,
        layer_id: None,
        clip_id: None,
        segment_index: None,
        midpoint_effect_frame: None,
    });
}

/// トラックヘッダ右クリック対象レイヤーをコントローラへ記録する。
pub(crate) fn record_header_right_click(view: &mut SeaEditorWindow, layer_id: LayerId) {
    view.controller.selected_layer_id = Some(layer_id.clone());
    view.controller.timeline_context_click = Some(TimelineContextClick {
        frame: view.controller.clock.current_frame(),
        layer_id: Some(layer_id),
        clip_id: None,
        segment_index: None,
        midpoint_effect_frame: None,
    });
}

fn finish_mutation(
    view: &mut SeaEditorWindow,
    window: &mut Window,
    cx: &mut Context<SeaEditorWindow>,
) {
    view.controller.inspector.invalidate_sync();
    view.sync_sliders_now(window, cx);
    view.reload_app_menus(cx);
    cx.notify();
}

/// 空きトラックレーンへ右クリックメニューを装着する。
pub(crate) fn attach_lane_context_menu<E: ContextMenuExt + 'static>(
    element: E,
    layer_id: LayerId,
    win: Entity<SeaEditorWindow>,
) -> ContextMenu<E> {
    element.context_menu(move |menu, window, cx| {
        let snap = win.read(cx);
        let frame = snap
            .controller
            .timeline_context_click
            .as_ref()
            .map_or_else(|| snap.controller.clock.current_frame(), |c| c.frame);
        let avail = evaluate_lane_menu_availability(&snap.controller, &layer_id, frame);
        build_lane_popup_menu(menu, &layer_id, avail, &win, window, cx)
    })
}

fn build_lane_popup_menu(
    menu: PopupMenu,
    layer_id: &LayerId,
    avail: LaneMenuAvailability,
    win: &Entity<SeaEditorWindow>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let lid_sub = layer_id.clone();
    let lid_file = layer_id.clone();
    let lid_paste = layer_id.clone();
    let frame = avail.clicked_frame;
    let frame_num = frame.get();
    let locked = avail.flags.layer_locked;

    let win_sub = win.clone();
    let win_file = win.clone();
    let win_paste = win.clone();

    let with_add = menu
        .submenu(
            "オブジェクトの追加",
            window,
            cx,
            move |sub, _, _| {
                let txt_layer = lid_sub.clone();
                let med_layer = lid_file.clone();
                let txt_win = win_sub.clone();
                let med_win = win_file.clone();
                sub.item(
                    PopupMenuItem::new(format!("テキスト・字幕 ({frame_num}f)"))
                        .disabled(locked)
                        .on_click(move |_, window, cx| {
                            txt_win.update(cx, |this, cx| {
                                if let Err(err) = this
                                    .controller
                                    .insert_default_subtitle_at(Some(&txt_layer), frame)
                                {
                                    this.controller.status_message = err;
                                }
                                finish_mutation(this, window, cx);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new("動画・画像・音声ファイル...")
                        .disabled(locked)
                        .on_click(move |_, _, cx| {
                            med_win.update(cx, |_, cx| {
                                SeaEditorWindow::prompt_import_media_at(
                                    Some(med_layer.clone()),
                                    Some(frame),
                                    cx,
                                );
                            });
                        }),
                )
                .separator()
                .item(PopupMenuItem::new("図形オブジェクト（準備中）").disabled(true))
                .item(PopupMenuItem::new("シーンオブジェクト（準備中）").disabled(true))
                .item(PopupMenuItem::new("カメラ制御（準備中）").disabled(true))
            },
        )
        .separator()
        .item(
            PopupMenuItem::new(format!("ここに貼り付け ({frame_num}f)"))
                .disabled(!avail.flags.has_clipboard || locked)
                .on_click(move |_, window, cx| {
                    win_paste.update(cx, |this, cx| {
                        if let Err(err) =
                            this.controller.paste_clipboard_at(Some(&lid_paste), frame)
                        {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        );

    append_range_and_toggle_items(with_add, avail, win)
}

fn append_range_and_toggle_items(
    menu: PopupMenu,
    avail: LaneMenuAvailability,
    win: &Entity<SeaEditorWindow>,
) -> PopupMenu {
    let frame = avail.clicked_frame;
    let frame_num = frame.get();
    let win_start = win.clone();
    let win_end = win.clone();
    let win_clear = win.clone();
    let win_auto = win.clone();
    let win_len = win.clone();
    let win_snap = win.clone();
    let win_follow = win.clone();
    let win_unit = win.clone();

    menu.separator()
        .item(
            PopupMenuItem::new(format!("このフレームを選択開始点 [ に設定 ({frame_num}f)"))
                .on_click(move |_, window, cx| {
                    win_start.update(cx, |this, cx| {
                        let _ = this.controller.set_selection_range_start_at(frame);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new(format!("このフレームを選択終了点 ] に設定 ({frame_num}f)"))
                .on_click(move |_, window, cx| {
                    win_end.update(cx, |this, cx| {
                        let _ = this.controller.set_selection_range_end_at(frame);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("選択範囲をクリア")
                .disabled(!avail.flags.has_selection_range)
                .on_click(move |_, window, cx| {
                    win_clear.update(cx, |this, cx| {
                        let _ = this.controller.set_selection_range(None);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new("最後のオブジェクト位置を終了点にする").on_click(
                move |_, window, cx| {
                    win_auto.update(cx, |this, cx| {
                        let _ = this
                            .controller
                            .set_timeline_length(TimelineLengthMode::AutoToLastClip);
                        finish_mutation(this, window, cx);
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new("タイムライン長を設定...").on_click(move |_, window, cx| {
                win_len.update(cx, |this, cx| {
                    this.open_timeline_length_modal(window, cx);
                });
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new("スナップを有効")
                .checked(avail.toggles.snap_enabled)
                .on_click(move |_, window, cx| {
                    win_snap.update(cx, |this, cx| {
                        this.controller.toggle_snap_enabled();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("シークで選択を追従")
                .checked(avail.toggles.follow_selected_on_seek)
                .on_click(move |_, window, cx| {
                    win_follow.update(cx, |this, cx| {
                        this.controller.toggle_follow_selected_on_seek();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("フレーム番号表示")
                .checked(avail.toggles.show_frames)
                .on_click(move |_, window, cx| {
                    win_unit.update(cx, |this, cx| {
                        this.controller.toggle_time_display_unit();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
}

/// クリップブロックへ右クリックメニューを装着する。
pub(crate) fn attach_clip_context_menu<E: ContextMenuExt + 'static>(
    element: E,
    clip_id: ClipId,
    win: Entity<SeaEditorWindow>,
) -> ContextMenu<E> {
    element.context_menu(move |menu, _, cx| {
        let snap = win.read(cx);
        let click_frame = snap
            .controller
            .timeline_context_click
            .as_ref()
            .map_or_else(|| snap.controller.clock.current_frame(), |c| c.frame);
        let playhead = snap.controller.clock.current_frame();
        let Some((layer, clip)) = snap.controller.document.find_clip(&clip_id) else {
            return menu;
        };
        let avail = evaluate_clip_menu_availability(
            &snap.controller.document,
            clip,
            layer.locked,
            click_frame,
            playhead,
        );
        let click_eff = midpoint_effect_frame_for_timeline_frame(clip, click_frame);
        build_clip_popup_menu(menu, &clip_id, click_frame, click_eff, avail, &win)
    })
}

fn build_clip_popup_menu(
    menu: PopupMenu,
    clip_id: &ClipId,
    click_frame: FrameIndex,
    click_eff: Option<i64>,
    avail: ClipMenuAvailability,
    win: &Entity<SeaEditorWindow>,
) -> PopupMenu {
    let locked = avail.mutation.layer_locked;
    let click_frame_num = click_frame.get();
    let split_click_clip = clip_id.clone();
    let split_head_clip = clip_id.clone();
    let mid_click_clip = clip_id.clone();
    let mid_head_clip = clip_id.clone();
    let win_split_click = win.clone();
    let win_split_head = win.clone();
    let win_mid_click = win.clone();
    let win_mid_head = win.clone();

    let top = menu
        .item(
            PopupMenuItem::new(format!("このクリック位置で分割 ({click_frame_num}f)"))
                .disabled(!avail.split.can_split_at_click)
                .on_click(move |_, window, cx| {
                    win_split_click.update(cx, |this, cx| {
                        if let Err(err) = this
                            .controller
                            .split_clip_at_frame(&split_click_clip, click_frame)
                        {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("再生位置で分割 (S)")
                .disabled(!avail.split.can_split_at_playhead)
                .on_click(move |_, window, cx| {
                    win_split_head.update(cx, |this, cx| {
                        let head = this.controller.clock.current_frame();
                        if let Err(err) =
                            this.controller.split_clip_at_frame(&split_head_clip, head)
                        {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new(format!("クリック位置へ中間点を追加 ({click_frame_num}f)"))
                .disabled(!avail.split.can_add_midpoint_at_click)
                .on_click(move |_, window, cx| {
                    if let Some(eff) = click_eff {
                        win_mid_click.update(cx, |this, cx| {
                            if let Err(err) = this.controller.add_midpoint(&mid_click_clip, eff) {
                                this.controller.status_message = err;
                            }
                            finish_mutation(this, window, cx);
                        });
                    }
                }),
        )
        .item(
            PopupMenuItem::new("再生位置へ中間点を追加 (/)")
                .disabled(!avail.mutation.can_add_midpoint_at_playhead)
                .on_click(move |_, window, cx| {
                    win_mid_head.update(cx, |this, cx| {
                        this.controller
                            .select_single_clip(Some(mid_head_clip.clone()));
                        if let Err(err) = this.controller.add_midpoint_at_playhead() {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        );

    append_clip_edit_and_align_items(
        top,
        clip_id,
        locked,
        avail.mutation.can_align_end_to_playhead,
        win,
    )
}

fn append_clip_edit_and_align_items(
    menu: PopupMenu,
    clip_id: &ClipId,
    locked: bool,
    can_align_end: bool,
    win: &Entity<SeaEditorWindow>,
) -> PopupMenu {
    let align_start_clip = clip_id.clone();
    let align_end_clip = clip_id.clone();
    let inspector_clip = clip_id.clone();
    let win_cut = win.clone();
    let win_copy = win.clone();
    let win_dup = win.clone();
    let win_del = win.clone();
    let win_align_start = win.clone();
    let win_align_end = win.clone();
    let win_inspector = win.clone();

    menu.separator()
        .item(
            PopupMenuItem::new("切り取り (Ctrl+X)")
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    win_cut.update(cx, |this, cx| {
                        let _ = this.controller.cut_selected_clips();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("コピー (Ctrl+C)").on_click(move |_, _, cx| {
                win_copy.update(cx, |this, cx| {
                    let _ = this.controller.copy_selected_clip();
                    cx.notify();
                });
            }),
        )
        .item(
            PopupMenuItem::new("複製 (Ctrl+D)")
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    win_dup.update(cx, |this, cx| {
                        let _ = this.controller.duplicate_selected_clip();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("削除 (Delete)")
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    win_del.update(cx, |this, cx| {
                        let _ = this.controller.delete_selected_clip();
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new("先頭を再生位置へ移動")
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    win_align_start.update(cx, |this, cx| {
                        if let Err(err) = this
                            .controller
                            .align_clip_start_to_playhead(&align_start_clip)
                        {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("末尾を再生位置へ揃える")
                .disabled(!can_align_end)
                .on_click(move |_, window, cx| {
                    win_align_end.update(cx, |this, cx| {
                        if let Err(err) =
                            this.controller.align_clip_end_to_playhead(&align_end_clip)
                        {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new("インスペクタを開く").on_click(move |_, window, cx| {
                win_inspector.update(cx, |this, cx| {
                    this.controller
                        .select_single_clip(Some(inspector_clip.clone()));
                    this.controller.right_tab = RightPanelTab::ClipInspector;
                    finish_mutation(this, window, cx);
                });
            }),
        )
}

/// 中間点ひし形へ右クリックメニューを装着する。
pub(crate) fn attach_midpoint_context_menu<E: ContextMenuExt + 'static>(
    element: E,
    clip_id: ClipId,
    effect_frame: i64,
    win: Entity<SeaEditorWindow>,
) -> ContextMenu<E> {
    element.context_menu(move |menu, _, cx| {
        let snap = win.read(cx);
        let Some((layer, clip)) = snap.controller.document.find_clip(&clip_id) else {
            return menu;
        };
        let locked = layer.locked;
        let seek_frame = midpoint_timeline_frame(clip, effect_frame);
        let seek_frame_num = seek_frame.get();

        let cid_rem = clip_id.clone();
        let cid_sel = clip_id.clone();
        let w_rem = win.clone();
        let w_seek = win.clone();
        let w_sel = win.clone();

        menu.item(
            PopupMenuItem::new(format!("中間点を削除 ({seek_frame_num}f)"))
                .disabled(locked)
                .on_click(move |_, window, cx| {
                    w_rem.update(cx, |this, cx| {
                        if let Err(err) = this.controller.remove_midpoint(&cid_rem, effect_frame) {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new(format!("この中間点へシーク ({seek_frame_num}f)")).on_click(
                move |_, window, cx| {
                    w_seek.update(cx, |this, cx| {
                        this.controller.seek_to_frame(seek_frame);
                        finish_mutation(this, window, cx);
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new("所属クリップを選択").on_click(move |_, window, cx| {
                w_sel.update(cx, |this, cx| {
                    this.controller.select_single_clip(Some(cid_sel.clone()));
                    finish_mutation(this, window, cx);
                });
            }),
        )
    })
}

/// ルーラーへ右クリックメニューを装着する。
pub(crate) fn attach_ruler_context_menu<E: ContextMenuExt + 'static>(
    element: E,
    win: Entity<SeaEditorWindow>,
) -> ContextMenu<E> {
    element.context_menu(move |menu, _, cx| {
        let snap = win.read(cx);
        let frame = snap
            .controller
            .timeline_context_click
            .as_ref()
            .map_or_else(|| snap.controller.clock.current_frame(), |c| c.frame);
        let is_frames = snap.controller.time_display_unit == TimeDisplayUnit::Frames;
        let has_sel = snap.controller.document.selection_range.is_some();
        build_ruler_popup_menu(menu, frame, is_frames, has_sel, &win)
    })
}

fn build_ruler_popup_menu(
    menu: PopupMenu,
    frame: FrameIndex,
    is_frames: bool,
    has_sel: bool,
    win: &Entity<SeaEditorWindow>,
) -> PopupMenu {
    let frame_num = frame.get();
    let win_seek = win.clone();
    let win_tc = win.clone();
    let win_fr = win.clone();
    let win_start = win.clone();
    let win_end = win.clone();
    let win_clear = win.clone();
    let win_auto = win.clone();
    let win_modal = win.clone();

    menu.item(
        PopupMenuItem::new(format!("このフレームへシーク ({frame_num}f)")).on_click(
            move |_, window, cx| {
                win_seek.update(cx, |this, cx| {
                    this.controller.seek_to_frame(frame);
                    finish_mutation(this, window, cx);
                });
            },
        ),
    )
    .separator()
    .item(
        PopupMenuItem::new("時間表示: タイムコード")
            .checked(!is_frames)
            .on_click(move |_, _, cx| {
                win_tc.update(cx, |this, cx| {
                    this.controller.time_display_unit = TimeDisplayUnit::Timecode;
                    cx.notify();
                });
            }),
    )
    .item(
        PopupMenuItem::new("時間表示: フレーム番号")
            .checked(is_frames)
            .on_click(move |_, _, cx| {
                win_fr.update(cx, |this, cx| {
                    this.controller.time_display_unit = TimeDisplayUnit::Frames;
                    cx.notify();
                });
            }),
    )
    .separator()
    .item(
        PopupMenuItem::new(format!("ここを選択開始点 [ に設定 ({frame_num}f)")).on_click(
            move |_, window, cx| {
                win_start.update(cx, |this, cx| {
                    let _ = this.controller.set_selection_range_start_at(frame);
                    finish_mutation(this, window, cx);
                });
            },
        ),
    )
    .item(
        PopupMenuItem::new(format!("ここを選択終了点 ] に設定 ({frame_num}f)")).on_click(
            move |_, window, cx| {
                win_end.update(cx, |this, cx| {
                    let _ = this.controller.set_selection_range_end_at(frame);
                    finish_mutation(this, window, cx);
                });
            },
        ),
    )
    .item(
        PopupMenuItem::new("選択範囲をクリア")
            .disabled(!has_sel)
            .on_click(move |_, window, cx| {
                win_clear.update(cx, |this, cx| {
                    let _ = this.controller.set_selection_range(None);
                    finish_mutation(this, window, cx);
                });
            }),
    )
    .separator()
    .item(
        PopupMenuItem::new("最後のクリップ末尾を終了点にする").on_click(move |_, window, cx| {
            win_auto.update(cx, |this, cx| {
                let _ = this
                    .controller
                    .set_timeline_length(TimelineLengthMode::AutoToLastClip);
                finish_mutation(this, window, cx);
            });
        }),
    )
    .item(
        PopupMenuItem::new("タイムライン長を設定...").on_click(move |_, window, cx| {
            win_modal.update(cx, |this, cx| {
                this.open_timeline_length_modal(window, cx);
            });
        }),
    )
}

/// トラック左端ヘッダへ右クリックメニューを装着する。
pub(crate) fn attach_track_header_context_menu<E: ContextMenuExt + 'static>(
    element: E,
    layer_id: LayerId,
    layer_idx: usize,
    win: Entity<SeaEditorWindow>,
) -> ContextMenu<E> {
    element.context_menu(move |menu, _, cx| {
        let snap = win.read(cx);
        let Some(layer) = snap.controller.document.find_layer(&layer_id) else {
            return menu;
        };
        let layer_count = snap.controller.document.layers.len();
        let visible = layer.visible;
        let muted = layer.muted;
        let locked = layer.locked;
        let ren_name = layer.name.clone();

        let lid_ren = layer_id.clone();
        let lid_vis = layer_id.clone();
        let lid_mut = layer_id.clone();
        let lid_lck = layer_id.clone();
        let lid_del = layer_id.clone();

        let w_ren = win.clone();
        let w_above = win.clone();
        let w_below = win.clone();
        let w_vis = win.clone();
        let w_mut = win.clone();
        let w_lck = win.clone();
        let w_del = win.clone();

        menu.item(
            PopupMenuItem::new("トラック名を変更...").on_click(move |_, window, cx| {
                let initial = ren_name.clone();
                w_ren.update(cx, |this, cx| {
                    this.begin_layer_rename(lid_ren.clone(), &initial, window, cx);
                });
            }),
        )
        .item(
            PopupMenuItem::new("上にトラックを挿入").on_click(move |_, window, cx| {
                w_above.update(cx, |this, cx| {
                    let _ = this.controller.insert_layer_at(layer_idx, None);
                    finish_mutation(this, window, cx);
                });
            }),
        )
        .item(
            PopupMenuItem::new("下にトラックを挿入").on_click(move |_, window, cx| {
                w_below.update(cx, |this, cx| {
                    let _ = this.controller.insert_layer_at(layer_idx + 1, None);
                    finish_mutation(this, window, cx);
                });
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new("表示 / 非表示")
                .checked(visible)
                .on_click(move |_, window, cx| {
                    w_vis.update(cx, |this, cx| {
                        let _ = this.controller.toggle_layer_visible(&lid_vis);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("ミュート")
                .checked(muted)
                .on_click(move |_, window, cx| {
                    w_mut.update(cx, |this, cx| {
                        let _ = this.controller.toggle_layer_muted(&lid_mut);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("トラックをロック")
                .checked(locked)
                .on_click(move |_, window, cx| {
                    w_lck.update(cx, |this, cx| {
                        let _ = this.controller.toggle_layer_locked(&lid_lck);
                        finish_mutation(this, window, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new("このトラックを削除")
                .disabled(locked || layer_count <= 1)
                .on_click(move |_, window, cx| {
                    w_del.update(cx, |this, cx| {
                        if let Err(err) = this.controller.remove_layer(&lid_del) {
                            this.controller.status_message = err;
                        }
                        finish_mutation(this, window, cx);
                    });
                }),
        )
    })
}
