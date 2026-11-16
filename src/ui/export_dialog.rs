//! 書き出し設定モーダルダイアログ UI（プラグイン選択・ライセンス注意表示・進捗・中断）。

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Side, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, AnyElement, Context, Div, ElementId, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Styled as _, Window, div, px, relative, rgba,
};

use crate::export::manifest::{ExportOptionField, ExportOptionKind, PresetSchema};
use crate::export::registry::{DiscoveredPlugin, PluginRegistry};
use crate::export::runner::{ExportJobHandle, ExportOutcome, ExportProgress};
use crate::timeline::document::ProjectDocument;
use crate::ui::app_controller::AppController;
use crate::ui::window::SeaEditorWindow;

/// 出力解像度プリセット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportResolutionPreset {
    /// プロジェクト設定解像度をそのまま使用。
    ProjectNative,
    /// HD (`1280x720`)。
    Hd720p,
    /// Full HD (`1920x1080`)。
    Fhd1080p,
}

impl ExportResolutionPreset {
    /// 表示ラベルを返す。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProjectNative => "プロジェクト設定",
            Self::Hd720p => "1280x720 (HD)",
            Self::Fhd1080p => "1920x1080 (FHD)",
        }
    }

    /// 偶数保証済みの `(width, height)` を返す。
    #[must_use]
    pub const fn dimensions(self, doc: &ProjectDocument) -> (u32, u32) {
        let (w, h) = match self {
            Self::ProjectNative => (doc.resolution.width.get(), doc.resolution.height.get()),
            Self::Hd720p => (1280, 720),
            Self::Fhd1080p => (1920, 1080),
        };
        let even_w = if w <= 2 { 2 } else { w & !1 };
        let even_h = if h <= 2 { 2 } else { h & !1 };
        (even_w, even_h)
    }
}

/// 書き出しダイアログの編集中パラメータと進行状態。
#[derive(Default)]
pub struct ExportDialogState {
    /// ダイアログが表示中かどうか。
    pub is_open: bool,
    /// 検出されたプラグイン一覧。
    pub registry: PluginRegistry,
    /// 選択中プラグインインデックス。
    pub selected_plugin_idx: usize,
    /// 選択中プリセットインデックス。
    pub selected_preset_idx: usize,
    /// 解像度プリセット。
    pub resolution: ExportResolutionPresetChoice,
    /// 出力ファイルパス文字列。
    pub output_path: String,
    /// プラグイン固有オプション上書き値マップ。
    pub option_overrides: serde_json::Map<String, serde_json::Value>,
    /// 最後に受信した進捗状態。
    pub last_progress: Option<ExportProgress>,
    /// 直近のエラー表示。
    pub last_error: Option<String>,
    /// 直近の完了結果。
    pub last_completed: Option<ExportOutcome>,
    /// バックグラウンドで実行中のジョブハンドル。
    pub running_job: Option<ExportJobHandle>,
}

/// [`Default`] 実装を簡潔にするための解像度ラッパー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportResolutionPresetChoice(pub ExportResolutionPreset);

impl Default for ExportResolutionPresetChoice {
    fn default() -> Self {
        Self(ExportResolutionPreset::ProjectNative)
    }
}

impl ExportDialogState {
    /// 指定レジストリを読み込んでダイアログを開き、既定プリセット・出力名を同期する。
    pub fn open_with_registry(&mut self, registry: PluginRegistry, base_dir: Option<&Path>) {
        self.registry = registry;
        self.is_open = true;
        self.last_error = None;
        self.last_completed = None;
        if self.selected_plugin_idx >= self.registry.plugins().len() {
            self.selected_plugin_idx = 0;
        }
        self.selected_preset_idx = 0;
        self.reset_options_from_preset();
        self.refresh_output_path_extension(base_dir);
    }

    /// ダイアログを閉じる（実行中でなければ状態を片付ける）。
    pub fn close(&mut self) {
        if self.running_job.is_none() {
            self.is_open = false;
        }
    }

    /// プラグイン選択を変更し、先頭プリセットと出力拡張子を切り替える。
    pub fn select_plugin(&mut self, index: usize, base_dir: Option<&Path>) {
        if index < self.registry.plugins().len() && self.running_job.is_none() {
            self.selected_plugin_idx = index;
            self.selected_preset_idx = 0;
            self.reset_options_from_preset();
            self.refresh_output_path_extension(base_dir);
        }
    }

    /// プリセット選択を変更し、オプション初期値と出力拡張子を更新する。
    pub fn select_preset(&mut self, index: usize, base_dir: Option<&Path>) {
        if let Some(plugin) = self.selected_plugin()
            && index < plugin.manifest.presets.len()
            && self.running_job.is_none()
        {
            self.selected_preset_idx = index;
            self.reset_options_from_preset();
            self.refresh_output_path_extension(base_dir);
        }
    }

    /// 現在選択中のプラグイン参照を返す。
    #[must_use]
    pub fn selected_plugin(&self) -> Option<&DiscoveredPlugin> {
        self.registry.plugins().get(self.selected_plugin_idx)
    }

    /// 現在選択中のプリセット参照を返す。
    #[must_use]
    pub fn selected_preset(&self) -> Option<&PresetSchema> {
        self.selected_plugin()
            .and_then(|p| p.manifest.presets.get(self.selected_preset_idx))
    }

    /// プリセット既定値とユーザー調整値を統合したオプション JSON を生成する。
    #[must_use]
    pub fn merged_options(&self) -> serde_json::Value {
        let mut map = self
            .selected_preset()
            .and_then(|p| p.default_options.as_object())
            .cloned()
            .unwrap_or_default();
        for (k, v) in &self.option_overrides {
            map.insert(k.clone(), v.clone());
        }
        serde_json::Value::Object(map)
    }

    /// 真偽オプション項目を反転する。
    pub fn toggle_bool_option(&mut self, key: &str, current: bool) {
        self.option_overrides
            .insert(key.to_owned(), serde_json::Value::Bool(!current));
    }

    /// 整数オプション項目を指定ステップ増減させる。
    pub fn step_int_option(&mut self, key: &str, delta: i64, min: i64, max: i64) {
        let cur = self.current_option_i64(key, min);
        let next = cur.saturating_add(delta).clamp(min, max);
        self.option_overrides
            .insert(key.to_owned(), serde_json::Value::from(next));
    }

    /// 選択肢オプション項目を更新する。
    pub fn set_select_option(&mut self, key: &str, value: String) {
        self.option_overrides
            .insert(key.to_owned(), serde_json::Value::String(value));
    }

    /// 実行中のジョブから進捗・完了イベントを非ブロッキングで吸い上げる。
    pub fn poll_job(&mut self) -> bool {
        let mut changed = false;
        let Some(handle) = self.running_job.as_ref() else {
            return false;
        };
        while let Ok(progress) = handle.progress_rx.try_recv() {
            self.last_progress = Some(progress);
            changed = true;
        }
        match handle.result_rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.last_completed = Some(outcome);
                self.last_error = None;
                self.running_job = None;
                changed = true;
            }
            Ok(Err(err)) => {
                self.last_error = Some(err);
                self.running_job = None;
                changed = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.last_error = Some("書き出しスレッドが予期せず終了しました".to_owned());
                self.running_job = None;
                changed = true;
            }
        }
        changed
    }

    /// 実行中の書き出しに対して中断要求を出す。
    pub fn cancel_active(&self) {
        if let Some(handle) = &self.running_job {
            handle.cancel_token.cancel();
        }
    }

    fn current_option_i64(&self, key: &str, default_val: i64) -> i64 {
        self.option_overrides
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .or_else(|| {
                self.selected_preset()
                    .and_then(|p| p.default_options.get(key))
                    .and_then(serde_json::Value::as_i64)
            })
            .unwrap_or(default_val)
    }

    fn reset_options_from_preset(&mut self) {
        self.option_overrides.clear();
        if let Some(plugin) = self.selected_plugin().cloned() {
            for field in &plugin.manifest.option_fields {
                if !field.default_value.is_null() {
                    self.option_overrides
                        .insert(field.key.clone(), field.default_value.clone());
                }
            }
        }
        if let Some(defaults) = self
            .selected_preset()
            .and_then(|p| p.default_options.as_object())
            .cloned()
        {
            for (k, v) in defaults {
                self.option_overrides.insert(k, v);
            }
        }
    }

    fn refresh_output_path_extension(&mut self, base_dir: Option<&Path>) {
        let ext = self
            .selected_preset()
            .map_or("mkv", |p| p.container_ext.as_str());
        if self.output_path.trim().is_empty() {
            let dir = base_dir
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            self.output_path = dir
                .join(format!("sea_export.{ext}"))
                .to_string_lossy()
                .into_owned();
        } else {
            let mut pb = PathBuf::from(&self.output_path);
            pb.set_extension(ext);
            self.output_path = pb.to_string_lossy().into_owned();
        }
    }
}

/// ウィンドウのビューポート寸法に応じた書き出しダイアログの `(width, max_height)` を算出する。
#[must_use]
pub fn compute_export_modal_bounds(viewport_w: f32, viewport_h: f32) -> (f32, f32) {
    let width = (viewport_w - 32.0).clamp(320.0, 560.0);
    let max_height = (viewport_h - 48.0).clamp(240.0, 620.0);
    (width, max_height)
}

fn render_combo_button<F>(
    id: impl Into<ElementId>,
    label: String,
    menu_width: f32,
    disabled: bool,
    builder: F,
) -> AnyElement
where
    F: Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
{
    let btn = Button::new(id)
        .outline()
        .small()
        .w_full()
        .dropdown_caret(true)
        .disabled(disabled)
        .label(label);
    if disabled {
        btn.into_any_element()
    } else {
        btn.dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, window, cx| {
            builder(
                menu.min_w(px(menu_width))
                    .max_h(px(300.))
                    .scrollable(true)
                    .check_side(Side::Right),
                window,
                cx,
            )
        })
        .into_any_element()
    }
}

/// 書き出し設定モーダルダイアログの GPUI オーバーレイを描画する。
///
/// 各選択項目はコンボボックス化し、中央設定エリアを独立スクロールさせることで、
/// 狭いウィンドウでも「保存先変更」「書き出し開始」等の操作ボタンが画面外へ見切れないようにする。
pub fn render_export_dialog_overlay(
    controller: &AppController,
    state: &ExportDialogState,
    window: &Window,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let theme = cx.theme().clone();
    let is_busy = state.running_job.is_some();
    let vp_size = window.viewport_size();
    let (modal_w, modal_max_h) =
        compute_export_modal_bounds(f32::from(vp_size.width), f32::from(vp_size.height));

    let body = v_flex()
        .id("export-dialog-scroll-body")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scrollbar()
        .px_4()
        .py_3()
        .gap_2p5()
        .child(render_plugin_selector(state, is_busy, cx))
        .child(render_license_notice(state, cx))
        .child(render_preset_selector(state, is_busy, cx))
        .child(render_range_and_resolution_row(
            controller, state, is_busy, cx,
        ))
        .child(render_option_fields(state, is_busy, cx))
        .child(render_output_target_row(state, is_busy, cx));

    let card = v_flex()
        .w(px(modal_w))
        .max_h(px(modal_max_h))
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .rounded(px(8.))
        .text_color(theme.foreground)
        .overflow_hidden()
        .child(render_dialog_header(is_busy, cx))
        .child(body)
        .child(render_progress_and_actions(state, is_busy, cx));

    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x0000_00b0))
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(card)
}

fn render_dialog_header(is_busy: bool, cx: &mut Context<SeaEditorWindow>) -> Div {
    let theme = cx.theme().clone();
    let mut close_btn = Button::new("export-close-x")
        .ghost()
        .xsmall()
        .label("閉じる");
    if !is_busy {
        close_btn = close_btn.on_click(cx.listener(|this, _, _, cx| {
            this.controller.export_dialog.close();
            cx.notify();
        }));
    }
    h_flex()
        .flex_shrink_0()
        .items_center()
        .justify_between()
        .px_4()
        .pt_3()
        .pb_2()
        .border_b_1()
        .border_color(theme.border)
        .child(div().text_sm().child("動画の書き出し (プラグイン形式)"))
        .child(close_btn)
}

fn render_plugin_selector(
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let theme = cx.theme().clone();
    let plugins = state.registry.plugins();
    if plugins.is_empty() {
        return v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("エンコーダプラグイン"),
            )
            .child(
                div()
                    .p_2()
                    .rounded(px(4.))
                    .bg(rgba(0x5a1d_1dff))
                    .text_xs()
                    .child("有効なプラグインが見つかりませんでした（plugins/export/ を確認してください）"),
            );
    }

    let current_label = state.selected_plugin().map_or_else(
        || "プラグインを選択...".to_owned(),
        |p| {
            if p.executable_path.is_some() {
                p.manifest.name.clone()
            } else {
                format!("{} [未ビルド]", p.manifest.name)
            }
        },
    );
    let items: Vec<(usize, String)> = plugins
        .iter()
        .enumerate()
        .map(|(idx, plugin)| {
            let suffix = if plugin.executable_path.is_some() {
                ""
            } else {
                " [未ビルド]"
            };
            (idx, format!("{}{}", plugin.manifest.name, suffix))
        })
        .collect();
    let selected_idx = state.selected_plugin_idx;
    let weak = cx.weak_entity();

    let combo = render_combo_button(
        "exp-plugin-combo",
        current_label,
        420.0,
        is_busy,
        move |menu, _, _| {
            let mut popup = menu.label("エンコーダプラグイン");
            for (idx, label) in &items {
                let weak_item = weak.clone();
                let target_idx = *idx;
                popup = popup.item(
                    PopupMenuItem::new(label.clone())
                        .checked(target_idx == selected_idx)
                        .on_click(move |_, _, cx| {
                            let _ = weak_item.update(cx, |this, cx| {
                                let base_dir = this
                                    .controller
                                    .project_path
                                    .as_deref()
                                    .and_then(Path::parent)
                                    .map(Path::to_path_buf);
                                this.controller
                                    .export_dialog
                                    .select_plugin(target_idx, base_dir.as_deref());
                                cx.notify();
                            });
                        }),
                );
            }
            popup
        },
    );

    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("エンコーダプラグイン"),
        )
        .child(div().w_full().child(combo))
}

fn render_license_notice(state: &ExportDialogState, cx: &mut Context<SeaEditorWindow>) -> Div {
    let Some(plugin) = state.selected_plugin() else {
        return div();
    };
    let theme = cx.theme().clone();
    let lic = &plugin.manifest.license;
    let missing_exe = plugin.executable_path.is_none();
    if !lic.requires_external_binary && lic.notice.is_none() && !missing_exe {
        return div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(format!(
                "ライセンス: {} / コア分離済み別プロセス実行",
                lic.spdx
            ));
    }

    let bg = if missing_exe {
        rgba(0x6e20_20ff)
    } else {
        rgba(0x523c_14ff)
    };
    let mut box_div = v_flex()
        .p_2()
        .gap_1()
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border)
        .bg(bg)
        .text_xs()
        .child(format!("ライセンス / 実行構成: {}", lic.spdx));
    if let Some(notice) = &lic.notice {
        box_div = box_div.child(notice.clone());
    }
    if missing_exe {
        box_div = box_div.child(format!(
            "警告: 実行ファイル '{}' が見つかりません。`cargo build --workspace --all-targets` を実行してください。",
            plugin.manifest.entrypoint
        ));
    }
    box_div
}

fn render_preset_selector(
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let Some(plugin) = state.selected_plugin() else {
        return div();
    };
    let theme = cx.theme().clone();
    let current_label = state.selected_preset().map_or_else(
        || "プリセットを選択...".to_owned(),
        |p| format!("{} (.{})", p.label, p.container_ext),
    );
    let items: Vec<(usize, String)> = plugin
        .manifest
        .presets
        .iter()
        .enumerate()
        .map(|(idx, preset)| (idx, format!("{} (.{})", preset.label, preset.container_ext)))
        .collect();
    let selected_idx = state.selected_preset_idx;
    let weak = cx.weak_entity();
    let disabled = is_busy || items.is_empty();

    let combo = render_combo_button(
        "exp-preset-combo",
        current_label,
        420.0,
        disabled,
        move |menu, _, _| {
            let mut popup = menu.label("プリセット");
            for (idx, label) in &items {
                let weak_item = weak.clone();
                let target_idx = *idx;
                popup = popup.item(
                    PopupMenuItem::new(label.clone())
                        .checked(target_idx == selected_idx)
                        .on_click(move |_, _, cx| {
                            let _ = weak_item.update(cx, |this, cx| {
                                let base_dir = this
                                    .controller
                                    .project_path
                                    .as_deref()
                                    .and_then(Path::parent)
                                    .map(Path::to_path_buf);
                                this.controller
                                    .export_dialog
                                    .select_preset(target_idx, base_dir.as_deref());
                                cx.notify();
                            });
                        }),
                );
            }
            popup
        },
    );

    let desc = state.selected_preset().map_or_else(String::new, |p| {
        format!(
            "映像: {} / 音声: {} — {}",
            p.video_codec, p.audio_codec, p.description
        )
    });
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("プリセット"),
        )
        .child(div().w_full().child(combo))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(desc),
        )
}

fn render_range_and_resolution_row(
    controller: &AppController,
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let theme = cx.theme().clone();
    let range_combo = render_range_combo(controller, is_busy, cx);
    let res_combo = render_resolution_combo(controller, state, is_busy, cx);

    let (w, h) = state.resolution.0.dimensions(&controller.document);
    let total_frames = controller.document.total_duration_frames().max(1);
    let (range_start, range_end) = crate::export::runner::resolve_export_frame_range(
        &controller.document,
        controller.export_selection_only,
    );
    let export_frames = range_end.get().saturating_sub(range_start.get());

    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("書き出し範囲"),
                        )
                        .child(range_combo),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("出力解像度"),
                        )
                        .child(res_combo),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "出力: {w}x{h} / 範囲: {}..{} ({export_frames}f / 総尺 {total_frames}f)",
                    range_start.get(),
                    range_end.get()
                )),
        )
}

fn render_range_combo(
    controller: &AppController,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let has_selection = controller.document.selection_range.is_some();
    let sel_only = controller.export_selection_only && has_selection;
    let sel_label = match controller.document.selection_range {
        Some(r) => format!("選択範囲のみ ({}..{})", r.start.get(), r.end.get()),
        None => "選択範囲のみ (未設定)".to_owned(),
    };
    let current_label = if sel_only {
        sel_label.clone()
    } else {
        "プロジェクト全体".to_owned()
    };
    let weak = cx.weak_entity();

    render_combo_button(
        "exp-range-combo",
        current_label,
        240.0,
        is_busy,
        move |menu, _, _| {
            let weak_full = weak.clone();
            let weak_sel = weak.clone();
            menu.label("書き出し範囲")
                .item(
                    PopupMenuItem::new("プロジェクト全体")
                        .checked(!sel_only)
                        .on_click(move |_, _, cx| {
                            let _ = weak_full.update(cx, |this, cx| {
                                this.controller.export_selection_only = false;
                                cx.notify();
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new(sel_label.clone())
                        .checked(sel_only)
                        .disabled(!has_selection)
                        .on_click(move |_, _, cx| {
                            let _ = weak_sel.update(cx, |this, cx| {
                                this.controller.export_selection_only = true;
                                cx.notify();
                            });
                        }),
                )
        },
    )
}

fn render_resolution_combo(
    controller: &AppController,
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let choices = [
        ExportResolutionPreset::ProjectNative,
        ExportResolutionPreset::Hd720p,
        ExportResolutionPreset::Fhd1080p,
    ];
    let current_choice = state.resolution.0;
    let current_dims = current_choice.dimensions(&controller.document);
    let current_label = if current_choice == ExportResolutionPreset::ProjectNative {
        format!(
            "{} ({}x{})",
            current_choice.label(),
            current_dims.0,
            current_dims.1
        )
    } else {
        current_choice.label().to_owned()
    };
    let items: Vec<(ExportResolutionPreset, String)> = choices
        .into_iter()
        .map(|choice| {
            let label = if choice == ExportResolutionPreset::ProjectNative {
                let d = choice.dimensions(&controller.document);
                format!("{} ({}x{})", choice.label(), d.0, d.1)
            } else {
                choice.label().to_owned()
            };
            (choice, label)
        })
        .collect();
    let weak = cx.weak_entity();

    render_combo_button(
        "exp-res-combo",
        current_label,
        240.0,
        is_busy,
        move |menu, _, _| {
            let mut popup = menu.label("出力解像度");
            for (choice, label) in &items {
                let weak_item = weak.clone();
                let chosen = *choice;
                popup = popup.item(
                    PopupMenuItem::new(label.clone())
                        .checked(chosen == current_choice)
                        .on_click(move |_, _, cx| {
                            let _ = weak_item.update(cx, |this, cx| {
                                this.controller.export_dialog.resolution =
                                    ExportResolutionPresetChoice(chosen);
                                cx.notify();
                            });
                        }),
                );
            }
            popup
        },
    )
}

fn render_option_fields(
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let Some(plugin) = state.selected_plugin() else {
        return div();
    };
    if plugin.manifest.option_fields.is_empty() {
        return div();
    }
    let theme = cx.theme().clone();
    let merged = state.merged_options();
    let mut box_div = v_flex().gap_2().child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("詳細オプション"),
    );
    let plugin_idx = state.selected_plugin_idx;
    for (idx, field) in plugin.manifest.option_fields.iter().enumerate() {
        box_div = box_div.child(render_single_option_field(
            plugin_idx * 64 + idx,
            field,
            merged.get(&field.key),
            is_busy,
            cx,
        ));
    }
    box_div
}

fn render_single_option_field(
    idx: usize,
    field: &ExportOptionField,
    current_val: Option<&serde_json::Value>,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    match &field.kind {
        ExportOptionKind::Boolean => render_bool_option(idx, field, current_val, is_busy, cx),
        ExportOptionKind::Integer { min, max } => {
            render_int_option(idx, field, (*min, *max), current_val, is_busy, cx)
        }
        ExportOptionKind::Select { choices } => {
            render_select_option(idx, field, choices, current_val, is_busy, cx)
        }
        ExportOptionKind::String => {
            let cur_str = current_val
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            h_flex()
                .items_center()
                .justify_between()
                .child(div().text_xs().child(field.label.clone()))
                .child(div().text_xs().child(cur_str.to_owned()))
        }
    }
}

fn render_bool_option(
    idx: usize,
    field: &ExportOptionField,
    current_val: Option<&serde_json::Value>,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let enabled = current_val
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let key_owned = field.key.clone();
    let mut btn = Button::new(("exp-opt-bool", idx))
        .xsmall()
        .label(if enabled { "ON" } else { "OFF" });
    btn = if enabled {
        btn.primary()
    } else {
        btn.outline()
    };
    if !is_busy {
        btn = btn.on_click(cx.listener(move |this, _, _, cx| {
            this.controller
                .export_dialog
                .toggle_bool_option(&key_owned, enabled);
            cx.notify();
        }));
    }
    h_flex()
        .items_center()
        .justify_between()
        .child(div().text_xs().child(field.label.clone()))
        .child(btn)
}

fn render_int_option(
    idx: usize,
    field: &ExportOptionField,
    bounds: (i64, i64),
    current_val: Option<&serde_json::Value>,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let (min_v, max_v) = bounds;
    let val = current_val
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(min_v);
    let key_dec = field.key.clone();
    let key_inc = field.key.clone();
    let mut minus = Button::new(("exp-opt-dec", idx))
        .outline()
        .xsmall()
        .label("-");
    let mut plus = Button::new(("exp-opt-inc", idx))
        .outline()
        .xsmall()
        .label("+");
    if !is_busy {
        minus = minus.on_click(cx.listener(move |this, _, _, cx| {
            this.controller
                .export_dialog
                .step_int_option(&key_dec, -1, min_v, max_v);
            cx.notify();
        }));
        plus = plus.on_click(cx.listener(move |this, _, _, cx| {
            this.controller
                .export_dialog
                .step_int_option(&key_inc, 1, min_v, max_v);
            cx.notify();
        }));
    }
    h_flex()
        .items_center()
        .justify_between()
        .child(div().text_xs().child(field.label.clone()))
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(minus)
                .child(div().text_xs().child(format!("{val}")))
                .child(plus),
        )
}

fn render_select_option(
    idx: usize,
    field: &ExportOptionField,
    choices: &[crate::export::manifest::OptionChoice],
    current_val: Option<&serde_json::Value>,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let cur_str = current_val
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let current_label = choices
        .iter()
        .find(|c| c.value == cur_str)
        .map_or_else(|| cur_str.to_owned(), |c| c.label.clone());
    let items: Vec<(String, String)> = choices
        .iter()
        .map(|c| (c.value.clone(), c.label.clone()))
        .collect();
    let cur_owned = cur_str.to_owned();
    let key_owned = field.key.clone();
    let menu_title = field.label.clone();
    let weak = cx.weak_entity();
    let disabled = is_busy || items.is_empty();

    let combo = render_combo_button(
        ("exp-opt-combo", idx),
        current_label,
        280.0,
        disabled,
        move |menu, _, _| {
            let mut popup = menu.label(menu_title.clone());
            for (val, label) in &items {
                let weak_item = weak.clone();
                let key_item = key_owned.clone();
                let val_item = val.clone();
                popup = popup.item(
                    PopupMenuItem::new(label.clone())
                        .checked(*val == cur_owned)
                        .on_click(move |_, _, cx| {
                            let _ = weak_item.update(cx, |this, cx| {
                                this.controller
                                    .export_dialog
                                    .set_select_option(&key_item, val_item.clone());
                                cx.notify();
                            });
                        }),
                );
            }
            popup
        },
    );

    h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_xs().flex_shrink_0().child(field.label.clone()))
        .child(div().flex_1().min_w(px(140.)).max_w(px(300.)).child(combo))
}

fn render_output_target_row(
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let theme = cx.theme().clone();
    let mut pick_btn = Button::new("exp-pick-out")
        .outline()
        .xsmall()
        .label("出力先変更...");
    if !is_busy {
        pick_btn = pick_btn.on_click(cx.listener(|this, _, _, cx| {
            let ext = this
                .controller
                .export_dialog
                .selected_preset()
                .map_or("mkv", |p| p.container_ext.as_str())
                .to_owned();
            let current = PathBuf::from(&this.controller.export_dialog.output_path);
            let dir = current
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            let suggested = format!("sea_export.{ext}");
            let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
            cx.spawn(async move |view, cx| {
                if let Ok(Ok(Some(chosen))) = rx.await {
                    let _ = view.update(cx, |win, cx| {
                        win.controller.export_dialog.output_path =
                            chosen.to_string_lossy().into_owned();
                        cx.notify();
                    });
                }
            })
            .detach();
        }));
    }

    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("保存先ファイル"),
        )
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .p_2()
                        .rounded(px(4.))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_middle()
                        .text_xs()
                        .child(state.output_path.clone()),
                )
                .child(div().flex_shrink_0().child(pick_btn)),
        )
}

fn render_progress_and_actions(
    state: &ExportDialogState,
    is_busy: bool,
    cx: &mut Context<SeaEditorWindow>,
) -> Div {
    let theme = cx.theme().clone();
    let mut section = v_flex()
        .flex_shrink_0()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border);
    if let Some(prog) = &state.last_progress {
        let ratio = if prog.total_frames == 0 {
            0.0_f32
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                (prog.frames_completed as f32 / prog.total_frames as f32).clamp(0.0, 1.0)
            }
        };
        section = section.child(
            v_flex()
                .gap_1()
                .child(div().text_xs().child(prog.message.clone()))
                .child(
                    div()
                        .w_full()
                        .h(px(8.))
                        .rounded(px(4.))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(relative(ratio))
                                .rounded(px(3.))
                                .bg(rgba(0x3a82_f6ff)),
                        ),
                ),
        );
    }
    if let Some(err) = &state.last_error {
        section = section.child(
            div()
                .p_2()
                .rounded(px(4.))
                .bg(rgba(0x6e20_20ff))
                .text_xs()
                .child(format!("エラー: {err}")),
        );
    }
    if let Some(done) = &state.last_completed {
        section = section.child(
            div()
                .p_2()
                .rounded(px(4.))
                .bg(rgba(0x1f52_36ff))
                .text_xs()
                .child(format!(
                    "完了: {} ({} bytes / {} frames)",
                    done.output_path.display(),
                    done.file_size_bytes,
                    done.frames_encoded
                )),
        );
    }

    let can_start = !is_busy
        && state
            .selected_plugin()
            .is_some_and(|p| p.executable_path.is_some());
    let mut start_btn = Button::new("exp-start")
        .primary()
        .small()
        .label("書き出し開始");
    if can_start {
        start_btn = start_btn.on_click(cx.listener(|this, _, _, cx| {
            if let Err(err) = this.controller.start_export_from_dialog() {
                this.controller.export_dialog.last_error = Some(err);
            }
            cx.notify();
        }));
    }

    let mut action_row = h_flex().gap_2().justify_end();
    if is_busy {
        let cancel_btn = Button::new("exp-cancel")
            .outline()
            .small()
            .label("書き出しを中断")
            .on_click(cx.listener(|this, _, _, cx| {
                this.controller.cancel_active_export();
                cx.notify();
            }));
        action_row = action_row.child(cancel_btn);
    }
    action_row = action_row.child(start_btn);
    section.child(action_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::manifest::{
        ExportPluginManifest, PluginCapabilities, PluginKind, PluginLicenseInfo, PresetSchema,
    };
    use crate::export::registry::DiscoveredPlugin;

    #[test]
    fn resolution_preset_ensures_even_dimensions_and_merges_options() {
        let doc = ProjectDocument::default();
        assert_eq!(ExportResolutionPreset::Hd720p.dimensions(&doc), (1280, 720));
        assert_eq!(
            ExportResolutionPreset::Fhd1080p.dimensions(&doc),
            (1920, 1080)
        );

        let mut state = ExportDialogState::default();
        state.toggle_bool_option("fast", false);
        let merged = state.merged_options();
        assert_eq!(
            merged.get("fast").and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let sample = DiscoveredPlugin {
            manifest: ExportPluginManifest {
                schema_version: 1,
                id: "dummy".to_owned(),
                name: "Dummy".to_owned(),
                version: "0.1.0".to_owned(),
                kind: PluginKind::Export,
                entrypoint: "dummy".to_owned(),
                args: Vec::new(),
                containers: vec!["mp4".to_owned()],
                license: PluginLicenseInfo {
                    spdx: "MIT".to_owned(),
                    notice: None,
                    requires_external_binary: false,
                },
                capabilities: PluginCapabilities {
                    supports_alpha: false,
                    supports_audio: true,
                    pixel_format: crate::export::protocol::PixelFormat::Bgra8,
                    audio_sample_format: crate::export::protocol::AudioSampleFormat::F32Interleaved,
                    hw_accelerated: false,
                },
                presets: vec![PresetSchema {
                    id: "p1".to_owned(),
                    label: "P1".to_owned(),
                    description: String::new(),
                    video_codec: "test".to_owned(),
                    audio_codec: "test".to_owned(),
                    container_ext: "mp4".to_owned(),
                    default_options: serde_json::json!({ "speed": 5 }),
                }],
                option_fields: Vec::new(),
            },
            manifest_path: PathBuf::from("plugin.json"),
            plugin_dir: PathBuf::from("."),
            executable_path: None,
        };
        let _ = sample;
    }

    #[test]
    fn modal_bounds_clamp_cleanly_for_small_and_large_viewports() {
        assert_eq!(compute_export_modal_bounds(1920.0, 1080.0), (560.0, 620.0));
        assert_eq!(compute_export_modal_bounds(1280.0, 720.0), (560.0, 620.0));
        assert_eq!(compute_export_modal_bounds(400.0, 400.0), (368.0, 352.0));
        assert_eq!(compute_export_modal_bounds(200.0, 200.0), (320.0, 240.0));
    }
}
