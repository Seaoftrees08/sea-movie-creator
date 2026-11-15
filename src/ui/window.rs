//! GPUI シングルウィンドウシェル (`SeaEditorWindow`) とアプリ起動エントリ (`launch_app`)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::component::input::InputState;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::{
    ActiveTheme as _, Theme, ThemeMode, TitleBar, VirtualListScrollHandle, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Bounds, Context, Div, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window,
    WindowBounds, WindowOptions, div, px, size,
};

use crate::core::time::FrameIndex;
use crate::timeline::document::ClipKind;
use crate::ui::app_controller::{AppController, clamp_right_panel_width};
use crate::ui::menu_bar::{
    DeleteSelectedClip, ImportMedia, NewProject, OpenProject, QuitApp, RedoAction, SaveProject,
    SaveProjectAs, SeekStart, SplitAtPlayhead, TogglePlayback, UndoAction, setup_menus,
};
use crate::ui::preview_panel::render_preview_panel;
use crate::ui::right_panel::render_right_panel;
use crate::ui::timeline_panel::render_timeline_panel;

const DEFAULT_WINDOW_WIDTH: f32 = 1280.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 800.0;
const TICK_INTERVAL_MS: u64 = 16;

/// `sea-movie-creator` のメインエディタウィンドウビュー。
pub struct SeaEditorWindow {
    /// アプリ状態・タイムライン・再生エンジン統括コントローラ。
    pub controller: AppController,
    menu_bar: Entity<AppMenuBar>,
    scrub_slider: Entity<SliderState>,
    opacity_slider: Entity<SliderState>,
    /// AI チャット欄のテキスト入力状態。
    pub(crate) ai_chat_input: Entity<InputState>,
    timeline_scroll: VirtualListScrollHandle,
    sliders_dirty: bool,
    syncing_widgets: bool,
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
        let opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.05)
                .default_value(1.0)
        });
        let ai_chat_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("AI 編集指示を入力..."));

        let subscriptions = vec![
            cx.subscribe_in(&scrub_slider, window, Self::on_scrub_slider),
            cx.subscribe_in(&opacity_slider, window, Self::on_opacity_slider),
        ];

        let mut this = Self {
            controller,
            menu_bar,
            scrub_slider,
            opacity_slider,
            ai_chat_input,
            timeline_scroll: VirtualListScrollHandle::new(),
            sliders_dirty: true,
            syncing_widgets: false,
            _subscriptions: subscriptions,
        };
        this.sync_sliders_now(window, cx);
        Self::spawn_playback_loop(cx);
        this
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

        if let Some((_, clip)) = self.controller.selected_clip() {
            let op_val = match &clip.kind {
                ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => {
                    visual.opacity.evaluate_constant_or_one()
                }
                ClipKind::Subtitle(sub) => sub.visual.opacity.evaluate_constant_or_one(),
                ClipKind::Audio { .. } => 1.0,
            };
            #[allow(clippy::cast_possible_truncation)]
            let op_f32 = op_val.clamp(0.0, 1.0) as f32;
            self.opacity_slider.update(cx, |slider, cx| {
                if (slider.value().end() - op_f32).abs() > 0.005 {
                    slider.set_value(op_f32, window, cx);
                }
            });
        }
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
                    if view.controller.clock.current_frame().get() != prev_frame {
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

    fn on_opacity_slider(
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
        let _ = self
            .controller
            .set_selected_clip_opacity(f64::from(val.end()));
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

trait AnimatedOpacityQuickHelper {
    fn evaluate_constant_or_one(&self) -> f64;
}

impl AnimatedOpacityQuickHelper for crate::core::animation::AnimatedParameter {
    fn evaluate_constant_or_one(&self) -> f64 {
        match self {
            Self::Constant { value } => *value,
            _ => 1.0,
        }
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

        h_resizable("main-workspace-split")
            .child(resizable_panel().child(left_area))
            .child(
                resizable_panel()
                    .fixed()
                    .size(px(default_right_w))
                    .size_range(px(220.)..px(560.))
                    .child(render_right_panel(
                        &self.controller,
                        &self.opacity_slider,
                        &self.ai_chat_input,
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
            .on_action(|_: &QuitApp, _, cx| cx.quit())
            .on_action(cx.listener(|this, _: &UndoAction, window, cx| {
                let _ = this.controller.undo();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RedoAction, window, cx| {
                let _ = this.controller.redo();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SplitAtPlayhead, window, cx| {
                let _ = this.controller.split_selected_clip_at_playhead();
                this.sync_sliders_now(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &DeleteSelectedClip, window, cx| {
                let _ = this.controller.delete_selected_clip();
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

        Self::bind_actions(v_flex().size_full().bg(theme.background), cx)
            .child(top_bar)
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(workspace),
            )
    }
}

/// GPUI アプリケーションを起動し、メインエディタウィンドウを表示する。
#[must_use]
pub fn launch_app(open_project: Option<PathBuf>, auto_close_ms: Option<u64>) -> i32 {
    gpui_kit::application().run(move |cx| {
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
