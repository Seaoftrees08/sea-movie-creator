//! 逆パッチ（Reverse Patch）ベースのアトミックな Undo/Redo 履歴管理。

use std::mem;

use crate::core::color::RgbaColor;
use crate::core::ids::{AssetId, ClipId, LayerId};
use crate::timeline::command::CommandError;
use crate::timeline::document::{
    Asset, Clip, FrameRange, Layer, ProjectDocument, ProjectResolution, TimelineLengthMode,
};

/// ドキュメント全体のコピーを持たずに変更箇所だけを可逆適用するための最小パッチ単位。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PatchStep {
    SetProjectMeta {
        name: String,
        resolution: ProjectResolution,
        background_color: RgbaColor,
    },
    InsertAsset {
        index: usize,
        asset: Asset,
    },
    RemoveAsset {
        asset_id: AssetId,
    },
    ReplaceAsset {
        asset: Asset,
    },
    InsertLayer {
        index: usize,
        layer: Layer,
    },
    RemoveLayer {
        layer_id: LayerId,
    },
    SetLayerFlags {
        layer_id: LayerId,
        name: String,
        visible: bool,
        muted: bool,
        locked: bool,
    },
    InsertClip {
        layer_id: LayerId,
        index: usize,
        clip: Clip,
    },
    RemoveClip {
        layer_id: LayerId,
        clip_id: ClipId,
    },
    ReplaceClip {
        layer_id: LayerId,
        clip: Clip,
    },
    SetTimelineLength {
        mode: TimelineLengthMode,
    },
    SetSelectionRange {
        range: Option<FrameRange>,
    },
}

/// 逆順（LIFO）でパッチ列を適用し、逆方向操作（Redo 用またはロールバック用）のパッチ列を返す。
///
/// # Errors
///
/// 対象要素の不一致などによりパッチを適用できない場合は [`CommandError`] を返す。
pub(crate) fn apply_patches_reversed(
    doc: &mut ProjectDocument,
    patches: Vec<PatchStep>,
) -> Result<Vec<PatchStep>, CommandError> {
    let mut inverse = Vec::with_capacity(patches.len());
    for step in patches.into_iter().rev() {
        inverse.push(apply_single_patch(doc, step)?);
    }
    Ok(inverse)
}

fn apply_single_patch(
    doc: &mut ProjectDocument,
    step: PatchStep,
) -> Result<PatchStep, CommandError> {
    match step {
        PatchStep::SetProjectMeta {
            name,
            resolution,
            background_color,
        } => Ok(PatchStep::SetProjectMeta {
            name: mem::replace(&mut doc.name, name),
            resolution: mem::replace(&mut doc.resolution, resolution),
            background_color: mem::replace(&mut doc.background_color, background_color),
        }),
        PatchStep::InsertAsset { .. }
        | PatchStep::RemoveAsset { .. }
        | PatchStep::ReplaceAsset { .. } => apply_asset_patch(doc, step),
        PatchStep::InsertLayer { .. }
        | PatchStep::RemoveLayer { .. }
        | PatchStep::SetLayerFlags { .. } => apply_layer_patch(doc, step),
        PatchStep::InsertClip { .. }
        | PatchStep::RemoveClip { .. }
        | PatchStep::ReplaceClip { .. } => apply_clip_patch(doc, step),
        PatchStep::SetTimelineLength { mode } => {
            let prev = mem::replace(&mut doc.timeline_length, mode);
            Ok(PatchStep::SetTimelineLength { mode: prev })
        }
        PatchStep::SetSelectionRange { range } => {
            let prev = mem::replace(&mut doc.selection_range, range);
            Ok(PatchStep::SetSelectionRange { range: prev })
        }
    }
}

fn apply_asset_patch(
    doc: &mut ProjectDocument,
    step: PatchStep,
) -> Result<PatchStep, CommandError> {
    match step {
        PatchStep::InsertAsset { index, asset } => {
            let asset_id = asset.id.clone();
            let pos = index.min(doc.assets.len());
            doc.assets.insert(pos, asset);
            Ok(PatchStep::RemoveAsset { asset_id })
        }
        PatchStep::RemoveAsset { asset_id } => {
            let pos = doc
                .assets
                .iter()
                .position(|a| a.id == asset_id)
                .ok_or_else(|| CommandError::NotFound(asset_id.to_string()))?;
            let removed = doc.assets.remove(pos);
            Ok(PatchStep::InsertAsset {
                index: pos,
                asset: removed,
            })
        }
        PatchStep::ReplaceAsset { asset } => {
            let slot = doc
                .find_asset_mut(&asset.id)
                .ok_or_else(|| CommandError::NotFound(asset.id.to_string()))?;
            let old = mem::replace(slot, asset);
            Ok(PatchStep::ReplaceAsset { asset: old })
        }
        _ => unreachable!(),
    }
}

fn apply_layer_patch(
    doc: &mut ProjectDocument,
    step: PatchStep,
) -> Result<PatchStep, CommandError> {
    match step {
        PatchStep::InsertLayer { index, layer } => {
            let layer_id = layer.id.clone();
            let pos = index.min(doc.layers.len());
            doc.layers.insert(pos, layer);
            Ok(PatchStep::RemoveLayer { layer_id })
        }
        PatchStep::RemoveLayer { layer_id } => {
            let pos = doc
                .layers
                .iter()
                .position(|l| l.id == layer_id)
                .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
            let removed = doc.layers.remove(pos);
            Ok(PatchStep::InsertLayer {
                index: pos,
                layer: removed,
            })
        }
        PatchStep::SetLayerFlags {
            layer_id,
            name,
            visible,
            muted,
            locked,
        } => {
            let layer = doc
                .find_layer_mut(&layer_id)
                .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
            Ok(PatchStep::SetLayerFlags {
                layer_id,
                name: mem::replace(&mut layer.name, name),
                visible: mem::replace(&mut layer.visible, visible),
                muted: mem::replace(&mut layer.muted, muted),
                locked: mem::replace(&mut layer.locked, locked),
            })
        }
        _ => unreachable!(),
    }
}

fn apply_clip_patch(doc: &mut ProjectDocument, step: PatchStep) -> Result<PatchStep, CommandError> {
    match step {
        PatchStep::InsertClip {
            layer_id,
            index,
            clip,
        } => {
            let layer = doc
                .find_layer_mut(&layer_id)
                .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
            let clip_id = clip.id.clone();
            let pos = index.min(layer.clips.len());
            layer.clips.insert(pos, clip);
            Ok(PatchStep::RemoveClip { layer_id, clip_id })
        }
        PatchStep::RemoveClip { layer_id, clip_id } => {
            let layer = doc
                .find_layer_mut(&layer_id)
                .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
            let pos = layer
                .clips
                .iter()
                .position(|c| c.id == clip_id)
                .ok_or_else(|| CommandError::NotFound(clip_id.to_string()))?;
            let removed = layer.clips.remove(pos);
            Ok(PatchStep::InsertClip {
                layer_id,
                index: pos,
                clip: removed,
            })
        }
        PatchStep::ReplaceClip { layer_id, clip } => {
            let layer = doc
                .find_layer_mut(&layer_id)
                .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
            let slot = layer
                .clips
                .iter_mut()
                .find(|c| c.id == clip.id)
                .ok_or_else(|| CommandError::NotFound(clip.id.to_string()))?;
            let old = mem::replace(slot, clip);
            Ok(PatchStep::ReplaceClip {
                layer_id,
                clip: old,
            })
        }
        _ => unreachable!(),
    }
}

/// 1 回のユーザー操作または `Batch` コマンドに対応する履歴エントリ。
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// UI 表示・ログ向けの操作概要。
    pub label: String,
    pub(crate) patches: Vec<PatchStep>,
}

/// 逆パッチスタックを用いた Undo/Redo 履歴マネージャ。
#[derive(Debug, Clone)]
pub struct UndoHistory {
    undo_stack: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    max_entries: usize,
}

impl UndoHistory {
    /// 指定した最大保持件数で空の履歴マネージャを作成する。
    #[must_use]
    pub const fn with_capacity(max_entries: usize) -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            max_entries,
        }
    }

    /// 新しい操作を Undo スタックへ積み、Redo スタックを破棄する。
    pub(crate) fn push(&mut self, entry: HistoryEntry) {
        self.redo_stack.clear();
        self.undo_stack.push(entry);
        if self.max_entries > 0 && self.undo_stack.len() > self.max_entries {
            let excess = self.undo_stack.len() - self.max_entries;
            self.undo_stack.drain(0..excess);
        }
    }

    /// Undo 可能かどうか。
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Redo 可能かどうか。
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 次に Undo される操作のラベルを返す。
    #[must_use]
    pub fn peek_undo_label(&self) -> Option<&str> {
        self.undo_stack.last().map(|entry| entry.label.as_str())
    }

    /// 次に Redo される操作のラベルを返す。
    #[must_use]
    pub fn peek_redo_label(&self) -> Option<&str> {
        self.redo_stack.last().map(|entry| entry.label.as_str())
    }

    /// 保持している Undo 件数を返す。
    #[must_use]
    pub fn undo_len(&self) -> usize {
        self.undo_stack.len()
    }

    /// 保持している Redo 件数を返す。
    #[must_use]
    pub fn redo_len(&self) -> usize {
        self.redo_stack.len()
    }

    /// 履歴をすべて消去する。
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// 直前の操作を取り消し、取り消された操作ラベルを返す。履歴が空なら `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 逆パッチの適用または検証に失敗した場合 [`CommandError`] を返す。
    pub fn undo(&mut self, doc: &mut ProjectDocument) -> Result<Option<String>, CommandError> {
        let Some(entry) = self.undo_stack.pop() else {
            return Ok(None);
        };
        let label = entry.label;
        let redo_patches = apply_patches_reversed(doc, entry.patches)?;
        self.redo_stack.push(HistoryEntry {
            label: label.clone(),
            patches: redo_patches,
        });
        doc.validate()?;
        Ok(Some(label))
    }

    /// 取り消した直前の操作をやり直し、適用された操作ラベルを返す。履歴が空なら `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 再適用パッチまたは検証に失敗した場合 [`CommandError`] を返す。
    pub fn redo(&mut self, doc: &mut ProjectDocument) -> Result<Option<String>, CommandError> {
        let Some(entry) = self.redo_stack.pop() else {
            return Ok(None);
        };
        let label = entry.label;
        let undo_patches = apply_patches_reversed(doc, entry.patches)?;
        self.undo_stack.push(HistoryEntry {
            label: label.clone(),
            patches: undo_patches,
        });
        doc.validate()?;
        Ok(Some(label))
    }
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self::with_capacity(500)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::time::{FrameCount, FrameIndex};
    use crate::media::visual_properties::SubtitleProperties;
    use crate::timeline::document::ClipKind;

    #[test]
    fn reverse_patch_undo_and_redo_restore_exact_state() -> Result<(), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("History Test");
        let layer_id = LayerId::new("l1")?;
        doc.layers.push(Layer::new(layer_id.clone(), "Track 1"));

        let clip = Clip {
            id: ClipId::new("c1")?,
            name: "Title".to_owned(),
            timeline_start: FrameIndex::new(10),
            duration: FrameCount::new(60)?,
            effect_offset_frames: 0,
            effect_duration_frames: None,
            midpoints: Vec::new(),
            kind: ClipKind::Subtitle(SubtitleProperties::new("V1")),
        };

        doc.layers[0].clips.push(clip.clone());
        let undo_insert = PatchStep::RemoveClip {
            layer_id: layer_id.clone(),
            clip_id: clip.id.clone(),
        };

        let mut updated_clip = clip.clone();
        updated_clip.name = "Renamed".to_owned();
        let undo_rename = apply_single_patch(
            &mut doc,
            PatchStep::ReplaceClip {
                layer_id,
                clip: updated_clip.clone(),
            },
        )?;

        let mut history = UndoHistory::default();
        history.push(HistoryEntry {
            label: "Insert".to_owned(),
            patches: vec![undo_insert],
        });
        history.push(HistoryEntry {
            label: "Rename".to_owned(),
            patches: vec![undo_rename],
        });

        assert_eq!(doc.layers[0].clips[0].name, "Renamed");
        history.undo(&mut doc)?;
        assert_eq!(doc.layers[0].clips[0].name, "Title");
        history.undo(&mut doc)?;
        assert_eq!(doc.layers[0].clips.len(), 0);

        history.redo(&mut doc)?;
        assert_eq!(doc.layers[0].clips[0].name, "Title");
        history.redo(&mut doc)?;
        assert_eq!(doc.layers[0].clips[0], updated_clip);
        Ok(())
    }
}
