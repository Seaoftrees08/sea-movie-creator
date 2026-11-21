# Repository Instructions (Multi-Agent Entrypoint)

このリポジトリの全体方針・開発ワークフロー・品質基準・ブランチ / `git worktree` 運用ルールは、`.github/` 配下のインストラクションを正本として適用・遵守すること（二重管理を避けるため本文は以下へ集約しています）。

- Antigravity 環境では、本ファイルの `@[...]` 展開および `.agents/rules/general.md` により下記 2 ファイルの内容が自動読み込みされます。
- 未展開の環境・エージェントで起動した場合は、調査や実装の前に必ず `.github/copilot-instructions.md` と `.github/instructions/general.instructions.md` を読み込んでから作業を開始すること。

@[copilot-instructions](.github/copilot-instructions.md)

## コンテキスト制御（高優先度、組み込みの並列先読み戦略を上書き）
- 狭い検索と精読、片っ端からの先読みは禁止：コードを特定するには、まず rg -l / rg -n で具体的なファイルと行番号を絞り込み、その後は直接の呼び出しチェーンか変更対象のファイルだけを読むこと。推測に基づく read_file の一括並列読み込みは禁止（1ターンの並列読み込みで不確定なファイルは ≤ 3個）。
- 大きなファイルは分割して読む：200行を超えるファイルは、StartLine/EndLine を指定せずにファイル全体を読むことを禁止。まず rg -n で行番号を特定し、必要に応じて対象範囲を読む（≤ 200行/回）。
- コマンド出力の制限：run_command で rg / find / git diff / ls を実行するときは、デフォルトで | head -n 60 または -l を付ける。10KB の切り詰めに当たったり、バックグラウンドログの再読み込みを引き起こしたりしないようにする。
