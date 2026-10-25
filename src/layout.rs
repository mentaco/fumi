use crate::{
    document::{Document, Kind, Node},
    source::sanitize,
};
use pulldown_cmark::{CodeBlockKind, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub style: Style,
    pub source: Range<usize>,
}

#[derive(Default, Debug)]
struct LogicalLine {
    runs: Vec<Run>,
    prefix: String,
    continuation: String,
    code: bool,
    rule: bool,
    source: usize,
}

#[derive(Clone, Debug)]
pub struct DisplayLine {
    pub line: Line<'static>,
    pub source: Range<usize>,
}

impl DisplayLine {
    pub fn text(&self) -> String {
        self.line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

#[derive(Clone, Debug)]
struct Cell {
    text: String,
    style: Style,
    source: Range<usize>,
    width: usize,
}

fn run(text: impl Into<String>, style: Style, range: Range<usize>) -> Run {
    Run {
        text: text.into(),
        style,
        source: range,
    }
}

fn inline(nodes: &[Node], style: Style, out: &mut Vec<Run>) {
    for node in nodes {
        let r = node.source.clone();
        match &node.kind {
            Kind::Text(s) | Kind::Html(s) | Kind::Math(s) => out.push(run(s, style, r)),
            Kind::Code(s) => out.push(run(
                s,
                style.add_modifier(Modifier::DIM | Modifier::UNDERLINED),
                r,
            )),
            Kind::SoftBreak => out.push(run(" ", style, r)),
            Kind::HardBreak => out.push(run("\n", style, r)),
            Kind::Task(done) => out.push(run(if *done { "[x] " } else { "[ ] " }, style, r)),
            Kind::Element(tag) => match tag {
                Tag::Emphasis => inline(&node.children, style.add_modifier(Modifier::ITALIC), out),
                Tag::Strong => inline(&node.children, style.add_modifier(Modifier::BOLD), out),
                Tag::Strikethrough => inline(
                    &node.children,
                    style.add_modifier(Modifier::CROSSED_OUT),
                    out,
                ),
                Tag::Link { dest_url, .. } => {
                    let mut label = Vec::new();
                    inline(
                        &node.children,
                        style.add_modifier(Modifier::UNDERLINED),
                        &mut label,
                    );
                    let same = label.iter().map(|r| r.text.as_str()).collect::<String>()
                        == dest_url.as_ref();
                    out.extend(label);
                    if !same {
                        out.push(run(" <", style, r.start..r.start));
                        out.push(run(dest_url.to_string(), style, r.clone()));
                        out.push(run(">", style, r.end..r.end));
                    }
                }
                Tag::Image { dest_url, .. } => {
                    let mut alt = Vec::new();
                    inline(&node.children, style, &mut alt);
                    let empty = alt.iter().all(|r| r.text.is_empty());
                    out.push(run(
                        if empty { "[image" } else { "[image: " },
                        style,
                        r.start..r.start,
                    ));
                    out.extend(alt);
                    out.push(run("] <", style, r.start..r.start));
                    out.push(run(dest_url.to_string(), style, r.clone()));
                    out.push(run(">", style, r.end..r.end));
                }
                _ => inline(&node.children, style, out),
            },
            _ => {}
        }
    }
}

fn is_block(node: &Node) -> bool {
    matches!(
        node.kind,
        Kind::Rule
            | Kind::Element(
                Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::BlockQuote(_)
                    | Tag::CodeBlock(_)
                    | Tag::List(_)
                    | Tag::Table(_)
                    | Tag::HtmlBlock
            )
    ) || matches!(&node.kind, Kind::Math(s) if s.trim_start().starts_with("$$"))
}

fn blank(lines: &mut Vec<LogicalLine>, source: usize) {
    if lines.last().is_some_and(|l| !l.runs.is_empty() || l.rule) {
        lines.push(LogicalLine {
            source,
            ..Default::default()
        });
    }
}

fn sequence(nodes: &[Node], gaps: bool) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        let node = &nodes[i];
        if gaps {
            blank(&mut lines, node.source.start);
        }
        if is_block(node) {
            lines.extend(block(node));
            i += 1;
        } else {
            let start = i;
            while i < nodes.len() && !is_block(&nodes[i]) {
                i += 1;
            }
            let mut runs = Vec::new();
            inline(&nodes[start..i], Style::default(), &mut runs);
            lines.push(LogicalLine {
                runs,
                source: node.source.start,
                ..Default::default()
            });
        }
    }
    lines
}

fn block(node: &Node) -> Vec<LogicalLine> {
    let base = || LogicalLine {
        source: node.source.start,
        ..Default::default()
    };
    match &node.kind {
        Kind::Rule => vec![LogicalLine {
            rule: true,
            ..base()
        }],
        Kind::Math(s) => vec![LogicalLine {
            runs: vec![run(s, Style::default(), node.source.clone())],
            code: true,
            ..base()
        }],
        Kind::Element(Tag::Heading { level, .. }) => {
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let mut runs = vec![run(
                format!("{} ", "#".repeat(*level as usize)),
                style,
                node.source.start..node.source.start,
            )];
            inline(&node.children, style, &mut runs);
            vec![LogicalLine { runs, ..base() }]
        }
        Kind::Element(Tag::Paragraph | Tag::HtmlBlock) => {
            let mut runs = Vec::new();
            inline(&node.children, Style::default(), &mut runs);
            vec![LogicalLine { runs, ..base() }]
        }
        Kind::Element(Tag::BlockQuote(_)) => {
            let mut lines = sequence(&node.children, true);
            for line in &mut lines {
                line.prefix.insert_str(0, "│ ");
                line.continuation.insert_str(0, "│ ");
            }
            lines
        }
        Kind::Element(Tag::List(start)) => {
            let mut lines = Vec::new();
            for (idx, item) in node.children.iter().enumerate() {
                let marker = start.map_or_else(
                    || "• ".to_string(),
                    |n| format!("{}. ", n.saturating_add(idx as u64)),
                );
                let indent = " ".repeat(marker.width());
                let loose = item
                    .children
                    .iter()
                    .any(|n| matches!(n.kind, Kind::Element(Tag::Paragraph)));
                if loose {
                    blank(&mut lines, item.source.start);
                }
                let mut contents = sequence(&item.children, loose);
                if contents.is_empty() {
                    contents.push(LogicalLine {
                        source: item.source.start,
                        ..Default::default()
                    });
                }
                for (j, line) in contents.iter_mut().enumerate() {
                    line.prefix
                        .insert_str(0, if j == 0 { &marker } else { &indent });
                    line.continuation.insert_str(0, &indent);
                }
                lines.extend(contents);
            }
            lines
        }
        Kind::Element(Tag::CodeBlock(kind)) => {
            let mut lines = Vec::new();
            if let CodeBlockKind::Fenced(language) = kind
                && !language.is_empty()
            {
                lines.push(LogicalLine {
                    runs: vec![run(
                        format!("[{language}]"),
                        Style::default().add_modifier(Modifier::DIM),
                        node.source.start..node.source.start,
                    )],
                    ..base()
                });
            }
            let mut runs = Vec::new();
            inline(&node.children, Style::default(), &mut runs);
            // パーサがコード本文の末尾へ付ける一つの改行は余分な空行にしない。
            if let Some(last) = runs.last_mut()
                && last.text.ends_with('\n')
            {
                last.text.pop();
            }
            lines.push(LogicalLine {
                runs,
                code: true,
                ..base()
            });
            lines
        }
        Kind::Element(Tag::Table(_)) => table(node),
        _ => sequence(&node.children, true),
    }
}

fn table(node: &Node) -> Vec<LogicalLine> {
    let mut headers: Vec<Vec<Run>> = Vec::new();
    let mut rows = Vec::new();
    for child in &node.children {
        if matches!(child.kind, Kind::Element(Tag::TableHead)) {
            for (idx, cell) in child.children.iter().enumerate() {
                let mut runs = Vec::new();
                inline(
                    &cell.children,
                    Style::default().add_modifier(Modifier::BOLD),
                    &mut runs,
                );
                if runs.iter().all(|r| r.text.trim().is_empty()) {
                    runs = vec![run(
                        format!("列{}", idx + 1),
                        Style::default(),
                        cell.source.clone(),
                    )];
                }
                headers.push(runs);
            }
        } else if matches!(child.kind, Kind::Element(Tag::TableRow)) {
            rows.push(child);
        }
    }
    let mut result = Vec::new();
    if rows.is_empty() {
        for runs in headers {
            result.push(LogicalLine {
                runs,
                source: node.source.start,
                ..Default::default()
            });
        }
        result.push(LogicalLine {
            runs: vec![run("(no rows)", Style::default(), node.source.clone())],
            source: node.source.start,
            ..Default::default()
        });
        return result;
    }
    for (idx, row) in rows.into_iter().enumerate() {
        blank(&mut result, row.source.start);
        result.push(LogicalLine {
            runs: vec![run(
                format!("[row {}]", idx + 1),
                Style::default(),
                row.source.start..row.source.start,
            )],
            source: row.source.start,
            ..Default::default()
        });
        for (col, header) in headers.iter().enumerate() {
            let cell = row.children.get(col);
            let range = cell.map_or(row.source.clone(), |c| c.source.clone());
            let mut runs = header.clone();
            // 複製したラベルは現在のデータセルに対応させる。
            for r in &mut runs {
                r.source = range.start..range.start;
            }
            runs.push(run(": ", Style::default(), range.start..range.start));
            let before = runs.len();
            if let Some(cell) = cell {
                inline(&cell.children, Style::default(), &mut runs);
            }
            if runs[before..].iter().all(|r| r.text.is_empty()) {
                runs.push(run("(empty)", Style::default(), range.clone()));
            }
            result.push(LogicalLine {
                runs,
                source: range.start,
                ..Default::default()
            });
        }
    }
    result
}

fn cells(line: &LogicalLine, document: &Document, colors: bool) -> Vec<Cell> {
    struct Mapping<'a> {
        run: &'a Run,
        start: usize,
        end: usize,
        raw: &'a str,
        content_start: Option<usize>,
        cursor: usize,
    }
    let mut rendered = String::new();
    let mut mappings = Vec::new();
    for run in &line.runs {
        if run.text.is_empty() {
            continue;
        }
        let start = rendered.len();
        rendered.push_str(&run.text);
        let raw = document.source.get(run.source.clone()).unwrap_or("");
        mappings.push(Mapping {
            run,
            start,
            end: rendered.len(),
            raw,
            content_start: raw.find(run.text.as_str()),
            cursor: 0,
        });
    }
    let mut result = Vec::new();
    let mut column = 0;
    for (offset, g) in rendered.grapheme_indices(true) {
        let first = mappings.partition_point(|m| m.end <= offset);
        let mut style = mappings[first].run.style;
        if !colors {
            style.fg = None;
            style.bg = None;
        }
        let mut source = None::<Range<usize>>;
        for m in &mut mappings[first..] {
            if m.start >= offset + g.len() {
                break;
            }
            let local = offset.saturating_sub(m.start);
            let end = (offset + g.len()).min(m.end) - m.start;
            let part = &m.run.text[local..end];
            let range = if let Some(start) = m.content_start {
                m.run.source.start + start + local..m.run.source.start + start + end
            } else if let Some(start) = m.raw[m.cursor..].find(part) {
                let begin = m.cursor + start;
                m.cursor = begin + part.len();
                m.run.source.start + begin..m.run.source.start + m.cursor
            } else if m.raw[m.cursor..].starts_with('&')
                && let Some(end) = m.raw[m.cursor..].find(';')
            {
                let begin = m.cursor;
                m.cursor += end + 1;
                m.run.source.start + begin..m.run.source.start + m.cursor
            } else {
                m.run.source.start..m.run.source.start
            };
            if let Some(source) = &mut source {
                source.end = source.end.max(range.end);
            } else {
                source = Some(range);
            }
        }
        let source = source.unwrap_or(line.source..line.source);
        if g == "\n" {
            result.push(Cell {
                text: "\n".into(),
                style,
                source,
                width: 0,
            });
            column = 0;
            continue;
        }
        let text = if g == "\t" {
            " ".repeat(4 - column % 4)
        } else {
            sanitize(g)
        };
        let width = text.width();
        column += width;
        result.push(Cell {
            text,
            style,
            source,
            width,
        });
    }
    result
}

pub fn layout(document: &Document, width: usize, colors: bool) -> Vec<DisplayLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut logical = sequence(&document.nodes, true);
    if logical.is_empty() {
        logical.push(LogicalLine {
            runs: vec![run("(empty)", Style::default(), 0..0)],
            ..Default::default()
        });
    }
    let mut result = Vec::new();
    for line in logical {
        if line.rule {
            let prefix = if line.prefix.width() + 4 <= width {
                &line.prefix
            } else {
                ""
            };
            result.push(DisplayLine {
                line: Line::raw(format!("{prefix}{}", "─".repeat(width - prefix.width()))),
                source: line.source..line.source,
            });
            continue;
        }
        let data = cells(&line, document, colors);
        let mut start = 0;
        let mut first = true;
        let mut code_continuation = false;
        loop {
            let outer = if first {
                &line.prefix
            } else {
                &line.continuation
            };
            let outer = if outer.width() + 4 <= width {
                outer.as_str()
            } else {
                ""
            };
            let marker = if line.code && code_continuation {
                "↪ "
            } else {
                ""
            };
            let prefix = format!("{outer}{marker}");
            let available = width.saturating_sub(prefix.width()).max(1);
            let mut end = start;
            let mut used = 0;
            let mut last_space = None;
            while end < data.len() && data[end].text != "\n" && used + data[end].width <= available
            {
                used += data[end].width;
                if data[end].text.chars().all(char::is_whitespace) {
                    last_space = Some(end);
                }
                end += 1;
            }
            let hard = end < data.len() && data[end].text == "\n";
            let mut next = end;
            if !line.code && !hard && end < data.len() {
                // 一語が本文幅を超える日本語やURLは、手前の空白へ戻さず
                // 現在行の残り幅を使って書記素境界で分割する。
                let space = last_space.filter(|&s| s > start).filter(|&s| {
                    data[s + 1..]
                        .iter()
                        .take_while(|c| !c.text.chars().all(char::is_whitespace))
                        .map(|c| c.width)
                        .sum::<usize>()
                        <= available
                });
                if let Some(space) = space {
                    end = space;
                    next = space + 1;
                } else {
                    // 空白がちょうど次の行に出る場合は通常文の折返し境界として消費する。
                    while next < data.len() && data[next].text == " " {
                        next += 1;
                    }
                }
            }
            if end == start && next == start && !hard && end < data.len() {
                end += 1;
                next = end;
            }
            let source_start = data.get(start).map_or_else(
                || data.last().map_or(line.source, |c| c.source.end),
                |c| c.source.start,
            );
            let source_end = data
                .get(next.saturating_sub(1))
                .map_or(source_start, |c| c.source.end);
            let mut spans = vec![Span::raw(prefix)];
            for c in &data[start..end] {
                // 通常の最小幅20以上では書記素は収まる。単独書記素が超過した場合も破壊しない。
                if let Some(last) = spans.last_mut().filter(|s| s.style == c.style) {
                    last.content.to_mut().push_str(&c.text);
                } else {
                    spans.push(Span::styled(c.text.clone(), c.style));
                }
            }
            result.push(DisplayLine {
                line: Line::from(spans),
                source: source_start..source_end.max(source_start),
            });
            first = false;
            if hard {
                start = end + 1;
                code_continuation = false;
            } else {
                start = next;
                code_continuation = true;
            }
            if start >= data.len() && (!hard || !line.code) {
                break;
            }
        }
    }
    result
}

pub fn anchored_row(lines: &[DisplayLine], position: usize) -> usize {
    // ラベル等のゼロ長範囲より、元の本文を含む行を優先する。
    lines
        .iter()
        .position(|l| l.source.contains(&position))
        .or_else(|| lines.iter().position(|l| l.source.start >= position))
        .unwrap_or_else(|| lines.len().saturating_sub(1))
}

pub fn truncate_middle(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let left_budget = (width - 1).div_ceil(2);
    let right_budget = (width - 1) / 2;
    let mut left = String::new();
    for g in text.graphemes(true) {
        if left.width() + g.width() > left_budget {
            break;
        }
        left.push_str(g);
    }
    let mut right = Vec::new();
    let mut used = 0;
    for g in text.graphemes(true).rev() {
        if used + g.width() > right_budget {
            break;
        }
        used += g.width();
        right.push(g);
    }
    format!("{left}…{}", right.into_iter().rev().collect::<String>())
}
