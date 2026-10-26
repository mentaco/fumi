use crate::{
    cli::GraphicsMode,
    input::{self, Decoder, InputEvent},
    math::Image,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

const QUERY_ID: u32 = 4_000_000_001;
static OWNED: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1_000_000);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub cell: (u16, u16),
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub monochrome: bool,
}

pub fn query(
    mode: GraphicsMode,
    plain: bool,
    colors: bool,
    decoder: &mut Decoder,
) -> io::Result<Result<Capabilities, String>> {
    if plain || mode == GraphicsMode::Off {
        return Ok(Err("画像無効".into()));
    }
    if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
        return Ok(Err("tmux/screenでは画像無効".into()));
    }
    let mut output = io::stdout();
    let start = Instant::now();
    write!(
        output,
        "\x1b_Gi={QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[14t\x1b]11;?\x1b\\\x1b]10;?\x1b\\"
    )?;
    output.flush()?;
    let mut supported = false;
    let mut background = None;
    let mut foreground = None;
    let mut cell = None;
    let mut pixels = None;
    let mut users = std::collections::VecDeque::new();
    while start.elapsed() < Duration::from_millis(150) {
        input::read(
            decoder,
            (Duration::from_millis(150) - start.elapsed().min(Duration::from_millis(150)))
                .min(Duration::from_millis(8)),
        )?;
        while let Some(event) = decoder.events.pop_front() {
            match event {
                InputEvent::User(_) => users.push_back(event),
                InputEvent::Reply(reply) => {
                    if kitty_reply(&reply).is_some_and(|(id, ok)| id == QUERY_ID && ok) {
                        supported = true;
                    }
                    if let Some(value) = parse_color(&reply, 11) {
                        background = Some(value);
                    }
                    if let Some(value) = parse_color(&reply, 10) {
                        foreground = Some(value);
                    }
                    if let Some(values) = reply
                        .strip_prefix("\x1b[")
                        .and_then(|s| s.strip_suffix('t'))
                    {
                        let values = values
                            .split(';')
                            .map(str::parse::<u16>)
                            .collect::<Result<Vec<_>, _>>();
                        if let Ok(values) = values
                            && values.len() == 3
                            && values[1] > 0
                            && values[2] > 0
                        {
                            if values[0] == 6 {
                                cell = Some((values[2], values[1]));
                            }
                            if values[0] == 4 {
                                pixels = Some((values[2], values[1]));
                            }
                        }
                    }
                }
            }
        }
        if supported && cell.is_some() && background.is_some() && foreground.is_some() {
            break;
        }
    }
    decoder.events.extend(users);
    if mode == GraphicsMode::Auto && !supported {
        return Ok(Err("画像非対応・照会無応答".into()));
    }
    if cell.is_none()
        && let Ok(size) = crossterm::terminal::window_size()
    {
        if size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0 {
            cell = Some((size.width / size.columns, size.height / size.rows));
        } else if let Some((w, h)) = pixels
            && size.columns > 0
            && size.rows > 0
        {
            cell = Some((w / size.columns, h / size.rows));
        }
    }
    let Some(cell) = cell.filter(|(w, h)| *w > 0 && *h > 0 && *w <= 512 && *h <= 512) else {
        return Ok(Err("セル寸法不明".into()));
    };
    let Some(mut background) = background else {
        return Ok(Err("背景色不明".into()));
    };
    let mut foreground = foreground.unwrap_or_else(|| {
        if luminance(background) > 128 {
            [0; 3]
        } else {
            [255; 3]
        }
    });
    if !colors {
        background = [luminance(background); 3];
        foreground = if luminance(background) > 128 {
            [0; 3]
        } else {
            [255; 3]
        };
    }
    Ok(Ok(Capabilities {
        cell,
        background,
        foreground,
        monochrome: !colors,
    }))
}
fn luminance(rgb: [u8; 3]) -> u8 {
    ((u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000) as u8
}
pub fn parse_color(reply: &str, index: u8) -> Option<[u8; 3]> {
    let prefix = format!("\x1b]{index};rgb:");
    let data = reply
        .strip_prefix(&prefix)?
        .trim_end_matches('\x07')
        .trim_end_matches("\x1b\\");
    let parts: Vec<_> = data.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut rgb = [0; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(p, 16).ok()?;
        rgb[i] = (value * 255 / ((1 << (p.len() * 4)) - 1)) as u8;
    }
    Some(rgb)
}
pub fn kitty_reply(reply: &str) -> Option<(u32, bool)> {
    let body = reply.strip_prefix("\x1b_G")?.strip_suffix("\x1b\\")?;
    let (keys, message) = body.split_once(';')?;
    let id = keys
        .split(',')
        .find_map(|key| key.strip_prefix("i=").and_then(|v| v.parse().ok()))?;
    Some((id, message == "OK"))
}

/// 全削除は使わず、このプロセスが送ったIDだけを解放する。
pub fn cleanup() {
    if let Ok(mut ids) = OWNED.lock() {
        let mut output = io::stdout();
        for id in ids.drain(..) {
            let _ = write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        let _ = output.flush();
    }
}
#[derive(Default)]
pub struct Kitty {
    transferred: HashMap<usize, (u32, std::sync::Arc<Image>)>,
    placed: HashSet<u32>,
}
impl Kitty {
    pub fn clear_placements(&mut self, output: &mut impl Write) -> io::Result<()> {
        for id in self.placed.drain() {
            write!(output, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")?;
        }
        Ok(())
    }
    pub fn reset(&mut self) {
        cleanup();
        self.transferred.clear();
        self.placed.clear();
    }
    pub fn has_error(&self, reply: &str) -> bool {
        kitty_reply(reply)
            .is_some_and(|(id, ok)| !ok && self.transferred.values().any(|(own, _)| *own == id))
    }
    pub fn draw(&mut self, app: &crate::app::App, output: &mut impl Write) -> io::Result<()> {
        if app.help || app.too_small() || !app.graphics_active {
            self.reset();
            return Ok(());
        }
        let mut visible = HashSet::new();
        for (row, line) in app
            .lines
            .iter()
            .enumerate()
            .skip(app.top)
            .take(app.viewport())
        {
            let Some((source, offset)) = line.image else {
                continue;
            };
            if !visible.insert(source) {
                continue;
            }
            let Some(crate::math::View::Ready(image)) = app.math_views.get(&source) else {
                continue;
            };
            if self
                .transferred
                .get(&source)
                .is_some_and(|(_, previous)| !std::sync::Arc::ptr_eq(previous, image))
            {
                let (id, _) = self.transferred.remove(&source).unwrap();
                write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
                if let Ok(mut ids) = OWNED.lock() {
                    ids.retain(|i| *i != id);
                }
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = self.transferred.entry(source)
            {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut ids) = OWNED.lock() {
                    ids.push(id);
                }
                let encoded = STANDARD.encode(&image.png);
                let count = encoded.len().div_ceil(4096);
                for (i, chunk) in encoded.as_bytes().chunks(4096).enumerate() {
                    let more = usize::from(i + 1 < count);
                    if i == 0 {
                        write!(output, "\x1b_Ga=t,t=d,f=100,i={id},q=1,m={more};")?;
                    } else {
                        write!(output, "\x1b_Gm={more},q=1;")?;
                    }
                    output.write_all(chunk)?;
                    output.write_all(b"\x1b\\")?;
                }
                entry.insert((id, image.clone()));
            }
            let id = self.transferred[&source].0;
            let remaining = image
                .rows
                .saturating_sub(offset)
                .min(app.viewport() - (row - app.top));
            let cell_height = image.height as usize / image.rows;
            write!(
                output,
                "\x1b[{};{}H\x1b_Ga=p,i={id},p=1,x=0,y={},w={},h={},c={},r={},C=1,q=1\x1b\\",
                row - app.top + 1,
                if app.width >= 40 { 2 } else { 1 },
                offset * cell_height,
                image.width,
                remaining * cell_height,
                image.columns,
                remaining
            )?;
            self.placed.insert(id);
        }
        let stale: Vec<_> = self
            .transferred
            .keys()
            .filter(|source| !visible.contains(source))
            .copied()
            .collect();
        for source in stale {
            let (id, _) = self.transferred.remove(&source).unwrap();
            write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
            if let Ok(mut ids) = OWNED.lock() {
                ids.retain(|i| *i != id);
            }
        }
        output.flush()
    }
}
