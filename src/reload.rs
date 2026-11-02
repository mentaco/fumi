//! ファイル入力の再読込（仕様3.1節）。読込と解析は補助スレッドで行い、結果だけを端末ループへ渡す。
use crate::{document::Document, source::load};
use std::{
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// `--watch` でファイルの状態を確かめる間隔。
pub const WATCH_INTERVAL: Duration = Duration::from_millis(200);

/// 変更の判定に使うファイルの状態。更新時刻（ナノ秒）・サイズ・inode・デバイス。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: (i64, i64),
    size: u64,
    inode: u64,
    device: u64,
}

/// 指定されたパスの状態。シンボリックリンクは解決する。読めなければ `None`（削除等）。
pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: (meta.mtime(), meta.mtime_nsec()),
        size: meta.size(),
        inode: meta.ino(),
        device: meta.dev(),
    })
}

/// 自動再読込の状態。
struct Watch {
    /// 最後に読んだ時点の状態。読む直前に取るため、読んでいる間の変更は次の確認で見つかる。
    read: Option<Stamp>,
    /// 前回の確認で見つけた変更後の状態。次の確認でも同じなら読み直す。
    changed: Option<Option<Stamp>>,
    next_check: Instant,
}

type Loaded = (Option<Stamp>, io::Result<Document>);

pub struct Reloader {
    /// 指定されたパス。シンボリックリンクは読むたびに解決し直す。
    path: PathBuf,
    pending: Option<Receiver<Loaded>>,
    /// 読込中に再読込を求められたか。読込の開始後に書き換わった可能性があるため、
    /// 終わった読込の結果は捨てて読み直す。
    again: bool,
    watch: Option<Watch>,
}

impl Reloader {
    /// 手動再読込（`R`）だけを行う。
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            pending: None,
            again: false,
            watch: None,
        }
    }

    /// `--watch` の自動再読込も行う。`read` は初回の読込の直前に取った状態。
    pub fn watching(path: PathBuf, read: Option<Stamp>, now: Instant) -> Self {
        Self {
            watch: Some(Watch {
                read,
                changed: None,
                next_check: now + WATCH_INTERVAL,
            }),
            ..Self::new(path)
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

    /// 自動再読込で、間隔ごとにファイルの状態を確かめる。最後に読んだ時点から変わり、
    /// 次の確認でも同じ状態なら読み直す。書込み途中のファイルを読まないため（仕様3.1節）。
    /// 読み直しを始めたら真を返す。
    pub fn check(&mut self, now: Instant) -> bool {
        let Some(watch) = &mut self.watch else {
            return false;
        };
        // 読込中は、読み終えた時点の状態が決まるまで比べない。
        if now < watch.next_check || self.pending.is_some() {
            return false;
        }
        watch.next_check = now + WATCH_INTERVAL;
        let current = stamp(&self.path);
        if current == watch.read {
            watch.changed = None;
            return false;
        }
        if watch.changed != Some(current) {
            watch.changed = Some(current);
            return false;
        }
        watch.changed = None;
        self.start();
        true
    }

    fn start(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let read = stamp(&path);
            let _ = sender.send((read, load(&path).map(Document::parse)));
        });
        self.pending = Some(receiver);
    }

    /// 終わった再読込の結果。読込中、または読み直しを始めた場合は `None`。
    pub fn poll(&mut self) -> Option<io::Result<Document>> {
        let (read, result) = match self.pending.as_ref()?.try_recv() {
            Ok(loaded) => loaded,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                (None, Err(io::Error::other("読込処理が異常終了しました")))
            }
        };
        self.pending = None;
        // 失敗した読込でも状態を記録し、自動再読込は次の変化で再試行する。
        if let Some(watch) = &mut self.watch {
            watch.read = read;
            watch.changed = None;
        }
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
