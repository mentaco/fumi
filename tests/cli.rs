use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fumi"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_and_version_do_not_need_tty_or_file() {
    for arg in ["--help", "-h", "--version", "-V"] {
        let output = run(&[arg]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(String::from_utf8_lossy(&output.stdout).contains("fumi"));
        assert!(!output.stdout.contains(&0x1b));
    }
}

#[test]
fn syntax_errors_precede_tty_checks() {
    for args in [
        vec![],
        vec!["a", "b"],
        vec!["--watch", "a"],
        vec!["--graphics", "unknown", "a"],
        vec!["--color", "wrong", "a"],
        vec!["-"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.contains(&0x1b));
    }
}

#[test]
fn redirected_output_has_no_terminal_controls() {
    for args in [
        vec!["missing.md"],
        vec!["--plain", "missing.md"],
        vec!["--graphics", "off", "missing.md"],
        vec!["--color", "always", "missing.md"],
        vec!["--", "-memo.md"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("TTY"));
    }
    let output = run(&["--unknown\x1b]52;secret\x07"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stderr.contains(&0x1b));
    assert!(!output.stderr.contains(&7));
}
