//! エディタ上部のメニューバー構築・7トップメニュー・キーコンフィグ連動 (`EditorMain`) バインド。

use std::rc::Rc;

use gpui_kit::base::GlobalState;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::{
    Action, App, Entity, KeyBinding, KeyBindingContextPredicate, Menu, MenuItem, actions,
};

use crate::ui::app_controller::{PreviewZoomMode, RightPanelTab};
use crate::ui::key_config::{KeyConfig, ShortcutAction};

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
        build_playback_menu(),
        build_export_menu(),
    ]
}

fn build_file_menu() -> Menu {
    Menu {
        name: "ファイル".into(),
        items: vec![
            MenuItem::action("新規プロジェクト", NewProject),
            MenuItem::action("開く...", OpenProject),
            MenuItem::action("上書き保存", SaveProject),
            MenuItem::action("名前を付けて保存...", SaveProjectAs),
            MenuItem::separator(),
            MenuItem::action("素材を追加...", ImportMedia),
            MenuItem::separator(),
            MenuItem::action("終了", QuitApp),
        ],
        disabled: false,
    }
}

fn build_edit_menu() -> Menu {
    Menu {
        name: "編集".into(),
        items: vec![
            MenuItem::action("元に戻す", UndoAction),
            MenuItem::action("やり直し", RedoAction),
            MenuItem::separator(),
            MenuItem::action("切り取り", CutClip),
            MenuItem::action("コピー", CopyClip),
            MenuItem::action("貼り付け", PasteClip),
            MenuItem::action("複製", DuplicateClip),
            MenuItem::action("削除", DeleteClip),
            MenuItem::separator(),
            MenuItem::action("すべて選択", SelectAllClips),
            MenuItem::action("選択範囲を解除", ClearSelectionRange),
        ],
        disabled: false,
    }
}

fn build_timeline_menu() -> Menu {
    Menu {
        name: "タイムライン".into(),
        items: vec![
            MenuItem::action("再生位置で分割", SplitClip),
            MenuItem::action("中間点を追加", AddMidpointAction),
            MenuItem::action("付近の中間点を削除", RemoveMidpointAction),
            MenuItem::separator(),
            MenuItem::action("再生位置へ字幕を追加", AddSubtitleAction),
            MenuItem::separator(),
            MenuItem::action("選択範囲の開始を設定", SetSelectionStartAction),
            MenuItem::action("選択範囲の終了を設定", SetSelectionEndAction),
            MenuItem::action("重なったクリップを巡回選択", CycleClipAction),
        ],
        disabled: false,
    }
}

fn build_settings_menu(state: MenuCheckState) -> Menu {
    Menu {
        name: "設定".into(),
        items: vec![
            MenuItem::action("タイムライン長を変更...", SetTimelineLengthAction),
            MenuItem::action("最終クリップ末尾に自動調整", SnapTimelineLengthAutoAction),
            MenuItem::separator(),
            MenuItem::action(
                "シーク・移動時にクリップ設定を追従",
                ToggleFollowSelectedOnSeekAction,
            )
            .checked(state.follow_selected_on_seek),
            MenuItem::separator(),
            MenuItem::action("外部ツール (ffmpeg) 設定...", OpenExternalToolsAction),
            MenuItem::action("ショートカットキー設定...", OpenKeyConfigAction),
        ],
        disabled: false,
    }
}

fn build_view_menu(state: MenuCheckState) -> Menu {
    Menu {
        name: "表示".into(),
        items: vec![
            MenuItem::action("枠・ハンドルを表示", TogglePreviewGuidesAction)
                .checked(state.preview_overlays.guides),
            MenuItem::action("グリッド線を表示", TogglePreviewGridAction)
                .checked(state.preview_overlays.grid),
            MenuItem::action("マスター音声波形を表示", TogglePreviewWaveformAction)
                .checked(state.preview_overlays.waveform),
            MenuItem::separator(),
            MenuItem::action("プレビュー倍率: フィット", PreviewZoomFitAction)
                .checked(state.preview_zoom == PreviewZoomMode::Fit),
            MenuItem::action("プレビュー倍率: 50%", PreviewZoom50Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(50)),
            MenuItem::action("プレビュー倍率: 100%", PreviewZoom100Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(100)),
            MenuItem::action("プレビュー倍率: 200%", PreviewZoom200Action)
                .checked(state.preview_zoom == PreviewZoomMode::Percent(200)),
            MenuItem::separator(),
            MenuItem::action("タイムラインを拡大", TimelineZoomIn),
            MenuItem::action("タイムラインを縮小", TimelineZoomOut),
            MenuItem::separator(),
            MenuItem::action("右パネル: クリップ設定", ShowTabClipInspectorAction)
                .checked(state.right_tab == RightPanelTab::ClipInspector),
            MenuItem::action("右パネル: AIチャット", ShowTabAiChatAction)
                .checked(state.right_tab == RightPanelTab::AiChat),
            MenuItem::action("右パネル: レイヤー", ShowTabLayerListAction)
                .checked(state.right_tab == RightPanelTab::LayerList),
            MenuItem::action("右パネル: プロジェクト情報", ShowTabProjectInfoAction)
                .checked(state.right_tab == RightPanelTab::ProjectInfo),
        ],
        disabled: false,
    }
}

fn build_playback_menu() -> Menu {
    Menu {
        name: "再生".into(),
        items: vec![
            MenuItem::action("再生 / 一時停止", TogglePlayPause),
            MenuItem::action("停止", StopPlaybackAction),
            MenuItem::separator(),
            MenuItem::action("1 フレーム戻る", StepBackOneFrame),
            MenuItem::action("1 フレーム進む", StepForwardOneFrame),
            MenuItem::action("0.5 秒戻る", StepBackHalfSecAction),
            MenuItem::action("0.5 秒進む", StepForwardHalfSecAction),
            MenuItem::separator(),
            MenuItem::action("前のクリップ境界・中間点", PrevBoundaryAction),
            MenuItem::action("次のクリップ境界・中間点", NextBoundaryAction),
            MenuItem::action("先頭フレームへ移動", SeekStart),
            MenuItem::action("末尾フレームへ移動", SeekEnd),
        ],
        disabled: false,
    }
}

fn build_export_menu() -> Menu {
    Menu {
        name: "書き出し".into(),
        items: vec![
            MenuItem::action("動画を書き出し...", OpenExportDialog),
            MenuItem::action("選択範囲のみを書き出し...", ExportSelectionDialogAction),
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
