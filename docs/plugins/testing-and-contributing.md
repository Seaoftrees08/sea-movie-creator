# プラグインのテスト・デバッグ・コントリビューションガイド

このドキュメントでは、作成したプラグインを **GUI を開かずに素早くテスト・デバッグする方法** と、**本リポジトリ（`plugins/` ＋ `plugins_src/`）へ新規プラグインをコントリビュート（PR）する際のルール** をまとめます。

---

## 1. スタンドアロンでのテスト・デバッグ方法

### 1.1 標準入出力プロトコルを単体で検証する

書き出しプラグインや AI プラグインは標準入出力（NDJSON）で動作するため、まず本ソフトを起動せずにコマンドラインから 1 行 JSON を流し込んで挙動を確認できます。

1. **`stdout` の純粋性を確認する**
   - プラグインの `stdout` 出力を 1 行ずつ `jq` や Python の `json.loads` に通し、すべての行が有効な JSON イベントになっているか検証します。
   - 初期化メッセージやエラーログが 1 行でも `stdout` に混入すると、ホスト側で `ProtocolError::Json` が発生します。
2. **ダミー実行での統合テストを参考にする**
   - 書き出しプラグインホストの統合テストは [`tests/phase_b_export_plugin_host/plugin_host_ipc.rs`](../../tests/phase_b_export_plugin_host/plugin_host_ipc.rs) にあり、実 `ShmHostRegion`・`PluginRegistry`・キャンセル（`ExportCancelToken`）・クラッシュ耐性の検証パターンを確認できます。
   - AI プラグインの統合テストは [`tests/phase_c_ai_plugins/acp_and_mcp_roundtrip.rs`](../../tests/phase_c_ai_plugins/acp_and_mcp_roundtrip.rs) とフィクスチャ [`examples/ai_cli_fixture.rs`](../../examples/ai_cli_fixture.rs) を参照してください。
   - 音声隔離ホストの統合テストは [`tests/phase_c_audio_plugins/isolated_host_resilience.rs`](../../tests/phase_c_audio_plugins/isolated_host_resilience.rs) を参照してください。

### 1.2 開発中のバイナリをアプリ本体から読み込ませる

ワークスペース外でビルドしたプラグインを手元の `sea-movie-creator` で試す場合は、環境変数で探索ディレクトリを差し替えると配布フォルダを汚さずに検証できます。

```powershell
# Windows PowerShell の例
$env:SEA_EXPORT_PLUGIN_DIR = "C:\work\my-export-plugins"
.\target\release\sea-movie-creator.exe
```

```bash
# macOS / Linux の例
SEA_EXPORT_PLUGIN_DIR="$HOME/work/my-export-plugins" ./target/release/sea-movie-creator
```

### 1.3 よくあるトラブルと原因

| 症状 | 主な原因と対処 |
| :--- | :--- |
| 書き出しダイアログにプラグインが出ない | ① `plugin.json` の構文・必須項目エラー（`PluginRegistry::warnings()` に記録されます）<br>② `license.requires_external_binary: true` にしており、アプリ側で外部 `ffmpeg` が未設定のため非表示になっている（[書き出しガイド §3](export-plugins.md#3-pluginjson-マニフェスト仕様リファレンス) 参照） |
| 実行時に `MissingExecutable` エラーになる | `entrypoint` に指定したファイルが `plugin.json` と同じ階層にも `target/{debug,release}/` にも存在しない。ワークスペース開発時は `cargo build --workspace --all-targets` を実行してください |
| `READY_TIMEOUT`（30 秒）で失敗する | `Init` を読み込んだ後、`Ready` イベントを書き込んで `flush()` していない、または `stdout` に JSON 以外の文字列を出している |
| `SLOT_ACK_TIMEOUT`（30 秒）で失敗する | `ProcessVideoSlot` / `ProcessAudioSlot` 受信後に `SlotAck` を返していない、または共有メモリ処理が 30 秒以上ブロックしている |
| プラグイン側でデバッグ中にサブプロセスの黒窓が一瞬出る（Windows） | プラグインが内部でさらに外部 CLI（`ffmpeg` 等）を起動する際、`Command` に `CREATE_NO_WINDOW`（`0x0800_0000`）を設定してください |

---

## 2. 本リポジトリへプラグインを PR する場合の規約

自作プラグインを `sea-movie-creator` リポジトリ本体（`plugins/` と `plugins_src/`）へ同梱したい場合の基準です。

### 2.1 ディレクトリ配置とワークスペース設定

1. ルート [`Cargo.toml`](../../Cargo.toml) の `[workspace]` `members`（`"plugins_src/*"`）に含まれるよう、`plugins_src/<crate-name>/` へクレートを配置する。
2. クレートの `Cargo.toml` には必ずワークスペース設定の継承を記述する（[`tests/quality_gates/repository_rules.rs`](../../tests/quality_gates/repository_rules.rs) で自動検査されます）：
   ```toml
   [package]
   name = "sea-export-myformat"
   version = "0.1.0"
   edition.workspace = true
   rust-version.workspace = true
   license.workspace = true

   [lints]
   workspace = true
   ```
3. マニフェストは `plugins/export/<short-name>/plugin.json`（または `plugins/ai/<short-name>/ai-plugin.json`）へ配置し、`entrypoint` が `plugins_src/` 内バイナリクレートの `[package] name` と一致するようにする（[`tests/quality_gates/plugin_manifests.rs`](../../tests/quality_gates/plugin_manifests.rs) で全件検査されます）。

### 2.2 Rust 品質・安全境界ルール（`[workspace.lints]` 準拠）

本リポジトリはルート `Cargo.toml` にて全ワークスペースクレートへ厳格なリントを適用しています。

- **パニック・未実装マクロ禁止**: `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro` はすべて `deny`。`Result` / `Option` で安全にエラー伝播してください。
- **Pedantic Clippy & 公開ドキュメント必須**: `clippy::pedantic = "deny"` および `missing_docs = "deny"` が有効です。クレートの公開アイテムには `///` ドキュメントコメントを記述し、`RUSTDOCFLAGS="-D warnings" cargo doc` で警告ゼロを維持してください。
- **`unsafe` コード原則禁止と `backends/` 隔離**:
  - `unsafe_code = "deny"` および `undocumented_unsafe_blocks = "deny"` が有効です。
  - OS API・FFI・`mmap` 等で `unsafe` が不可避な場合は、必ず **`backends/` パス配下**（例: `src/backends/`）の専用モジュールへ隔離し、最小範囲に `#[allow(unsafe_code)]` を付けた上で**直前に `// SAFETY:` コメントで安全不変条件を明記**してください（`backends/` 外での `allow(unsafe_code)` は `tests/quality_gates.rs` で拒否されます）。
- **ライセンス・特許ポリシー**:
  - コントリビューションは [`README.md`](../../README.md) に記載の **`MIT OR Apache-2.0`** デュアルライセンスで提供されます。
  - GPLv2 / GPLv3 / AGPL 依存クレートや再配布制限のある特許コーデックバイナリをプラグインの必須依存として同梱しないでください（[`deny.toml`](../../deny.toml) の `cargo deny check` およびマニフェスト `license.spdx` 検査でブロックされます。外部ツール連携はユーザー環境への別プロセス呼び出しとして分離してください）。

### 2.3 テストの配置と PR 前必須チェック

- **単体テスト**: 各 `.rs` ファイル末尾の `#[cfg(test)] mod tests { ... }` に配置します。
- **結合テスト**: `tests/<実装名>.rs`（入り口）＋ `tests/<実装名>/mod.rs` の組で配置します。
- **重い実機依存テストの分離**: 外部 CLI バイナリ・実 GPU・OS ハードウェアエンコーダ・実オーディオデバイスに依存しヘッドレス CI（Linux / macOS / Windows）で実行できないテストには `#[ignore = "理由"]` を付与し、通常の `cargo test` が環境非依存で全通過する状態を維持してください。

PR 作成前に、統一スクリプト（[`scripts/pre-pr.ps1`](../../scripts/pre-pr.ps1) / [`scripts/pre-pr.sh`](../../scripts/pre-pr.sh)）または以下のチェック群（[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) と同一基準）が警告・エラーゼロで通ることを確認してください。

```powershell
# Windows PowerShell（一括実行）
powershell -ExecutionPolicy Bypass -File scripts/pre-pr.ps1
```

```bash
# macOS / Linux（一括実行）
./scripts/pre-pr.sh
```

```bash
# 個別コマンド（Rustdoc・品質ゲートテスト・ライセンス監査含む）
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo build --workspace --all-targets
cargo test --workspace --all-targets --all-features
cargo deny check
```
