# [002-01] データモデル・編集コマンド・コントローラ拡張仕様

- **ステータス**: Draft
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **関連ドキュメント**:
  - 概要設計書: [`README.md`](README.md)
  - タイムライン仕様: [`02-timeline-spec.md`](02-timeline-spec.md)
  - 設定ダイアログ仕様: [`03-inspector-dialog-spec.md`](03-inspector-dialog-spec.md)
  - 既存実装: [`src/timeline/document.rs`](../../src/timeline/document.rs), [`src/timeline/command.rs`](../../src/timeline/command.rs), [`src/ui/app_controller.rs`](../../src/ui/app_controller.rs)

---

## 1. 概要・目的

AviUtl 酷似のタイムライン操作・設定ダイアログ操作を実現するため、既存の純粋 Rust ドメインモデル（[`ProjectDocument`](../../src/timeline/document.rs)、[`EditCommand`](../../src/timeline/command.rs)）および UI 統括コントローラ（[`AppController`](../../src/ui/app_controller.rs)）に必要な拡張を定義する。

設計上の最重要原則は以下の 3 点とする。
1. **後方互換性維持**: 既存の `*.smc.json` プロジェクトファイルを破壊せず、`#[serde(default)]` で新フィールドを安全に読み込めること。
2. **AI スキーマ整合（`schemars::JsonSchema`）**: すべての [`ProjectDocument`](../../src/timeline/document.rs) / [`EditCommand`](../../src/timeline/command.rs) 拡張型に `#[derive(JsonSchema)]` を実装し、AI チャット・MCP サーバーからも「タイムライン長の変更」や「中間点操作」を一貫して発行できること。
3. **カット連続性・丸め誤差ゼロの不変条件維持**: 既存の `FrameIndex` / `FrameCount` / `effect_offset_frames` / `effect_duration_frames` / `source_in` / `PlaybackRate` 計算則を一切崩さないこと。

---

## 2. スコープ

### 対象（In Scope）
- `ProjectDocument` へのタイムライン長設定（自動追従／手動固定）・選択範囲（`[` `]`）の追加仕様。
- `EditCommand` へのタイムライン長設定コマンド・一括移動/削除のバッチ運用・中間点同期ヘルパー仕様。
- `AppController` への複数クリップ選択、中間点区間インデックス、時間表示単位（時分秒/フレーム）、タイムライン表示設定（行高さ・自動追従・領域外表示）追加仕様。

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
    /// 配置されている全クリップの最大終端フレームへ自動追従する（空時は最小 1 基準尺、例: 300f）。
    #[default]
    AutoToLastClip,
    /// ユーザーが指定した最終フレーム位置（1 フレーム以上）で固定する。
    /// クリップがその終端を超えて配置されていても、最終フレーム境界線までは独立保持する
    /// （再生・書き出し範囲の上限となる）。
    Manual {
        /// プロジェクトの総フレーム数（最終フレーム境界）。
        end_frame: FrameCount,
    },
}

/// タイムライン上の連続フレーム区間 `[start, end)`（`start < end`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FrameRange {
    pub start: FrameIndex,
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

    /// `[` / `]` 等で指定された出力・範囲操作用の選択区間（未指定時はプロジェクト全体）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_range: Option<FrameRange>,
}
```

### 3.2 実効長計算メソッドと既存 API との整合

| メソッド | 振る舞い | 用途 |
| --- | --- | --- |
| `clips_end_frame(&self) -> u64` | 全クリップの `max(timeline_end)`（空なら `0`） | 既存計算の明示化・「最後のオブジェクト位置を最終フレーム」算出 |
| `total_duration_frames(&self) -> u64` | ・`AutoToLastClip`: `self.clips_end_frame()`<br>・`Manual { end_frame }`: `end_frame.get()` | 再生上限・書き出し総フレーム数・最終フレーム縦線位置 |
| `ruler_display_span_frames(&self) -> u64` | `self.total_duration_frames().max(self.clips_end_frame()).max(u64::from(self.fps.numerator()) * 10)` | 空プロジェクトでも最低 10 秒分の目盛り・スクロール領域を確保 |

> [!NOTE]
> `AutoToLastClip` 時にクリップが 0 個の場合、`total_duration_frames()` は既存テストとの互換のため `0` を返す。UI 描画上の作業キャンバス幅は `ruler_display_span_frames()` によって常に 10 秒以上確保され、ユーザーは空タイムラインでも `0..600f` へシーク・オブジェクト配置・最終フレームドラッグができる（最終フレームをドラッグした瞬間に `Manual { end_frame }` へ切り替わる）。

### 3.3 `selection_range` の不変条件

- `ProjectDocument::validate()` において、`selection_range` が `Some(r)` の場合は **`r.start < r.end`** を検証する（違反時は `DocumentValidationError`）。
- `[` キーで現在再生位置を `start` に、`]` キーで現在再生位置を `end` に設定（`start >= end` になる場合は反対端を自動調整または 1f 確保）。`Ctrl+A` または右クリック「選択範囲を解除」で `None` へ戻す。

---

## 4. `EditCommand` 拡張仕様（`src/timeline/command.rs`）

### 4.1 新規追加コマンド

```rust
pub enum EditCommand {
    // ... 既存バリアント ...

    /// プロジェクトのタイムライン長（最終フレーム位置）を変更する。
    SetTimelineLength {
        mode: TimelineLengthMode,
    },

    /// タイムラインの選択範囲（`[` / `]` 出力範囲）を変更または解除する。
    SetSelectionRange {
        range: Option<FrameRange>,
    },
}
```

- **Undo/Redo 対応**: `SetTimelineLength` および `SetSelectionRange` はプロジェクト状態の一部として [`CommandDispatcher`](../../src/timeline/dispatcher.rs) の履歴スタックに積み、逆パッチで元に戻せるようにする。
- **「最後のオブジェクト位置を最終フレームにする」操作**:
  - タイムライン右クリック → `範囲設定` → `最後のオブジェクト位置を最終フレーム` を実行した際は、現在の `clips_end_frame().max(1)` を用いて `EditCommand::SetTimelineLength { mode: TimelineLengthMode::Manual { end_frame } }` または `AutoToLastClip` へ切り替えられる（UI では「最後のオブジェクト位置に合わせる（固定）」「クリップ末尾へ自動追従」の両方を提供）。

### 4.2 複数クリップの一括操作（`EditCommand::Batch` 活用）

新しいコマンド型を増やさず、既存の `EditCommand::Batch { commands }` を活用して単一 Undo を保証する。

- **複数クリップ一括移動**: 選択中の全クリップに対して `MoveClip` を生成。ただし移動先に元の他の選択クリップが居座る順で移動すると一時衝突エラーになるため、**移動方向に応じたトポロジカル順（右へ動かす場合は後ろのクリップから、左へ動かす場合は前のクリップから、別レイヤーへ移す場合は移動先計算後）で `MoveClip` を並べる** か、衝突検査をバッチ最終状態または一時退避で行う仕様とする（Phase D-1 で `dispatcher` の一括移動順決定ヘルパーを実装）。
- **複数クリップ一括削除 / コピー / 貼り付け / 複製**:
  - `Delete`: 全選択クリップ分の `RemoveClip` を `Batch` 化。
  - `Copy` / `Paste` / `Duplicate`: 既存の [`copy_clips`](../../src/timeline/clipboard.rs#L9) / [`build_collision_free_paste_command`](../../src/timeline/clipboard.rs#L19) / [`build_duplicate_selection_command`](../../src/timeline/clipboard.rs#L69) は**すでに複数クリップ（`&[ClipId]`）に対応済み**であるため、UI 側の単一 `selected_clip_id` 制限を外すだけでそのまま動作する。

### 4.3 AviUtl「中間点」と `AnimatedParameter::Keyframes` の同期仕様

AviUtl ではオブジェクト上に **「中間点」** を打つと、オブジェクト全体のアニメーション可能パラメータがその時刻で区切られる。`sea-movie-creator` ではパラメータごとに独立した [`AnimatedParameter`](../../src/core/animation.rs#L213) を持つため、以下の規約で両者をシームレスに結びつける。

1. **クリップ内中間点コレクションの導出（仮想中間点統合モデル）**:
   - クリップ内の全アニメーションスロット（`x`, `y`, `scale_x`, `scale_y`, `rotation_deg`, `anchor_x`, `anchor_y`, `opacity`, `brightness`, `contrast`, `saturation`, `hue_deg`, `gamma`, `font_size`, `gain_db`, `pan`）から、`AnimatedParameter::Keyframes` に設定されている **ユニークかつソート済みのフレーム座標列 `M = [f_0, f_1, ..., f_n]`** を抽出する。
   - クリップの有効範囲に対応する開始エフェクトフレーム `eff_start = clip.effect_offset_frames` と終了エフェクトフレーム `eff_end = eff_start + clip.duration.get() as i64` を両端とし、`eff_start < f < eff_end` に存在する内点 `f` をタイムライン上で **「中間点 `|`」** として描画する。
2. **「中間点を追加 (`P` キー / `+` ボタン)」の挙動**:
   - 対象クリップにおいて再生ヘッドが `start < playhead < end` にあるとき、`eff_frame = effect_frame_at_playhead(clip, playhead)` を算出。
   - 現在すでに `Keyframes` モードであるスロット群（および主要空間スロット `x`, `y`, `scale_x`, `scale_y`, `rotation_deg`, `opacity` のうちユーザーが中間点分割対象とするもの、または現在 `Keyframes` 化済みスロット＋未設定なら全基本描画スロットの現在値スナップショット）に対し、**`eff_start`・`eff_frame`・`eff_end` にキーフレームを挿入** する `EditCommand::SetParameter` 群を 1 つの `EditCommand::Batch` として実行する。
   - これにより、既存の値が一切ジャンプせず（各時刻の評価値をそのまま固定・補間維持）、即座に中間点の前後区間を独立編集できる。
3. **「中間点の移動」の挙動**:
   - タイムライン上で中間点縦線（または設定ダイアログ上の中間点マーカー）を `old_eff_frame` から `new_eff_frame` へドラッグすると、そのクリップ内で `frame == old_eff_frame` を持つすべての `Keyframes` スロットのキーフレーム時刻を `new_eff_frame` へ一括更新（`Batch`）する。
   - `new_eff_frame` は隣接中間点およびクリップ境界 `[eff_start + 1, eff_end - 1]` 内にクランプし、他の中間点と重ならないように制御する。
4. **「中間点の削除」の挙動**:
   - 指定した中間点 `target_eff_frame` に対応する `Keyframe` を各 `Keyframes` スロットから削除し、内点が 0 個になった場合は任意で `Constant` または `Easing` へ整理可能とする（2 点のみ残った場合は `Easing` 互換として扱える）。
5. **設定ダイアログの区間選択**:
   - タイムライン上でクリックされた位置、または設定ダイアログの `◀` / `▶` ボタンにより、区間インデックス `selected_segment_index`（`0..segment_count`）が決まる。
   - 設定ダイアログの各パラメータ行の左欄は「区間開始点 `seg_start` での値（または `Keyframe`）」、右欄は「区間終了点 `seg_end` での値（または `Keyframe`）」を編集する。

---

## 5. `AppController` 拡張仕様（`src/ui/app_controller.rs`）

[`AppController`](../../src/ui/app_controller.rs#L98) に以下の UI 編集状態・メソッドを追加する（すべて純粋 Rust で単体テスト可能）。

```rust
/// タイムラインの時間軸表示単位（AviUtl 環境設定相当）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeDisplayUnit {
    /// 時分秒＋フレーム小数（例: `00:00:02.86` 形式、または `00:00:02:51`）。
    #[default]
    Timecode,
    /// 総フレーム番号（例: `173f`）。
    Frames,
}

/// タイムラインのトラック高さ密度（AviUtl レイヤー「サイズ変更」相当）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineRowDensity {
    /// 小（22px・AviUtl 既定に近い高密度表示）。
    #[default]
    Compact,
    /// 中（30px・波形やラベルを少し見やすく）。
    Medium,
    /// 大（44px・波形詳細確認用）。
    Large,
}

/// プレビューキャンバス表示倍率モード。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PreviewZoomMode {
    #[default]
    Fit,
    Percent(u16), // 25, 50, 100, 200
}
```

### 5.1 追加フィールド一覧

| フィールド | 型 | 初期値 | 役割 |
| --- | --- | --- | --- |
| `selected_clip_ids` | `Vec<ClipId>` | 既存 `selected_clip_id` を先頭統合 | 複数選択対応（`primary_selected_clip_id()` は最後の選択または先頭） |
| `selected_segment_index` | `usize` | `0` | 中間点分割済みクリップで現在編集中の区間番号 |
| `time_display_unit` | `TimeDisplayUnit` | `Timecode` | ルーラーおよびヘッダの時刻/フレーム表示切替 |
| `timeline_row_density` | `TimelineRowDensity` | `Compact` (`22.0px`) | タイムライン行高 |
| `timeline_scroll_y_px` | `f32` | `0.0` | 縦スクロール位置同期用 |
| `follow_selected_on_seek` | `bool` | `false` | AviUtl「選択オブジェクトの追従」：シーク時に再生位置上のクリップを自動選択 |
| `show_preview_guides` | `bool` | `true` | バウンディングボックス・中心アンカー十字表示 |
| `show_preview_grid` | `bool` | `false` | 中央十字・10分割グリッド表示 |
| `show_preview_waveform` | `bool` | `true` | プレビュー直下の主音声波形ストリップ表示 |
| `preview_zoom` | `PreviewZoomMode` | `Fit` | プレビュー表示倍率 |
| `scale_aspect_locked` | `bool` | `true` | 設定ダイアログ「拡大率」の縦横連動 ON/OFF |
| `inspector_sections` | `InspectorSectionCollapseState` | 主要展開済み | 標準描画 / 色調補正 / 字幕 / 音声 / `f(t)` 折りたたみ状態 |

### 5.2 新規追加コントローラメソッド一覧

- **選択系**:
  - `select_single_clip(&mut self, clip_id: Option<ClipId>, segment_index: usize)`
  - `toggle_clip_selection(&mut self, clip_id: &ClipId)`（`Ctrl+Click` 用）
  - `select_clips_in_rect(&mut self, min_frame: u64, max_frame: u64, min_layer: usize, max_layer: usize, additive: bool)`（マーキー囲み選択用）
  - `select_all_clips(&mut self)`（`Ctrl+A`）
  - `jump_to_adjacent_clip_or_midpoint(&mut self, forward: bool)`（`,` / `.` キー）
  - `select_clip_at_playhead(&mut self)`（`Tab` キーおよび追従機能）
- **中間点系**:
  - `clip_midpoint_effect_frames(&self, clip_id: &ClipId) -> Vec<i64>`
  - `add_midpoint_at_playhead(&mut self, clip_id: &ClipId) -> Result<(), String>`
  - `move_midpoint(&mut self, clip_id: &ClipId, old_eff: i64, new_eff: i64) -> Result<(), String>`
  - `remove_midpoint(&mut self, clip_id: &ClipId, target_eff: i64) -> Result<(), String>`
- **タイムライン長＆範囲系**:
  - `set_timeline_length_mode(&mut self, mode: TimelineLengthMode) -> Result<(), String>`
  - `fit_timeline_length_to_last_clip(&mut self) -> Result<(), String>`
  - `set_selection_in_out(&mut self, set_start: bool, frame: FrameIndex) -> Result<(), String>`（`[` / `]`）
  - `clear_selection_range(&mut self) -> Result<(), String>`
- **新規オブジェクト生成系（空欄右クリック追加）**:
  - `add_subtitle_clip_at(&mut self, layer_id: Option<&LayerId>, start: FrameIndex, duration: FrameCount, text: &str) -> Result<ClipId, String>`
  - `add_layer_at(&mut self, insert_index: usize) -> Result<LayerId, String>`（上に挿入／下に挿入）
  - `remove_layer(&mut self, layer_id: &LayerId) -> Result<(), String>`
  - `toggle_layer_visible(&mut self, layer_id: &LayerId) -> Result<(), String>`
  - `toggle_layer_locked(&mut self, layer_id: &LayerId) -> Result<(), String>`

---

## 6. エラーハンドリング・境界条件

- **旧バージョンプロジェクト読込**: `timeline_length` / `selection_range` が JSON に存在しない旧ファイルは `TimelineLengthMode::AutoToLastClip` / `None` として無エラーで読み込む。
- **ロック中レイヤー保護**: 複数クリップ一括移動・削除・中間点編集のいずれにおいても、対象クリップが属するレイヤーが `locked == true` であればコマンド実行を拒否し、ロールバックしたうえでステータスメッセージへ明確な理由を表示する。
- **クリップ長より外の中間点**: カット分割・左端トリム等で既存キーフレームがクリップ範囲外に存在しても [`AnimatedParameter::Keyframes`](../../src/core/animation.rs#L233) 評価としては有効（カット連続性維持）だが、タイムライン UI 上に「中間点マーカー `|`」として描画・編集対象とするのは `clip_start < timeline_frame < clip_end` の範囲内のみとする。
- **手動画面長より後ろへのシーク**: `Manual { end_frame }` が設定されている場合、`seek_to_frame()` の上限クランプは `document.total_duration_frames().max(document.clips_end_frame())` または `Manual` 上限のいずれとするかについては、**AviUtl 準拠で「再生・シーク上限は `total_duration_frames()`（最終フレーム）まで」** とし、それより後ろのクリップを編集したい場合は最終フレームを延ばす（またはドラッグ時に自動確認/伸長オプションを提供）ことで整合を保つ。

---

## 7. テスト計画

- **単体テスト（各 `.rs` 末尾）**:
  - `src/timeline/document.rs`: `TimelineLengthMode::AutoToLastClip` / `Manual` の JSON 往復・旧フォーマット互換読込・`FrameRange` 不正範囲（`start >= end`）検証拒否テスト。
  - `src/timeline/operations.rs`: `SetTimelineLength` / `SetSelectionRange` の適用および Undo/Redo 往復テスト。
  - `src/ui/app_controller.rs`: 複数選択・マーキー範囲選択・一括移動（重複しない順序解決）・中間点追加/移動/削除の連続性（追加前後で任意時刻の評価値が変化しないこと）テスト。
- **結合テスト（`tests/phase_d_timeline_model.rs` + `tests/phase_d_timeline_model/mod.rs`）**:
  - 空プロジェクトでの時間長変更 → 字幕追加 → 中間点 2 箇所追加 → カット分割 → Undo/Redo 一貫性テスト。
