//! 数式とローカル画像の生成ジョブ。同じバイナリの隔離ワーカーで処理し、UI側で期限・世代・容量を
//! 管理する。同時実行数とキャッシュの予算は数式と画像で共通（仕様8.2・8.3節）。
use crate::{
    document::Document,
    graphics::{Capabilities, PLACEHOLDER_COLUMNS, Transport, ViewState},
    layout::anchored_row,
    math::{self, Image, MAX_TEX, OUTPUT_LIMIT, View, png_dimensions},
    picture,
    reload::{self, Stamp},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    ops::Range,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub const CACHE_LIMIT: usize = 128 * 1024 * 1024;
/// キャッシュに保持する結果の件数上限。容量は `CACHE_LIMIT` でも制限する。
const CACHE_ENTRIES: usize = 4096;
/// 同時に実行するワーカーの上限。
const MAX_JOBS: usize = 2;
/// ワーカー1件の実行時間の上限。超えたらkillして回収する。
const JOB_TIMEOUT: Duration = Duration::from_secs(2);
/// ワーカーとして起動する環境変数。
pub const WORKER_ENV: &str = "FUMI_INTERNAL_WORKER";
/// ワーカーが返す失敗理由の上限。
const REASON_LIMIT: u64 = 256;

/// 生成要求。結果のキャッシュの鍵にもなる。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Math(math::Request),
    Picture(picture::Request),
}
impl Key {
    fn cell(&self) -> (u16, u16) {
        match self {
            Self::Math(request) => request.cell,
            Self::Picture(request) => request.cell,
        }
    }
    fn render(&self) -> Result<Image, String> {
        match self {
            Self::Math(request) => math::render(request),
            Self::Picture(request) => picture::render(request),
        }
    }
}

/// 非公開の子プロセス経路。TTYや外部レンダラーを必要としない。
pub fn worker() -> u8 {
    // メモリ使用量はワーカー単位でも制限する（親のTUIには影響しない）。
    let limit = libc::rlimit {
        rlim_cur: 512 * 1024 * 1024,
        rlim_max: 512 * 1024 * 1024,
    };
    unsafe {
        libc::setrlimit(libc::RLIMIT_DATA, &limit);
    }
    let result = (|| -> Result<(), String> {
        let mut input = Vec::new();
        io::stdin()
            .take((MAX_TEX * 8) as u64)
            .read_to_end(&mut input)
            .map_err(|_| "入力失敗")?;
        let key: Key = serde_json::from_slice(&input).map_err(|_| "入力形式")?;
        let image = key.render()?;
        io::stdout()
            .write_all(&image.png)
            .map_err(|_| "出力失敗".into())
    })();
    if let Err(message) = result {
        let _ = write!(io::stderr(), "{message}");
        1
    } else {
        0
    }
}

struct Job {
    key: Key,
    child: Child,
    bytes: Vec<u8>,
    start: Instant,
}
impl Job {
    /// 出力を読み進める。終了・失敗・期限切れなら、その結果を返す。
    fn poll(&mut self) -> Option<View> {
        let mut failed = false;
        if let Some(output) = &mut self.child.stdout {
            let mut chunk = [0; 16384];
            loop {
                match output.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        self.bytes.extend_from_slice(&chunk[..count]);
                        if self.bytes.len() > OUTPUT_LIMIT {
                            failed = true;
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
        }
        let status = self.child.try_wait();
        let expired = self.start.elapsed() >= JOB_TIMEOUT;
        if !failed && !expired && matches!(status, Ok(None)) {
            return None;
        }
        if matches!(status, Ok(Some(_)))
            && let Some(output) = &mut self.child.stdout
        {
            let _ = output
                .take(OUTPUT_LIMIT.saturating_sub(self.bytes.len()) as u64)
                .read_to_end(&mut self.bytes);
        }
        let cell = self.key.cell();
        Some(if expired {
            View::Failed(format!("{}秒タイムアウト", JOB_TIMEOUT.as_secs()))
        } else if matches!(status, Ok(Some(s)) if s.success())
            && let Some((width, height)) = png_dimensions(&self.bytes)
        {
            View::Ready(Arc::new(Image {
                columns: width as usize / usize::from(cell.0),
                rows: height as usize / usize::from(cell.1),
                width,
                height,
                png: std::mem::take(&mut self.bytes),
            }))
        } else {
            View::Failed(self.reason())
        })
    }
    /// 失敗の理由。画像はワーカーの理由を使い、数式は従来どおりまとめて示す。
    fn reason(&mut self) -> String {
        let generic = match self.key {
            Key::Math(_) => return "構文・未対応コマンド・生成失敗".into(),
            Key::Picture(_) => "画像の読込・デコード失敗",
        };
        let mut message = String::new();
        let read = self.child.stderr.take().is_some_and(|stderr| {
            stderr
                .take(REASON_LIMIT)
                .read_to_string(&mut message)
                .is_ok()
        });
        // 理由はワーカーが返す固定の文言。想定外の内容は端末へ出さない。
        if read && !message.is_empty() && !message.chars().any(char::is_control) {
            message
        } else {
            generic.into()
        }
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 実行中のワーカー。同時実行数を `MAX_JOBS` までに抑える。
#[derive(Default)]
struct Workers {
    jobs: Vec<Job>,
}
impl Workers {
    /// 終わったジョブを回収し、その要求と結果を返す。
    fn collect(&mut self) -> Vec<(Key, View)> {
        let mut done = Vec::new();
        for i in (0..self.jobs.len()).rev() {
            if let Some(view) = self.jobs[i].poll() {
                let job = self.jobs.remove(i);
                done.push((job.key.clone(), view));
            }
        }
        done
    }
    fn is_running(&self, key: &Key) -> bool {
        self.jobs.iter().any(|job| &job.key == key)
    }
    fn is_full(&self) -> bool {
        self.jobs.len() >= MAX_JOBS
    }
    fn start(&mut self, key: Key) -> io::Result<()> {
        self.jobs.push(start_job(key)?);
        Ok(())
    }
    /// `keep` が偽の要求のジョブを止めて回収する。
    fn retain(&mut self, mut keep: impl FnMut(&Key) -> bool) {
        self.jobs.retain(|job| keep(&job.key));
    }
}

struct Entry {
    view: View,
    used: u64,
}

/// 生成結果のLRUキャッシュ。件数と、PNGとデコード相当量の合計で古いものから捨てる。
#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    clock: u64,
    cost: usize,
}
impl Cache {
    fn cost(view: &View) -> usize {
        match view {
            View::Ready(image) => image.cost(),
            View::Failed(_) => 0,
        }
    }
    fn get(&self, key: &Key) -> Option<&View> {
        self.entries.get(key).map(|entry| &entry.view)
    }
    /// 結果があれば最近使ったものとして扱う。
    fn touch(&mut self, key: &Key) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        self.clock += 1;
        entry.used = self.clock;
        true
    }
    fn insert(&mut self, key: Key, view: View) {
        self.clock += 1;
        self.cost += Self::cost(&view);
        let entry = Entry {
            view,
            used: self.clock,
        };
        if let Some(previous) = self.entries.insert(key, entry) {
            self.cost -= Self::cost(&previous.view);
        }
        while self.entries.len() > CACHE_ENTRIES || self.cost > CACHE_LIMIT {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.cost -= Self::cost(&entry.view);
            }
        }
    }
}

/// 画像にする要素。生成要求は本文幅と端末の能力から作る。
enum Target {
    Math(String),
    Picture { path: PathBuf, stamp: Option<Stamp> },
}

pub struct Manager {
    /// `targets` を作った文書の世代。
    generation: u64,
    /// 相対参照の画像の基準ディレクトリ。
    base: PathBuf,
    /// 画像にする要素のソース範囲。文書の順に並ぶ。
    targets: Vec<(Range<usize>, Target)>,
    /// 各要素の現在の本文幅での生成要求。生成できなければその理由。
    keys: Vec<Result<Key, String>>,
    width: Option<usize>,
    workers: Workers,
    cache: Cache,
    /// 前回返した結果から変わり得るか。
    changed: bool,
    pub capabilities: Capabilities,
}
impl Manager {
    pub fn new(
        document: &Document,
        base: &Path,
        generation: u64,
        capabilities: Capabilities,
    ) -> Self {
        let mut manager = Self {
            generation,
            base: base.into(),
            targets: Vec::new(),
            keys: Vec::new(),
            width: None,
            workers: Workers::default(),
            cache: Cache::default(),
            changed: true,
            capabilities,
        };
        manager.set_targets(document);
        manager
    }
    /// 文書の数式と画像を集める。画像はこの時点のファイルの状態を要求に含め、
    /// 文書を読み直すと書き換わった画像も読み直す。
    fn set_targets(&mut self, document: &Document) {
        let mut targets: Vec<_> = document
            .display_math()
            .map(|(node, tex)| (node.source.clone(), Target::Math(tex.into())))
            .collect();
        for (source, path) in document.local_images() {
            let path = picture::resolve(&self.base, &path);
            let stamp = reload::stamp(&path);
            targets.push((source, Target::Picture { path, stamp }));
        }
        targets.sort_by_key(|(source, _)| source.start);
        self.targets = targets;
        self.update_keys();
    }
    fn update_keys(&mut self) {
        let width = self.request_width(self.width.unwrap_or(0));
        self.keys = self
            .targets
            .iter()
            .map(|(_, target)| self.key(target, width))
            .collect();
    }
    /// 差し替えた文書の数式・画像へ切り替える（仕様3.1節）。新しい文書で使わない要求のジョブは
    /// 止め、生成要求が同じ結果はキャッシュから再利用する。結果は `generation` の表示状態にだけ返す。
    pub fn replace_document(&mut self, document: &Document, generation: u64) {
        self.generation = generation;
        self.set_targets(document);
        let keys = &self.keys;
        self.workers
            .retain(|key| keys.iter().any(|wanted| wanted.as_ref() == Ok(key)));
        self.changed = true;
    }
    fn request_width(&self, width: usize) -> usize {
        if self.capabilities.transport == Transport::Tmux {
            width.min(PLACEHOLDER_COLUMNS)
        } else {
            width
        }
    }
    fn key(&self, target: &Target, width: usize) -> Result<Key, String> {
        let cell = self.capabilities.cell;
        match target {
            Target::Math(tex) => {
                if tex.len() > MAX_TEX {
                    return Err("16 KiB上限".into());
                }
                // 画像は文字色がなくても表示できるため、数式だけを文字色不明として戻す。
                let foreground = self.capabilities.foreground.ok_or("文字色不明")?;
                Ok(Key::Math(math::Request {
                    tex: tex.clone(),
                    width,
                    cell,
                    foreground,
                    monochrome: self.capabilities.monochrome,
                }))
            }
            Target::Picture { path, stamp } => Ok(Key::Picture(picture::Request {
                path: path.clone(),
                stamp: *stamp,
                columns: width,
                cell,
            })),
        }
    }
    /// 本文幅が変わったら、各要素の要求を新しい幅へ合わせる。
    fn set_width(&mut self, width: usize) {
        if self.width == Some(width) {
            return;
        }
        self.width = Some(width);
        self.update_keys();
        self.changed = true;
    }
    /// 画面と前後1画面にある要素の番号。画面内を上から、続いて画面に近い順に並べる。
    fn wanted(&self, view: &ViewState) -> Vec<usize> {
        if !view.body_visible {
            return Vec::new();
        }
        let (top, rows, lines) = (view.top, view.rows, view.lines);
        let range = top.saturating_sub(rows)..(top + rows * 2).min(lines.len());
        let mut targets: Vec<_> =
            self.targets
                .iter()
                .enumerate()
                .filter_map(|(index, (source, _))| {
                    if !lines[range.clone()].iter().any(|line| {
                        line.source.start < source.end && source.start <= line.source.end
                    }) {
                        return None;
                    }
                    let row = anchored_row(lines, source.start);
                    range.contains(&row).then(|| {
                        (
                            index,
                            if row >= top && row < top + rows {
                                (0, row - top)
                            } else {
                                (1, row.abs_diff(top))
                            },
                        )
                    })
                })
                .collect();
        targets.sort_by_key(|(_, distance)| *distance);
        targets.into_iter().map(|(index, _)| index).collect()
    }
    /// 表示状態に合わせてジョブを進める。結果が変わり得るときだけ、要素ごとの結果を返す。
    pub fn update(&mut self, view: &ViewState) -> Option<HashMap<usize, View>> {
        // 要素の位置は文書ごとに違うため、別の世代の表示行へ結果を返さない。
        if view.generation != self.generation {
            return None;
        }
        self.set_width(view.width);
        // 実行中ジョブは位置に関わらず最大2秒で終える。古い幅の結果はキャッシュだけへ。
        for (key, view) in self.workers.collect() {
            self.cache.insert(key, view);
            self.changed = true;
        }
        for index in self.wanted(view) {
            let Ok(key) = &self.keys[index] else {
                continue;
            };
            if self.cache.touch(key) || self.workers.is_running(key) || self.workers.is_full() {
                continue;
            }
            let key = key.clone();
            if self.workers.start(key.clone()).is_err() {
                self.cache
                    .insert(key, View::Failed("レンダラー起動失敗".into()));
                self.changed = true;
            }
        }
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        Some(
            self.targets
                .iter()
                .zip(&self.keys)
                .filter_map(|((source, _), key)| match key {
                    Ok(key) => self.cache.get(key).map(|view| (source.start, view.clone())),
                    Err(reason) => Some((source.start, View::Failed(reason.clone()))),
                })
                .collect(),
        )
    }
    pub fn active_jobs(&self) -> usize {
        self.workers.jobs.len()
    }
}

fn set_nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn start_job(key: Key) -> io::Result<Job> {
    let mut child = Command::new(std::env::current_exe()?)
        .env(WORKER_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // 16 KiBのTeXでもJSONエスケープ後はpipe容量を超え得るので送信は補助スレッド。
    let mut input = child.stdin.take().unwrap();
    let bytes = serde_json::to_vec(&key)?;
    std::thread::spawn(move || {
        let _ = input.write_all(&bytes);
    });
    if let Err(error) = set_nonblocking(child.stdout.as_ref().unwrap()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(Job {
        key,
        child,
        bytes: Vec::new(),
        start: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    /// 数式の生成要求。
    fn math_key(manager: &Manager, tex: &str, width: usize) -> Key {
        manager.key(&Target::Math(tex.into()), width).unwrap()
    }
    fn capabilities() -> Capabilities {
        Capabilities {
            transport: Transport::Direct,
            cell: (10, 20),
            foreground: Some([255; 3]),
            monochrome: false,
        }
    }
    #[test]
    fn cache_evicts_oldest_images_under_the_shared_budget() {
        let document = Document::parse(String::new());
        let mut manager = Manager::new(&document, Path::new("."), 0, capabilities());
        for i in 0..3 {
            let key = math_key(&manager, &format!("x_{i}"), 78);
            manager.cache.insert(
                key,
                View::Ready(Arc::new(Image {
                    png: vec![0],
                    width: 8192,
                    height: 2048,
                    columns: 1,
                    rows: 1,
                })),
            );
        }
        assert_eq!(manager.cache.entries.len(), 1);
        assert!(
            matches!(manager.cache.entries.keys().next(), Some(Key::Math(r)) if r.tex == "x_2")
        );
        let total: usize = manager
            .cache
            .entries
            .values()
            .map(|entry| Cache::cost(&entry.view))
            .sum();
        assert_eq!(manager.cache.cost, total);
        assert!(total <= CACHE_LIMIT);
    }
    #[test]
    fn update_returns_results_only_when_they_may_have_changed() {
        let mut app = App::new(Document::parse("$$ x $$".into()), "update.md", false);
        app.resize(80, 24);
        let mut manager = Manager::new(
            app.document(),
            Path::new("."),
            app.generation(),
            capabilities(),
        );
        // ワーカーを起動しないよう、使う幅の結果を先に入れておく。
        for (width, reason) in [(78, "wide"), (58, "narrow")] {
            let key = math_key(&manager, "x", width);
            manager.cache.insert(key, View::Failed(reason.into()));
        }
        let failed = |views: Option<HashMap<usize, View>>| match views.unwrap().get(&0) {
            Some(View::Failed(reason)) => reason.clone(),
            _ => panic!("結果がない"),
        };
        assert_eq!(failed(manager.update(&app.view_state())), "wide");
        assert!(manager.update(&app.view_state()).is_none());
        app.resize(60, 24);
        assert_eq!(failed(manager.update(&app.view_state())), "narrow");
        assert!(manager.update(&app.view_state()).is_none());
        assert_eq!(manager.active_jobs(), 0);
    }
    #[test]
    fn replacing_the_document_stops_unused_jobs_and_ignores_other_generations() {
        let mut app = App::new(Document::parse("$$ x $$".into()), "replace.md", false);
        app.resize(80, 24);
        let mut manager = Manager::new(
            app.document(),
            Path::new("."),
            app.generation(),
            capabilities(),
        );
        for tex in ["x", "y"] {
            let key = math_key(&manager, tex, 78);
            manager.cache.insert(key, View::Failed(tex.into()));
        }
        assert!(manager.update(&app.view_state()).is_some());
        let child = Command::new("/bin/sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        manager.workers.jobs.push(Job {
            key: math_key(&manager, "gone", 78),
            child,
            bytes: Vec::new(),
            start: Instant::now(),
        });
        set_nonblocking(manager.workers.jobs[0].child.stdout.as_ref().unwrap()).unwrap();
        app.replace_document(Document::parse("intro\n\n$$ x $$\n\n$$ y $$\n".into()));
        // 差し替え前の数式の位置を、新しい文書の表示行へ返さない。
        assert!(manager.update(&app.view_state()).is_none());
        manager.replace_document(app.document(), app.generation());
        assert_eq!(manager.active_jobs(), 0);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        let views = manager.update(&app.view_state()).unwrap();
        let reasons: Vec<_> = app
            .document()
            .display_math()
            .map(|(node, _)| match views.get(&node.source.start) {
                Some(View::Failed(reason)) => reason.as_str(),
                _ => panic!("キャッシュの結果がない"),
            })
            .collect();
        assert_eq!(reasons, ["x", "y"]);
    }
    #[test]
    fn timeout_kills_and_reaps_worker_without_waiting_for_completion() {
        let mut app = App::new(Document::parse("$$ x $$".into()), "timeout.md", false);
        app.resize(80, 24);
        let mut manager = Manager::new(
            app.document(),
            Path::new("."),
            app.generation(),
            capabilities(),
        );
        let key = math_key(&manager, "x", 78);
        let child = Command::new("/bin/sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        manager.workers.jobs.push(Job {
            key,
            child,
            bytes: Vec::new(),
            start: Instant::now() - Duration::from_secs(3),
        });
        // 実際のジョブと同じ非ブロッキングstdoutを使用する。
        set_nonblocking(manager.workers.jobs[0].child.stdout.as_ref().unwrap()).unwrap();
        let start = Instant::now();
        let views = manager.update(&app.view_state()).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(manager.active_jobs(), 0);
        assert!(
            matches!(views.get(&0),Some(View::Failed(message)) if message.contains("タイムアウト"))
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[test]
    fn pictures_resolve_from_the_base_and_follow_file_changes_on_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.png");
        std::fs::write(&path, b"one").unwrap();
        let source = "![a](a.png)\n\n$$ x $$\n";
        let mut app = App::new(Document::parse(source.into()), "p.md", false);
        app.resize(80, 24);
        let mut capabilities = capabilities();
        capabilities.foreground = None;
        let mut manager = Manager::new(
            app.document(),
            directory.path(),
            app.generation(),
            capabilities,
        );
        manager.set_width(400);
        let Ok(Key::Picture(first)) = manager.keys[0].clone() else {
            panic!("画像の要求がない");
        };
        assert_eq!(first.path, path);
        assert_eq!(first.columns, 400);
        // 画像は文字色がなくても表示でき、数式だけを文字色不明として戻す。
        assert_eq!(manager.keys[1], Err("文字色不明".into()));
        // ワーカーを起動しないよう、画像の結果を先に入れておく。
        manager.set_width(78);
        let key = manager.keys[0].clone().unwrap();
        manager.cache.insert(key, View::Failed("cached".into()));
        let views = manager.update(&app.view_state()).unwrap();
        assert!(matches!(views.get(&0), Some(View::Failed(reason)) if reason == "cached"));
        let math = source.find("$$").unwrap();
        assert!(matches!(views.get(&math), Some(View::Failed(reason)) if reason == "文字色不明"));
        assert_eq!(manager.active_jobs(), 0);
        // 文書を読み直すと、書き換わった画像は別の要求になる。
        std::fs::write(&path, b"two!").unwrap();
        app.replace_document(Document::parse(source.into()));
        manager.replace_document(app.document(), app.generation());
        let Ok(Key::Picture(second)) = manager.keys[0].clone() else {
            panic!("画像の要求がない");
        };
        assert_ne!(first.stamp, second.stamp);
    }
    #[test]
    fn pictures_in_tmux_fit_the_placeholder_columns() {
        let document = Document::parse("![a](a.png)\n".into());
        let mut capabilities = capabilities();
        capabilities.transport = Transport::Tmux;
        let mut manager = Manager::new(&document, Path::new("docs"), 0, capabilities);
        manager.set_width(400);
        let Ok(Key::Picture(request)) = &manager.keys[0] else {
            panic!("画像の要求がない");
        };
        assert_eq!(request.columns, PLACEHOLDER_COLUMNS);
        assert_eq!(request.path, Path::new("docs/a.png"));
    }
}
