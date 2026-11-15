//! 右側 1/5 リサイズ可能タブパネル: クリップ設定・AI チャット枠・レイヤー・プロジェクト情報。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{Context, Entity, IntoElement, ParentElement as _, Styled as _, div};

use crate::timeline::document::ClipKind;
use crate::ui::app_controller::{AppController, RightPanelTab};
use crate::ui::window::SeaEditorWindow;

/// 右側パネルのタブバーと選択中タブコンテンツを描画する。
pub fn render_right_panel(
    controller: &AppController,
    opacity_slider: &Entity<SliderState>,
    ai_chat_input: &Entity<InputState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let selected_ix = match controller.right_tab {
        RightPanelTab::ClipInspector => 0,
        RightPanelTab::AiChat => 1,
        RightPanelTab::LayerList => 2,
        RightPanelTab::ProjectInfo => 3,
    };
    let weak = cx.weak_entity();
    let theme = cx.theme();

    v_flex()
        .size_full()
        .bg(theme.background)
        .border_l_1()
        .border_color(theme.border)
        .child(
            TabBar::new("right-inspector-tabs")
                .selected_index(selected_ix)
                .child(Tab::new().label("Clip"))
                .child(Tab::new().label("AI Chat"))
                .child(Tab::new().label("Layers"))
                .child(Tab::new().label("Project"))
                .on_click(move |ix, _, cx| {
                    let next_tab = match *ix {
                        1 => RightPanelTab::AiChat,
                        2 => RightPanelTab::LayerList,
                        3 => RightPanelTab::ProjectInfo,
                        _ => RightPanelTab::ClipInspector,
                    };
                    let _ = weak.update(cx, |view, cx| {
                        view.controller.right_tab = next_tab;
                        cx.notify();
                    });
                }),
        )
        .child(match controller.right_tab {
            RightPanelTab::ClipInspector => {
                render_clip_tab(controller, opacity_slider, cx).into_any_element()
            }
            RightPanelTab::AiChat => render_ai_chat_tab(ai_chat_input, cx).into_any_element(),
            RightPanelTab::LayerList => render_layers_tab(controller, cx).into_any_element(),
            RightPanelTab::ProjectInfo => render_project_tab(controller, cx).into_any_element(),
        })
}

fn render_clip_tab(
    controller: &AppController,
    opacity_slider: &Entity<SliderState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let Some((layer, clip)) = controller.selected_clip() else {
        return v_flex()
            .p_3()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("クリップが選択されていません。")
            .child("メニューまたはドラッグ操作でタイムライン上のクリップを選択してください。");
    };

    let clip_info = format!(
        "{} ({})  Track: {}  [{}..{})",
        clip.name,
        match &clip.kind {
            ClipKind::Video { .. } => "Video",
            ClipKind::Audio { .. } => "Audio",
            ClipKind::Image { .. } => "Image",
            ClipKind::Subtitle(_) => "Subtitle",
        },
        layer.name,
        clip.timeline_start.get(),
        clip.timeline_start.get() + clip.duration.get(),
    );
    let can_opacity = matches!(
        clip.kind,
        ClipKind::Video { .. } | ClipKind::Image { .. } | ClipKind::Subtitle(_)
    );
    let can_speed = matches!(clip.kind, ClipKind::Video { .. } | ClipKind::Audio { .. });

    let mut panel = v_flex()
        .p_3()
        .gap_3()
        .text_xs()
        .child(div().text_color(theme.foreground).child(clip_info));

    if can_opacity {
        panel = panel
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("不透明度 (Opacity)"),
            )
            .child(Slider::new(opacity_slider));
    }

    if can_speed {
        panel = panel
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("再生速度 (PlaybackRate)"),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(speed_button("speed-05", "0.5x", 1, 2, cx))
                    .child(speed_button("speed-10", "1.0x", 1, 1, cx))
                    .child(speed_button("speed-15", "1.5x", 3, 2, cx))
                    .child(speed_button("speed-20", "2.0x", 2, 1, cx)),
            );
    }

    panel.child(
        h_flex()
            .gap_2()
            .child(
                Button::new("clip-split-btn")
                    .ghost()
                    .small()
                    .label("現在位置で分割")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.controller.split_selected_clip_at_playhead();
                        this.sync_widgets_from_controller(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("clip-delete-btn")
                    .danger()
                    .small()
                    .label("削除")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.controller.delete_selected_clip();
                        this.sync_widgets_from_controller(cx);
                        cx.notify();
                    })),
            ),
    )
}

fn speed_button(
    id: &'static str,
    label: &'static str,
    num: u32,
    den: u32,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .small()
        .label(label)
        .on_click(cx.listener(move |this, _, _, cx| {
            let _ = this.controller.set_selected_clip_playback_rate(num, den);
            cx.notify();
        }))
}

fn render_ai_chat_tab(
    ai_chat_input: &Entity<InputState>,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .p_3()
        .gap_2()
        .text_xs()
        .child(
            div()
                .text_color(theme.foreground)
                .child("AI 編集アシスタント（Phase B 連携枠）"),
        )
        .child(
            div()
                .flex_1()
                .p_2()
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .text_color(theme.muted_foreground)
                .child("EditCommand / CommandDispatcher 直結の対話ログ領域（Phase B で接続）"),
        )
        .child(
            h_flex()
                .gap_2()
                .child(div().flex_1().child(Input::new(ai_chat_input)))
                .child(
                    Button::new("ai-send-btn")
                        .primary()
                        .small()
                        .label("送信")
                        .on_click(cx.listener(|this, _, window, cx| {
                            let msg = this.ai_chat_input.read(cx).value().to_string();
                            if !msg.trim().is_empty() {
                                this.controller.ai_chat_draft.clone_from(&msg);
                                this.controller.status_message = format!("AI コマンド予約: {msg}");
                                this.ai_chat_input.update(cx, |input, cx| {
                                    input.set_value("", window, cx);
                                });
                            }
                            cx.notify();
                        })),
                ),
        )
}

fn render_layers_tab(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .p_3()
        .gap_2()
        .text_xs()
        .children(
            controller
                .document
                .layers
                .iter()
                .enumerate()
                .map(|(ix, layer)| {
                    let lid = layer.id.clone();
                    let mute_label = if layer.muted { "Unmute" } else { "Mute" };
                    let title = format!("{}: {} ({} clips)", ix + 1, layer.name, layer.clips.len());
                    h_flex()
                        .justify_between()
                        .items_center()
                        .p_2()
                        .border_1()
                        .border_color(theme.border)
                        .rounded_sm()
                        .child(div().text_color(theme.foreground).child(title))
                        .child(
                            Button::new(("layer-mute-btn", ix))
                                .ghost()
                                .small()
                                .label(mute_label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = this.controller.toggle_layer_muted(&lid);
                                    cx.notify();
                                })),
                        )
                }),
        )
}

fn render_project_tab(
    controller: &AppController,
    cx: &mut Context<SeaEditorWindow>,
) -> impl IntoElement {
    let theme = cx.theme();
    let doc = &controller.document;
    let audio_mode = if controller.has_hardware_audio_output() {
        "AudioMasterClock (cpal hardware)"
    } else {
        "SystemPlaybackClock (fallback)"
    };
    v_flex()
        .p_3()
        .gap_2()
        .text_xs()
        .text_color(theme.foreground)
        .child(format!("プロジェクト名: {}", doc.name))
        .child(format!(
            "解像度 / FPS: {}x{} @ {:.2}fps",
            doc.resolution.width,
            doc.resolution.height,
            doc.fps.as_f64()
        ))
        .child(format!("総フレーム数: {}", doc.total_duration_frames()))
        .child(format!(
            "素材数 / レイヤー数: {} / {}",
            doc.assets.len(),
            doc.layers.len()
        ))
        .child(format!("同期クロック: {audio_mode}"))
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(format!("状態: {}", controller.status_message)),
        )
}
