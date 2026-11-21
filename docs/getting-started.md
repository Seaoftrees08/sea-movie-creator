# はじめに（導入・ビルド・起動方法）

このドキュメントは `sea-movie-creator`（**Alpha v0.1.0**）の入手・起動・ソースコードからのビルド手順をまとめたガイドです。

## 1. アルファ版パッケージ（配布フォルダ）を使う場合

ビルド済みアルファ版フォルダ（例: `dist/sea-movie-creator-v0.1.0-alpha-windows-x86_64/` または `target/release/` 基準の配置）には、本体実行ファイルと各種別プロセスプラグインが含まれています。

### 必要なフォルダ配置

アプリは実行ファイルと同じ階層（またはカレントディレクトリ）の `plugins/` と `lang/` を自動検出し、ユーザー設定は実行ファイルと同階層の `settings/` フォルダへ保存します。配布フォルダ直下には起動ソフト (`sea-movie-creator.exe`) のみを置き、プラグイン・音声隔離ホスト実行ファイル・設定ファイルはそれぞれサブフォルダへ整理されています。

```text
<配布フォルダ>/
├── sea-movie-creator.exe        # アプリ本体（起動用）
├── LICENSE-MIT
├── LICENSE-APACHE
├── lang/                        # 多言語ファイル（`<code >.json` を追加するだけで言語追加可能）
│   ├── ja.json
│   └── en.json
├── settings/                    # ポータブル設定フォルダ（初回起動・設定変更時に自動生成）
│   ├── ai_settings.json         # AI 接続設定（前回選択AI/モデル・カスタムモデル・任意APIキー・予算）
│   ├── ai_sessions/             # AI チャット履歴 JSON 群
│   ├── key_config.json          # ショートカットキー設定
│   ├── external_tools.json      # 外部 ffmpeg パス設定
│   └── language.json            # 表示言語選択状態
└── plugins/
    ├── ai/
    │   ├── acp/
    │   │   ├── ai-plugin.json
    │   │   └── sea-ai-acp.exe          # AI CLI プラグイン: ACP Stdio (Claude / Codex / Copilot)
    │   └── antigravity/
    │       ├── ai-plugin.json
    │       └── sea-ai-antigravity.exe  # AI CLI プラグイン: Antigravity 公式 ACP
    ├── audio/
    │   └── sea-audio-host.exe          # 音声プラグイン (VST3 / CLAP) 用内部隔離ワーカー
    └── export/
        ├── av1/
        │   ├── plugin.json
        │   └── sea-export-av1.exe      # 書き出しプラグイン: 標準 AV1 (rav1e + FLAC)
        ├── h264/
        │   ├── plugin.json
        │   └── sea-export-h264.exe     # 書き出しプラグイン: H.264 (OS Media Foundation 等)
        └── h265/
            ├── plugin.json
            └── sea-export-h265.exe     # 書き出しプラグイン: H.265 / HEVC
```

> [!TIP]
> - Linux / macOS 環境では `.exe` 拡張子なしの各バイナリが対応します。プラグイン配置フォルダを明示的に上書きしたい場合は環境変数 `SEA_EXPORT_PLUGIN_DIR` / `SEA_AI_PLUGIN_DIR` を、設定フォルダ (`settings/`) の保存先を上書きしたい場合は環境変数 **`SEA_SETTINGS_DIR`** を使用できます。
> - ソースコードから `cargo run` で起動する場合、既定の実行ファイル位置が `target/debug/` になるため、設定ファイル群は `target/debug/settings/` に作成されます（リポジトリ直下で設定を固定したい場合は `SEA_SETTINGS_DIR=./settings` を指定してください。`/settings/` は `.gitignore` 対象です）。

> [!WARNING]
> **API キーの取り扱いについて**:
> AI 設定モーダル（`⚙` ボタンまたはメニューバー `設定 -> AI 設定...`）から API キーを OS 資格情報ストア（Keyring）へ保存するほか、`settings/ai_settings.json` へ直接記述・保存することも可能です。**`settings/ai_settings.json` に API キーを記載している場合は、配布フォルダや `settings/` フォルダごと第三者へ共有・アップロードしないようご注意ください。**（プロジェクトファイル `.smc.json` には API キーは一切保存されません。）

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

### 開発時の品質チェック・全テスト（PR 前チェック）

コードを変更した場合や PR を作成する前は、ワークスペース全体のフォーマット・Clippy・Rustdoc・ビルド・全テスト（品質ゲート結合テスト `tests/quality_gates.rs` 含む）を実行します。統一スクリプトを使うと一括で確認できます（ローカルに `cargo-deny` を導入している場合はライセンス・脆弱性監査も自動実行されます）。

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -File scripts/pre-pr.ps1
```

```bash
# macOS / Linux
./scripts/pre-pr.sh
```

個別に実行する場合は以下を実行します：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo build --workspace --all-targets
cargo test --workspace --all-targets --all-features
```

各画面の具体的な操作方法は [ユーザーガイド](user-guide.md) に続きます。
