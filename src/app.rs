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
use unicode_width::UnicodeWidthStr;

/// 本文とステータスを表示できる最小の端末サイズ。
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 4;
/// 左右に1列ずつ余白を置く最小の端末幅。
const MARGIN_WIDTH: u16 = 40;
/// 検索語の上限（書記素）。
const SEARCH_LIMIT: usize = 256;
/// ホイール1回で動かす表示行数。
const WHEEL_LINES: usize = 3;

/// ヘルプの本文。操作の数値は定数から埋め込み、実装との食い違いを防ぐ。
fn help_text() -> String {
    format!(
        "# fumi 操作一覧\n\n読み取り専用Markdownビューア（v1）\n\nj / ↓ : 1表示行下\nk / ↑ : 1表示行上\nCtrl+d / Ctrl+u : 半画面下 / 上\nCtrl+f / PageDown : 1画面下（1行重複）\nCtrl+b / PageUp : 1画面上（1行重複）\ngg / Home : 先頭\nG / End : 末尾\n? : ヘルプを開閉\nq : ツールを終了（ヘルプ表示中も同じ）\nEsc : 検索語・ハイライトを解除、ヘルプから本文へ戻る\n/ : 検索入力（最大{SEARCH_LIMIT}書記素）\nn / N : 次 / 前の一致（循環）\nホイール : {WHEEL_LINES}表示行上下\n検索中 Enter : 確定、Esc / Ctrl+c : 取消\n検索中 Backspace : 1書記素削除、Ctrl+u : 全消去\nCtrl+c : 割り込み終了（130）\nCtrl+z : 中断、fgで復帰\n\nヘルプでも移動できます。本文位置は変わりません。\nサイズ不足中はq / Ctrl+cで終了できます。\n\nfumi - : パイプ・リダイレクトの標準入力を表示\n--plain : 端末の画像対応の確認や画像を使わないテキストTUI\n--color auto|always|never : 色の指定\n--graphics auto|off|kitty : 数式画像の方式\n\n対応端末のブロック数式は画像、それ以外はTeXソースを表示します。\nMarkdown画像は代替テキストと参照先を表示します。\nリンクを開く操作はありません。"
    )
}

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

/// 文書・表示行・閲覧位置をまとめた画面。本文とヘルプは同じ操作で独立して動く。
struct Pane {
    document: Document,
    lines: Vec<DisplayLine>,
    top: usize,
    pending_g: bool,
}

impl Pane {
    fn new(document: Document) -> Self {
        Self {
            document,
            lines: Vec::new(),
            top: 0,
            pending_g: false,
        }
    }
    fn max_top(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport)
    }
    fn percentage(&self, viewport: usize) -> usize {
        percentage(self.top, self.max_top(viewport))
    }
    /// 先頭行のソース位置。再レイアウト後の閲覧位置の基準にする。
    fn anchor(&self) -> usize {
        self.lines.get(self.top).map_or(0, |l| l.source.start)
    }
    /// 表示行を作り直したものへ替え、先頭行のソース位置へ戻す。
    fn relayout(&mut self, lines: Vec<DisplayLine>, viewport: usize) {
        let anchor = self.anchor();
        self.relayout_at(lines, anchor, viewport);
    }
    /// 表示行を替え、ソース位置 `anchor` を含む行（なければ後ろで最も近い行）へ移る。
    fn relayout_at(&mut self, lines: Vec<DisplayLine>, anchor: usize, viewport: usize) {
        self.lines = lines;
        self.top = anchored_row(&self.lines, anchor).min(self.max_top(viewport));
    }
    fn clamp(&mut self, viewport: usize) {
        self.top = self.top.min(self.max_top(viewport));
    }
    fn scroll(&mut self, scroll: Scroll, viewport: usize) {
        let max = self.max_top(viewport);
        self.top = match scroll {
            Scroll::Down(n) => self.top.saturating_add(n).min(max),
            Scroll::Up(n) => self.top.saturating_sub(n),
            Scroll::Top => 0,
            Scroll::Bottom => max,
        };
    }
}

/// 本文の文書の内容から作った状態。文書を差し替えたら既定値へ戻す（仕様3.1節）。
#[derive(Default)]
struct DocumentState {
    /// 検索用の論理文字列とソース範囲。最初の検索で作る。
    search_index: Option<Vec<Segment>>,
    /// 数式ごとの生成結果。キーは数式のソース開始位置。
    math_views: HashMap<usize, View>,
    /// 画像の生成後に戻す閲覧位置。数式のソース開始位置と画像内の行。
    deferred_image_anchor: Option<(usize, usize)>,
}

pub struct App {
    body: Pane,
    doc: DocumentState,
    /// 本文の文書の世代。差し替えるたびに増やし、古い文書から作った結果を区別する。
    generation: u64,
    /// 検索語は文書を差し替えても残し、一致は新しい文書で求め直す（仕様3.1節）。
    search: Search,
    math_status: Status,
    graphics_active: bool,
    warning: String,
    help_visible: bool,
    help: Pane,
    layout_width: usize,
    width: u16,
    height: u16,
    colors: bool,
    name: String,
}

impl App {
    pub fn new(document: Document, name: &str, colors: bool) -> Self {
        Self {
            body: Pane::new(document),
            doc: DocumentState::default(),
            generation: 0,
            search: Search::default(),
            math_status: Default::default(),
            graphics_active: false,
            warning: String::new(),
            help_visible: false,
            help: Pane::new(Document::parse(help_text())),
            layout_width: 0,
            width: 0,
            height: 0,
            colors,
            name: sanitize(name),
        }
    }

    /// 本文の文書。
    pub fn document(&self) -> &Document {
        &self.body.document
    }
    /// 本文の文書の世代。
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// 本文の表示行。
    pub fn lines(&self) -> &[DisplayLine] {
        &self.body.lines
    }
    /// 本文の先頭に表示する行。ヘルプ表示中も本文の位置を返す。
    pub fn top(&self) -> usize {
        self.body.top
    }
    pub fn search(&self) -> &Search {
        &self.search
    }
    pub fn help_visible(&self) -> bool {
        self.help_visible
    }
    /// 表示中の画面（本文またはヘルプ）が `gg` の2文字目を待っているか。
    pub fn pending_g(&self) -> bool {
        self.active().pending_g
    }
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
    pub fn too_small(&self) -> bool {
        self.width < MIN_WIDTH || self.height < MIN_HEIGHT
    }
    pub fn viewport(&self) -> usize {
        usize::from(self.height.saturating_sub(1))
    }
    pub fn body_width(&self) -> usize {
        usize::from(if self.width >= MARGIN_WIDTH {
            self.width - 2
        } else {
            self.width
        })
    }
    /// 本文の左端の列。40列以上では左右に1列ずつ余白を置く。
    fn left_margin(&self) -> u16 {
        u16::from(self.width >= MARGIN_WIDTH)
    }
    pub fn view_state(&self) -> ViewState<'_> {
        ViewState {
            generation: self.generation,
            lines: &self.body.lines,
            top: self.body.top,
            rows: self.viewport(),
            width: self.body_width(),
            left: self.left_margin(),
            body_visible: !self.help_visible && !self.too_small(),
            images: self.graphics_active.then_some(&self.doc.math_views),
        }
    }
    /// 本文の先頭行の最大値。
    pub fn max_top(&self) -> usize {
        self.body.max_top(self.viewport())
    }
    /// 本文の閲覧位置の百分率。
    pub fn percentage(&self) -> usize {
        self.body.percentage(self.viewport())
    }
    /// 本文を指定の表示行へ移す。末尾を超える位置は末尾へ丸める。
    pub fn scroll_to(&mut self, row: usize) {
        self.doc.deferred_image_anchor = None;
        self.body.top = row.min(self.max_top());
    }

    fn active(&self) -> &Pane {
        if self.help_visible {
            &self.help
        } else {
            &self.body
        }
    }
    fn active_mut(&mut self) -> &mut Pane {
        if self.help_visible {
            &mut self.help
        } else {
            &mut self.body
        }
    }
    fn top_image(&self) -> Option<(usize, usize)> {
        self.body.lines.get(self.body.top).and_then(|l| l.image)
    }
    fn clear_pending_g(&mut self) {
        self.body.pending_g = false;
        self.help.pending_g = false;
    }

    /// 表示中の画面（本文またはヘルプ）をスクロールする。
    fn scroll(&mut self, scroll: Scroll) {
        let viewport = self.viewport();
        self.active_mut().scroll(scroll, viewport);
    }

    /// 本文の表示行を作り直し、先頭行のソース位置へ戻す。
    fn relayout(&mut self) {
        let lines = layout(&self.body.document, self.body_width(), self.colors);
        let lines = self.overlay_math(lines);
        let viewport = self.viewport();
        self.body.relayout(lines, viewport);
    }

    /// 本文の文書を差し替える（仕様3.1節）。文書の内容から作った状態を捨てて世代を進め、
    /// 画面上端の元ソース行番号に最も近い新しい行へ移る。範囲外なら末尾へ丸める。
    /// 検索語・入力中の検索語・ヘルプの状態は残し、一致は新しい文書で求め直す。
    pub fn replace_document(&mut self, document: Document) {
        let line = line_number(&self.body.document.source, self.body.anchor());
        let anchor = line_start(&document.source, line);
        self.body.document = document;
        self.body.pending_g = false;
        self.doc = DocumentState::default();
        self.generation += 1;
        if !self.search.query.is_empty() || self.search.input.is_some() {
            self.doc.search_index = Some(search_segments(&self.body.document));
        }
        if self.width > 0 {
            let body_width = self.body_width();
            let lines = layout(&self.body.document, body_width, self.colors);
            let lines = self.overlay_math(lines);
            let viewport = self.viewport();
            self.body.relayout_at(lines, anchor, viewport);
            self.layout_width = body_width;
        }
        if !self.search.query.is_empty() {
            // 閲覧位置は保ち、一致へは移動しない。現在の一致は確定時と同じく画面上端以降の最初。
            self.find_matches();
        } else {
            self.search.matches.clear();
            self.search.current = None;
            self.search.index_destinations();
        }
        if self.search.input.is_some() {
            self.refresh_candidates();
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        // 幅が変わると画像は作り直しになるため、画像内の行は生成後に復元する。
        if let Some(image) = self.top_image() {
            self.doc.deferred_image_anchor = Some(image);
        }
        self.width = width;
        self.height = height;
        if self.too_small() {
            self.search.input = None;
            self.help_visible = false;
            self.clear_pending_g();
            return;
        }
        let body_width = self.body_width();
        let viewport = self.viewport();
        if self.layout_width != body_width {
            self.doc.math_views.clear();
            self.relayout();
            let lines = layout(&self.help.document, body_width, self.colors);
            self.help.relayout(lines, viewport);
            self.layout_width = body_width;
        }
        self.body.clamp(viewport);
        self.help.clamp(viewport);
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
        let pending = &mut self.active_mut().pending_g;
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
                KeyCode::Esc if self.help_visible => {
                    self.help_visible = false;
                    self.help.pending_g = false;
                    return Action::None;
                }
                KeyCode::Char('q') => return Action::Exit(0),
                KeyCode::Esc => {
                    self.search = Search::default();
                    return Action::None;
                }
                KeyCode::Char('?') => {
                    self.help_visible = !self.help_visible;
                    self.clear_pending_g();
                    self.help.top = 0;
                    return Action::None;
                }
                KeyCode::Char('/') if !self.help_visible => {
                    self.doc
                        .search_index
                        .get_or_insert_with(|| search_segments(&self.body.document));
                    self.search.input = Some(String::new());
                    self.search.candidates.clear();
                    self.search.limited = false;
                    return Action::None;
                }
                KeyCode::Char('n' | 'N') if !self.help_visible => {
                    self.next_match(code == KeyCode::Char('N'));
                    return Action::None;
                }
                KeyCode::Char('g') => {
                    self.active_mut().pending_g = true;
                    return Action::None;
                }
                _ => {}
            }
        }
        self.doc.deferred_image_anchor = None;
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
            && self.doc.math_views.len() == views.len()
            && views.iter().all(
                |(source, view)| match (view, self.doc.math_views.get(source)) {
                    (View::Ready(a), Some(View::Ready(b))) => Arc::ptr_eq(a, b),
                    (View::Failed(a), Some(View::Failed(b))) => a == b,
                    _ => false,
                },
            );
        if same {
            return false;
        }
        let image_anchor = self.top_image().or(self.doc.deferred_image_anchor);
        if image_anchor.is_some() && status == Status::Pending {
            self.doc.deferred_image_anchor = image_anchor;
        }
        self.doc.math_views = views;
        self.math_status = status;
        if self.too_small() {
            self.layout_width = 0;
            return false;
        }
        if !self.body.document.has_display_math() {
            return false;
        }
        self.relayout();
        if let Some((source, offset)) = image_anchor
            && let Some(View::Ready(image)) = self.doc.math_views.get(&source)
            && let Some(row) =
                self.body.lines.iter().position(|l| {
                    l.image == Some((source, offset.min(image.rows.saturating_sub(1))))
                })
        {
            self.body.top = row.min(self.max_top());
            self.doc.deferred_image_anchor = None;
        }
        true
    }

    /// 画像表示を始める。数式は生成を待つ状態から表示し直す。
    pub fn enable_graphics(&mut self) {
        self.graphics_active = true;
        self.warning.clear();
        self.set_math(HashMap::new(), Status::Pending);
    }

    /// 画像表示をやめ、ブロック数式をTeX表示へ戻す。`reason` は数式に付ける理由で、
    /// `warn` が真ならステータスにも警告として出す。
    pub fn disable_graphics(&mut self, reason: String, warn: bool) {
        self.graphics_active = false;
        self.warning = if warn { reason.clone() } else { String::new() };
        self.set_math(HashMap::new(), Status::Unavailable(reason));
    }

    /// ブロック数式の表示行を、画像の行またはTeXと理由のラベルにする。
    fn overlay_math(&self, lines: Vec<DisplayLine>) -> Vec<DisplayLine> {
        // 数式も表示行もソース順に並ぶ。前の数式の後ろから探せば、全体を一度の走査で済む。
        let mut edits = Vec::new();
        let mut cursor = 0;
        for (node, _) in self.body.document.display_math() {
            let Some(start) = lines[cursor..]
                .iter()
                .position(|line| {
                    !line.source.is_empty() && node.source.contains(&line.source.start)
                })
                .map(|i| cursor + i)
            else {
                continue;
            };
            let end = lines[start..]
                .iter()
                .position(|line| line.source.start >= node.source.end)
                .map_or(lines.len(), |i| start + i);
            cursor = end;
            let label = |text: String| {
                let mappings = vec![Mapping {
                    text: 0..text.len(),
                    source: node.source.clone(),
                }];
                DisplayLine::new(Line::raw(text), node.source.clone(), mappings)
            };
            match self.doc.math_views.get(&node.source.start) {
                Some(View::Ready(image)) if self.graphics_active => {
                    let mut replacement: Vec<_> = (0..image.rows)
                        .map(|offset| DisplayLine {
                            image: Some((node.source.start, offset)),
                            ..DisplayLine::new(Line::raw(""), node.source.clone(), Vec::new())
                        })
                        .collect();
                    replacement.push(label("[math]".into()));
                    edits.push((start..end, replacement));
                }
                value => {
                    let reason = match value {
                        Some(View::Failed(reason)) => Some(reason.as_str()),
                        _ => self.math_status.label(),
                    };
                    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
                        edits.push((end..end, vec![label(format!("[math: {reason}]"))]));
                    }
                }
            }
        }
        if edits.is_empty() {
            return lines;
        }
        let mut result = Vec::with_capacity(lines.len() + edits.len());
        let mut lines = lines.into_iter();
        let mut position = 0;
        for (range, replacement) in edits {
            result.extend(lines.by_ref().take(range.start - position));
            lines.by_ref().take(range.len()).for_each(drop);
            result.extend(replacement);
            position = range.end;
        }
        result.extend(lines);
        result
    }

    fn refresh_candidates(&mut self) {
        self.search.limited = false;
        self.search.candidates = search::find(
            self.doc.search_index.as_deref().unwrap_or(&[]),
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
            if input.graphemes(true).count() > SEARCH_LIMIT {
                input.truncate(length);
                limited = true;
                break;
            }
        }
        self.refresh_candidates();
        self.search.limited = limited;
    }

    fn confirm_search(&mut self) {
        self.doc.deferred_image_anchor = None;
        let input = self.search.input.take().unwrap_or_default();
        if !input.is_empty() {
            self.search.query = input;
        }
        self.find_matches();
        self.reveal_match();
    }

    /// 検索語の一致を求め、画面上端以降の最初の一致を現在の一致にする。
    fn find_matches(&mut self) {
        self.search.matches = search::find(
            self.doc.search_index.as_deref().unwrap_or(&[]),
            &self.search.query,
        );
        self.search.index_destinations();
        let anchor = self.body.anchor();
        self.search.wrapped = false;
        self.search.current = if self.search.matches.is_empty() {
            None
        } else {
            let index = self.search.matches.partition_point(|m| m.start() < anchor);
            self.search.wrapped = index == self.search.matches.len();
            Some(index % self.search.matches.len())
        };
    }

    fn next_match(&mut self, previous: bool) {
        self.doc.deferred_image_anchor = None;
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
            let row = anchored_row(&self.body.lines, m.start());
            if row < self.body.top || row >= self.body.top + self.viewport() {
                self.body.top = row.min(self.max_top());
            }
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) {
        if self.too_small() || self.search.input.is_some() {
            return;
        }
        self.doc.deferred_image_anchor = None;
        match event.kind {
            MouseEventKind::ScrollDown => self.scroll(Scroll::Down(WHEEL_LINES)),
            MouseEventKind::ScrollUp => self.scroll(Scroll::Up(WHEEL_LINES)),
            _ => {}
        }
    }

    fn highlight_line(&self, line: &DisplayLine) -> Line<'static> {
        if self.search.matches.is_empty() {
            return line.line.clone();
        }
        // 対応表も書記素も表示行の先頭から順に並ぶため、位置を進めながら照合する。
        let mut mappings = line.mappings.iter().peekable();
        let mut destinations = line.destinations.iter().peekable();
        let mut spans = Vec::new();
        let mut offset = 0;
        for span in &line.line.spans {
            for g in span.content.graphemes(true) {
                while mappings.next_if(|m| m.text.end <= offset).is_some() {}
                while destinations.next_if(|d| d.text.end <= offset).is_some() {}
                let source = mappings
                    .peek()
                    .filter(|m| m.text.contains(&offset))
                    .map(|m| &m.source);
                let destination = destinations
                    .peek()
                    .filter(|d| d.text.contains(&offset))
                    .copied();
                let hit = self.search.hit(source, destination);
                let mut style = span.style;
                if hit.current {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if hit.other {
                    style = style.add_modifier(Modifier::UNDERLINED);
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
            let limited = if self.search.limited {
                format!(" 上限{SEARCH_LIMIT}書記素")
            } else {
                String::new()
            };
            let suffix = format!("  {}件{limited}", self.search.candidates.len());
            return format!(
                "/{}{}",
                truncate_middle(input, width.saturating_sub(1 + suffix.width())),
                suffix
            );
        }
        let pane = self.active();
        let percentage = format!("{}%", pane.percentage(self.viewport()));
        let pending = pane.pending_g;
        let base_extras = if self.help_visible && pending {
            " HELP g待ち"
        } else if self.help_visible {
            " HELP"
        } else if pending {
            " g待ち"
        } else {
            " ? ヘルプ"
        };
        let search_status = if !self.help_visible && !self.search.query.is_empty() {
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
            if suffix.width() <= width {
                return format!(
                    "{}{}",
                    truncate_middle(&self.name, width - suffix.width()),
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
                    &format!("端末サイズ不足 ({MIN_WIDTH}×{MIN_HEIGHT}以上)"),
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
        let pane = self.active();
        let text: Vec<_> = pane
            .lines
            .iter()
            .skip(pane.top)
            .take(self.viewport())
            .map(|l| {
                if self.help_visible {
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

/// ソース位置 `position` を含む行の番号（0始まり）。
fn line_number(source: &str, position: usize) -> usize {
    source.as_bytes()[..position.min(source.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// `line` 行目（0始まり）の先頭のソース位置。行がなければ文書の末尾。
fn line_start(source: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    source
        .match_indices('\n')
        .nth(line - 1)
        .map_or(source.len(), |(i, _)| i + 1)
}

/// 末尾位置を100%とする百分率。スクロール不要な場合も100%。
fn percentage(top: usize, max_top: usize) -> usize {
    (100 * top).checked_div(max_top).unwrap_or(100)
}
