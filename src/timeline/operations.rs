//! 個別編集コマンドの実行ロジックおよびコピー/ペースト等のタイムライン操作実装。

use std::collections::{HashMap, HashSet};

use crate::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use crate::core::time::{FrameCount, FrameIndex, PlaybackRate, frames_to_source_seconds};
use crate::media::visual_properties::{SubtitleProperties, VisualProperties};
use crate::timeline::command::{
    CommandError, EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget,
    ParameterValue,
};
use crate::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind, Layer,
    ProjectDocument, ProjectResolution,
};
use crate::timeline::history::PatchStep;

/// バッチコマンド実行中に仮 ID の生成と解決を追跡するスコープ。
#[derive(Debug, Default)]
pub(crate) struct ExecutionContext {
    assets: HashMap<String, AssetId>,
    layers: HashMap<String, LayerId>,
    clips: HashMap<String, ClipId>,
    next_asset_seq: u64,
    next_layer_seq: u64,
    next_clip_seq: u64,
}

impl ExecutionContext {
    pub(crate) fn with_document(doc: &ProjectDocument) -> Self {
        Self {
            assets: HashMap::new(),
            layers: HashMap::new(),
            clips: HashMap::new(),
            next_asset_seq: u64::try_from(doc.assets.len()).unwrap_or(0) + 1,
            next_layer_seq: u64::try_from(doc.layers.len()).unwrap_or(0) + 1,
            next_clip_seq: doc
                .layers
                .iter()
                .map(|l| u64::try_from(l.clips.len()).unwrap_or(0))
                .sum::<u64>()
                + 1,
        }
    }

    pub(crate) fn into_created_ids(
        self,
    ) -> (
        HashMap<String, AssetId>,
        HashMap<String, LayerId>,
        HashMap<String, ClipId>,
    ) {
        (self.assets, self.layers, self.clips)
    }

    fn resolve_asset(&self, reference: &AssetRef) -> Result<AssetId, CommandError> {
        match reference {
            AssetRef::Id(id) => Ok(id.clone()),
            AssetRef::Temp(temp) => self
                .assets
                .get(temp)
                .cloned()
                .ok_or_else(|| CommandError::UnknownTempId(temp.clone())),
        }
    }

    fn resolve_layer(&self, reference: &LayerRef) -> Result<LayerId, CommandError> {
        match reference {
            LayerRef::Id(id) => Ok(id.clone()),
            LayerRef::Temp(temp) => self
                .layers
                .get(temp)
                .cloned()
                .ok_or_else(|| CommandError::UnknownTempId(temp.clone())),
        }
    }

    fn resolve_clip(&self, reference: &ClipRef) -> Result<ClipId, CommandError> {
        match reference {
            ClipRef::Id(id) => Ok(id.clone()),
            ClipRef::Temp(temp) => self
                .clips
                .get(temp)
                .cloned()
                .ok_or_else(|| CommandError::UnknownTempId(temp.clone())),
        }
    }

    fn register_asset_temp(
        &mut self,
        temp_id: Option<&str>,
        id: &AssetId,
    ) -> Result<(), CommandError> {
        if let Some(temp) = validate_optional_temp_id(temp_id)? {
            if self.assets.contains_key(temp) {
                return Err(CommandError::DuplicateTempId(temp.to_owned()));
            }
            self.assets.insert(temp.to_owned(), id.clone());
        }
        Ok(())
    }

    fn register_layer_temp(
        &mut self,
        temp_id: Option<&str>,
        id: &LayerId,
    ) -> Result<(), CommandError> {
        if let Some(temp) = validate_optional_temp_id(temp_id)? {
            if self.layers.contains_key(temp) {
                return Err(CommandError::DuplicateTempId(temp.to_owned()));
            }
            self.layers.insert(temp.to_owned(), id.clone());
        }
        Ok(())
    }

    fn register_clip_temp(
        &mut self,
        temp_id: Option<&str>,
        id: &ClipId,
    ) -> Result<(), CommandError> {
        if let Some(temp) = validate_optional_temp_id(temp_id)? {
            if self.clips.contains_key(temp) {
                return Err(CommandError::DuplicateTempId(temp.to_owned()));
            }
            self.clips.insert(temp.to_owned(), id.clone());
        }
        Ok(())
    }

    fn allocate_asset_id(
        &mut self,
        doc: &ProjectDocument,
        explicit: Option<AssetId>,
    ) -> Result<AssetId, CommandError> {
        if let Some(id) = explicit {
            if doc.find_asset(&id).is_some() {
                return Err(CommandError::AlreadyExists(id.to_string()));
            }
            return Ok(id);
        }
        loop {
            let candidate = AssetId::new(format!("asset_{}", self.next_asset_seq))?;
            self.next_asset_seq += 1;
            if doc.find_asset(&candidate).is_none() {
                return Ok(candidate);
            }
        }
    }

    fn allocate_layer_id(
        &mut self,
        doc: &ProjectDocument,
        explicit: Option<LayerId>,
    ) -> Result<LayerId, CommandError> {
        if let Some(id) = explicit {
            if doc.find_layer(&id).is_some() {
                return Err(CommandError::AlreadyExists(id.to_string()));
            }
            return Ok(id);
        }
        loop {
            let candidate = LayerId::new(format!("layer_{}", self.next_layer_seq))?;
            self.next_layer_seq += 1;
            if doc.find_layer(&candidate).is_none() {
                return Ok(candidate);
            }
        }
    }

    fn allocate_clip_id(
        &mut self,
        doc: &ProjectDocument,
        explicit: Option<ClipId>,
    ) -> Result<ClipId, CommandError> {
        if let Some(id) = explicit {
            if doc.find_clip(&id).is_some() {
                return Err(CommandError::AlreadyExists(id.to_string()));
            }
            return Ok(id);
        }
        loop {
            let candidate = ClipId::new(format!("clip_{}", self.next_clip_seq))?;
            self.next_clip_seq += 1;
            if doc.find_clip(&candidate).is_none() {
                return Ok(candidate);
            }
        }
    }
}

fn validate_optional_temp_id(raw: Option<&str>) -> Result<Option<&str>, CommandError> {
    match raw {
        Some(value) if value.trim().is_empty() => {
            Err(CommandError::Id(crate::core::ids::IdError::Empty))
        }
        other => Ok(other),
    }
}

struct CommandRunner<'doc, 'ctx, 'patch> {
    doc: &'doc mut ProjectDocument,
    ctx: &'ctx mut ExecutionContext,
    undo_patches: &'patch mut Vec<PatchStep>,
}

/// コマンドを実行して逆パッチを `undo_patches` に追記する（エラー時は呼び出し側で巻き戻す）。
///
/// # Errors
///
/// ID 未検出・ロック違反・パラメータ不整合などで実行できない場合 [`CommandError`] を返す。
pub(crate) fn apply_command_recording_patches(
    doc: &mut ProjectDocument,
    command: &EditCommand,
    ctx: &mut ExecutionContext,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let mut runner = CommandRunner {
        doc,
        ctx,
        undo_patches,
    };
    runner.run(command)
}

impl CommandRunner<'_, '_, '_> {
    fn run(&mut self, command: &EditCommand) -> Result<(), CommandError> {
        match command {
            EditCommand::SetProjectMeta {
                name,
                resolution,
                background_color,
            } => {
                self.apply_project_meta(name.as_deref(), *resolution, *background_color);
                Ok(())
            }
            EditCommand::AddAsset { .. }
            | EditCommand::RemoveAsset { .. }
            | EditCommand::RelinkAsset { .. } => self.run_asset_command(command),
            EditCommand::AddLayer { .. }
            | EditCommand::RemoveLayer { .. }
            | EditCommand::SetLayerFlags { .. } => self.run_layer_command(command),
            EditCommand::Batch { commands } => {
                if commands.is_empty() {
                    return Err(CommandError::EmptyBatch);
                }
                for child in commands {
                    self.run(child)?;
                }
                Ok(())
            }
            _ => self.run_clip_command(command),
        }
    }

    fn apply_project_meta(
        &mut self,
        name: Option<&str>,
        resolution: Option<ProjectResolution>,
        background_color: Option<crate::core::color::RgbaColor>,
    ) {
        self.undo_patches.push(PatchStep::SetProjectMeta {
            name: self.doc.name.clone(),
            resolution: self.doc.resolution,
            background_color: self.doc.background_color,
        });
        if let Some(new_name) = name {
            new_name.clone_into(&mut self.doc.name);
        }
        if let Some(new_res) = resolution {
            self.doc.resolution = new_res;
        }
        if let Some(new_bg) = background_color {
            self.doc.background_color = new_bg;
        }
    }

    fn run_asset_command(&mut self, command: &EditCommand) -> Result<(), CommandError> {
        match command {
            EditCommand::AddAsset {
                temp_id,
                id,
                name,
                path,
                kind,
            } => {
                let asset_id = self.ctx.allocate_asset_id(self.doc, id.clone())?;
                self.ctx
                    .register_asset_temp(temp_id.as_deref(), &asset_id)?;
                self.doc.assets.push(Asset {
                    id: asset_id.clone(),
                    name: name.clone(),
                    path: path.clone(),
                    kind: kind.clone(),
                    status: AssetStatus::Available,
                });
                self.undo_patches.push(PatchStep::RemoveAsset { asset_id });
                Ok(())
            }
            EditCommand::RemoveAsset { asset_id } => {
                remove_asset(self.doc, asset_id, self.undo_patches)
            }
            EditCommand::RelinkAsset {
                asset_id,
                new_path,
                new_kind,
            } => relink_asset(
                self.doc,
                asset_id,
                new_path,
                new_kind.as_ref(),
                self.undo_patches,
            ),
            _ => unreachable!(),
        }
    }

    fn run_layer_command(&mut self, command: &EditCommand) -> Result<(), CommandError> {
        match command {
            EditCommand::AddLayer {
                temp_id,
                id,
                name,
                insert_index,
            } => {
                let layer_id = self.ctx.allocate_layer_id(self.doc, id.clone())?;
                self.ctx
                    .register_layer_temp(temp_id.as_deref(), &layer_id)?;
                let pos = insert_index
                    .unwrap_or(self.doc.layers.len())
                    .min(self.doc.layers.len());
                self.doc
                    .layers
                    .insert(pos, Layer::new(layer_id.clone(), name));
                self.undo_patches.push(PatchStep::RemoveLayer { layer_id });
                Ok(())
            }
            EditCommand::RemoveLayer { layer_id } => {
                remove_layer(self.doc, layer_id, self.undo_patches)
            }
            EditCommand::SetLayerFlags {
                layer_id,
                name,
                visible,
                muted,
                locked,
            } => {
                let resolved = self.ctx.resolve_layer(layer_id)?;
                let layer = self
                    .doc
                    .find_layer_mut(&resolved)
                    .ok_or_else(|| CommandError::NotFound(resolved.to_string()))?;
                self.undo_patches.push(PatchStep::SetLayerFlags {
                    layer_id: resolved,
                    name: layer.name.clone(),
                    visible: layer.visible,
                    muted: layer.muted,
                    locked: layer.locked,
                });
                if let Some(v) = name {
                    v.clone_into(&mut layer.name);
                }
                if let Some(v) = visible {
                    layer.visible = *v;
                }
                if let Some(v) = muted {
                    layer.muted = *v;
                }
                if let Some(v) = locked {
                    layer.locked = *v;
                }
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn run_clip_command(&mut self, command: &EditCommand) -> Result<(), CommandError> {
        match command {
            EditCommand::AddClip {
                temp_id,
                id,
                layer_id,
                clip,
            } => self.add_clip(temp_id.as_deref(), id.clone(), layer_id, clip),
            EditCommand::RemoveClip { clip_id } => {
                remove_clip(self.doc, self.ctx, clip_id, self.undo_patches)
            }
            EditCommand::MoveClip {
                clip_id,
                target_layer_id,
                new_timeline_start,
            } => move_clip(
                self.doc,
                self.ctx,
                clip_id,
                target_layer_id.as_ref(),
                *new_timeline_start,
                self.undo_patches,
            ),
            EditCommand::SplitClip {
                clip_id,
                split_at_timeline_frame,
                right_temp_id,
                right_id,
            } => self.split_clip(
                clip_id,
                *split_at_timeline_frame,
                right_temp_id.as_deref(),
                right_id.clone(),
            ),
            EditCommand::TrimLeft {
                clip_id,
                delta_frames,
            } => trim_left(
                self.doc,
                self.ctx,
                clip_id,
                *delta_frames,
                self.undo_patches,
            ),
            EditCommand::TrimRight {
                clip_id,
                delta_frames,
            } => trim_right(
                self.doc,
                self.ctx,
                clip_id,
                *delta_frames,
                self.undo_patches,
            ),
            EditCommand::DuplicateClip {
                clip_id,
                target_layer_id,
                new_timeline_start,
                new_temp_id,
                new_id,
            } => self.duplicate_clip(
                clip_id,
                target_layer_id.as_ref(),
                *new_timeline_start,
                new_temp_id.as_deref(),
                new_id.clone(),
            ),
            EditCommand::SetParameter {
                target,
                path,
                value,
            } => set_parameter(self.doc, self.ctx, target, *path, value, self.undo_patches),
            EditCommand::SetPlaybackRate {
                clip_id,
                new_rate,
                preserve_pitch,
            } => set_playback_rate(
                self.doc,
                self.ctx,
                clip_id,
                *new_rate,
                *preserve_pitch,
                self.undo_patches,
            ),
            _ => unreachable!(),
        }
    }

    fn add_clip(
        &mut self,
        temp_id: Option<&str>,
        explicit_id: Option<ClipId>,
        layer_ref: &LayerRef,
        spec: &NewClipSpec,
    ) -> Result<(), CommandError> {
        let layer_id = self.ctx.resolve_layer(layer_ref)?;
        let clip_id = self.ctx.allocate_clip_id(self.doc, explicit_id)?;
        self.ctx.register_clip_temp(temp_id, &clip_id)?;

        let kind = resolve_clip_kind(&spec.kind, self.ctx)?;
        let clip = Clip {
            id: clip_id.clone(),
            name: spec.name.clone(),
            timeline_start: spec.timeline_start,
            duration: spec.duration,
            effect_offset_frames: spec.effect_offset_frames,
            effect_duration_frames: spec.effect_duration_frames,
            kind,
        };

        insert_clip_sorted(self.doc, &layer_id, clip)?;
        self.undo_patches
            .push(PatchStep::RemoveClip { layer_id, clip_id });
        Ok(())
    }

    fn split_clip(
        &mut self,
        clip_ref: &ClipRef,
        split_at: FrameIndex,
        right_temp_id: Option<&str>,
        right_explicit_id: Option<ClipId>,
    ) -> Result<(), CommandError> {
        let clip_id = self.ctx.resolve_clip(clip_ref)?;
        let right_id = self.ctx.allocate_clip_id(self.doc, right_explicit_id)?;
        self.ctx.register_clip_temp(right_temp_id, &right_id)?;

        let (layer_idx, clip_idx) = locate_clip_mutable(self.doc, &clip_id)?;
        if self.doc.layers[layer_idx].locked {
            return Err(CommandError::LayerLocked(
                self.doc.layers[layer_idx].id.clone(),
            ));
        }

        let left = self.doc.layers[layer_idx].clips[clip_idx].clone();
        let start = left.timeline_start.get();
        let end = left.timeline_end()?.get();
        if split_at.get() <= start || split_at.get() >= end {
            return Err(CommandError::Incompatible(format!(
                "split frame {} must be strictly inside ({start}..{end})",
                split_at.get()
            )));
        }

        let left_len = split_at.get() - start;
        let right_len = end - split_at.get();
        let offset_delta = i64::try_from(left_len)
            .map_err(|_| CommandError::Time(crate::core::time::TimeError::Overflow))?;
        let right_effect_offset = left
            .effect_offset_frames
            .checked_add(offset_delta)
            .ok_or(CommandError::Time(crate::core::time::TimeError::Overflow))?;

        let preserved_effect_duration = left.effective_effect_duration();
        let mut updated_left = left.clone();
        updated_left.duration = FrameCount::new(left_len)?;
        updated_left.effect_duration_frames = Some(preserved_effect_duration);

        let mut right_kind = left.kind.clone();
        advance_source_in(&mut right_kind, left_len, self.doc.fps)?;
        let right = Clip {
            id: right_id.clone(),
            name: format!("{} (2)", left.name),
            timeline_start: split_at,
            duration: FrameCount::new(right_len)?,
            effect_offset_frames: right_effect_offset,
            effect_duration_frames: Some(preserved_effect_duration),
            kind: right_kind,
        };

        let layer_id = self.doc.layers[layer_idx].id.clone();
        self.doc.layers[layer_idx].clips[clip_idx] = updated_left;
        self.undo_patches.push(PatchStep::ReplaceClip {
            layer_id: layer_id.clone(),
            clip: left,
        });
        insert_clip_sorted(self.doc, &layer_id, right)?;
        self.undo_patches.push(PatchStep::RemoveClip {
            layer_id,
            clip_id: right_id,
        });
        Ok(())
    }

    fn duplicate_clip(
        &mut self,
        clip_ref: &ClipRef,
        target_layer_ref: Option<&LayerRef>,
        new_start: FrameIndex,
        new_temp_id: Option<&str>,
        explicit_id: Option<ClipId>,
    ) -> Result<(), CommandError> {
        let src_id = self.ctx.resolve_clip(clip_ref)?;
        let dst_id = self.ctx.allocate_clip_id(self.doc, explicit_id)?;
        self.ctx.register_clip_temp(new_temp_id, &dst_id)?;

        let (src_layer_idx, src_clip_idx) = locate_clip_mutable(self.doc, &src_id)?;
        let dst_layer_id = match target_layer_ref {
            Some(target_ref) => self.ctx.resolve_layer(target_ref)?,
            None => self.doc.layers[src_layer_idx].id.clone(),
        };
        let mut copy = self.doc.layers[src_layer_idx].clips[src_clip_idx].clone();
        copy.id = dst_id.clone();
        copy.timeline_start = new_start;

        insert_clip_sorted(self.doc, &dst_layer_id, copy)?;
        self.undo_patches.push(PatchStep::RemoveClip {
            layer_id: dst_layer_id,
            clip_id: dst_id,
        });
        Ok(())
    }
}

fn remove_asset(
    doc: &mut ProjectDocument,
    asset_id: &AssetId,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let referenced = doc
        .layers
        .iter()
        .flat_map(|layer| layer.clips.iter())
        .any(|clip| clip_asset_id(&clip.kind) == Some(asset_id));
    if referenced {
        return Err(CommandError::AssetInUse(asset_id.clone()));
    }
    let pos = doc
        .assets
        .iter()
        .position(|a| &a.id == asset_id)
        .ok_or_else(|| CommandError::NotFound(asset_id.to_string()))?;
    let removed = doc.assets.remove(pos);
    undo_patches.push(PatchStep::InsertAsset {
        index: pos,
        asset: removed,
    });
    Ok(())
}

fn relink_asset(
    doc: &mut ProjectDocument,
    asset_id: &AssetId,
    new_path: &AssetPath,
    new_kind: Option<&AssetKind>,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let asset = doc
        .find_asset_mut(asset_id)
        .ok_or_else(|| CommandError::NotFound(asset_id.to_string()))?;
    let old_asset = asset.clone();
    asset.path = new_path.clone();
    asset.status = AssetStatus::Available;
    if let Some(kind) = new_kind {
        asset.kind = kind.clone();
    }
    undo_patches.push(PatchStep::ReplaceAsset { asset: old_asset });
    Ok(())
}

fn remove_layer(
    doc: &mut ProjectDocument,
    layer_id: &LayerId,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let pos = doc
        .layers
        .iter()
        .position(|l| &l.id == layer_id)
        .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
    if doc.layers[pos].locked {
        return Err(CommandError::LayerLocked(layer_id.clone()));
    }
    let removed = doc.layers.remove(pos);
    undo_patches.push(PatchStep::InsertLayer {
        index: pos,
        layer: removed,
    });
    Ok(())
}

fn remove_clip(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer = &mut doc.layers[layer_idx];
    if layer.locked {
        return Err(CommandError::LayerLocked(layer.id.clone()));
    }
    let layer_id = layer.id.clone();
    let removed = layer.clips.remove(clip_idx);
    undo_patches.push(PatchStep::InsertClip {
        layer_id,
        index: clip_idx,
        clip: removed,
    });
    Ok(())
}

fn move_clip(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    target_layer_ref: Option<&LayerRef>,
    new_start: FrameIndex,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (src_layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let src_layer_id = doc.layers[src_layer_idx].id.clone();
    if doc.layers[src_layer_idx].locked {
        return Err(CommandError::LayerLocked(src_layer_id));
    }

    let dst_layer_id = match target_layer_ref {
        Some(target_ref) => ctx.resolve_layer(target_ref)?,
        None => src_layer_id.clone(),
    };
    let dst_layer = doc
        .find_layer(&dst_layer_id)
        .ok_or_else(|| CommandError::NotFound(dst_layer_id.to_string()))?;
    if dst_layer.locked {
        return Err(CommandError::LayerLocked(dst_layer_id));
    }

    let mut moved = doc.layers[src_layer_idx].clips.remove(clip_idx);
    undo_patches.push(PatchStep::InsertClip {
        layer_id: src_layer_id,
        index: clip_idx,
        clip: moved.clone(),
    });
    moved.timeline_start = new_start;
    insert_clip_sorted(doc, &dst_layer_id, moved)?;
    undo_patches.push(PatchStep::RemoveClip {
        layer_id: dst_layer_id,
        clip_id,
    });
    Ok(())
}

fn trim_left(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    delta_frames: i64,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let old_clip = doc.layers[layer_idx].clips[clip_idx].clone();
    let preserved_effect_duration = old_clip.effective_effect_duration();
    let mut updated = old_clip.clone();
    updated.timeline_start = old_clip.timeline_start.checked_add_signed(delta_frames)?;
    updated.duration = old_clip.duration.checked_sub_signed(delta_frames)?;
    updated.effect_offset_frames = old_clip
        .effect_offset_frames
        .checked_add(delta_frames)
        .ok_or(CommandError::Time(crate::core::time::TimeError::Overflow))?;
    updated.effect_duration_frames = Some(preserved_effect_duration);
    shift_source_in_signed(&mut updated.kind, delta_frames, doc.fps)?;

    doc.layers[layer_idx].clips[clip_idx] = updated;
    doc.layers[layer_idx].sort_clips();
    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn trim_right(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    delta_frames: i64,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let old_clip = doc.layers[layer_idx].clips[clip_idx].clone();
    let preserved_effect_duration = old_clip.effective_effect_duration();
    let mut updated = old_clip.clone();
    updated.duration = old_clip.duration.checked_sub_signed(delta_frames)?;
    updated.effect_duration_frames = Some(preserved_effect_duration);
    doc.layers[layer_idx].clips[clip_idx] = updated;
    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn set_parameter(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    target: &ParameterTarget,
    path: ParameterPath,
    value: &ParameterValue,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let ParameterTarget::Clip { clip_id: clip_ref } = target;
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let old_clip = doc.layers[layer_idx].clips[clip_idx].clone();
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    apply_clip_parameter(&mut clip.kind, path, value)?;
    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn set_playback_rate(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    new_rate: PlaybackRate,
    preserve_pitch: Option<bool>,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let old_clip = doc.layers[layer_idx].clips[clip_idx].clone();
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    match &mut clip.kind {
        ClipKind::Video {
            playback_rate,
            preserve_pitch: pitch_flag,
            ..
        }
        | ClipKind::Audio {
            playback_rate,
            preserve_pitch: pitch_flag,
            ..
        } => {
            *playback_rate = new_rate;
            if let Some(pitch) = preserve_pitch {
                *pitch_flag = pitch;
            }
        }
        ClipKind::Image { .. } | ClipKind::Subtitle(_) => {
            return Err(CommandError::Incompatible(
                "playback_rate applies only to video and audio clips".to_owned(),
            ));
        }
    }

    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn insert_clip_sorted(
    doc: &mut ProjectDocument,
    layer_id: &LayerId,
    clip: Clip,
) -> Result<(), CommandError> {
    let layer = doc
        .find_layer_mut(layer_id)
        .ok_or_else(|| CommandError::NotFound(layer_id.to_string()))?;
    if layer.locked {
        return Err(CommandError::LayerLocked(layer_id.clone()));
    }
    let pos = layer
        .clips
        .partition_point(|existing| existing.timeline_start <= clip.timeline_start);
    layer.clips.insert(pos, clip);
    Ok(())
}

fn locate_clip_mutable(
    doc: &ProjectDocument,
    clip_id: &ClipId,
) -> Result<(usize, usize), CommandError> {
    for (layer_idx, layer) in doc.layers.iter().enumerate() {
        if let Some(clip_idx) = layer.clips.iter().position(|c| &c.id == clip_id) {
            return Ok((layer_idx, clip_idx));
        }
    }
    Err(CommandError::NotFound(clip_id.to_string()))
}

fn clip_asset_id(kind: &ClipKind) -> Option<&AssetId> {
    match kind {
        ClipKind::Video { asset_id, .. }
        | ClipKind::Audio { asset_id, .. }
        | ClipKind::Image { asset_id, .. } => Some(asset_id),
        ClipKind::Subtitle(_) => None,
    }
}

fn resolve_clip_kind(spec: &NewClipKind, ctx: &ExecutionContext) -> Result<ClipKind, CommandError> {
    match spec {
        NewClipKind::Video {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            visual,
            audio,
        } => Ok(ClipKind::Video {
            asset_id: ctx.resolve_asset(asset_id)?,
            source_in: *source_in,
            playback_rate: *playback_rate,
            preserve_pitch: *preserve_pitch,
            visual: visual.clone(),
            audio: audio.clone(),
        }),
        NewClipKind::Audio {
            asset_id,
            source_in,
            playback_rate,
            preserve_pitch,
            audio,
        } => Ok(ClipKind::Audio {
            asset_id: ctx.resolve_asset(asset_id)?,
            source_in: *source_in,
            playback_rate: *playback_rate,
            preserve_pitch: *preserve_pitch,
            audio: audio.clone(),
        }),
        NewClipKind::Image { asset_id, visual } => Ok(ClipKind::Image {
            asset_id: ctx.resolve_asset(asset_id)?,
            visual: visual.clone(),
        }),
        NewClipKind::Subtitle(sub) => Ok(ClipKind::Subtitle(sub.clone())),
    }
}

fn advance_source_in(
    kind: &mut ClipKind,
    delta_frames: u64,
    fps: crate::core::time::FrameRate,
) -> Result<(), CommandError> {
    match kind {
        ClipKind::Video {
            source_in,
            playback_rate,
            ..
        }
        | ClipKind::Audio {
            source_in,
            playback_rate,
            ..
        } => {
            let delta = frames_to_source_seconds(delta_frames, fps, *playback_rate)?;
            *source_in = source_in.checked_add(delta)?;
            Ok(())
        }
        ClipKind::Image { .. } | ClipKind::Subtitle(_) => Ok(()),
    }
}

fn shift_source_in_signed(
    kind: &mut ClipKind,
    delta_frames: i64,
    fps: crate::core::time::FrameRate,
) -> Result<(), CommandError> {
    match kind {
        ClipKind::Video {
            source_in,
            playback_rate,
            ..
        }
        | ClipKind::Audio {
            source_in,
            playback_rate,
            ..
        } => {
            *source_in = source_in.checked_add_signed_frames(delta_frames, fps, *playback_rate)?;
            Ok(())
        }
        ClipKind::Image { .. } | ClipKind::Subtitle(_) => Ok(()),
    }
}

fn apply_clip_parameter(
    kind: &mut ClipKind,
    path: ParameterPath,
    value: &ParameterValue,
) -> Result<(), CommandError> {
    match path {
        ParameterPath::X
        | ParameterPath::Y
        | ParameterPath::ScaleX
        | ParameterPath::ScaleY
        | ParameterPath::RotationDeg
        | ParameterPath::AnchorX
        | ParameterPath::AnchorY
        | ParameterPath::Opacity
        | ParameterPath::BlendMode => {
            let visual = clip_visual_mut(kind)?;
            apply_visual_parameter(visual, path, value)
        }
        ParameterPath::GainDb | ParameterPath::Pan | ParameterPath::AudioMuted => {
            let audio = clip_audio_mut(kind)?;
            apply_audio_parameter(audio, path, value)
        }
        ParameterPath::PreservePitch => apply_preserve_pitch(kind, value),
        ParameterPath::SubtitleText
        | ParameterPath::SubtitleFontFamily
        | ParameterPath::SubtitleFontSize
        | ParameterPath::SubtitleTextColor
        | ParameterPath::SubtitleOutlineColor
        | ParameterPath::SubtitleOutlineWidth
        | ParameterPath::SubtitleAlignment => {
            let sub = clip_subtitle_mut(kind)?;
            apply_subtitle_parameter(sub, path, value)
        }
    }
}

fn clip_visual_mut(kind: &mut ClipKind) -> Result<&mut VisualProperties, CommandError> {
    match kind {
        ClipKind::Video { visual, .. } | ClipKind::Image { visual, .. } => Ok(visual),
        ClipKind::Subtitle(sub) => Ok(&mut sub.visual),
        ClipKind::Audio { .. } => Err(CommandError::Incompatible(
            "audio clip has no visual properties".to_owned(),
        )),
    }
}

fn clip_audio_mut(kind: &mut ClipKind) -> Result<&mut AudioProperties, CommandError> {
    match kind {
        ClipKind::Audio { audio, .. }
        | ClipKind::Video {
            audio: Some(audio), ..
        } => Ok(audio),
        ClipKind::Video { audio: None, .. } | ClipKind::Image { .. } | ClipKind::Subtitle(_) => {
            Err(CommandError::Incompatible(
                "clip does not carry editable audio properties".to_owned(),
            ))
        }
    }
}

fn clip_subtitle_mut(kind: &mut ClipKind) -> Result<&mut SubtitleProperties, CommandError> {
    match kind {
        ClipKind::Subtitle(sub) => Ok(sub),
        _ => Err(CommandError::Incompatible(
            "target clip is not a subtitle".to_owned(),
        )),
    }
}

fn expect_animated(
    value: &ParameterValue,
) -> Result<&crate::core::animation::AnimatedParameter, CommandError> {
    match value {
        ParameterValue::Animated { param } => {
            if let crate::core::animation::AnimatedParameter::Expression { expr } = param {
                crate::core::expression::CompiledExpression::parse(expr)
                    .map_err(|err| CommandError::Incompatible(err.to_string()))?;
            }
            Ok(param)
        }
        _ => Err(CommandError::Incompatible(
            "expected Animated ParameterValue".to_owned(),
        )),
    }
}

fn apply_visual_parameter(
    visual: &mut VisualProperties,
    path: ParameterPath,
    value: &ParameterValue,
) -> Result<(), CommandError> {
    match path {
        ParameterPath::X => visual.x = expect_animated(value)?.clone(),
        ParameterPath::Y => visual.y = expect_animated(value)?.clone(),
        ParameterPath::ScaleX => visual.scale_x = expect_animated(value)?.clone(),
        ParameterPath::ScaleY => visual.scale_y = expect_animated(value)?.clone(),
        ParameterPath::RotationDeg => visual.rotation_deg = expect_animated(value)?.clone(),
        ParameterPath::AnchorX => visual.anchor_x = expect_animated(value)?.clone(),
        ParameterPath::AnchorY => visual.anchor_y = expect_animated(value)?.clone(),
        ParameterPath::Opacity => visual.opacity = expect_animated(value)?.clone(),
        ParameterPath::BlendMode => {
            let ParameterValue::BlendMode { value: mode } = value else {
                return Err(CommandError::Incompatible(
                    "expected BlendMode ParameterValue".to_owned(),
                ));
            };
            visual.blend_mode = *mode;
        }
        _ => {
            return Err(CommandError::Incompatible(
                "unexpected non-visual parameter path".to_owned(),
            ));
        }
    }
    Ok(())
}

fn apply_audio_parameter(
    audio: &mut AudioProperties,
    path: ParameterPath,
    value: &ParameterValue,
) -> Result<(), CommandError> {
    match path {
        ParameterPath::GainDb => audio.gain_db = expect_animated(value)?.clone(),
        ParameterPath::Pan => audio.pan = expect_animated(value)?.clone(),
        ParameterPath::AudioMuted => {
            let ParameterValue::Bool { value: muted } = value else {
                return Err(CommandError::Incompatible(
                    "expected Bool ParameterValue for audio_muted".to_owned(),
                ));
            };
            audio.muted = *muted;
        }
        _ => {
            return Err(CommandError::Incompatible(
                "unexpected non-audio parameter path".to_owned(),
            ));
        }
    }
    Ok(())
}

fn apply_preserve_pitch(kind: &mut ClipKind, value: &ParameterValue) -> Result<(), CommandError> {
    let ParameterValue::Bool { value: flag } = value else {
        return Err(CommandError::Incompatible(
            "expected Bool ParameterValue for preserve_pitch".to_owned(),
        ));
    };
    match kind {
        ClipKind::Video { preserve_pitch, .. } | ClipKind::Audio { preserve_pitch, .. } => {
            *preserve_pitch = *flag;
            Ok(())
        }
        _ => Err(CommandError::Incompatible(
            "preserve_pitch applies only to video and audio clips".to_owned(),
        )),
    }
}

fn apply_subtitle_parameter(
    sub: &mut SubtitleProperties,
    path: ParameterPath,
    value: &ParameterValue,
) -> Result<(), CommandError> {
    match path {
        ParameterPath::SubtitleText => {
            let ParameterValue::Text { value: text } = value else {
                return Err(CommandError::Incompatible(
                    "expected Text ParameterValue for subtitle_text".to_owned(),
                ));
            };
            sub.text.clone_from(text);
        }
        ParameterPath::SubtitleFontFamily => {
            let ParameterValue::Text { value: font } = value else {
                return Err(CommandError::Incompatible(
                    "expected Text ParameterValue for subtitle_font_family".to_owned(),
                ));
            };
            sub.font_family.clone_from(font);
        }
        ParameterPath::SubtitleFontSize => sub.font_size = expect_animated(value)?.clone(),
        ParameterPath::SubtitleTextColor => {
            let ParameterValue::Color { value: color } = value else {
                return Err(CommandError::Incompatible(
                    "expected Color ParameterValue".to_owned(),
                ));
            };
            sub.text_color = *color;
        }
        ParameterPath::SubtitleOutlineColor => {
            let ParameterValue::Color { value: color } = value else {
                return Err(CommandError::Incompatible(
                    "expected Color ParameterValue".to_owned(),
                ));
            };
            sub.outline_color = *color;
        }
        ParameterPath::SubtitleOutlineWidth => {
            let ParameterValue::Number { value: width } = value else {
                return Err(CommandError::Incompatible(
                    "expected Number ParameterValue for outline_width".to_owned(),
                ));
            };
            if !width.is_finite() || *width < 0.0 {
                return Err(CommandError::Incompatible(
                    "subtitle_outline_width must be finite and >= 0".to_owned(),
                ));
            }
            #[allow(clippy::cast_possible_truncation)]
            // 字幕アウトライン幅の表示用 f32 格納（非負・有限値は直上で検査済み）
            {
                sub.outline_width = *width as f32;
            }
        }
        ParameterPath::SubtitleAlignment => {
            let ParameterValue::TextAlignment { value: align } = value else {
                return Err(CommandError::Incompatible(
                    "expected TextAlignment ParameterValue".to_owned(),
                ));
            };
            sub.alignment = *align;
        }
        _ => {
            return Err(CommandError::Incompatible(
                "unexpected non-subtitle parameter path".to_owned(),
            ));
        }
    }
    Ok(())
}

/// 複数クリップのコピー結果を相対座標つきで保持するクリップボード表現。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipClipboard {
    items: Vec<CopiedClipItem>,
}

#[derive(Debug, Clone, PartialEq)]
struct CopiedClipItem {
    relative_layer_offset: usize,
    relative_start_frames: u64,
    name: String,
    duration: FrameCount,
    effect_offset_frames: i64,
    effect_duration_frames: Option<FrameCount>,
    kind: NewClipKind,
}

impl ClipClipboard {
    /// コピー済みアイテム数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// クリップボードが空かどうかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 指定したクリップ群をコピーし、相対位置を保持した [`ClipClipboard`] を構築する。
///
/// # Errors
///
/// 空配列または存在しないクリップ ID が含まれる場合に [`CommandError`] を返す。
pub fn copy_clips(
    doc: &ProjectDocument,
    clip_ids: &[ClipId],
) -> Result<ClipClipboard, CommandError> {
    if clip_ids.is_empty() {
        return Err(CommandError::Incompatible(
            "cannot copy an empty selection".to_owned(),
        ));
    }

    let wanted: HashSet<&ClipId> = clip_ids.iter().collect();
    let mut found: Vec<(usize, &Clip)> = Vec::with_capacity(wanted.len());
    for (layer_idx, layer) in doc.layers.iter().enumerate() {
        for clip in &layer.clips {
            if wanted.contains(&clip.id) {
                found.push((layer_idx, clip));
            }
        }
    }

    if found.len() != wanted.len() {
        return Err(CommandError::NotFound(
            "one or more selected clips not found".to_owned(),
        ));
    }

    let min_layer = found.iter().map(|(l, _)| *l).min().unwrap_or(0);
    let min_start = found
        .iter()
        .map(|(_, c)| c.timeline_start.get())
        .min()
        .unwrap_or(0);

    let items = found
        .into_iter()
        .map(|(layer_idx, clip)| CopiedClipItem {
            relative_layer_offset: layer_idx - min_layer,
            relative_start_frames: clip.timeline_start.get() - min_start,
            name: clip.name.clone(),
            duration: clip.duration,
            effect_offset_frames: clip.effect_offset_frames,
            effect_duration_frames: clip.effect_duration_frames,
            kind: to_new_clip_kind(&clip.kind),
        })
        .collect();

    Ok(ClipClipboard { items })
}

/// コピー済みクリップ群を指定タイムライン時刻・基準レイヤーへ貼り付ける [`EditCommand::Batch`] を生成する。
///
/// # Errors
///
/// クリップボードが空、または貼り付け先レイヤーがドキュメントに存在しない場合にエラーを返す。
pub fn build_paste_command(
    clipboard: &ClipClipboard,
    doc: &ProjectDocument,
    paste_at_frame: FrameIndex,
    base_layer_index: usize,
) -> Result<EditCommand, CommandError> {
    if clipboard.is_empty() {
        return Err(CommandError::Incompatible("clipboard is empty".to_owned()));
    }

    let mut commands = Vec::with_capacity(clipboard.items.len());
    for item in &clipboard.items {
        let target_layer_idx = base_layer_index + item.relative_layer_offset;
        let layer = doc
            .layers
            .get(target_layer_idx)
            .ok_or_else(|| CommandError::NotFound(format!("layer index {target_layer_idx}")))?;
        let target_start = paste_at_frame
            .get()
            .checked_add(item.relative_start_frames)
            .ok_or(CommandError::Time(crate::core::time::TimeError::Overflow))?;
        commands.push(EditCommand::AddClip {
            temp_id: None,
            id: None,
            layer_id: LayerRef::Id(layer.id.clone()),
            clip: Box::new(NewClipSpec {
                name: item.name.clone(),
                timeline_start: FrameIndex::new(target_start),
                duration: item.duration,
                effect_offset_frames: item.effect_offset_frames,
                effect_duration_frames: item.effect_duration_frames,
                kind: item.kind.clone(),
            }),
        });
    }

    Ok(EditCommand::Batch { commands })
}

fn to_new_clip_kind(kind: &ClipKind) -> NewClipKind {
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
