# はじめに（Getting Started）

このドキュメントは `sea-movie-creator` のセットアップ方法や基本的な使い方をまとめた人間向けガイドです。

> [!NOTE]
> Phase 0（Rust プロジェクト土台、自動整形・静的解析設定、開発ルールの整備）が完了しています。機能設計・製品実装は Phase A 以降で順次追加されます。

## 必要な環境（Prerequisites）

- Rust ツールチェーン（**Rust 1.92 以上**、`rustup` による `stable` チャンネル、`rustfmt`・`clippy` コンポーネント含む。`rust-toolchain.toml` および `Cargo.toml` の `rust-version` 参照）
- Windows 環境では Visual Studio Build Tools（MSVC C++ ビルドツールおよび Windows SDK）
- ※ 既定のビルド構成は `rav1d` の純粋 Rust パス（`default-features = false`）を使用するため、`nasm` アセンブラの導入は必須ではありません（リリースビルド等で x86_64 SIMD ASM を有効化する場合のみ `nasm` が必要です）。

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
