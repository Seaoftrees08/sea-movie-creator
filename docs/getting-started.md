# はじめに（導入・ビルド・起動方法）

このドキュメントは `sea-movie-creator`（**Alpha v0.1.0**）の入手・起動・ソースコードからのビルド手順をまとめたガイドです。

## 1. アルファ版パッケージ（配布フォルダ）を使う場合

ビルド済みアルファ版フォルダ（例: `dist/sea-movie-creator-v0.1.0-alpha-windows-x86_64/` または `target/release/` 基準の配置）には、本体実行ファイルと各種別プロセスプラグインが含まれています。

### 必要なフォルダ配置

アプリは実行ファイルと同じ階層（またはカレントディレクトリ）の `plugins/` を自動検出します。**バイナリ単体だけを別フォルダへ移動せず、以下の構成を保ったまま配置してください。**

```text
<配布フォルダ>/
├── sea-movie-creator.exe        # アプリ本体（GUI / CLI / MCP サーバー）
├── sea-export-av1.exe           # 書き出しプラグイン: 標準 AV1 (rav1e + FLAC) / OS HW
├── sea-export-h264.exe          # 書き出しプラグイン: H.264 (OS Media Foundation / 外部 ffmpeg)
├── sea-export-h265.exe          # 書き出しプラグイン: H.265 (OS HW HEVC / 外部 ffmpeg)
├── sea-audio-host.exe           # 音声プラグイン隔離ホスト (VST3 / CLAP)
├── sea-ai-acp.exe               # AI CLI プラグイン: ACP Stdio (Claude / Codex / Copilot)
├── sea-ai-antigravity.exe       # AI CLI プラグイン: Antigravity 公式 ACP (agy acp --sandbox)
├── LICENSE-MIT
├── LICENSE-APACHE
└── plugins/
    ├── ai/
    │   ├── acp/ai-plugin.json
    │   └── antigravity/ai-plugin.json
    └── export/
        ├── av1/plugin.json
        ├── h264/plugin.json
        └── h265/plugin.json
```

> [!TIP]
> Linux / macOS 環境では `.exe` 拡張子なしの各バイナリが対応します。プラグイン配置フォルダを明示的に上書きしたい場合は、環境変数 `SEA_EXPORT_PLUGIN_DIR` および `SEA_AI_PLUGIN_DIR` を使用できます。

### 起動方法

```bash
# GUI エディタを起動
./sea-movie-creator

# 既存プロジェクト (.smc.json) を開いて起動
./sea-movie-creator --open path/to/project.smc.json
```

その他の CLI オプションや MCP サーバーモードについては [AI 連携・MCP・音声プラグインガイド](ai-and-plugins.md) を参照してください。

---

## 2. ソースコードからビルドする場合

### 必要な環境（Prerequisites）

- **Rust ツールチェーン**: **Rust 1.92 以上**（`rustup` による `stable` チャンネル、`rustfmt`・`clippy` コンポーネント含む。[`../Cargo.toml`](../Cargo.toml) の `rust-version` 参照）
- **Windows**: Visual Studio Build Tools（MSVC C++ ビルドツールおよび Windows SDK）
- **Linux（Ubuntu / Debian 例）**:
  - `pkg-config`, `libasound2-dev`, `libx11-dev`, `libx11-xcb-dev`, `libxcb-render0-dev`, `libxcb-shape0-dev`, `libxcb-xfixes0-dev`, `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libwayland-dev`, `libfontconfig1-dev`, `libvulkan-dev`
- **macOS**: Xcode Command Line Tools
- ※ 既定構成は `rav1d` の純粋 Rust パス（`default-features = false`）を使用するため、`nasm` アセンブラの導入は必須ではありません。

### クローンとアルファリリースビルド

`sea-movie-creator` は本体とプラグイン群が Cargo ワークスペースを構成しています。書き出し機能や AI プラグインをすべて動作させるには、必ず `--workspace` で本体とプラグインをまとめてビルドしてください。

```bash
git clone <repository-url>
cd sea-movie-creator
rustup show

# リリース（アルファ配布向け最適化）ビルド: 本体 + 全プラグインバイナリ
cargo build --release --workspace
```

ビルドが完了すると、Linux/macOS では `target/release/sea-movie-creator`（Windows では `target\release\sea-movie-creator.exe`）および各プラグインバイナリ一式が生成されます。プロジェクトルートからそのまま試す場合は以下を実行します。

```bash
# Windows (PowerShell)
.\target\release\sea-movie-creator.exe

# macOS / Linux
./target/release/sea-movie-creator
```

### 開発時の品質チェック・全テスト

コードを変更した場合や動作環境を詳しく確認したい場合は、ワークスペース全体のフォーマット・Clippy・全テストを実行します（テスト実行前にプラグインバイナリのビルドが必要です）。

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace --all-targets --all-features
```

各画面の具体的な操作方法は [ユーザーガイド](user-guide.md) に続きます。
