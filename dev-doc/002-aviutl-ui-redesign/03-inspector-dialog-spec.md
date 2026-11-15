# [002-03] 設定ダイアログ（Clip インスペクタ）UI/UX 詳細仕様

- **ステータス**: Draft
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **関連ドキュメント**:
  - 概要設計書: [`README.md`](README.md)
  - データモデル＆コマンド仕様: [`01-data-model-and-commands.md`](01-data-model-and-commands.md)
  - タイムライン仕様: [`02-timeline-spec.md`](02-timeline-spec.md)
  - プレビュー＆ショートカット仕様: [`04-preview-and-shortcuts-spec.md`](04-preview-and-shortcuts-spec.md)
  - 既存実装: [`src/ui/inspector_panel.rs`](../../src/ui/inspector_panel.rs), [`src/ui/right_panel.rs`](../../src/ui/right_panel.rs), [`src/core/animation.rs`](../../src/core/animation.rs), [`src/media/visual_properties.rs`](../../src/media/visual_properties.rs)

---

## 1. 概要・目的

現行の [`src/ui/inspector_panel.rs`](../../src/ui/inspector_panel.rs) は、16 個のパラメータ（[`InspectorParameterSlot`](../../src/ui/inspector_panel.rs)）から 1 つだけをボタン選択し、`+step` / `-step` ボタンを連打して値を変える方式となっており、`Opacity` 以外のスライダー・数値直接入力欄・同時一覧表示・AviUtl「設定ダイアログ」的な左右トラックバーが存在しない。

本仕様では、[`../draft.md`](../draft.md) §2.1 の 1 ウィンドウ構成（右 1/5 リサイズ可能タブパネル）を維持したまま、`Clip` タブ内に **AviUtl「設定ダイアログ」と同等の高密度・高速編集 UI** を構築するための詳細設計を定義する。

---

## 2. スコープ

### 対象（In Scope）
- 右パネル `Clip` タブ全体の構成（オブジェクトヘッダ・中間点バー・各種設定ブロック）。
- レスポンシブ 2 モード（幅 `240..379px` のコンパクト 2 段表示 ／ 幅 `380px` 以上の AviUtl 準拠 1 行横並び表示）を持つ **`TrackBarRow`** コンポーネント仕様。
- トラックバーの「左値（開始）— 中央ボタン（パラメータ名＋移動方法メニュー）— 右値（終了）」操作、`Shift+ドラッグ` 精密調整、直接数値入力、単位変換（`拡大率 %` / `透明度 0..100` / `中心 px`）。
- 中間点区間バー（`◀ [====] ▶ [+][-]`）と [`AnimatedParameter`](../../src/core/animation.rs)（`Constant` / `Easing` / `Keyframes` / `Expression f(t)`）の整合マッピング。
- クリップ種別（`Video` / `Image` / `Subtitle` / `Audio`）ごとの折りたたみセクション構成、複数選択時／未選択時表示、`16ms` 描画ティックとのフォーカス競合回避。

### 対象外（Out of Scope）
- AviUtl 拡張プラグイン由来のカスタムアニメーション効果 DLL パラメータ。
- AI チャットタブ（`AI Chat`）そのもののプロンプト応答 UI（[`ai_chat_panel.rs`](../../src/ui/ai_chat_panel.rs) 現行維持）。

---

## 3. 要件・制約

- **機能要件**:
  - `Clip` を選択した瞬間に、`X`, `Y`, `拡大率`, `透明度`, `回転`, `合成モード`（および字幕／音声パラメータ）がすべて同時に視認・即編集できること。
  - 右パネル左端の境界ドラッグ（既定 1/5 幅 = `1280px` 時 `256px`〜最大 `50%` = `640px`）のいずれの幅でも要素が重なったり見切れたりしないこと（最小実効幅 `240px`）。
  - 数値テキストボックス入力中に `16ms` 再生更新タイマーで入力中の文字が上書きされないこと。
- **互換制約**:
  - 内部コマンドは既存の [`EditCommand::SetParameter`](../../src/timeline/command.rs)（[`ParameterPath`](../../src/timeline/command.rs) の `BlendMode` / `SubtitleText` / `SubtitleFontSize` / 各種アニメスロット等）および `EditCommand::Batch` のみを使用し、`src/core/animation.rs` の評価規則を変えないこと。

---

## 4. アーキテクチャ / 設計詳細

### 4.1 全体構成（右パネル `Clip` タブ）

```text
+-------------------------------------------------------+
| [Clip] [AI Chat] [Layers] [Project]                   |  <-- TabBar
+-------------------------------------------------------+
| 動画/画像: Title_Clip.mp4        [00:02.00 - 00:05.00]|  <-- 1. オブジェクトヘッダ (36px)
| Layer 2 | 長さ: 180f (3.00s)                          |
+-------------------------------------------------------+
| 67 ◀ [■■■■■■■■■■|■■■■■■■■] ▶ 142   [+] [-]   |  <-- 2. 中間点バー (28px)
+-------------------------------------------------------+
| ▼ 標準描画                                  [リセット] |  <-- 3. 折りたたみセクション群
|  (ここに TrackBarRow × 5 + 中心X/Y + 合成モード)      |      (縦スクロール可能)
| ▶ 色調補正 (5項目)                                    |
| ▼ 字幕設定 (Subtitle 時のみ表示・展開)                |
| ▼ 音声再生 (Audio 時のみ表示・展開)                   |
| ▼ 数式 f(t) / イージングカーブ詳細                    |
+-------------------------------------------------------+
```

- **右パネル最小幅の調整**: 現行 [`MIN_RIGHT_PANEL_WIDTH = 220.0`](../../src/ui/app_controller.rs) を **`240.0px`** に引き上げ、`240px` 以上で常に破綻なく操作可能とする（初期 1/5 比率は `1280px` ウィンドウで `256px` のため初期状態から余裕を持って収まる）。

---

### 4.2 オブジェクトヘッダと中間点バー

#### A. オブジェクトヘッダ
- **上段**：クリップ種別色ドット（青/赤/黄橙）+ クリップ名（例：`[動画] scene01.mp4`、`[字幕] すごい！`）+ `[リセット]` 小ボタン（全スロット既定値戻し、`EditCommand::Batch`）。
- **下段**：`Layer N | 開始 120f 〜 終了 300f (長さ 180f / 3.00s)`。開始/長さクリックでタイムラインをその位置へスクロールフォーカスできる。

#### B. 中間点バー（Segment Navigator Bar）
画面上部スクリーンショットの中間点コントロールを忠実に組み込む。
- 左数値（例：`67`）：現在選択区間の開始エフェクトフレーム（またはクリップローカル位置 `seg_start`）。
- `◀` ボタン：`selected_segment_index` を `1` つ前へ移動し、再生ヘッドも前区間境界付近（オプション）または区間先頭へ同期。
- **中央青い中間点プログレスバー**：
  - クリップ全長をバー幅 100% とし、中間点位置を白縦線 `|` で区切る。
  - 現在選択中の区間（`selected_segment_index`）だけを明るい青（`#3B82F6`）、他区間を暗グレー（`#374151`）で塗り分ける。
  - **バー内の各区間を直接左クリック**することでも `selected_segment_index` を即座に切り替えられる（タイムライン上をクリックせず設定ダイアログ内だけで区間切替が完結）。
- `▶` ボタン：次の区間へ移動。
- 右数値（例：`142`）：現在選択区間の終端包括エフェクトフレーム `seg_last`（クリップ末尾なら `eff_last = eff_start + duration - 1`）。
- `[+]` ボタン：**現在再生位置に中間点を追加**（`EditCommand::AddMidpoint` 相当、`P` キーと同等）。再生ヘッドが `eff_start < eff < eff_last` の範囲外、または既設中間点上にある場合は無効化＋ツールチップ案内。
- `[-]` ボタン：**現在区間境界の中間点を削除**（`EditCommand::RemoveMidpoint`、可視中間点が 0 個の時は無効）。

---

### 4.3 `TrackBarRow` コンポーネント仕様（AviUtl トラックバーの核心）

AviUtl の設定ダイアログ最大の特徴である **「左（開始）— 中央（項目名＆移動モード）— 右（終了）」** の左右連動型トラックバーを専用ウィジェット `TrackBarRow` として実装する。

#### A. レスポンシブ 2 モード自動切替（`panel_width` 基準）

1. **幅 `>= 380px`：AviUtl 1 行横並びレイアウト（推奨幅・高さ `28px/行`）**
```text
[◀=====●===] [<][>] [  0.0 ]   [   X ▼   ]   [ 100.0 ] [<][>] [===●=====▶]
  左スライダ   微調    開始値   項目/移動モード   終了値    微調   右スライダ
```

2. **幅 `240px .. 379px`：コンパクト 2 段レイアウト（初期 1/5 幅 `256px` 時・高さ `46px/行`）**
```text
Row 1: [ X  直線 ▼ ]     [   0.0 ] [<][>]  →  [ 100.0 ] [<][>]
Row 2: [◀=====●========]  |  [========●=====▶]
```

#### B. 各操作部品の厳密な振る舞い

| 部品 | `Constant` (`移動なし`) 時の振る舞い | `Easing` / `Keyframes` (`移動あり`) 時の振る舞い |
| --- | --- | --- |
| **左数値欄 / 左スライダ** | 現在値（= 全区間共通定数）を表示・編集。即時 `SetParameter` 発行 | **現在区間の開始点値**を表示・編集。未カット `Easing` 時は `start_value`、整列済み `Keyframes` 時は `keyframes[seg_idx].value` を更新（未整列の AI/旧データ編集時は編集コマンド先頭で `sync_and_align_clip_keyframes` を実行して `grid(clip)` へ 1:1 整列）。**カット済み `Easing`（`effect_offset_frames != 0` 等）は初回編集コマンド時に現在評価値ベースの `Keyframes([eff_start, ...visible..., eff_last])` へ自動昇格してから更新する** |
| **中央項目名ボタン**（例：`X` / `X 直線`） | 項目名＋現在モード小アイコン表示。**クリックで移動モード選択 `PopupMenu` を展開**。さらに `Constant` 中は背景が通常色、アニメーション有効時は青アクセント枠 `#3B82F6` で強調 | 同左。現在採用している **全区間共通の移動方法**（`直線`/`加減速`/`ベジェ`/`f(t)` 等、旧データ混在時は `混在`）の略称バッジを表示 |
| **右数値欄 / 右スライダ** | 薄いグレー（無効状態・値も左と同値表示）。**特例利便性**: ユーザーが直接右数値欄をクリックして値を入力するか右スライダをドラッグした場合は、自動的に `Constant` → `Easing(Linear)`（`visible_clip_midpoints(clip)` 非空時は全境界 `grid(clip)` で 1:1 整列した `Keyframes(Linear)`）へ昇格し即座にアニメーション開始できる | **現在区間の終了点値**を表示・編集。未カット `Easing` 時は `end_value`、整列済み `Keyframes` 時は `keyframes[seg_idx + 1].value` を更新（カット済み `Easing`／未整列データは左欄と同規約で自動昇格／整列してから更新） |
| **`<` / `>` 微調ボタン** | 1 クリックで `±step`（Shift 押下で `±0.1 * step`）増減 | 左 `<`/`>` は開始値、右 `<`/`>` は終了値を同様に微調 |

- **`Shift + スライダードラッグ`**: 通常感度の **`1/10` 速度** で微細ドラッグ（例：`拡大率 100.00%` の `0.01%` 調整など）。
- **スライダードラッグ中のコマンドバッチ化**: ドラッグ中のプレビュー更新は即時にフレーム評価・コンポジタ再描画しつつ、`CommandDispatcher` の履歴（Undo）には **`MouseUp` 時に 1 回だけ** コマンドを積む（または同一ドラッグセッションを `Batch`/上書き統合し、`Ctrl+Z` 1 回でドラッグ前に戻る）。
- **直接数値入力の確定契機**: `Enter` キー押下、または `focus_out`（別要素クリック）時にテキストをパースして検証・コマンド発行する。不正文字や範囲外値はフォーカスアウト時に現在値へ戻し、エラーメッセージをステータスバーへ出す。
- **右クリック（パラメータ行メニュー）**: `既定値に戻す`, `全区間の値を同じにする`, `数式 f(t) で編集...`, `他スロットへカーブをコピー`。

#### C. 中央ボタン「移動方法選択 `PopupMenu`」と内部モデル対応マッピング

中央の項目名ボタンをクリックすると以下の `PopupMenu`（`gpui_component::menu::PopupMenu`）が開く。**AviUtl 準拠により、ここで選ぶ移動方法（補間カーブ）はパラメータ行の全区間に共通適用される（全区間共通）**。

| メニュー表示名 | `AnimatedParameter` への変換結果 | 補間モード転換・統一ルール |
| --- | --- | --- |
| **`移動なし` (`Constant`)** | `AnimatedParameter::Constant { value }` | 現在再生位置の評価値をそのまま `value` に採用（クリップ自身の [`Clip::midpoints`](../../src/timeline/document.rs#L346) は消さずに保持） |
| **`直線移動` (`Linear`)** | `AnimatedParameter::Easing { start_value, end_value, curve: Linear, extrapolation }`（中間点あり時は `Keyframes`） | 中間点なし時：`Easing { start_value, end_value, curve: Linear }`。<br>中間点あり（または既存 `Keyframes`）：先に `sync_and_align_clip_keyframes` で `grid(clip)` へ 1:1 整列した上で、**全 `keyframes[*].easing` を一括で `Linear` へ統一変更** |
| **`加減速移動` (`EaseInOut`)** | `AnimatedParameter::Easing { start_value, end_value, curve: EaseInOut, extrapolation }`（中間点あり時は `Keyframes`） | 同上（中間点存在時は `grid(clip)` へ 1:1 整列＋全 `keyframes[*].easing` を `EaseInOut` へ統一） |
| **`曲線移動（加速）` (`EaseIn`)** | `AnimatedParameter::Easing { start_value, end_value, curve: EaseIn, extrapolation }`（中間点あり時は `Keyframes`） | 同上（全 `keyframes[*].easing` を `EaseIn` へ統一） |
| **`曲線移動（減速）` (`EaseOut`)** | `AnimatedParameter::Easing { start_value, end_value, curve: EaseOut, extrapolation }`（中間点あり時は `Keyframes`） | 同上（全 `keyframes[*].easing` を `EaseOut` へ統一） |
| **`ベジェ曲線` (`CubicBezier`)** | `AnimatedParameter::Easing { start_value, end_value, curve: CubicBezier { x1, y1, x2, y2 }, extrapolation }`（中間点あり時は `Keyframes`） | 全区間に同一 `CubicBezier` を適用し、下部に小型制御点 `(x1, y1, x2, y2)` 入力 UI を表示 |
| **`キーフレーム固定` (`Keyframes`)** | `AnimatedParameter::Keyframes { keyframes, extrapolation }` | `grid(clip) = [eff_start, ...visible_midpoints..., eff_last]` の全境界フレームで現在値を評価し `Keyframes` 化（現在カーブを維持・`Constant` 時は `Linear`） |
| **`数式 f(t)` (`Expression`)** | `AnimatedParameter::Expression { expr, base_offset, scale }` | 現在評価値を初期定数文字列 `expr`（例：`"100.000"`）、`base_offset: 0.0`, `scale: 1.0` として切替し、`expr` / `base_offset` / `scale` 入力欄を展開（`build_switch_mode_command` 既存仕様準拠） |

> [!IMPORTANT]
> 1. **カット済み `Easing` の表示と自動昇格規約**: [`AnimatedParameter::Easing`](../../src/core/animation.rs#L220) は `effect_frame = 0` .. `effect_duration_frames - 1` を基準に進行率 `p` を算出するため（[`EvaluationContext::normalized_progress`](../../src/core/animation.rs#L53)）、カット分割・左トリム直後（`effect_offset_frames != 0` 等）で中間点なし `Easing` が残っている場合、UI 表示値は `param.evaluate(...)` による現在クリップ境界実測値（注記 `[分割継続]`）を表示し、**ユーザーが境界数値またはスライダを編集する初回コマンド実行時に自動で `[eff_start, ...visible_midpoints..., eff_last]` の評価値ベース `Keyframes` へ昇格して編集する**（カット直後の未編集再生時は既存テスト通り連続性を保持）。
> 2. **移動方法の単位は全区間共通（AviUtl 準拠）＋ `(a) 強制整列`**: AviUtl と同じく、移動方法（`直線移動` / `加減速移動` 等）の変更はスロットの **全区間に共通適用**（全 `keyframes[*].easing` を一括設定）する。また全 `Keyframes` スロットは [`01-data-model-and-commands.md`](01-data-model-and-commands.md) §4.3 の `sync_and_align_clip_keyframes` によって `grid(clip)` と常に 1 対 1 整列されるため、`keyframes[seg_idx]` / `keyframes[seg_idx + 1]` との齟齬が起きない（API 経由で混在カーブや未整列キーを持つ既存クリップを初めて表示する際も、`read_trackbar_segment_view` は評価値フォールバックにより安全に読み取り、編集コマンド適用時に 1:1 整列＋全区間統一を行う）。

---

### 4.4 パラメータ定義・UI 表示単位・既定レンジ一覧

[`VisualProperties`](../../src/media/visual_properties.rs)（`with_color_adjustments` のドメインクランプ範囲）および [`SubtitleProperties`](../../src/media/visual_properties.rs) / [`InspectorParameterSlot::bounds`](../../src/ui/inspector_panel.rs) と UI 表示値の変換規約を以下に統一する（`Saturation` 上限 `3.0 → 4.0` および `Gamma` 範囲 `0.1..4.0 → 0.05..5.0` は Phase D-3 でドメイン値域へ揃える）。

| 表示セクション | UI ラベル | 内部スロット / パス | 表示単位・レンジ（ドメイン整合） | ステップ / Shift微調 | 内部値 ⇔ UI 表示変換式 |
| --- | --- | --- | --- | --- | --- |
| **標準描画** | `X` | `PositionX` | `px`（数値 `-4000..4000`、スライダ `-1920..1920`） | `1.0` / `0.1` | 1 : 1 |
| **標準描画** | `Y` | `PositionY` | `px`（数値 `-4000..4000`、スライダ `-1080..1080`） | `1.0` / `0.1` | 1 : 1 |
| **標準描画** | `拡大率` / `拡大率X` / `拡大率Y` | `ScaleX` (+`ScaleY`) | `%`（数値 `0.0 .. 1000.0%`、スライダ `5.0 .. 500.0%`、既定 `100.00%`） | `1.0` / `0.1` | `ui = scale * 100.0` / `scale = ui / 100.0` |
| **標準描画** | `透明度` | `Opacity` | `%`（`0.0` 不透明 〜 `100.0` 完全透明） | `1.0` / `0.1` | `ui = (1.0 - opacity) * 100.0` / `opacity = 1.0 - (ui / 100.0)` |
| **標準描画** | `回転` | `RotationDeg` | `deg`（数値 `-3600..3600`、スライダ `-1080..1080`） | `1.0` / `0.1` | 1 : 1（時計回り正） |
| **標準描画** | `中心X` | `AnchorX` | `px`（数値 `-4000..4000`、スライダ `-1920..1920`） | `1.0` / `0.1` | 1 : 1（`VisualProperties` 既存仕様と一致） |
| **標準描画** | `中心Y` | `AnchorY` | `px`（数値 `-4000..4000`、スライダ `-1920..1920`） | `1.0` / `0.1` | 1 : 1 |
| **標準描画** | `合成モード` | `ParameterPath::BlendMode` | `通常` / `加算` / `乗算` / `スクリーン` / `オーバーレイ` | Select ドロップダウン | `BlendMode::{Normal, Add, Multiply, Screen, Overlay}` |
| **字幕設定** | `文字サイズ` | `SubtitleFontSize` | `px`（数値 `8.0 .. 400.0`、スライダ `8.0 .. 200.0`、既定 `48.0`） | `1.0` / `0.5` | 1 : 1 |
| **色調補正** | `明るさ` | `Brightness` | `-1.0 .. +1.0`（既定 `0.0`） | `0.05` / `0.01` | 1 : 1 |
| **色調補正** | `コントラスト` | `Contrast` | `0.0 .. 4.0`（既定 `1.0`） | `0.05` / `0.01` | 1 : 1 |
| **色調補正** | `彩度` | `Saturation` | `0.0 .. 4.0`（既定 `1.0`） | `0.05` / `0.01` | 1 : 1 |
| **色調補正** | `色相` | `HueDeg` | `-180.0 .. +180.0 deg`（既定 `0.0`） | `1.0` / `0.1` | 1 : 1 |
| **色調補正** | `ガンマ` | `Gamma` | `0.05 .. 5.0`（既定 `1.0`） | `0.05` / `0.01` | 1 : 1 |
| **音声再生** | `音量` | `AudioVolume` (`GainDb`) | `dB`（`-60.0 .. +24.0`、既定 `0.0`） | `0.5` / `0.1` | 1 : 1 |
| **音声再生** | `左右` (Pan) | `AudioPan` (`Pan`) | `-100.0 .. +100.0`（L 〜 R、既定 `0.0`） | `1.0` / `0.5` | `ui = pan * 100.0` / `pan = ui / 100.0` |

- **`拡大率` 縦横連動ボタン `[🔗]`**:
  - `scale_aspect_locked == true`（既定）：`拡大率` 1 行のみを表示し、編集時は `ScaleX` と `ScaleY` の双方へ同一コマンドを `Batch` 発行（`ScaleX == ScaleY` を保持）。
  - `scale_aspect_locked == false`（アイコンクリックで解除）：`拡大率X` と `拡大率Y` の 2 行へ展開し、縦横独立した伸縮やアニメーションを可能にする。

---

### 4.5 クリップ種別別セクション表示と折りたたみ（`Collapsible`）

選択クリップの `ClipKind` に応じて不要セクションを自動非表示にし、必要な操作へ最短で届く構成とする。

1. **`ClipKind::Video` / `ClipKind::Image`**:
   - `[▼] 標準描画`（既定：**展開**）：`X`, `Y`, `拡大率`, `透明度`, `回転`, （`[▶] 中心座標・追加` 内に `中心X`, `中心Y`、または常設小型行） + `合成モード`
   - `[▶] 色調補正`（既定：**折りたたみ**、デフォルト値以外が設定中時はヘッダに青い変更ドット表示）
   - `[▶] 数式 f(t) / カーブプレビュー`（任意スロット選択で `Expression` またはイージング曲線プレビューを表示）
2. **`ClipKind::Subtitle`**:
   - `[▼] 字幕設定`（既定：**展開**・現行 [`SubtitleProperties`](../../src/media/visual_properties.rs#L307-L331) 全フィールドに準拠）：
     - 字幕テキスト複数行入力欄（`ParameterPath::SubtitleText`：`Input` multiline、フォーカスアウト / `Ctrl+Enter` 確定）
     - フォント名（`ParameterPath::SubtitleFontFamily`：`Input` またはシステムフォント候補選択）
     - 揃えボタン `[左][中央][右]`（`ParameterPath::SubtitleAlignment` -> `TextAlignment`）、文字色（`ParameterPath::SubtitleTextColor`：`ColorPicker`）、縁取り色（`ParameterPath::SubtitleOutlineColor`：`ColorPicker`）、縁取り幅（`ParameterPath::SubtitleOutlineWidth`：数値入力）
     - `文字サイズ`（`SubtitleFontSize` の `TrackBarRow`）
   - `[▼] 標準描画`（既定：**展開**）：字幕自体の `X, Y, 拡大率, 透明度, 回転, 合成モード` を動画と同様にキーフレーム＆移動アニメ可能
   - `[▶] 色調補正`（既定：折りたたみ）
3. **`ClipKind::Audio`**:
   - `[▼] 音声再生`（既定：**展開**）：`音量 (dB)`, `左右バランス (Pan)`
   - `[▶] 数式 f(t) / カーブプレビュー`

#### `f(t)` 数式エディタ統合（`draft.md` §1 要件直結）
- 各パラメータの中央モードメニューで `数式 f(t)` を選択するか、`数式 f(t) / カーブプレビュー` セクションを開くと [`AnimatedParameter::Expression`](../../src/core/animation.rs#L241) の全フィールドを編集可能にする：
  - 数式入力欄 `f(t) = [ InputState ]`（`expr` フィールド。変数チートシート：`t`:経過秒, `p`:正規化進捗 `0..1`, `d`:長さ秒, `pi`, `e` / 関数：`sin, cos, tan, abs, sqrt, pow, clamp, lerp, smoothstep`）
  - 基準調整欄：`base_offset`（オフセット）と `scale`（倍率）の数値入力（`scale * raw + base_offset` 評価）
  - リアルタイム構文検証ステータス（緑 `OK: t=0.5s -> 42.3` / 赤パースエラー理由）
  - Mini Curve Graph：[`sample_parameter_curve`](../../src/ui/inspector_panel.rs) による 32〜64 サンプル折れ線プレビュー（現在再生ヘッド位置を縦線で併記）。

---

### 4.6 複数選択・未選択時の UI 状態

- **クリップ未選択時（`selected_clip_ids.is_empty()`）**:
  - タブ名横に `(選択なし)` 表示。
  - パネル中央に分かりやすいガイドカード：「タイムライン上のオブジェクトをクリックして編集 / 空トラックを右クリックして新規作成 / `[Import]` でメディア読込」。
- **複数選択時（`selected_clip_ids.len() >= 2`）**:
  - ヘッダに **`[N 個のオブジェクトを選択中]`** バッジ＋選択リスト要約（先頭 3 件名）＋操作ボタン（`一括削除`, `一括複製`, `選択中の先頭へ移動`）を表示。
  - 値表示・トラックバーはそのうち主選択オブジェクト（`primary_selected_clip_id()`）の値を編集対象とする旨を明示（「主選択を編集中」ラベル）し、任意で「一括オフセット（相対移動 `ΔX / ΔY`）」用の補助行を提供（安全のため不意の全上書きを防止）。

---

## 5. インターフェース / コントローラ連動仕様

### 5.1 パラメータ読み書き純粋関数（テスト可能設計）

```rust
/// クリップと選択区間における、指定パラメータの開始値・終了値・移動モードを取得する（UI単位換算済み）。
pub struct TrackBarSegmentView {
    pub param: InspectorParameterSlot,
    pub mode: InspectorMode,
    pub easing_curve: Option<EasingCurve>,
    pub has_mixed_easing: bool,
    pub start_ui_value: f64,
    pub end_ui_value: f64,
    pub current_ui_value: f64,
    pub min_ui: f64,
    pub max_ui: f64,
    pub step_ui: f64,
    pub unit_label: &'static str,
    pub expression_text: String,
    pub expr_base_offset: f64,
    pub expr_scale: f64,
    pub has_midpoints: bool,
    pub is_cut_easing_origin: bool,
}

/// 指定クリップと現在区間のパラメータ表示値を純粋算出する。
///
/// `Keyframes` が `grid(clip)` と 1:1 整列済みなら `keyframes[seg_idx]` / `keyframes[seg_idx + 1]` を直接返し、
/// 未整列の旧JSON／カット直後 `Easing` クリップに対しても `param.evaluate(...)` フォールバックで安全に現在境界値を取得する。
pub fn read_trackbar_segment_view(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    segment_index: usize,
    playhead: FrameIndex,
) -> TrackBarSegmentView;

/// UI単位値を受け取り、現在選択区間の開始点または終了点を更新する EditCommand を構築する。
///
/// 未整列の `Keyframes` やカット済み `Easing` (`effect_offset_frames != 0` 等) の初回編集時は、
/// 単一 `Batch` / コマンド内で `sync_and_align_clip_keyframes`（または `[eff_start, ...visible..., eff_last]` への
/// `Keyframes` 自動昇格）を実行した上で該当境界値を書き換える。
pub fn build_set_segment_endpoint_command(
    doc: &ProjectDocument,
    clip: &Clip,
    slot: InspectorParameterSlot,
    segment_index: usize,
    is_end_endpoint: bool,
    new_ui_value: f64,
    aspect_locked: bool,
) -> EditCommand;
```

### 5.2 `16ms` 描画ティックとテキスト入力フォーカス保護ルール

1. [`SeaEditorWindow::sync_sliders_now`](../../src/ui/window.rs) は 16ms タイマーおよびコマンド実行後に発火するが、各 `InputState`（X/Y/拡大率/数式/字幕テキスト等）について **`if !input_state.focus_handle(cx).is_focused(window)` の場合のみ** ドメイン値でテキストを再同期する。
2. 選択クリップや `selected_segment_index` が切り替わった場合のみ強制同期し、編集中フォーカスを安全に確定または破棄する。
3. スライダードラッグ中はテキスト欄へ即時反映し、スライダー自体の `SliderState` 値はドラッグ中の自身の変更を無視またはそのまま保持してカクつきを防ぐ。

---

## 6. エラーハンドリング・エッジケース

- **透明度 `0..100` 逆転入力の境界**: UI で `-5` や `120` が入力された場合は `0.0..=100.0` へ自動クランプして内部 `opacity` を `0.0..=1.0` に収める（`0` 除算や負アルファ防止）。
- **拡大率負数・`0%` 入力**: UI では `0.0%` 以上に制限（反転用途で負値が必要でない限り `0.0..=1000.0%` とし、`scale = 0.0` でも `Matrix2D` がクラッシュしないことを確認）。
- **不正な数式 `f(t)`**: パースエラー時は即時赤枠エラー表示し、ドメインには構文正当な数式のみ `EditCommand::SetParameter` で反映する（入力途中の `"sin("` で既存アニメが壊れない）。
- **短いクリップ（`duration == 1f` または `2f`）**: `duration < 3` のため中間点追加 `[+]` ボタンは無効化、`Easing` 時のみ `start_value` / `end_value` 編集可（`1f` 上で補間は開始値相当）。
- **カット済みクリップの `Easing` 編集**: 前述の通り元エフェクト基準の進行率 `0..=effect_duration_frames-1` で動いているため、カット直後の未編集状態までは `Easing` をそのまま維持し、`effect_offset_frames != 0` 等で区間左/右の境界数値またはスライダをユーザーが変更した初回コマンド実行時に、確認ダイアログなしで自動的に `[eff_start, ...visible_midpoints..., eff_last]` の評価値ベース `Keyframes` へ昇格した上で新値を書き込む。
- **パネル幅の極端なリサイズ**: `240px` 未満への縮小は [`clamp_right_panel_width`](../../src/ui/app_controller.rs#L77) が阻止し、文字潰れを防止。

---

## 7. テスト計画・受け入れ基準

### 単体テスト（`src/ui/inspector_panel.rs` 末尾 `#[cfg(test)] mod tests`）
1. **UI 単位変換往復テスト**:
   - `Scale`（`1.5` ⇔ `150.0%`）、`Opacity`（`0.25` ⇔ `透明度 75.0`）、`AudioPan`（`-0.5` ⇔ `-50.0`）の往復精度が `1e-6` 以内であること。
2. **中間点あり時の区間別編集＆全区間共通テスト**:
   - `midpoints = [30]`・`duration = 61`・キーフレーム `[0f: 0.0, 30f: 100.0, 60f: 50.0]` に対し、`segment_index = 1` で `start_ui_value == 100.0`, `end_ui_value == 50.0` が読み取れ、`is_end_endpoint = true` で `200.0` を設定するコマンドを適用すると `60f` のみが `200.0` に変わり `0f`/`30f` が不変であること。また移動方法を `EaseInOut` へ切り替えたとき全 `keyframes[*].easing` が `EaseInOut` へ統一更新されること。
3. **未整列データ・カット済み `Easing` 自動昇格テスト**:
   - 両端欠落／任意キー付きデータに対する編集コマンドで `grid(clip)` へ 1:1 強制整列されること、および `effect_offset_frames != 0` の `Easing` 境界編集で自動 `Keyframes([eff_start, eff_last])` 昇格が行われること。
4. **`移動なし (Constant)` から右値入力での自動アニメ化テスト**:
   - `Constant { 10.0 }` の右端へ `200.0` を入力したとき `Easing(Linear, 10.0 -> 200.0)`（中間点あり時は全境界整列 `Keyframes`）が生成されること。
5. **`拡大率` 縦横連動ロックテスト**:
   - `aspect_locked == true` で `拡大率 = 125.0%` 編集時、`ScaleX` と `ScaleY` 両方を更新する `Batch` が返ること。

### 結合テスト（[`05-implementation-plan.md`](05-implementation-plan.md) Phase D-3）
- クリップ選択 → 中間点追加 → 各区間で X/Y/拡大率/透明度を変更 → `数式 f(t)` 切替 → Undo 連続実行で初期定数状態まで完全一致すること。

### 受け入れ基準（Done 判定）
- `1280x800` 既定ウィンドウ幅（右パネル `256px` 2段表示）および拡大時（`>= 380px` 1行表示）の双方で、`X / Y / 拡大率 / 透明度 / 回転 / 合成モード` がタブ切替なしで一覧・数値入力・スライダードラッグできること。
- 中間点バーの区間クリックとタイムライン上の中間点区間クリックが双方向に同期すること。
