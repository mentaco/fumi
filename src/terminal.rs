use crate::app::{Action, App};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event},
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::{SigId, consts::signal::*, flag};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static RAW: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
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

/// 復旧は各段階を独立に試みる。出力失敗でraw mode解除を飛ばさない。
fn restore() {
    let mut stdout = io::stdout();
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
        Ok(result)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

pub fn run(mut app: App, signals: &Signals) -> io::Result<u8> {
    install_panic_hook();
    if let Some(code) = signals.exit_code() {
        return Ok(code);
    }
    let mut guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    loop {
        if let Some(code) = signals.exit_code() {
            return Ok(code);
        }
        let mut suspend = signals.suspend.swap(false, Ordering::Relaxed);
        if dirty && last_draw.elapsed() >= Duration::from_millis(16) {
            terminal.draw(|f| app.draw(f))?;
            last_draw = Instant::now();
            dirty = false;
        }
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    match app.key(key) {
                        Action::Exit(code) => return Ok(code),
                        Action::Suspend => suspend = true,
                        Action::None => {}
                    }
                    dirty = true;
                }
                Event::Resize(w, h) => {
                    app.resize(w, h);
                    dirty = true;
                }
                _ => {}
            }
        }
        if suspend {
            drop(guard);
            // SIGTSTPはハンドラへ配送されるため、復旧後の実際の停止にはSIGSTOPを使う。
            signal_hook::low_level::raise(SIGSTOP)?;
            if let Some(code) = signals.exit_code() {
                return Ok(code);
            }
            guard = Guard::enter()?;
            let size = terminal::size()?;
            app.resize(size.0, size.1);
            terminal.clear()?;
            dirty = true;
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
        match std::env::var("MDVIEW_FAULT_PROBE").unwrap().as_str() {
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
