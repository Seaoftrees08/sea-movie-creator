//! GUI 非依存のアプリ状態・メディア読込・再生状態機械・コマンド統括コントローラ。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::audio::mixer::AudioMixer;
use crate::audio::playback::{
    AudioOutputHandle, PlaybackClock, SharedPlaybackState, resample_interleaved_pcm,
    start_audio_output,
};
use crate::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use crate::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, RationalDuration, frames_to_project_seconds,
};
use crate::media::compositor::WgpuCompositor;
use crate::media::decoder::{
    BackendPreference, VideoDecoder, open_audio_decoder, open_video_decoder, probe_media,
};
use crate::media::frame_cache::{FrameCache, FrameCacheKey};
use crate::timeline::command::{
    EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget, ParameterValue,
};
use crate::timeline::dispatcher::CommandDispatcher;
use crate::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind, Layer,
    ProjectDocument,
};
use crate::timeline::serialization::{
    build_asset_path, load_project_file, resolve_document_assets, resolve_existing_asset_path,
    save_project_file,
};
use crate::ui::surface_bridge::{
    PreviewSurfaceBridge, RenderImageSurfaceBridge, composite_with_gpu_or_fallback,
};

const DEFAULT_FRAME_CACHE_BYTES: usize = 64 * 1024 * 1024;
const PREVIEW_CANVAS_MAX_WIDTH: u32 = 480;
const OUTPUT_SAMPLE_RATE_HZ: u32 = 48_000;
/// 右インスペクタパネルの最小許容幅 (px)。
pub const MIN_RIGHT_PANEL_WIDTH: f32 = 240.0;
/// タイムライン拡大率の最小値 (px / frame)。
pub const MIN_TIMELINE_PX_PER_FRAME: f32 = 0.2;
/// タイムライン拡大率の最大値 (px / frame)。
pub const MAX_TIMELINE_PX_PER_FRAME: f32 = 32.0;
const DEFAULT_RIGHT_PANEL_RATIO: f32 = 0.2;
const MAX_RIGHT_PANEL_RATIO: f32 = 0.5;
const MIN_TIMELINE_HEIGHT: f32 = 140.0;
const DEFAULT_TIMELINE_RATIO: f32 = 0.3;
const MAX_TIMELINE_RATIO: f32 = 0.6;

/// ヘッダー・時刻バッジの時間表記単位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeDisplayUnit {
    /// タイムコード表記 (`HH:MM:SS.cc`)。
    #[default]
    Timecode,
    /// 総フレーム数表記 (`0f`)。
    Frames,
}

/// タイムラインレイヤー行の表示密度プリセット。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineRowDensity {
    /// 高密度表示 (22px)。
    Compact,
    /// 標準表示 (30px)。
    #[default]
    Medium,
    /// 波形重視の広め表示 (44px)。
    Large,
}

impl TimelineRowDensity {
    /// 行密度に応じたピクセル高さを返す。
    #[must_use]
    pub const fn row_height_px(self) -> f32 {
        match self {
            Self::Compact => 22.0,
            Self::Medium => 30.0,
            Self::Large => 44.0,
        }
    }

    /// 次の密度プリセットへ順送りする (`Compact -> Medium -> Large -> Compact`)。
    #[must_use]
    pub const fn cycle(self) -> Self {
        match self {
            Self::Compact => Self::Medium,
            Self::Medium => Self::Large,
            Self::Large => Self::Compact,
        }
    }

    /// UI ボタン表示用の短いラベルを返す。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "小",
            Self::Medium => "中",
            Self::Large => "大",
        }
    }
}

/// プレビューキャンバス表示倍率モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewZoomMode {
    /// ペイン全体へ自動フィット表示する。
    #[default]
    Fit,
    /// 固定パーセント倍率（25, 50, 100, 200 など）。
    Percent(u16),
}

impl PreviewZoomMode {
    /// 固定倍率の場合はスカラー係数 (`1.0 == 100%`) を、`Fit` の場合は `None` を返す。
    #[must_use]
    pub fn zoom_factor(self) -> Option<f32> {
        match self {
            Self::Fit => None,
            Self::Percent(pct) => Some((f32::from(pct.clamp(10, 800))) / 100.0),
        }
    }
}

fn default_inspector_sections() -> std::collections::HashMap<String, bool> {
    let mut map = std::collections::HashMap::new();
    map.insert("standard".to_owned(), true);
    map.insert("color".to_owned(), false);
    map.insert("subtitle".to_owned(), true);
    map.insert("audio".to_owned(), true);
    map.insert("plugins".to_owned(), false);
    map
}

fn find_unlocked_free_layer(
    doc: &ProjectDocument,
    preferred: Option<&LayerId>,
    start_frame: u64,
    end_frame: u64,
) -> Option<LayerId> {
    let is_free = |layer: &crate::timeline::document::Layer| -> bool {
        !layer.locked
            && layer.clips.iter().all(|c| {
                let c_start = c.timeline_start.get();
                let c_end = c_start.saturating_add(c.duration.get());
                c_end <= start_frame || end_frame <= c_start
            })
    };
    if let Some(pref_id) = preferred
        && let Some(layer) = doc.find_layer(pref_id)
        && is_free(layer)
    {
        return Some(layer.id.clone());
    }
    doc.layers
        .iter()
        .find(|layer| is_free(layer))
        .map(|layer| layer.id.clone())
}

/// タイムライン再生の状態機械。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// 先頭または停止状態。
    #[default]
    Stopped,
    /// 連続再生中。
    Playing,
    /// 現在位置で一時停止中。
    Paused,
    /// シーク操作によるフレーム移動直後。
    Seeking,
}

/// 右パネルの表示タブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightPanelTab {
    /// 選択クリップのパラメータ編集。
    #[default]
    ClipInspector,
    /// Phase B AI エージェント向けチャット入力枠（初期骨格）。
    AiChat,
    /// レイヤー一覧と可視・ミュート切替。
    LayerList,
    /// プロジェクト設定・再生バックエンド情報。
    ProjectInfo,
}

/// ウィンドウ幅から右パネルの有効幅（初期値 1/5・クランプ済み）を計算する。
#[must_use]
pub fn clamp_right_panel_width(window_width: f32, requested: Option<f32>) -> f32 {
    let safe_win = window_width.max(MIN_RIGHT_PANEL_WIDTH * 2.0);
    let max_width = (safe_win * MAX_RIGHT_PANEL_RATIO).max(MIN_RIGHT_PANEL_WIDTH);
    let initial = safe_win * DEFAULT_RIGHT_PANEL_RATIO;
    requested
        .unwrap_or(initial)
        .clamp(MIN_RIGHT_PANEL_WIDTH, max_width)
}

/// ウィンドウ高さから下部タイムラインの有効高さを計算する。
#[must_use]
pub fn clamp_timeline_height(window_height: f32, requested: Option<f32>) -> f32 {
    let safe_h = window_height.max(MIN_TIMELINE_HEIGHT * 2.0);
    let max_h = (safe_h * MAX_TIMELINE_RATIO).max(MIN_TIMELINE_HEIGHT);
    let initial = safe_h * DEFAULT_TIMELINE_RATIO;
    requested
        .unwrap_or(initial)
        .clamp(MIN_TIMELINE_HEIGHT, max_h)
}

/// プロジェクト編集・デコード・再生状態を一元保持するヘッドレスコントローラ。
#[allow(clippy::struct_excessive_bools)]
pub struct AppController {
    /// 現在のプロジェクトドキュメント。
    pub document: ProjectDocument,
    /// コマンド適用および Undo/Redo 履歴管理。
    pub dispatcher: CommandDispatcher,
    /// 保存・読込済みプロジェクトファイルパス。
    pub project_path: Option<PathBuf>,
    /// デコード済み映像・画像フレームの LRU キャッシュ。
    pub frame_cache: FrameCache,
    /// タイムライン音声ミキサ。
    pub mixer: AudioMixer,
    /// 再生クロック（音声デバイスまたはシステム単調時計）。
    pub clock: Arc<PlaybackClock>,
    /// 現在の再生状態。
    pub playback_state: PlaybackState,
    /// 現在の選択レイヤー ID。
    pub selected_layer_id: Option<LayerId>,
    /// 現在の選択クリップ ID（複数選択時は先頭要素と同期）。
    pub selected_clip_id: Option<ClipId>,
    /// 現在選択中のクリップ ID 一覧（順序維持・重複なし）。
    pub selected_clip_ids: Vec<ClipId>,
    /// 右インスペクタで編集中のセグメント番号 (`0 .. visible_midpoints.len()`)。
    pub selected_segment_index: Option<usize>,
    /// 時間表示単位（タイムコード / フレーム）。
    pub time_display_unit: TimeDisplayUnit,
    /// タイムラインレイヤー行密度。
    pub timeline_row_density: TimelineRowDensity,
    /// タイムラインの垂直スクロール位置 (px)。
    pub timeline_scroll_y_px: f32,
    /// プレイヘッド移動時に再生位置上のクリップを選択追従させるか。
    pub follow_selected_on_seek: bool,
    /// プレビュー選択枠・ハンドルを表示するか。
    pub show_preview_guides: bool,
    /// プレビュー中央補助グリッドを表示するか。
    pub show_preview_grid: bool,
    /// プレビュー下部マスター波形ストリップを表示するか。
    pub show_preview_waveform: bool,
    /// プレビュー表示倍率モード。
    pub preview_zoom: PreviewZoomMode,
    /// 固定倍率表示時のパンオフセット (px)。
    pub preview_pan_offset_px: (f32, f32),
    /// 拡大率 `X/Y` のアスペクト比連動ロック。
    pub scale_aspect_locked: bool,
    /// インスペクタ各セクションの開閉状態 (`section_key -> expanded`)。
    pub inspector_sections: std::collections::HashMap<String, bool>,
    /// ショートカット設定。
    pub key_config: crate::ui::key_config::KeyConfig,
    /// 右タブパネルの現在タブ。
    pub right_tab: RightPanelTab,
    /// ユーザーが変更した右パネル幅（未設定時は 1/5）。
    pub right_panel_width: Option<f32>,
    /// クリップインスペクタの編集中下書き状態。
    pub inspector: crate::ui::inspector_panel::InspectorState,
    /// 動画書き出し設定モーダルダイアログ状態。
    pub export_dialog: crate::ui::export_dialog::ExportDialogState,
    /// 画面下部の最新ステータスメッセージ。
    pub status_message: String,
    /// GPUI 用 BGRA ダブルバッファ転送ブリッジ。
    pub surface_bridge: RenderImageSurfaceBridge,
    /// 現在フレームでリンク切れ素材が存在する場合 `true`（「Media Offline」表示条件）。
    pub has_media_offline_at_playhead: bool,
    /// 直前に送信したフレーム位置（変化検出用）。
    last_rendered_frame: Option<u64>,
    /// 編集直後など即時再合成が必要なフラグ。
    dirty_preview: bool,
    /// アクティブな動画デコーダ（素材 ID ごと）。
    video_decoders: HashMap<AssetId, Box<dyn VideoDecoder>>,
    /// VS Code 風 AI チャットパネル状態・提案カード・履歴。
    pub ai_chat: crate::ui::ai_chat_panel::AiChatState,
    /// タイムライン 1 フレームあたりの横幅（px）。
    pub timeline_px_per_frame: f32,
    /// タイムラインの水平スクロール基準フレーム。
    pub timeline_scroll_x_frames: u64,
    /// コピーされたクリップ群保持用クリップボード。
    pub clipboard: Option<crate::timeline::clipboard::ClipClipboard>,
    /// タイムラインドラッグ時のスナップ設定。
    pub snap_config: crate::timeline::snap::SnapConfig,
    /// 現在表示すべきスナップ縦ガイド線のフレーム位置。
    pub snap_guide_frame: Option<u64>,
    /// タイムライン上のアクティブなドラッグ編集状態。
    pub timeline_drag: Option<crate::ui::timeline_panel::TimelineDragState>,
    /// タイムライン上の一般化ポインタジェスチャ状態（マーキー・中間点・Ctrl複製等）。
    pub timeline_gesture: Option<crate::ui::timeline::state::TimelinePointerGesture>,
    /// タイムラインキャンバスの推定表示幅 (px)。
    pub timeline_viewport_canvas_width_px: f32,
    /// 音声デバイス稼働状態と共有スナップショット。
    audio_shared: SharedPlaybackState,
    audio_handle: Option<AudioOutputHandle>,
    gpu_compositor: Option<WgpuCompositor>,
    next_id_seq: u64,
}

impl Default for AppController {
    fn default() -> Self {
        Self::new()
    }
}

impl AppController {
    /// 空の新規プロジェクトでコントローラを初期化する（オーディオデバイス未接続でも失敗しない）。
    #[must_use]
    pub fn new() -> Self {
        Self::from_document(ProjectDocument::new("Untitled Project"))
    }

    /// 既存 [`ProjectDocument`] からコントローラを初期化する。
    #[must_use]
    pub fn from_document(document: ProjectDocument) -> Self {
        let clock = Arc::new(PlaybackClock::new(document.fps, OUTPUT_SAMPLE_RATE_HZ));
        clock.update_timeline_metrics(document.fps, document.total_duration_frames());
        let mixer = AudioMixer::new();
        let audio_shared = SharedPlaybackState {
            document: Arc::new(Mutex::new(document.clone())),
            mixer: Arc::new(Mutex::new(mixer.clone())),
            clock: Arc::clone(&clock),
        };
        let selected_layer_id = document.layers.first().map(|l| l.id.clone());
        let selected_clip_id = document
            .layers
            .first()
            .and_then(|l| l.clips.first())
            .map(|c| c.id.clone());

        let mut controller = Self {
            document,
            dispatcher: CommandDispatcher::new(),
            project_path: None,
            frame_cache: FrameCache::new(DEFAULT_FRAME_CACHE_BYTES),
            mixer,
            clock,
            playback_state: PlaybackState::Stopped,
            selected_layer_id,
            selected_clip_ids: selected_clip_id.iter().cloned().collect(),
            selected_clip_id,
            selected_segment_index: None,
            time_display_unit: TimeDisplayUnit::Timecode,
            timeline_row_density: TimelineRowDensity::Medium,
            timeline_scroll_y_px: 0.0,
            follow_selected_on_seek: false,
            show_preview_guides: true,
            show_preview_grid: false,
            show_preview_waveform: true,
            preview_zoom: PreviewZoomMode::Fit,
            preview_pan_offset_px: (0.0, 0.0),
            scale_aspect_locked: true,
            inspector_sections: default_inspector_sections(),
            key_config: crate::ui::key_config::KeyConfig::with_defaults(),
            right_tab: RightPanelTab::ClipInspector,
            right_panel_width: None,
            inspector: crate::ui::inspector_panel::InspectorState::default(),
            export_dialog: crate::ui::export_dialog::ExportDialogState::default(),
            status_message: "準備完了".to_owned(),
            surface_bridge: RenderImageSurfaceBridge::new(),
            has_media_offline_at_playhead: false,
            last_rendered_frame: None,
            dirty_preview: true,
            video_decoders: HashMap::new(),
            ai_chat: crate::ui::ai_chat_panel::AiChatState::default(),
            timeline_px_per_frame: 3.0,
            timeline_scroll_x_frames: 0,
            clipboard: None,
            snap_config: crate::timeline::snap::SnapConfig::default(),
            snap_guide_frame: None,
            timeline_drag: None,
            timeline_gesture: None,
            timeline_viewport_canvas_width_px: 900.0,
            audio_shared,
            audio_handle: None,
            gpu_compositor: WgpuCompositor::try_new().ok(),
            next_id_seq: 1,
        };
        controller.refresh_preview_now();
        controller
    }

    /// OS 既定オーディオデバイスの起動を試行する（未接続時はシステム時計フォールバックを維持）。
    pub fn try_start_audio_output(&mut self) {
        if self.audio_handle.is_some() {
            return;
        }
        if let Ok(handle) = start_audio_output(&self.audio_shared) {
            self.status_message = format!("Audio output active ({} Hz)", handle.sample_rate());
            self.audio_handle = Some(handle);
        }
        // ヘッドレス環境・音声デバイス非搭載環境では無音＋SystemPlaybackClock に自動フォールバック。
    }

    /// 実オーディオデバイスが稼働中かどうかを返す。
    #[must_use]
    pub const fn has_hardware_audio_output(&self) -> bool {
        self.audio_handle.is_some()
    }

    /// `wgpu` GPU コンポジタが有効化されているかを返す。
    #[must_use]
    pub const fn is_gpu_compositor_active(&self) -> bool {
        self.gpu_compositor.is_some()
    }

    /// 新規空プロジェクトへ初期化する。
    pub fn new_project(&mut self) {
        self.stop();
        self.document = ProjectDocument::new("Untitled Project");
        self.dispatcher.clear_history();
        self.project_path = None;
        self.frame_cache.clear();
        self.mixer = AudioMixer::new();
        self.video_decoders.clear();
        self.selected_layer_id = None;
        self.clear_clip_selection();
        self.timeline_gesture = None;
        self.timeline_scroll_y_px = 0.0;
        "新規プロジェクトを作成しました".clone_into(&mut self.status_message);
        self.sync_shared_state();
        self.dirty_preview = true;
        self.refresh_preview_now();
    }

    /// プロジェクト JSON を読み込み、アセット解決・音声デコード準備を行う。
    ///
    /// # Errors
    ///
    /// 読込または検証に失敗した場合エラー文字列を返す。
    pub fn open_project_file(&mut self, path: &Path) -> Result<(), String> {
        self.stop();
        let doc = load_project_file(path).map_err(|err| err.to_string())?;
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty());

        self.document = doc;
        self.dispatcher.clear_history();
        self.project_path = Some(path.to_path_buf());
        self.frame_cache.clear();
        self.mixer = AudioMixer::new();
        self.video_decoders.clear();
        let first_clip = self
            .document
            .layers
            .first()
            .and_then(|l| l.clips.first())
            .map(|c| c.id.clone());
        self.select_single_clip(first_clip);
        self.selected_layer_id = self.document.layers.first().map(|l| l.id.clone());
        self.timeline_scroll_y_px = 0.0;
        self.preload_available_media(project_dir);
        self.sync_shared_state();
        self.clock.seek_to_frame(FrameIndex::ZERO);
        self.dirty_preview = true;
        self.refresh_preview_now();
        self.status_message = format!("プロジェクトを開きました: {}", path.display());
        Ok(())
    }

    /// 現在のプロジェクトを JSON ファイルへ保存する。
    ///
    /// # Errors
    ///
    /// 検証または I/O エラー時にエラー文字列を返す。
    pub fn save_project_file(&mut self, path: &Path) -> Result<(), String> {
        save_project_file(&self.document, path).map_err(|err| err.to_string())?;
        self.project_path = Some(path.to_path_buf());
        self.status_message = format!("保存しました: {}", path.display());
        Ok(())
    }

    /// メディアファイル（動画・音声・画像）を取り込み、タイムライン末尾（または先頭）へ配置する。
    ///
    /// # Errors
    ///
    /// ファイルが存在しない・未対応形式・コマンド実行エラー時。
    pub fn import_media_file(&mut self, path: &Path) -> Result<AssetId, String> {
        let mut ids = self.import_media_files_at_drop(&[path.to_path_buf()], None, None)?;
        ids.pop()
            .ok_or_else(|| "取り込みアセット ID が生成されませんでした".to_owned())
    }

    /// タイムラインへのドラッグ＆ドロップで複数メディアファイルを取り込み、指定トラック・開始位置へ配置する。
    ///
    /// - `target_layer_id` が `None` の場合は選択中または先頭レイヤーへ配置し、レイヤーが空なら `Track 1` を同時生成します。
    /// - `requested_start` が `None` の場合は対象レイヤー末尾へ追加し、`Some(frame)` の場合はスナップ判定のうえ
    ///   既存クリップと衝突しない最初の空き区間へ順に配置します。
    /// - 成功した全ファイルは 1 つの `EditCommand::Batch` として実行されるため、1 回の Undo で取り消せます。
    ///
    /// # Errors
    ///
    /// 指定レイヤーがロック中・存在しない場合、または有効なメディアファイルが 1 件も無い場合。
    pub fn import_media_files_at_drop(
        &mut self,
        paths: &[PathBuf],
        target_layer_id: Option<&LayerId>,
        requested_start: Option<FrameIndex>,
    ) -> Result<Vec<AssetId>, String> {
        if paths.is_empty() {
            return Err("ドロップされたファイルがありません".to_owned());
        }

        let (prepared, skipped_errors) = self.prepare_media_import_batch(paths)?;
        let (layer_id, new_layer_cmd) = self.resolve_drop_target_layer(target_layer_id)?;
        let cursor_frame = self.resolve_drop_start_frame(&layer_id, requested_start);
        let (cmd, planned_assets) =
            self.build_drop_import_batch(&layer_id, new_layer_cmd, cursor_frame, &prepared)?;

        let report = self
            .dispatcher
            .dispatch(&mut self.document, &cmd)
            .map_err(|err| err.to_string())?;

        for (asset_id, item) in planned_assets.iter().zip(&prepared) {
            self.register_decoded_resources_for_asset(asset_id, &item.source_path, item.ext_kind);
        }
        let last_key = format!("imported_clip_{}", prepared.len().saturating_sub(1));
        if let Some(new_clip) = report.created_clips.get(&last_key) {
            self.select_single_clip(Some(new_clip.clone()));
        }
        self.selected_layer_id = Some(layer_id);
        self.sync_shared_state();
        self.dirty_preview = true;
        self.refresh_preview_now();
        self.status_message = format_drop_import_status(&prepared, &skipped_errors);
        Ok(planned_assets)
    }

    fn prepare_media_import_batch(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<(Vec<PreparedMediaImport>, Vec<String>), String> {
        let mut prepared = Vec::with_capacity(paths.len());
        let mut skipped_errors = Vec::new();
        for path in paths {
            match self.prepare_media_import(path) {
                Ok(item) => prepared.push(item),
                Err(err) => skipped_errors.push(err),
            }
        }
        if prepared.is_empty() {
            let detail = skipped_errors.into_iter().next().unwrap_or_else(|| {
                "対応している動画・音声・静止画ファイルが見つかりません".to_owned()
            });
            return Err(detail);
        }
        Ok((prepared, skipped_errors))
    }

    fn resolve_drop_start_frame(
        &self,
        layer_id: &LayerId,
        requested_start: Option<FrameIndex>,
    ) -> u64 {
        let Some(frame) = requested_start else {
            return self.layer_end_frame(layer_id);
        };
        let raw = frame.get();
        if !self.snap_config.enabled {
            return raw;
        }
        let anchors = crate::timeline::snap::collect_snap_anchors(
            &self.document,
            self.clock.current_frame(),
            &[],
            &self.snap_config,
        );
        crate::timeline::snap::snap_frame_edge(raw, &anchors, self.snap_config.threshold_frames)
            .map_or(raw, |hit| hit.frame)
    }

    fn build_drop_import_batch(
        &mut self,
        layer_id: &LayerId,
        new_layer_cmd: Option<EditCommand>,
        mut cursor_frame: u64,
        prepared: &[PreparedMediaImport],
    ) -> Result<(EditCommand, Vec<AssetId>), String> {
        let mut occupied = self.collect_layer_intervals(layer_id);
        let mut commands =
            Vec::with_capacity(prepared.len() * 2 + usize::from(new_layer_cmd.is_some()));
        if let Some(add_layer) = new_layer_cmd {
            commands.push(add_layer);
        }

        let mut planned_assets = Vec::with_capacity(prepared.len());
        for (idx, item) in prepared.iter().enumerate() {
            let asset_id = self.allocate_asset_id()?;
            let dur_frames = item.clip_frames.get();
            let start = find_free_interval_start(&occupied, cursor_frame, dur_frames);
            occupied.push((start, start.saturating_add(dur_frames)));
            cursor_frame = start.saturating_add(dur_frames);

            commands.push(EditCommand::AddAsset {
                temp_id: None,
                id: Some(asset_id.clone()),
                name: item.display_name.clone(),
                path: item.asset_path.clone(),
                kind: item.asset_kind.clone(),
            });
            commands.push(EditCommand::AddClip {
                temp_id: Some(format!("imported_clip_{idx}")),
                id: None,
                layer_id: LayerRef::Id(layer_id.clone()),
                clip: Box::new(NewClipSpec {
                    name: item.display_name.clone(),
                    timeline_start: FrameIndex::new(start),
                    duration: item.clip_frames,
                    effect_offset_frames: 0,
                    effect_duration_frames: None,
                    midpoints: Vec::new(),
                    kind: build_new_clip_kind(&asset_id, item.clip_kind),
                }),
            });
            planned_assets.push(asset_id);
        }
        Ok((EditCommand::Batch { commands }, planned_assets))
    }

    /// テストおよび欠損リカバリ検証向けに、`AssetStatus::Missing` の動画クリップを配置する。
    ///
    /// # Errors
    ///
    /// コマンド適用または ID 生成に失敗した場合。
    pub fn insert_missing_placeholder_clip(
        &mut self,
        name: &str,
        expected_path: &Path,
        duration_frames: u64,
    ) -> Result<(AssetId, ClipId), String> {
        let asset_id = self.allocate_asset_id()?;
        let layer_id = self.ensure_target_layer_id()?;
        let frames = FrameCount::new(duration_frames.max(1)).map_err(|e| e.to_string())?;
        let start = FrameIndex::new(self.layer_end_frame(&layer_id));
        let clip_id =
            ClipId::new(format!("clip-missing-{}", self.next_seq())).map_err(|e| e.to_string())?;

        self.document.assets.push(Asset {
            id: asset_id.clone(),
            name: name.to_owned(),
            path: AssetPath::from_absolute(expected_path.to_string_lossy().into_owned()),
            kind: AssetKind::Video {
                has_audio: false,
                default_fps: Some(self.document.fps),
                duration_frames: Some(frames),
                width: Some(self.document.resolution.width.get()),
                height: Some(self.document.resolution.height.get()),
                source_duration: RationalDuration::from_millis(duration_frames.saturating_mul(17)),
            },
            status: AssetStatus::Missing {
                expected_path: expected_path.to_string_lossy().into_owned(),
            },
        });
        if let Some(layer) = self.document.find_layer_mut(&layer_id) {
            layer.clips.push(Clip {
                id: clip_id.clone(),
                name: name.to_owned(),
                timeline_start: start,
                duration: frames,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: ClipKind::Video {
                    asset_id: asset_id.clone(),
                    source_in: RationalDuration::ZERO,
                    playback_rate: PlaybackRate::NORMAL,
                    preserve_pitch: true,
                    visual: crate::media::visual_properties::VisualProperties::default(),
                    audio: None,
                },
            });
            layer.sort_clips();
        }
        self.select_single_clip(Some(clip_id.clone()));
        self.selected_layer_id = Some(layer_id);
        self.sync_shared_state();
        self.dirty_preview = true;
        self.refresh_preview_now();
        Ok((asset_id, clip_id))
    }

    /// 統一コマンドディスパッチャ経由で編集コマンドを実行し、クロックとプレビューを更新する。
    ///
    /// # Errors
    ///
    /// コマンドの検証または実行に失敗した場合。
    pub fn dispatch_command(&mut self, cmd: &EditCommand) -> Result<String, String> {
        let report = self
            .dispatcher
            .dispatch(&mut self.document, cmd)
            .map_err(|err| err.to_string())?;
        self.sync_selection_after_edit();
        self.sync_shared_state();
        self.dirty_preview = true;
        self.refresh_preview_now();
        self.status_message.clone_from(&report.summary);
        Ok(report.summary)
    }

    /// Undo を実行する。
    ///
    /// # Errors
    ///
    /// Undo の逆パッチ適用に失敗した場合。
    pub fn undo(&mut self) -> Result<bool, String> {
        let undone = self
            .dispatcher
            .undo(&mut self.document)
            .map_err(|err| err.to_string())?;
        if let Some(label) = undone {
            self.timeline_gesture = None;
            self.timeline_drag = None;
            self.sync_selection_after_edit();
            self.sync_shared_state();
            self.dirty_preview = true;
            self.refresh_preview_now();
            self.status_message = format!("Undo: {label}");
            return Ok(true);
        }
        Ok(false)
    }

    /// Redo を実行する。
    ///
    /// # Errors
    ///
    /// Redo の再適用に失敗した場合。
    pub fn redo(&mut self) -> Result<bool, String> {
        let redone = self
            .dispatcher
            .redo(&mut self.document)
            .map_err(|err| err.to_string())?;
        if let Some(label) = redone {
            self.timeline_gesture = None;
            self.timeline_drag = None;
            self.sync_selection_after_edit();
            self.sync_shared_state();
            self.dirty_preview = true;
            self.refresh_preview_now();
            self.status_message = format!("Redo: {label}");
            return Ok(true);
        }
        Ok(false)
    }

    /// 選択中クリップを現在再生ヘッド位置で左右 2 つに非破壊分割する。
    ///
    /// # Errors
    ///
    /// 選択クリップ不在、または再生ヘッドがクリップ境界外の場合。
    pub fn split_selected_clip_at_playhead(&mut self) -> Result<(), String> {
        let clip_id = self
            .selected_clip_id
            .clone()
            .ok_or_else(|| "分割するクリップが選択されていません".to_owned())?;
        let playhead = self.clock.current_frame();
        let cmd = EditCommand::SplitClip {
            clip_id: ClipRef::Id(clip_id),
            split_at_timeline_frame: playhead,
            right_temp_id: None,
            right_id: None,
        };
        let _ = self.dispatch_command(&cmd)?;
        Ok(())
    }

    /// 選択中クリップを削除する。
    ///
    /// # Errors
    ///
    /// クリップ未選択または削除失敗時。
    pub fn delete_selected_clip(&mut self) -> Result<(), String> {
        let clip_id = self
            .selected_clip_id
            .take()
            .ok_or_else(|| "削除するクリップが選択されていません".to_owned())?;
        let _ = self.dispatch_command(&EditCommand::RemoveClip {
            clip_id: ClipRef::Id(clip_id),
        })?;
        self.inspector.invalidate_sync();
        Ok(())
    }

    /// 選択中クリップを内部クリップボードへコピーする。
    ///
    /// # Errors
    ///
    /// クリップ未選択または存在しない ID の場合。
    pub fn copy_selected_clip(&mut self) -> Result<usize, String> {
        let clip_id = self
            .selected_clip_id
            .clone()
            .ok_or_else(|| "コピーするクリップが選択されていません".to_owned())?;
        let cb = crate::timeline::clipboard::copy_clips(&self.document, &[clip_id])
            .map_err(|e| e.to_string())?;
        let count = cb.len();
        self.clipboard = Some(cb);
        self.status_message = format!("{count} 個のクリップをコピーしました");
        Ok(count)
    }

    /// クリップボード内容を現在再生ヘッド（衝突時は空きスロット）へ貼り付ける。
    ///
    /// # Errors
    ///
    /// クリップボードが空、または貼り付け失敗時。
    pub fn paste_clipboard_at_playhead(&mut self) -> Result<(), String> {
        let cb = self
            .clipboard
            .as_ref()
            .ok_or_else(|| "貼り付けるクリップボードが空です".to_owned())?
            .clone();
        let base_layer = self
            .selected_layer_id
            .as_ref()
            .and_then(|lid| self.document.layers.iter().position(|l| &l.id == lid))
            .unwrap_or(0);
        let head = self.clock.current_frame();
        let cmd = crate::timeline::clipboard::build_collision_free_paste_command(
            &cb,
            &self.document,
            head,
            base_layer,
        )
        .map_err(|e| e.to_string())?;
        let _ = self.dispatch_command(&cmd)?;
        self.inspector.invalidate_sync();
        Ok(())
    }

    /// 選択中クリップを即座に複製 (`Ctrl+D`) して空きフレームへ配置する。
    ///
    /// # Errors
    ///
    /// クリップ未選択または複製エラー時。
    pub fn duplicate_selected_clip(&mut self) -> Result<(), String> {
        let clip_id = self
            .selected_clip_id
            .clone()
            .ok_or_else(|| "複製するクリップが選択されていません".to_owned())?;
        let cmd = crate::timeline::clipboard::build_duplicate_selection_command(
            &self.document,
            &[clip_id],
        )
        .map_err(|e| e.to_string())?;
        let _ = self.dispatch_command(&cmd)?;
        self.inspector.invalidate_sync();
        Ok(())
    }

    /// スナップ判定を適用しつつ指定クリップを別時間位置・別レイヤーへ移動する。
    ///
    /// # Errors
    ///
    /// クリップ／レイヤー不在、または既存クリップとの重複・ロックエラー時。
    pub fn move_clip_with_snap(
        &mut self,
        clip_id: &ClipId,
        target_layer_idx: usize,
        raw_start: FrameIndex,
    ) -> Result<FrameIndex, String> {
        let duration = self
            .document
            .find_clip(clip_id)
            .ok_or_else(|| format!("クリップ `{clip_id}` が見つかりません"))?
            .1
            .duration;
        let target_layer_id = self
            .document
            .layers
            .get(target_layer_idx)
            .ok_or_else(|| format!("レイヤーインデックス {target_layer_idx} が範囲外です"))?
            .id
            .clone();
        let snap = crate::timeline::snap::snap_clip_move(
            &self.document,
            self.clock.current_frame(),
            duration,
            raw_start,
            std::slice::from_ref(clip_id),
            &self.snap_config,
        );
        self.snap_guide_frame = snap.guide_frame;
        let cmd = EditCommand::MoveClip {
            clip_id: ClipRef::Id(clip_id.clone()),
            target_layer_id: Some(LayerRef::Id(target_layer_id.clone())),
            new_timeline_start: snap.snapped_start,
        };
        let _ = self.dispatch_command(&cmd)?;
        self.selected_layer_id = Some(target_layer_id);
        Ok(snap.snapped_start)
    }

    /// 指定クリップの左端を指定フレーム数ぶんトリム（正）または延長（負）する。
    ///
    /// # Errors
    ///
    /// 不正なトリム量またはクリップ長制約違反時。
    pub fn trim_clip_left_delta(
        &mut self,
        clip_id: &ClipId,
        delta_frames: i64,
    ) -> Result<(), String> {
        let cmd = EditCommand::TrimLeft {
            clip_id: ClipRef::Id(clip_id.clone()),
            delta_frames,
        };
        let _ = self.dispatch_command(&cmd)?;
        self.inspector.invalidate_sync();
        Ok(())
    }

    /// 指定クリップの右端を指定フレーム数ぶんトリム（正）または延長（負）する。
    ///
    /// # Errors
    ///
    /// 不正なトリム量またはクリップ長制約違反時。
    pub fn trim_clip_right_delta(
        &mut self,
        clip_id: &ClipId,
        delta_frames: i64,
    ) -> Result<(), String> {
        let cmd = EditCommand::TrimRight {
            clip_id: ClipRef::Id(clip_id.clone()),
            delta_frames,
        };
        let _ = self.dispatch_command(&cmd)?;
        self.inspector.invalidate_sync();
        Ok(())
    }

    /// 新しい空トラックレイヤーを末尾へ追加する。
    ///
    /// # Errors
    ///
    /// コマンド実行失敗時。
    pub fn add_empty_layer(&mut self) -> Result<LayerId, String> {
        let new_id =
            LayerId::new(format!("layer-{}", self.next_seq())).map_err(|e| e.to_string())?;
        let idx = self.document.layers.len() + 1;
        let cmd = EditCommand::AddLayer {
            temp_id: None,
            id: Some(new_id.clone()),
            name: format!("Track {idx}"),
            insert_index: None,
        };
        let _ = self.dispatch_command(&cmd)?;
        self.selected_layer_id = Some(new_id.clone());
        Ok(new_id)
    }

    /// タイムラインの表示倍率 (`timeline_px_per_frame`) を安全範囲内で拡大・縮小する。
    pub fn zoom_timeline(&mut self, factor: f32) {
        self.timeline_px_per_frame = (self.timeline_px_per_frame * factor)
            .clamp(MIN_TIMELINE_PX_PER_FRAME, MAX_TIMELINE_PX_PER_FRAME);
    }

    /// 選択クリップの不透明度を定数 `AnimatedParameter` として設定する。
    ///
    /// # Errors
    ///
    /// 非視覚クリップまたは検証失敗時。
    pub fn set_selected_clip_opacity(&mut self, opacity: f64) -> Result<(), String> {
        let clip_id = self
            .selected_clip_id
            .clone()
            .ok_or_else(|| "クリップが選択されていません".to_owned())?;
        let cmd = EditCommand::SetParameter {
            target: ParameterTarget::Clip {
                clip_id: ClipRef::Id(clip_id),
            },
            path: ParameterPath::Opacity,
            value: ParameterValue::Animated {
                param: crate::core::animation::AnimatedParameter::constant(opacity.clamp(0.0, 1.0)),
            },
        };
        let _ = self.dispatch_command(&cmd)?;
        Ok(())
    }

    /// 選択クリップの再生速度を設定する。
    ///
    /// # Errors
    ///
    /// 速度値が不正、または素材長超過の場合。
    pub fn set_selected_clip_playback_rate(&mut self, num: u32, den: u32) -> Result<(), String> {
        let clip_id = self
            .selected_clip_id
            .clone()
            .ok_or_else(|| "クリップが選択されていません".to_owned())?;
        let new_rate = PlaybackRate::new(num, den).map_err(|e| e.to_string())?;
        let cmd = EditCommand::SetPlaybackRate {
            clip_id: ClipRef::Id(clip_id),
            new_rate,
            preserve_pitch: None,
        };
        let _ = self.dispatch_command(&cmd)?;
        Ok(())
    }

    /// 指定レイヤーのミュート状態を切り替える。
    ///
    /// # Errors
    ///
    /// レイヤーが見つからない場合。
    pub fn toggle_layer_muted(&mut self, layer_id: &LayerId) -> Result<(), String> {
        let current = self
            .document
            .find_layer(layer_id)
            .ok_or_else(|| format!("レイヤー `{layer_id}` が見つかりません"))?
            .muted;
        let _ = self.dispatch_command(&EditCommand::SetLayerFlags {
            layer_id: LayerRef::Id(layer_id.clone()),
            name: None,
            visible: None,
            muted: Some(!current),
            locked: None,
        })?;
        Ok(())
    }

    /// 再生を開始する。末尾にいる場合は先頭へ戻してから再生する。
    pub fn play(&mut self) {
        let total = self.document.total_duration_frames();
        if total > 0 && self.clock.current_frame().get() >= total {
            self.clock.seek_to_frame(FrameIndex::ZERO);
        }
        self.clock.set_playing(true);
        self.playback_state = PlaybackState::Playing;
    }

    /// 現在位置で一時停止する。
    pub fn pause(&mut self) {
        self.clock.set_playing(false);
        self.playback_state = PlaybackState::Paused;
    }

    /// 再生と一時停止を切り替える。
    pub fn toggle_play_pause(&mut self) {
        match self.playback_state {
            PlaybackState::Playing => self.pause(),
            PlaybackState::Stopped | PlaybackState::Paused | PlaybackState::Seeking => self.play(),
        }
    }

    /// 再生を停止して先頭フレーム (`0`) へ戻す。
    pub fn stop(&mut self) {
        self.clock.set_playing(false);
        self.clock.seek_to_frame(FrameIndex::ZERO);
        self.playback_state = PlaybackState::Stopped;
        self.dirty_preview = true;
        self.refresh_preview_now();
    }

    /// 指定フレームへシークし、プレビューを即時更新する。
    pub fn seek_to_frame(&mut self, frame: FrameIndex) {
        self.clock.update_timeline_metrics(
            self.document.fps,
            self.document.total_duration_frames(),
        );
        let max_frame = self.document.seek_upper_bound_frames();
        let clamped = FrameIndex::new(frame.get().min(max_frame));
        let was_playing = self.playback_state == PlaybackState::Playing;
        self.playback_state = PlaybackState::Seeking;
        self.clock.seek_to_frame(clamped);
        self.dirty_preview = true;
        self.refresh_preview_now();
        self.playback_state = if was_playing {
            PlaybackState::Playing
        } else if clamped.get() == 0 {
            PlaybackState::Stopped
        } else {
            PlaybackState::Paused
        };
    }

    /// 一定経過時間ごとに UI スレッドから呼ばれる再生進行ティック（`~60Hz`）。
    ///
    /// 実音声デバイスが稼働していない場合は `clock.advance_by_elapsed_seconds` で単調時計フォールバックを進める。
    pub fn tick_playback(&mut self, elapsed_seconds: f64) {
        if self.playback_state == PlaybackState::Playing {
            if self.audio_handle.is_none() {
                self.clock.advance_by_elapsed_seconds(elapsed_seconds);
            }
            let total = self.document.total_duration_frames();
            if total > 0 && self.clock.current_frame().get() >= total {
                self.clock.set_playing(false);
                self.clock.seek_to_frame(FrameIndex::new(total));
                self.playback_state = PlaybackState::Paused;
                self.dirty_preview = true;
            }
        }
        let now_frame = self.clock.current_frame().get();
        if self.dirty_preview || self.last_rendered_frame != Some(now_frame) {
            self.refresh_preview_now();
        }
    }

    /// 現在再生位置のプレビューを合成して `surface_bridge` へ転送する。
    pub fn refresh_preview_now(&mut self) {
        let playhead = self.clock.current_frame();
        self.ensure_decoded_frames_for_playhead(playhead);

        let (pw, ph) = preview_dimensions(&self.document);
        let cache = &mut self.frame_cache;
        let output = composite_with_gpu_or_fallback(
            &self.document,
            playhead,
            pw,
            ph,
            self.gpu_compositor.as_mut(),
            &mut |asset_id, src_idx| cache.get(&FrameCacheKey::new(asset_id.clone(), src_idx, 100)),
        );

        self.has_media_offline_at_playhead = output.has_missing_media;
        self.surface_bridge.submit_bgra_frame(&output.buffer);
        self.last_rendered_frame = Some(playhead.get());
        self.dirty_preview = false;
    }

    /// プロジェクトを保存済みディレクトリ基準で全アセット再解決する（ファイル変更・再リンク反映）。
    pub fn resolve_assets(&mut self) {
        let dir = self
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty());
        resolve_document_assets(&mut self.document, dir);
        self.dirty_preview = true;
        self.refresh_preview_now();
    }

    /// 現在の選択クリップ参照を返す。
    #[must_use]
    pub fn selected_clip(&self) -> Option<(&Layer, &Clip)> {
        let cid = self.selected_clip_id.as_ref()?;
        self.document.find_clip(cid)
    }

    fn sync_shared_state(&self) {
        let total = self.document.total_duration_frames();
        self.clock.update_timeline_metrics(self.document.fps, total);
        if let Ok(mut guard) = self.audio_shared.document.lock() {
            *guard = self.document.clone();
        }
        if let Ok(mut guard) = self.audio_shared.mixer.lock() {
            *guard = self.mixer.clone();
        }
    }

    fn sync_selection_after_edit(&mut self) {
        self.selected_clip_ids
            .retain(|cid| self.document.find_clip(cid).is_some());
        if self.selected_clip_ids.is_empty()
            && let Some(cid) = &self.selected_clip_id
            && self.document.find_clip(cid).is_some()
        {
            self.selected_clip_ids.push(cid.clone());
        }

        if let Some(cid) = &self.selected_clip_id
            && self.document.find_clip(cid).is_none()
        {
            self.selected_clip_id = self.selected_clip_ids.first().cloned().or_else(|| {
                self.document
                    .layers
                    .first()
                    .and_then(|l| l.clips.first())
                    .map(|c| c.id.clone())
            });
            if self.selected_clip_ids.is_empty()
                && let Some(fallback) = &self.selected_clip_id
            {
                self.selected_clip_ids.push(fallback.clone());
            }
        } else if !self.selected_clip_ids.is_empty() {
            self.selected_clip_id = self.selected_clip_ids.first().cloned();
        }

        if let Some(seg_idx) = self.selected_segment_index {
            let valid_seg = self
                .selected_clip_id
                .as_ref()
                .and_then(|cid| self.document.find_clip(cid))
                .is_some_and(|(_, clip)| seg_idx <= clip.visible_midpoints().len());
            if !valid_seg {
                self.selected_segment_index = None;
            }
        }

        if let Some(lid) = &self.selected_layer_id
            && self.document.find_layer(lid).is_none()
        {
            self.selected_layer_id = self.document.layers.first().map(|l| l.id.clone());
        }
    }

    /// 実質的な選択クリップ ID 一覧を返す（`selected_clip_id` 直接代入への後方互換を維持）。
    #[must_use]
    pub fn effective_selected_clip_ids(&self) -> Vec<ClipId> {
        if self.selected_clip_ids.is_empty() {
            self.selected_clip_id.iter().cloned().collect()
        } else {
            self.selected_clip_ids.clone()
        }
    }

    /// 単一クリップ選択状態を設定する（`None` で選択解除）。
    pub fn select_single_clip(&mut self, clip_id: Option<ClipId>) {
        self.selected_clip_ids.clear();
        if let Some(cid) = clip_id {
            if let Some((layer, _)) = self.document.find_clip(&cid) {
                self.selected_layer_id = Some(layer.id.clone());
            }
            self.selected_clip_id = Some(cid.clone());
            self.selected_clip_ids.push(cid);
        } else {
            self.selected_clip_id = None;
        }
        self.selected_segment_index = None;
    }

    /// 指定クリップの選択状態をトグルする（`Ctrl+Click` 用）。
    pub fn toggle_clip_selection(&mut self, clip_id: &ClipId) {
        if self.selected_clip_ids.is_empty()
            && let Some(prev) = self.selected_clip_id.take()
        {
            self.selected_clip_ids.push(prev);
        }
        if let Some(pos) = self.selected_clip_ids.iter().position(|id| id == clip_id) {
            self.selected_clip_ids.remove(pos);
        } else {
            self.selected_clip_ids.push(clip_id.clone());
            if let Some((layer, _)) = self.document.find_clip(clip_id) {
                self.selected_layer_id = Some(layer.id.clone());
            }
        }
        self.selected_clip_id = self.selected_clip_ids.first().cloned();
        self.selected_segment_index = None;
    }

    /// 複数選択クリップ一覧を明示的に設定する（重複除去・先頭を `selected_clip_id` へ同期）。
    pub fn set_selected_clips(&mut self, clip_ids: Vec<ClipId>) {
        let mut unique = Vec::with_capacity(clip_ids.len());
        for cid in clip_ids {
            if !unique.contains(&cid) {
                unique.push(cid);
            }
        }
        self.selected_clip_ids = unique;
        self.selected_clip_id = self.selected_clip_ids.first().cloned();
        if let Some(first) = &self.selected_clip_id
            && let Some((layer, _)) = self.document.find_clip(first)
        {
            self.selected_layer_id = Some(layer.id.clone());
        }
        self.selected_segment_index = None;
    }

    /// 全レイヤーの全クリップを選択状態にする (`Ctrl+A`)。
    pub fn select_all_clips(&mut self) {
        let all: Vec<ClipId> = self
            .document
            .layers
            .iter()
            .flat_map(|layer| layer.clips.iter().map(|clip| clip.id.clone()))
            .collect();
        self.set_selected_clips(all);
    }

    /// クリップ・セグメント選択をすべて解除する (`Ctrl+Shift+A` / `Escape`)。
    pub fn clear_clip_selection(&mut self) {
        self.selected_clip_ids.clear();
        self.selected_clip_id = None;
        self.selected_segment_index = None;
    }

    /// 指定クリップおよびセグメントインデックスを単一選択する。
    pub fn select_clip_segment(&mut self, clip_id: &ClipId, segment_index: usize) {
        self.select_single_clip(Some(clip_id.clone()));
        if let Some((_, clip)) = self.document.find_clip(clip_id) {
            let max_seg = clip.grid_frames().len().saturating_sub(2);
            self.selected_segment_index = Some(segment_index.min(max_seg));
        }
    }

    /// タイムラインキャンバスのウィンドウ X 原点（ヘッダ幅）を返す。
    #[must_use]
    pub const fn timeline_canvas_origin_x(&self) -> f32 {
        crate::ui::timeline::state::TRACK_HEADER_WIDTH
    }

    /// タイムラインのスナップ有効・無効を切り替える。
    pub fn toggle_snap_enabled(&mut self) {
        self.snap_config.enabled = !self.snap_config.enabled;
    }

    /// 再生位置移動時の選択クリップ追従を切り替える。
    pub fn toggle_follow_selected_on_seek(&mut self) {
        self.follow_selected_on_seek = !self.follow_selected_on_seek;
    }

    /// 時間表示単位 (`Timecode` / `Frames`) を切り替える。
    pub fn toggle_time_display_unit(&mut self) {
        self.time_display_unit = match self.time_display_unit {
            TimeDisplayUnit::Timecode => TimeDisplayUnit::Frames,
            TimeDisplayUnit::Frames => TimeDisplayUnit::Timecode,
        };
    }

    /// タイムラインレイヤー行密度プリセットを順送りする (`Compact -> Medium -> Large`)。
    pub fn cycle_timeline_row_density(&mut self) {
        self.timeline_row_density = self.timeline_row_density.cycle();
    }

    /// インスペクタセクションの開閉状態を返す（未登録時は規定で開く）。
    #[must_use]
    pub fn is_inspector_section_open(&self, key: &str) -> bool {
        self.inspector_sections.get(key).copied().unwrap_or(true)
    }

    /// インスペクタセクションの開閉状態をトグルする。
    pub fn toggle_inspector_section(&mut self, key: &str) {
        let next = !self.is_inspector_section_open(key);
        self.inspector_sections.insert(key.to_owned(), next);
    }

    /// プロジェクト総尺モードを設定し、手動短縮時は再生位置をクランプする。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn set_timeline_length(
        &mut self,
        mode: crate::timeline::document::TimelineLengthMode,
    ) -> Result<String, String> {
        let res = self.dispatch_command(&EditCommand::SetTimelineLength { mode })?;
        let upper = self.document.seek_upper_bound_frames();
        if self.clock.current_frame().get() > upper {
            self.seek_to_frame(FrameIndex::new(upper));
        }
        Ok(res)
    }

    /// タイムライン上の選択範囲 `[start, end)` を設定または解除する。
    ///
    /// # Errors
    ///
    /// `start >= end` 等の検証失敗時。
    pub fn set_selection_range(
        &mut self,
        range: Option<crate::timeline::document::FrameRange>,
    ) -> Result<String, String> {
        self.dispatch_command(&EditCommand::SetSelectionRange { range })
    }

    /// 現在の再生位置を `[`（選択開始点）として設定する。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn set_selection_range_start_at_playhead(&mut self) -> Result<String, String> {
        let start = self.clock.current_frame();
        let end = match self.document.selection_range {
            Some(existing) if existing.end > start => existing.end,
            _ => FrameIndex::new(start.get().saturating_add(1)),
        };
        self.set_selection_range(Some(crate::timeline::document::FrameRange { start, end }))
    }

    /// 現在の再生位置を `]`（選択終了点、当該フレームを含むため `playhead + 1`）として設定する。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn set_selection_range_end_at_playhead(&mut self) -> Result<String, String> {
        let end = FrameIndex::new(self.clock.current_frame().get().saturating_add(1));
        let start = match self.document.selection_range {
            Some(existing) if existing.start < end => existing.start,
            Some(_) => FrameIndex::new(end.get().saturating_sub(1)),
            None => FrameIndex::ZERO,
        };
        self.set_selection_range(Some(crate::timeline::document::FrameRange { start, end }))
    }

    /// 指定クリップの指定エフェクト座標へ中間点を追加する。
    ///
    /// # Errors
    ///
    /// クリップ長が 3 フレーム未満、境界外、既存重複、ロック時。
    pub fn add_midpoint(&mut self, clip_id: &ClipId, effect_frame: i64) -> Result<String, String> {
        self.dispatch_command(&EditCommand::AddMidpoint {
            clip_id: crate::core::ids::ClipRef::Id(clip_id.clone()),
            effect_frame,
        })
    }

    /// 選択中クリップの現在再生位置に中間点を追加する。
    ///
    /// # Errors
    ///
    /// クリップ未選択、再生位置がクリップ内部にない、または中間点制約違反時。
    pub fn add_midpoint_at_playhead(&mut self) -> Result<String, String> {
        let selected = self.effective_selected_clip_ids();
        if selected.len() != 1 {
            return Err("中間点追加は単一クリップ選択時のみ実行できます".to_owned());
        }
        let cid = selected[0].clone();
        let (_, clip) = self
            .document
            .find_clip(&cid)
            .ok_or_else(|| format!("クリップ `{cid}` が見つかりません"))?;
        let playhead = self.clock.current_frame().get();
        let start = clip.timeline_start.get();
        let end = clip.timeline_end().map_err(|e| e.to_string())?.get();
        if !(start < playhead && playhead + 1 < end) {
            return Err("再生位置がクリップ内部の分割可能位置にありません".to_owned());
        }
        let local_i64 = i64::try_from(playhead - start).map_err(|_| "frame overflow".to_owned())?;
        let eff_frame = clip
            .effect_offset_frames
            .checked_add(local_i64)
            .ok_or_else(|| "effect frame overflow".to_owned())?;
        self.add_midpoint(&cid, eff_frame)
    }

    /// 指定クリップ上の中間点を移動する。
    ///
    /// # Errors
    ///
    /// 移動先が範囲外または既存中間点と重複する場合。
    pub fn move_midpoint(
        &mut self,
        clip_id: &ClipId,
        old_effect_frame: i64,
        new_effect_frame: i64,
    ) -> Result<String, String> {
        self.dispatch_command(&EditCommand::MoveMidpoint {
            clip_id: crate::core::ids::ClipRef::Id(clip_id.clone()),
            old_effect_frame,
            new_effect_frame,
        })
    }

    /// 指定クリップから指定中間点を削除する。
    ///
    /// # Errors
    ///
    /// 中間点未存在またはレイヤーロック時。
    pub fn remove_midpoint(
        &mut self,
        clip_id: &ClipId,
        effect_frame: i64,
    ) -> Result<String, String> {
        self.dispatch_command(&EditCommand::RemoveMidpoint {
            clip_id: crate::core::ids::ClipRef::Id(clip_id.clone()),
            effect_frame,
        })
    }

    /// 選択中クリップについて再生位置と一致または直前の中間点を削除する (`Ctrl+/`)。
    ///
    /// # Errors
    ///
    /// クリップ未選択または対象中間点が存在しない場合。
    pub fn remove_midpoint_near_playhead(&mut self) -> Result<String, String> {
        let selected = self.effective_selected_clip_ids();
        if selected.len() != 1 {
            return Err("中間点削除は単一クリップ選択時のみ実行できます".to_owned());
        }
        let cid = selected[0].clone();
        let (_, clip) = self
            .document
            .find_clip(&cid)
            .ok_or_else(|| format!("クリップ `{cid}` が見つかりません"))?;
        let playhead = self.clock.current_frame().get();
        let start = clip.timeline_start.get();
        let end = clip.timeline_end().map_err(|e| e.to_string())?.get();
        if !(start <= playhead && playhead < end) {
            return Err("再生位置が選択クリップの範囲外です".to_owned());
        }
        let local_i64 = i64::try_from(playhead - start).map_err(|_| "frame overflow".to_owned())?;
        let eff_frame = clip
            .effect_offset_frames
            .checked_add(local_i64)
            .ok_or_else(|| "effect frame overflow".to_owned())?;
        let target_m = clip
            .visible_midpoints()
            .into_iter()
            .rfind(|&m| m <= eff_frame)
            .ok_or_else(|| "再生位置周辺に削除可能な中間点がありません".to_owned())?;
        self.remove_midpoint(&cid, target_m)
    }

    /// 指定タイムライン位置に新しい字幕クリップを追加する。
    ///
    /// `layer_id` が `None` の場合は現在選択中または空いている非ロックレイヤーを選び、
    /// 空きがなければ新規レイヤーを自動追加する（1 回の Undo で取り消し可能）。
    ///
    /// # Errors
    ///
    /// 指定レイヤーがロック状態または区間重複・ID 生成失敗時。
    pub fn add_subtitle_clip_at(
        &mut self,
        layer_id: Option<&LayerId>,
        start_frame: FrameIndex,
        duration: FrameCount,
        text: impl Into<String>,
    ) -> Result<ClipId, String> {
        let sub_text = text.into();
        let new_clip_id =
            ClipId::new(format!("clip-{}", self.next_seq())).map_err(|e| e.to_string())?;
        let target_end = start_frame.get().saturating_add(duration.get());

        let (needs_new_layer, layer_ref, chosen_layer_id) = if let Some(lid) = layer_id {
            (
                false,
                crate::core::ids::LayerRef::Id(lid.clone()),
                lid.clone(),
            )
        } else if let Some(found) = find_unlocked_free_layer(
            &self.document,
            self.selected_layer_id.as_ref(),
            start_frame.get(),
            target_end,
        ) {
            (false, crate::core::ids::LayerRef::Id(found.clone()), found)
        } else {
            let new_lid =
                LayerId::new(format!("layer-{}", self.next_seq())).map_err(|e| e.to_string())?;
            (
                true,
                crate::core::ids::LayerRef::Id(new_lid.clone()),
                new_lid,
            )
        };

        let mut cmds = Vec::new();
        if needs_new_layer {
            let idx = self.document.layers.len() + 1;
            cmds.push(EditCommand::AddLayer {
                temp_id: None,
                id: Some(chosen_layer_id.clone()),
                name: format!("Track {idx}"),
                insert_index: None,
            });
        }
        cmds.push(EditCommand::AddClip {
            temp_id: None,
            id: Some(new_clip_id.clone()),
            layer_id: layer_ref,
            clip: Box::new(NewClipSpec {
                name: "字幕".to_owned(),
                timeline_start: start_frame,
                duration,
                effect_offset_frames: 0,
                effect_duration_frames: None,
                midpoints: Vec::new(),
                kind: NewClipKind::Subtitle(
                    crate::media::visual_properties::SubtitleProperties::new(sub_text),
                ),
            }),
        });
        let batch = EditCommand::Batch { commands: cmds };
        let _ = self.dispatch_command(&batch)?;
        self.select_single_clip(Some(new_clip_id.clone()));
        Ok(new_clip_id)
    }

    /// 指定名のトラックレイヤーを末尾に追加して選択する。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn add_layer(&mut self, name: impl Into<String>) -> Result<LayerId, String> {
        let new_id =
            LayerId::new(format!("layer-{}", self.next_seq())).map_err(|e| e.to_string())?;
        let cmd = EditCommand::AddLayer {
            temp_id: None,
            id: Some(new_id.clone()),
            name: name.into(),
            insert_index: None,
        };
        let _ = self.dispatch_command(&cmd)?;
        self.selected_layer_id = Some(new_id.clone());
        Ok(new_id)
    }

    /// 指定レイヤーを削除する。
    ///
    /// # Errors
    ///
    /// レイヤーが存在しないか削除不可時。
    pub fn remove_layer(&mut self, layer_id: &LayerId) -> Result<(), String> {
        let _ = self.dispatch_command(&EditCommand::RemoveLayer {
            layer_id: layer_id.clone(),
        })?;
        Ok(())
    }

    /// 指定レイヤー名を変更する。
    ///
    /// # Errors
    ///
    /// レイヤーが存在しないか変更失敗時。
    pub fn rename_layer(
        &mut self,
        layer_id: &LayerId,
        name: impl Into<String>,
    ) -> Result<(), String> {
        let _ = self.dispatch_command(&EditCommand::SetLayerFlags {
            layer_id: crate::core::ids::LayerRef::Id(layer_id.clone()),
            name: Some(name.into()),
            visible: None,
            muted: None,
            locked: None,
        })?;
        Ok(())
    }

    /// 指定レイヤーの可視フラグ (`visible`) を反転する。
    ///
    /// # Errors
    ///
    /// レイヤーが存在しない場合。
    pub fn toggle_layer_visible(&mut self, layer_id: &LayerId) -> Result<(), String> {
        let cur = self
            .document
            .find_layer(layer_id)
            .ok_or_else(|| format!("レイヤー `{layer_id}` が見つかりません"))?
            .visible;
        let _ = self.dispatch_command(&EditCommand::SetLayerFlags {
            layer_id: crate::core::ids::LayerRef::Id(layer_id.clone()),
            name: None,
            visible: Some(!cur),
            muted: None,
            locked: None,
        })?;
        Ok(())
    }

    /// 指定レイヤーのロックフラグ (`locked`) を反転する。
    ///
    /// # Errors
    ///
    /// レイヤーが存在しない場合。
    pub fn toggle_layer_locked(&mut self, layer_id: &LayerId) -> Result<(), String> {
        let cur = self
            .document
            .find_layer(layer_id)
            .ok_or_else(|| format!("レイヤー `{layer_id}` が見つかりません"))?
            .locked;
        let _ = self.dispatch_command(&EditCommand::SetLayerFlags {
            layer_id: crate::core::ids::LayerRef::Id(layer_id.clone()),
            name: None,
            visible: None,
            muted: None,
            locked: Some(!cur),
        })?;
        Ok(())
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_id_seq;
        self.next_id_seq = self.next_id_seq.saturating_add(1);
        seq
    }

    fn allocate_asset_id(&mut self) -> Result<AssetId, String> {
        loop {
            let candidate = AssetId::new(format!("asset-{}", self.next_seq()))
                .map_err(|err| err.to_string())?;
            if self.document.find_asset(&candidate).is_none() {
                return Ok(candidate);
            }
        }
    }

    fn allocate_layer_id(&mut self) -> Result<LayerId, String> {
        loop {
            let candidate = LayerId::new(format!("layer-{}", self.next_seq()))
                .map_err(|err| err.to_string())?;
            if self.document.find_layer(&candidate).is_none() {
                return Ok(candidate);
            }
        }
    }

    fn resolve_drop_target_layer(
        &mut self,
        requested: Option<&LayerId>,
    ) -> Result<(LayerId, Option<EditCommand>), String> {
        if let Some(lid) = requested {
            let layer = self
                .document
                .find_layer(lid)
                .ok_or_else(|| format!("指定レイヤー `{lid}` が見つかりません"))?;
            if layer.locked {
                return Err(format!("トラック `{}` はロックされています", layer.name));
            }
            return Ok((lid.clone(), None));
        }
        if let Some(lid) = &self.selected_layer_id
            && self.document.find_layer(lid).is_some_and(|l| !l.locked)
        {
            return Ok((lid.clone(), None));
        }
        if let Some(unlocked) = self.document.layers.iter().find(|l| !l.locked) {
            return Ok((unlocked.id.clone(), None));
        }
        if let Some(first) = self.document.layers.first() {
            return Err(format!("トラック `{}` はロックされています", first.name));
        }
        let new_id = self.allocate_layer_id()?;
        let add_layer = EditCommand::AddLayer {
            temp_id: None,
            id: Some(new_id.clone()),
            name: "Track 1".to_owned(),
            insert_index: None,
        };
        Ok((new_id, Some(add_layer)))
    }

    fn collect_layer_intervals(&self, layer_id: &LayerId) -> Vec<(u64, u64)> {
        self.document
            .find_layer(layer_id)
            .map(|layer| {
                layer
                    .clips
                    .iter()
                    .map(|c| {
                        let start = c.timeline_start.get();
                        (start, start.saturating_add(c.duration.get()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn prepare_media_import(&self, path: &Path) -> Result<PreparedMediaImport, String> {
        if !path.exists() {
            return Err(format!("ファイルが見つかりません: {}", path.display()));
        }
        if !path.is_file() {
            return Err(format!(
                "フォルダや特殊ファイルは追加できません: {}",
                path.display()
            ));
        }
        let ext_kind = classify_extension(path);
        let project_dir = self
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty());
        let asset_path = build_asset_path(path, project_dir);
        let display_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Media")
            .to_owned();

        let (asset_kind, clip_frames, clip_kind) = match ext_kind {
            MediaExtensionKind::Image => self.prepare_image_import(path)?,
            MediaExtensionKind::Audio => self.prepare_audio_import(path)?,
            MediaExtensionKind::Video => self.prepare_video_import(path)?,
        };
        Ok(PreparedMediaImport {
            display_name,
            asset_path,
            asset_kind,
            clip_frames,
            clip_kind,
            ext_kind,
            source_path: path.to_path_buf(),
        })
    }

    fn ensure_target_layer_id(&mut self) -> Result<LayerId, String> {
        if let Some(lid) = &self.selected_layer_id
            && self.document.find_layer(lid).is_some()
        {
            return Ok(lid.clone());
        }
        if let Some(first) = self.document.layers.first() {
            return Ok(first.id.clone());
        }
        let new_id =
            LayerId::new(format!("layer-{}", self.next_seq())).map_err(|e| e.to_string())?;
        let layer_count = self.document.layers.len() + 1;
        let _ = self
            .dispatcher
            .dispatch(
                &mut self.document,
                &EditCommand::AddLayer {
                    temp_id: None,
                    id: Some(new_id.clone()),
                    name: format!("Track {layer_count}"),
                    insert_index: None,
                },
            )
            .map_err(|err| err.to_string())?;
        Ok(new_id)
    }

    fn layer_end_frame(&self, layer_id: &LayerId) -> u64 {
        self.document
            .find_layer(layer_id)
            .and_then(|l| {
                l.clips
                    .iter()
                    .filter_map(|c| c.timeline_end().ok().map(FrameIndex::get))
                    .max()
            })
            .unwrap_or(0)
    }

    fn prepare_image_import(
        &self,
        path: &Path,
    ) -> Result<(AssetKind, FrameCount, PreparedClipKind), String> {
        let info =
            crate::media::image_loader::inspect_image_file(path).map_err(|e| e.to_string())?;
        let frames_count = self.document.fps.numerator().max(1);
        let duration = FrameCount::new(u64::from(frames_count) * 5).map_err(|e| e.to_string())?;
        Ok((
            AssetKind::Image {
                width: info.width,
                height: info.height,
            },
            duration,
            PreparedClipKind::Image,
        ))
    }

    fn prepare_audio_import(
        &self,
        path: &Path,
    ) -> Result<(AssetKind, FrameCount, PreparedClipKind), String> {
        let dec = open_audio_decoder(path, BackendPreference::Auto).map_err(|e| e.to_string())?;
        let (source_duration, frame_count) =
            quantize_source_duration_and_frames(dec.duration_seconds(), 1.0, self.document.fps)?;
        Ok((
            AssetKind::Audio {
                sample_rate_hz: dec.sample_rate(),
                channels: dec.channels(),
                source_duration,
            },
            frame_count,
            PreparedClipKind::Audio,
        ))
    }

    fn prepare_video_import(
        &self,
        path: &Path,
    ) -> Result<(AssetKind, FrameCount, PreparedClipKind), String> {
        let info = probe_media(path, BackendPreference::Auto).map_err(|e| e.to_string())?;
        let (source_duration, frame_count) =
            quantize_source_duration_and_frames(info.duration_seconds, 2.0, self.document.fps)?;
        let default_fps = info
            .frame_rate_num
            .zip(info.frame_rate_den)
            .and_then(|(n, d)| FrameRate::new(n.max(1), d.max(1)).ok());
        Ok((
            AssetKind::Video {
                has_audio: info.has_audio,
                default_fps,
                duration_frames: Some(frame_count),
                width: info.width,
                height: info.height,
                source_duration,
            },
            frame_count,
            PreparedClipKind::Video {
                has_audio: info.has_audio,
            },
        ))
    }

    fn register_decoded_resources_for_asset(
        &mut self,
        asset_id: &AssetId,
        path: &Path,
        kind: MediaExtensionKind,
    ) {
        match kind {
            MediaExtensionKind::Image => {
                if let Ok(buf) = crate::media::image_loader::load_image_file(path, Some(1920)) {
                    self.frame_cache
                        .insert(FrameCacheKey::new(asset_id.clone(), 0, 100), Arc::new(buf));
                }
            }
            MediaExtensionKind::Audio => {
                self.load_audio_into_mixer(asset_id, path);
            }
            MediaExtensionKind::Video => {
                if let Ok(dec) = open_video_decoder(path, BackendPreference::Auto) {
                    self.video_decoders.insert(asset_id.clone(), dec);
                }
                self.load_audio_into_mixer(asset_id, path);
            }
        }
    }

    fn load_audio_into_mixer(&mut self, asset_id: &AssetId, path: &Path) {
        if let Ok(mut dec) = open_audio_decoder(path, BackendPreference::Auto)
            && let Ok(pcm) = dec.decode_all_interleaved()
            && let Ok(resampled) = resample_interleaved_pcm(&pcm, OUTPUT_SAMPLE_RATE_HZ)
        {
            self.mixer
                .register_pcm(asset_id.clone(), Arc::new(resampled));
        }
    }

    fn preload_available_media(&mut self, project_dir: Option<&Path>) {
        let assets: Vec<Asset> = self.document.assets.clone();
        for asset in assets {
            if asset.status != AssetStatus::Available {
                continue;
            }
            if let Some(real_path) = resolve_existing_asset_path(&asset.path, project_dir) {
                let ext_kind = match asset.kind {
                    AssetKind::Image { .. } => MediaExtensionKind::Image,
                    AssetKind::Audio { .. } => MediaExtensionKind::Audio,
                    AssetKind::Video { .. } => MediaExtensionKind::Video,
                };
                self.register_decoded_resources_for_asset(&asset.id, &real_path, ext_kind);
            }
        }
    }

    fn ensure_decoded_frames_for_playhead(&mut self, playhead: FrameIndex) {
        let project_dir = self
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty());

        let mut needed: Vec<(AssetId, u64)> = Vec::new();
        for layer in &self.document.layers {
            if !layer.visible {
                continue;
            }
            for clip in &layer.clips {
                let start = clip.timeline_start.get();
                let end = start.saturating_add(clip.duration.get());
                if playhead.get() < start || playhead.get() >= end {
                    continue;
                }
                if let ClipKind::Video {
                    asset_id,
                    source_in,
                    playback_rate,
                    ..
                } = &clip.kind
                {
                    let local = playhead.get().saturating_sub(start);
                    #[allow(clippy::cast_precision_loss)]
                    let local_sec = (local as f64) / self.document.fps.as_f64();
                    let media_sec = source_in.as_seconds_f64() + local_sec * playback_rate.as_f64();
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let src_idx = (media_sec * self.document.fps.as_f64()).max(0.0).round() as u64;
                    needed.push((asset_id.clone(), src_idx));
                }
            }
        }

        for (asset_id, src_idx) in needed {
            let key = FrameCacheKey::new(asset_id.clone(), src_idx, 100);
            if self.frame_cache.get(&key).is_some() {
                continue;
            }
            if !self.video_decoders.contains_key(&asset_id)
                && let Some(asset) = self.document.find_asset(&asset_id)
                && asset.status == AssetStatus::Available
                && let Some(real_path) = resolve_existing_asset_path(&asset.path, project_dir)
                && let Ok(dec) = open_video_decoder(&real_path, BackendPreference::Auto)
            {
                self.video_decoders.insert(asset_id.clone(), dec);
            }
            if let Some(decoder) = self.video_decoders.get_mut(&asset_id) {
                #[allow(clippy::cast_precision_loss)]
                let target_sec = (src_idx as f64) / self.document.fps.as_f64();
                if let Ok(Some(decoded)) = decoder.decode_frame_at_seconds(target_sec) {
                    self.frame_cache.insert(key, Arc::new(decoded.buffer));
                }
            }
        }
    }

    /// 書き出し設定ダイアログを開き、既定パスからプラグイン一覧を自動検出する。
    pub fn open_export_dialog(&mut self) {
        let registry = crate::export::registry::PluginRegistry::discover_default();
        let base_dir = self
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        self.export_dialog
            .open_with_registry(registry, base_dir.as_deref());
    }

    /// 書き出しダイアログの現在設定でバックグラウンドエンコードを開始する。
    ///
    /// # Errors
    ///
    /// プラグイン／プリセット未選択またはジョブ検証違反時にエラー文字列を返す。
    pub fn start_export_from_dialog(&mut self) -> Result<(), String> {
        if self.export_dialog.running_job.is_some() {
            return Err("すでに書き出しを実行中です".to_owned());
        }
        let plugin_id = self
            .export_dialog
            .selected_plugin()
            .map(|p| p.manifest.id.clone())
            .ok_or_else(|| "エンコーダプラグインが選択されていません".to_owned())?;
        let preset_id = self
            .export_dialog
            .selected_preset()
            .map(|p| p.id.clone())
            .ok_or_else(|| "書き出しプリセットが選択されていません".to_owned())?;
        let (width, height) = self.export_dialog.resolution.0.dimensions(&self.document);
        let options = self.export_dialog.merged_options();
        let out_path = PathBuf::from(self.export_dialog.output_path.trim());
        let mut request = crate::export::runner::ExportJobRequest::full_project(
            &self.document,
            plugin_id,
            preset_id,
            out_path,
            options,
        );
        request.width = width;
        request.height = height;

        let project_dir = self
            .project_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let mut sources = crate::export::runner::ExportMediaSources::new(project_dir);
        sources.seed_from_cache_and_mixer(&self.frame_cache, &self.mixer);

        self.export_dialog.last_error = None;
        self.export_dialog.last_completed = None;
        let handle = crate::export::runner::spawn_export_job(
            self.document.clone(),
            self.export_dialog.registry.clone(),
            request,
            sources,
        );
        self.export_dialog.running_job = Some(handle);
        "動画の書き出しを開始しました...".clone_into(&mut self.status_message);
        Ok(())
    }

    /// 実行中の書き出しジョブへ中断通知を送る。
    pub fn cancel_active_export(&mut self) {
        self.export_dialog.cancel_active();
        "書き出しを中断しています...".clone_into(&mut self.status_message);
    }

    /// 非同期書き出しジョブの進捗・完了イベントを取り込む（更新時 `true`）。
    pub fn poll_export_status(&mut self) -> bool {
        let changed = self.export_dialog.poll_job();
        if changed {
            if let Some(outcome) = &self.export_dialog.last_completed {
                self.status_message = format!("書き出し完了: {}", outcome.output_path.display());
            } else if let Some(err) = &self.export_dialog.last_error {
                self.status_message = format!("書き出しエラー: {err}");
            }
        }
        changed
    }
}

#[derive(Clone, Copy)]
enum MediaExtensionKind {
    Image,
    Audio,
    Video,
}

#[derive(Clone, Copy)]
enum PreparedClipKind {
    Image,
    Audio,
    Video { has_audio: bool },
}

struct PreparedMediaImport {
    display_name: String,
    asset_path: AssetPath,
    asset_kind: AssetKind,
    clip_frames: FrameCount,
    clip_kind: PreparedClipKind,
    ext_kind: MediaExtensionKind,
    source_path: PathBuf,
}

fn find_free_interval_start(
    occupied: &[(u64, u64)],
    desired_start: u64,
    duration_frames: u64,
) -> u64 {
    let mut candidate = desired_start;
    for _ in 0..=occupied.len() {
        let end = candidate.saturating_add(duration_frames);
        if let Some((_, ex_end)) = occupied
            .iter()
            .copied()
            .filter(|&(ex_start, ex_end)| candidate < ex_end && ex_start < end)
            .max_by_key(|&(_, ex_end)| ex_end)
        {
            candidate = ex_end.max(candidate.saturating_add(1));
        } else {
            return candidate;
        }
    }
    candidate
}

fn classify_extension(path: &Path) -> MediaExtensionKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "webp" | "gif" => MediaExtensionKind::Image,
        "wav" | "flac" | "ogg" | "mp3" | "aac" | "m4a" => MediaExtensionKind::Audio,
        _ => MediaExtensionKind::Video,
    }
}

fn build_new_clip_kind(asset_id: &AssetId, prepared: PreparedClipKind) -> NewClipKind {
    match prepared {
        PreparedClipKind::Image => NewClipKind::Image {
            asset_id: AssetRef::Id(asset_id.clone()),
            visual: crate::media::visual_properties::VisualProperties::default(),
        },
        PreparedClipKind::Audio => NewClipKind::Audio {
            asset_id: AssetRef::Id(asset_id.clone()),
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            audio: AudioProperties::default(),
        },
        PreparedClipKind::Video { has_audio } => NewClipKind::Video {
            asset_id: AssetRef::Id(asset_id.clone()),
            source_in: RationalDuration::ZERO,
            playback_rate: PlaybackRate::NORMAL,
            preserve_pitch: true,
            visual: crate::media::visual_properties::VisualProperties::default(),
            audio: has_audio.then(AudioProperties::default),
        },
    }
}

fn preview_dimensions(doc: &ProjectDocument) -> (u32, u32) {
    let w = doc.resolution.width.get();
    let h = doc.resolution.height.get();
    if w <= PREVIEW_CANVAS_MAX_WIDTH {
        return (w, h);
    }
    let scaled_h = (u64::from(h) * u64::from(PREVIEW_CANVAS_MAX_WIDTH)) / u64::from(w);
    let ph = u32::try_from(scaled_h.max(1)).unwrap_or(PREVIEW_CANVAS_MAX_WIDTH);
    (PREVIEW_CANVAS_MAX_WIDTH, ph)
}

fn format_drop_import_status(
    prepared: &[PreparedMediaImport],
    skipped_errors: &[String],
) -> String {
    if prepared.len() == 1 && skipped_errors.is_empty() {
        format!("素材を追加しました: {}", prepared[0].display_name)
    } else if skipped_errors.is_empty() {
        format!("{} 件の素材をタイムラインに追加しました", prepared.len())
    } else {
        format!(
            "{} 件の素材を追加（{} 件スキップ: {}）",
            prepared.len(),
            skipped_errors.len(),
            skipped_errors[0]
        )
    }
}

fn quantize_source_duration_and_frames(
    duration_secs: Option<f64>,
    default_secs: f64,
    fps: FrameRate,
) -> Result<(RationalDuration, FrameCount), String> {
    let raw_sec = duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(default_secs)
        .max(0.1);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = (raw_sec * 1_000.0).round() as u64;
    let initial_source = RationalDuration::from_millis(millis.max(100));

    // ドキュメント検証 (`frames_to_project_seconds(frames, fps) <= source_duration`) を常に満たすよう切り捨て計算する
    let scaled_num = u128::from(initial_source.numerator()) * u128::from(fps.numerator());
    let scaled_den = u128::from(initial_source.denominator()) * u128::from(fps.denominator());
    let floored_frames = u64::try_from(scaled_num / scaled_den.max(1)).unwrap_or(u64::MAX);

    if floored_frames == 0 {
        let min_source = frames_to_project_seconds(1, fps).map_err(|e| e.to_string())?;
        let adjusted = initial_source.max(min_source);
        Ok((adjusted, FrameCount::ONE))
    } else {
        let frame_count = FrameCount::new(floored_frames).map_err(|e| e.to_string())?;
        Ok((initial_source, frame_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "sea_movie_{prefix}_{}_{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_test_png(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        img.save(path)?;
        Ok(())
    }

    fn write_test_wav_mono_i16(
        path: &Path,
        sample_rate_hz: u32,
        sample_count: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let data_bytes = u32::try_from(sample_count * 2)?;
        let riff_size = data_bytes + 36;
        let byte_rate = sample_rate_hz * 2;
        let mut buf = Vec::with_capacity(44 + usize::try_from(data_bytes)?);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16_u32.to_le_bytes());
        buf.extend_from_slice(&1_u16.to_le_bytes());
        buf.extend_from_slice(&1_u16.to_le_bytes());
        buf.extend_from_slice(&sample_rate_hz.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&2_u16.to_le_bytes());
        buf.extend_from_slice(&16_u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        buf.resize(44 + usize::try_from(data_bytes)?, 0);
        std::fs::write(path, buf)?;
        Ok(())
    }

    #[test]
    fn right_panel_width_uses_one_fifth_default_and_clamps_bounds() {
        // 1280px ウィンドウでの初期幅 = 1280 * 0.2 = 256px
        let default_w = clamp_right_panel_width(1280.0, None);
        assert!((default_w - 256.0).abs() < 1e-4);

        // 下限 240px・上限 50% (640px) でクランプ
        assert!((clamp_right_panel_width(1280.0, Some(50.0)) - 240.0).abs() < 1e-4);
        assert!((clamp_right_panel_width(1280.0, Some(999.0)) - 640.0).abs() < 1e-4);
    }

    #[test]
    fn playback_state_machine_and_missing_media_flag_update_correctly()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = AppController::new();
        assert_eq!(controller.playback_state, PlaybackState::Stopped);
        assert!(!controller.has_media_offline_at_playhead);

        let (_asset, _clip) = controller.insert_missing_placeholder_clip(
            "OfflineClip",
            Path::new("C:/none.mkv"),
            60,
        )?;
        assert!(controller.has_media_offline_at_playhead);

        controller.play();
        assert_eq!(controller.playback_state, PlaybackState::Playing);
        controller.tick_playback(0.5); // 60fps * 0.5s = 30 frames
        assert_eq!(controller.clock.current_frame(), FrameIndex::new(30));

        controller.pause();
        assert_eq!(controller.playback_state, PlaybackState::Paused);
        controller.seek_to_frame(FrameIndex::new(15));
        assert_eq!(controller.clock.current_frame(), FrameIndex::new(15));
        assert_eq!(controller.playback_state, PlaybackState::Paused);

        controller.stop();
        assert_eq!(controller.playback_state, PlaybackState::Stopped);
        assert_eq!(controller.clock.current_frame(), FrameIndex::ZERO);
        Ok(())
    }

    #[test]
    fn drop_import_on_empty_timeline_creates_track_and_skips_non_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDirGuard::create("dnd_empty")?;
        let img1 = temp.path().join("clip_a.png");
        let folder = temp.path().join("not_a_file");
        write_test_png(&img1)?;
        std::fs::create_dir_all(&folder)?;

        let mut controller = AppController::new();
        assert_eq!(controller.document.layers.first(), None);

        let added = controller.import_media_files_at_drop(
            &[img1, folder],
            None,
            Some(FrameIndex::new(30)),
        )?;
        assert_eq!(added.len(), 1);
        assert_eq!(controller.document.layers.len(), 1);
        assert_eq!(controller.document.layers[0].name, "Track 1");
        assert_eq!(controller.document.layers[0].clips.len(), 1);
        assert_eq!(
            controller.document.layers[0].clips[0].timeline_start,
            FrameIndex::new(30)
        );
        assert!(
            controller.status_message.contains("スキップ"),
            "unexpected status_message: {}",
            controller.status_message
        );

        assert!(controller.undo()?);
        assert_eq!(controller.document.layers.first(), None);
        Ok(())
    }

    #[test]
    fn drop_import_multi_collision_snaps_undoes_and_rejects_locked_layer()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDirGuard::create("dnd_multi")?;
        let img1 = temp.path().join("clip_a.png");
        let img2 = temp.path().join("clip_b.png");
        write_test_png(&img1)?;
        write_test_png(&img2)?;

        let mut controller = AppController::new();
        let (_placeholder_asset, _placeholder_clip) =
            controller.insert_missing_placeholder_clip("Existing", &img1, 60)?;
        let layer1_id = controller.document.layers[0].id.clone();

        let multi_added = controller.import_media_files_at_drop(
            &[img1.clone(), img2],
            Some(&layer1_id),
            Some(FrameIndex::new(20)),
        )?;
        assert_eq!(multi_added.len(), 2);
        let layer1 = controller
            .document
            .find_layer(&layer1_id)
            .ok_or("Track 1 missing")?;
        assert_eq!(layer1.clips.len(), 3);
        assert_eq!(layer1.clips[1].timeline_start, FrameIndex::new(60));
        let clip1_end = layer1.clips[1].timeline_start.get() + layer1.clips[1].duration.get();
        assert_eq!(clip1_end, 360);
        assert_eq!(layer1.clips[2].timeline_start, FrameIndex::new(360));

        assert!(controller.undo()?);
        let layer1_after_undo = controller
            .document
            .find_layer(&layer1_id)
            .ok_or("Track 1 missing after undo")?;
        assert_eq!(layer1_after_undo.clips.len(), 1);

        assert!(controller.redo()?);
        let layer1_after_redo = controller
            .document
            .find_layer(&layer1_id)
            .ok_or("Track 1 missing after redo")?;
        assert_eq!(layer1_after_redo.clips.len(), 3);
        assert_eq!(
            layer1_after_redo.clips[1].timeline_start,
            FrameIndex::new(60)
        );
        assert_eq!(
            layer1_after_redo.clips[2].timeline_start,
            FrameIndex::new(360)
        );

        let layer2_id = controller.add_empty_layer()?;
        controller.seek_to_frame(FrameIndex::new(120));
        controller.snap_config.enabled = true;
        let snapped_added = controller.import_media_files_at_drop(
            std::slice::from_ref(&img1),
            Some(&layer2_id),
            Some(FrameIndex::new(119)),
        )?;
        assert_eq!(snapped_added.len(), 1);
        let layer2 = controller
            .document
            .find_layer(&layer2_id)
            .ok_or("Track 2 missing")?;
        assert_eq!(layer2.clips.len(), 1);
        assert_eq!(layer2.clips[0].timeline_start, FrameIndex::new(120));

        let _ = controller.dispatch_command(&EditCommand::SetLayerFlags {
            layer_id: LayerRef::Id(layer2_id.clone()),
            name: None,
            visible: None,
            muted: None,
            locked: Some(true),
        })?;
        let locked_res = controller.import_media_files_at_drop(
            std::slice::from_ref(&img1),
            Some(&layer2_id),
            Some(FrameIndex::new(500)),
        );
        match locked_res {
            Ok(_) => return Err("locked layer drop unexpectedly succeeded".into()),
            Err(err) => assert!(err.contains("ロック"), "unexpected locked err: {err}"),
        }
        let layer2_after_locked = controller
            .document
            .find_layer(&layer2_id)
            .ok_or("Track 2 missing after locked drop")?;
        assert_eq!(layer2_after_locked.clips.len(), 1);

        Ok(())
    }

    #[test]
    fn quantize_source_duration_never_exceeds_asset_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let (src, frames) =
            quantize_source_duration_and_frames(Some(2.717), 2.0, FrameRate::FPS_60)?;
        assert_eq!(src, RationalDuration::from_millis(2_717));
        assert_eq!(frames.get(), 163);
        assert!(frames_to_project_seconds(frames.get(), FrameRate::FPS_60)? <= src);

        let (ntsc_src, ntsc_frames) =
            quantize_source_duration_and_frames(Some(2.717), 2.0, FrameRate::FPS_29_97)?;
        assert!(frames_to_project_seconds(ntsc_frames.get(), FrameRate::FPS_29_97)? <= ntsc_src);

        let (fallback_src, fallback_frames) =
            quantize_source_duration_and_frames(Some(f64::NAN), 2.0, FrameRate::FPS_60)?;
        assert_eq!(fallback_src, RationalDuration::from_millis(2_000));
        assert_eq!(fallback_frames.get(), 120);

        let fps_1 = FrameRate::new(1, 1)?;
        let (tiny_src, tiny_frames) = quantize_source_duration_and_frames(Some(0.1), 1.0, fps_1)?;
        assert_eq!(tiny_frames.get(), 1);
        assert!(frames_to_project_seconds(tiny_frames.get(), fps_1)? <= tiny_src);
        Ok(())
    }

    #[test]
    fn drop_fractional_duration_wav_2717ms_validates_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDirGuard::create("dnd_2717ms")?;
        let wav_path = temp.path().join("sample_2717ms.wav");
        // 48_000 Hz * 2.717 s = 130_416 samples
        write_test_wav_mono_i16(&wav_path, 48_000, 130_416)?;

        let mut controller = AppController::new();
        let added =
            controller.import_media_files_at_drop(&[wav_path], None, Some(FrameIndex::ZERO))?;
        assert_eq!(added.len(), 1);
        controller.document.validate()?;
        assert_eq!(controller.document.layers.len(), 1);
        assert_eq!(controller.document.layers[0].clips.len(), 1);
        assert_eq!(controller.document.layers[0].clips[0].duration.get(), 163);
        Ok(())
    }

    #[test]
    fn phase_d1_selection_range_midpoints_subtitle_and_layer_controls_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = AppController::new();
        controller.seek_to_frame(FrameIndex::new(150));
        assert_eq!(controller.clock.current_frame().get(), 150);

        controller.set_selection_range_start_at_playhead()?;
        assert_eq!(
            controller.document.selection_range,
            Some(crate::timeline::document::FrameRange {
                start: FrameIndex::new(150),
                end: FrameIndex::new(151),
            })
        );
        controller.seek_to_frame(FrameIndex::new(100));
        controller.set_selection_range_end_at_playhead()?;
        assert_eq!(
            controller.document.selection_range,
            Some(crate::timeline::document::FrameRange {
                start: FrameIndex::new(100),
                end: FrameIndex::new(101),
            })
        );

        let sub_id =
            controller.add_subtitle_clip_at(None, FrameIndex::ZERO, FrameCount::new(120)?, "Hi")?;
        assert_eq!(
            controller.effective_selected_clip_ids(),
            vec![sub_id.clone()]
        );
        controller.seek_to_frame(FrameIndex::new(40));
        controller.add_midpoint_at_playhead()?;
        assert_eq!(
            controller
                .document
                .find_clip(&sub_id)
                .ok_or("missing sub")?
                .1
                .midpoints,
            vec![40]
        );

        let lid = controller.document.layers[0].id.clone();
        controller.toggle_layer_visible(&lid)?;
        assert!(!controller.document.layers[0].visible);
        controller.toggle_layer_muted(&lid)?;
        assert!(controller.document.layers[0].muted);
        controller.toggle_layer_locked(&lid)?;
        assert!(controller.document.layers[0].locked);

        controller.undo()?;
        assert!(!controller.document.layers[0].locked);
        Ok(())
    }
}
