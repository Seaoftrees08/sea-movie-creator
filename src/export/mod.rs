//! 動画書き出しおよび外部プラグイン形式エンコーダ管理モジュール。
//!
//! コア本体から GPL・特許依存コーデックを分離するため、書き出し処理は
//! `plugin.json` に基づいて発見される別プロセスプラグインへ共有メモリ IPC 経由で委譲する。

pub mod cpu_fallback;
pub mod external_tools;
pub mod manifest;
pub mod protocol;
pub mod registry;
pub mod runner;
