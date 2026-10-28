# fumi

数式を含むMarkdownを、ターミナルでサッと読むための読み取り専用CLI/TUIです。macOSとLinuxを対象に、Vimキーバインドの移動、検索、コードハイライト、画面幅に応じた表の描画を提供します。対応端末ではブロック数式を画像で表示します。

## ビルドと起動

RustとCargoを用意し、リポジトリのルートで実行します。

```sh
cargo build --release --locked
./target/release/fumi README.md
```

PATHから使う場合は、チェックアウトしたソースからインストールできます。

```sh
cargo install --path . --locked
fumi README.md
```

（`Cargo.toml`の最小Rustバージョン指定は1.88）

## 使い方

```sh
fumi notes.md
fumi --plain notes.md
fumi --graphics off --color never notes.md
fumi --help
```

`--plain`は能力照会と画像転送を無効にする全画面テキストTUIです。検索・色・表表示は使えます。`--graphics auto|off|kitty`の既定は`auto`で、`--plain`が優先します。`--color auto|always|never`の`auto`は空でない`NO_COLOR`を尊重します。

| 操作 | キー |
|---|---|
| 上下に1表示行移動 | `k` / `j`、上下矢印 |
| 半画面移動 | `Ctrl+u` / `Ctrl+d` |
| 1画面移動 | `Ctrl+b` / `Ctrl+f`、PageUp / PageDown |
| 先頭・末尾 | `gg` / `G`、Home / End |
| 検索・次／前の一致 | `/`、`n` / `N` |
| ヘルプ | `?` |
| 終了 | `q` / Esc |

検索入力中のEsc / `Ctrl+c`は取消、ヘルプ内の`q` / Escは本文へ戻る操作です。ホイールは3表示行ずつ移動します。詳しい入力規則は[利用ガイド](docs/usage.md)を参照してください。

## 表示と制約

- 見出し、強調、引用、リスト、タスク、コード、リンク、画像の代替テキストを表示します。HTMLはタグを含め文字として表示します。
- 日本語、結合文字、絵文字の書記素境界とセル幅で折り返します。表が画面幅に収まらない場合は縦形式へ切り替えます。
- ブロック数式はRaTeXと同梱KaTeXフォントで画像化します。端末能力を確認できない場合や式の生成に失敗した場合はTeXを表示します。完全なLaTeX互換ではありません。
- tmuxで画像表示の条件を満たす場合、起動したペインの`allow-passthrough`を自動で`all`へ設定します。**この設定は終了後もそのペインに残ります。** 条件と制約は[数式・端末ガイド](docs/terminal-graphics.md)を参照してください。
- ファイルは起動時のスナップショットとして読み、書き換えません。

Markdown画像の実画像表示、インライン数式の画像化、標準入力、自動再読込、拡大見出しは現在、未実装です。
