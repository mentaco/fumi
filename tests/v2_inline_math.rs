//! v2のインライン数式の画像化（仕様7.4節）。生成する画像の大きさ、本文中の配置、折り返し、
//! 上下の行、検索の強調、TeX表示へ戻す条件。
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fumi::{
    app::App,
    document::Document,
    layout::DisplayLine,
    math::{self, Image, Request, Status, View},
};
use ratatui::{Terminal, backend::TestBackend, style::Modifier};
use std::{collections::HashMap, sync::Arc};

const CELL: (u16, u16) = (10, 20);

fn request(tex: &str, width: usize) -> Request {
    Request {
        tex: tex.into(),
        width,
        cell: CELL,
        foreground: [235; 3],
        monochrome: false,
        inline: true,
    }
}

/// 生成済みとして扱う画像。PNGの中身はレイアウトでは使わない。
fn image(columns: usize, rows: usize, above: usize) -> View {
    View::Ready(Arc::new(Image {
        png: Vec::new(),
        width: columns as u32 * u32::from(CELL.0),
        height: rows as u32 * u32::from(CELL.1),
        columns,
        rows,
        above,
    }))
}

fn texts(lines: &[DisplayLine]) -> Vec<String> {
    lines.iter().map(DisplayLine::text).collect()
}

/// インライン数式の画像化を有効にし、画像表示を始めた画面。
fn viewer(source: &str, width: u16) -> App {
    let mut app = App::new(Document::parse(source.into()), "inline.md", false);
    app.set_inline_math(true);
    app.resize(width, 12);
    app.enable_graphics();
    app
}

/// 文書中の `$` の位置（`n` 番目）。インライン数式の要素のソース開始位置になる。
fn dollar(source: &str, n: usize) -> usize {
    source.match_indices('$').nth(n).unwrap().0
}

/// PNGの不透明な画素がある行（画素単位）の範囲。
fn opaque_rows(png: &[u8]) -> (u32, u32) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().unwrap();
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).unwrap();
    let rows: Vec<u32> = (0..info.height)
        .filter(|&y| {
            data[(y * info.width * 4) as usize..((y + 1) * info.width * 4) as usize]
                .chunks_exact(4)
                .any(|p| p[3] > 0)
        })
        .collect();
    (rows[0], *rows.last().unwrap())
}

#[test]
fn every_formula_uses_two_rows_with_the_text_row_below() {
    // 数式を大きめに描くため、どの式も本文の行の上へ1行足した2行の画像にする。
    for tex in [
        "x",
        "x_i",
        "E = mc^2",
        r"\frac{a}{b}",
        r"A^{B^{C^{D^{E}}}}",
        r"x_{i_{j_{k}}}",
        r"\sum_{i=1}^{n} x_i",
    ] {
        let image = math::render(&request(tex, 38)).unwrap();
        assert_eq!((image.rows, image.above), (2, 1), "{tex}");
        assert_eq!(
            image.width,
            image.columns as u32 * u32::from(CELL.0),
            "{tex}"
        );
        assert_eq!(image.height, 2 * u32::from(CELL.1), "{tex}");
        // 描いた画素は画像の上下の端で切れず、隣の行の文字にも接しない。
        let (top, bottom) = opaque_rows(&image.png);
        assert!(
            top > 0 && bottom + 1 < image.height,
            "{tex}: {top}..{bottom}"
        );
    }
    // 2行に収まらない数式はTeX表示へ戻す。
    let tall = math::render(&request(r"\begin{pmatrix}a\\b\\c\\d\\e\end{pmatrix}", 38));
    assert_eq!(tall.unwrap_err(), "2行上限");
    // 本文幅を超えて縮めると読めない数式もTeX表示へ戻す。
    let wide = math::render(&request(&"x+".repeat(40), 10));
    assert_eq!(wide.unwrap_err(), "幅超過");
    // ブロック数式の表示体裁と大きさは変わらない。
    let block = math::render(&Request {
        inline: false,
        ..request(r"\frac{a}{b}", 38)
    })
    .unwrap();
    assert_eq!(block.above, 0);
}

#[test]
fn formulas_keep_one_large_size_instead_of_shrinking_to_the_text_row() {
    // 行間の広い端末（セル14×33）。数式はセル幅から見積もった本文の大きさの1.5倍で描く。
    let cell = (14, 33);
    let render = |tex: &str| {
        math::render(&Request {
            cell,
            ..request(tex, 78)
        })
        .unwrap()
    };
    let height = |tex: &str| {
        let (top, bottom) = opaque_rows(&render(tex).png);
        bottom - top + 1
    };
    // xの高さが行の約45%。
    assert!(height("x") * 100 >= 33 * 43, "{}", height("x"));
    // 本文の行の下へ出る括弧も縮めない。1行に収めるため縮めていた以前は、`f(x)` の高さが
    // 行の高さに収まっていた。
    assert!(height("f(x)") > 33, "{}", height("f(x)"));
    // 分数は分子・分母が本文に近い大きさの表示体裁で、2行の大半を使う。
    assert!(
        height(r"\frac{a}{b}") * 10 > 66 * 8,
        "{}",
        height(r"\frac{a}{b}")
    );
    // 二項係数・括弧で囲んだ分数も、少し縮めて表示体裁で描く。テキスト体裁では分子・分母が
    // 0.7倍になり、2行の3分の2ほどしか使わず小さく見えた。
    for tex in [r"\binom{n}{k}", r"\left(\frac{a}{b}\right)"] {
        assert!(height(tex) * 10 > 66 * 8, "{tex}: {}", height(tex));
    }
    // 積分は表示体裁では縮めないと収まらず、本体の `f(x)` が本文より小さくなるため、
    // 添字を横に付けて縮めずに描く（2行の8割未満）。
    let integral = height(r"\int_0^1 f(x)\,dx");
    assert!(integral * 10 < 66 * 8, "{integral}");
    // 上下に添字の付く総和は、2行へ大きく縮めて押し込まず、添字を横に付けて描く。
    assert!(
        height(r"\sum_{i=1}^{n} x_i") >= 33,
        "{}",
        height(r"\sum_{i=1}^{n} x_i")
    );
    // セルが小さい端末でも、分数は表示体裁で1行より大きく描く。
    let (top, bottom) = opaque_rows(&math::render(&request(r"\frac{a}{b}", 38)).unwrap().png);
    assert!(bottom - top > u32::from(CELL.1), "{top}..{bottom}");
}

#[test]
fn the_text_row_baseline_matches_for_symbols_with_and_without_descenders() {
    // `a` の下端は本文の行（2行目）のベースライン（セルの上端から高さの78%）にある。
    // `y` は下へ出る分が本文の行に収まらないため縮めずに上へずらすが、ずれは数画素にとどまる。
    let a = math::render(&request("a", 38)).unwrap();
    let y = math::render(&request("y", 38)).unwrap();
    let (a_top, a_bottom) = opaque_rows(&a.png);
    let (y_top, y_bottom) = opaque_rows(&y.png);
    assert!(a_top.abs_diff(y_top) <= 3, "{a_top} {y_top}");
    let baseline = u32::from(CELL.1) + u32::from(CELL.1) * 78 / 100;
    assert!(a_bottom.abs_diff(baseline) <= 1, "{a_bottom} {baseline}");
    // `y` の下端はベースラインより下へ出る。
    assert!(y_bottom > a_bottom, "{y_bottom} {a_bottom}");
}

#[test]
fn ready_formulas_become_atomic_cells_inside_the_text() {
    let source = "前 $x_i$ と $\\alpha$ 後\n";
    let mut app = viewer(source, 40);
    assert_eq!(texts(app.lines()), ["前 $x_i$ と $\\alpha$ 後"]);
    let (x, alpha) = (dollar(source, 0), dollar(source, 2));
    // 生成に失敗した数式はTeXのまま。インライン数式には理由のラベルを付けない。
    app.set_images(
        HashMap::from([(x, image(2, 1, 0)), (alpha, View::Failed("失敗".into()))]),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(texts(lines), ["前    と $\\alpha$ 後"]);
    assert_eq!(lines[0].formulas.len(), 1);
    let formula = &lines[0].formulas[0];
    // 「前」は2セル幅。列は表示幅で数える。
    assert_eq!((formula.source, formula.column, formula.row), (x, 3, 0));
    assert_eq!(&lines[0].text()[formula.text.clone()], "  ");
    // 画像の下の空白は数式のソース全体に対応する。
    assert!(
        lines[0]
            .mappings
            .iter()
            .any(|m| m.text == formula.text && m.source == (x..x + "$x_i$".len()))
    );
}

#[test]
fn tall_formulas_add_image_only_rows_above_or_below_the_text_row() {
    let source = "a $\\frac{1}{2}$ b $y_{j_k}$ c\n\nnext\n";
    let mut app = viewer(source, 40);
    let (fraction, deep) = (dollar(source, 0), dollar(source, 2));
    app.set_images(
        HashMap::from([(fraction, image(1, 2, 1)), (deep, image(2, 2, 0))]),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(texts(lines), ["", "a   b    c", "", "", "next"]);
    // 上の行には分数の上半分、本文の行には両方、下の行には深い数式の下半分を置く。
    let placed = |line: &DisplayLine| -> Vec<_> {
        line.formulas
            .iter()
            .map(|f| (f.source, f.row, f.column))
            .collect()
    };
    assert_eq!(placed(&lines[0]), [(fraction, 0, 2)]);
    assert_eq!(placed(&lines[1]), [(fraction, 1, 2), (deep, 0, 6)]);
    assert_eq!(placed(&lines[2]), [(deep, 1, 6)]);
    // 画像だけの行も本文の行と同じソース範囲にし、閲覧位置の基準をずらさない。
    assert_eq!(lines[0].source, lines[1].source);
    assert_eq!(lines[2].source, lines[1].source);
    assert!(lines[0].formulas.iter().all(|f| f.text.is_empty()));
}

#[test]
fn formulas_wrap_as_a_whole_and_are_not_break_points() {
    // 本文幅20（端末幅40未満は余白なし）。数式は行末で分割せず次の行へ送る。
    let source = "aaaaaaaaaaaaaaaaa $x$ b\n";
    let mut app = viewer(source, 20);
    app.set_images(
        HashMap::from([(dollar(source, 0), image(4, 1, 0))]),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(texts(lines), ["aaaaaaaaaaaaaaaaa", "     b"]);
    assert_eq!(lines[1].formulas[0].column, 0);
    // 1列の数式も空白として扱わず、行頭で落とさない。
    let source = "aaaaaaaaaaaaaaaaaaa $x$\n";
    let mut app = viewer(source, 20);
    app.set_images(
        HashMap::from([(dollar(source, 0), image(1, 1, 0))]),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(texts(lines), ["aaaaaaaaaaaaaaaaaaa", " "]);
    assert_eq!(lines[1].formulas.len(), 1);
}

#[test]
fn formulas_follow_list_indent_and_fall_back_when_wider_than_the_line() {
    let source = "- 項目 $x$\n- $y$\n";
    let mut app = viewer(source, 40);
    app.set_images(
        HashMap::from([
            (dollar(source, 0), image(2, 1, 0)),
            // 字下げの後の幅（38 - 2）を超える数式は、折り返せないためTeXのまま。
            (dollar(source, 2), image(37, 1, 0)),
        ]),
        Status::Pending,
    );
    let lines = app.lines();
    assert_eq!(texts(lines), ["• 項目   ", "• $y$"]);
    assert_eq!(lines[0].formulas[0].column, 7);
    assert!(lines[1].formulas.is_empty());
}

#[test]
fn tables_code_and_disabled_option_keep_tex() {
    let source = "| a |\n|---|\n| $x$ |\n\n`$y$`\n\n$z$\n";
    let document = Document::parse(source.into());
    // 表のセルとコードの中は画像化の対象にしない。
    let targets: Vec<_> = document
        .inline_math()
        .into_iter()
        .map(|(node, tex)| (node.source.start, tex))
        .collect();
    assert_eq!(targets, [(source.rfind("$z$").unwrap(), "z")]);
    // `--inline-math image` を指定しなければ、結果があってもTeXのまま。
    let mut app = App::new(document, "inline.md", false);
    app.resize(40, 12);
    app.enable_graphics();
    let z = source.rfind("$z$").unwrap();
    app.set_images(HashMap::from([(z, image(1, 1, 0))]), Status::Pending);
    assert!(app.lines().iter().all(|line| line.formulas.is_empty()));
    assert!(texts(app.lines()).contains(&"$z$".to_string()));
}

#[test]
fn search_matches_in_formula_images_are_underlined_not_reversed() {
    let source = "word $x_i$ word\n";
    let mut app = viewer(source, 40);
    app.set_images(
        HashMap::from([(dollar(source, 0), image(2, 1, 0))]),
        Status::Pending,
    );
    // 本文の一致は反転し、画像の下の一致は現在の一致でも下線にする。
    app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    app.paste("x_i");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|f| assert!(app.draw(f))).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let column = 1 + "word ".len() as u16;
    for x in column..column + 2 {
        let cell = &buffer[(x, 0)];
        assert!(cell.modifier.contains(Modifier::UNDERLINED), "{x}");
        assert!(!cell.modifier.contains(Modifier::REVERSED), "{x}");
    }
    assert!(app.status().contains("1/1"), "{}", app.status());
}

#[test]
fn width_changes_and_disabling_graphics_restore_the_tex() {
    let source = "前 $x$ 後\n";
    let mut app = viewer(source, 40);
    app.set_images(
        HashMap::from([(dollar(source, 0), image(1, 1, 0))]),
        Status::Pending,
    );
    assert_eq!(texts(app.lines()), ["前   後"]);
    app.disable_graphics("画像転送失敗".into(), true);
    assert_eq!(texts(app.lines()), ["前 $x$ 後"]);
}
