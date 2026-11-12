use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fumi::{
    app::{Action, App},
    document::Document,
    math::{self, Request, Status, View},
    reload::{Reloader, WATCH_INTERVAL, reason, stamp},
    source::{MAX_BYTES, decode, read_stream},
};
use std::{
    io::Write,
    os::fd::AsFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
fn key(app: &mut App, code: KeyCode) -> Action {
    app.key(KeyEvent::new(code, KeyModifiers::NONE))
}
/// 再読込の結果が届くまで待つ。
fn reloaded(reloader: &mut Reloader) -> std::io::Result<Document> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(result) = reloader.poll() {
            return result;
        }
        assert!(Instant::now() < until, "再読込の待ちタイムアウト");
        thread::sleep(Duration::from_millis(5));
    }
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
fn stream_reads_until_eof_across_chunks() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let feeder = thread::spawn(move || {
        for chunk in data.chunks(7000) {
            writer.write_all(chunk).unwrap();
        }
    });
    assert_eq!(
        read_stream(reader.as_fd(), MAX_BYTES, || false).unwrap(),
        expected
    );
    feeder.join().unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(writer);
    assert!(
        read_stream(reader.as_fd(), MAX_BYTES, || false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn stream_stops_on_interrupt_while_the_writer_is_idle() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"partial").unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&flag);
    let start = Instant::now();
    let timer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        setter.store(true, Ordering::Relaxed);
    });
    let error =
        read_stream(reader.as_fd(), MAX_BYTES, || flag.load(Ordering::Relaxed)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    assert!(start.elapsed() < Duration::from_secs(2));
    timer.join().unwrap();
    drop(writer);
}

#[test]
fn stream_rejects_input_over_the_limit_without_reading_to_eof() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"12345").unwrap();
    // 書き手を開いたままでも、上限を超えた時点でエラーにする。
    assert!(read_stream(reader.as_fd(), 4, || false).is_err());
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"1234").unwrap();
    drop(writer);
    assert_eq!(read_stream(reader.as_fd(), 4, || false).unwrap(), b"1234");
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
            inline: false,
        })
        .unwrap(),
    );
    app.enable_graphics();
    app.set_images([(0, View::Ready(image))].into(), Status::Pending);
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

#[test]
fn capital_r_requests_a_reload_only_in_normal_viewing() {
    let mut app = app(&paragraphs("p", 30));
    assert_eq!(key(&mut app, KeyCode::Char('R')), Action::Reload);
    assert_eq!(key(&mut app, KeyCode::Char('r')), Action::None);
    key(&mut app, KeyCode::Char('?'));
    assert_eq!(key(&mut app, KeyCode::Char('R')), Action::None);
    assert!(app.help_visible());
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('/'));
    assert_eq!(key(&mut app, KeyCode::Char('R')), Action::None);
    assert_eq!(app.search().input.as_deref(), Some("R"));
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(key(&mut app, KeyCode::Char('R')), Action::Reload);
    assert!(!app.pending_g());
    app.resize(10, 2);
    assert_eq!(key(&mut app, KeyCode::Char('R')), Action::None);
}

#[test]
fn notice_lasts_until_the_next_key_and_is_sanitized() {
    let mut app = app("body");
    app.resize(80, 8);
    app.set_notice("再読込失敗: \u{1b}]52;x\u{7}");
    let status = app.status();
    assert!(status.contains("再読込失敗: ␛]52;x\\u{0007}"), "{status}");
    assert!(!status.contains('\u{1b}'));
    key(&mut app, KeyCode::Char('?'));
    assert!(!app.status().contains("再読込"));
    key(&mut app, KeyCode::Esc);
    app.set_notice("再読込");
    // ヘルプ表示中は本文のお知らせを出さない。
    key(&mut app, KeyCode::Char('?'));
    app.set_notice("再読込");
    assert!(!app.status().contains("再読込"));
    key(&mut app, KeyCode::Esc);
    app.set_notice("再読込");
    assert!(app.status().contains("再読込"));
    // 狭い画面では、ヘルプの案内より先にお知らせを残す。
    app.resize(30, 8);
    assert!(
        app.status().contains("TEXT 100% 再読込"),
        "{}",
        app.status()
    );
    key(&mut app, KeyCode::Char('j'));
    assert!(!app.status().contains("再読込"));
}

#[test]
fn reloader_reads_the_path_again_and_reports_failures() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "first\r\n").unwrap();
    let mut reloader = Reloader::new(path.clone());
    assert!(reloader.poll().is_none());
    reloader.request();
    assert!(reloader.is_loading());
    assert_eq!(reloaded(&mut reloader).unwrap().source, "first\n");
    assert!(!reloader.is_loading());
    std::fs::remove_file(&path).unwrap();
    reloader.request();
    assert_eq!(
        reason(&reloaded(&mut reloader).unwrap_err()),
        "ファイルが見つかりません"
    );
    std::fs::write(&path, b"\xff").unwrap();
    reloader.request();
    assert_eq!(
        reason(&reloaded(&mut reloader).unwrap_err()),
        "UTF-8が不正です"
    );
    // 削除後に同じパスへ戻ったファイルも読める。
    std::fs::write(&path, "back").unwrap();
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "back");
}

#[test]
fn reload_requested_while_loading_reads_again_after_the_first_load() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "old").unwrap();
    let mut reloader = Reloader::new(path.clone());
    reloader.request();
    std::fs::write(&path, "new").unwrap();
    // 1回目の読込が古い内容を読んでいても、その結果は捨てて読み直す。
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "new");
    assert!(!reloader.is_loading());
}

#[test]
fn reloader_resolves_symlinks_and_atomic_replacements_each_time() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.md");
    let second = directory.path().join("second.md");
    let link = directory.path().join("link.md");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    std::os::unix::fs::symlink(&first, &link).unwrap();
    let mut reloader = Reloader::new(link.clone());
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "first");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "second");
    let temporary = directory.path().join(".second.md.tmp");
    std::fs::write(&temporary, "saved").unwrap();
    std::fs::rename(&temporary, &second).unwrap();
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "saved");
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
}

/// `--watch` の確認を間隔ごとに進め、読み直しを始めたら結果を返す。最大 `checks` 回確かめる。
fn watched(
    reloader: &mut Reloader,
    now: &mut Instant,
    checks: usize,
) -> Option<std::io::Result<Document>> {
    for _ in 0..checks {
        *now += WATCH_INTERVAL;
        if reloader.check(*now) {
            return Some(reloaded(reloader));
        }
    }
    None
}

#[test]
fn watch_reloads_after_the_change_is_stable_for_one_more_check() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "first").unwrap();
    let mut now = Instant::now();
    let mut reloader = Reloader::watching(path.clone(), stamp(&path), now);
    // 変化がなければ読まない。間隔より早い確認では状態を見ない。
    assert!(watched(&mut reloader, &mut now, 5).is_none());
    std::fs::write(&path, "second!").unwrap();
    assert!(!reloader.check(now + WATCH_INTERVAL / 2));
    now += WATCH_INTERVAL;
    assert!(!reloader.check(now), "最初に変化を見つけた確認では読まない");
    // 次の確認までに書き換わったら、さらに次の確認まで待つ。
    std::fs::write(&path, "third!!!").unwrap();
    now += WATCH_INTERVAL;
    assert!(!reloader.check(now));
    assert_eq!(
        watched(&mut reloader, &mut now, 1).unwrap().unwrap().source,
        "third!!!"
    );
    assert!(
        watched(&mut reloader, &mut now, 5).is_none(),
        "読んだ後は同じ状態で読み直さない"
    );
}

#[test]
fn watch_detects_same_size_rewrites_by_modification_time() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "aaaa").unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
        .unwrap();
    let mut now = Instant::now();
    let mut reloader = Reloader::watching(path.clone(), stamp(&path), now);
    std::fs::write(&path, "bbbb").unwrap();
    assert_eq!(
        watched(&mut reloader, &mut now, 3).unwrap().unwrap().source,
        "bbbb"
    );
}

#[test]
fn watch_retries_only_on_the_next_change_after_a_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "first").unwrap();
    let mut now = Instant::now();
    let mut reloader = Reloader::watching(path.clone(), stamp(&path), now);
    std::fs::remove_file(&path).unwrap();
    let error = watched(&mut reloader, &mut now, 3).unwrap().unwrap_err();
    assert_eq!(reason(&error), "ファイルが見つかりません");
    assert!(
        watched(&mut reloader, &mut now, 5).is_none(),
        "削除されたままなら再試行しない"
    );
    std::fs::write(&path, b"\xff").unwrap();
    assert!(watched(&mut reloader, &mut now, 3).unwrap().is_err());
    assert!(watched(&mut reloader, &mut now, 5).is_none());
    // atomic saveによる置換はinodeの変化として見つかる。
    let temporary = directory.path().join(".doc.md.tmp");
    std::fs::write(&temporary, "saved").unwrap();
    std::fs::rename(&temporary, &path).unwrap();
    assert_eq!(
        watched(&mut reloader, &mut now, 3).unwrap().unwrap().source,
        "saved"
    );
}

#[test]
fn manual_reload_also_updates_the_watched_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("doc.md");
    std::fs::write(&path, "first").unwrap();
    let mut now = Instant::now();
    let mut reloader = Reloader::watching(path.clone(), stamp(&path), now);
    std::fs::write(&path, "second").unwrap();
    reloader.request();
    assert_eq!(reloaded(&mut reloader).unwrap().source, "second");
    // 手動で読んだ内容を、自動再読込で読み直さない。
    assert!(watched(&mut reloader, &mut now, 5).is_none());
    // 手動の読込中は状態を比べず、読み直しを重ねない。
    std::fs::write(&path, "third!").unwrap();
    reloader.request();
    assert!(!reloader.check(now + WATCH_INTERVAL * 10));
    assert_eq!(reloaded(&mut reloader).unwrap().source, "third!");
    let mut manual = Reloader::new(path.clone());
    std::fs::write(&path, "fourth!!").unwrap();
    assert!(
        watched(&mut manual, &mut now, 5).is_none(),
        "--watchなしでは監視しない"
    );
}
