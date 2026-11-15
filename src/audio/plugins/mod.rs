//! 外部音声プラグイン (`VST3` / `CLAP` / テスト用組み込み `DSP`) スキャンおよび別プロセス隔離ホスト。

pub mod descriptor;
pub mod isolated_host;
pub mod scanner;
