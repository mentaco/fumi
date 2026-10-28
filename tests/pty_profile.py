#!/usr/bin/env python3
"""startup-profile feature付きreleaseを計測する。使い方はdocs/performance.md参照。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import statistics
import tempfile
import time
from pty_smoke import Session


def frame(session, since=0):
    deadline = time.monotonic() + 60
    while True:
        ready, _, _ = select.select([session.master], [], [], 0.1)
        if ready:
            chunk = os.read(session.master, 65536)
            assert chunk, "PTY closed before frame"
            session.data.extend(chunk)
            if b"TEXT" in session.data and b"\x1b[0m\x1b[?25l" in session.data[since:]:
                return
        assert time.monotonic() < deadline, "frame timeout"


def fixture(kind):
    rows = []
    for i in range(10000):
        size = 104 + (i < 8576)
        if kind == "paragraphs" and i % 2:
            rows.append(b"\n")
            continue
        if kind == "paragraphs":
            size = 208 + (i // 2 < 3576)
        prefix, suffix, token = ("line %05d " % i).encode(), b"\n", b"a"
        if kind == "japanese":
            token = "日本語e\u0301👩‍💻 ".encode()
        elif kind == "table":
            prefix, suffix = ("| %05d | " % i).encode(), b" |\n"
            if i == 0:
                prefix = b"| index | "
            elif i == 1:
                prefix, token = b"|---|", b"-"
        elif kind == "math":
            prefix += b"$x_i^2 + **literal**$ "
        elif kind == "longword":
            prefix = b""  # 改行なしの1 MiB一語は末尾で生成する。
        remaining = size - len(prefix) - len(suffix)
        rows.append(prefix + token * (remaining // len(token)) + b"a" * (remaining % len(token)) + suffix)
    data = b"".join(rows)
    if kind == "longword":
        data = b"word " + b"a" * (1048576 - 5)
    assert len(data) == 1048576, (kind, len(data))
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--moves", type=int, default=100)
    parser.add_argument("--fixtures", nargs="+", choices=["ascii", "paragraphs", "japanese", "table", "math", "longword"], default=["ascii"])
    parser.add_argument("--copy-binary", action="store_true", help="fixtureごとに同一内容の新しい実行ファイルをコピーする")
    args = parser.parse_args()
    if args.runs < 1 or args.moves < 0:
        parser.error("--runsは1以上、--movesは0以上を指定してください")
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for kind in args.fixtures:
            data = fixture(kind)
            path = root / (kind + ".md")
            path.write_bytes(data)
            executable = binary
            if args.copy_binary:
                executable = root / (kind + "-fumi")
                executable.unlink(missing_ok=True)
                shutil.copyfile(binary, executable)
                executable.chmod(0o755)
            samples = []
            for run in range(args.runs):
                profile_path = root / (kind + "-%d.json" % run)
                profile_path.unlink(missing_ok=True)
                start = time.monotonic()
                session = Session(["--plain", path], size=(120, 40), binary=str(executable),
                    extra_env={"FUMI_PROFILE": str(profile_path)}, exec_clock_env="FUMI_PROFILE_EXEC_NS")
                try:
                    frame(session)
                    external = (time.monotonic() - start) * 1000
                    # 計測ファイルはflush時刻の採取後に書かれるため、別途待つ。
                    deadline = time.monotonic() + 3
                    while not profile_path.exists() or not profile_path.read_bytes().endswith(b"\n"):
                        assert time.monotonic() < deadline, "profile missing: build with --features startup-profile"
                        time.sleep(0.001)
                    sample = json.loads(profile_path.read_text())
                    sample["external_startup_ms"] = external
                    sample["exec_to_flush_ms"] = sample["exec_to_main_ms"] + sample["main_to_flush_ms"]
                    moves = []
                    for _ in range(args.moves):
                        since, start = len(session.data), time.monotonic()
                        os.write(session.master, b"j")
                        frame(session, since)
                        moves.append((time.monotonic() - start) * 1000)
                    if moves:
                        sample["move_median_ms"] = statistics.median(moves)
                        sample["move_p95_ms"] = sorted(moves)[max(0, (len(moves) * 95 + 99) // 100 - 1)]
                    since, start = len(session.data), time.monotonic()
                    session.resize(80, 24)
                    frame(session, since)
                    sample["resize_ms"] = (time.monotonic() - start) * 1000
                    # 高さだけの変更も独立に測定する。
                    since, start = len(session.data), time.monotonic()
                    session.resize(80, 30)
                    frame(session, since)
                    sample["height_resize_ms"] = (time.monotonic() - start) * 1000
                    os.write(session.master, b"q")
                    session.wait(0)
                    samples.append(sample)
                finally:
                    session.close()
            result = {"fixture": kind, "bytes": len(data), "lines": len(data.splitlines()),
                "newline_count": data.count(b"\n"),
                "sha256": hashlib.sha256(data).hexdigest(), "samples": samples}
            print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
