//! コードブロックのシンタックスハイライト。計算は文書ごとの別スレッドで行い、操作と描画を止めない。
//! 結果はブロックの先頭から順に確定するため、計算済みの範囲から描画時に色を重ねる。
use crate::layout::DisplayLine;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, LazyLock, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};
use unicode_segmentation::UnicodeSegmentation;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// 計算途中の結果を描画側へ渡す間隔。
const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

/// 同じ装飾が続く区間。`end` は論理行の文字列内の終端バイト位置。
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub end: usize,
    pub style: Style,
}

/// コードブロックの識別子。ソース開始位置と論理行の文字列の長さ。
pub type Key = (usize, usize);

/// コードブロックごとのハイライトの結果。文書の内容だけで決まり幅によらないため、
/// 文書と同じ期間保持し、リサイズ等の再レイアウトで計算し直さない。文書を捨てると計算も止める。
#[derive(Debug, Default)]
pub(crate) struct HighlightCache {
    shared: Arc<Shared>,
    /// 計算スレッドへの依頼。最初の依頼でスレッドを起動する。
    sender: Mutex<Option<Sender<Job>>>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    finished: Condvar,
    cancel: AtomicBool,
}

#[derive(Debug, Default)]
struct State {
    blocks: HashMap<Key, Block>,
    /// 依頼済みで計算が終わっていないブロックの数。
    pending: usize,
    /// 計算スレッドが終了した。以後は結果が増えない。
    closed: bool,
}

#[derive(Debug, Default)]
struct Block {
    tokens: Vec<Token>,
    /// 論理行の文字列の先頭からこの位置までの装飾が確定している。
    covered: usize,
    finished: bool,
}

#[derive(Debug)]
struct Job {
    key: Key,
    text: String,
    language: String,
}

/// 色が確定していない部分のある表示行。ブロックの `covered` を超えて確定したら描き直す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Waiting {
    key: Key,
    covered: usize,
}

/// 表示行の位置と論理行の文字列の位置の対応。コードの表示行だけが持つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeOffsets {
    pub key: Key,
    pub runs: Vec<CodeRun>,
}

/// 表示行の `text` バイト目から始まる区間が、論理行の文字列の `rendered` バイト目に対応する。
/// `verbatim` の区間は同じ長さで対応し、そうでない区間（タブ・制御文字の置換）は全体が
/// `rendered` の一書記素に対応する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeRun {
    pub text: usize,
    pub rendered: usize,
    pub verbatim: bool,
}

impl CodeOffsets {
    fn rendered(&self, text: usize) -> Option<usize> {
        let index = self
            .runs
            .partition_point(|r| r.text <= text)
            .checked_sub(1)?;
        let run = &self.runs[index];
        Some(if run.verbatim {
            run.rendered + (text - run.text)
        } else {
            run.rendered
        })
    }
}

impl HighlightCache {
    /// コードブロックの計算を依頼する。依頼済みなら何もしない。
    pub(crate) fn request(&self, key: Key, text: &str, language: &str) {
        let mut state = self.shared.lock();
        if state.blocks.contains_key(&key) {
            return;
        }
        let mut sender = self.sender.lock().unwrap_or_else(|e| e.into_inner());
        if sender.is_none() && !state.closed {
            *sender = self.spawn();
        }
        let job = Job {
            key,
            text: text.to_string(),
            language: language.to_string(),
        };
        let sent = sender.as_ref().is_some_and(|s| s.send(job).is_ok());
        // スレッドを起動できなければ装飾なしのままにする。
        state.blocks.insert(
            key,
            Block {
                finished: !sent,
                ..Default::default()
            },
        );
        if sent {
            state.pending += 1;
        }
    }

    fn spawn(&self) -> Option<Sender<Job>> {
        let (sender, receiver) = mpsc::channel::<Job>();
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                let _close = Close(&shared);
                while let Ok(job) = receiver.recv() {
                    if shared.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    shared.run(job);
                }
            })
            .ok()
            .map(|_| sender)
    }

    /// 描いた時点より先まで色が確定した行があるか。
    pub(crate) fn advanced(&self, waiting: &[Waiting]) -> bool {
        let state = self.shared.lock();
        waiting.iter().any(|w| {
            state
                .blocks
                .get(&w.key)
                .is_some_and(|b| b.finished || b.covered > w.covered)
        })
    }

    /// 依頼済みの計算がすべて終わるまで待つ。
    pub(crate) fn wait(&self) {
        let mut state = self.shared.lock();
        while state.pending > 0 && !state.closed {
            state = self
                .shared
                .finished
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 確定した範囲の装飾を表示行へ重ねる。コードでない行と、装飾が一つも確定していない行は
    /// `None`。この行にまだ確定していない部分があれば `Waiting` も返す。
    pub(crate) fn apply(&self, line: &DisplayLine) -> (Option<Line<'static>>, Option<Waiting>) {
        let Some(code) = &line.code else {
            return (None, None);
        };
        let state = self.shared.lock();
        let Some(block) = state.blocks.get(&code.key) else {
            return (None, None);
        };
        let last = line
            .mappings
            .last()
            .and_then(|m| code.rendered(m.text.end - 1));
        let pending = (!block.finished && !state.closed)
            .then_some(last)
            .flatten()
            .filter(|&r| r >= block.covered)
            .map(|_| Waiting {
                key: code.key,
                // 行より手前が確定しても、この行の見た目は変わらない。
                covered: block
                    .covered
                    .max(code.runs.first().map_or(0, |r| r.rendered)),
            });
        if block.tokens.is_empty() {
            return (None, pending);
        }
        let tokens = &block.tokens;
        let mut token = None::<usize>;
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut offset = 0;
        for span in &line.line.spans {
            for g in span.content.graphemes(true) {
                let mut style = span.style;
                if let Some(rendered) = code.rendered(offset).filter(|&r| r < block.covered) {
                    // 表示行の中では論理行の位置は増える一方なので、最初だけ二分探索する。
                    let mut index =
                        token.unwrap_or_else(|| tokens.partition_point(|t| t.end <= rendered));
                    while index + 1 < tokens.len() && tokens[index].end <= rendered {
                        index += 1;
                    }
                    token = Some(index);
                    if let Some(t) = tokens.get(index) {
                        style = t.style;
                    }
                }
                match spans.last_mut().filter(|s| s.style == style) {
                    Some(last) => last.content.to_mut().push_str(g),
                    None => spans.push(Span::styled(g.to_string(), style)),
                }
                offset += g.len();
            }
        }
        let styled = Line {
            spans,
            ..line.line.clone()
        };
        (Some(styled), pending)
    }
}

impl Drop for HighlightCache {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

/// 計算スレッドの終了（panicを含む）を待機側へ知らせる。
struct Close<'a>(&'a Shared);

impl Drop for Close<'_> {
    fn drop(&mut self) {
        self.0.lock().closed = true;
        self.0.finished.notify_all();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn run(&self, job: Job) {
        let mut last = Instant::now();
        highlight_with(
            &job.text,
            &job.language,
            || self.cancel.load(Ordering::Relaxed),
            |tokens, covered, replace| {
                if !replace && covered < job.text.len() && last.elapsed() < PUBLISH_INTERVAL {
                    return;
                }
                last = Instant::now();
                let mut state = self.lock();
                if let Some(block) = state.blocks.get_mut(&job.key) {
                    if replace {
                        block.tokens.clear();
                    }
                    append(&mut block.tokens, tokens);
                    block.covered = covered;
                }
            },
        );
        let mut state = self.lock();
        if let Some(block) = state.blocks.get_mut(&job.key) {
            block.finished = true;
        }
        state.pending = state.pending.saturating_sub(1);
        drop(state);
        self.finished.notify_all();
    }
}

/// 同梱定義だけを使う。未知の言語と解析の失敗では、全体を装飾なしの一区間にする。
pub fn highlight(text: &str, language: &str) -> Vec<Token> {
    let mut result = Vec::new();
    highlight_with(
        text,
        language,
        || false,
        |tokens, _, replace| {
            if replace {
                result.clear();
            }
            append(&mut result, tokens);
        },
    );
    result
}

/// `tokens` を空にして `target` の後ろへ足す。境界で同じ装飾が続けば一区間にまとめる。
fn append(target: &mut Vec<Token>, tokens: &mut Vec<Token>) {
    for token in tokens.drain(..) {
        match target.last_mut().filter(|t| t.style == token.style) {
            Some(last) => last.end = token.end,
            None => target.push(token),
        }
    }
}

/// 先頭から一行ずつ計算し、`publish(tokens, covered, replace)` で途中の結果を渡す。受け取った
/// `publish` は `tokens` を空にし、受け取らなければ次の行の結果を後ろへ足す。`replace` は以前の
/// 結果を捨てる指示。最後の行の後には必ず呼ぶ。`stop` が真になったら途中で止める。
fn highlight_with(
    text: &str,
    language: &str,
    stop: impl Fn() -> bool,
    mut publish: impl FnMut(&mut Vec<Token>, usize, bool),
) {
    let plain = |publish: &mut dyn FnMut(&mut Vec<Token>, usize, bool)| {
        let mut tokens = vec![Token {
            end: text.len(),
            style: Style::default(),
        }];
        publish(&mut tokens, text.len(), true);
    };
    let language = language.split_whitespace().next().unwrap_or("");
    let Some(syntax) = SYNTAXES.find_syntax_by_token(language) else {
        return plain(&mut publish);
    };
    let mut highlighter = HighlightLines::new(syntax, &THEMES.themes["base16-ocean.dark"]);
    let mut tokens: Vec<Token> = Vec::new();
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if stop() {
            return;
        }
        let Ok(parts) = highlighter.highlight_line(line, &SYNTAXES) else {
            return plain(&mut publish);
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
            match tokens.last_mut().filter(|t| t.style == style) {
                Some(last) => last.end = end,
                None => tokens.push(Token { end, style }),
            }
        }
        if end < text.len() {
            publish(&mut tokens, end, false);
        }
    }
    publish(&mut tokens, text.len(), false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document::Document,
        layout::{layout, layout_with_images},
    };

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

    #[test]
    fn partial_results_color_only_the_covered_prefix() {
        let code = "fn a() {}\nfn b() {}";
        let doc = Document::parse(format!("```rust\n{code}\n```"));
        let lines = layout_with_images(&doc, 80, true, &[], &Default::default());
        doc.highlights.wait();
        let marker = Style::default().fg(Color::Rgb(1, 2, 3));
        // 1行目だけ確定した状態にして、2行目は元の装飾のまま未確定と報告されることを確かめる。
        {
            let mut state = doc.highlights.shared.lock();
            let block = state.blocks.values_mut().next().unwrap();
            block.tokens = vec![Token {
                end: 10,
                style: marker,
            }];
            block.covered = 10;
            block.finished = false;
        }
        let first = lines.iter().position(|l| l.text() == "fn a() {}").unwrap();
        let (styled, waiting) = doc.highlights.apply(&lines[first]);
        assert_eq!(waiting, None);
        assert!(styled.unwrap().spans.iter().all(|s| s.style == marker));
        let (styled, waiting) = doc.highlights.apply(&lines[first + 1]);
        assert!(
            styled
                .unwrap()
                .spans
                .iter()
                .all(|s| s.style == Style::default())
        );
        let waiting = waiting.unwrap();
        // 2行目の先頭を超えて確定するまでは描き直しを求めない。
        assert!(!doc.highlights.advanced(&[waiting]));
        doc.highlights
            .shared
            .lock()
            .blocks
            .values_mut()
            .next()
            .unwrap()
            .covered = 11;
        assert!(doc.highlights.advanced(&[waiting]));
    }

    #[test]
    fn tabs_and_control_characters_keep_the_token_of_their_source() {
        let code = "\tlet a = \"\u{7}\";\n\tlet b = 1;";
        let doc = Document::parse(format!("```rust\n{code}\n```"));
        let colored = layout(&doc, 80, true);
        let plain = layout(&doc, 80, false);
        assert_eq!(
            colored.iter().map(|l| l.text()).collect::<Vec<_>>(),
            plain.iter().map(|l| l.text()).collect::<Vec<_>>()
        );
        let string = highlight(code, "rust")
            .into_iter()
            .find(|t| t.end > code.find('\u{7}').unwrap())
            .unwrap()
            .style;
        let line = colored
            .iter()
            .find(|l| l.text().contains("\\u{0007}"))
            .unwrap();
        assert!(
            line.line
                .spans
                .iter()
                .filter(|s| s.content.contains("\\u{0007}"))
                .all(|s| s.style == string),
            "{line:?}"
        );
    }

    #[test]
    fn dropping_the_document_stops_the_computation() {
        let code = "let value = 1;\n".repeat(1_000_000);
        let doc = Document::parse(String::new());
        doc.highlights.request((0, code.len()), &code, "rust");
        let shared = Arc::clone(&doc.highlights.shared);
        drop(doc);
        let mut state = shared.lock();
        while !state.closed {
            state = shared.finished.wait(state).unwrap();
        }
        // 最後まで計算せずに止まる。
        assert!(state.blocks[&(0, code.len())].covered < code.len());
    }
}
