use crate::{
    app::{Action, App},
    cli::GraphicsMode,
    graphics::{Capabilities, Kitty},
    input::{Decoder, InputEvent},
    math::{Manager, Status},
    reload::{self, Reloader},
};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::{SigId, consts::signal::*, flag};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 描画の最短間隔と、一回の入力待ちの上限。
const FRAME: Duration = Duration::from_millis(16);

static RAW: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);
static PASTE: AtomicBool = AtomicBool::new(false);
static CURSOR: AtomicBool = AtomicBool::new(false);

pub struct Signals {
    interrupt: Arc<AtomicBool>,
    terminate: Arc<AtomicBool>,
    suspend: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl Signals {
    pub fn new() -> io::Result<Self> {
        let mut result = Self {
            interrupt: Arc::new(AtomicBool::new(false)),
            terminate: Arc::new(AtomicBool::new(false)),
            suspend: Arc::new(AtomicBool::new(false)),
            ids: Vec::new(),
        };
        for (signal, target) in [
            (SIGINT, &result.interrupt),
            (SIGTERM, &result.terminate),
            (SIGTSTP, &result.suspend),
        ] {
            result.ids.push(flag::register(signal, Arc::clone(target))?);
        }
        Ok(result)
    }
    pub fn exit_code(&self) -> Option<u8> {
        if self.interrupt.load(Ordering::Relaxed) {
            Some(130)
        } else if self.terminate.load(Ordering::Relaxed) {
            Some(143)
        } else {
            None
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// 制御端末を開く。`fumi -` では、キー入力と端末応答をこの端末から読む（仕様2.4節）。
/// 標準入力が閉じていて最小のfdとして0が割り当てられた場合は、文書の入力元がないためエラーにする。
pub fn open_tty() -> io::Result<File> {
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    if tty.as_raw_fd() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "標準入力が閉じています",
        ));
    }
    Ok(tty)
}

/// 文書を読み終えた標準入力を端末に置き換える。以降のキー入力・端末応答・raw modeは、
/// ファイル入力と同じく標準入力（fd 0）の端末を使う。
pub fn replace_stdin(tty: File) -> io::Result<()> {
    if unsafe { libc::dup2(tty.as_raw_fd(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 復旧は各段階を独立に試みる。出力失敗でraw mode解除を飛ばさない。
fn restore() {
    let mut stdout = io::stdout();
    crate::graphics::cleanup();
    if MOUSE.swap(false, Ordering::SeqCst) {
        let _ = write!(stdout, "\x1b[?1000l\x1b[?1006l");
    }
    if PASTE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, DisableBracketedPaste);
    }
    if CURSOR.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, ResetColor);
        let _ = execute!(stdout, SetAttribute(Attribute::Reset));
        let _ = execute!(stdout, Show);
    }
    if RAW.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
    }
    if ALT.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
    let _ = stdout.flush();
}

struct Guard;
impl Guard {
    fn enter() -> io::Result<Self> {
        Self::enter_with(io::stdout())
    }

    fn enter_with(mut output: impl Write) -> io::Result<Self> {
        let result = Self;
        terminal::enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);
        // 書込途中の失敗も復旧対象にする。
        ALT.store(true, Ordering::SeqCst);
        execute!(output, EnterAlternateScreen)?;
        CURSOR.store(true, Ordering::SeqCst);
        execute!(output, Hide)?;
        MOUSE.store(true, Ordering::SeqCst);
        write!(output, "\x1b[?1000h\x1b[?1006h")?;
        PASTE.store(true, Ordering::SeqCst);
        execute!(output, EnableBracketedPaste)?;
        Ok(result)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// 端末の画像対応の確認と画像生成に使うCLI指定。
#[derive(Clone, Copy, Debug)]
pub struct GraphicsOptions {
    pub mode: GraphicsMode,
    pub plain: bool,
    pub colors: bool,
}

/// `reloader` はファイル入力のときだけ渡す。標準入力は読み直せない（仕様2.4節）。
pub fn run(
    mut app: App,
    signals: &Signals,
    options: GraphicsOptions,
    mut reloader: Option<Reloader>,
) -> io::Result<u8> {
    install_panic_hook();
    if let Some(code) = signals.exit_code() {
        return Ok(code);
    }
    let mut guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    crate::profile::mark("terminal_ms");
    app.resize(size.width, size.height);
    crate::profile::mark("layout_ms");
    // 端末の画像対応の確認の待ち時間で初回表示を遅らせないよう、確認前に本文をテキストで描く。
    // 描けなかった場合も、ループの最初の描画で描き直す。
    terminal.draw(|f| {
        app.draw(f);
    })?;
    crate::profile::finish()?;
    let mut decoder = Decoder::default();
    let mut kitty = Kitty::default();
    let mut math = configure_graphics(&mut app, options, &mut decoder, None)?;
    // 画像表示の可否を決めたか。数式のない文書では決めず、再読込で数式が現れたときに確かめる。
    let mut graphics_decided = app.document().has_display_math();
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if let Some(code) = signals.exit_code() {
            return Ok(code);
        }
        let mut suspend = signals.suspend.swap(false, Ordering::Relaxed);
        let current_size = terminal::size()?;
        if current_size != app.size() {
            app.resize(current_size.0, current_size.1);
            if math.is_some() {
                kitty.reset();
                if !app.too_small() {
                    let previous = math.as_ref().map(|m| m.capabilities);
                    math = configure_graphics(&mut app, options, &mut decoder, previous)?;
                }
            }
            dirty = true;
        }
        if let Some(reloader) = &mut reloader {
            reloader.check(Instant::now());
        }
        if let Some(result) = reloader.as_mut().and_then(Reloader::poll) {
            match result {
                Ok(document) => {
                    app.replace_document(document);
                    if let Some(manager) = &mut math {
                        manager.replace_document(app.document(), app.generation());
                    } else if !graphics_decided && app.document().has_display_math() {
                        math = configure_graphics(&mut app, options, &mut decoder, None)?;
                        graphics_decided = true;
                    }
                    app.set_notice("再読込");
                }
                // 旧表示を維持する（仕様3.1節）。
                Err(e) => app.set_notice(&format!("再読込失敗: {}", reload::reason(&e))),
            }
            dirty = true;
        }
        if let Some(manager) = &mut math
            && let Some(views) = manager.update(&app.view_state())
        {
            dirty |= app.set_math(views, Status::Pending);
        }
        if dirty && last_draw.elapsed() >= FRAME {
            kitty.clear_placements(terminal.backend_mut())?;
            let prepared = kitty.prepare(&app.view_state(), terminal.backend_mut());
            // 描画の直前に端末サイズが変わると描けない。次の周回でサイズを合わせて描き直す。
            let mut drawn = false;
            if prepared.is_ok() {
                terminal.draw(|f| {
                    drawn = app.draw(f);
                    if drawn {
                        kitty.render_placeholders(&app.view_state(), f);
                    }
                })?;
            }
            let placed = match prepared {
                Ok(()) if drawn => kitty.draw(&app.view_state(), terminal.backend_mut()),
                result => result,
            };
            if placed.is_err() {
                disable_graphics(&mut app, &mut kitty, &mut math);
                terminal.draw(|f| {
                    drawn = app.draw(f);
                })?;
            }
            crate::profile::finish()?;
            last_draw = Instant::now();
            dirty = !drawn;
        }
        crate::input::read(&mut decoder, FRAME)?;
        while let Some(event) = decoder.events.pop_front() {
            match event {
                InputEvent::User(Event::Key(key)) => {
                    match app.key(key) {
                        Action::Exit(code) => return Ok(code),
                        Action::Suspend => suspend = true,
                        Action::Reload => match &mut reloader {
                            Some(reloader) => {
                                reloader.request();
                                app.set_notice("再読込中");
                            }
                            None => app.set_notice("stdinは再読込できません"),
                        },
                        Action::None => {}
                    }
                    dirty = true;
                }
                InputEvent::User(Event::Mouse(mouse)) => {
                    app.mouse(mouse);
                    dirty = true;
                }
                InputEvent::User(Event::Paste(text)) => {
                    app.paste(&text);
                    dirty = true;
                }
                InputEvent::Reply(reply) if kitty.has_error(&reply) => {
                    disable_graphics(&mut app, &mut kitty, &mut math);
                    dirty = true;
                }
                _ => {}
            }
        }
        if suspend {
            kitty.reset();
            drop(math.take());
            drop(guard);
            // SIGTSTPはハンドラへ配送されるため、復旧後の実際の停止にはSIGSTOPを使う。
            signal_hook::low_level::raise(SIGSTOP)?;
            if let Some(code) = signals.exit_code() {
                return Ok(code);
            }
            guard = Guard::enter()?;
            let size = terminal::size()?;
            app.resize(size.0, size.1);
            math = configure_graphics(&mut app, options, &mut decoder, None)?;
            graphics_decided = app.document().has_display_math();
            terminal.clear()?;
            dirty = true;
        }
    }
}

/// 転送失敗後はこの起動中に画像を再試行せず、テキスト表示へ戻す。
fn disable_graphics(app: &mut App, kitty: &mut Kitty, math: &mut Option<Manager>) {
    kitty.reset();
    *math = None;
    app.disable_graphics("画像転送失敗".into(), true);
}

fn configure_graphics(
    app: &mut App,
    options: GraphicsOptions,
    decoder: &mut Decoder,
    previous: Option<Capabilities>,
) -> io::Result<Option<Manager>> {
    // 画像化する数式がなければ、端末の画像対応を確かめない。再読込で数式が現れたら呼び直す。
    if !app.document().has_display_math() {
        return Ok(None);
    }
    match crate::graphics::query(
        options.mode,
        options.plain,
        options.colors,
        decoder,
        previous,
    )? {
        Ok(capabilities) => {
            app.enable_graphics();
            Ok(Some(Manager::new(
                app.document(),
                app.generation(),
                capabilities,
            )))
        }
        Err(reason) => {
            // 利用者が画像を無効にした場合は、理由を数式に付けるだけで警告は出さない。
            let requested = options.plain || options.mode == GraphicsMode::Off;
            app.disable_graphics(reason, !requested);
            Ok(None)
        }
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FailAfter(usize);
    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("injected terminal output failure"));
            }
            let count = self.0.min(bytes.len());
            self.0 -= count;
            io::stdout().write(&bytes[..count])
        }
        fn flush(&mut self) -> io::Result<()> {
            io::stdout().flush()
        }
    }

    #[test]
    #[ignore = "PTY専用。tests/pty_smoke.pyから明示実行する"]
    fn restoration_fault_probe() {
        assert!(std::io::IsTerminal::is_terminal(&io::stdin()));
        install_panic_hook();
        match std::env::var("FUMI_FAULT_PROBE").unwrap().as_str() {
            "0" => assert!(Guard::enter_with(FailAfter(0)).is_err()),
            "8" => assert!(Guard::enter_with(FailAfter(8)).is_err()),
            "panic" => assert!(
                std::panic::catch_unwind(|| {
                    let _guard = Guard::enter().unwrap();
                    panic!("intentional restoration probe");
                })
                .is_err()
            ),
            _ => panic!("unknown fault scenario"),
        }
        assert!(!terminal::is_raw_mode_enabled().unwrap());
        println!("RESTORATION_PROBE_OK");
    }
}
