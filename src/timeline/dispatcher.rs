//! コマンド検証（ドライラン）・アトミック実行・仮 ID 解決・履歴管理を統合するディスパッチャ。

use std::collections::HashMap;

use crate::core::ids::{AssetId, ClipId, LayerId};
use crate::timeline::command::{CommandError, EditCommand};
use crate::timeline::document::ProjectDocument;
use crate::timeline::history::{HistoryEntry, UndoHistory, apply_patches_reversed};
use crate::timeline::operations::{ExecutionContext, apply_command_recording_patches};

/// コマンドの実行またはドライラン検証で生成・解決された識別子マップと概要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchReport {
    /// 操作の概要ラベル。
    pub summary: String,
    /// `Batch` 内で `temp_id` から確定した素材 ID マップ。
    pub created_assets: HashMap<String, AssetId>,
    /// `Batch` 内で `temp_id` から確定したレイヤー ID マップ。
    pub created_layers: HashMap<String, LayerId>,
    /// `Batch` 内で `temp_id` から確定したクリップ ID マップ。
    pub created_clips: HashMap<String, ClipId>,
}

/// すべての編集操作を検証・実行・履歴記録する単一エントリーポイント。
#[derive(Debug, Clone, Default)]
pub struct CommandDispatcher {
    history: UndoHistory,
}

impl CommandDispatcher {
    /// 既定の履歴容量（500件）でディスパッチャを作成する。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 任意の最大履歴容量を指定してディスパッチャを作成する。
    #[must_use]
    pub const fn with_history_capacity(max_entries: usize) -> Self {
        Self {
            history: UndoHistory::with_capacity(max_entries),
        }
    }

    /// ドキュメントや履歴を一切変更せずにコマンドが成立するか事前検証する。
    ///
    /// AI エージェントによる適用前検証や UI ドラッグ中の配置可否判定に用いる。
    ///
    /// # Errors
    ///
    /// 実行時または実行後の不変条件検証でエラーが発生した場合 [`CommandError`] を返す。
    pub fn dry_run_validate(
        &self,
        doc: &ProjectDocument,
        command: &EditCommand,
    ) -> Result<DispatchReport, CommandError> {
        let mut scratch = doc.clone();
        let mut ctx = ExecutionContext::with_document(&scratch);
        let mut undo_patches = Vec::new();
        apply_command_recording_patches(&mut scratch, command, &mut ctx, &mut undo_patches)?;
        scratch.validate()?;
        Ok(build_report(command, ctx))
    }

    /// コマンドを実行してドキュメントを更新し、Undo 履歴に積む。
    ///
    /// 途中の操作または実行後の不変条件検証に失敗した場合は記録済み逆パッチで即座に全ロールバックし、
    /// ドキュメントを完全に実行前の状態へ戻す。
    ///
    /// # Errors
    ///
    /// コマンド適用・整合性検証に失敗した場合 [`CommandError`] を返す。
    pub fn dispatch(
        &mut self,
        doc: &mut ProjectDocument,
        command: &EditCommand,
    ) -> Result<DispatchReport, CommandError> {
        let mut ctx = ExecutionContext::with_document(doc);
        let mut undo_patches = Vec::new();

        let exec_result =
            apply_command_recording_patches(doc, command, &mut ctx, &mut undo_patches)
                .and_then(|()| doc.validate().map_err(CommandError::from));

        if let Err(err) = exec_result {
            let _ = apply_patches_reversed(doc, undo_patches);
            return Err(err);
        }

        let report = build_report(command, ctx);
        self.history.push(HistoryEntry {
            label: report.summary.clone(),
            patches: undo_patches,
        });
        Ok(report)
    }

    /// 直前の操作を取り消す。
    ///
    /// # Errors
    ///
    /// 逆パッチ適用・検証失敗時に [`CommandError`] を返す。
    pub fn undo(&mut self, doc: &mut ProjectDocument) -> Result<Option<String>, CommandError> {
        self.history.undo(doc)
    }

    /// 取り消した操作をやり直す。
    ///
    /// # Errors
    ///
    /// 再適用・検証失敗時に [`CommandError`] を返す。
    pub fn redo(&mut self, doc: &mut ProjectDocument) -> Result<Option<String>, CommandError> {
        self.history.redo(doc)
    }

    /// 履歴マネージャへの読み取り専用参照を返す。
    #[must_use]
    pub const fn history(&self) -> &UndoHistory {
        &self.history
    }

    /// 履歴をすべてクリアする（別プロジェクト読込時など）。
    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

fn build_report(command: &EditCommand, ctx: ExecutionContext) -> DispatchReport {
    let summary = describe_command(command);
    let (created_assets, created_layers, created_clips) = ctx.into_created_ids();
    DispatchReport {
        summary,
        created_assets,
        created_layers,
        created_clips,
    }
}

fn describe_command(command: &EditCommand) -> String {
    match command {
        EditCommand::SetProjectMeta { .. } => "Update Project Metadata".to_owned(),
        EditCommand::AddAsset { name, .. } => format!("Add Asset `{name}`"),
        EditCommand::RemoveAsset { asset_id } => format!("Remove Asset `{asset_id}`"),
        EditCommand::RelinkAsset { asset_id, .. } => format!("Relink Asset `{asset_id}`"),
        EditCommand::AddLayer { name, .. } => format!("Add Layer `{name}`"),
        EditCommand::RemoveLayer { layer_id } => format!("Remove Layer `{layer_id}`"),
        EditCommand::SetLayerFlags { .. } => "Update Layer Flags".to_owned(),
        EditCommand::AddClip { clip, .. } => format!("Add Clip `{}`", clip.name),
        EditCommand::RemoveClip { .. } => "Remove Clip".to_owned(),
        EditCommand::MoveClip { .. } => "Move Clip".to_owned(),
        EditCommand::SplitClip { .. } => "Split Clip".to_owned(),
        EditCommand::TrimLeft { .. } => "Trim Clip Left".to_owned(),
        EditCommand::TrimRight { .. } => "Trim Clip Right".to_owned(),
        EditCommand::DuplicateClip { .. } => "Duplicate Clip".to_owned(),
        EditCommand::SetParameter { .. } => "Set Parameter".to_owned(),
        EditCommand::SetPlaybackRate { .. } => "Set Playback Rate".to_owned(),
        EditCommand::SetTimelineLength { .. } => "Set Timeline Length".to_owned(),
        EditCommand::SetSelectionRange { .. } => "Set Selection Range".to_owned(),
        EditCommand::AddMidpoint { .. } => "Add Midpoint".to_owned(),
        EditCommand::MoveMidpoint { .. } => "Move Midpoint".to_owned(),
        EditCommand::RemoveMidpoint { .. } => "Remove Midpoint".to_owned(),
        EditCommand::Batch { commands } => format!("Batch ({} commands)", commands.len()),
    }
}
