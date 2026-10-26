//! 端末応答を入力から分離する。遅延OSC/APC応答をキーとして配送しない。
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum InputEvent {
    User(Event),
    Reply(String),
}

#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    escape_at: Option<Instant>,
    paste: Option<Vec<u8>>,
    discarding: bool,
    pub events: VecDeque<InputEvent>,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        self.decode(false);
    }
    pub fn expire(&mut self) {
        let expired = self
            .escape_at
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(30));
        self.decode(expired);
    }
    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.events
            .push_back(InputEvent::User(Event::Key(KeyEvent::new(code, modifiers))));
    }
    fn decode(&mut self, expired: bool) {
        loop {
            if self.bytes.is_empty() {
                self.escape_at = None;
                break;
            }
            if let Some(paste) = &mut self.paste {
                if let Some(end) = self.bytes.windows(6).position(|w| w == b"\x1b[201~") {
                    paste.extend(
                        self.bytes[..end]
                            .iter()
                            .take(65536usize.saturating_sub(paste.len())),
                    );
                    self.bytes.drain(..end + 6);
                    let text = String::from_utf8_lossy(&self.paste.take().unwrap()).into_owned();
                    self.events.push_back(InputEvent::User(Event::Paste(text)));
                    continue;
                }
                let count = self.bytes.len().saturating_sub(5);
                paste.extend(
                    self.bytes[..count]
                        .iter()
                        .take(65536usize.saturating_sub(paste.len())),
                );
                self.bytes.drain(..count);
                break;
            }
            if self.discarding {
                if let Some(end) = self
                    .bytes
                    .iter()
                    .position(|&b| b == 7)
                    .map(|i| i + 1)
                    .or_else(|| {
                        self.bytes
                            .windows(2)
                            .position(|w| w == b"\x1b\\")
                            .map(|i| i + 2)
                    })
                {
                    self.bytes.drain(..end);
                    self.discarding = false;
                    continue;
                }
                if self.bytes.len() > 1 {
                    self.bytes.drain(..self.bytes.len() - 1);
                }
                break;
            }
            if self.bytes[0] != 27 {
                let b = self.bytes[0];
                let (code, modifiers, count) = match b {
                    b'\r' | b'\n' => (KeyCode::Enter, KeyModifiers::NONE, 1),
                    8 | 127 => (KeyCode::Backspace, KeyModifiers::NONE, 1),
                    b'\t' => (KeyCode::Tab, KeyModifiers::NONE, 1),
                    1..=26 => (
                        KeyCode::Char((b'a' + b - 1) as char),
                        KeyModifiers::CONTROL,
                        1,
                    ),
                    0..=31 => {
                        self.bytes.remove(0);
                        continue;
                    }
                    _ => {
                        let count = if b < 128 {
                            1
                        } else if b < 224 {
                            2
                        } else if b < 240 {
                            3
                        } else {
                            4
                        };
                        if self.bytes.len() < count {
                            break;
                        }
                        match std::str::from_utf8(&self.bytes[..count])
                            .ok()
                            .and_then(|s| s.chars().next())
                        {
                            Some(c) => (KeyCode::Char(c), KeyModifiers::NONE, count),
                            None => {
                                self.bytes.remove(0);
                                continue;
                            }
                        }
                    }
                };
                self.bytes.drain(..count);
                self.key(code, modifiers);
                continue;
            }
            self.escape_at.get_or_insert_with(Instant::now);
            if self.bytes.len() == 1 {
                if expired {
                    self.bytes.clear();
                    self.key(KeyCode::Esc, KeyModifiers::NONE);
                }
                break;
            }
            match self.bytes[1] {
                b'_' | b']' | b'P' => {
                    let end = self
                        .bytes
                        .windows(2)
                        .position(|w| w == b"\x1b\\")
                        .map(|i| i + 2)
                        .or_else(|| {
                            (self.bytes[1] == b']')
                                .then(|| self.bytes.iter().position(|&b| b == 7).map(|i| i + 1))
                                .flatten()
                        });
                    if let Some(end) = end {
                        self.events.push_back(InputEvent::Reply(
                            String::from_utf8_lossy(&self.bytes[..end]).into(),
                        ));
                        self.bytes.drain(..end);
                        continue;
                    }
                    if self.bytes.len() > 4096 {
                        self.bytes.clear();
                        self.discarding = true;
                    }
                    break;
                }
                b'[' => {
                    let Some(end) = self.bytes[2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map(|i| i + 2)
                    else {
                        if self.bytes.len() > 4096 {
                            self.bytes.clear();
                        }
                        break;
                    };
                    let sequence = String::from_utf8_lossy(&self.bytes[2..=end]).into_owned();
                    self.bytes.drain(..=end);
                    if sequence == "200~" {
                        self.paste = Some(Vec::new());
                        continue;
                    }
                    if sequence.ends_with('t') || sequence.ends_with('c') {
                        self.events
                            .push_back(InputEvent::Reply(format!("\x1b[{sequence}")));
                        continue;
                    }
                    if sequence.starts_with('<') {
                        let parts: Vec<u16> = sequence[1..sequence.len() - 1]
                            .split(';')
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        if parts.len() == 3
                            && sequence.ends_with('M')
                            && matches!(parts[0] & 0x7f, 64 | 65)
                        {
                            self.events
                                .push_back(InputEvent::User(Event::Mouse(MouseEvent {
                                    kind: if parts[0] & 1 == 0 {
                                        MouseEventKind::ScrollUp
                                    } else {
                                        MouseEventKind::ScrollDown
                                    },
                                    column: parts[1].saturating_sub(1),
                                    row: parts[2].saturating_sub(1),
                                    modifiers: KeyModifiers::NONE,
                                })));
                        }
                        continue;
                    }
                    let code = match sequence.as_str() {
                        "A" => KeyCode::Up,
                        "B" => KeyCode::Down,
                        "C" => KeyCode::Right,
                        "D" => KeyCode::Left,
                        "H" | "1~" | "7~" => KeyCode::Home,
                        "F" | "4~" | "8~" => KeyCode::End,
                        "5~" => KeyCode::PageUp,
                        "6~" => KeyCode::PageDown,
                        "3~" => KeyCode::Delete,
                        _ => continue,
                    };
                    self.key(code, KeyModifiers::NONE);
                }
                b'O' => {
                    if self.bytes.len() < 3 {
                        break;
                    }
                    let code = match self.bytes[2] {
                        b'A' => Some(KeyCode::Up),
                        b'B' => Some(KeyCode::Down),
                        b'H' => Some(KeyCode::Home),
                        b'F' => Some(KeyCode::End),
                        _ => None,
                    };
                    self.bytes.drain(..3);
                    if let Some(code) = code {
                        self.key(code, KeyModifiers::NONE);
                    }
                }
                _ => {
                    self.bytes.remove(0);
                    // Alt操作は通常操作へ誤配送しない。
                    if self.bytes[0].is_ascii() {
                        let c = self.bytes.remove(0) as char;
                        self.key(KeyCode::Char(c), KeyModifiers::ALT);
                    }
                }
            }
        }
    }
}

pub fn read(decoder: &mut Decoder, timeout: Duration) -> io::Result<()> {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // stdinは起動時に検証済みのTTY。一つの所有者だけが読み込む。
    let ready = unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as i32) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    } else if ready > 0 {
        let mut bytes = [0u8; 8192];
        let count = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count > 0 {
            decoder.feed(&bytes[..count as usize]);
        } else if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "端末入力が切断されました",
            ));
        }
    }
    decoder.expire();
    Ok(())
}
