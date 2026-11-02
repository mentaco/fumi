//! v2のローカル画像（仕様8.3節）。参照先の解決、読込・デコード・縮小、レイアウトと画像の行。
use fumi::{
    app::App,
    document::Document,
    layout::{layout, layout_with_images},
    math::{Status, View},
    picture::{self, MAX_FILE, Request, local_path, resolve},
};
use image::{ImageEncoder, codecs::jpeg::JpegEncoder};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

/// RGBAの単色の領域を並べたPNG。`colors` の各色を横に `block` 画素ずつ並べる。
fn png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&pixel(x, y));
        }
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
    }
    output
}

/// 出力したPNGをRGBAの画素へ戻す。
fn pixels(png: &[u8]) -> (u32, u32, Vec<u8>) {
    let image = image::load_from_memory(png).unwrap().to_rgba8();
    (image.width(), image.height(), image.into_raw())
}

fn request(path: &Path, columns: usize, cell: (u16, u16)) -> Request {
    Request {
        path: path.into(),
        stamp: fumi::reload::stamp(path),
        columns,
        cell,
    }
}

fn write(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn local_paths_exclude_schemes_authorities_queries_and_nul() {
    let path = |text: &str| local_path(text).map(|p| p.to_string_lossy().into_owned());
    assert_eq!(path("img/cat.png").as_deref(), Some("img/cat.png"));
    assert_eq!(path("/abs/cat.png").as_deref(), Some("/abs/cat.png"));
    assert_eq!(path("./a%20b.png").as_deref(), Some("./a b.png"));
    assert_eq!(path("%E7%8C%AB.png").as_deref(), Some("猫.png"));
    assert_eq!(
        path("~/cat.png").as_deref(),
        Some("~/cat.png"),
        "~ は展開しない"
    );
    for rejected in [
        "",
        "https://example.com/a.png",
        "http:a.png",
        "file:///tmp/a.png",
        "data:image/png;base64,AAAA",
        "C:/a.png",
        "//host/a.png",
        "a.png?size=2",
        "a.png#top",
        "a\0.png",
        "a%00.png",
        "a%zz.png",
        "a%2.png",
    ] {
        assert_eq!(path(rejected), None, "{rejected:?}");
    }
    assert_eq!(
        resolve(Path::new("docs"), Path::new("a.png")),
        Path::new("docs/a.png")
    );
    assert_eq!(
        resolve(Path::new("docs"), Path::new("/a.png")),
        Path::new("/a.png")
    );
}

#[test]
fn png_keeps_transparency_and_colors_and_pads_to_cells() {
    let directory = tempfile::tempdir().unwrap();
    // 左半分は半透明の赤、右半分は完全に透明。
    let path = write(
        directory.path(),
        "a.png",
        &png(
            40,
            30,
            |x, _| if x < 20 { [255, 0, 0, 128] } else { [0; 4] },
        ),
    );
    let image = picture::render(&request(&path, 80, (10, 20))).unwrap();
    assert_eq!((image.columns, image.rows), (4, 2));
    let (width, height, data) = pixels(&image.png);
    assert_eq!((width, height), (40, 40), "セル境界まで透明で埋める");
    assert_eq!(&data[..4], &[255, 0, 0, 128], "色と透明度を保つ");
    assert_eq!(&data[(25 * 4)..(25 * 4 + 4)], &[0; 4]);
    let padding = (35 * 40 * 4) as usize;
    assert_eq!(&data[padding..padding + 4], &[0; 4]);
}

#[test]
fn images_fit_the_width_and_twelve_rows_without_upscaling() {
    let directory = tempfile::tempdir().unwrap();
    let solid = |_, _| [0, 128, 255, 255];
    let wide = write(directory.path(), "wide.png", &png(1000, 100, solid));
    let image = picture::render(&request(&wide, 10, (10, 20))).unwrap();
    assert_eq!(
        (image.columns, image.rows),
        (10, 1),
        "本文幅100画素に収める"
    );
    let tall = write(directory.path(), "tall.png", &png(100, 1000, solid));
    let image = picture::render(&request(&tall, 80, (10, 20))).unwrap();
    assert_eq!(image.rows, 12, "12行（240画素）に収める");
    assert_eq!(image.columns, 3, "縦横比を保つ（24画素）");
    let small = write(directory.path(), "small.png", &png(4, 2, solid));
    let image = picture::render(&request(&small, 80, (10, 20))).unwrap();
    let (width, height, data) = pixels(&image.png);
    assert_eq!((image.columns, image.rows, width, height), (1, 1, 10, 20));
    assert_eq!(
        &data[(4 * 4)..(4 * 4 + 4)],
        &[0; 4],
        "元の大きさより拡大しない"
    );
}

/// EXIFの向き（Orientation）を持つJPEG。
fn jpeg_with_orientation(width: u32, height: u32, rgb: &[u8], orientation: u16) -> Vec<u8> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), 95)
        .write_image(rgb, width, height, image::ExtendedColorType::Rgb8)
        .unwrap();
    let mut tiff = b"MM\0*\0\0\0\x08\0\x01".to_vec();
    tiff.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1]);
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut result = jpeg[..2].to_vec();
    result.extend_from_slice(&[0xff, 0xe1]);
    result.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    result.extend_from_slice(&app1);
    result.extend_from_slice(&jpeg[2..]);
    result
}

#[test]
fn jpeg_applies_the_exif_orientation() {
    let directory = tempfile::tempdir().unwrap();
    // 横32×縦16。左半分が赤、右半分が青。
    let mut rgb = Vec::new();
    for _ in 0..16 {
        for x in 0..32 {
            rgb.extend_from_slice(if x < 16 { &[255, 0, 0] } else { &[0, 0, 255] });
        }
    }
    let plain = write(
        directory.path(),
        "plain.jpg",
        &jpeg_with_orientation(32, 16, &rgb, 1),
    );
    let rotated = write(
        directory.path(),
        "rotated.jpg",
        &jpeg_with_orientation(32, 16, &rgb, 6),
    );
    // セルは1×4画素。12行（48画素）までなので縮小しない。
    let image = picture::render(&request(&plain, 100, (1, 4))).unwrap();
    assert_eq!((image.columns, image.rows), (32, 4));
    // 6は時計回りに90度。左側（赤）が上に来る。
    let image = picture::render(&request(&rotated, 100, (1, 4))).unwrap();
    assert_eq!((image.columns, image.rows), (16, 8));
    let (width, _, data) = pixels(&image.png);
    let at = |x: u32, y: u32| &data[((y * width + x) * 4) as usize..][..3];
    assert!(
        at(8, 4)[0] > 200 && at(8, 4)[2] < 60,
        "上は赤: {:?}",
        at(8, 4)
    );
    assert!(
        at(8, 28)[2] > 200 && at(8, 28)[0] < 60,
        "下は青: {:?}",
        at(8, 28)
    );
}

#[test]
fn format_is_detected_by_content_not_extension() {
    let directory = tempfile::tempdir().unwrap();
    let path = write(
        directory.path(),
        "named.jpg",
        &png(10, 10, |_, _| [1, 2, 3, 255]),
    );
    assert!(picture::render(&request(&path, 80, (10, 20))).is_ok());
    let gif = write(directory.path(), "a.png", b"GIF89a\x01\0\x01\0\0\0\0;");
    let text = write(directory.path(), "b.png", b"not an image");
    for path in [gif, text] {
        assert_eq!(
            picture::render(&request(&path, 80, (10, 20))).unwrap_err(),
            "形式未対応"
        );
    }
}

#[test]
fn failures_fall_back_with_a_reason_before_allocating_pixels() {
    let directory = tempfile::tempdir().unwrap();
    let fail = |path: &Path| picture::render(&request(path, 80, (10, 20))).unwrap_err();
    assert_eq!(
        fail(&directory.path().join("none.png")),
        "ファイルが見つかりません"
    );
    assert_eq!(fail(directory.path()), "通常ファイルではありません");
    let fifo = directory.path().join("fifo.png");
    assert_eq!(unsafe { libc::mkfifo(c_path(&fifo).as_ptr(), 0o600) }, 0);
    assert_eq!(
        fail(&fifo),
        "通常ファイルではありません",
        "FIFOで停止しない"
    );
    let huge = directory.path().join("huge.png");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(MAX_FILE + 1)
        .unwrap();
    assert_eq!(fail(&huge), "20 MiB上限");
    // IHDRだけが6000×6000（3600万画素）を名乗る。画素の領域を確保する前に止める。
    let mut header = png(1, 1, |_, _| [0; 4]);
    header[16..24].copy_from_slice(&[0, 0, 0x17, 0x70, 0, 0, 0x17, 0x70]);
    let crc = crc32(&header[12..29]);
    header[29..33].copy_from_slice(&crc.to_be_bytes());
    let many = write(directory.path(), "many.png", &header);
    assert_eq!(fail(&many), "25メガピクセル上限");
    let broken = write(directory.path(), "broken.png", b"\x89PNG\r\n\x1a\nbroken");
    assert_eq!(fail(&broken), "画像が壊れています");
}

fn c_path(path: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[test]
fn animated_png_keeps_the_alternative_text() {
    let directory = tempfile::tempdir().unwrap();
    let mut apng = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut apng, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(2, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255; 16]).unwrap();
        writer.write_image_data(&[0; 16]).unwrap();
    }
    let path = write(directory.path(), "anim.png", &apng);
    assert_eq!(
        picture::render(&request(&path, 80, (10, 20))).unwrap_err(),
        "アニメーション未対応"
    );
}

#[test]
fn only_local_images_in_top_level_paragraphs_and_headings_are_displayed() {
    let source = "# 見出し ![h](h.png)\n\n前 ![a](a.png) 後 *強調 ![b](b.png)*\n\n\
        - ![list](l.png)\n\n> ![quote](q.png)\n\n[![link](k.png)](https://example.com)\n\n\
        ![remote](https://example.com/r.png) ![data](data:image/png;base64,AA)\n\n\
        | ![table](t.png) |\n|---|\n| x |\n";
    let document = Document::parse(source.into());
    let images: Vec<_> = document
        .local_images()
        .into_iter()
        .map(|(range, path)| (source[range].to_string(), path))
        .collect();
    assert_eq!(
        images,
        [
            ("![h](h.png)".into(), PathBuf::from("h.png")),
            ("![a](a.png)".into(), PathBuf::from("a.png")),
            ("![b](b.png)".into(), PathBuf::from("b.png")),
        ]
    );
}

fn texts(lines: &[fumi::layout::DisplayLine]) -> Vec<String> {
    lines.iter().map(|line| line.text()).collect()
}

#[test]
fn displayed_images_split_the_paragraph_into_their_own_lines() {
    let source = "前の文 ![猫](cat.png) 後の文\nつづき\n\n- ![list](l.png) 項目\n";
    let document = Document::parse(source.into());
    let ranges: Vec<_> = document
        .local_images()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(
        texts(&layout_with_images(&document, 40, false, &ranges)),
        [
            "前の文",
            "[image: 猫] <cat.png>",
            "後の文 つづき",
            "",
            "• [image: list] <l.png> 項目"
        ]
    );
    // 画像を表示しないときは、v1と同じく文中の代替テキストのまま。
    assert_eq!(
        texts(&layout(&document, 60, false))[0],
        "前の文 [image: 猫] <cat.png> 後の文 つづき"
    );
    // 段落の先頭・末尾の画像、連続する画像では空行を作らない。
    let document = Document::parse("![a](a.png)\n![b](b.png)\n\n# ![h](h.png)\n".into());
    let ranges: Vec<_> = document
        .local_images()
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(
        texts(&layout_with_images(&document, 40, false, &ranges)),
        [
            "[image: a] <a.png>",
            "[image: b] <b.png>",
            "",
            "# ",
            "[image: h] <h.png>"
        ]
    );
}

fn image(rows: usize) -> Arc<fumi::math::Image> {
    Arc::new(fumi::math::Image {
        png: Vec::new(),
        width: 10,
        height: 20 * rows as u32,
        columns: 1,
        rows,
    })
}

#[test]
fn image_rows_go_above_the_alternative_text_and_reasons_below() {
    let source = "前 ![猫](cat.png) 後\n\n![ない](none.png)\n";
    let mut app = App::new(Document::parse(source.into()), "images.md", false);
    app.resize(40, 12);
    let cat = source.find("![猫").unwrap();
    let none = source.find("![ない").unwrap();
    assert_eq!(texts(app.lines())[0], "前 [image: 猫] <cat.png> 後");
    app.enable_graphics();
    assert_eq!(
        texts(app.lines()),
        [
            "前",
            "[image: 猫] <cat.png>",
            "[image: 生成中]",
            "後",
            "",
            "[image: ない] <none.png>",
            "[image: 生成中]"
        ]
    );
    app.set_images(
        [
            (cat, View::Ready(image(2))),
            (none, View::Failed("ファイルが見つかりません".into())),
        ]
        .into(),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(lines[1].image, Some((cat, 0)));
    assert_eq!(lines[2].image, Some((cat, 1)));
    assert_eq!(
        texts(&lines[3..]),
        [
            "[image: 猫] <cat.png>",
            "後",
            "",
            "[image: ない] <none.png>",
            "[image: ファイルが見つかりません]"
        ]
    );
    // 画像表示をやめると、v1と同じ文中の代替テキストへ戻り、理由も付けない。
    app.disable_graphics("画像転送失敗".into(), true);
    assert_eq!(texts(app.lines())[0], "前 [image: 猫] <cat.png> 後");
    assert!(
        texts(app.lines())
            .iter()
            .all(|t| !t.contains("[image: 画像転送失敗]"))
    );
}
