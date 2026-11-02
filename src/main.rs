use clap::Parser;
use fumi::{
    app::App,
    cli::Cli,
    document::Document,
    profile,
    reload::Reloader,
    source::{STDIN_NAME, load, load_stdin, sanitize},
    terminal::{GraphicsOptions, Signals, open_tty, replace_stdin, run},
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
    // `-` は標準入力。実ファイル名が `-` の場合は `./-` を指定する（仕様2.1・2.4節）。
    let stdin_input = cli.file.as_os_str() == "-";
    let tty = if stdin_input {
        if !io::stdout().is_terminal() {
            return error("端末", "stdoutにTTYが必要です", 1);
        }
        if io::stdin().is_terminal() {
            return error(
                "端末",
                "fumi - には、パイプかリダイレクトで標準入力を渡してください",
                1,
            );
        }
        match open_tty() {
            Ok(tty) => Some(tty),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                return error(STDIN_NAME, &e.to_string(), 1);
            }
            Err(e) => return error("端末", &format!("/dev/ttyを開けません: {e}"), 1),
        }
    } else {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return error("端末", "stdinとstdoutの両方にTTYが必要です", 1);
        }
        None
    };
    let term = std::env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" {
        return error("端末", "TERMに有効な端末種別を設定してください", 1);
    }
    let signals = match Signals::new() {
        Ok(s) => s,
        Err(e) => return error("端末", &e.to_string(), 1),
    };
    profile::mark("setup_ms");
    // 読込エラーの対象はファイルなら指定されたパス全体、標準入力なら表示名。
    let target = if stdin_input {
        STDIN_NAME.into()
    } else {
        cli.file.to_string_lossy()
    };
    let loaded = if stdin_input {
        load_stdin(|| signals.exit_code().is_some())
    } else {
        load(&cli.file)
    };
    let source = match loaded {
        Ok(s) => s,
        Err(e) => {
            return signals
                .exit_code()
                .map_or_else(|| error(&target, &e.to_string(), 1), ExitCode::from);
        }
    };
    profile::mark("load_ms");
    if let Some(code) = signals.exit_code() {
        return ExitCode::from(code);
    }
    if let Some(tty) = tty
        && let Err(e) = replace_stdin(tty)
    {
        return error("端末", &e.to_string(), 1);
    }
    let colors = cli
        .color
        .enabled(std::env::var_os("NO_COLOR").as_deref(), &term);
    // 本文の色の有無はレイアウトとハイライトで決める。crossterm自身のNO_COLOR対応は
    // tmuxの画像IDを伝えるセルのRGB指定まで消すため、常に色を出力できる状態にする。
    crossterm::style::force_color_output(true);
    let name = if stdin_input {
        STDIN_NAME.into()
    } else {
        cli.file
            .file_name()
            .unwrap_or(cli.file.as_os_str())
            .to_string_lossy()
    };
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
    let reloader = (!stdin_input).then(|| Reloader::new(cli.file.clone()));
    match run(app, &signals, options, reloader) {
        Ok(code) => ExitCode::from(code),
        Err(e) => error("端末", &e.to_string(), 1),
    }
}
