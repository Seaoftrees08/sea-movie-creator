# sea-movie-creator

AI チャットから動画編集操作を行える、Rust 製の軽量デスクトップ動画編集ソフト（AI-native lightweight movie editor built with Rust）の開発リポジトリです。

> [!NOTE]
> 現在 **Alpha v0.1.0**（Phase 0 〜 Phase C 全実装）に到達しています。利用手順は [人間向けドキュメント](docs/README.md)（[はじめに](docs/getting-started.md)・[ユーザーガイド](docs/user-guide.md)）、内部設計は [`dev-doc/`](dev-doc/README.md) を参照してください。

## 主な特徴

- **AI-First 設計**: 一意 ID で参照可能な宣言的データモデルと決定論的な `EditCommand` を中核に据え、手動 UI 操作・内蔵 AI チャット・外部 CLI プラグイン（Antigravity / ACP）・ローカル MCP サーバーを同一パイプラインで統一。適用前の差分プレビュー確認と 1 クリック Undo に対応しています。
- **軽量なデスクトップ構成**: Rust による高速なタイムライン/音声コアに加え、GPUI（`gpui-kit`）製ダーク UI と `wgpu` マルチレイヤー合成（CPU フォールバック付き）を採用しています。
- **別プロセス隔離プラグイン**: 動画書き出し（`sea-export-av1` / `h264` / `h265`）、AI CLI アダプタ（`sea-ai-acp` / `antigravity`）、外部音声プラグインホスト（VST3 / CLAP 対応 `sea-audio-host`）を別プロセスに隔離し、本体クラッシュや GPL/特許コーデック同梱を回避しています。
- **安全な開発基盤**: `[workspace.lints]` にて `unsafe_code = "deny"` と厳格な Clippy (`pedantic`) リントを全クレートに適用し、`unsafe` FFI/COM 呼び出しは各 `backends/` サブモジュールのみへ隔離しています。

## クイックスタート（ビルド・動作確認）

Rust ツールチェーン（**Rust 1.92 以上**、`rustup` による `stable` チャンネル、`rustfmt`・`clippy` 含む）が必要です。OS ごとの前提パッケージは [はじめに（導入・ビルド・起動方法）](docs/getting-started.md) を参照してください。

```bash
# アルファリリース向け最適化ビルド（本体 + 全プラグインバイナリ）
cargo build --release --workspace

# 起動（ビルド済み target/release/ 配下を自動検出）
cargo run --release
```

```bash
# 開発時の品質チェック・全ワークスペーステスト
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace --all-targets --all-features
```

## ディレクトリ構成

- `.github/` — AI アシスタント向けインストラクション、CI マトリックスワークフロー（`ci.yml`）
- `src/` — アプリケーション本体（`ai` / `audio` / `core` / `export` / `media` / `timeline` / `ui`）
- `plugins/` — 書き出し（`export/`）・AI（`ai/`）プラグインのマニフェスト定義（`plugin.json` / `ai-plugin.json`）
- `plugins_src/` — 別プロセスプラグインおよびプロトコルクレート（`sea-export-*` / `sea-ai-*` / `sea-audio-host` / `sea-export-protocol`）
- `tests/` — 結合テスト・E2E テスト（`tests/<実装名>.rs` + `tests/<実装名>/`）
- `dev-doc/` — 機能ごとの設計書・技術仕様（開発者・AI 向け）
- `docs/` — 導入・操作方法・AI/MCP・FAQ などの人間向けドキュメント
- `Cargo.toml`, `rustfmt.toml`, `clippy.toml`, `rust-toolchain.toml` — Rust ワークスペース・整形・静的解析設定

## ドキュメント

- 人間向けドキュメント: [docs/README.md](docs/README.md)
  - [はじめに（導入・ビルド・起動方法）](docs/getting-started.md)
  - [ユーザーガイド（画面構成・編集・書き出し）](docs/user-guide.md)
  - [AI 連携・MCP・音声プラグインガイド](docs/ai-and-plugins.md)
  - [トラブルシューティング・FAQ](docs/troubleshooting.md)
- 開発設計ドキュメント: [dev-doc/README.md](dev-doc/README.md)
- 全体基本設計書: [dev-doc/draft.md](dev-doc/draft.md)

## ライセンス（License）

本プロジェクトは、以下のいずれかのライセンスを選択して利用できるデュアルライセンス（`MIT OR Apache-2.0`）で提供されています。

- **MIT License**（[LICENSE-MIT](LICENSE-MIT)）
- **Apache License, Version 2.0**（[LICENSE-APACHE](LICENSE-APACHE)）

### コントリビューション（Contribution）

特段の明記がない限り、本プロジェクトへ意図的に提出されたすべてのコントリビューション（Apache-2.0 License 第 5 項で定義される Contribution）は、追加の条件なしに上記の **MIT License と Apache License, Version 2.0 のデュアルライセンス** の下で提供されるものとします。
