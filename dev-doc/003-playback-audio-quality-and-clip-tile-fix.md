# [003] タイムラインクリップ表示・再生FPS・音声再生・プレビュー画質の抜本改修

- **ステータス**: Implemented
- **作成日**: 2026-11-16
- **更新日**: 2026-11-16
- **関連ドキュメント**: [`dev-doc/002-aviutl-ui-redesign/00_overview.md`](./002-aviutl-ui-redesign/00_overview.md), [`docs/user-guide.md`](../docs/user-guide.md)

## 1. 概要・目的

実機動作確認（スクリーンショットおよび手動操作）で判明した以下 4 件の重大な不具合・品質劣化を解消する。

1. **タイムラインのクリップが細い水平線として描画される**: AviUtl2 風の塗りつぶし矩形リボン／タイル（クリップ名・トリムハンドル・中間点・波形付き）ではなく、トラック上端に約 `4px` の黄色い横線のみが表示される。
2. **再生時の動作が 1〜2fps のカクカクになる**: 再生ボタン押下後、再生ヘッドおよびプレビュー描画が著しく遅延する。
3. **音声が再生されず、マスター波形も無音のまま**: 動画（MP4 等）読込・再生時にスピーカーから音が出ず、プレビュー下部の緑色マスター波形ストリップがフラットな破線のままになる。
4. **プレビュー映像の画質が荒い**: 高解像度動画を読み込んでもプレビュー画面が粗くぼやけて表示される。

対象ユーザーは `sea-movie-creator` を用いて動画・音声・テロップ編集を行うすべてのユーザー。

## 2. スコープ

### 対象（In Scope）

- `src/ui/timeline/track_view.rs` および `src/ui/timeline/waveform_view.rs`:
  - クリップブロックの高さ・絶対配置崩れの修正、および動画クリップ内包音声（`ClipKind::Video { audio: Some(_), .. }`）の波形表示対応。
- `src/media/decoder.rs`, `src/media/backends/windows_mf.rs`, `src/media/backends/open_sw.rs`, `src/media/backends/mod.rs`:
  - `decode_frame_at_seconds` における毎フレーム強制キーフレームシークの撤廃（順方向連続再生時のシーケンシャルデコード最適化）と、スキップフレーム時のピクセルコピー抑止。
  - Windows Media Foundation における音声ストリームプローブ（`has_audio`, `audio_sample_rate`, `audio_channels`, `audio_codec`）と `AudioDecoderStream` 実装、および `Auto` バックエンドでの Symphonia 失敗時 MF フォールバック。
- `src/audio/playback.rs`, `src/audio/mixer.rs`, `src/ui/app_controller.rs`:
  - `cpal` 出力デバイスのサンプルフォーマット（`F32` / `I16` / `U16` / `I32`）および実サンプルレートと `PlaybackClock` の同期、`play()` 時の音声出力再試行。
- 非同期映像先読みデコードワーカー（`src/media/prefetch.rs`）と `AppController` 統合、単一未編集レイヤーの不要な GPU リードバック回避、GPU スプライトアトラス解放（`window.drop_image`）。
- プレビュー解像度上限（`PREVIEW_CANVAS_MAX_WIDTH: 480 -> 1920`）、フレームキャッシュ上限（`64 MiB -> 256 MiB`）、CPU フォールバックのバイリニア補間化。

### 対象外（Out of Scope）

- タイムライン・インスペクタ全体の配色やメニュー構成の再設計（`002-aviutl-ui-redesign` で完了済み）。
- 音声出力プラグイン（VST/別プロセスホスト）の大幅刷新。
- 動画書き出しエンコーダ仕様変更（ただし `VideoDecoder` の順方向シーク回避により書き出し速度も副次的に向上する）。

## 3. 要件・制約

### 機能要件
- タイムライン上のクリップはトラック行の上下 `2px` 余白を除いた矩形リボン（例: 高さ `30px` 行なら `26px`）として描画され、タイトル・リサイズハンドル・中間点ダイヤ・音声波形が表示されること。
- 動画クリップ・音声クリップを再生した際、実時間（例: `30fps` / `60fps`）に追従して再生ヘッド・プレビュー・音声が途切れなく同期再生されること。
- `windows_mf` でデコードした MP4/MOV 等を含む動画内包音声および単体音声ファイルの音声が正しく再生され、プレビュー下部のマスター波形・タイムラインクリップ波形に描画されること。
- プレビュー映像はプロジェクト解像度（最大 `1920px` 幅 / フル HD 相当）まで劣化なく鮮明に表示されること。

### 非機能要件・制約
- `Cargo.toml` の厳しいリント規約（`unsafe_code = "deny"`, `missing_docs = "deny"`, `clippy::pedantic = "deny"`, `unwrap_used` / `expect_used` / `panic` = `"deny"`, `too_many_lines` <= 100）を遵守すること。
- `unsafe` は既存どおり `src/media/backends/windows_mf.rs` 内の Windows COM / Media Foundation FFI 境界のみに限定し、全ブロックに `// SAFETY:` コメントを付すこと。
- 自動テスト中に実 GUI ウィンドウや実音声デバイスへ依存し過ぎず、ヘッドレス環境でも全単体・結合テストが安定して高速に完走すること。

## 4. アーキテクチャ / 設計詳細

### 4.1 タイムラインクリップの表示崩れ（線状化）修正

#### 原因
- `src/ui/timeline/track_view.rs` の `render_clip_block` で `.absolute().top(px(2.)).bottom(px(2.)).left(px(left_px)).w(px(width_px))` を指定した直後に `.relative()` を呼び出しており、GPUI の `Position::Absolute` が `Position::Relative` で上書きされていた。
- さらに `gpui-component` の `ContextMenuExt::context_menu` 自体も要素を `relative` に上書きする仕様を持つ。
- クリップ内部の子要素（セグメント・中間点・ハンドル・ラベル）はすべて `.absolute()` のため、通常フロー内容の高さが `0px` となり、`border_2()` の上下枠線（約 `4px`）だけが黄色い横線として見えた。

#### 解決策
- 外側コンテナ `outer` に `.absolute().top(px(2.)).bottom(px(2.)).left(px(left_px)).w(px(width_px)).rounded(px(3.)).bg(rgba(palette.fill)).border_color(border_color).overflow_hidden()`（および `border_1` / `border_2`）とイベントハンドラを持たせる。
- 右クリックメニュー `attach_clip_context_menu` には、内側コンテナ `.size_full().relative()`（内部にセグメント・波形・トリムハンドル・ラベル・中間点を格納）を渡し、外側の `.absolute()` 配置・伸縮と内側の `relative` 基点を分離する。
- `src/ui/timeline/waveform_view.rs` と `track_view.rs` を見直し、`ClipKind::Audio` に加えて `ClipKind::Video { audio: Some(_), .. }` でも下半分に音声波形を描画する。

### 4.2 再生 FPS 低下（1〜2fps）と描画遅延の解消

#### 原因
1. `VideoDecoder::decode_frame_at_seconds`（`src/media/decoder.rs`）が **毎フレーム無条件で `self.seek_seconds(clamped)?` を呼んでいた**。キーフレーム間隔（1〜5秒）先頭へ巻き戻してから現在フレームまで全フレーム再デコードするため、再生時に O(GOP長) の計算負荷が毎フレーム発生していた。
2. `WindowsMfReader::decode_next_frame` 等が目的時刻までのスキップフレームについても毎回フルサイズ BGRA8 ピクセル配列への変換・コピーを行っていた。
3. `AppController::tick_playback()`（UI スレッド 16ms タイマー）内で「映像デコード」「wgpu コンポジット＋同期 `readback_texture_bgra8` 待機」「GPU 画像データ複製」を直列・同期実行していた。
4. `DEFAULT_FRAME_CACHE_BYTES` が `64 MiB`（1080p BGRA8 でわずか約 7 フレーム）しかなくキャッシュミスが多発していた。
5. `SeaEditorWindow::render` が `surface_bridge.drain_pending_drops()` を `window.drop_image(...)` へ渡しておらず、古いプレビューフレームが GPUI スプライトアトラスへ蓄積していた。

#### 解決策
1. **シーケンシャルデコード状態の保持と高速スキップ**:
   - `src/media/decoder.rs` に共通の順方向デコード追跡器 `SequentialVideoTracker`（直前デコード時刻および 1 フレーム分の先行デコード済みルックアヘッドを保持）を追加し、`WindowsMfReader` / `OpenSwVideoDecoder` に組み込む。
   - 目標時刻 `target` が現在位置から同一方向かつ近傍（`-0.004s ..= +2.5s`）であれば `seek_seconds` を呼ばず、現在位置から順方向に最小限のデコードだけで前進する。逆方向シークや `> 2.5s` のジャンプ時のみ `seek_seconds` を呼ぶ。
   - 早送りスキップ中に捨てるフレームについては、`WindowsMfReader::read_next_raw_sample` 等でピクセル展開（BGRA8 コピー・`Vec` 確保）を省き、最終一致フレームのみピクセル配列へ変換する。
2. **非同期バックグラウンド先読みデコードワーカー（`src/media/prefetch.rs`）**:
   - 再生中（`PlaybackState::Playing`）に UI スレッドをブロックしないよう、専用ワーカースレッド `VideoPrefetchWorker` を新設する。
   - UI スレッドは現在の必要フレーム・前方先読みウィンドウ（例: 現在フレームから `+16` フレーム）をワーカーへリクエストし、完了済みフレームをロックフリー／軽量チャンネルで回収して `FrameCache` へ格納する。
   - キャッシュ命中時はデコードを一切行わず即座に描画する。再生開始直後やシーク直後の未到着フレームも、UI スレッドでのブロッキング待機（再生中）を避けつつ直近キャッシュまたは軽量フォールバックで再生ヘッドを止めない（停止／一時停止中のシークや単体テストでは同期デコードを維持する）。
   - `DEFAULT_FRAME_CACHE_BYTES` を `256 MiB` に拡張する。
3. **単一未編集レイヤーの GPU リードバック回避 & 所有権ムーブ**:
   - アクティブ映像／画像レイヤーが 1 つだけで、位置・拡縮・回転・不透明度・ブレンドモード・色調補正がデフォルト（恒等変換）、かつプロジェクト解像度比と一致する未編集クリップ再生時は、1920x1080 の重い `wgpu` ステージングリードバック（`device.poll(Wait)`）を省き、デコード済み `VideoFrameBuffer`（`Arc<Vec<u8>>` または所有権ムーブ）を直接 `RenderImageSurfaceBridge::submit_bgra_frame_owned` へ渡せる高速パスを用意する。
   - 多レイヤー合成、位置／拡大／回転変更、カラー調整、テロップ表示時は従来通り `WgpuCompositor` で正確に GPU 合成する。
4. **GPUI スプライト解放の接続**:
   - `SeaEditorWindow::render` 内で `self.controller.surface_bridge.drain_pending_drops()` を取り出し、`let _ = window.drop_image(old);` を毎フレーム実行して GPU テクスチャリークを防ぐ。

```mermaid
flowchart LR
    UI["UI スレッド (16ms tick_playback)"] -->|キャッシュヒット| Bridge["RenderImageSurfaceBridge"]
    UI -->|先読み要求 frame..frame+16| Worker["VideoPrefetchWorker (専用スレッド)"]
    Worker -->|順方向デコード (seekなし)| Decoder["WindowsMfReader / OpenSwVideoDecoder"]
    Worker -->|完了フレーム返却| Cache["FrameCache (256 MiB)"]
    Cache --> UI
    Bridge -->|RenderImage + drop_image| GPUI["GPUI Viewport"]
```

### 4.3 音声プローブ・デコード・デバイス再生・波形修復

#### 原因
1. `src/media/backends/windows_mf.rs` の `read_stream_metadata()` が **`has_audio: false` をハードコード**しており、`MF_SOURCE_READER_FIRST_AUDIO_STREAM` を検査していなかった。結果として MP4 動画インポート時に `ClipKind::Video { audio: None }` となり、再生音もマスター波形も無音化していた。
2. `src/media/backends/mod.rs` の `open_audio_decoder` が `Auto` 時でも `open_sw::open_audio`（Symphonia）しか呼ばず、Symphonia 非対応コンテナ／コーデック時に Windows Media Foundation 音声デコーダへフォールバックする実装がなかった。
3. `src/audio/playback.rs` の `start_audio_output` が `SampleFormat::F32` 以外の既定デバイス（例: `I16` / `U16`）でエラーを返していた。またデバイス側サンプルレート（例: `44100 Hz`）を `PlaybackClock`（初期値 `48000 Hz`）やミキサへ反映しておらず、再生速度や音声ピッチずれの要因になっていた。`play()` 時に未起動の音声出力を再接続する処理もなかった。

#### 解決策
1. **Windows Media Foundation 音声プローブと PCM デコーダ**:
   - `windows_mf.rs` で映像ストリームだけでなく `MF_SOURCE_READER_FIRST_AUDIO_STREAM` の有無・サンプルレート（`MF_MT_AUDIO_SAMPLES_PER_SECOND`）・チャンネル数（`MF_MT_AUDIO_NUM_CHANNELS`）を検出し、`MediaStreamInfo` へ正確に設定する。音声のみのメディア（映像ストリームなし＋音声ストリームあり）も `probe` で扱えるようにする。
   - `WindowsMfAudioDecoder`（`AudioDecoderStream` 実装）を追加し、`MF_SOURCE_READER_FIRST_AUDIO_STREAM` の出力メディアタイプを `MFAudioFormat_Float`（32-bit PCM）に設定して全サンプルをインターリーブ `f32` で読み出す。
   - `backends/mod.rs` の `open_audio_decoder` で `Auto` 時は `open_sw::open_audio` -> 失敗時に `windows_mf::open_audio` へフォールバックし、`WindowsMediaFoundation` / `NativeHardware` 指定時も `windows_mf::open_audio` を利用できるようにする。
2. **音声デバイス互換性・クロック同期・リトライ**:
   - `PlaybackClock` に `set_sample_rate(&self, sample_rate: u32)` を追加し、`sample_position` を現在時刻不変のまま新レートへ換算する。
   - `AppController::load_audio_into_mixer` は `clock.sample_rate()` を出力ターゲットサンプルレートとしてリサンプリングし、`try_start_audio_output()` で実デバイスのサンプルレートへ同期する。
   - `start_audio_output` で `SampleFormat::F32` に加えて `SampleFormat::I16`・`SampleFormat::U16`・`SampleFormat::I32` 出力ストリームをサポートし、クリップ防止付き変換でデバイスへ書き出す。
   - `AppController::play()` で `self.audio_output_requested && self.audio_handle.is_none()` の場合に `self.try_start_audio_output()` を再試行し、失敗時は単調時計フォールバックへ切り替える。

### 4.4 プレビュー画質向上

#### 原因
- `src/ui/app_controller.rs` の `PREVIEW_CANVAS_MAX_WIDTH` が `480` に固定されており、1920x1080 プロジェクトでも `480x270` へ大幅縮小レンダリングしてからウィンドウ幅へ引き伸ばしていた。
- `src/ui/surface_bridge.rs` の CPU フォールバック合成関数 `blit_scaled_layer` が最近傍補間（Nearest-neighbor）のみで斜め線や文字のジャギーが目立っていた。

#### 解決策
- `PREVIEW_CANVAS_MAX_WIDTH` を `480` から `1920` へ引き上げ、プロジェクト解像度（最大 フル HD 幅）で鮮明に合成・転送する。
- `blit_scaled_layer` をバイリニア補間へ改善し、GPU フォールバック時もエッジが滑らかになるようにする（さらに単一等倍コピー相当ケースの高速パスも備える）。

## 5. インターフェース / API 仕様

- `src/media/backends/windows_mf.rs`
  - `pub fn open_audio(path: &Path) -> Result<Box<dyn AudioDecoderStream>, MediaError>`（`#[cfg(target_os = "windows")]`）
- `src/media/prefetch.rs`（新規モジュール、`src/media/mod.rs` から公開）
  - `pub struct VideoPrefetchWorker`: バックグラウンドデコーダスレッド管理・先読み要求・完了フレーム回収
  - `pub struct PrefetchRequest { pub key: FrameCacheKey, pub path: PathBuf, pub target_seconds: f64 }`
- `src/audio/playback.rs`
  - `impl PlaybackClock`: `pub fn set_sample_rate(&self, sample_rate: u32)`
  - `start_audio_output`: `SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::I32` 対応
- `src/ui/surface_bridge.rs`
  - `impl RenderImageSurfaceBridge`: `pub fn submit_bgra_frame_owned(&mut self, frame: VideoFrameBuffer)`（既存 `&VideoFrameBuffer` 版も互換維持）

## 6. エラーハンドリング・エッジケース

- **非常に狭いクリップ（幅 2〜6px 等）**: 外側コンテナが `.absolute().top(px(2.)).bottom(px(2.)).w(px(width_px))` であるため、幅が極小でも縦長リボンとして視認・選択可能。
- **逆方向スクラブや別ファイルへのシーク**: `VideoPrefetchWorker` の世代番号（`generation: u64`）およびシーク時キュー破棄により、古い再生中フレームがシーク後プレビューを上書きしないようにする。
- **音声のない動画・壊れた音声ストリーム**: `MF_SOURCE_READER_FIRST_AUDIO_STREAM` 読取が失敗または非存在の場合はエラー終了せず `has_audio: false` として映像のみ正常に読み込む。
- **音声デバイス不在の CI／ヘッドレス環境**: `start_audio_output` がデバイス未検出エラーを返しても `PlaybackClock` の単調時計フォールバックにより映像再生自体は継続できる。

## 7. テスト計画・受け入れ基準

### 単体テスト・結合テスト
- **クリップ形状／波形抽出**:
  - `ClipKind::Video { audio: Some(_), .. }` からタイムライン波形バー生成用パラメータが抽出できるテスト（`waveform_view.rs`）。
  - `render_clip_block` 関連レイアウトヘルパの単体テスト。
- **デコーダ順方向最適化**:
  - モックまたは実テストデコーダで、順方向連続 `decode_frame_at_seconds` が `seek_seconds` を毎フレーム呼ばずに連続前進する検証、および逆方向・遠距離ジャンプ時に `seek_seconds` が呼ばれる検証（`src/media/decoder.rs`）。
- **音声プローブ・変換・サンプルレート同期**:
  - `PlaybackClock::set_sample_rate(44_100)` 時にタイムライン秒・フレーム位置が保存されるテスト（`src/audio/playback.rs`）。
  - `f32 -> i16` / `f32 -> u16` / `f32 -> i32` 出力サンプル変換関数の境界値・クリッピングテスト（`src/audio/playback.rs`）。
  - `windows_mf` 音声サンプルデコード（`F32` / `I16` 変換）単体テスト `append_pcm_bytes_decodes_f32_and_i16_samples`（`src/media/backends/windows_mf.rs`）。
- **プレビュー画質・キャッシュ・サーフェス解放**:
  - `preview_dimensions` が `1920x1080` プロジェクトで `(1920, 1080)` を返すテスト `preview_dimensions_keep_full_hd_sharpness`（`src/ui/app_controller.rs`）。
  - `submit_bgra_frame_owned` とバイリニア拡大サンプリングの検証テスト `owned_submit_and_bilinear_sampling_behave_cleanly`（`src/ui/surface_bridge.rs`）。
  - `VideoPrefetchWorker` の世代無効化・保留ドロップ検証テスト `invalidate_generation_drops_pending_jobs_and_tracks_state`（`src/media/prefetch.rs`）。

### 完了条件（Definition of Done）
- `cargo fmt --all -- --check` が終了コード 0
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` が終了コード 0
- `cargo build --workspace --all-targets` が終了コード 0
- `cargo test --workspace --all-features` が全合格
- `cargo build --workspace --release` により最新バイナリが生成され、`dist/sea-movie-creator-v0.1.0-alpha-windows-x86_64/` へ反映されること

## 8. 未決事項・検討メモ（Open Questions）

- [x] 単一未編集動画再生時に GPU 合成パイプラインを完全バイパスするか、`WgpuCompositor` 経由と使い分けるか → **決定**: 1レイヤー・デフォルト視覚パラメータ・プロジェクト縦横比一致・テロップ無し時のみ直接フレームを `RenderImageSurfaceBridge` へ送出し、少しでも変換・オーバーレイがある場合は自動的に `WgpuCompositor` を用いる（品質・互換性と 1080p 再生速度を両立）。
- [x] GPUI `ContextMenu` の `.relative()` 上書き回避方法 → **決定**: 位置・サイズ決定用の `.absolute()` 外側 `div` と、右クリックメニュー付きの `.size_full().relative()` 内側 `div` の 2 層構造にする。
