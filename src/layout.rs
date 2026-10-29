use crate::{
    document::{Document, Kind, Node},
    highlight::highlight,
    search::Segment,
    source::sanitize,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Cow, ops::Range};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub style: Style,
    pub source: Range<usize>,
    pub destination: bool,
}

#[derive(Default, Debug)]
pub(crate) struct LogicalLine {
    runs: Vec<Run>,
    prefix: String,
    continuation: String,
    code: bool,
    rule: bool,
    source: usize,
    language: Option<String>,
}

/// 表示行・論理文字列のバイト範囲と、元のソースのバイト範囲の対応。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub text: Range<usize>,
    pub source: Range<usize>,
}

/// リンク・画像の参照先文字列の位置。参照先はソースと一対一に対応しないため、
/// 要素の開始位置 `id` と参照先文字列内のバイト範囲 `target` で識別する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub text: Range<usize>,
    pub id: usize,
    pub target: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct DisplayLine {
    pub line: Line<'static>,
    pub source: Range<usize>,
    pub mappings: Vec<Mapping>,
    pub image: Option<(usize, usize)>,
    pub destinations: Vec<Destination>,
}

impl DisplayLine {
    pub fn text(&self) -> String {
        self.line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

#[derive(Clone, Debug)]
struct Cell<'a> {
    text: Cow<'a, str>,
    style: Style,
    source: Range<usize>,
    width: usize,
    destination: Option<(usize, Range<usize>)>,
    offset: usize,
}

fn run(text: impl Into<String>, style: Style, range: Range<usize>) -> Run {
    Run {
        text: text.into(),
        style,
        source: range,
        destination: false,
    }
}

fn inline(nodes: &[Node], style: Style, out: &mut Vec<Run>) {
    for node in nodes {
        let r = node.source.clone();
        match &node.kind {
            Kind::Text(s) | Kind::Html(s) | Kind::Math { raw: s, .. } => out.push(run(s, style, r)),
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
                        let mut destination = run(dest_url.to_string(), style, r.clone());
                        destination.destination = true;
                        out.push(destination);
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
                    let mut destination = run(dest_url.to_string(), style, r.clone());
                    destination.destination = true;
                    out.push(destination);
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
    ) || matches!(node.kind, Kind::Math { display: true, .. })
}

fn blank(lines: &mut Vec<LogicalLine>, source: usize) {
    if lines.last().is_some_and(|l| !l.runs.is_empty() || l.rule) {
        lines.push(LogicalLine {
            source,
            ..Default::default()
        });
    }
}

fn sequence(nodes: &[Node], gaps: bool, width: usize) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        let node = &nodes[i];
        if gaps {
            blank(&mut lines, node.source.start);
        }
        if is_block(node) {
            lines.extend(block(node, width));
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

fn block(node: &Node, width: usize) -> Vec<LogicalLine> {
    let base = || LogicalLine {
        source: node.source.start,
        ..Default::default()
    };
    match &node.kind {
        Kind::Rule => vec![LogicalLine {
            rule: true,
            ..base()
        }],
        Kind::Math { raw, .. } => vec![LogicalLine {
            runs: vec![run(raw, Style::default(), node.source.clone())],
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
            let mut lines = sequence(&node.children, true, width.saturating_sub(2));
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
                let mut contents =
                    sequence(&item.children, loose, width.saturating_sub(marker.width()));
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
                language: match kind {
                    CodeBlockKind::Fenced(language) if !language.is_empty() => {
                        Some(language.to_string())
                    }
                    _ => None,
                },
                ..base()
            });
            lines
        }
        Kind::Element(Tag::Table(_)) if width == 0 => node
            .children
            .iter()
            .flat_map(|row| row.children.iter())
            .map(|cell| {
                let mut runs = Vec::new();
                inline(&cell.children, Style::default(), &mut runs);
                LogicalLine {
                    runs,
                    source: cell.source.start,
                    ..Default::default()
                }
            })
            .collect(),
        Kind::Element(Tag::Table(alignments)) => {
            grid_table(node, alignments, width).unwrap_or_else(|| table(node))
        }
        _ => sequence(&node.children, true, width.saturating_sub(2)),
    }
}

/// 見出しセルの本文。空の見出しは `(列N)` のラベルで置き換える。
fn header_runs(cell: &Node, index: usize, label_source: Range<usize>) -> Vec<Run> {
    let mut runs = Vec::new();
    inline(
        &cell.children,
        Style::default().add_modifier(Modifier::BOLD),
        &mut runs,
    );
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        runs = vec![run(
            format!("(列{})", index + 1),
            Style::default(),
            label_source,
        )];
    }
    runs
}

fn grid_table(node: &Node, alignments: &[Alignment], width: usize) -> Option<Vec<LogicalLine>> {
    let mut rows: Vec<Vec<Vec<Run>>> = Vec::new();
    let mut sources = Vec::new();
    for row in &node.children {
        if !matches!(row.kind, Kind::Element(Tag::TableHead | Tag::TableRow)) {
            continue;
        }
        let header = rows.is_empty();
        let mut cells = Vec::new();
        for (i, cell) in row.children.iter().enumerate() {
            let label_source = cell.source.start..cell.source.start;
            if header {
                cells.push(header_runs(cell, i, label_source));
                continue;
            }
            let mut runs = Vec::new();
            inline(&cell.children, Style::default(), &mut runs);
            if runs.iter().all(|r| r.text.trim().is_empty()) {
                runs = vec![run("(empty)", Style::default(), label_source)];
            }
            cells.push(runs);
        }
        rows.push(cells);
        sources.push(row.source.start);
    }
    let count = alignments.len();
    let mut widths = vec![0; count];
    for row in &rows {
        for (i, cell) in row.iter().enumerate().take(count) {
            let text: String = cell.iter().map(|r| sanitize(&r.text)).collect();
            if text.contains('\n') || text.contains('\t') {
                return None;
            }
            widths[i] = widths[i].max(text.width());
        }
    }
    let no_row = "(no row)";
    if rows.len() == 1 {
        // 空表のメッセージにも左右1セルを確保し、セルを結合した本文行へ収める。
        let natural = widths.iter().sum::<usize>() + 3 * count + 1;
        let extra = (no_row.width() + 4).saturating_sub(natural);
        for (i, column) in widths.iter_mut().enumerate() {
            *column += extra / count + usize::from(i < extra % count);
        }
    }
    let table_width = widths.iter().sum::<usize>() + 3 * count + 1;
    if table_width > width {
        return None;
    }
    let border = |left: &str, middle: &str, right: &str, source: usize| LogicalLine {
        runs: vec![run(
            format!(
                "{left}{}{right}",
                widths
                    .iter()
                    .map(|w| "─".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join(middle)
            ),
            Style::default(),
            source..source,
        )],
        source,
        ..Default::default()
    };
    let mut result = vec![border("┌", "┬", "┐", node.source.start)];
    for (row_index, row) in rows.iter().enumerate() {
        let source = sources[row_index];
        let mut runs = vec![run("│", Style::default(), source..source)];
        for (i, cell) in row.iter().enumerate().take(count) {
            let text: String = cell.iter().map(|r| sanitize(&r.text)).collect();
            let extra = widths[i].saturating_sub(text.width());
            let left = match alignments[i] {
                Alignment::Right => extra,
                Alignment::Center => extra / 2,
                _ => 0,
            };
            runs.push(run(" ".repeat(1 + left), Style::default(), source..source));
            runs.extend(cell.clone());
            runs.push(run(
                format!("{}│", " ".repeat(1 + extra - left)),
                Style::default(),
                source..source,
            ));
        }
        result.push(LogicalLine {
            runs,
            source,
            ..Default::default()
        });
        if row_index == 0 {
            result.push(border(
                "├",
                if rows.len() == 1 { "┴" } else { "┼" },
                "┤",
                source,
            ));
        }
    }
    if rows.len() == 1 {
        let remaining = table_width - 2 - no_row.width();
        let left = remaining / 2;
        result.push(LogicalLine {
            runs: vec![run(
                format!(
                    "│{}{no_row}{}│",
                    " ".repeat(left),
                    " ".repeat(remaining - left)
                ),
                Style::default(),
                node.source.start..node.source.start,
            )],
            source: node.source.start,
            ..Default::default()
        });
    }
    result.push(border(
        "└",
        if rows.len() == 1 { "─" } else { "┴" },
        "┘",
        node.source.end,
    ));
    Some(result)
}

fn table(node: &Node) -> Vec<LogicalLine> {
    let mut headers: Vec<Vec<Run>> = Vec::new();
    let mut rows = Vec::new();
    for child in &node.children {
        if matches!(child.kind, Kind::Element(Tag::TableHead)) {
            for (idx, cell) in child.children.iter().enumerate() {
                headers.push(header_runs(cell, idx, cell.source.clone()));
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
            runs: vec![run("(no row)", Style::default(), node.source.clone())],
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

fn cells<'a>(
    line: &LogicalLine,
    document: &Document,
    colors: bool,
    rendered: &'a str,
) -> Vec<Cell<'a>> {
    struct Mapping<'a> {
        run: &'a Run,
        start: usize,
        end: usize,
        raw: &'a str,
        content_start: Option<usize>,
        cursor: usize,
    }
    let mut mappings = Vec::new();
    let mut offset = 0;
    for run in &line.runs {
        if run.text.is_empty() {
            continue;
        }
        let start = offset;
        offset += run.text.len();
        let raw = document.source.get(run.source.clone()).unwrap_or("");
        mappings.push(Mapping {
            run,
            start,
            end: offset,
            raw,
            content_start: raw.find(run.text.as_str()),
            cursor: 0,
        });
    }
    let mut result = Vec::new();
    let mut column = 0;
    let mut first = 0;
    for (offset, g) in rendered.grapheme_indices(true) {
        // 書記素とrunは両方とも先頭から順に進む。毎回の二分探索は不要。
        while mappings[first].end <= offset {
            first += 1;
        }
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
            let range = if m.run.destination && m.content_start.is_none() {
                m.run.source.end.saturating_sub(1)..m.run.source.end
            } else if let Some(start) = m.content_start {
                m.run.source.start + start + local..m.run.source.start + start + end
            } else if let Some(start) = m.raw[m.cursor..].find(part) {
                let begin = m.cursor + start;
                m.cursor = begin + part.len();
                m.run.source.start + begin..m.run.source.start + m.cursor
            } else if part == " " && m.raw.chars().all(char::is_whitespace) {
                m.run.source.clone()
            } else if m.raw[m.cursor..].starts_with('&')
                && let Some(end) = m.raw[m.cursor..].find(';')
            {
                let begin = m.cursor;
                m.cursor += end + 1;
                m.run.source.start + begin..m.run.source.start + m.cursor
            } else {
                m.run.source.clone()
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
                destination: None,
                offset,
            });
            column = 0;
            continue;
        }
        let text = if g == "\t" {
            Cow::Owned(" ".repeat(4 - column % 4))
        } else if g.chars().any(char::is_control) {
            Cow::Owned(sanitize(g))
        } else {
            Cow::Borrowed(g)
        };
        let width = text.width();
        column += width;
        let destination = mappings[first].run.destination.then(|| {
            (
                mappings[first].run.source.start,
                offset - mappings[first].start..offset - mappings[first].start + g.len(),
            )
        });
        result.push(Cell {
            text,
            style,
            source,
            width,
            destination,
            offset,
        });
    }
    result
}

pub fn layout(document: &Document, width: usize, colors: bool) -> Vec<DisplayLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut logical = sequence(&document.nodes, true, width);
    if logical.is_empty() {
        logical.push(LogicalLine {
            runs: vec![run("(empty)", Style::default(), 0..0)],
            ..Default::default()
        });
    }
    let mut result = Vec::new();
    for line in logical {
        if line.rule {
            result.push(rule_line(&line, width));
            continue;
        }
        // 装飾境界をまたぐ書記素を維持し、通常文字のCellはこの文字列を借用する。
        let rendered: String = line.runs.iter().map(|r| r.text.as_str()).collect();
        let mut data = cells(&line, document, colors, &rendered);
        if colors && let Some(language) = &line.language {
            apply_highlight(&mut data, &rendered, language);
        }
        wrap(&line, &data, width, &mut result);
    }
    result
}

/// 字下げを入れると本文が4セル未満になる狭い画面では、外側の字下げを省く。
fn outer_prefix(prefix: &str, width: usize) -> &str {
    if prefix.width() + 4 <= width {
        prefix
    } else {
        ""
    }
}

fn rule_line(line: &LogicalLine, width: usize) -> DisplayLine {
    let prefix = outer_prefix(&line.prefix, width);
    DisplayLine {
        line: Line::raw(format!("{prefix}{}", "─".repeat(width - prefix.width()))),
        source: line.source..line.source,
        mappings: Vec::new(),
        image: None,
        destinations: Vec::new(),
    }
}

/// ソース対応を作ってから装飾だけを重ねる。引用・字下げでコードの元位置が飛んでも対応を壊さない。
fn apply_highlight(data: &mut [Cell], rendered: &str, language: &str) {
    let tokens = highlight(
        vec![run(rendered, Style::default(), 0..rendered.len())],
        language,
    );
    let mut token = 0;
    for cell in data {
        while token + 1 < tokens.len() && tokens[token].source.end <= cell.offset {
            token += 1;
        }
        if let Some(highlight) = tokens.get(token) {
            cell.style = highlight.style;
        }
    }
}

/// 論理行の書記素セルを本文幅で表示行へ折り返す。
fn wrap(line: &LogicalLine, data: &[Cell], width: usize, result: &mut Vec<DisplayLine>) {
    let mut start = 0;
    let mut first = true;
    let mut code_continuation = false;
    loop {
        let outer = outer_prefix(
            if first {
                &line.prefix
            } else {
                &line.continuation
            },
            width,
        );
        let marker = if line.code && code_continuation {
            "↪ "
        } else {
            ""
        };
        let prefix = format!("{outer}{marker}");
        let available = width.saturating_sub(prefix.width()).max(1);
        let (end, next, hard) = break_point(data, start, available, line.code);
        let source_start = data.get(start).map_or_else(
            || data.last().map_or(line.source, |c| c.source.end),
            |c| c.source.start,
        );
        let source_end = data[start..next]
            .iter()
            .map(|c| c.source.end)
            .max()
            .unwrap_or(source_start);
        result.push(display_line(
            prefix,
            &data[start..end],
            source_start..source_end.max(source_start),
        ));
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

/// `start` から始まる表示行の終端 `end`、次の行の開始 `next`、改行で終わるかを返す。
fn break_point(data: &[Cell], start: usize, available: usize, code: bool) -> (usize, usize, bool) {
    let mut end = start;
    let mut used = 0;
    let mut last_space = None;
    while end < data.len() && data[end].text != "\n" && used + data[end].width <= available {
        used += data[end].width;
        if data[end].text.chars().all(char::is_whitespace) {
            last_space = Some(end);
        }
        end += 1;
    }
    let hard = end < data.len() && data[end].text == "\n";
    let mut next = end;
    if !code && !hard && end < data.len() {
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
    (end, next, hard)
}

fn display_line(prefix: String, cells: &[Cell], source: Range<usize>) -> DisplayLine {
    let mut byte_offset = prefix.len();
    let mut mappings = Vec::new();
    let mut destinations = Vec::new();
    let mut spans = vec![Span::raw(prefix)];
    for c in cells {
        let text = byte_offset..byte_offset + c.text.len();
        if let Some((id, target)) = &c.destination {
            destinations.push(Destination {
                text: text.clone(),
                id: *id,
                target: target.clone(),
            });
        }
        mappings.push(Mapping {
            text,
            source: c.source.clone(),
        });
        byte_offset += c.text.len();
        // 通常の最小幅20以上では書記素は収まる。単独書記素が超過した場合も破壊しない。
        if let Some(last) = spans.last_mut().filter(|s| s.style == c.style) {
            last.content.to_mut().push_str(&c.text);
        } else {
            spans.push(Span::styled(c.text.to_string(), c.style));
        }
    }
    DisplayLine {
        line: Line::from(spans),
        source,
        mappings,
        image: None,
        destinations,
    }
}

/// 折返し・表形式によらない論理文字列とUTF-8ソース範囲。生成ラベルは除く。
pub fn search_segments(document: &Document) -> Vec<Segment> {
    let mut result = Vec::new();
    for line in sequence(&document.nodes, true, 0) {
        let rendered: String = line
            .runs
            .iter()
            .filter(|r| !r.source.is_empty())
            .map(|r| r.text.as_str())
            .collect();
        let filtered = LogicalLine {
            runs: line
                .runs
                .into_iter()
                .filter(|r| !r.source.is_empty())
                .collect(),
            ..Default::default()
        };
        let mut segment = Segment::default();
        for cell in cells(&filtered, document, false, &rendered) {
            if cell.text == "\n" {
                if !segment.text.is_empty() {
                    result.push(segment);
                }
                segment = Segment::default();
            } else {
                let start = segment.text.len();
                segment.text.push_str(&cell.text);
                let previous_destination = segment
                    .destinations
                    .last()
                    .is_some_and(|d| d.text.end == start);
                if let Some((id, target)) = &cell.destination {
                    segment.destinations.push(Destination {
                        text: start..segment.text.len(),
                        id: *id,
                        target: target.clone(),
                    });
                }
                // 1バイトずつ対応する連続した本文は一つの対応にまとめる。
                if let Some(last) = segment.mappings.last_mut().filter(|last| {
                    last.text.len() == last.source.len()
                        && cell.text.len() == cell.source.len()
                        && last.source.end == cell.source.start
                        && cell.destination.is_none()
                        && !previous_destination
                }) {
                    last.text.end = segment.text.len();
                    last.source.end = cell.source.end;
                } else {
                    segment.mappings.push(Mapping {
                        text: start..segment.text.len(),
                        source: cell.source,
                    });
                }
            }
        }
        if !segment.text.is_empty() {
            result.push(segment);
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
