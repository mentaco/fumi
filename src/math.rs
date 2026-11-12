//! 数式の画像化。処理は同じバイナリの隔離ワーカーで行う（`jobs` を参照）。
//! ブロック数式は表示体裁で本文幅に、インライン数式は表示体裁（2行に収まらなければテキスト体裁）で
//! 本文の行と上の1行の2行に合わせる。
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
    /// インライン数式。本文の行のベースラインに合わせ、最大 `INLINE_ROWS` 行に収める。
    pub inline: bool,
}
#[derive(Debug)]
pub struct Image {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub columns: usize,
    pub rows: usize,
    /// インライン数式で、本文の行より上にはみ出す行数。ブロック数式・画像は0。
    pub above: usize,
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
    use ratex_types::{color::Color, display_item::DisplayList, math_style::MathStyle};
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
    let lay_out = |style: MathStyle| -> Result<DisplayList, String> {
        let options = LayoutOptions::default()
            .with_style(style)
            .with_color(color(request.foreground));
        let box_ = layout(&ast, &options);
        validate_layout(&box_).map_err(|_| "数式レイアウト失敗")?;
        Ok(to_display_list(&box_))
    };
    let limit = (request.width * usize::from(request.cell.0)).min(16384) as f32;
    let (list, font_size, padding, inline) = if request.inline {
        let metrics =
            |list: &DisplayList| (list.width as f32, list.height as f32, list.depth as f32);
        // 分数の分子・分母や二項係数が添字の大きさに縮まないよう、少しの縮小で2行に収まれば
        // 表示体裁で描く。収まらない式（上下に添字の付く総和等）は、添字を横に付けるテキスト
        // 体裁にし、それでも収まらなければ下限まで縮める。
        let display = lay_out(MathStyle::Display)?;
        let (list, placement) =
            match inline_placement(metrics(&display), request.cell, limit, display_scale(&ast)) {
                Ok(placement) => (display, placement),
                Err(_) => {
                    let text = lay_out(MathStyle::Text)?;
                    let placement =
                        inline_placement(metrics(&text), request.cell, limit, MIN_SCALE)
                            .map_err(Overflow::reason)?;
                    (text, placement)
                }
            };
        (list, placement.font_size, INLINE_PADDING, true)
    } else {
        let list = lay_out(MathStyle::Display)?;
        let mut font_size = f32::from(request.cell.1);
        let padding = 2.0;
        let natural_width = list.width as f32 * font_size + padding * 2.0;
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
        (list, font_size, padding, false)
    };
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
    let (rgba, width, height) = decode_rgba(png, request)?;
    if inline {
        // PNGのベースラインは上の余白と数式の高さの分だけ下にある。
        let baseline = padding + list.height as f32 * font_size;
        inline_image(&rgba, width, height, baseline, request.cell)
    } else {
        cell_image(&rgba, width, height, request.cell)
    }
}

/// レンダラーのPNGをRGBAの画素にする。色なし指定では全体を文字色で描く。
fn decode_rgba(png: Vec<u8>, request: &Request) -> Result<(Vec<u8>, u32, u32), String> {
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
    Ok((rgba, info.width, info.height))
}

/// インライン数式の画像の行数（本文の行を含む）。数式を大きめに描くため、一律に本文の行の
/// 上へ1行足した2行にする（2026-11-12、所有者の判断）。
pub const INLINE_ROWS: usize = 2;
/// 等幅フォントの文字送りに対する1emの比。セル幅から本文の文字の大きさを見積もる。
const ADVANCE_PER_EM: f32 = 0.6;
/// 本文の1emの見積もりに対する数式の1emの比。KaTeXの字形は同じ大きさの指定でも等幅フォントより
/// 小さく細く見える（大文字の高さ0.683em、xの高さ0.431em）ため、大きめに描く。
const MATH_PER_TEXT_EM: f32 = 1.5;
/// セルの高さに対する数式の1emの上限。行間の狭い設定でも本文より極端に大きくしない。
const EM_PER_CELL_HEIGHT: f32 = 1.05;
/// セルの上端から本文のベースラインまでの、セルの高さに対する比。
const BASELINE_PER_CELL_HEIGHT: f32 = 0.78;
/// 表示体裁で2行に収めるため縮小してよい下限。これより縮むならテキスト体裁にする。
const DISPLAY_SCALE: f32 = 0.85;
/// 最上位に分数・二項係数がある式の、表示体裁の縮小の下限。テキスト体裁では分子・分母が
/// 0.7倍の大きさになるため、これ以上なら表示体裁のほうが分子・分母を大きく描ける。
const FRACTION_DISPLAY_SCALE: f32 = 0.7;
/// 縮小の下限。これより縮めないと収まらない数式はTeX表示へ戻す。
const MIN_SCALE: f32 = 0.5;
const INLINE_PADDING: f32 = 1.0;
/// 画像の上端に残す余白の、セルの高さに対する比（最小1.5ピクセル）。分子の数字等は
/// 輪郭がフォントの高さの値より上へ出るため、端で切れず上の行の文字にも接しないようにする。
const TOP_EDGE_PER_CELL_HEIGHT: f32 = 0.05;
/// 画像の下端に残す余白（ピクセル）。下へ出る文字は深さの値にほぼ収まるため、にじみの分だけ。
const BOTTOM_EDGE: f32 = 1.0;
/// 描いた画素がフォントの高さ・深さの値からはみ出す分として、2行の高さから除く、セルの
/// 高さに対する比（最小2ピクセル）。
const INK_OVERSHOOT_PER_CELL_HEIGHT: f32 = 0.06;

/// インライン数式の大きさ。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    pub font_size: f32,
}

/// インライン数式が収まらない理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Overflow {
    /// 縮小の下限まで縮めても、本文幅に収まらない。
    Width,
    /// `min_scale` 倍まで縮めても2行に収まらない。
    Height,
}
impl Overflow {
    fn reason(self) -> String {
        match self {
            Self::Width => "幅超過".into(),
            Self::Height => format!("{INLINE_ROWS}行上限"),
        }
    }
}

/// 表示体裁で描くために縮めてよい下限。分数・二項係数が最上位（括弧の中を含み、添字の中は
/// 含めない）にある式は、分子・分母を大きく描けるよう下限を下げる。積分等は、縮めると本体が
/// 本文より小さくなるため下げない。
fn display_scale(ast: &[ratex_parser::ParseNode]) -> f32 {
    use ratex_parser::ParseNode;
    fn has_fraction(nodes: &[ParseNode]) -> bool {
        nodes.iter().any(|node| match node {
            ParseNode::GenFrac { .. } => true,
            ParseNode::OrdGroup { body, .. } | ParseNode::LeftRight { body, .. } => {
                has_fraction(body)
            }
            _ => false,
        })
    }
    if has_fraction(ast) {
        FRACTION_DISPLAY_SCALE
    } else {
        DISPLAY_SCALE
    }
}

fn top_edge(cell_height: f32) -> f32 {
    (cell_height * TOP_EDGE_PER_CELL_HEIGHT).max(1.5)
}

/// 数式の幅・高さ・深さ（em）から、2行の画像に描く文字の大きさを決める。大きさはセルから
/// 見積もった一定の値で、高さと深さの合計が2行に収まれば縮めない。ベースラインの位置で
/// 上下にはみ出す分は、画像を作るときに上下へずらして収める。2行と本文幅に収めるための
/// 縮小は `min_scale` 倍（縦）と `MIN_SCALE` 倍（幅）まで。
pub(crate) fn inline_placement(
    (width, height, depth): (f32, f32, f32),
    cell: (u16, u16),
    limit: f32,
    min_scale: f32,
) -> Result<Placement, Overflow> {
    let (cell_width, cell_height) = (f32::from(cell.0), f32::from(cell.1));
    let em = (cell_width / ADVANCE_PER_EM * MATH_PER_TEXT_EM).min(cell_height * EM_PER_CELL_HEIGHT);
    let total = (height.max(0.0) + depth.max(0.0)) * em;
    let overshoot = (cell_height * INK_OVERSHOOT_PER_CELL_HEIGHT).max(2.0);
    let budget = INLINE_ROWS as f32 * cell_height - top_edge(cell_height) - BOTTOM_EDGE - overshoot;
    let mut scale = if total > budget { budget / total } else { 1.0 };
    if !scale.is_finite() || scale < min_scale.max(MIN_SCALE) {
        return Err(Overflow::Height);
    }
    let natural_width = width * em * scale + INLINE_PADDING * 2.0;
    if natural_width > limit {
        scale *= (limit - INLINE_PADDING * 2.0) / (natural_width - INLINE_PADDING * 2.0);
    }
    if !scale.is_finite() || scale < MIN_SCALE {
        return Err(Overflow::Width);
    }
    Ok(Placement {
        font_size: em * scale,
    })
}

/// レンダラーの画素を、2行の格子の下の行（本文の行）のベースラインに数式のベースラインが
/// 重なるよう置く。上下の余白を残して収まらない分は、ベースラインの位置をずらして収める。
/// 余白は透明。
fn inline_image(
    rgba: &[u8],
    width: u32,
    height: u32,
    baseline: f32,
    cell: (u16, u16),
) -> Result<Image, String> {
    let (cell_width, cell_height) = (usize::from(cell.0), usize::from(cell.1));
    let rows = INLINE_ROWS;
    let columns = (width as usize).div_ceil(cell_width).max(1);
    let (padded_width, padded_height) = (columns * cell_width, rows * cell_height);
    if padded_width * padded_height * 4 > OUTPUT_LIMIT {
        return Err("画像容量上限".into());
    }
    let target =
        (rows - 1) as f32 * cell_height as f32 + cell_height as f32 * BASELINE_PER_CELL_HEIGHT;
    let row = width as usize * 4;
    // 実際に描いた画素で範囲を決める。文字の輪郭はフォントの高さ・深さの値から1画素ほど
    // はみ出し得るため。
    // 各画素の4バイト目（アルファ）だけを見る。
    let inked = |y: &usize| {
        rgba[y * row + 3..(y + 1) * row]
            .iter()
            .step_by(4)
            .any(|&alpha| alpha > 0)
    };
    let mut offset = (target - baseline).round() as isize;
    if let (Some(first), Some(last)) = (
        (0..height as usize).find(inked),
        (0..height as usize).rev().find(inked),
    ) {
        let (first, last) = (first as isize, last as isize);
        let top = top_edge(cell_height as f32).ceil() as isize;
        let bottom = padded_height as isize - 1 - BOTTOM_EDGE as isize;
        let (low, high) = if bottom - top >= last - first {
            (top - first, bottom - last)
        } else {
            // 余白を残すと収まらない場合も、画像の端では切らない。
            (-first, padded_height as isize - 1 - last)
        };
        offset = offset.min(high).max(low);
    }
    let mut padded = vec![0; padded_width * padded_height * 4];
    for y in 0..height as usize {
        let Ok(to) = usize::try_from(y as isize + offset) else {
            continue;
        };
        if to >= padded_height {
            break;
        }
        let at = to * padded_width * 4;
        padded[at..at + row].copy_from_slice(&rgba[y * row..(y + 1) * row]);
    }
    let mut image = encode(&padded, padded_width as u32, padded_height as u32)?;
    image.columns = columns;
    image.rows = rows;
    image.above = rows - 1;
    Ok(image)
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
    let mut image = encode(&padded, padded_width, padded_height)?;
    image.columns = columns;
    image.rows = rows;
    Ok(image)
}

/// RGBAの画素をPNGにする。行数・列数は呼び出し側で設定する。
fn encode(rgba: &[u8], width: u32, height: u32) -> Result<Image, String> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .map_err(|_| "PNG出力失敗")?
            .write_image_data(rgba)
            .map_err(|_| "PNG出力失敗")?;
    }
    Ok(Image {
        png: output,
        width,
        height,
        columns: 0,
        rows: 0,
        above: 0,
    })
}
