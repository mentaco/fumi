use clap::Parser;
use fumi::{
    app::App,
    cli::Cli,
    document::Document,
    profile,
    source::{load, sanitize},
    terminal::{GraphicsOptions, Signals, run},
};
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

fn error(target: &str, message: &str, code: u8) -> ExitCode {
    let _ = writeln!(
        io::stderr(),
        "fumi: {}: {}",
        sanitize(target),
        sanitize(message)
    );
    ExitCode::from(code)
}

fn main() -> ExitCode {
    if std::env::var_os("FUMI_INTERNAL_MATH_WORKER").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return ExitCode::from(fumi::math::worker());
    }
    profile::start();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = e.exit_code() as u8;
            // clapの診断に含まれるファイル名や未知の引数にも無害化を適用する。
            let text = e
                .to_string()
                .split('\n')
                .map(sanitize)
                .collect::<Vec<_>>()
                .join("\n");
            if e.use_stderr() {
                let _ = writeln!(io::stderr(), "fumi: 引数: {text}");
            } else {
                let _ = write!(io::stdout(), "{text}");
            }
            return ExitCode::from(code);
        }
    };
    if cli.file.as_os_str() == "-" {
        return error(
            "-",
            "標準入力はv1では未対応です。実ファイルには ./- を指定してください",
            2,
        );
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return error("端末", "stdinとstdoutの両方にTTYが必要です", 1);
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return error("端末", "TERMに有効な端末種別を設定してください", 1);
    }
    let signals = match Signals::new() {
        Ok(s) => s,
        Err(e) => return error("端末", &e.to_string(), 1),
    };
    profile::mark("setup_ms");
    let source = match load(&cli.file) {
        Ok(s) => s,
        Err(e) => {
            return signals.exit_code().map_or_else(
                || error(&cli.file.to_string_lossy(), &e.to_string(), 1),
                ExitCode::from,
            );
        }
    };
    profile::mark("load_ms");
    if let Some(code) = signals.exit_code() {
        return ExitCode::from(code);
    }
    let colors = cli
        .color
        .enabled(std::env::var_os("NO_COLOR").as_deref(), &term);
    // crossterm自身もNO_COLORを参照するため、CLIで確定した優先順位を反映する。
    crossterm::style::force_color_output(colors);
    let name = cli
        .file
        .file_name()
        .unwrap_or(cli.file.as_os_str())
        .to_string_lossy();
    profile::mark("prepare_ms");
    let document = Document::parse(source);
    profile::mark("parse_ms");
    let app = App::new(document, &name, colors);
    profile::mark("app_ms");
    let options = GraphicsOptions {
        mode: cli.graphics,
        plain: cli.plain,
        colors,
    };
    match run(app, &signals, options) {
        Ok(code) => ExitCode::from(code),
        Err(e) => error("端末", &e.to_string(), 1),
    }
}
