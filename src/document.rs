use pulldown_cmark::{Event, Options, Parser, Tag};
use std::ops::Range;

#[derive(Debug)]
pub enum Kind {
    Element(Tag<'static>),
    Text(String),
    Code(String),
    Html(String),
    Math(String),
    SoftBreak,
    HardBreak,
    Rule,
    Task(bool),
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    pub source: Range<usize>,
    pub children: Vec<Node>,
}

#[derive(Debug)]
pub struct Document {
    pub source: String,
    pub line_starts: Vec<usize>,
    pub nodes: Vec<Node>,
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

#[derive(Debug)]
struct Replacement {
    original: Range<usize>,
    transformed: Range<usize>,
    block: bool,
}

fn escaped(bytes: &[u8], at: usize) -> bool {
    bytes[..at]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
        % 2
        == 1
}

fn single_dollar(bytes: &[u8], at: usize) -> bool {
    bytes[at] == b'$'
        && (at == 0 || bytes[at - 1] != b'$')
        && bytes.get(at + 1) != Some(&b'$')
        && !escaped(bytes, at)
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut result: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = result.last_mut().filter(|last| last.end >= range.start) {
            last.end = last.end.max(range.end);
        } else {
            result.push(range);
        }
    }
    result
}

fn overlaps(ranges: &[Range<usize>], range: &Range<usize>) -> bool {
    let idx = ranges.partition_point(|r| r.end <= range.start);
    ranges.get(idx).is_some_and(|r| r.start < range.end)
}

/// 通常パースの構造を使い、コード・HTML・参照先を避けて数式を走査する。
/// 数式だけを一時的なコード要素にし、数式内のMarkdown再解釈を防ぐ。
fn math_ranges(source: &str) -> Vec<(Range<usize>, bool)> {
    let mut forbidden = Vec::new();
    let mut containers = Vec::new();
    let mut text = Vec::new();
    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_) => forbidden.push(range),
            Event::Start(Tag::List(_) | Tag::BlockQuote(_) | Tag::Table(_)) => {
                containers.push(range)
            }
            Event::Text(_) => text.push(range),
            _ => {}
        }
    }
    let mut forbidden = merge_ranges(forbidden);
    let containers = merge_ranges(containers);
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        lines.push((offset, line.trim_end_matches('\n')));
        offset += line.len();
    }
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (start, line) = lines[i];
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent <= 3 && trimmed.starts_with("$$") && (i == 0 || lines[i - 1].1.trim().is_empty())
        {
            let mut end_line = None;
            if trimmed.len() > 4
                && trimmed.ends_with("$$")
                && !trimmed.starts_with("$$$")
                && !trimmed.ends_with("$$$")
                && !trimmed[2..trimmed.len() - 2].trim().is_empty()
            {
                end_line = Some(i);
            } else if trimmed == "$$" {
                for j in i + 1..lines.len() {
                    let l = lines[j].1;
                    if l.trim() == "$$" && l.len() - l.trim_start_matches(' ').len() <= 3 {
                        if !source[start + line.len()..lines[j].0].trim().is_empty() {
                            end_line = Some(j);
                        }
                        break;
                    }
                }
            }
            if let Some(j) = end_line {
                let end = lines[j].0 + lines[j].1.len();
                let range = start..end;
                if (j + 1 == lines.len() || lines[j + 1].1.trim().is_empty())
                    && !overlaps(&forbidden, &(start..start + line.len()))
                    && !overlaps(&containers, &(start..start + line.len()))
                {
                    found.push((range, true));
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    // Textイベントの範囲に区切りがあることを要求するため、URL内等は候補にならない。
    let eligible = |at: usize| {
        let idx = text.partition_point(|r| r.end <= at);
        text.get(idx).is_some_and(|r| r.contains(&at))
    };
    forbidden.extend(found.iter().map(|(r, _)| r.clone()));
    let forbidden = merge_ranges(forbidden);
    let bytes = source.as_bytes();
    for (start, line) in lines {
        let end = start + line.len();
        let mut opening = None;
        for p in start..end {
            if !single_dollar(bytes, p) || !eligible(p) {
                continue;
            }
            if opening.is_some_and(|o| overlaps(&forbidden, &(o..p + 1))) {
                opening = None;
            }
            if let Some(o) = opening
                && source[..p]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace())
                && !bytes.get(p + 1).is_some_and(u8::is_ascii_digit)
            {
                found.push((o..p + 1, false));
                opening = None;
            } else if opening.is_none()
                && !overlaps(&forbidden, &(p..p + 1))
                && source[p + 1..end]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace())
            {
                opening = Some(p);
            }
        }
    }
    found.sort_by_key(|(r, _)| r.start);
    found
}

impl Document {
    pub fn parse(source: String) -> Self {
        let ranges = math_ranges(&source);
        let mut transformed = String::new();
        let mut replacements = Vec::new();
        let mut previous = 0;
        for (idx, (range, block)) in ranges.into_iter().enumerate() {
            transformed.push_str(&source[previous..range.start]);
            let start = transformed.len();
            if block {
                transformed.push_str(&format!("```\nmdview-math-{idx}\n```"));
            } else {
                transformed.push_str(&format!("`mdview-math-{idx}`"));
            }
            replacements.push(Replacement {
                original: range.clone(),
                transformed: start..transformed.len(),
                block,
            });
            previous = range.end;
        }
        transformed.push_str(&source[previous..]);
        let map = |pos: usize| -> usize {
            let idx = replacements.partition_point(|r| r.transformed.end <= pos);
            if let Some(r) = replacements
                .get(idx)
                .filter(|r| r.transformed.contains(&pos))
            {
                return r.original.start;
            }
            if idx == 0 {
                pos
            } else {
                let r = &replacements[idx - 1];
                r.original.end + pos - r.transformed.end
            }
        };
        let mut nodes = Vec::new();
        let mut stack: Vec<Node> = Vec::new();
        for (event, range) in Parser::new_ext(&transformed, options()).into_offset_iter() {
            let replacement = replacements
                .binary_search_by_key(&range.start, |r| r.transformed.start)
                .ok()
                .map(|i| &replacements[i]);
            let source_range = map(range.start)..map(range.end);
            let kind = match event {
                Event::Start(tag) => {
                    stack.push(Node {
                        kind: Kind::Element(tag.into_static()),
                        source: source_range,
                        children: Vec::new(),
                    });
                    continue;
                }
                Event::End(_) => {
                    if let Some(mut node) = stack.pop() {
                        if let Ok(idx) = replacements
                            .binary_search_by_key(&node.source.start, |r| r.original.start)
                            && let r = &replacements[idx]
                            && r.block
                            && matches!(node.kind, Kind::Element(Tag::CodeBlock(_)))
                        {
                            node.kind = Kind::Math(source[r.original.clone()].to_string());
                            node.source = r.original.clone();
                            node.children.clear();
                        }
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(node);
                        } else {
                            nodes.push(node);
                        }
                    }
                    continue;
                }
                Event::Text(s) => Kind::Text(if source.get(source_range.clone()) == Some("\0") {
                    "\0".into()
                } else {
                    s.into_string()
                }),
                Event::Code(s) => {
                    if let Some(r) = replacement.filter(|r| !r.block) {
                        Kind::Math(source[r.original.clone()].to_string())
                    } else {
                        Kind::Code(s.into_string())
                    }
                }
                Event::Html(s) | Event::InlineHtml(s) => Kind::Html(s.into_string()),
                Event::SoftBreak => Kind::SoftBreak,
                Event::HardBreak => Kind::HardBreak,
                Event::Rule => Kind::Rule,
                Event::TaskListMarker(b) => Kind::Task(b),
                _ => continue,
            };
            let node = Node {
                kind,
                source: source_range,
                children: Vec::new(),
            };
            if let Some(parent) = stack.last_mut() {
                parent.children.push(node);
            } else {
                nodes.push(node);
            }
        }
        let mut line_starts = vec![0];
        line_starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
        Self {
            source,
            line_starts,
            nodes,
        }
    }
}
