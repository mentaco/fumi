//! Markdownのローカル画像（仕様8.3節）。参照先の解決と、ワーカーで行う読込・デコード・縮小。
use crate::{
    math::{Image, cell_image},
    reload::Stamp,
    source::bounded_read,
};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::png::PngDecoder,
    imageops::FilterType, metadata::Orientation,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Cursor},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

/// 1画像のファイルの上限。
pub const MAX_FILE: u64 = 20 * 1024 * 1024;
/// デコード後の画素数の上限。
pub const MAX_PIXELS: u64 = 25_000_000;
/// 表示の高さの上限（セル行）。数式と同じ。
pub const MAX_ROWS: usize = 12;
/// デコードで確保するメモリの上限。ワーカーのデータ上限（512 MiB）の内側に収める。
const MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// 画像の生成要求。ファイルの状態を含め、書き換わった画像を別の要求として扱う。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Request {
    pub path: PathBuf,
    pub stamp: Option<Stamp>,
    /// 表示に使える幅（セル）。
    pub columns: usize,
    pub cell: (u16, u16),
}

/// 参照先をローカルのパスにする。基準ディレクトリとの結合は呼び出し側で行う。
/// スキーム・authority・query・fragment・NULを含む参照は表示対象外。`~` は展開しない。
pub fn local_path(destination: &str) -> Option<PathBuf> {
    if destination.is_empty()
        || destination.contains(['\0', '?', '#'])
        || destination.starts_with("//")
        || has_scheme(destination)
    {
        return None;
    }
    let decoded = percent_decode(destination)?;
    if decoded.is_empty() || decoded.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&decoded)))
}

/// `http:` 等のスキームで始まるか（RFC 3986の`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`）。
fn has_scheme(text: &str) -> bool {
    let Some((scheme, _)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `%XX` をバイトへ戻す。不正な `%` があれば `None`。
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            result.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    Some(result)
}

/// 参照先のパスを、文書の基準ディレクトリから解決する。絶対パスはそのまま使う。
pub fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.into()
    } else {
        base.join(path)
    }
}

fn read_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "ファイルが見つかりません".into(),
        io::ErrorKind::PermissionDenied => "読み取り権限がありません".into(),
        _ => "読込失敗".into(),
    }
}

/// ワーカーで画像を読み、表示サイズのPNGにする。処理は「読込 → 形式判定 → 寸法確認 →
/// デコード → 向きの反映 → 縮小 → PNG化」の順。寸法を確かめてから画素の領域を確保する。
pub fn render(request: &Request) -> Result<Image, String> {
    let (cell_width, cell_height) = request.cell;
    if cell_width == 0 || cell_height == 0 || request.columns == 0 {
        return Err("寸法不明".into());
    }
    // FIFOを開く段階で停止しない。開いたFDの属性で通常ファイルか確かめる。
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&request.path)
        .map_err(|e| read_error(&e))?;
    let meta = file.metadata().map_err(|e| read_error(&e))?;
    if !meta.is_file() {
        return Err("通常ファイルではありません".into());
    }
    if meta.len() > MAX_FILE {
        return Err("20 MiB上限".into());
    }
    let bytes = bounded_read(file, MAX_FILE).map_err(|_| "20 MiB上限")?;
    let reader = || {
        ImageReader::new(Cursor::new(&bytes[..]))
            .with_guessed_format()
            .map_err(|_| "読込失敗".to_string())
    };
    match reader()?.format() {
        Some(ImageFormat::Png) => {
            // APNGは最初のフレームだけを静止画として出さず、代替テキストのままにする。
            let png = PngDecoder::new(Cursor::new(&bytes[..])).map_err(|_| "画像が壊れています")?;
            if png.is_apng().map_err(|_| "画像が壊れています")? {
                return Err("アニメーション未対応".into());
            }
        }
        Some(ImageFormat::Jpeg) => {}
        _ => return Err("形式未対応".into()),
    }
    let (width, height) = reader()?
        .into_dimensions()
        .map_err(|_| "画像が壊れています")?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("25メガピクセル上限".into());
    }
    let mut reader = reader()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "画像が壊れています")?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "画像が壊れています")?;
    image.apply_orientation(orientation);
    let image = fit(
        image,
        request.columns * usize::from(cell_width),
        MAX_ROWS * usize::from(cell_height),
    );
    let rgba = image.to_rgba8();
    cell_image(rgba.as_raw(), rgba.width(), rgba.height(), request.cell)
}

/// 縦横比を保って `max_width`×`max_height` に収める。元の大きさより拡大しない。
fn fit(image: DynamicImage, max_width: usize, max_height: usize) -> DynamicImage {
    let (width, height) = (image.width() as f64, image.height() as f64);
    let scale = (max_width as f64 / width).min(max_height as f64 / height);
    if scale >= 1.0 {
        return image;
    }
    let target = |size: f64| ((size * scale).round() as u32).max(1);
    image.resize_exact(target(width), target(height), FilterType::Triangle)
}
