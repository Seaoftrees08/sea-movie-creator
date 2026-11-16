//! 外部ツール（`ffmpeg` 等）の実行ファイルパス設定・検証・保存モーダル UI。

use std::path::Path;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, PathPromptOptions, Styled as _, Window, div,
    px, rgba,
};

use crate::export::external_tools::{
    ExternalToolsConfig, resolve_executable_path, validate_ffmpeg_input,
};
use crate::ui::window::SeaEditorWindow;

/// 外部ツール設定モーダルの編集中状態。
pub struct ExternalToolsModalState {
    /// `ffmpeg` パス／コマンド入力欄の状態。
    pub input: Entity<InputState>,
    /// ファイル選択ダイアログから非同期で渡された未反映パス値。
    pub pending_value: Option<String>,
    /// 入力検証エラーメッセージ。
    pub error: Option<String>,
}

impl SeaEditorWindow {
    /// 外部ツール (`ffmpeg`) 設定モーダルを開く。
    pub fn open_external_tools_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let initial = self
            .controller
            .external_tools
            .ffmpeg_path
            .clone()
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("例: C:\\tools\\ffmpeg\\bin\\ffmpeg.exe または ffmpeg")
                .default_value(initial)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                this.apply_external_tools_modal(window, cx);
            }
        });
        let focus_handle = input.read(cx).focus_handle(cx);
        let focus_sub = cx.on_focus_in(&focus_handle, window, |_, _, cx| cx.notify());
        let blur_sub = cx.on_blur(&focus_handle, window, |_, _, cx| cx.notify());
        self.external_tools_modal = Some(ExternalToolsModalState {
            input,
            pending_value: None,
            error: None,
        });
        self.dynamic_input_sub = Some(sub);
        self.modal_focus_subs = vec![focus_sub, blur_sub];
        cx.notify();
    }

    /// 外部ツール設定モーダルを破棄して閉じる。
    pub fn close_external_tools_modal(&mut self) {
        self.external_tools_modal = None;
        self.dynamic_input_sub = None;
        self.modal_focus_subs.clear();
    }

    /// モーダルで入力された `ffmpeg` パスを検証・永続化し、書き出しダイアログへ即座に反映する。
    pub fn apply_external_tools_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = &self.external_tools_modal else {
            return;
        };
        let raw = modal.input.read(cx).value().to_string();
        let validated = match validate_ffmpeg_input(&raw) {
            Ok(v) => v,
            Err(err) => {
                if let Some(m) = &mut self.external_tools_modal {
                    m.error = Some(err);
                }
                cx.notify();
                return;
            }
        };

        let next_cfg = ExternalToolsConfig {
            ffmpeg_path: validated,
        };
        if let Some(save_path) = ExternalToolsConfig::default_path()
            && let Err(err) = next_cfg.save_to_path(&save_path)
        {
            if let Some(m) = &mut self.external_tools_modal {
                m.error = Some(err);
            }
            cx.notify();
            return;
        }

        let resolved = next_cfg.resolved_ffmpeg_path_string();
        self.controller.external_tools = next_cfg;
        let base_dir = self
            .controller
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        self.controller
            .export_dialog
            .apply_external_ffmpeg_path(resolved.clone(), base_dir.as_deref());
        self.controller.status_message = if resolved.is_some() {
            "外部 ffmpeg 設定を保存しました（書き出し設定で有効化）".to_owned()
        } else {
            "外部 ffmpeg 設定を解除しました".to_owned()
        };

        self.close_external_tools_modal();
        window.focus(&self.workspace_focus, cx);
        cx.notify();
    }

    fn browse_external_ffmpeg_executable(cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("ffmpeg 実行ファイルを選択".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |view, cx| {
                    if let Some(modal) = &mut view.external_tools_modal {
                        modal.pending_value = Some(path.to_string_lossy().into_owned());
                        modal.error = None;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn detect_external_ffmpeg_from_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = resolve_executable_path("ffmpeg") else {
            if let Some(modal) = &mut self.external_tools_modal {
                modal.error = Some(
                    "PATH 上で ffmpeg が見つかりませんでした。「参照...」から実行ファイルを直接指定してください。"
                        .to_owned(),
                );
            }
            cx.notify();
            return;
        };
        let found_str = found.to_string_lossy().into_owned();
        if let Some(modal) = &mut self.external_tools_modal {
            modal.error = None;
            modal
                .input
                .update(cx, |input, cx| input.set_value(&found_str, window, cx));
        }
        cx.notify();
    }
}

/// 外部ツール (`ffmpeg`) 設定モーダルオーバーレイを描画する。
pub fn render_external_tools_modal_overlay(
    modal: &ExternalToolsModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    div()
        .id("external-tools-modal-backdrop")
        .absolute()
        .inset_0()
        .bg(rgba(0x0206_17aa))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.stop_propagation()),
        )
        .child(render_external_tools_card(modal, cx))
        .into_any_element()
}

fn render_external_tools_card(
    modal: &ExternalToolsModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let theme = cx.theme().clone();
    let config_hint = ExternalToolsConfig::default_path().map_or_else(
        || "設定ファイル: ローカル設定ディレクトリ".to_owned(),
        |p| format!("保存先: {}", p.display()),
    );

    let mut card = v_flex()
        .id("external-tools-modal-card")
        .w(px(520.0))
        .p_4()
        .gap_3()
        .rounded(px(8.0))
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.stop_propagation()),
        )
        .child(
            div()
                .text_sm()
                .font_semibold()
                .text_color(theme.foreground)
                .child("外部ツール設定 (ffmpeg)"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(
                    "ユーザー導入済みの ffmpeg 実行ファイルを設定すると、動画書き出しダイアログで外部 ffmpeg バックエンドおよびプリセットが選択可能になります。",
                ),
        )
        .child(render_external_tools_input_row(modal, cx));

    if let Some(err) = &modal.error {
        card = card.child(
            div()
                .p_2()
                .rounded(px(4.0))
                .bg(rgba(0x5a1d_1dff))
                .text_xs()
                .text_color(rgba(0xfeca_caff))
                .child(err.clone()),
        );
    }

    card.child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis_middle()
            .child(config_hint),
    )
    .child(render_external_tools_footer(cx))
    .into_any_element()
}

fn render_external_tools_input_row(
    modal: &ExternalToolsModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let theme = cx.theme().clone();
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("ffmpeg 実行ファイルパス / コマンド"),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&modal.input).small()),
                )
                .child(
                    Button::new("ext-ffmpeg-browse-btn")
                        .outline()
                        .small()
                        .label("参照...")
                        .on_click(cx.listener(|_, _, _, cx| {
                            SeaEditorWindow::browse_external_ffmpeg_executable(cx);
                        })),
                )
                .child(
                    Button::new("ext-ffmpeg-detect-btn")
                        .outline()
                        .small()
                        .label("自動検出")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.detect_external_ffmpeg_from_path(window, cx);
                        })),
                ),
        )
        .into_any_element()
}

fn render_external_tools_footer(cx: &mut Context<SeaEditorWindow>) -> AnyElement {
    h_flex()
        .justify_between()
        .items_center()
        .pt_1()
        .child(
            Button::new("ext-ffmpeg-clear-btn")
                .ghost()
                .small()
                .label("設定をクリア（非表示に戻す）")
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(m) = &mut this.external_tools_modal {
                        m.error = None;
                        m.input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    this.apply_external_tools_modal(window, cx);
                })),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("ext-ffmpeg-cancel-btn")
                        .outline()
                        .small()
                        .label("キャンセル")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_external_tools_modal();
                            window.focus(&this.workspace_focus, cx);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("ext-ffmpeg-save-btn")
                        .primary()
                        .small()
                        .label("保存")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_external_tools_modal(window, cx);
                        })),
                ),
        )
        .into_any_element()
}
