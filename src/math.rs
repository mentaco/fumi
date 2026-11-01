//! 数式は同じバイナリの隔離ワーカーで処理し、UI側で期限・世代・容量を管理する。
use crate::{
    document::Document,
    graphics::{Capabilities, PLACEHOLDER_COLUMNS, Transport, ViewState},
    layout::anchored_row,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    ops::Range,
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_TEX: usize = 16 * 1024;
pub const CACHE_LIMIT: usize = 128 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
/// キャッシュに保持する結果の件数上限。容量は `CACHE_LIMIT` でも制限する。
const CACHE_ENTRIES: usize = 4096;
/// 同時に実行するワーカーの上限。
const MAX_JOBS: usize = 2;
/// ワーカー1件の実行時間の上限。超えたらkillして回収する。
const JOB_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Request {
    pub tex: String,
    pub width: usize,
    pub cell: (u16, u16),
    pub foreground: [u8; 3],
    pub monochrome: bool,
}
#[derive(Debug)]
pub struct Image {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub columns: usize,
    pub rows: usize,
}
impl Image {
    fn cost(&self) -> usize {
        self.png.len() + self.width as usize * self.height as usize * 4
    }
}
#[derive(Clone, Debug)]
pub enum View {
    Ready(Arc<Image>),
    Failed(String),
}

/// 個別の結果がまだない数式の扱い。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// ラベルを付けない。
    #[default]
    Idle,
    /// 画像生成を待っている。
    Pending,
    /// 画像を使えない理由。
    Unavailable(String),
}
impl Status {
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Pending => Some("生成中"),
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// PNG署名に続くIHDRから幅と高さを読む。
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// 入力からパスやコマンドを取り出さない。マクロ状態は式ごとに新規パーサで初期化。
pub fn render(request: &Request) -> Result<Image, String> {
    use ratex_layout::{LayoutOptions, layout, to_display_list, validate_layout};
    use ratex_types::{color::Color, math_style::MathStyle};
    if request.tex.len() > MAX_TEX {
        return Err("16 KiB上限".into());
    }
    if request.cell.0 == 0 || request.cell.1 == 0 || request.width == 0 || request.width > 8192 {
        return Err("寸法不明".into());
    }
    // ファイル参照・マクロ定義・URL関連の拡張は受理しない。未知コマンドもパーサのエラーになる。
    for command in [
        "input",
        "include",
        "includegraphics",
        "write",
        "openout",
        "read",
        "href",
        "url",
        "html",
        "def",
        "gdef",
        "edef",
        "newcommand",
        "renewcommand",
        "global",
    ] {
        if request.tex.contains(&format!("\\{command}")) {
            return Err("未対応コマンド".into());
        }
    }
    let color = |rgb: [u8; 3]| {
        Color::parse(&format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])).unwrap()
    };
    let ast = ratex_parser::parser::parse(&request.tex).map_err(|_| "TeX構文または未知コマンド")?;
    let options = LayoutOptions::default()
        .with_style(MathStyle::Display)
        .with_color(color(request.foreground));
    let box_ = layout(&ast, &options);
    validate_layout(&box_).map_err(|_| "数式レイアウト失敗")?;
    let list = to_display_list(&box_);
    let mut font_size = f32::from(request.cell.1);
    let padding = 2.0;
    let natural_width = list.width as f32 * font_size + padding * 2.0;
    let limit = (request.width * usize::from(request.cell.0)).min(16384) as f32;
    if natural_width > limit {
        let scale = (limit - padding * 2.0) / (natural_width - padding * 2.0);
        if scale < 0.5 {
            return Err("幅超過".into());
        }
        font_size *= scale;
    }
    let height = (list.height + list.depth) as f32 * font_size + padding * 2.0;
    if !height.is_finite() || height > f32::from(request.cell.1) * 12.0 {
        return Err("12行上限".into());
    }
    let png = ratex_render::render_to_png(
        &list,
        &ratex_render::RenderOptions {
            font_size,
            padding,
            // 背景は透明にし、端末の背景との合成は端末側に任せる。
            background_color: Color::new(0.0, 0.0, 0.0, 0.0),
            ..Default::default()
        },
    )
    .map_err(|_| "画像生成失敗")?;
    let (width, height) = png_dimensions(&png).ok_or("画像生成失敗")?;
    let columns = (width as usize).div_ceil(usize::from(request.cell.0));
    let rows = (height as usize).div_ceil(usize::from(request.cell.1));
    // セル境界へ余白を付け、部分矩形の転送でも縦横比とセル座標を一致させる。
    let padded_width = columns as u32 * u32::from(request.cell.0);
    let padded_height = rows as u32 * u32::from(request.cell.1);
    if padded_width as usize * padded_height as usize * 4 > OUTPUT_LIMIT {
        return Err("画像容量上限".into());
    }
    let mut decoder = png::Decoder::new(io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| "PNG読込失敗")?;
    let mut raw = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut raw).map_err(|_| "PNG読込失敗")?;
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => return Err("PNG形式".into()),
    };
    let mut padded = Vec::with_capacity(padded_width as usize * padded_height as usize * 4);
    for y in 0..padded_height {
        for x in 0..padded_width {
            if x < width && y < height {
                let at = (y as usize * width as usize + x as usize) * channels;
                if request.monochrome {
                    // 背景は透明なので、透明度だけを残して全体を文字色で描く。
                    // `\color`で暗い色を指定した部分も、本文と同じ明るさで読める。
                    padded.extend_from_slice(&request.foreground);
                } else {
                    padded.extend_from_slice(&raw[at..at + 3]);
                }
                padded.push(if channels == 4 { raw[at + 3] } else { 255 });
            } else {
                padded.extend_from_slice(&[0; 4]);
            }
        }
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, padded_width, padded_height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .map_err(|_| "PNG出力失敗")?
            .write_image_data(&padded)
            .map_err(|_| "PNG出力失敗")?;
    }
    Ok(Image {
        png: output,
        width: padded_width,
        height: padded_height,
        columns,
        rows,
    })
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
        let request: Request = serde_json::from_slice(&input).map_err(|_| "入力形式")?;
        let image = render(&request)?;
        io::stdout()
            .write_all(&image.png)
            .map_err(|_| "出力失敗".into())
    })();
    if let Err(message) = result {
        let _ = writeln!(io::stderr(), "{message}");
        1
    } else {
        0
    }
}

struct Job {
    key: Request,
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
        Some(if expired {
            View::Failed(format!("{}秒タイムアウト", JOB_TIMEOUT.as_secs()))
        } else if matches!(status, Ok(Some(s)) if s.success())
            && let Some((width, height)) = png_dimensions(&self.bytes)
        {
            View::Ready(Arc::new(Image {
                columns: width as usize / usize::from(self.key.cell.0),
                rows: height as usize / usize::from(self.key.cell.1),
                width,
                height,
                png: std::mem::take(&mut self.bytes),
            }))
        } else {
            View::Failed("構文・未対応コマンド・生成失敗".into())
        })
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
    fn collect(&mut self) -> Vec<(Request, View)> {
        let mut done = Vec::new();
        for i in (0..self.jobs.len()).rev() {
            if let Some(view) = self.jobs[i].poll() {
                let job = self.jobs.remove(i);
                done.push((job.key.clone(), view));
            }
        }
        done
    }
    fn is_running(&self, key: &Request) -> bool {
        self.jobs.iter().any(|job| &job.key == key)
    }
    fn is_full(&self) -> bool {
        self.jobs.len() >= MAX_JOBS
    }
    fn start(&mut self, key: Request) -> io::Result<()> {
        self.jobs.push(start_job(key)?);
        Ok(())
    }
}

struct Entry {
    view: View,
    used: u64,
}

/// 生成結果のLRUキャッシュ。件数と、PNGとデコード相当量の合計で古いものから捨てる。
#[derive(Default)]
struct Cache {
    entries: HashMap<Request, Entry>,
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
    fn get(&self, key: &Request) -> Option<&View> {
        self.entries.get(key).map(|entry| &entry.view)
    }
    /// 結果があれば最近使ったものとして扱う。
    fn touch(&mut self, key: &Request) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        self.clock += 1;
        entry.used = self.clock;
        true
    }
    fn insert(&mut self, key: Request, view: View) {
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

pub struct Manager {
    /// ブロック数式のソース範囲と、現在の本文幅での生成要求。
    formulas: Vec<(Range<usize>, Request)>,
    width: Option<usize>,
    workers: Workers,
    cache: Cache,
    /// 前回返した結果から変わり得るか。
    changed: bool,
    pub capabilities: Capabilities,
}
impl Manager {
    pub fn new(document: &Document, capabilities: Capabilities) -> Self {
        let mut manager = Self {
            formulas: Vec::new(),
            width: None,
            workers: Workers::default(),
            cache: Cache::default(),
            changed: true,
            capabilities,
        };
        manager.formulas = document
            .display_math()
            .map(|(node, tex)| (node.source.clone(), manager.key(tex, 0)))
            .collect();
        manager
    }
    fn request_width(&self, width: usize) -> usize {
        if self.capabilities.transport == Transport::Tmux {
            width.min(PLACEHOLDER_COLUMNS)
        } else {
            width
        }
    }
    fn key(&self, tex: &str, width: usize) -> Request {
        Request {
            tex: tex.into(),
            width: self.request_width(width),
            cell: self.capabilities.cell,
            foreground: self.capabilities.foreground,
            monochrome: self.capabilities.monochrome,
        }
    }
    /// 本文幅が変わったら、各数式の要求を新しい幅へ合わせる。
    fn set_width(&mut self, width: usize) {
        if self.width == Some(width) {
            return;
        }
        self.width = Some(width);
        let width = self.request_width(width);
        for (_, key) in &mut self.formulas {
            key.width = width;
        }
        self.changed = true;
    }
    /// 画面と前後1画面にある数式の番号。画面内を上から、続いて画面に近い順に並べる。
    fn wanted(&self, view: &ViewState) -> Vec<usize> {
        if !view.body_visible {
            return Vec::new();
        }
        let (top, rows, lines) = (view.top, view.rows, view.lines);
        let range = top.saturating_sub(rows)..(top + rows * 2).min(lines.len());
        let mut formulas: Vec<_> =
            self.formulas
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
        formulas.sort_by_key(|(_, distance)| *distance);
        formulas.into_iter().map(|(index, _)| index).collect()
    }
    /// 表示状態に合わせてジョブを進める。結果が変わり得るときだけ、数式ごとの結果を返す。
    pub fn update(&mut self, view: &ViewState) -> Option<HashMap<usize, View>> {
        self.set_width(view.width);
        // 実行中ジョブは位置に関わらず最大2秒で終える。古い幅の結果はキャッシュだけへ。
        for (key, view) in self.workers.collect() {
            self.cache.insert(key, view);
            self.changed = true;
        }
        for index in self.wanted(view) {
            let key = &self.formulas[index].1;
            if self.cache.touch(key) || self.workers.is_running(key) || self.workers.is_full() {
                continue;
            }
            let key = key.clone();
            let view = if key.tex.len() > MAX_TEX {
                View::Failed("16 KiB上限".into())
            } else {
                match self.workers.start(key.clone()) {
                    Ok(()) => continue,
                    Err(_) => View::Failed("レンダラー起動失敗".into()),
                }
            };
            self.cache.insert(key, view);
            self.changed = true;
        }
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        Some(
            self.formulas
                .iter()
                .filter_map(|(source, key)| {
                    self.cache.get(key).map(|view| (source.start, view.clone()))
                })
                .collect(),
        )
    }
    pub fn active_jobs(&self) -> usize {
        self.workers.jobs.len()
    }
}
/// ワーカーの出力を読むときに描画ループを止めないよう、パイプを非ブロッキングにする。
fn set_nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn start_job(key: Request) -> io::Result<Job> {
    let mut child = Command::new(std::env::current_exe()?)
        .env("FUMI_INTERNAL_MATH_WORKER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    fn capabilities() -> Capabilities {
        Capabilities {
            transport: Transport::Direct,
            cell: (10, 20),
            foreground: [255; 3],
            monochrome: false,
        }
    }
    #[test]
    fn cache_evicts_oldest_images_under_the_shared_budget() {
        let document = Document::parse(String::new());
        let mut manager = Manager::new(&document, capabilities());
        for i in 0..3 {
            let key = manager.key(&format!("x_{i}"), 78);
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
        assert_eq!(manager.cache.entries.keys().next().unwrap().tex, "x_2");
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
        let mut manager = Manager::new(app.document(), capabilities());
        // ワーカーを起動しないよう、使う幅の結果を先に入れておく。
        for (width, reason) in [(78, "wide"), (58, "narrow")] {
            let key = manager.key("x", width);
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
    fn timeout_kills_and_reaps_worker_without_waiting_for_completion() {
        let mut app = App::new(Document::parse("$$ x $$".into()), "timeout.md", false);
        app.resize(80, 24);
        let mut manager = Manager::new(app.document(), capabilities());
        let key = manager.key("x", 78);
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
}
