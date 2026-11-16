//! 個別編集コマンドの実行ロジックおよびコピー/ペースト等のタイムライン操作実装。

use std::collections::{HashMap, HashSet};

use crate::audio::properties::AudioPluginParamValue;
use crate::core::animation::{
    AnimatedParameter, EasingCurve, EvaluationContext, ExtrapolationMode, Keyframe,
};
use crate::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use crate::core::time::{
    FrameCount, FrameIndex, FrameRate, PlaybackRate, frames_to_source_seconds,
};
use crate::media::visual_properties::{SubtitleProperties, VisualProperties};
use crate::timeline::command::{
    CommandError, EditCommand, NewClipKind, NewClipSpec, ParameterPath, ParameterTarget,
    ParameterValue,
};
use crate::timeline::document::{
    Asset, AssetKind, AssetPath, AssetStatus, AudioProperties, Clip, ClipKind,
    DocumentValidationError, FrameRange, Layer, ProjectDocument, ProjectResolution,
    TimelineLengthMode,
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
            EditCommand::SetTimelineLength { mode } => {
                set_timeline_length(self.doc, *mode, self.undo_patches);
                Ok(())
            }
            EditCommand::SetSelectionRange { range } => {
                set_selection_range(self.doc, *range, self.undo_patches)
            }
            EditCommand::AddMidpoint {
                clip_id,
                effect_frame,
            } => add_midpoint(
                self.doc,
                self.ctx,
                clip_id,
                *effect_frame,
                self.undo_patches,
            ),
            EditCommand::MoveMidpoint {
                clip_id,
                old_effect_frame,
                new_effect_frame,
            } => move_midpoint(
                self.doc,
                self.ctx,
                clip_id,
                *old_effect_frame,
                *new_effect_frame,
                self.undo_patches,
            ),
            EditCommand::RemoveMidpoint {
                clip_id,
                effect_frame,
            } => remove_midpoint(
                self.doc,
                self.ctx,
                clip_id,
                *effect_frame,
                self.undo_patches,
            ),
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
            } => set_parameter(self.doc, self.ctx, target, path, value, self.undo_patches),
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
        if !spec.midpoints.windows(2).all(|w| w[0] < w[1]) {
            return Err(CommandError::Validation(
                DocumentValidationError::InvalidMidpoints { clip_id },
            ));
        }

        let mut clip = Clip {
            id: clip_id.clone(),
            name: spec.name.clone(),
            timeline_start: spec.timeline_start,
            duration: spec.duration,
            effect_offset_frames: spec.effect_offset_frames,
            effect_duration_frames: spec.effect_duration_frames,
            midpoints: spec.midpoints.clone(),
            kind,
        };
        if !clip.midpoints.is_empty() || clip_has_non_empty_keyframes(&clip) {
            sync_and_align_clip_keyframes(&mut clip, self.doc.fps);
        }

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
            midpoints: left.midpoints.clone(),
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
    let mut updated = old_clip.clone();
    updated.duration = old_clip.duration.checked_sub_signed(delta_frames)?;
    if old_clip.effect_duration_frames.is_some()
        || old_clip.effect_offset_frames != 0
        || !old_clip.midpoints.is_empty()
        || old_clip.has_time_varying_animation()
    {
        updated.effect_duration_frames = Some(old_clip.effective_effect_duration());
    }
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
    path: &ParameterPath,
    value: &ParameterValue,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let (clip_ref, slot_id_opt) = match target {
        ParameterTarget::Clip { clip_id } => (clip_id, None),
        ParameterTarget::AudioPlugin { clip_id, slot_id } => (clip_id, Some(slot_id.as_str())),
    };
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let old_clip = doc.layers[layer_idx].clips[clip_idx].clone();
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    if let Some(slot_id) = slot_id_opt {
        let audio = clip_audio_mut(&mut clip.kind)?;
        apply_audio_plugin_parameter(audio, slot_id, path, value)?;
    } else {
        let fps = doc.fps;
        apply_clip_parameter(&mut clip.kind, path, value)?;
        if matches!(
            value,
            ParameterValue::Animated {
                param: AnimatedParameter::Keyframes { keyframes, .. }
            } if !keyframes.is_empty()
        ) {
            sync_and_align_clip_keyframes(clip, fps);
        }
    }
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
    path: &ParameterPath,
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
        | ParameterPath::BlendMode
        | ParameterPath::Brightness
        | ParameterPath::Contrast
        | ParameterPath::Saturation
        | ParameterPath::HueDeg
        | ParameterPath::Gamma => {
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
        ParameterPath::PluginEnabled | ParameterPath::PluginParam { .. } => {
            Err(CommandError::Incompatible(
                "audio plugin paths require ParameterTarget::AudioPlugin".to_owned(),
            ))
        }
    }
}

fn apply_audio_plugin_parameter(
    audio: &mut AudioProperties,
    slot_id: &str,
    path: &ParameterPath,
    value: &ParameterValue,
) -> Result<(), CommandError> {
    let slot = audio
        .plugin_slots
        .iter_mut()
        .find(|s| s.slot_id == slot_id)
        .ok_or_else(|| {
            CommandError::Incompatible(format!("audio plugin slot not found: {slot_id}"))
        })?;
    match path {
        ParameterPath::PluginEnabled => {
            let ParameterValue::Bool { value: enabled } = value else {
                return Err(CommandError::Incompatible(
                    "expected Bool ParameterValue for plugin_enabled".to_owned(),
                ));
            };
            slot.enabled = *enabled;
            Ok(())
        }
        ParameterPath::PluginParam { param_name } => {
            let plugin_val = match value {
                ParameterValue::Animated { param } => AudioPluginParamValue::Animated {
                    param: param.clone(),
                },
                ParameterValue::Number { value } => {
                    if !value.is_finite() {
                        return Err(CommandError::Incompatible(
                            "plugin parameter number must be finite".to_owned(),
                        ));
                    }
                    AudioPluginParamValue::Number { value: *value }
                }
                ParameterValue::Bool { value } => AudioPluginParamValue::Bool { value: *value },
                _ => {
                    return Err(CommandError::Incompatible(
                        "plugin_param requires Animated, Number, or Bool ParameterValue".to_owned(),
                    ));
                }
            };
            slot.parameters.insert(param_name.clone(), plugin_val);
            Ok(())
        }
        _ => Err(CommandError::Incompatible(
            "expected PluginEnabled or PluginParam for ParameterTarget::AudioPlugin".to_owned(),
        )),
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
            if let crate::core::animation::AnimatedParameter::Expression {
                expr,
                base_offset,
                scale,
            } = param
            {
                if !base_offset.is_finite() || !scale.is_finite() {
                    return Err(CommandError::Incompatible(
                        "expression tuning base_offset and scale must be finite".to_owned(),
                    ));
                }
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
    path: &ParameterPath,
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
        ParameterPath::Brightness => visual.brightness = expect_animated(value)?.clone(),
        ParameterPath::Contrast => visual.contrast = expect_animated(value)?.clone(),
        ParameterPath::Saturation => visual.saturation = expect_animated(value)?.clone(),
        ParameterPath::HueDeg => visual.hue_deg = expect_animated(value)?.clone(),
        ParameterPath::Gamma => visual.gamma = expect_animated(value)?.clone(),
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
    path: &ParameterPath,
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
    path: &ParameterPath,
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
    midpoints: Vec<i64>,
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
            midpoints: clip.midpoints.clone(),
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
                midpoints: item.midpoints.clone(),
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

fn clip_has_non_empty_keyframes(clip: &Clip) -> bool {
    let mut found = false;
    let mut probe = clip.clone();
    probe.visit_animated_params_mut(|param| {
        if let AnimatedParameter::Keyframes { keyframes, .. } = param
            && !keyframes.is_empty()
        {
            found = true;
        }
    });
    found
}

/// クリップ内の中間点配列 (`Clip.midpoints`) と全パラメータのキーフレーム境界を双方向同期する。
///
/// 1. 未登録の中間キーフレーム (`eff_start < frame < eff_last`) を `clip.midpoints` へ取り込む。
/// 2. `visible_midpoints` が存在する場合は全 [`AnimatedParameter::Easing`] を `Keyframes` へ展開する。
/// 3. 既存の [`AnimatedParameter::Keyframes`] を `clip.grid_frames()` 上に再サンプルする。
pub fn sync_and_align_clip_keyframes(clip: &mut Clip, fps: FrameRate) {
    let (eff_start, eff_last) = clip.effect_bounds();
    let mut harvested = Vec::new();
    clip.visit_animated_params_mut(|param| {
        if let AnimatedParameter::Keyframes { keyframes, .. } = param {
            for kf in keyframes.iter() {
                if eff_start < kf.frame && kf.frame < eff_last {
                    harvested.push(kf.frame);
                }
            }
        }
    });
    clip.midpoints.extend(harvested);
    clip.midpoints.sort_unstable();
    clip.midpoints.dedup();

    align_clip_keyframes_to_midpoints(clip, fps);
}

/// 現在の `Clip.midpoints` を正として、全パラメータの `Easing` 展開および `Keyframes` 再サンプルを行う。
fn align_clip_keyframes_to_midpoints(clip: &mut Clip, fps: FrameRate) {
    let (eff_start, _) = clip.effect_bounds();
    let has_visible_midpoints = !clip.visible_midpoints().is_empty();
    let grid = clip.grid_frames();
    let eff_dur = clip.effective_effect_duration();

    clip.visit_animated_params_mut(|param| {
        align_single_parameter(param, eff_start, eff_dur, fps, has_visible_midpoints, &grid);
    });
}

fn align_single_parameter(
    param: &mut AnimatedParameter,
    eff_start: i64,
    eff_dur: FrameCount,
    fps: FrameRate,
    has_visible_midpoints: bool,
    grid: &[i64],
) {
    match param {
        AnimatedParameter::Constant { .. } | AnimatedParameter::Expression { .. } => {}
        AnimatedParameter::Easing { curve, .. } => {
            if !has_visible_midpoints || grid.len() <= 1 {
                return;
            }
            let curve_cloned = *curve;
            let snapshot = param.clone();
            let keyframes = grid
                .iter()
                .map(|&f| {
                    let val = eval_param_at_effect_frame(&snapshot, f, eff_start, eff_dur, fps);
                    Keyframe::with_easing(f, val, curve_cloned)
                })
                .collect();
            *param = AnimatedParameter::Keyframes {
                keyframes,
                extrapolation: ExtrapolationMode::Clamp,
            };
        }
        AnimatedParameter::Keyframes {
            keyframes,
            extrapolation,
        } => {
            if keyframes.is_empty() {
                return;
            }
            let orig_keyframes = keyframes.clone();
            let snapshot = AnimatedParameter::Keyframes {
                keyframes: orig_keyframes.clone(),
                extrapolation: *extrapolation,
            };
            let mut next = Vec::with_capacity(grid.len());
            for &f in grid {
                if let Some(existing) = orig_keyframes.iter().find(|kf| kf.frame == f) {
                    next.push(existing.clone());
                } else {
                    let val = eval_param_at_effect_frame(&snapshot, f, eff_start, eff_dur, fps);
                    let inherited_easing = orig_keyframes
                        .iter()
                        .find(|kf| kf.frame > f)
                        .or_else(|| orig_keyframes.last())
                        .map_or(EasingCurve::Linear, |kf| kf.easing);
                    next.push(Keyframe::with_easing(f, val, inherited_easing));
                }
            }
            *keyframes = next;
        }
    }
}

fn eval_param_at_effect_frame(
    param: &AnimatedParameter,
    effect_frame: i64,
    eff_start: i64,
    eff_dur: FrameCount,
    fps: FrameRate,
) -> f64 {
    let local = u64::try_from(effect_frame.saturating_sub(eff_start)).unwrap_or(0);
    let ctx = EvaluationContext::new(local, eff_start, eff_dur, fps);
    param.evaluate(ctx).unwrap_or(0.0)
}

fn set_timeline_length(
    doc: &mut ProjectDocument,
    mode: TimelineLengthMode,
    undo_patches: &mut Vec<PatchStep>,
) {
    undo_patches.push(PatchStep::SetTimelineLength {
        mode: doc.timeline_length,
    });
    doc.timeline_length = mode;
}

fn set_selection_range(
    doc: &mut ProjectDocument,
    range: Option<FrameRange>,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    if let Some(r) = range
        && r.start >= r.end
    {
        return Err(CommandError::Validation(
            DocumentValidationError::InvalidSelectionRange {
                start: r.start,
                end: r.end,
            },
        ));
    }
    undo_patches.push(PatchStep::SetSelectionRange {
        range: doc.selection_range,
    });
    doc.selection_range = range;
    Ok(())
}

fn add_midpoint(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    effect_frame: i64,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let fps = doc.fps;
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    if clip.duration.get() < 3 {
        return Err(CommandError::Incompatible(
            "3フレーム未満のクリップには中間点を追加できません".to_owned(),
        ));
    }
    let (eff_start, eff_last) = clip.effect_bounds();
    if !(eff_start < effect_frame && effect_frame < eff_last) {
        return Err(CommandError::Incompatible(format!(
            "中間点 {effect_frame} はクリップ範囲 ({eff_start}..{eff_last}) の内側である必要があります"
        )));
    }
    if clip.midpoints.contains(&effect_frame) {
        return Err(CommandError::Incompatible(format!(
            "中間点 {effect_frame} は既に存在します"
        )));
    }

    let old_clip = clip.clone();
    let pos = clip.midpoints.partition_point(|&m| m < effect_frame);
    clip.midpoints.insert(pos, effect_frame);
    align_clip_keyframes_to_midpoints(clip, fps);

    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn move_midpoint(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    old_effect_frame: i64,
    new_effect_frame: i64,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let fps = doc.fps;
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    let Some(old_pos) = clip.midpoints.iter().position(|&m| m == old_effect_frame) else {
        return Err(CommandError::NotFound(format!(
            "midpoint `{old_effect_frame}` on clip `{clip_id}`"
        )));
    };
    let (eff_start, eff_last) = clip.effect_bounds();
    if !(eff_start < new_effect_frame && new_effect_frame < eff_last) {
        return Err(CommandError::Incompatible(format!(
            "移動先中間点 {new_effect_frame} は ({eff_start}..{eff_last}) の内側である必要があります"
        )));
    }
    if new_effect_frame == old_effect_frame {
        return Ok(());
    }
    if clip.midpoints.contains(&new_effect_frame) {
        return Err(CommandError::Incompatible(format!(
            "移動先中間点 {new_effect_frame} は他の中間点と重複しています"
        )));
    }

    let old_clip = clip.clone();
    clip.midpoints.remove(old_pos);
    let new_pos = clip.midpoints.partition_point(|&m| m < new_effect_frame);
    clip.midpoints.insert(new_pos, new_effect_frame);

    clip.visit_animated_params_mut(|param| {
        if let AnimatedParameter::Keyframes { keyframes, .. } = param {
            for kf in keyframes.iter_mut() {
                if kf.frame == old_effect_frame {
                    kf.frame = new_effect_frame;
                }
            }
            keyframes.sort_by_key(|kf| kf.frame);
            keyframes.dedup_by_key(|kf| kf.frame);
        }
    });
    align_clip_keyframes_to_midpoints(clip, fps);

    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

fn remove_midpoint(
    doc: &mut ProjectDocument,
    ctx: &ExecutionContext,
    clip_ref: &ClipRef,
    effect_frame: i64,
    undo_patches: &mut Vec<PatchStep>,
) -> Result<(), CommandError> {
    let clip_id = ctx.resolve_clip(clip_ref)?;
    let (layer_idx, clip_idx) = locate_clip_mutable(doc, &clip_id)?;
    let layer_id = doc.layers[layer_idx].id.clone();
    if doc.layers[layer_idx].locked {
        return Err(CommandError::LayerLocked(layer_id));
    }

    let fps = doc.fps;
    let clip = &mut doc.layers[layer_idx].clips[clip_idx];
    let Some(pos) = clip.midpoints.iter().position(|&m| m == effect_frame) else {
        return Err(CommandError::NotFound(format!(
            "midpoint `{effect_frame}` on clip `{clip_id}`"
        )));
    };

    let old_clip = clip.clone();
    clip.midpoints.remove(pos);
    align_clip_keyframes_to_midpoints(clip, fps);

    undo_patches.push(PatchStep::ReplaceClip {
        layer_id,
        clip: old_clip,
    });
    Ok(())
}

/// 複数クリップ一括移動コマンド生成時の単体移動計画。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipMovePlan {
    /// 移動対象クリップ ID。
    pub clip_id: ClipId,
    /// 移動先レイヤー ID。
    pub target_layer_id: LayerId,
    /// 移動先の開始フレーム位置。
    pub new_timeline_start: FrameIndex,
}

#[derive(Debug, Clone)]
struct ResolvedMovePlan {
    clip_id: ClipId,
    src_layer: LayerId,
    dst_layer: LayerId,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_end: u64,
}

/// 複数クリップの移動計画を検証し、一時衝突を起こさない順序の [`EditCommand::Batch`] を構築する。
///
/// # Errors
///
/// - `plans` が空、または変化のある移動が 1 件もない場合
/// - 移動元・移動先レイヤーがロック状態、または存在しない場合
/// - 移動後配置に重複がある場合、または相互依存サイクルにより順次移動できない場合
pub fn build_batch_move_clips_command(
    doc: &ProjectDocument,
    plans: &[ClipMovePlan],
) -> Result<EditCommand, CommandError> {
    if plans.is_empty() {
        return Err(CommandError::EmptyBatch);
    }

    let resolved = resolve_move_plans(doc, plans)?;
    validate_final_move_positions(doc, &resolved)?;

    let active: Vec<_> = resolved
        .into_iter()
        .filter(|m| m.src_layer != m.dst_layer || m.old_start != m.new_start)
        .collect();
    if active.is_empty() {
        return Err(CommandError::EmptyBatch);
    }

    let ordered = topological_sort_moves(&active)?;
    let commands = ordered
        .into_iter()
        .map(|idx| {
            let m = &active[idx];
            EditCommand::MoveClip {
                clip_id: ClipRef::Id(m.clip_id.clone()),
                target_layer_id: Some(LayerRef::Id(m.dst_layer.clone())),
                new_timeline_start: FrameIndex::new(m.new_start),
            }
        })
        .collect();
    Ok(EditCommand::Batch { commands })
}

fn resolve_move_plans(
    doc: &ProjectDocument,
    plans: &[ClipMovePlan],
) -> Result<Vec<ResolvedMovePlan>, CommandError> {
    let mut seen_ids = std::collections::HashSet::with_capacity(plans.len());
    let mut resolved = Vec::with_capacity(plans.len());
    for plan in plans {
        if !seen_ids.insert(&plan.clip_id) {
            return Err(CommandError::Incompatible(format!(
                "duplicate clip `{}` in multi-clip move",
                plan.clip_id
            )));
        }
        let (src_layer, clip) = doc
            .find_clip(&plan.clip_id)
            .ok_or_else(|| CommandError::NotFound(plan.clip_id.to_string()))?;
        if src_layer.locked {
            return Err(CommandError::LayerLocked(src_layer.id.clone()));
        }
        let dst_layer = doc
            .find_layer(&plan.target_layer_id)
            .ok_or_else(|| CommandError::NotFound(plan.target_layer_id.to_string()))?;
        if dst_layer.locked {
            return Err(CommandError::LayerLocked(dst_layer.id.clone()));
        }
        let old_end = clip.timeline_end()?.get();
        let new_end = plan
            .new_timeline_start
            .get()
            .checked_add(clip.duration.get())
            .ok_or(CommandError::Time(crate::core::time::TimeError::Overflow))?;
        resolved.push(ResolvedMovePlan {
            clip_id: plan.clip_id.clone(),
            src_layer: src_layer.id.clone(),
            dst_layer: plan.target_layer_id.clone(),
            old_start: clip.timeline_start.get(),
            old_end,
            new_start: plan.new_timeline_start.get(),
            new_end,
        });
    }
    Ok(resolved)
}

fn validate_final_move_positions(
    doc: &ProjectDocument,
    resolved: &[ResolvedMovePlan],
) -> Result<(), CommandError> {
    let moving_ids: std::collections::HashSet<&ClipId> =
        resolved.iter().map(|m| &m.clip_id).collect();
    for (idx_a, a) in resolved.iter().enumerate() {
        for b in &resolved[idx_a + 1..] {
            if a.dst_layer == b.dst_layer
                && intervals_overlap(a.new_start, a.new_end, b.new_start, b.new_end)
            {
                return Err(CommandError::Validation(
                    DocumentValidationError::OverlappingClips {
                        layer_id: a.dst_layer.clone(),
                        first: a.clip_id.clone(),
                        second: b.clip_id.clone(),
                    },
                ));
            }
        }
        if let Some(dst_layer) = doc.find_layer(&a.dst_layer) {
            for other in &dst_layer.clips {
                if moving_ids.contains(&other.id) {
                    continue;
                }
                let other_end = other.timeline_end()?.get();
                if intervals_overlap(
                    a.new_start,
                    a.new_end,
                    other.timeline_start.get(),
                    other_end,
                ) {
                    return Err(CommandError::Validation(
                        DocumentValidationError::OverlappingClips {
                            layer_id: dst_layer.id.clone(),
                            first: a.clip_id.clone(),
                            second: other.id.clone(),
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

fn topological_sort_moves(active: &[ResolvedMovePlan]) -> Result<Vec<usize>, CommandError> {
    let n = active.len();
    let mut adj = vec![Vec::new(); n];
    let mut in_deg = vec![0_usize; n];

    for (dst_idx, dst) in active.iter().enumerate() {
        for (src_idx, src) in active.iter().enumerate() {
            if dst_idx == src_idx {
                continue;
            }
            if dst.dst_layer == src.src_layer
                && intervals_overlap(dst.new_start, dst.new_end, src.old_start, src.old_end)
            {
                adj[src_idx].push(dst_idx);
                in_deg[dst_idx] += 1;
            }
        }
    }

    let mut queue = std::collections::VecDeque::new();
    for (idx, &deg) in in_deg.iter().enumerate() {
        if deg == 0 {
            queue.push_back(idx);
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(u) = queue.pop_front() {
        order.push(u);
        for &v in &adj[u] {
            in_deg[v] -= 1;
            if in_deg[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    if order.len() < n {
        return Err(CommandError::Incompatible(
            "循環依存により複数クリップを一括移動できません".to_owned(),
        ));
    }
    Ok(order)
}

const fn intervals_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::dispatcher::CommandDispatcher;

    fn seed_subtitle_doc(
        clips: &[(u64, u64, &str)],
    ) -> Result<(ProjectDocument, LayerId, Vec<ClipId>), Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("Ops Test");
        let layer_id = LayerId::new("l1")?;
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::AddLayer {
                temp_id: None,
                id: Some(layer_id.clone()),
                name: "Track 1".to_owned(),
                insert_index: None,
            },
        )?;
        let mut ids = Vec::new();
        for (idx, (start, dur, text)) in clips.iter().enumerate() {
            let cid = ClipId::new(format!("c{idx}"))?;
            dispatcher.dispatch(
                &mut doc,
                &EditCommand::AddClip {
                    temp_id: None,
                    id: Some(cid.clone()),
                    layer_id: LayerRef::Id(layer_id.clone()),
                    clip: Box::new(NewClipSpec {
                        name: format!("Clip {idx}"),
                        timeline_start: FrameIndex::new(*start),
                        duration: FrameCount::new(*dur)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        midpoints: Vec::new(),
                        kind: NewClipKind::Subtitle(SubtitleProperties::new(*text)),
                    }),
                },
            )?;
            ids.push(cid);
        }
        Ok((doc, layer_id, ids))
    }

    #[test]
    fn midpoint_add_move_remove_promote_easing_and_roundtrip_undo()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, _l1, ids) = seed_subtitle_doc(&[(0, 120, "Sub")])?;
        let cid = &ids[0];
        let mut dispatcher = CommandDispatcher::new();

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(cid.clone()),
                },
                path: ParameterPath::X,
                value: ParameterValue::Animated {
                    param: AnimatedParameter::Easing {
                        start_value: 0.0,
                        end_value: 100.0,
                        curve: EasingCurve::Linear,
                        extrapolation: ExtrapolationMode::Clamp,
                    },
                },
            },
        )?;

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::AddMidpoint {
                clip_id: ClipRef::Id(cid.clone()),
                effect_frame: 60,
            },
        )?;

        let (_, clip) = doc.find_clip(cid).ok_or("missing clip")?;
        assert_eq!(clip.midpoints, vec![60]);
        let ClipKind::Subtitle(sub) = &clip.kind else {
            return Err("expected subtitle".into());
        };
        assert!(matches!(
            &sub.visual.x,
            AnimatedParameter::Keyframes { keyframes, .. }
                if keyframes.iter().map(|kf| kf.frame).collect::<Vec<_>>() == vec![0, 60, 119]
        ));

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::MoveMidpoint {
                clip_id: ClipRef::Id(cid.clone()),
                old_effect_frame: 60,
                new_effect_frame: 80,
            },
        )?;
        let (_, clip) = doc.find_clip(cid).ok_or("missing clip")?;
        assert_eq!(clip.midpoints, vec![80]);

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::RemoveMidpoint {
                clip_id: ClipRef::Id(cid.clone()),
                effect_frame: 80,
            },
        )?;
        let (_, clip) = doc.find_clip(cid).ok_or("missing clip")?;
        assert_eq!(clip.midpoints, Vec::<i64>::new());
        let ClipKind::Subtitle(sub) = &clip.kind else {
            return Err("expected subtitle".into());
        };
        assert!(matches!(
            &sub.visual.x,
            AnimatedParameter::Keyframes { keyframes, .. }
                if keyframes.iter().map(|kf| kf.frame).collect::<Vec<_>>() == vec![0, 119]
        ));

        dispatcher.undo(&mut doc)?;
        assert_eq!(doc.find_clip(cid).ok_or("missing")?.1.midpoints, vec![80]);
        dispatcher.undo(&mut doc)?;
        assert_eq!(doc.find_clip(cid).ok_or("missing")?.1.midpoints, vec![60]);
        Ok(())
    }

    #[test]
    fn short_clip_rejects_midpoint_and_split_preserves_midpoints()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, _l1, ids) = seed_subtitle_doc(&[(0, 2, "Tiny"), (10, 100, "Long")])?;
        let mut dispatcher = CommandDispatcher::new();

        let tiny_err = dispatcher.dispatch(
            &mut doc,
            &EditCommand::AddMidpoint {
                clip_id: ClipRef::Id(ids[0].clone()),
                effect_frame: 1,
            },
        );
        assert!(matches!(tiny_err, Err(CommandError::Incompatible(_))));

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::AddMidpoint {
                clip_id: ClipRef::Id(ids[1].clone()),
                effect_frame: 60,
            },
        )?;
        let report = dispatcher.dispatch(
            &mut doc,
            &EditCommand::SplitClip {
                clip_id: ClipRef::Id(ids[1].clone()),
                split_at_timeline_frame: FrameIndex::new(50),
                right_temp_id: Some("r".to_owned()),
                right_id: None,
            },
        )?;
        let right_id = &report.created_clips["r"];
        let (_, left) = doc.find_clip(&ids[1]).ok_or("missing left")?;
        let (_, right) = doc.find_clip(right_id).ok_or("missing right")?;
        assert_eq!(left.midpoints, vec![60]);
        assert_eq!(left.visible_midpoints(), Vec::<i64>::new());
        assert_eq!(right.midpoints, vec![60]);
        assert_eq!(right.visible_midpoints(), vec![60]);
        Ok(())
    }

    #[test]
    fn batch_move_orders_right_shift_left_shift_and_rejects_cycles()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut doc, l1, ids) = seed_subtitle_doc(&[(0, 30, "A"), (30, 30, "B")])?;
        let mut dispatcher = CommandDispatcher::new();

        let shift_right = build_batch_move_clips_command(
            &doc,
            &[
                ClipMovePlan {
                    clip_id: ids[0].clone(),
                    target_layer_id: l1.clone(),
                    new_timeline_start: FrameIndex::new(20),
                },
                ClipMovePlan {
                    clip_id: ids[1].clone(),
                    target_layer_id: l1.clone(),
                    new_timeline_start: FrameIndex::new(50),
                },
            ],
        )?;
        dispatcher.dispatch(&mut doc, &shift_right)?;
        assert_eq!(
            doc.find_clip(&ids[0]).ok_or("a")?.1.timeline_start.get(),
            20
        );
        assert_eq!(
            doc.find_clip(&ids[1]).ok_or("b")?.1.timeline_start.get(),
            50
        );

        let shift_left = build_batch_move_clips_command(
            &doc,
            &[
                ClipMovePlan {
                    clip_id: ids[1].clone(),
                    target_layer_id: l1.clone(),
                    new_timeline_start: FrameIndex::new(30),
                },
                ClipMovePlan {
                    clip_id: ids[0].clone(),
                    target_layer_id: l1.clone(),
                    new_timeline_start: FrameIndex::new(0),
                },
            ],
        )?;
        dispatcher.dispatch(&mut doc, &shift_left)?;
        assert_eq!(doc.find_clip(&ids[0]).ok_or("a")?.1.timeline_start.get(), 0);
        assert_eq!(
            doc.find_clip(&ids[1]).ok_or("b")?.1.timeline_start.get(),
            30
        );

        let swap_cycle = build_batch_move_clips_command(
            &doc,
            &[
                ClipMovePlan {
                    clip_id: ids[0].clone(),
                    target_layer_id: l1.clone(),
                    new_timeline_start: FrameIndex::new(30),
                },
                ClipMovePlan {
                    clip_id: ids[1].clone(),
                    target_layer_id: l1,
                    new_timeline_start: FrameIndex::new(0),
                },
            ],
        );
        assert!(matches!(swap_cycle, Err(CommandError::Incompatible(_))));
        Ok(())
    }

    fn setup_trim_right_test_doc(
        c1: &ClipId,
        c2: &ClipId,
    ) -> Result<ProjectDocument, Box<dyn std::error::Error>> {
        let mut doc = ProjectDocument::new("TrimRightDuration");
        let l1 = LayerId::new("l1")?;
        let mut dispatcher = CommandDispatcher::new();
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::Batch {
                commands: vec![
                    EditCommand::AddLayer {
                        temp_id: None,
                        id: Some(l1.clone()),
                        name: "Track 1".to_owned(),
                        insert_index: None,
                    },
                    EditCommand::AddClip {
                        temp_id: None,
                        id: Some(c1.clone()),
                        layer_id: LayerRef::Id(l1.clone()),
                        clip: Box::new(NewClipSpec {
                            name: "StaticImage".to_owned(),
                            timeline_start: FrameIndex::ZERO,
                            duration: FrameCount::new(300)?,
                            effect_offset_frames: 0,
                            effect_duration_frames: None,
                            midpoints: Vec::new(),
                            kind: NewClipKind::Subtitle(SubtitleProperties::new("Static")),
                        }),
                    },
                    EditCommand::AddClip {
                        temp_id: None,
                        id: Some(c2.clone()),
                        layer_id: LayerRef::Id(l1),
                        clip: Box::new(NewClipSpec {
                            name: "AnimImage".to_owned(),
                            timeline_start: FrameIndex::new(300),
                            duration: FrameCount::new(300)?,
                            effect_offset_frames: 0,
                            effect_duration_frames: None,
                            midpoints: Vec::new(),
                            kind: NewClipKind::Subtitle({
                                let mut s = SubtitleProperties::new("Anim");
                                s.visual.x = AnimatedParameter::Easing {
                                    start_value: 500.0,
                                    end_value: -500.0,
                                    curve: EasingCurve::Linear,
                                    extrapolation: ExtrapolationMode::Clamp,
                                };
                                s
                            }),
                        }),
                    },
                ],
            },
        )?;
        Ok(doc)
    }

    #[test]
    fn trim_right_constant_clip_adapts_future_easing_duration_while_animated_preserves()
    -> Result<(), Box<dyn std::error::Error>> {
        let c1 = ClipId::new("c1")?;
        let c2 = ClipId::new("c2")?;
        let mut doc = setup_trim_right_test_doc(&c1, &c2)?;
        let mut dispatcher = CommandDispatcher::new();

        dispatcher.dispatch(
            &mut doc,
            &EditCommand::Batch {
                commands: vec![
                    EditCommand::TrimRight {
                        clip_id: ClipRef::Id(c1.clone()),
                        delta_frames: 137,
                    },
                    EditCommand::TrimRight {
                        clip_id: ClipRef::Id(c2.clone()),
                        delta_frames: 137,
                    },
                ],
            },
        )?;

        let (_, trimmed_static) = doc.find_clip(&c1).ok_or("missing c1")?;
        assert_eq!(trimmed_static.duration.get(), 163);
        assert_eq!(trimmed_static.effective_effect_duration().get(), 163);

        // 静的クリップへ後から Easing を付けた場合、トリム後の終端 (162f) で 100% 到達する
        dispatcher.dispatch(
            &mut doc,
            &EditCommand::SetParameter {
                target: ParameterTarget::Clip {
                    clip_id: ClipRef::Id(c1.clone()),
                },
                path: ParameterPath::X,
                value: ParameterValue::Animated {
                    param: AnimatedParameter::Easing {
                        start_value: 500.0,
                        end_value: -500.0,
                        curve: EasingCurve::Linear,
                        extrapolation: ExtrapolationMode::Clamp,
                    },
                },
            },
        )?;
        let (_, animated_after_trim) = doc.find_clip(&c1).ok_or("missing c1 after set")?;
        let ctx_end = animated_after_trim
            .evaluation_context(FrameIndex::new(162), FrameRate::FPS_60)
            .ok_or("missing eval ctx")?;
        let x_end = match &animated_after_trim.kind {
            ClipKind::Subtitle(s) => s.visual.x.evaluate(ctx_end)?,
            _ => return Err("unexpected clip kind".into()),
        };
        assert!((x_end - (-500.0)).abs() < 1e-9);

        // もともとアニメーション済みのクリップはトリム後も元のイージング尺 (300f) を維持する
        let (_, trimmed_anim) = doc.find_clip(&c2).ok_or("missing c2")?;
        assert_eq!(trimmed_anim.effective_effect_duration().get(), 300);
        Ok(())
    }
}
