# mdview

日本語を含むMarkdownを読む、macOS/Linux向けの読み取り専用CLI/TUIです。v1の検索・コードハイライト・可変幅の表・ホイール操作・ブロック数式画像化を実装しています。実端末でのリッチ表示の受け入れ確認は残っています。

```sh
cargo build --release --locked
./target/release/mdview README.md
./target/release/mdview tests/fixtures/v1.md
./target/release/mdview --plain mdview-spec.md
./target/release/mdview --graphics off --color never README.md
```

Rust 1.88以上を指定し、今回のv1はRust 1.92.0・macOS arm64で検証しています。実行時にRust・Node.js・ブラウザ・外部TeXは不要です。stdinとstdoutがTTYで、`TERM`が有効な対話端末から起動してください。

- `j` / `k`・上下矢印で1表示行、ホイールで3表示行移動します。
- `Ctrl+d` / `Ctrl+u`で半画面、`Ctrl+f` / `Ctrl+b`・PageDown / PageUpで1画面移動します。
- `gg` / `G`・Home / Endで先頭・末尾へ移動します。
- `/`で検索入力、Enterで確定、`n` / `N`で次・前の一致へ循環します。大文字小文字を区別するリテラル検索です。
- 検索中のEsc / `Ctrl+c`は取消、Backspaceは1書記素削除、`Ctrl+u`は全消去です。最大256書記素、日本語や貼り付けにも対応します。
- `?`で操作一覧を開き、`q` / Escで本文へ戻ります。通常閲覧の`q` / Escで終了します。
- `Ctrl+c`で割り込み終了、`Ctrl+z`で中断し、シェルの`fg`で復帰します。

`--plain`は全画面のテキストTUIで、検索・ハイライト・表も使えます。能力照会と画像転送を行いません。`--graphics auto|off|kitty`の既定はautoです。plainはgraphicsの指定より優先します。`--color auto|always|never`も使え、autoでは空でない`NO_COLOR`を尊重します。色なしでも見出しと検索結果を装飾で区別できます。

見出し・引用・リスト・タスク・コード・リンク・画像の代替テキスト・HTMLの可視テキスト表示に対応します。表は罫線と余白を含む自然幅で収まる場合だけ表形式になり、狭い場合は縦形式に戻ります。既知言語のコードは同梱定義でハイライトし、未知言語は本文を保持します。

ブロック数式は、Kittyプロトコル・セルのピクセル寸法・背景色を確認できた場合に画像化します。RaTeXとKaTeXフォントをバイナリへ同梱し、最大2ジョブ・1式2秒・16 KiB・12セル行、キャッシュ128 MiBを上限とします。生成中や失敗時はTeXを読めます。数式のない文書は照会を省略します。tmux/screen内はテキスト表示です。フォントの出典とOFLは[licenses](licenses/)に置いています。

ファイルは起動時のスナップショットとして読み、書き換えません。リンク先起動・クリップボード操作・通信は行いません。Markdown画像の実画像表示、標準入力、監視、インライン数式画像、拡大文字は後続バージョンの範囲です。

仕様は[mdview-spec.md](mdview-spec.md)、検証結果と残る制約は[検証記録](docs/verification.md)、端末での確認手順は[チェックリスト](docs/manual-testing.md)、v1の実装判断と測定は[v1検証記録](docs/v1-verification.md)を参照してください。2026-10-26にユーザーの手動確認報告を受領しています。修正した行列・表も再確認OKとの報告を受領しました。今回の環境詳細、Linuxでのv1表示確認は残っています。

## 開発・検証

```sh
rtk cargo fmt --all -- --check
rtk cargo clippy --locked --all-targets --all-features -- -D warnings
rtk cargo test --locked --all-targets --all-features
rtk cargo build --locked
rtk proxy python3 tests/pty_smoke.py target/debug/mdview
rtk proxy python3 tests/pty_v1.py target/debug/mdview
rtk cargo build --release --locked --features startup-profile
rtk proxy python3 tests/pty_v1_bench.py target/release/mdview
```

PTY検証はPython 3の標準ライブラリだけで実行できます。`pty_v1.py`のKitty応答はエミュレータであり、実端末での画像描画の合格を意味しません。通常のcargo testで無視される端末故障注入テストは、`pty_smoke.py`が専用プロセスで実行します。スナップショットの意図的な更新は`rtk proxy env MDVIEW_UPDATE_SNAPSHOTS=1 cargo test --test core representative_display_snapshots`で行い、差分を確認します。

起動計測の方式とMVPの調査は[性能調査記録](docs/performance.md)を参照してください。`startup-profile`は開発用の任意featureで、通常ビルドでは計測しません。
