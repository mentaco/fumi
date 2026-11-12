#!/usr/bin/env python3
"""v2の標準入力（`fumi -`）・再読込（`R`・`--watch`）・ローカル画像をPTYで検証する。標準入力のキー入力は制御端末（/dev/tty）から送る。"""
import base64
import os
from pathlib import Path
import re
import signal
import struct
import subprocess
import tempfile
import threading
import time
import zlib
from pty_smoke import BINARY, Session, case
from pty_v1 import V1, commands, respond, started_kitty, unwrapped

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


def wait_screen(session, text, seconds=3):
    until = time.monotonic() + seconds
    while not any(text in row for row in session.screen()):
        session.pump(.02)
        assert time.monotonic() < until, session.screen()


def status(session):
    return session.screen()[-1]


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
        session.send(b"R")
        assert "stdinは再読込できません" in status(session), status(session)
        # ヘルプを1画面ずつ送り、起動方法の説明を探す。
        session.send(b"?")
        for _ in range(6):
            if any("fumi -" in row for row in session.screen()):
                break
            session.send(b"\x06")
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


def manual_reload():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "doc.md"
        path.write_text("# 初版\n\n検索語 one\n", encoding="utf-8")
        session = Session([path])
        try:
            session.started()
            session.send("/検索語\r".encode())
            path.write_text("# 第二版\n\n検索語 two\n\n検索語 three\n", encoding="utf-8")
            session.send(b"R")
            wait_screen(session, "第二版")
            assert "再読込" in status(session) and "/検索語 1/2" in status(session), status(session)
            session.send(b"j")
            assert "再読込" not in status(session), status(session)
            # 一時的な削除と不正なUTF-8では旧表示を維持する。
            path.unlink()
            session.send(b"R")
            wait_screen(session, "再読込失敗: ファイルが見つかりません")
            assert any("第二版" in row for row in session.screen())
            path.write_bytes(b"\xff")
            session.send(b"R")
            wait_screen(session, "再読込失敗: UTF-8が不正です")
            assert any("第二版" in row for row in session.screen())
            # atomic saveによる置換も読む。
            temporary = Path(directory) / ".doc.md.tmp"
            temporary.write_text("# 第三版\n", encoding="utf-8")
            os.replace(temporary, path)
            before = path.read_bytes()
            session.send(b"R")
            wait_screen(session, "第三版")
            # 検索語は残り、一致しない新しい文書では「見つかりません」になる。
            assert "見つかりません 再読込" in status(session), status(session)
            session.send(b"q")
            session.wait(0)
            assert path.read_bytes() == before
        finally:
            session.close()


def reload_adds_math():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "doc.md"
        path.write_text("# 数式なし\n", encoding="utf-8")
        session = Session([path])
        try:
            session.started()
            session.pump(0.2)
            # 数式のない文書では、端末の画像対応を確かめない。
            assert b"a=q" not in session.data
            path.write_bytes(V1.read_bytes())
            session.send(b"R")
            started_kitty(session)
            assert any(fields.get(b"a") == b"t" for fields, _ in commands(session)), "数式画像が転送されていない"
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()


def reload_replaces_math_images():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "doc.md"
        path.write_text("$$ \\frac{1}{2} $$\n", encoding="utf-8")
        session = Session([path])
        try:
            started_kitty(session)
            old = {int(f[b"i"]) for f, _ in commands(session) if f.get(b"a") == b"t"}
            assert old, "数式画像が転送されていない"
            since = len(session.data)
            path.write_text("$$ \\sqrt{x} $$\n", encoding="utf-8")
            session.send(b"R")

            def freed_at(after):
                """古い画像をすべて解放した位置。まだなら `None`。"""
                freed = set()
                for i, (f, _) in enumerate(after):
                    if f.get(b"a") == b"d" and f.get(b"d") == b"I":
                        freed.add(int(f[b"i"]))
                        if old <= freed:
                            return i
                return None

            # 読込が終わるまでは旧文書の画像を表示し続けるため、解放の後の配置を待つ。
            until = time.monotonic() + 5
            while True:
                after = commands(session, since)
                released = freed_at(after)
                if released is not None and any(f.get(b"a") == b"p" for f, _ in after[released:]):
                    break
                session.pump(.02)
                assert time.monotonic() < until, session.visible()
            new = {int(f[b"i"]) for f, _ in after if f.get(b"a") == b"t"}
            placed = {int(f[b"i"]) for f, _ in after[released:] if f.get(b"a") == b"p"}
            # 別の数式の画像を新しいIDで送り、解放した後は新しい画像だけを配置する。
            assert new and not new & old, (old, new)
            assert placed <= new, (placed, new)
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()


def watch_reload():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "doc.md"
        path.write_text("# 初版\n", encoding="utf-8")
        session = Session(["--watch", path])
        try:
            session.started()
            # 変化がなければ読み直さない。
            session.pump(0.6)
            assert "再読込" not in status(session), status(session)
            path.write_text("# 第二版\n", encoding="utf-8")
            # キーを押さずに、200 ms間隔の確認2回（変化の発見と安定の確認）の後に読み直す。
            wait_screen(session, "第二版", seconds=3)
            assert "再読込" in status(session), status(session)
            session.send(b"j")
            assert "再読込" not in status(session), status(session)
            path.unlink()
            wait_screen(session, "再読込失敗: ファイルが見つかりません", seconds=3)
            assert any("第二版" in row for row in session.screen())
            # 削除後に同じパスへ戻ったファイル（atomic save）も変化として読む。
            temporary = Path(directory) / ".doc.md.tmp"
            temporary.write_text("# 第三版\n", encoding="utf-8")
            os.replace(temporary, path)
            wait_screen(session, "第三版", seconds=3)
            before = path.read_bytes()
            session.send(b"q")
            session.wait(0)
            assert path.read_bytes() == before
        finally:
            session.close()


def png(width, height, rgba):
    """単色のRGBA PNG。"""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    raw = b"".join(b"\x00" + bytes(rgba) * width for _ in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def transferred(session, since=0):
    """直接転送された画像のPNG。"""
    result, payload = [], bytearray()
    for fields, data in commands(session, since):
        if fields.get(b"a") == b"t":
            payload = bytearray()
        if data and (fields.get(b"a") == b"t" or b"m" in fields):
            payload.extend(data)
            if fields.get(b"m") == b"0":
                result.append(base64.b64decode(payload))
    return result


def wait_placed(session, since=0):
    until = time.monotonic() + 6
    while not any(f.get(b"a") == b"p" for f, _ in commands(session, since)):
        session.pump(.02)
        assert time.monotonic() < until, session.visible()[-600:]
    session.pump(.2)


def image_session(args, reply=None, **kwargs):
    session = Session(args, **kwargs)
    respond(session, **(reply or {}))
    return session


def local_images():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "docs" / "img").mkdir(parents=True)
        (root / "docs" / "img" / "cat.png").write_bytes(png(40, 30, (255, 0, 0, 255)))
        document = root / "docs" / "doc.md"
        document.write_text(
            "前の文 ![猫の絵](img/cat.png) 後の文\n\n- ![リスト](img/cat.png)\n\n"
            "![遠隔](https://example.com/cat.png) ![ない](none.png)\n",
            encoding="utf-8",
        )
        before = document.read_bytes()
        payloads = []
        for args in [[document], ["--color", "never", document]]:
            session = image_session(args)
            try:
                wait_placed(session)
                pngs = transferred(session)
                # 文書の所在からの相対参照を読み、40×30をセル（10×20）の境界まで埋めて送る。
                assert len(pngs) == 1, len(pngs)
                assert struct.unpack(">II", pngs[0][16:24]) == (40, 40)
                payloads.append(pngs[0])
                screen = session.screen()
                assert any(row.strip() == "前の文" for row in screen), screen
                assert any(row.strip() == "[image: 猫の絵] <img/cat.png>" for row in screen), screen
                assert any(row.strip() == "後の文" for row in screen), screen
                # リストの中・遠隔の画像は代替テキストのまま。ないファイルは理由を出す。
                assert any("• [image: リスト] <img/cat.png>" in row for row in screen), screen
                assert any("[image: 遠隔] <https://example.com/cat.png>" in row for row in screen), screen
                assert any("[image: ファイルが見つかりません]" in row for row in screen), screen
                session.send(b"q")
                session.wait(0)
            finally:
                session.close()
        # 色なしの指定は本文装飾だけに効き、画像は元の色のまま送る。
        assert payloads[0] == payloads[1]
        assert document.read_bytes() == before
        # シンボリックリンクから開いた場合も、指定したパス側の所在を基準にする。
        link = root / "link.md"
        link.symlink_to(document)
        session = image_session([link])
        try:
            session.pump(1.0)
            assert not transferred(session)
            assert any("[image: ファイルが見つかりません]" in row for row in session.screen())
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()


def inline_math_images():
    """`--inline-math image` のときだけインライン数式を画像にし、本文の行の中へ置く（仕様7.4節）。"""
    with tempfile.TemporaryDirectory() as directory:
        document = Path(directory) / "inline.md"
        document.write_text("前の文 $x_i$ と $\\frac{a}{b}$ 後の文\n", encoding="utf-8")
        before = document.read_bytes()
        # 既定（text）と plain では、インライン数式だけの文書で端末の画像対応を確かめない。
        for args in [[document], ["--plain", "--inline-math", "image", document]]:
            session = Session(args)
            try:
                session.pump(1.0)
                assert b"a=q" not in session.data, args
                assert any("前の文 $x_i$ と $\\frac{a}{b}$ 後の文" in row for row in session.screen())
                session.send(b"q")
                session.wait(0)
            finally:
                session.close()
        session = image_session(["--inline-math", "image", document])
        try:
            until = time.monotonic() + 6
            while sum(f.get(b"a") == b"p" for f, _ in commands(session)) < 2:
                session.pump(.02)
                assert time.monotonic() < until, session.visible()[-600:]
            session.pump(.2)
            pngs = transferred(session)
            assert len(pngs) == 2, len(pngs)
            sizes = sorted(struct.unpack(">II", data[16:24]) for data in pngs)
            # どの式も本文の行の上へ1行足した2行（セル10×20の格子）に描く。
            assert all(w % 10 == 0 and h == 40 for w, h in sizes), sizes
            screen = session.screen()
            text_row = next(i for i, row in enumerate(screen) if "前の文" in row)
            row = screen[text_row]
            # TeXは表示せず、画像の幅の空白を敷く。
            assert "$" not in row and "後の文" in row, row
            # 画像は本文の行の上の1行から、「前の文 」の後の列へ置く。
            placed = [f for f, _ in commands(session) if f.get(b"a") == b"p"]
            assert all(f.get(b"C") == b"1" for f in placed), placed
            cursor = [m for m in re.finditer(rb"\x1b\[(\d+);(\d+)H\x1b_Ga=p", unwrapped(bytes(session.data)))]
            columns = sorted(int(m[2]) for m in cursor)
            # 余白1列＋「前の文 」の7セルの次（1始まりで9列目）。
            assert columns[0] == 9, columns
            # 配置は本文の行の1つ上の行から（画面の行は1始まり、`text_row` は0始まり）。
            assert all(int(m[1]) == text_row for m in cursor), (text_row, [m[1] for m in cursor])
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()
        assert document.read_bytes() == before


def images_from_stdin_use_the_working_directory():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "cat.png").write_bytes(png(10, 20, (0, 0, 255, 255)))
        session, thread, _ = piped("![猫](cat.png)\n".encode(), cwd=root)
        try:
            respond(session)
            wait_placed(session)
            assert struct.unpack(">II", transferred(session)[0][16:24]) == (10, 20)
            session.send(b"q")
            session.wait(0)
        finally:
            thread.join()
            session.close()


def images_without_colors_and_in_plain():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "cat.png").write_bytes(png(10, 20, (0, 255, 0, 255)))
        document = root / "doc.md"
        document.write_text("$$ x $$\n\n![猫](cat.png)\n", encoding="utf-8")
        # 文字色も背景色も分からない端末でも、画像は表示し、数式だけをTeX表示へ戻す。
        session = image_session([document], reply={"background": False, "foreground": False})
        try:
            wait_placed(session)
            assert len(transferred(session)) == 1
            assert any("[math: 文字色不明]" in row for row in session.screen()), session.screen()
            assert "KITTY" in session.screen()[-1]
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()
        session = Session(["--plain", document])
        try:
            session.started()
            session.pump(.3)
            assert b"a=q" not in session.data and b"a=t" not in session.data
            assert any(row.strip() == "[image: 猫] <cat.png>" for row in session.screen())
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()


def reload_reads_changed_images():
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        image = root / "cat.png"
        image.write_bytes(png(10, 20, (255, 0, 0, 255)))
        document = root / "doc.md"
        document.write_text("![猫](cat.png)\n", encoding="utf-8")
        session = image_session([document])
        try:
            wait_placed(session)
            first = transferred(session)
            # 画像そのものは監視しない。文書を読み直したときに、書き換わった画像も読み直す。
            image.write_bytes(png(20, 20, (0, 0, 255, 255)))
            since = len(session.data)
            session.send(b"R")
            wait_placed(session, since)
            second = transferred(session, since)
            assert second and struct.unpack(">II", second[0][16:24]) == (20, 20), second
            assert first[0] != second[0]
            session.send(b"q")
            session.wait(0)
        finally:
            session.close()


def startup_errors():
    session = Session(["-"])
    try:
        session.wait(1)
        assert_not_started(session)
        assert "パイプかリダイレクト" in session.visible()
    finally:
        session.close()
    # 標準入力の監視は、読込の前に使用法エラーにする。
    session, thread, _ = piped(DOCUMENT, args=("--watch", "-"))
    try:
        session.wait(2)
        assert_not_started(session)
        assert "--watch はファイル入力でのみ使えます" in session.visible()
    finally:
        thread.join()
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
    case("stdinが端末・--watchとの併用・stdoutが端末でない・不正UTF-8・上限超過", startup_errors)
    case("読込中のCtrl+c・SIGTERM", interrupted_while_reading)
    case("制御端末なし", without_controlling_terminal)
    case("手動再読込・検索の再計算・削除と不正UTF-8で旧表示維持・atomic save", manual_reload)
    case("再読込で数式が現れたら画像対応を確認", reload_adds_math)
    case("再読込で数式が変わると画像を送り直し、古い画像を解放", reload_replaces_math_images)
    case("--watchの自動再読込・変化なし・削除と復旧", watch_reload)
    case("ローカル画像・相対参照・色なしでも元の色・対象外の画像・シンボリックリンクの基準", local_images)
    case("標準入力の画像は作業ディレクトリが基準", images_from_stdin_use_the_working_directory)
    case("--inline-math imageのときだけインライン数式を本文の行の中で画像にする", inline_math_images)
    case("色の応答なしでも画像は表示・plainでは画像なし", images_without_colors_and_in_plain)
    case("再読込で書き換わった画像を読み直す", reload_reads_changed_images)
