#!/usr/bin/env python3
"""v1の端末入出力をPTY上のKitty応答エミュレータで検証する。GUI実機の検証とは区別する。"""
import base64
import hashlib
import os
from pathlib import Path
import re
import signal
import struct
import tempfile
import time
from pty_smoke import Session, case, FIXTURE

V1 = Path(__file__).parent / "fixtures/v1.md"
APC = re.compile(rb"\x1b_G([^\x1b]*)\x1b\\")
MOCK_TMUX_ENV = {}

PASSTHROUGH = re.compile(rb"\x1bPtmux;((?:[^\x1b]|\x1b\x1b)*)\x1b\\")


def mock_tmux_env(extra_env):
    if extra_env and "TMUX" in extra_env:
        return {**MOCK_TMUX_ENV, **extra_env}
    return extra_env


def unwrapped(data):
    return PASSTHROUGH.sub(lambda m: m[1].replace(b"\x1b\x1b", b"\x1b"), data)


def respond(session, cell=(10, 20), background=True, malformed=False, supported=True, placeholders=True):
    until = time.monotonic() + 5
    while b"a=q" not in session.data:
        session.pump(.01)
        assert time.monotonic() < until, "query timeout"
    replies = b"\x1b_Gi=4000000001;OK\x1b\\" if supported else b""
    session.pump(.005)
    for fields, _ in commands(session):
        if supported and fields.get(b"a") == b"T":
            replies += b"\x1b_Gi=" + fields[b"i"] + (b";OK" if placeholders else b";EINVAL") + b"\x1b\\"
    if cell:
        replies += ("\x1b[6;%d;%dt" % (cell[1], cell[0])).encode()
    if malformed:
        replies += b"\x1b[6;bad;20;10t"
    if background:
        replies += b"\x1b]11;rgb:1414/1414/1414\x1b\\"
    replies += b"\x1b]10;rgb:ebeb/ebeb/ebeb\x1b\\"
    os.write(session.master, replies)


def started_kitty(session):
    respond(session)
    session.send(b"/frac\r")
    until = time.monotonic() + 6
    while b"a=p" not in session.data:
        session.pump(.02)
        assert time.monotonic() < until, session.visible()
    assert b"KITTY" in session.data


def commands(session, since=0):
    result = []
    for match in APC.finditer(unwrapped(bytes(session.data[since:]))):
        header, _, payload = match[1].partition(b";")
        fields = dict(part.split(b"=", 1) for part in header.split(b",") if b"=" in part)
        result.append((fields, payload))
    return result


def graphics_lifecycle(tmux=False):
    before = hashlib.sha256(V1.read_bytes()).hexdigest()
    session = Session([V1], size=(80, 12), extra_env=mock_tmux_env({"TMUX": "test"}) if tmux else None)
    try:
        started_kitty(session)
        for _ in range(50):
            session.send(b"j")
        session.send(b"Ggg")
        session.send(b"/frac\r")
        session.pump(.3)
        assert "[math]" in session.visible()
        # 一致は数式画像のラベルにも残る。上端を画像途中へ動かす。
        session.send(b"jj")
        session.send(b"?")
        since = len(session.data)
        session.pump(.05)
        assert not any(fields.get(b"a") == b"p" for fields, _ in commands(session, since))
        session.send(b"q")
        session.resize(40, 8)
        session.pump(.02)
        respond(session)
        session.pump(.4)
        session.send(b"\x1a")
        session.stopped()
        os.kill(session.pid, signal.SIGCONT)
        session.pump(.02)
        respond(session)
        session.pump(.4)
        session.send(b"q")
        session.wait(0)
        all_commands = commands(session)
        transferred = {int(f[b"i"]) for f, _ in all_commands if f.get(b"a") == b"t"}
        deleted = {int(f[b"i"]) for f, _ in all_commands if f.get(b"d") == b"I"}
        assert transferred and transferred <= deleted, (transferred, deleted)
        assert all(f.get(b"d") in [b"i", b"I"] for f, _ in all_commands if f.get(b"a") == b"d")
        assert all(f.get(b"C") == b"1" for f, _ in all_commands if f.get(b"a") == b"p")
        if tmux:
            data = bytes(session.data)
            assert "\U0010eeee".encode() in data
            assert "\U0010eeee\u030e".encode() in data, "上端クリッピングの行番号なし"
            assert all(f.get(b"U") == b"1" for f, _ in all_commands if f.get(b"a") == b"p")
            assert all(f.get(b"q") == b"2" for f, _ in all_commands if f.get(b"a") in [b"p", b"t"])
            assert b"\x1b_G" not in PASSTHROUGH.sub(b"", data), "ラップされていない画像命令"
            assert all(len(m[0]) <= 4096 for m in PASSTHROUGH.finditer(data))
        else:
            assert any(int(f.get(b"y", b"0")) > 0 for f, _ in all_commands if f.get(b"a") == b"p"), "上端クリッピングなし"
        payload = bytearray()
        for fields, data in all_commands:
            if fields.get(b"a") == b"t": payload = bytearray()
            if data and (fields.get(b"a") == b"t" or b"m" in fields):
                payload.extend(data)
                if fields.get(b"m") == b"0":
                    png = base64.b64decode(payload)
                    assert png.startswith(b"\x89PNG\r\n\x1a\n")
                    width, height = struct.unpack(">II", png[16:24])
                    assert width <= 780 and height <= 240
        assert hashlib.sha256(V1.read_bytes()).hexdigest() == before
    finally:
        session.close()


def fallback(args, extra_env=None, reply=None):
    log = Path(MOCK_TMUX_ENV["FUMI_TEST_TMUX_LOG"])
    before = log.read_text() if log.exists() else ""
    session = Session(args + [V1], extra_env=mock_tmux_env(extra_env))
    try:
        if reply:
            session.pump(.03)
            respond(session, **reply)
        session.started()
        if "--plain" in args or "off" in args or (extra_env and "STY" in extra_env):
            assert b"a=q" not in session.data
        assert b"a=t" not in session.data
        session.send(b"G")
        assert "unknownfumi" in session.visible()
        session.send(b"q")
        session.wait(0)
        if "--plain" in args or "off" in args or not (extra_env and "TMUX" in extra_env):
            assert (log.read_text() if log.exists() else "") == before, "不要なtmux設定変更"
        if extra_env and ("FUMI_TEST_TMUX_MODE" in extra_env or "TMUX_PANE" in extra_env or "PATH" in extra_env):
            assert b"a=q" not in session.data, "自動設定失敗後に画像照会"
    finally:
        session.close()


def setup_failure(mode):
    start = time.monotonic()
    fallback([], {"TMUX": "test", "FUMI_TEST_TMUX_MODE": mode})
    assert time.monotonic() - start < 2, "自動設定が閲覧を長時間ブロック"
    if mode == "hang":
        pid = int(Path(MOCK_TMUX_ENV["FUMI_TEST_TMUX_LOG"] + ".pid").read_text())
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError("タイムアウトしたtmux子プロセスが残存")


def query_keys_and_late_replies():
    session = Session([V1])
    try:
        session.pump(.03)
        session.send(b"/hello world\r")
        session.started()
        session.pump(.2)
        assert "1/3" in session.visible(), session.visible()
        # タイムアウト後のAPC/OSC応答に含まれる q 等が終了キーにならない。
        session.send(b"\x1b_Gi=4000000001;quite late\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\")
        session.send(b"/q\x03")
        session.send(b"nN")
        session.send(b"q")
        session.wait(0)
    finally:
        session.close()


def transfer_failure(tmux=False):
    session = Session([V1], size=(80, 12), extra_env=mock_tmux_env({"TMUX": "test"}) if tmux else None)
    try:
        started_kitty(session)
        image_id = next(f[b"i"] for f, _ in commands(session) if f.get(b"a") == b"t")
        session.send(b"\x1b_Gi=" + image_id + b";EINVAL: injected failure\x1b\\")
        assert "画像転送失敗" in session.visible()
        session.send(b"q")
        session.wait(0)
    finally:
        session.close()


def input_and_mouse():
    session = Session(["--plain", FIXTURE])
    try:
        session.started()
        session.send(b"/q\x03")  # 検索のqとCtrl+cは終了しない。
        session.send("/日本語\r".encode())
        session.send(b"nN\x1b[<65;3;2M\x1b[<64;3;2M")
        session.send(b"/\x1b[200~hello\n\x1b[201~\x15\x1b")
        session.pump(.08)
        session.send(b"q")
        session.wait(0)
        assert b"\x1b_G" not in session.data
    finally:
        session.close()


def run():
    case("数式PNG・直接転送・上下切抜き・ID削除・検索・ヘルプ・リサイズ・中断復帰", graphics_lifecycle)
    case("plain（kitty指定より優先）", lambda: fallback(["--plain", "--graphics", "kitty"]))
    case("graphics off", lambda: fallback(["--graphics", "off"]))
    case("無応答", lambda: fallback([]))
    case("tmux", lambda: fallback(["--graphics", "kitty"], {"TMUX": "test"}))
    case("tmuxパススルー・Unicode配置・上下クリッピング・復旧", lambda: graphics_lifecycle(tmux=True))
    case("tmux kitty指定でも応答必須", lambda: fallback(["--graphics", "kitty"], {"TMUX": "test"}, {"supported": False}))
    case("tmux Unicode仮想配置未対応", lambda: fallback([], {"TMUX": "test"}, {"placeholders": False}))
    case("tmux plain", lambda: fallback(["--plain"], {"TMUX": "test"}))
    case("tmux off", lambda: fallback(["--graphics", "off"], {"TMUX": "test"}))
    case("tmux仮想配置失敗のテキスト復帰", lambda: transfer_failure(tmux=True))
    case("tmux自動設定失敗", lambda: setup_failure("fail"))
    case("tmux自動設定タイムアウト・子プロセス回収", lambda: setup_failure("hang"))
    case("tmuxペインID不正", lambda: fallback([], {"TMUX": "test", "TMUX_PANE": "other:0"}))
    case("tmuxコマンド未導入", lambda: fallback([], {"TMUX": "test", "PATH": "/nonexistent-fumi-test"}))
    case("screen", lambda: fallback([], {"STY": "test"}))
    case("セル寸法不明", lambda: fallback([], reply={"cell": None}))
    case("不正な寸法応答", lambda: fallback([], reply={"cell": None, "malformed": True}))
    case("背景色不明", lambda: fallback([], reply={"background": False}))
    case("照会中の入力保持・遅延応答", query_keys_and_late_replies)
    case("転送失敗のテキスト復帰", transfer_failure)
    case("検索UTF-8・取消・貼付け・ホイール・復旧", input_and_mouse)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="fumi-mock-tmux-") as directory:
        executable = Path(directory) / "tmux"
        executable.write_text('''#!/bin/sh
printf '%s\n' "$*" >> "$FUMI_TEST_TMUX_LOG"
printf '%s\n' "$$" > "$FUMI_TEST_TMUX_LOG.pid"
case "$FUMI_TEST_TMUX_MODE" in
    fail) exit 1 ;;
    hang) exec /bin/sleep 10 ;;
esac
exit 0
''')
        executable.chmod(0o700)
        MOCK_TMUX_ENV = {
            "PATH": directory + os.pathsep + os.environ.get("PATH", ""),
            "TMUX_PANE": "%42",
            "FUMI_TEST_TMUX_LOG": str(Path(directory) / "calls"),
        }
        run()
        calls = Path(MOCK_TMUX_ENV["FUMI_TEST_TMUX_LOG"]).read_text().splitlines()
        assert calls and all(call == "set-option -p -t %42 allow-passthrough all" for call in calls), calls
