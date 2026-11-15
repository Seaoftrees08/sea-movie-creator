# はじめに（Getting Started）

このドキュメントは `sea-movie-creator` のセットアップ方法や基本的な使い方をまとめた人間向けガイドです。

> [!NOTE]
> Phase 0（Rust プロジェクト土台、自動整形・静的解析設定、開発ルールの整備）が完了しています。機能設計・製品実装は Phase A 以降で順次追加されます。

## 必要な環境（Prerequisites）

- Rust ツールチェーン（`rustup` による `stable` チャンネル、`rustfmt`・`clippy` コンポーネント含む。`rust-toolchain.toml` 参照）
- Windows 環境では Visual Studio Build Tools（MSVC C++ ビルドツールおよび Windows SDK）

## セットアップ手順

```bash
git clone <repository-url>
cd sea-movie-creator
rustup show
```

## 実行方法・品質チェック

```bash
# 見た目の自動整形チェック（差分がある場合は cargo fmt）
cargo fmt --check

# ミスの自動検出（警告をすべてエラー扱い）
cargo clippy --all-targets --all-features -- -D warnings

# すべての単体テスト・結合テストを実行
cargo test --all-targets --all-features

# アプリケーションの動作確認実行
cargo run
```
