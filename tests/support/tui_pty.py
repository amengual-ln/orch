"""Exercise the real crossterm input/restore paths on a Linux pseudo-terminal."""
import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata

ALT_IN = b"\x1b[?1049h"
ALT_OUT = b"\x1b[?1049l"


# Ratatui emits cursor-addressed diffs. Reconstruct text cells instead of asserting how
# a string happens to be split across ANSI writes.
CSI = re.compile(r"\x1b\[([?0-9;]*)([ -/]*)([@-~])")


class Screen:
    def __init__(self):
        self.row, self.column = 1, 1
        self.cells = {}
        self.tail = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")()

    def write(self, data):
        text = self.tail + self.decoder.decode(data)
        self.tail = ""
        cursor = 0
        while cursor < len(text):
            ch = text[cursor]
            if ch == "\x1b":
                match = CSI.match(text, cursor)
                if not match:
                    self.tail = text[cursor:]
                    return
                args, _, command = match.groups()
                if command in "Hf":
                    row, _, column = args.partition(";")
                    self.row, self.column = int(row or 1), int(column or 1)
                if command == "J" and args == "2":
                    self.cells.clear()
                if command == "h" and args == "?1049":
                    self.cells.clear()
                cursor = match.end()
                continue
            if ch == "\r":
                self.column = 1
            elif ch == "\n":
                self.row += 1
            elif ch.isprintable():
                self.cells[self.row, self.column] = ch
                self.column += 2 if unicodedata.east_asian_width(ch) in ("F", "W") else 1
            cursor += 1

    def text(self):
        if not self.cells:
            return ""
        height = max(row for row, _ in self.cells)
        width = max(column for _, column in self.cells)
        return "\n".join("".join(self.cells.get((row, column), " ") for column in range(1, width + 1))
                         for row in range(1, height + 1))


class Terminal:
    def __init__(self, binary, directory, args=(), size=(100, 24), env=None):
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        self._size(*size)
        self.process = subprocess.Popen([binary, *args], cwd=directory,
                                        stdin=self.slave, stdout=self.slave, stderr=self.slave,
                                        env=env)
        self.output = bytearray()
        self.screen = Screen()

    def _size(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    def resize(self, width, height):
        mark = len(self.output)
        self._size(width, height)
        self.process.send_signal(signal.SIGWINCH)
        return mark

    def send(self, data):
        mark = len(self.output)
        os.write(self.master, data)
        return mark

    def read(self, duration):
        if select.select([self.master], [], [], duration)[0]:
            try:
                chunk = os.read(self.master, 65536)
                self.output.extend(chunk)
                self.screen.write(chunk)
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise

    def expect(self, mark, *parts):
        until = time.monotonic() + 5
        while True:
            screen = self.screen.text()
            if len(self.output) > mark and all(
                part in self.output[mark:] if isinstance(part, bytes) else part in screen for part in parts
            ):
                return
            assert time.monotonic() < until, (parts, screen)
            self.read(.04)

    def finish(self, code):
        until = time.monotonic() + 8
        while self.process.poll() is None and time.monotonic() < until:
            self.read(.04)
        assert self.process.poll() == code, (self.process.poll(), bytes(self.output)[-2500:])
        self.read(.03)
        assert ALT_IN in self.output and ALT_OUT in self.output, bytes(self.output)[-1000:]
        assert b"\x1b[?2004h" in self.output and b"\x1b[?2004l" in self.output
        assert termios.tcgetattr(self.slave) == self.original

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.terminate()
            deadline = time.monotonic() + 6
            while self.process.poll() is None and time.monotonic() < deadline:
                self.read(.04)
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=5)
        os.close(self.master)
        os.close(self.slave)


def requests(directory, count, terminal):
    until = time.monotonic() + 5
    while time.monotonic() < until:
        try:
            records = [json.loads(line) for line in (directory / "requests").read_text().splitlines()]
            if len(records) >= count:
                return records
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        terminal.read(.02)
    raise AssertionError((f"provider did not launch {count} times in {directory}", terminal.screen.text()))


def stopped(pid):
    deadline = time.monotonic() + 4
    while Path(f"/proc/{pid}").exists() and time.monotonic() < deadline:
        time.sleep(.02)
    assert not Path(f"/proc/{pid}").exists(), pid


def flow(binary, root):
    codex, agy = root / "codex", root / "agy"
    codex.mkdir()
    agy.mkdir()
    fake = root / "provider"
    fake.write_bytes(b"#!/usr/bin/env python3\n" + Path(__file__).with_name("tui_provider.py").read_bytes())
    fake.chmod(0o700)
    args = ["--codex-bin", fake, "--agy-bin", fake, "--timeout-secs", "15"]
    with Terminal(binary, root, args) as term:
        term.expect(0, ALT_IN, "NORMAL", "Ctrl+n")
        assert termios.tcgetattr(term.slave) != term.original
        term.expect(term.send(b"\x0e"), "Nueva")
        mark = term.send(b"\tfixture-codex\talpha\t\x15/nonexistent-orch-cwd\r")
        term.expect(mark, "invalid cwd")
        term.expect(term.send(b"\x15" + os.fsencode(codex) + b"\r"), "alpha", "/codex")

        term.send(b"ilong\r")
        first = requests(codex, 1, term)[0]
        assert first["prompt"] == "long"
        assert "--model=fixture-codex" in first["args"]
        assert first["cwd"] == str(codex)
        term.expect(0, "LINE-49")
        # Put resize readiness first while input also waits in the descriptor.
        mark = term.resize(102, 24)
        term.send(b"\x15" * 9)
        term.expect(mark, "LINE-00")
        term.expect(term.send(b"icodex draft"), "codex draft")
        term.expect(term.send(b"\x1b"), "NORMAL")

        term.expect(term.send(b"\x0e"), "Nueva")
        term.expect(term.send(b"\x1b[C"), "agy")
        term.expect(term.send(b"\t\tbeta\t\x15" + os.fsencode(agy) + b"\r"), "beta", "/agy")
        term.send(b"iwait\r")
        active_agy = requests(agy, 1, term)[0]
        assert not any(arg.startswith("--model=") for arg in active_agy["args"])
        assert active_agy["cwd"] == str(agy)

        term.send(b"k")
        mark = term.resize(101, 24)
        term.expect(mark, "alpha", "LINE-00", "codex draft")
        assert "LINE-49" not in term.screen.text()
        term.send(b"i\x15wait\r")
        active_codex = requests(codex, 2, term)[1]
        assert "tui-codex" in active_codex["args"]

        term.expect(term.send(b"ikeep draft\r"), "session is busy")
        term.send(b"\x04" * 12)
        term.send(b"\x1b")
        term.expect(len(term.output), "Turno cancelado")
        stopped(active_codex["pid"])
        assert Path(f"/proc/{active_agy['pid']}").exists()
        mark = term.resize(102, 24)
        term.expect(mark, "keep draft", "Turno cancelado")

        (agy / "continue").touch()
        term.send(b"j")
        term.expect(term.resize(103, 24), "beta", "RESPUESTA")
        term.send(b"i\x1b[200~uno\x1b[201~\x0adosx\x7f\r")
        resumed = requests(agy, 2, term)[1]
        assert resumed["prompt"] == "uno\ndos"
        assert "--conversation=tui-agy" in resumed["args"]
        term.send(b"ifail\r")
        requests(agy, 3, term)
        term.expect(term.resize(104, 24), "Error", "error de prueba")
        term.send(b"irecuperar\r")
        recovered = requests(agy, 4, term)[3]
        assert "--conversation=tui-agy" in recovered["args"]
        term.expect(len(term.output), "Lista · reanuda")
        term.send(b"iholi\r")
        denied = requests(agy, 5, term)[4]
        assert "--conversation=tui-agy" in denied["args"]
        term.expect(len(term.output), "agy denied 1 tool action(s)", "read_file")

        term.expect(term.resize(40, 13), "NORMAL", "beta")
        term.expect(term.resize(10, 4), "Terminal p")
        term.expect(term.resize(100, 24), "NORMAL", "beta")
        term.send(b"ki\x15wait\r")
        closing = requests(codex, 3, term)[2]
        mark = term.send(b"q")
        term.expect(mark, ALT_OUT)
        term.finish(0)
        stopped(closing["pid"])
        assert b"shutdown signal monitor ready" not in term.output
    assert (root / ".orch" / "orch.log").exists()


def partial(binary, root):
    # ioctl returns an empty size and the usual tput fallback is unavailable.
    with Terminal(binary, root, size=(0, 0), env={**os.environ, "PATH": "/nonexistent-orch-path"}) as term:
        term.expect(0, ALT_IN, ALT_OUT)
        term.finish(1)


def interrupted(binary, root):
    for external in (True, False):
        with Terminal(binary, root) as term:
            term.expect(0, "NORMAL")
            if external:
                term.process.send_signal(signal.SIGTERM)
            else:
                term.send(b"\x03")
            term.finish(130)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="orch-tui-test-") as temporary:
        globals()[sys.argv[2]](sys.argv[1], Path(temporary))
