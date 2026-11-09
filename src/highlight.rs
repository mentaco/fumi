use ratatui::style::{Color, Modifier, Style};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, LazyLock},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// 同じ装飾が続く区間。`end` は論理行の文字列内の終端バイト位置。
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub end: usize,
    pub style: Style,
}

/// コードブロックごとのハイライトの結果。文書の内容だけで決まり幅によらないため、
/// 文書と同じ期間保持し、リサイズ等の再レイアウトで計算し直さない。
#[derive(Debug, Default)]
pub(crate) struct HighlightCache(RefCell<HashMap<(usize, usize), Tokens>>);

type Tokens = Arc<Vec<Token>>;

impl HighlightCache {
    /// `key` はコードブロックのソース開始位置と論理行の文字列の長さ。
    pub(crate) fn get_or_compute(
        &self,
        key: (usize, usize),
        compute: impl FnOnce() -> Vec<Token>,
    ) -> Tokens {
        self.0
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| Arc::new(compute()))
            .clone()
    }
}

/// 同梱定義だけを使う。未知の言語と解析の失敗では、全体を装飾なしの一区間にする。
pub fn highlight(text: &str, language: &str) -> Vec<Token> {
    let plain = || {
        vec![Token {
            end: text.len(),
            style: Style::default(),
        }]
    };
    let language = language.split_whitespace().next().unwrap_or("");
    let Some(syntax) = SYNTAXES.find_syntax_by_token(language) else {
        return plain();
    };
    let mut highlighter = HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"]);
    let mut result: Vec<Token> = Vec::new();
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        let Ok(parts) = highlighter.highlight_line(line, &SYNTAXES) else {
            return plain();
        };
        for (s, part) in parts {
            let mut style =
                Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
            if s.font_style.contains(FontStyle::BOLD) {
                style = style.add_modifier(Modifier::BOLD);
            }
            if s.font_style.contains(FontStyle::ITALIC) {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if s.font_style.contains(FontStyle::UNDERLINE) {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            end += part.len();
            match result.last_mut().filter(|t| t.style == style) {
                Some(last) => last.end = end,
                None => result.push(Token { end, style }),
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::Document, layout::layout};

    #[test]
    fn relayout_uses_the_stored_result_at_every_width() {
        let code = "fn main() { let value = 1; }";
        let doc = Document::parse(format!("```rust\n{code}\n```"));
        // 計算では出ない色を先に入れておき、どの幅でも計算し直さずこれを使うことを確かめる。
        let marker = Style::default().fg(Color::Rgb(1, 2, 3));
        doc.highlights.get_or_compute((0, code.len()), || {
            vec![Token {
                end: code.len(),
                style: marker,
            }]
        });
        for width in [80, 40, 20] {
            let lines = layout(&doc, width, true);
            let spans: Vec<_> = lines
                .iter()
                .flat_map(|l| &l.line.spans)
                .filter(|s| !s.content.trim().is_empty() && !s.content.contains("[rust]"))
                .filter(|s| !s.content.contains('↪'))
                .collect();
            assert!(!spans.is_empty());
            assert!(spans.iter().all(|s| s.style == marker), "{spans:?}");
        }
    }

    #[test]
    fn adjacent_tokens_with_the_same_style_are_merged_and_cover_the_text() {
        let text = "fn main() {\n    /* 複数行の\n    コメント */\n}";
        let tokens = highlight(text, "rust");
        assert_eq!(tokens.last().map(|t| t.end), Some(text.len()));
        assert!(tokens.windows(2).all(|t| t[0].style != t[1].style));
        assert!(tokens.windows(2).all(|t| t[0].end < t[1].end));
    }

    #[test]
    fn unknown_language_is_one_plain_token() {
        let tokens = highlight("fn main() {}", "no-such-language");
        assert_eq!(
            tokens,
            [Token {
                end: 12,
                style: Style::default()
            }]
        );
    }
}
