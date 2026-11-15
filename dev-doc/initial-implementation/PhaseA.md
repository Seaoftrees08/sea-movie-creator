# 初期実装設計書：Phase A（基盤モデル・評価エンジン・再生基盤・GPUI 1ウィンドウ骨格）

- **ステータス**: Draft
- **作成日**: 2026-11-15
- **更新日**: 2026-11-15
- **関連ドキュメント**:
  - 全体基本設計書: [`../draft.md`](../draft.md)
  - 次段階設計書: [`PhaseB.md`](PhaseB.md)
  - 開発・テスト配置ルール: [`../../.github/copilot-instructions.md`](../../.github/copilot-instructions.md)

---

## 1. Phase A のゴールとスコープ

Phase A では、まず **GPUI 技術検証スパイク（PhaseA-0）** でウィンドウ・メニュー・プレビュー描画方式の前提を確定したうえで、OS 非依存のプロジェクトデータ構造・フレーム境界タイムライン演算・`f(t)` 評価エンジンを純粋 Rust で確立し、**「Windows 上で動画を読み込み、音声付きで滑らかに再生できる 1 ウィンドウアプリ（GPUI）」** を構築する。

- **Phase A 完了時の状態**:
  - `gpui` / `gpui-component` の動作バージョンとプレビュー表示方式が検証済み（`Cargo.toml` の `rust-version` も実要件に整合）。
  - GPUI で 1 ウィンドウ（カスタムメニューバー・中央プレビュー・下部タイムライン・右 1/5 タブパネル）が起動する。
  - Windows 環境で **Media Foundation / D3D11VA（`backends/windows/` に `unsafe` 隔離）＋ 純粋 Rust ロイヤリティフリー SW デコード 1 経路（`matroska-demuxer` + `rav1d` + `symphonia` [FLAC/Vorbis/WAV]）** を用いて動画を読み込み、最小限の `AudioMixer` 経由で音声付きプレビュー再生・停止・シークができる（macOS / Linux の OS HW バックエンドは Phase C-5 で追加）。
  - AI からも UI からも使える宣言的 `ProjectDocument`（`schemars` JSON Schema 出力対応）、フレーム境界・有理再生速度・トリム・カット編集の途中オフセット継承、および `AnimatedParameter` が自動テスト付きで揃っている。

---

## 2. 作業単位一覧

- **PhaseA-0**: GPUI / `gpui-component` / プレビュー表面連携の技術検証スパイク（実装着手前の前提確認）
- **PhaseA-1**: フレーム境界時間表現・`PlaybackRate`・`ProjectDocument`・`AssetPath`・`schemars` スキーマ出力
- **PhaseA-2**: 統一 `EditCommand`・バッチ仮 ID 解決・`CommandDispatcher`・Undo/Redo・カット＆トリムロジック
- **PhaseA-3**: `AnimatedParameter`（定数 / イージング / キーフレーム / `f(t)` / `ExtrapolationMode`）と座標・アンカー評価
- **PhaseA-4**: `MediaBackend`（Windows MF + 純粋 Rust 1 経路）・先読みキャッシュ・最小 `AudioMixer` と統合 `PlaybackClock`
- **PhaseA-5**: GPUI 1 ウィンドウ骨格（`gpui-component` + カスタムメニューバー）と `PreviewSurfaceBridge` 再生結合

---

## PhaseA-0：GPUI 連携・`rav1d` 検証・MSRV 技術検証スパイク

### 目的と対象モジュール
GPUI の Windows 対応状況（外部 GPU テクスチャ共有可否、メニューバー描画、ウィジェット利用、要求 Rust バージョン）および `rav1d` のビルド・API・`unsafe` 範囲を最小コードで実測検証し、Phase A〜B の手戻りリスクをゼロにする。

- **検証項目**:
  1. `gpui` および `gpui-component`（`0.5.x` 系または最新整合リビジョン）を `Cargo.toml` へ導入し、Windows（`rustc 1.100.0`）でビルドできることを確認し、`Cargo.toml` の `rust-version` を実測要件に合わせて更新する。
  2. Windows 上の GPUI で `Arc<gpui::RenderImage>` による縮小プレビューフレーム更新（60fps 更新時の CPU/GPU 負荷）と、外部 D3D11/wgpu テクスチャのゼロコピー取り込み可否を検証する。
     - **基本方針**: ゼロコピー外部テクスチャが Windows GPUI 公開 API で安全に成立しないと判明した場合は、**「`wgpu` でプレビューサイズへ縮小後にダブルバッファ `Arc<RenderImage>` へ渡す方式」を正式主経路** として確定し、`draft.md` §3.2 へ実測結果を反映する。
  3. Windows 用のカスタム GPUI メニューバー描画と、`gpui-component` の入力欄・スライダー・タブ・仮想リストの動作を確認する。
  4. **`rav1d` の API・ビルド要件・`unsafe` 範囲の検証**:
     - `rav1d`（`BSD-2-Clause`）が公開する `dav1d` 互換 C ABI（`dav1d_open` / `dav1d_send_data` / `dav1d_get_picture` 等）の呼び出し側で必要になる `unsafe` 境界を確認する（AGPL-3.0 の `rav1d-safe` は使用しない）。
     - x86_64 Windows 環境で `asm` feature 有効時に `nasm` がビルド依存として必要になるか、および `default-features = false` での純粋 Rust フォールバック時の速度・実ビルド成否を確認する（必要なら `docs/getting-started.md` 前提条件へ `nasm` 要否を追記）。

- **実測・検証結果（完了済み）**:
  1. `gpui-kit = "0.9.1"`（`gpui-pre = 0.3.12` + `gpui-component = 0.9.1`）を `Cargo.toml` へ導入し、Windows 11（`rustc 1.100.0`）でビルド・実ウィンドウ起動に成功。推移的依存（`cosmic-text 0.19.0`: Rust 1.89、`oo7 0.6.0`: Rust 1.92）に基づき **`Cargo.toml` の `rust-version` を `"1.92"`** へ確定した。
  2. `gpui-pre 0.3.12` / `gpui 0.2.2` の `Surface` / `paint_surface` は macOS/iOS 限定（`CVPixelBuffer`）であり、Windows 版 GPUI に外部 GPU テクスチャのゼロコピー公開 API がないことを確認。よって **BGRA 形式 `Arc<gpui::RenderImage>` ダブルバッファ（プレビュー解像度へ縮小＋`window.drop_image(...)` でアトラス解放）を正式主経路** として確定した（`960x540` BGRA フレーム生成は debug ビルドでも `2.1〜2.4ms/frame` であり 60fps 要件を満たす）。
  3. `gpui_component::menu::AppMenuBar` + `TitleBar` + `Theme::change(ThemeMode::Dark, ...)` + `TabBar` + `Input` + `Slider` + `h_resizable` + `v_virtual_list` の Windows 上での実動作とダークテーマ表示を確認。
  4. `rav1d = "1.1.0"` 検証：既定 `asm` feature は Windows x86_64 で `nasm` 不在時にビルドエラー（`nasm-rs` パニック）となる一方、`default-features = false, features = ["bitdepth_8", "bitdepth_16"]` は `nasm` 不要の純粋 Rust でビルド成功。さらに Windows 既定 1MiB スレッドスタックでは非最適化 `dev` ビルド時にスタックオーバーフローを起こすため、**`[profile.dev.package.rav1d]` / `rav1e` に `opt-level = 2` を設定し、デコーダスレッドでも 8MiB スタックを確保** することで回避できることを実測確認した（`dav1d_*` C ABI 呼び出し境界に `#[allow(unsafe_code)]` + `// SAFETY:` を隔離し、64x64 AV1 1 フレームを 27.26ms でデコード成功）。
  5. 使い捨てスパイクコード `examples/phase_a0_spike.rs` は本記録の反映後に削除して `PhaseA-1` へ進んだ。

### 動作確認とテストの実施
1. **スパイク動作・ビルド確認**:
   - 最小ウィンドウでダミー色のフレーム更新（`RenderImage` 60fps ループ）とタブ切替・メニュー表示、`rav1d` による 1 フレーム AV1 デコード疎通を確認する。
2. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` ゼロ警告・ゼロエラー。

---

## PhaseA-1：フレーム表現・`PlaybackRate`・`ProjectDocument`・`schemars` スキーマ

### 目的と対象モジュール
浮動小数点の丸め誤差・フレーム境界ズレを構造的に排除し、AI（`schemars` JSON Schema 自動出力）と人間の双方が安全に扱える `ProjectDocument` スキーマを確定・実装する。

- **主な対象ファイル（計画）**:
  - `src/core/time.rs`（`FrameRate`, `FrameIndex`, `FrameCount`, `PlaybackRate`, `RationalDuration` と GCD 約分）
  - `src/core/ids.rs`（`AssetId`, `LayerId`, `ClipId`, `ClipRef`, `LayerRef`）
  - `src/media/visual_properties.rs`（キャンバス中央原点座標系・`anchor_x`/`anchor_y` を含む `VisualProperties` 型骨格）
  - `src/timeline/document.rs`（`ProjectDocument`, `ProjectResolution`, `Layer`, `Clip`, `ClipKind`, `Asset`, `AssetPath`, `AssetStatus`）
  - `src/timeline/serialization.rs`（スキーマバージョン付き JSON 読み書き＋`AssetPath` 相対/絶対解決）
  - `src/core/schema.rs`（`schemars` によるコマンド・ドキュメント JSON Schema エクスポート）

### 設計仕様
- **フレーム境界・有理数演算・プロジェクト FPS ポリシー**:
  - タイムライン座標 `timeline_start_frame: FrameIndex(u64)`（非負）・長さ `duration_frames: FrameCount(u64)`（`>= 1`）を整数フレームで保持する。
  - `ProjectDocument.fps` は Phase A では作成時に固定とし、作成後の直接変更は禁止する（将来対応時は全クリップを比例変換・フレーム再スナップする専用コマンド `SetProjectFrameRate` で実装）。
  - `Clip.playback_rate: PlaybackRate { numerator: u32, denominator: u32 }`（正の有理数、既定 `1/1`）を Phase A から `Clip` に持たせる。
  - `Clip.source_offset: RationalDuration`（秒の既約分数）、`Clip.effect_offset_frames: i64`、`Clip.effect_duration_frames: FrameCount`（`>= 1`）。
- **素材パスと Missing Media**:
  - `Asset.source_path: AssetPath { relative_to_project: Option<PathBuf>, fallback_absolute: PathBuf }`。保存時はプロジェクトファイル配下なら相対パス優先生成、ロード時に存在しなければクラッシュさせず `AssetStatus::Missing` として読み込む。
- **JSON Schema 自動生成**:
  - すべての公開ドキュメント・コマンド型へ `#[derive(JsonSchema)]` を付与し、CLI フラグ `--print-command-schema` 等から常に最新スキーマを出力できるようにする。

### 動作確認とテストの実施
1. **単体テスト**（`src/core/time.rs`, `src/timeline/document.rs`, `src/timeline/serialization.rs`, `src/core/schema.rs` 各末尾）:
   - `29.97fps (30000/1001)` と `playback_rate = 3/2` の組み合わせでフレーム→`RationalDuration` 変換に丸め誤差が出ないことを検証。
   - `duration_frames == 0` / `playback_rate.numerator == 0` のデシリアライズ・バリデーション拒否テスト。
   - 100 レイヤー以上の混在配置および JSON 往復テスト、`schemars` 出力が有効な JSON Schema であることの検証。
   - 存在しない素材パスでも `ProjectDocument` 読み込みが成功し `AssetStatus::Missing` になることの確認。
2. **結合テスト**:
   - `tests/phase_a_document.rs`
   - `tests/phase_a_document/mod.rs`
   - `tests/phase_a_document/project_roundtrip.rs`
   （複数レイヤー・複数メディア・欠損素材を含むプロジェクト保存・再読込・スキーマ生成の結合検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` ゼロ警告・ゼロエラー。

---

## PhaseA-2：統一 `EditCommand`・仮 ID 解決・Undo/Redo・カット＆トリムロジック

### 目的と対象モジュール
手動操作・AI 操作の両方が使う統一コマンド基盤（仮 ID 解決付き `Batch`、パラメータ統一コマンド `SetParameter`、逆パッチ Undo/Redo）と、カット・左端トリム・右端トリム・バリデーションを実装する。

- **主な対象ファイル（計画）**:
  - `src/timeline/command.rs`（`EditCommand`, `ParameterTarget`, `ParameterPath`, `ClipRef`, `LayerRef`）
  - `src/timeline/dispatcher.rs`（`CommandDispatcher`, `dry_run_validate`, バッチ内仮 ID テーブル解決と不変条件検証）
  - `src/timeline/history.rs`（逆パッチ生成と Undo/Redo スタック）
  - `src/timeline/operations.rs`（`SplitClip`, `TrimLeft`, `TrimRight`, `MoveClip`, `DuplicateClip`, `CopyClips`, `PasteClips`, `RelinkAsset`）

### 設計仕様
- **統一パラメータ更新コマンド（非数値型を含む `ParameterValue`）**:
  - `EditCommand::SetParameter { target: ParameterTarget, path: ParameterPath, value: ParameterValue }` に一本化する。
  - `ParameterValue` は `Animated(AnimatedParameter)` / `Text(String)` / `Bool(bool)` / `Color(RgbaColor)` / `Extrapolation(ExtrapolationMode)` / `EnumVariant(String)` を持ち、字幕本文・フォント・文字色・`preserve_pitch`・`ExtrapolationMode` 等も同一コマンドで扱う（`path` と型が合わない場合は `TypeMismatchError` を返す）。
- **再生速度変更の専用コマンド**:
  - `EditCommand::SetPlaybackRate { clip: ClipRef, rate: PlaybackRate }` を用意し、`rate > 0` および変更後の `source_offset + duration_sec * rate <= asset.duration` を原子的に再検証する（超過時はエラー拒否）。
- **バッチ内仮 ID 解決**:
  - `AddClip { temp_id: Option<String>, ... }` / `AddLayer { temp_id: Option<String>, ... }` 等で指定した `temp_id` を、同じ `EditCommand::Batch` 内の後続 `SetParameter` / `SetPlaybackRate` / `SplitClip` から `ClipRef::Temp(temp_id)` で参照可能にする。
- **カット・トリムの厳密演算とバリデーション**:
  - `SplitClip`: 前半 `duration_frames = split_offset_frames`、後半 `timeline_start_frame += split_offset_frames`, `duration_frames -= split_offset_frames`, `source_offset += split_offset_frames 相当の速度補正秒数`, `effect_offset_frames += split_offset_frames`, `effect_duration_frames` 不変。
  - `TrimLeft`: `timeline_start_frame += delta_frames`, `duration_frames -= delta_frames`, `source_offset += delta_sec * playback_rate`, `effect_offset_frames += delta_frames`。
  - `TrimRight`: `duration_frames = new_duration_frames` のみ更新（`effect_duration_frames` 不変）。
  - **不変条件検査**: 負の開始時刻禁止、`duration_frames >= 1`、同一レイヤー内重複禁止、有限素材に対して `source_offset + duration_sec * playback_rate <= asset.duration` をチェック（違反時はロールバック＋詳細エラー）。

### 動作確認とテストの実施
1. **単体テスト**（`src/timeline/operations.rs`, `src/timeline/dispatcher.rs`, `src/timeline/history.rs` 各末尾）:
   - 左トリム（縮め・伸ばし）・右トリム・連続 2 回カット後の `source_offset` と `effect_offset_frames` が有理数演算で完全一致すること。
   - `SetParameter` で `Text` / `Bool` / `Color` / `Animated` 各バリアントの更新と型不一致エラー拒否を検証。
   - `SetPlaybackRate` 変更時に素材長超過となる速度指定がエラー拒否され、正常範囲では Undo で元に戻ること。
   - 素材長超過、負時刻移動、同一レイヤー衝突時にエラーとなり変更がロールバックされること。
   - 同一 `Batch` 内で `AddClip(temp_id: "$c1")` → `SetParameter(ClipRef::Temp("$c1"))` → `SetPlaybackRate(ClipRef::Temp("$c1"))` → `SplitClip(ClipRef::Temp("$c1"))` が成功すること。
2. **結合テスト**:
   - `tests/phase_a_timeline_commands.rs`
   - `tests/phase_a_timeline_commands/mod.rs`
   - `tests/phase_a_timeline_commands/split_and_undo.rs`
   （追加→移動→トリム→カット→複製→一括 Undo/Redo の結合検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` ゼロ警告・ゼロエラー。

---

## PhaseA-3：`AnimatedParameter`・`f(t)` 評価エンジン・座標/アンカー評価

### 目的と対象モジュール
`Constant / Easing / Keyframes / Expression f(t)` の 4 モード統一評価エンジンと、キャンバス中央原点の変形行列算出ロジックを実装する。

- **主な対象ファイル（計画）**:
  - `src/core/animation.rs`（`AnimatedParameter`, `ExtrapolationMode`, `EasingCurve`, `Keyframe`, `EvaluationContext`）
  - `src/core/expression.rs`（純粋 Rust 算術 AST パーサ＆サンドボックス評価器）
  - `src/media/visual_properties.rs`（時刻 `t` での `VisualProperties` 評価・2D/3D 変換行列生成）

### 設計仕様
- **`p` の範囲と `ExtrapolationMode`**:
  - `effect_duration_frames >= 1` をバリデーション必須化（ゼロ除算ガードでも `p = 0.0`）。
  - `raw_p = effective_frame / effect_duration_frames` に対し、既定 `ExtrapolationMode::Clamp` で `0.0..=1.0` に収める（トリム延長しても安全）。`Extrapolate` / `Loop` / `PingPong` も明示選択可能。
- **`Keyframes` の基準時刻**:
  - `Keyframe.time_frames` は常に **エフェクト時刻（`effect_offset_frames + local_frame`）** 基準で評価するため、カット分割時も補間値が連続する。
- **数式 AST サンドボックス**:
  - 変数 `t`（秒）・`p`（外挿適用後進捗）・`d`（秒）、定数 `pi`・`e`、数学関数（`sin`, `cos`, `tan`, `abs`, `min`, `max`, `clamp`, `lerp`, `smoothstep`, `sqrt`, `pow`, `floor`, `ceil`）のみ許可。ノード数上限・ゼロ除算/`NaN` ガードを実装。
- **座標・アンカー適用順**:
  - キャンバス中央原点 `(0.0, 0.0)`、`anchor_x/y`（`-0.5..=0.5`）補正 → 縦横拡大（`scale_x`, `scale_y`） → 時計回り回転 → 平行移動（`x, y`）。

### 動作確認とテストの実施
1. **単体テスト**（`src/core/expression.rs`, `src/core/animation.rs`, `src/media/visual_properties.rs` 各末尾）:
   - `Clamp / Extrapolate / Loop / PingPong` 各モードの境界（`p < 0`, `p = 0`, `p = 1`, `p > 1`）検証。
   - 不正数式、未定義変数、ゼロ除算、`NaN` 発生式が安全にエラーになること。
   - アンカー `(0.0, 0.0)` 中心回転と `(-0.5, -0.5)` 左上角中心回転の変換行列結果比較。
2. **結合テスト**:
   - `tests/phase_a_animation_cut.rs`
   - `tests/phase_a_animation_cut/mod.rs`
   - `tests/phase_a_animation_cut/continuous_across_split.rs`
   （`Easing` / `Keyframes` / `Expression f(t)` それぞれについて、カット分割直前の終端と分割直後の先頭で値が連続することを検証）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` 通過。

---

## PhaseA-4：`MediaBackend`（Windows 先行 + 純粋 Rust 1 経路）・最小 `AudioMixer`・統合 `PlaybackClock`

### 目的と対象モジュール
Phase A ではスコープを **Windows OS バックエンド（Media Foundation / D3D11VA）＋ 純粋 Rust ロイヤリティフリー SW 1 経路** に絞り、さらに Phase B 書き出しと依存が逆転しないよう **最小限の `AudioMixer`** をこの段階で構築する。

- **主な対象ファイル（計画）**:
  - `src/media/decoder.rs`（OS 非依存 `MediaBackend` / `VideoDecoder` / `AudioDecoder` トレイト）
  - `src/media/backends/open_sw.rs`（純粋 Rust / ロイヤリティフリー SW 経路：`matroska-demuxer` + `rav1d` [AV1：`dav1d` C ABI 呼び出し境界に `#[allow(unsafe_code)]`＋`// SAFETY:` を隔離] + `symphonia` [features = `flac`, `vorbis`, `pcm`/`wav` のみ]）
  - `src/media/backends/windows_mf.rs`（Windows Media Foundation / D3D11VA デコーダ：`#[cfg(target_os = "windows")]`、`#[allow(unsafe_code)]`＋`// SAFETY:` 必須）
  - `src/media/frame_cache.rs`（先読みリングバッファ・シーク世代番号キャンセル・LRU キャッシュ）
  - `src/audio/mixer.rs`（**最小 `AudioMixer`**：複数クリップの線形加算・クリップ音量・マスター音量・指定区間 PCM 抽出）
  - `src/audio/playback.rs`（`cpal` 出力・`rubato` リサンプル・`PlaybackClock` [`AudioMasterClock` と `SystemPlaybackClock` の自動切替]）

### 設計仕様
- **代替時計を含む `PlaybackClock`**:
  - 音声ストリームがアクティブな再生中は `AudioMasterClock`（ハードウェア消費サンプル数基準）を採用する。
  - 音声クリップなし・全無音・一時停止・シーク直後・音声デバイス未接続環境では、`std::time::Instant` ベースの単調時計 `SystemPlaybackClock` へシームレスにフォールバックする。
- **最小 `AudioMixer` の前倒し**:
  - `AudioMixer::render_pcm_range(doc, start_sample, frame_count, sample_rate)` を純粋関数として先に用意し、`cpal` 再生も将来の `PhaseB-4` 書き出しも同一の `AudioMixer` を使う（速度変更や VST3 は Phase C でこの上に拡張）。
- **環境依存テスト方針**:
  - 純粋 Rust テスト（WAV/FLAC/Vorbis + プログラム生成 AV1/RGBA テスト、モックデコーダ、`AudioMixer`、`PlaybackClock`）は通常テストとして常時実行。
  - 実 Windows Media Foundation ハードウェアや実オーディオ出力デバイスを叩くテストには `#[ignore = "requires OS media hardware / audio output device"]` を付与し、ヘッドレス CI でも `cargo test --all-targets --all-features` が必ず通るようにする。

### 動作確認とテストの実施
1. **単体テスト**（`src/media/frame_cache.rs`, `src/audio/mixer.rs`, `src/audio/playback.rs` 各末尾）:
   - `PlaybackClock` の音声有/無切替時における時刻単調性・シークリセットテスト。
   - 最小 `AudioMixer` による複数 WAV トラック加算と `source_offset` 途中再生テスト。
   - シーク世代番号による旧先読みジョブ破棄テスト。
2. **結合テスト**:
   - `tests/phase_a_media_playback.rs`
   - `tests/phase_a_media_playback/mod.rs`
   - `tests/phase_a_media_playback/decode_and_sync.rs`
   （`matroska-demuxer` + `rav1d` + `symphonia` SW 経路またはテスト用最小ストリームでのフレーム取得・A/V 同期・シーク検証。Windows MF 実機検証は `#[ignore]` テストとして併設）
3. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` 通過。

---

## PhaseA-5：GPUI 1 ウィンドウ骨格と `PreviewSurfaceBridge` 初期再生結合

### 目的と対象モジュール
`PhaseA-0` スパイクで検証済みの `gpui` + `gpui-component` を使い、1 ウィンドウ（カスタムメニューバー・中央プレビュー・下部タイムライン・右 1/5 タブパネル）を構築し、動画を開いて滑らかに再生できる状態を完成させる。

- **主な対象ファイル（計画）**:
  - `src/ui/window.rs`（ルートウィンドウ、DaVinci Resolve / VS Code 風ダークテーマ）
  - `src/ui/menu_bar.rs`（Windows/Linux 向けカスタム GPUI メニューバー：ファイルを開く、プロジェクト保存/読込）
  - `src/ui/preview_panel.rs`（中央プレビュー＋再生/停止/シークバー＋`AssetStatus::Missing` 時の「Media Offline」表示）
  - `src/ui/right_panel.rs`（初期幅 1/5・リサイズ可・タブ切り替え：AI チャット枠／クリップ情報枠）
  - `src/ui/timeline_panel.rs`（下部タイムライン初期表示＋再生ヘッド）
  - `src/ui/surface_bridge.rs`（`PreviewSurfaceBridge` トレイト：`Arc<RenderImage>` ダブルバッファ経路を初期標準実装とし、スパイクで実証された場合のみゼロコピー実装を有効化）
  - `src/main.rs`（GPUI アプリケーション起動接続）

### 動作確認とテストの実施
1. **単体テスト**（`src/ui/` 内状態管理・レイアウト計算ロジック末尾）:
   - 右パネル幅計算（初期 1/5、クランプ境界）、再生状態機械（Stopped / Playing / Paused / Seeking）テスト。
2. **結合テスト**:
   - `tests/phase_a_app_wiring.rs`
   - `tests/phase_a_app_wiring/mod.rs`
   - `tests/phase_a_app_wiring/open_and_play_state.rs`
   （GUI 非依存の `AppController` へ動画・欠損アセットを読み込み、`ProjectDocument` 更新→再生コマンド→`PlaybackClock` 経過後の表示フレーム進行・Missing プレースホルダ判定を自動検証）
3. **手動確認（Phase A 完了ゲート）**:
   - `cargo run` で実ウィンドウを起動し、動画を読み込んで音声付きで滑らかに再生・シーク・一時停止できること、4 領域の 1 ウィンドウ構成とメニューバーが機能することを確認。
4. **必須チェック**:
   - `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo test --all-targets --all-features` ゼロ警告・ゼロエラー。
