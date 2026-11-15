# dev-doc（機能設計書・技術ドキュメント）

このディレクトリは、`sea-movie-creator` における**機能ごとの設計書・技術仕様・設計上の意思決定**を蓄積するための開発者向けドキュメント領域です。
人間と AI アシスタントの両方が仕様の前提を正確に共有できるよう、実装前にここへ設計を残すことを推奨します。

## 運用ルール

1. **テンプレートを使う**
   - 新しい機能の設計書を作成する場合は、[`_template.md`](_template.md) をコピーして作成してください。
2. **ファイル命名規則**
   - 連番 + ケバブケースを推奨します。
   - 例: `001-project-architecture.md`, `002-video-timeline-model.md`, `003-export-pipeline.md`
3. **ステータス管理**
   - 各設計書の先頭にステータス（`Draft` / `In Review` / `Approved` / `Implemented` / `Deprecated`）を明記してください。
4. **実装との同期**
   - 実装中に仕様やインターフェースを変更した場合は、対応する設計書も合わせて更新してください。

## 設計書一覧

| ID | タイトル | ステータス | ファイル |
| --- | --- | --- | --- |
| - | 設計書テンプレート | Template | [`_template.md`](_template.md) |
| - | 要件メモ（初期実装の要件定義） | Draft | [`initial-implementation/draft.md`](initial-implementation/draft.md) |
| - | 全体基本設計書（アーキテクチャ・AI ネイティブ設計） | Draft | [`draft.md`](draft.md) |
| - | 初期実装設計書：Phase A（基盤・評価・再生・GPUI 骨格） | Draft | [`initial-implementation/PhaseA.md`](initial-implementation/PhaseA.md) |
| - | 初期実装設計書：Phase B（編集 UI・GPU 合成・書き出しプラグイン） | Draft | [`initial-implementation/PhaseB.md`](initial-implementation/PhaseB.md) |
| - | 初期実装設計書：Phase C（AI チャット/プラグイン・VST3/CLAP・拡張） | Draft | [`initial-implementation/PhaseC.md`](initial-implementation/PhaseC.md) |
| 001 | ゆっくり実況向け専用モジュール（初期設計完了後・将来機能） | Draft | [`001-yukkuri-live-module.md`](001-yukkuri-live-module.md) |
