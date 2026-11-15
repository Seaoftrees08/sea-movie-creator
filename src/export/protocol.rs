//! ホスト〜書き出しプラグイン間の IPC プロトコルと共有メモリ輸送層の再エクスポート。

pub use sea_export_protocol::backends::shm_mmap::{ShmError, ShmHostRegion, ShmPluginRegion};
pub use sea_export_protocol::{
    AudioSampleFormat, EXPORT_PROTOCOL_VERSION, ExportInitPayload, HostToPluginMessage,
    PixelFormat, PluginToHostMessage, ProtocolError, ShmLayout, ShmLayoutSpec, SlotKind,
    read_json_line, write_json_line,
};
