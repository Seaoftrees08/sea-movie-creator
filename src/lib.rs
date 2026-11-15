//! AI ネイティブな動画編集ソフト `sea-movie-creator` のライブラリクレート。
//!
//! 将来的な機能追加・差し替えを容易にするため、機能ごとのモジュールを独立させて配置している。

pub mod ai;
pub mod audio;
pub mod core;
pub mod export;
pub mod media;
pub mod timeline;
pub mod ui;

/// アプリケーションの識別名。
pub const APPLICATION_NAME: &str = "sea-movie-creator";
