# [010] PR 前コード品質ゲート・自動テスト基盤

- **ステータス**: Approved
- **作成日**: 2026-11-18
- **更新日**: 2026-11-18
- **関連ドキュメント**:
  - 全体ガイドライン: [`.github/copilot-instructions.md`](../.github/copilot-instructions.md)
  - 開発・並行作業ルール: [`.github/instructions/general.instructions.md`](../.github/instructions/general.instructions.md)
  - CI ワークフロー: [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
  - Phase P0 設計書（`.smc.json` 後方互換要件）: [`004-phase-p0-core-prerequisites.md`](004-phase-p0-core-prerequisites.md)

---

## 1. 概要・目的

- **解決したい課題・達成したいこと**:
  - Worktree による並行開発を進める中で、AI エージェントおよび人間のどちらが実装したブランチであっても、**PR 提出前（およびマージ前）に一貫した品質・安全性・互換性基準を満たすこと**を自動検証できるようにする。
  - GitHub Actions の標準ランナー無料枠（特に Windows 2x / macOS 10x の消費倍率）を圧迫しないよう、OS 非依存チェックを `ubuntu-latest`（1x）単体ジョブへ集約し、ローカルでも同じ基準を 1 コマンドで再現可能にする。
- **対象ユーザー / 利用シーン**:
  - PR 作成前・Phase 完了前の開発者および AI コーディングエージェント（ローカル環境・GitHub Actions）。

---

## 2. スコープ

### 対象（In Scope）
1. **Rust Lint 強化（`Cargo.toml`）**:
   - `[workspace.lints.clippy]` へ `undocumented_unsafe_blocks = "deny"` を追加し、すべての `unsafe` ブロック直前に `// SAFETY:` コメントを必須化する。
   - 既存コードの未記載箇所（`plugins_src/sea-export-protocol/src/backends/mf_sink.rs` 等）へ安全不変条件コメントを補完する。
2. **Rustdoc 警告ゼロ検証**:
   - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` をローカル統一チェックおよび CI へ追加する。
3. **プロジェクト固有の品質ガード結合テスト（`tests/quality_gates.rs` + `tests/quality_gates/`）**:
   - プラグイン実マニフェスト整合検査（`plugins/export/*/plugin.json`, `plugins/ai/*/ai-plugin.json` と `plugins_src/*` の一致）。
   - 多言語カタログ（`lang/*.json`）の構文・必須キー・置換プレースホルダ（`{...}`）整合検査。
   - リポジトリ規約ガード（全ワークスペースメンバーの `[lints] workspace = true` 継承、`allow(unsafe_code)` の `backends/` 隔離、Antigravity ルール展開サイズ上限 `24,000` bytes 検査、シークレット混入パターン検査）。
   - `.smc.json`（`schema_version = 1`）ゴールデンフィクスチャ後方互換テスト。
4. **依存関係・ライセンス・脆弱性監査（`deny.toml` / `cargo-deny`）**:
   - RustSec 既知脆弱性・非許可ライセンス（GPL 系等）の混入をブロックする。
5. **ローカル統一実行スクリプト・CI・PR テンプレート・インストラクション同期**:
   - `scripts/pre-pr.ps1`（Windows 主系）および `scripts/pre-pr.sh`（macOS / Linux 用）。
   - `.github/workflows/ci.yml` の 2 ジョブ化（`quality-gates` + `check-and-test`）と `concurrency` キャンセル設定。
   - `.github/pull_request_template.md` の追加と、インストラクション内の完了前コマンド同期。

### 対象外（Out of Scope）
- `#[ignore]` 付きの重い実機テスト（GPU `wgpu` / OS HW エンコーダ / 外部 AI CLI / 実オーディオデバイス）の CI 常時実行。
- `cargo-llvm-cov` によるカバレッジ率のハードゲート化（開発速度とのトレードオフから本スコープ外）。
- Git `pre-commit` フックによる強制（Phase ごとの細切れコミット運用を妨げないため強制しない）。

---

## 3. 要件・制約

- **機能要件**:
  - ローカルで `pwsh -File scripts/pre-pr.ps1`（または `./scripts/pre-pr.sh`）を実行すれば、PR 前に必要なすべての検査が順序立てて実行され、失敗時に非ゼロ終了コードで即停止すること。
  - `cargo test --workspace --all-targets --all-features` 単独でも、第 2 層の品質ガード結合テスト群が環境依存なく数秒で全通過すること。
- **非機能要件（ランナー節約・クロスプラットフォーム・誤検知防止）**:
  - GitHub Actions では OS 非依存の検査（`fmt` / `doc` / `cargo-deny` / シークレットスキャン）を `ubuntu-latest` 単体ジョブのみで実行し、Windows/macOS ランナー消費を抑制する。
  - `concurrency` に `cancel-in-progress: true` を設定し、連続プッシュ時の冗長ジョブを自動停止する。
  - シークレット検査では、テスト用の明らかなプレースホルダ（`test-key`, `dummy`, `example`, `placeholder` 等を含むものや最小長未満）で偽陽性を出さないようにする。
  - `pre-pr` スクリプトはローカルに `cargo-deny` が未導入でも他の必須検査を完走させ、未導入時は警告・導入手順を出して判断できるようにする（`-RequireDeny` 等のフラグで厳格化可能）。

---

## 4. アーキテクチャ / 設計詳細

### 4.1 検査の 4 層構造

| 層 | 実行主体 | 内容 | 失敗時の扱い |
| --- | --- | --- | --- |
| 第 1 層 | Cargo (`scripts/pre-pr` + CI) | `fmt --all --check` / `clippy` / `doc (-D warnings)` / `build` / `test` | 即エラー停止 |
| 第 2 層 | `tests/quality_gates.rs` | マニフェスト整合 / `lang/*.json` / 規約ガード / `.smc.json` 後方互換 / シークレット検査 | `cargo test` 失敗 |
| 第 3 層 | `cargo-deny` + スクリプト | RustSec 脆弱性 / SPDX ライセンス許可リスト / ソース検証 | CI では必須エラー、ローカルは導入時エラー |
| 第 4 層 | CI + PR テンプレート + 指示書 | `quality-gates` (Ubuntu) + `check-and-test` (3 OS) + PR チェックリスト | PR マージ前確認 |

### 4.2 CI ワークフロー分割（`.github/workflows/ci.yml`）

```text
push / pull_request
 ├── quality-gates (ubuntu-latest, 1x)
 │    ├── cargo fmt --all --check
 │    ├── RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
 │    ├── EmbarkStudios/cargo-deny-action@v2 (advisories, licenses, bans, sources)
 │    └── scripts/pre-pr.sh --ci-static-only (秘密情報・規約の追加静的チェック)
 └── check-and-test (windows-latest / macos-latest / ubuntu-latest)
      ├── cargo clippy --workspace --all-targets --all-features -- -D warnings
      ├── cargo build --workspace --all-targets
      └── cargo test --workspace --all-targets --all-features
```

### 4.3 `tests/quality_gates/` モジュール構成

- `tests/quality_gates.rs`: テストエントリポイント（`#[path = "quality_gates/mod.rs"] mod quality_gates;`）
- `tests/quality_gates/mod.rs`: 配下テストモジュール宣言とワークスペースルート解決ヘルパー
- `tests/quality_gates/plugin_manifests.rs`: `plugins/export/*/plugin.json` と `plugins/ai/*/ai-plugin.json` の全数読み込み・ID 重複・`plugins_src/` バイナリ名一致検証
- `tests/quality_gates/localization_assets.rs`: `lang/*.json` のパース、非空値、プレースホルダ `{...}` 集合一致検証
- `tests/quality_gates/repository_rules.rs`: `[lints] workspace = true` 継承、`allow(unsafe_code)` の `backends/` 隔離、Antigravity ルール展開バイト上限（24,000 bytes）、追跡テキストファイル内のシークレット誤混入パターン検証
- `tests/quality_gates/project_compat.rs`: `schema_version = 1` の `.smc.json` ゴールデンフィクスチャ後方互換デシリアライズ＆再シリアライズ往復検証

---

## 5. エラーハンドリング・エッジケース

- **`.worktrees/` がテスト走査に混入する問題**:
  - リポジトリルートの `tests/quality_gates/` から `src/`, `plugins/`, `plugins_src/`, `lang/` を走査する際は、`CARGO_MANIFEST_DIR`（＝現在の worktree またはルートのルートディレクトリ）を起点に明示的サブディレクトリのみを走査し、`.worktrees/` や `target/` を決して辿らない設計とする。
- **文字コード・改行差**:
  - ファイル読込時は `fs::read_to_string` を使い、バイナリファイル誤読を避けるため拡張子対象を明示（`.rs`, `.toml`, `.json`, `.md`, `.yml`, `.yaml`, `.ps1`, `.sh`）する。
- **Antigravity ルール展開サイズ計測**:
  - `AGENTS.md` と `.agents/rules/general.md` 内の `@[label](path)` 参照を再帰展開した実バイト数が `24,000` bytes 以下であることを検証する。

---

## 6. テスト計画・受け入れ基準

- **自動テスト**:
  - `tests/quality_gates/` の各結合テストがすべて通過すること。
  - `scripts/pre-pr.ps1` がローカル（Windows）で終了コード 0 で全工程を通過すること。
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`（`undocumented_unsafe_blocks = "deny"` 含む）が警告ゼロで通過すること。
  - `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` が警告ゼロで通過すること。
- **完了条件（Definition of Done）**:
  - Phase 1〜4 を worktree ブランチ `feat/pre-pr-quality-gates` 内で細切れコミットし、上記自動テストとスクリプトが全通過していること。
