//! AI と UI が共通で使用する統一編集コマンドと値型定義。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::animation::AnimatedParameter;
use crate::core::color::RgbaColor;
use crate::core::ids::{AssetId, AssetRef, ClipId, ClipRef, LayerId, LayerRef};
use crate::core::time::{FrameCount, FrameIndex, PlaybackRate, RationalDuration, TimeError};
use crate::media::visual_properties::{
    BlendMode, SubtitleProperties, TextAlignment, VisualProperties,
};
use crate::timeline::document::{
    AssetKind, AssetPath, AudioProperties, DocumentValidationError, ProjectResolution,
};

/// 編集コマンドの検証・実行時に発生するエラー。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    /// 空のバッチコマンドが渡された。
    #[error("batch command cannot be empty")]
    EmptyBatch,
    /// 指定された仮 ID が同一バッチ内で重複定義された。
    #[error("duplicate temporary identifier `{0}` in batch")]
    DuplicateTempId(String),
    /// 未定義の仮 ID が参照された。
    #[error("unknown temporary identifier `{0}`")]
    UnknownTempId(String),
    /// 指定された ID が既にドキュメント内に存在する。
    #[error("identifier `{0}` already exists")]
    AlreadyExists(String),
    /// 操作対象（素材・レイヤー・クリップ）が見つからない。
    #[error("target `{0}` was not found")]
    NotFound(String),
    /// クリップに依存関係があるため削除できない。
    #[error("asset `{0}` is still referenced by one or more clips")]
    AssetInUse(AssetId),
    /// レイヤーがロックされているため編集できない。
    #[error("layer `{0}` is locked")]
    LayerLocked(LayerId),
    /// コマンドのパラメータや種別が対象クリップと互換性がない。
    #[error("incompatible operation: {0}")]
    Incompatible(String),
    /// 時刻計算エラー。
    #[error("time calculation failed: {0}")]
    Time(#[from] TimeError),
    /// ドキュメントの不変条件検証に失敗した。
    #[error("document validation failed: {0}")]
    Validation(#[from] DocumentValidationError),
    /// ID 生成ルールに違反した。
    #[error("invalid identifier: {0}")]
    Id(#[from] crate::core::ids::IdError),
}

/// [`EditCommand::SetParameter`] が対象とする要素の識別子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParameterTarget {
    /// 単一クリップ上のプロパティを変更する。
    Clip {
        /// 対象クリップの確定 ID または同一バッチ内仮 ID。
        clip_id: ClipRef,
    },
}

/// [`EditCommand::SetParameter`] で変更可能なプロパティパス。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParameterPath {
    /// キャンバス中心基準の X 座標。
    X,
    /// キャンバス中心基準の Y 座標。
    Y,
    /// X 方向拡大率。
    ScaleX,
    /// Y 方向拡大率。
    ScaleY,
    /// 時計回りの回転角（度）。
    RotationDeg,
    /// アンカーポイント X。
    AnchorX,
    /// アンカーポイント Y。
    AnchorY,
    /// 不透明度 (`0.0..=1.0`)。
    Opacity,
    /// ブレンドモード。
    BlendMode,
    /// 音声ゲイン (dB)。
    GainDb,
    /// 音声パン (`-1.0..=1.0`)。
    Pan,
    /// クリップ個別音声ミュート。
    AudioMuted,
    /// 再生速度変更時のピッチ維持フラグ。
    PreservePitch,
    /// 字幕テキスト本文。
    SubtitleText,
    /// 字幕フォントファミリ。
    SubtitleFontFamily,
    /// 字幕フォントサイズ。
    SubtitleFontSize,
    /// 字幕文字色。
    SubtitleTextColor,
    /// 字幕縁取り色。
    SubtitleOutlineColor,
    /// 字幕縁取り幅。
    SubtitleOutlineWidth,
    /// 字幕水平揃え位置。
    SubtitleAlignment,
}

/// 数値アニメーションからテキスト・真偽値・色まで統一的に扱う変更値。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "value_kind", rename_all = "snake_case")]
pub enum ParameterValue {
    /// アニメーション対応数値パラメータ（固定値・イージング・キーフレーム・数式）。
    Animated {
        /// 新しいアニメーションパラメータ値。
        param: AnimatedParameter,
    },
    /// 文字列値（字幕本文・フォント名等）。
    Text {
        /// 新しい文字列値。
        value: String,
    },
    /// 真偽値（ミュート・ピッチ維持等）。
    Bool {
        /// 新しい真偽値。
        value: bool,
    },
    /// 非アニメーションのスカラー数値（字幕縁取り幅等）。
    Number {
        /// 新しい数値。
        value: f64,
    },
    /// RGBA カラー値（文字色・縁取り色等）。
    Color {
        /// 新しいカラー値。
        value: RgbaColor,
    },
    /// 字幕水平揃え位置。
    TextAlignment {
        /// 新しい水平揃え設定。
        value: TextAlignment,
    },
    /// 合成ブレンドモード。
    BlendMode {
        /// 新しいブレンドモード。
        value: BlendMode,
    },
}

/// [`EditCommand::AddClip`] 時に指定するクリップ作成スペック（バッチ内仮アセット ID 参照可）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NewClipSpec {
    /// クリップ表示名。
    pub name: String,
    /// タイムライン上の開始フレーム位置。
    pub timeline_start: FrameIndex,
    /// タイムライン上の長さ（1 フレーム以上）。
    pub duration: FrameCount,
    /// エフェクト基準フレームオフセット（既定 `0`）。
    #[serde(default)]
    pub effect_offset_frames: i64,
    /// エフェクト基準尺（省略時はクリップ長を使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_duration_frames: Option<FrameCount>,
    /// 種別ごとのペイロード。
    pub kind: NewClipKind,
}

/// 仮 ID (`AssetRef`) を許容するクリップ作成用種別データ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewClipKind {
    /// 動画クリップ作成スペック。
    Video {
        /// 参照する素材 ID またはバッチ内仮 ID。
        asset_id: AssetRef,
        /// ソース開始位置（秒・有理数）。
        #[serde(default)]
        source_in: RationalDuration,
        /// 再生速度倍率。
        #[serde(default)]
        playback_rate: PlaybackRate,
        /// ピッチ維持フラグ。
        #[serde(default = "default_true")]
        preserve_pitch: bool,
        /// 視覚変換設定。
        #[serde(default)]
        visual: VisualProperties,
        /// 音声設定。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<AudioProperties>,
    },
    /// 音声クリップ作成スペック。
    Audio {
        /// 参照する素材 ID またはバッチ内仮 ID。
        asset_id: AssetRef,
        /// ソース開始位置（秒・有理数）。
        #[serde(default)]
        source_in: RationalDuration,
        /// 再生速度倍率。
        #[serde(default)]
        playback_rate: PlaybackRate,
        /// ピッチ維持フラグ。
        #[serde(default = "default_true")]
        preserve_pitch: bool,
        /// 音声設定。
        #[serde(default)]
        audio: AudioProperties,
    },
    /// 静止画クリップ作成スペック。
    Image {
        /// 参照する素材 ID またはバッチ内仮 ID。
        asset_id: AssetRef,
        /// 視覚変換設定。
        #[serde(default)]
        visual: VisualProperties,
    },
    /// 字幕クリップ作成スペック。
    Subtitle(SubtitleProperties),
}

const fn default_true() -> bool {
    true
}

/// AI・UI 双方から発行されるタイムライン編集の単一コマンド体系。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditCommand {
    /// プロジェクト名・解像度・背景色を更新する。
    SetProjectMeta {
        /// 新しいプロジェクト名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 新しい解像度。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution: Option<ProjectResolution>,
        /// 新しい背景色。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background_color: Option<RgbaColor>,
    },
    /// 新しい素材アセットを登録する。
    AddAsset {
        /// バッチ内後続コマンドから参照するための仮 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temp_id: Option<String>,
        /// 明示的に指定する確定 ID（省略時は自動採番）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<AssetId>,
        /// 表示名。
        name: String,
        /// ファイルパス情報。
        path: AssetPath,
        /// メディア属性。
        kind: AssetKind,
    },
    /// どのクリップからも参照されていない素材アセットを削除する。
    RemoveAsset {
        /// 削除対象の素材 ID。
        asset_id: AssetId,
    },
    /// リンク切れまたは別パスへの素材アセット再リンクを行う。
    RelinkAsset {
        /// 再リンク対象の素材 ID。
        asset_id: AssetId,
        /// 新しいファイルパス情報。
        new_path: AssetPath,
        /// ファイル差し替えに伴う新しいメディア属性（省略時は既存維持）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_kind: Option<AssetKind>,
    },
    /// 新しいレイヤーを追加する。
    AddLayer {
        /// バッチ内後続コマンドから参照するための仮 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temp_id: Option<String>,
        /// 明示的に指定する確定 ID（省略時は自動採番）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<LayerId>,
        /// レイヤー名。
        name: String,
        /// 挿入位置（省略時は最前面＝末尾に追加）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert_index: Option<usize>,
    },
    /// レイヤーを削除する（内包クリップも同時に取り除かれる）。
    RemoveLayer {
        /// 削除対象のレイヤー ID。
        layer_id: LayerId,
    },
    /// レイヤーの可視・ミュート・ロック・名称を変更する。
    SetLayerFlags {
        /// 対象レイヤー ID。
        layer_id: LayerRef,
        /// 新しいレイヤー名。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 可視状態。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible: Option<bool>,
        /// ミュート状態。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        muted: Option<bool>,
        /// ロック状態。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locked: Option<bool>,
    },
    /// 指定レイヤーに新しいクリップを追加する。
    AddClip {
        /// バッチ内後続コマンドから参照するための仮 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temp_id: Option<String>,
        /// 明示的に指定する確定 ID（省略時は自動採番）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ClipId>,
        /// 追加先レイヤー。
        layer_id: LayerRef,
        /// 作成するクリップの仕様。
        clip: Box<NewClipSpec>,
    },
    /// クリップをタイムラインから削除する。
    RemoveClip {
        /// 削除対象クリップ。
        clip_id: ClipRef,
    },
    /// クリップを別位置または別レイヤーへ移動する。
    MoveClip {
        /// 移動対象クリップ。
        clip_id: ClipRef,
        /// 移動先レイヤー（省略時は同一レイヤー内で時間移動）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_layer_id: Option<LayerRef>,
        /// 新しい開始フレーム位置。
        new_timeline_start: FrameIndex,
    },
    /// クリップを指定タイムラインフレーム位置で左右 2 つに非破壊分割する。
    SplitClip {
        /// 分割対象クリップ（左半分は既存 ID を維持）。
        clip_id: ClipRef,
        /// 分割点となるタイムラインフレーム位置（`start < frame < end` 必須）。
        split_at_timeline_frame: FrameIndex,
        /// 生成される右半分クリップ用のバッチ内仮 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        right_temp_id: Option<String>,
        /// 生成される右半分クリップに明示的に割り当てる確定 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        right_id: Option<ClipId>,
    },
    /// クリップの左端を移動してトリム（正）または延長（負）する。
    TrimLeft {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 左端の移動フレーム量（正で右方向へ縮み、負で左方向へ伸びる）。
        delta_frames: i64,
    },
    /// クリップの右端を移動してトリム（正）または延長（負）する。
    TrimRight {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 右端から削るフレーム量（正で短くなり、負で長くなる）。
        delta_frames: i64,
    },
    /// 既存クリップのパラメータ・オフセットを保った複製を配置する。
    DuplicateClip {
        /// 複製元クリップ。
        clip_id: ClipRef,
        /// 配置先レイヤー（省略時は元と同じレイヤー）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_layer_id: Option<LayerRef>,
        /// 複製先のタイムライン開始フレーム位置。
        new_timeline_start: FrameIndex,
        /// バッチ内で複製先を参照するための仮 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_temp_id: Option<String>,
        /// 明示的に指定する複製先クリップの確定 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<ClipId>,
    },
    /// クリップや各要素のパラメータを更新する。
    SetParameter {
        /// 変更対象。
        target: ParameterTarget,
        /// 変更するプロパティパス。
        path: ParameterPath,
        /// 設定する新しい値。
        value: ParameterValue,
    },
    /// 動画・音声クリップの再生速度倍率とピッチ維持設定を変更する。
    SetPlaybackRate {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 新しい再生速度倍率（正の有理数）。
        new_rate: PlaybackRate,
        /// ピッチ維持フラグも同時に変更する場合は `Some(...)`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preserve_pitch: Option<bool>,
    },
    /// 複数コマンドを 1 つの Undo 単位かつ全成功/全ロールバックのアトミック単位として実行する。
    Batch {
        /// 順番に実行するコマンド配列。
        commands: Vec<EditCommand>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_command_with_temp_ids_roundtrips_json() -> Result<(), Box<dyn std::error::Error>> {
        let cmd = EditCommand::Batch {
            commands: vec![
                EditCommand::AddLayer {
                    temp_id: Some("tmp_layer".to_owned()),
                    id: None,
                    name: "Video 1".to_owned(),
                    insert_index: None,
                },
                EditCommand::AddClip {
                    temp_id: Some("tmp_clip".to_owned()),
                    id: None,
                    layer_id: LayerRef::Temp("tmp_layer".to_owned()),
                    clip: Box::new(NewClipSpec {
                        name: "Subtitle".to_owned(),
                        timeline_start: FrameIndex::new(0),
                        duration: FrameCount::new(120)?,
                        effect_offset_frames: 0,
                        effect_duration_frames: None,
                        kind: NewClipKind::Subtitle(SubtitleProperties::new("Hello")),
                    }),
                },
                EditCommand::SetParameter {
                    target: ParameterTarget::Clip {
                        clip_id: ClipRef::Temp("tmp_clip".to_owned()),
                    },
                    path: ParameterPath::SubtitleText,
                    value: ParameterValue::Text {
                        value: "Updated".to_owned(),
                    },
                },
            ],
        };

        let json = serde_json::to_string(&cmd)?;
        let restored: EditCommand = serde_json::from_str(&json)?;
        assert_eq!(restored, cmd);
        Ok(())
    }
}
