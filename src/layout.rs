use crate::{
    document::{Document, Kind, Node},
    highlight::{CodeOffsets, CodeRun, Key},
    search::Segment,
    source::sanitize,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{borrow::Cow, collections::HashMap, ops::Range};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    pub style: Style,
    pub source: Range<usize>,
    pub destination: bool,
    /// インライン数式の要素全体（区切りを含む）。画像にするときは一つのセルにまとめる。
    pub math: bool,
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

impl LogicalLine {
    /// 字下げ・コード等の属性を持たない論理行。
    fn new(source: usize, runs: Vec<Run>) -> Self {
        Self {
            runs,
            source,
            ..Default::default()
        }
    }
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

/// 画像にしたインライン数式の、セル単位の大きさ（仕様7.4節）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineSize {
    pub columns: usize,
    pub rows: usize,
    /// 本文の行より上にはみ出す行数。
    pub above: usize,
}

/// 表示行に置くインライン数式の画像の一部。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formula {
    /// 要素のソース開始位置。
    pub source: usize,
    /// 本文の左端からの列。
    pub column: usize,
    /// この表示行に置く画像内の行。
    pub row: usize,
    /// 画像の下に敷く空白の、表示行の文字列内のバイト範囲。本文の行以外では空。
    pub text: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct DisplayLine {
    pub line: Line<'static>,
    pub source: Range<usize>,
    pub mappings: Vec<Mapping>,
    /// 行全体を使う画像（ブロック数式・ローカル画像）の要素のソース開始位置と、画像内の行。
    pub image: Option<(usize, usize)>,
    /// 行内に置くインライン数式の画像。
    pub formulas: Vec<Formula>,
    pub destinations: Vec<Destination>,
    /// ハイライトするコードの表示行の、論理行の文字列との対応。色は描画時に重ねる。
    pub code: Option<CodeOffsets>,
}

impl DisplayLine {
    /// 画像・参照先を持たない表示行。
    pub fn new(line: Line<'static>, source: Range<usize>, mappings: Vec<Mapping>) -> Self {
        Self {
            line,
            source,
            mappings,
            image: None,
            formulas: Vec::new(),
            destinations: Vec::new(),
            code: None,
        }
    }
    pub fn text(&self) -> String {
        self.line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    /// この表示行に置く画像の要素のソース開始位置・画像内の行・本文の左端からの列。
    pub fn placements(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.image
            .map(|(source, row)| (source, row, 0))
            .into_iter()
            .chain(self.formulas.iter().map(|f| (f.source, f.row, f.column)))
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
    /// 画像にしたインライン数式。セルの幅は画像の列数で、途中で折り返さない。
    formula: Option<(usize, InlineSize)>,
}

impl Cell<'_> {
    /// 折返しの境界にできる空白。画像の下に敷く空白は含めない。
    fn is_space(&self) -> bool {
        self.formula.is_none() && self.text.chars().all(char::is_whitespace)
    }
}

fn run(text: impl Into<String>, style: Style, range: Range<usize>) -> Run {
    Run {
        text: text.into(),
        style,
        source: range,
        destination: false,
        math: false,
    }
}

fn inline(nodes: &[Node], style: Style, out: &mut Vec<Run>) {
    for node in nodes {
        let r = node.source.clone();
        match &node.kind {
            Kind::Math {
                raw,
                display: false,
            } => out.push(Run {
                math: true,
                ..run(raw, style, r)
            }),
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
        lines.push(LogicalLine::new(source, Vec::new()));
    }
}

/// `images` は画像として表示する要素の範囲。最上位の段落・見出しでだけ使い、入れ子では空にする。
fn sequence(nodes: &[Node], gaps: bool, width: usize, images: &[Range<usize>]) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        let node = &nodes[i];
        if gaps {
            blank(&mut lines, node.source.start);
        }
        if is_block(node) {
            lines.extend(block(node, width, images));
            i += 1;
        } else {
            let start = i;
            while i < nodes.len() && !is_block(&nodes[i]) {
                i += 1;
            }
            let mut runs = Vec::new();
            inline(&nodes[start..i], Style::default(), &mut runs);
            lines.push(LogicalLine::new(node.source.start, runs));
        }
    }
    lines
}

/// 画像として表示する要素の前後で行を分ける（仕様8.3節）。区切りの前の行末と後の行頭には
/// 空白を残さない。
fn split_images(runs: Vec<Run>, images: &[Range<usize>]) -> Vec<Run> {
    if images.is_empty() {
        return runs;
    }
    let image_of = |run: &Run| {
        images
            .iter()
            .position(|r| r.start <= run.source.start && run.source.end <= r.end)
    };
    let mut result: Vec<Run> = Vec::with_capacity(runs.len() + 2);
    let mut current = None;
    for mut run in runs {
        let image = image_of(&run);
        if image == current {
            result.push(run);
            continue;
        }
        let leaving = image.is_none();
        current = image;
        // 区切りの前後の本文は行末・行頭の空白を除く。ソースとの対応は文字列の検索で保たれる。
        if leaving && !result.is_empty() {
            run.text = run.text.trim_start().into();
        } else if let Some(last) = result.last_mut().filter(|last| !last.source.is_empty()) {
            // 見出しの記号等の生成した文字列は除かない。
            last.text = last.text.trim_end().into();
        }
        match result.last_mut() {
            None => {}
            Some(last) if last.text == "\n" => {}
            Some(last) => {
                let (style, at) = (last.style, last.source.end);
                result.push(Run {
                    text: "\n".into(),
                    style,
                    source: at..at,
                    destination: false,
                    math: false,
                });
            }
        }
        // 空白だけの本文（連続する画像の間の改行等）は、区切りの改行と重ねない。
        if !run.text.is_empty() {
            result.push(run);
        }
    }
    result
}

fn block(node: &Node, width: usize, images: &[Range<usize>]) -> Vec<LogicalLine> {
    let base = || LogicalLine::new(node.source.start, Vec::new());
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
            vec![LogicalLine {
                runs: split_images(runs, images),
                ..base()
            }]
        }
        Kind::Element(Tag::Paragraph | Tag::HtmlBlock) => {
            let mut runs = Vec::new();
            inline(&node.children, Style::default(), &mut runs);
            vec![LogicalLine {
                runs: split_images(runs, images),
                ..base()
            }]
        }
        Kind::Element(Tag::BlockQuote(_)) => {
            let mut lines = sequence(&node.children, true, width.saturating_sub(2), &[]);
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
                let mut contents = sequence(
                    &item.children,
                    loose,
                    width.saturating_sub(marker.width()),
                    &[],
                );
                if contents.is_empty() {
                    contents.push(LogicalLine::new(item.source.start, Vec::new()));
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
                LogicalLine::new(cell.source.start, runs)
            })
            .collect(),
        Kind::Element(Tag::Table(alignments)) => {
            grid_table(node, alignments, width).unwrap_or_else(|| table(node))
        }
        _ => sequence(&node.children, true, width.saturating_sub(2), &[]),
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
    let border = |left: &str, middle: &str, right: &str, source: usize| {
        LogicalLine::new(
            source,
            vec![run(
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
        )
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
        result.push(LogicalLine::new(source, runs));
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
        result.push(LogicalLine::new(
            node.source.start,
            vec![run(
                format!(
                    "│{}{no_row}{}│",
                    " ".repeat(left),
                    " ".repeat(remaining - left)
                ),
                Style::default(),
                node.source.start..node.source.start,
            )],
        ));
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
            result.push(LogicalLine::new(node.source.start, runs));
        }
        result.push(LogicalLine::new(
            node.source.start,
            vec![run("(no row)", Style::default(), node.source.clone())],
        ));
        return result;
    }
    for (idx, row) in rows.into_iter().enumerate() {
        blank(&mut result, row.source.start);
        result.push(LogicalLine::new(
            row.source.start,
            vec![run(
                format!("[row {}]", idx + 1),
                Style::default(),
                row.source.start..row.source.start,
            )],
        ));
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
            result.push(LogicalLine::new(range.start, runs));
        }
    }
    result
}

/// 論理行の文字列内でのrunの位置と、その元のソース。
struct RunSource<'a> {
    run: &'a Run,
    /// 論理行の文字列内でのバイト範囲。
    start: usize,
    end: usize,
    /// runに対応する元のソース。
    raw: &'a str,
    /// runの文字列がソースにそのまま現れる場合の、`raw` 内の開始位置。
    content_start: Option<usize>,
    /// 書記素ごとに探す場合の、`raw` 内の次の探索開始位置。
    cursor: usize,
}

impl<'a> RunSource<'a> {
    fn new(run: &'a Run, start: usize, document: &'a Document) -> Self {
        let raw = document.source.get(run.source.clone()).unwrap_or("");
        Self {
            run,
            start,
            end: start + run.text.len(),
            raw,
            content_start: raw.find(run.text.as_str()),
            cursor: 0,
        }
    }

    /// run内のバイト範囲 `local..end` に対応するソース範囲。
    fn source(&mut self, local: usize, end: usize) -> Range<usize> {
        let base = self.run.source.start;
        let part = &self.run.text[local..end];
        if self.run.destination && self.content_start.is_none() {
            // 参照先がリンク要素のソースに現れない（参照リンク等）。要素の末尾1バイトに対応させる。
            self.run.source.end.saturating_sub(1)..self.run.source.end
        } else if let Some(start) = self.content_start {
            // runの文字列がソースにそのまま現れる。
            base + start + local..base + start + end
        } else if let Some(start) = self.raw[self.cursor..].find(part) {
            // エスケープ等でrun全体は一致しない。前回の続きから書記素を探す。
            let begin = self.cursor + start;
            self.cursor = begin + part.len();
            base + begin..base + self.cursor
        } else if part == " " && self.raw.chars().all(char::is_whitespace) {
            // ソフト改行等、空白だけのソースから作った空白はrun全体に対応させる。
            self.run.source.clone()
        } else if self.raw[self.cursor..].starts_with('&')
            && let Some(end) = self.raw[self.cursor..].find(';')
        {
            // 文字参照は、デコード前の `&...;` 全体に対応させる。
            let begin = self.cursor;
            self.cursor += end + 1;
            base + begin..base + self.cursor
        } else {
            self.run.source.clone()
        }
    }
}

/// 論理行を書記素ごとのセルにする。`formulas` の数式は、列数が `max_columns` 以下なら
/// 画像の幅の一つのセルにする。
fn cells<'a>(
    line: &LogicalLine,
    document: &Document,
    colors: bool,
    rendered: &'a str,
    formulas: &HashMap<usize, InlineSize>,
    max_columns: usize,
) -> Vec<Cell<'a>> {
    let mut mappings = Vec::new();
    let mut offset = 0;
    for run in &line.runs {
        if run.text.is_empty() {
            continue;
        }
        mappings.push(RunSource::new(run, offset, document));
        offset += run.text.len();
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
        let current = &mappings[first];
        if current.run.math
            && let Some(&size) = formulas
                .get(&current.run.source.start)
                .filter(|size| size.columns <= max_columns)
        {
            // 数式の文字列は表示せず、先頭の書記素の位置に画像の幅の空白を一つ置く。
            if offset == current.start {
                result.push(Cell {
                    text: Cow::Owned(" ".repeat(size.columns)),
                    style,
                    source: current.run.source.clone(),
                    width: size.columns,
                    destination: None,
                    offset,
                    formula: Some((current.run.source.start, size)),
                });
                column += size.columns;
            }
            continue;
        }
        let mut source = None::<Range<usize>>;
        for m in &mut mappings[first..] {
            if m.start >= offset + g.len() {
                break;
            }
            let local = offset.saturating_sub(m.start);
            let end = (offset + g.len()).min(m.end) - m.start;
            let range = m.source(local, end);
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
                formula: None,
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
            formula: None,
        });
    }
    result
}

/// コードのハイライトの計算を待ち、色を付けた表示行を作る。
pub fn layout(document: &Document, width: usize, colors: bool) -> Vec<DisplayLine> {
    let mut lines = layout_with_images(document, width, colors, &[], &HashMap::new());
    document.highlights.wait();
    for line in &mut lines {
        if let (Some(styled), _) = document.highlights.apply(line) {
            line.line = styled;
        }
    }
    lines
}

/// `images` の範囲の画像を、前後の本文から分けた行にして表示行を作る。範囲は
/// `Document::local_images` が返す最上位の段落・見出しの画像に限る。`formulas` は画像にした
/// インライン数式の大きさで、行に収まるものを本文中の画像にする。コードのハイライトは
/// 別スレッドへ依頼するだけで待たず、色は描画時に `HighlightCache::apply` で重ねる。
pub fn layout_with_images(
    document: &Document,
    width: usize,
    colors: bool,
    images: &[Range<usize>],
    formulas: &HashMap<usize, InlineSize>,
) -> Vec<DisplayLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut logical = sequence(&document.nodes, true, width, images);
    if logical.is_empty() {
        logical.push(LogicalLine::new(
            0,
            vec![run("(empty)", Style::default(), 0..0)],
        ));
    }
    let mut result = Vec::new();
    for line in logical {
        if line.rule {
            result.push(rule_line(&line, width));
            continue;
        }
        // 装飾境界をまたぐ書記素を維持し、通常文字のCellはこの文字列を借用する。
        let rendered: String = line.runs.iter().map(|r| r.text.as_str()).collect();
        // 字下げの後に残る幅を超える数式は、折り返せないためTeX表示のままにする。
        let indent = outer_prefix(&line.prefix, width)
            .width()
            .max(outer_prefix(&line.continuation, width).width());
        let max_columns = if line.code {
            0
        } else {
            width.saturating_sub(indent)
        };
        let data = cells(&line, document, colors, &rendered, formulas, max_columns);
        let code = match &line.language {
            Some(language) if colors => {
                let key = (line.source, rendered.len());
                document.highlights.request(key, &rendered, language);
                Some(key)
            }
            _ => None,
        };
        wrap(&line, &data, width, code, &mut result);
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
    DisplayLine::new(
        Line::raw(format!("{prefix}{}", "─".repeat(width - prefix.width()))),
        line.source..line.source,
        Vec::new(),
    )
}

/// 論理行の書記素セルを本文幅で表示行へ折り返す。`code` はハイライトするコードブロック。
fn wrap(
    line: &LogicalLine,
    data: &[Cell],
    width: usize,
    code: Option<Key>,
    result: &mut Vec<DisplayLine>,
) {
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
        push_with_formulas(
            display_line(
                prefix,
                &data[start..end],
                source_start..source_end.max(source_start),
                code,
            ),
            &data[start..end],
            result,
        );
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

/// 表示行を加える。行内の数式の画像が上下にはみ出す分は、画像だけの行を前後に加える。
fn push_with_formulas(line: DisplayLine, cells: &[Cell], result: &mut Vec<DisplayLine>) {
    let sizes: Vec<_> = cells.iter().filter_map(|c| c.formula).collect();
    let up = sizes.iter().map(|(_, s)| s.above).max().unwrap_or(0);
    let down = sizes
        .iter()
        .map(|(_, s)| s.rows - 1 - s.above)
        .max()
        .unwrap_or(0);
    // 本文の行からの相対位置 `offset` の行に置く画像の部分。
    let extra = |offset: isize| {
        let formulas = line
            .formulas
            .iter()
            .zip(&sizes)
            .filter_map(|(formula, (_, size))| {
                let row = usize::try_from(size.above as isize + offset).ok()?;
                (row < size.rows).then(|| Formula {
                    row,
                    text: 0..0,
                    ..formula.clone()
                })
            })
            .collect();
        DisplayLine {
            formulas,
            ..DisplayLine::new(Line::raw(""), line.source.clone(), Vec::new())
        }
    };
    for offset in (1..=up).rev() {
        result.push(extra(-(offset as isize)));
    }
    let below: Vec<_> = (1..=down).map(|offset| extra(offset as isize)).collect();
    result.push(line);
    result.extend(below);
}

/// `start` から始まる表示行の終端 `end`、次の行の開始 `next`、改行で終わるかを返す。
fn break_point(data: &[Cell], start: usize, available: usize, code: bool) -> (usize, usize, bool) {
    let mut end = start;
    let mut used = 0;
    let mut last_space = None;
    while end < data.len() && data[end].text != "\n" && used + data[end].width <= available {
        used += data[end].width;
        if data[end].is_space() {
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
                .take_while(|c| !c.is_space())
                .map(|c| c.width)
                .sum::<usize>()
                <= available
        });
        if let Some(space) = space {
            end = space;
            next = space + 1;
        } else {
            // 空白がちょうど次の行に出る場合は通常文の折返し境界として消費する。
            while next < data.len() && data[next].text == " " && data[next].is_space() {
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

fn display_line(
    prefix: String,
    cells: &[Cell],
    source: Range<usize>,
    code: Option<Key>,
) -> DisplayLine {
    let mut byte_offset = prefix.len();
    let mut mappings = Vec::new();
    let mut destinations = Vec::new();
    let mut runs: Vec<CodeRun> = Vec::new();
    // 直前のセルが表示行と論理行で同じ文字列なら、その終端（表示行の位置、論理行の位置）。
    let mut verbatim_end = None;
    let mut column = prefix.width();
    let mut formulas = Vec::new();
    let mut spans = vec![Span::raw(prefix)];
    for c in cells {
        let text = byte_offset..byte_offset + c.text.len();
        if let Some((source, size)) = c.formula {
            formulas.push(Formula {
                source,
                column,
                row: size.above,
                text: text.clone(),
            });
        }
        column += c.width;
        if code.is_some() {
            // タブの展開・制御文字の置換は表示行の文字列だけが変わる。
            let verbatim = matches!(c.text, Cow::Borrowed(_));
            if !(verbatim && verbatim_end == Some((text.start, c.offset))) {
                runs.push(CodeRun {
                    text: text.start,
                    rendered: c.offset,
                    verbatim,
                });
            }
            verbatim_end = verbatim.then_some((text.end, c.offset + c.text.len()));
        }
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
        destinations,
        formulas,
        code: code.map(|key| CodeOffsets { key, runs }),
        ..DisplayLine::new(Line::from(spans), source, mappings)
    }
}

/// 折返し・表形式によらない論理文字列とUTF-8ソース範囲。生成ラベルは除く。
pub fn search_segments(document: &Document) -> Vec<Segment> {
    let mut result = Vec::new();
    for line in sequence(&document.nodes, true, 0, &[]) {
        let rendered: String = line
            .runs
            .iter()
            .filter(|r| !r.source.is_empty())
            .map(|r| r.text.as_str())
            .collect();
        let filtered = LogicalLine::new(
            0,
            line.runs
                .into_iter()
                .filter(|r| !r.source.is_empty())
                .collect(),
        );
        let mut segment = Segment::default();
        for cell in cells(&filtered, document, false, &rendered, &HashMap::new(), 0) {
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
