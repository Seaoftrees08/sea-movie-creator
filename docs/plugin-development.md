# プラグイン開発ガイド（Plugin Development Overview）

このドキュメントは、`sea-movie-creator` 向けに**動画書き出しプラグイン**、**外部ツール／AI 連携（MCP・AI CLI プラグイン）**、および **VST3 / CLAP 音声プラグイン** を開発・連携・テストしたい外部開発者およびコントリビューター向けの総合案内です。

> [!NOTE]
> プラグインを「使う・設定する」手順は [AI 連携・MCP・音声プラグインガイド](ai-and-plugins.md) を、UI の多言語翻訳ファイル追加は [多言語ファイル (`lang/*.json`) 作成ガイド](localization.md) を参照してください。

---

## 1. 用語定義：「プラグイン」と「機能モジュール」の違い

`sea-movie-creator` では、設計・ドキュメント・コード全体で以下の 2 つを明確に区別しています。

- **プラグイン（Plugin）**: **無くても `sea-movie-creator` 本体が起動・基本編集できる着脱可能な拡張機能**
  - 本体とは別プロセス（`plugins/*/` のマニフェスト＋`plugins_src/*/` のワーカーバイナリ）または外部バイナリ／外部規格（VST3 / CLAP 等）として分離されます。
  - GPL や特許制限のあるエンコーダ／ネイティブライブラリによる本体クラッシュやライセンス汚染を防ぐため、**プロセス境界＋IPC（標準入出力 JSON Lines）／共有メモリ（`mmap`）** で隔離します。
- **機能モジュール（Feature Module）**: **`sea-movie-creator` 本体が直接依存してビルド・起動される機能**
  - `src/` 配下に組み込まれ、タイムライン操作・UI・合成・再生など本体の中核を担います（プラグインを呼び出すホスト窓口側も含みます）。機能モジュールの追加・変更は本体リポジトリへの PR として実装します。

---

## 2. 拡張手段と現在の対応状況（マトリックス）

現時点（Alpha v0.1.0）で外部開発者が利用できる拡張ポイントと、設計中（未実装）の将来領域は以下のとおりです。

| 拡張カテゴリ | 配置／接続方式 | 現在の開発可否 | 特徴・主な制約 | 詳細ガイド |
| :--- | :--- | :--- | :--- | :--- |
| **動画書き出しプラグイン** | `plugins/export/<id>/plugin.json` + 別プロセスバイナリ | **開発可能（推奨）** | `sea_export_protocol` v1（NDJSON + `mmap` 共有メモリ）。UI 上にプリセットとオプションを自動表示可能 | [書き出しプラグイン開発ガイド](plugins/export-plugins.md) |
| **MCP / スキーマ連携** | `sea-movie-creator mcp --project <path>` | **利用可能（推奨）** | JSON-RPC 2.0 over stdio / ループバック HTTP。タイムライン取得・`EditCommand` ドライラン・適用・Undo/Redo | [AI 連携・MCP ガイド](plugins/ai-and-mcp.md) |
| **外部音声プラグイン** | OS 標準パス / `SEA_VST3_PATH` / `SEA_CLAP_PATH` | **対応済み（制限あり）** | `sea-audio-host` 隔離ワーカーでステレオ `f32` 処理。独自 GUI ウィンドウは未対応（パラメータ露出のみ） | [VST3 / CLAP 対応ガイド](plugins/audio-vst3-clap.md) |
| **AI CLI プラグイン** | `plugins/ai/<id>/ai-plugin.json` + 別プロセスバイナリ | **プロトコル実装済／UI 固定** | `AiPluginRegistry` は動的検出するが、UI コンボボックス側（`AiProviderKind`）は現在 `sea.ai.antigravity` / `sea.ai.acp` 固定 | [AI 連携・MCP ガイド](plugins/ai-and-mcp.md) |
| **外部 TTS プラグイン** | `plugins/tts/<id>/tts-plugin.json`（`sea_tts_v1`） | **設計中（Draft / 未実装）** | Phase P1 で導入予定の外部音声合成ブリッジ | [`005-phase-p1-tts-voice-plugin.md`](../dev-doc/005-phase-p1-tts-voice-plugin.md) |
| **ローカル ASR プラグイン** | `plugins/asr/<id>/asr-plugin.json`（`sea_asr_v1`） | **設計中（Draft / 未実装）** | Phase P4 で導入予定の肉声自動字幕ワーカー | [`008-phase-p4-extended-input-plugins.md`](../dev-doc/008-phase-p4-extended-input-plugins.md) |
| **他ソフト書き出しプラグイン** | `plugins/project_export/<id>/plugin.json` | **設計中（Draft / 未実装）** | Phase P5 で導入予定の `.ymmp` / `.exo` 変換ワーカー | [`009-phase-p5-external-project-export-plugins.md`](../dev-doc/009-phase-p5-external-project-export-plugins.md) |

---

## 3. プラグイン配置ディレクトリと検出ルール

配布版および開発ビルドにおいて、本体は以下のディレクトリ配置を自動探索します。

```text
<アプリ配置ルート または リポジトリルート>/
├── sea-movie-creator(.exe)
└── plugins/
    ├── export/
    │   └── <plugin-folder>/
    │       ├── plugin.json
    │       └── <entrypoint>(.exe)   # 同フォルダ配置、または target/{debug,release}/ から解決
    ├── ai/
    │   └── <plugin-folder>/
    │       ├── ai-plugin.json
    │       └── <entrypoint>(.exe)
    └── audio/
        └── sea-audio-host(.exe)     # 本体付属の VST3 / CLAP 隔離ワーカー
```

### 検索パスの優先順位と環境変数オーバーライド

同一プラグイン ID が複数の場所で見つかった場合、**先に検出された定義が採用**され、後続の重複 ID はスキップされます。

- **書き出しプラグイン（`PluginRegistry::default_search_dirs`）**
  1. 環境変数 `SEA_EXPORT_PLUGIN_DIR`（設定時）
  2. `<実行ファイル親ディレクトリ>/plugins/export/`
  3. ワークスペースルート探索時の `<workspace_root>/plugins/export/`
  4. `<カレント作業ディレクトリ>/plugins/export/`
- **AI プラグイン（`AiPluginRegistry::default_search_dirs`）**
  1. 環境変数 `SEA_AI_PLUGIN_DIR`（設定時）
  2. `<実行ファイル親ディレクトリ>/plugins/ai/` および祖先ディレクトリの `plugins/ai/`
  3. `<カレント作業ディレクトリ>/plugins/ai/`
- **VST3 / CLAP 音声プラグイン（`src/audio/plugins/scanner.rs`）**
  - `SEA_VST3_PATH` / `SEA_CLAP_PATH`（OS のパス区切り文字で複数指定可）のあとに各 OS の標準インストール先を走査します。

---

## 4. すべての別プロセスプラグインに共通する開発ルール

1. **`stdout` は IPC 専用、`stderr` をログ出力に使う**
   - `sea-movie-creator` の書き出しランナーおよび AI ホストは子プロセスの `stdout` を 1 行 1 JSON（NDJSON）として厳格にパースします。
   - 初期化バナーやデバッグ出力を `stdout`（`println!` 等）へ出すと即座にプロトコルエラーになります。診断ログは必ず **`stderr`** へ出力してください。
2. **マニフェストの `entrypoint` は安全な相対パスに限定する**
   - 絶対パスや `..` を含む上位ディレクトリ参照はセキュリティ検証で拒否されます（AI プラグインではさらに `/` や `\` を含むサブディレクトリ指定も禁止されています）。
   - Windows 環境では実行ファイル名解決時に `.exe` が自動補完されるため、マニフェストの `entrypoint` には拡張子なしのベース名（例: `"my-export-plugin"`）を書くことを推奨します。
3. **ライセンスおよび外部バイナリ分離の原則**
   - リポジトリ標準のライセンスは **`MIT OR Apache-2.0`** です。本リポジトリへ同梱するコード・依存クレートに GPL 系や再配布制限のある特許バイナリを含めることはできません。
   - 外部ツール（例: ユーザーが各自導入する CLI 等）を呼び出す場合は、プラグイン側で別プロセス実行し、マニフェストのライセンス開示欄へ明記してください。
4. **Windows で子プロセスからさらに外部 CLI を起動する場合**
   - プラグイン内部から別の CLI ツールを起動する場合は、GUI 上でコンソールウィンドウが一瞬点滅しないよう `CREATE_NO_WINDOW`（`0x0800_0000`）の付与を推奨します（`sea_export_protocol::apply_background_process_flags` が参考になります）。

---

## 5. 次に読むドキュメント

- **[書き出しプラグイン開発ガイド（`Export Plugin SDK`）](plugins/export-plugins.md)** — `plugin.json` スキーマ、NDJSON メッセージ、`ShmLayout` 共有メモリ仕様、タイムアウト契約、最小サンプル
- **[AI 連携・MCP 開発ガイド](plugins/ai-and-mcp.md)** — ローカル MCP サーバー、`EditCommand` スキーマ、`sea_ai_v1` / `acp_stdio` プロトコル
- **[外部音声プラグイン（VST3 / CLAP）互換仕様](plugins/audio-vst3-clap.md)** — スキャン仕様、オーディオ処理契約、パラメータ露出、非対応機能
- **[テスト・デバッグ・コントリビューションガイド](plugins/testing-and-contributing.md)** — GUI なしの単体疎通テスト、よくあるエラー対処、リポジトリ PR 時の品質チェック手順
- **（設計背景）[`dev-doc/011-plugin-protocol-compatibility-policy.md`](../dev-doc/011-plugin-protocol-compatibility-policy.md)** — プロトコル互換性ポリシーと現行制限事項の整理
