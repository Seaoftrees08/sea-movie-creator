//! エディタ上部のメニューバー構築・7トップメニュー・キーコンフィグ連動 (`EditorMain`) バインド。

use std::rc::Rc;

use gpui_kit::base::GlobalState;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::{
    Action, App, Entity, KeyBinding, KeyBindingContextPredicate, Menu, MenuItem, actions,
};

use crate::ui::app_controller::{PreviewZoomMode, RightPanelTab};
use crate::ui::i18n::{active_language_code, available_languages, tr};
use crate::ui::key_config::{KeyConfig, ShortcutAction};

/// `lang/` から読み込んだ任意言語コードへ表示言語を切り替えるアクション。
#[derive(Clone, PartialEq, Eq, gpui_kit::Action)]
#[action(namespace = sea_editor, no_json)]
pub struct SelectLanguageAction {
    /// 切り替え先の言語コード（例: `"ja"`, `"en"`, `"ko"`）。
    pub code: String,
}

actions!(
    sea_editor,
    [
        /// 新規プロジェクトを作成する。
        NewProject,
        /// 既存プロジェクトファイルを開く。
        OpenProject,
        /// プロジェクトを上書き保存する。
        SaveProject,
        /// プロジェクトを別名で保存する。
        SaveProjectAs,
        /// 素材メディアを追加する。
        ImportMedia,
        /// アプリケーションを終了する。
        QuitApp,
        /// 操作を 1 つ元に戻す。
        UndoAction,
        /// 取り消した操作をやり直す。
        RedoAction,
        /// 選択クリップを切り取る。
        CutClip,
        /// 選択クリップをコピーする。
        CopyClip,
        /// クリップボード内容を再生位置へ貼り付ける。
        PasteClip,
        /// 選択クリップを複製する。
        DuplicateClip,
        /// 選択中のクリップを削除する。
        DeleteClip,
        /// 全クリップを選択する。
        SelectAllClips,
        /// 選択範囲を解除する。
        ClearSelectionRange,
        /// 再生位置で選択クリップを分割する。
        SplitClip,
        /// 現在位置へ中間点を追加する。
        AddMidpointAction,
        /// 現在位置付近の中間点を削除する。
        RemoveMidpointAction,
        /// 現在位置へ字幕クリップを追加する。
        AddSubtitleAction,
        /// 再生位置を選択範囲の開始 `[` に設定する。
        SetSelectionStartAction,
        /// 再生位置を選択範囲の終了 `]` に設定する。
        SetSelectionEndAction,
        /// 同一時刻の重なりクリップまたはパネルを巡回する (`Tab`)。
        CycleClipAction,
        /// タイムライン長変更モーダルを開く。
        SetTimelineLengthAction,
        /// タイムライン長を最終クリップ末尾に合わせる。
        SnapTimelineLengthAutoAction,
        /// シーク・オブジェクト移動時に選択を追従させるか切り替える。
        ToggleFollowSelectedOnSeekAction,
        /// ショートカット設定モーダルを開く。
        OpenKeyConfigAction,
        /// 外部ツール (`ffmpeg`) 設定モーダルを開く。
        OpenExternalToolsAction,
        /// プレビュー枠・アンカー・ハンドル表示を切り替える。
        TogglePreviewGuidesAction,
        /// プレビューグリッド表示を切り替える。
        TogglePreviewGridAction,
        /// プレビューマスター音声波形表示を切り替える。
        TogglePreviewWaveformAction,
        /// プレビュー倍率を Fit にする。
        PreviewZoomFitAction,
        /// プレビュー倍率を 50% にする。
        PreviewZoom50Action,
        /// プレビュー倍率を 100% にする。
        PreviewZoom100Action,
        /// プレビュー倍率を 200% にする。
        PreviewZoom200Action,
        /// タイムラインを拡大する。
        TimelineZoomIn,
        /// タイムラインを縮小する。
        TimelineZoomOut,
        /// 右パネルをクリップ設定タブへ切り替える。
        ShowTabClipInspectorAction,
        /// 右パネルを AI チャットタブへ切り替える。
        ShowTabAiChatAction,
        /// 右パネルをレイヤータブへ切り替える。
        ShowTabLayerListAction,
        /// 右パネルをプロジェクト情報タブへ切り替える。
        ShowTabProjectInfoAction,
        /// 再生と一時停止を切り替える。
        TogglePlayPause,
        /// 再生を停止する。
        StopPlaybackAction,
        /// 再生停止時に開始位置へ戻すオプションを切り替える。
        ToggleReturnToStartOnStopAction,
        /// 先頭フレームへ移動する。
        SeekStart,
        /// 末尾フレームへ移動する。
        SeekEnd,
        /// 1 フレーム戻る。
        StepBackOneFrame,
        /// 1 フレーム進む。
        StepForwardOneFrame,
        /// 0.5 秒戻る。
        StepBackHalfSecAction,
        /// 0.5 秒進む。
        StepForwardHalfSecAction,
        /// 前の境界または中間点へ移動する。
        PrevBoundaryAction,
        /// 次の境界または中間点へ移動する。
        NextBoundaryAction,
        /// X を -1px 移動する。
        NudgeLeftAction,
        /// X を +1px 移動する。
        NudgeRightAction,
        /// Y を -1px 移動する。
        NudgeUpAction,
        /// Y を +1px 移動する。
        NudgeDownAction,
        /// 反時計回り -1° 回転する。
        RotateLeftAction,
        /// 時計回り +1° 回転する。
        RotateRightAction,
        /// 書き出し設定ダイアログを開く。
        OpenExportDialog,
        /// 選択範囲のみで書き出しダイアログを開く。
        ExportSelectionDialogAction,
        /// `lang/*.json` を再読み込みする。
        ReloadLanguagePacksAction,
    ]
);

/// プレビュー上の 3 種オーバーレイ表示チェックフラグ束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewOverlayChecks {
    /// 枠・ハンドル表示。
    pub guides: bool,
    /// グリッド線表示。
    pub grid: bool,
    /// マスター波形表示。
    pub waveform: bool,
}

impl Default for PreviewOverlayChecks {
    fn default() -> Self {
        Self {
            guides: true,
            grid: false,
            waveform: true,
        }
    }
}

/// メニューバーのチェック状態反映用スナップショット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuCheckState {
    /// プレビューオーバーレイ表示チェック状態。
    pub preview_overlays: PreviewOverlayChecks,
    /// シーク追従チェック状態。
    pub follow_selected_on_seek: bool,
    /// 再生停止時に開始位置へ戻すチェック状態。
    pub return_to_start_on_stop: bool,
    /// 現在のプレビュー倍率モード。
    pub preview_zoom: PreviewZoomMode,
    /// 現在の右パネルタブ。
    pub right_tab: RightPanelTab,
}

/// 起動時のメニュー項目・デフォルトキーバインドおよび `AppMenuBar` を初期化する。
pub fn setup_menus(cx: &mut App) -> Entity<AppMenuBar> {
    let default_cfg = KeyConfig::load_or_default();
    apply_key_config_bindings(cx, &default_cfg);
    update_app_menus(cx, MenuCheckState::default());
    AppMenuBar::new(cx)
}

/// メニュー項目のチェック状態を最新化して OS メニュー・アプリメニューバー両方へ反映する。
pub fn update_app_menus(cx: &mut App, state: MenuCheckState) {
    let owned_menus = build_app_menus(state)
        .into_iter()
        .map(Menu::owned)
        .collect();
    GlobalState::global_mut(cx).set_app_menus(owned_menus);
    cx.set_menus(build_app_menus(state));
}

fn is_editor_main_binding(binding: &KeyBinding) -> bool {
    matches!(
        binding.predicate().as_deref(),
        Some(KeyBindingContextPredicate::Identifier(name)) if name == "EditorMain"
    )
}

/// [`KeyConfig`] の全割り当てを `"EditorMain"` コンテキストへ再登録する。
///
/// `gpui_kit::init(cx)` が登録した `"Input"`（`Backspace` / `Delete` / 矢印キー等）や
/// 各標準ウィジェットのキーバインドを保持したまま、`"EditorMain"` コンテキスト分のみを更新する。
pub fn apply_key_config_bindings(cx: &mut App, config: &KeyConfig) {
    let preserved: Vec<KeyBinding> = {
        let map_handle = cx.key_bindings();
        let km = map_handle.borrow();
        km.bindings()
            .filter(|kb| !is_editor_main_binding(kb))
            .cloned()
            .collect()
    };
    cx.clear_key_bindings();
    if !preserved.is_empty() {
        cx.bind_keys(preserved);
    }

    let editor_ctx: Option<Rc<KeyBindingContextPredicate>> =
        KeyBindingContextPredicate::parse("EditorMain")
            .ok()
            .map(Rc::new);

    let mut used_strokes = Vec::new();
    let mut bindings = Vec::new();
    for &action in ShortcutAction::all() {
        let stroke = config.binding_for(action);
        if let Ok(kb) = KeyBinding::load(
            stroke,
            boxed_action_for(action),
            editor_ctx.clone(),
            false,
            None,
            cx,
        ) {
            used_strokes.push(stroke.to_ascii_lowercase());
            bindings.push(kb);
        }
    }

    // 標準サブバインド（ユーザー上書きに未衝突の場合のみ有効）
    for (fallback_key, boxed) in [
        ("ctrl-shift-z", Box::new(RedoAction) as Box<dyn Action>),
        ("backspace", Box::new(DeleteClip) as Box<dyn Action>),
    ] {
        if !used_strokes.iter().any(|s| s == fallback_key)
            && let Ok(kb) =
                KeyBinding::load(fallback_key, boxed, editor_ctx.clone(), false, None, cx)
        {
            bindings.push(kb);
        }
    }

    cx.bind_keys(bindings);
}

fn boxed_action_for(action: ShortcutAction) -> Box<dyn Action> {
    match action {
        ShortcutAction::NewProject => Box::new(NewProject),
        ShortcutAction::OpenProject => Box::new(OpenProject),
        ShortcutAction::SaveProject => Box::new(SaveProject),
        ShortcutAction::SaveProjectAs => Box::new(SaveProjectAs),
        ShortcutAction::ImportMedia => Box::new(ImportMedia),
        ShortcutAction::Undo => Box::new(UndoAction),
        ShortcutAction::Redo => Box::new(RedoAction),
        ShortcutAction::Cut => Box::new(CutClip),
        ShortcutAction::Copy => Box::new(CopyClip),
        ShortcutAction::Paste => Box::new(PasteClip),
        ShortcutAction::Duplicate => Box::new(DuplicateClip),
        ShortcutAction::Delete => Box::new(DeleteClip),
        ShortcutAction::SelectAll => Box::new(SelectAllClips),
        ShortcutAction::ClearSelection => Box::new(ClearSelectionRange),
        ShortcutAction::SplitClip => Box::new(SplitClip),
        ShortcutAction::AddMidpoint => Box::new(AddMidpointAction),
        ShortcutAction::RemoveMidpoint => Box::new(RemoveMidpointAction),
        ShortcutAction::AddSubtitle => Box::new(AddSubtitleAction),
        ShortcutAction::SetSelectionStart => Box::new(SetSelectionStartAction),
        ShortcutAction::SetSelectionEnd => Box::new(SetSelectionEndAction),
        ShortcutAction::CycleClip => Box::new(CycleClipAction),
        ShortcutAction::TogglePlayPause => Box::new(TogglePlayPause),
        ShortcutAction::StopPlayback => Box::new(StopPlaybackAction),
        ShortcutAction::SeekStart => Box::new(SeekStart),
        ShortcutAction::SeekEnd => Box::new(SeekEnd),
        ShortcutAction::StepBackFrame => Box::new(StepBackOneFrame),
        ShortcutAction::StepForwardFrame => Box::new(StepForwardOneFrame),
        ShortcutAction::StepBackHalfSec => Box::new(StepBackHalfSecAction),
        ShortcutAction::StepForwardHalfSec => Box::new(StepForwardHalfSecAction),
        ShortcutAction::PrevBoundary => Box::new(PrevBoundaryAction),
        ShortcutAction::NextBoundary => Box::new(NextBoundaryAction),
        ShortcutAction::NudgeLeft => Box::new(NudgeLeftAction),
        ShortcutAction::NudgeRight => Box::new(NudgeRightAction),
        ShortcutAction::NudgeUp => Box::new(NudgeUpAction),
        ShortcutAction::NudgeDown => Box::new(NudgeDownAction),
        ShortcutAction::RotateLeft => Box::new(RotateLeftAction),
        ShortcutAction::RotateRight => Box::new(RotateRightAction),
        ShortcutAction::TimelineZoomIn => Box::new(TimelineZoomIn),
        ShortcutAction::TimelineZoomOut => Box::new(TimelineZoomOut),
        ShortcutAction::TogglePreviewGuides => Box::new(TogglePreviewGuidesAction),
        ShortcutAction::TogglePreviewGrid => Box::new(TogglePreviewGridAction),
        ShortcutAction::TogglePreviewWaveform => Box::new(TogglePreviewWaveformAction),
        ShortcutAction::SetTimelineLength => Box::new(SetTimelineLengthAction),
        ShortcutAction::OpenKeyConfig => Box::new(OpenKeyConfigAction),
        ShortcutAction::OpenExportDialog => Box::new(OpenExportDialog),
    }
}

/// 7 大項目のトップレベルメニュー階層を生成する。
#[must_use]
pub fn build_app_menus(state: MenuCheckState) -> Vec<Menu> {
    vec![
        build_file_menu(),
        build_edit_menu(),
        build_timeline_menu(),
        build_settings_menu(state),
        build_view_menu(state),
        build_playback_menu(state),
        build_export_menu(),
    ]
}

fn build_file_menu() -> Menu {
    Menu {
        name: tr("menu.file").into(),
        items: vec![
            MenuItem::action(tr("menu.file.new"), NewProject),
            MenuItem::action(tr("menu.file.open"), OpenProject),
            MenuItem::action(tr("menu.file.save"), SaveProject),
            MenuItem::action(tr("menu.file.save_as"), SaveProjectAs),
            MenuItem::separator(),
            MenuItem::action(tr("menu.file.import"), ImportMedia),
            MenuItem::separator(),
            MenuItem::action(tr("menu.file.quit"), QuitApp),
        ],
        disabled: false,
    }
}

fn build_edit_menu() -> Menu {
    Menu {
        name: tr("menu.edit").into(),
        items: vec![
            MenuItem::action(tr("menu.edit.undo"), UndoAction),
            MenuItem::action(tr("menu.edit.redo"), RedoAction),
            MenuItem::separator(),
            MenuItem::action(tr("menu.edit.cut"), CutClip),
            MenuItem::action(tr("menu.edit.copy"), CopyClip),
            MenuItem::action(tr("menu.edit.paste"), PasteClip),
            MenuItem::action(tr("menu.edit.duplicate"), DuplicateClip),
            MenuItem::action(tr("menu.edit.delete"), DeleteClip),
            MenuItem::separator(),
            MenuItem::action(tr("menu.edit.select_all"), SelectAllClips),
            MenuItem::action(tr("menu.edit.clear_selection"), ClearSelectionRange),
        ],
        disabled: false,
    }
}

fn build_timeline_menu() -> Menu {
    Menu {
        name: tr("menu.timeline").into(),
        items: vec![
            MenuItem::action(tr("menu.timeline.split"), SplitClip),
            MenuItem::action(tr("menu.timeline.add_midpoint"), AddMidpointAction),
            MenuItem::action(tr("menu.timeline.remove_midpoint"), RemoveMidpointAction),
            MenuItem::separator(),
            MenuItem::action(tr("menu.timeline.add_subtitle"), AddSubtitleAction),
            MenuItem::separator(),
            MenuItem::action(tr("menu.timeline.set_sel_start"), SetSelectionStartAction),
            MenuItem::action(tr("menu.timeline.set_sel_end"), SetSelectionEndAction),
            MenuItem::action(tr("menu.timeline.cycle_clip"), CycleClipAction),
        ],
        disabled: false,
    }
}

fn build_settings_menu(state: MenuCheckState) -> Menu {
    Menu {
        name: tr("menu.settings").into(),
        items: vec![
            MenuItem::action(tr("menu.settings.timeline_length"), SetTimelineLengthAction),
            MenuItem::action(
                tr("menu.settings.timeline_snap_auto"),
                SnapTimelineLengthAutoAction,
            ),
            MenuItem::separator(),
            MenuItem::action(
                tr("menu.settings.follow_seek"),
                ToggleFollowSelectedOnSeekAction,
            )
            .checked(state.follow_selected_on_seek),
            MenuItem::separator(),
            MenuItem::submenu(build_language_submenu()),
            MenuItem::separator(),
            MenuItem::action(tr("menu.settings.external_tools"), OpenExternalToolsAction),
            MenuItem::action(tr("menu.settings.key_config"), OpenKeyConfigAction),
        ],
        disabled: false,
    }
}

fn build_language_submenu() -> Menu {
    let active = active_language_code();
    let mut items: Vec<MenuItem> = available_languages()
        .into_iter()
        .map(|lang| {
            let label = format!("{} ({})", lang.name, lang.code);
            let checked = lang.code == active;
            MenuItem::action(label, SelectLanguageAction { code: lang.code }).checked(checked)
        })
        .collect();
    items.push(MenuItem::separator());
    items.push(MenuItem::action(
        tr("menu.settings.language_reload"),
        ReloadLanguagePacksAction,
    ));
    Menu {
        name: tr("menu.settings.language").into(),
        items,
        disabled: false,
    }
}

fn build_view_menu(state: MenuCheckState) -> Menu {
    Menu {
        name: tr("menu.view").into(),
        items: vec![
            MenuItem::action(tr("menu.view.guides"), TogglePreviewGuidesAction)
                .checked(state.preview_overlays.guides),
            MenuItem::action(tr("menu.view.grid"), TogglePreviewGridAction)
                .checked(state.preview_overlays.grid),
            MenuItem::action(tr("menu.view.waveform"), TogglePreviewWaveformAction)
                .checked(state.preview_overlays.waveform),
            MenuItem::separator(),
            MenuItem::action(tr("menu.view.zoom_fit"), PreviewZoomFitAction)
                .checked(state.preview_zoom == PreviewZoomMode::Fit),
            MenuItem::action(tr("menu.view.zoom_50"), PreviewZoom50Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(50)),
            MenuItem::action(tr("menu.view.zoom_100"), PreviewZoom100Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(100)),
            MenuItem::action(tr("menu.view.zoom_200"), PreviewZoom200Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(200)),
            MenuItem::separator(),
            MenuItem::action(tr("menu.view.tl_zoom_in"), TimelineZoomIn),
            MenuItem::action(tr("menu.view.tl_zoom_out"), TimelineZoomOut),
            MenuItem::separator(),
            MenuItem::action(tr("menu.view.tab_inspector"), ShowTabClipInspectorAction)
                .checked(state.right_tab == RightPanelTab::ClipInspector),
            MenuItem::action(tr("menu.view.tab_ai"), ShowTabAiChatAction)
                .checked(state.right_tab == RightPanelTab::AiChat),
            MenuItem::action(tr("menu.view.tab_layers"), ShowTabLayerListAction)
                .checked(state.right_tab == RightPanelTab::LayerList),
            MenuItem::action(tr("menu.view.tab_project"), ShowTabProjectInfoAction)
                .checked(state.right_tab == RightPanelTab::ProjectInfo),
        ],
        disabled: false,
    }
}

fn build_playback_menu(state: MenuCheckState) -> Menu {
    Menu {
        name: tr("menu.playback").into(),
        items: vec![
            MenuItem::action(tr("menu.playback.toggle"), TogglePlayPause),
            MenuItem::action(tr("menu.playback.stop"), StopPlaybackAction),
            MenuItem::action(
                tr("menu.playback.return_start"),
                ToggleReturnToStartOnStopAction,
            )
            .checked(state.return_to_start_on_stop),
            MenuItem::separator(),
            MenuItem::action(tr("menu.playback.step_back"), StepBackOneFrame),
            MenuItem::action(tr("menu.playback.step_fwd"), StepForwardOneFrame),
            MenuItem::action(tr("menu.playback.half_back"), StepBackHalfSecAction),
            MenuItem::action(tr("menu.playback.half_fwd"), StepForwardHalfSecAction),
            MenuItem::separator(),
            MenuItem::action(tr("menu.playback.prev_boundary"), PrevBoundaryAction),
            MenuItem::action(tr("menu.playback.next_boundary"), NextBoundaryAction),
            MenuItem::action(tr("menu.playback.seek_start"), SeekStart),
            MenuItem::action(tr("menu.playback.seek_end"), SeekEnd),
        ],
        disabled: false,
    }
}

fn build_export_menu() -> Menu {
    Menu {
        name: tr("menu.export").into(),
        items: vec![
            MenuItem::action(tr("menu.export.open"), OpenExportDialog),
            MenuItem::action(tr("menu.export.selection"), ExportSelectionDialogAction),
        ],
        disabled: false,
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::KeyBinding;
    use gpui_kit::component::input::Backspace;

    use super::{DeleteClip, is_editor_main_binding};

    #[test]
    fn key_config_preserve_filter_keeps_input_and_replaces_editor_main() {
        let input_bs = KeyBinding::new("backspace", Backspace, Some("Input"));
        let editor_bs = KeyBinding::new("backspace", DeleteClip, Some("EditorMain"));
        let global_action = KeyBinding::new("backspace", DeleteClip, None);

        assert!(!is_editor_main_binding(&input_bs));
        assert!(is_editor_main_binding(&editor_bs));
        assert!(!is_editor_main_binding(&global_action));
    }
}
