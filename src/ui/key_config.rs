//! エディタショートカット設定モデル（Phase D-4 で詳細バインド・保存 UI を拡張）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// ユーザーがカスタマイズ可能なショートカットキー割り当て表。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyConfig {
    /// アクション識別子 -> GPUI キーストローク文字列（例: `"space"`, `"ctrl-z"`）。
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

impl KeyConfig {
    /// デフォルト設定の [`KeyConfig`] を返す。
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }
}
