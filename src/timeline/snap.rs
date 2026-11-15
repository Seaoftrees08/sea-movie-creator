//! タイムライン編集時のフレームスナップ判定・衝突チェック・可視クリップ間引き計算。

use crate::core::ids::{ClipId, LayerId};
use crate::core::time::{FrameCount, FrameIndex};
use crate::timeline::document::ProjectDocument;

/// スナップ吸着の設定値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapConfig {
    /// スナップ機能の有効/無効。
    pub enabled: bool,
    /// 吸着判定の最大許容誤差（フレーム数）。
    pub threshold_frames: u64,
    /// 再生ヘッド位置へスナップするか。
    pub snap_to_playhead: bool,
    /// 他クリップの開始端・終了端へスナップするか。
    pub snap_to_clip_edges: bool,
}

impl Default for SnapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_frames: 8,
            snap_to_playhead: true,
            snap_to_clip_edges: true,
        }
    }
}

/// スナップ吸着先の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapEdgeKind {
    /// タイムライン原点 (`0f`)。
    TimelineOrigin,
    /// 現在再生ヘッド位置。
    Playhead,
    /// 他クリップの開始フレーム。
    ClipStart,
    /// 他クリップの終了フレーム。
    ClipEnd,
    /// 他クリップの中間点フレーム。
    Midpoint,
    /// 選択範囲 `[start, end)` の境界フレーム。
    SelectionBoundary,
    /// タイムライン総尺終端フレーム。
    TimelineEnd,
}

/// 単一フレーム位置に対するスナップ候補解決結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapMatch {
    /// 吸着先フレーム位置。
    pub frame: u64,
    /// 元座標からの差分フレーム数。
    pub distance: u64,
    /// 吸着先種別。
    pub kind: SnapEdgeKind,
}

/// クリップ移動時のスナップ解決結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapMoveResult {
    /// 補正後のクリップ開始フレーム。
    pub snapped_start: FrameIndex,
    /// UI にスナップガイド縦線を表示する基準フレーム位置（吸着時のみ `Some`）。
    pub guide_frame: Option<u64>,
    /// 吸着先の種別（吸着時のみ `Some`）。
    pub edge_kind: Option<SnapEdgeKind>,
}

/// 指定タイムライン内のスナップ基準点（原点・再生ヘッド・他クリップ両端・中間点・選択範囲・総尺終端）を収集する。
#[must_use]
pub fn collect_snap_anchors(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    excluded_clip_ids: &[ClipId],
    config: &SnapConfig,
) -> Vec<(u64, SnapEdgeKind)> {
    let mut anchors = vec![(0, SnapEdgeKind::TimelineOrigin)];
    if config.snap_to_playhead {
        anchors.push((playhead.get(), SnapEdgeKind::Playhead));
    }
    if config.snap_to_clip_edges {
        for layer in &doc.layers {
            for clip in &layer.clips {
                if excluded_clip_ids.iter().any(|id| id == &clip.id) {
                    continue;
                }
                let start = clip.timeline_start.get();
                let end = start.saturating_add(clip.duration.get());
                anchors.push((start, SnapEdgeKind::ClipStart));
                anchors.push((end, SnapEdgeKind::ClipEnd));
                for rel in clip.visible_midpoints() {
                    if let Ok(rel_u64) = u64::try_from(rel) {
                        anchors.push((start.saturating_add(rel_u64), SnapEdgeKind::Midpoint));
                    }
                }
            }
        }
    }
    if let Some(range) = doc.selection_range {
        anchors.push((range.start.get(), SnapEdgeKind::SelectionBoundary));
        anchors.push((range.end.get(), SnapEdgeKind::SelectionBoundary));
    }
    anchors.push((doc.total_duration_frames(), SnapEdgeKind::TimelineEnd));
    anchors
}

/// 単一境界座標（トリムハンドル等）に対する最寄りスナップ位置を求める。
#[must_use]
pub fn snap_frame_edge(
    candidate_frame: u64,
    anchors: &[(u64, SnapEdgeKind)],
    threshold_frames: u64,
) -> Option<SnapMatch> {
    let mut best: Option<SnapMatch> = None;
    for &(anchor_frame, kind) in anchors {
        let distance = candidate_frame.abs_diff(anchor_frame);
        if distance <= threshold_frames && best.is_none_or(|prev| distance < prev.distance) {
            best = Some(SnapMatch {
                frame: anchor_frame,
                distance,
                kind,
            });
        }
    }
    best
}

/// クリップ移動時に「左端」と「右端」の両方を対象に最寄りアンカーへスナップする。
#[must_use]
pub fn snap_clip_move(
    doc: &ProjectDocument,
    playhead: FrameIndex,
    duration: FrameCount,
    candidate_start: FrameIndex,
    excluded_clip_ids: &[ClipId],
    config: &SnapConfig,
) -> SnapMoveResult {
    if !config.enabled {
        return SnapMoveResult {
            snapped_start: candidate_start,
            guide_frame: None,
            edge_kind: None,
        };
    }
    let anchors = collect_snap_anchors(doc, playhead, excluded_clip_ids, config);
    let cand_start = candidate_start.get();
    let cand_end = cand_start.saturating_add(duration.get());

    let left_hit = snap_frame_edge(cand_start, &anchors, config.threshold_frames);
    let right_hit = snap_frame_edge(cand_end, &anchors, config.threshold_frames)
        .filter(|m| m.frame >= duration.get());

    match (left_hit, right_hit) {
        (Some(l), Some(r)) if r.distance < l.distance => SnapMoveResult {
            snapped_start: FrameIndex::new(r.frame.saturating_sub(duration.get())),
            guide_frame: Some(r.frame),
            edge_kind: Some(r.kind),
        },
        (Some(l), _) => SnapMoveResult {
            snapped_start: FrameIndex::new(l.frame),
            guide_frame: Some(l.frame),
            edge_kind: Some(l.kind),
        },
        (None, Some(r)) => SnapMoveResult {
            snapped_start: FrameIndex::new(r.frame.saturating_sub(duration.get())),
            guide_frame: Some(r.frame),
            edge_kind: Some(r.kind),
        },
        (None, None) => SnapMoveResult {
            snapped_start: candidate_start,
            guide_frame: None,
            edge_kind: None,
        },
    }
}

/// 指定レイヤー上の `[start, start + duration)` 区間が他クリップと重複しないか判定する。
#[must_use]
pub fn is_layer_interval_free(
    doc: &ProjectDocument,
    layer_id: &LayerId,
    start: u64,
    duration: u64,
    excluded_clip: Option<&ClipId>,
) -> bool {
    let Some(layer) = doc.find_layer(layer_id) else {
        return false;
    };
    let end = start.saturating_add(duration);
    !layer.clips.iter().any(|clip| {
        if excluded_clip.is_some_and(|ex| ex == &clip.id) {
            return false;
        }
        let c_start = clip.timeline_start.get();
        let c_end = c_start.saturating_add(clip.duration.get());
        start < c_end && c_start < end
    })
}

/// 可視フレーム窓 `[visible_start, visible_end)` と交差するクリップだけを抽出する。
#[must_use]
pub const fn clip_intersects_window(
    clip_start: u64,
    clip_duration: u64,
    visible_start: u64,
    visible_end: u64,
) -> bool {
    let clip_end = clip_start.saturating_add(clip_duration);
    clip_start < visible_end && visible_start < clip_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::ClipId;
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::{Clip, ClipKind, Layer};

    fn build_two_clip_doc() -> Result<(ProjectDocument, ClipId, ClipId), Box<dyn std::error::Error>>
    {
        let mut doc = ProjectDocument::new("snap_test");
        let l1 = LayerId::new("l1")?;
        let c1 = ClipId::new("c1")?;
        let c2 = ClipId::new("c2")?;
        let mut layer = Layer::new(l1, "T1");
        layer.clips.push(Clip {
            id: c1.clone(),
            name: "First".to_owned(),
            timeline_start: FrameIndex::new(100),
            duration: FrameCount::new(50)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("A")),
        });
        layer.clips.push(Clip {
            id: c2.clone(),
            name: "Second".to_owned(),
            timeline_start: FrameIndex::new(220),
            duration: FrameCount::new(40)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("B")),
        });
        doc.layers.push(layer);
        Ok((doc, c1, c2))
    }

    #[test]
    fn snaps_both_left_and_right_clip_edges_and_playhead() -> Result<(), Box<dyn std::error::Error>>
    {
        let (doc, _c1, c2) = build_two_clip_doc()?;
        let cfg = SnapConfig::default();

        // 右端 (145 + 40 = 185) ではなく左端 (153 -> 150 = First.end) へ吸着
        let left_snap = snap_clip_move(
            &doc,
            FrameIndex::new(10),
            FrameCount::new(40)?,
            FrameIndex::new(153),
            std::slice::from_ref(&c2),
            &cfg,
        );
        assert_eq!(left_snap.snapped_start, FrameIndex::new(150));
        assert_eq!(left_snap.guide_frame, Some(150));
        assert_eq!(left_snap.edge_kind, Some(SnapEdgeKind::ClipEnd));

        // クリップ右端 (57 + 40 = 97) が First.start (100) へ吸着 -> start = 60
        let right_snap = snap_clip_move(
            &doc,
            FrameIndex::new(10),
            FrameCount::new(40)?,
            FrameIndex::new(57),
            &[c2],
            &cfg,
        );
        assert_eq!(right_snap.snapped_start, FrameIndex::new(60));
        assert_eq!(right_snap.guide_frame, Some(100));
        assert_eq!(right_snap.edge_kind, Some(SnapEdgeKind::ClipStart));
        Ok(())
    }

    #[test]
    fn collects_midpoint_selection_range_and_timeline_end_anchors()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::timeline::document::{FrameRange, TimelineLengthMode};

        let (mut doc, _c1, c2) = build_two_clip_doc()?;
        // c1 spans 100..150; add midpoint at relative frame 25 => absolute 125
        doc.layers[0].clips[0].midpoints = vec![25];
        doc.selection_range = Some(FrameRange {
            start: FrameIndex::new(15),
            end: FrameIndex::new(310),
        });
        doc.timeline_length = TimelineLengthMode::Manual {
            end_frame: FrameCount::new(400)?,
        };

        let anchors = collect_snap_anchors(
            &doc,
            FrameIndex::new(10),
            std::slice::from_ref(&c2),
            &SnapConfig::default(),
        );
        assert!(anchors.contains(&(125, SnapEdgeKind::Midpoint)));
        assert!(anchors.contains(&(15, SnapEdgeKind::SelectionBoundary)));
        assert!(anchors.contains(&(310, SnapEdgeKind::SelectionBoundary)));
        assert!(anchors.contains(&(400, SnapEdgeKind::TimelineEnd)));
        Ok(())
    }
}
