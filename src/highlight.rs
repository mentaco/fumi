use crate::layout::Run;
use ratatui::style::{Color, Modifier, Style};
use std::sync::LazyLock;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// 同梱定義だけを使い、元のバイト範囲をトークンへ引き継ぐ。
pub fn highlight(runs: Vec<Run>, language: &str) -> Vec<Run> {
    let language = language.split_whitespace().next().unwrap_or("");
    let Some(syntax) = SYNTAXES.find_syntax_by_token(language) else {
        return runs;
    };
    let mut highlighter = HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"]);
    let mut result = Vec::new();
    for run in &runs {
        let mut offset = 0;
        for line in run.text.split_inclusive('\n') {
            let Ok(parts) = highlighter.highlight_line(line, &SYNTAXES) else {
                return runs;
            };
            for (s, text) in parts {
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
                result.push(Run {
                    text: text.into(),
                    style,
                    source: run.source.start + offset..run.source.start + offset + text.len(),
                    destination: false,
                });
                offset += text.len();
            }
        }
    }
    result
}
