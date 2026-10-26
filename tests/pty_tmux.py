#!/usr/bin/env python3
"""実際のtmuxとPTY端末応答エミュレータを使う。GUIの画像表示検証とは区別する。"""
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from pty_smoke import BINARY, Session, case
from pty_v1 import APC, V1, commands

TMUX = shutil.which("tmux")
PLACEHOLDER = "\U0010eeee"


class TmuxSession(Session):
    def close(self):
        # tmuxのデーモン化でPTYの監督プロセスの回収が遅れる場合も確実に終了する。
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        os.close(self.keepalive)
        until = time.monotonic() + 1
        while os.waitpid(self.supervisor, os.WNOHANG)[0] == 0:
            if time.monotonic() >= until:
                os.kill(self.supervisor, signal.SIGKILL)
                os.waitpid(self.supervisor, 0)
                break
            time.sleep(.01)
        self.status_pipe.close()
        os.close(self.master)
        os.close(self.slave)

    def __init__(self, socket, config, args):
        self.responded = 0
        # tmux自身の初期能力照会が済んでからmdviewを起動する。
        launcher = "import os,sys,time; time.sleep(.4); os.execv(sys.argv[1],sys.argv[1:])"
        super().__init__(["-S", socket, "-f", config, "-u", "new-session", "-s", "mdview",
                          sys.executable, "-c", launcher, BINARY] + args,
                         binary=TMUX, term="xterm-256color", size=(80, 24))

    def pump(self, seconds=.08):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            super().pump(min(.01, max(0, until - time.monotonic())))
            data = bytes(self.data[self.responded:])
            # 途中のシーケンスは次のチャンクで再検査する。
            end = max([0] + [m.end() for m in re.finditer(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b(?:_|\])[^\x1b\x07]*(?:\x1b\\|\x07)", data)])
            data = data[:end]
            self.responded += end
            replies = bytearray()
            for m in APC.finditer(data):
                if b"a=q" in m[1] or b"a=T" in m[1]:
                    image_id = re.search(rb"(?:^|,)i=(\d+)", m[1])[1]
                    replies.extend(b"\x1b_Gi=" + image_id + b";OK\x1b\\")
            for code, reply in [(b"\x1b[16t", b"\x1b[6;20;10t"),
                                (b"\x1b[14t", b"\x1b[4;480;800t"),
                                (b"\x1b[18t", b"\x1b[8;24;80t"),
                                (b"\x1b[c", b"\x1b[?62;22c"),
                                (b"\x1b]11;?", b"\x1b]11;rgb:1414/1414/1414\x1b\\"),
                                (b"\x1b]10;?", b"\x1b]10;rgb:ebeb/ebeb/ebeb\x1b\\")]:
                replies.extend(reply * data.count(code))
            if replies:
                os.write(self.master, replies)


def integration(initial=None, flags=(), no_math=False, lifecycle=True):
    with tempfile.TemporaryDirectory(prefix="mdview-tmux-") as directory:
        socket = str(Path(directory) / "socket")
        config = Path(directory) / "tmux.conf"
        settings = "set -g status off\nset -as terminal-features ',xterm-256color:RGB'\n"
        if initial:
            settings += "set -g allow-passthrough %s\n" % initial
        config.write_text(settings)
        before_config = config.read_bytes()
        source = V1.resolve()
        if no_math:
            source = Path(directory) / "text.md"
            source.write_text("# 数式のない文書\n\n本文\n")
        rich = not no_math and "--plain" not in flags and "off" not in flags

        def tmux(*args):
            return subprocess.check_output([TMUX, "-S", socket] + list(args), timeout=5).decode()

        session = TmuxSession(socket, str(config), list(flags) + [str(source)])
        try:
            until = time.monotonic() + 6
            while (b"KITTY" if rich else b"TEXT") not in session.data:
                session.pump()
                assert time.monotonic() < until, bytes(session.data)
            assert config.read_bytes() == before_config, "tmux設定ファイルが変更された"
            assert tmux("show-options", "-gv", "allow-passthrough").strip() == (initial or "off"), "グローバル設定が変更された"
            assert tmux("show-options", "-pAv", "-t", "mdview:0.0", "allow-passthrough").strip() == ("all" if rich else (initial or "off"))
            tmux("send-keys", "-t", "mdview:0.0", "-l", "/frac")
            tmux("send-keys", "-t", "mdview:0.0", "Enter")
            session.pump(.4)
            if rich:
                capture = tmux("capture-pane", "-p", "-e", "-t", "mdview:0.0")
                assert PLACEHOLDER in capture, capture
                assert "38;2;" in capture, "画像IDのRGB色が保持されていない"
                assert all(f.get(b"U") == b"1" for f, _ in commands(session) if f.get(b"a") == b"p")
            if rich and lifecycle:
                # ペイン分割とリサイズ。外側の画面寸法ではなくペインのセル幅を使う。
                tmux("split-window", "-h", "-t", "mdview:0.0", "sleep", "60")
                session.pump(.5)
                capture = tmux("capture-pane", "-p", "-t", "mdview:0.0")
                assert PLACEHOLDER in capture, capture
                assert PLACEHOLDER not in tmux("capture-pane", "-p", "-t", "mdview:0.1")
                assert tmux("show-options", "-pAv", "-t", "mdview:0.1", "allow-passthrough").strip() == (initial or "off"), "他ペインの設定が変更された"
                # 別ウィンドウへ切り替えて戻しても、通常のセルとして再描画される。
                tmux("new-window", "-t", "mdview:1", "sleep", "60")
                session.pump(.1)
                since = len(session.data)
                tmux("select-window", "-t", "mdview:0")
                session.pump(.2)
                assert PLACEHOLDER.encode() in session.data[since:]
                tmux("kill-window", "-t", "mdview:1")
                tmux("kill-pane", "-t", "mdview:0.1")
                session.pump(.3)
                tmux("send-keys", "-t", "mdview:0.0", "-l", "?")
                session.pump(.1)
                assert PLACEHOLDER not in tmux("capture-pane", "-p", "-t", "mdview:0.0")
                tmux("send-keys", "-t", "mdview:0.0", "-l", "q")
                session.pump(.2)
                pane_pid = int(tmux("display-message", "-p", "-t", "mdview:0.0", "#{pane_pid}"))
                tmux("send-keys", "-t", "mdview:0.0", "C-z")
                session.pump(.1)
                os.kill(pane_pid, signal.SIGCONT)
                session.pump(.4)
                assert PLACEHOLDER in tmux("capture-pane", "-p", "-t", "mdview:0.0")
            if not rich:
                assert not any(f.get(b"a") == b"t" for f, _ in commands(session))
                assert PLACEHOLDER.encode() not in session.data
            tmux("send-keys", "-t", "mdview:0.0", "-l", "q")
            session.wait(0)
            if rich:
                all_commands = commands(session)
                sent = {f[b"i"] for f, _ in all_commands if f.get(b"a") == b"t"}
                deleted = {f[b"i"] for f, _ in all_commands if f.get(b"d") == b"I"}
                assert sent and sent <= deleted, (sent, deleted)
        finally:
            subprocess.run([TMUX, "-S", socket, "kill-server"], capture_output=True, timeout=5)
            session.close()


if __name__ == "__main__":
    if TMUX is None:
        print("SKIP tmux未導入（実tmux経由の検証は未実施）")
        sys.exit(0)
    print(subprocess.check_output([TMUX, "-V"]).decode().strip())
    case("実tmux: 設定なしで自動有効化・画像転送・ペイン分離・リサイズ・中断復帰・端末復旧", integration)
    case("実tmux: 明示offからkitty指定で自動有効化", lambda: integration("off", ["--graphics", "kitty"], lifecycle=False))
    case("実tmux: plainでは自動設定なし", lambda: integration("off", ["--plain"]))
    case("実tmux: graphics offでは自動設定なし", lambda: integration("off", ["--graphics", "off"]))
    case("実tmux: 数式なしでは自動設定なし", lambda: integration("off", no_math=True))
