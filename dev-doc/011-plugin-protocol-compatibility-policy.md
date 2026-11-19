# [011] 外部プラグイン向け SDK・プロトコル互換性ポリシー

- **ステータス**: Draft
- **作成日**: 2026-11-18
- **更新日**: 2026-11-18
- **関連ドキュメント**:
  - プラグイン開発ガイド（外部開発者向け概要）: [`../docs/plugin-development.md`](../docs/plugin-development.md)
  - 書き出しプラグイン開発ガイド: [`../docs/plugins/export-plugins.md`](../docs/plugins/export-plugins.md)
  - AI 連携・MCP 開発ガイド: [`../docs/plugins/ai-and-mcp.md`](../docs/plugins/ai-and-mcp.md)
  - 外部音声プラグイン互換ガイド: [`../docs/plugins/audio-vst3-clap.md`](../docs/plugins/audio-vst3-clap.md)
  - 機能モジュール・プラグイン開発ロードマップ: [`loadmap/Plugin Loadmap.md`](loadmap/Plugin%20Loadmap.md)

---

## 1. 概要・目的

- **解決したい課題・達成したいこと**:
  - サードパーティおよびコントリビューターが安心してプラグインを開発できるよう、既存プロトコル（`plugin.json` / `EXPORT_PROTOCOL_VERSION` / `ai-plugin.json` / `sea-audio-host`）の互換性維持方針と、現行バージョンで公開可能な拡張面 vs 今後の改善が必要な制限事項を明確化する。
- **対象ユーザー / 利用シーン**:
  - `sea-movie-creator` のプラグインホスト仕様・プロトコルを保守する開発者および AI エージェント。

---

## 2. スコープ

### 対象（In Scope）
- `docs/plugin-development.md` および `docs/plugins/*` と本体コード間の仕様同期ルール
- マニフェスト `schema_version` および IPC プロトコルバージョンの後方互換・破壊的変更方針
- ドキュメント整備時に確認された現行ホスト／UI 実装上の制限事項の記録（フォローアップ課題）

### 対象外（Out of Scope）
- 各プラグインプロトコルの網羅的なコードレベル手順（[`../docs/plugins/`](../docs/plugin-development.md) を正本とする）
- 将来フェーズ（P1 外部 TTS / P4 ローカル ASR / P5 他ソフト書き出し）の個別詳細設計（各 Phase 設計書 005/008/009 参照）

---

## 3. 要件・制約

- **用語定義の厳守**:
  - **プラグイン**: 無くても `sea-movie-creator` 本体が起動できる着脱可能拡張（`plugins/*/` + `plugins_src/*/`、外部 VST3/CLAP）。
  - **機能モジュール**: 本体（`src/`）が直接依存する組み込み機能。
- **互換性原則**:
  - 同一メジャーバージョン（`schema_version = 1`, `EXPORT_PROTOCOL_VERSION = 1`）内では、既知フィールドの削除や意味の破壊的変更を行わない。
  - 新しい任意フィールドを追加する場合は、`serde(default)` により旧マニフェスト／旧プラグインが継続動作する後方互換を維持する。
  - 破壊的変更が不可避な場合は `schema_version` / `protocol_version` をインクリメントし、移行手順を [`../docs/plugins/`](../docs/plugin-development.md) と本ファイルへ同時に反映する。
- **ドキュメント同期義務**:
  - [`src/export/manifest.rs`](../src/export/manifest.rs)、[`plugins_src/sea-export-protocol/src/lib.rs`](../plugins_src/sea-export-protocol/src/lib.rs)、[`src/export/runner.rs`](../src/export/runner.rs)、[`src/ai/plugin_host.rs`](../src/ai/plugin_host.rs)、[`src/audio/plugins/scanner.rs`](../src/audio/plugins/scanner.rs) の仕様を変更した場合は、対応する `docs/plugins/*.md` も同時に更新する。

---

## 4. アーキテクチャ / 設計詳細（現行契約と改善候補）

### 4.1 現在の公開拡張面と制約一覧

| 領域 | 現行バージョン | 主なホスト側制約・注意点 |
| :--- | :--- | :--- |
| 動画書き出し (`plugins/export/`) | `schema_version = 1`<br>`EXPORT_PROTOCOL_VERSION = 1` | 1. `ExportDialogState::is_plugin_visible` が `license.requires_external_binary == true` を `ffmpeg` 未設定時の非表示条件に流用している（[`src/ui/export_dialog.rs`](../src/ui/export_dialog.rs)）<br>2. `ExportOptionKind::String` は汎用テキスト入力 UI を持たず `ffmpeg_path` 以外は読み取り専用表示<br>3. `runner.rs` の音声送出は `f32_interleaved` / 2ch / 4096 frames 固定 |
| AI CLI (`plugins/ai/`) | `schema_version = 1`<br>`sea_ai_v1` / `acp_stdio` | `AiPluginRegistry` は動的検出可能だが、GUI コンボボックス（[`src/ai/provider.rs`](../src/ai/provider.rs) `AiProviderKind::ui_selectable_providers`）が `sea.ai.antigravity` / `sea.ai.acp` 固定 |
| 外部音声 (`VST3` / `CLAP`) | `sea-audio-host` 隔離プロセス | ステレオ 2ch / `f32` / 最大 4096 frames / 独自 GUI ウィンドウ非対応 / VST3 は単一コンポーネント `IEditController` 前提 |
| MCP サーバー | `protocolVersion = "2024-11-05"` | 6 ツール固定（`get_timeline_summary`, `get_command_schema`, `preview_edit_command`, `apply_edit_command`, `undo`, `redo`）、`127.0.0.1` 限定 HTTP Bearer 対応 |

---

## 5. インターフェース / API 仕様

- 詳細仕様の正本（人間・外部開発者向け）は以下へ集約する：
  - [`../docs/plugin-development.md`](../docs/plugin-development.md)
  - [`../docs/plugins/export-plugins.md`](../docs/plugins/export-plugins.md)
  - [`../docs/plugins/ai-and-mcp.md`](../docs/plugins/ai-and-mcp.md)
  - [`../docs/plugins/audio-vst3-clap.md`](../docs/plugins/audio-vst3-clap.md)
  - [`../docs/plugins/testing-and-contributing.md`](../docs/plugins/testing-and-contributing.md)

---

## 6. エラーハンドリング・エッジケース

- 外部開発者が誤って非互換仕様を踏まないよう、プロトコル上の制限（`stdout` 汚染、`entrypoint` パストラバーサル、タイムアウト 30s / 120s / 2s、現行 UI 制限）をすべてガイド側に明示する。

---

## 7. テスト計画・受け入れ基準

- ドキュメント更新時はリンク先ファイルパス・シンボル名・コマンド例がリポジトリ実体と一致していることを確認する。
- リポジトリ全体のビルド・リント・テストが引き続きエラーゼロで通過することを確認する。

---

## 8. 未決事項・検討メモ（Open Questions）

- [ ] **`requires_external_binary` と `ffmpeg` 専用判定の分離**: サードパーティが `ffmpeg` 以外の外部 CLI に依存する書き出しプラグインを作った際に非表示にならないよう、`requires_external_ffmpeg` 等の明示判定または起動前チェック UI への分離を検討する。
- [ ] **`ExportOptionKind::String` の汎用入力 UI 化**: 書き出しダイアログ上で任意の文字列オプションを編集可能にするか、`Path` 型オプションを新設するか検討する。
- [ ] **`AiPluginRegistry` と `AI Chat` コンボボックスの動的連携**: `plugins/ai/` に外部開発者が新規 `ai-plugin.json` を追加した際、マニフェストから UI ラベル・プロファイル一覧を自動構築して表示できるようにする。
- [ ] **`sea-export-protocol` の外部配布方針**: 将来 crates.io 等へ公開するか、別言語向け JSON Schema / C ヘッダ定義を公式同梱するかを検討する。
