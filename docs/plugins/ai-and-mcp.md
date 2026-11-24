# AI 連携・MCP・AI CLI プラグイン開発ガイド

外部の AI エージェント、自動化スクリプト、または独自 CLI アダプタから `sea-movie-creator` の編集機能を操作・拡張するための技術仕様です。

> [!TIP]
> **どの方式を選ぶべきか**:
> 外部の AI エージェントやスクリプトからプロジェクトを編集したい場合は、**まず「§1. ローカル MCP サーバー ＋ CLI スキーマ出力」**の利用を推奨します（現時点で完全に動作し、GUI や他社ツールからも即接続できます）。`plugins/ai/` の AI CLI プラグイン方式は、§2 に記載の現行 UI 制限に注意してください。

---

## 1. ローカル MCP サーバー & スキーマ駆動連携（推奨）

`sea-movie-creator` 実行ファイル自体が MCP（Model Context Protocol, JSON-RPC 2.0）サーバーおよび JSON Schema 出力 CLI を内蔵しています（[`src/main.rs`](../../src/main.rs)・[`src/ai/mcp_server.rs`](../../src/ai/mcp_server.rs)）。

### 1.1 スキーマ取得 CLI オプション

外部ツールが編集コマンドやプロジェクト JSON を生成する際は、実装とズレないよう本体バイナリから最新の JSON Schema（`schemars` 生成）を取得できます。

```bash
# プロジェクトファイル (.smc.json) の JSON Schema を標準出力へ出力
sea-movie-creator --print-document-schema

# 編集コマンド (EditCommand) の JSON Schema を標準出力へ出力
sea-movie-creator --print-command-schema
```

### 1.2 `mcp` サブコマンド（stdio トランスポート）

```bash
sea-movie-creator mcp --project path/to/project.smc.json
```

- `--project` を省略した場合は新規プロジェクト（`"Untitled MCP Project"`）をメモリ上で開きます。
- `--project` を指定した場合、`apply_edit_command` / `undo` / `redo` 成功時に対象 `.smc.json` ファイルへ自動保存されます。
- **フレーミング両対応**: 標準入力は **NDJSON（1 行 1 JSON）** と **`Content-Length: <bytes>\r\n\r\n<body>`** の両方を受理し、受信した方式に合わせて応答を返します。

### 1.3 提供ツール一覧（`tools/list`）

| ツール名 | 主な引数 | 動作・戻り値概要 |
| :--- | :--- | :--- |
| `get_timeline_summary` | `playhead_frame`（任意）, `selected_clip_id`（任意）, `selected_layer_id`（任意） | 現在の解像度・FPS・トラック・クリップ・素材のコンパクト要約 JSON を返す |
| `get_command_schema` | なし | `EditCommand` の JSON Schema を返す |
| `preview_edit_command` | `command`（必須: `object` または JSON 文字列） | プロジェクトを変更せず `dry_run_validate` を実行し、`summary_lines` と `affected_targets` を返す |
| `apply_edit_command` | `command`（必須: `EditCommand`） | `CommandDispatcher` 経由で単一 Undo 単位として適用し、`--project` 指定時はファイル保存する |
| `undo` | なし | 直前の `EditCommand` 操作を 1 段階取り消す |
| `redo` | なし | 取り消した `EditCommand` 操作を 1 段階やり直す |

### 1.4 JSON-RPC 呼び出し例（`stdio` NDJSON）

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"preview_edit_command","arguments":{"command":{"type":"batch","commands":[{"type":"add_layer","temp_id":"l1","name":"Subtitles"},{"type":"add_clip","temp_id":"c1","layer_id":{"temp":"l1"},"clip":{"name":"Sub 1","timeline_start":0,"duration":60,"kind":{"kind":"subtitle","text":"テスト字幕"}}}]}}}}
```

### 1.5 ループバック HTTP MCP サーバー（Rust ライブラリ API）

クレート内から `McpLoopbackHttpServer::bind` を使う場合は、以下のセキュリティ境界が強制されます（[`src/ai/mcp_server.rs`](../../src/ai/mcp_server.rs#L425-L535)）。

- `127.0.0.1` / `::1` のループバックアドレスのみバインド許可（`0.0.0.0` 等は `NonLoopbackForbidden` で拒否）。
- 起動ごとにランダム生成される `sea_mcp_<64hex>` トークンを **`Authorization: Bearer <token>`** ヘッダで必須検証（`POST` のみ受理）。

---

## 2. AI CLI プラグイン仕様（`plugins/ai/<id>/ai-plugin.json`）

本体チャットパネルの裏側で外部 AI CLI を呼び出す別プロセスプラグイン機構です（[`src/ai/plugin_host.rs`](../../src/ai/plugin_host.rs)）。

> [!WARNING]
> **現行バージョン（Alpha v0.1.1）の UI 露出制限**:
> `AiPluginRegistry` 自体は `plugins/ai/` 配下の任意ディレクトリを動的スキャンできますが、GUI のプロバイダ選択コンボボックス（[`src/ai/provider.rs`](../../src/ai/provider.rs#L579-L629) `AiProviderKind::ui_selectable_providers` および `model_catalog`）には現在 **`sea.ai.antigravity`** と **`sea.ai.acp`** の 2 つだけがハードコードされています。
> そのため、サードパーティが新しい `id` の AI プラグインを置いても現時点の GUI コンボボックスには自動出現しません（将来的に UI 側を動的リスト化するまで、独自 AI プラグインのテスト・利用は `AiPluginRegistry` API 経由または上記既存プロファイル連携を前提としてください）。

### 2.1 `ai-plugin.json` スキーマ（`AiPluginManifest`）

```json
{
  "schema_version": 1,
  "id": "sea.ai.example",
  "name": "Example AI CLI Adapter",
  "version": "0.1.0",
  "entrypoint": "sea-ai-example",
  "args": [],
  "protocol": "sea_ai_v1",
  "requires_external_cli": "example-cli",
  "capabilities": {
    "vision": true
  },
  "profiles": [
    {
      "id": "default",
      "label": "Default Profile",
      "cli": "example-cli",
      "args": ["--acp"]
    }
  ]
}
```

| フィールド | 型 | 必須 | 説明・制約 |
| :--- | :--- | :---: | :--- |
| `schema_version` | `u32` | ○ | **`1` 固定** |
| `id`, `name`, `version` | `string` | ○ | いずれも空文字不可。`id` はプラグイン間で一意 |
| `entrypoint` | `string` | ○ | 実行バイナリ名。**`/`, `\`, `..`, 絶対パスすべて不可**（プラグインフォルダ直下またはビルド出力のフラット名のみ） |
| `args` | `string[]` | - | 起動時固定引数（既定 `[]`） |
| `protocol` | `string` | - | `"sea_ai_v1"`（既定）または `"acp_stdio"` |
| `requires_external_cli` | `string \| null` | - | 内部で呼び出す外部 CLI のヒント名 |
| `capabilities` | `object` | - | 能力フラグ（例: `{"vision": true}`、省略時はすべて `false`） |
| `profiles` | `object[]` | - | サブプロファイル一覧（`id`, `label`, `cli`, `args`） |

### 2.2 プロトコル A: `sea_ai_v1`（推奨 NDJSON エンベロープ）

リファレンス実装は [`plugins_src/sea-ai-antigravity/src/main.rs`](../../plugins_src/sea-ai-antigravity/src/main.rs) と [`plugins_src/sea-ai-acp/src/main.rs`](../../plugins_src/sea-ai-acp/src/main.rs) です。

1. **ホスト → プラグイン（`stdin` 1 行 JSON）**:
   - `method` は既定の `"generate_command"` に加え、任意で `"list_models"`（現在利用可能なモデル・プロファイル一覧の動的照会）を指定できます（`CallParams` のプロンプト系フィールドは `#[serde(default)]` のため `list_models` 時は省略可能）。
   ```json
   {
     "protocol": "sea_ai_v1",
     "method": "generate_command",
     "params": {
       "system_prompt": "...",
       "command_schema_json": { "...": "..." },
       "context_summary": { "...": "..." },
       "history": [{ "role": "user", "content": "..." }],
       "user_prompt": "字幕を追加して",
       "stream": true,
       "model": "gemini-4-argon",
       "thinking_level": "medium",
       "external_cli": null,
       "external_args": [],
       "images": [
         { "mime_type": "image/jpeg", "base64_data": "<Base64>" }
       ]
     }
   }
   ```
   - `params.images`（省略時は空配列、1 呼び出しあたり最大 `12` 枚・合計 Base64 `8 MiB` 以下）が渡された場合、`capabilities.vision == true` かつ接続先 ACP エージェントが画像入力に対応していれば `session/prompt` のコンテンツブロック（`{"type": "image", "mimeType": ..., "data": ...}`）へ変換して送信してください。
   - 接続先 CLI の `initialize` 応答にある `result.agentCapabilities.promptCapabilities.image`（または `result.capabilities.promptCapabilities.image`）が明示的に `false` であるにもかかわらず `params.images` が空でない場合は、誤推測を防ぐため `{"event":"error","code":"vision_unsupported","message":"..."}` を出力して失敗終了してください。
   - `params.history`（省略時は空配列）を受け取るプラグインは、役割（`[system]` / `[user]` / `[assistant]`）付きの時系列コンテキストとしてプロンプトへ展開してください（上限目安: `128 KiB`）。
     - 履歴内に `# Original User Instruction` で始まるメッセージが含まれる場合（自律エージェントの複数ステップ実行中）、そのメッセージを最優先でピン留め保持し、`# Session Context` や古い `[ToolResult ` 本文の圧縮を優先したうえで中間ステップを省略します（UTF-8 文字境界を破壊しないこと）。
     - `command_schema_json` や `context_summary` が `null` または空オブジェクト `{}` の場合は、対応するプロンプトセクション自体を省略してください。
     - プラグイン側のサンドボックス制約に特定の出力形式（`EditCommand` や `{"type":"batch","commands":[]}` 等）をハードコード強制してはいけません。出力形式（通常編集か JSON Action Mode か）は常にホストから渡された `system_prompt` に従います。
   ```json
   {
     "protocol": "sea_ai_v1",
     "method": "list_models",
     "params": {
       "external_cli": null,
       "external_args": []
     }
   }
   ```
   - `thinking_level` は `"low"` / `"medium"` / `"high"` / `"xhigh"` 等が渡される場合があります（未指定時は省略）。
2. **プラグイン → ホスト（`stdout` NDJSON イベント）**:
   - 状態通知: `{"event":"status","message":"接続中..."}`
   - ストリーミング差分: `{"event":"delta","text":"{\"type\":\"add_layer\","}`
   - 利用可能モデル一覧（`list_models` 用）: `{"event":"models","models":[{"id":"gemini-4-argon","display_name":"Gemini 4 Argon","badge":null,"thinking_levels":["low","medium","high"],"default_thinking":"medium","available":true,"note":"ACP利用可能"}]}`
   - 生成完了: `{"event":"completed","raw_text":"{\"type\":\"add_layer\",\"name\":\"...\"}","model":"example:model-1","usage":{"input_tokens":120,"output_tokens":45,"total_tokens":165,"cost_usd":null,"is_estimated":false}}`
     - `raw_text` が空文字の場合、ホストはそれまでに受信した `delta` の連結文字列を最終応答として採用します。
     - `usage` フィールドは任意（`#[serde(default)]`）であり、省略時は後方互換としてホスト側が推定・未指定扱いにフォールバックします。
   - エラー通知: `{"event":"error","code":"missing_external_cli","message":"詳細説明"}`（画像入力非対応時は `code: "vision_unsupported"` を使用。ホスト側の初回フォルダ制作 `inspect_images_vision` は警告付きメタデータフォールバックを継続し、既存タイムライン監査 `audit_timeline_slides` は推測誤編集を防ぐため安全停止します）

### 2.3 プロトコル B: `acp_stdio`（直接 ACP JSON-RPC 2.0）

`protocol: "acp_stdio"` を指定した場合、ホストは子プロセスに直接 ACP over stdio（JSON-RPC 2.0）で話しかけます（`initialize` → `session/new` → `session/prompt`、`session/update` 通知でテキスト収集）。
- 画像が添付されている場合、`initialize` 応答の `promptCapabilities.image` が明示 `false` なら `"vision_unsupported"` エラーを返し、対応時は `session/prompt` のコンテンツ配列へ `{"type":"image","mimeType":...,"data":...}` ブロックを追加します。
- ホスト側の `acp_stdio` プロンプト組み立ておよびリファレンスプラグイン `sea-ai-acp` でも、`history` は `128 KiB` 上限で時系列展開（`# Original User Instruction` ピン留め・圧縮・省略）され、空の `command_schema_json` / `context_summary` セクションは省略されます。
- `session/update` 通知を解釈する際は、`update.sessionUpdate` が存在する場合 `"agent_message_chunk"` のみを応答本文として採用し、`agent_thought_chunk` や `tool_call` が混入しないようフィルタすることを推奨します。
また、外部エージェントからツール権限要求（`session/request_permission`）を受ける構成では、[`sea-ai-antigravity/src/main.rs`](../../plugins_src/sea-ai-antigravity/src/main.rs#L268-L322) のように**プロジェクトファイル破壊やシェル実行を防ぐため明示的に拒否応答する**のが推奨パターンです。

### 2.4 テスト用フィクスチャ

外部 CLI を実際に起動せずにテストするため、[`examples/ai_cli_fixture.rs`](../../examples/ai_cli_fixture.rs) が用意されています。`SEA_AI_FIXTURE_MODE` 環境変数（`acp-agy-sandbox` / `acp-stdio`）で決定論的な ACP ストリーミングを再現できます。
