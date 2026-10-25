use clap::{Parser, ValueEnum};
use std::{ffi::OsStr, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn enabled(self, no_color: Option<&OsStr>, term: &str) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => no_color.is_none_or(OsStr::is_empty) && term != "vt100",
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "mdview", version, about = "読み取り専用Markdownビューア", color = clap::ColorChoice::Never)]
pub struct Cli {
    /// 画像・数式画像化・拡大文字・能力照会を無効にする
    #[arg(long)]
    pub plain: bool,
    /// 本文装飾の色を指定する
    #[arg(long, value_enum, default_value = "auto")]
    pub color: ColorMode,
    /// 閲覧するUTF-8の通常ファイル
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
}
