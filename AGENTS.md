# Repository Instructions (Multi-Agent Entrypoint)

このリポジトリの全体方針・開発ワークフロー・品質基準・ブランチ / `git worktree` 運用ルールは、`.github/` 配下のインストラクションを正本として適用・遵守すること（二重管理を避けるため共通ルール本文は以下へ集約し、末尾には Antigravity 固有のコンテキスト制御上書きのみを記載しています）。

- Antigravity 環境では、本ファイルの `@[...]` 展開および `.agents/rules/general.md` により下記 2 ファイルの内容が自動読み込みされます。
- 未展開の環境・エージェントで起動した場合は、調査や実装の前に必ず `.github/copilot-instructions.md` と `.github/instructions/general.instructions.md` を読み込んでから作業を開始すること。

@[copilot-instructions](.github/copilot-instructions.md)

## コンテキスト制御（高優先度、組み込みの並列先読み戦略を上書き）
- 未特定ファイルの推測一括読込を禁止：対象ファイルが未確定の段階では、まず `rg -l` / `rg -n`（または PowerShell の `Select-String`）で候補ファイルと行番号を絞り込み、1 ターンの並列読み込みで不確定なファイルは `≤ 3` 個に抑える。特定後は直接の呼び出しチェーンと変更対象のみを読むこと。
- 大きなファイルは対象範囲のみ読込：200 行を超える関連外・部分参照ファイルは `StartLine` / `EndLine` なしに全体を読まず、`rg -n` や `Select-String` で行番号を特定してから必要範囲を読む。一方、編集対象として特定済みのファイルは、細切れ分割で往復ターン数を増やさないよう `read_file` 上限内で必要な連続範囲をまとめて読んでよい。
- コマンド出力の制限：`run_command` で検索・一覧・差分（`rg` / `Select-String` / `Get-ChildItem` / `git diff` 等）を実行するときは、デフォルトで出力行数を制限する（PowerShell は `| Select-Object -First 60`、Unix シェルは `| head -n 60`、あるいは `--stat` / `-l` でファイル名のみ取得）。10KB の切り詰めに当たったり、バックグラウンドログの再読み込みを引き起こしたりしないようにする。
