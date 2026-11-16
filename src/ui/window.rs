//! GPUI シングルウィンドウシェル (`SeaEditorWindow`) とアプリ起動エントリ (`launch_app`)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode, TitleBar, VirtualListScrollHandle, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, Div, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton,
    MouseMoveEvent, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Window, WindowBounds, WindowOptions, div, px, size,
};

use crate::core::ids::{AssetId, LayerId};
use crate::core::time::{FrameCount, FrameIndex};
use crate::timeline::document::TimelineLengthMode;
use crate::ui::ai_chat_panel::{
    AiProviderSelectItem, build_provider_select_items, provider_select_index,
};
use crate::ui::app_controller::{
    AppController, PreviewZoomMode, RightPanelTab, clamp_right_panel_width,
};
use crate::ui::export_dialog::render_export_dialog_overlay;
use crate::ui::external_tools_modal::{
    ExternalToolsModalState, render_external_tools_modal_overlay,
};
use crate::ui::inspector_panel::InspectorWidgetBank;
use crate::ui::key_config::{
    KeyConfig, KeyConfigModalState, keystroke_from_gpui_event, render_key_config_modal_overlay,
};
use crate::ui::menu_bar::{
    AddMidpointAction, AddSubtitleAction, ClearSelectionRange, CopyClip, CutClip, CycleClipAction,
    DeleteClip, DuplicateClip, ExportSelectionDialogAction, ImportMedia, MenuCheckState,
    NewProject, NextBoundaryAction, NudgeDownAction, NudgeLeftAction, NudgeRightAction,
    NudgeUpAction, OpenExportDialog, OpenExternalToolsAction, OpenKeyConfigAction, OpenProject,
    PasteClip, PrevBoundaryAction, PreviewOverlayChecks, PreviewZoom50Action, PreviewZoom100Action,
    PreviewZoom200Action, PreviewZoomFitAction, QuitApp, RedoAction, RemoveMidpointAction,
    RotateLeftAction, RotateRightAction, SaveProject, SaveProjectAs, SeekEnd, SeekStart,
    SelectAllClips, SetSelectionEndAction, SetSelectionStartAction, SetTimelineLengthAction,
    ShowTabAiChatAction, ShowTabClipInspectorAction, ShowTabLayerListAction,
    ShowTabProjectInfoAction, SnapTimelineLengthAutoAction, SplitClip, StepBackHalfSecAction,
    StepBackOneFrame, StepForwardHalfSecAction, StepForwardOneFrame, StopPlaybackAction,
    TimelineZoomIn, TimelineZoomOut, ToggleFollowSelectedOnSeekAction, TogglePlayPause,
    TogglePreviewGridAction, TogglePreviewGuidesAction, TogglePreviewWaveformAction, UndoAction,
    apply_key_config_bindings, setup_menus, update_app_menus,
};
use crate::ui::preview::DragModifiers;
use crate::ui::preview_panel::render_preview_panel;
use crate::ui::right_panel::{RightPanelWidgets, render_right_panel};
use crate::ui::surface_bridge::PreviewSurfaceBridge as _;
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

/// プレビューキャンバス／音声波形バーに対する一時的なポインタ・修飾入力状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum PreviewPointerInputMode {
    /// 通常状態。
    #[default]
    Idle,
    /// `Space` キー押下中（左ドラッグでキャンバスパン）。
    SpacePanHeld,
    /// マスター音声波形ストリップを左ドラッグスクラブ中。
    WaveformScrubbing,
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
    pub(crate) ai_chat_input: Entity<TextareaState>,
    pub(crate) timeline_scroll: VirtualListScrollHandle,
    /// インライン名前変更中の対象レイヤー ID。
    pub(crate) layer_rename_target: Option<LayerId>,
    /// レイヤー名インライン編集用テキスト入力状態。
    pub(crate) layer_rename_input: Option<Entity<InputState>>,
    /// タイムライン長変更モーダルの状態（`None` なら閉）。
    pub(crate) timeline_length_modal: Option<TimelineLengthModalState>,
    /// 外部ツール (`ffmpeg`) 設定モーダルの状態（`None` なら閉）。
    pub(crate) external_tools_modal: Option<ExternalToolsModalState>,
    /// ショートカット設定モーダルの状態（`None` なら閉）。
    pub(crate) key_config_modal: Option<KeyConfigModalState>,
    /// 非テキスト編集領域（プレビュー・タイムライン等）用の既定フォーカスハンドル。
    pub(crate) workspace_focus: FocusHandle,
    /// プレビュー領域ポインタ・パン修飾入力モード。
    pub(crate) preview_pointer_mode: PreviewPointerInputMode,
    #[allow(dead_code)]
    pub(crate) dynamic_input_sub: Option<Subscription>,
    pub(crate) modal_focus_subs: Vec<Subscription>,
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
            TextareaState::new(window, cx)
                .auto_grow(1, 5)
                .submit_on_enter(true)
                .placeholder("編集指示を入力 (Enter で送信 / Shift+Enter で改行)")
        });

        let mut subscriptions = vec![
            cx.subscribe_in(&scrub_slider, window, Self::on_scrub_slider),
            cx.subscribe_in(&ai_provider_select, window, Self::on_ai_provider_select),
            cx.subscribe_in(&ai_chat_input, window, Self::on_ai_chat_input_event),
        ];
        let inspector_widgets = InspectorWidgetBank::new(window, cx, &mut subscriptions);
        let workspace_focus = cx.focus_handle();

        // 全入力欄のフォーカス／ブラー時に再描画し、`.key_context` を即座に切替える
        let ai_fh = ai_chat_input.read(cx).focus_handle(cx);
        subscriptions.push(cx.on_focus_in(&ai_fh, window, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_blur(&ai_fh, window, |_, _, cx| cx.notify()));
        let sub_fh = inspector_widgets
            .subtitle_textarea
            .read(cx)
            .focus_handle(cx);
        subscriptions.push(cx.on_focus_in(&sub_fh, window, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_blur(&sub_fh, window, |_, _, cx| cx.notify()));
        for input in inspector_widgets.all_text_inputs() {
            let fh = input.read(cx).focus_handle(cx);
            subscriptions.push(cx.on_focus_in(&fh, window, |_, _, cx| cx.notify()));
            subscriptions.push(cx.on_blur(&fh, window, |_, _, cx| cx.notify()));
        }
        window.focus(&workspace_focus, cx);

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
            external_tools_modal: None,
            key_config_modal: None,
            workspace_focus,
            preview_pointer_mode: PreviewPointerInputMode::Idle,
            dynamic_input_sub: None,
            modal_focus_subs: Vec::new(),
            sliders_dirty: true,
            syncing_widgets: false,
            _subscriptions: subscriptions,
        };
        this.reload_app_menus(cx);
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
                    let prev_surface_id = view.controller.surface_bridge.current_frame_id();
                    let prev_play_state = view.controller.playback_state;
                    view.controller.tick_playback(elapsed);
                    let export_changed = view.controller.poll_export_status();
                    let ai_changed = view.controller.poll_ai_chat();
                    let waveform_changed = view.controller.tick_master_waveform();
                    if view.controller.clock.current_frame().get() != prev_frame
                        || view.controller.surface_bridge.current_frame_id() != prev_surface_id
                        || view.controller.playback_state != prev_play_state
                        || export_changed
                        || ai_changed
                        || waveform_changed
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
        _: &Entity<TextareaState>,
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
        Self::prompt_import_media_at(None, None, cx);
    }

    /// 動画・画像・音声ファイルを選択し、任意の位置・レイヤー（未指定時はプレイヘッド基準）へ追加する。
    pub(crate) fn prompt_import_media_at(
        layer_id: Option<LayerId>,
        target_frame: Option<FrameIndex>,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(SharedString::from("動画・音声・静止画ファイルを選択")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && !paths.is_empty()
            {
                let _ = this.update(cx, |view, cx| {
                    let result = if let Some(frame) = target_frame {
                        view.controller
                            .import_media_files_at_drop(&paths, layer_id.as_ref(), Some(frame))
                            .map(|_| ())
                    } else if paths.len() == 1 {
                        view.controller.import_media_file(&paths[0]).map(|_| ())
                    } else {
                        let head = view.controller.clock.current_frame();
                        view.controller
                            .import_media_files_at_drop(&paths, layer_id.as_ref(), Some(head))
                            .map(|_| ())
                    };
                    if let Err(err) = result {
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

    /// 現在いずれかのテキスト・数値入力欄へフォーカスが入っているかを返す。
    #[must_use]
    pub fn is_any_text_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.ai_chat_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
            || self
                .layer_rename_input
                .as_ref()
                .is_some_and(|i| i.read(cx).focus_handle(cx).is_focused(window))
            || self
                .timeline_length_modal
                .as_ref()
                .is_some_and(|m| m.input.read(cx).focus_handle(cx).is_focused(window))
            || self
                .external_tools_modal
                .as_ref()
                .is_some_and(|m| m.input.read(cx).focus_handle(cx).is_focused(window))
            || self.inspector_widgets.is_any_input_focused(window, cx)
    }

    #[must_use]
    fn allow_editor_shortcut(&self, window: &Window, cx: &App) -> bool {
        !self.is_any_text_input_focused(window, cx)
            && self.key_config_modal.is_none()
            && self.external_tools_modal.is_none()
            && !self.controller.export_dialog.is_open
            && self.timeline_length_modal.is_none()
    }

    /// コントローラの最新トグル／表示状態へメニューバーのチェックマークを同期する。
    pub(crate) fn reload_app_menus(&mut self, cx: &mut Context<Self>) {
        let state = MenuCheckState {
            preview_overlays: PreviewOverlayChecks {
                guides: self.controller.show_preview_guides,
                grid: self.controller.show_preview_grid,
                waveform: self.controller.show_preview_waveform,
            },
            follow_selected_on_seek: self.controller.follow_selected_on_seek,
            preview_zoom: self.controller.preview_zoom,
            right_tab: self.controller.right_tab,
        };
        update_app_menus(cx, state);
        self.menu_bar.update(cx, AppMenuBar::reload);
    }

    /// キーコンフィグモーダルのドラフトを永続化＋キーマップ再バインドして閉じる。
    pub(crate) fn apply_and_save_key_config_modal(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.key_config_modal.take() else {
            return;
        };
        self.controller.key_config = modal.draft;
        apply_key_config_bindings(cx, &self.controller.key_config);
        if let Some(path) = KeyConfig::default_path()
            && let Err(err) = self.controller.key_config.save_to_path(&path)
        {
            self.controller.status_message = err;
            return;
        }
        "ショートカットキー設定を保存しました".clone_into(&mut self.controller.status_message);
    }

    /// `Escape` キーの優先順位スタックに従ってドラッグ中断・モーダル閉鎖・フォーカス解除・選択解除を行う。
    pub(crate) fn handle_escape_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.controller.preview_drag.is_some() {
            self.controller.cancel_preview_drag();
        } else if self.controller.trackbar_drag.is_some() {
            self.controller.cancel_trackbar_drag();
        } else if self.controller.timeline_drag.is_some()
            || self.controller.timeline_gesture.is_some()
        {
            self.controller.timeline_drag = None;
            self.controller.timeline_gesture = None;
            self.controller.snap_guide_frame = None;
        } else if let Some(modal) = &mut self.key_config_modal {
            if modal.capturing.is_some() {
                modal.capturing = None;
            } else {
                self.key_config_modal = None;
            }
        } else if self.external_tools_modal.is_some() {
            self.close_external_tools_modal();
            window.focus(&self.workspace_focus, cx);
        } else if self.controller.export_dialog.is_open {
            self.controller.export_dialog.is_open = false;
        } else if self.timeline_length_modal.is_some() {
            self.close_timeline_length_modal();
        } else if self.layer_rename_target.is_some() {
            self.layer_rename_target = None;
            self.layer_rename_input = None;
            self.dynamic_input_sub = None;
        } else if self.is_any_text_input_focused(window, cx) {
            window.focus(&self.workspace_focus, cx);
        } else if self.controller.document.selection_range.is_some() {
            let _ = self.controller.set_selection_range(None);
        } else if !self.controller.effective_selected_clip_ids().is_empty() {
            self.controller.clear_clip_selection();
        }
        self.controller.inspector.invalidate_sync();
        cx.notify();
    }

    /// リンク切れ素材を再リンクするためのファイル選択ダイアログを開く。
    pub(crate) fn prompt_relink_asset(asset_id: AssetId, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("再リンクするメディアファイルを選択".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| {
                    if let Err(err) = view.controller.relink_asset(&asset_id, &path) {
                        view.controller.status_message = format!("再リンク失敗: {err}");
                    } else {
                        view.controller.status_message =
                            format!("メディアを再リンクしました: {}", path.display());
                    }
                    cx.notify();
                });
            }
        })
        .detach();
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

        let space_pan = self.preview_pointer_mode == PreviewPointerInputMode::SpacePanHeld;
        let left_area = v_resizable("main-left-vertical")
            .child(
                resizable_panel()
                    .size(px(460.))
                    .size_range(px(220.)..px(1600.))
                    .child(render_preview_panel(
                        &mut self.controller,
                        &self.scrub_slider,
                        space_pan,
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
        let with_file_edit = Self::bind_file_and_edit_actions(root, cx);
        let with_tl = Self::bind_timeline_edit_actions(with_file_edit, cx);
        let with_view = Self::bind_view_and_settings_actions(with_tl, cx);
        Self::bind_playback_and_canvas_actions(with_view, cx)
    }

    fn bind_file_and_edit_actions(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(Self::on_new_project))
            .on_action(cx.listener(Self::on_open_project))
            .on_action(cx.listener(Self::on_import_media))
            .on_action(cx.listener(Self::on_save_project))
            .on_action(cx.listener(Self::on_save_project_as))
            .on_action(|_: &QuitApp, _, cx| cx.quit())
            .on_action(cx.listener(|this, _: &OpenExportDialog, _, cx| {
                this.controller.export_selection_only = false;
                this.controller.open_export_dialog();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ExportSelectionDialogAction, _, cx| {
                this.controller.export_selection_only = true;
                this.controller.open_export_dialog();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &UndoAction, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.undo();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RedoAction, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.redo();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CutClip, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.cut_selected_clips();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CopyClip, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.copy_selected_clip();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PasteClip, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.paste_clipboard_at_playhead();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DuplicateClip, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.duplicate_selected_clip();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteClip, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.delete_selected_clip();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectAllClips, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                this.controller.select_all_clips();
                this.controller.inspector.invalidate_sync();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearSelectionRange, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.set_selection_range(None);
                this.controller.clear_clip_selection();
                this.controller.inspector.invalidate_sync();
                cx.notify();
            }))
    }

    fn bind_timeline_edit_actions(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &SplitClip, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            let _ = this.controller.split_selected_clip_at_playhead();
            this.controller.inspector.invalidate_sync();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &AddMidpointAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            let _ = this.controller.add_midpoint_at_playhead();
            this.controller.inspector.invalidate_sync();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &RemoveMidpointAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            let _ = this.controller.remove_midpoint_near_playhead();
            this.controller.inspector.invalidate_sync();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &AddSubtitleAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            let _ = this.controller.add_subtitle_at_playhead();
            this.controller.inspector.invalidate_sync();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(
            cx.listener(|this, _: &SetSelectionStartAction, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                let _ = this.controller.set_selection_range_start_at_playhead();
                cx.notify();
            }),
        )
        .on_action(cx.listener(|this, _: &SetSelectionEndAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            let _ = this.controller.set_selection_range_end_at_playhead();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &CycleClipAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            if !this.controller.cycle_clip_at_playhead() {
                this.controller.right_tab = match this.controller.right_tab {
                    RightPanelTab::ClipInspector => RightPanelTab::AiChat,
                    RightPanelTab::AiChat => RightPanelTab::LayerList,
                    RightPanelTab::LayerList => RightPanelTab::ProjectInfo,
                    RightPanelTab::ProjectInfo => RightPanelTab::ClipInspector,
                };
                this.reload_app_menus(cx);
            }
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(
            cx.listener(|this, _: &SetTimelineLengthAction, window, cx| {
                this.open_timeline_length_modal(window, cx);
            }),
        )
        .on_action(
            cx.listener(|this, _: &SnapTimelineLengthAutoAction, window, cx| {
                let _ = this
                    .controller
                    .set_timeline_length(TimelineLengthMode::AutoToLastClip);
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        )
    }

    fn bind_view_and_settings_actions(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(
            cx.listener(|this, _: &ToggleFollowSelectedOnSeekAction, _, cx| {
                this.controller.toggle_follow_selected_on_seek();
                this.reload_app_menus(cx);
                cx.notify();
            }),
        )
        .on_action(cx.listener(|this, _: &OpenKeyConfigAction, _, cx| {
            this.key_config_modal = Some(KeyConfigModalState::open(&this.controller.key_config));
            cx.notify();
        }))
        .on_action(
            cx.listener(|this, _: &OpenExternalToolsAction, window, cx| {
                this.open_external_tools_modal(window, cx);
            }),
        )
        .on_action(cx.listener(|this, _: &TogglePreviewGuidesAction, _, cx| {
            this.controller.show_preview_guides = !this.controller.show_preview_guides;
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &TogglePreviewGridAction, _, cx| {
            this.controller.show_preview_grid = !this.controller.show_preview_grid;
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &TogglePreviewWaveformAction, _, cx| {
            this.controller.show_preview_waveform = !this.controller.show_preview_waveform;
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PreviewZoomFitAction, _, cx| {
            this.controller.preview_zoom = PreviewZoomMode::Fit;
            this.controller.preview_pan_offset_px = (0.0, 0.0);
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PreviewZoom50Action, _, cx| {
            this.controller.preview_zoom = PreviewZoomMode::Percent(50);
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PreviewZoom100Action, _, cx| {
            this.controller.preview_zoom = PreviewZoomMode::Percent(100);
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PreviewZoom200Action, _, cx| {
            this.controller.preview_zoom = PreviewZoomMode::Percent(200);
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &TimelineZoomIn, _, cx| {
            this.controller.zoom_timeline(1.25);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &TimelineZoomOut, _, cx| {
            this.controller.zoom_timeline(0.8);
            cx.notify();
        }))
        .on_action(
            cx.listener(|this, _: &ShowTabClipInspectorAction, window, cx| {
                this.controller.right_tab = RightPanelTab::ClipInspector;
                this.reload_app_menus(cx);
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        )
        .on_action(cx.listener(|this, _: &ShowTabAiChatAction, _, cx| {
            this.controller.right_tab = RightPanelTab::AiChat;
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ShowTabLayerListAction, _, cx| {
            this.controller.right_tab = RightPanelTab::LayerList;
            this.reload_app_menus(cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ShowTabProjectInfoAction, _, cx| {
            this.controller.right_tab = RightPanelTab::ProjectInfo;
            this.reload_app_menus(cx);
            cx.notify();
        }))
    }

    fn bind_playback_and_canvas_actions(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &TogglePlayPause, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.toggle_play_pause();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &StopPlaybackAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.stop();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &SeekStart, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.seek_to_frame(FrameIndex::ZERO);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &SeekEnd, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.seek_to_end();
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &StepBackOneFrame, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.step_frames(-1);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &StepForwardOneFrame, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.step_frames(1);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &StepBackHalfSecAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.step_half_second(false);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(
            cx.listener(|this, _: &StepForwardHalfSecAction, window, cx| {
                if !this.allow_editor_shortcut(window, cx) {
                    return;
                }
                this.controller.step_half_second(true);
                this.sync_sliders_now(window, cx);
                cx.notify();
            }),
        )
        .on_action(cx.listener(|this, _: &PrevBoundaryAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.jump_to_adjacent_boundary_or_midpoint(false);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &NextBoundaryAction, window, cx| {
            if !this.allow_editor_shortcut(window, cx) {
                return;
            }
            this.controller.jump_to_adjacent_boundary_or_midpoint(true);
            this.sync_sliders_now(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &NudgeLeftAction, window, cx| {
            Self::dispatch_nudge_xy(this, -1.0, 0.0, window, cx);
        }))
        .on_action(cx.listener(|this, _: &NudgeRightAction, window, cx| {
            Self::dispatch_nudge_xy(this, 1.0, 0.0, window, cx);
        }))
        .on_action(cx.listener(|this, _: &NudgeUpAction, window, cx| {
            Self::dispatch_nudge_xy(this, 0.0, -1.0, window, cx);
        }))
        .on_action(cx.listener(|this, _: &NudgeDownAction, window, cx| {
            Self::dispatch_nudge_xy(this, 0.0, 1.0, window, cx);
        }))
        .on_action(cx.listener(|this, _: &RotateLeftAction, window, cx| {
            Self::dispatch_rotate_by(this, -1.0, window, cx);
        }))
        .on_action(cx.listener(|this, _: &RotateRightAction, window, cx| {
            Self::dispatch_rotate_by(this, 1.0, window, cx);
        }))
    }

    fn dispatch_nudge_xy(&mut self, dx: f64, dy: f64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.allow_editor_shortcut(window, cx) {
            return;
        }
        let _ = self.controller.nudge_selected_clips_xy(dx, dy);
        self.sync_sliders_now(window, cx);
        cx.notify();
    }

    fn dispatch_rotate_by(&mut self, deg: f64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.allow_editor_shortcut(window, cx) {
            return;
        }
        let _ = self.controller.rotate_selected_clips_by(deg);
        self.sync_sliders_now(window, cx);
        cx.notify();
    }
    fn attach_key_and_pointer_handlers(root: Div, cx: &mut Context<Self>) -> Div {
        Self::attach_pointer_handlers(
            root.on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if let Some(modal) = &mut this.key_config_modal
                    && modal.capturing.is_some()
                {
                    if ev.keystroke.key == "escape" {
                        modal.capturing = None;
                    } else if let Some(stroke) = keystroke_from_gpui_event(&ev.keystroke) {
                        modal.capture_keystroke(&stroke);
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if ev.keystroke.key == "escape" {
                    this.handle_escape_key(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if ev.keystroke.key == "space"
                    && ev.keystroke.modifiers == Modifiers::none()
                    && !this.is_any_text_input_focused(window, cx)
                    && this.preview_pointer_mode == PreviewPointerInputMode::Idle
                {
                    this.preview_pointer_mode = PreviewPointerInputMode::SpacePanHeld;
                }
            }))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| {
                if ev.keystroke.key == "space"
                    && this.preview_pointer_mode == PreviewPointerInputMode::SpacePanHeld
                {
                    this.preview_pointer_mode = PreviewPointerInputMode::Idle;
                    cx.notify();
                }
            })),
            cx,
        )
    }

    fn attach_pointer_handlers(root: Div, cx: &mut Context<Self>) -> Div {
        root.on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
            let mx: f32 = ev.position.x / px(1.0);
            let my: f32 = ev.position.y / px(1.0);
            if this.controller.preview_drag.is_some() {
                this.controller.update_preview_drag(
                    (mx, my),
                    DragModifiers {
                        shift: ev.modifiers.shift,
                        ctrl: ev.modifiers.control,
                        alt: ev.modifiers.alt,
                    },
                );
                this.sliders_dirty = true;
                cx.notify();
            } else if this.preview_pointer_mode == PreviewPointerInputMode::WaveformScrubbing {
                this.controller.seek_by_waveform_x(mx);
                this.sliders_dirty = true;
                cx.notify();
            } else if this.controller.trackbar_drag.is_some() {
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
                if this.controller.preview_drag.is_some() {
                    let _ = this.controller.finish_preview_drag();
                    this.sync_sliders_now(window, cx);
                    cx.notify();
                } else if this.preview_pointer_mode == PreviewPointerInputMode::WaveformScrubbing {
                    this.preview_pointer_mode = PreviewPointerInputMode::Idle;
                    cx.notify();
                } else if this.controller.trackbar_drag.is_some() {
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
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(|this, _, _, cx| {
                if this.controller.preview_drag.is_some() {
                    let _ = this.controller.finish_preview_drag();
                    cx.notify();
                }
            }),
        )
    }
}

impl Render for SeaEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for stale_img in self.controller.surface_bridge.drain_pending_drops() {
            let _ = window.drop_image(stale_img);
        }
        let _ = self.controller.tick_master_waveform();
        if self.sliders_dirty {
            self.sync_sliders_now(window, cx);
        }
        if let Some(modal) = &mut self.external_tools_modal
            && let Some(val) = modal.pending_value.take()
        {
            modal
                .input
                .update(cx, |input, cx| input.set_value(&val, window, cx));
        }

        let theme = cx.theme().clone();
        let key_ctx = if self.is_any_text_input_focused(window, cx) {
            "EditorTextInput"
        } else {
            "EditorMain"
        };
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

        let base = v_flex()
            .size_full()
            .relative()
            .bg(theme.background)
            .track_focus(&self.workspace_focus)
            .key_context(key_ctx);
        let bound = Self::bind_actions(base, cx);
        let root = Self::attach_key_and_pointer_handlers(bound, cx)
            .child(top_bar)
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(workspace),
            );

        if let Some(modal) = &self.external_tools_modal {
            root.child(render_external_tools_modal_overlay(modal, cx))
        } else if let Some(modal) = &self.key_config_modal {
            root.child(render_key_config_modal_overlay(modal, cx))
        } else if self.controller.export_dialog.is_open {
            root.child(render_export_dialog_overlay(
                &self.controller,
                &self.controller.export_dialog,
                window,
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
