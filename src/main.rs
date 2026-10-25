use clap::Parser;
use mdview::{
    app::App,
    cli::Cli,
    document::Document,
    source::{load, sanitize},
    terminal::{Signals, run},
};
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

fn error(target: &str, message: &str, code: u8) -> ExitCode {
    let _ = writeln!(
        io::stderr(),
        "mdview: {}: {}",
        sanitize(target),
        sanitize(message)
    );
    ExitCode::from(code)
}

fn main() -> ExitCode {
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
                let _ = writeln!(io::stderr(), "mdview: 引数: {text}");
            } else {
                let _ = write!(io::stdout(), "{text}");
            }
            return ExitCode::from(code);
        }
    };
    if cli.file.as_os_str() == "-" {
        return error(
            "-",
            "標準入力はMVPでは未対応です。実ファイルには ./- を指定してください",
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
    let source = match load(&cli.file) {
        Ok(s) => s,
        Err(e) => {
            return signals.exit_code().map_or_else(
                || error(&cli.file.to_string_lossy(), &e.to_string(), 1),
                ExitCode::from,
            );
        }
    };
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
    let app = App::new(Document::parse(source), &name, colors);
    // MVPは常にテキスト描画。plainでも通常表示でも照会を送信しない。
    match run(app, &signals) {
        Ok(code) => ExitCode::from(code),
        Err(e) => error("端末", &e.to_string(), 1),
    }
}
