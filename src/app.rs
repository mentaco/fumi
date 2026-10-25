use crate::{
    document::Document,
    layout::{DisplayLine, anchored_row, layout, truncate_middle},
    source::sanitize,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::Paragraph,
};

pub const HELP: &str = "# mdview 操作一覧\n\n読み取り専用Markdownビューア（MVP）\n\nj / ↓ : 1表示行下\nk / ↑ : 1表示行上\nCtrl+d / Ctrl+u : 半画面下 / 上\nCtrl+f / PageDown : 1画面下（1行重複）\nCtrl+b / PageUp : 1画面上（1行重複）\ngg / Home : 先頭\nG / End : 末尾\n? : ヘルプを開閉\nq / Esc : 通常閲覧を終了、ヘルプから本文へ戻る\nCtrl+c : 割り込み終了（130）\nCtrl+z : 中断、fgで復帰\n\nヘルプでも移動できます。本文位置は変わりません。\nサイズ不足中はq / Ctrl+cで終了できます。\n\n--plain : 能力照会や画像を使わないテキストTUI\n--color auto|always|never : 色の指定\n\n数式はTeXソース、画像は代替テキストと参照先を表示します。\nリンクを開く操作はありません。";

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
    pub help: bool,
    pub pending_g: bool,
    help_pending_g: bool,
    help_top: usize,
    help_document: Document,
    help_lines: Vec<DisplayLine>,
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
            help: false,
            pending_g: false,
            help_pending_g: false,
            help_top: 0,
            help_document: Document::parse(HELP.to_string()),
            help_lines: Vec::new(),
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
        if self.max_top() == 0 {
            100
        } else {
            100 * self.top / self.max_top()
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if self.width == width && self.height == height {
            return;
        }
        let anchor = self.lines.get(self.top).map_or(0, |l| l.source.start);
        let help_anchor = self
            .help_lines
            .get(self.help_top)
            .map_or(0, |l| l.source.start);
        self.width = width;
        self.height = height;
        if self.too_small() {
            self.help = false;
            self.pending_g = false;
            self.help_pending_g = false;
            return;
        }
        self.lines = layout(&self.document, self.body_width(), self.colors);
        self.help_lines = layout(&self.help_document, self.body_width(), self.colors);
        self.top = anchored_row(&self.lines, anchor).min(self.max_top());
        self.help_top = anchored_row(&self.help_lines, help_anchor)
            .min(self.help_lines.len().saturating_sub(self.viewport()));
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
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

    pub fn status(&self) -> String {
        let width = usize::from(self.width);
        let (position, pending) = if self.help {
            let max = self.help_lines.len().saturating_sub(self.viewport());
            (
                if max == 0 {
                    100
                } else {
                    100 * self.help_top / max
                },
                self.help_pending_g,
            )
        } else {
            (self.percentage(), self.pending_g)
        };
        let percentage = format!("{position}%");
        let extras = if self.help && pending {
            " HELP g待ち"
        } else if self.help {
            " HELP"
        } else if pending {
            " g待ち"
        } else {
            " ? ヘルプ"
        };
        for suffix in [
            format!(" TEXT {percentage}{extras}"),
            format!(" TEXT {percentage}"),
            format!(" {percentage}"),
        ] {
            if suffix.width_cells() + 1 < width {
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
            .map(|l| l.line.clone())
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
