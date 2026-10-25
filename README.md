# mdview

日本語を含むMarkdownを読む、macOS/Linux向けの読み取り専用CLI/TUIです。MVPのテキスト閲覧を実装しています。

```sh
cargo build --release --locked
./target/release/mdview README.md
./target/release/mdview --plain mdview-spec.md
./target/release/mdview --color never tests/fixtures/representative.md
```

ビルドにはRust 1.88以上を指定しています。検証に使用したのはRust 1.92.0 / 1.99.1・macOS arm64です。実行時にRustや外部レンダラーは必要ありません。stdinとstdoutがTTYで、`TERM`が有効な対話端末から起動してください。

- `j` / `k`、上下矢印で1表示行ずつ移動します。
- `Ctrl+d` / `Ctrl+u`で半画面、`Ctrl+f` / `Ctrl+b`・PageDown / PageUpで1画面移動します。
- `gg` / `G`、Home / Endで先頭・末尾へ移動します。
- `?`で操作一覧を開き、`q` / Escで本文へ戻ります。本文の`q` / Escで終了します。
- `Ctrl+c`で割り込み終了、`Ctrl+z`で中断し、シェルの`fg`で復帰します。

`--plain`も全画面のテキストTUIです。MVPの通常表示と見た目は同じで、能力照会や画像転送を行いません。`--color auto|always|never`で色を指定でき、autoでは空でない`NO_COLOR`を尊重します。色を切っても太字や反転は使います。

見出し、強調、引用、リスト、タスク、コード、リンク、画像の代替テキスト、表の縦形式に対応します。数式は仕様の区切りを認識し、Markdownとして再解釈せずTeXソースを表示します。ファイルは起動時のスナップショットとして読み、書き換えません。リンク先・画像を取得する通信も行いません。

検索、マウス操作、数式画像化、ローカル画像表示、標準入力、監視は後続バージョンの範囲です。

仕様は[mdview-spec.md](mdview-spec.md)、検証結果と残る制約は[検証記録](docs/verification.md)を参照してください。実端末で確認する際は[デバッグ用チェックリスト](docs/manual-testing.md)を使えます。

## 開発・検証

```sh
rtk cargo fmt --all -- --check
rtk cargo clippy --locked --all-targets --all-features -- -D warnings
rtk cargo test --locked --all-targets --all-features
rtk proxy python3 tests/pty_smoke.py target/debug/mdview
rtk cargo build --release --locked
rtk proxy python3 tests/pty_bench.py target/release/mdview
```

PTY検証はPython 3の標準ライブラリだけで実行できます。通常の`cargo test`で無視される端末故障注入テストも、PTYスクリプトが専用プロセスで実行します。スナップショットを意図的に更新する場合は、`rtk proxy env MDVIEW_UPDATE_SNAPSHOTS=1 cargo test --test core representative_display_snapshots`を実行し、差分を確認してください。

採用APIは公式資料で確認し、依存を`Cargo.lock`へ固定しています：[clap 4.5.60](https://docs.rs/clap/4.5.60/clap/)、[ratatui 0.29.0](https://docs.rs/ratatui/0.29.0/ratatui/)、[crossterm 0.28.1](https://docs.rs/crossterm/0.28.1/crossterm/)、[pulldown-cmark 0.13.0](https://docs.rs/pulldown-cmark/0.13.0/pulldown_cmark/)。
