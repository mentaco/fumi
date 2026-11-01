use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fumi::{
    app::App,
    document::Document,
    math::{self, Request, Status, View},
    source::decode,
};
use std::sync::Arc;
fn key(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}
fn app(source: &str) -> App {
    let mut app = App::new(Document::parse(source.into()), "v2.md", false);
    app.resize(40, 8);
    app
}
/// 段落を `count` 個並べた文書。`label` と番号の段落が、番号の2倍の行から始まる。
fn paragraphs(label: &str, count: usize) -> String {
    (0..count).map(|i| format!("{label}{i}\n\n")).collect()
}
fn top_text(app: &App) -> String {
    app.lines()[app.top()].text()
}
fn query(app: &mut App, text: &str) {
    key(app, KeyCode::Char('/'));
    app.paste(text);
    key(app, KeyCode::Enter);
}

#[test]
fn decoding_is_shared_by_every_input_source() {
    assert_eq!(
        decode(b"\xef\xbb\xbf# a\r\nb\r\n".to_vec()).unwrap(),
        "# a\nb\n"
    );
    assert_eq!(decode(Vec::new()).unwrap(), "");
    // 先頭以外のBOMは本文として残す。
    assert_eq!(decode("a\u{feff}".into()).unwrap(), "a\u{feff}");
    assert!(decode(b"\xff".to_vec()).is_err());
}

#[test]
fn replacing_keeps_the_top_source_line_number_and_clamps_to_the_end() {
    let mut app = app(&paragraphs("old", 100));
    let row = app
        .lines()
        .iter()
        .position(|l| l.text() == "old40")
        .unwrap();
    app.scroll_to(row);
    let generation = app.generation();
    app.replace_document(Document::parse(paragraphs("new", 100)));
    assert_eq!(app.generation(), generation + 1);
    assert_eq!(top_text(&app), "new40");
    // 行番号の位置が空行なら、その空行の表示行か、後ろで最も近い本文の行へ移る。
    app.replace_document(Document::parse(format!(
        "{}\n{}",
        paragraphs("x", 40),
        paragraphs("y", 60)
    )));
    let visible = app.lines()[app.top()..]
        .iter()
        .map(|l| l.text())
        .find(|text| !text.is_empty());
    assert_eq!(visible.as_deref(), Some("y0"));
    assert_eq!(app.lines()[app.top() - 1].text(), "x39");
    app.replace_document(Document::parse(paragraphs("short", 10)));
    assert_eq!(app.top(), app.max_top());
    assert_eq!(app.percentage(), 100);
    app.replace_document(Document::parse(String::new()));
    assert_eq!(app.top(), 0);
    assert!(app.status().contains("100%"));
}

#[test]
fn replacing_keeps_the_search_query_and_recomputes_matches_without_jumping() {
    let mut app = app(&format!("{}target\n", paragraphs("p", 30)));
    query(&mut app, "target");
    assert_eq!(app.search().matches.len(), 1);
    let top = app.top();
    app.replace_document(Document::parse(format!(
        "{}target\n\ntarget again\n\ntarget\n",
        paragraphs("q", 30)
    )));
    assert_eq!(app.search().query, "target");
    assert_eq!(app.search().matches.len(), 3);
    assert_eq!(app.top(), top, "再読込で一致へ移動しない");
    assert!(app.status().contains("/target 1/3"));
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(app.search().current, Some(1));
    app.replace_document(Document::parse("no hit".into()));
    assert!(app.search().matches.is_empty());
    assert_eq!(app.search().current, None);
    assert!(app.status().contains("見つかりません"));
}

#[test]
fn replacing_during_search_input_refreshes_candidates_and_keeps_the_input() {
    let mut app = app("abc abc");
    key(&mut app, KeyCode::Char('/'));
    app.paste("abc");
    assert_eq!(app.search().candidates.len(), 2);
    app.replace_document(Document::parse("abc abc abc".into()));
    assert_eq!(app.search().input.as_deref(), Some("abc"));
    assert_eq!(app.search().candidates.len(), 3);
    assert!(app.search().query.is_empty());
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.search().matches.len(), 3);
}

#[test]
fn replacing_discards_math_results_of_the_old_document() {
    let mut app = app("$$ x $$\n\nbody");
    let image = Arc::new(
        math::render(&Request {
            tex: "x".into(),
            width: 38,
            cell: (10, 20),
            foreground: [235; 3],
            monochrome: false,
        })
        .unwrap(),
    );
    app.enable_graphics();
    app.set_math([(0, View::Ready(image))].into(), Status::Pending);
    assert!(app.lines().iter().any(|l| l.image.is_some()));
    // 同じ位置に別の数式がある文書へ替えても、旧文書の画像を配置しない。
    app.replace_document(Document::parse("$$ y $$\n\nbody".into()));
    assert!(app.lines().iter().all(|l| l.image.is_none()));
    assert!(app.lines().iter().any(|l| l.text() == "[math: 生成中]"));
    assert!(app.view_state().images.unwrap().is_empty());
}

#[test]
fn replacing_before_layout_or_at_a_tiny_size_lays_out_the_new_document() {
    let mut fresh = App::new(Document::parse("old".into()), "v2.md", false);
    fresh.replace_document(Document::parse("new".into()));
    fresh.resize(40, 8);
    assert_eq!(top_text(&fresh), "new");
    let mut app = app("old text");
    key(&mut app, KeyCode::Char('?'));
    app.resize(10, 2);
    app.replace_document(Document::parse("日本語の文章を通常の幅で表示する。".into()));
    app.resize(40, 8);
    assert_eq!(
        app.lines().iter().map(|l| l.text()).collect::<Vec<_>>(),
        ["日本語の文章を通常の幅で表示する。"]
    );
}

#[test]
fn replacing_keeps_help_open_and_leaves_its_position_alone() {
    let mut app = app(&paragraphs("p", 30));
    key(&mut app, KeyCode::Char('?'));
    key(&mut app, KeyCode::Char('j'));
    let status = app.status();
    app.replace_document(Document::parse(paragraphs("q", 30)));
    assert!(app.help_visible());
    assert_eq!(app.status(), status);
    key(&mut app, KeyCode::Esc);
    assert_eq!(top_text(&app), "q0");
}
