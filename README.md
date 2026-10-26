# mdview

日本語と数式を含むMarkdownを、ターミナルで読むための読み取り専用CLI/TUIです。macOSとLinuxを対象に、Vim風の移動、検索、コードハイライト、画面幅に応じた表表示を提供します。対応端末ではブロック数式を画像で表示します。

現在は開発版（`0.2.0`）です。v1機能は実装済みですが、端末ごとのリッチ表示、Linux実端末、配布環境の受け入れ確認は継続中です。検証済みの範囲は[v1検証記録](docs/v1-verification.md)を参照してください。

## ビルドと起動

RustとCargoを用意し、リポジトリのルートで実行します。

```sh
cargo build --release --locked
./target/release/mdview README.md
```

PATHから使う場合は、チェックアウトしたソースからインストールできます。

```sh
cargo install --path . --locked
mdview README.md
```

`Cargo.toml`の最小Rustバージョン指定は1.88です。ローカル検証は主にRust 1.92.0で行っており、1.88でのビルドは未検証です。実行時にRust、Node.js、ブラウザ、外部TeXは不要です。

stdinとstdoutがTTYで、`TERM`が有効な対話端末から起動してください。入力はUTF-8の通常ファイル一つ、最大32 MiBです。標準入力やパイプ出力には対応していません。

## 使い方

```sh
mdview notes.md
mdview --plain notes.md
mdview --graphics off --color never notes.md
mdview --help
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
| 通常閲覧から終了 | `q` / Esc |
| 割り込み終了 | `Ctrl+c` |
| 中断・シェルから復帰 | `Ctrl+z`、`fg` |

検索入力中のEsc / `Ctrl+c`は取消、ヘルプ内の`q` / Escは本文へ戻る操作です。ホイールは3表示行ずつ移動します。詳しい入力規則は[利用ガイド](docs/usage.md)を参照してください。

## 表示と制約

- 見出し、強調、引用、リスト、タスク、コード、リンク、画像の代替テキストを表示します。HTMLはタグを含め文字として表示します。
- 日本語、結合文字、絵文字の書記素境界とセル幅で折り返します。表が画面幅に収まらない場合は縦形式へ切り替えます。
- ブロック数式はRaTeXと同梱KaTeXフォントで画像化します。端末能力を確認できない場合や式の生成に失敗した場合はTeXを表示します。完全なLaTeX互換ではありません。
- tmuxで画像表示の条件を満たす場合、起動したペインの`allow-passthrough`を自動で`all`へ設定します。**この設定は終了後もそのペインに残ります。** 条件と制約は[数式・端末ガイド](docs/terminal-graphics.md)を参照してください。
- ファイルは起動時のスナップショットとして読み、書き換えません。文書の閲覧によるネットワーク通信、リンク先起動、クリップボード操作は行いません。

Markdown画像の実画像表示、インライン数式の画像化、標準入力、自動再読込、拡大見出しは未実装です。初回起動200 msの性能目標は一部条件で未達です。

## ドキュメント・開発

[ドキュメント一覧](docs/README.md)から、利用ガイド、端末設定、仕様、検証記録へ進めます。開発環境とテスト手順、不具合報告の方法は[CONTRIBUTING.md](CONTRIBUTING.md)にまとめています。

## ライセンス

mdview本体は[MIT License](LICENSE)で公開します。依存ライブラリと同梱フォントにはそれぞれのライセンスが適用されます。出典と同梱通知は[第三者コンポーネント](THIRD_PARTY_NOTICES.md)を参照してください。
