#!/usr/bin/env python3
"""大きな入力の負荷: 上限付近の文書と大きな画像で、時間と最大メモリを測る。

実行: LOAD_RUNS=3 python3 tests/pty_load_bench.py BINARY [NAME...]
BINARYはreleaseかつstartup-profile付き。NAMEは下のFIXTURESの名前か、画像の
`image:パス`。省略するとFIXTURESをすべて測る。

時間はPTYで受け取るまでで、端末エミュレータの描画時間は含まない。最大メモリは
プロセスグループ（数式・画像のワーカーを含む）のRSS合計をpsで約20 msごとに見た値。
"""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import threading
import time
from pty_smoke import Session
from pty_v1 import respond

FRAME_END = b"\x1b[0m\x1b[?25l"
LIMIT = 32 * 1024 * 1024
TIMEOUT = 300
RUNS = int(os.environ.get("LOAD_RUNS", "1"))
REASONS = ("タイムアウト", "上限", "形式未対応", "壊れ")


def padded(parts, size, tail=b""):
    """合計がちょうどsizeになるまでpartsを連ね、残りをASCIIで埋める。"""
    room = size - len(tail)
    out, used = [], 0
    for part in parts:
        if used + len(part) > room:
            break
        out.append(part)
        used += len(part)
    out.append(b"x" * (room - used))
    out.append(tail)
    return b"".join(out)


def counter(template):
    i = 0
    while True:
        yield template(i)
        i += 1


JAPANESE = "日本語の本文を折り返して表示するための検証用の段落です。" * 6


def paragraphs(size):
    return padded(counter(lambda i: b"para %07d " % i + b"a" * 85 + b"\n\n"), size)


def single_paragraph(size):
    return padded(counter(lambda i: b"line %07d " % i + b"a" * 92 + b"\n"), size)


def japanese(size):
    body = JAPANESE.encode()
    return padded(counter(lambda i: b"%07d " % i + body + b"\n\n"), size)


def long_line(size):
    body = ("ascii words と" + JAPANESE).encode()
    return padded(counter(lambda i: body), size, b"\n")


def code(size):
    line = b'    let value_%07d = compute(%d, "text") + other::call(&input)?; // comment\n'
    fence = b"```rust\n"
    return fence + padded(counter(lambda i: line % (i, i)), size - len(fence), b"\n```\n")


def table(size):
    head = b"| id | name | value |\n|---|---|---|\n"
    row = "| %07d | 日本語のセル | aaaaaaaaaaaaaaaaaaaa |\n"
    return head + padded(counter(lambda i: (row % i).encode()), size - len(head), b"\n")


def math(count):
    def build():
        return b"".join(b"$$ x_{%d} $$\n\ntext %d\n\n" % (i, i) for i in range(count))
    return build


FIXTURES = {
    "paragraphs-32mib": (lambda: paragraphs(LIMIT), "para"),
    "single-paragraph-32mib": (lambda: single_paragraph(LIMIT), "line"),
    "japanese-32mib": (lambda: japanese(LIMIT), "検証"),
    "long-line-32mib": (lambda: long_line(LIMIT), "検証"),
    "code-1mib": (lambda: code(1024 * 1024), "compute"),
    "code-8mib": (lambda: code(8 * 1024 * 1024), "compute"),
    "code-32mib": (lambda: code(LIMIT), "compute"),
    "table-32mib": (lambda: table(LIMIT), "セル"),
    "math-20000": (math(20000), None),
    "math-100000": (math(100000), None),
}


class Peak:
    """プロセスグループのRSS合計の最大値を別スレッドで記録する。"""

    def __init__(self, group):
        self.group, self.kib = group, 0
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            out = subprocess.run(["ps", "-A", "-o", "pgid=,rss="], capture_output=True, text=True).stdout
            pairs = (row.split() for row in out.splitlines())
            self.kib = max(self.kib, sum(int(rss) for pgid, rss in pairs if int(pgid) == self.group))
            time.sleep(.02)

    def finish(self):
        self.stop.set()
        self.thread.join()
        return self.kib


def read_some(session, wait):
    ready, _, _ = select.select([session.master], [], [], wait)
    if ready:
        session.data.extend(os.read(session.master, 65536))
    return bool(ready)


def frame(session, since=0):
    until = time.monotonic() + TIMEOUT
    while True:
        if read_some(session, .05) and b"TEXT" in session.data and FRAME_END in session.data[since:]:
            return
        assert time.monotonic() < until, "frame timeout"


def timed(session, action):
    """actionの後、最初のフレームを受け取るまでのミリ秒。"""
    since, start = len(session.data), time.monotonic()
    action()
    frame(session, since)
    return (time.monotonic() - start) * 1000


def settled(session, action, quiet=.4):
    """actionの後、出力が止まる直前の出力までのミリ秒。入力が連続する検索語に使う。"""
    start = last = time.monotonic()
    action()
    until = start + TIMEOUT
    while time.monotonic() - last < quiet:
        if read_some(session, .02):
            last = time.monotonic()
        assert time.monotonic() < until, "settle timeout"
    return (last - start) * 1000


def measure_document(fixture, term, directory, kitty):
    profile = Path(directory) / "profile.json"
    started = time.monotonic()
    session = Session([fixture], size=(120, 40), extra_env={"FUMI_PROFILE": str(profile)}, exec_clock_env="FUMI_PROFILE_EXEC_NS")
    peak = Peak(session.pid)
    try:
        frame(session)
        result = dict(first_frame_ms=(time.monotonic() - started) * 1000)
        if kitty:
            respond(session)
            until = time.monotonic() + TIMEOUT
            while b"a=p" not in session.data:
                session.pump(.005)
                assert time.monotonic() < until, "placement timeout"
            result["first_image_ms"] = (time.monotonic() - started) * 1000
            session.pump(.2)
        moves = sorted(timed(session, lambda: os.write(session.master, b"j")) for _ in range(100))
        result["move_p95_ms"] = moves[94]
        result["jump_end_ms"] = timed(session, lambda: os.write(session.master, b"G"))
        if term is not None:
            result["search_open_ms"] = timed(session, lambda: os.write(session.master, b"/"))
            result["search_input_settled_ms"] = settled(session, lambda: os.write(session.master, term.encode()))
            result["search_confirm_ms"] = timed(session, lambda: os.write(session.master, b"\r"))
        result["resize_ms"] = timed(session, lambda: session.resize(80, 24))
        session.send(b"q")
        session.wait(0)
        result["max_rss_mib"] = peak.finish() / 1024
        # 初回フレームのflush直後に別途書かれるため、終了を待ってから読む。
        result["profile"] = json.loads(profile.read_text())
        return result
    finally:
        peak.stop.set()
        session.close()


def measure_image(path, directory):
    document = Path(directory) / "image.md"
    document.write_text("![photo](%s)\n" % path)
    started = time.monotonic()
    session = Session([document], size=(120, 40))
    peak = Peak(session.pid)
    try:
        respond(session)
        until = time.monotonic() + 30
        reason = None
        while b"a=p" not in session.data and reason is None:
            session.pump(.005)
            # 差分描画では文字列が連続して届くとは限らないため、制御列を除いた文字で探す。
            visible = session.visible()
            reason = next((text for text in REASONS if text in visible), None)
            assert time.monotonic() < until, "image timeout: " + session.visible()[-400:]
        result = dict(done_ms=(time.monotonic() - started) * 1000, reason=reason, bytes=Path(path).stat().st_size)
        session.pump(.3)
        session.send(b"q")
        session.wait(0)
        result["max_rss_mib"] = peak.finish() / 1024
        return result
    finally:
        peak.stop.set()
        session.close()


def main():
    # 途中で失敗しても測れた分が残るよう、1件ごとに1行のJSONで出す。
    for name in sys.argv[2:] or list(FIXTURES):
        with tempfile.TemporaryDirectory() as directory:
            if name.startswith("image:"):
                runs = [measure_image(name[len("image:"):], directory) for _ in range(RUNS)]
                entry = dict(name=name, runs=runs)
            else:
                build, term = FIXTURES[name]
                fixture = Path(directory) / (name + ".md")
                fixture.write_bytes(build())
                kitty = name.startswith("math-")
                runs = [measure_document(fixture, term, directory, kitty) for _ in range(RUNS)]
                entry = dict(name=name, bytes=fixture.stat().st_size, runs=runs)
        print(json.dumps(entry, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
