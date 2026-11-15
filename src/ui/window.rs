//! GPUI シングルウィンドウシェル (`SeaEditorWindow`) とアプリ起動エントリ (`launch_app`)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode, TitleBar, VirtualListScrollHandle, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Bounds, Context, Div, Entity, InteractiveElement as _, IntoElement,
    MouseButton, MouseMoveEvent, ParentElement as _, PathPromptOptions, Render, SharedString,
    Styled as _, Subscription, Window, WindowBounds, WindowOptions, div, px, size,
};

use crate::core::ids::LayerId;
use crate::core::time::{FrameCount, FrameIndex};
use crate::timeline::document::TimelineLengthMode;
use crate::ui::ai_chat_panel::{
    AiProviderSelectItem, build_provider_select_items, provider_select_index,
};
use crate::ui::app_controller::{AppController, clamp_right_panel_width};
use crate::ui::export_dialog::render_export_dialog_overlay;
use crate::ui::inspector_panel::InspectorWidgetBank;
use crate::ui::menu_bar::{
    CopySelectedClip, DeleteSelectedClip, DuplicateSelectedClip, ImportMedia, NewProject,
    OpenExportDialog, OpenProject, PasteAtPlayhead, QuitApp, RedoAction, SaveProject,
    SaveProjectAs, SeekStart, SplitAtPlayhead, TogglePlayback, UndoAction, setup_menus,
};
use crate::ui::preview_panel::render_preview_panel;
use crate::ui::right_panel::{RightPanelWidgets, render_right_panel};
use crate::ui::timeline_panel::{
    TRACK_HEADER_WIDTH, handle_timeline_window_mouse_move, handle_timeline_window_mouse_up,
    parse_timeline_length_input, render_timeline_length_modal_overlay, render_timeline_panel,
};

const DEFAULT_WINDOW_WIDTH: f32 = 1280.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 800.0;
const TICK_INTERVAL_MS: u64 = 16;

/// 手動タイムライン総尺指定モーダルの一時状態。
pub(crate) struct TimelineLengthModalState {
    /// 手動指定モードが選択されているか。
    pub is_manual: bool,
    /// 手動総尺入力欄のテキスト状態。
    pub input: Entity<InputState>,
}

/// `sea-movie-creator` のメインエディタウィンドウビュー。
pub struct SeaEditorWindow {
    /// アプリ状態・タイムライン・再生エンジン統括コントローラ。
    pub controller: AppController,
    menu_bar: Entity<AppMenuBar>,
    scrub_slider: Entity<SliderState>,
    /// 右インスペクタ全ウィジェット群。
    pub(crate) inspector_widgets: InspectorWidgetBank,
    /// AI API / プロバイダ切り替えコンボボックス状態。
    pub(crate) ai_provider_select: Entity<SelectState<Vec<AiProviderSelectItem>>>,
    /// AI チャット欄のテキスト入力状態。
    pub(crate) ai_chat_input: Entity<InputState>,
    pub(crate) timeline_scroll: VirtualListScrollHandle,
    /// インライン名前変更中の対象レイヤー ID。
    pub(crate) layer_rename_target: Option<LayerId>,
    /// レイヤー名インライン編集用テキスト入力状態。
    pub(crate) layer_rename_input: Option<Entity<InputState>>,
    /// タイムライン長変更モーダルの状態（`None` なら閉）。
    pub(crate) timeline_length_modal: Option<TimelineLengthModalState>,
    #[allow(dead_code)]
    dynamic_input_sub: Option<Subscription>,
    sliders_dirty: bool,
    pub(crate) syncing_widgets: bool,
    _subscriptions: Vec<Subscription>,
}

impl SeaEditorWindow {
    /// ウィンドウと各子ウィジェット・再生ループを構築する。
    pub fn new(
        menu_bar: Entity<AppMenuBar>,
        open_project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut controller = AppController::new();
        controller.try_start_audio_output();
        if let Some(path) = open_project
            && let Err(err) = controller.open_project_file(&path)
        {
            controller.status_message = format!("プロジェクト読込エラー: {err}");
        }

        let scrub_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(100.0)
                .step(0.5)
                .default_value(0.0)
        });
        let initial_provider_idx = provider_select_index(&controller.ai_chat.selected_provider);
        let ai_provider_select = cx.new(|cx| {
            SelectState::new(
                build_provider_select_items(),
                initial_provider_idx,
                window,
                cx,
            )
        });
        let ai_chat_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("AI 編集指示を入力 (Enter で送信)...")
        });

        let mut subscriptions = vec![
            cx.subscribe_in(&scrub_slider, window, Self::on_scrub_slider),
            cx.subscribe_in(&ai_provider_select, window, Self::on_ai_provider_select),
            cx.subscribe_in(&ai_chat_input, window, Self::on_ai_chat_input_event),
        ];
        let inspector_widgets = InspectorWidgetBank::new(window, cx, &mut subscriptions);

        let mut this = Self {
            controller,
            menu_bar,
            scrub_slider,
            inspector_widgets,
            ai_provider_select,
            ai_chat_input,
            timeline_scroll: VirtualListScrollHandle::new(),
            layer_rename_target: None,
            layer_rename_input: None,
            timeline_length_modal: None,
            dynamic_input_sub: None,
            sliders_dirty: true,
            syncing_widgets: false,
            _subscriptions: subscriptions,
        };
        this.sync_sliders_now(window, cx);
        Self::spawn_playback_loop(cx);
        this
    }

    /// 指定レイヤー名のインライン編集を開始する（ダブルクリック時）。
    pub(crate) fn begin_layer_rename(
        &mut self,
        layer_id: LayerId,
        current_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current_name));
        input.update(cx, |state, cx| state.focus(window, cx));
        let sub =
            cx.subscribe_in(
                &input,
                window,
                |this, entity, event: &InputEvent, _, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur => {
                        let val = entity.read(cx).value().to_string();
                        this.commit_layer_rename(&val, cx);
                    }
                    _ => {}
                },
            );
        self.layer_rename_target = Some(layer_id);
        self.layer_rename_input = Some(input);
        self.dynamic_input_sub = Some(sub);
        cx.notify();
    }

    fn commit_layer_rename(&mut self, raw_name: &str, cx: &mut Context<Self>) {
        if let Some(layer_id) = self.layer_rename_target.take() {
            let trimmed = raw_name.trim();
            if !trimmed.is_empty()
                && let Err(err) = self.controller.rename_layer(&layer_id, trimmed)
            {
                self.controller.status_message = format!("レイヤー名変更失敗: {err}");
            }
        }
        self.layer_rename_input = None;
        self.dynamic_input_sub = None;
        cx.notify();
    }

    /// タイムライン長変更モーダルを開く。
    pub(crate) fn open_timeline_length_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (is_manual, initial_text) = match self.controller.document.timeline_length {
            TimelineLengthMode::AutoToLastClip => (
                false,
                format!(
                    "{}f",
                    self.controller.document.total_duration_frames().max(1)
                ),
            ),
            TimelineLengthMode::Manual { end_frame } => (true, format!("{}f", end_frame.get())),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("フレーム数 / 10s / HH:MM:SS:FF")
                .default_value(initial_text)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                if let Some(modal) = &mut this.timeline_length_modal {
                    modal.is_manual = true;
                }
                this.apply_timeline_length_modal(window, cx);
            }
        });
        self.timeline_length_modal = Some(TimelineLengthModalState { is_manual, input });
        self.dynamic_input_sub = Some(sub);
        cx.notify();
    }

    /// モーダルの設定を `EditCommand::SetTimelineLength` として適用する。
    pub(crate) fn apply_timeline_length_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = match self.timeline_length_modal.as_ref() {
            Some(modal) if modal.is_manual => {
                let raw = modal.input.read(cx).value().to_string();
                match parse_timeline_length_input(&raw, self.controller.document.fps) {
                    Ok(frames) => match FrameCount::new(frames) {
                        Ok(end_frame) => TimelineLengthMode::Manual { end_frame },
                        Err(err) => {
                            self.controller.status_message = err.to_string();
                            cx.notify();
                            return;
                        }
                    },
                    Err(err) => {
                        self.controller.status_message = err;
                        cx.notify();
                        return;
                    }
                }
            }
            _ => TimelineLengthMode::AutoToLastClip,
        };

        if let Err(err) = self.controller.set_timeline_length(mode) {
            self.controller.status_message = err;
        }
        self.close_timeline_length_modal();
        self.mark_sliders_dirty();
        self.sync_sliders_now(window, cx);
        cx.notify();
    }

    /// タイムライン長変更モーダルを閉じる。
    pub(crate) fn close_timeline_length_modal(&mut self) {
        self.timeline_length_modal = None;
        self.dynamic_input_sub = None;
    }

    /// 次回描画時にスライダ表示をコントローラ状態へ同期するよう印を付ける。
    pub fn mark_sliders_dirty(&mut self) {
        self.sliders_dirty = true;
    }

    /// コントローラ状態からスライダ類の現在値を即時同期する。
    pub fn sync_sliders_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sliders_dirty = false;
        self.syncing_widgets = true;
        let total = self.controller.document.total_duration_frames().max(1);
        let head = self.controller.clock.current_frame().get().min(total);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let pct = ((head as f64) / (total as f64) * 100.0) as f32;
        self.scrub_slider.update(cx, |slider, cx| {
            if (slider.value().end() - pct).abs() > 0.05 {
                slider.set_value(pct, window, cx);
            }
        });

        self.inspector_widgets
            .sync_from_controller(&mut self.controller, window, cx);
        self.syncing_widgets = false;
    }

    fn spawn_playback_loop(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut last_tick = Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(TICK_INTERVAL_MS))
                    .await;
                let now = Instant::now();
                let elapsed = (now - last_tick).as_secs_f64();
                last_tick = now;

                let keep_going = this.update(cx, |view, cx| {
                    let prev_frame = view.controller.clock.current_frame().get();
                    view.controller.tick_playback(elapsed);
                    let export_changed = view.controller.poll_export_status();
                    let ai_changed = view.controller.poll_ai_chat();
                    if view.controller.clock.current_frame().get() != prev_frame
                        || export_changed
                        || ai_changed
                    {
                        view.sliders_dirty = true;
                        cx.notify();
                    }
                });
                if keep_going.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// AI チャット入力欄の現在文字列を送信し、受理された場合は入力欄を空にする。
    pub(crate) fn submit_ai_chat_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.controller.ai_chat.is_busy() {
            return;
        }
        let msg = self.ai_chat_input.read(cx).value().to_string();
        if msg.trim().is_empty() {
            return;
        }
        match self.controller.start_ai_prompt_async(&msg) {
            Ok(()) => {
                self.ai_chat_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
            }
            Err(err) => {
                self.controller.ai_chat.last_error = Some(err);
            }
        }
        cx.notify();
    }

    fn on_ai_provider_select(
        &mut self,
        _: &Entity<SelectState<Vec<AiProviderSelectItem>>>,
        event: &SelectEvent<Vec<AiProviderSelectItem>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SelectEvent::Confirm(Some(kind)) = event
            && &self.controller.ai_chat.selected_provider != kind
        {
            self.controller.ai_chat.select_provider(kind.clone());
            cx.notify();
        }
    }

    fn on_ai_chat_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
            self.submit_ai_chat_input(window, cx);
        }
    }

    fn on_scrub_slider(
        &mut self,
        _: &Entity<SliderState>,
        event: &SliderEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.syncing_widgets {
            return;
        }
        let (SliderEvent::Change(val) | SliderEvent::Release(val)) = event;
        let total = self.controller.document.total_duration_frames();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let target = ((f64::from(val.end()) / 100.0) * (total as f64))
            .round()
            .max(0.0) as u64;
        self.controller.seek_to_frame(FrameIndex::new(target));
        cx.notify();
    }

    fn on_new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        self.controller.new_project();
        self.sync_sliders_now(window, cx);
        cx.notify();
    }

    #[allow(clippy::unused_self)] // GPUI cx.listener シグネチャ要件のため &mut self を維持
    fn on_open_project(&mut self, _: &OpenProject, _: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("プロジェクトファイル (.json) を選択")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| {
                    if let Err(err) = view.controller.open_project_file(&path) {
                        view.controller.status_message = format!("読込失敗: {err}");
                    }
                    view.sliders_dirty = true;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    #[allow(clippy::unused_self)] // GPUI cx.listener シグネチャ要件のため &mut self を維持
    fn on_import_media(&mut self, _: &ImportMedia, _: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("動画・音声・静止画ファイルを選択")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| {
                    if let Err(err) = view.controller.import_media_file(&path) {
                        view.controller.status_message = format!("素材追加失敗: {err}");
                    }
                    view.sliders_dirty = true;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_save_project(&mut self, _: &SaveProject, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(existing) = self.controller.project_path.clone() {
            if let Err(err) = self.controller.save_project_file(&existing) {
                self.controller.status_message = format!("保存失敗: {err}");
            }
            cx.notify();
        } else {
            self.on_save_project_as(&SaveProjectAs, window, cx);
        }
    }

    #[allow(clippy::unused_self)] // GPUI cx.listener シグネチャ要件のため &mut self を維持
    fn on_save_project_as(&mut self, _: &SaveProjectAs, _: &mut Window, cx: &mut Context<Self>) {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("project.smc.json"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = this.update(cx, |view, cx| {
                    if let Err(err) = view.controller.save_project_file(&path) {
                        view.controller.status_message = format!("保存失敗: {err}");
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl SeaEditorWindow {
    /// 子ウィジェット用：不透明度変更時など同期を要求する。
    pub(crate) fn sync_widgets_from_controller(&mut self, cx: &mut Context<Self>) {
        self.sliders_dirty = true;
        cx.notify();
    }
}

impl SeaEditorWindow {
    fn build_workspace_split(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let win_w = f32::from(window.viewport_size().width).max(DEFAULT_WINDOW_WIDTH);
        let default_right_w = clamp_right_panel_width(win_w, self.controller.right_panel_width);
        self.controller.timeline_viewport_canvas_width_px =
            (win_w - default_right_w - TRACK_HEADER_WIDTH).max(200.0);

        let left_area = v_resizable("main-left-vertical")
            .child(
                resizable_panel()
                    .size(px(460.))
                    .size_range(px(220.)..px(1600.))
                    .child(render_preview_panel(
                        &mut self.controller,
                        &self.scrub_slider,
                        window,
                        cx,
                    )),
            )
            .child(
                resizable_panel()
                    .size(px(240.))
                    .size_range(px(140.)..px(700.))
                    .child(render_timeline_panel(
                        &self.controller,
                        &self.timeline_scroll,
                        cx,
                    )),
            );

        let win_entity = cx.entity().clone();
        h_resizable("main-workspace-split")
            .on_resize(move |state: &Entity<ResizableState>, _, cx| {
                let sizes = state.read(cx).sizes();
                if let Some(&right_px) = sizes.get(1) {
                    let right_f32: f32 = right_px / px(1.0);
                    win_entity.update(cx, |this, cx| {
                        let clamped = clamp_right_panel_width(win_w, Some(right_f32));
                        if this
                            .controller
                            .right_panel_width
                            .is_none_or(|prev| (prev - clamped).abs() > 0.5)
                        {
                            this.controller.right_panel_width = Some(clamped);
                            cx.notify();
                        }
                    });
                }
            })
            .child(resizable_panel().child(left_area))
            .child(
                resizable_panel()
                    .fixed()
                    .size(px(default_right_w))
                    .size_range(px(220.)..px(560.))
                    .child(render_right_panel(
                        &self.controller,
                        RightPanelWidgets {
                            inspector_bank: &self.inspector_widgets,
                            ai_provider_select: &self.ai_provider_select,
                            ai_chat_input: &self.ai_chat_input,
                        },
                        cx,
                    )),
            )
    }

    fn bind_actions(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(Self::on_new_project))
            .on_action(cx.listener(Self::on_open_project))
            .on_action(cx.listener(Self::on_import_media))
            .on_action(cx.listener(Self::on_save_project))
            .on_action(cx.listener(Self::on_save_project_as))
            .on_action(cx.listener(|this, _: &OpenExportDialog, _, cx| {
                this.controller.open_export_dialog();
                cx.notify();
            }))
            .on_action(|_: &QuitApp, _, cx| cx.quit())
            .on_action(cx.listener(|this, _: &UndoAction, window, cx| {
                let _ = this.controller.undo();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RedoAction, window, cx| {
                let _ = this.controller.redo();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SplitAtPlayhead, window, cx| {
                let _ = this.controller.split_selected_clip_at_playhead();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CopySelectedClip, _, cx| {
                let _ = this.controller.copy_selected_clip();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PasteAtPlayhead, window, cx| {
                let _ = this.controller.paste_clipboard_at_playhead();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DuplicateSelectedClip, window, cx| {
                let _ = this.controller.duplicate_selected_clip();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteSelectedClip, window, cx| {
                let _ = this.controller.delete_selected_clip();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TogglePlayback, _, cx| {
                this.controller.toggle_play_pause();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SeekStart, window, cx| {
                this.controller.seek_to_frame(FrameIndex::ZERO);
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
    }
}

impl Render for SeaEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sliders_dirty {
            self.sync_sliders_now(window, cx);
        }

        let theme = cx.theme().clone();
        let top_bar = TitleBar::new().child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child("sea-movie-creator"),
                )
                .child(div().flex_1().child(self.menu_bar.clone())),
        );
        let workspace = self.build_workspace_split(window, cx);

        let root = Self::bind_actions(v_flex().size_full().relative().bg(theme.background), cx)
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if this.controller.trackbar_drag.is_some() {
                    let mx: f32 = ev.position.x / px(1.0);
                    this.controller.update_trackbar_drag(mx, ev.modifiers.shift);
                    this.sliders_dirty = true;
                    cx.notify();
                } else if this.controller.timeline_gesture.is_some()
                    || this.controller.timeline_drag.is_some()
                {
                    handle_timeline_window_mouse_move(this, ev, window, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.controller.trackbar_drag.is_some() {
                        if this.controller.finish_trackbar_drag().is_ok() {
                            this.controller.inspector.invalidate_sync();
                            this.sync_sliders_now(window, cx);
                        }
                        cx.notify();
                    } else if this.controller.timeline_gesture.is_some()
                        || this.controller.timeline_drag.is_some()
                    {
                        handle_timeline_window_mouse_up(this, window, cx);
                    }
                }),
            )
            .child(top_bar)
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(workspace),
            );
        if self.controller.export_dialog.is_open {
            root.child(render_export_dialog_overlay(
                &self.controller,
                &self.controller.export_dialog,
                cx,
            ))
        } else if self.timeline_length_modal.is_some() {
            root.child(render_timeline_length_modal_overlay(self, cx))
        } else {
            root
        }
    }
}

/// タイトルバーのウィンドウ操作アイコン等を含む組み込みアセットソースを返す。
#[must_use]
fn editor_assets() -> gpui_kit::assets::Assets {
    gpui_kit::assets::Assets
}

/// GPUI アプリケーションを起動し、メインエディタウィンドウを表示する。
#[must_use]
pub fn launch_app(open_project: Option<PathBuf>, auto_close_ms: Option<u64>) -> i32 {
    gpui_kit::application()
        .with_assets(editor_assets())
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Dark, None, cx);
            let menu_bar = setup_menus(cx);

            let bounds = Bounds::centered(
                None,
                size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
                cx,
            );
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: true,
                show: true,
                ..TitleBar::window_options()
            };

            if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| SeaEditorWindow::new(menu_bar, open_project, window, cx))
            }) {
                eprintln!("failed to open window: {err}");
                cx.quit();
                return;
            }

            if let Some(ms) = auto_close_ms {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(ms))
                        .await;
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
    0
}

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;

    use super::editor_assets;

    #[test]
    fn editor_assets_provides_all_window_control_svgs() -> Result<(), Box<dyn std::error::Error>> {
        let assets = editor_assets();
        for path in [
            "icons/window-minimize.svg",
            "icons/window-maximize.svg",
            "icons/window-restore.svg",
            "icons/window-close.svg",
            "icons/check.svg",
            "icons/chevron-down.svg",
            "icons/chevron-right.svg",
        ] {
            let bytes = assets
                .load(path)?
                .ok_or_else(|| format!("missing bundled icon asset: {path}"))?;
            let svg = std::str::from_utf8(bytes.as_ref())?;
            assert!(
                svg.contains("<svg"),
                "expected SVG markup in {path}, got {svg}"
            );
        }
        Ok(())
    }
}
