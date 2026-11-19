# 自律エージェントプラグイン開発ガイド（第三者向け / `sea_agent_v1`）

このドキュメントでは、`sea-movie-creator` に高レベルの自律ワークフロー（例：フォルダ内の写真群からプロフィールムービーやダイジェストを構成するエージェント、定型編集ワークフローなど）を追加するための **公式エージェントプラグイン契約 (`sea_agent_v1`)** を解説します。

---

## 1. プラグインの定義と隔離アーキテクチャ

`sea-movie-creator` では、拡張機能を以下の 2 種類に厳格に区別しています。

- **プラグイン（Plugin）**: **無くても本体（`sea-movie-creator`）が起動・動作できる着脱可能な別プロセス拡張**（`plugins/agent/*`、`plugins/export/*`、`plugins/ai/*`、外部 VST3/CLAP）。
- **機能モジュール（Feature Module）**: **本体がビルド・動作に依存する内蔵機能**（`src/` 配下のホスト基盤・UI・メディア処理・タイムラインエンジン）。

エージェントプラグインは独立した OS プロセスとして起動し、`stdin` / `stdout` の **1 行 JSON（NDJSON）** でホスト（`AgentSessionRunner`）と双方向通信します。
これにより、開発者は **Rust（`sea-agent-protocol` クレート）だけでなく Python / Node.js / Go 等任意の言語**で独自エージェントを実装でき、クラッシュ時やライセンス面でも本体を巻き込みません。

> [!IMPORTANT]
> **`stdout` / `stderr` 厳格分離ルール**
> - **`stdout`**: **`sea_agent_v1` の NDJSON メッセージ専用**です。デバッグ用 `println!` 等を 1 行でも `stdout` に出力するとプロトコルエラーになります。
> - **`stderr`**: 診断・デバッグログ専用です（ホスト側が最大 40 行を安全にドレインして保持します）。

---

## 2. 配置場所と検出優先順位

各プラグインは専用サブディレクトリ内に **`agent-plugin.json`** と実行バイナリ（または起動ラッパー）を配置します。

```text
plugins/
  agent/
    my-agent/
      agent-plugin.json
      my-agent-bin(.exe)
```

ホスト（`AgentPluginRegistry::discover_default()`）は以下の順で探索し、同じ `id` が複数見つかった場合は先に見つかった方を採用します。

1. 環境変数 `SEA_AGENT_PLUGIN_DIR` が指すディレクトリ
2. 実行ファイル隣接 `<exe_dir>/plugins/agent/`
3. Cargo ワークスペースルート `<repo>/plugins/agent/`（開発・テスト時、実行ファイルは `target/debug/` / `target/release/` も自動解決）
4. カレント作業ディレクトリ `<cwd>/plugins/agent/`

---

## 3. マニフェスト仕様（`plugins/agent/<id>/agent-plugin.json`）

公式同梱の `plugins/agent/director/agent-plugin.json` と同じスキーマに従います。

```json
{
  "schema_version": 1,
  "id": "com.example.agent.storyboard",
  "name": "ストーリーボード構成エージェント",
  "version": "0.1.0",
  "entrypoint": "example-agent-storyboard",
  "args": [],
  "protocol": "sea_agent_v1",
  "ai_hints": {
    "purpose": "複数フォルダの写真とメモから構成案と日本語字幕を設計する",
    "capabilities": ["vision_batch", "interactive_planning", "movie_blueprint"],
    "recommended_scenarios": ["結婚式プロフィールムービー", "旅行Vlog"]
  }
}
```

### セキュリティ検証制約

- `schema_version`: 現在は `1` のみ許可。
- `id` / `name` / `version`: 空文字禁止（`id` は逆ドメイン形式を推奨）。
- `entrypoint`: 実行ファイル名（拡張子 `.exe` は省略可）のみ指定可。**`/`・`\`・`..`・NUL 文字を含むパスは起動前に拒否**されます。
- `protocol`: 現在は `"sea_agent_v1"` を指定します。
- `ai_hints`（任意 / Layer 3 契約）: 本体側 `HostCapabilitySnapshot` に収録され、他の AI やルーターが用途・推奨シナリオを把握できます。

---

## 4. 将来の本体拡張を自動認識する「4 層契約」

本体（`sea-movie-creator`）に将来 `EditCommand`・エフェクト・音声解析ツール・外部プラグインが追加されても、**エージェントプラグイン側を再コンパイルせずに新機能を自動認識・利用できる** よう 4 層の契約を定めています。

1. **Layer 1: 能力スナップショット (`HostCapabilitySnapshot`)**
   - 初回の `start_task` / `resume_session` 内 `capabilities` フィールド（および `get_capabilities` ツール）で、利用可能な `blend_modes`、`parameter_paths`、ローカルフォント一覧、導入済みプラグイン一覧（`ai_hints` 付き）、`EditCommand` / `MovieBlueprint` JSON Schema を通知します。
2. **Layer 2: 自己記述型ホストツール (`HostToolRegistry` / `AgentTool`)**
   - 初回の `tools` 配列、および動的変更時の `tool_result.updated_tools` で、ホストが提供中の全ツール（`name`, `description`, `input_schema`, `safety_class`, `source`）を開示します。
   - エージェントは未知のツールであっても `description` と `input_schema` をそのまま `llm_chat.tools` へ渡すことで即座に自律利用できます。
3. **Layer 3: マニフェストヒント (`ai_hints`)**
   - `ExportPluginManifest`・`AiPluginManifest`・`AgentPluginManifest` のすべてが `#[serde(default)] pub ai_hints: Option<PluginAiHints>` をサポートしており、新拡張の目的と推奨ユースケースを AI へ伝達します。
4. **Layer 4: `DraftSandbox` ＋ `ToolSafetyClass` ＋ `Manual` / `Autopilot` ゲート**
   - タイムライン操作・構成試行錯誤はすべて軽量クローン `DraftSandbox` 上で行われ、本番プロジェクトを汚しません。
   - 各ツールの `safety_class` に応じてホスト側が自動制御します。
     - `"read_only"`: 確認なし即時実行（ファイル走査は明示添付された許可フォルダ境界内に厳格制限）。
     - `"draft_mutation"`: `DraftSandbox` 内の試行錯誤。最後に `proposal`（単一 `EditCommand::Batch`）として UI に提示。
     - `"external_side_effect"`: ファイル出力や外部副作用を伴う操作。UI が **`Manual` モード時は事前確認ダイアログで一時停止**し、ユーザー許可後に実行します。

---

## 5. 双方向 NDJSON プロトコル（`sea_agent_v1`）

### 5.1 ホスト → プラグイン（`stdin` 1 行 JSON）

#### `start_task`（セッション新規開始）

```json
{
  "type": "start_task",
  "protocol_version": "sea_agent_v1",
  "session_id": "sess_1795000000_1",
  "goal": "このフォルダの画像を使って結婚式プロフィールムービーを3分で作成してください。",
  "attached_folders": ["D:/photos/wedding"],
  "attached_clips": [],
  "capabilities": {
    "schema_version": 1,
    "supported_blend_modes": ["normal", "add", "multiply", "screen", "overlay"],
    "available_fonts": ["Yu Mincho", "Meiryo"]
  },
  "tools": [
    {
      "name": "inspect_media_folder",
      "description": "許可フォルダ内の画像・音声・動画・テキストメモを走査します。",
      "input_schema": { "type": "object", "required": ["path"] },
      "safety_class": "read_only",
      "source": "inspection"
    }
  ],
  "timeline_summary": "{\"name\":\"New Project\",\"layers\":[]}",
  "agent_memory": {}
}
```

#### `resume_session`（保存済みセッション復元）

保存されていた `agent_memory`（画像解析キャッシュや前回構成など）を受け取り、常駐状態へ復帰します。

```json
{
  "type": "resume_session",
  "protocol_version": "sea_agent_v1",
  "session_id": "sess_1795000000_1",
  "capabilities": { "schema_version": 1 },
  "tools": [],
  "timeline_summary": "{\"name\":\"Wedding\",\"layers\":[\"Photos A\",\"Photos B\"]}",
  "agent_memory": { "analyzed_images": [] }
}
```

#### `follow_up`（同一常駐セッション上の追加指示）

```json
{
  "type": "follow_up",
  "prompt": "10枚目の字幕をもっと感動的に修正し、エンディングを少し長く調整してください。",
  "attached_folders": [],
  "attached_clips": [],
  "timeline_summary": "{\"total_duration_frames\":10800}"
}
```

#### `user_answer`（`ask_user` カードへのユーザー回答）

```json
{
  "type": "user_answer",
  "request_id": "ask_1",
  "answer": {
    "choice_id": "customize_apply",
    "field_values": {
      "title": "太郎 & 花子 プロフィールムービー",
      "duration_seconds": "180"
    },
    "free_text": null,
    "skipped": false
  }
}
```

#### `tool_result` / `llm_response` / `cancel`

```json
{
  "type": "tool_result",
  "request_id": "tool_1",
  "ok": true,
  "result": { "images": [], "audios": [] }
}
```

```json
{
  "type": "llm_response",
  "request_id": "llm_1",
  "ok": true,
  "text": "{\"project_name\":\"Wedding\"}",
  "tool_calls": [],
  "finish_reason": "stop"
}
```

```json
{
  "type": "cancel",
  "reason": "user_clicked_stop"
}
```

---

### 5.2 プラグイン → ホスト（`stdout` 1 行 JSON）

#### `status`（進捗通知）

```json
{
  "type": "status",
  "message": "2/4 写真を Vision 解析中 (6/32)...",
  "step": 2,
  "total_steps": 4
}
```

#### `call_tool`（ホストツールの呼び出し）

```json
{
  "type": "call_tool",
  "request_id": "tool_1",
  "tool_name": "inspect_media_folder",
  "arguments": {
    "path": "D:/photos/wedding",
    "recursive": true
  }
}
```

#### `llm_chat`（ホスト認証済み LLM ゲートウェイ呼び出し）

エージェントプラグインは API キーを一切保持・参照せず、ホスト側のプロバイダ（Gemini / OpenAI / Anthropic / Ollama / Antigravity `agy` / ACP CLI）経由で推論します。レート制限保護（`min_llm_interval_ms`）や `image_paths` の 512px サムネイル縮小もホスト側が自動実施します。

```json
{
  "type": "llm_chat",
  "request_id": "llm_1",
  "system_prompt": "You are a helpful video director.",
  "messages": [
    {
      "role": "user",
      "text": "写真群を 3 分のタイムラインへ設計してください。"
    }
  ],
  "tools": [],
  "prefer_json_object": true
}
```

#### `ask_user`（VS Code 風リッチ対話カード）

```json
{
  "type": "ask_user",
  "request_id": "ask_1",
  "title": "ムービー構成の希望を確認",
  "message": "写真32枚とBGM1曲が見つかりました。お名前や雰囲気を指定できます。",
  "fields": [
    {
      "id": "title",
      "label": "タイトル・お名前",
      "placeholder": "太郎 & 花子 Profile Movie",
      "kind": "text"
    },
    {
      "id": "duration_seconds",
      "label": "目標再生時間（秒）",
      "default_value": "180",
      "kind": "number"
    }
  ],
  "choices": [
    {
      "id": "customize_apply",
      "label": "この設定で構成を作成",
      "is_primary": true
    }
  ],
  "allow_skip": true
}
```

#### `proposal`（検証済みタイムライン適用提案）

ホストは受信した `command` に対して `dry_run_validate` を自動実行し、`Manual` モードでは適用差分カードを提示、`Autopilot` モードでは 1 クリック Undo 付きで即時適用します。`agent_memory_patch`（RFC 7396 マージパッチ形式）を返すとセッションファイルへ自動永続化されます。

```json
{
  "type": "proposal",
  "summary_markdown": "### 太郎 & 花子 プロフィールムービー\n- 総尺: 180.0 秒",
  "command": {
    "type": "batch",
    "commands": []
  },
  "agent_memory_patch": {
    "analyzed_count": 32
  }
}
```

#### `error`（エラー通知）

429/403 等の中断時でもそれまでに完了した解析キャッシュ等を失わないよう、任意の `agent_memory_patch` を同梱できます。

```json
{
  "type": "error",
  "code": "no_media",
  "message": "添付フォルダ内に画像ファイルが見つかりませんでした。",
  "recoverable": true,
  "agent_memory_patch": {
    "analyzed_count": 4
  }
}
```

---

## 6. 標準ホストツールと `MovieBlueprint` コンパイラ

エージェントが利用できる主な組み込みツールは以下のとおりです（最新一覧は常に `start_task.tools` を参照してください）。

| ツール名 | 安全クラス | 主な用途 |
| --- | --- | --- |
| `get_capabilities` | `read_only` | `EditCommand` / `MovieBlueprint` 完全スキーマや拡張ヒント取得 |
| `get_timeline_summary` | `read_only` | 現在またはドラフト上のレイヤー／クリップ要約取得 |
| `inspect_media_folder` | `read_only` | 許可フォルダ内画像（サイズ・EXIF撮影日時）、音声尺、メモ読取 |
| `inspect_images_vision` | `read_only` | 512px 縮小画像を使った Vision バッチ解析（非対応時はメタフォールバック） |
| `compile_movie_blueprint` | `draft_mutation` | `MovieBlueprint` → 厳密尺・A/B クロスフェード・縦長暗転額縁・Ken Burns・BGM 一括展開 |
| `apply_draft_edit_command` | `draft_mutation` | 個別の `EditCommand`（クリップ移動、字幕変更等）を `DraftSandbox` へ適用 |
| `reset_draft_to_live` | `draft_mutation` | 未確定のドラフト変更を破棄してライブ状態へ戻す |
| `validate_draft_plan` | `read_only` | `dry_run_validate` 検証と送信用 `EditCommand::Batch` (`include_command: true`) 取得 |

---

## 7. 実装テンプレート

### 7.1 Rust で実装する場合（`sea-agent-protocol` 利用）

```rust
use std::io;
use sea_agent_protocol::{
    AgentToHostMessage, HostToAgentMessage, read_ndjson_line, write_ndjson_line,
};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Some(msg) = read_ndjson_line::<_, HostToAgentMessage>(&mut reader)? {
        match msg {
            HostToAgentMessage::StartTask { goal, .. } => {
                write_ndjson_line(&mut writer, &AgentToHostMessage::Status {
                    message: format!("受信: {goal}"),
                    step: Some(1),
                    total_steps: Some(1),
                })?;
                write_ndjson_line(&mut writer, &AgentToHostMessage::Proposal {
                    summary_markdown: "変更なしのテスト応答です。".to_owned(),
                    command: serde_json::json!({ "type": "batch", "commands": [] }),
                    agent_memory_patch: None,
                    blueprint: None,
                })?;
            }
            HostToAgentMessage::Cancel { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
```

### 7.2 Python（標準ライブラリのみ）で実装する場合

```python
import json
import sys

def send(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    kind = msg.get("type")
    if kind == "start_task":
        send({"type": "status", "message": "Python agent running...", "step": 1, "total_steps": 1})
        send({
            "type": "proposal",
            "summary_markdown": "Python サンプル構成提案",
            "command": {"type": "batch", "commands": []},
        })
    elif kind == "cancel":
        break
```

---

## 8. テスト・デバッグ・品質ルール

1. **単体 CLI デバッグ（NDJSON パイプ確認）**:
   ビルドしたバイナリへ 1 行ずつ JSON をパイプして `stdout` が厳密な NDJSON になっているか確認できます。
   ```powershell
   '{"type":"start_task","protocol_version":"sea_agent_v1","session_id":"dbg1","goal":"テスト","attached_folders":[],"attached_clips":[],"capabilities":{},"tools":[],"timeline_summary":"{}"}' |
     .\target\debug\sea-agent-director.exe
   ```
2. **結合テスト**:
   `MockAiProvider` を使うと、ネットワークや API キーなしでエージェントプラグインの全プロトコル往復（`ask_user` / `call_tool` / `llm_chat` / `proposal` / `follow_up` / `ResumeSession`）を再現決定論的に検証できます（参考: `tests/phase_agent_director/mod.rs`）。
3. **ワークスペース品質ゲート（同梱プラグインの場合）**:
   - `plugins_src/*` に Rust クレートとして同梱する場合は `Cargo.toml` に `[lints] workspace = true` を設定し、`unsafe_code = "deny"`・`missing_docs = "deny"`・`clippy::pedantic`・許容ライセンス（`deny.toml`：`MIT OR Apache-2.0` 等）を満たす必要があります。
   - `cargo test --test quality_gates` が `plugins/agent/*/agent-plugin.json` と本書内の JSON プロトコル例の整合性を自動検証します。
