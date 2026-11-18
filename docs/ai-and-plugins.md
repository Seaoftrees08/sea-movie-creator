# AI 連携・MCP・音声プラグインガイド

このガイドでは、`sea-movie-creator` Alpha 版が備えている **AI チャット編集支援**、**ローカル MCP サーバー / CLI**、および **外部 VST3 / CLAP 音声プラグインの別プロセス隔離ホスト** の設定と使い方を解説します。

> [!TIP]
> **自作プラグインの開発や外部ツールとの詳細なプロトコル連携を行いたい場合**は、開発者向けの [プラグイン開発ガイド](plugin-development.md)（[AI 連携・MCP 開発仕様](plugins/ai-and-mcp.md)・[VST3 / CLAP 互換仕様](plugins/audio-vst3-clap.md)）を参照してください。

---

## 1. AI チャットによる編集支援（`AI Chat` タブ）

右パネルの **`AI Chat`** タブから、自然言語でタイムライン構成・クリップ追加・アニメーション数式・音声変更などの編集を提案させ、適用前に差分を確認してから反映できます。

### 安全設計（2 ステップ適用 ＋ 1 クリック Undo）

AI の応答は直ちにプロジェクトへ反映されません。必ず以下の流れを通ります。

1. 送信後、アプリが応答 JSON を `dry_run_validate` で事前検証し、構文や整合性に問題があれば**プロジェクトを一切変更せず**エラー表示します。
2. 検証に成功するとチャット欄下に **適用差分カード** が表示され、変更内容の要約を確認できます。
3. **`承認して適用`** を押すとバッチ全体が 1 つのアトミック編集として適用されます（不要なら **`破棄`**）。
4. 適用直後でも、失敗時・意図しない結果だった場合は **`直前の AI 適用を取り消す (Undo)`** ボタンで 1 クリック巻き戻しできます。

### 選択できる AI プロバイダと認証設定

画面上部の **API コンボボックス** から利用する AI サービス・ローカルエンドポイント・プラグインを切り替えられます。API キーなどの機密情報は **絶対に `.smc.json` プロジェクトファイルへ保存されません**。環境変数または OS 資格情報ストア（service 名: `sea-movie-creator`）から解決されます。

| コンボボックス表示名 | 対象サービス / プロトコル | 既定ベース URL / CLI | 認証用の環境変数 / 備考 |
| :--- | :--- | :--- | :--- |
| `OpenAI API` | OpenAI Chat Completions (SSE) | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `OpenRouter API` | OpenRouter API | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `Local (Ollama / LM Studio)` | Ollama / LM Studio / vLLM (`OpenAI` 互換) | `http://127.0.0.1:11434/v1` | ローカルサーバーは既定で認証キー任意（必要な場合のみ `OPENAI_API_KEY`） |
| `Google Gemini API` | Google Gemini REST API | `generativelanguage.googleapis.com` | `GEMINI_API_KEY` |
| `Anthropic Claude API` | Anthropic Messages API | `api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `Antigravity (agy CLI)` | Google Antigravity CLI (`sea.ai.antigravity`) | `agy acp --sandbox --stdio` | 公式外部エディタ向け ACP プロトコル使用。先に `agy`（または `agy acp --login`）でサインインし `PATH` を通す（パス上書きは `SEA_ANTIGRAVITY_CLI`）。※ 安全注意事項は下記参照 |
| `ACP CLI (Claude / Codex / Copilot)` | 外部エージェント (`sea.ai.acp` / ACP over stdio) | `claude` / `codex` / `copilot` | 各公式 CLI が内部で使用するプロファイルに従います。CLI パスは `SEA_ACP_CMD`、引数は `SEA_ACP_ARGS` で上書き可能 |

### モデル・思考レベルの選択と送信操作

- **モデル選択・思考レベル (`Low` / `Medium` / `High` / `Extra` / `Auto`)**:
  入力欄左下のモデルメニュー（例: `Gemini 4 Argon Medium ▾`）を開くと、選択中プロバイダに応じたモデル一覧が表示されます。推論強度を切り替えられるモデルでは、モデル名に続けて思考レベルのサブメニューが開き、さらに隣の **`思考:`** ボタンからも現在のモデルの思考レベルを 1 クリックで切り替えられます（非対応モデル選択時は自動で無効化 / `Auto` に戻ります）。
  - **Antigravity (`agy CLI`)**: `Gemini 4 Hydrogen` では `Extra` (`xhigh`) まで切り替えられ、`Orchestrator (Argon)` など非対応モデルでは自動的にスキップされます。
  - **Gemini / Claude / OpenAI / OpenRouter**: 対応モデル選択時、それぞれ REST / API 仕様に合わせた `thinkingLevel` / `thinkingBudget` / `thinking.budget_tokens` / `reasoning_effort` が自動付与されます。
- **送信ショートカット**: 入力欄で **`Enter`** を押すとそのまま送信され、`送信` ボタンからも送信できます。

> [!WARNING]
> **Antigravity (`agy acp`) 利用時の利用規約・アカウント保護に関する注意**
> - 本アプリの `sea-ai-antigravity` は OAuth トークンの抜き取りや非公開 API 直叩きを一切行わず、Google Antigravity が外部エディタ統合用として公式提供する **`agy acp --sandbox --stdio`**（一時サンドボックスディレクトリ・ツール権限拒否構成）のみを呼び出します。
> - ただし Google Antigravity 利用規約（Section 6 等）は不正アクセスや過剰な自動リクエストを禁止しているため、短時間の連続送信やスクリプトによる自動ループ利用は、誤検知を含めアカウント側（Antigravity / Gemini CLI / Gemini Code Assist 共通）の `403` 利用制限につながるリスクをゼロにできません。
> - アプリ内の日常的・高頻度な AI 編集支援には **Google AI Studio / Vertex AI の `GEMINI_API_KEY`（`Gemini` ボタン）** または **ローカル LLM（`Local` ボタン）** の利用を推奨します。また、普段お使いの Antigravity IDE / CLI 側から本アプリの MCP サーバー（`sea-movie-creator mcp --project ...`）へ接続する構成も安全に利用できます。

> [!TIP]
> **Windows PowerShell での環境変数設定例（起動前の一時設定）:**
> ```powershell
> $env:GEMINI_API_KEY = "your-api-key"
> .\sea-movie-creator.exe
> ```
> ※ API キーやトークンをリポジトリ内ファイルにコミットしないでください。

---

## 2. CLI オプションとローカル MCP サーバー

`sea-movie-creator` 実行ファイルは GUI エディタに加えて、外部 AI エージェントや自動化スクリプト向けの CLI / MCP（Model Context Protocol）サーバーとして動作します。

### コマンドラインオプション一覧

```bash
# GUI ウィンドウを開いて起動
sea-movie-creator

# 指定プロジェクトを読み込んで起動
sea-movie-creator --open path/to/project.smc.json

# 指定ミリ秒後にウィンドウを自動終了（起動確認・スモークテスト向け）
sea-movie-creator --auto-close-ms 300

# プロジェクト JSON スキーマ (Draft 2020-12 / schemars) を標準出力へ出力
sea-movie-creator --print-document-schema

# 編集コマンド (EditCommand) JSON スキーマを標準出力へ出力
sea-movie-creator --print-command-schema

# ローカル MCP サーバー (JSON-RPC 2.0 over stdio) を起動
sea-movie-creator mcp --project path/to/project.smc.json
```

### MCP サーバーが提供するツール（`tools/list`）

MCP クライアント（Antigravity、Claude Code、Codex 等）の MCP 設定へ `sea-movie-creator mcp --project <project.smc.json>` を登録すると、以下の 6 ツールを呼び出せます。編集ツール適用後は対象プロジェクトファイルへ安全に永続化されます。

- `get_timeline_summary`: プロジェクト解像度・FPS・レイヤー/クリップ概要を取得
- `get_command_schema`: `EditCommand` の JSON Schema を取得
- `preview_edit_command`: プロジェクトを変更せずに `EditCommand` バッチの適用可否・変更プレビューを確認
- `apply_edit_command`: バッチコマンドをアトミック実行し Undo 履歴とファイル保存を更新
- `undo`: 直前のコマンド履歴を 1 段階戻す
- `redo`: Undo した操作をやり直す

> [!NOTE]
> ループバック HTTP 版 MCP サーバー（`127.0.0.1` 限定バインド）を利用する API では、起動ごとに生成されるランダム Bearer トークン（`sea_mcp_...`）による認証が必須となっており、他ホストからの外部アクセスを遮断します。

---

## 3. 外部音声プラグイン（VST3 / CLAP）と隔離ホスト

`sea-movie-creator` の音声処理は、サードパーティ製 VST3 / CLAP プラグインのクラッシュによって動画編集本体や未保存プロジェクトが巻き込まれないよう、専用ワーカープロセス **`sea-audio-host`** 内で実行されます。

- **VST2 は対象外**: Steinberg 社のライセンス・SDK 配布終了方針に従い、VST3 およびオープン規格 CLAP のみをサポートしています。
- **メインプロセスでの安全な検査**: バンドル走査時はバイナリを本体側へロード（`dlopen` / `LoadLibrary`）せず、OS の実行形式ヘッダ（PE `MZ` / ELF `\x7fELF` / Mach-O マジックナンバー）を静的に検査して破損ファイルを事前除外します。
- **クラッシュ時の自動バイパス**: 万が一プラグインがハング・異常終了・パイプ切断しても、ホスト側は即座にバイパス状態（`is_plugin_slot_bypassed`）へ移行し、未加工の dry 音声を無傷のまま再生・書き出しに流し続けます。

### プラグイン探索パス（環境変数で追加可能）

標準の OS プラグインディレクトリに加えて、以下の環境変数で検索先ディレクトリを指定できます。

| 種別 | 環境変数オーバーライド | 既定の探索ディレクトリ（例） |
| :--- | :--- | :--- |
| **VST3** | `SEA_VST3_PATH` | **Windows**: `%ProgramFiles%\Common Files\VST3`, `%LOCALAPPDATA%\Programs\Common\VST3`<br>**macOS**: `/Library/Audio/Plug-Ins/VST3`, `~/Library/Audio/Plug-Ins/VST3`<br>**Linux**: `/usr/lib/vst3`, `/usr/local/lib/vst3`, `~/.vst3` |
| **CLAP** | `SEA_CLAP_PATH` | **Windows**: `%ProgramFiles%\Common Files\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`<br>**macOS**: `/Library/Audio/Plug-Ins/CLAP`, `~/Library/Audio/Plug-Ins/CLAP`<br>**Linux**: `/usr/lib/clap`, `/usr/local/lib/clap`, `~/.clap` |

テスト・動作確認用には外部バイナリ不要の内蔵テスト DSP（`sea.test.gain`）も `sea-audio-host` 内に同梱されています。
