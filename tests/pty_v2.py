#!/usr/bin/env python3
"""v2の標準入力（`fumi -`）をPTYで検証する。キー入力は制御端末（/dev/tty）から送る。"""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
from pty_smoke import BINARY, Session, case
from pty_v1 import V1, commands, started_kitty

DOCUMENT = "\ufeff# 標準入力\r\n\r\n本文の行\r\n".encode("utf-8")


def piped(data, args=("-",), keep_open=False, **kwargs):
    """`data` をパイプで標準入力へ渡すセッション。`keep_open` なら書き手を閉じずにEOFを送らない。"""
    read_fd, write_fd = os.pipe()
    session = Session(list(args), stdin_fd=read_fd, close_fds=(write_fd,), **kwargs)
    os.close(read_fd)

    def feed():
        try:
            view = memoryview(data)
            while view:
                view = view[os.write(write_fd, view):]
        except BrokenPipeError:
            pass
        finally:
            if not keep_open:
                os.close(write_fd)

    thread = threading.Thread(target=feed, daemon=True)
    thread.start()
    return session, thread, write_fd


def assert_not_started(session):
    assert b"\x1b[?1049h" not in session.data, bytes(session.data)


def pipe_display_and_keys():
    session, thread, _ = piped(DOCUMENT)
    try:
        session.started()
        screen = "\n".join(session.screen())
        assert "[stdin]" in screen, screen
        assert "標準入力" in screen and "本文の行" in screen, screen
        assert "\ufeff" not in session.visible()
        # キーを押さずに待つ間も、リサイズに追従する（入力待ちで止まらない）。
        session.resize(50, 12)
        session.pump(0.3)
        assert "[stdin]" in session.screen()[11], session.screen()
        session.send("/本文\r".encode())
        assert "/本文 1/1" in session.screen()[-1], session.screen()[-1]
        session.send(b"?G")
        assert any("fumi -" in row for row in session.screen()), session.screen()
        session.send(b"q")
        session.wait(0)
    finally:
        thread.join()
        session.close()


def redirect_and_empty():
    with tempfile.TemporaryDirectory() as directory:
        for content, expected in [(DOCUMENT, "本文の行"), (b"", "(empty)")]:
            path = Path(directory) / "input.md"
            path.write_bytes(content)
            fd = os.open(path, os.O_RDONLY)
            session = Session(["-"], stdin_fd=fd)
            os.close(fd)
            try:
                session.started()
                assert any(expected in row for row in session.screen())
                session.send(b"q")
                session.wait(0)
            finally:
                session.close()
            assert path.read_bytes() == content


def suspension():
    session, thread, _ = piped(DOCUMENT)
    try:
        session.started()
        session.send(b"\x1a")
        session.stopped()
        os.kill(session.pid, signal.SIGCONT)
        session.pump(0.2)
        assert session.data.count(b"\x1b[?1049h") == 2
        # 中断再開後も、キー入力は制御端末から読む。
        session.send("/行\r".encode())
        assert "/行 1/1" in session.screen()[-1], session.screen()[-1]
        session.send(b"q")
        session.wait(0)
    finally:
        thread.join()
        session.close()


def math_images():
    session, thread, _ = piped(V1.read_bytes())
    try:
        # 端末の画像対応の確認の応答も、キー入力と同じ制御端末から読む。
        started_kitty(session)
        assert any(fields.get(b"a") == b"t" for fields, _ in commands(session)), "数式画像が転送されていない"
        session.send(b"q")
        session.wait(0)
    finally:
        thread.join()
        session.close()


def startup_errors():
    session = Session(["-"])
    try:
        session.wait(1)
        assert_not_started(session)
        assert "パイプかリダイレクト" in session.visible()
    finally:
        session.close()
    cases = [
        ({"stdout_tty": False}, DOCUMENT, None),
        ({}, b"\xff", "[stdin]: UTF-8が不正です"),
        ({}, b"x" * (32 * 1024 * 1024 + 1), "[stdin]: 文書が32 MiBの上限を超えています"),
    ]
    for kwargs, data, message in cases:
        session, thread, _ = piped(data, **kwargs)
        try:
            session.wait(1)
            assert_not_started(session)
            if message:
                assert message in session.visible(), session.visible()
        finally:
            thread.join()
            session.close()


def interrupted_while_reading():
    for send, code in [(b"\x03", 130), (signal.SIGTERM, 143)]:
        session, thread, write_fd = piped(b"partial", keep_open=True)
        try:
            session.pump(0.2)
            if isinstance(send, bytes):
                session.send(send)
            else:
                os.kill(session.pid, send)
            session.wait(code)
            assert_not_started(session)
        finally:
            thread.join()
            os.close(write_fd)
            session.close()


def without_controlling_terminal():
    master, slave = os.openpty()
    try:
        process = subprocess.Popen(
            [BINARY, "-"],
            stdin=subprocess.PIPE,
            stdout=slave,
            stderr=subprocess.PIPE,
            start_new_session=True,
            env={**os.environ, "TERM": "xterm-256color"},
        )
        _, stderr = process.communicate(DOCUMENT, timeout=5)
        assert process.returncode == 1, process.returncode
        assert "/dev/ttyを開けません" in stderr.decode(), stderr
        assert b"\x1b" not in stderr
    finally:
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    case("パイプ入力の表示・BOMとCRLF・検索・ヘルプ", pipe_display_and_keys)
    case("リダイレクト・空の入力・入力を変更しない", redirect_and_empty)
    case("Ctrl+z / SIGCONT後も制御端末から入力", suspension)
    case("Kitty応答エミュレータでの数式画像", math_images)
    case("stdinが端末・stdoutが端末でない・不正UTF-8・上限超過", startup_errors)
    case("読込中のCtrl+c・SIGTERM", interrupted_while_reading)
    case("制御端末なし", without_controlling_terminal)
