# 多言語ファイル (`lang/*.json`) 作成ガイド

`sea-movie-creator` は、ソースコードの変更や再ビルドを行わずに **`lang/` フォルダへ JSON ファイルを 1 つ追加するだけ**で UI の表示言語を追加・カスタマイズできる仕組み（i18n）を備えています。

有志による新しい言語への翻訳ファイル作成や、既存翻訳の改善・方言パック等の作成にご活用ください。

---

## 1. クイックスタート（5 分で新しい言語を追加する）

### ステップ 1: ベースファイルをコピーする

既存の英語パック [`lang/en.json`](../lang/en.json) または日本語パック [`lang/ja.json`](../lang/ja.json)（配布パッケージの場合は `lang/` フォルダ内）をコピーし、**`<言語コード>.json`** にリネームします。

- 例: 韓国語 → `lang/ko.json`
- 例: 簡体字中国語 → `lang/zh-cn.json`
- 例: フランス語 → `lang/fr.json`

### ステップ 2: メタデータと翻訳文を編集する

文字コードは **UTF-8** で保存してください（BOM 付き UTF-8 も許容されます）。

```json
{
  "code": "ko",
  "name": "한국어",
  "strings": {
    "app.ready": "준비 완료",
    "menu.file": "파일",
    "menu.file.new": "새 프로젝트",
    "menu.file.open": "열기..."
  }
}
```

- **`code`**: 小文字で正規化される言語識別コード（省略時はファイル名の拡張子なし部分、例: `ko.json` → `ko`）。
- **`name`**: メニューやプロジェクトタブに表示される言語名（日本語なら `"日本語"`、韓国語なら `"한국어"` など母語表記を推奨。省略時は `code` が使われます）。
- **`strings`**: `"翻訳キー": "翻訳テキスト"` の連想配列（現在 212 キー）。

### ステップ 3: アプリを再起動せず即座に読み込む

作成した `.json` を `lang/` に置いたら、アプリを開いたまま次のいずれかで即時反映できます。

1. 上部メニュー **`設定` → `言語 (Language)` → `lang フォルダを再読み込み`** をクリック
2. 右パネル **`プロジェクト` タブ → `表示言語 (lang/)` 内の `再読込`** ボタンをクリック

再読込後に **`設定 → 言語 (Language)`** または **`プロジェクト` タブ** に新しい言語ボタンが自動追加され、クリックするとメニュー・各パネル・モーダルが即座に切り替わります。

---

## 2. ファイルの配置場所と読み込み優先順位

`sea-movie-creator` は、組み込み言語（`ja`・`en`）をベースとして、以下の順番で外部 `lang/*.json` を走査します（あとから読み込まれた場所が同じ `code` のキーを上書きマージします）。

| 順番（優先度） | 配置ディレクトリ | 用途 |
| :--- | :--- | :--- |
| 1（最低） | アプリ組み込み (`include_str!`) | 標準 `ja`・`en` のベース定義 |
| 2 | `<作業ディレクトリ>/lang/` | リポジトリ直下で開発実行する場合 |
| 3 | `<実行ファイル隣接>/lang/` | 配布版（`sea-movie-creator.exe` と同じ階層の `lang/`） |
| 4（最高） | OS ユーザー設定フォルダ配下の `lang/` | アプリ更新で上書きされない個人用カスタム言語 |

OS ごとのユーザー設定ディレクトリ (`user_config_file_path("lang")`):

- **Windows**: `%APPDATA%\sea-movie-creator\lang\`
- **Linux (`XDG_CONFIG_HOME` 設定時)**: `$XDG_CONFIG_HOME/sea-movie-creator/lang/`
- **macOS / Linux（標準）**: `~/.config/sea-movie-creator/lang/`

> [!TIP]
> 選択した言語コードは同じユーザー設定ディレクトリ直下の **`language.json`**（例: `{"active_code": "en"}`）へ自動保存され、次回起動時も維持されます。

---

## 3. JSON フォーマット仕様とフォールバック

### 対応している 2 種類の記述形式

1. **標準形式（推奨）**: `"code"`, `"name"`, `"strings"` を持つオブジェクト
   ```json
   {
     "code": "es",
     "name": "Español",
     "strings": {
       "menu.file": "Archivo",
       "menu.edit": "Editar"
     }
   }
   ```
2. **フラット形式（簡易記述）**: トップレベルに直接 `"キー": "翻訳文"` を並べる形式（`code` / `name` / `$schema` 以外の文字列エントリが翻訳マップとして読み込まれます）。

### 部分翻訳と自動フォールバック

すべてのキーを最初から翻訳し切る必要はありません。アプリは次の順で文言を解決します。

1. 現在選択中言語の `strings[key]`
2. **`en`**（英語）の `strings[key]`
3. **`ja`**（日本語）の `strings[key]`
4. キー文字列そのもの（例: `"menu.file"`）

また、たとえば `ja.json` の一部表現だけ自分好みに変えたい場合は、変えたいキーだけを書いた小さなオーバーライド用 JSON を `%APPDATA%\sea-movie-creator\lang\ja.json` に置くことも可能です（既存キーへ上書きマージされます）。

---

## 4. プレースホルダ変数 (`{name}`) のルール

一部の翻訳キーには、実行時に数値や状態が差し込まれる **`{変数名}`** が含まれています。

> [!IMPORTANT]
> `{total}` や `{frame}` などの**中括弧内の英単語は翻訳・削除しないでください**（並び順は語順に合わせて自由に入れ替えて問題ありません）。

現在プレースホルダを使用するキーと変数の意味は次のとおりです：

| キー | 変数 | 日本語原文の例 | 説明 |
| :--- | :--- | :--- | :--- |
| `tab.clip_count` | `{n}` | `クリップ ({n}個)` | 選択中の複数クリップ数 |
| `timeline.len_auto` | `{total}` | `尺: 自動 ({total}f)` | 自動タイムライン長（フレーム数） |
| `timeline.len_manual` | `{total}` | `尺: 手動 ({total}f)` | 手動設定タイムライン長（フレーム数） |
| `timeline.selected_count` | `{count}` | `{count}個選択中` | タイムライン上で複数選択中の件数 |
| `timeline.range_badge` | `{start}`, `{end}` | `選択: [{start}..{end})` | 選択範囲の開始・終了フレーム |
| `inspector.multi_select_title` | `{count}` | `{count} 個のオブジェクトを選択中` | インスペクタ見出しの選択数 |
| `ctx.add_subtitle` | `{frame}` | `テキスト・字幕 ({frame}f)` | 右クリック位置のフレーム番号 |
| `ctx.paste_here` | `{frame}` | `ここに貼り付け ({frame}f)` | 貼り付け先フレーム番号 |
| `ctx.split_here` | `{frame}` | `このクリック位置で分割 ({frame}f)` | 分割位置フレーム番号 |
| `ctx.add_midpoint_here` | `{frame}` | `クリック位置へ中間点を追加 ({frame}f)` | 中間点追加フレーム番号 |
| `ctx.seek_here` | `{frame}` | `このフレームへシーク ({frame}f)` | ルーラー右クリックのフレーム番号 |
| `ctx.set_sel_start_here` | `{frame}` | `ここを選択開始点 [ に設定 ({frame}f)` | 選択開始 `[` フレーム番号 |
| `ctx.set_sel_end_here` | `{frame}` | `ここを選択終了点 ] に設定 ({frame}f)` | 選択終了 `]` フレーム番号 |
| `ctx.seek_midpoint` | `{frame}` | `この中間点へシーク ({frame}f)` | 中間点のタイムラインフレーム番号 |
| `ctx.remove_midpoint` | `{frame}` | `中間点を削除 ({frame}f)` | 削除対象中間点のフレーム番号 |
| `project.resolution` | `{w}`, `{h}`, `{fps}` | `解像度: {w}x{h} @ {fps}fps` | キャンバス幅・高さ・フレームレート |
| `project.summary` | `{tracks}`, `{assets}`, `{len}` | `トラック: {tracks} / 素材: {assets} / 総尺: {len}` | トラック数・素材数・総尺文字列 |
| `project.status` | `{status}` | `状態: {status}` | アプリの最新ステータスメッセージ |

---

## 5. 翻訳作業のコツと確認ポイント

1. **JSON 構文エラーに注意する**
   - 最後の行に余分なカンマ（`,}`）を入れないでください。
   - 文中にダブルクォート `"` を含める場合は `\"` とエスケープしてください（メニュー等では `'...'` にすると読みやすくなります）。
   - 解析に失敗した JSON ファイルは無視されるため、反映されない場合はまず JSON バリデータ等で構文を確認してください。
2. **ボタン幅・UI 収まりを意識する**
   - `preview.guides`（枠）・`preview.waveform`（波形）・`timeline.snap`（スナップ）・`inspector.align_left`（左）・`inspector.font_gf_fetch`（GF取得）などの短いラベルは、長すぎると横並びバーからはみ出すことがあります。ベース言語（`en.json` / `ja.json`）と同程度の長さの略語・短語にすると綺麗に収まります。
3. **配布パッケージの初期 `lang/` フォルダが見当たらない場合**
   - リポジトリの [`lang/ja.json`](../lang/ja.json) と [`lang/en.json`](../lang/en.json) をテンプレートとして取得してください。右パネルの **`プロジェクト` タブ → `表示言語 (lang/)`** の下部にも、アプリが参照している `lang` ディレクトリのフルパスが表示されています。

---

## 6. 有志による Pull Request（コントリビューション）手順

作成した言語パックを本体リポジトリへ同梱したい場合（大歓迎です）：

1. リポジトリ直下の `lang/<言語コード>.json` にファイルを保存します（例: `lang/ko.json`）。
2. アプリ標準パックとキーが一致しているか確認し、ワークスペースの単体テストが通ることを確認します。
   ```bash
   cargo test --lib ui::i18n
   ```
   - `ui::i18n::tests::builtin_ja_and_en_have_matching_key_sets`（標準バンドル間のキー整合検証）や `dropping_custom_lang_file_registers_new_language_with_fallback` が実行されます。
   - **ソースコードを編集した場合**は、`src/ui/i18n.rs` に組み込まれる言語以外は `lang/*.json` の配置だけで追加・テストできます（新規言語をアプリ本体へ標準バンドル同梱したい場合は PR 本文でお知らせください）。
3. 提出された翻訳ファイルは、本プロジェクトのポリシーに基づき **MIT License OR Apache License, Version 2.0** のデュアルライセンスの下で提供されるものとして扱われます（詳しくは [`../README.md`](../README.md) を参照してください）。
