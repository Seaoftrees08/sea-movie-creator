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
use crate::core::time::{FrameCount, FrameIndex, PlaybackRate, RationalDuration};
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
const MIN_RIGHT_PANEL_WIDTH: f32 = 220.0;
const DEFAULT_RIGHT_PANEL_RATIO: f32 = 0.2;
const MAX_RIGHT_PANEL_RATIO: f32 = 0.5;
const MIN_TIMELINE_HEIGHT: f32 = 140.0;
const DEFAULT_TIMELINE_RATIO: f32 = 0.3;
const MAX_TIMELINE_RATIO: f32 = 0.6;

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
    /// 現在の選択クリップ ID。
    pub selected_clip_id: Option<ClipId>,
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
            selected_clip_id,
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
        self.selected_clip_id = None;
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
        self.selected_layer_id = self.document.layers.first().map(|l| l.id.clone());
        self.selected_clip_id = self
            .document
            .layers
            .first()
            .and_then(|l| l.clips.first())
            .map(|c| c.id.clone());
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
        if !path.exists() {
            return Err(format!("ファイルが見つかりません: {}", path.display()));
        }
        let kind_category = classify_extension(path);
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

        let (asset_kind, clip_frames, clip_kind) = match kind_category {
            MediaExtensionKind::Image => self.prepare_image_import(path)?,
            MediaExtensionKind::Audio => self.prepare_audio_import(path)?,
            MediaExtensionKind::Video => self.prepare_video_import(path)?,
        };

        let asset_id = self.allocate_asset_id()?;
        let layer_id = self.ensure_target_layer_id()?;
        let start_frame = FrameIndex::new(self.layer_end_frame(&layer_id));

        let clip_kind_cmd = build_new_clip_kind(&asset_id, clip_kind);
        let cmd = EditCommand::Batch {
            commands: vec![
                EditCommand::AddAsset {
                    temp_id: None,
                    id: Some(asset_id.clone()),
                    name: display_name.clone(),
                    path: asset_path,
                    kind: asset_kind,
                },
                EditCommand::AddClip {
                    temp_id: Some("imported_clip".to_owned()),
                    id: None,
                    layer_id: LayerRef::Id(layer_id.clone()),
                    clip: Box::new(NewClipSpec {
                        name: display_name.clone(),
                        timeline_start: start_frame,
                        duration: clip_frames,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: clip_kind_cmd,
                    }),
                },
            ],
        };

        let report = self
            .dispatcher
            .dispatch(&mut self.document, &cmd)
            .map_err(|err| err.to_string())?;
        self.register_decoded_resources_for_asset(&asset_id, path, kind_category);
        self.selected_layer_id = Some(layer_id);
        if let Some(new_clip) = report.created_clips.get("imported_clip") {
            self.selected_clip_id = Some(new_clip.clone());
        }
        self.sync_shared_state();
        self.dirty_preview = true;
        self.refresh_preview_now();
        self.status_message = format!("素材を追加しました: {display_name}");
        Ok(asset_id)
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
        self.selected_layer_id = Some(layer_id);
        self.selected_clip_id = Some(clip_id.clone());
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
        self.timeline_px_per_frame = (self.timeline_px_per_frame * factor).clamp(0.5, 24.0);
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
        let max_frame = self.document.total_duration_frames();
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
        if let Some(cid) = &self.selected_clip_id
            && self.document.find_clip(cid).is_none()
        {
            self.selected_clip_id = self
                .document
                .layers
                .first()
                .and_then(|l| l.clips.first())
                .map(|c| c.id.clone());
        }
        if let Some(lid) = &self.selected_layer_id
            && self.document.find_layer(lid).is_none()
        {
            self.selected_layer_id = self.document.layers.first().map(|l| l.id.clone());
        }
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
        let dur_sec = dec.duration_seconds().unwrap_or(1.0).max(0.1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let millis = (dur_sec * 1_000.0).round() as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = (dur_sec * self.document.fps.as_f64()).ceil().max(1.0) as u64;
        Ok((
            AssetKind::Audio {
                sample_rate_hz: dec.sample_rate(),
                channels: dec.channels(),
                source_duration: RationalDuration::from_millis(millis.max(100)),
            },
            FrameCount::new(frames).map_err(|e| e.to_string())?,
            PreparedClipKind::Audio,
        ))
    }

    fn prepare_video_import(
        &self,
        path: &Path,
    ) -> Result<(AssetKind, FrameCount, PreparedClipKind), String> {
        let info = probe_media(path, BackendPreference::Auto).map_err(|e| e.to_string())?;
        let dur_sec = info.duration_seconds.unwrap_or(2.0).max(0.1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let millis = (dur_sec * 1_000.0).round() as u64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = (dur_sec * self.document.fps.as_f64()).ceil().max(1.0) as u64;
        let frame_count = FrameCount::new(frames).map_err(|e| e.to_string())?;
        let default_fps = info
            .frame_rate_num
            .zip(info.frame_rate_den)
            .and_then(|(n, d)| crate::core::time::FrameRate::new(n.max(1), d.max(1)).ok());
        Ok((
            AssetKind::Video {
                has_audio: info.has_audio,
                default_fps,
                duration_frames: Some(frame_count),
                width: info.width,
                height: info.height,
                source_duration: RationalDuration::from_millis(millis.max(100)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_panel_width_uses_one_fifth_default_and_clamps_bounds() {
        // 1280px ウィンドウでの初期幅 = 1280 * 0.2 = 256px
        let default_w = clamp_right_panel_width(1280.0, None);
        assert!((default_w - 256.0).abs() < 1e-4);

        // 下限 220px・上限 50% (640px) でクランプ
        assert!((clamp_right_panel_width(1280.0, Some(50.0)) - 220.0).abs() < 1e-4);
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
}
