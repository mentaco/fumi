//! ファイル入力の再読込（仕様3.1節）。読込と解析は補助スレッドで行い、結果だけを端末ループへ渡す。
use crate::{document::Document, source::load};
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

pub struct Reloader {
    /// 指定されたパス。シンボリックリンクは読むたびに解決し直す。
    path: PathBuf,
    pending: Option<Receiver<io::Result<Document>>>,
    /// 読込中に再読込を求められたか。読込の開始後に書き換わった可能性があるため、
    /// 終わった読込の結果は捨てて読み直す。
    again: bool,
}

impl Reloader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            pending: None,
            again: false,
        }
    }

    /// 再読込を始める。読込中なら、終わった後にもう一度読む。
    pub fn request(&mut self) {
        if self.pending.is_some() {
            self.again = true;
        } else {
            self.start();
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    fn start(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let _ = sender.send(load(&path).map(Document::parse));
        });
        self.pending = Some(receiver);
    }

    /// 終わった再読込の結果。読込中、または読み直しを始めた場合は `None`。
    pub fn poll(&mut self) -> Option<io::Result<Document>> {
        let result = match self.pending.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(io::Error::other("読込処理が異常終了しました")),
        };
        self.pending = None;
        if std::mem::take(&mut self.again) {
            self.start();
            return None;
        }
        Some(result)
    }
}

/// 再読込の失敗理由。OSのエラーのうち、一時的な削除と権限不足は日本語にする。
pub fn reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "ファイルが見つかりません".into(),
        io::ErrorKind::PermissionDenied => "読み取り権限がありません".into(),
        _ => error.to_string(),
    }
}
