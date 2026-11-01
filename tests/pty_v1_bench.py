#!/usr/bin/env python3
"""v1性能: 1MiB/1万行/120x40、検索入力・確定、plainとauto無応答を分ける。

実行: python3 tests/pty_v1_bench.py BINARY [MODE...]
MODEを省略するとplain・auto・auto-mathを測る。ブロック数式2,500個の文書は、
無応答（auto-math-many）とKitty応答エミュレータ（kitty-math-many）で測る。
kitty-math-manyは最初の画像配置と移動だけを測り、検索・リサイズは測らない。
"""
import hashlib
import json
import os
from pathlib import Path
import select
import statistics
import sys
import tempfile
import time
from pty_smoke import Session
from pty_v1 import respond
FRAME_END = b"\x1b[0m\x1b[?25l"
MODES = sys.argv[2:] or ["plain", "auto", "auto-math"]

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
    plain_rows = list(rows)
    # 4行ごとにブロック数式・空行・本文・空行を置く。
    many = []
    for i in range(2500):
        many += [b"$$ x_{%d} $$\n" % i, b"\n", plain_rows[i * 4 + 2], b"\n"]
    fixture.write_bytes(b"".join(rows))
    results = []
    for mode in MODES:
        if mode == "auto-math":
            rows = list(plain_rows)
            rows[0] = b"$$ x $$" + b" " * (len(rows[0]) - 8) + b"\n"
            rows[1] = b" " * (len(rows[1]) - 1) + b"\n"
            fixture.write_bytes(b"".join(rows))
        elif mode.endswith("-math-many"):
            fixture.write_bytes(b"".join(many))
        else:
            fixture.write_bytes(b"".join(plain_rows))
        for run in range(3):
            profile = Path(directory) / ("%s-%d.json" % (mode, run))
            started = time.monotonic()
            session = Session((["--plain"] if mode == "plain" else []) + [fixture], size=(120,40), extra_env={"FUMI_PROFILE":str(profile)}, exec_clock_env="FUMI_PROFILE_EXEC_NS")
            try:
                frame(session)
                startup = (time.monotonic() - started) * 1000
                first_image = None
                if mode == "kitty-math-many":
                    respond(session)
                    until = time.monotonic() + 20
                    while b"a=p" not in session.data:
                        session.pump(.005)
                        assert time.monotonic() < until, "placement timeout"
                    first_image = (time.monotonic() - started) * 1000
                    session.pump(.2)
                moves = []
                for _ in range(100):
                    since = len(session.data); start = time.monotonic(); os.write(session.master,b"j"); frame(session,since)
                    moves.append((time.monotonic()-start)*1000)
                if mode == "kitty-math-many":
                    session.send(b"q"); session.wait(0)
                    results.append(dict(mode=mode,fixture_sha256=hashlib.sha256(fixture.read_bytes()).hexdigest(),run=run+1,external_startup_ms=startup,first_image_ms=first_image,move_p95_ms=sorted(moves)[94],profile=json.loads(profile.read_text())))
                    continue
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
