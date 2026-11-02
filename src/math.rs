//! ブロック数式の画像化。処理は同じバイナリの隔離ワーカーで行う（`jobs` を参照）。
use serde::{Deserialize, Serialize};
use std::{io, sync::Arc};

pub const MAX_TEX: usize = 16 * 1024;
/// ワーカーが返す画像の上限。
pub(crate) const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

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
    pub(crate) fn cost(&self) -> usize {
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
pub(crate) fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
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
    let mut rgba = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    for pixel in raw[..info.buffer_size()].chunks_exact(channels) {
        if request.monochrome {
            // 背景は透明なので、透明度だけを残して全体を文字色で描く。
            // `\color`で暗い色を指定した部分も、本文と同じ明るさで読める。
            rgba.extend_from_slice(&request.foreground);
        } else {
            rgba.extend_from_slice(&pixel[..3]);
        }
        rgba.push(if channels == 4 { pixel[3] } else { 255 });
    }
    cell_image(&rgba, info.width, info.height, request.cell)
}

/// RGBAの画素をセル境界まで透明で埋め、PNGにする。数式とローカル画像で共通。
/// 部分矩形の転送でも縦横比とセル座標を一致させるため。
pub(crate) fn cell_image(
    rgba: &[u8],
    width: u32,
    height: u32,
    cell: (u16, u16),
) -> Result<Image, String> {
    let columns = (width as usize).div_ceil(usize::from(cell.0));
    let rows = (height as usize).div_ceil(usize::from(cell.1));
    let padded_width = columns as u32 * u32::from(cell.0);
    let padded_height = rows as u32 * u32::from(cell.1);
    if padded_width as usize * padded_height as usize * 4 > OUTPUT_LIMIT {
        return Err("画像容量上限".into());
    }
    let mut padded = vec![0; padded_width as usize * padded_height as usize * 4];
    let row = width as usize * 4;
    for y in 0..height as usize {
        let at = y * padded_width as usize * 4;
        padded[at..at + row].copy_from_slice(&rgba[y * row..(y + 1) * row]);
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
