//! ドキュメント内の各要素およびバッチ仮 ID を識別するための強い型付き ID 定義。

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// ID 文字列の生成・検証時に発生し得るエラー。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// 空文字または空白文字のみの ID が指定された。
    #[error("identifier cannot be empty or whitespace-only")]
    Empty,
}

fn validate_non_empty_id(raw: String) -> Result<String, IdError> {
    if raw.trim().is_empty() {
        return Err(IdError::Empty);
    }
    Ok(raw)
}

macro_rules! define_string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 文字列から識別子を生成する。空または空白のみの場合はエラーを返す。
            ///
            /// # Errors
            ///
            /// `value` が空文字または空白文字のみの場合、[`IdError::Empty`] を返す。
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                validate_non_empty_id(value.into()).map(Self)
            }

            /// 内部の文字列表現への参照を返す。
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_string_id!(
    /// 素材（動画・画像・音声ファイル）を一意に識別する ID。
    AssetId
);

define_string_id!(
    /// タイムライン上のレイヤーを一意に識別する ID。
    LayerId
);

define_string_id!(
    /// タイムライン上のクリップを一意に識別する ID。
    ClipId
);

/// コマンド内で素材を参照する方法（確定 ID または同一バッチ内の仮 ID）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssetRef {
    /// プロジェクト内に存在する確定済み素材 ID。
    Id(AssetId),
    /// 同一 `Batch` コマンド内の先行 `AddAsset` で宣言された仮 ID（例: `"$asset_1"`）。
    Temp(String),
}

impl From<AssetId> for AssetRef {
    fn from(id: AssetId) -> Self {
        Self::Id(id)
    }
}

/// コマンド内でレイヤーを参照する方法（確定 ID または同一バッチ内の仮 ID）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayerRef {
    /// プロジェクト内に存在する確定済みレイヤー ID。
    Id(LayerId),
    /// 同一 `Batch` コマンド内の先行 `AddLayer` で宣言された仮 ID（例: `"$layer_1"`）。
    Temp(String),
}

impl From<LayerId> for LayerRef {
    fn from(id: LayerId) -> Self {
        Self::Id(id)
    }
}

/// コマンド内でクリップを参照する方法（確定 ID または同一バッチ内の仮 ID）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClipRef {
    /// プロジェクト内に存在する確定済みクリップ ID。
    Id(ClipId),
    /// 同一 `Batch` コマンド内の先行 `AddClip` で宣言された仮 ID（例: `"$clip_1"`）。
    Temp(String),
}

impl From<ClipId> for ClipRef {
    fn from(id: ClipId) -> Self {
        Self::Id(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_ids_and_deserialization() {
        assert_eq!(ClipId::new(""), Err(IdError::Empty));
        assert_eq!(LayerId::new("   "), Err(IdError::Empty));
        assert!(AssetId::new("asset_01").is_ok());

        let parsed: Result<ClipId, _> = serde_json::from_str(r#""""#);
        assert!(parsed.is_err());
    }

    #[test]
    fn serializes_and_deserializes_clip_ref_unambiguously() {
        let by_id = ClipRef::from(ClipId::new("clip_1").expect("valid id"));
        let by_temp = ClipRef::Temp("$c1".to_owned());

        let id_json = serde_json::to_string(&by_id).expect("serialize id ref");
        let temp_json = serde_json::to_string(&by_temp).expect("serialize temp ref");
        assert_ne!(id_json, temp_json);

        assert_eq!(
            serde_json::from_str::<ClipRef>(&id_json).expect("deserialize id ref"),
            by_id
        );
        assert_eq!(
            serde_json::from_str::<ClipRef>(&temp_json).expect("deserialize temp ref"),
            by_temp
        );
    }
}
