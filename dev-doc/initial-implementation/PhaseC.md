# 初期実装設計書：Phase C（AI チャット＆プラグイン・高度音声・VST3/CLAP・マルチ OS 拡張）

- **ステータス**: Draft
- **作成日**: 2026-11-15
- **更新日**: 2026-11-15
- **関連ドキュメント**:
  - 全体基本設計書: [`../draft.md`](../draft.md)
  - 前段階設計書: [`PhaseB.md`](PhaseB.md)
  - 将来機能設計書（初期実装完了後）: [`../001-yukkuri-live-module.md`](../001-yukkuri-live-module.md)
  - 開発・テスト配置ルール: [`../../.github/copilot-instructions.md`](../../.github/copilot-instructions.md)

---

## 1. Phase C のゴールとスコープ

Phase C では、**「VS Code 風 AI チャット＆拡張エンドポイントプラグイン（Antigravity / Claude Code / Codex CLI / GitHub Copilot / OpenRouter / Gemini / Anthropic / OpenAI 互換）」**、**「PhaseA-4 最小ミキサーの高度音楽モジュール拡張（再生速度・音量 `f(t)`）」**、**「VST3 + CLAP 外部音声プラグインホスト」**、および **「macOS/Linux バックエンド・H.265 等追加書き出しプラグイン・マルチ OS 総合検証」** を完成させる。

※ ゆっくり実況向け専用モジュールは初期設計範囲外のため、Phase C 完了後の将来機能として [`../001-yukkuri-live-module.md`](../001-yukkuri-live-module.md) に独立定義している。

---

## 2. 作業単位一覧

- **PhaseC-1**: VS Code 風 AI チャット UI・`schemars` 駆動コンテキスト・組み込み HTTP プロバイダ群
- **PhaseC-2**: AI エンドポイントプラグイン（Antigravity / ACP: Claude Code・Codex・Copilot）＋ローカル MCP サーバー
- **PhaseC-3**: 音楽モジュール拡張（`AudioMixer` 拡張：有理 `PlaybackRate` タイムストレッチ・音量 `f(t)`・波形キャッシュ）
- **PhaseC-4**: 外部音声プラグインホスト（VST3 [MIT SDK 準拠] + CLAP [MIT] 別プロセス隔離実行）
- **PhaseC-5**: macOS / Linux `MediaBackend` 実装・追加書き出しプラグイン（H.265 等）・マルチ OS 総合検証

---

## PhaseC-1：VS Code 風 AI チャット UI と組み込み HTTP プロバイダ群

### 目的と対象モジュール
右パネルに VS Code 風 AI チャット UI を実装し、`schemars` で自動生成した `EditCommand` JSON Schema とコンパクト要約 JSON を用いて、安全かつ正確にタイムラインを操作できるようにする。

- **主な対象ファイル（計画）**:
  - `src/ai/provider.rs`（`AiProvider` トレイト、ストリーミング応答イベント）
  - `src/ai/context_builder.rs`（現在再生時刻・選択中クリップ・レイヤー概要 JSON＋`schemars` スキーマ供給）
  - `src/ai/planner.rs`（AI 出力からの `EditCommand` パース、`dry_run_validate` 検証、差分サマリ生成）
  - `src/ai/providers/openai_compat.rs`（OpenAI / **OpenRouter** / Ollama / LM Studio / vLLM 共通クライアント）
  - `src/ai/providers/gemini.rs`（Google Gemini API クライアント）
  - `src/ai/providers/anthropic.rs`（Anthropic Messages API クライアント）
  - `src/ai/providers/mock.rs`（ネットワーク不要決定論テスト用モックプロバイダ）
  - `src/ui/ai_chat_panel.rs`（VS Code 風スレッド、プロバイダ選択、適用差分カード、承認／取消）

### 設計仕様
- **認証情報管理**: API キーはプロジェクトファイルへ保存せず、環境変数または OS 資格情報ストア（`keyring` 等）から取得する。
- **バッチ仮 ID 連携**: チャット AI が「新規クリップ追加＋アニメーション設定＋途中カット」を 1 回の `EditCommand::Batch` 内で `ClipRef::Temp("$c1")` を使って指示でき、ユーザーはワンクリックで一括適用または一括 Undo できる。
- **環境依存テスト分離**: 実外部 HTTP エンドポイントを叩く疎通テストは `#[ignore = "requires live LLM API credentials"]` とし、`MockAiProvider` とローカル JSON フィクスチャを用いた全テストを常時実行する。

### 動作確認とテストの実施
1. **単体テスト**（`src/ai/context_builder.rs`, `src/ai/planner.rs`, `src/ai/providers/` 各末尾）:
   - `schemars` による `EditCommand` スキーマ生成が `Batch` / `SetParameter` / `ClipRef::Temp` を正しく表現すること。
   - 不正 JSON・存在しない ID 応答の安全拒否、API キー非漏洩（`Debug` 実装マスク）テスト。
2. **結合テスト**:
   - `tests/phase_c_ai_chat.rs`
   - `tests/phase_c_ai_chat/mod.rs`
   - `tests/phase_c_ai_chat/chat_command_execution.rs`
   （`MockAiProvider` で自然言語指示→`EditCommand::Batch`（`ClipRef::Temp` 使用）→`CommandDispatcher` 適用→単一 Undo の一連を検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` 通過。

---

## PhaseC-2：AI エンドポイントプラグイン（Antigravity / ACP）とローカル MCP サーバー

### 目的と対象モジュール
エンドポイントの増加に耐えられるプラグイン機構（`plugins/ai/`）を実装し、**Antigravity（`agy` CLI）**・**Claude Code**・**Codex CLI**・**GitHub Copilot** を正規インタフェース経由で接続する。あわせて外部エージェントから本ソフトを駆動できるローカル MCP サーバーを提供する。

- **主な対象ファイル（計画）**:
  - `src/ai/plugin_host.rs`（`plugins/ai/<id>/ai-plugin.json` 走査、JSON-RPC over `stdio`）
  - `plugins_src/sea-ai-antigravity/`（`[lints] workspace = true`、公式 `agy` CLI の `--print --input-format stream-json --output-format stream-json --json-schema` 呼び出しアダプタ）
  - `plugins_src/sea-ai-acp/`（`[lints] workspace = true`、**ACP [Agent Client Protocol]** 汎用アダプタ：`Claude Code` / `Codex CLI` / `GitHub Copilot [--acp --stdio]`）
  - `src/ai/mcp_server.rs`（外部エージェント向けローカル MCP サーバー）

### 設計仕様
- **正規接続の徹底**: 認証トークン抽出や迂回は一切行わず、ユーザー自身の環境に導入・認証された公式 CLI / ACP 規格のみを使用する。
- **ローカル MCP サーバーの通信方式と認証**:
  - **標準方式（第 1 推奨）**: **`stdio` トランスポート**（`sea-movie-creator mcp --project <path>`）として動作し、呼び出し元エージェントプロセスとの標準入出力パイプのみで安全に通信する。
  - **起動中アプリへのローカル接続オプション**: ローカルソケット／HTTP 待ち受けを使う場合は、**`127.0.0.1` ループバック限定バインド＋起動ごとにランダム生成する Bearer トークン認証（または同一ユーザー権限のローカル名前付きパイプ / UNIX ドメインソケット）** を必須とし、外部ネットワーク露出を防ぐ。
- **環境依存テスト分離**: 実 `agy` / `claude` / `codex` / `copilot` CLI を呼ぶテストは `#[ignore = "requires installed external AI CLI"]` とし、CI ではテスト用 Mock CLI / Mock ACP バイナリを用いて検証する。

### 動作確認とテストの実施
1. **単体テスト**（`src/ai/plugin_host.rs`, `src/ai/mcp_server.rs` 各末尾）:
   - `ai-plugin.json` 検証、CLI 未導入時の親切なエラー、MCP ツール定義・Bearer トークン検証拒否テスト。
2. **結合テスト**:
   - `tests/phase_c_ai_plugins.rs`
   - `tests/phase_c_ai_plugins/mod.rs`
   - `tests/phase_c_ai_plugins/acp_and_mcp_roundtrip.rs`
   （ダミー stream-json バイナリ / ダミー ACP バイナリとの往復、および `stdio` MCP サーバーへの `tools/call` で `ProjectDocument` が更新される結合検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` 通過。

---

## PhaseC-3：音楽モジュール拡張（再生速度・ピッチ制御・音量 `f(t)`・ミキサー拡張）

### 目的と対象モジュール
PhaseA-4 で先行実装した最小 `AudioMixer` を拡張し、**PhaseA-1 で `Clip` に定義した有理再生速度 `Clip.playback_rate`** に連動したタイムストレッチ／リサンプル、音量・パンの `AnimatedParameter` オートメーション、ソフトクリッピング保護、および波形ピークキャッシュを完成させる。

- **主な対象ファイル（計画）**:
  - `src/audio/properties.rs`（`AudioProperties`：`volume: AnimatedParameter`, `pan: AnimatedParameter`, `preserve_pitch: bool`, `plugin_slots: Vec<AudioPluginSlot>`。※ ピッチ維持 ON/OFF は `SetParameter { ..., value: ParameterValue::Bool(...) }`、速度変更は `EditCommand::SetPlaybackRate { clip, rate }` を使用）
  - `src/audio/time_stretch.rs`（ピッチ非維持リサンプル `rubato`／ピッチ維持 WSOLA タイムストレッチ処理）
  - `src/audio/mixer.rs`（PhaseA-4 ミキサーの拡張：エンベロープ適用・タイムストレッチ統合・リミッター）
  - `src/audio/waveform.rs`（タイムライン描画用の非同期ピークキャッシュ）

### 動作確認とテストの実施
1. **単体テスト**（`src/audio/properties.rs`, `src/audio/time_stretch.rs`, `src/audio/mixer.rs` 各末尾）:
   - 有理 `PlaybackRate`（例：`3/2`, `1/2`）適用時のサンプル位置整合および出力サンプル長テスト。
   - 多重加算時のリミッター動作（絶対値 `<= 1.0`、有限値維持）テスト。
   - `volume = f(t)` のカット分割前後での連続性テスト。
2. **結合テスト**:
   - `tests/phase_c_audio_mixing.rs`
   - `tests/phase_c_audio_mixing/mod.rs`
   - `tests/phase_c_audio_mixing/speed_and_volume_automation.rs`
   （異なる再生速度と音量 `AnimatedParameter` を持つ複数正弦波トラックを途中でカット分割し、`AudioMixer` 出力 PCM の振幅変化が仕様どおりであることを検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` 通過。

---

## PhaseC-4：外部音声プラグインホスト（VST3 + CLAP 別プロセス隔離実行）

### 目的と対象モジュール
音楽モジュールへ **VST3（MIT ライセンス移行後 SDK 準拠）** および **CLAP（MIT ライセンス）** の外部音声プラグイン対応を組み込む。**VST2 は提供終了とライセンス制約のため対象外** とする。

- **主な対象ファイル（計画）**:
  - `src/audio/plugins/descriptor.rs`（`PluginFormat::Vst3 | PluginFormat::Clap`、パラメータ一覧）
  - `src/audio/plugins/scanner.rs`（Win/Mac/Linux 標準 VST3/CLAP ディレクトリ走査）
  - `src/audio/plugins/isolated_host.rs`（別プロセス管理、共有メモリ PCM リングバッファ、パラメータ変更・異常終了時バイパス復帰）
  - `plugins_src/sea-audio-host/`（`[lints] workspace = true`、分離ワーカー実行体：FFI 境界の `#[allow(unsafe_code)]` + `// SAFETY:` をこのクレート内に完全隔離）

### 設計仕様
- **統一コマンド連携**: プラグインパラメータ変更も `EditCommand::SetParameter { target: ParameterTarget::AudioPlugin { ... }, path, value: ParameterValue::Animated(...) }` を経由する。
- **環境依存テスト分離**: PC 上のサードパーティ実 `.vst3` / `.clap` を読み込むテストは `#[ignore = "requires third-party VST3/CLAP plugins installed"]` とし、通常テストではワークスペース内の最小テスト用エフェクトワーカー（例：テストモードでゲイン 0.5 倍処理を返す隔離ワーカー）で通信・クラッシュ隔離を自動検証する。

### 動作確認とテストの実施
1. **単体テスト**（`src/audio/plugins/descriptor.rs`, `src/audio/plugins/scanner.rs` 各末尾）:
   - OS 別標準ディレクトリ走査・不正バンドル除外・パラメータ値正規化テスト。
2. **結合テスト**:
   - `tests/phase_c_audio_plugins.rs`
   - `tests/phase_c_audio_plugins/mod.rs`
   - `tests/phase_c_audio_plugins/isolated_host_resilience.rs`
   （隔離ワーカーを別プロセス起動して PCM ブロックへゲイン処理を実行→ワーカープロセス強制 `kill` 後に親 `AudioMixer` がパニックせず安全にバイパス復帰することを自動検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` 通過。

---

## PhaseC-5：macOS / Linux バックエンド・追加書き出しプラグイン（H.265 等）・総合検証

### 目的と対象モジュール
Phase A で Windows に先行させていた `MediaBackend` および書き出しプラグインを **macOS（VideoToolbox / AVFoundation）**・**Linux（VA-API / Vulkan Video）** へ拡張し、さらに H.265 等の別形式書き出しプラグインを追加してマルチ OS 総合検証体制を確立する。

- **主な対象ファイル（計画）**:
  - `src/media/backends/macos_vt.rs`（`#[cfg(target_os = "macos")]`、`#[allow(unsafe_code)]` + `// SAFETY:`）
  - `src/media/backends/linux_vaapi.rs`（`#[cfg(target_os = "linux")]`、`#[allow(unsafe_code)]` + `// SAFETY:`）
  - `plugins_src/sea-export-h265/`（`[lints] workspace = true`、1 形式＝1 モジュール原則に沿った H.265 書き出しプラグイン：OS ネイティブ HW API＋ユーザー導入外部ツール方式）
  - `.github/workflows/ci.yml`（Windows / macOS / Linux の 3 OS で `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace --all-targets --all-features` を実行する CI 設定）

### 動作確認とテストの実施
1. **単体テスト**（`plugins_src/sea-export-h265/` および OS 別バックエンド設定変換の末尾）:
   - H.265 プロファイル設定検証、OS 非依存コア境界チェック。
2. **結合テスト**:
   - `tests/phase_c_full_integration.rs`
   - `tests/phase_c_full_integration/mod.rs`
   - `tests/phase_c_full_integration/end_to_end_pipeline.rs`
   （プロジェクト新規作成 → `MockAiProvider` で動画・字幕・BGM 配置＆`f(t)` 設定 → トリム・カット編集 → Undo/Redo → `AudioMixer` ミキシング → `sea-export-av1` 書き出し → 純粋 Rust デコーダでの再確認を通し実行。さらに複数プラグイン動的認識を確認）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` ゼロ警告・ゼロエラー。
