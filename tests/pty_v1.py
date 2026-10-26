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


def respond(session, cell=(10, 20), background=True, malformed=False):
    until = time.monotonic() + 5
    while b"a=q" not in session.data:
        session.pump(.01)
        assert time.monotonic() < until, "query timeout"
    replies = b"\x1b_Gi=4000000001;OK\x1b\\"
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
    for match in APC.finditer(bytes(session.data[since:])):
        header, _, payload = match[1].partition(b";")
        fields = dict(part.split(b"=", 1) for part in header.split(b",") if b"=" in part)
        result.append((fields, payload))
    return result


def graphics_lifecycle():
    before = hashlib.sha256(V1.read_bytes()).hexdigest()
    session = Session([V1], size=(80, 12))
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
    session = Session(args + [V1], extra_env=extra_env)
    try:
        if reply:
            session.pump(.03)
            respond(session, **reply)
        session.started()
        if "--plain" in args or "off" in args or extra_env:
            assert b"a=q" not in session.data
        assert b"a=t" not in session.data
        session.send(b"G")
        assert "unknownmdview" in session.visible()
        session.send(b"q")
        session.wait(0)
    finally:
        session.close()


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


def transfer_failure():
    session = Session([V1], size=(80, 12))
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


if __name__ == "__main__":
    case("数式PNG・直接転送・上下切抜き・ID削除・検索・ヘルプ・リサイズ・中断復帰", graphics_lifecycle)
    case("plain（kitty指定より優先）", lambda: fallback(["--plain", "--graphics", "kitty"]))
    case("graphics off", lambda: fallback(["--graphics", "off"]))
    case("無応答", lambda: fallback([]))
    case("tmux", lambda: fallback(["--graphics", "kitty"], {"TMUX": "test"}))
    case("screen", lambda: fallback([], {"STY": "test"}))
    case("セル寸法不明", lambda: fallback([], reply={"cell": None}))
    case("不正な寸法応答", lambda: fallback([], reply={"cell": None, "malformed": True}))
    case("背景色不明", lambda: fallback([], reply={"background": False}))
    case("照会中の入力保持・遅延応答", query_keys_and_late_replies)
    case("転送失敗のテキスト復帰", transfer_failure)
    case("検索UTF-8・取消・貼付け・ホイール・復旧", input_and_mouse)
