use crate::{
    cli::GraphicsMode,
    input::{self, Decoder, InputEvent},
    layout::DisplayLine,
    math::{Image, View},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ratatui::{Frame, style::Color};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::BuildHasher,
    io::{self, Write},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "graphics_diacritics.rs"]
mod diacritics;
pub const PLACEHOLDER_COLUMNS: usize = diacritics::DIACRITICS.len();
const QUERY_ID: u32 = 4_000_000_001;
/// 端末の画像対応の確認で応答を待つ上限。tmuxの自動設定もこの時間に含める。
const QUERY_TIMEOUT: Duration = Duration::from_millis(150);
static OWNED: Mutex<Vec<(u32, Transport)>> = Mutex::new(Vec::new());
static NEXT_ID: OnceLock<AtomicU32> = OnceLock::new();

// ペイン間でのID衝突を避けるため、プロセスごとに開始値をランダム化する。
fn next_id() -> u32 {
    let ids = NEXT_ID.get_or_init(|| {
        AtomicU32::new(
            std::collections::hash_map::RandomState::new().hash_one(std::process::id()) as u32,
        )
    });
    loop {
        let id = ids.fetch_add(1, Ordering::Relaxed);
        if id != 0 && id != QUERY_ID {
            return id;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Direct,
    Tmux,
}
impl Transport {
    fn detect() -> Self {
        if std::env::var_os("TMUX").is_some() {
            Self::Tmux
        } else {
            Self::Direct
        }
    }

    // カーソル移動とプレースホルダーはラップしない。tmuxのペイン座標で扱う。
    fn control(self, output: &mut impl Write, sequence: &str) -> io::Result<()> {
        if self == Self::Tmux {
            output.write_all(b"\x1bPtmux;")?;
            for part in sequence.split_inclusive('\x1b') {
                output.write_all(part.as_bytes())?;
                if part.ends_with('\x1b') {
                    output.write_all(b"\x1b")?;
                }
            }
            output.write_all(b"\x1b\\")
        } else {
            output.write_all(sequence.as_bytes())
        }
    }
}

/// 起動元のペインだけを設定する。tmux.confやグローバル設定には触れない。
fn enable_tmux_passthrough(deadline: Instant) -> Result<(), &'static str> {
    let pane = std::env::var("TMUX_PANE").map_err(|_| "tmuxペインID不明")?;
    if !pane
        .strip_prefix('%')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("tmuxペインID不正");
    }
    let mut child = Command::new("tmux")
        .args(["set-option", "-p", "-t", &pane, "allow-passthrough", "all"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "tmux自動設定コマンドを起動できません")?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err("tmuxパススルー自動設定失敗"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(2)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if result.is_err() {
                    "tmux自動設定の終了確認失敗"
                } else {
                    "tmux自動設定タイムアウト"
                });
            }
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub transport: Transport,
    pub cell: (u16, u16),
    pub foreground: [u8; 3],
    pub monochrome: bool,
}

pub fn query(
    mode: GraphicsMode,
    plain: bool,
    colors: bool,
    decoder: &mut Decoder,
    previous: Option<Capabilities>,
) -> io::Result<Result<Capabilities, String>> {
    if plain || mode == GraphicsMode::Off {
        return Ok(Err("画像無効".into()));
    }
    if std::env::var_os("STY").is_some() {
        return Ok(Err("screenでは画像無効".into()));
    }
    let transport = Transport::detect();
    let mut output = io::stdout();
    let start = Instant::now();
    // tmuxは外側のAPC応答をアクティブなペインへ配送する。非アクティブな
    // ペインのリサイズでは確認済みの対応状態を再利用し、寸法と色だけ更新する。
    let reused =
        transport == Transport::Tmux && previous.is_some_and(|c| c.transport == Transport::Tmux);
    if transport == Transport::Tmux && !reused {
        if let Err(reason) = enable_tmux_passthrough(start + QUERY_TIMEOUT) {
            return Ok(Err(reason.into()));
        }
        if start.elapsed() >= QUERY_TIMEOUT {
            return Ok(Err("tmux自動設定タイムアウト".into()));
        }
    }
    let probe_id = (transport == Transport::Tmux && !reused).then(next_id);
    send_probes(&mut output, transport, !reused, probe_id)?;
    let mut replies = Replies {
        supported: reused,
        placeholders: transport != Transport::Tmux || reused,
        ..Default::default()
    };
    collect_replies(decoder, &mut replies, probe_id, start)?;
    if let Some(id) = probe_id {
        release_image(id, transport, &mut output)?;
        output.flush()?;
    }
    if !replies.supported && (mode == GraphicsMode::Auto || transport == Transport::Tmux) {
        return Ok(Err(if transport == Transport::Tmux {
            "tmuxで画像対応の確認に応答なし"
        } else {
            "画像非対応・確認に応答なし"
        }
        .into()));
    }
    if !replies.placeholders {
        return Ok(Err("tmux用Unicode画像配置に未対応・確認に応答なし".into()));
    }
    let Some(cell) = cell_size(replies.cell, replies.pixels) else {
        return Ok(Err("セル寸法不明".into()));
    };
    let Some(foreground) = text_color(replies.foreground, replies.background, colors) else {
        return Ok(Err("文字色不明".into()));
    };
    Ok(Ok(Capabilities {
        transport,
        cell,
        foreground,
        monochrome: !colors,
    }))
}

/// 端末の画像対応の確認で受け取った応答。
#[derive(Default)]
struct Replies {
    /// Kittyの照会に成功が返ったか。
    supported: bool,
    /// tmux用の仮想配置に成功が返ったか。
    placeholders: bool,
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
    /// セルのピクセル寸法（幅, 高さ）。
    cell: Option<(u16, u16)>,
    /// ウィンドウのピクセル寸法（幅, 高さ）。
    pixels: Option<(u16, u16)>,
}
impl Replies {
    fn record(&mut self, reply: &str, probe_id: Option<u32>) {
        if let Some((id, true)) = kitty_reply(reply) {
            self.supported |= id == QUERY_ID;
            self.placeholders |= Some(id) == probe_id;
        }
        if let Some(value) = parse_color(reply, 10) {
            self.foreground = Some(value);
        }
        if let Some(value) = parse_color(reply, 11) {
            self.background = Some(value);
        }
        match parse_window_report(reply) {
            Some((6, size)) => self.cell = Some(size),
            Some((4, size)) => self.pixels = Some(size),
            _ => {}
        }
    }
    /// 必要な応答がそろい、期限まで待たなくてよいか。
    fn complete(&self) -> bool {
        self.supported && self.placeholders && self.cell.is_some() && self.foreground.is_some()
    }
}

/// Kittyの照会、tmux用の仮想配置、寸法と色の照会を送る。
fn send_probes(
    output: &mut impl Write,
    transport: Transport,
    kitty: bool,
    probe_id: Option<u32>,
) -> io::Result<()> {
    if kitty {
        transport.control(
            output,
            &format!("\x1b_Gi={QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"),
        )?;
    }
    if let Some(id) = probe_id {
        register_owned(id, transport);
        // 透明な1ピクセルの仮想配置。実画像を送る前にU=1のエラーを確認する。
        transport.control(
            output,
            &format!("\x1b_Ga=T,U=1,i={id},s=1,v=1,c=1,r=1,t=d,f=32;AAAAAA==\x1b\\"),
        )?;
    }
    // 寸法・色はtmux自身へ問い合わせる。外側端末の全画面寸法をペインへ流用しない。
    output.write_all(b"\x1b[16t\x1b[14t\x1b]11;?\x1b\\\x1b]10;?\x1b\\")?;
    output.flush()
}

/// 応答を期限まで集める。確認中に届いたキー入力は、確認後に入力の待ち行列へ戻す。
fn collect_replies(
    decoder: &mut Decoder,
    replies: &mut Replies,
    probe_id: Option<u32>,
    start: Instant,
) -> io::Result<()> {
    let mut users = VecDeque::new();
    while start.elapsed() < QUERY_TIMEOUT {
        input::read(
            decoder,
            QUERY_TIMEOUT
                .saturating_sub(start.elapsed())
                .min(Duration::from_millis(8)),
        )?;
        while let Some(event) = decoder.events.pop_front() {
            match event {
                InputEvent::User(_) => users.push_back(event),
                InputEvent::Reply(reply) => replies.record(&reply, probe_id),
            }
        }
        if replies.complete() {
            break;
        }
    }
    decoder.events.extend(users);
    Ok(())
}

/// 応答のセル寸法。応答がなければ、端末のウィンドウ寸法を行数・列数で割って求める。
fn cell_size(reported: Option<(u16, u16)>, pixels: Option<(u16, u16)>) -> Option<(u16, u16)> {
    let cell = reported.or_else(|| {
        let size = crossterm::terminal::window_size().ok()?;
        if size.columns == 0 || size.rows == 0 {
            return None;
        }
        if size.width > 0 && size.height > 0 {
            Some((size.width / size.columns, size.height / size.rows))
        } else {
            pixels.map(|(w, h)| (w / size.columns, h / size.rows))
        }
    });
    cell.filter(|(w, h)| *w > 0 && *h > 0 && *w <= 512 && *h <= 512)
}

/// 数式を描く色。数式画像の背景は透明にするため、必要なのは文字色だけ。
/// 文字色を取得できない場合は、背景色に対して読める白か黒を選ぶ。
/// 色なしでは、文字色の明るさに合わせて白か黒にする。
fn text_color(
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
    colors: bool,
) -> Option<[u8; 3]> {
    let white_or_black = |light: bool| if light { [255; 3] } else { [0; 3] };
    let foreground =
        foreground.or_else(|| background.map(|rgb| white_or_black(luminance(rgb) <= 128)))?;
    Some(if colors {
        foreground
    } else {
        white_or_black(luminance(foreground) > 128)
    })
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
/// `CSI 種別;高さ;幅 t` のウィンドウ報告。種別と（幅, 高さ）を返す。
fn parse_window_report(reply: &str) -> Option<(u16, (u16, u16))> {
    let values = reply.strip_prefix("\x1b[")?.strip_suffix('t')?;
    let values = values
        .split(';')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    match values[..] {
        [kind, height, width] if height > 0 && width > 0 => Some((kind, (width, height))),
        _ => None,
    }
}
pub fn kitty_reply(reply: &str) -> Option<(u32, bool)> {
    let body = reply.strip_prefix("\x1b_G")?.strip_suffix("\x1b\\")?;
    let (keys, message) = body.split_once(';')?;
    let id = keys
        .split(',')
        .find_map(|key| key.strip_prefix("i=").and_then(|v| v.parse().ok()))?;
    Some((id, message == "OK"))
}

/// `free` では画像データも解放し、そうでなければ配置だけを消す。
fn delete_command(id: u32, free: bool) -> String {
    let target = if free { 'I' } else { 'i' };
    format!("\x1b_Ga=d,d={target},i={id},q=2\x1b\\")
}

/// 全削除は使わず、このプロセスが送ったIDだけを解放する。
pub fn cleanup() {
    if let Ok(mut ids) = OWNED.lock() {
        let mut output = io::stdout();
        for (id, transport) in ids.drain(..) {
            let _ = transport.control(&mut output, &delete_command(id, true));
        }
        let _ = output.flush();
    }
}

/// 終了時に解放できるよう、このプロセスが送った画像IDを記録する。
fn register_owned(id: u32, transport: Transport) {
    if let Ok(mut ids) = OWNED.lock() {
        ids.push((id, transport));
    }
}

fn release_image(id: u32, transport: Transport, output: &mut impl Write) -> io::Result<()> {
    transport.control(output, &delete_command(id, true))?;
    if let Ok(mut ids) = OWNED.lock() {
        ids.retain(|(own, _)| *own != id);
    }
    Ok(())
}

/// 画像処理が参照する本文の表示状態。UIの状態全体には依存しない。
#[derive(Clone, Copy)]
pub struct ViewState<'a> {
    /// 本文の文書の世代。異なる世代の数式の結果を配置しない。
    pub generation: u64,
    pub lines: &'a [DisplayLine],
    pub top: usize,
    /// 本文の表示行数。
    pub rows: usize,
    /// 本文の表示幅（セル）。
    pub width: usize,
    /// 本文の左端の列。
    pub left: u16,
    /// 本文を表示しているか。ヘルプ表示中とサイズ不足では偽。
    pub body_visible: bool,
    /// 画像表示が有効なときの数式の結果。
    pub images: Option<&'a HashMap<usize, View>>,
}
impl<'a> ViewState<'a> {
    /// 画面上に画像を置ける状態での数式の結果。
    fn shown_images(&self) -> Option<&'a HashMap<usize, View>> {
        self.images.filter(|_| self.body_visible)
    }
    /// 画面上の行番号と、その行に表示する本文の表示行。
    fn screen_lines(&self) -> impl Iterator<Item = (usize, &'a DisplayLine)> + use<'a> {
        self.lines.iter().skip(self.top).take(self.rows).enumerate()
    }
}

pub struct Kitty {
    transport: Transport,
    transferred: HashMap<usize, (u32, Arc<Image>)>,
    placed: HashSet<u32>,
}
impl Default for Kitty {
    fn default() -> Self {
        Self::new(Transport::detect())
    }
}
impl Kitty {
    pub fn new(transport: Transport) -> Self {
        Self {
            transport,
            transferred: HashMap::new(),
            placed: HashSet::new(),
        }
    }
    pub fn clear_placements(&mut self, output: &mut impl Write) -> io::Result<()> {
        for id in self.placed.drain() {
            self.transport.control(output, &delete_command(id, false))?;
        }
        Ok(())
    }
    pub fn reset(&mut self) {
        cleanup();
        self.transferred.clear();
        self.placed.clear();
    }
    fn release(&self, id: u32, output: &mut impl Write) -> io::Result<()> {
        release_image(id, self.transport, output)
    }
    pub fn has_error(&self, reply: &str) -> bool {
        kitty_reply(reply)
            .is_some_and(|(id, ok)| !ok && self.transferred.values().any(|(own, _)| *own == id))
    }
    /// 画像を先に転送する。Unicodeセルはratatuiの差分管理に含める。
    pub fn prepare(&mut self, view: &ViewState, output: &mut impl Write) -> io::Result<()> {
        let Some(views) = view.shown_images() else {
            self.reset();
            return Ok(());
        };
        let mut visible = HashSet::new();
        for (_, line) in view.screen_lines() {
            let Some((source, _)) = line.image else {
                continue;
            };
            if !visible.insert(source) {
                continue;
            }
            let Some(View::Ready(image)) = views.get(&source) else {
                continue;
            };
            if let Some(&(id, ref previous)) = self.transferred.get(&source)
                && !Arc::ptr_eq(previous, image)
            {
                self.transferred.remove(&source);
                self.release(id, output)?;
            }
            if !self.transferred.contains_key(&source) {
                let id = next_id();
                register_owned(id, self.transport);
                self.transmit(id, image, output)?;
                self.transferred.insert(source, (id, image.clone()));
            }
        }
        let stale: Vec<_> = self
            .transferred
            .keys()
            .filter(|source| !visible.contains(source))
            .copied()
            .collect();
        for source in stale {
            if let Some((id, _)) = self.transferred.remove(&source) {
                self.release(id, output)?;
            }
        }
        output.flush()
    }
    /// 画像データを分割して送る。tmuxでは続けてUnicodeセル用の仮想配置を作る。
    fn transmit(&self, id: u32, image: &Image, output: &mut impl Write) -> io::Result<()> {
        let tmux = self.transport == Transport::Tmux;
        let encoded = STANDARD.encode(&image.png);
        // DCSのヘッダーとESCエスケープを含めても4096バイトを超えない。
        let chunk_size = if tmux { 3072 } else { 4096 };
        let count = encoded.len().div_ceil(chunk_size);
        // tmuxでは失敗応答も他ペインへ入り得るため、初回照会後はquietにする。
        let quiet = if tmux { 2 } else { 1 };
        for (i, chunk) in encoded.as_bytes().chunks(chunk_size).enumerate() {
            let more = usize::from(i + 1 < count);
            let header = if i == 0 {
                format!("a=t,t=d,f=100,i={id},q={quiet},m={more}")
            } else {
                format!("m={more},q={quiet}")
            };
            // base64はASCIIなので変換は常に成立する。
            self.transport.control(
                output,
                &format!(
                    "\x1b_G{header};{}\x1b\\",
                    std::str::from_utf8(chunk).unwrap()
                ),
            )?;
        }
        if tmux {
            self.transport.control(
                output,
                &format!(
                    "\x1b_Ga=p,U=1,i={id},p=1,c={},r={},C=1,q=2\x1b\\",
                    image.columns, image.rows
                ),
            )?;
        }
        Ok(())
    }
    pub fn render_placeholders(&self, view: &ViewState, frame: &mut Frame) {
        if self.transport != Transport::Tmux || view.shown_images().is_none() {
            return;
        }
        for (row, line) in view.screen_lines() {
            let Some((source, offset)) = line.image else {
                continue;
            };
            let Some((id, image)) = self.transferred.get(&source) else {
                continue;
            };
            let Some(&row_mark) = diacritics::DIACRITICS.get(offset) else {
                continue;
            };
            let high = diacritics::DIACRITICS[(id >> 24) as usize];
            let color = Color::Rgb((id >> 16) as u8, (id >> 8) as u8, *id as u8);
            for (column, &column_mark) in diacritics::DIACRITICS
                .iter()
                .take(image.columns.min(view.width))
                .enumerate()
            {
                let symbol = format!("\u{10eeee}{row_mark}{column_mark}{high}");
                let cell = &mut frame.buffer_mut()[(column as u16 + view.left, row as u16)];
                cell.reset();
                cell.set_symbol(&symbol).set_fg(color);
            }
        }
    }
    /// 直接接続時だけ、本文描画後のカーソル座標へ画像を配置する。
    pub fn draw(&mut self, view: &ViewState, output: &mut impl Write) -> io::Result<()> {
        if self.transport == Transport::Tmux || view.shown_images().is_none() {
            return Ok(());
        }
        let mut visible = HashSet::new();
        for (row, line) in view.screen_lines() {
            let Some((source, offset)) = line.image else {
                continue;
            };
            if !visible.insert(source) {
                continue;
            }
            let Some((id, image)) = self.transferred.get(&source) else {
                continue;
            };
            let remaining = image.rows.saturating_sub(offset).min(view.rows - row);
            let cell_height = image.height as usize / image.rows;
            write!(output, "\x1b[{};{}H", row + 1, view.left + 1)?;
            self.transport.control(
                output,
                &format!(
                    "\x1b_Ga=p,i={id},p=1,x=0,y={},w={},h={},c={},r={},C=1,q=1\x1b\\",
                    offset * cell_height,
                    image.width,
                    remaining * cell_height,
                    image.columns,
                    remaining
                ),
            )?;
            self.placed.insert(*id);
        }
        output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::App,
        document::{Document, Kind},
    };
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn tmux_passthrough_escapes_each_escape_and_keeps_commands_separate() {
        let mut bytes = Vec::new();
        for command in ["\x1b_Ga=t;AAAA\x1b\\", "\x1b_Ga=d,d=I,i=42\x1b\\"] {
            Transport::Tmux.control(&mut bytes, command).unwrap();
        }
        assert_eq!(bytes, b"\x1bPtmux;\x1b\x1b_Ga=t;AAAA\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=42\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn window_reports_and_text_color_fallbacks() {
        assert_eq!(parse_window_report("\x1b[6;20;10t"), Some((6, (10, 20))));
        assert_eq!(
            parse_window_report("\x1b[4;480;800t"),
            Some((4, (800, 480)))
        );
        assert_eq!(parse_window_report("\x1b[6;0;10t"), None);
        assert_eq!(parse_window_report("\x1b[6;20t"), None);
        assert_eq!(parse_window_report("\x1b[6;x;10t"), None);
        // 文字色を優先し、なければ背景に対して読める白か黒にする。
        assert_eq!(
            text_color(Some([10, 20, 30]), Some([255; 3]), true),
            Some([10, 20, 30])
        );
        assert_eq!(text_color(None, Some([250; 3]), true), Some([0; 3]));
        assert_eq!(text_color(None, Some([5; 3]), true), Some([255; 3]));
        assert_eq!(text_color(None, None, true), None);
        // 色なしでは文字色の明るさで白か黒にする。
        assert_eq!(text_color(Some([200; 3]), None, false), Some([255; 3]));
        assert_eq!(text_color(Some([40; 3]), None, false), Some([0; 3]));
    }

    #[test]
    fn placeholder_cells_clip_at_both_edges_clear_on_help_and_return() {
        let mut app = App::new(
            Document::parse("before\n\n$$ x $$\n\nafter\n\n".repeat(10)),
            "test.md",
            true,
        );
        app.resize(40, 5);
        app.enable_graphics();
        let source = app
            .document()
            .nodes
            .iter()
            .find(|n| matches!(n.kind, Kind::Math { .. }))
            .unwrap()
            .source
            .start;
        let image = Arc::new(Image {
            png: vec![],
            width: 30,
            height: 80,
            columns: 3,
            rows: 4,
        });
        app.set_math(
            HashMap::from([(source, View::Ready(image.clone()))]),
            Default::default(),
        );
        let start = app
            .lines()
            .iter()
            .position(|line| line.image == Some((source, 0)))
            .unwrap();
        let mut kitty = Kitty::new(Transport::Tmux);
        // 全32bitのID（上位バイトも結合文字で保持）、列ごとに明示的な列番号。
        kitty.transferred.insert(source, (0x02030405, image));
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, app: &App| {
            terminal
                .draw(|f| {
                    app.draw(f);
                    kitty.render_placeholders(&app.view_state(), f);
                })
                .unwrap()
                .buffer
                .clone()
        };
        app.scroll_to(start + 2);
        let buffer = draw(&mut terminal, &app);
        assert_eq!(
            buffer[(1, 0)].symbol(),
            "\u{10eeee}\u{030e}\u{0305}\u{030e}"
        );
        assert_eq!(
            buffer[(2, 0)].symbol(),
            "\u{10eeee}\u{030e}\u{030d}\u{030e}"
        );
        assert_eq!(buffer[(1, 0)].fg, Color::Rgb(3, 4, 5));
        assert!(!buffer[(1, 2)].symbol().starts_with('\u{10eeee}'));
        assert!(!buffer[(1, 4)].symbol().starts_with('\u{10eeee}'));
        app.scroll_to(start - 2);
        let buffer = draw(&mut terminal, &app);
        assert_eq!(
            buffer[(1, 2)].symbol(),
            "\u{10eeee}\u{0305}\u{0305}\u{030e}"
        );
        assert!(!buffer[(1, 4)].symbol().starts_with('\u{10eeee}'));
        app.key(KeyEvent::from(KeyCode::Char('?')));
        let buffer = draw(&mut terminal, &app);
        assert!(
            !buffer
                .content
                .iter()
                .any(|c| c.symbol().starts_with('\u{10eeee}'))
        );
        app.key(KeyEvent::from(KeyCode::Char('?')));
        draw(&mut terminal, &app);
        assert!(
            terminal.backend().buffer()[(1, 2)]
                .symbol()
                .starts_with('\u{10eeee}')
        );
    }
}
