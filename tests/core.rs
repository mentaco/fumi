use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mdview::{
    app::{Action, App},
    cli::ColorMode,
    document::{Document, Kind, Node},
    layout::{layout, truncate_middle},
    source::{MAX_BYTES, bounded_read, load, sanitize},
};
use ratatui::{Terminal, backend::TestBackend};
use std::{ffi::OsStr, io::Write};
use unicode_width::UnicodeWidthStr;

fn text(source: &str, width: usize) -> String {
    layout(&Document::parse(source.into()), width, false)
        .iter()
        .map(|l| l.text())
        .collect::<Vec<_>>()
        .join("\n")
}
fn maths(nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .flat_map(|n| {
            let mut result = if let Kind::Math(s) = &n.kind {
                vec![s.clone()]
            } else {
                Vec::new()
            };
            result.extend(maths(&n.children));
            result
        })
        .collect()
}
fn key(app: &mut App, code: KeyCode) -> Action {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn ctrl(app: &mut App, c: char) -> Action {
    app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}
fn long_app() -> App {
    let mut app = App::new(
        Document::parse(
            (0..120)
                .map(|i| format!("行{i:03} 日本語の文章を読み進めます。\n\n"))
                .collect(),
        ),
        "長い日本語のファイル.md",
        false,
    );
    app.resize(40, 10);
    app
}

#[test]
fn markdown_visible_content() {
    let rendered = text(include_str!("fixtures/representative.md"), 120);
    for expected in [
        "# 日本語",
        "## 見出し2",
        "太字",
        "│ │ 二段目",
        "• [x]",
        "9. 開始番号9",
        "10. 次は10",
        "[rust]",
        "[image: 日本語の図] <./missing.png>",
        "[image] <https://example.com/image.png>",
        "[row 1]",
        "列3: (empty)",
        "名前: mdview",
        "用途: 日本語",
        "<div onclick=",
        "$x_i^2 + **literal**$",
        "\\frac{a_b}{c^2}",
        "**TeXはMarkdownにしない**",
        "indented $code$ **literal**",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}\n{rendered}"
        );
    }
    assert_eq!(text("<https://example.com>", 80), "https://example.com");
    assert_eq!(
        text("[参照][id]\n\n[id]: https://example.com", 80),
        "参照 <https://example.com>"
    );
}

#[test]
fn math_boundaries_and_contexts() {
    for (source, expected) in [
        ("$x_i^2$", vec!["$x_i^2$"]),
        ("$x**y**_z$", vec!["$x**y**_z$"]),
        ("$ x $ / $100 and $200 / $$$x$$$ / $x$$", vec![]),
        ("\\$x$", vec![]),
        ("\\\\$x$", vec!["$x$"]),
        ("$x\\$y$", vec!["$x\\$y$"]),
        ("$x\ny$", vec![]),
        ("`$x$`\n\n```\n$x$\n```\n\n    $x$", vec![]),
        ("<span title='$x$'>text</span> [a](https://a/$x$)", vec![]),
        ("[$x$](https://a)", vec!["$x$"]),
        ("$$ x_y $$", vec!["$$ x_y $$"]),
        ("$$\nx_y\n\n**z**\n$$", vec!["$$\nx_y\n\n**z**\n$$"]),
        ("$$\n$$\n\n$$\nnever closed\n\nlast", vec![]),
        ("before\n$$ x_y $$\n\nafter", vec![]),
        ("$$ x_y $$\nafter", vec![]),
        ("> $$ x_y $$\n\n- $$ z_w $$", vec![]),
        ("    $$ x $$", vec![]),
        ("   $$ x $$", vec!["   $$ x $$"]),
        (
            "$$\n- x\n```\n**TeX**\n$$",
            vec!["$$\n- x\n```\n**TeX**\n$$"],
        ),
        ("$$$x$$$", vec![]),
        ("$$\nx\n$$\n\n$x$", vec!["$$\nx\n$$", "$x$"]),
    ] {
        let doc = Document::parse(source.into());
        assert_eq!(maths(&doc.nodes), expected, "{source:?}");
        for n in &doc.nodes {
            assert!(doc.source.is_char_boundary(n.source.start));
            assert!(n.source.end <= doc.source.len());
        }
    }
}

#[test]
fn unicode_widths_and_source_ranges() {
    let source = include_str!("fixtures/representative.md");
    let doc = Document::parse(source.into());
    for width in [20, 40, 80, 120] {
        let lines = layout(&doc, width, false);
        for line in &lines {
            assert!(
                line.text().width() <= width,
                "width {width}: {:?}",
                line.text()
            );
            assert!(doc.source.is_char_boundary(line.source.start));
            assert!(doc.source.is_char_boundary(line.source.end));
            for s in &line.line.spans {
                assert!(s.style.fg.is_none() && s.style.bg.is_none());
            }
        }
    }
    let source = "日".repeat(37) + "e\u{301}か\u{3099}👩‍💻🇯🇵👨‍👩‍👧‍👦";
    let rendered = text(&source, 20).replace('\n', "");
    assert_eq!(rendered, source);
    // Markdownの装飾境界をまたぐ書記素も、端末上で一つとして扱う。
    let split_emoji = format!("{}👩**&#x200d;💻** z", "a".repeat(18));
    assert_eq!(text(&split_emoji, 20), format!("{}👩‍💻\nz", "a".repeat(18)));
    assert_eq!(text("one two three", 8), "one two\nthree");
    assert_eq!(
        text("```\na\tb\n123456789012345678901234\n```", 20),
        "a   b\n12345678901234567890\n↪ 1234"
    );
}

#[test]
fn tables_retain_empty_headers_cells_and_no_rows() {
    assert_eq!(
        text("| | A |\n|---|---|\n| | v |", 80),
        "[row 1]\n列1: (empty)\nA: v"
    );
    assert_eq!(text("| A | B |\n|---|---|", 80), "A\nB\n(no rows)");
}

#[test]
fn lists_spacing_and_narrow_nesting() {
    assert_eq!(text("- a\n- b", 80), "• a\n• b");
    assert_eq!(text("- a\n\n- b", 80), "• a\n\n• b");
    assert_eq!(
        text("- 123456789 abcdefghij", 12),
        "• 123456789\n  abcdefghij"
    );
    let nested = "> ".repeat(20) + "本文を消さない";
    assert!(text(&nested, 20).contains("本文を消さない"));
}

#[test]
fn code_retains_internal_and_trailing_blank_lines() {
    assert_eq!(text("```\na\n\n```", 80), "a\n");
    assert_eq!(text("```\na\n\nb\n\n\n```", 80), "a\n\nb\n\n");
}

#[test]
fn scrolling_help_and_pending_g() {
    let mut app = long_app();
    assert_eq!(app.percentage(), 0);
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.top, 1);
    ctrl(&mut app, 'd');
    assert_eq!(app.top, 5);
    ctrl(&mut app, 'f');
    assert_eq!(app.top, 13);
    ctrl(&mut app, 'u');
    assert_eq!(app.top, 9);
    ctrl(&mut app, 'b');
    assert_eq!(app.top, 1);
    key(&mut app, KeyCode::Char('G'));
    assert_eq!(app.top, app.max_top());
    assert_eq!(app.percentage(), 100);
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.top, app.max_top());
    key(&mut app, KeyCode::Char('g'));
    assert!(app.pending_g);
    assert!(app.status().contains("g待ち"));
    assert_eq!(key(&mut app, KeyCode::Esc), Action::None);
    assert!(!app.pending_g);
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(app.top, 0);
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.top, 1);
    key(&mut app, KeyCode::Char('?'));
    assert!(app.help);
    key(&mut app, KeyCode::Char('G'));
    assert_eq!(app.top, 1);
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(key(&mut app, KeyCode::Char('q')), Action::None);
    assert!(!app.help);
    assert_eq!(key(&mut app, KeyCode::Char('q')), Action::Exit(0));
    assert_eq!(ctrl(&mut app, 'c'), Action::Exit(130));
    assert_eq!(ctrl(&mut app, 'z'), Action::Suspend);
}

#[test]
fn resize_keeps_source_and_small_mode_preserves_position() {
    let mut app = App::new(Document::parse("日本語abc".repeat(300)), "文書.md", false);
    app.resize(80, 12);
    for _ in 0..10 {
        key(&mut app, KeyCode::Char('j'));
    }
    let anchor = app.lines[app.top].source.start;
    app.resize(40, 12);
    assert!(app.lines[app.top].source.contains(&anchor));
    let top = app.top;
    key(&mut app, KeyCode::Char('?'));
    app.resize(19, 3);
    assert!(!app.help);
    assert_eq!(app.top, top);
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.top, top);
    app.resize(40, 12);
    assert_eq!(app.top, top);
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() == "%")
    );
}

#[test]
fn empty_document_status_and_color_priority() {
    let mut app = App::new(Document::parse(String::new()), "empty.md", false);
    app.resize(80, 24);
    assert_eq!(app.lines[0].text(), "(empty)");
    assert_eq!(app.percentage(), 100);
    assert!(!ColorMode::Auto.enabled(Some(OsStr::new("1")), "xterm"));
    assert!(ColorMode::Auto.enabled(Some(OsStr::new("")), "xterm"));
    assert!(ColorMode::Always.enabled(Some(OsStr::new("1")), "xterm"));
    assert!(!ColorMode::Never.enabled(None, "xterm"));
    for width in [0, 1, 4, 20, 40] {
        assert!(truncate_middle("長いファイル名👩‍💻.md", width).width() <= width);
    }
}

#[test]
fn loading_is_bounded_normalized_and_read_only() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let bytes = b"\xef\xbb\xbf# title\r\ntext\r\n";
    file.write_all(bytes).unwrap();
    assert_eq!(load(file.path()).unwrap(), "# title\ntext\n");
    assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("alias.md");
    std::os::unix::fs::symlink(file.path(), &link).unwrap();
    assert_eq!(load(&link).unwrap(), "# title\ntext\n");
    assert!(bounded_read(&b"12345"[..], 4).is_err());
    assert_eq!(bounded_read(&b"1234"[..], 4).unwrap(), b"1234");
    file.as_file().set_len(MAX_BYTES + 1).unwrap();
    assert!(load(file.path()).is_err());
    let mut invalid = tempfile::NamedTempFile::new().unwrap();
    invalid.write_all(b"\xff").unwrap();
    assert!(load(invalid.path()).is_err());
    assert!(load(tempfile::tempdir().unwrap().path()).is_err());
}

#[test]
fn controls_are_visible_and_never_terminal_commands() {
    let source = "before\x1b]52;c;secret\x07after\u{009b}31m\0\x1b[31m";
    let rendered = text(source, 120);
    assert!(!rendered.chars().any(|c| c.is_control() && c != '\n'));
    assert!(rendered.contains("␛]52;c;secret"));
    assert!(rendered.contains("\\u{009B}"));
    assert!(rendered.contains("␀"));
    assert_eq!(sanitize("a\x1b\0\n\t"), "a␛␀\\u{000A}\\u{0009}");
}

#[test]
fn resize_with_inline_code_and_link_preserves_content() {
    for source in [
        format!("`{}`", "日本語abc".repeat(200)),
        format!("[link](https://example.com/{})", "abcXYZ".repeat(200)),
    ] {
        let mut app = App::new(Document::parse(source), "inline.md", false);
        app.resize(80, 10);
        for _ in 0..8 {
            key(&mut app, KeyCode::Char('j'));
        }
        let anchor = app.lines[app.top].source.start;
        assert!(anchor > 20);
        app.resize(40, 10);
        assert!(app.lines[app.top].source.contains(&anchor));
    }
}

#[test]
fn representative_display_snapshots() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    for width in [20, 40, 80, 120] {
        let actual = text(include_str!("fixtures/representative.md"), width) + "\n";
        let path = root.join(format!("representative-{width}.txt"));
        if std::env::var_os("MDVIEW_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(&path, &actual).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            actual,
            "width={width}"
        );
    }
}
