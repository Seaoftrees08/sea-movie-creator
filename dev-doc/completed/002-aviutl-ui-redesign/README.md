# [002] AviUtl 準拠 UI 大幅修正設計書（全体概要・レイアウト設計）

- **ステータス**: Implemented
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **実装範囲**: Phase D-1 〜 D-6 完了
- **関連ドキュメント**:
  - 全体基本設計書: [`../draft.md`](../draft.md)
  - 初期実装要件メモ: [`../completed/initial-implementation/draft.md`](../completed/initial-implementation/draft.md)
  - 初期実装設計書: [`../completed/initial-implementation/PhaseA.md`](../completed/initial-implementation/PhaseA.md), [`../completed/initial-implementation/PhaseB.md`](../completed/initial-implementation/PhaseB.md), [`../completed/initial-implementation/PhaseC.md`](../completed/initial-implementation/PhaseC.md)
  - 将来機能（ゆっくり実況モジュール）: [`../001-yukkuri-live-module.md`](../001-yukkuri-live-module.md)
  - 開発・テスト規約: [`../../.github/copilot-instructions.md`](../../.github/copilot-instructions.md), [`../../.github/instructions/general.instructions.md`](../../.github/instructions/general.instructions.md)

---

## 1. 概要・目的

### 1.1 背景と解決したい課題

`sea-movie-creator` は [`../completed/initial-implementation/`](../completed/initial-implementation/draft.md) に基づき、フレーム境界時間モデル（[`FrameIndex`](../../src/core/time.rs) / [`PlaybackRate`](../../src/core/time.rs)）、統一編集コマンド（[`EditCommand`](../../src/timeline/command.rs)）、アニメーション評価器（[`AnimatedParameter`](../../src/core/animation.rs)）、`wgpu` コンポジタ（[`WgpuCompositor`](../../src/media/compositor/mod.rs)）、音声ミキサ＆波形キャッシュ（[`AudioMixer`](../../src/audio/mixer.rs) / [`WaveformCache`](../../src/audio/waveform.rs)）、AI チャット＆MCP を実装済みである。

一方で、初期実装では UI 固有の操作設計書を作成せずに最小骨格（[`src/ui/`](../../src/ui/mod.rs)）を実装したため、要件メモ（[`../completed/initial-implementation/draft.md`](../completed/initial-implementation/draft.md) §1）が掲げた **「操作感は AviUtl に近く、見た目は DaVinci Resolve / VS Code 風」** という体験と大きく乖離している。

| 領域 | 現行実装（[`src/ui/`](../../src/ui/mod.rs)）の課題 | 本設計書（`002`）での刷新方針 |
| --- | --- | --- |
| **タイムライン**<br>[`timeline_panel.rs`](../../src/ui/timeline_panel.rs) | ・上部ルーラーが空白黒帯＋2px 線のみで時間・フレーム表示なし<br>・ルーラークリックで `timeline_scroll_x_frames` 未加算かつドラッグスクラブ不可<br>・タイムライン全長（最終フレーム）の調整・表示やズームバー/`Ctrl+ホイール` がない<br>・行高 44px と過大で右クリックメニュー・複数選択・中間点・波形描画がない | ・**時刻/フレーム目盛り付きルーラー**（クリック＆ドラッグで即時シーク）<br>・**タイムライン長変更**（自動追従 / 最後のオブジェクト合わせ / 最終フレーム線ドラッグ / 数値指定 / `[` `]` 選択範囲）<br>・**左上青いズームバー**・`Ctrl+ホイール`・横/縦スクロールバー<br>・22px 高密度レイヤー行、右クリック追加/編集、中間点 `P`、波形描画、複数選択・`Ctrl+ドラッグ` 複製 |
| **プロパティ操作**<br>[`inspector_panel.rs`](../../src/ui/inspector_panel.rs) | ・16 スロットから 1 項目ずつボタン切替し `+step`/`-step` のみで編集<br>・`Opacity` 以外のスライダー・数値直接入力欄がない<br>・AviUtl「設定ダイアログ」の左右 2 値トラックバーや中間点操作がない | ・**`Clip` タブ内 AviUtl「設定ダイアログ」完全再現**<br>・`X / Y / 拡大率 / 透明度 / 回転` 等を同時一覧表示<br>・**左（開始値）— 中央ボタン（移動モード）— 右（終了値）** のトラックバー＆直接数値入力<br>・中間点バー（`◀ [====] ▶ [+]`）、折りたたみ式フィルタブロック、`f(t)` 数式統合 |
| **プレビュー**<br>[`preview_panel.rs`](../../src/ui/preview_panel.rs) | ・映像表示と汎用シークスライダのみで直接ドラッグ操作不可<br>・1 フレーム送り・主音声波形帯・バウンディング枠表示がない | ・**キャンバス直接ドラッグ**（移動、`Alt+ドラッグ` 拡大、回転ハンドル/`*`/`/`）<br>・選択オブジェクトの枠＋中心アンカー表示、主音声波形帯、タイムコード＆`1f` 送り |

### 1.2 本設計書の目標

1. **AviUtl（拡張編集）熟練者が違和感なく手を動かせる操作体系**（タイムライン・設定ダイアログ・プレビュー直接操作・ショートカット）を詳細に定義する。
2. **1 ウィンドウ原案（[`../draft.md`](../draft.md) §2.1）を維持**し、右 1/5 リサイズ可能タブパネル内で「設定ダイアログ」と「AI チャット」を快適に両立させる。
3. **AI-First アーキテクチャを完全維持**し、手動 UI 操作がすべて [`EditCommand`](../../src/timeline/command.rs) を通ることで AI チャット操作と対称・Undo/Redo 可能・テスト可能であることを保証する。

---

## 2. ドキュメント構成と索引

| 番号 | ファイル | 内容 |
| --- | --- | --- |
| **00** | [`README.md`](README.md)（本書） | 全体概要、1 ウィンドウ構成、テーマ・配色規約、AviUtl 用語対照表 |
| **01** | [`01-data-model-and-commands.md`](01-data-model-and-commands.md) | タイムライン長・選択範囲・中間点同期・複数選択・新規追加に伴うデータモデル＆コマンド拡張仕様 |
| **02** | [`02-timeline-spec.md`](02-timeline-spec.md) | タイムライン（拡張編集）詳細 UI/UX 仕様（ルーラー・ズーム・長さ変更・右クリック・レイヤー・中間点・波形） |
| **03** | [`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md) | 右パネル「Clip 設定ダイアログ」詳細仕様（左右トラックバー・移動モード・中間点バー・折りたたみブロック・`f(t)`） |
| **04** | [`04-preview-and-shortcuts-spec.md`](04-preview-and-shortcuts-spec.md) | プレビュー直接操作・主音声波形帯・メニューバー拡張・キーボードショートカット＆フォーカス制御仕様 |
| **05** | [`05-implementation-plan.md`](05-implementation-plan.md) | Phase D-1 〜 D-6 段階的実装計画、ファイル分割、テスト配置・受け入れ基準 |

---

## 3. スコープ

### 対象（In Scope）
- `sea-movie-creator` のシングルウィンドウ UI（[`src/ui/`](../../src/ui/mod.rs)）の全面再設計仕様。
- AviUtl（拡張編集）に酷似したタイムライン操作、設定ダイアログ操作、プレビュー直接操作、メニューバー、ショートカット体系。
- 上記 UI 操作を支えるための [`ProjectDocument`](../../src/timeline/document.rs)、[`EditCommand`](../../src/timeline/command.rs)、[`AppController`](../../src/ui/app_controller.rs) への最小限の拡張仕様。
- Phase D 実装計画および単体/結合テスト要件。

### 対象外（Out of Scope）
- AviUtl 独自の 32bit Win32 DLL プラグイン（`.auf` / `.aui` / `.auo` / `.anm`）のバイナリ読み込み（[`../draft.md`](../draft.md) §7.1 により見送り済み）。
- AviUtl のマルチウィンドウ分離（ウィンドウを OS 上で 3〜4 枚バラバラに浮かべる構成）。本アプリは [`../draft.md`](../draft.md) §2.1 に準拠した **1 ウィンドウ＋ペイン分割** を採用する。
- ゆっくり実況専用モジュール（[`../001-yukkuri-live-module.md`](../001-yukkuri-live-module.md)）自体の詳細実装（本設計のレイヤー・クリップ UI 上へ将来統合可能な設計余地のみ考慮する）。

---

## 4. 1 ウィンドウ画面構成（`draft.md` 原案維持＋AviUtl 操作統合）

[`../draft.md`](../draft.md) §2.1 の 4 領域構成を維持したまま、各領域を AviUtl の対応ウィンドウ機能へ高密度にマッピングする。

```text
+------------------------------------------------------------------------+------------------------------------+
| TitleBar + AppMenuBar: [ファイル] [編集] [タイムライン] [設定] [表示] [再生] [書き出し]             |
+------------------------------------------------------------------------+------------------------------------+
| プレビューヘッダ: 1920x1080 60fps | [00:00:02.86] [173/1852] | [Fit▼]  | [Clip][AI Chat][Layers][Project]   |
|                                                                        |------------------------------------|
|                                                                        | 設定ダイアログ (Clipタブ)          |
|                  中央プレビューキャンバス (wgpu 合成結果)               | 動画/画像/テキスト[標準描画]       |
|          [バウンディング枠 / アンカー十字 / グリッドオーバーレイ]        | ◀ [====中間点バー====] ▶ [+] [-]  |
|          直接左ドラッグ: X,Y移動  |  Alt+ドラッグ: 拡大率変更           | ▼ 標準描画                         |
|                                                                        |  0.0 <==[ X ]==> 0.0               |
|------------------------------------------------------------------------|  0.0 <==[ Y ]==> 0.0               |
| 主音声波形ストリップ（ミックス済み緑色波形 / 表示トグル可）              |  100 <==[拡大]==> 100              |
|------------------------------------------------------------------------|  0.0 <==[透明]==> 0.0              |
| [|<][◀][▶/⏸][■][▶|][>|]  [==== シークバー ====]  [S:分割]             |  0.0 <==[回転]==> 0.0              |
+------------------------------------------------------------------------+  合成モード: [通常 ▼]              |
| 拡張編集タイムライン [00:00:02.86] [173/1852]                           | ▼ 色調補正 / 字幕 / 音声           |
| [■ズームバー■] | 00:00:00.00 | 00:00:02.00 | 00:00:04.00 | ... | 暗色外 | (パネル幅で1段/2段自動切替)       |
| Layer 1        | [======== 音声クリップ (赤・波形表示) =========]  |     |------------------------------------|
| Layer 2        | [== 動画クリップ (青・|中間点|区切り) ==]        |     | (境界ドラッグで幅変更:             |
| Layer 3        |       [== 字幕クリップ (黄橙) ==]               |     |  初期1/5, 最小240px, 推奨300~440px)|
| Layer 4        |                                                 |     |                                    |
+------------------------------------------------------------------------+------------------------------------+
```

### 4.1 レイアウト分割とリサイズ仕様（[`window.rs`](../../src/ui/window.rs)）

- **右パネル（`h_resizable`）**:
  - 初期幅：ウィンドウ幅の `1/5`（例：`1280px` 時は `256px`、[`clamp_right_panel_width`](../../src/ui/app_controller.rs#L77) を継承。推奨操作幅への拡張上限はウィンドウ幅の `50%`）。
  - 最小幅 `240px` でも「設定ダイアログ」の左右トラックバーが操作可能な **2 段コンパクトレイアウト** を採用し、`380px` 以上に広げると AviUtl と同等の **1 行横並びレイアウト** へ自動移行する（[`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md) 参照）。
  - タブ構成：
    1. **`Clip`（設定）**: 選択クリップの AviUtl 風「設定ダイアログ」。
    2. **`AI Chat`**: VS Code 風 AI チャット（[`ai_chat_panel.rs`](../../src/ui/ai_chat_panel.rs)）。
    3. **`Layers`**: 全レイヤーの表示/非表示・ミュート・ロック・クリップ数管理（タイムライン左ヘッダ操作とも双方向同期）。
    4. **`Project`**: 解像度・FPS・タイムライン長設定・同期クロック・素材リンク状態（再リンク操作入口）。
- **左側上下分割（`v_resizable`）**:
  - 上部：プレビューパネル（`Preview` + 主音声波形 + トランスポート）。
  - 下部：タイムラインパネル（既定比率 `0.35` 程度、`160px..75%` で自由に伸縮可）。

### 4.2 GPUI 実装基盤との対応

本プロジェクトは `gpui-kit = "0.9.1"`（`gpui-pre` + `gpui_component 0.9.1`）を採用している。各 UI 部品は以下の標準モジュールへマッピングする。

| UI 要素 | 採用コンポーネント / GPUI プリミティブ | 備考 |
| --- | --- | --- |
| メニューバー | `gpui_component::menu::AppMenuBar` + `TitleBar` | [`menu_bar.rs`](../../src/ui/menu_bar.rs) 拡張 |
| 右パネルタブ・分割 | `gpui_component::tab::TabBar`, `h_resizable`, `v_resizable` | [`window.rs`](../../src/ui/window.rs) 継承 |
| コンテキストメニュー | `gpui_component::menu::ContextMenuExt`, `PopupMenu`, `PopupMenuItem` | タイムライン空白/クリップ/ルーラー/レイヤー右クリック・移動モードメニュー |
| 数値直接入力・テキスト | `gpui_component::input::Input`, `InputState`, `NumberInput` | 設定ダイアログ数値・字幕・`f(t)`・長さ変更モーダル |
| 折りたたみ・チェック・色 | `gpui_component::collapsible`, `checkbox`, `color_picker`, `select` | フィルタブロック、合成モード、字幕色 |
| ルーラー・波形・ギズモ | GPUI `div` + `canvas(...)` / `paint_quad` / `paint_path` + マウスイベント | 高密度タイムスケール・音声波形・選択枠描画 |
| 仮想スクロール | `gpui_component::v_virtual_list` + `VirtualListScrollHandle` | 100 段以上対応 |

---

## 5. ビジュアル・テーマ＆配色設計（DaVinci / VS Code × AviUtl 視認性）

全体のベース色は `gpui_component::ThemeMode::Dark`（VS Code / DaVinci Resolve 風低彩度ダークグレー）を維持しつつ、クリップや再生ヘッドの視認性は AviUtl（dark mode / `patch.aul` 相当）の色分け規約を採用する。

| 対象 | 色トークン（目安 RGBA） | 意味・AviUtl との対応 |
| --- | --- | --- |
| **ウィンドウ / パネル背景** | `#181A1F` / `#1E2128` | DaVinci / VS Code 基調ダーク背景 |
| **境界線・グリッド線** | `#2E333E` | ペイン境界・タイムライングリッド |
| **再生ヘッド（現在フレーム）** | `#EF4444` または `#38BDF8`（視認性設定可、既定 `#38BDF8` + 上端赤/シアン逆三角） | AviUtl 赤縦線 / ダークテーマシアン線 |
| **タイムライン最終フレーム以降** | `#0F1115`（暗色マスク）＋境界線 `#64748B` | AviUtl 灰色の最終フレーム線以降の無効領域 |
| **選択範囲（`[` 〜 `]`）** | `#2563EB33`（半透明青帯）＋境界 `#3B82F6` | AviUtl 青い選択範囲 |
| **映像クリップ** | `#1D4ED8`（青系） | AviUtl メディアオブジェクト（青） |
| **画像クリップ** | `#2563EB`（青〜インディゴ系） | AviUtl 画像オブジェクト（青） |
| **音声クリップ** | `#B91C1C`（赤系）＋明緑/白波形 `#4ADE80` | AviUtl 音声オブジェクト（赤） |
| **字幕テキストクリップ** | `#D97706`（黄橙系、画像/動画と即座に区別） | `sea-movie-creator` 視認性最適化 |
| **制御/フィルタ枠（将来）** | `#0D9488`（青緑系・予約色） | AviUtl カメラ/グループ制御（緑青） |
| **リンク切れ（Missing Media）** | `#7F1D1D`＋斜線/警告枠 `#F87171` | [`AssetStatus::Missing`](../../src/timeline/document.rs#L188) 表示 |
| **選択中クリップ枠・区間** | 外枠 `#FACC15`（2px）＋非選択区間減光 | 選択オブジェクト＋中間点選択区間の明示 |
| **左上青いズームバー / 中間点バー** | `#2563EB` / `#3B82F6` | AviUtl の青いズーム＆中間点進捗バー |

---

## 6. AviUtl 用語 ⇔ `sea-movie-creator` 内部モデル対照表

設計書を読む開発者および AI エージェントが概念を混同しないよう、用語対応を統一する。

| AviUtl（拡張編集）用語 | `sea-movie-creator` 内部概念 | 補足・注意点 |
| --- | --- | --- |
| オブジェクト | [`Clip`](../../src/timeline/document.rs#L346) | `Video` / `Image` / `Audio` / `Subtitle` |
| レイヤー（Layer 1 が最背面、下が前面） | [`Layer`](../../src/timeline/document.rs#L414)（`layers[0]` が最背面、末尾が前面） | **重ね順は両者完全一致**（UI 表示も上から `Layer 1, Layer 2, ...`） |
| 現在フレーム（赤い縦線） | `AppController.clock.current_frame()` ([`FrameIndex`](../../src/core/time.rs#L159)) | 整数フレーム `>= 0` |
| 最終フレーム（灰色の縦線） | `ProjectDocument.timeline_length` / `total_duration_frames()` | 今回新規導入（自動/手動切替、空時シーク上限は `seek_upper_bound_frames()`） |
| 中間点（`P` キー） | [`Clip`](../../src/timeline/document.rs#L346)`.midpoints` ＋ 1:1 整列 `Keyframes` | `Constant` 状態でも配置可・区間ごとに値編集／移動方法は全区間共通（強制整列） |
| トラックバー移動方法 | `AnimatedParameter`（`Constant` / `Easing` / `Keyframes` / `Expression`） | 中間点あり時は区間 `Keyframe.easing` に対応 |
| 設定ダイアログ | 右パネル `Clip` タブ ([`inspector_panel.rs`](../../src/ui/inspector_panel.rs)) | 1 ウィンドウ右ペイン内に常時ドッキング |
| 拡大率（`%` 表示） | `VisualProperties.scale_x`, `scale_y`（倍率 `1.0` = `100.0%`） | UI 表示時のみ `× 100` 換算 |
| 透明度（`0.0`〜`100.0`） | `VisualProperties.opacity`（`0.0` 透明〜`1.0` 不透明） | UI 表示時 `transparency = (1.0 - opacity) * 100.0` |
| 中心 X / 中心 Y | `VisualProperties.anchor_x`, `anchor_y`（px 単位） | 要素中心 `(0, 0)` 基準のピクセルオフセット |
| スクリプト制御 / イージング数値 | `AnimatedParameter::Expression` (`t`, `p`, `d`, `pi`, `e`) ＋ `EasingCurve` | AI チャットが生成した数式と共通 |

---

## 7. テスト・品質方針（全設計書共通）

- 本設計に基づく実装コードは、すべて統一コマンド [`EditCommand`](../../src/timeline/command.rs) 経由でドキュメントを更新し、純粋関数・状態ロジックの単体テストを各 `.rs` 末尾 `#[cfg(test)] mod tests` へ、結合テストを `tests/<実装名>.rs` + `tests/<実装名>/mod.rs` へ配置する。
- GUI ウィンドウを実際に起動する確認は AI 自動連続実行を禁止し（[`../../.github/instructions/general.instructions.md`](../../.github/instructions/general.instructions.md) 準拠）、ヘッドレスで検証可能なコントローラ・計算ロジック・コマンド往復テストを自動化する。
