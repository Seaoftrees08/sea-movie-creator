# [002-01] データモデル・編集コマンド・コントローラ拡張仕様

- **ステータス**: Draft
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **関連ドキュメント**:
  - 概要設計書: [`README.md`](README.md)
  - タイムライン仕様: [`02-timeline-spec.md`](02-timeline-spec.md)
  - 設定ダイアログ仕様: [`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md)
  - プレビュー＆ショートカット仕様: [`04-preview-and-shortcuts-spec.md`](04-preview-and-shortcuts-spec.md)
  - 既存実装: [`src/timeline/document.rs`](../../src/timeline/document.rs), [`src/timeline/command.rs`](../../src/timeline/command.rs), [`src/timeline/operations.rs`](../../src/timeline/operations.rs), [`src/ai/planner.rs`](../../src/ai/planner.rs), [`src/ui/app_controller.rs`](../../src/ui/app_controller.rs)

---

## 1. 概要・目的

AviUtl 酷似のタイムライン操作・設定ダイアログ操作を実現するため、既存の純粋 Rust ドメインモデル（[`ProjectDocument`](../../src/timeline/document.rs)、[`EditCommand`](../../src/timeline/command.rs)）および UI 統括コントローラ（[`AppController`](../../src/ui/app_controller.rs)）に必要な拡張を定義する。

設計上の最重要原則は以下の 3 点とする。
1. **後方互換性維持**: 既存の `*.smc.json` プロジェクトファイルを破壊せず、`#[serde(default)]` で新フィールドを安全に読み込めること。
2. **AI-Native スキーマ整合（`schemars::JsonSchema`）**: すべての [`ProjectDocument`](../../src/timeline/document.rs) / [`EditCommand`](../../src/timeline/command.rs) 拡張型に `#[derive(JsonSchema)]` を実装し、宣言的な JSON と明確なコマンド体系（中間点の明示保持・タイムライン長変更等）を通じて AI チャット・MCP サーバーからも人間の UI 操作と対称に操作できること。
3. **カット連続性・整数フレーム境界則の維持**: 既存の `FrameIndex` / `FrameCount` / `effect_offset_frames` / `effect_duration_frames` / `source_in` / `PlaybackRate` および半開区間 `[start, end)`（排他的上端）の定義を厳密に守ること。

---

## 2. スコープ

### 対象（In Scope）
- `ProjectDocument` へのタイムライン長設定（自動追従／手動固定）・選択範囲（`[` `]`）・空プロジェクト手動シーク上限の追加仕様。
- `Clip`（および `NewClipSpec` / クリップボード項目）への明示的 `midpoints: Vec<i64>` フィールド追加。
- `EditCommand` へのタイムライン長・選択範囲・中間点操作コマンド追加、および [`src/timeline/operations.rs`](../../src/timeline/operations.rs)・[`src/timeline/dispatcher.rs`](../../src/timeline/dispatcher.rs)・[`src/ai/planner.rs`](../../src/ai/planner.rs) での網羅対応。
- `AppController` への複数クリップ選択、中間点区間インデックス、時間表示単位、タイムライン密度・ズーム定数統一（`0.2..=32.0`）、右パネル最小幅（`240.0`）、カスタマイズ可能キーコンフィグ（`KeyConfig`）仕様。

### 対象外（Out of Scope）
- カメラ制御・グループ制御・図形専用ジェネレータなど新規 `ClipKind` バリアント本体のシェーダ実装（将来拡張余地とメニュー導線のみ定義）。

---

## 3. `ProjectDocument` 拡張仕様（`src/timeline/document.rs`）

### 3.1 タイムライン長（最終フレーム位置）モデル

現状の [`ProjectDocument::total_duration_frames()`](../../src/timeline/document.rs#L534) は配置済みクリップの最大終端フレームを返すのみで、空プロジェクト時に `0` となり「プロジェクトの尺をあらかじめ延ばす／縮める」「最後のオブジェクトの後に余白を残す」ことができない。

そこで `ProjectDocument` に以下を追加する。

```rust
/// プロジェクト全体の最終フレーム（タイムラインの長さ）決定ポリシー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TimelineLengthMode {
    /// 配置されている全クリップの最大終端フレームへ自動追従する。
    #[default]
    AutoToLastClip,
    /// ユーザーが指定した最終フレーム位置（1 フレーム以上）で固定する。
    /// クリップがその終端を超えて配置されていても、最終フレーム境界線（再生・書き出し上限）を独立保持する。
    Manual {
        /// プロジェクトの総フレーム数（最終フレーム境界）。
        end_frame: FrameCount,
    },
}

/// タイムライン上の連続フレーム区間 `[start, end)`（半開区間・`start < end`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FrameRange {
    /// 開始フレーム（包括）。
    pub start: FrameIndex,
    /// 終了フレーム（排他的上端）。
    pub end: FrameIndex,
}
```

`ProjectDocument` への追加フィールド（後方互換 `#[serde(default)]` 付き）：

```rust
pub struct ProjectDocument {
    // ... 既存フィールド ...

    /// プロジェクトの最終フレーム長設定（未指定時は AutoToLastClip）。
    #[serde(default)]
    pub timeline_length: TimelineLengthMode,

    /// `[` / `]` 等で指定された出力・範囲操作用の選択区間 `[start, end)`（未指定時はプロジェクト全体）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_range: Option<FrameRange>,
}
```

### 3.2 実効長・作業スパン・シーク上限計算（29.97fps 厳密対応）

| メソッド | 振る舞い | 用途 |
| --- | --- | --- |
| `clips_end_frame(&self) -> u64` | 全クリップの `max(timeline_end)`（空なら `0`） | 「最後のオブジェクト位置を最終フレーム」算出 |
| `total_duration_frames(&self) -> u64` | ・`AutoToLastClip`: `self.clips_end_frame()`<br>・`Manual { end_frame }`: `end_frame.get()` | 再生上限・書き出し総フレーム数・最終フレーム灰色線位置（空 `Auto` 時 `0` を保ち既存テスト互換） |
| `ruler_display_span_frames(&self) -> u64` | `ten_sec = ((u64::from(fps.numerator()) * 10) + u64::from(fps.denominator()) / 2) / u64::from(fps.denominator()).max(1);`<br>`self.total_duration_frames().max(self.clips_end_frame()).max(ten_sec.max(10))` | 空タイムラインでも正確に 10 秒相当（60fps=`600f`, `29.97fps`=`300f`）のルーラー＆スクロール幅を確保 |
| `seek_upper_bound_frames(&self) -> u64` | ・`AutoToLastClip`: **`self.ruler_display_span_frames()`**<br>・`Manual { end_frame }`: `end_frame.get()` | `AppController::seek_to_frame()` の手動シーク上限（`AutoToLastClip` 時はクリップ有無を問わず、ルーラー表示域 `[0..=ruler_display_span_frames()]` の末尾側空白域へもクリック／ドラッグシーク可能） |

> [!IMPORTANT]
> [`FrameRate::FPS_29_97`](../../src/core/time.rs#L77) のように分子が `30_000`・分母が `1_001` の場合、`fps.numerator() * 10` では `300_000` フレーム（約 2.8 時間）になってしまうため、必ず分母で除算した実フレームレート基準で 10 秒フレーム数（`300f`）を算出する。
> また、[`AppController::seek_to_frame`](../../src/ui/app_controller.rs#L887) の上限クランプ先を `total_duration_frames()` から `seek_upper_bound_frames()` へ変更することで、`AutoToLastClip` 時は空プロジェクト（例：`0..=600f`）でも短尺クリップ配置後（例：1 クリップ `120f` 配置済みで残りの `121..=600f` 空域へ移動したいとき）でも、ルーラー上に表示されている任意時刻へ再生ヘッドを置いてクリップ追加・`[` `]`・右クリック操作を実行できる（再生 `play()` / `tick_playback()` および書き出し上限は引き続き `total_duration_frames()`）。

### 3.3 `selection_range` の不変条件と `[` / `]` 計算

- `ProjectDocument::validate()` において、`selection_range` が `Some(r)` の場合は **`r.start < r.end`** を検証する（違反時は新規エラー `DocumentValidationError::InvalidSelectionRange` を返す）。
- **`FrameRange` は排他的上端 `[start, end)`** とする（`Clip::timeline_end()` と同一規約）。
  - `[` キー / `選択開始位置に設定`：`new_start = current_playhead`。もし既存の `end` が `None` または `new_start >= old_end` なら `end = FrameIndex::new(new_start.get().saturating_add(1))`。
  - `]` キー / `選択終了位置に設定`：プレビューに表示されている現在フレーム `current_playhead` を範囲末尾に含めるため、**`new_end = FrameIndex::new(current_playhead.get().saturating_add(1))`** とする。もし既存の `start` が `None` なら `start = FrameIndex::ZERO`、もし `old_start >= new_end` なら `start = FrameIndex::new(new_end.get().saturating_sub(1))`。
  - これにより同一フレーム `f` 上で `[` と `]` を順に押した場合、1 フレーム区間 `[f, f + 1)` が正しく選択される。
- 選択範囲の解除は **`Ctrl+Shift+A`**、`Escape`（選択範囲優先解除オプション）、または右クリック → `選択範囲を解除` により `None` へ戻す（`Ctrl+A` は全クリップ選択専用とする）。

### 3.4 `Clip` への明示的 `midpoints` フィールド追加（AI-Native 構造）

中間点をキーフレーム配列のみから暗黙導出する設計では、「全パラメータが `移動なし (Constant)` のクリップへ中間点を打つ」「一部スロットを `Constant` へ戻してもオブジェクト区切りを保つ」ことが成立しない。さらに AI エージェントが JSON 上でオブジェクト構造を把握する際も、全パラメータを走査せず **クリップ自身の宣言的プロパティとして中間点を持てる方が遥かに AI-Native** である。

そこで [`Clip`](../../src/timeline/document.rs#L346) および [`NewClipSpec`](../../src/timeline/command.rs#L180)（さらに `ClipboardClip`）へ以下を追加する。

```rust
pub struct Clip {
    // ... 既存フィールド ...

    /// クリップ内の区間分割点（エフェクト基準フレーム座標・厳密昇順・重複なし）。
    ///
    /// 有効な表示・編集対象となる内部中間点は `eff_start < m && m < eff_last` の範囲
    /// （`eff_start = effect_offset_frames`, `eff_last = eff_start + duration.get() as i64 - 1`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midpoints: Vec<i64>,
}
```

- **エフェクト境界座標の厳密定義**:
  - `eff_start = clip.effect_offset_frames`（クリップ先頭の包括エフェクトフレーム）
  - `eff_last = clip.effect_offset_frames + i64::try_from(clip.duration.get().saturating_sub(1))`（クリップ末尾の包括エフェクトフレーム。[`build_switch_mode_command`](../../src/ui/inspector_panel.rs#L442) および [`EvaluationContext::normalized_progress`](../../src/core/animation.rs#L53) の `span - 1` と完全一致）
  - 内部中間点 `m` は **必ず `eff_start < m && m < eff_last`** を満たす位置にのみ追加でき（したがって中間点を追加するには `clip.duration.get() >= 3` が必要）、すべての区間 `[k_i, k_{i+1}]` が `1f` 以上の正の幅を持つ。
- **カット・トリムとの一貫性（[`SplitClip`](../../src/timeline/operations.rs#L527) / [`TrimLeft`](../../src/timeline/operations.rs#L738) / [`TrimRight`](../../src/timeline/operations.rs#L758)）**:
  - `Clip.midpoints` は `Keyframe.frame` と同じエフェクト基準フレーム座標であるため、分割・左右トリム時は両クリップへそのまま複製保持される。
  - UI および区間一覧ヘルパーは **`eff_start < m && m < eff_last`** を満たす要素だけを「現在クリップ上の可視中間点列」として抽出する（クリップ範囲外の中間点は非破壊で温存され、再び延長すれば復元する）。
- **`ProjectDocument::validate()` 検証**: `clip.midpoints.windows(2).all(|w| w[0] < w[1])`（厳密単調増加・重複なし）を検証し、違反時は `DocumentValidationError::InvalidMidpoints` を返す。

---

## 4. `EditCommand` 拡張仕様（`src/timeline/command.rs`）

### 4.1 新規追加コマンド

```rust
pub enum EditCommand {
    // ... 既存バリアント ...

    /// プロジェクトのタイムライン長（最終フレーム位置）を変更する。
    SetTimelineLength {
        /// 新しいタイムライン長決定ポリシー。
        mode: TimelineLengthMode,
    },

    /// タイムラインの選択範囲（`[` / `]` 出力・操作範囲）を変更または解除する。
    SetSelectionRange {
        /// 新しい半開選択区間 `[start, end)`。`None` で解除。
        range: Option<FrameRange>,
    },

    /// 指定クリップに中間点を追加し、アニメーション済みスロットを同座標へ整列する。
    AddMidpoint {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 追加するエフェクト基準フレーム位置（`eff_start < frame < eff_last` 必須）。
        effect_frame: i64,
    },

    /// 指定クリップ上の既存中間点を新しいエフェクト基準フレームへ移動する。
    MoveMidpoint {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 移動前の中間点エフェクト基準フレーム。
        old_effect_frame: i64,
        /// 移動後の中間点エフェクト基準フレーム。
        new_effect_frame: i64,
    },

    /// 指定クリップから中間点を削除する。
    RemoveMidpoint {
        /// 対象クリップ。
        clip_id: ClipRef,
        /// 削除対象の中間点エフェクト基準フレーム。
        effect_frame: i64,
    },
}
```

- **網羅マッチの同期更新箇所（Phase D-1 対象）**:
  - [`src/timeline/operations.rs`](../../src/timeline/operations.rs): 適用ロジック＋Undo 用 `PatchStep` 実装。
  - [`src/timeline/dispatcher.rs`](../../src/timeline/dispatcher.rs): `describe_command()` の履歴表示文字列追加。
  - [`src/ai/planner.rs`](../../src/ai/planner.rs): `summarize_command()` / `summarize_other_command()` へ新 5 バリアントを追加。
- **Undo/Redo 対応**: 5 コマンドすべてが Undo パッチを生成し、`Ctrl+Z` / `Ctrl+Y` で完全復元する。

### 4.2 複数クリップの一括操作（`EditCommand::Batch` 活用）

新しい専用コマンドを増やさず、既存の `EditCommand::Batch { commands }` を活用して単一 Undo を保証する。

- **複数クリップ一括移動**: 選択中の全クリップに対して `MoveClip` を生成する際、移動先に別の選択クリップが一時的に居座って衝突エラーにならないよう、同一レイヤー上を右方向（`new_start > old_start`）へ移動する場合は後ろのクリップから、左方向へ移動する場合は前のクリップから順列構成するトポロジカル移動順解決ヘルパーを実装する。
- **複数クリップ一括削除 / コピー / 貼り付け / 複製**:
  - `Delete`: 全選択クリップ分の `RemoveClip` を `Batch` 化。
  - `Copy` / `Paste` / `Duplicate`: 既存の [`copy_clips`](../../src/timeline/clipboard.rs#L9) / [`build_collision_free_paste_command`](../../src/timeline/clipboard.rs#L19) / [`build_duplicate_selection_command`](../../src/timeline/clipboard.rs#L69) は**すでに複数クリップ（`&[ClipId]`）に対応済み**であるため、UI 側の単一 `selected_clip_id` 制限を外して連携する。

### 4.3 `Clip.midpoints` と `AnimatedParameter` の同期ポリシー（(a) 強制整列 × AviUtl 全区間共通）

[`AnimatedParameter`](../../src/core/animation.rs#L213) には 4 モード（`Constant` / `Easing` / `Keyframes` / `Expression`）がある。本設計では AviUtl 拡張編集の単一タイムライン分割モデルと AI-Native 構造を両立させるため、**「(a) `Clip.midpoints` と `Keyframes` を 1 対 1 へ強制整列（正規化）する」** 方針および **「移動方法（`EasingCurve`）はスロット全区間で共通」** 方針を採用する。

#### A. 正規化不変条件と共通ヘルパー（`sync_and_align_clip_keyframes`）
- クリップの有効境界グリッドを `grid(clip) = [eff_start, ...visible_clip_midpoints(clip)..., eff_last]`（`clip.duration.get() == 1` の場合のみ `[eff_start]`）と定義する。
- **正規化関数 `sync_and_align_clip_keyframes(clip: &mut Clip, fps: FrameRate)`** は、以下 2 ステップでクリップ内の全アニメーション済みスロットを `grid(clip)` と完全 1 対 1 に揃える：
  1. **未登録中間キーの非破壊収穫（Harvest）**: AI 計画生成（[`src/ai/planner.rs`](../../src/ai/planner.rs)）や旧形式 `.smc.json` に由来する `Keyframes` 内に `clip.midpoints` 未登録の中間時刻 `eff_start < kf.frame && kf.frame < eff_last` が含まれていた場合、切り捨てる前に全スロットからそのフレーム座標を `clip.midpoints` へ昇順マージ（重複除去）して `grid(clip)` を再構築する。
  2. **`grid(clip)` への 1 対 1 リサンプリング整列（Force Align）**:
     - `Constant` / `Expression`：対象外（そのまま維持し、無関係スロットを肥大化させない）。
     - `Easing { curve, extrapolation, .. }`：`visible_clip_midpoints(clip)` が空の間はそのまま維持し、**中間点が 1 点以上存在する場合**は各 `f ∈ grid(clip)` の `param.evaluate(ctx_at(f, fps))` を評価値とし、`easing = curve` を全点へ設定した `Keyframes` へ昇格する。
     - `Keyframes { keyframes, extrapolation }`：全 `f ∈ grid(clip)` について、既存 `keyframes` に同フレームが存在すればその `value`（および既存 `easing`）を継承し、欠落境界（両端 `eff_start` / `eff_last` 欠落や他スロット由来中間点）は `param.evaluate(ctx_at(f, fps))` で実測補間値を算出して埋めた結果へ置き換える。整列後は **`keyframes.len() == grid.len()` かつ全 `i` で `keyframes[i].frame == grid[i]`** が常に成立する。

#### B. 各操作の確定ルールと既存互換境界
1. **`AddMidpoint { clip_id, effect_frame }` 実行時**:
   - **検証**: ロックレイヤー拒否、`eff_start < effect_frame < eff_last` 検証、重複しない位置への昇順挿入。
   - 挿入後、`sync_and_align_clip_keyframes(clip, fps)` を実行して全 `Easing`（昇格）および既存 `Keyframes` を新 `grid(clip)` へ 1 対 1 整列する（`Constant` / `Expression` は維持）。
   - **厳密性の明示**: 元の曲線形状を数式レベルで 100% 無損失維持するのは `Constant` および `Easing(Linear)` であり、非線形カーブ（`EaseIn` / `EaseOut` / `EaseInOut` / `CubicBezier`）は **「境界通過点の値を厳密維持した上で、AviUtl と同様に分割された各区間へ統一 `curve` を再適用する」** 仕様とする。
2. **`MoveMidpoint { clip_id, old_effect_frame, new_effect_frame }` 実行時**:
   - `new_effect_frame` は隣接中間点および `(eff_start, eff_last)` の間に収まること（重複・飛び越し禁止）。
   - `clip.midpoints` の要素更新と同時に、全 `Keyframes` スロットで `frame == old_effect_frame` を持つキーの値を保ったまま `frame = new_effect_frame` へ移動した上で、`grid(clip)` との 1:1 整合を再確認する。
3. **`RemoveMidpoint { clip_id, effect_frame }` 実行時**:
   - `clip.midpoints` から `effect_frame` を削除し、`sync_and_align_clip_keyframes(clip, fps)`（該当フレーム除去＆残り `grid` と 1:1 再確認）を実行する。
4. **移動方法の変更（AviUtl 準拠：全区間共通）とカット済み `Easing` の自動昇格**:
   - UI（設定ダイアログ）でスロットの移動方法（`直線移動` 等）を切り替えた場合、対象スロットの **全 `keyframes[*].easing` を選択カーブで一括更新（全区間共通）** する。
   - **カット済みクリップの `Easing` 境界値編集（自動昇格）**: `SplitClip` / `TrimLeft` / `TrimRight` 直後はカット連続性維持のため `Easing` をそのまま保持するが、ユーザーが右パネルまたはプレビュードラッグで境界値を編集する初回コマンド実行時、自動的に `[eff_start, ...visible..., eff_last]` の現在評価値ベース `Keyframes` へ昇格してから新値を書き込む。
5. **低レベル JSON 往復・既存結合テストとの非破壊両立**:
   - `ProjectDocument::to_json` / `from_json` / `validate()` 自体は低レベルデータ構造の破壊を防ぐため無断リサンプリングを行わず（`validate()` の `InvalidMidpoints` は `clip.midpoints` 自体の厳密昇順検査に限定）、UI 編集・中間点コマンド・AI プラン生成時に `sync_and_align_clip_keyframes` を通すことで、既存 `project_roundtrip` や `cut_preservation` を一切壊さずエディタ上の 1:1 整列を保証する。
6. **中間点あり状態で `Constant` スロットをアニメーション開始した時**:
   - `visible_clip_midpoints(clip).len() > 0` かつ `Constant` のスロットに対して移動方法選択や右欄入力を行った場合、そのスロットも即座に `grid(clip)` へ 1 対 1 整列した `Keyframes` として生成する。

---

## 5. `AppController` 拡張仕様（`src/ui/app_controller.rs`）

[`AppController`](../../src/ui/app_controller.rs#L98) に以下の定数変更および UI 編集状態・メソッドを追加する（すべて純粋 Rust で単体テスト可能）。

- **既存定数調整**:
  - `MIN_RIGHT_PANEL_WIDTH`: `220.0` → **`240.0`**（右パネル設定ダイアログ 2 段コンパクト表示保証、既存ユニットテスト [`right_panel_width_uses_one_fifth_default_and_clamps_bounds`](../../src/ui/app_controller.rs#L1614) も同期更新）。
  - `MIN_TIMELINE_PX_PER_FRAME = 0.2`, `MAX_TIMELINE_PX_PER_FRAME = 32.0` を新設し、[`zoom_timeline`](../../src/ui/app_controller.rs#L785) の `.clamp(0.5, 24.0)` を共用定数に統一。

```rust
/// タイムラインの時間軸表示単位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeDisplayUnit {
    /// 時分秒＋小数/フレーム（例: `00:00:02.86`）。
    #[default]
    Timecode,
    /// 総フレーム番号（例: `173f`）。
    Frames,
}

/// タイムラインのトラック高さ密度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineRowDensity {
    /// 小（22px・AviUtl 既定に近い高密度表示）。
    #[default]
    Compact,
    /// 中（30px）。
    Medium,
    /// 大（44px・波形詳細表示）。
    Large,
}

/// プレビューキャンバス表示倍率モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewZoomMode {
    #[default]
    Fit,
    Percent(u16), // 25, 50, 100, 200
}
```

### 5.1 追加フィールド一覧

| フィールド | 型 | 初期値 | 役割 |
| --- | --- | --- | --- |
| `selected_clip_ids` | `Vec<ClipId>` | `selected_clip_id` と同期維持 | 複数選択対応（主選択は `selected_clip_id` と整合） |
| `selected_segment_index` | `usize` | `0` | 選択クリップで設定ダイアログ表示中の中間点区間番号 |
| `time_display_unit` | `TimeDisplayUnit` | `Timecode` | ルーラー・ヘッダの時刻/フレーム表示切替 |
| `timeline_row_density` | `TimelineRowDensity` | `Compact` (`22.0px`) | タイムライン行高 |
| `timeline_scroll_y_px` | `f32` | `0.0` | 縦スクロール位置同期用 |
| `follow_selected_on_seek` | `bool` | `false` | 再生ヘッド移動時に再生位置上のクリップを自動選択 |
| `show_preview_guides` | `bool` | `true` | バウンディング枠・ハンドル・中心アンカー十字表示 |
| `show_preview_grid` | `bool` | `false` | 中央十字・10 分割グリッド表示 |
| `show_preview_waveform` | `bool` | `true` | プレビュー直下の主音声波形ストリップ表示 |
| `preview_zoom` | `PreviewZoomMode` | `Fit` | プレビュー表示倍率 |
| `scale_aspect_locked` | `bool` | `true` | 設定ダイアログ「拡大率」の縦横連動 ON/OFF |
| `inspector_sections` | `InspectorSectionCollapseState` | 主要展開済み | 標準描画 / 色調補正 / 字幕設定 / 音声再生 / `f(t)` 折りたたみ状態 |
| `key_config` | `KeyConfig` | AviUtl 準拠既定＋衝突なしマップ | メニューから編集・初期化・JSON 保存可能なキーコンフィグ設定 |

### 5.2 新規追加コントローラメソッド一覧

- **選択系**:
  - `select_single_clip(&mut self, clip_id: Option<ClipId>, segment_index: usize)`
  - `toggle_clip_selection(&mut self, clip_id: &ClipId)`（`Ctrl+Click` 用）
  - `select_clips_in_rect(&mut self, min_frame: u64, max_frame: u64, min_layer: usize, max_layer: usize, additive: bool)`
  - `select_all_clips(&mut self)`（`Ctrl+A`）
  - `jump_to_adjacent_clip_or_midpoint(&mut self, forward: bool)`（`,` / `.` キー）
  - `select_clip_at_playhead(&mut self)`（`Tab` キーおよび追従機能）
- **中間点系**:
  - `visible_clip_midpoints(&self, clip: &Clip) -> Vec<i64>`（`eff_start < m && m < eff_last` 抽出）
  - `add_midpoint_at_playhead(&mut self, clip_id: &ClipId) -> Result<(), String>`
  - `move_midpoint(&mut self, clip_id: &ClipId, old_eff: i64, new_eff: i64) -> Result<(), String>`
  - `remove_midpoint(&mut self, clip_id: &ClipId, target_eff: i64) -> Result<(), String>`
- **タイムライン長＆範囲系**:
  - `set_timeline_length_mode(&mut self, mode: TimelineLengthMode) -> Result<(), String>`
  - `fit_timeline_length_to_last_clip(&mut self) -> Result<(), String>`
  - `set_selection_in_at_playhead(&mut self) -> Result<(), String>`（`[`）
  - `set_selection_out_at_playhead(&mut self) -> Result<(), String>`（`]`：`end = playhead + 1`）
  - `clear_selection_range(&mut self) -> Result<(), String>`（`Ctrl+Shift+A` / メニュー）
- **新規オブジェクト・レイヤー操作**:
  - `add_subtitle_clip_at(&mut self, layer_id: Option<&LayerId>, start: FrameIndex, duration: FrameCount, text: &str) -> Result<ClipId, String>`
  - `add_layer_at(&mut self, insert_index: usize) -> Result<LayerId, String>`
  - `remove_layer(&mut self, layer_id: &LayerId) -> Result<(), String>`
  - `toggle_layer_visible(&mut self, layer_id: &LayerId) -> Result<(), String>`
  - `toggle_layer_locked(&mut self, layer_id: &LayerId) -> Result<(), String>`

---

## 6. エラーハンドリング・境界条件

- **旧バージョンプロジェクト読込**: `timeline_length` / `selection_range` / `Clip.midpoints` が JSON に存在しない旧 `.smc.json` はそれぞれ `TimelineLengthMode::AutoToLastClip` / `None` / `Vec::new()` として無エラーで読み込む。
- **ロック中レイヤー保護**: 複数クリップ一括移動・削除・中間点操作のいずれにおいても、対象クリップが属するレイヤーが `locked == true` であればコマンド実行を拒否し、ロールバックしたうえでステータスバーへ通知する。
- **クリップ長不足・境界上の中間点**: `clip.duration.get() < 3` または `eff_frame <= eff_start || eff_frame >= eff_last` の場合、`AddMidpoint` を拒否する（`MoveMidpoint` も隣接中間点または境界と `1f` 未満の間隔になる位置へはクランプ/拒否）。
- **手動長 `Manual { end_frame }` 時の再生/シーク上限**: `seek_upper_bound_frames()` は `Manual { end_frame }` 時も `end_frame` を上限とし（編集したいクリップが後ろにある場合は境界線をドラッグ延長するか `AutoToLastClip` へ戻すことで整合）。
- **`select_clips_in_rect` と `FrameRange` の閉/半開整合**: マーキー選択矩形 `[min_frame, max_frame)` はクリップ `[start, end)` と `start < max_frame && end > min_frame` で重なり判定する。

---

## 7. テスト計画

- **単体テスト（各 `.rs` 末尾）**:
  - `src/timeline/document.rs`: `TimelineLengthMode` / `FrameRange` / `Clip.midpoints` の JSON 往復・旧フォーマット互換読込、`FPS_29_97` 時 `ruler_display_span_frames() == 300`、`seek_upper_bound_frames()`（空 `Auto` 時 `600f`、短尺 `120f` クリップ配置済み `Auto` 時も `600f`、手動長 `Manual { end_frame: 240 }` 時 `240f`）、`InvalidSelectionRange` (`start >= end`)・重複 `midpoints` 検証拒否。
  - `src/timeline/operations.rs`: 5 つの新規 `EditCommand`（`SetTimelineLength`, `SetSelectionRange`, `AddMidpoint`, `MoveMidpoint`, `RemoveMidpoint`）の適用＋Undo/Redo 往復、`Linear` 無損失変換・非線形 `Easing` 境界値一致、`sync_and_align_clip_keyframes` による両端欠落／任意フレームキーの収穫＋1:1 強制整列テスト、`SplitClip` 時の中間点保持テスト。
  - `src/ai/planner.rs`: 新規 `EditCommand` バリアントを含む JSON パース・`dry_run_validate`・サマリ生成・AI 生成キーフレームの `Clip.midpoints` 整列テスト。
  - `src/ui/app_controller.rs`: 空時＆短尺クリップ時 `AutoToLastClip` `seek_to_frame`、複数選択・マーキー選択・一括隣接移動、`[` / `]`（同一フレーム `[f, f+1)`）、`MIN_RIGHT_PANEL_WIDTH = 240.0` 更新済みクランプテスト。
- **結合テスト（`tests/phase_d_timeline_model.rs` + `tests/phase_d_timeline_model/mod.rs`）**:
  - 空プロジェクトで `120f` へシーク → タイムライン手動長変更 → 字幕追加 → `Constant` のまま中間点 2 箇所追加 → `Easing` 編集 → `SplitClip` 分割 → `Undo` 全戻し一貫性テスト。
