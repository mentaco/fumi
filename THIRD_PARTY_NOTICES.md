# 第三者コンポーネント

mdview本体の[MIT License](LICENSE)とは別に、依存ライブラリ、フォント、参照データにはそれぞれの条件が適用されます。

## Rust依存関係

採用バージョンは[Cargo.lock](Cargo.lock)、直接依存は[Cargo.toml](Cargo.toml)に記録しています。ソースリポジトリにはCargoが取得する依存クレート自体を同梱していません。

数式生成には[RaTeX](https://github.com/erweixin/RaTeX) 0.1.15、コードハイライトには[syntect](https://github.com/trishume/syntect) 5.3.0を使用します。両者のクレートのライセンス表記はMITです。推移的依存とsyntect内の構文・テーマ等は、各配布物の通知も確認してください。この文書は全依存のライセンス全文を集約したものではありません。

バイナリ配布物を作る際は、使用するターゲット・feature・Cargo.lockに対応する依存関係のライセンスと通知を集約し、この文書と`licenses/`も配布物へ含めてください。バイナリ配布用の通知一式は今回のソース公開準備では未作成です。

## 同梱KaTeXフォント

`ratex-render`の`embed-fonts` featureを通じ、`ratex-katex-fonts` 0.1.15のKaTeX TTFをバイナリに埋め込みます。フォントは改変しません。

- 出典: [KaTeX](https://github.com/KaTeX/KaTeX)
- 通知・著作権・Reserved Font Names: [KaTeX-FONT_NOTICE.txt](licenses/KaTeX-FONT_NOTICE.txt)
- ライセンス全文: [SIL Open Font License 1.1](licenses/KaTeX-OFL.txt)

上記は実際に依存する0.1.15の配布クレートの通知とTTF内のライセンスメタデータに基づきます。他の版や上流の現在のフォント配布条件へ一律に適用するものではありません。依存更新時は同梱物を再確認してください。

## Kittyプレースホルダーの文字表

[src/graphics_diacritics.rs](src/graphics_diacritics.rs)は、Kitty Graphics Protocolの行・列番号に使用するUnicodeコードポイントの表です。[公式の文字表](https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt)と同じ順序を保持します。上流の説明ではUnicode 6.0.0の`UnicodeData.txt`から抽出されています。Kitty本体の実装コードは同梱していません。
