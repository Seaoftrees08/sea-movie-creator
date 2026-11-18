## 変更概要

- 変更した内容と目的（関連する `dev-doc/` があればリンクを記載）:
  - 

## PR 前品質チェック（ローカル実行）

- [ ] `powershell -ExecutionPolicy Bypass -File scripts/pre-pr.ps1`（または `./scripts/pre-pr.sh`）を実行し、全項目を通過した
  - [ ] `cargo fmt --all --check`
  - [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
  - [ ] `cargo build --workspace --all-targets`
  - [ ] `cargo test --workspace --all-targets --all-features`（`tests/quality_gates.rs` 含む）
- [ ] `unsafe` を追加・変更した場合、`backends/` 配下（または隔離クレート）に閉じ込め、`// SAFETY:` コメントを記載した
- [ ] 機能や仕様・UI が変わる場合、対応する `dev-doc/` および `docs/` を更新した
- [ ] 「プラグイン（無くても本体起動可能）」と「機能モジュール（本体依存）」の用語を混同していない

## 実機・手動確認（該当する場合のみ）

- [ ] `#[ignore]` 対象の実機テスト（GPU / OS HW エンコーダ / 音声デバイス / 外部 CLI）や画面起動確認を実施した（未該当時・CI のみ時は空欄で可）
  - 確認環境・結果:
