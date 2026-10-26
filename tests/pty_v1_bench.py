#!/usr/bin/env python3
"""v1性能: 1MiB/1万行/120x40、検索入力・確定、plainとauto無応答を分ける。"""
import hashlib
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
        ready, _, _ = select.select([session.master], [], [], .05)
        if ready:
            session.data.extend(os.read(session.master, 65536))
            if b"TEXT" in session.data and FRAME_END in session.data[since:]: return
        assert time.monotonic() < until, "frame timeout"

with tempfile.TemporaryDirectory() as directory:
    fixture = Path(directory) / "1mib-10000-lines.md"
    rows = []
    for i in range(10000):
        size = 104 + (i < 8576)
        prefix = ("line %05d " % i).encode()
        rows.append(prefix + b"a" * (size - len(prefix) - 1) + b"\n")
    fixture.write_bytes(b"".join(rows))
    results = []
    for mode in ["plain", "auto", "auto-math"]:
        if mode == "auto-math":
            rows[0] = b"$$ x $$" + b" " * (len(rows[0]) - 8) + b"\n"
            rows[1] = b" " * (len(rows[1]) - 1) + b"\n"
            fixture.write_bytes(b"".join(rows))
        for run in range(3):
            profile = Path(directory) / ("%s-%d.json" % (mode, run))
            started = time.monotonic()
            session = Session((["--plain"] if mode == "plain" else []) + [fixture], size=(120,40), extra_env={"MDVIEW_PROFILE":str(profile)}, exec_clock_env="MDVIEW_PROFILE_EXEC_NS")
            try:
                frame(session)
                startup = (time.monotonic() - started) * 1000
                moves = []
                for _ in range(100):
                    since = len(session.data); start = time.monotonic(); os.write(session.master,b"j"); frame(session,since)
                    moves.append((time.monotonic()-start)*1000)
                since = len(session.data); start = time.monotonic(); os.write(session.master,b"/"); frame(session,since)
                search_open = (time.monotonic()-start)*1000
                since = len(session.data); start = time.monotonic(); os.write(session.master,b"line"); frame(session,since)
                search_input = (time.monotonic()-start)*1000
                # 最後の文字の反映まで待ってからEnterを測る。
                session.pump(.1)
                since = len(session.data); start = time.monotonic(); os.write(session.master,b"\r"); frame(session,since)
                search_confirm = (time.monotonic()-start)*1000
                since = len(session.data); start = time.monotonic(); session.resize(80,24); frame(session,since)
                resize = (time.monotonic()-start)*1000
                session.send(b"q"); session.wait(0)
                results.append(dict(mode=mode,fixture_sha256=hashlib.sha256(fixture.read_bytes()).hexdigest(),run=run+1,external_startup_ms=startup,move_p95_ms=sorted(moves)[94],search_open_ms=search_open,search_input_ms=search_input,search_confirm_ms=search_confirm,resize_after_search_ms=resize,profile=json.loads(profile.read_text())))
            finally: session.close()
    print(json.dumps(dict(bytes=fixture.stat().st_size,lines=10000,fixture_sha256=hashlib.sha256(fixture.read_bytes()).hexdigest(),results=results),indent=2))
