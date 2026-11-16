//! プレビューキャンバス上の直接操作（Z順ヒットテスト・8点拡縮・回転・アンカー移動・アニメ平行移動・ズーム計算）。

use crate::core::animation::{AnimatedParameter, Keyframe};
use crate::core::ids::{ClipId, LayerId};
use crate::core::time::FrameIndex;
use crate::media::compositor::compute_layer_quad_ndc;
use crate::media::subtitle_renderer::render_subtitle_cached;
use crate::media::visual_properties::{EvaluatedVisual, Matrix2D, VisualProperties};
use crate::timeline::command::{EditCommand, ParameterPath, ParameterTarget, ParameterValue};
use crate::timeline::document::{AssetKind, AssetStatus, Clip, ClipKind, ProjectDocument};
use crate::ui::app_controller::PreviewZoomMode;

/// プレビュー `[#]` グリッド線・`Ctrl` スナップ間隔（プロジェクトキャンバス座標 px）。
pub const PREVIEW_GRID_STEP_PX: f64 = 40.0;

/// 選択ハンドル（8角/辺・回転・アンカー）の当たり判定半径（画面ピクセル）。
pub const HANDLE_HIT_RADIUS_PX: f32 = 8.0;

/// 上辺中央から回転ハンドルを離す画面ピクセル距離。
pub const ROTATE_HANDLE_OFFSET_PX: f32 = 24.0;

/// 最小拡大率（極小・反転暴走防止）。
const MIN_SCALE_MAGNITUDE: f64 = 0.01;
/// 最大拡大率。
const MAX_SCALE_MAGNITUDE: f64 = 50.0;

/// 選択オブジェクト上の操作ハンドル種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewHandleKind {
    /// オブジェクト本体（平行移動）。
    Move,
    /// 左上コーナー拡大縮小。
    ScaleTL,
    /// 上辺拡大縮小。
    ScaleT,
    /// 右上コーナー拡大縮小。
    ScaleTR,
    /// 右辺拡大縮小。
    ScaleR,
    /// 右下コーナー拡大縮小。
    ScaleBR,
    /// 下辺拡大縮小。
    ScaleB,
    /// 左下コーナー拡大縮小。
    ScaleBL,
    /// 左辺拡大縮小。
    ScaleL,
    /// 上方突起の回転ハンドル。
    Rotate,
    /// 回転・拡縮の基準アンカー `⊕` ハンドル。
    Anchor,
}

impl PreviewHandleKind {
    /// 左クリック時に対辺を固定する「反対側アンカー」と操作対象軸 `(affects_x, affects_y)` のローカル記号を返す。
    #[must_use]
    const fn scale_spec(self) -> Option<(f64, f64, f64, f64, bool, bool)> {
        // (handle_sign_x, handle_sign_y, opp_sign_x, opp_sign_y, affects_x, affects_y)
        match self {
            Self::ScaleTL => Some((-1.0, -1.0, 1.0, 1.0, true, true)),
            Self::ScaleT => Some((0.0, -1.0, 0.0, 1.0, false, true)),
            Self::ScaleTR => Some((1.0, -1.0, -1.0, 1.0, true, true)),
            Self::ScaleR => Some((1.0, 0.0, -1.0, 0.0, true, false)),
            Self::ScaleBR => Some((1.0, 1.0, -1.0, -1.0, true, true)),
            Self::ScaleB => Some((0.0, 1.0, 0.0, -1.0, false, true)),
            Self::ScaleBL => Some((-1.0, 1.0, 1.0, -1.0, true, true)),
            Self::ScaleL => Some((-1.0, 0.0, 1.0, 0.0, true, false)),
            Self::Move | Self::Rotate | Self::Anchor => None,
        }
    }
}

/// プレビューキャンバス上のドラッグ操作種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewDragMode {
    /// キャンバス全体のパン移動（中ボタンまたは `Space`+左ドラッグ）。
    PanViewport,
    /// 選択オブジェクトの位置 `X`/`Y` 移動。
    MoveObject,
    /// 選択ハンドルによる拡大縮小（`X`/`Y`/`ScaleX`/`ScaleY`）。
    ScaleHandle {
        /// 操作中のハンドル種別。
        handle: PreviewHandleKind,
    },
    /// 回転ハンドルによる `RotationDeg` 変更。
    RotateHandle,
    /// `⊕` アンカー位置変更（見た目の映像位置を固定したまま `AnchorX/Y` + `X/Y` を相殺更新）。
    MoveAnchor,
}

/// ドラッグ中の修飾キー状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DragModifiers {
    /// `Shift` キー（軸ロック・等比維持・15度スナップ）。
    pub shift: bool,
    /// `Ctrl` キー（グリッドスナップ）。
    pub ctrl: bool,
    /// `Alt` キー（中心アンカー基準拡縮）。
    pub alt: bool,
}

/// ドラッグ開始時点における 1 クリップ分の初期アニメーション状態と評価済み見た目。
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewClipDragSnapshot {
    /// 対象クリップ ID。
    pub clip_id: ClipId,
    /// 初期 `X` パラメータ。
    pub x: AnimatedParameter,
    /// 初期 `Y` パラメータ。
    pub y: AnimatedParameter,
    /// 初期 `ScaleX` パラメータ。
    pub scale_x: AnimatedParameter,
    /// 初期 `ScaleY` パラメータ。
    pub scale_y: AnimatedParameter,
    /// 初期 `RotationDeg` パラメータ。
    pub rotation_deg: AnimatedParameter,
    /// 初期 `AnchorX` パラメータ。
    pub anchor_x: AnimatedParameter,
    /// 初期 `AnchorY` パラメータ。
    pub anchor_y: AnimatedParameter,
    /// ドラッグ開始フレームでの評価済み視覚プロパティ。
    pub eval: EvaluatedVisual,
    /// ソースメディアまたは字幕ラスタサイズ `(width, height)`。
    pub source_size: (u32, u32),
}

/// プレビュー上の直接ドラッグ操作状態（ライブプレビューと単一 Undo 確定を両立）。
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewDragState {
    /// 実行中のドラッグ種別。
    pub mode: PreviewDragMode,
    /// 主操作クリップ ID（`PanViewport` の場合は `None`）。
    pub primary_clip_id: Option<ClipId>,
    /// ドラッグ開始時のウィンドウ座標 `(x, y)`。
    pub start_mouse_window: (f32, f32),
    /// ドラッグ開始時のプレビューパンオフセット `(px_x, px_y)`。
    pub start_pan_px: (f32, f32),
    /// 対象クリップ群の初期スナップショット一覧。
    pub initial_clips: Vec<PreviewClipDragSnapshot>,
    /// 閾値以上の移動が行われたか（単なるクリック選択とUndo履歴生成を区別）。
    pub has_moved: bool,
}

/// プレビュー領域とプロジェクトキャンバス座標系を相互変換する座標変換器。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewViewportTransform {
    /// プレビュー領域矩形 `(x, y, width, height)`（ウィンドウ px）。
    pub bounds: (f32, f32, f32, f32),
    /// プロジェクトキャンバス解像度 `(width, height)`。
    pub proj_size: (u32, u32),
    /// 表示倍率モード。
    pub zoom: PreviewZoomMode,
    /// パン移動オフセット（画面 px）。
    pub pan_px: (f32, f32),
}

impl PreviewViewportTransform {
    /// 自動フィット時の表示スケール (`画面 px / プロジェクト px`) を算出する。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn fit_scale(&self) -> f32 {
        let bw = self.bounds.2.max(16.0);
        let bh = self.bounds.3.max(16.0);
        let pw = self.proj_size.0.max(1) as f32;
        let ph = self.proj_size.1.max(1) as f32;
        (bw / pw).min(bh / ph).max(0.01)
    }

    /// 現在の表示倍率における実効スケール (`画面 px / プロジェクト px`) を返す。
    #[must_use]
    pub fn effective_scale(&self) -> f32 {
        self.zoom
            .zoom_factor()
            .unwrap_or_else(|| self.fit_scale())
            .max(0.01)
    }

    /// 実際にキャンバス映像が描画されるウィンドウ座標矩形 `(x, y, w, h)` を返す。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn rendered_rect(&self) -> (f32, f32, f32, f32) {
        let scale = self.effective_scale();
        let rw = (self.proj_size.0.max(1) as f32) * scale;
        let rh = (self.proj_size.1.max(1) as f32) * scale;
        let cx = self.bounds.0 + self.bounds.2 * 0.5 + self.pan_px.0;
        let cy = self.bounds.1 + self.bounds.3 * 0.5 + self.pan_px.1;
        (cx - rw * 0.5, cy - rh * 0.5, rw, rh)
    }

    /// キャンバス中心基準座標 `(cx, cy)` をウィンドウ座標 `(sx, sy)` へ変換する。
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn canvas_px_to_screen(&self, cx: f64, cy: f64) -> (f32, f32) {
        let scale = self.effective_scale();
        let center_x = self.bounds.0 + self.bounds.2 * 0.5 + self.pan_px.0;
        let center_y = self.bounds.1 + self.bounds.3 * 0.5 + self.pan_px.1;
        (
            center_x + (cx as f32) * scale,
            center_y + (cy as f32) * scale,
        )
    }

    /// ウィンドウ座標 `(sx, sy)` をキャンバス中心基準座標 `(cx, cy)` へ逆変換する。
    #[must_use]
    pub fn screen_to_canvas_px(&self, sx: f32, sy: f32) -> (f64, f64) {
        let scale = f64::from(self.effective_scale());
        let center_x = f64::from(self.bounds.0 + self.bounds.2 * 0.5 + self.pan_px.0);
        let center_y = f64::from(self.bounds.1 + self.bounds.3 * 0.5 + self.pan_px.1);
        (
            (f64::from(sx) - center_x) / scale,
            (f64::from(sy) - center_y) / scale,
        )
    }

    /// 画面 px の移動量 `(dx, dy)` をプロジェクトキャンバス座標の移動量へ変換する。
    #[must_use]
    pub fn screen_delta_to_canvas_delta(&self, dx: f32, dy: f32) -> (f64, f64) {
        let scale = f64::from(self.effective_scale());
        (f64::from(dx) / scale, f64::from(dy) / scale)
    }
}

/// 1 フレーム時点でプレビュー上に描かれる視覚クリップの幾何情報。
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewLayerHitQuad {
    /// クリップ ID。
    pub clip_id: ClipId,
    /// 所属レイヤー ID。
    pub layer_id: LayerId,
    /// `layers` 配列上のインデックス。
    pub track_index: usize,
    /// 描画順（大きいほど前面）。
    pub z_index: usize,
    /// 所属レイヤーがロック中か（ヒット判定で貫通するため保持）。
    pub is_locked: bool,
    /// ソースメディアまたは字幕ラスタサイズ。
    pub source_size: (u32, u32),
    /// 評価済み視覚プロパティ。
    pub visual: EvaluatedVisual,
    /// プロジェクト中心基準の四隅 `[TL, TR, BR, BL]`。
    pub canvas_corners: [(f64, f64); 4],
    /// ウィンドウ画面座標での四隅 `[TL, TR, BR, BL]`。
    pub screen_corners: [(f32, f32); 4],
    /// プロジェクト中心基準のアンカー座標 `(x, y)`。
    pub anchor_canvas: (f64, f64),
    /// ウィンドウ座標のアンカー `⊕` 表示位置。
    pub anchor_screen: (f32, f32),
    /// ウィンドウ座標の回転ハンドル位置。
    pub rotate_handle_screen: (f32, f32),
}

/// ドラッグ計算結果として得られる 1 クリップ分の更新済み視覚パラメータ群。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdatedClipVisualParams {
    /// 更新後 `X`。
    pub x: AnimatedParameter,
    /// 更新後 `Y`。
    pub y: AnimatedParameter,
    /// 更新後 `ScaleX`。
    pub scale_x: AnimatedParameter,
    /// 更新後 `ScaleY`。
    pub scale_y: AnimatedParameter,
    /// 更新後 `RotationDeg`。
    pub rotation_deg: AnimatedParameter,
    /// 更新後 `AnchorX`。
    pub anchor_x: AnimatedParameter,
    /// 更新後 `AnchorY`。
    pub anchor_y: AnimatedParameter,
}

/// ドラッグ結果（更新パラメータまたはブロック通知）。
#[derive(Debug, Clone, PartialEq)]
pub struct DragUpdateOutcome {
    /// 更新後パラメータ（数式ロック等で更新できない場合は `None`）。
    pub updated: Option<UpdatedClipVisualParams>,
    /// ユーザーへ提示する警告メッセージ（例: `Expression` 設定時）。
    pub notice: Option<String>,
}

/// クリップが保持する [`VisualProperties`] の不変参照を返す（音声専用クリップは `None`）。
#[must_use]
pub const fn clip_visual_props(clip: &Clip) -> Option<&VisualProperties> {
    match &clip.kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Some(visual),
        ClipKind::Subtitle(sub) => Some(&sub.visual),
        ClipKind::Audio { .. } => None,
    }
}

/// クリップが保持する [`VisualProperties`] の可変参照を返す（音声専用クリップは `None`）。
#[must_use]
pub const fn clip_visual_props_mut(clip: &mut Clip) -> Option<&mut VisualProperties> {
    match &mut clip.kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Some(visual),
        ClipKind::Subtitle(sub) => Some(&mut sub.visual),
        ClipKind::Audio { .. } => None,
    }
}

/// クリップがリンク切れ素材を参照しているか判定する。
#[must_use]
pub fn clip_has_missing_asset(doc: &ProjectDocument, clip: &Clip) -> bool {
    let asset_id = match &clip.kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => asset_id,
        ClipKind::Subtitle(_) => return false,
    };
    doc.find_asset(asset_id)
        .is_some_and(|asset| matches!(asset.status, AssetStatus::Missing { .. }))
}

/// 現在再生フレームで可視範囲に含まれ、かつリンク切れとなっている先頭クリップの素材 ID とファイル名を返す。
#[must_use]
pub fn find_missing_asset_clip_at_playhead(
    doc: &ProjectDocument,
    playhead: FrameIndex,
) -> Option<(crate::core::ids::AssetId, String)> {
    for layer in &doc.layers {
        for clip in &layer.clips {
            if clip.evaluation_context(playhead, doc.fps).is_none() {
                continue;
            }
            let asset_id = match &clip.kind {
                ClipKind::Video { asset_id, .. }
                | ClipKind::Audio { asset_id, .. }
                | ClipKind::Image { asset_id, .. } => asset_id,
                ClipKind::Subtitle(_) => continue,
            };
            if let Some(asset) = doc.find_asset(asset_id)
                && matches!(asset.status, AssetStatus::Missing { .. })
            {
                return Some((asset.id.clone(), asset.name.clone()));
            }
        }
    }
    None
}

/// GPU 合成エンジンと同一ロジックでクリップの論理ソースサイズ `(width, height)` を求める。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn clip_source_dimensions(
    doc: &ProjectDocument,
    clip: &Clip,
    eval_ctx: crate::core::animation::EvaluationContext,
) -> Option<(u32, u32)> {
    let proj_w = doc.resolution.width.get();
    let proj_h = doc.resolution.height.get();
    match &clip.kind {
        ClipKind::Audio { .. } => None,
        ClipKind::Video { asset_id, .. } => Some(match doc.find_asset(asset_id).map(|a| &a.kind) {
            Some(AssetKind::Video { width, height, .. }) => (
                (*width).unwrap_or(proj_w).max(1),
                (*height).unwrap_or(proj_h).max(1),
            ),
            _ => (proj_w.max(1), proj_h.max(1)),
        }),
        ClipKind::Image { asset_id, .. } => Some(match doc.find_asset(asset_id).map(|a| &a.kind) {
            Some(AssetKind::Image { width, height }) => ((*width).max(1), (*height).max(1)),
            _ => (proj_w.max(1), proj_h.max(1)),
        }),
        ClipKind::Subtitle(sub) => {
            let font_size = sub
                .font_size
                .evaluate(eval_ctx)
                .unwrap_or(48.0)
                .clamp(8.0, 400.0) as f32;
            let max_width = proj_w.clamp(128, 3840);
            let frame = render_subtitle_cached(sub, font_size, max_width);
            Some((frame.width.max(1), frame.height.max(1)))
        }
    }
}

/// 指定再生フレームで可視となる全レイヤーのプレビュー幾何情報一覧を Z 順昇順（背面 -> 前面）で構築する。
#[must_use]
pub fn build_preview_hit_quads(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    viewport: &PreviewViewportTransform,
) -> Vec<PreviewLayerHitQuad> {
    let proj_w = doc.resolution.width.get().max(1);
    let proj_h = doc.resolution.height.get().max(1);
    let half_w = f64::from(proj_w) * 0.5;
    let half_h = f64::from(proj_h) * 0.5;
    let mut result = Vec::new();
    let mut z_index = 0_usize;

    for (track_index, layer) in doc.layers.iter().enumerate() {
        if !layer.visible {
            continue;
        }
        for clip in &layer.clips {
            let Some(eval_ctx) = clip.evaluation_context(playhead, doc.fps) else {
                continue;
            };
            let Some(visual_props) = clip_visual_props(clip) else {
                continue;
            };
            let Some(source_size) = clip_source_dimensions(doc, clip, eval_ctx) else {
                continue;
            };
            let Ok(eval) = visual_props.evaluate(eval_ctx) else {
                continue;
            };
            let verts = compute_layer_quad_ndc(source_size.0, source_size.1, proj_w, proj_h, &eval);
            let canvas_corners = [
                (
                    f64::from(verts[0].ndc_pos[0]) * half_w,
                    -f64::from(verts[0].ndc_pos[1]) * half_h,
                ),
                (
                    f64::from(verts[1].ndc_pos[0]) * half_w,
                    -f64::from(verts[1].ndc_pos[1]) * half_h,
                ),
                (
                    f64::from(verts[2].ndc_pos[0]) * half_w,
                    -f64::from(verts[2].ndc_pos[1]) * half_h,
                ),
                (
                    f64::from(verts[3].ndc_pos[0]) * half_w,
                    -f64::from(verts[3].ndc_pos[1]) * half_h,
                ),
            ];
            let screen_corners = [
                viewport.canvas_px_to_screen(canvas_corners[0].0, canvas_corners[0].1),
                viewport.canvas_px_to_screen(canvas_corners[1].0, canvas_corners[1].1),
                viewport.canvas_px_to_screen(canvas_corners[2].0, canvas_corners[2].1),
                viewport.canvas_px_to_screen(canvas_corners[3].0, canvas_corners[3].1),
            ];
            let anchor_screen = viewport.canvas_px_to_screen(eval.x, eval.y);
            let rotate_handle_screen = compute_rotate_handle_screen(&screen_corners);
            result.push(PreviewLayerHitQuad {
                clip_id: clip.id.clone(),
                layer_id: layer.id.clone(),
                track_index,
                z_index,
                is_locked: layer.locked,
                source_size,
                visual: eval,
                canvas_corners,
                screen_corners,
                anchor_canvas: (eval.x, eval.y),
                anchor_screen,
                rotate_handle_screen,
            });
            z_index += 1;
        }
    }
    result
}

fn compute_rotate_handle_screen(screen_corners: &[(f32, f32); 4]) -> (f32, f32) {
    let top_mid = midpoint(screen_corners[0], screen_corners[1]);
    let bot_mid = midpoint(screen_corners[3], screen_corners[2]);
    let dx = top_mid.0 - bot_mid.0;
    let dy = top_mid.1 - bot_mid.1;
    let len = dx.hypot(dy);
    let (nx, ny) = if len > 1e-3 {
        (dx / len, dy / len)
    } else {
        (0.0, -1.0)
    };
    (
        top_mid.0 + nx * ROTATE_HANDLE_OFFSET_PX,
        top_mid.1 + ny * ROTATE_HANDLE_OFFSET_PX,
    )
}

#[must_use]
fn midpoint(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    (f32::midpoint(a.0, b.0), f32::midpoint(a.1, b.1))
}

/// 選択オブジェクトの全ハンドル（アンカー・回転・8拡縮）画面座標一覧を返す。
#[must_use]
pub fn handle_screen_positions(
    quad: &PreviewLayerHitQuad,
) -> [(PreviewHandleKind, (f32, f32)); 10] {
    let [tl, tr, br, bl] = quad.screen_corners;
    [
        (PreviewHandleKind::Anchor, quad.anchor_screen),
        (PreviewHandleKind::Rotate, quad.rotate_handle_screen),
        (PreviewHandleKind::ScaleTL, tl),
        (PreviewHandleKind::ScaleT, midpoint(tl, tr)),
        (PreviewHandleKind::ScaleTR, tr),
        (PreviewHandleKind::ScaleR, midpoint(tr, br)),
        (PreviewHandleKind::ScaleBR, br),
        (PreviewHandleKind::ScaleB, midpoint(br, bl)),
        (PreviewHandleKind::ScaleBL, bl),
        (PreviewHandleKind::ScaleL, midpoint(bl, tl)),
    ]
}

/// 選択済みクリップ上のハンドル当たり判定を行う（アンカー -> 回転 -> 8点拡縮の優先順）。
#[must_use]
pub fn hit_test_selection_handle(
    quad: &PreviewLayerHitQuad,
    sx: f32,
    sy: f32,
) -> Option<PreviewHandleKind> {
    if quad.is_locked {
        return None;
    }
    for (kind, pos) in handle_screen_positions(quad) {
        let radius = if kind == PreviewHandleKind::Anchor {
            HANDLE_HIT_RADIUS_PX + 2.0
        } else {
            HANDLE_HIT_RADIUS_PX
        };
        if (sx - pos.0).hypot(sy - pos.1) <= radius {
            return Some(kind);
        }
    }
    None
}

/// 最前面（Z順降順）から当たり判定し、ロックされていない最上位のヒットクリップを返す。
#[must_use]
pub fn hit_test_preview_layers(
    quads: &[PreviewLayerHitQuad],
    sx: f32,
    sy: f32,
) -> Option<&PreviewLayerHitQuad> {
    quads
        .iter()
        .rev()
        .find(|quad| !quad.is_locked && point_in_quad_screen((sx, sy), &quad.screen_corners))
}

/// 任意の凸四角形 `[TL, TR, BR, BL]` の内外を反転（負スケール）に依存せず判定する。
#[must_use]
pub fn point_in_quad_screen(pt: (f32, f32), corners: &[(f32, f32); 4]) -> bool {
    point_in_triangle_2d(pt, corners[0], corners[1], corners[2])
        || point_in_triangle_2d(pt, corners[0], corners[2], corners[3])
}

fn point_in_triangle_2d(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let area = triangle_double_area(a, b, c).abs();
    if area < 1e-3 {
        return false;
    }
    let a1 = triangle_double_area(p, a, b).abs();
    let a2 = triangle_double_area(p, b, c).abs();
    let a3 = triangle_double_area(p, c, a).abs();
    (a1 + a2 + a3) <= area + 0.5
}

fn triangle_double_area(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// アニメーション種別を保ったまま全セグメント/キーフレームを平行移動する。
///
/// - [`AnimatedParameter::Constant`] -> `value + delta`
/// - [`AnimatedParameter::Easing`] -> `start_value + delta`, `end_value + delta`
/// - [`AnimatedParameter::Keyframes`] -> 全キーフレームへ `+ delta`
/// - [`AnimatedParameter::Expression`] -> 直接ドラッグ不可としてエラー通知を返す
///
/// # Errors
///
/// 対象パラメータが数式 (`Expression`) の場合はエラー文字列を返す。
pub fn shift_animated_param(
    param: &AnimatedParameter,
    delta: f64,
) -> Result<AnimatedParameter, &'static str> {
    match param {
        AnimatedParameter::Constant { value } => Ok(AnimatedParameter::constant(value + delta)),
        AnimatedParameter::Easing {
            start_value,
            end_value,
            curve,
            extrapolation,
        } => Ok(AnimatedParameter::Easing {
            start_value: start_value + delta,
            end_value: end_value + delta,
            curve: *curve,
            extrapolation: *extrapolation,
        }),
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => Ok(AnimatedParameter::Keyframes {
            keyframes: keyframes
                .iter()
                .map(|kf| Keyframe {
                    frame: kf.frame,
                    value: kf.value + delta,
                    easing: kf.easing,
                })
                .collect(),
            extrapolation: *extrapolation,
        }),
        AnimatedParameter::Expression { .. } => {
            Err("数式 f(t) が設定されているパラメータはプレビューから直接編集できません")
        }
    }
}

/// ドラッグ開始時のスナップショットから現在のマウス位置に応じた更新後パラメータを計算する。
#[must_use]
pub fn compute_drag_updated_clip(
    drag: &PreviewDragState,
    snapshot: &PreviewClipDragSnapshot,
    viewport: &PreviewViewportTransform,
    current_mouse_window: (f32, f32),
    modifiers: DragModifiers,
    scale_aspect_locked: bool,
) -> DragUpdateOutcome {
    let raw_delta_px = (
        current_mouse_window.0 - drag.start_mouse_window.0,
        current_mouse_window.1 - drag.start_mouse_window.1,
    );
    let canvas_delta = viewport.screen_delta_to_canvas_delta(raw_delta_px.0, raw_delta_px.1);

    match drag.mode {
        PreviewDragMode::PanViewport => DragUpdateOutcome {
            updated: None,
            notice: None,
        },
        PreviewDragMode::MoveObject => compute_move_update(snapshot, canvas_delta, modifiers),
        PreviewDragMode::ScaleHandle { handle } => compute_scale_update(
            snapshot,
            handle,
            canvas_delta,
            modifiers,
            scale_aspect_locked,
        ),
        PreviewDragMode::RotateHandle => {
            let start_c =
                viewport.screen_to_canvas_px(drag.start_mouse_window.0, drag.start_mouse_window.1);
            let cur_c =
                viewport.screen_to_canvas_px(current_mouse_window.0, current_mouse_window.1);
            compute_rotate_update(snapshot, start_c, cur_c, modifiers.shift)
        }
        PreviewDragMode::MoveAnchor => compute_anchor_update(snapshot, canvas_delta),
    }
}

fn blocked_outcome(msg: &'static str) -> DragUpdateOutcome {
    DragUpdateOutcome {
        updated: None,
        notice: Some(msg.to_owned()),
    }
}

fn snapshot_to_updated(snapshot: &PreviewClipDragSnapshot) -> UpdatedClipVisualParams {
    UpdatedClipVisualParams {
        x: snapshot.x.clone(),
        y: snapshot.y.clone(),
        scale_x: snapshot.scale_x.clone(),
        scale_y: snapshot.scale_y.clone(),
        rotation_deg: snapshot.rotation_deg.clone(),
        anchor_x: snapshot.anchor_x.clone(),
        anchor_y: snapshot.anchor_y.clone(),
    }
}

fn compute_move_update(
    snapshot: &PreviewClipDragSnapshot,
    canvas_delta: (f64, f64),
    modifiers: DragModifiers,
) -> DragUpdateOutcome {
    let (mut dx, mut dy) = if modifiers.shift {
        if canvas_delta.0.abs() >= canvas_delta.1.abs() {
            (canvas_delta.0, 0.0)
        } else {
            (0.0, canvas_delta.1)
        }
    } else {
        canvas_delta
    };
    if modifiers.ctrl {
        let target_x =
            ((snapshot.eval.x + dx) / PREVIEW_GRID_STEP_PX).round() * PREVIEW_GRID_STEP_PX;
        let target_y =
            ((snapshot.eval.y + dy) / PREVIEW_GRID_STEP_PX).round() * PREVIEW_GRID_STEP_PX;
        dx = target_x - snapshot.eval.x;
        dy = target_y - snapshot.eval.y;
    }

    let Ok(x) = shift_animated_param(&snapshot.x, dx) else {
        return blocked_outcome("X 座標に数式 f(t) が設定されているため直接移動できません");
    };
    let Ok(y) = shift_animated_param(&snapshot.y, dy) else {
        return blocked_outcome("Y 座標に数式 f(t) が設定されているため直接移動できません");
    };
    let mut updated = snapshot_to_updated(snapshot);
    updated.x = x;
    updated.y = y;
    DragUpdateOutcome {
        updated: Some(updated),
        notice: None,
    }
}

fn compute_scale_update(
    snapshot: &PreviewClipDragSnapshot,
    handle: PreviewHandleKind,
    delta_canvas: (f64, f64),
    modifiers: DragModifiers,
    scale_aspect_locked: bool,
) -> DragUpdateOutcome {
    let Some((handle_horiz, handle_vert, opp_sign_x, opp_sign_y, affects_x, affects_y)) =
        handle.scale_spec()
    else {
        return DragUpdateOutcome {
            updated: Some(snapshot_to_updated(snapshot)),
            notice: None,
        };
    };

    let (sin_r, cos_r) = snapshot.eval.rotation_deg.to_radians().sin_cos();
    let local_delta = (
        cos_r * delta_canvas.0 + sin_r * delta_canvas.1,
        -sin_r * delta_canvas.0 + cos_r * delta_canvas.1,
    );

    let half_w = f64::from(snapshot.source_size.0.max(1)) * 0.5;
    let half_h = f64::from(snapshot.source_size.1.max(1)) * 0.5;
    let anchor = (snapshot.eval.anchor_x, snapshot.eval.anchor_y);
    let pivot_local = if modifiers.alt {
        anchor
    } else {
        (opp_sign_x * half_w, opp_sign_y * half_h)
    };
    let handle_local = (handle_horiz * half_w, handle_vert * half_h);

    let computed_scales = compute_axis_scales(
        snapshot,
        handle_local,
        pivot_local,
        local_delta,
        (affects_x, affects_y),
    );
    let final_scales = if affects_x && affects_y && (scale_aspect_locked || modifiers.shift) {
        enforce_aspect_ratio(
            snapshot.eval.scale_x,
            snapshot.eval.scale_y,
            computed_scales,
        )
    } else {
        computed_scales
    };

    build_scale_outcome(snapshot, pivot_local, final_scales, modifiers.alt)
}

fn compute_axis_scales(
    snapshot: &PreviewClipDragSnapshot,
    handle_local: (f64, f64),
    pivot_local: (f64, f64),
    local_delta: (f64, f64),
    affects: (bool, bool),
) -> (f64, f64) {
    let sx0 = snapshot.eval.scale_x;
    let sy0 = snapshot.eval.scale_y;
    let scaled_horiz = if affects.0 {
        scale_single_axis(sx0, (handle_local.0 - pivot_local.0) * sx0, local_delta.0)
    } else {
        sx0
    };
    let scaled_vert = if affects.1 {
        scale_single_axis(sy0, (handle_local.1 - pivot_local.1) * sy0, local_delta.1)
    } else {
        sy0
    };
    (scaled_horiz, scaled_vert)
}

fn scale_single_axis(old_scale: f64, span_world: f64, delta_local: f64) -> f64 {
    if span_world.abs() < 1e-3 {
        return old_scale;
    }
    let sign = if old_scale < 0.0 { -1.0 } else { 1.0 };
    let ratio = (span_world + delta_local) / span_world;
    let mag = (old_scale.abs() * ratio.max(0.01)).clamp(MIN_SCALE_MAGNITUDE, MAX_SCALE_MAGNITUDE);
    sign * mag
}

fn enforce_aspect_ratio(sx0: f64, sy0: f64, candidate_scales: (f64, f64)) -> (f64, f64) {
    let rx = if sx0.abs() > 1e-6 {
        candidate_scales.0 / sx0
    } else {
        1.0
    };
    let ry = if sy0.abs() > 1e-6 {
        candidate_scales.1 / sy0
    } else {
        1.0
    };
    let uniform = if (rx - 1.0).abs() >= (ry - 1.0).abs() {
        rx
    } else {
        ry
    }
    .clamp(MIN_SCALE_MAGNITUDE, MAX_SCALE_MAGNITUDE);
    let sign_x = if sx0 < 0.0 { -1.0 } else { 1.0 };
    let sign_y = if sy0 < 0.0 { -1.0 } else { 1.0 };
    (
        sign_x * (sx0.abs() * uniform).clamp(MIN_SCALE_MAGNITUDE, MAX_SCALE_MAGNITUDE),
        sign_y * (sy0.abs() * uniform).clamp(MIN_SCALE_MAGNITUDE, MAX_SCALE_MAGNITUDE),
    )
}

fn build_scale_outcome(
    snapshot: &PreviewClipDragSnapshot,
    pivot_local: (f64, f64),
    target_scales: (f64, f64),
    alt_centered: bool,
) -> DragUpdateOutcome {
    let scale_delta = (
        target_scales.0 - snapshot.eval.scale_x,
        target_scales.1 - snapshot.eval.scale_y,
    );
    let Ok(scale_x) = shift_animated_param(&snapshot.scale_x, scale_delta.0) else {
        return blocked_outcome("拡大率 X に数式 f(t) が設定されているため直接拡縮できません");
    };
    let Ok(scale_y) = shift_animated_param(&snapshot.scale_y, scale_delta.1) else {
        return blocked_outcome("拡大率 Y に数式 f(t) が設定されているため直接拡縮できません");
    };

    let mut updated = snapshot_to_updated(snapshot);
    updated.scale_x = scale_x;
    updated.scale_y = scale_y;

    if !alt_centered {
        let old_pivot_world = snapshot
            .eval
            .matrix
            .transform_point(pivot_local.0, pivot_local.1);
        let zero_matrix = Matrix2D::from_transform(
            (0.0, 0.0),
            target_scales,
            (snapshot.eval.anchor_x, snapshot.eval.anchor_y),
            snapshot.eval.rotation_deg,
        );
        let offset = zero_matrix.transform_point(pivot_local.0, pivot_local.1);
        let next_pos = (old_pivot_world.0 - offset.0, old_pivot_world.1 - offset.1);
        let dx = next_pos.0 - snapshot.eval.x;
        let dy = next_pos.1 - snapshot.eval.y;
        let Ok(x) = shift_animated_param(&snapshot.x, dx) else {
            return blocked_outcome("X 座標に数式 f(t) が設定されているため片側固定拡縮できません");
        };
        let Ok(y) = shift_animated_param(&snapshot.y, dy) else {
            return blocked_outcome("Y 座標に数式 f(t) が設定されているため片側固定拡縮できません");
        };
        updated.x = x;
        updated.y = y;
    }

    DragUpdateOutcome {
        updated: Some(updated),
        notice: None,
    }
}

fn compute_rotate_update(
    snapshot: &PreviewClipDragSnapshot,
    start_canvas: (f64, f64),
    cur_canvas: (f64, f64),
    snap_15deg: bool,
) -> DragUpdateOutcome {
    let ax = snapshot.eval.x;
    let ay = snapshot.eval.y;
    let theta_start = (start_canvas.1 - ay)
        .atan2(start_canvas.0 - ax)
        .to_degrees();
    let theta_cur = (cur_canvas.1 - ay).atan2(cur_canvas.0 - ax).to_degrees();
    let raw_delta = normalize_signed_angle_deg(theta_cur - theta_start);
    let target_rot = if snap_15deg {
        ((snapshot.eval.rotation_deg + raw_delta) / 15.0).round() * 15.0
    } else {
        snapshot.eval.rotation_deg + raw_delta
    };
    let delta_rot = target_rot - snapshot.eval.rotation_deg;
    let Ok(rotation_deg) = shift_animated_param(&snapshot.rotation_deg, delta_rot) else {
        return blocked_outcome("回転角に数式 f(t) が設定されているため直接回転できません");
    };
    let mut updated = snapshot_to_updated(snapshot);
    updated.rotation_deg = rotation_deg;
    DragUpdateOutcome {
        updated: Some(updated),
        notice: None,
    }
}

fn compute_anchor_update(
    snapshot: &PreviewClipDragSnapshot,
    canvas_delta: (f64, f64),
) -> DragUpdateOutcome {
    let sx = if snapshot.eval.scale_x.abs() > 1e-4 {
        snapshot.eval.scale_x
    } else {
        1.0
    };
    let sy = if snapshot.eval.scale_y.abs() > 1e-4 {
        snapshot.eval.scale_y
    } else {
        1.0
    };
    let (sin_r, cos_r) = snapshot.eval.rotation_deg.to_radians().sin_cos();
    let unrot = (
        cos_r * canvas_delta.0 + sin_r * canvas_delta.1,
        -sin_r * canvas_delta.0 + cos_r * canvas_delta.1,
    );
    let d_anchor_x = unrot.0 / sx;
    let d_anchor_y = unrot.1 / sy;

    let Ok(x) = shift_animated_param(&snapshot.x, canvas_delta.0) else {
        return blocked_outcome("X 座標に数式 f(t) が設定されているためアンカーを移動できません");
    };
    let Ok(y) = shift_animated_param(&snapshot.y, canvas_delta.1) else {
        return blocked_outcome("Y 座標に数式 f(t) が設定されているためアンカーを移動できません");
    };
    let Ok(anchor_x) = shift_animated_param(&snapshot.anchor_x, d_anchor_x) else {
        return blocked_outcome("アンカー X に数式 f(t) が設定されているため直接移動できません");
    };
    let Ok(anchor_y) = shift_animated_param(&snapshot.anchor_y, d_anchor_y) else {
        return blocked_outcome("アンカー Y に数式 f(t) が設定されているため直接移動できません");
    };

    let mut updated = snapshot_to_updated(snapshot);
    updated.x = x;
    updated.y = y;
    updated.anchor_x = anchor_x;
    updated.anchor_y = anchor_y;
    DragUpdateOutcome {
        updated: Some(updated),
        notice: None,
    }
}

fn normalize_signed_angle_deg(deg: f64) -> f64 {
    let mut wrapped = (deg + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped < -180.0 {
        wrapped += 360.0;
    }
    wrapped
}

/// 更新済み視覚パラメータと初期ドキュメントとの差分から最小の [`EditCommand`] 列を生成する。
#[must_use]
pub fn preview_visual_edit_commands(
    doc: &ProjectDocument,
    clip_id: &ClipId,
    updated: &UpdatedClipVisualParams,
) -> Vec<EditCommand> {
    let Some((_, clip)) = doc.find_clip(clip_id) else {
        return Vec::new();
    };
    let Some(orig) = clip_visual_props(clip) else {
        return Vec::new();
    };
    let pairs = [
        (ParameterPath::X, &orig.x, &updated.x),
        (ParameterPath::Y, &orig.y, &updated.y),
        (ParameterPath::ScaleX, &orig.scale_x, &updated.scale_x),
        (ParameterPath::ScaleY, &orig.scale_y, &updated.scale_y),
        (
            ParameterPath::RotationDeg,
            &orig.rotation_deg,
            &updated.rotation_deg,
        ),
        (ParameterPath::AnchorX, &orig.anchor_x, &updated.anchor_x),
        (ParameterPath::AnchorY, &orig.anchor_y, &updated.anchor_y),
    ];
    let mut cmds = Vec::new();
    for (path, before, after) in pairs {
        if before != after {
            cmds.push(EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: crate::core::ids::ClipRef::Id(clip_id.clone()),
                },
                path,
                value: ParameterValue::Animated {
                    param: after.clone(),
                },
            });
        }
    }
    cmds
}

/// ホイールスクロール方向（+1: 拡大, -1: 縮小）に応じてプリセット倍率を 1 段階進める。
#[must_use]
pub fn step_zoom_preset(
    current: PreviewZoomMode,
    fit_scale: f32,
    delta_sign: i32,
) -> PreviewZoomMode {
    const STEPS: [u16; 8] = [25, 50, 75, 100, 150, 200, 300, 400];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cur_pct = match current {
        PreviewZoomMode::Fit => ((fit_scale * 100.0).round().clamp(25.0, 400.0)) as u16,
        PreviewZoomMode::Percent(pct) => pct,
    };
    match delta_sign.cmp(&0) {
        std::cmp::Ordering::Greater => {
            let next = STEPS.iter().copied().find(|&s| s > cur_pct).unwrap_or(400);
            PreviewZoomMode::Percent(next)
        }
        std::cmp::Ordering::Less => {
            let prev = STEPS.iter().copied().rfind(|&s| s < cur_pct).unwrap_or(25);
            PreviewZoomMode::Percent(prev)
        }
        std::cmp::Ordering::Equal => current,
    }
}

/// カーソル直下のキャンバス座標を固定したままズーム倍率とパンオフセットを同時更新する。
#[must_use]
pub fn zoom_around_cursor(
    bounds: (f32, f32, f32, f32),
    proj_size: (u32, u32),
    old_zoom: PreviewZoomMode,
    old_pan: (f32, f32),
    new_zoom: PreviewZoomMode,
    cursor_window: (f32, f32),
) -> (f32, f32) {
    let old_tf = PreviewViewportTransform {
        bounds,
        proj_size,
        zoom: old_zoom,
        pan_px: old_pan,
    };
    let canvas_pt = old_tf.screen_to_canvas_px(cursor_window.0, cursor_window.1);
    let new_tf = PreviewViewportTransform {
        bounds,
        proj_size,
        zoom: new_zoom,
        pan_px: old_pan,
    };
    let after_screen = new_tf.canvas_px_to_screen(canvas_pt.0, canvas_pt.1);
    (
        old_pan.0 + (cursor_window.0 - after_screen.0),
        old_pan.1 + (cursor_window.1 - after_screen.1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::animation::EasingCurve;
    use crate::core::ids::AssetId;
    use crate::core::time::{FrameCount, FrameRate};
    use crate::timeline::document::{Asset, AssetPath, Layer};

    fn sample_image_doc(
        locked_front: bool,
    ) -> Result<(ProjectDocument, ClipId, ClipId), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("test");
        doc.fps = FrameRate::FPS_30;
        let asset_id = AssetId::new("asset-img")?;
        doc.assets.push(Asset {
            id: asset_id.clone(),
            name: "box.png".to_owned(),
            path: AssetPath::new(Some("box.png".to_owned()), None),
            kind: AssetKind::Image {
                width: 200,
                height: 100,
            },
            status: AssetStatus::Available,
        });

        let back_id = ClipId::new("clip-back")?;
        let front_id = ClipId::new("clip-front")?;
        let mut l0 = Layer::new(LayerId::new("layer-0")?, "Back");
        let dur = FrameCount::new(60)?;
        l0.clips.push(Clip {
            id: back_id.clone(),
            name: "Back".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: dur,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id: asset_id.clone(),
                visual: VisualProperties::default(),
            },
        });
        let mut l1 = Layer::new(LayerId::new("layer-1")?, "Front");
        l1.locked = locked_front;
        let front_vis = VisualProperties {
            rotation_deg: AnimatedParameter::constant(45.0),
            ..VisualProperties::default()
        };
        l1.clips.push(Clip {
            id: front_id.clone(),
            name: "Front".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: dur,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Image {
                asset_id,
                visual: front_vis,
            },
        });
        doc.layers = vec![l0, l1];
        Ok((doc, back_id, front_id))
    }

    #[test]
    fn reverse_z_hit_test_and_lock_passthrough() -> Result<(), Box<dyn std::error::Error>> {
        let viewport = PreviewViewportTransform {
            bounds: (0.0, 0.0, 1920.0, 1080.0),
            proj_size: (1920, 1080),
            zoom: PreviewZoomMode::Percent(100),
            pan_px: (0.0, 0.0),
        };
        let (doc_unlocked, _back_id, front_id) = sample_image_doc(false)?;
        let quads_unlocked = build_preview_hit_quads(&doc_unlocked, FrameIndex::ZERO, &viewport);
        let hit = hit_test_preview_layers(&quads_unlocked, 960.0, 540.0);
        assert_eq!(hit.map(|q| &q.clip_id), Some(&front_id));

        let (doc_locked, back_id, _) = sample_image_doc(true)?;
        let quads_locked = build_preview_hit_quads(&doc_locked, FrameIndex::ZERO, &viewport);
        let hit_locked = hit_test_preview_layers(&quads_locked, 960.0, 540.0);
        assert_eq!(hit_locked.map(|q| &q.clip_id), Some(&back_id));
        Ok(())
    }

    #[test]
    fn anchor_drag_keeps_world_corners_fixed() -> Result<(), Box<dyn std::error::Error>> {
        let vis = VisualProperties {
            x: AnimatedParameter::constant(100.0),
            y: AnimatedParameter::constant(-40.0),
            scale_x: AnimatedParameter::constant(2.0),
            scale_y: AnimatedParameter::constant(1.5),
            rotation_deg: AnimatedParameter::constant(30.0),
            ..VisualProperties::default()
        };

        let eval = EvaluatedVisual::from_components(
            100.0,
            -40.0,
            2.0,
            1.5,
            30.0,
            0.0,
            0.0,
            1.0,
            crate::media::visual_properties::BlendMode::Normal,
        );
        let snap = PreviewClipDragSnapshot {
            clip_id: ClipId::new("c")?,
            x: vis.x,
            y: vis.y,
            scale_x: vis.scale_x,
            scale_y: vis.scale_y,
            rotation_deg: vis.rotation_deg,
            anchor_x: vis.anchor_x,
            anchor_y: vis.anchor_y,
            eval,
            source_size: (200, 100),
        };
        let outcome = compute_anchor_update(&snap, (25.0, -15.0));
        assert!(outcome.updated.is_some());
        if let Some(updated) = outcome.updated {
            assert!(matches!(updated.x, AnimatedParameter::Constant { .. }));
            if let (
                AnimatedParameter::Constant { value: nx },
                AnimatedParameter::Constant { value: ny },
                AnimatedParameter::Constant { value: ax },
                AnimatedParameter::Constant { value: ay },
            ) = (updated.x, updated.y, updated.anchor_x, updated.anchor_y)
            {
                let m_new = Matrix2D::from_transform((nx, ny), (2.0, 1.5), (ax, ay), 30.0);
                let old_tl = eval.matrix.transform_point(-100.0, -50.0);
                let new_tl = m_new.transform_point(-100.0, -50.0);
                assert!((old_tl.0 - new_tl.0).abs() < 1e-6);
                assert!((old_tl.1 - new_tl.1).abs() < 1e-6);
            }
        }
        Ok(())
    }

    #[test]
    fn shift_animated_param_parallel_and_expression_guard() {
        let easing = AnimatedParameter::Easing {
            start_value: 10.0,
            end_value: 50.0,
            curve: EasingCurve::EaseInOut,
            extrapolation: crate::core::animation::ExtrapolationMode::Clamp,
        };
        let shifted = shift_animated_param(&easing, 15.0);
        assert_eq!(
            shifted,
            Ok(AnimatedParameter::Easing {
                start_value: 25.0,
                end_value: 65.0,
                curve: EasingCurve::EaseInOut,
                extrapolation: crate::core::animation::ExtrapolationMode::Clamp,
            })
        );

        let expr = AnimatedParameter::Expression {
            expr: "sin(t)".to_owned(),
            base_offset: 0.0,
            scale: 1.0,
        };
        assert!(shift_animated_param(&expr, 5.0).is_err());
    }

    #[test]
    fn alt_scale_preserves_anchor_world_pos_while_normal_scale_pins_opposite()
    -> Result<(), Box<dyn std::error::Error>> {
        let eval = EvaluatedVisual::from_components(
            0.0,
            0.0,
            1.0,
            1.0,
            0.0,
            0.0,
            0.0,
            1.0,
            crate::media::visual_properties::BlendMode::Normal,
        );
        let snap = PreviewClipDragSnapshot {
            clip_id: ClipId::new("c")?,
            x: AnimatedParameter::constant(0.0),
            y: AnimatedParameter::constant(0.0),
            scale_x: AnimatedParameter::constant(1.0),
            scale_y: AnimatedParameter::constant(1.0),
            rotation_deg: AnimatedParameter::constant(0.0),
            anchor_x: AnimatedParameter::constant(0.0),
            anchor_y: AnimatedParameter::constant(0.0),
            eval,
            source_size: (200, 100),
        };

        let alt_res = compute_scale_update(
            &snap,
            PreviewHandleKind::ScaleR,
            (50.0, 0.0),
            DragModifiers {
                alt: true,
                ..DragModifiers::default()
            },
            false,
        );
        assert!(alt_res.updated.is_some());
        if let Some(updated) = alt_res.updated {
            assert_eq!(updated.x, AnimatedParameter::constant(0.0));
            assert_eq!(updated.scale_x, AnimatedParameter::constant(1.5));
        }

        let pin_res = compute_scale_update(
            &snap,
            PreviewHandleKind::ScaleR,
            (100.0, 0.0),
            DragModifiers::default(),
            false,
        );
        assert!(pin_res.updated.is_some());
        if let Some(updated) = pin_res.updated {
            assert_eq!(updated.scale_x, AnimatedParameter::constant(1.5));
            assert_eq!(updated.x, AnimatedParameter::constant(50.0));
        }
        Ok(())
    }

    #[test]
    fn rotate_shift_snaps_to_15_degrees() -> Result<(), Box<dyn std::error::Error>> {
        let viewport = PreviewViewportTransform {
            bounds: (0.0, 0.0, 1920.0, 1080.0),
            proj_size: (1920, 1080),
            zoom: PreviewZoomMode::Percent(100),
            pan_px: (0.0, 0.0),
        };
        let eval = EvaluatedVisual::from_components(
            0.0,
            0.0,
            1.0,
            1.0,
            0.0,
            0.0,
            0.0,
            1.0,
            crate::media::visual_properties::BlendMode::Normal,
        );
        let snap = PreviewClipDragSnapshot {
            clip_id: ClipId::new("c")?,
            x: AnimatedParameter::constant(0.0),
            y: AnimatedParameter::constant(0.0),
            scale_x: AnimatedParameter::constant(1.0),
            scale_y: AnimatedParameter::constant(1.0),
            rotation_deg: AnimatedParameter::constant(0.0),
            anchor_x: AnimatedParameter::constant(0.0),
            anchor_y: AnimatedParameter::constant(0.0),
            eval,
            source_size: (200, 100),
        };
        let drag = PreviewDragState {
            mode: PreviewDragMode::RotateHandle,
            primary_clip_id: Some(snap.clip_id.clone()),
            start_mouse_window: viewport.canvas_px_to_screen(0.0, -100.0),
            start_pan_px: (0.0, 0.0),
            initial_clips: vec![snap.clone()],
            has_moved: true,
        };
        // ~17度付近へ回転し Shift で 15度へスナップされることを検証
        let rad = 17.0_f64.to_radians();
        let cur = viewport.canvas_px_to_screen(100.0 * rad.sin(), -100.0 * rad.cos());
        let out = compute_drag_updated_clip(
            &drag,
            &snap,
            &viewport,
            cur,
            DragModifiers {
                shift: true,
                ..DragModifiers::default()
            },
            false,
        );
        assert!(out.updated.is_some());
        if let Some(updated) = out.updated {
            assert_eq!(updated.rotation_deg, AnimatedParameter::constant(15.0));
        }
        Ok(())
    }
}
