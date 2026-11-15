//! Windows/Linux 向けカスタム GPUI メニューバーとエディタアクション定義。

use gpui_kit::component::GlobalState;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::{App, Entity, KeyBinding, Menu, MenuItem, actions};

actions!(
    sea_editor,
    [
        /// 空の新規プロジェクトを作成するアクション。
        NewProject,
        /// 既存のプロジェクト JSON ファイルを開くアクション。
        OpenProject,
        /// 現在のプロジェクトを上書き保存するアクション。
        SaveProject,
        /// 現在のプロジェクトを別名で保存するアクション。
        SaveProjectAs,
        /// 動画・静止画・音声素材をタイムラインへ取り込むアクション。
        ImportMedia,
        /// アプリケーションを終了するアクション。
        QuitApp,
        /// 直前の編集操作を取り消すアクション。
        UndoAction,
        /// 取り消した編集操作をやり直すアクション。
        RedoAction,
        /// 選択中クリップを現在再生ヘッド位置で分割するアクション。
        SplitAtPlayhead,
        /// 選択中クリップを削除するアクション。
        DeleteSelectedClip,
        /// 再生・一時停止を切り替えるアクション。
        TogglePlayback,
        /// 再生ヘッドをタイムライン先頭へ戻すアクション。
        SeekStart,
    ]
);

/// キーバインドと標準メニュー構成を登録し、`AppMenuBar` エンティティを生成する。
pub fn setup_menus(cx: &mut App) -> Entity<AppMenuBar> {
    cx.bind_keys([
        KeyBinding::new("ctrl-n", NewProject, None),
        KeyBinding::new("ctrl-o", OpenProject, None),
        KeyBinding::new("ctrl-s", SaveProject, None),
        KeyBinding::new("ctrl-shift-s", SaveProjectAs, None),
        KeyBinding::new("ctrl-i", ImportMedia, None),
        KeyBinding::new("ctrl-z", UndoAction, None),
        KeyBinding::new("ctrl-y", RedoAction, None),
        KeyBinding::new("s", SplitAtPlayhead, None),
        KeyBinding::new("delete", DeleteSelectedClip, None),
        KeyBinding::new("space", TogglePlayback, None),
        KeyBinding::new("home", SeekStart, None),
    ]);

    let file_menu = Menu::new("ファイル").items([
        MenuItem::action("新規プロジェクト", NewProject),
        MenuItem::action("プロジェクトを開く...", OpenProject),
        MenuItem::action("メディアを読み込む...", ImportMedia),
        MenuItem::separator(),
        MenuItem::action("保存", SaveProject),
        MenuItem::action("名前を付けて保存...", SaveProjectAs),
        MenuItem::separator(),
        MenuItem::action("終了", QuitApp),
    ]);

    let edit_menu = Menu::new("編集").items([
        MenuItem::action("元に戻す (Undo)", UndoAction),
        MenuItem::action("やり直す (Redo)", RedoAction),
        MenuItem::separator(),
        MenuItem::action("再生ヘッドでクリップ分割", SplitAtPlayhead),
        MenuItem::action("選択クリップを削除", DeleteSelectedClip),
    ]);

    let playback_menu = Menu::new("再生").items([
        MenuItem::action("再生 / 一時停止", TogglePlayback),
        MenuItem::action("先頭へシーク", SeekStart),
    ]);

    let owned = [file_menu.owned(), edit_menu.owned(), playback_menu.owned()];
    GlobalState::global_mut(cx).set_app_menus(owned.to_vec());
    cx.set_menus(vec![
        Menu::new("ファイル").items([
            MenuItem::action("新規プロジェクト", NewProject),
            MenuItem::action("プロジェクトを開く...", OpenProject),
            MenuItem::action("メディアを読み込む...", ImportMedia),
            MenuItem::separator(),
            MenuItem::action("保存", SaveProject),
            MenuItem::action("名前を付けて保存...", SaveProjectAs),
            MenuItem::separator(),
            MenuItem::action("終了", QuitApp),
        ]),
        Menu::new("編集").items([
            MenuItem::action("元に戻す (Undo)", UndoAction),
            MenuItem::action("やり直す (Redo)", RedoAction),
            MenuItem::separator(),
            MenuItem::action("再生ヘッドでクリップ分割", SplitAtPlayhead),
            MenuItem::action("選択クリップを削除", DeleteSelectedClip),
        ]),
        Menu::new("再生").items([
            MenuItem::action("再生 / 一時停止", TogglePlayback),
            MenuItem::action("先頭へシーク", SeekStart),
        ]),
    ]);

    AppMenuBar::new(cx)
}
