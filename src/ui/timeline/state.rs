//! タイムラインパネルの座標変換・ズーム・ドラッグ・範囲選択などの純粋状態と操作ロジック。

use crate::core::ids::{AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use crate::core::time::{FrameCount, FrameIndex, FrameRate};
use crate::timeline::command::{EditCommand, NewClipKind, NewClipSpec};
use crate::timeline::document::{
    Clip, ClipKind, FrameRange, Layer, ProjectDocument, TimelineLengthMode,
};
use crate::timeline::operations::{ClipMovePlan, build_batch_move_clips_command};
use crate::timeline::snap::{
    SnapConfig, collect_snap_anchors, is_layer_interval_free, snap_clip_move, snap_frame_edge,
};
use crate::ui::app_controller::{
    AppController, MAX_TIMELINE_PX_PER_FRAME, MIN_TIMELINE_PX_PER_FRAME, TimelineRowDensity,
};

/// トラック左端ヘッダ列の固定幅 (px)。
pub const TRACK_HEADER_WIDTH: f32 = 136.0;
/// タイムライン上部ヘッダ行の高さ (px)。
pub const TIMELINE_HEADER_HEIGHT_PX: f32 = 24.0;
/// ルーラー領域の高さ (px)。
pub const RULER_HEIGHT_PX: f32 = 28.0;
/// 水平スクロールバー領域の高さ (px)。
pub const H_SCROLLBAR_HEIGHT_PX: f32 = 14.0;
/// `Ctrl+マウスダウン` をクリックか複製ドラッグか判定する閾値距離 (px)。
pub const CTRL_DRAG_THRESHOLD_PX: f32 = 4.0;
/// クリップ左右端のトリム判定幅 (px)。
pub const TRIM_EDGE_HIT_PX: f32 = 6.0;
/// 総尺境界ハンドルのヒット半径 (px)。
pub const TIMELINE_END_HIT_PX: f32 = 7.0;
/// 中間点ひし形ハンドルのヒット半径 (px)。
pub const MIDPOINT_HIT_RADIUS_PX: f32 = 7.0;

/// トラック行密度に応じた行高さ (px) を返す。
#[must_use]
pub const fn row_height_px(density: TimelineRowDensity) -> f32 {
    match density {
        TimelineRowDensity::Compact => 22.0,
        TimelineRowDensity::Medium => 30.0,
        TimelineRowDensity::Large => 44.0,
    }
}

/// `[0.0, 1.0]` のズームバー比率を対数スケールで `px_per_frame` (`0.2..=32.0`) へ変換する。
#[must_use]
pub fn px_per_frame_from_zoom_ratio(ratio: f32) -> f32 {
    let clamped = ratio.clamp(0.0, 1.0);
    let span = MAX_TIMELINE_PX_PER_FRAME / MIN_TIMELINE_PX_PER_FRAME;
    MIN_TIMELINE_PX_PER_FRAME * span.powf(clamped)
}

/// 現在の `px_per_frame` を `[0.0, 1.0]` のズームバー比率へ逆変換する。
#[must_use]
pub fn zoom_ratio_from_px_per_frame(px_per_frame: f32) -> f32 {
    let clamped = px_per_frame.clamp(MIN_TIMELINE_PX_PER_FRAME, MAX_TIMELINE_PX_PER_FRAME);
    let span = MAX_TIMELINE_PX_PER_FRAME / MIN_TIMELINE_PX_PER_FRAME;
    ((clamped / MIN_TIMELINE_PX_PER_FRAME).ln() / span.ln()).clamp(0.0, 1.0)
}

/// タイムライン上のフレーム番号をキャンバス左端基準のローカル X 座標 (px) へ変換する。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn canvas_x_from_frame(frame: u64, scroll_x_frames: u64, px_per_frame: f32) -> f32 {
    let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
    if frame >= scroll_x_frames {
        ((frame - scroll_x_frames) as f32) * safe_ppf
    } else {
        -((scroll_x_frames - frame) as f32) * safe_ppf
    }
}

/// クリップや区間のフレーム長を現在の拡大率における幅 (px) へ変換する。
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn canvas_width_for_duration(duration_frames: u64, px_per_frame: f32) -> f32 {
    let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
    (duration_frames as f32) * safe_ppf
}

/// キャンバス左端基準のローカル X 座標 (px) からフレーム番号を算出する。
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn frame_at_canvas_local_x(local_x: f32, scroll_x_frames: u64, px_per_frame: f32) -> u64 {
    let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
    let delta = (local_x / safe_ppf).max(0.0).round() as u64;
    scroll_x_frames.saturating_add(delta)
}

/// ウィンドウ座標 `X` からタイムラインレーン上のドロップ先フレーム位置を算出する。
///
/// 左端トラックヘッダ領域 (`x < TRACK_HEADER_WIDTH`) 上に落とされた場合は `None` を返す。
#[must_use]
pub fn frame_at_timeline_window_x(
    window_x: f32,
    scroll_x_frames: u64,
    px_per_frame: f32,
) -> Option<FrameIndex> {
    if window_x < TRACK_HEADER_WIDTH {
        return None;
    }
    let local_x = window_x - TRACK_HEADER_WIDTH;
    Some(FrameIndex::new(frame_at_canvas_local_x(
        local_x,
        scroll_x_frames,
        px_per_frame,
    )))
}

/// カーソル位置 (`cursor_canvas_x`) 上のフレームを画面上の同じ位置に保ったままズーム倍率を変更する。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn zoom_timeline_anchored(
    controller: &mut AppController,
    zoom_factor: f32,
    cursor_canvas_x: f32,
) {
    let old_ppf = controller
        .timeline_px_per_frame
        .clamp(MIN_TIMELINE_PX_PER_FRAME, MAX_TIMELINE_PX_PER_FRAME);
    let new_ppf =
        (old_ppf * zoom_factor).clamp(MIN_TIMELINE_PX_PER_FRAME, MAX_TIMELINE_PX_PER_FRAME);
    let anchor_x = cursor_canvas_x.max(0.0);
    let anchor_frame =
        controller.timeline_scroll_x_frames as f64 + (f64::from(anchor_x) / f64::from(old_ppf));
    let new_scroll = (anchor_frame - (f64::from(anchor_x) / f64::from(new_ppf)))
        .max(0.0)
        .round() as u64;
    controller.timeline_px_per_frame = new_ppf;
    controller.timeline_scroll_x_frames = new_scroll;
}

/// ズームバー比率 `[0.0, 1.0]` を基準アンカー付きで直接設定する。
pub fn set_timeline_zoom_ratio_anchored(
    controller: &mut AppController,
    ratio: f32,
    anchor_canvas_x: f32,
) {
    let target_ppf = px_per_frame_from_zoom_ratio(ratio);
    let factor = target_ppf
        / controller
            .timeline_px_per_frame
            .max(MIN_TIMELINE_PX_PER_FRAME);
    zoom_timeline_anchored(controller, factor, anchor_canvas_x);
}

/// タイムライン長入力モーダルの文字列（例: `300`, `300f`, `10s`, `00:00:10:00`）をフレーム数へ変換する。
///
/// # Errors
///
/// 不正書式または `0` フレームになる入力時。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn parse_timeline_length_input(raw: &str, fps: FrameRate) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("タイムライン長が空です".to_owned());
    }
    if trimmed.contains(':') {
        return parse_timecode_frames(trimmed, fps);
    }
    if let Some(secs_str) = trimmed.strip_suffix(['s', 'S']) {
        let secs: f64 = secs_str
            .trim()
            .parse()
            .map_err(|_| "秒数の数値解釈に失敗しました".to_owned())?;
        if !secs.is_finite() || secs <= 0.0 {
            return Err("秒数は正の数で指定してください".to_owned());
        }
        let frames = (secs * fps.as_f64()).round().max(1.0) as u64;
        return Ok(frames);
    }
    let digits = trimmed.trim_end_matches(['f', 'F']).trim();
    let frames: u64 = digits
        .parse()
        .map_err(|_| "フレーム数は整数で指定してください".to_owned())?;
    if frames == 0 {
        return Err("総尺は 1 フレーム以上で指定してください".to_owned());
    }
    Ok(frames)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn parse_timecode_frames(text: &str, fps: FrameRate) -> Result<u64, String> {
    let parts: Vec<&str> = text.split(':').collect();
    let nominal_fps = fps.as_f64().round().max(1.0) as u64;
    let total_frames = match parts.as_slice() {
        [mm, ss] => {
            let m: u64 = mm.trim().parse().map_err(|_| "分が不正です".to_owned())?;
            let s: u64 = ss.trim().parse().map_err(|_| "秒が不正です".to_owned())?;
            m.saturating_mul(60)
                .saturating_add(s)
                .saturating_mul(nominal_fps)
        }
        [hh, mm, ss] => {
            let h: u64 = hh.trim().parse().map_err(|_| "時が不正です".to_owned())?;
            let m: u64 = mm.trim().parse().map_err(|_| "分が不正です".to_owned())?;
            let s: u64 = ss.trim().parse().map_err(|_| "秒が不正です".to_owned())?;
            (h.saturating_mul(3600)
                .saturating_add(m.saturating_mul(60))
                .saturating_add(s))
            .saturating_mul(nominal_fps)
        }
        [hh, mm, ss, ff] => {
            let h: u64 = hh.trim().parse().map_err(|_| "時が不正です".to_owned())?;
            let m: u64 = mm.trim().parse().map_err(|_| "分が不正です".to_owned())?;
            let s: u64 = ss.trim().parse().map_err(|_| "秒が不正です".to_owned())?;
            let f: u64 = ff
                .trim()
                .parse()
                .map_err(|_| "フレームが不正です".to_owned())?;
            (h.saturating_mul(3600)
                .saturating_add(m.saturating_mul(60))
                .saturating_add(s))
            .saturating_mul(nominal_fps)
            .saturating_add(f)
        }
        _ => return Err("タイムコード形式 (HH:MM:SS:FF / MM:SS) が不正です".to_owned()),
    };
    if total_frames == 0 {
        return Err("総尺は 1 フレーム以上で指定してください".to_owned());
    }
    Ok(total_frames)
}

/// タイムライン上のクリップドラッグ編集種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragMode {
    /// クリップの時間位置・レイヤー移動。
    Move,
    /// 左端を動かして開始時刻と長さをトリム・延長。
    TrimLeft,
    /// 右端を動かして長さをトリム・延長。
    TrimRight,
}

/// 複数選択・複製ドラッグに含まれる個別クリップの移動情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraggedClipItem {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// ドラッグ開始時のレイヤーインデックス。
    pub origin_layer_idx: usize,
    /// ドラッグ開始時の開始フレーム。
    pub origin_start_frame: u64,
    /// 元の長さ（フレーム）。
    pub origin_duration_frames: u64,
    /// 現在の移動先レイヤーインデックス（`layers.len()` は末尾自動追加レイヤーを示す）。
    pub target_layer_idx: usize,
    /// 現在の移動先開始フレーム。
    pub target_start_frame: u64,
}

/// ドラッグ時の座標計算・スナップ判定に使う読み取り専用の環境情報。
#[derive(Debug, Clone, Copy)]
pub struct DragSnapEnv<'a> {
    /// ドキュメント参照。
    pub doc: &'a ProjectDocument,
    /// 現在再生ヘッド位置。
    pub playhead: FrameIndex,
    /// スナップ設定。
    pub snap_cfg: &'a SnapConfig,
    /// `Shift` 押下中などでスナップを一時無効化するか。
    pub shift_bypass_snap: bool,
}

/// ドラッグ中（`MouseDown`〜`MouseUp`）のリアルタイムプレビュー状態。
#[derive(Debug, Clone)]
pub struct TimelineDragState {
    /// 主ドラッグ対象クリップ ID。
    pub clip_id: ClipId,
    /// 元レイヤー位置。
    pub origin_layer_idx: usize,
    /// 元開始フレーム。
    pub origin_start_frame: u64,
    /// 元クリップ長（フレーム）。
    pub origin_duration_frames: u64,
    /// ドラッグ開始時マウス X (px)。
    pub start_mouse_x: f32,
    /// ドラッグ開始時マウス Y (px)。
    pub start_mouse_y: f32,
    /// 現在の編集モード。
    pub mode: DragMode,
    /// 現在のプレビュー先レイヤーインデックス。
    pub current_layer_idx: usize,
    /// 現在のプレビュー開始フレーム。
    pub current_start_frame: u64,
    /// 現在のプレビュー長（フレーム）。
    pub current_duration_frames: u64,
    /// ドラッグ開始時のトラック行高さ (px)。
    pub row_height_px: f32,
    /// `Ctrl+ドラッグ` による複製移動かどうか。
    pub duplicate_mode: bool,
    /// 複数選択時に追従移動する全クリップ一覧。
    pub group_items: Vec<DraggedClipItem>,
    /// 現在のプレビュー配置が衝突やロック違反なく確定可能か。
    pub preview_valid: bool,
}

impl TimelineDragState {
    /// クリップ現在状態とマウス位置から単一ドラッグ状態を作成する。
    #[must_use]
    pub fn begin(
        clip: &Clip,
        origin_layer_idx: usize,
        mouse_x: f32,
        mouse_y: f32,
        mode: DragMode,
    ) -> Self {
        Self::begin_with_options(
            clip,
            origin_layer_idx,
            mouse_x,
            mouse_y,
            mode,
            44.0,
            false,
            Vec::new(),
        )
    }

    /// 行高さ・複製フラグ・複数選択クリップ群を指定してドラッグ状態を作成する。
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn begin_with_options(
        clip: &Clip,
        origin_layer_idx: usize,
        mouse_x: f32,
        mouse_y: f32,
        mode: DragMode,
        row_height_px: f32,
        duplicate_mode: bool,
        mut group_items: Vec<DraggedClipItem>,
    ) -> Self {
        let start = clip.timeline_start.get();
        let dur = clip.duration.get();
        if group_items.is_empty() {
            group_items.push(DraggedClipItem {
                clip_id: clip.id.clone(),
                origin_layer_idx,
                origin_start_frame: start,
                origin_duration_frames: dur,
                target_layer_idx: origin_layer_idx,
                target_start_frame: start,
            });
        }
        Self {
            clip_id: clip.id.clone(),
            origin_layer_idx,
            origin_start_frame: start,
            origin_duration_frames: dur,
            start_mouse_x: mouse_x,
            start_mouse_y: mouse_y,
            mode,
            current_layer_idx: origin_layer_idx,
            current_start_frame: start,
            current_duration_frames: dur,
            row_height_px: row_height_px.max(16.0),
            duplicate_mode,
            group_items,
            preview_valid: true,
        }
    }

    /// 既存シグネチャ互換: デフォルト行高さとスナップ設定でドラッグプレビューを更新する。
    pub fn update_from_cursor(
        &mut self,
        cursor: (f32, f32),
        px_per_frame: f32,
        doc: &ProjectDocument,
        playhead: FrameIndex,
        snap_cfg: &SnapConfig,
    ) -> Option<u64> {
        let env = DragSnapEnv {
            doc,
            playhead,
            snap_cfg,
            shift_bypass_snap: false,
        };
        self.update_from_cursor_env(cursor, px_per_frame, self.row_height_px, &env)
    }

    /// 行高さと [`DragSnapEnv`] を指定してプレビュー位置・衝突妥当性・ガイド線を再計算する。
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub fn update_from_cursor_env(
        &mut self,
        cursor: (f32, f32),
        px_per_frame: f32,
        row_height_px: f32,
        env: &DragSnapEnv<'_>,
    ) -> Option<u64> {
        let (mouse_x, mouse_y) = cursor;
        let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
        let raw_delta_frames = ((mouse_x - self.start_mouse_x) / safe_ppf).round() as i64;
        let excluded = self.excluded_clip_ids();
        let effective_snap = SnapConfig {
            enabled: env.snap_cfg.enabled && !env.shift_bypass_snap,
            ..*env.snap_cfg
        };

        match self.mode {
            DragMode::Move => self.update_move_mode(
                mouse_y,
                raw_delta_frames,
                env.doc,
                env.playhead,
                &excluded,
                &effective_snap,
                row_height_px.max(16.0),
            ),
            DragMode::TrimLeft => {
                let guide = self.update_trim_left(
                    raw_delta_frames,
                    env.doc,
                    env.playhead,
                    &excluded,
                    &effective_snap,
                );
                self.preview_valid = self.validate_single_trim(env.doc);
                guide
            }
            DragMode::TrimRight => {
                let guide = self.update_trim_right(
                    raw_delta_frames,
                    env.doc,
                    env.playhead,
                    &excluded,
                    &effective_snap,
                );
                self.preview_valid = self.validate_single_trim(env.doc);
                guide
            }
        }
    }

    fn excluded_clip_ids(&self) -> Vec<ClipId> {
        if self.duplicate_mode {
            Vec::new()
        } else if self.group_items.is_empty() {
            vec![self.clip_id.clone()]
        } else {
            self.group_items.iter().map(|i| i.clip_id.clone()).collect()
        }
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    fn update_move_mode(
        &mut self,
        mouse_y: f32,
        raw_delta_frames: i64,
        doc: &ProjectDocument,
        playhead: FrameIndex,
        excluded: &[ClipId],
        snap_cfg: &SnapConfig,
        row_height: f32,
    ) -> Option<u64> {
        let row_delta_raw = ((mouse_y - self.start_mouse_y) / row_height).round() as isize;
        let allow_auto_new_layer = row_delta_raw > 0;
        let max_layer_limit = if allow_auto_new_layer {
            doc.layers.len()
        } else {
            doc.layers.len().saturating_sub(1)
        };
        let min_orig_layer = self
            .group_items
            .iter()
            .map(|g| g.origin_layer_idx)
            .min()
            .unwrap_or(self.origin_layer_idx);
        let min_row_delta = -isize::try_from(min_orig_layer).unwrap_or(0);
        let max_row_delta =
            isize::try_from(max_layer_limit.saturating_sub(min_orig_layer)).unwrap_or(0);
        let clamped_row_delta = row_delta_raw.clamp(min_row_delta, max_row_delta);

        let min_orig_start = self
            .group_items
            .iter()
            .map(|g| g.origin_start_frame)
            .min()
            .unwrap_or(self.origin_start_frame);
        let min_frame_delta = -i64::try_from(min_orig_start).unwrap_or(0);
        let pre_delta = raw_delta_frames.max(min_frame_delta);
        let cand_primary =
            (i64::try_from(self.origin_start_frame).unwrap_or(0) + pre_delta).max(0) as u64;
        let dur = FrameCount::new(self.origin_duration_frames.max(1)).unwrap_or(FrameCount::ONE);
        let snap = snap_clip_move(
            doc,
            playhead,
            dur,
            FrameIndex::new(cand_primary),
            excluded,
            snap_cfg,
        );
        let snapped_delta = (i64::try_from(snap.snapped_start.get()).unwrap_or(0)
            - i64::try_from(self.origin_start_frame).unwrap_or(0))
        .max(min_frame_delta);

        for item in &mut self.group_items {
            let target_layer =
                isize::try_from(item.origin_layer_idx).unwrap_or(0) + clamped_row_delta;
            item.target_layer_idx = usize::try_from(target_layer.max(0)).unwrap_or(0);
            item.target_start_frame =
                (i64::try_from(item.origin_start_frame).unwrap_or(0) + snapped_delta).max(0) as u64;
        }
        self.current_layer_idx = usize::try_from(
            (isize::try_from(self.origin_layer_idx).unwrap_or(0) + clamped_row_delta).max(0),
        )
        .unwrap_or(0);
        self.current_start_frame =
            (i64::try_from(self.origin_start_frame).unwrap_or(0) + snapped_delta).max(0) as u64;
        self.current_duration_frames = self.origin_duration_frames;
        self.preview_valid = self.validate_group_move(doc, excluded);
        snap.guide_frame
    }

    fn validate_group_move(&self, doc: &ProjectDocument, excluded: &[ClipId]) -> bool {
        if !self.duplicate_mode {
            for item in &self.group_items {
                if doc
                    .layers
                    .get(item.origin_layer_idx)
                    .is_some_and(|l| l.locked)
                {
                    return false;
                }
            }
        }
        for (ix, item) in self.group_items.iter().enumerate() {
            if let Some(layer) = doc.layers.get(item.target_layer_idx) {
                if layer.locked {
                    return false;
                }
                let occupied = layer.clips.iter().any(|existing| {
                    !excluded.contains(&existing.id)
                        && intervals_overlap(
                            item.target_start_frame,
                            item.origin_duration_frames,
                            existing.timeline_start.get(),
                            existing.duration.get(),
                        )
                });
                if occupied {
                    return false;
                }
            }
            for other in &self.group_items[ix + 1..] {
                if item.target_layer_idx == other.target_layer_idx
                    && intervals_overlap(
                        item.target_start_frame,
                        item.origin_duration_frames,
                        other.target_start_frame,
                        other.origin_duration_frames,
                    )
                {
                    return false;
                }
            }
        }
        true
    }

    fn validate_single_trim(&self, doc: &ProjectDocument) -> bool {
        let Some(layer) = doc.layers.get(self.origin_layer_idx) else {
            return false;
        };
        if layer.locked {
            return false;
        }
        is_layer_interval_free(
            doc,
            &layer.id,
            self.current_start_frame,
            self.current_duration_frames,
            Some(&self.clip_id),
        )
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn update_trim_left(
        &mut self,
        delta_frames: i64,
        doc: &ProjectDocument,
        playhead: FrameIndex,
        excluded: &[ClipId],
        snap_cfg: &SnapConfig,
    ) -> Option<u64> {
        let orig_end = self
            .origin_start_frame
            .saturating_add(self.origin_duration_frames);
        let max_delta = i64::try_from(self.origin_duration_frames.saturating_sub(1)).unwrap_or(0);
        let min_delta = -i64::try_from(self.origin_start_frame).unwrap_or(0);
        let clamped = delta_frames.clamp(min_delta, max_delta);
        let raw_left =
            (i64::try_from(self.origin_start_frame).unwrap_or(0) + clamped).max(0) as u64;
        let mut guide = None;
        let final_left = if snap_cfg.enabled {
            let anchors = collect_snap_anchors(doc, playhead, excluded, snap_cfg);
            if let Some(hit) = snap_frame_edge(raw_left, &anchors, snap_cfg.threshold_frames)
                && hit.frame < orig_end
            {
                guide = Some(hit.frame);
                hit.frame
            } else {
                raw_left
            }
        } else {
            raw_left
        };
        self.current_start_frame = final_left;
        self.current_duration_frames = orig_end.saturating_sub(final_left).max(1);
        guide
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn update_trim_right(
        &mut self,
        delta_frames: i64,
        doc: &ProjectDocument,
        playhead: FrameIndex,
        excluded: &[ClipId],
        snap_cfg: &SnapConfig,
    ) -> Option<u64> {
        let min_delta = -i64::try_from(self.origin_duration_frames.saturating_sub(1)).unwrap_or(0);
        let clamped = delta_frames.max(min_delta);
        let raw_dur =
            (i64::try_from(self.origin_duration_frames).unwrap_or(1) + clamped).max(1) as u64;
        let raw_right = self.origin_start_frame.saturating_add(raw_dur);
        let mut guide = None;
        let final_right = if snap_cfg.enabled {
            let anchors = collect_snap_anchors(doc, playhead, excluded, snap_cfg);
            if let Some(hit) = snap_frame_edge(raw_right, &anchors, snap_cfg.threshold_frames)
                && hit.frame > self.origin_start_frame
            {
                guide = Some(hit.frame);
                hit.frame
            } else {
                raw_right
            }
        } else {
            raw_right
        };
        self.current_start_frame = self.origin_start_frame;
        self.current_duration_frames = final_right.saturating_sub(self.origin_start_frame).max(1);
        guide
    }

    /// ドラッグ完了時に単一 Undo 可能な実コマンドとして `AppController` へ確定適用する。
    ///
    /// # Errors
    ///
    /// ドメイン制約違反（重複・ロック中レイヤー・ソース範囲超過等）が発生した場合にエラー文字列を返す。
    #[allow(clippy::cast_possible_wrap)]
    pub fn commit(&self, controller: &mut AppController) -> Result<(), String> {
        if !self.preview_valid {
            return Err("他クリップとの重複またはロック中トラックへの移動はできません".to_owned());
        }
        match self.mode {
            DragMode::Move if self.duplicate_mode => self.commit_duplicate_move(controller),
            DragMode::Move => self.commit_move(controller),
            DragMode::TrimLeft => {
                let delta = i64::try_from(self.current_start_frame).unwrap_or(0)
                    - i64::try_from(self.origin_start_frame).unwrap_or(0);
                if delta == 0 {
                    return Ok(());
                }
                controller.trim_clip_left_delta(&self.clip_id, delta)
            }
            DragMode::TrimRight => {
                let delta = i64::try_from(self.origin_duration_frames).unwrap_or(0)
                    - i64::try_from(self.current_duration_frames).unwrap_or(0);
                if delta == 0 {
                    return Ok(());
                }
                controller.trim_clip_right_delta(&self.clip_id, delta)
            }
        }
    }

    fn commit_move(&self, controller: &mut AppController) -> Result<(), String> {
        let items: Vec<&DraggedClipItem> = self
            .group_items
            .iter()
            .filter(|i| {
                i.target_layer_idx != i.origin_layer_idx
                    || i.target_start_frame != i.origin_start_frame
            })
            .collect();
        if items.is_empty() {
            return Ok(());
        }

        let max_target = items.iter().map(|i| i.target_layer_idx).max().unwrap_or(0);
        if items.len() == 1 && max_target < controller.document.layers.len() {
            let item = items[0];
            let target_layer_id = controller.document.layers[item.target_layer_idx].id.clone();
            let cmd = EditCommand::MoveClip {
                clip_id: ClipRef::Id(item.clip_id.clone()),
                target_layer_id: Some(LayerRef::Id(target_layer_id.clone())),
                new_timeline_start: FrameIndex::new(item.target_start_frame),
            };
            let _ = controller.dispatch_command(&cmd)?;
            controller.selected_layer_id = Some(target_layer_id);
            return Ok(());
        }

        let (sim_doc, mut batch_cmds) =
            build_auto_layer_prepends(&controller.document, max_target)?;
        let mut plans = Vec::with_capacity(self.group_items.len());
        for item in &self.group_items {
            if item.target_layer_idx == item.origin_layer_idx
                && item.target_start_frame == item.origin_start_frame
            {
                continue;
            }
            let target_layer_id = sim_doc
                .layers
                .get(item.target_layer_idx)
                .map(|l| l.id.clone())
                .ok_or_else(|| "移動先レイヤーの生成に失敗しました".to_owned())?;
            plans.push(ClipMovePlan {
                clip_id: item.clip_id.clone(),
                target_layer_id,
                new_timeline_start: FrameIndex::new(item.target_start_frame),
            });
        }
        let move_batch =
            build_batch_move_clips_command(&sim_doc, &plans).map_err(|err| err.to_string())?;
        let _ = sim_doc.layers.len();
        batch_cmds.push(move_batch);
        let outer = if batch_cmds.len() == 1 {
            batch_cmds
                .pop()
                .ok_or_else(|| "コマンドが空です".to_owned())?
        } else {
            EditCommand::Batch {
                commands: batch_cmds,
            }
        };
        let _ = controller.dispatch_command(&outer)?;
        Ok(())
    }

    fn commit_duplicate_move(&self, controller: &mut AppController) -> Result<(), String> {
        let max_target = self
            .group_items
            .iter()
            .map(|i| i.target_layer_idx)
            .max()
            .unwrap_or(0);
        let (sim_doc, mut cmds) = build_auto_layer_prepends(&controller.document, max_target)?;
        let mut new_ids = Vec::with_capacity(self.group_items.len());
        let mut seq = 1_u64;
        for item in &self.group_items {
            let Some((_, orig_clip)) = controller.document.find_clip(&item.clip_id) else {
                continue;
            };
            let target_layer_id = sim_doc
                .layers
                .get(item.target_layer_idx)
                .map(|l| l.id.clone())
                .ok_or_else(|| "複製先レイヤーが見つかりません".to_owned())?;
            let new_id = allocate_unique_clip_id(&controller.document, &new_ids, &mut seq)?;
            new_ids.push(new_id.clone());
            cmds.push(EditCommand::AddClip {
                temp_id: None,
                id: Some(new_id),
                layer_id: LayerRef::Id(target_layer_id),
                clip: Box::new(NewClipSpec {
                    name: orig_clip.name.clone(),
                    timeline_start: FrameIndex::new(item.target_start_frame),
                    duration: orig_clip.duration,
                    effect_offset_frames: orig_clip.effect_offset_frames,
                    effect_duration_frames: orig_clip.effect_duration_frames,
                    midpoints: orig_clip.midpoints.clone(),
                    kind: clip_kind_to_new_spec_kind(&orig_clip.kind),
                }),
            });
        }
        if cmds.is_empty() {
            return Ok(());
        }
        let batch = EditCommand::Batch { commands: cmds };
        let _ = controller.dispatch_command(&batch)?;
        controller.set_selected_clips(new_ids);
        Ok(())
    }
}

fn clip_kind_to_new_spec_kind(kind: &ClipKind) -> NewClipKind {
    match kind {
        ClipKind::Video {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            visual,
            audio,
        } => NewClipKind::Video {
            asset_id: AssetRef::Id(asset_id.clone()),
            source_in: *source_in,
            playback_rate: *playback_rate,
            preserve_pitch: *preserve_pitch,
            visual: visual.clone(),
            audio: audio.clone(),
        },
        ClipKind::Audio {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            audio,
        } => NewClipKind::Audio {
            asset_id: AssetRef::Id(asset_id.clone()),
            source_in: *source_in,
            playback_rate: *playback_rate,
            preserve_pitch: *preserve_pitch,
            audio: audio.clone(),
        },
        ClipKind::Image { asset_id, visual } => NewClipKind::Image {
            asset_id: AssetRef::Id(asset_id.clone()),
            visual: visual.clone(),
        },
        ClipKind::Subtitle(sub) => NewClipKind::Subtitle(sub.clone()),
    }
}

fn intervals_overlap(s1: u64, d1: u64, s2: u64, d2: u64) -> bool {
    let e1 = s1.saturating_add(d1);
    let e2 = s2.saturating_add(d2);
    s1 < e2 && s2 < e1
}

fn build_auto_layer_prepends(
    doc: &ProjectDocument,
    max_target_idx: usize,
) -> Result<(ProjectDocument, Vec<EditCommand>), String> {
    let mut sim = doc.clone();
    let mut add_cmds = Vec::new();
    let mut suffix = sim.layers.len() + 1;
    while sim.layers.len() <= max_target_idx {
        let candidate = loop {
            let id = LayerId::new(format!("layer_auto_{suffix}")).map_err(|err| err.to_string())?;
            suffix += 1;
            if !sim.layers.iter().any(|l| l.id == id) {
                break id;
            }
        };
        let label = format!("T{}", sim.layers.len() + 1);
        sim.layers.push(Layer::new(candidate.clone(), &label));
        add_cmds.push(EditCommand::AddLayer {
            temp_id: None,
            id: Some(candidate),
            name: label,
            insert_index: None,
        });
    }
    Ok((sim, add_cmds))
}

fn allocate_unique_clip_id(
    doc: &ProjectDocument,
    pending: &[ClipId],
    seq: &mut u64,
) -> Result<ClipId, String> {
    loop {
        let candidate = ClipId::new(format!("clip_dup_{}", *seq)).map_err(|err| err.to_string())?;
        *seq += 1;
        if doc.find_clip(&candidate).is_none() && !pending.contains(&candidate) {
            return Ok(candidate);
        }
    }
}

/// `Ctrl+マウスダウン` 直後の保留状態（4px 以上移動で複製ドラッグ昇格、未満の離上で選択トグル）。
#[derive(Debug, Clone)]
pub struct PendingCtrlGesture {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// 元レイヤーインデックス。
    pub origin_layer_idx: usize,
    /// 押下開始マウス X (px)。
    pub start_mouse_x: f32,
    /// 押下開始マウス Y (px)。
    pub start_mouse_y: f32,
    /// 開始時のトラック行高さ (px)。
    pub row_height_px: f32,
}

impl PendingCtrlGesture {
    /// 移動距離が閾値 (`4px`) 以上なら `Ctrl+ドラッグ` 複製移動へ昇格する。
    #[must_use]
    pub fn try_promote_to_duplicate_drag(
        &self,
        mouse_x: f32,
        mouse_y: f32,
        doc: &ProjectDocument,
        selected_ids: &[ClipId],
    ) -> Option<TimelineDragState> {
        let dx = mouse_x - self.start_mouse_x;
        let dy = mouse_y - self.start_mouse_y;
        if dx.hypot(dy) < CTRL_DRAG_THRESHOLD_PX {
            return None;
        }
        let (_, primary) = doc.find_clip(&self.clip_id)?;
        let active_group_ids: Vec<ClipId> = if selected_ids.contains(&self.clip_id) {
            selected_ids.to_vec()
        } else {
            vec![self.clip_id.clone()]
        };
        let items = collect_group_items(doc, &active_group_ids);
        Some(TimelineDragState::begin_with_options(
            primary,
            self.origin_layer_idx,
            self.start_mouse_x,
            self.start_mouse_y,
            DragMode::Move,
            self.row_height_px,
            true,
            items,
        ))
    }
}

/// 指定クリップ群のドラッグ開始時情報を収集する。
#[must_use]
pub fn collect_group_items(doc: &ProjectDocument, clip_ids: &[ClipId]) -> Vec<DraggedClipItem> {
    let mut items = Vec::with_capacity(clip_ids.len());
    for (layer_idx, layer) in doc.layers.iter().enumerate() {
        for clip in &layer.clips {
            if clip_ids.contains(&clip.id) {
                items.push(DraggedClipItem {
                    clip_id: clip.id.clone(),
                    origin_layer_idx: layer_idx,
                    origin_start_frame: clip.timeline_start.get(),
                    origin_duration_frames: clip.duration.get(),
                    target_layer_idx: layer_idx,
                    target_start_frame: clip.timeline_start.get(),
                });
            }
        }
    }
    items
}

/// クリップ内中間点 (`◆`) の水平ドラッグ状態。
#[derive(Debug, Clone)]
pub struct MidpointDragState {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// クリップ開始タイムラインフレーム。
    pub clip_start_frame: u64,
    /// ドラッグ開始時の中間点エフェクト相対フレーム。
    pub origin_rel_frame: i64,
    /// 現在のエフェクト相対フレーム。
    pub current_rel_frame: i64,
    /// 隣接中間点やクリップ境界に基づく許容最小相対フレーム。
    pub min_rel_frame: i64,
    /// 隣接中間点やクリップ境界に基づく許容最大相対フレーム。
    pub max_rel_frame: i64,
    /// 開始時マウス X (px)。
    pub start_mouse_x: f32,
}

impl MidpointDragState {
    /// クリップと対象中間点からドラッグ状態を構築する。
    #[must_use]
    pub fn begin(clip: &Clip, orig_rel_frame: i64, mouse_x: f32) -> Option<Self> {
        let visible = clip.visible_midpoints();
        let idx = visible.iter().position(|&m| m == orig_rel_frame)?;
        let (bound_start, bound_end) = clip.effect_bounds();
        let min_rel = idx
            .checked_sub(1)
            .and_then(|i| visible.get(i).copied())
            .unwrap_or(bound_start)
            .saturating_add(1);
        let max_rel = visible
            .get(idx + 1)
            .copied()
            .unwrap_or(bound_end)
            .saturating_sub(1);
        if min_rel > max_rel {
            return None;
        }
        Some(Self {
            clip_id: clip.id.clone(),
            clip_start_frame: clip.timeline_start.get(),
            origin_rel_frame: orig_rel_frame,
            current_rel_frame: orig_rel_frame,
            min_rel_frame: min_rel,
            max_rel_frame: max_rel,
            start_mouse_x: mouse_x,
        })
    }

    /// マウス水平移動から中間点位置を更新し、必要ならスナップガイド線を返す。
    #[allow(clippy::cast_possible_truncation)]
    pub fn update_from_cursor(
        &mut self,
        mouse_x: f32,
        px_per_frame: f32,
        env: &DragSnapEnv<'_>,
    ) -> Option<u64> {
        let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
        let delta_frames = ((mouse_x - self.start_mouse_x) / safe_ppf).round() as i64;
        let raw_rel =
            (self.origin_rel_frame + delta_frames).clamp(self.min_rel_frame, self.max_rel_frame);
        let mut guide = None;
        let final_rel = if env.snap_cfg.enabled && !env.shift_bypass_snap && raw_rel >= 0 {
            let candidate_abs = self
                .clip_start_frame
                .saturating_add(u64::try_from(raw_rel).unwrap_or(0));
            let anchors = collect_snap_anchors(
                env.doc,
                env.playhead,
                std::slice::from_ref(&self.clip_id),
                env.snap_cfg,
            );
            if let Some(hit) =
                snap_frame_edge(candidate_abs, &anchors, env.snap_cfg.threshold_frames)
                && hit.frame >= self.clip_start_frame
                && let Ok(snapped_rel) = i64::try_from(hit.frame - self.clip_start_frame)
                && snapped_rel >= self.min_rel_frame
                && snapped_rel <= self.max_rel_frame
            {
                guide = Some(hit.frame);
                snapped_rel
            } else {
                raw_rel
            }
        } else {
            raw_rel
        };
        self.current_rel_frame = final_rel;
        guide
    }

    /// 確定時に `MoveMidpoint` コマンドをディスパッチする。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn commit(&self, controller: &mut AppController) -> Result<(), String> {
        if self.current_rel_frame == self.origin_rel_frame {
            return Ok(());
        }
        controller
            .move_midpoint(&self.clip_id, self.origin_rel_frame, self.current_rel_frame)
            .map(|_| ())
    }
}

/// 空タイムライン領域ドラッグによる矩形範囲選択（マーキー）状態。
#[derive(Debug, Clone)]
pub struct MarqueeSelectionState {
    /// 開始座標（タイムラインキャンバス相対 `(x, y)` px）。
    pub start_local: (f32, f32),
    /// 現在座標（タイムラインキャンバス相対 `(x, y)` px）。
    pub current_local: (f32, f32),
    /// ドラッグ開始時ウィンドウ Y 座標 (px)。
    pub start_window_y: f32,
    /// ドラッグ開始前の既存選択（追加選択時用）。
    pub base_selection: Vec<ClipId>,
    /// `Shift` / `Ctrl` で既存選択に追加するか。
    pub additive: bool,
}

impl MarqueeSelectionState {
    /// 現在のマーキー矩形に交差するクリップ ID 一覧を計算する。
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn compute_selected_clips(
        &self,
        doc: &ProjectDocument,
        scroll_x_frames: u64,
        px_per_frame: f32,
        row_height: f32,
    ) -> Vec<ClipId> {
        let min_x = self.start_local.0.min(self.current_local.0).max(0.0);
        let max_x = self.start_local.0.max(self.current_local.0).max(0.0);
        let min_y = self.start_local.1.min(self.current_local.1).max(0.0);
        let max_y = self.start_local.1.max(self.current_local.1).max(0.0);
        let f_start = frame_at_canvas_local_x(min_x, scroll_x_frames, px_per_frame);
        let f_end = frame_at_canvas_local_x(max_x, scroll_x_frames, px_per_frame).max(f_start + 1);

        let safe_row_h = row_height.max(16.0);
        let row_first = usize::try_from((min_y / safe_row_h).floor() as i64).unwrap_or(0);
        let row_last = usize::try_from((max_y / safe_row_h).floor() as i64).unwrap_or(0);

        let mut selected = if self.additive {
            self.base_selection.clone()
        } else {
            Vec::new()
        };
        for (layer_idx, layer) in doc.layers.iter().enumerate() {
            if layer_idx < row_first || layer_idx > row_last {
                continue;
            }
            for clip in &layer.clips {
                let c_start = clip.timeline_start.get();
                let c_end = c_start.saturating_add(clip.duration.get());
                if c_start < f_end && f_start < c_end && !selected.contains(&clip.id) {
                    selected.push(clip.id.clone());
                }
            }
        }
        selected
    }
}

/// ルーラー上の `Shift+ドラッグ` による `[start, end)` 選択範囲作成状態。
#[derive(Debug, Clone, Copy)]
pub struct SelectionRangeDragState {
    /// ドラッグ開始基準フレーム。
    pub anchor_frame: u64,
    /// 現在カーソルフレーム。
    pub current_frame: u64,
}

impl SelectionRangeDragState {
    /// 現在の半開フレーム区間 `[min, max + 1)` を返す。
    #[must_use]
    pub fn frame_range(self) -> FrameRange {
        let start = self.anchor_frame.min(self.current_frame);
        let end = self.anchor_frame.max(self.current_frame).saturating_add(1);
        FrameRange {
            start: FrameIndex::new(start),
            end: FrameIndex::new(end),
        }
    }
}

/// タイムライン終端境界ラインドラッグによる手動総尺調整状態。
#[derive(Debug, Clone, Copy)]
pub struct TimelineEndDragState {
    /// 開始時の総尺フレーム。
    pub origin_end_frame: u64,
    /// 現在のプレビュー総尺フレーム（>= 1）。
    pub current_end_frame: u64,
    /// 開始時マウス X (px)。
    pub start_mouse_x: f32,
}

impl TimelineEndDragState {
    /// マウス位置とスナップ設定から新しい総尺フレームを算出し、ガイド位置を返す。
    #[allow(clippy::cast_possible_truncation)]
    pub fn update_from_cursor(
        &mut self,
        mouse_x: f32,
        px_per_frame: f32,
        env: &DragSnapEnv<'_>,
    ) -> Option<u64> {
        let safe_ppf = px_per_frame.max(MIN_TIMELINE_PX_PER_FRAME);
        let delta = ((mouse_x - self.start_mouse_x) / safe_ppf).round() as i64;
        let raw_end =
            u64::try_from((i64::try_from(self.origin_end_frame).unwrap_or(1) + delta).max(1))
                .unwrap_or(1);
        let mut guide = None;
        let final_end = if env.snap_cfg.enabled && !env.shift_bypass_snap {
            let anchors = collect_snap_anchors(env.doc, env.playhead, &[], env.snap_cfg);
            if let Some(hit) = snap_frame_edge(raw_end, &anchors, env.snap_cfg.threshold_frames)
                && hit.frame >= 1
            {
                guide = Some(hit.frame);
                hit.frame
            } else {
                raw_end
            }
        } else {
            raw_end
        };
        self.current_end_frame = final_end.max(1);
        guide
    }

    /// 確定時に `SetTimelineLength(Manual)` を実行する。
    ///
    /// # Errors
    ///
    /// コマンド適用失敗時。
    pub fn commit(self, controller: &mut AppController) -> Result<(), String> {
        let count =
            FrameCount::new(self.current_end_frame.max(1)).map_err(|err| err.to_string())?;
        controller
            .set_timeline_length(TimelineLengthMode::Manual { end_frame: count })
            .map(|_| ())
    }
}

/// タイムライン上で現在進行中のポインタ操作。
#[derive(Debug, Clone)]
pub enum TimelinePointerGesture {
    /// クリップ本体・両端トリム・グループ/複製ドラッグ。
    ClipDrag(TimelineDragState),
    /// `Ctrl+マウスダウン` 保留判定。
    PendingCtrl(PendingCtrlGesture),
    /// 中間点 `◆` の水平移動。
    MidpointDrag(MidpointDragState),
    /// 空領域からのマーキー範囲選択。
    Marquee(MarqueeSelectionState),
    /// ルーラー上の連続プレイヘッドスクラブ。
    RulerScrub,
    /// ルーラー上の `Shift+ドラッグ` 選択範囲指定。
    SelectionRange(SelectionRangeDragState),
    /// 総尺境界ハンドルのドラッグ。
    TimelineEnd(TimelineEndDragState),
    /// 左上ズームバーの水平ドラッグ。
    ZoomBar {
        /// バー左端のウィンドウ基準 X (px)。
        bar_left_x: f32,
        /// バー全幅 (px)。
        bar_width: f32,
    },
    /// 下部水平スクロールバーつまみドラッグ。
    HScrollbar {
        /// ドラッグ開始時マウス X (px)。
        start_mouse_x: f32,
        /// 開始時スクロール位置（フレーム）。
        origin_scroll_frames: u64,
        /// レール幅 (px)。
        track_width_px: f32,
        /// 総コンテンツ幅（フレーム）。
        content_frames: u64,
    },
}
