# [017] エディタ UX 改善設計書（タイムライン再生位置追随・プレビュー画面外カラー＆境界・JSON 設定ハイライト）

- **ステータス**: Implemented
- **作成日**: 2026-11-24
- **更新日**: 2026-11-24
- **関連ドキュメント**:
  - 拡張編集タイムライン UI/UX 詳細仕様（完了済み）: [`002-aviutl-ui-redesign/02-timeline-spec.md`](002-aviutl-ui-redesign/02-timeline-spec.md)
  - プレビュー直接操作・メニューバー・ショートカット詳細仕様（完了済み）: [`002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md`](002-aviutl-ui-redesign/04-preview-and-shortcuts-spec.md)
  - AI チャット UI 改修・VS Code 風設定画面・起動時状態復元・モデル管理・トークン残量表示設計書（完了済み）: [`013-ai-ui-settings-and-usage.md`](013-ai-ui-settings-and-usage.md)

---

## 1. 概要・目的

- **この機能で解決したい課題・達成したいこと**:
  動画編集エディタ本体（タイムライン・メインプレビュー・設定画面）における日常操作の視認性と作業効率を改善するため、以下 3 点の改修を行います。
  1. **タイムラインの再生位置（プレイヘッド）追随スクロール＋設定維持**:
     ズームインした状態で再生・シークを行うとプレイヘッドがタイムライン表示範囲の右端外へ消えてしまい、現在の編集箇所を追いづらくなります。ページ送り方式と中央固定方式を切り替えられる追随スクロール機能を追加し、アプリ再起動後も前回の追随モードを維持します。
  2. **ダークモードにおけるメイン動画プレビュー画面外（背景）カラーと境界の視認性向上**:
     現在のプレビュー周辺背景はほぼ黒（`rgba(0x0b0e_14ff)`）であり、さらにキャンバス枠線描画後に動画画像が上書きされるため、暗い動画や黒背景シーンで「どこまでが動画の画面内で、どこからが画面外か」が判別しづらくなっています。画面外を分かりやすいグレーに変更し、境界枠線の描画順とクリッピングも是正します。
  3. **AI 設定モーダル JSON 直接編集ビューのシンタックスハイライト**:
     現在の JSON 設定編集画面（`{ } JSON` モード）はプレーンなテキストエリアのため、キー・値・括弧の対応が見づらく編集ミスが起きやすい状態です。導入済み `gpui-kit` の `Editor` コンポーネント（`tree-sitter-json`）を活用し、シンタックスハイライト＋行番号付きのエディタへ刷新します。
- **対象ユーザー / 利用シーン**:
  - 長尺または高倍率ズームのタイムラインを再生しながら字幕・カット点を細かく確認したい編集者。
  - ダークモードでテロップや画像オブジェクトを画面端ギリギリ・画面外へ配置調整する編集者。
  - `ai_settings.json` を直接編集・微調整する上級ユーザー。

### 1.1 現状の調査結果（実コード確認）

1. **タイムライン「追従」とスクロール・スクラブの現状**:
   - [`src/ui/timeline/mod.rs`](../../src/ui/timeline/mod.rs) の `tl-follow-toggle`（キー `timeline.follow`＝「追従」）は [`AppController::follow_selected_on_seek`](../../src/ui/app_controller.rs)（インスペクタで選択するクリップを再生位置上のクリップへ切り替える機能）であり、水平スクロール位置 `timeline_scroll_x_frames` を自動スクロールする機能は未実装でした。
   - ズーム倍率は `AppController::timeline_px_per_frame` で保持され、最大スクロール量は `timeline/mod.rs` 内で [`document.ruler_display_span_frames()`](../../src/timeline/document.rs) から都度算出されており、`AppController` 側の共通メソッドはまだありませんでした。
   - また、ルーラー上のドラッグ操作（`TimelinePointerGesture::RulerScrub`）はドラッグ中に直接 `seek_to_frame()` を呼んでいたため、スクラブ中に追随スクロールが動くとカーソル下のフレームが逃げる点に配慮が必要でした。
2. **プレビュー画面外カラー＆枠線描画順の現状**:
   - [`src/ui/preview/panel.rs`](../../src/ui/preview/panel.rs) のルート背景およびビューポート背景が `rgba(0x0b0e_14ff)` になっていました。
   - `paint_preview_viewport_canvas` は `cx` を受け取らない関数であり、`canvas_bounds` に `Edges::all(px(1.0))` の枠線付き `quad` を描いた**直後**に `window.paint_image(viewport_bounds, canvas_bounds, ...)` を同じ `canvas_bounds` へ描画するため、内側の 1px 枠線が動画フレームによって完全に塗り潰されていました。また `window.paint_quad` 自体にはクリップ矩形引数がないため、ズーム時のクリップ配慮も必要でした。
3. **JSON エディタの現状**:
   - [`src/ui/ai_settings_modal/json_view.rs`](../../src/ui/ai_settings_modal/json_view.rs) および [`actions.rs`](../../src/ui/ai_settings_modal/actions.rs) では `Textarea` / `TextareaState` を使用していました。一方 [`Cargo.toml`](../../Cargo.toml) で利用中の `gpui-kit = "0.9.1"` には、`tree-sitter` feature（最小依存の `tree-sitter 0.26.13` + `tree-sitter-json 0.24.8` のみ）を有効化することで使える `Editor` / `EditorState::language("json")` が同梱されていることを確認済みです。

---

## 2. スコープ

> [!NOTE]
> **用語区分（リポジトリ規約準拠）**:
> 本設計書で変更する項目はすべて `sea-movie-creator` 本体の UI・コントローラを構成する **機能モジュール（Feature Module）**（`src/core/`, `src/ui/*`, `lang/*`, `Cargo.toml`）です。

### 対象（In Scope）

- [`src/ui/editor_prefs.rs`](../../src/ui/editor_prefs.rs)・[`src/ui/app_controller.rs`](../../src/ui/app_controller.rs):
  - `TimelinePlayheadFollowMode`（`Off` / `Page` / `Center`）追加、`timeline_max_scroll_frames(&self)` 共通化、水平自動スクロール計算（高ズーム保護含む）、ルーラースクラブ分離（`scrub_to_frame`）、手動水平スクロール時の一時休止＆復帰ロジック。
  - [`src/core/mod.rs`](../../src/core/mod.rs) の `user_config_file_path` / `user_config_read_path` を用いた `settings/editor_prefs.json` による `playhead_follow_mode` 永続化・起動時復元。
- [`src/ui/timeline/mod.rs`](../../src/ui/timeline/mod.rs)・[`src/ui/timeline/ruler.rs`](../../src/ui/timeline/ruler.rs)・[`src/ui/timeline/context_menu.rs`](../../src/ui/timeline/context_menu.rs)・[`src/ui/menu_bar.rs`](../../src/ui/menu_bar.rs)・[`src/ui/window.rs`](../../src/ui/window.rs):
  - タイムラインヘッダ・右クリックメニュー・表示メニューへの「再生追従」モード切替 UI 追加。
  - 既存の「追従」（`follow_selected_on_seek`）のラベルを「選択追従」（`Follow Clip`）へ明確化し機能混同を防止。
- [`src/ui/preview/panel.rs`](../../src/ui/preview/panel.rs): `PreviewCanvasDrawPayload` への `is_dark: bool` 受け渡し、ダーク／ライトモード別の分かりやすいグレー画面外背景色適用、および `paint_preview_viewport_canvas` での `window.with_content_mask` 付き境界枠線描画順修正。
- [`Cargo.toml`](../../Cargo.toml): `gpui-kit` の `tree-sitter` feature 有効化。
- [`src/ui/ai_settings_modal/mod.rs`](../../src/ui/ai_settings_modal/mod.rs)・[`actions.rs`](../../src/ui/ai_settings_modal/actions.rs)・[`json_view.rs`](../../src/ui/ai_settings_modal/json_view.rs)・[`src/ui/window.rs`](../../src/ui/window.rs): `TextareaState` / `Textarea` から `EditorState` / `Editor`（`language("json")`）への移行、フォーカス判定互換・`Escape` キー競合防止。
- [`lang/ja.json`](../../lang/ja.json)・[`lang/en.json`](../../lang/en.json): タイムライン追従モード・選択追従名称等の翻訳キー追加／更新。

### 対象外（Out of Scope）

- AI チャット側のステータス・思考・質問早期表示（`015-ai-activity-ui-and-early-ask.md`）。
- プレビューの背景チェッカーボード（透過確認用）レンダリングやカラーグレーディングスコープ。
- JSON スキーマ補完（LSP）やマルチファイルコードエディタ機能。

---

## 3. 要件・制約

### 機能要件

- **FR-1（タイムライン再生位置追随スクロール・2 モード切替＋設定永続化）**:
  - 再生位置（プレイヘッド）の追随スクロールとして **`Page`（ページ送り）**・**`Center`（中央固定）**・**`Off`** の 3 状態を切り替えられること（初回既定は `Page`）。
  - 前回選択した `playhead_follow_mode` を `settings/editor_prefs.json` へ保存し、アプリ再起動後も維持すること。
  - **`Page` モード**: プレイヘッドが現在表示範囲の左端より左（`head < scroll_x`）または右端付近（表示幅の `90%` 位置）に達したとき、プレイヘッドが左端から約 `12%` の位置になるよう `timeline_scroll_x_frames` を自動スクロールすること。
  - **`Center` モード**: 再生中および明示シーク時、プレイヘッドがビューポート中央（`50%` 位置）に維持されるよう `timeline_scroll_x_frames` を連続追随すること（先頭付近は `0` でクランプ）。
  - **ルーラースクラブとの非干渉**: `TimelinePointerGesture::RulerScrub` によるドラッグ中は追随ジャンプを起こさず、カーソル下のフレーム操作を妨げないこと。
  - **手動スクロールとの協調**: 再生中にユーザーが水平ホイール、水平スクロールバー、または中ボタンドラッグでタイムラインをスクロールした場合、現在の一時的な手動確認が勝手に引き戻されないよう追随を一時休止し、再生開始・明示シーク・または追従モードボタン再操作時に追随を再開すること。再生中のズーム操作（`set_timeline_zoom_ratio_anchored`）では一時休止扱いにしないこと。
  - 既存の `follow_selected_on_seek`（「追従」）と区別しやすいよう、既存ボタンを「選択追従」（`Follow Clip`）、新ボタンを「再生追従: ページ / 中央 / OFF」として提供すること。
- **FR-2（メインプレビュー画面外グレー＆境界線視認性）**:
  - ダークモード時、動画プレビューキャンバスの周囲（画面外エリア）は動画の黒（`#000000`）と明確に区別できる落ち着いたニュートラルグレー（`#2B303B`）で描画すること（ライトモード時は `#D4D8E0`）。
  - 動画画像が真っ黒であっても端が 100% 判別できるよう、動画画像描画**後**にキャンバス外周 1px 外側へ境界枠線を描画し、ズーム 200% 時等もビューポート矩形外へはみ出さないようクリップすること。
- **FR-3（設定 JSON 編集のシンタックスハイライト）**:
  - AI 設定モーダルの `{ } JSON` ビューにて、JSON のキー・文字列・数値・真偽値・`null`・括弧がテーマに合わせて色分け表示され、行番号・自動括弧閉じ・既存ボタン（「整形」「再読込」「保存」）がすべて動作すること。
  - JSON エディタへフォーカスがある間は、エディタ本体の再生ショートカット（`Space`, `Delete` 等）が誤発火せず、また検索バーとの `Escape` キー競合を起こさないこと。

### 非機能要件（互換性・性能・ライセンス）

- **ライセンス・ビルド環境（`deny.toml` 準拠）**:
  - `gpui-kit` の `tree-sitter` feature 追加により `tree-sitter` と `tree-sitter-json`（C コードを `cc` クレートでコンパイル）が依存に加わるため、CI/ビルド環境の C コンパイラ前提および `cargo deny check`（特にライセンス・`multiple-versions` 警告）の通過を確認すること。
- **軽量性**:
  - `ensure_playhead_visible_in_timeline` は単純な算術演算のみで O(1) とし、`~60Hz` の再生ティックへ負荷を与えない。
  - `paint_preview_viewport_canvas` の枠線描画は `with_content_mask` 内の 1 回の `paint_quad` 追加のみで描画負荷は実質ゼロ。
- **安全性**:
  - `unsafe` 不使用（`unsafe_code = "deny"` 維持）。

---

## 4. アーキテクチャ / 設計詳細

### 4.1 コンポーネント A: タイムライン再生位置追随スクロール＆永続化

#### データ構造（[`src/ui/editor_prefs.rs`](../../src/ui/editor_prefs.rs)・`settings/editor_prefs.json`）
```rust
/// タイムライン再生位置（プレイヘッド）の自動追随スクロール方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelinePlayheadFollowMode {
    /// 追随しない。
    Off,
    /// 右端付近（90%）到達時に左端付近（12%）へページ送り（既定）。
    #[default]
    Page,
    /// プレイヘッドをタイムライン表示中央（50%）へ常時追随。
    Center,
}

/// エディタ UI の操作設定永続化データ（`settings/editor_prefs.json`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorPreferences {
    /// タイムライン再生位置の自動追随スクロール方式。
    pub playhead_follow_mode: TimelinePlayheadFollowMode,
}
```
`AppController` 追加フィールド：
- `pub playhead_follow_mode: TimelinePlayheadFollowMode`（起動時に `settings/editor_prefs.json` から読込、未作成時は `Page`）
- `pub playhead_follow_paused_by_manual_scroll: bool`（既定 `false`）
- `pub editor_prefs_path: Option<PathBuf>`（ライブ起動時は `user_config_file_path("editor_prefs.json")`、テスト時は任意注入可）

#### スクロール計算ロジック（実コードフィールド準拠＆高ズーム保護）
`viewport_w = self.timeline_viewport_canvas_width_px.max(320.0)`、`ppf = self.timeline_px_per_frame.max(0.01)`、`visible_frames = (viewport_w / ppf).max(1.0)`、`head = self.clock.current_frame().get()`、`max_scroll = self.timeline_max_scroll_frames()` とする：
- `timeline_max_scroll_frames(&self) -> u64`:
  - `timeline/mod.rs` の既存スパン算出と統一しつつ、1 画面に全フレームが収まっている場合は `0`（スクロール不要）となるよう算出する：
    ```rust
    let span = self.document.ruler_display_span_frames();
    if span <= visible_u64 {
        0
    } else {
        span.saturating_sub(visible_u64 / 2)
    }
    ```
- **`Page` モード（超高倍率ズーム保護付き）**:
  - `right_trigger_offset = (visible_frames * 0.90).floor().max(1.0) as u64`
  - `right_trigger = scroll_x.saturating_add(right_trigger_offset)`
  - `if head < scroll_x || head >= right_trigger`:
    - `raw_lead = (visible_frames * 0.12).floor() as u64`
    - `lead = raw_lead.min(right_trigger_offset.saturating_sub(1))`（`visible_frames` が極小でも毎フレーム再トリガしない）
    - `self.timeline_scroll_x_frames = head.saturating_sub(lead).min(max_scroll)`
- **`Center` モード**:
  - `half = (visible_frames * 0.50).floor() as u64`
  - `self.timeline_scroll_x_frames = head.saturating_sub(half).min(max_scroll)`
- **ルーラースクラブ分離・手動スクロール一時休止・ズーム操作**:
  - `seek_to_frame(frame)`（再生シーク／ショートカット移動／ボタン移動等）は `follow = true` で追随を実行する。
  - `scrub_to_frame(frame)`（[`src/ui/timeline/ruler.rs`](../../src/ui/timeline/ruler.rs) / [`src/ui/timeline/mod.rs`](../../src/ui/timeline/mod.rs) の `TimelinePointerGesture::RulerScrub` ドラッグ中）は追随ジャンプを行わない。
  - ユーザーが水平ホイール・スクロールバー操作・中ボタンドラッグで手動スクロールした際、再生中であれば `playhead_follow_paused_by_manual_scroll = true` にする。再生中のズーム操作（`set_timeline_zoom_ratio_anchored`）では休止フラグを立てない。
  - `tick_playback` でフレーム更新された際は `!playhead_follow_paused_by_manual_scroll` 時のみ追随を実行し、再生開始時・明示 `seek_to_frame` 時・追従モード変更時は `playhead_follow_paused_by_manual_scroll = false` へ戻す。

#### UI 配置（`src/ui/timeline/mod.rs`・`context_menu.rs`・`menu_bar.rs`）
- タイムラインヘッダ：
  - 既存 `tl-follow-toggle`：ラベルを `tr("timeline.follow_clip")`（「選択追従」 / `"Follow Clip"`）へ変更。
  - 新設 `tl-follow-playhead-mode`：アクティブ表示（`Off` 以外でハイライト）＋クリックで `Page` → `Center` → `Off` の順にサイクル切替＆`editor_prefs.json` 保存。表示ラベル：`再生追従: ページ` / `再生追従: 中央` / `再生追従: OFF`（英語：`Playhead: Page` / `Playhead: Center` / `Playhead: Off`）。
- タイムライン右クリックメニュー & 上部「表示」メニュー：同様に選択・切り替えできる項目を配置。

### 4.2 コンポーネント B: メインプレビュー画面外グレー＆境界線

#### カラー定義＆`is_dark` 受け渡し（[`src/ui/preview/panel.rs`](../../src/ui/preview/panel.rs)）
`paint_preview_viewport_canvas` 自体は `cx` を受け取らないため、`render_preview_viewport` 側で `let is_dark = cx.theme().mode.is_dark();` を取得して `PreviewCanvasDrawPayload` に `pub is_dark: bool` として格納する（`render_preview_panel` 側も同様にテーマで背景色を決定）。
```rust
struct PreviewPalette {
    backdrop_bg: gpui_kit::Rgba,
    canvas_inner_bg: gpui_kit::Rgba,
    canvas_border: gpui_kit::Rgba,
}

fn preview_palette(is_dark: bool) -> PreviewPalette {
    if is_dark {
        PreviewPalette {
            backdrop_bg: rgba(0x2b30_3bff),      // #2B303B: 動画の黒と明確に分離するニュートラルグレー
            canvas_inner_bg: rgba(0x0000_00ff),  // キャンバス内は純黒
            canvas_border: rgba(0x6474_8bcc),    // 外枠 1px 境界線
        }
    } else {
        PreviewPalette {
            backdrop_bg: rgba(0xd4d8_e0ff),      // #D4D8E0: ライトモード用画面外グレー
            canvas_inner_bg: rgba(0x0000_00ff),
            canvas_border: rgba(0x4755_69cc),
        }
    }
}
```

#### `paint_preview_viewport_canvas` 描画順＆クリッピング修正
1. `window.with_content_mask(Some(ContentMask { bounds: viewport_bounds }), |window| { ... })` 内でキャンバス描画を行い、`paint_quad` がビューポート境界外へはみ出さないよう確実にクリップする。
2. `canvas_bounds` 内部を `palette.canvas_inner_bg` で塗りつぶす（枠線なし）。
3. `data.image` が存在する場合、`window.paint_image(viewport_bounds, canvas_bounds, ...)` を描画する。
4. **動画描画後に**、`canvas_bounds` を上下左右 `1.0px` 外側へ拡張した `outer_border_bounds` に対し、背景透明（`rgba(0x0000_0000)`）・枠線 `Edges::all(px(1.0))`・枠線色 `palette.canvas_border` の `quad` を描画する（動画本体を 1px も欠けさせず、かつ暗い画面端をくっきり表示）。
5. `show_grid` 時のグリッド線と `show_guides` 時の選択ワイヤーフレームを最前面に描画する。

### 4.3 コンポーネント C: 設定 JSON エディタのシンタックスハイライト

#### 依存設定（[`Cargo.toml`](../../Cargo.toml)）
```toml
gpui-kit = { version = "0.9.1", features = ["tree-sitter"] }
```
（※`gpui-component-0.9.1` にて `tree-sitter 0.26.13` + `tree-sitter-json 0.24.8` のみが有効になり、`EditorState::new(window, cx).language("json")` / `set_value` / `value` / `focus_handle` が実在）

#### UI・状態置換＆`Escape` キー競合防止（`src/ui/ai_settings_modal/`・`src/ui/window.rs`）
- `mod.rs`: `pub json_editor: Entity<EditorState>` へ変更。
- `actions.rs`:
  ```rust
  let json_editor = cx.new(|cx| EditorState::new(window, cx).language("json").searchable(false));
  json_editor.update(cx, |s, cx| s.set_value(&initial_json, window, cx));
  ```
  - **検索バーとの `Escape` 競合防止**: `EditorState::new` は既定で `searchable = true`（`Ctrl+F` 検索バー有効）だが、`.searchable(false)` を明示設定し、既存の「モーダル内で `Escape` 押下＝モーダル閉じ」の操作感を維持する。
- `json_view.rs`:
  ```rust
  let theme = cx.theme();
  div()
      .w_full()
      .h(px(360.))
      .rounded(px(6.0))
      .border_1()
      .border_color(theme.border)
      .bg(theme.background)
      .child(Editor::new(&modal.json_editor).h(px(360.)).appearance(false))
  ```
- `window.rs`: `is_any_text_input_focused` 内で `modal.json_editor.read(cx).focus_handle(cx).is_focused(window)` が引き続き機能することを維持。

---

## 5. インターフェース / API 仕様

- `AppController::cycle_playhead_follow_mode(&mut self)` -> `TimelinePlayheadFollowMode`（`Page` → `Center` → `Off` → `Page`、同時に `editor_prefs.json` へ保存）
- `AppController::set_playhead_follow_mode(&mut self, mode: TimelinePlayheadFollowMode)`
- `AppController::timeline_max_scroll_frames(&self) -> u64`
- `AppController::scrub_to_frame(&mut self, frame: FrameIndex)`（`RulerScrub` 用、強制ジャンプ追随なし）
- `AppController::mark_timeline_manual_scroll(&mut self)`（再生中の手動水平スクロール時に呼ぶ）
- `AppController::ensure_playhead_visible_in_timeline(&mut self)`
- `lang/ja.json` / `lang/en.json` 追加・更新キー：
  - `"timeline.follow_clip"`: `"選択追従"` / `"Follow Clip"`
  - `"timeline.playhead_follow_page"`: `"再生追従: ページ"` / `"Playhead: Page"`
  - `"timeline.playhead_follow_center"`: `"再生追従: 中央"` / `"Playhead: Center"`
  - `"timeline.playhead_follow_off"`: `"再生追従: OFF"` / `"Playhead: Off"`

---

## 6. エラーハンドリング・エッジケース

- **タイムライン追随**:
  - `total_duration_frames == 0` または全フレームが 1 画面内に収まっている（`max_scroll == 0`）場合：`timeline_scroll_x_frames` は常に `0` に維持され不要なスクロールを行わない。
  - `Center` モードで末尾付近を再生中：`timeline_max_scroll_frames()` でクランプし、タイムライン末尾の先に過剰な空白が流れないようにする。
  - 超高倍率ズームで `visible_frames` が非常に小さい場合：`(visible_frames * 0.90).floor().max(1.0)` および `lead < right_trigger_offset` 保護で毎フレームの振動スクロールを防止する。
  - `settings/editor_prefs.json` が破損している場合は既定値 `Page` で起動し、エラーで起動を止めない。
- **プレビュー画面外カラー＆境界**:
  - ズーム `200%` 等で動画キャンバスがビューポートより大きい場合：`window.with_content_mask(Some(ContentMask { bounds: viewport_bounds }), ...)` により外枠ボーダーがビューポート外へはみ出さず、他パネルへの描画漏れを完全に防ぐ。
  - ズーム `25%` 等でキャンバスが小さい場合：周囲のグレー（`#2B303B`）と 1px 境界枠線により、キャンバス位置・比率が一目で分かる。
- **JSON エディタ (`EditorState`)**:
  - `gpui-kit` の `Editor` が使用する `HighlightTheme` が `SeaEditorWindow` の `ThemeMode::Dark` / `Light` 切替時に正しく追従する（`theme.border` / `theme.background` 使用）。
  - 不正な JSON を入力した状態で「保存」や「整形」を押した場合は、既存のパースエラー通知バナーへ正確な行番号・列番号付きエラーメッセージをそのまま表示する。

---

## 7. テスト計画・受け入れ基準

### 単体テスト（画面を出さない軽量テスト）
- **`src/ui/editor_prefs.rs` / `src/ui/app_controller.rs`**:
  - `TimelinePlayheadFollowMode::Page` 時に右端 90% 超過で 12% 位置へページ送りされること、左端未満への巻き戻しシークでも追随すること、および超高倍率ズームでも `lead < right_trigger_offset` が維持されること。
  - `TimelinePlayheadFollowMode::Center` 時に中央 50% 位置へ追随し、先頭付近では `0` クランプ、末尾では `timeline_max_scroll_frames()` クランプされること。
  - `span <= visible_u64`（全フレームが 1 画面内に収まっている状態）で `timeline_max_scroll_frames() == 0` となり、`Center` モードでも不要スクロールが発生しないこと。
  - `scrub_to_frame`（`RulerScrub`）が不要なスクロールジャンプを起こさないこと。
  - `playhead_follow_paused_by_manual_scroll == true` の間は `tick_playback` でスクロールを上書きせず、再生中の `set_timeline_zoom_ratio_anchored` では休止しないこと、`seek_to_frame` / 再生開始 / モード再設定で解除されること。
  - `EditorPreferences` の JSON 保存・再読込・破損時フォールバックを検証。
- **`src/ui/preview/panel.rs`**:
  - ダーク／ライト両モードの `preview_palette` がキャンバス内部色（黒）と十分な輝度差を持つこと。
  - 外周 1px 境界枠の `Bounds` 計算が `canvas_bounds` を上下左右 1px 外側へ拡張し、動画本体矩形と重ならないこと。
- **`src/ui/ai_settings_modal/`**:
  - 既存のパーサー・メタ整形・UI 設定永続化テスト（`ui::ai_settings_modal::tests`, `phase_ai_settings_ui`）が全通過すること。

### 結合テスト・手動確認項目（実装フェーズ完了後）
- **手動確認（実 GUI 操作）**:
  1. タイムラインをズームインして再生し、`ページ` モードで右端到達時にページ送り、`中央` モードでプレイヘッド中央維持、`OFF` で固定、再生中の手動ホイールで一時休止、アプリ再起動後に選択モードが維持されていることを実機確認。
  2. ダークモードで黒背景のシーンを表示し、動画の四隅と外枠が画面外グレー（`#2B303B`）とはっきり区別できること。
  3. 設定モーダルの `{ } JSON` タブを開き、JSON キー・文字列・数値・括弧がシンタックスハイライトされ、行番号表示・整形・保存・`Escape` 閉じができること。

### 完了条件（Definition of Done）
- `scripts/pre-pr.ps1`（`fmt` / `clippy -D warnings` / `doc -D warnings` / `build` / `test`）および利用可能環境での `cargo deny check` がエラーゼロで通過すること。

---

## 8. 未決事項・検討メモ（Open Questions）

- [x] **決定済み**: タイムライン再生位置追随は案 B（`Page` ページ送り / `Center` 中央固定 / `Off` の切替方式）を採用し、初回既定値を `Page` とする。
- [x] **決定済み**: `playhead_follow_mode` は `settings/editor_prefs.json` に保存し、アプリ再起動後も前回選択を維持する。
- [x] **決定済み**: JSON 設定エディタのハイライトには `gpui-kit = "0.9.1"` 内蔵の `Editor`（`tree-sitter` feature）を採用する。
- [x] **調査解消済み**: `gpui-kit = { version = "0.9.1", features = ["tree-sitter"] }` で `tree-sitter 0.26.13` + `tree-sitter-json 0.24.8` のみが有効になり、`EditorState::new(window, cx).language("json")` / `set_value` / `value` / `focus_handle` / `searchable(bool)`、および `Window::with_content_mask(Option<ContentMask<Pixels>>, ...)` が実在することを確認済み。
