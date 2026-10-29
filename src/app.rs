use crate::{
    document::Document,
    graphics::ViewState,
    layout::{DisplayLine, Mapping, anchored_row, layout, search_segments, truncate_middle},
    math::{Status, View},
    search::{self, Search, Segment},
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
use std::{collections::HashMap, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

pub const HELP: &str = "# fumi 操作一覧\n\n読み取り専用Markdownビューア（v1）\n\nj / ↓ : 1表示行下\nk / ↑ : 1表示行上\nCtrl+d / Ctrl+u : 半画面下 / 上\nCtrl+f / PageDown : 1画面下（1行重複）\nCtrl+b / PageUp : 1画面上（1行重複）\ngg / Home : 先頭\nG / End : 末尾\n? : ヘルプを開閉\nq : ツールを終了（ヘルプ表示中も同じ）\nEsc : 検索語・ハイライトを解除、ヘルプから本文へ戻る\n/ : 検索入力（最大256書記素）\nn / N : 次 / 前の一致（循環）\nホイール : 3表示行上下\n検索中 Enter : 確定、Esc / Ctrl+c : 取消\n検索中 Backspace : 1書記素削除、Ctrl+u : 全消去\nCtrl+c : 割り込み終了（130）\nCtrl+z : 中断、fgで復帰\n\nヘルプでも移動できます。本文位置は変わりません。\nサイズ不足中はq / Ctrl+cで終了できます。\n\n--plain : 能力照会や画像を使わないテキストTUI\n--color auto|always|never : 色の指定\n--graphics auto|off|kitty : 数式画像の方式\n\n対応端末のブロック数式は画像、それ以外はTeXソースを表示します。\nMarkdown画像は代替テキストと参照先を表示します。\nリンクを開く操作はありません。";

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Exit(u8),
    Suspend,
}

/// 本文とヘルプで共通のスクロール操作。
#[derive(Clone, Copy, Debug)]
enum Scroll {
    Down(usize),
    Up(usize),
    Top,
    Bottom,
}

/// 本文と独立した閲覧位置を持つヘルプ。
struct Help {
    document: Document,
    lines: Vec<DisplayLine>,
    top: usize,
    pending_g: bool,
}

impl Help {
    fn relayout(&mut self, width: usize, colors: bool) {
        let anchor = source_anchor(&self.lines, self.top);
        self.lines = layout(&self.document, width, colors);
        self.top = anchored_row(&self.lines, anchor);
    }
    fn max_top(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport)
    }
}

pub struct App {
    pub document: Document,
    pub lines: Vec<DisplayLine>,
    pub top: usize,
    pub search: Search,
    search_index: Option<Vec<Segment>>,
    pub graphics_active: bool,
    pub math_views: HashMap<usize, View>,
    pub math_status: Status,
    deferred_image_anchor: Option<(usize, usize)>,
    pub warning: String,
    pub help: bool,
    pub pending_g: bool,
    help_view: Help,
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
            math_views: HashMap::new(),
            math_status: Default::default(),
            deferred_image_anchor: None,
            warning: String::new(),
            help: false,
            pending_g: false,
            help_view: Help {
                document: Document::parse(HELP.to_string()),
                lines: Vec::new(),
                top: 0,
                pending_g: false,
            },
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
    /// 本文の左端の列。40列以上では左右に1列ずつ余白を置く。
    fn left_margin(&self) -> u16 {
        u16::from(self.width >= 40)
    }
    pub fn view_state(&self) -> ViewState<'_> {
        ViewState {
            lines: &self.lines,
            top: self.top,
            rows: self.viewport(),
            width: self.body_width(),
            left: self.left_margin(),
            body_visible: !self.help && !self.too_small(),
            images: self.graphics_active.then_some(&self.math_views),
        }
    }
    pub fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport())
    }
    pub fn percentage(&self) -> usize {
        percentage(self.top, self.max_top())
    }

    fn top_image(&self) -> Option<(usize, usize)> {
        self.lines.get(self.top).and_then(|l| l.image)
    }
    fn clear_pending_g(&mut self) {
        self.pending_g = false;
        self.help_view.pending_g = false;
    }
    fn active_pending_g_mut(&mut self) -> &mut bool {
        if self.help {
            &mut self.help_view.pending_g
        } else {
            &mut self.pending_g
        }
    }

    /// 表示中の画面（本文またはヘルプ）をスクロールする。
    fn scroll(&mut self, scroll: Scroll) {
        let viewport = self.viewport();
        let (top, max) = if self.help {
            let max = self.help_view.max_top(viewport);
            (&mut self.help_view.top, max)
        } else {
            let max = self.max_top();
            (&mut self.top, max)
        };
        *top = match scroll {
            Scroll::Down(n) => top.saturating_add(n).min(max),
            Scroll::Up(n) => top.saturating_sub(n),
            Scroll::Top => 0,
            Scroll::Bottom => max,
        };
    }

    /// 本文の表示行を作り直し、先頭行のソース位置へ戻す。
    fn relayout(&mut self) {
        let anchor = source_anchor(&self.lines, self.top);
        self.lines = layout(&self.document, self.body_width(), self.colors);
        self.overlay_math();
        self.top = anchored_row(&self.lines, anchor).min(self.max_top());
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        // 幅が変わると画像は作り直しになるため、画像内の行は生成後に復元する。
        if let Some(image) = self.top_image() {
            self.deferred_image_anchor = Some(image);
        }
        self.width = width;
        self.height = height;
        if self.too_small() {
            self.search.input = None;
            self.help = false;
            self.clear_pending_g();
            return;
        }
        let body_width = self.body_width();
        if self.layout_width != body_width {
            self.math_views.clear();
            self.relayout();
            self.help_view.relayout(body_width, self.colors);
            self.layout_width = body_width;
        }
        self.top = self.top.min(self.max_top());
        self.help_view.top = self
            .help_view
            .top
            .min(self.help_view.max_top(self.viewport()));
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
            if !ctrl && key.code == KeyCode::Esc {
                self.search = Search::default();
            }
            return if key.code == KeyCode::Char('q') {
                Action::Exit(0)
            } else {
                Action::None
            };
        }
        if key.modifiers.intersects(
            KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
        ) {
            self.clear_pending_g();
            return Action::None;
        }
        if self.search.input.is_some() {
            self.search_key(ctrl, key.code);
            return Action::None;
        }
        self.view_key(ctrl, key.code)
    }

    /// 検索語の入力中の操作。
    fn search_key(&mut self, ctrl: bool, code: KeyCode) {
        match (ctrl, code) {
            (false, KeyCode::Esc) => self.search.input = None,
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
    }

    /// 本文・ヘルプの閲覧中の操作。
    fn view_key(&mut self, ctrl: bool, code: KeyCode) -> Action {
        let pending = self.active_pending_g_mut();
        if *pending {
            *pending = false;
            if !ctrl && code == KeyCode::Esc {
                return Action::None;
            }
            if !ctrl && code == KeyCode::Char('g') {
                self.scroll(Scroll::Top);
                return Action::None;
            }
        }
        if !ctrl {
            match code {
                KeyCode::Esc if self.help => {
                    self.help = false;
                    self.help_view.pending_g = false;
                    return Action::None;
                }
                KeyCode::Char('q') => return Action::Exit(0),
                KeyCode::Esc => {
                    self.search = Search::default();
                    return Action::None;
                }
                KeyCode::Char('?') => {
                    self.help = !self.help;
                    self.clear_pending_g();
                    self.help_view.top = 0;
                    return Action::None;
                }
                KeyCode::Char('/') if !self.help => {
                    self.search_index
                        .get_or_insert_with(|| search_segments(&self.document));
                    self.search.input = Some(String::new());
                    self.search.candidates.clear();
                    self.search.limited = false;
                    return Action::None;
                }
                KeyCode::Char('n' | 'N') if !self.help => {
                    self.next_match(code == KeyCode::Char('N'));
                    return Action::None;
                }
                KeyCode::Char('g') => {
                    *self.active_pending_g_mut() = true;
                    return Action::None;
                }
                _ => {}
            }
        }
        self.deferred_image_anchor = None;
        let v = self.viewport();
        let half = (v / 2).max(1);
        let page = v.saturating_sub(1).max(1);
        let scroll = match (ctrl, code) {
            (false, KeyCode::Char('j') | KeyCode::Down) => Scroll::Down(1),
            (false, KeyCode::Char('k') | KeyCode::Up) => Scroll::Up(1),
            (true, KeyCode::Char('d')) => Scroll::Down(half),
            (true, KeyCode::Char('u')) => Scroll::Up(half),
            (true, KeyCode::Char('f')) | (false, KeyCode::PageDown) => Scroll::Down(page),
            (true, KeyCode::Char('b')) | (false, KeyCode::PageUp) => Scroll::Up(page),
            (false, KeyCode::Home) => Scroll::Top,
            (false, KeyCode::End | KeyCode::Char('G')) => Scroll::Bottom,
            _ => return Action::None,
        };
        self.scroll(scroll);
        Action::None
    }

    pub fn set_math(&mut self, views: HashMap<usize, View>, status: Status) -> bool {
        let same = self.math_status == status
            && self.math_views.len() == views.len()
            && views
                .iter()
                .all(|(source, view)| match (view, self.math_views.get(source)) {
                    (View::Ready(a), Some(View::Ready(b))) => Arc::ptr_eq(a, b),
                    (View::Failed(a), Some(View::Failed(b))) => a == b,
                    _ => false,
                });
        if same {
            return false;
        }
        let image_anchor = self.top_image().or(self.deferred_image_anchor);
        if image_anchor.is_some() && status == Status::Pending {
            self.deferred_image_anchor = image_anchor;
        }
        self.math_views = views;
        self.math_status = status;
        if self.too_small() {
            self.layout_width = 0;
            return false;
        }
        if !self.document.has_display_math() {
            return false;
        }
        self.relayout();
        if let Some((source, offset)) = image_anchor
            && let Some(View::Ready(image)) = self.math_views.get(&source)
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
        for (node, _) in self.document.display_math().rev() {
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
                mappings: vec![Mapping {
                    text: 0..text.len(),
                    source: node.source.clone(),
                }],
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
                        Some(View::Failed(reason)) => Some(reason.as_str()),
                        _ => self.math_status.label(),
                    };
                    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
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
        let anchor = source_anchor(&self.lines, self.top);
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
        match event.kind {
            MouseEventKind::ScrollDown => self.scroll(Scroll::Down(3)),
            MouseEventKind::ScrollUp => self.scroll(Scroll::Up(3)),
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
                    .find(|m| m.text.contains(&offset))
                    .map(|m| &m.source);
                let mut style = span.style;
                if let Some(destination) =
                    line.destinations.iter().find(|d| d.text.contains(&offset))
                {
                    let target = &destination.target;
                    if let Some(entries) = self.search.destination_matches.get(&destination.id) {
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
            let help = &self.help_view;
            (
                percentage(help.top, help.max_top(self.viewport())),
                help.pending_g,
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
        let body = Rect::new(
            self.left_margin(),
            0,
            self.body_width() as u16,
            self.height - 1,
        );
        let (lines, top) = if self.help {
            (&self.help_view.lines, self.help_view.top)
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

/// 先頭行のソース位置。再レイアウト後の閲覧位置の基準にする。
fn source_anchor(lines: &[DisplayLine], top: usize) -> usize {
    lines.get(top).map_or(0, |l| l.source.start)
}

/// 末尾位置を100%とする百分率。スクロール不要な場合も100%。
fn percentage(top: usize, max_top: usize) -> usize {
    (100 * top).checked_div(max_top).unwrap_or(100)
}

trait WidthCells {
    fn width_cells(&self) -> usize;
}
impl WidthCells for str {
    fn width_cells(&self) -> usize {
        unicode_width::UnicodeWidthStr::width(self)
    }
}
