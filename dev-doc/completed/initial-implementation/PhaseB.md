# 初期実装設計書：Phase B（GPU 合成・各メディア編集・タイムライン UI・書き出しプラグイン）

- **ステータス**: Draft
- **作成日**: 2026-11-15
- **更新日**: 2026-11-15
- **関連ドキュメント**:
  - 全体基本設計書: [`../draft.md`](../draft.md)
  - 前段階設計書: [`PhaseA.md`](PhaseA.md)
  - 次段階設計書: [`PhaseC.md`](PhaseC.md)
  - 開発・テスト配置ルール: [`../../.github/copilot-instructions.md`](../../.github/copilot-instructions.md)

---

## 1. Phase B のゴールとスコープ

Phase B では、Phase A で構築した基盤の上に **「`wgpu` GPU 多層合成」「動画・画像・字幕の編集機能」「仮想化マルチレイヤータイムライン UI」「Cargo Workspace ベースの書き出しプラグインシステム」** を実装する。音声ミックスには PhaseA-4 で先行構築した `AudioMixer` をそのまま接続する。

- **Phase B 完了時の状態**:
  - 動画・画像・字幕をどの段（レイヤー）にも無制限（仮想スクロール）で配置でき、ドラッグ＆ドロップ移動・カット・トリム・コピー・ペースト・複製ができる。
  - 動画・画像・字幕のすべてに対し、位置・サイズ・アンカー・縦横拡大率・回転・透明度・色調整を `Constant / Easing / Keyframes / Expression f(t)` の 4 モードとつまみ微調整で編集できる。
  - `wgpu` シェーダによりプレビューも書き出しも同じ画質で高速合成され、`PreviewSurfaceBridge` 経由（縮小 `Arc<RenderImage>` 標準経路またはスパイク実証済み OS のゼロコピー経路）で軽快にプレビュー表示できる。
  - 専用フォルダ（`plugins/export/`）へプラグインを追加するだけで書き出し形式が増え、SW / GPU 両対応で動画を書き出せる。

---

## 2. 作業単位一覧

- **PhaseB-1**: `wgpu` GPU 合成エンジン（WGSL シェーダ群）と `PreviewSurfaceBridge` 統合
- **PhaseB-2**: 動画・画像・字幕編集モジュールとインスペクタ UI（4 モード＋`Keyframes`＋つまみ微調整）
- **PhaseB-3**: マルチレイヤータイムライン編集 UI（仮想スクロール・D&D・フレーム境界スナップ・カット・複製・コピペ）
- **PhaseB-4**: Cargo Workspace 化・書き出しプラグインシステム（自動認識・共有メモリ IPC・テストバイナリ解決基盤）
- **PhaseB-5**: 書き出しプラグイン実装（ロイヤリティフリー AV1 + OS ネイティブ HW 支援 H.264、SW/GPU 両対応）

---

## PhaseB-1：`wgpu` GPU 合成エンジンと `PreviewSurfaceBridge` 統合

### 目的と対象モジュール
多層レイヤーの変形・アンカー回転・透明度・色調整を WGSL シェーダで並列実行し、プレビューと書き出しの双方で共用できる `wgpu` 合成エンジンを構築する。

- **主な対象ファイル（計画）**:
  - `src/media/compositor/mod.rs`（`Compositor` 本体、レンダーシーン評価）
  - `src/media/compositor/shaders/layer_composite.wgsl`（アンカー補正・縦横拡大・回転・平行移動・透明度・色調整 5 項目シェーダ）
  - `src/media/compositor/texture_pool.rs`（GPU テクスチャプール・YUV→RGBA GPU 変換）
  - `src/ui/surface_bridge.rs`（`wgpu` → プレビュー解像度ダウンケール＋非同期ステージング読み出しによる `Arc<RenderImage>` 実装、および検証済み OS 向けゼロコピー共有実装。共有ハンドル操作部は `backends/` へ隔離し `#[allow(unsafe_code)]` + `// SAFETY:` 必須）

### 設計仕様
- **座標・アンカーの厳密適用**:
  - [`../draft.md`](../draft.md) §5.4 に従い、キャンバス中央 `(0.0, 0.0)` 原点・`anchor_x/y`（`-0.5..=0.5`）基準で **アンカー補正 → 縦横拡大 → 時計回り回転 → 中央からの平行移動** の順に行列を計算し、シェーダへ渡す。
- **GPU リソース非依存のテスト設計**:
  - シーン構築・レイヤー描画順・変換行列生成は純粋 Rust テストで常時検証する。
  - 実際の `wgpu` アダプタ生成とオフスクリーン描画を行うピクセル検証テストは、ヘッドレス CI 環境（GPU 不在環境）で失敗しないよう **`#[ignore = "requires wgpu graphics adapter"]`** を付与し、実 GPU / WARP 環境下で `cargo test -- --ignored` により検証する。

### 動作確認とテストの実施
1. **単体テスト**（`src/media/compositor/mod.rs` 等末尾）:
   - 複数レイヤーの重ね順解決、アンカー中心回転の頂点座標計算、`AssetStatus::Missing` クリップの「Media Offline」フォールバックシーン生成テスト。
2. **結合テスト**:
   - `tests/phase_b_compositor.rs`
   - `tests/phase_b_compositor/mod.rs`
   - `tests/phase_b_compositor/gpu_layer_blending.rs`
   （シーン生成の結合テスト＋実 `wgpu` アダプタ上で半透明＋回転＋色相変更レイヤーを合成し出力 RGBA を検査する `#[ignore = "requires wgpu graphics adapter"]` テスト）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` 通過。

---

## PhaseB-2：動画・画像・字幕編集モジュールと設定パネル（4 モード＋`Keyframes`＋つまみ連携）

### 目的と対象モジュール
動画編集・画像・字幕の 3 モジュールを共通 `VisualProperties` 上に実装し、右パネル（インスペクタ）から **`Constant` / `Easing` / `Keyframes` / `Expression f(t)` の 4 モードすべて** とつまみ微調整、さらに `EditCommand::SetParameter` で統一操作できるようにする。

- **主な対象ファイル（計画）**:
  - `src/media/image_loader.rs`（`image` クレートによる PNG / JPEG / WebP 読み込みとテクスチャキャッシュ）
  - `src/media/subtitle_renderer.rs`（`cosmic-text` 等を用いた OS 非依存テキストレイアウト・フォント・文字色・縁取り・背景ラスタライズ）
  - `src/ui/inspector_panel.rs`（`gpui-component` スライダー・数値ボックス・**`Constant / Easing / Keyframes / Expression f(t)` 4 モード切替タブ**・キーフレーム追加/削除/現在時刻移動・`base_offset`/`scale` ツマミ・時間変化カーブプレビュー）

### 設計仕様
- **4 モード操作仕様**:
  - `Constant`: 数値スライダー直接更新。
  - `Easing`: 始点値・終点値・カーブ種別・`ExtrapolationMode` を設定。
  - `Keyframes`: 現在再生時刻の **エフェクト時刻（`effect_offset_frames + local_frame`）** 位置へキーフレームを追加・削除・値変更でき、キーフレーム一覧またはタイムライン上のマーカーと連動する。
  - `Expression f(t)`: 数式入力欄＋数式構文エラーのリアルタイム表示＋数式文字列を壊さない `base_offset` / `scale` ツマミ微調整。
- **統一コマンド発行**:
  - UI 操作も AI 操作もすべて `EditCommand::SetParameter { target, path, value: ParameterValue }` を発行する（数値・アニメーションは `ParameterValue::Animated`、字幕本文・フォント名は `ParameterValue::Text`、文字色・縁取り色・背景色は `ParameterValue::Color` を使用）。

### 動作確認とテストの実施
1. **単体テスト**（`src/media/subtitle_renderer.rs`, `src/media/image_loader.rs`, `src/ui/inspector_panel.rs` 状態ロジック末尾）:
   - 日本語字幕テキストの縁取り付きラスタライズ結果が有効な非空 RGBA ビットマップになること。
   - インスペクタから `Keyframes` / `Expression` 変更時に正しい `SetParameter` コマンドが生成されること。
2. **結合テスト**:
   - `tests/phase_b_media_modules.rs`
   - `tests/phase_b_media_modules/mod.rs`
   - `tests/phase_b_media_modules/transform_and_subtitle_split.rs`
   （動画・画像・字幕クリップへ `Keyframes` と `Expression f(t)` を `SetParameter` で適用→カット分割→前半終端と後半先頭での評価結果・変換行列一致を結合検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` 通過。

---

## PhaseB-3：マルチレイヤータイムライン編集 UI（仮想スクロール・D&D・フレーム境界スナップ）

### 目的と対象モジュール
無制限のレイヤーを仮想スクロールで軽快に描画し、フレーム境界単位でドラッグ＆ドロップ移動・トリム・カット・コピー・ペースト・複製が行えるタイムライン UI を完成させる。

- **主な対象ファイル（計画）**:
  - `src/timeline/clipboard.rs`（選択クリップ群のコピー＆ペースト、相対時間・相対レイヤー位置保持、衝突回避シフト計算）
  - `src/timeline/snap.rs`（再生ヘッド・他クリップ端へのフレーム単位スナップ計算）
  - `src/ui/timeline_panel.rs`（仮想化レイヤーリスト、D&D レイヤー横断移動、左右端トリムハンドル、カット操作、ショートカット `Ctrl+C`/`V`/`D`/`Z`）

### 設計仕様
- **フレーム境界の厳格維持**:
  - マウスピクセル座標→時間変換時は必ず `FrameIndex`（整数フレーム）へスナップしてから `MoveClip` / `TrimLeft` / `TrimRight` / `SplitClip` を発行する。
- **仮想スクロール描画**:
  - 画面内の可視レイヤー行と表示時間範囲に該当するクリップのみを描画し、100 段以上配置しても 60fps UI 操作を維持する。

### 動作確認とテストの実施
1. **単体テスト**（`src/timeline/clipboard.rs`, `src/timeline/snap.rs` 各末尾）:
   - スナップしきい値、複数レイヤー跨ぎクリップのペースト位置計算、可視範囲クリッピング純粋関数テスト。
2. **結合テスト**:
   - `tests/phase_b_timeline_editing.rs`
   - `tests/phase_b_timeline_editing/mod.rs`
   - `tests/phase_b_timeline_editing/drag_copy_paste_duplicate.rs`
   （80 段目を含む複数レイヤーへの混在配置→ドラッグ移動→左右トリム→コピペ→複製→カット→Undo の一連を検証）
3. **手動確認**:
   - 実ウィンドウ上で D&D 移動、トリム、カット、コピペ、複製、多段スクロールが引っかかりなく動作することを確認。
4. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` 通過。

---

## PhaseB-4：Cargo Workspace 化・書き出しプラグインシステム（自動検出・共有メモリ IPC）

### 目的と対象モジュール
ルート `Cargo.toml` を Cargo Workspace（`members = [".", "plugins_src/*"]`）へ移行して全メンバーへ統一リントを適用しつつ、専用フォルダ `plugins/export/` へプラグインを追加するだけで自動認識される書き出しホスト基盤を構築する。音声データは PhaseA-4 で先行実装済みの `AudioMixer` から受け取る。

- **主な対象ファイル（計画）**:
  - `Cargo.toml`（`[workspace]` 構成追加、既存の `[lints.rust]` / `[lints.clippy]` を `[workspace.lints.rust]` / `[workspace.lints.clippy]` へ移設し、ルート自身も `[lints] workspace = true` に更新）
  - `plugins_src/*/Cargo.toml`（すべてのプラグインメンバーで `[lints] workspace = true` を設定し、`unsafe_code = "deny"` や Clippy ルール対象外になるのを防止）
  - `src/export/manifest.rs`（`plugin.json` スキーマ：形式名、拡張子、SW/GPU 能力フラグ、設定項目定義）
  - `src/export/registry.rs`（`plugins/export/<id>/` 走査、不正マニフェスト・欠損バイナリ隔離）
  - `src/export/protocol.rs`（`stdio` JSON-RPC 制御＋クロスプラットフォーム共有メモリフレーム転送。`unsafe` マッピング部は `backends/` に隔離し `#[allow(unsafe_code)]`＋`// SAFETY:` 必須）
  - `src/export/runner.rs`（`Compositor` と PhaseA-4 の `AudioMixer` から全フレーム・全 PCM ブロックを順送りし、進捗・キャンセルを制御）
  - `src/ui/export_dialog.rs`（`gpui-component` による書き出しダイアログ）

### 設計仕様
- **テスト時の Workspace プラグインバイナリ解決ルール**:
  - `CARGO_BIN_EXE_*` は同一パッケージ内のバイナリにしか使えないため、検証・CI 手順では **`cargo build --workspace --all-targets` を事前実行** したうえで `cargo test --workspace --all-targets --all-features` を実行する。
  - 結合テストには共通テストヘルパー（`std::env::current_exe()` の親ディレクトリ＝`target/debug/` 等からワークスペースビルド済みプラグインバイナリを解決して一時ディレクトリへ `plugin.json` と共に配置する関数）を用意し、**バイナリが見つからない場合はサイレントスキップせず `"plugin binary '<name>' not found; run 'cargo build --workspace --all-targets' first"` のように明確な理由付きでテストを失敗（panic）** させる。

### 動作確認とテストの実施
1. **単体テスト**（`src/export/manifest.rs`, `src/export/registry.rs`, `src/export/protocol.rs` 各末尾）:
   - 不正な `plugin.json` や実行ファイル不在プラグインを安全にスキップし、正常なプラグインのみ列挙すること。
   - 共有メモリリングバッファの送受信および境界チェックテスト。
2. **結合テスト**:
   - `tests/phase_b_export_plugin_host.rs`
   - `tests/phase_b_export_plugin_host/mod.rs`
   - `tests/phase_b_export_plugin_host/dummy_plugin_lifecycle.rs`
   （テスト用ダミープラグインバイナリを一時 `plugins/export/dummy-test/` へ配置し、自動認識→フレーム＆`AudioMixer` PCM 送信→進捗・完了、およびキャンセル・子プロセス強制終了からの安全なエラー回復を検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` 通過。

---

## PhaseB-5：書き出しプラグイン実装（標準 AV1 + OS ネイティブ HW 支援 H.264、SW/GPU 両対応）

### 目的と対象モジュール
「1 書き出し形式＝1 モジュール」「専用フォルダ追加で機能拡張」「SW/GPU 両対応」「特許・ライセンス非抵触」をすべて満たす最初の実書き出しプラグイン 2 本を実装する。

- **主な対象ファイル（計画）**:
  - `plugins_src/sea-export-av1/`（Workspace メンバー、`[lints] workspace = true`：純粋 Rust `rav1e` [BSD-2-Clause] による AV1 映像エンコード＋`flacenc` [Apache-2.0] による純粋 Rust FLAC 音声エンコード＋`oxideav-mkv` [MIT] または `webm-iterable` [MIT] による MKV/WebM コンテナ多重化出力。※ `matroska-demuxer` は読込専用のため書き出し用 muxer を明示採用。対応環境での GPU AV1 出力モードもサポート）
  - `plugins_src/sea-export-h264/`（Workspace メンバー、`[lints] workspace = true`：Windows Media Foundation HW/SW エンコーダ [`#[allow(unsafe_code)]` + `// SAFETY:`] 優先、およびユーザー自身が環境に導入した外部 `ffmpeg` CLI を別プロセス起動する任意オプション。GPL/特許バイナリは一切同梱しない）

### 動作確認とテストの実施
1. **単体テスト**（各プラグインクレートの各 `.rs` 末尾）:
   - SW / GPU モード切替、ビットレート・FPS・解像度パラメータ検証、MKV muxer / FLAC エンコーダ出力ヘッダ検証テスト。
2. **結合テスト**:
   - `tests/phase_b_export_execution.rs`
   - `tests/phase_b_export_execution/mod.rs`
   - `tests/phase_b_export_execution/export_av1_and_h264.rs`
   （`AudioMixer` の BGM 音声＋字幕＋変形 `f(t)`＋カット編集を含むテストプロジェクトを `sea-export-av1` [SW] で実際に書き出し、出力 MKV ファイルを `PhaseA-4` の純粋 Rust デコーダ [`matroska-demuxer` + `rav1d` + `symphonia`] で再読込して再生長・フレーム数一致を自動検証。OS HW H.264 エンコーダ実機テストは `#[ignore = "requires OS hardware H.264 encoder"]` として併設）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo build --workspace --all-targets` / `cargo test --workspace --all-targets --all-features` ゼロ警告・ゼロエラー。
