//! 複数クリップのクリップボード保持・重複回避ペースト・複製 (`Ctrl+D`) コマンド生成。

use std::collections::HashSet;

use crate::core::ids::ClipId;
use crate::core::time::FrameIndex;
use crate::timeline::command::{CommandError, EditCommand};
use crate::timeline::document::{Clip, ProjectDocument};
pub use crate::timeline::operations::{ClipClipboard, build_paste_command, copy_clips};

/// コピー済みクリップ群を指定開始フレーム付近へ**既存クリップと衝突しない最初の空き位置**にペーストするコマンドを生成する。
///
/// `preferred_frame` で衝突がない場合はそのまま `preferred_frame` へ貼り付け、
/// 同一レイヤー上に既存クリップがある場合は全アイテムが一括で収まる次の空き位置へ自動で後送りする。
///
/// # Errors
///
/// クリップボードが空、または貼り付け先レイヤーがドキュメント内に不足している場合にエラーを返す。
pub fn build_collision_free_paste_command(
    clipboard: &ClipClipboard,
    doc: &ProjectDocument,
    preferred_frame: FrameIndex,
    base_layer_index: usize,
) -> Result<EditCommand, CommandError> {
    if clipboard.is_empty() {
        return Err(CommandError::Incompatible("clipboard is empty".to_owned()));
    }

    // 元選択の各レイヤーオフセットとフレーム相対位置を検査しながら重複しない開始位置を探索
    let probe = build_paste_command(clipboard, doc, preferred_frame, base_layer_index)?;
    let EditCommand::Batch { commands } = &probe else {
        return Ok(probe);
    };

    let mut placements: Vec<(usize, u64, u64)> = Vec::with_capacity(commands.len());
    for cmd in commands {
        let EditCommand::AddClip { layer_id, clip, .. } = cmd else {
            continue;
        };
        let crate::core::ids::LayerRef::Id(lid) = layer_id else {
            continue;
        };
        let layer_idx = doc
            .layers
            .iter()
            .position(|l| &l.id == lid)
            .ok_or_else(|| CommandError::NotFound(format!("layer `{lid}`")))?;
        let rel_start = clip
            .timeline_start
            .get()
            .saturating_sub(preferred_frame.get());
        placements.push((layer_idx, rel_start, clip.duration.get()));
    }

    let safe_base_frame = find_first_non_overlapping_start(doc, &placements, preferred_frame.get());
    build_paste_command(
        clipboard,
        doc,
        FrameIndex::new(safe_base_frame),
        base_layer_index,
    )
}

/// 選択中の単一または複数クリップ群を、それぞれの相対レイヤー・相対位置を保ったまま複製 (`Ctrl+D`) するコマンドを生成する。
///
/// # Errors
///
/// 空選択・存在しないクリップ ID が含まれる場合に [`CommandError`] を返す。
pub fn build_duplicate_selection_command(
    doc: &ProjectDocument,
    clip_ids: &[ClipId],
) -> Result<EditCommand, CommandError> {
    let clipboard = copy_clips(doc, clip_ids)?;
    let wanted: HashSet<&ClipId> = clip_ids.iter().collect();

    let mut min_layer = usize::MAX;
    let mut max_end = 0_u64;
    for (layer_idx, layer) in doc.layers.iter().enumerate() {
        for clip in &layer.clips {
            if wanted.contains(&clip.id) {
                min_layer = min_layer.min(layer_idx);
                max_end = max_end.max(clip_end(clip));
            }
        }
    }
    if min_layer == usize::MAX {
        return Err(CommandError::NotFound(
            "selected clips were not found".to_owned(),
        ));
    }

    build_collision_free_paste_command(&clipboard, doc, FrameIndex::new(max_end), min_layer)
}

fn find_first_non_overlapping_start(
    doc: &ProjectDocument,
    placements: &[(usize, u64, u64)],
    initial_start: u64,
) -> u64 {
    let mut candidate = initial_start;
    for _ in 0..1024 {
        let mut next_candidate: Option<u64> = None;
        for &(layer_idx, rel_start, duration) in placements {
            let Some(layer) = doc.layers.get(layer_idx) else {
                continue;
            };
            let item_start = candidate.saturating_add(rel_start);
            let item_end = item_start.saturating_add(duration);
            for existing in &layer.clips {
                let ex_start = existing.timeline_start.get();
                let ex_end = clip_end(existing);
                if item_start < ex_end && ex_start < item_end {
                    let pushed = ex_end
                        .saturating_sub(rel_start)
                        .max(candidate.saturating_add(1));
                    next_candidate = Some(next_candidate.map_or(pushed, |cur| cur.max(pushed)));
                }
            }
        }
        match next_candidate {
            Some(next) => candidate = next,
            None => return candidate,
        }
    }
    candidate
}

fn clip_end(clip: &Clip) -> u64 {
    clip.timeline_start
        .get()
        .saturating_add(clip.duration.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ids::{ClipId, LayerId};
    use crate::core::time::FrameCount;
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::dispatcher::CommandDispatcher;
    use crate::timeline::document::{ClipKind, Layer};

    #[test]
    fn collision_free_paste_and_duplicate_skip_occupied_intervals()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("clipboard_test");
        let l1_id = LayerId::new("l1")?;
        let l2_id = LayerId::new("l2")?;
        let c1_id = ClipId::new("c1")?;
        let c_blocker = ClipId::new("blocker")?;
        let c2_id = ClipId::new("c2")?;

        let mut l1 = Layer::new(l1_id, "V1");
        l1.clips.push(Clip {
            id: c1_id.clone(),
            name: "Main".to_owned(),
            timeline_start: FrameIndex::ZERO,
            duration: FrameCount::new(30)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Subtitle(SubtitleProperties::new("One")),
        });
        l1.clips.push(Clip {
            id: c_blocker,
            name: "Blocker".to_owned(),
            timeline_start: FrameIndex::new(30),
            duration: FrameCount::new(40)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Subtitle(SubtitleProperties::new("Block")),
        });

        let mut l2 = Layer::new(l2_id, "V2");
        l2.clips.push(Clip {
            id: c2_id.clone(),
            name: "Sub".to_owned(),
            timeline_start: FrameIndex::new(10),
            duration: FrameCount::new(20)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            kind: ClipKind::Subtitle(SubtitleProperties::new("Two")),
        });
        doc.layers.push(l1);
        doc.layers.push(l2);

        // [0..30) と [10..30) の 2 レイヤー選択を duplicate -> [30..70) の blocker を回避し 70f 以降へ複製されること
        let dup_cmd = build_duplicate_selection_command(&doc, &[c1_id, c2_id])?;
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.dispatch(&mut doc, &dup_cmd)?;

        assert_eq!(doc.layers[0].clips.len(), 3);
        assert_eq!(doc.layers[1].clips.len(), 2);
        assert_eq!(doc.layers[0].clips[2].timeline_start, FrameIndex::new(70));
        assert_eq!(doc.layers[1].clips[1].timeline_start, FrameIndex::new(80));
        Ok(())
    }
}
