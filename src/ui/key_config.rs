//! カスタマイズ可能なエディタショートカットキー設定・JSON 永続化・重複検出・モーダル UI。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, Keystroke, MouseButton,
    ParentElement as _, Styled as _, div, px, rgba,
};
use serde::{Deserialize, Serialize};

use crate::ui::i18n::tr;
use crate::ui::window::SeaEditorWindow;

/// キー設定ファイル名。
const KEY_CONFIG_FILE_NAME: &str = "key_config.json";

/// ユーザーが割り当て変更可能な全ショートカットアクション一覧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ShortcutAction {
    /// ファイル: 新規プロジェクト。
    NewProject,
    /// ファイル: プロジェクトを開く。
    OpenProject,
    /// ファイル: 上書き保存。
    SaveProject,
    /// ファイル: 名前を付けて保存。
    SaveProjectAs,
    /// ファイル: 素材を追加。
    ImportMedia,
    /// 編集: 元に戻す。
    Undo,
    /// 編集: やり直し。
    Redo,
    /// 編集: 切り取り。
    Cut,
    /// 編集: コピー。
    Copy,
    /// 編集: 貼り付け。
    Paste,
    /// 編集: 複製。
    Duplicate,
    /// 編集: 削除。
    Delete,
    /// 編集: すべてのクリップを選択。
    SelectAll,
    /// 編集: 選択範囲を解除。
    ClearSelection,
    /// タイムライン: 再生位置で分割。
    SplitClip,
    /// タイムライン: 現在位置に中間点を追加。
    AddMidpoint,
    /// タイムライン: 付近の中間点を削除。
    RemoveMidpoint,
    /// タイムライン: 再生位置へ字幕クリップ追加。
    AddSubtitle,
    /// タイムライン: 選択開始 `[` を現在フレームに設定。
    SetSelectionStart,
    /// タイムライン: 選択終了 `]` を現在フレームに設定。
    SetSelectionEnd,
    /// タイムライン: 重なりクリップまたはパネルを巡回選択 (`Tab`)。
    CycleClip,
    /// 再生: 再生 / 一時停止。
    TogglePlayPause,
    /// 再生: 停止して先頭または開始位置へ戻す。
    StopPlayback,
    /// 再生: 先頭フレームへ移動。
    SeekStart,
    /// 再生: 末尾フレームへ移動。
    SeekEnd,
    /// 再生: 1 フレーム戻る。
    StepBackFrame,
    /// 再生: 1 フレーム進む。
    StepForwardFrame,
    /// 再生: 0.5 秒戻る。
    StepBackHalfSec,
    /// 再生: 0.5 秒進む。
    StepForwardHalfSec,
    /// 再生: 前の境界または中間点へジャンプ。
    PrevBoundary,
    /// 再生: 次の境界または中間点へジャンプ。
    NextBoundary,
    /// キャンバス: X を -1px 微調整。
    NudgeLeft,
    /// キャンバス: X を +1px 微調整。
    NudgeRight,
    /// キャンバス: Y を -1px 微調整。
    NudgeUp,
    /// キャンバス: Y を +1px 微調整。
    NudgeDown,
    /// キャンバス: 反時計回り -1° 微調整。
    RotateLeft,
    /// キャンバス: 時計回り +1° 微調整。
    RotateRight,
    /// 表示: タイムライン拡大。
    TimelineZoomIn,
    /// 表示: タイムライン縮小。
    TimelineZoomOut,
    /// 表示: プレビュー枠・ハンドル表示切替。
    TogglePreviewGuides,
    /// 表示: プレビューグリッド表示切替。
    TogglePreviewGrid,
    /// 表示: プレビューマスター波形表示切替。
    TogglePreviewWaveform,
    /// 設定: タイムライン長変更ダイアログ。
    SetTimelineLength,
    /// 設定: キーコンフィグ設定ダイアログ。
    OpenKeyConfig,
    /// 書き出し: 書き出しダイアログを開く。
    OpenExportDialog,
}

impl ShortcutAction {
    /// 全定義済みアクションを宣言順で返す。
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::NewProject,
            Self::OpenProject,
            Self::SaveProject,
            Self::SaveProjectAs,
            Self::ImportMedia,
            Self::Undo,
            Self::Redo,
            Self::Cut,
            Self::Copy,
            Self::Paste,
            Self::Duplicate,
            Self::Delete,
            Self::SelectAll,
            Self::ClearSelection,
            Self::SplitClip,
            Self::AddMidpoint,
            Self::RemoveMidpoint,
            Self::AddSubtitle,
            Self::SetSelectionStart,
            Self::SetSelectionEnd,
            Self::CycleClip,
            Self::TogglePlayPause,
            Self::StopPlayback,
            Self::SeekStart,
            Self::SeekEnd,
            Self::StepBackFrame,
            Self::StepForwardFrame,
            Self::StepBackHalfSec,
            Self::StepForwardHalfSec,
            Self::PrevBoundary,
            Self::NextBoundary,
            Self::NudgeLeft,
            Self::NudgeRight,
            Self::NudgeUp,
            Self::NudgeDown,
            Self::RotateLeft,
            Self::RotateRight,
            Self::TimelineZoomIn,
            Self::TimelineZoomOut,
            Self::TogglePreviewGuides,
            Self::TogglePreviewGrid,
            Self::TogglePreviewWaveform,
            Self::SetTimelineLength,
            Self::OpenKeyConfig,
            Self::OpenExportDialog,
        ]
    }

    /// 設定ファイル保存用の不変キー ID を返す。
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewProject => "file.new",
            Self::OpenProject => "file.open",
            Self::SaveProject => "file.save",
            Self::SaveProjectAs => "file.save_as",
            Self::ImportMedia => "file.import",
            Self::Undo => "edit.undo",
            Self::Redo => "edit.redo",
            Self::Cut => "edit.cut",
            Self::Copy => "edit.copy",
            Self::Paste => "edit.paste",
            Self::Duplicate => "edit.duplicate",
            Self::Delete => "edit.delete",
            Self::SelectAll => "edit.select_all",
            Self::ClearSelection => "edit.clear_selection",
            Self::SplitClip => "timeline.split",
            Self::AddMidpoint => "timeline.add_midpoint",
            Self::RemoveMidpoint => "timeline.remove_midpoint",
            Self::AddSubtitle => "timeline.add_subtitle",
            Self::SetSelectionStart => "timeline.selection_start",
            Self::SetSelectionEnd => "timeline.selection_end",
            Self::CycleClip => "timeline.cycle_clip",
            Self::TogglePlayPause => "playback.toggle",
            Self::StopPlayback => "playback.stop",
            Self::SeekStart => "playback.seek_start",
            Self::SeekEnd => "playback.seek_end",
            Self::StepBackFrame => "playback.step_back",
            Self::StepForwardFrame => "playback.step_forward",
            Self::StepBackHalfSec => "playback.step_back_half",
            Self::StepForwardHalfSec => "playback.step_forward_half",
            Self::PrevBoundary => "playback.prev_boundary",
            Self::NextBoundary => "playback.next_boundary",
            Self::NudgeLeft => "canvas.nudge_left",
            Self::NudgeRight => "canvas.nudge_right",
            Self::NudgeUp => "canvas.nudge_up",
            Self::NudgeDown => "canvas.nudge_down",
            Self::RotateLeft => "canvas.rotate_ccw",
            Self::RotateRight => "canvas.rotate_cw",
            Self::TimelineZoomIn => "view.timeline_zoom_in",
            Self::TimelineZoomOut => "view.timeline_zoom_out",
            Self::TogglePreviewGuides => "view.toggle_guides",
            Self::TogglePreviewGrid => "view.toggle_grid",
            Self::TogglePreviewWaveform => "view.toggle_waveform",
            Self::SetTimelineLength => "settings.timeline_length",
            Self::OpenKeyConfig => "settings.key_config",
            Self::OpenExportDialog => "export.open",
        }
    }

    /// UI 表示用の日本語アクション名を返す。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NewProject => "ファイル / 新規プロジェクト",
            Self::OpenProject => "ファイル / 開く...",
            Self::SaveProject => "ファイル / 上書き保存",
            Self::SaveProjectAs => "ファイル / 名前を付けて保存...",
            Self::ImportMedia => "ファイル / 素材を追加...",
            Self::Undo => "編集 / 元に戻す",
            Self::Redo => "編集 / やり直し",
            Self::Cut => "編集 / 切り取り",
            Self::Copy => "編集 / コピー",
            Self::Paste => "編集 / 貼り付け",
            Self::Duplicate => "編集 / 複製",
            Self::Delete => "編集 / 削除",
            Self::SelectAll => "編集 / すべてのクリップを選択",
            Self::ClearSelection => "編集 / 選択範囲を解除",
            Self::SplitClip => "タイムライン / 再生位置で分割",
            Self::AddMidpoint => "タイムライン / 中間点を追加",
            Self::RemoveMidpoint => "タイムライン / 付近の中間点を削除",
            Self::AddSubtitle => "タイムライン / 字幕クリップ追加",
            Self::SetSelectionStart => "タイムライン / 選択範囲の開始設定",
            Self::SetSelectionEnd => "タイムライン / 選択範囲の終了設定",
            Self::CycleClip => "タイムライン / 重なりクリップ巡回",
            Self::TogglePlayPause => "再生 / 再生・一時停止",
            Self::StopPlayback => "再生 / 停止",
            Self::SeekStart => "再生 / 先頭フレームへ移動",
            Self::SeekEnd => "再生 / 末尾フレームへ移動",
            Self::StepBackFrame => "再生 / 1フレーム戻る",
            Self::StepForwardFrame => "再生 / 1フレーム進む",
            Self::StepBackHalfSec => "再生 / 0.5秒戻る",
            Self::StepForwardHalfSec => "再生 / 0.5秒進む",
            Self::PrevBoundary => "再生 / 前の境界・中間点",
            Self::NextBoundary => "再生 / 次の境界・中間点",
            Self::NudgeLeft => "キャンバス / 左へ 1px 移動",
            Self::NudgeRight => "キャンバス / 右へ 1px 移動",
            Self::NudgeUp => "キャンバス / 上へ 1px 移動",
            Self::NudgeDown => "キャンバス / 下へ 1px 移動",
            Self::RotateLeft => "キャンバス / 反時計回り -1°",
            Self::RotateRight => "キャンバス / 時計回り +1°",
            Self::TimelineZoomIn => "表示 / タイムライン拡大",
            Self::TimelineZoomOut => "表示 / タイムライン縮小",
            Self::TogglePreviewGuides => "表示 / プレビュー枠・ハンドル",
            Self::TogglePreviewGrid => "表示 / プレビューグリッド",
            Self::TogglePreviewWaveform => "表示 / マスター音声波形",
            Self::SetTimelineLength => "設定 / タイムライン長変更...",
            Self::OpenKeyConfig => "設定 / ショートカット設定...",
            Self::OpenExportDialog => "書き出し / 動画を書き出し...",
        }
    }

    /// `AviUtl` 準拠の標準デフォルトキーストロークを返す。
    #[must_use]
    pub const fn default_keystroke(self) -> &'static str {
        match self {
            Self::NewProject => "ctrl-n",
            Self::OpenProject => "ctrl-o",
            Self::SaveProject => "ctrl-s",
            Self::SaveProjectAs => "ctrl-shift-s",
            Self::ImportMedia => "ctrl-i",
            Self::Undo => "ctrl-z",
            Self::Redo => "ctrl-y",
            Self::Cut => "ctrl-x",
            Self::Copy => "ctrl-c",
            Self::Paste => "ctrl-v",
            Self::Duplicate => "ctrl-d",
            Self::Delete => "delete",
            Self::SelectAll => "ctrl-a",
            Self::ClearSelection => "ctrl-shift-a",
            Self::SplitClip => "s",
            Self::AddMidpoint => "m",
            Self::RemoveMidpoint => "shift-m",
            Self::AddSubtitle => "t",
            Self::SetSelectionStart => "[",
            Self::SetSelectionEnd => "]",
            Self::CycleClip => "tab",
            Self::TogglePlayPause => "space",
            Self::StopPlayback => "ctrl-space",
            Self::SeekStart => "home",
            Self::SeekEnd => "end",
            Self::StepBackFrame => "left",
            Self::StepForwardFrame => "right",
            Self::StepBackHalfSec => "shift-left",
            Self::StepForwardHalfSec => "shift-right",
            Self::PrevBoundary => "up",
            Self::NextBoundary => "down",
            Self::NudgeLeft => ",",
            Self::NudgeRight => ".",
            Self::NudgeUp => "^",
            Self::NudgeDown => "\\",
            Self::RotateLeft => "/",
            Self::RotateRight => "*",
            Self::TimelineZoomIn => "ctrl-=",
            Self::TimelineZoomOut => "ctrl--",
            Self::TogglePreviewGuides => "g",
            Self::TogglePreviewGrid => "shift-g",
            Self::TogglePreviewWaveform => "w",
            Self::SetTimelineLength => "ctrl-l",
            Self::OpenKeyConfig => "ctrl-k",
            Self::OpenExportDialog => "ctrl-e",
        }
    }
}

/// ユーザーがカスタマイズ可能なショートカットキー割り当て表。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyConfig {
    /// アクション識別子 -> GPUI キーストローク文字列（例: `"space"`, `"ctrl-z"`）。
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

impl KeyConfig {
    /// 全アクションの標準バインドを格納した [`KeyConfig`] を返す。
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut bindings = BTreeMap::new();
        for &action in ShortcutAction::all() {
            bindings.insert(
                action.id().to_owned(),
                action.default_keystroke().to_owned(),
            );
        }
        Self { bindings }
    }

    /// 既存マップに不足している新規アクションのデフォルト補完を行う。
    pub fn fill_missing_defaults(&mut self) {
        for &action in ShortcutAction::all() {
            self.bindings
                .entry(action.id().to_owned())
                .or_insert_with(|| action.default_keystroke().to_owned());
        }
    }

    /// 指定アクションの現在割り当て済みキーストローク文字列を返す。
    #[must_use]
    pub fn binding_for(&self, action: ShortcutAction) -> &str {
        self.bindings
            .get(action.id())
            .map_or_else(|| action.default_keystroke(), String::as_str)
    }

    /// キーストロークを差し替え、同じキーへ重複割り当てされている他アクション一覧を返す。
    pub fn set_binding(
        &mut self,
        action: ShortcutAction,
        raw_keystroke: &str,
    ) -> Vec<ShortcutAction> {
        let normalized = normalize_keystroke_str(raw_keystroke);
        if normalized.is_empty() {
            return Vec::new();
        }
        self.bindings
            .insert(action.id().to_owned(), normalized.clone());
        self.find_conflicts_for_key(action, &normalized)
    }

    /// 指定キーストロークが他のどのアクションと重複しているかを返す。
    #[must_use]
    pub fn find_conflicts_for_key(
        &self,
        target: ShortcutAction,
        raw_keystroke: &str,
    ) -> Vec<ShortcutAction> {
        let norm = normalize_keystroke_str(raw_keystroke);
        if norm.is_empty() {
            return Vec::new();
        }
        ShortcutAction::all()
            .iter()
            .copied()
            .filter(|&other| {
                other != target && normalize_keystroke_str(self.binding_for(other)) == norm
            })
            .collect()
    }

    /// 現在テーブル内の全重複ペア `(keystroke, actions)` を返す。
    #[must_use]
    pub fn all_conflicts(&self) -> Vec<(String, Vec<ShortcutAction>)> {
        let mut by_key: HashMap<String, Vec<ShortcutAction>> = HashMap::new();
        for &action in ShortcutAction::all() {
            let norm = normalize_keystroke_str(self.binding_for(action));
            if !norm.is_empty() {
                by_key.entry(norm).or_default().push(action);
            }
        }
        let mut dupes: Vec<(String, Vec<ShortcutAction>)> = by_key
            .into_iter()
            .filter(|(_, list)| list.len() > 1)
            .collect();
        dupes.sort_by(|a, b| a.0.cmp(&b.0));
        dupes
    }

    /// 全アクションを初期値へ戻す。
    pub fn reset_defaults(&mut self) {
        *self = Self::with_defaults();
    }

    /// 既定の設定保存パス (`%APPDATA%\sea-movie-creator\key_config.json` 等) を返す。
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        crate::core::user_config_file_path(KEY_CONFIG_FILE_NAME)
    }

    /// 指定 JSON ファイルから設定を読み込み、未定義アクションは既定値で補完する。
    ///
    /// # Errors
    ///
    /// ファイル読み込みまたは JSON デシリアライズに失敗した場合。
    pub fn load_from_path(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("キー設定読込失敗: {e}"))?;
        let mut cfg: Self =
            serde_json::from_str(&text).map_err(|e| format!("キー設定JSON解析失敗: {e}"))?;
        cfg.fill_missing_defaults();
        Ok(cfg)
    }

    /// 既定パスが存在すれば読み込み、未作成・壊れている場合は初期値を返す。
    #[must_use]
    pub fn load_or_default() -> Self {
        Self::default_path()
            .and_then(|p| Self::load_from_path(&p).ok())
            .unwrap_or_else(Self::with_defaults)
    }

    /// 指定ファイルパスへ JSON 形式で保存する。
    ///
    /// # Errors
    ///
    /// 親ディレクトリ作成、JSON 生成、またはファイル書き込みに失敗した場合。
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("キー設定フォルダ作成失敗: {e}"))?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("キー設定JSON生成失敗: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("キー設定保存失敗: {e}"))
    }
}

/// 修飾キー順 (`ctrl-alt-shift-key`) を正規化したキーストローク文字列へ整形する。
#[must_use]
pub fn normalize_keystroke_str(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed == "-" {
        return "-".to_owned();
    }
    if trimmed == "+" {
        return "+".to_owned();
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut platform = false;
    let mut base_key = String::new();

    let mut parts = trimmed.split(['-', '+']).peekable();
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case("ctrl") || part.eq_ignore_ascii_case("control") {
            ctrl = true;
        } else if part.eq_ignore_ascii_case("alt") {
            alt = true;
        } else if part.eq_ignore_ascii_case("shift") {
            shift = true;
        } else if part.eq_ignore_ascii_case("cmd")
            || part.eq_ignore_ascii_case("super")
            || part.eq_ignore_ascii_case("win")
        {
            platform = true;
        } else if parts.peek().is_some_and(|next| next.is_empty())
            && (trimmed.ends_with('-') || trimmed.ends_with('+'))
        {
            base_key = if trimmed.ends_with('-') {
                "-".to_owned()
            } else {
                "+".to_owned()
            };
            break;
        } else if !part.is_empty() {
            base_key = part.to_ascii_lowercase();
        }
    }
    if base_key.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    if ctrl {
        out.push_str("ctrl-");
    }
    if alt {
        out.push_str("alt-");
    }
    if shift {
        out.push_str("shift-");
    }
    if platform {
        out.push_str("cmd-");
    }
    out.push_str(&base_key);
    out
}

/// GPUI の `Keystroke` イベントからバインド用の正規化文字列を生成する（修飾キー単体は `None`）。
#[must_use]
pub fn keystroke_from_gpui_event(ks: &Keystroke) -> Option<String> {
    let key = ks.key.trim().to_ascii_lowercase();
    if matches!(
        key.as_str(),
        "" | "shift" | "control" | "ctrl" | "alt" | "platform" | "super" | "meta" | "function"
    ) {
        return None;
    }
    let mut out = String::new();
    if ks.modifiers.control {
        out.push_str("ctrl-");
    }
    if ks.modifiers.alt {
        out.push_str("alt-");
    }
    if ks.modifiers.shift {
        out.push_str("shift-");
    }
    if ks.modifiers.platform {
        out.push_str("cmd-");
    }
    out.push_str(&key);
    Some(normalize_keystroke_str(&out))
}

/// ショートカット設定モーダルの一時編集状態。
#[derive(Debug, Clone)]
pub struct KeyConfigModalState {
    /// 編集中ドラフトコピー。
    pub draft: KeyConfig,
    /// 現在キー入力待ち中の対象アクション。
    pub capturing: Option<ShortcutAction>,
    /// 重複または検証メッセージ。
    pub warning: Option<String>,
}

impl KeyConfigModalState {
    /// 現在の設定から新しいモーダル状態を生成する。
    #[must_use]
    pub fn open(current: &KeyConfig) -> Self {
        let draft = current.clone();
        let warning = format_conflict_summary(&draft);
        Self {
            draft,
            capturing: None,
            warning,
        }
    }

    /// キャプチャ中のアクションへキーストロークを割り当て、競合警告を更新する。
    pub fn capture_keystroke(&mut self, stroke: &str) {
        let Some(action) = self.capturing.take() else {
            return;
        };
        let conflicts = self.draft.set_binding(action, stroke);
        if conflicts.is_empty() {
            self.warning = format_conflict_summary(&self.draft);
        } else {
            let names: Vec<&str> = conflicts.iter().map(|a| a.label()).collect();
            self.warning = Some(format!(
                "キー `{}` は既に [{}] と重複しています",
                stroke,
                names.join(", ")
            ));
        }
    }
}

fn format_conflict_summary(cfg: &KeyConfig) -> Option<String> {
    let dupes = cfg.all_conflicts();
    if dupes.is_empty() {
        return None;
    }
    let first = &dupes[0];
    let labels: Vec<&str> = first.1.iter().map(|a| a.label()).collect();
    Some(format!("重複警告 ({}): {}", first.0, labels.join(" / ")))
}

/// キーコンフィグ設定モーダルオーバーレイを描画する。
pub fn render_key_config_modal_overlay(
    modal: &KeyConfigModalState,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let rows: Vec<AnyElement> = ShortcutAction::all()
        .iter()
        .copied()
        .enumerate()
        .map(|(ix, action)| render_key_config_row(modal, ix, action, cx))
        .collect();

    let header = render_key_config_header(cx);
    let footer = render_key_config_footer(cx);
    let warn_banner = modal.warning.as_ref().map(|w| {
        div()
            .w_full()
            .px_2p5()
            .py_1p5()
            .rounded(px(4.0))
            .bg(rgba(0x7835_0fe6))
            .border_1()
            .border_color(rgba(0xf59e_0bff))
            .text_size(px(11.0))
            .text_color(rgba(0xfef3_c7ff))
            .child(w.clone())
    });

    div()
        .id("key-config-modal-backdrop")
        .absolute()
        .inset_0()
        .bg(rgba(0x0206_17aa))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, _| {}))
        .child(
            v_flex()
                .id("key-config-modal-card")
                .w(px(580.0))
                .h(px(520.0))
                .p_4()
                .gap_2()
                .rounded(px(8.0))
                .bg(rgba(0x1e29_3bff))
                .border_1()
                .border_color(rgba(0x4755_69ff))
                .child(header)
                .children(warn_banner)
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scrollbar()
                        .child(v_flex().gap_1().children(rows)),
                )
                .child(footer),
        )
        .into_any_element()
}

fn render_key_config_header(cx: &mut Context<SeaEditorWindow>) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(14.0))
                .font_bold()
                .text_color(rgba(0xf8fa_fcff))
                .child(tr("keycfg.title")),
        )
        .child(
            Button::new("key-cfg-reset")
                .xsmall()
                .ghost()
                .label("初期設定に戻す")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(m) = &mut this.key_config_modal {
                        m.draft.reset_defaults();
                        m.capturing = None;
                        m.warning = format_conflict_summary(&m.draft);
                    }
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_key_config_footer(cx: &mut Context<SeaEditorWindow>) -> AnyElement {
    h_flex()
        .w_full()
        .justify_end()
        .gap_2()
        .pt_2()
        .border_t_1()
        .border_color(rgba(0x3341_55ff))
        .child(
            Button::new("key-cfg-cancel")
                .small()
                .ghost()
                .label(tr("common.cancel"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.key_config_modal = None;
                    cx.notify();
                })),
        )
        .child(
            Button::new("key-cfg-apply")
                .small()
                .primary()
                .label(tr("keycfg.save"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.apply_and_save_key_config_modal(cx);
                    cx.notify();
                })),
        )
        .into_any_element()
}

fn render_key_config_row(
    modal: &KeyConfigModalState,
    ix: usize,
    action: ShortcutAction,
    cx: &mut Context<SeaEditorWindow>,
) -> AnyElement {
    let key_str = modal.draft.binding_for(action).to_owned();
    let has_conflict = !modal
        .draft
        .find_conflicts_for_key(action, &key_str)
        .is_empty();
    let is_capturing = modal.capturing == Some(action);

    let badge_text = if is_capturing {
        "入力待ち... (Escで中止)".to_owned()
    } else {
        key_str
    };

    let btn_label = if is_capturing { "待機中" } else { "割当" };

    h_flex()
        .w_full()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .bg(rgba(0x0f17_2a99))
        .border_1()
        .border_color(if is_capturing {
            rgba(0x38bd_f8ff)
        } else if has_conflict {
            rgba(0xf59e_0bff)
        } else {
            rgba(0x3341_5566)
        })
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgba(0xe2e8_f0ff))
                .child(action.label()),
        )
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded(px(3.0))
                        .bg(if has_conflict {
                            rgba(0x7835_0fff)
                        } else {
                            rgba(0x1e29_3bff)
                        })
                        .text_size(px(10.0))
                        .font_family("monospace")
                        .text_color(rgba(0xf8fa_fcff))
                        .child(badge_text),
                )
                .child(
                    Button::new(("key-cfg-bind", ix))
                        .xsmall()
                        .label(btn_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(m) = &mut this.key_config_modal {
                                m.capturing = if m.capturing == Some(action) {
                                    None
                                } else {
                                    Some(action)
                                };
                            }
                            cx.notify();
                        })),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_default_keystrokes_parse_in_gpui_without_conflicts() {
        let cfg = KeyConfig::with_defaults();
        assert!(cfg.all_conflicts().is_empty(), "defaults must be unique");
        for &action in ShortcutAction::all() {
            let stroke = action.default_keystroke();
            assert_eq!(cfg.binding_for(action), stroke);
            let parsed = Keystroke::parse(stroke);
            assert!(
                parsed.is_ok(),
                "action {action:?} default `{stroke}` failed Keystroke::parse"
            );
        }
    }

    #[test]
    fn key_config_conflict_detection_and_json_roundtrip() {
        let mut cfg = KeyConfig::with_defaults();
        let conflicts = cfg.set_binding(ShortcutAction::SplitClip, "Space");
        assert_eq!(conflicts, vec![ShortcutAction::TogglePlayPause]);
        assert_eq!(cfg.all_conflicts().len(), 1);

        let tmp = std::env::temp_dir().join("sea_key_config_test.json");
        assert!(cfg.save_to_path(&tmp).is_ok());
        let loaded = KeyConfig::load_from_path(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(loaded.is_ok());
        if let Ok(read_cfg) = loaded {
            assert_eq!(read_cfg.binding_for(ShortcutAction::SplitClip), "space");
        }

        let mut plus_cfg = KeyConfig::with_defaults();
        let dup = plus_cfg.set_binding(ShortcutAction::SplitClip, "Ctrl+Space");
        assert_eq!(dup, vec![ShortcutAction::StopPlayback]);
        assert_eq!(
            plus_cfg.binding_for(ShortcutAction::SplitClip),
            "ctrl-space"
        );
        assert_eq!(normalize_keystroke_str("Ctrl++"), "ctrl-+");
        assert_eq!(normalize_keystroke_str("Ctrl+Shift+-"), "ctrl-shift--");
    }
}
