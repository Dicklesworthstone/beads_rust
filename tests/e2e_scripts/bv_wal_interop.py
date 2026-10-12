#!/usr/bin/env python3
"""GH#555: real bv CLI/TUI reads followed by br reads and writes (Unix).

Run with freshly built binaries, not wrappers that redirect bv to JSONL:
  python3 tests/e2e_scripts/bv_wal_interop.py --br /abs/br --bv /abs/bv

Requires Python 3.10+, SQLite support, git, and a Unix pseudo-terminal. Missing
binaries fail, never skip. All databases are new scratch fixtures. The scratch
workspace, command outputs, terminal transcript, and receipt are retained on
both success and failure. This never rewrites, restores, or removes live SHM.
"""

from __future__ import annotations

import argparse
import contextlib
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time

ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\].*?(?:\x07|\x1b\\))", re.DOTALL)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def reader_worker(db: str, issue_id: str) -> None:
    """A separate process owns SQLite locks; parent file reads cannot drop them."""
    conn = sqlite3.connect(Path(db).resolve().as_uri() + "?mode=ro", uri=True)
    try:
        conn.execute("PRAGMA query_only=1")
        conn.execute("PRAGMA busy_timeout=5000")
        conn.execute("BEGIN")
        def title() -> str:
            row = conn.execute("SELECT title FROM issues WHERE id=?", (issue_id,)).fetchone()
            require(row is not None, "snapshot lost the fixture issue")
            return row[0]

        print(json.dumps({"title": title()}), flush=True)
        for line in sys.stdin:
            command = json.loads(line)
            if command == "close":
                break
            if command == "renew":
                conn.rollback()
                conn.execute("BEGIN")
            if command == "integrity":
                reply = {"integrity": conn.execute("PRAGMA integrity_check").fetchall()}
            else:
                require(command in ("title", "renew"), "unknown reader command")
                reply = {"title": title()}
            print(json.dumps(reply), flush=True)
    finally:
        conn.rollback()
        conn.close()


class Reader:
    def __init__(self, db: Path, issue_id: str, timeout: float):
        self.timeout = timeout
        self.proc = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--reader", str(db), issue_id],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        try:
            self.initial = self.receive()
        except BaseException:
            self.close()
            raise

    def receive(self) -> dict:
        require(bool(select.select([self.proc.stdout], [], [], self.timeout)[0]),
                "stock reader handshake timed out")
        line = self.proc.stdout.readline()
        require(bool(line), "stock reader exited before replying")
        return json.loads(line)

    def request(self, command: str) -> dict:
        self.proc.stdin.write(json.dumps(command) + "\n")
        self.proc.stdin.flush()
        return self.receive()

    def close(self) -> None:
        if self.proc.poll() is None:
            try:
                self.proc.stdin.write('"close"\n')
                self.proc.stdin.flush()
                self.proc.wait(timeout=5)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                self.proc.kill()
                self.proc.wait(timeout=5)
        error = self.proc.stderr.read()
        for stream in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
            stream.close()
        require(self.proc.returncode == 0, f"stock reader did not close cleanly: {error}")


class Terminal:
    def __init__(self, argv: list[str], cwd: Path, env: dict[str, str], timeout: float):
        import fcntl
        import pty
        import termios

        self.timeout = timeout
        self.output = bytearray()
        self.condition = threading.Condition()
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 48, 160, 0, 0))
        try:
            self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=slave,
                                         stdout=slave, stderr=slave, start_new_session=True)
        except BaseException:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        self.thread = threading.Thread(target=self.drain, daemon=True)
        self.thread.start()

    def drain(self) -> None:
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno not in (errno.EIO, errno.EBADF):
                    raise
                break
            if not data:
                break
            with self.condition:
                self.output.extend(data)
                self.condition.notify_all()
        with self.condition:
            self.condition.notify_all()

    def expect(self, text: str) -> None:
        deadline = time.monotonic() + self.timeout
        with self.condition:
            while text.encode() not in ANSI.sub(b"", bytes(self.output)):
                require(self.proc.poll() is None,
                        f"interactive bv exited before rendering {text!r}")
                remaining = deadline - time.monotonic()
                require(remaining > 0, f"interactive bv did not refresh to {text!r}")
                self.condition.wait(min(remaining, 0.2))

    def close(self, log: Path) -> None:
        try:
            if self.proc.poll() is None:
                os.write(self.master, b"q")
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(self.proc.pid, signal.SIGKILL)
                    self.proc.wait(timeout=5)
            self.thread.join(timeout=5)
        finally:
            os.close(self.master)
            log.write_bytes(bytes(self.output))


class Scenario:
    def __init__(self, root: Path, br: str, bv: str, timeout: float):
        self.root, self.br, self.bv, self.timeout = root, br, bv, timeout
        self.db = root / ".beads/beads.db"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("BR_", "BV_", "BD_", "BEADS_", "GIT_"))}
        self.env.update(HOME=str(root / "home"), XDG_CONFIG_HOME=str(root / "home/config"),
                        BEADS_DIR=str(self.db.parent), GIT_CONFIG_NOSYSTEM="1",
                        TERM="xterm-256color", CI="1", BV_TEST_MODE="1", BV_THEME="dark")
        (root / "home/config").mkdir(parents=True)
        self.serial = 0

    def run(self, argv: list[str], parse: bool = True):
        self.serial += 1
        result = subprocess.run(argv, cwd=self.root, env=self.env, text=True,
                                capture_output=True, timeout=self.timeout)
        with (self.root / "commands.jsonl").open("a") as log:
            log.write(json.dumps({"n": self.serial, "argv": argv, "code": result.returncode,
                                  "stdout": result.stdout, "stderr": result.stderr}) + "\n")
        require(result.returncode == 0, f"command failed: {argv!r}\n{result.stdout}\n{result.stderr}")
        require("quarantined poisoned WAL index" not in result.stderr,
                f"unnecessary WAL-index recovery: {argv!r}")
        return json.loads(result.stdout) if parse else result.stdout.strip()

    def command(self, *args: str):
        # TMPDIR can itself live inside another tracked repository. Never let
        # init or later mutations discover a parent tracker or inherit its DB.
        return self.run([self.br, "--db", str(self.db), "--no-auto-import",
                         "--no-auto-flush", *args, "--json"])

    def protected(self) -> dict[str, str | None]:
        paths = [self.db, Path(str(self.db) + "-wal"), Path(str(self.db) + "-journal"),
                 self.root / ".beads/issues.jsonl"]
        paths.extend(path for path in self.db.parent.iterdir() if "cert" in path.name)
        return {path.name: hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
                for path in paths if not path.is_dir()}

    def no_quarantine(self) -> None:
        require(not list(self.db.parent.glob(".br-wal-index-*")),
                "valid reader index triggered a quarantine directory")

    def show(self, issue_id: str, title: str) -> None:
        result = self.command("show", issue_id)
        rows = result if isinstance(result, list) else [result]
        require(any(row.get("id") == issue_id and row.get("title") == title for row in rows),
                f"br returned stale or missing issue: {result!r}")
        self.no_quarantine()

    def robot(self, issue_id: str, title: str) -> None:
        before = self.protected()
        result = self.run([self.bv, "--db", str(self.db), "--robot-next"])
        require(result.get("id") == issue_id and result.get("title") == title,
                f"bv did not read the current database issue: {result!r}")
        require(self.protected() == before, "bv read changed main/WAL/journal/certificate/JSONL")
        self.no_quarantine()


def checksum(data: bytes, byteorder: str) -> tuple[int, int]:
    require(len(data) % 8 == 0, "invalid checksum input length")
    a = b = 0
    for i in range(0, len(data), 8):
        a = (a + int.from_bytes(data[i:i + 4], byteorder) + b) & 0xffffffff
        b = (b + int.from_bytes(data[i + 4:i + 8], byteorder) + a) & 0xffffffff
    return a, b


def check_empty_index(db: Path) -> str:
    wal = Path(str(db) + "-wal").read_bytes()
    require(len(wal) == 32, f"fixture must exercise a header-only WAL, got {len(wal)} bytes")
    magic, version, page_size = struct.unpack(">III", wal[:12])
    require(magic in (0x377f0682, 0x377f0683) and version == 3007000,
            "fixture WAL header has an unsupported format")
    require(512 <= page_size <= 65536 and page_size & (page_size - 1) == 0,
            "fixture WAL page size is invalid")
    order = "big" if magic & 1 else "little"
    require(checksum(wal[:24], order) == struct.unpack(">II", wal[24:32]),
            "fixture WAL header checksum is invalid")
    shm = Path(str(db) + "-shm").read_bytes()
    require(len(shm) >= 136 and shm[:48] == shm[48:96], "index headers are not settled")
    require(int.from_bytes(shm[:4], sys.byteorder) == 3007000 and shm[12] == 1,
            "index is not stock's initialized header")
    require(shm[14:40] == bytes(26), "reader did not produce the unindexed empty header")
    require(checksum(shm[:40], sys.byteorder) == struct.unpack("=II", shm[40:48]),
            "stock header checksum is invalid")
    return hashlib.sha256(shm).hexdigest()


def exercise(s: Scenario, iterations: int) -> dict:
    s.run(["git", "init", "--quiet", "--initial-branch=main"], parse=False)
    versions = {"br": s.run([s.br, "--version"], parse=False),
                "bv": s.run([s.bv, "--version"], parse=False), "sqlite": sqlite3.sqlite_version}
    s.command("init", "--prefix", "wal")
    title = "GH555_CLI_SEED"
    issue_id = s.command("create", title)["id"]
    hashes = []
    for i in range(iterations):
        s.robot(issue_id, title)
        hashes.append(check_empty_index(s.db))
        s.show(issue_id, title)
        title = f"GH555_CLI_{i:03}"
        s.command("update", issue_id, "--title", title)
        s.show(issue_id, title)
    exported = s.root / ".beads/issues.jsonl"
    require(not exported.exists() or issue_id not in exported.read_text(),
            "fixture was exported; stale JSONL could mask a database-read failure")

    title = "GH555_TUI_SEED"
    s.command("update", issue_id, "--title", title)
    with contextlib.ExitStack() as stack:
        tui = Terminal([s.bv, "--db", str(s.db)], s.root, s.env, s.timeout)
        stack.callback(tui.close, s.root / "tui.log")
        tui.expect(title)  # Rendered data, not an arbitrary startup sleep.
        main_reader = Reader(s.db, issue_id, s.timeout)
        stack.callback(main_reader.close)
        require(main_reader.initial["title"] == title, "main snapshot was not established")
        main_snapshot = s.db.read_bytes()
        identity = Path(str(s.db) + "-shm").stat()
        wal_reader = None
        for i in range(iterations):
            title = f"GH555_TUI_{i:03}"
            s.command("update", issue_id, "--title", title)
            s.show(issue_id, title)
            s.robot(issue_id, title)
            tui.expect(title)
            require(main_reader.request("title")["title"] == "GH555_TUI_SEED",
                    "a br write changed the pinned main-file snapshot")
            if wal_reader is None:
                wal_reader = Reader(s.db, issue_id, s.timeout)
                stack.callback(wal_reader.close)
                require(wal_reader.initial["title"] == title, "WAL snapshot was not established")
            require(wal_reader.request("title")["title"] == "GH555_TUI_000",
                    "a br write changed the pinned WAL snapshot")
            require(s.db.read_bytes() == main_snapshot, "checkpoint overwrote a pinned main snapshot")
            after = Path(str(s.db) + "-shm").stat()
            require((identity.st_dev, identity.st_ino) == (after.st_dev, after.st_ino),
                    "live reader's WAL-index inode was replaced")
            s.no_quarantine()
        for reader in (main_reader, wal_reader):
            require(reader.request("renew")["title"] == title, "renewed snapshot missed latest commit")
        require(wal_reader.request("integrity")["integrity"] == [["ok"]], "integrity check failed")
    require(tui.proc.returncode == 0, "interactive bv did not exit cleanly")
    title = "GH555_AFTER_TUI"
    s.command("update", issue_id, "--title", title)
    s.robot(issue_id, title)
    s.show(issue_id, title)
    # A final mutation must still work after the final read-only CLI closes.
    s.command("update", issue_id, "--title", "GH555_FINAL_WRITE")
    s.show(issue_id, "GH555_FINAL_WRITE")
    return {"versions": versions, "iterations_per_phase": iterations,
            "stock_shm_hashes": sorted(set(hashes)), "recovery_directories": 0,
            "cli_freshness": True, "tui_freshness": True,
            "main_and_wal_snapshot_isolation": True, "integrity_check": "ok"}


def main() -> None:
    if len(sys.argv) == 4 and sys.argv[1] == "--reader":
        reader_worker(sys.argv[2], sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--br", required=True, help="freshly built br binary")
    parser.add_argument("--bv", required=True, help="freshly built bv binary")
    parser.add_argument("--iterations", type=int, default=12)
    parser.add_argument("--timeout", type=float, default=30)
    args = parser.parse_args()
    require(os.name == "posix", "the interactive regression requires a Unix PTY")
    require(args.iterations >= 2 and args.timeout > 0, "use at least two iterations and a positive timeout")
    binaries = [shutil.which(name) for name in (args.br, args.bv, "git")]
    require(all(binaries), "br, bv and git must all be available; no tests were run")
    root = Path(tempfile.mkdtemp(prefix="br-bv-wal-555-")).resolve()
    print(f"Retained regression workspace: {root}", flush=True)
    receipt = {"status": "failed", "workspace": str(root)}
    try:
        receipt.update(exercise(Scenario(root, str(Path(binaries[0]).resolve()),
                                        str(Path(binaries[1]).resolve()), args.timeout), args.iterations))
        receipt["status"] = "passed"
    except BaseException as error:
        receipt["error"] = str(error)
        raise
    finally:
        (root / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
