# sea-movie-creator ドキュメント

このディレクトリは、**人間（利用者・コントリビューター）向け**のドキュメントを管理する場所です。
`sea-movie-creator` は、Rust + GPUI による軽量な非破壊動画編集ソフト（**Alpha v0.1.0**）であり、既定で安全・高機能な AI 支援編集（プロバイダ直接接続・外部 CLI プラグイン・ローカル MCP サーバー）と、ライセンスクリーンなプラグイン方式の書き出し・音声プラグイン隔離を備えています。

> [!NOTE]
> 内部アーキテクチャや設計書・開発仕様については [`../dev-doc/README.md`](../dev-doc/README.md) を参照してください。

## 目次

- [はじめに（導入・ビルド・起動方法）](getting-started.md) — アルファ版パッケージの起動、ソースからのビルド、開発時の品質チェック手順
- [ユーザーガイド（画面構成・編集・書き出し）](user-guide.md) — メニュー、プレビュー、仮想化タイムライン、インスペクタ（4 補間モード・数式 `f(t)`・合成モード）、キーボードショートカット、AV1 / H.264 / H.265 書き出し
- [AI 連携・MCP・音声プラグインガイド](ai-and-plugins.md) — 内蔵 AI プロバイダ（API キー / Keyring 設定）、Antigravity / ACP CLI プラグイン、ローカル MCP サーバー、VST3 / CLAP 音声プラグイン別プロセス隔離
- [プラグイン開発ガイド（Plugin Development Overview）](plugin-development.md) — 外部開発者・コントリビューター向けのプラグイン境界定義、拡張手段マトリックス、共通配置・安全ルール
  - [動画書き出しプラグイン開発ガイド（`Export Plugin SDK`）](plugins/export-plugins.md) — `plugin.json` スキーマ、NDJSON IPC、`ShmLayout` 共有メモリ仕様、タイムアウト契約
  - [AI 連携・MCP・AI CLI プラグイン開発ガイド](plugins/ai-and-mcp.md) — MCP サーバー（stdio / ループバック HTTP）、`EditCommand` スキーマ、`sea_ai_v1` / `acp_stdio` プロトコル
  - [外部音声プラグイン（VST3 / CLAP）互換・開発仕様ガイド](plugins/audio-vst3-clap.md) — 探索規則、ステレオ `f32` 処理契約、パラメータ露出、現行制約
  - [プラグインのテスト・デバッグ・コントリビューションガイド](plugins/testing-and-contributing.md) — 単体デバッグ手順、トラブルシューティング、PR 時の品質・ライセンス基準
- [多言語ファイル (`lang/*.json`) 作成ガイド](localization.md) — 再ビルド不要で UI 言語を追加・上書きする手順、検索優先順位、プレースホルダ仕様、有志 PR ガイド
- [トラブルシューティング・FAQ（アルファ版仕様）](troubleshooting.md) — よくある疑問、メディアオフライン復旧、プラグイン未検出、ハードウェアエンコーダ未対応時の対処とアルファ版時点の制限事項
