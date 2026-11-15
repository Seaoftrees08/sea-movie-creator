# [002-04] プレビュー直接操作・メニューバー・ショートカット詳細仕様

- **ステータス**: Draft
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **関連ドキュメント**:
  - 概要設計書: [`README.md`](README.md)
  - データモデル＆コマンド仕様: [`01-data-model-and-commands.md`](01-data-model-and-commands.md)
  - タイムライン仕様: [`02-timeline-spec.md`](02-timeline-spec.md)
  - 設定ダイアログ仕様: [`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md)
  - 既存実装: [`src/ui/preview_panel.rs`](../../src/ui/preview_panel.rs), [`src/ui/menu_bar.rs`](../../src/ui/menu_bar.rs), [`src/ui/window.rs`](../../src/ui/window.rs), [`src/media/compositor/mod.rs`](../../src/media/compositor/mod.rs), [`src/audio/mixer.rs`](../../src/audio/mixer.rs)

---

## 1. 概要・目的

AviUtl ではメインウィンドウ上で映像・図形・テキストを **「直接左ドラッグで X/Y 移動」「Alt+ドラッグで拡大」「ハンドルやキーで回転」「緑の主音声波形を確認しながら `◀`/`▶` で 1 フレーム送り」** でき、これが直感的な画面構成作業を支えている。

一方、現行の [`src/ui/preview_panel.rs`](../../src/ui/preview_panel.rs) は合成画像の静的表示と汎用シークスライダ、`再生/停止`・`先頭へ`・`分割` ボタンのみであり、キャンバス上のオブジェクトを直接つまんで動かすことも、`1f` コマ送りや音声波形視認を行うこともできない。

本仕様では、AviUtl メインウィンドウ相当のプレビュー体験・上部メニューバー体系・完全ショートカットキー仕様およびテキスト入力時の競合防止機構を定義する。

---

## 2. スコープ

### 対象（In Scope）
- プレビューヘッダ（解像度/FPS・時刻/フレーム表示・表示倍率・ガイド/グリッド/波形トグル）。
- プレビューキャンバス上の直接操作（座標変換、前面優先ヒットテスト、移動・`Alt+拡大`・回転ハンドル、バウンディング枠＆中心アンカー十字オーバーレイ）。
- リンク切れバナー＋再リンク動線、主音声波形ストリップ、トランスポートバー（`1f` 送り/戻し・最終移動・分割）。
- [`AppMenuBar`](../../src/ui/menu_bar.rs) 拡張メニュー構造とキーボードショートカット／フォーカスガード仕様。

### 対象外（Out of Scope）
- GPU シェーダ合成パイプラインそのものの変更（既存の [`build_render_scene`](../../src/media/compositor/mod.rs) や [`compute_layer_quad_ndc`](../../src/media/compositor/mod.rs#L110) の数学基盤をそのまま利用）。

---

## 3. 要件・制約

- **機能要件**:
  - プレビューキャンバス上の非ロック可視オブジェクトをクリックで選択でき、そのままドラッグして `X / Y`（`Alt+ドラッグ` で拡大率）を変更できること。
  - プレビュー直下に緑色のマスター音声波形ストリップと `1f` 送り付きトランスポートバーを備えること。
  - `Input` / `NumberInput` / AI チャット欄 / 字幕編集欄へ文字入力中は、`Space`, `S`, `P`, `Delete`, `←/→`, `Ctrl+C/V` 等のエディタショートカットが文字入力を横取りしないこと。
- **性能・安定要件**:
  - ドラッグ操作中は毎フレームの評価値のみをプレビュー反映し、履歴スタック（`CommandDispatcher`）には `MouseUp` 時に `1` コマンド（`EditCommand::SetParameter` / `Batch`）のみ積むこと。

---

## 4. アーキテクチャ / 設計詳細

### 4.1 プレビュー領域全体構成（[`preview_panel.rs`](../../src/ui/preview_panel.rs)）

```text
+------------------------------------------------------------------------------+
| 1920x1080 60fps | [00:00:02.86] [173/1852] | [Fit ▼] [枠][#][波形]           | <-- 1. ヘッダ (28px)
+------------------------------------------------------------------------------+
|                  (黒背景レターボックス・余白クリックで選択解除)                |
|        +------------------------------------------------------------+        |
|        |                   回転ハンドル ○                           |        |
|        |                       │                                    |        |
|        |           ■───────────■───────────■                        |        |
|        |           │   画像 / 動画 / 字幕  │    (バウンディング枠   |        |
|        |           │          ⊕ (中心)     │     + 8箇所ハンドル)   |        |
|        |           ■───────────■───────────■                        |        |
|        +------------------------------------------------------------+        |
|                   [Media Offline 時: 赤色警告バナー + 再リンクボタン]          |
+------------------------------------------------------------------------------+
| ▓▓▓▒▓▓▓▓▓▒▒▓▓▓▓▓▓▓▒▒▓▓|▓▓▒▓▓▓▓▓▓▓▓▓▒▒▓▓▓▒▓▓▓  (主音声波形: ±3s窓 / 中央線)  | <-- 3. 波形帯 (34px)
+------------------------------------------------------------------------------+
| [ |< ] [ ◀-1f ] [ ▶/⏸ ] [ ■ ] [ ▶|+1f ] [ >| ]  [==== シークバー ====] [分割]| <-- 4. トランスポート
+------------------------------------------------------------------------------+
```

### 4.2 プレビューヘッダ仕様

- **左群**：`{width}x{height} {fps}fps`（クリックで `Project` タブへジャンプ）。
- **中央群**：`[00:00:02.86] [173/1852]`（現在時刻および `現在フレーム / 総フレーム`、クリックで `Timecode ⇔ Frames` 表示切替）。
- **右群**：
  - 表示倍率ドロップダウン（`PreviewZoomMode`）：`Fit`（ウィンドウ合わせ・既定） / `25%` / `50%` / `100%` / `200%`（拡大時にキャンバスからはみ出した部分はスクロール可能＋中ボタンドラッグパン）。
  - オーバーレイトグル 3 点（小アイコンボタン）：
    - `[枠]` `show_preview_guides`：選択クリップのバウンディングボックス・ハンドル・中心アンカー `⊕` 表示 ON/OFF。
    - `[#]` `show_preview_grid`：画面中央十字線＋10×10 均等グリッド線表示 ON/OFF。
    - `[波形]` `show_preview_waveform`：主音声波形ストリップ表示 ON/OFF。

---

### 4.3 キャンバス座標変換・ヒットテスト・直接操作仕様

#### A. スクリーン座標 ⇔ キャンバス座標変換

プレビュー領域内で実際に合成映像が描画される矩形を `image_rect = (img_x, img_y, img_w, img_h)`、プロジェクト基準解像度を `(proj_w, proj_h)` とする。

```rust
/// プレビュー画像矩形内における表示倍率（ウィンドウ実px / プロジェクト基準px）。
pub fn preview_display_scale(image_rect_width: f32, project_width: u32) -> f32 {
    image_rect_width / (project_width.max(1) as f32)
}

/// ウィンドウ座標のドラッグ移動量 (dx_win, dy_win) をプロジェクトピクセル単位 (dx_canvas, dy_canvas) に変換する。
pub fn screen_delta_to_canvas_delta(
    dx_win: f32,
    dy_win: f32,
    display_scale: f32,
) -> (f64, f64) {
    let s = display_scale.max(0.001) as f64;
    (dx_win as f64 / s, dy_win as f64 / s)
}
```

- **前面優先ヒットテスト（Point-in-Quad）**:
  - [`RenderLayerItem`](../../src/media/compositor/mod.rs#L77) 自体は `clip_id` を保持しないため、既存コンポジタ型・結合テストを一切壊さず、UI 向けに [`compute_layer_quad_ndc`](../../src/media/compositor/mod.rs#L110) と [`Matrix2D::transform_point`](../../src/media/visual_properties.rs#L43) を流用する軽量判定データ `PreviewLayerHitQuad { clip_id, layer_locked, ndc_vertices: [LayerVertex; 4], anchor_ndc, rotation_deg }` を再生フレーム単位で構築する。
  - 各レイヤーの `ndc_vertices` を `image_rect` スケールでウィンドウ座標へ射影し、**描画配列の末尾（最前面レイヤー）から逆順に** 四角形内判定（2 三角形の外積符号判定）を行う。
  - ロック済みレイヤー（`layer_locked == true`）に属するオブジェクトはヒットテストからスキップ（背面オブジェクト操作を妨げない）する。
  - バウンディングボックスがすでに描画されている選択中オブジェクトに対しては、まず **ハンドル判定（8 点拡縮ハンドル・上端回転ハンドル・中心アンカー `⊕` 各半径 `8px`）** を最優先でチェックする。

#### B. ドラッグ操作種別と更新先アニメーション規約

| 操作 | 対象 | 変更するパラメータ | 修飾キー補正 |
| --- | --- | --- | --- |
| クリップ本体を左ドラッグ | `X`, `Y`（移動） | `PositionX`, `PositionY` | `Shift`: 水平/垂直のみに軸固定<br>`Ctrl`: `5px` / `10px` 単位スナップ（または中央ガイド線スナップ） |
| **`Alt` + クリップ本体左ドラッグ**<br>または四隅ハンドルドラッグ | `拡大率` | `ScaleX` (+`ScaleY`) | 横移動/縦移動/中心からの距離比で `0%..1000%` 調整。中心点から離すと拡大・近づけると縮小 |
| 上端回転ハンドル ○ ドラッグ | `回転` | `RotationDeg` | 中心アンカー基準の `atan2` 角度差分。`Shift`: `15°` 刻みスナップ |
| `Ctrl + Alt` + 中心十字 `⊕` ドラッグ | `中心X/Y` | `AnchorX`, `AnchorY` | アンカー位置調整（ピクセル単位） |

**アニメーション中（`Easing` / `Keyframes`）オブジェクトをプレビューでドラッグした際の更新先ルール**:
マウスカーソルとオブジェクト描画位置の `1:1` 追従（および線形・非線形全カーブでの波形破綻防止）を保証するため、以下の確定ルールに従って書き換え先を決める。
1. **`Constant`**: 定数 `value` を直接更新する（`value += delta`）。
2. **`Keyframes`**：
   - 未整列（旧 JSON／AI 直後）のクリップを初回操作した場合は、同一コマンド先頭で `sync_and_align_clip_keyframes` を実行して `grid(clip) = [eff_start, ...visible_midpoints..., eff_last]` へ 1:1 整列する。
   - **境界／中間点フレーム一致時（`eff_playhead == k_i`）**：該当キーフレーム `keyframes[i]` の値のみを更新する（`value += delta`、カーソル `1:1` 追従）。
   - **区間途中時（`k_i < eff_playhead < k_{i+1}`）**：現在再生位置 `eff_playhead` が属する区間 `playhead_seg = i` を特定し（右パネル設定ダイアログも同区間 `selected_segment_index = i` へフォーカス切替）、**その区間の開始キー `keyframes[i]` と終了キー `keyframes[i + 1]` の両方に同じ `delta` を加算して区間全体を平行移動する**（補間カーブに関わらずカーソル移動量と表示位置変化が常に `1:1` 完全一致し、50% 境界付近での対象急変も起きない。片端のみ変えたいユーザー向けにツールチップ/ステータスへ「開始点・中間点・終了点へ移動または `P` で中間点追加すると単点編集可」とガイド表示）。
3. **`Easing`**：
   - **未カット状態**（`effect_offset_frames == 0` かつ `effect_duration_frames.is_none_or(|d| d == clip.duration.get())`）：`eff_playhead == eff_start` なら `start_value += delta`、`eff_playhead == eff_last` なら `end_value += delta`、区間途中 `(eff_start < eff_playhead < eff_last)` なら **`start_value += delta` と `end_value += delta` の両方を平行移動** する。
   - **カット済み状態**（`effect_offset_frames != 0` 等）：[`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md) §6 規約に従い、プレビュードラッグの初回コマンド実行時に確認なしで現在評価値ベースの `Keyframes([eff_start, ...visible..., eff_last])` へ自動昇格した上で、上記 2 と同じ規則で更新する。
4. **`Expression`**：数式による動的計算中のため直接ドラッグ変更は不可とし、キャンバス上に「`f(t)` 制御中のためプレビュー移動不可」ツールチップを一瞬表示する。

#### C. Media Offline 表示＋再リンク導線
- 既存の [`render_media_offline_overlay`](../../src/ui/preview_panel.rs) を拡張し、リンク切れクリップ（`has_media_offline_at_playhead == true`）が再生位置に存在する場合は、警告バナーに加えて **`[メディア再リンク...]`** ボタンを配置し、クリックで [`ProjectInfo`](../../src/ui/right_panel.rs) タブの再リンクまたは直接ファイル選択ダイアログを起動して `AssetId` パスを張り替えられるようにする。

---

### 4.4 主音声波形ストリップ＆トランスポートバー仕様

#### A. 主音声波形ストリップ（プレビュー直下・高さ `34px`）
- **キャッシュ済みデータ生成（`MasterWaveformCache`）**:
  - プレビュー再生中（`16ms` 描画フレーム毎）に UI スレッド上で重たい [`AudioMixer::mix_timeline_window`](../../src/audio/mixer.rs#L132) を直接全計算しないよう、**リビジョン管理＋チャンク LRU キャッシュ `MasterWaveformCache`** を設ける。
  - **解像度と単位**: ピーク列は `10ms` ビン（`100 bins/sec`、左右 `(min_l, max_l, min_r, max_r)` またはモノラル `(min, max)`）で保持し、バックグラウンド生成時は [`AudioMixer::mix_timeline_window`](../../src/audio/mixer.rs#L132) を軽量レート `target_sample_rate = 4_000 Hz`（1 秒あたり `40 samples/bin` で十分な包絡線ピーク抽出可能）で呼び出してピーク集約する。
  - **チャンク＆無効化**: `10秒 = 1,000 bin` を 1 チャンクとして最大 16 チャンク（160 秒分）をキャッシュし、キーは `(doc_audio_revision: u64, chunk_index: u64)` とする。タイムライン上で音声に影響する `EditCommand`（音声/動画クリップ追加・移動・削除・トリム・`PlaybackRate`・`AudioVolume`・`AudioPan`・ミュート・Undo/Redo）適用時のみ `doc_audio_revision` をインクリメントしてバックグラウンドワーカースレッド（古い `revision` の結果は破棄）へ再生成を依頼する。
  - **描画パス (`O(viewport_bars)`)**: 毎フレーム描画時は、キャッシュ済みチャンクから現在再生フレーム前後 **`±2.5 秒`（計 5 秒間 = 500 bins）** の範囲をビューポート横幅 `bars` 本へ間引くだけで即座に描画する（キャッシュ未生成チャンク到着時は `cx.notify()` で再描画し、未到着部分のみベースライン表示）。
- **描画**:
  - 黒背景 `#0B0D11` に鮮やかなグリーン（`#22C55E` / `#4ADE80`）の縦バー群を描画。画面写真と同一の視認性を確保する。
  - 中央の赤またはシアン `1px` 縦線が「現在再生フレーム位置」。
  - **波形ストリップ上を左ドラッグ** すると、そのままローカル音声スクラブ（ドラッグ量に応じた微細シーク）が可能。

#### B. トランスポートバー（最下部・高さ `36px`）
- 左側ボタン群：
  - `|<`：タイムライン先頭（`0f`）へ移動（`Home`）
  - `◀`：`-1` フレーム戻す（`Left`、ホールドまたは `Shift+クリック` で `-10f`）
  - `▶ / ⏸`：再生 / 一時停止トグル（`Space`）
  - `■`：停止して再生開始位置または先頭へ戻る
  - `▶|`：`+1` フレーム進める（`Right`、`Shift+クリック` で `+10f`）
  - `>|`：タイムライン最終フレーム（`total_duration_frames()`）へ移動（`End`）
- 中央：プロジェクト全体シークスライダ（左端 `0f` 〜 右端 `total_duration_frames()`、選択範囲 `[` `]` があれば青帯を併記）。
- 右側：`[S: 分割]` ボタン（再生ヘッド位置のクリップを分割）、現在タイムコード・フレーム確認小表示。

---

### 4.5 メニューバー構成拡張（[`src/ui/menu_bar.rs`](../../src/ui/menu_bar.rs)）

`gpui_component::menu::AppMenuBar` を利用し、既存のアクション群に加えて以下メニュー体系へ整理・拡充する。

| トップメニュー | 項目（アクション / ショートカット表示） |
| --- | --- |
| **ファイル** | `新規プロジェクト (Ctrl+N)` / `開く... (Ctrl+O)` / `保存 (Ctrl+S)` / `名前を付けて保存... (Ctrl+Shift+S)` / `メディアの読み込み... (Ctrl+I)` / `書き出し... (Ctrl+Shift+E)` / `終了` |
| **編集** | `元に戻す (Ctrl+Z)` / `やり直す (Ctrl+Y)` / `切り取り (Ctrl+X)` / `コピー (Ctrl+C)` / `貼り付け (Ctrl+V)` / `複製 (Ctrl+D)` / `削除 (Del)` / `分割 (S)` / `すべて選択 (Ctrl+A)` |
| **タイムライン** | `中間点を追加 (P)` / `現在位置を開始位置に ([)` / `現在位置を終了位置に (])` / `選択範囲を解除 (Ctrl+Shift+A)` / `最後のオブジェクト位置を最終フレームにする` / `タイムライン長を設定...` / `タイムラインにテキストを追加` |
| **設定** | `キーコンフィグ設定（ショートカット変更）...` / `タイムコード表示単位 (Timecode / Frames)` |
| **表示** | `バウンディング枠・中心を表示` (Check) / `グリッド線を表示` (Check) / `主音声波形を表示` (Check) / `スナップを有効にする` (Check) / `選択オブジェクトの追従` (Check) / `ズーム拡大・縮小` |
| **再生** | `再生 / 一時停止 (Space)` / `先頭へ (Home)` / `末尾へ (End)` / `1フレーム戻る (Left)` / `1フレーム進む (Right)` / `前の中間点・境界 ( , )` / `次の中間点・境界 ( . )` |
| **書き出し** | `書き出し設定ダイアログを開く... (Ctrl+Shift+E)` / `選択範囲のみ書き出し切替` |

---

### 4.6 キーボードショートカット体系・キーコンフィグ・フォーカス競合防止

#### A. 既定ショートカット一覧（AviUtl 準拠 ＋ 競合解消済み）

| キー | コンテキスト | 機能 | AviUtl 対応・補足 |
| --- | --- | --- | --- |
| `Space` | `EditorMain` | 再生 / 一時停止トグル | AviUtl `Space` |
| `Left` / `Right` | `EditorMain` | `-1f` / `+1f` コマ送り | AviUtl `←` / `→` |
| `Shift+Left` / `Shift+Right` | `EditorMain` | `-10f` / `+10f`（または設定フレーム数）ジャンプ | AviUtl `Shift+←/→` |
| `Home` / `End` | `EditorMain` | プロジェクト先頭 `0f` / 最終フレームへ移動 | AviUtl `Home` / `End` |
| `,` / `.` | `EditorMain` | 前・次のクリップ境界または中間点へシークジャンプ | AviUtl 相当 |
| `S` | `EditorMain` | 再生位置で選択クリップを分割 | 動画編集標準 / 既存継承 |
| `P` | `EditorMain` | 再生位置へ中間点を追加（`EditCommand::AddMidpoint`） | AviUtl `P`（中間点追加） |
| `[` / `]` | `EditorMain` | 現在位置を選択範囲開始 / 終了（`end = current + 1`）に設定 | AviUtl 範囲指定標準 |
| `Ctrl+A` | `EditorMain` | **全クリップ選択** | 汎用編集標準（範囲解除と分離） |
| `Ctrl+Shift+A` / `Escape` | `EditorMain` | **選択範囲（`[` `]`）を解除**（`Escape` はモーダル閉 → テキストフォーカス解除 → `selection_range` 解除 → `selected_clip_ids` 解除の優先順） | 選択クリア専用 |
| `Delete` / `Backspace` | `EditorMain` | 選択中の全クリップを削除 | AviUtl `Delete` |
| `Ctrl+X` / `Ctrl+C` / `Ctrl+V` / `Ctrl+D` | `EditorMain` | 切り取り / コピー / 貼り付け / 複製 | AviUtl & 既存継承 |
| `Ctrl+Z` / `Ctrl+Y` (`Ctrl+Shift+Z`) | `EditorMain` | Undo / Redo | AviUtl & 既存継承 |
| `Tab` | `EditorMain` | 現在再生位置にあるオブジェクトを選択・巡回（テキスト欄/モーダルフォーカス時は標準フォーカス移動を優先） | 複数重なり時の選択切替 |
| `^` / `\` または `Ctrl++` / `Ctrl+-` | `EditorMain` | タイムラインズームイン / ズームアウト | AviUtl `^` / `\` |
| `Alt + Arrow` / `Alt+Shift + Arrow` | `EditorMain`（選択時） | プレビュー上 `X/Y` を `1px` / `10px` 微調整 | 矢印コマ送りと安全分離 |
| `*` / `/`（Numpad 等）<br>微調整: `Ctrl + *` / `Ctrl + /` | `EditorMain`（選択時） | 回転角 `+5°` / `-5°`（`Ctrl` 併用で **`1°` 刻み**） | JIS 配列で `*` 自体が `Shift+:` のため、`Shift` ではなく **`Ctrl` 修飾** で 1° 刻みを識別（マウス回転ハンドルドラッグのみ `Shift` 15° スナップ可） |

#### B. メニューバーからのキーコンフィグ設定（カスタマイズ・衝突回避）
JIS / US 配列差やユーザー個人の AviUtl カスタム割り当て（例：タイムライン上での `Alt` / `Ctrl` 好み、回転キーを `Q`/`E` へ割当など）を自由に調整できるよう、メニューバー **`設定` → `キーコンフィグ設定...`** モーダルを提供する。
- `ShortcutAction`（全エディタアクション列挙体）⇔ キー組合せ文字列のマップ構造体 `KeyConfig` を [`AppController`](../../src/ui/app_controller.rs) およびアプリローカル設定ファイル（OS 標準設定ディレクトリ配下の `key_config.json`、プロジェクトファイル `.smc.json` には混入させない）へ保持。
- モーダル内でアクション行をクリック→新しいキーを押下してリアルタイム変更し、**他アクションとのキー重複があれば警告ハイライト表示**（確認の上で上書き可）。
- 「`AviUtl 既定に戻す`」ボタンを備え、変更時は即座に `cx.bind_keys(...)` を再登録して再起動不要で反映する。

#### C. 既存グローバルバインド移行とテキスト入力中のフォーカス保護（必須移行作業）
現行 [`menu_bar.rs#L47-L63`](../../src/ui/menu_bar.rs#L47-L63) は `KeyBinding::new("space", TogglePlayback, None)` や `KeyBinding::new("s", SplitAtPlayhead, None)` のように **キーコンテキスト `None`（ウィンドウ無条件グローバル）** で登録されており、かつ [`window.rs#L469-L527`](../../src/ui/window.rs#L469-L527) の `on_action` でも入力フォーカス検査を行っていない。このまま新入力欄を増やすと、字幕や `f(t)` 欄への入力中に `"s"` や `" "` が奪われるため、以下 2 重の移行・保護作業を Phase D-4 に必須で組み込む。

1. **既存グローバル `KeyBinding` の `"EditorMain"` コンテキスト移行**:
   - [`menu_bar.rs#L47-L63`](../../src/ui/menu_bar.rs#L47-L63) の既存 15 バインディング（および新規ショートカット群）のコンテキストを **`None` → `Some("EditorMain")`** へすべて移行し、[`window.rs`](../../src/ui/window.rs) の非入力ワークスペース外枠のみ `.key_context("EditorMain")` を設定（`InputState` は自身の `"Input"` コンテキストを持つため単一キーが自動マスクされる）。
2. **アクションハンドラ冒頭での防御判定（`is_any_text_input_focused`）**:
   - [`window.rs`](../../src/ui/window.rs) にヘルパー `fn is_any_text_input_focused(&self, window: &Window, cx: &App) -> bool` を設け、登録済みの全 `InputState`（設定ダイアログ数値欄、`f(t)` 欄、字幕テキスト欄、`ai_chat_input`、`layer_rename_input`、`timeline_length_input`、`export_dialog` 等）の `focus_handle(cx).is_focused(window)` を検査する。
   - いずれかがフォーカス中であれば、編集・再生・削除系アクション（`Ctrl+S` 等を除く）は何もせず即リターン／`cx.propagate()` し、文字入力・テキスト欄内コピー＆ペーストを絶対優先する。

---

## 5. インターフェース / コントローラ連動仕様

[`src/ui/preview_panel.rs`](../../src/ui/preview_panel.rs) へ追加する主要純粋関数・状態：

```rust
/// プレビューキャンバス上のドラッグ状態機械。
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewDragMode {
    MovePosition { start_x: f64, start_y: f64 },
    AltScale { initial_scale_x: f64, initial_scale_y: f64 },
    Rotate { center_screen: (f32, f32), start_angle_rad: f64, initial_rot_deg: f64 },
    MoveAnchor { start_anchor_x: f64, start_anchor_y: f64 },
    ScrubMasterWaveform { start_frame: u64 },
}

/// プレビューヒット判定用のクリップ領域情報（RenderLayerItem とは独立に構築）。
pub struct PreviewLayerHitQuad {
    pub clip_id: ClipId,
    pub layer_locked: bool,
    pub ndc_vertices: [LayerVertex; 4],
    pub anchor_ndc: [f32; 2],
    pub rotation_deg: f64,
}

/// 4頂点ヒット領域一覧へのウィンドウ座標ヒット判定（逆順・最前面優先）。
pub fn hit_test_preview_layers(
    quads: &[PreviewLayerHitQuad],
    window_pos: (f32, f32),
    image_rect: (f32, f32, f32, f32),
) -> Option<ClipId>;
```

---

## 6. エラーハンドリング・エッジケース

- **透明・スケール 0 クリップの選択**: `scale_x == 0` などで面積が潰れたオブジェクトはキャンバス上からはクリック不可となるが、タイムラインおよび右パネル一覧からは常時選択・回復可能とする。
- **ドラッグ中の再生開始防止**: `PreviewDragMode` アクティブ中に誤って `Space` が押された場合、まずドラッグをキャンセル（または確定）してから再生トグルを行う。
- **音声デコード未完了・プロジェクト内無音時**: 主音声波形ストリップはエラーにせずフラットな `0dB` ベースライン線のみを描画し、左ドラッグスクラブ（時刻移動）自体は有効のままにする。
- **ハイ DPI（ScaleFactor 1.5x / 2.0x）環境**: GPUI の論理ピクセル（`px`）座標は自動的にスケール抽象化されているため、`image_rect` と `evt.position` を同じ論理ピクセル座標系で差分計算することで Windows 高スケーリング環境でもポインタとオブジェクトのずれが発生しない。

---

## 7. テスト計画・受け入れ基準

### 単体テスト（`src/ui/preview_panel.rs` 末尾 `#[cfg(test)] mod tests`）
1. **点内包ヒットテスト（回転あり）**: 45° 回転・拡大 150% された背面クリップと前面クリップが重なっている時、重なり中心点のテストで最前面クリップの `ClipId` が返り、背面クリップのみ領域で背面 `ClipId` が返ること（さらに前面レイヤーロック時は背面が貫通選択されること）。
2. **`screen_delta_to_canvas_delta` スケールテスト**: `1920x1080` プロジェクトが `960x540` プレビュー枠（`0.5x`）に表示されているとき、画面上 `+10px` ドラッグが正確に `+20.0 canvas px` へ換算されること。
3. **Shift 軸ロック＆Alt スケール換算テスト**: 斜めドラッグ時に `Shift` 指定で主軸以外の変化量が `0.0` に固定されること。

### 結合テスト（[`05-implementation-plan.md`](05-implementation-plan.md) Phase D-4）
- プレビュー上でクリップ選択 → 直接移動ドラッグ → 回転・`Alt+拡大` コマンド生成 → 1f ステップ操作 → `Undo` 全復元テスト。

### 受け入れ基準（Done 判定）
- プレビュー中央の画像・文字をドラッグして `X, Y` が動き、その変化が右パネル `Clip` タブ数値およびタイムライン合成映像に即時同期すること。
- 字幕欄・`f(t)` 欄へのテキスト入力中に `Space`, `S`, `P`, `←`, `→` をタイプしても再生・分割・シークが誤発動しないこと。
