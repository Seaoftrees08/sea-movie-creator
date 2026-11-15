//! `AviUtl` スタイルの設定ダイアログ風クリップインスペクタモジュール。

pub mod midpoint_bar;
pub mod sections;
pub mod segment_ops;
pub mod trackbar_row;

pub use sections::{InspectorWidgets, render_clip_inspector};
pub use segment_ops::*;
pub use trackbar_row::{InspectorWidgetBank, render_trackbar_row};
