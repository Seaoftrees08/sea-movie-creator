# sea-movie-creator

`sea-movie-creator` の開発リポジトリです。

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
