use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use fumi::{
    app::{Action, App},
    document::Document,
    input::{Decoder, InputEvent},
    layout::{layout, search_segments},
    math::{self, Request, Status, View},
    search,
};
use ratatui::{Terminal, backend::TestBackend, style::Modifier};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;
fn key(app: &mut App, code: KeyCode) -> Action {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn ctrl(app: &mut App, c: char) -> Action {
    app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}
fn app(source: &str) -> App {
    let mut app = App::new(Document::parse(source.into()), "v1.md", true);
    app.resize(40, 8);
    app
}
fn query(app: &mut App, text: &str) {
    key(app, KeyCode::Char('/'));
    app.paste(text);
    key(app, KeyCode::Enter);
}
fn request(tex: &str) -> Request {
    Request {
        tex: tex.into(),
        width: 78,
        cell: (10, 20),
        background: [20; 3],
        foreground: [235; 3],
        monochrome: false,
    }
}

#[test]
fn logical_search_respects_boundaries_and_generated_markers() {
    let doc = Document::parse("**hello** world\n日本語 e\u{301} 👩‍💻\n\n- item\n\n```rust\nhello\nworld\n```\n\n| A | B |\n|---|---|\n| cat | dog |\n\n![代替](image.png) [リンク](https://example.org)\n\n$$ x_1 + x_2 $$".into());
    let segments = search_segments(&doc);
    for word in [
        "hello world 日本語",
        "e\u{301}",
        "👩‍💻",
        "item",
        "A",
        "cat",
        "dog",
        "代替",
        "image.png",
        "https://example.org",
        "x_1",
    ] {
        assert!(!search::find(&segments, word).is_empty(), "{word}");
    }
    for word in [
        "•",
        "[row 1]",
        "catdog",
        "Acat",
        "hello\nworld",
        "HELLO",
        "[rust]",
    ] {
        assert!(search::find(&segments, word).is_empty(), "{word}");
    }
}
#[test]
fn search_input_preview_cancel_confirm_cycle_and_resize() {
    let mut app = app(&("日本語 hello **world** hello\n\n".repeat(25)));
    query(&mut app, "hello world");
    assert_eq!(app.search.matches.len(), 25);
    let matches: Vec<_> = app.search.matches.iter().map(|m| m.start()).collect();
    key(&mut app, KeyCode::Char('N'));
    assert_eq!(app.search.current, Some(24));
    assert!(app.search.wrapped);
    let top = app.top;
    key(&mut app, KeyCode::Char('/'));
    app.paste("q日本語");
    assert_eq!(app.top, top);
    assert_eq!(app.search.matches.len(), 25);
    assert_eq!(ctrl(&mut app, 'c'), Action::None);
    assert_eq!(app.search.query, "hello world");
    assert_eq!(app.top, top);
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.search.current, Some(0));
    app.resize(20, 5);
    assert_eq!(
        matches,
        app.search
            .matches
            .iter()
            .map(|m| m.start())
            .collect::<Vec<_>>()
    );
    query(&mut app, "missing");
    let top = app.top;
    assert!(app.status().contains("見つかりません"));
    assert_eq!(app.top, top);
    query(&mut app, "");
    assert_eq!(app.search.query, "missing");
}
#[test]
fn search_grapheme_limit_backspace_paste_and_modes() {
    let mut app = app("日本語");
    key(&mut app, KeyCode::Char('/'));
    app.paste(&"e\u{301}".repeat(256));
    app.paste("x");
    assert!(app.search.limited);
    assert!(app.status().contains("256"));
    key(&mut app, KeyCode::Backspace);
    app.paste("👩‍💻\n\r\x1b\0");
    assert!(!app.search.limited);
    assert!(app.search.input.as_ref().unwrap().ends_with("👩‍💻"));
    ctrl(&mut app, 'u');
    assert_eq!(app.search.input.as_deref(), Some(""));
    key(&mut app, KeyCode::Char('q'));
    assert_eq!(app.search.input.as_deref(), Some("q"));
    app.resize(10, 2);
    assert!(app.search.input.is_none());
    app.resize(40, 8);
    key(&mut app, KeyCode::Char('?'));
    key(&mut app, KeyCode::Char('/'));
    assert!(app.search.input.is_none());
}
#[test]
fn escape_cancels_input_and_modes_before_ending_search() {
    let mut app = app(&"[hello](https://example.com)\n\n".repeat(25));
    query(&mut app, "example.com");
    key(&mut app, KeyCode::Char('N'));
    let top = app.top;
    let current = app.search.current;
    assert!(app.search.wrapped);
    assert!(!app.search.destination_matches.is_empty());

    key(&mut app, KeyCode::Char('/'));
    app.paste("missing");
    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    assert!(app.search.input.is_none());
    assert_eq!(app.search.query, "example.com");
    assert_eq!(app.search.current, current);
    assert_eq!(app.top, top);

    key(&mut app, KeyCode::Char('?'));
    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    assert!(!app.help);
    assert_eq!(app.search.current, current);
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    assert!(!app.pending_g);
    assert_eq!(app.search.current, current);

    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    assert!(app.search.query.is_empty());
    assert!(app.search.matches.is_empty());
    assert!(app.search.current.is_none());
    assert!(app.search.destination_matches.is_empty());
    assert!(!app.search.wrapped);
    assert!(!app.status().contains("example.com"));
    for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
        assert_eq!(key(&mut app, code), Action::None);
        assert_eq!(app.top, top);
    }
    query(&mut app, "");
    assert!(app.search.matches.is_empty());
    assert_eq!(app.top, top);
    app.resize(80, 8);
    assert!(app.search.matches.is_empty());
    query(&mut app, "hello");
    assert_eq!(app.search.matches.len(), 25);
    assert_eq!(key(&mut app, KeyCode::Char('q')), Action::Exit(0));
}
#[test]
fn escape_clears_no_match_search_and_search_in_small_terminal() {
    for size in [(40, 8), (10, 2)] {
        let mut app = app("日本語");
        query(&mut app, "missing");
        assert!(app.status().contains("見つかりません"));
        app.resize(size.0, size.1);
        assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
        app.resize(40, 8);
        assert!(!app.status().contains("見つかりません"));
        assert!(app.search.query.is_empty());
        assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
        assert_eq!(app.top, 0);
    }
}
#[test]
fn search_highlights_all_matches_without_color() {
    let mut app = App::new(
        Document::parse("**hello** world hello world".into()),
        "a",
        false,
    );
    app.resize(40, 8);
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    let original = terminal.backend().buffer().clone();
    let original_status = app.status();
    query(&mut app, "hello world");
    terminal.draw(|f| app.draw(f)).unwrap();
    let cells = &terminal.backend().buffer().content;
    assert!(
        cells
            .iter()
            .any(|c| c.modifier.contains(Modifier::REVERSED))
    );
    assert!(
        cells
            .iter()
            .any(|c| c.modifier.contains(Modifier::UNDERLINED))
    );
    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    terminal.draw(|f| app.draw(f)).unwrap();
    assert_eq!(
        &terminal.backend().buffer().content[..40 * 7],
        &original.content[..40 * 7]
    );
    assert_eq!(app.status(), original_status);
}
#[test]
fn tables_choose_natural_width_align_and_preserve_empty_values() {
    let source = "| 名 | 数 | 中央 |\n|:---|---:|:---:|\n| 日本 | 1 | x |\n| a | 222 | |";
    let doc = Document::parse(source.into());
    let wide = layout(&doc, 80, false);
    let narrow = layout(&doc, 20, false);
    assert!(wide[0].text().starts_with('┌'));
    assert!(narrow[0].text().starts_with("[row 1]"));
    for lines in [&wide, &narrow] {
        for value in ["日本", "222", "(empty)"] {
            assert!(lines.iter().any(|line| line.text().contains(value)));
        }
    }
    let width = wide[0].text().width();
    assert!(wide.iter().all(|line| line.text().width() == width));
    assert!(layout(&doc, width, false)[0].text().starts_with('┌'));
    assert!(
        layout(&doc, width - 1, false)[0]
            .text()
            .starts_with("[row 1]")
    );
    assert!(wide.iter().any(|line| line.text().contains("   1 │")));
}
#[test]
fn code_highlight_and_unknown_language_keep_text_and_mapping() {
    let doc = Document::parse("```rust\nfn main() { let 日本語 = 123; }\n```".into());
    let color = layout(&doc, 80, true);
    let plain = layout(&doc, 80, false);
    assert_eq!(
        color.iter().map(|l| l.text()).collect::<Vec<_>>(),
        plain.iter().map(|l| l.text()).collect::<Vec<_>>()
    );
    assert!(
        color
            .iter()
            .flat_map(|l| &l.line.spans)
            .any(|s| s.style.fg.is_some())
    );
    assert!(
        plain
            .iter()
            .flat_map(|l| &l.line.spans)
            .all(|s| s.style.fg.is_none() && s.style.bg.is_none())
    );
    let unknown = layout(
        &Document::parse("```no-such-language\nfn main() {}\n```".into()),
        40,
        true,
    );
    assert!(unknown.iter().any(|l| l.text() == "fn main() {}"));
}
#[test]
fn wheel_scrolls_three_rows_and_search_input_does_not_move() {
    let mut app = app(&"paragraph\n\n".repeat(100));
    let mouse = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    app.mouse(mouse);
    assert_eq!(app.top, 3);
    key(&mut app, KeyCode::Char('/'));
    app.mouse(mouse);
    assert_eq!(app.top, 3);
}
#[test]
fn decoder_separates_fragmented_delayed_replies_and_preserves_keys() {
    let mut decoder = Decoder::default();
    for bytes in [
        b"j\x1b_Gi=4".as_slice(),
        b"000000001;OK\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\",
        "日本語".as_bytes(),
        b"\x1b[6;20;10tq",
    ] {
        decoder.feed(bytes);
    }
    let mut keys = String::new();
    let mut replies = 0;
    while let Some(event) = decoder.events.pop_front() {
        match event {
            InputEvent::User(Event::Key(k)) => {
                if let KeyCode::Char(c) = k.code {
                    keys.push(c);
                }
            }
            InputEvent::Reply(_) => replies += 1,
            _ => panic!("unexpected event"),
        }
    }
    assert_eq!(keys, "j日本語q");
    assert_eq!(replies, 3);
    decoder.feed(b"\x1b[200~q\nj\x1b[201~\x1b[<65;3;2M");
    assert!(
        matches!(decoder.events.pop_front(), Some(InputEvent::User(Event::Paste(s))) if s == "q\nj")
    );
    assert!(
        matches!(decoder.events.pop_front(), Some(InputEvent::User(Event::Mouse(m))) if m.kind == MouseEventKind::ScrollDown)
    );
}
#[test]
fn math_target_notation_generates_png_with_cell_sized_dimensions() {
    for tex in [
        r"\frac{1}{2}",
        r"\sqrt{x}",
        r"\sum_{i=1}^n i",
        r"\int_0^1 x dx",
        r"\alpha_1^2+\beta",
        r"\begin{matrix}1&2\\3&4\end{matrix}",
        r"\begin{pmatrix}1&2\\3&4\end{pmatrix}",
        r"\begin{bmatrix}1&2\\3&4\end{bmatrix}",
        r"\begin{aligned}a&=b+c\\d&=e\end{aligned}",
    ] {
        let image = math::render(&request(tex)).unwrap_or_else(|error| panic!("{tex}: {error}"));
        assert!(image.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(image.rows <= 12 && image.columns <= 78);
        assert_eq!(image.width % 10, 0);
        assert_eq!(image.height % 20, 0);
    }
    for tex in [
        r"\unknownfumi{x}",
        r"\input{/etc/passwd}",
        r"\write18{touch nope}",
        r"\def\a{x}\a",
    ] {
        assert!(math::render(&request(tex)).is_err(), "{tex}");
    }
    assert!(math::render(&request(&"x".repeat(math::MAX_TEX + 1))).is_err());
}
#[test]
fn math_overlay_preserves_source_position_and_search_jump() {
    let mut app = app(&format!(
        "before\n\n$$ \\frac{{1}}{{2}} $$\n\n{}",
        "after\n\n".repeat(30)
    ));
    let source = app.document.source.find("$$").unwrap();
    let image = Arc::new(math::render(&request(r"\frac{1}{2}")).unwrap());
    app.graphics_active = true;
    app.set_math(
        [(source, View::Ready(image.clone()))].into(),
        Status::Pending,
    );
    let row = app
        .lines
        .iter()
        .position(|l| l.image == Some((source, 1)))
        .unwrap();
    app.top = row;
    app.set_math([(source, View::Ready(image.clone()))].into(), Status::Idle);
    assert_eq!(app.top, row);
    query(&mut app, "frac");
    assert_eq!(app.search.matches.len(), 1);
    assert!(app.lines.iter().any(|l| l.text() == "[math]"));
    app.graphics_active = false;
    app.set_math(Default::default(), Status::Unavailable("画像非対応".into()));
    assert!(app.lines.iter().any(|l| l.text().contains("frac")));
}

#[test]
fn table_search_anchors_to_the_row_containing_the_cell() {
    let mut source = String::from("| A | B |\n|---|---|\n");
    for i in 0..50 {
        source.push_str(&format!("| a{i} | cell{i} |\n"));
    }
    let mut app = app(&source);
    query(&mut app, "cell40");
    assert!(
        app.lines[app.top..app.top + app.viewport()]
            .iter()
            .any(|line| line.text().contains("cell40"))
    );
    app.resize(20, 8);
    assert!(
        app.lines[app.top..app.top + app.viewport()]
            .iter()
            .any(|line| line.text().contains("cell40"))
    );
}

#[test]
fn image_row_offset_survives_a_width_change_and_generation() {
    let mut app = app(&format!(
        "$$ \\frac{{1}}{{2}} $$\n\n{}",
        "after\n\n".repeat(20)
    ));
    let image = Arc::new(math::render(&request(r"\frac{1}{2}")).unwrap());
    app.graphics_active = true;
    app.set_math([(0, View::Ready(image.clone()))].into(), Status::Pending);
    app.top = 1;
    app.resize(30, 8);
    app.set_math([(0, View::Ready(image))].into(), Status::Pending);
    assert_eq!(app.lines[app.top].image, Some((0, 1)));
}

#[test]
fn reference_destinations_have_independent_precise_highlights() {
    let mut app = app("[aaaa][ref] rest [url][ref]\n\n[ref]: https://example.org/aaaa");
    app.resize(120, 8);
    query(&mut app, "aaaa");
    assert_eq!(app.search.matches.len(), 3);
    key(&mut app, KeyCode::Char('n'));
    let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let selected: String = terminal.backend().buffer().content[..120]
        .iter()
        .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
        .map(|cell| cell.symbol())
        .collect();
    assert_eq!(selected, "aaaa");
    assert!(
        !terminal.backend().buffer()[(1, 0)]
            .modifier
            .contains(Modifier::REVERSED)
    );
    query(&mut app, "rest");
    assert_eq!(app.search.matches.len(), 1);
}

#[test]
fn color_never_removes_tex_color_commands_from_the_resulting_pixels() {
    let mut req = request(r"\color{red}{x}");
    req.monochrome = true;
    let image = math::render(&req).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(image.png))
        .read_info()
        .unwrap();
    let mut bytes = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut bytes).unwrap();
    let (pixels, remainder) = bytes[..info.buffer_size()].as_chunks::<4>();
    assert!(
        remainder.is_empty(),
        "RGBA画像のバイト数は4の倍数であること"
    );
    assert!(
        pixels
            .iter()
            .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
    );
}

#[test]
fn generation_update_at_tiny_size_does_not_reuse_a_tiny_layout_after_expansion() {
    let mut app = app("$$ x $$\n\n日本語の文章を通常の幅で表示する。");
    let expected: Vec<_> = app.lines.iter().map(|line| line.text()).collect();
    app.resize(10, 2);
    app.set_math(Default::default(), Status::Pending);
    app.resize(40, 8);
    assert_eq!(
        app.lines
            .iter()
            .filter(|line| !line.text().starts_with("[math:"))
            .map(|line| line.text())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn highlighting_preserves_source_ranges_inside_nested_code() {
    let doc = Document::parse("> ```rust\n> fn main() {\n>     let value = 1;\n> }\n> ```".into());
    let colored = layout(&doc, 40, true);
    let text = layout(&doc, 40, false);
    assert_eq!(
        colored
            .iter()
            .map(|line| &line.mappings)
            .collect::<Vec<_>>(),
        text.iter().map(|line| &line.mappings).collect::<Vec<_>>()
    );
    assert_eq!(
        colored.iter().map(|line| &line.source).collect::<Vec<_>>(),
        text.iter().map(|line| &line.source).collect::<Vec<_>>()
    );
}

#[test]
fn empty_headers_have_parentheses_in_grid_and_vertical_tables() {
    let doc = Document::parse(
        "| | B |\n|---|---|\n| value | a long value that needs a narrow layout |".into(),
    );
    let wide = layout(&doc, 80, false);
    let narrow = layout(&doc, 20, false);
    assert!(wide[0].text().starts_with('┌'));
    assert!(wide.iter().any(|line| line.text().contains("(列1)")));
    assert!(narrow.iter().any(|line| line.text() == "(列1): value"));
    assert!(search::find(&search_segments(&doc), "(列1)").is_empty());
}

#[test]
fn empty_table_message_is_centered_inside_a_merged_row() {
    let doc = Document::parse("| A | B |\n|---|---|".into());
    let lines = layout(&doc, 80, false);
    assert_eq!(
        lines.iter().map(|line| line.text()).collect::<Vec<_>>(),
        [
            "┌─────┬────┐",
            "│ A   │ B  │",
            "├─────┴────┤",
            "│ (no row) │",
            "└──────────┘",
        ]
    );
    let width = lines[0].text().width();
    assert!(lines.iter().all(|line| line.text().width() == width));
    assert!(layout(&doc, width, false)[0].text().starts_with('┌'));
    assert_eq!(
        layout(&doc, width - 1, false)
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>(),
        ["A", "B", "(no row)"]
    );
    assert!(search::find(&search_segments(&doc), "(no row)").is_empty());
}

#[test]
fn empty_table_centering_respects_unicode_cell_width_and_one_column() {
    for markdown in [
        "| A |\n|---|",
        "| 名称 | 数量 |\n|---|---|",
        "| | | |\n|---|---|---|",
    ] {
        let doc = Document::parse(markdown.into());
        let lines = layout(&doc, 80, false);
        let width = lines[0].text().width();
        let text = lines
            .iter()
            .find(|line| line.text().contains("(no row)"))
            .unwrap()
            .text();
        assert!(text.starts_with('│') && text.ends_with('│'));
        let (left, right) = text.split_once("(no row)").unwrap();
        assert!(left.width().abs_diff(right.width()) <= 1);
        assert!(lines.iter().all(|line| line.text().width() == width));
    }
}
