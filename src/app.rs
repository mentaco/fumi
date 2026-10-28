use crate::{
    document::Document,
    layout::{DisplayLine, anchored_row, layout, truncate_middle},
    source::sanitize,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::search::{self, Search, Segment};
use unicode_segmentation::UnicodeSegmentation;

pub const HELP: &str = "# fumi 操作一覧\n\n読み取り専用Markdownビューア（v1）\n\nj / ↓ : 1表示行下\nk / ↑ : 1表示行上\nCtrl+d / Ctrl+u : 半画面下 / 上\nCtrl+f / PageDown : 1画面下（1行重複）\nCtrl+b / PageUp : 1画面上（1行重複）\ngg / Home : 先頭\nG / End : 末尾\n? : ヘルプを開閉\nq / Esc : 通常閲覧を終了、ヘルプから本文へ戻る\n/ : 検索入力（最大256書記素）\nn / N : 次 / 前の一致（循環）\nホイール : 3表示行上下\n検索中 Enter : 確定、Esc / Ctrl+c : 取消\n検索中 Backspace : 1書記素削除、Ctrl+u : 全消去\nCtrl+c : 割り込み終了（130）\nCtrl+z : 中断、fgで復帰\n\nヘルプでも移動できます。本文位置は変わりません。\nサイズ不足中はq / Ctrl+cで終了できます。\n\n--plain : 能力照会や画像を使わないテキストTUI\n--color auto|always|never : 色の指定\n--graphics auto|off|kitty : 数式画像の方式\n\n対応端末のブロック数式は画像、それ以外はTeXソースを表示します。\nMarkdown画像は代替テキストと参照先を表示します。\nリンクを開く操作はありません。";

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Exit(u8),
    Suspend,
}

pub struct App {
    pub document: Document,
    pub lines: Vec<DisplayLine>,
    pub top: usize,
    pub search: Search,
    search_index: Option<Vec<Segment>>,
    pub graphics_active: bool,
    pub math_views: std::collections::HashMap<usize, crate::math::View>,
    pub math_reason: String,
    deferred_image_anchor: Option<(usize, usize)>,
    pub warning: String,
    pub help: bool,
    pub pending_g: bool,
    help_pending_g: bool,
    help_top: usize,
    help_document: Document,
    help_lines: Vec<DisplayLine>,
    layout_width: usize,
    pub width: u16,
    pub height: u16,
    colors: bool,
    name: String,
}

impl App {
    pub fn new(document: Document, name: &str, colors: bool) -> Self {
        Self {
            document,
            lines: Vec::new(),
            top: 0,
            search: Search::default(),
            search_index: None,
            graphics_active: false,
            math_views: std::collections::HashMap::new(),
            math_reason: String::new(),
            deferred_image_anchor: None,
            warning: String::new(),
            help: false,
            pending_g: false,
            help_pending_g: false,
            help_top: 0,
            help_document: Document::parse(HELP.to_string()),
            help_lines: Vec::new(),
            layout_width: 0,
            width: 0,
            height: 0,
            colors,
            name: sanitize(name),
        }
    }

    pub fn too_small(&self) -> bool {
        self.width < 20 || self.height < 4
    }
    pub fn viewport(&self) -> usize {
        usize::from(self.height.saturating_sub(1))
    }
    pub fn body_width(&self) -> usize {
        usize::from(if self.width >= 40 {
            self.width - 2
        } else {
            self.width
        })
    }
    pub fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport())
    }
    pub fn percentage(&self) -> usize {
        (100 * self.top).checked_div(self.max_top()).unwrap_or(100)
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        let anchor = self.lines.get(self.top).map_or(0, |l| l.source.start);
        let image_anchor = self.lines.get(self.top).and_then(|l| l.image);
        if image_anchor.is_some() {
            self.deferred_image_anchor = image_anchor;
        }
        let help_anchor = self
            .help_lines
            .get(self.help_top)
            .map_or(0, |l| l.source.start);
        self.width = width;
        self.height = height;
        if self.too_small() {
            self.search.input = None;
            self.help = false;
            self.pending_g = false;
            self.help_pending_g = false;
            return;
        }
        let body_width = self.body_width();
        if self.layout_width != body_width {
            self.math_views.clear();
            self.lines = layout(&self.document, body_width, self.colors);
            self.overlay_math();
            self.help_lines = layout(&self.help_document, body_width, self.colors);
            self.layout_width = body_width;
            self.top = anchored_row(&self.lines, anchor);
            if let Some((source, offset)) = image_anchor
                && let Some(row) = self
                    .lines
                    .iter()
                    .position(|l| l.image == Some((source, offset)))
            {
                self.top = row;
            }
            self.help_top = anchored_row(&self.help_lines, help_anchor);
        }
        self.top = self.top.min(self.max_top());
        self.help_top = self
            .help_top
            .min(self.help_lines.len().saturating_sub(self.viewport()));
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.search.input.take().is_some() {
                return Action::None;
            }
            return Action::Exit(130);
        }
        if ctrl && key.code == KeyCode::Char('z') {
            return Action::Suspend;
        }
        if self.too_small() {
            return if key.code == KeyCode::Char('q') {
                Action::Exit(0)
            } else {
                Action::None
            };
        }
        if key.modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        ) {
            self.pending_g = false;
            self.help_pending_g = false;
            return Action::None;
        }
        if self.search.input.is_some() {
            match (ctrl, key.code) {
                (false, KeyCode::Esc) => {
                    self.search.input = None;
                }
                (false, KeyCode::Enter) => self.confirm_search(),
                (true, KeyCode::Char('u')) => {
                    self.search.input = Some(String::new());
                    self.refresh_candidates();
                }
                (false, KeyCode::Backspace) => {
                    if let Some(input) = &mut self.search.input {
                        let end = input
                            .grapheme_indices(true)
                            .next_back()
                            .map_or(0, |(i, _)| i);
                        input.truncate(end);
                    }
                    self.refresh_candidates();
                }
                (false, KeyCode::Char(c)) if !c.is_control() => self.paste(&c.to_string()),
                _ => {}
            }
            return Action::None;
        }
        let pending = if self.help {
            &mut self.help_pending_g
        } else {
            &mut self.pending_g
        };
        if *pending {
            *pending = false;
            if !ctrl && key.code == KeyCode::Esc {
                return Action::None;
            }
            if !ctrl && key.code == KeyCode::Char('g') {
                if self.help {
                    self.help_top = 0;
                } else {
                    self.top = 0;
                }
                return Action::None;
            }
        }
        if !ctrl {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc if self.help => {
                    self.help = false;
                    self.help_pending_g = false;
                    return Action::None;
                }
                KeyCode::Char('q') | KeyCode::Esc => return Action::Exit(0),
                KeyCode::Char('?') => {
                    self.help = !self.help;
                    self.pending_g = false;
                    self.help_pending_g = false;
                    self.help_top = 0;
                    return Action::None;
                }
                KeyCode::Char('/') if !self.help => {
                    self.search_index
                        .get_or_insert_with(|| crate::layout::search_segments(&self.document));
                    self.search.input = Some(String::new());
                    self.search.candidates.clear();
                    self.search.limited = false;
                    return Action::None;
                }
                KeyCode::Char('n' | 'N') if !self.help => {
                    self.next_match(key.code == KeyCode::Char('N'));
                    return Action::None;
                }
                KeyCode::Char('g') => {
                    if self.help {
                        self.help_pending_g = true;
                    } else {
                        self.pending_g = true;
                    }
                    return Action::None;
                }
                _ => {}
            }
        }
        self.deferred_image_anchor = None;
        let v = self.viewport();
        let max = if self.help {
            self.help_lines.len().saturating_sub(v)
        } else {
            self.max_top()
        };
        let top = if self.help {
            &mut self.help_top
        } else {
            &mut self.top
        };
        let movement = match (ctrl, key.code) {
            (false, KeyCode::Char('j') | KeyCode::Down) => Some((true, 1)),
            (false, KeyCode::Char('k') | KeyCode::Up) => Some((false, 1)),
            (true, KeyCode::Char('d')) => Some((true, (v / 2).max(1))),
            (true, KeyCode::Char('u')) => Some((false, (v / 2).max(1))),
            (true, KeyCode::Char('f')) | (false, KeyCode::PageDown) => {
                Some((true, v.saturating_sub(1).max(1)))
            }
            (true, KeyCode::Char('b')) | (false, KeyCode::PageUp) => {
                Some((false, v.saturating_sub(1).max(1)))
            }
            (false, KeyCode::Home) => {
                *top = 0;
                None
            }
            (false, KeyCode::End | KeyCode::Char('G')) => {
                *top = max;
                None
            }
            _ => None,
        };
        if let Some((down, n)) = movement {
            *top = if down {
                top.saturating_add(n).min(max)
            } else {
                top.saturating_sub(n)
            };
        }
        Action::None
    }

    pub fn set_math(
        &mut self,
        views: std::collections::HashMap<usize, crate::math::View>,
        reason: &str,
    ) -> bool {
        let same = self.math_reason == reason
            && self.math_views.len() == views.len()
            && views
                .iter()
                .all(|(source, view)| match (view, self.math_views.get(source)) {
                    (crate::math::View::Ready(a), Some(crate::math::View::Ready(b))) => {
                        std::sync::Arc::ptr_eq(a, b)
                    }
                    (crate::math::View::Failed(a), Some(crate::math::View::Failed(b))) => a == b,
                    _ => false,
                });
        if same {
            return false;
        }
        let anchor = self.lines.get(self.top).map_or(0, |l| l.source.start);
        let image_anchor = self
            .lines
            .get(self.top)
            .and_then(|l| l.image)
            .or(self.deferred_image_anchor);
        if image_anchor.is_some() && reason == "生成中" {
            self.deferred_image_anchor = image_anchor;
        }
        self.math_views = views;
        self.math_reason = reason.into();
        if self.too_small() {
            self.layout_width = 0;
            return false;
        }
        if !self.document.nodes.iter().any(
            |n| matches!(&n.kind, crate::document::Kind::Math(s) if s.trim().starts_with("$$")),
        ) {
            return false;
        }
        self.lines = layout(&self.document, self.body_width(), self.colors);
        self.overlay_math();
        self.top = anchored_row(&self.lines, anchor).min(self.max_top());
        if let Some((source, offset)) = image_anchor
            && let Some(crate::math::View::Ready(image)) = self.math_views.get(&source)
            && let Some(row) = self
                .lines
                .iter()
                .position(|l| l.image == Some((source, offset.min(image.rows.saturating_sub(1)))))
        {
            self.top = row.min(self.max_top());
            self.deferred_image_anchor = None;
        }
        true
    }

    fn overlay_math(&mut self) {
        use crate::{document::Kind, math::View};
        for node in self.document.nodes.iter().rev() {
            if !matches!(&node.kind, Kind::Math(s) if s.trim().starts_with("$$")) {
                continue;
            }
            let Some(start) = self.lines.iter().position(|line| {
                !line.source.is_empty() && node.source.contains(&line.source.start)
            }) else {
                continue;
            };
            let end = self.lines[start..]
                .iter()
                .position(|line| line.source.start >= node.source.end)
                .map_or(self.lines.len(), |i| start + i);
            let label = |text: String| DisplayLine {
                mappings: vec![(0..text.len(), node.source.clone())],
                line: Line::raw(text),
                source: node.source.clone(),
                image: None,
                destinations: Vec::new(),
            };
            match self.math_views.get(&node.source.start) {
                Some(View::Ready(image)) if self.graphics_active => {
                    let mut replacement: Vec<_> = (0..image.rows)
                        .map(|offset| DisplayLine {
                            line: Line::raw(""),
                            source: node.source.clone(),
                            mappings: Vec::new(),
                            image: Some((node.source.start, offset)),
                            destinations: Vec::new(),
                        })
                        .collect();
                    replacement.push(label("[math]".into()));
                    self.lines.splice(start..end, replacement);
                }
                value => {
                    let reason = match value {
                        Some(View::Failed(reason)) => reason,
                        _ => &self.math_reason,
                    };
                    if !reason.is_empty() {
                        self.lines.insert(end, label(format!("[math: {reason}]")));
                    }
                }
            }
        }
    }

    fn refresh_candidates(&mut self) {
        self.search.limited = false;
        self.search.candidates = search::find(
            self.search_index.as_deref().unwrap_or(&[]),
            self.search.input.as_deref().unwrap_or(""),
        );
    }

    pub fn paste(&mut self, text: &str) {
        let Some(input) = &mut self.search.input else {
            return;
        };
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut limited = false;
        for g in clean.graphemes(true) {
            // 結合文字を追加入力した場合も、結合後の書記素数で判定する。
            let length = input.len();
            input.push_str(g);
            if input.graphemes(true).count() > 256 {
                input.truncate(length);
                limited = true;
                break;
            }
        }
        self.refresh_candidates();
        self.search.limited = limited;
    }

    fn confirm_search(&mut self) {
        self.deferred_image_anchor = None;
        let input = self.search.input.take().unwrap_or_default();
        if !input.is_empty() {
            self.search.query = input;
        }
        self.search.matches = search::find(
            self.search_index.as_deref().unwrap_or(&[]),
            &self.search.query,
        );
        self.search.index_destinations();
        let anchor = self.lines.get(self.top).map_or(0, |l| l.source.start);
        self.search.wrapped = false;
        self.search.current = if self.search.matches.is_empty() {
            None
        } else {
            let index = self.search.matches.partition_point(|m| m.start() < anchor);
            self.search.wrapped = index == self.search.matches.len();
            Some(index % self.search.matches.len())
        };
        self.reveal_match();
    }

    fn next_match(&mut self, previous: bool) {
        self.deferred_image_anchor = None;
        let count = self.search.matches.len();
        if count == 0 {
            return;
        }
        let current = self.search.current.unwrap_or(0);
        self.search.wrapped = if previous {
            current == 0
        } else {
            current + 1 == count
        };
        self.search.current = Some(if previous {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        });
        self.reveal_match();
    }

    fn reveal_match(&mut self) {
        if let Some(m) = self.search.current.and_then(|i| self.search.matches.get(i)) {
            let row = anchored_row(&self.lines, m.start());
            if row < self.top || row >= self.top + self.viewport() {
                self.top = row.min(self.max_top());
            }
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) {
        if self.too_small() || self.search.input.is_some() {
            return;
        }
        self.deferred_image_anchor = None;
        let max = if self.help {
            self.help_lines.len().saturating_sub(self.viewport())
        } else {
            self.max_top()
        };
        let top = if self.help {
            &mut self.help_top
        } else {
            &mut self.top
        };
        match event.kind {
            MouseEventKind::ScrollDown => *top = top.saturating_add(3).min(max),
            MouseEventKind::ScrollUp => *top = top.saturating_sub(3),
            _ => {}
        }
    }

    fn highlight_line(&self, line: &DisplayLine) -> Line<'static> {
        if self.search.matches.is_empty() {
            return line.line.clone();
        }
        let mut spans = Vec::new();
        let mut offset = 0;
        for span in &line.line.spans {
            for g in span.content.graphemes(true) {
                let source = line
                    .mappings
                    .iter()
                    .find(|(r, _)| r.contains(&offset))
                    .map(|(_, s)| s);
                let mut style = span.style;
                if let Some((_, id, target)) = line
                    .destinations
                    .iter()
                    .find(|(r, _, _)| r.contains(&offset))
                {
                    if let Some(entries) = self.search.destination_matches.get(id) {
                        let first = entries.partition_point(|(range, _)| range.end <= target.start);
                        for (_, i) in entries[first..]
                            .iter()
                            .take_while(|(range, _)| range.start < target.end)
                        {
                            style = style.add_modifier(if self.search.current == Some(*i) {
                                Modifier::REVERSED
                            } else {
                                Modifier::UNDERLINED
                            });
                        }
                    }
                } else if let Some(source) = source {
                    let end = self
                        .search
                        .matches
                        .partition_point(|m| m.start() < source.end);
                    let start = self
                        .search
                        .matches
                        .partition_point(|m| m.start() < source.start)
                        .saturating_sub(1);
                    for i in start..end {
                        if self.search.matches[i].contains(source) {
                            style = style.add_modifier(if self.search.current == Some(i) {
                                Modifier::REVERSED
                            } else {
                                Modifier::UNDERLINED
                            });
                        }
                    }
                }
                spans.push(Span::styled(g.to_string(), style));
                offset += g.len();
            }
        }
        Line::from(spans)
    }

    pub fn status(&self) -> String {
        let width = usize::from(self.width);
        if let Some(input) = &self.search.input {
            let suffix = format!(
                "  {}件{}",
                self.search.candidates.len(),
                if self.search.limited {
                    " 上限256書記素"
                } else {
                    ""
                }
            );
            return format!(
                "/{}{}",
                truncate_middle(input, width.saturating_sub(1 + suffix.width_cells())),
                suffix
            );
        }
        let (position, pending) = if self.help {
            let max = self.help_lines.len().saturating_sub(self.viewport());
            (
                (100 * self.help_top).checked_div(max).unwrap_or(100),
                self.help_pending_g,
            )
        } else {
            (self.percentage(), self.pending_g)
        };
        let percentage = format!("{position}%");
        let base_extras = if self.help && pending {
            " HELP g待ち"
        } else if self.help {
            " HELP"
        } else if pending {
            " g待ち"
        } else {
            " ? ヘルプ"
        };
        let search_status = if !self.help && !self.search.query.is_empty() {
            if self.search.matches.is_empty() {
                " 見つかりません".into()
            } else {
                format!(
                    " /{} {}/{}{}",
                    self.search.query,
                    self.search.current.map_or(0, |i| i + 1),
                    self.search.matches.len(),
                    if self.search.wrapped { " 循環" } else { "" }
                )
            }
        } else {
            String::new()
        };
        let extras = format!(
            "{search_status}{base_extras}{}",
            if self.warning.is_empty() {
                String::new()
            } else {
                format!(" {}", self.warning)
            }
        );
        let mode = if self.graphics_active {
            "KITTY"
        } else {
            "TEXT"
        };
        for suffix in [
            format!(" {mode} {percentage}{extras}"),
            format!(" {mode} {percentage}{search_status}"),
            format!(" {percentage}{search_status}"),
            format!(" {mode} {percentage}"),
            format!(" {percentage}"),
        ] {
            if suffix.width_cells() <= width {
                return format!(
                    "{}{}",
                    truncate_middle(&self.name, width - suffix.width_cells()),
                    suffix
                );
            }
        }
        truncate_middle(&percentage, width)
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        if self.too_small() {
            frame.render_widget(
                Paragraph::new(truncate_middle(
                    "端末サイズ不足 (20×4以上)",
                    usize::from(area.width),
                )),
                Rect::new(0, 0, area.width, area.height.min(1)),
            );
            return;
        }
        let margin = u16::from(self.width >= 40);
        let body = Rect::new(margin, 0, self.body_width() as u16, self.height - 1);
        let (lines, top) = if self.help {
            (&self.help_lines, self.help_top)
        } else {
            (&self.lines, self.top)
        };
        let text: Vec<_> = lines
            .iter()
            .skip(top)
            .take(self.viewport())
            .map(|l| {
                if self.help {
                    l.line.clone()
                } else {
                    self.highlight_line(l)
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(text), body);
        frame.render_widget(
            Paragraph::new(self.status()).style(Style::default().add_modifier(Modifier::REVERSED)),
            Rect::new(0, self.height - 1, self.width, 1),
        );
    }
}

trait WidthCells {
    fn width_cells(&self) -> usize;
}
impl WidthCells for str {
    fn width_cells(&self) -> usize {
        unicode_width::UnicodeWidthStr::width(self)
    }
}
