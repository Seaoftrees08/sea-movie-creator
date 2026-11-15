# sea-movie-creator

AI チャットから動画編集操作を行える、Rust 製の軽量デスクトップ動画編集ソフト（AI-native lightweight movie editor built with Rust）の開発リポジトリです。

> [!NOTE]
> 現在は **Phase 0**（Rust プロジェクト土台、自動整形・静的解析設定、基本モジュール骨格、開発ルールの整備）が完了している段階です。機能設計・製品実装は [`dev-doc/initial-implementation/`](dev-doc/initial-implementation/PhaseA.md) の計画に沿って Phase A 以降で順次追加されます。

## 主な特徴（設計方針）

- **AI-First 設計**: 一意な ID で参照可能な宣言的データモデルと決定論的な編集コマンドを中核に据え、手動 UI 操作と AI チャット操作を同一のコマンドパイプラインで統一します。
- **軽量なデスクトップ構成**: Rust による高速なコア処理に加え、UI に GPUI、GPU 合成に `wgpu` を採用する設計です。
- **モジュール独立構成**: `ai` / `audio` / `core` / `export` / `media` / `timeline` / `ui` の各モジュールを疎結合に分割し、機能追加や差し替えを容易にします。
- **安全な開発基盤**: `unsafe_code = "deny"` および厳格な Clippy リント設定を既定とし、FFI 等で不可避な箇所は境界モジュールに隔離します。

## クイックスタート

Rust ツールチェーン（`rustup` による `stable` チャンネル、`rustfmt`・`clippy` 含む）が必要です。詳細は [はじめに（セットアップ・基本構成）](docs/getting-started.md) を参照してください。

```bash
# 見た目の自動整形チェック
cargo fmt --check

# 静的解析（警告をすべてエラー扱い）
cargo clippy --all-targets --all-features -- -D warnings

# 単体テスト・結合テストの実行
cargo test --all-targets --all-features

# 動作確認実行
cargo run
```

## ディレクトリ構成

- `.github/` — AI アシスタント向けインストラクション（`copilot-instructions.md` / `instructions/`）および GitHub 設定
- `src/` — アプリケーション本体（機能ごとの独立したモジュール構成）
- `tests/` — 結合テスト・全体テスト（`tests/<実装名>.rs` + `tests/<実装名>/`）
- `dev-doc/` — 機能ごとの設計書・技術仕様・アーキテクチャ決定記録（開発者・AI 向け）
- `docs/` — セットアップ手順・使い方・仕様解説などの人間向けドキュメント
- `Cargo.toml`, `rustfmt.toml`, `clippy.toml`, `rust-toolchain.toml` — Rust プロジェクト・整形・静的解析設定

## ドキュメント

- 人間向けドキュメント: [docs/README.md](docs/README.md)
- 開発設計ドキュメント: [dev-doc/README.md](dev-doc/README.md)
- 全体基本設計書: [dev-doc/draft.md](dev-doc/draft.md)

## ライセンス（License）

本プロジェクトは、以下のいずれかのライセンスを選択して利用できるデュアルライセンス（`MIT OR Apache-2.0`）で提供されています。

- **MIT License**（[LICENSE-MIT](LICENSE-MIT)）
- **Apache License, Version 2.0**（[LICENSE-APACHE](LICENSE-APACHE)）

### コントリビューション（Contribution）

特段の明記がない限り、本プロジェクトへ意図的に提出されたすべてのコントリビューション（Apache-2.0 License 第 5 項で定義される Contribution）は、追加の条件なしに上記の **MIT License と Apache License, Version 2.0 のデュアルライセンス** の下で提供されるものとします。
