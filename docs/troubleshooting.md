# トラブルシューティング・FAQ（アルファ版仕様）

`sea-movie-creator` Alpha v0.1.0 を利用する上で発生しやすい状況と解決策、および現時点の仕様・制限事項をまとめています。

---

## Q1. 「有効なプラグインが見つかりませんでした」／AI や音声プラグインが動かない

書き出し（`sea-export-av1`, `sea-export-h264`, `sea-export-h265`）、AI CLI アダプタ（`sea-ai-acp`, `sea-ai-antigravity`）、音声隔離ホスト（`sea-audio-host`）は、本体と同じフォルダに存在する**別実行ファイル ＋ `plugins/` マニフェスト**を必要とします。

- **確認点 1（配布フォルダ）**: `sea-movie-creator.exe` の横に各 `sea-*.exe` と `plugins/export/`, `plugins/ai/` フォルダが揃っているか確認してください。
- **確認点 2（ソースビルド時）**: `cargo run` だけではプラグイン側バイナリがビルドされない場合があります。必ずワークスペース全体を事前ビルドしてください。
  ```bash
  cargo build --release --workspace
  # またはデバッグ開発時
  cargo build --workspace --all-targets
  ```
- **確認点 3（カスタムパス）**: プラグインフォルダを移動した場合は環境変数 `SEA_EXPORT_PLUGIN_DIR` / `SEA_AI_PLUGIN_DIR` を設定してください。

---

## Q2. プレビュー上部に赤い「Media Offline」表示が出て、タイムラインのクリップも赤くなった

プロジェクトファイル（`.smc.json`）に記録されている元ファイルが見つからない状態です。エディタ自体はクラッシュせず、赤色のオフライン表示として安全に編集を継続できます。

- **対処法**:
  - 元の動画・音声・静止画ファイルを元の相対位置に戻してからプロジェクトを再読込するか、正しいファイルへ再リンクしてください。
  - プロジェクト保存時は相対パスを優先するため、動画素材一式と `.smc.json` を同じ親ディレクトリ配下にまとめて管理することを推奨します。

---

## Q3. H.264 や H.265 で動画書き出しに失敗する（`ffmpeg` 未導入／HW 非対応）

`sea-movie-creator` は特許・GPL エンコーダを同梱していません。そのため環境によって次の挙動になります。

- **外部 `ffmpeg` モードでエラーが出る場合**:
  - `ffmpeg` コマンドがシステムに未インストールか、`PATH` に含まれていません。`ffmpeg` を導入して再実行するか、書き出しダイアログの `ffmpeg_path` へ絶対パスを指定してください。
- **`OS HW`（Media Foundation AV1 / H.264 / H.265）モードで失敗する場合**:
  - お使いの GPU ドライバや OS エディションが該当コーデックのハードウェア MFT を提供していない可能性があります。
- **確実に書き出したい場合**:
  - エンコーダプラグインで **`標準 AV1 (sea.export.av1)`** → プリセット **`av1_fast`**（または `av1_quality`）を選択してください。純粋 Rust (`rav1e` + `flacenc`) で動作するため外部 `ffmpeg` や GPU MFT が不要です。
  - なお、書き出し途中で失敗・中断した場合は出力先の途中ファイルを残さない仕組みになっています（元から存在していた上書き対象以外は安全に掃除されます）。

---

## Q4. AI チャットが「認証未設定」と表示される／Antigravity・ACP が起動しない

- **Cloud API (`OpenAI` / `OpenRouter` / `Gemini` / `Claude`)**:
  - それぞれ `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `GEMINI_API_KEY`, `ANTHROPIC_API_KEY` を起動前シェルで設定するか、OS 資格情報ストアに登録してください（[AI 連携・MCP ガイド](ai-and-plugins.md) 参照）。
- **`Local` (Ollama / LM Studio / vLLM)**:
  - 既定では `http://127.0.0.1:11434/v1` に接続します。ローカル LLM サーバーが起動しているか確認してください。
- **`Antigravity` (`agy acp --sandbox`) / `ACP CLI`**:
  - `agy`, `claude`, `codex`, `copilot` 等の外部公式 CLI をあらかじめインストールし、各自の公式手順（`Antigravity` の場合は `agy acp --login` 等）でサインインを完了した状態で `PATH` を通してください（本ソフト側でログインや OAuth トークン取得は行いません）。
  - 実行コマンドパスを明示したい場合は `SEA_ANTIGRAVITY_CLI` / `SEA_ACP_CMD` を設定してください。
  - `Antigravity` タブは公式の `agy acp --sandbox --stdio` を呼び出しますが、過度な連続送信による利用制限リスクを避けたい場合は `Gemini` タブ（`GEMINI_API_KEY`）または `Local`（Ollama）をご利用ください。

---

## Q5. オーディオデバイスがない環境やリモートデスクトップでも動作しますか？

はい。再生クロックは `cpal` による実オーディオデバイス同期（`AudioMasterClock`）を優先しますが、デバイスが検出・オープンできない環境では自動的にシステム単調時計（`SystemPlaybackClock (fallback)`）へフォールバックしてプレビュー再生・タイムライン編集・オフライン書き出しを継続できます。右パネルの `Project` タブの `同期クロック` 欄で現在の状態を確認できます。

---

## Q6. アルファ版（v0.1.0）時点での主な既知範囲・プラットフォーム注記

- **プロジェクトフレームレート**: プロジェクト作成後はフレームベース整合性維持のため FPS 固定となります。
- **macOS (`VideoToolbox`) / Linux (`VA-API`) ハードウェア支援**:
  - デコーダ抽象（`macos_vt.rs`, `linux_vaapi.rs`）と 3 OS CI マトリックス（[`.github/workflows/ci.yml`](../.github/workflows/ci.yml)）は整備済みですが、ローカル検証は主に Windows 環境で実施しており、macOS / Linux 実機でのハードウェアエンコード／デコード経路は環境固有テストおよび CI 実行対象となります。非対応または未初期化時はオープンソフトウェア経路へのフォールバックで動作します。
- **将来のゆっくり実況専用モジュール**:
  - 立ち絵口パク・音声合成連携等を含むゆっくり実況者向けモジュールは初期リリース対象外の将来機能として別設計（[`../dev-doc/001-yukkuri-live-module.md`](../dev-doc/001-yukkuri-live-module.md)）になっています。
