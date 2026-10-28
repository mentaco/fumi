//! 数式は同じバイナリの隔離ワーカーで処理し、UI側で期限・世代・容量を管理する。
use crate::{
    app::App,
    document::{Document, Kind},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    ops::Range,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_TEX: usize = 16 * 1024;
pub const CACHE_LIMIT: usize = 128 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Request {
    pub tex: String,
    pub width: usize,
    pub cell: (u16, u16),
    pub background: [u8; 3],
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
            background_color: color(request.background),
            ..Default::default()
        },
    )
    .map_err(|_| "画像生成失敗")?;
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
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
                    let gray = ((u32::from(raw[at]) * 299
                        + u32::from(raw[at + 1]) * 587
                        + u32::from(raw[at + 2]) * 114)
                        / 1000) as u8;
                    padded.extend_from_slice(&[gray; 3]);
                } else {
                    padded.extend_from_slice(&raw[at..at + 3]);
                }
                padded.push(if channels == 4 { raw[at + 3] } else { 255 });
            } else {
                padded.extend_from_slice(&[
                    request.background[0],
                    request.background[1],
                    request.background[2],
                    255,
                ]);
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
impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Cache {
    key: Request,
    view: View,
}
pub struct Manager {
    formulas: Vec<(Range<usize>, String)>,
    jobs: Vec<Job>,
    cache: VecDeque<Cache>,
    pub capabilities: crate::graphics::Capabilities,
}
impl Manager {
    pub fn new(document: &Document, capabilities: crate::graphics::Capabilities) -> Self {
        let formulas = document
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                Kind::Math(s) if s.trim().starts_with("$$") => Some((
                    node.source.clone(),
                    s.trim()
                        .trim_start_matches("$$")
                        .trim_end_matches("$$")
                        .trim()
                        .to_string(),
                )),
                _ => None,
            })
            .collect();
        Self {
            formulas,
            jobs: Vec::new(),
            cache: VecDeque::new(),
            capabilities,
        }
    }
    fn key(&self, tex: &str, width: usize) -> Request {
        Request {
            tex: tex.into(),
            width: if self.capabilities.transport == crate::graphics::Transport::Tmux {
                width.min(crate::graphics::PLACEHOLDER_COLUMNS)
            } else {
                width
            },
            cell: self.capabilities.cell,
            background: self.capabilities.background,
            foreground: self.capabilities.foreground,
            monochrome: self.capabilities.monochrome,
        }
    }
    fn insert(&mut self, key: Request, view: View) {
        self.cache.retain(|entry| entry.key != key);
        self.cache.push_back(Cache { key, view });
        while self.cache.len() > 4096
            || self
                .cache
                .iter()
                .map(|e| match &e.view {
                    View::Ready(image) => image.cost(),
                    View::Failed(_) => 0,
                })
                .sum::<usize>()
                > CACHE_LIMIT
        {
            self.cache.pop_front();
        }
    }
    pub fn update(&mut self, app: &App) -> HashMap<usize, View> {
        let width = app.body_width();
        let v = app.viewport();
        let range = app.top.saturating_sub(v)..(app.top + v * 2).min(app.lines.len());
        let wanted: Vec<_> = if app.help || app.too_small() {
            Vec::new()
        } else {
            let mut formulas: Vec<_> = self
                .formulas
                .iter()
                .filter_map(|(source, tex)| {
                    if !app.lines[range.clone()].iter().any(|line| {
                        line.source.start < source.end && source.start <= line.source.end
                    }) {
                        return None;
                    }
                    let row = crate::layout::anchored_row(&app.lines, source.start);
                    range.contains(&row).then(|| {
                        (
                            source.start,
                            tex.clone(),
                            if row >= app.top && row < app.top + v {
                                (0, row - app.top)
                            } else {
                                (1, row.abs_diff(app.top))
                            },
                        )
                    })
                })
                .collect();
            formulas.sort_by_key(|(_, _, distance)| *distance);
            formulas
        };
        // 実行中ジョブは位置に関わらず最大2秒で終える。古い幅の結果はキャッシュだけへ。
        for i in (0..self.jobs.len()).rev() {
            let job = &mut self.jobs[i];
            let mut failed = false;
            if let Some(output) = &mut job.child.stdout {
                let mut chunk = [0; 16384];
                loop {
                    match output.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            job.bytes.extend_from_slice(&chunk[..count]);
                            if job.bytes.len() > OUTPUT_LIMIT {
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
            let status = job.child.try_wait();
            let expired = job.start.elapsed() >= Duration::from_secs(2);
            if failed || expired || !matches!(status, Ok(None)) {
                let mut job = self.jobs.remove(i);
                if matches!(status, Ok(Some(_)))
                    && let Some(output) = &mut job.child.stdout
                {
                    let _ = output
                        .take(OUTPUT_LIMIT.saturating_sub(job.bytes.len()) as u64)
                        .read_to_end(&mut job.bytes);
                }
                let view = if expired {
                    View::Failed("2秒タイムアウト".into())
                } else if matches!(status, Ok(Some(s)) if s.success())
                    && job.bytes.len() >= 24
                    && job.bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                {
                    let width = u32::from_be_bytes(job.bytes[16..20].try_into().unwrap());
                    let height = u32::from_be_bytes(job.bytes[20..24].try_into().unwrap());
                    View::Ready(Arc::new(Image {
                        columns: width as usize / usize::from(job.key.cell.0),
                        rows: height as usize / usize::from(job.key.cell.1),
                        width,
                        height,
                        png: std::mem::take(&mut job.bytes),
                    }))
                } else {
                    View::Failed("構文・未対応コマンド・生成失敗".into())
                };
                self.insert(job.key.clone(), view);
            }
        }
        for (source, tex, _) in wanted {
            let key = self.key(&tex, width);
            if let Some(index) = self.cache.iter().position(|e| e.key == key) {
                let entry = self.cache.remove(index).unwrap();
                self.cache.push_back(entry);
                continue;
            }
            if self.jobs.iter().any(|j| j.key == key) || self.jobs.len() >= 2 {
                continue;
            }
            if tex.len() > MAX_TEX {
                self.insert(key, View::Failed("16 KiB上限".into()));
                continue;
            }
            match start_job(key.clone(), source) {
                Ok(job) => self.jobs.push(job),
                Err(_) => self.insert(key, View::Failed("レンダラー起動失敗".into())),
            }
        }
        self.formulas
            .iter()
            .filter_map(|(source, tex)| {
                let key = self.key(tex, width);
                self.cache
                    .iter()
                    .find(|entry| entry.key == key)
                    .map(|entry| (source.start, entry.view.clone()))
            })
            .collect()
    }
    pub fn active_jobs(&self) -> usize {
        self.jobs.len()
    }
}
fn start_job(key: Request, _source: usize) -> io::Result<Job> {
    use std::os::fd::AsRawFd;
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
    let fd = child.stdout.as_ref().unwrap().as_raw_fd();
    if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::last_os_error());
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
    fn capabilities() -> crate::graphics::Capabilities {
        crate::graphics::Capabilities {
            transport: crate::graphics::Transport::Direct,
            cell: (10, 20),
            background: [0; 3],
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
            manager.insert(
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
        assert_eq!(manager.cache.len(), 1);
        assert_eq!(manager.cache.front().unwrap().key.tex, "x_2");
        assert!(
            manager
                .cache
                .iter()
                .map(|entry| match &entry.view {
                    View::Ready(image) => image.cost(),
                    _ => 0,
                })
                .sum::<usize>()
                <= CACHE_LIMIT
        );
    }
    #[test]
    fn timeout_kills_and_reaps_worker_without_waiting_for_completion() {
        let mut app = App::new(Document::parse("$$ x $$".into()), "timeout.md", false);
        app.resize(80, 24);
        let mut manager = Manager::new(&app.document, capabilities());
        let key = manager.key("x", 78);
        let child = Command::new("/bin/sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        manager.jobs.push(Job {
            key,
            child,
            bytes: Vec::new(),
            start: Instant::now() - Duration::from_secs(3),
        });
        // pollと同じ非ブロッキングstdoutを使用する。
        use std::os::fd::AsRawFd;
        let fd = manager.jobs[0].child.stdout.as_ref().unwrap().as_raw_fd();
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
        let start = Instant::now();
        let views = manager.update(&app);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(manager.active_jobs(), 0);
        assert!(
            matches!(views.get(&0),Some(View::Failed(message)) if message.contains("タイムアウト"))
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
