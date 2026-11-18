# 外部音声プラグイン（VST3 / CLAP）互換・開発仕様ガイド

`sea-movie-creator` はサードパーティ製の **VST3** および **CLAP** 音声エフェクトプラグインに対応しています。本体プロセス（`unsafe_code = "deny"`）の安全性と安定性を守るため、すべてのネイティブプラグインは専用ワーカープロセス **`sea-audio-host`**（[`plugins_src/sea-audio-host/`](../../plugins_src/sea-audio-host/src/main.rs)）内で隔離実行されます。

このドキュメントは、自作または既存の VST3 / CLAP プラグインを `sea-movie-creator` 上で正しく動作させるための**探索規則**・**音声処理契約**・**現時点での非対応機能**をまとめた技術仕様です。

---

## 1. 隔離アーキテクチャと安全性

1. **メインプロセスは動的ライブラリをロードしない**
   - スキャン（[`src/audio/plugins/scanner.rs`](../../src/audio/plugins/scanner.rs)）は `LoadLibrary` / `dlopen` を行わず、拡張子・バンドル構造・実行バイナリのマジックバイト（PE `MZ` / ELF `\x7fELF` / Mach-O）のみを静的に検査します。
2. **クリップ・スロット単位の別プロセス隔離**
   - 有効化されたプラグインスロットごとに [`IsolatedAudioPluginHost`](../../src/audio/plugins/isolated_host.rs) が `sea-audio-host` を起動し、標準入出力 NDJSON（[`audio_ipc.rs`](../../plugins_src/sea-export-protocol/src/audio_ipc.rs)）と共有メモリ `mmap`（2 スロット：`input_slot = 0`, `output_slot = 1`）経由で PCM ブロックをやり取りします。
3. **クラッシュ時の自動バイパス**
   - プラグイン側がクラッシュ（SIGSEGV / 非ゼロ終了）または不正応答を返した場合、ホストは即座にそのスロットをバイパス（`is_bypassed() == true`）へ切り替え、元のドライ音声を壊さず再生・書き出しを継続します。

---

## 2. 対応フォーマットと検索パス

### 2.1 対応規格

- **対応**: **VST3**（`.vst3`）、**CLAP**（`.clap`）
- **非対応**: **VST2**（Steinberg 社の SDK/ライセンス配布終了方針により対象外）

### 2.2 探索ディレクトリ

環境変数で追加ディレクトリを指定できるほか、各 OS の標準インストール先を自動走査します。

| 種別 | 環境変数（複数指定可） | OS 標準の検索先 |
| :--- | :--- | :--- |
| **VST3** | `SEA_VST3_PATH` | **Windows**: `%ProgramFiles%\Common Files\VST3`, `%LOCALAPPDATA%\Programs\Common\VST3`<br>**macOS**: `/Library/Audio/Plug-Ins/VST3`, `~/Library/Audio/Plug-Ins/VST3`<br>**Linux**: `/usr/lib/vst3`, `/usr/local/lib/vst3`, `~/.vst3` |
| **CLAP** | `SEA_CLAP_PATH` | **Windows**: `%ProgramFiles%\Common Files\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`<br>**macOS**: `/Library/Audio/Plug-Ins/CLAP`, `~/Library/Audio/Plug-Ins/CLAP`<br>**Linux**: `/usr/lib/clap`, `/usr/local/lib/clap`, `~/.clap` |

### 2.3 バンドル構造要件

スキャナは以下のいずれかの配置を認識します（[`resolve_bundle_binary_path`](../../src/audio/plugins/scanner.rs#L212-L238)）。

- 単一バイナリファイル（例: `MyEffect.clap`, `MyEffect.vst3`）
- バンドルディレクトリ内の OS 別バイナリ：
  - Windows: `<bundle>/Contents/x86_64-win/<binary>`
  - Linux: `<bundle>/Contents/x86_64-linux/<binary>`
  - macOS: `<bundle>/Contents/MacOS/<binary>`

※ 上記ディレクトリにネイティブ実行形式ヘッダ（PE / ELF / Mach-O）を持たないファイルしかない場合、スキャン警告として除外されます。

---

## 3. 音声処理・パラメータの互換契約

プラグイン開発者が「自分の VST3 / CLAP がどの仕様で呼ばれるか」を把握するための実行時契約です。

| 項目 | ホスト側の仕様（`sea-audio-host`） |
| :--- | :--- |
| **チャンネル構成** | **ステレオ 2ch のみ**（VST3 では `SPEAKER_STEREO = 0x3`、入出力各 1 バス） |
| **サンプル形式** | **32-bit 浮動小数点（`f32` / `kSample32`）**、非インターリーブ（L/R 2 チャンネル分離バッファ）へ変換してプラグインへ供給 |
| **最大ブロック長** | **最大 `4096` フレーム / ブロック**（`DEFAULT_MAX_BLOCK_FRAMES = 4096`） |
| **サンプリングレート** | プロジェクト再生・書き出し側のサンプルレート（例: `48000` Hz） |
| **処理モード（VST3）** | `ProcessModes::kRealtime` で初期化し `IAudioProcessor::process` を呼び出し |
| **パラメータ取得・変更（VST3）** | 単一コンポーネントからキャスト可能な `IEditController` を参照し、パラメータの `title` をキー、`0.0 ..= 1.0` の正規化値（`setParamNormalized`）として扱う（[`vst3_host.rs`](../../plugins_src/sea-audio-host/src/backends/vst3_host.rs)） |
| **パラメータ取得・変更（CLAP）** | `PluginParams`（`clack_extensions::params`）から `info.name` をキーとして列挙し、`ParamValueEvent` で `[min_value ..= max_value]` の実値を送信（[`clap_host.rs`](../../plugins_src/sea-audio-host/src/backends/clap_host.rs)） |

### 現行バージョンの非対応機能（制約事項）

外部音声プラグインを開発・検証する際は、以下の制限に注意してください。

- **独自プラグイン GUI ウィンドウ（`IPlugView` / CLAP GUI 拡張）は未対応**: 本ソフトはプラグイン側のカスタムウィンドウを開きません。パラメータはテキスト／数値リストとして公開される項目のみを編集・オートメーション可能です。
- **パラメータ名の重複に注意**: VST3 では `ParameterInfo.title`、CLAP では `ParamInfo.name` を `BTreeMap<String, Id>` のキーとして引きます。同一プラグイン内で重複するパラメータ名があると後勝ちで上書きされます。
- **VST3 のコンポーネント分離構成**: `IComponent` と別インスタンスでしか `IEditController` を提供しない VST3（単一コンポーネントで `cast::<IEditController>()` できない構成）では、`edit_controller` が `None` となりパラメータ操作がエラーになります（音声処理自体は動作します）。
- **MIDI / インストゥルメント・サイドチェイン・サラウンド**: オーディオエフェクト（1 ステレオ入力 → 1 ステレオ出力）のみを想定しています。

---

## 4. `sea-audio-host` 内部プロトコルとテスト用内蔵 DSP

`sea-audio-host` 単体の動作確認や、`AudioMixer` との結合テストでは外部バイナリ不要のテスト用内蔵 DSP **`sea.test.gain`**（`WireAudioPluginFormat::BuiltinTest`、パラメータ名 `"gain"` 範囲 `0.0 ..= 4.0`）が同梱されています（[`builtin_dsp.rs`](../../plugins_src/sea-audio-host/src/builtin_dsp.rs)）。

`sea-audio-host` の `stdin` / `stdout` NDJSON スキーマは [`plugins_src/sea-export-protocol/src/audio_ipc.rs`](../../plugins_src/sea-export-protocol/src/audio_ipc.rs) に定義されており、主に以下のコマンドペアで通信します。

- `load_plugin` → `loaded`（または `error`）
- `describe_plugin` → `described`
- `set_parameter` → `parameter_ack`
- `process_block`（`input_slot: 0`, `output_slot: 1`, `frame_count`, `channels`）→ `block_ack`
- `simulate_crash`（クラッシュ分離テスト用、終了コード `101` で即時終了）
- `shutdown` → `shutdown_ack`
