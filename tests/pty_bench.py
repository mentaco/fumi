#!/usr/bin/env python3
"""PTYの描画フレーム受信完了までを測る。端末エミュレータの描画時間は含まない。"""
import json
import os
from pathlib import Path
import select
import statistics
import tempfile
import time
from pty_smoke import Session

FRAME_END = b"\x1b[0m\x1b[?25l"


def frame(session, since=0):
    until = time.monotonic() + 20
    while True:
        ready, _, _ = select.select([session.master], [], [], 0.1)
        if ready:
            session.data.extend(os.read(session.master, 65536))
            if b"TEXT" in session.data and FRAME_END in session.data[since:]:
                return
        assert time.monotonic() < until, "frame timeout"


with tempfile.TemporaryDirectory() as directory:
    fixture = Path(directory) / "1mib-10000-lines.md"
    # 1 MiBを10000行へ配分する。単一の長い段落も同時に検証する。
    rows = []
    for i in range(10000):
        size = 104 + (i < 8576)
        row = ("line %05d " % i).encode()
        rows.append(row + b"a" * (size - len(row) - 1) + b"\n")
    fixture.write_bytes(b"".join(rows))
    startup = []
    moves = []
    resizes = []
    for run in range(3):
        start = time.monotonic()
        session = Session(["--plain", fixture], size=(120, 40))
        try:
            frame(session)
            startup.append((time.monotonic() - start) * 1000)
            for _ in range(100):
                since = len(session.data)
                start = time.monotonic()
                os.write(session.master, b"j")
                frame(session, since)
                moves.append((time.monotonic() - start) * 1000)
            since = len(session.data)
            start = time.monotonic()
            session.resize(80, 24)
            frame(session, since)
            resizes.append((time.monotonic() - start) * 1000)
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()
    print(json.dumps({"bytes": fixture.stat().st_size, "lines": 10000, "runs": 3,
        "startup_ms": startup, "move_p95_ms": sorted(moves)[int(len(moves) * 0.95) - 1],
        "move_median_ms": statistics.median(moves), "resize_ms": resizes}, indent=2))
