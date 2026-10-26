# 開発・貢献ガイド

## 開発環境

- macOSまたはLinux、RustとCargo。最小Rust指定は1.88ですが、この版でのビルドは未検証です。
- `rustfmt`とClippy。
- PTY検証にはPython 3。標準ライブラリのみ使用します。
- tmux統合検証にはtmux。未導入時はスクリプトがスキップを明示します。

以下はリポジトリのルートで実行します。個人用の設定ファイルやRTKの導入は不要です。RTKを使う環境では各コマンドを`rtk` / `rtk proxy`で実行できます。

```sh
cargo build --locked
./target/debug/mdview tests/fixtures/v1.md
```

## 構成と変更方針

製品の挙動と受け入れ条件は[仕様](mdview-spec.md)を基準にします。将来計画と実装・検証済みの範囲を区別し、利用者に見える動作を変えた場合は関連文書も更新してください。

| ファイル | 責務 |
|---|---|
| `src/main.rs`、`cli.rs`、`source.rs` | 起動、引数、ファイル読込・制御文字の無害化 |
| `src/document.rs`、`math.rs` | ソース範囲を保つ文書解析、数式認識・生成 |
| `src/layout.rs`、`highlight.rs`、`search.rs` | 幅依存の配置、コード装飾、検索 |
| `src/app.rs`、`input.rs`、`terminal.rs` | 操作状態、入力、端末描画と復旧 |
| `src/graphics.rs`、`graphics_diacritics.rs` | 画像プロトコルとプレースホルダー |
| `src/profile.rs` | 任意featureによる開発用計測 |
| `tests/` | Rust・CLI・PTY検証、fixture、表示スナップショット |

入力文書を書き換えず、文書から通信・リンク起動・クリップボード操作を行わない方針を維持します。解析とレイアウトを分離し、UTF-8バイト位置、書記素、セル幅を区別してください。依存更新時は`Cargo.lock`も管理対象に含めます。

## 基本チェック

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo build --locked
python3 tests/pty_smoke.py target/debug/mdview
python3 tests/pty_v1.py target/debug/mdview
python3 tests/pty_tmux.py target/debug/mdview
```

PTYは対話セッションを再現し、正常終了・割り込み・中断再開・端末属性の復旧を検証します。通常の`cargo test`で無視される端末故障注入テストは`pty_smoke.py`が専用プロセスで実行します。`pty_tmux.py`は一時ソケットと専用設定で独立したサーバーを作り、既存セッションを変更しません。

Kitty応答はエミュレータによるもので、実端末の画像描画の合格を意味しません。表示変更時は[実端末チェックリスト](docs/manual-testing.md)と[手動確認用文書](docs/examples/manual-debug.md)も使います。CIはUbuntuとmacOSで基本チェックとPTY検証を実行します。各変更の成功は該当するCI結果で確認してください。

意図的にスナップショットを更新する場合は次を実行し、差分を確認します。

```sh
env MDVIEW_UPDATE_SNAPSHOTS=1 cargo test --locked --test core representative_display_snapshots
```

文書のみの変更では、ローカルリンク、Markdown構造、仕様と実装の整合を確認します。コードのテストは関連する変更がある場合に実施します。

## 性能測定

```sh
cargo build --release --locked --features startup-profile
python3 tests/pty_v1_bench.py target/release/mdview
```

`startup-profile`は任意の開発用featureです。通常ビルドでは計測しません。MVPの内訳計測は[性能調査](docs/performance.md#再現手順)、v1の条件と結果は[v1検証記録](docs/v1-verification.md#性能)を参照してください。測定中は別のビルドやベンチマークを並行実行せず、初回と再実行を分けて記録します。

## 不具合報告

GitHubのIssueに、共有できる最小のMarkdownと次の情報を記載してください。個人情報・認証情報を含む文書やログは添付しないでください。

```text
コミットまたはmdview --version:
OS / CPU:
端末名・バージョン:
フォント名・サイズ:
tmux / SSHの有無:
端末の幅×高さ（セル数）:
起動コマンド:
再現する最小のMarkdown:
操作・リサイズの順序:
期待した動作:
実際の動作:
終了・中断後のシェル状態:
再現頻度:
```

未修正の脆弱性や秘密情報の漏えいは公開Issueへ貼らず、GitHubの非公開の脆弱性報告が有効な場合はそこから報告してください。
