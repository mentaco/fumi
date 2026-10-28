#!/usr/bin/env python3
"""macOS/LinuxのPTYで起動・入力・リサイズ・シグナル・端末属性復旧を検証する。

実行: rtk proxy python3 tests/pty_smoke.py target/debug/fumi
GUI端末・フォントの互換性を検証するものではない。
"""
import errno
import fcntl
import hashlib
import os
import re
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

BINARY = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/fumi").resolve())
FIXTURE = Path(__file__).parent / "fixtures/representative.md"


def terminal_settings(attributes):
    # PENDINは入力キューの再処理を示すカーネルの一時状態。macOSで
    # 入力を読まずrawを解除した場合に立つため、設定の比較からだけ除く。
    # ICANON/ECHO/ISIG等と制御文字・速度はそのまま比較する。
    result = list(attributes)
    result[3] &= ~getattr(termios, "PENDIN", 0)
    return result


class Session:
    def __init__(self, args, term="xterm-256color", stdin_tty=True, stdout_tty=True, stderr_tty=True, no_color=None, binary=None, extra_env=None, size=(80, 24), exec_clock_env=None):
        self.master, self.slave = os.openpty()
        self.resize(*size)
        self.before = termios.tcgetattr(self.slave)
        self.data = bytearray()
        status_read, status_write = os.pipe()
        keepalive_read, keepalive_write = os.pipe()
        self.supervisor = os.fork()
        self.status = None
        if self.supervisor == 0:
            os.close(status_read)
            os.close(keepalive_write)
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
            ready_read, ready_write = os.pipe()
            child = os.fork()
            if child:
                os.close(ready_read)
                # シェルに相当する監督プロセスを残す。macOSではセッションリーダーが
                # 終了すると、保持中のslave FDでもtcgetattrがENOTTYになる。
                signal.signal(signal.SIGTTOU, signal.SIG_IGN)
                try:
                    os.setpgid(child, child)
                except PermissionError:
                    pass
                os.tcsetpgrp(self.slave, child)
                os.write(ready_write, b"1")
                os.close(ready_write)
                os.write(status_write, ("PID %d\n" % child).encode())
                while True:
                    _, status = os.waitpid(child, os.WUNTRACED)
                    os.write(status_write, ("STATUS %d\n" % status).encode())
                    if not os.WIFSTOPPED(status):
                        break
                os.read(keepalive_read, 1)
                os._exit(0)
            os.close(status_write)
            os.close(keepalive_read)
            os.close(ready_write)
            os.setpgid(0, 0)
            os.read(ready_read, 1)
            os.close(ready_read)
            null = os.open(os.devnull, os.O_RDWR)
            for fd, enabled in [(0, stdin_tty), (1, stdout_tty), (2, stderr_tty)]:
                os.dup2(self.slave if enabled else null, fd)
            os.close(self.master)
            os.close(self.slave)
            os.close(null)
            if term is None:
                os.environ.pop("TERM", None)
            else:
                os.environ["TERM"] = term
            if no_color is None:
                os.environ.pop("NO_COLOR", None)
            else:
                os.environ["NO_COLOR"] = no_color
            # 実行元がtmux/screenでも、各ケースが指定した端末条件だけで検証する。
            os.environ.pop("TMUX", None)
            os.environ.pop("TMUX_PANE", None)
            os.environ.pop("STY", None)
            if extra_env:
                os.environ.update(extra_env)
            executable = binary or BINARY
            if exec_clock_env:
                os.environ[exec_clock_env] = str(time.clock_gettime_ns(time.CLOCK_MONOTONIC))
            os.execv(executable, [executable] + [str(arg) for arg in args])
        os.close(status_write)
        os.close(keepalive_read)
        self.status_pipe = os.fdopen(status_read, "rb", buffering=0)
        self.keepalive = keepalive_write
        self.pid = int(self.status_pipe.readline().split()[1])

    def pump(self, seconds=0.08):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            ready, _, _ = select.select([self.master], [], [], max(0, until - time.monotonic()))
            if ready:
                try:
                    chunk = os.read(self.master, 65536)
                    if not chunk:
                        break
                    self.data.extend(chunk)
                except OSError as exc:
                    if exc.errno != errno.EIO:
                        raise
                    break

    def started(self):
        until = time.monotonic() + 5
        while b"TEXT" not in self.data:
            self.pump()
            assert time.monotonic() < until, bytes(self.data)
        assert not termios.tcgetattr(self.slave)[3] & termios.ICANON
        assert b"\x1b[?1049h" in self.data
        return self

    def send(self, data):
        os.write(self.master, data)
        self.pump()

    def visible(self):
        return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(self.data)).decode("utf-8", errors="replace")

    def resize(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def wait(self, code):
        until = time.monotonic() + 5
        while True:
            ready, _, _ = select.select([self.status_pipe], [], [], 0)
            if ready:
                status = int(self.status_pipe.readline().split()[1])
                self.status = status
                break
            self.pump()
            assert time.monotonic() < until, "終了待ちタイムアウト"
        self.pump(0.02)
        assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == code, (status, bytes(self.data))
        assert terminal_settings(termios.tcgetattr(self.slave)) == terminal_settings(self.before), "端末属性が復旧していない"

    def stopped(self):
        until = time.monotonic() + 5
        while True:
            self.pump(0.03)
            ready, _, _ = select.select([self.status_pipe], [], [], 0)
            if ready:
                status = int(self.status_pipe.readline().split()[1])
                assert os.WIFSTOPPED(status), status
                break
            assert time.monotonic() < until, "中断待ちタイムアウト"
        assert terminal_settings(termios.tcgetattr(self.slave)) == terminal_settings(self.before), "中断前に端末属性が復旧していない"
        assert self.data.count(b"\x1b[?1049l") >= 1

    def close(self):
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        os.close(self.keepalive)
        os.waitpid(self.supervisor, 0)
        self.status_pipe.close()
        os.close(self.master)
        os.close(self.slave)


def case(name, callback):
    callback()
    print("PASS", name)


def interaction():
    before = hashlib.sha256(FIXTURE.read_bytes()).hexdigest()
    session = Session(["--plain", "--color", "never", FIXTURE])
    try:
        session.started()
        assert "日本語" in session.visible()
        session.send(b"G")
        session.send(b"gg")
        session.send(b"?")
        assert "操作一覧" in session.visible()
        session.send(b"Ggg?")
        session.resize(20, 4)
        session.pump()
        session.resize(10, 2)
        session.pump()
        session.send(b"jkg?")
        session.resize(120, 40)
        session.pump()
        session.send(b"q")
        session.wait(0)
        assert b"\x1b[?1049l" in session.data and b"\x1b[?25h" in session.data
        assert b"\x1b[?1000l" in session.data and b"\x1b[?1006l" in session.data
        assert b"\x1b_G" not in session.data and b"\x1b[?1000h" in session.data
        assert hashlib.sha256(FIXTURE.read_bytes()).hexdigest() == before
    finally:
        session.close()


def exit_path(key=None, sig=None, code=0):
    session = Session([FIXTURE])
    try:
        session.started()
        if key is not None:
            session.send(key)
        if sig is not None:
            os.kill(session.pid, sig)
        session.wait(code)
        assert b"\x1b[?1049l" in session.data and b"\x1b[?25h" in session.data
        assert b"\x1b[?1000l" in session.data and b"\x1b[?1006l" in session.data
    finally:
        session.close()


def escape_keeps_viewer_open():
    session = Session(["--plain", FIXTURE])
    try:
        session.started()
        for keys in [b"\x1b", b"/Markdown\r", b"\x1b", b"nN", b"/\r", b"\x1b"]:
            session.send(keys)
            session.pump(.08)  # 単独Escの判定タイムアウトを待つ。
            ready, _, _ = select.select([session.status_pipe], [], [], 0)
            assert not ready, "Escでツールが終了した"
            assert b"\x1b[?1049l" not in session.data
            assert not termios.tcgetattr(session.slave)[3] & termios.ICANON
        session.send(b"q")
        session.wait(0)
        assert b"\x1b[?1049l" in session.data and b"\x1b[?25h" in session.data
    finally:
        session.close()


def suspension(external=False):
    session = Session([FIXTURE])
    try:
        session.started()
        session.send(b"j")
        if external:
            os.kill(session.pid, signal.SIGTSTP)
        else:
            session.send(b"\x1a")
        session.stopped()
        session.resize(40, 12)
        os.kill(session.pid, signal.SIGCONT)
        session.pump(0.2)
        assert not termios.tcgetattr(session.slave)[3] & termios.ICANON
        assert session.data.count(b"\x1b[?1049h") == 2
        session.send(b"q")
        session.wait(0)
    finally:
        session.close()


def early_error(args, code=1, **kwargs):
    session = Session(args, **kwargs)
    try:
        session.wait(code)
        assert b"\x1b[?1049h" not in session.data
        assert b"\x1b]52" not in session.data
    finally:
        session.close()


def errors():
    for term in [None, "", "dumb"]:
        early_error([FIXTURE], term=term)
    early_error([FIXTURE], stdin_tty=False)
    early_error([FIXTURE], stdout_tty=False)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        early_error([root / "missing\x1b]52;secret\x07"])
        early_error([root])
        invalid = root / "invalid.md"
        invalid.write_bytes(b"\xff")
        early_error([invalid])
        huge = root / "huge.md"
        with huge.open("wb") as file:
            file.truncate(33554433)
        early_error([huge])
        fifo = root / "fifo"
        os.mkfifo(fifo)
        early_error([fifo])
        early_error(["-"], code=2)
        dash = root / "-"
        dash.write_text("# dash\n", encoding="utf-8")
        session = Session([dash], stderr_tty=False)
        try:
            session.started().send(b"q")
            session.wait(0)
        finally:
            session.close()


def controls():
    with tempfile.TemporaryDirectory() as directory:
        file = Path(directory) / "control\x1b]52;name\x07.md"
        file.write_text("# control\n\n\x1b]52;c;secret\x07\x1b[31m\u009b31m\0\n", encoding="utf-8")
        session = Session([file])
        try:
            session.started().send(b"q")
            session.wait(0)
            assert b"\x1b]52" not in session.data
            assert "␛]52" in session.visible()
        finally:
            session.close()


def color_modes():
    for mode, no_color, expected in [("auto", "1", False), ("auto", "", True), ("always", "1", True), ("never", "", False)]:
        session = Session(["--color", mode, FIXTURE], no_color=no_color)
        try:
            session.started().send(b"q")
            session.wait(0)
            colored = re.search(rb"\x1b\[(?:38;5;\d+|3[0-7])[;m]", session.data) is not None
            assert colored == expected, (mode, no_color)
        finally:
            session.close()


def restoration_faults():
    # cargo testで作成されたlibテスト実行ファイルを選ぶ。
    for candidate in sorted(Path(BINARY).parent.joinpath("deps").glob("fumi-*"), key=lambda path: path.stat().st_mtime, reverse=True):
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            continue
        listed = subprocess.run([str(candidate), "--list"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if b"terminal::tests::restoration_fault_probe" in listed.stdout:
            for scenario in ["0", "8", "panic"]:
                session = Session(["--ignored", "--exact", "terminal::tests::restoration_fault_probe", "--nocapture"], binary=str(candidate), extra_env={"FUMI_FAULT_PROBE": scenario})
                try:
                    session.wait(0)
                    assert b"RESTORATION_PROBE_OK" in session.data
                    assert b"\x1b[?1049l" in session.data
                    if scenario != "0":
                        assert b"\x1b[?25h" in session.data
                finally:
                    session.close()
            return
    raise AssertionError("先に cargo test --all-targets --all-features を実行してください")


if __name__ == "__main__":
    case("表示・操作・ヘルプ・リサイズ・読取専用・plain", interaction)
    case("Esc検索終了・閲覧継続・q終了の端末復旧", escape_keeps_viewer_open)
    case("ヘルプ表示中のq終了・端末復旧", lambda: exit_path(key=b"?q"))
    case("Ctrl+c終了", lambda: exit_path(key=b"\x03", code=130))
    case("SIGINT終了", lambda: exit_path(sig=signal.SIGINT, code=130))
    case("SIGTERM終了", lambda: exit_path(sig=signal.SIGTERM, code=143))
    case("Ctrl+z / SIGCONT", suspension)
    case("SIGTSTP / SIGCONT", lambda: suspension(True))
    case("TTY・TERM・ファイル読込エラー", errors)
    case("本文・ファイル名の制御文字", controls)
    case("色指定とNO_COLORの優先順位", color_modes)
    case("初期化途中の出力失敗・panic時の端末復旧", restoration_faults)
