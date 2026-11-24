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
| - | 全体基本設計書（アーキテクチャ・AI ネイティブ設計） | Implemented | [`completed/draft.md`](completed/draft.md) |
| - | 初期実装設計書：Phase A（基盤・評価・再生・GPUI 骨格） | Implemented | [`completed/initial-implementation/PhaseA.md`](completed/initial-implementation/PhaseA.md) |
| - | 初期実装設計書：Phase B（編集 UI・GPU 合成・書き出しプラグイン） | Implemented | [`completed/initial-implementation/PhaseB.md`](completed/initial-implementation/PhaseB.md) |
| - | 初期実装設計書：Phase C（AI チャット/プラグイン・VST3/CLAP・拡張） | Implemented | [`completed/initial-implementation/PhaseC.md`](completed/initial-implementation/PhaseC.md) |
| 001 | ゆっくり実況向け専用モジュール（ドメイン共通基盤設計） | Draft | [`001-yukkuri-live-module.md`](001-yukkuri-live-module.md) |
| 002 | AviUtl 準拠 UI 大幅修正設計書（全体概要・レイアウト設計） | Implemented | [`completed/002-aviutl-ui-redesign/README.md`](completed/002-aviutl-ui-redesign/README.md) |
| 002-01 | ↳ データモデル・編集コマンド・コントローラ拡張仕様 | Implemented | [`completed/002-aviutl-ui-redesign/01-data-model-and-commands.md`](completed/002-aviutl-ui-redesign/01-data-model-and-commands.md) |
| 002-02 | ↳ 拡張編集タイムライン UI/UX 詳細仕様 | Implemented | [`completed/002-aviutl-ui-redesign/02-timeline-spec.md`](completed/002-aviutl-ui-redesign/02-timeline-spec.md) |
| 002-03 | ↳ 設定ダイアログ（Clip インスペクタ）UI/UX 詳細仕様 | Implemented | [`completed/002-aviutl-ui-redesign/03-inspector-dialog-spec.md`](completed/002-aviutl-ui-redesign/03-inspector-dialog-spec.md) |
| 002-04 | ↳ プレビュー直接操作・メニューバー・ショートカット詳細仕様 | Implemented | [`completed/002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md`](completed/002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md) |
| 002-05 | ↳ AviUtl 準拠 UI 段階的実装計画（Phase D-1 〜 D-6） | Implemented | [`completed/002-aviutl-ui-redesign/05-implementation-plan.md`](completed/002-aviutl-ui-redesign/05-implementation-plan.md) |
| 003 | タイムラインクリップ表示・再生FPS・音声再生・プレビュー画質の抜本改修 | Implemented | [`completed/003-playback-audio-quality-and-clip-tile-fix.md`](completed/003-playback-audio-quality-and-clip-tile-fix.md) |
| - | 市場調査・開発戦略レポート | Reference | [`loadmap/Market Research.md`](loadmap/Market%20Research.md) |
| - | 機能モジュール・プラグイン開発ロードマップ（要件・優先順位） | Draft | [`loadmap/Plugin Loadmap.md`](loadmap/Plugin%20Loadmap.md) |
| - | 自律エージェントプラグイン 将来拡張メモ | Memo | [`loadmap/Agent-Future-Features.md`](loadmap/Agent-Future-Features.md) |
| 004 | Phase P0 設計書：本体前提基盤機能モジュール（キャラプリセット・連続字幕入力・APIキーガイド・プラグイン統合窓口） | Draft | [`004-phase-p0-core-prerequisites.md`](004-phase-p0-core-prerequisites.md) |
| 005 | Phase P1 設計書：音声作成機能モジュール＆外部 TTS プラグイン（TTS 連携・尺自動伸縮・リップル・間・キャッシュ） | Draft | [`005-phase-p1-tts-voice-plugin.md`](005-phase-p1-tts-voice-plugin.md) |
| 006 | Phase P2 設計書：感情連動立ち絵・定番演出自動配置（機能モジュール） | Draft | [`006-phase-p2-tachie-ai-effects-plugin.md`](006-phase-p2-tachie-ai-effects-plugin.md) |
| 007 | Phase P3 設計書：無音カット（機能モジュール：録画下準備用・完全ローカル音量判定） | Draft | [`007-phase-p3-silence-cut-plugin.md`](007-phase-p3-silence-cut-plugin.md) |
| 008 | Phase P4 設計書：拡張入力（ローカル ASR プラグイン ／ 外部台本インポート機能モジュール） | Draft | [`008-phase-p4-extended-input-plugins.md`](008-phase-p4-extended-input-plugins.md) |
| 009 | Phase P5 設計書：他ソフト向けプロジェクト書き出しプラグイン（YMM4 → AviUtl サブ機能） | Draft | [`009-phase-p5-external-project-export-plugins.md`](009-phase-p5-external-project-export-plugins.md) |
| 010 | PR 前コード品質ゲート・自動テスト基盤 | Implemented | [`completed/010-pre-pr-quality-gates.md`](completed/010-pre-pr-quality-gates.md) |
| 011 | 外部プラグイン向け SDK・プロトコル互換性ポリシー | Draft | [`011-plugin-protocol-compatibility-policy.md`](011-plugin-protocol-compatibility-policy.md) |
| 012 | 自律エージェントプラグイン基盤＆構成コンパイラ設計書 | Implemented | [`completed/012-agentic-plugin-architecture.md`](completed/012-agentic-plugin-architecture.md) |
| 013 | AI チャット UI 改修・VS Code 風設定画面・起動時状態復元・モデル管理・トークン残量表示設計書 | Implemented | [`completed/013-ai-ui-settings-and-usage.md`](completed/013-ai-ui-settings-and-usage.md) |
| 014 | AI チャット継続性修正・作業中／完了状態の可視化・選択＆コピー対応設計書 | Implemented | [`completed/014-ai-chat-reliability-status-and-copy.md`](completed/014-ai-chat-reliability-status-and-copy.md) |
| 017 | エディタ UX 改善設計書（タイムライン再生位置追随・プレビュー画面外カラー＆境界・JSON 設定ハイライト） | Implemented | [`completed/017-editor-ux-timeline-follow-preview-backdrop-json-hl.md`](completed/017-editor-ux-timeline-follow-preview-backdrop-json-hl.md) |
