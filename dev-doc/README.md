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
| - | 要件メモ（初期実装の要件定義） | Implemented | [`completed/initial-implementation/draft.md`](completed/initial-implementation/draft.md) |
| - | 全体基本設計書（アーキテクチャ・AI ネイティブ設計） | Draft | [`draft.md`](draft.md) |
| - | 初期実装設計書：Phase A（基盤・評価・再生・GPUI 骨格） | Implemented | [`completed/initial-implementation/PhaseA.md`](completed/initial-implementation/PhaseA.md) |
| - | 初期実装設計書：Phase B（編集 UI・GPU 合成・書き出しプラグイン） | Implemented | [`completed/initial-implementation/PhaseB.md`](completed/initial-implementation/PhaseB.md) |
| - | 初期実装設計書：Phase C（AI チャット/プラグイン・VST3/CLAP・拡張） | Implemented | [`completed/initial-implementation/PhaseC.md`](completed/initial-implementation/PhaseC.md) |
| 001 | ゆっくり実況向け専用モジュール（初期設計完了後・将来機能） | Draft | [`001-yukkuri-live-module.md`](001-yukkuri-live-module.md) |
| 002 | AviUtl 準拠 UI 大幅修正設計書（全体概要・レイアウト設計） | Draft | [`002-aviutl-ui-redesign/README.md`](002-aviutl-ui-redesign/README.md) |
| 002-01 | ↳ データモデル・編集コマンド・コントローラ拡張仕様 | Draft | [`002-aviutl-ui-redesign/01-data-model-and-commands.md`](002-aviutl-ui-redesign/01-data-model-and-commands.md) |
| 002-02 | ↳ 拡張編集タイムライン UI/UX 詳細仕様 | Draft | [`002-aviutl-ui-redesign/02-timeline-spec.md`](002-aviutl-ui-redesign/02-timeline-spec.md) |
| 002-03 | ↳ 設定ダイアログ（Clip インスペクタ）UI/UX 詳細仕様 | Draft | [`002-aviutl-ui-redesign/03-inspector-dialog-spec.md`](002-aviutl-ui-redesign/03-inspector-dialog-spec.md) |
| 002-04 | ↳ プレビュー直接操作・メニューバー・ショートカット詳細仕様 | Draft | [`002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md`](002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md) |
| 002-05 | ↳ AviUtl 準拠 UI 段階的実装計画（Phase D-1 〜 D-6） | Draft | [`002-aviutl-ui-redesign/05-implementation-plan.md`](002-aviutl-ui-redesign/05-implementation-plan.md) |
