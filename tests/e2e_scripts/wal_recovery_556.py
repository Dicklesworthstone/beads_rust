#!/usr/bin/env python3
"""GH #556: exercise recovery only on new, disposable database copies.

Run: python3 tests/e2e_scripts/wal_recovery_556.py --br target/debug/br
Fixture/oracle checks only (does NOT test br): python3 ... --self-test

All files, including failed cases and CLI logs, are retained in a newly allocated
system temporary directory. No input database is accepted or modified. The WAL
case uses a real stock SQLite reader transaction and committed issue closure;
returning an error after changing a protected file is a test FAILURE.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import queue
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import unittest

try:
    import fcntl
except ImportError:
    fcntl = None

HAS_LINUX_OFD = (sys.platform.startswith("linux") and ctypes.sizeof(ctypes.c_void_p) == 8
                 and fcntl is not None and hasattr(fcntl, "F_OFD_SETLK"))

SUFFIXES = ("", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head")
PROTECTED = tuple(suffix for suffix in SUFFIXES if suffix != "-shm")
TITLE = "GH556 disposable recovery sentinel"
REASON = "GH556 committed close present only in WAL"
STAMP = "2026-10-13T00:00:00Z"
REFUSAL = "engine recovery refused before opening"
Family = dict[str, bytes | None]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def component(db: Path, suffix: str) -> Path:
    return db.with_name(db.name + suffix)


def family(db: Path) -> Family:
    result: Family = {}
    for suffix in SUFFIXES:
        try:
            result[suffix] = component(db, suffix).read_bytes()
        except FileNotFoundError:
            result[suffix] = None
    return result


def describe(state: Family) -> dict:
    return {
        suffix: None if data is None else {
            "length": len(data), "sha256": hashlib.sha256(data).hexdigest()
        }
        for suffix, data in state.items()
    }


def retain(state: Family, db: Path) -> None:
    """Write a new evidence copy; never overwrite an existing file."""
    db.parent.mkdir(parents=True, exist_ok=True)
    for suffix, data in state.items():
        if data is not None:
            with component(db, suffix).open("xb") as out:
                out.write(data)


def unchanged(before: Family, after: Family, suffixes=SUFFIXES) -> None:
    changed = [suffix or "main" for suffix in suffixes if before[suffix] != after[suffix]]
    require(not changed, f"recovery changed file bytes/presence: {changed}; "
            f"before={describe(before)} after={describe(after)}")


def family_modes(db: Path) -> dict[str, int | None]:
    """Record live permission bits without opening any database descriptor."""
    modes = {}
    for suffix in SUFFIXES:
        try:
            modes[suffix] = stat.S_IMODE(component(db, suffix).stat().st_mode)
        except FileNotFoundError:
            modes[suffix] = None
    return modes


def unchanged_modes(before: dict[str, int | None], after: dict[str, int | None]) -> None:
    require(before == after, f"recovery changed live permissions/presence: {before} -> {after}")


def readonly(db: Path) -> sqlite3.Connection:
    return sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True, timeout=1)


def issue_state(conn: sqlite3.Connection, issue_id: str) -> tuple:
    row = conn.execute(
        "SELECT status, closed_at, close_reason FROM issues WHERE id = ?", (issue_id,)
    ).fetchone()
    require(row is not None, "the sentinel issue disappeared")
    return row


@contextlib.contextmanager
def pinned_closure(db: Path, *, leave_wal: bool = False):
    """Hold SQLite connections in a child; byte observers stay in this process.

    Closing ANY raw descriptor for an inode can release the calling process's
    SQLite POSIX locks. Keeping connection holders separate from family() and
    db.read_bytes() is therefore part of the fixture, not an optimization.
    """
    script = r"""
import contextlib, json, os, sqlite3, sys
from pathlib import Path
db, title, stamp, reason = sys.argv[1:]
uri = Path(db).as_uri()
writer = sqlite3.connect(uri + '?mode=rw', uri=True, timeout=1)
reader = None
def state(conn, issue_id):
    return conn.execute('SELECT status, closed_at, close_reason FROM issues WHERE id=?',
                        (issue_id,)).fetchone()
try:
    assert writer.execute('PRAGMA journal_mode=WAL').fetchone()[0] == 'wal'
    writer.execute('PRAGMA wal_autocheckpoint=0')
    assert writer.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()[0] == 0
    found = writer.execute('SELECT id FROM issues WHERE title=?', (title,)).fetchall()
    assert len(found) == 1
    issue_id = found[0][0]
    reader = sqlite3.connect(uri + '?mode=ro', uri=True, timeout=1)
    reader.execute('BEGIN')
    initial = state(reader, issue_id)  # BEGIN alone does not pin a snapshot.
    assert initial[0] == 'open'
    print(json.dumps({'stage': 'pinned', 'issue_id': issue_id}), flush=True)
    if sys.stdin.readline().strip() == 'commit':
        writer.execute("UPDATE issues SET status='closed', closed_at=?, close_reason=? WHERE id=?",
                       (stamp, reason, issue_id))
        writer.commit()
        assert state(reader, issue_id) == initial
        with contextlib.closing(sqlite3.connect(uri + '?mode=ro', uri=True)) as fresh:
            assert state(fresh, issue_id) == ('closed', stamp, reason)
        print(json.dumps({'stage': 'committed', 'issue_id': issue_id}), flush=True)
        command = sys.stdin.readline().strip()
        if command == 'abandon':
            os._exit(0)  # Committed WAL survives; no close-time checkpoint.
        assert state(reader, issue_id) == initial
finally:
    if reader is not None:
        reader.close()
    writer.close()
"""
    child = subprocess.Popen(
        [sys.executable, "-I", "-u", "-c", script, str(db.resolve()), TITLE, STAMP, REASON],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    messages = queue.Queue()

    def read_messages():
        for line in child.stdout:
            messages.put(line)
        messages.put(None)

    observer = threading.Thread(target=read_messages, daemon=True)
    observer.start()

    def receive(stage: str):
        try:
            line = messages.get(timeout=10)
        except queue.Empty as error:
            raise AssertionError(f"SQLite holder did not reach {stage}") from error
        require(line is not None, f"SQLite holder exited before {stage}")
        message = json.loads(line)
        require(message["stage"] == stage, f"unexpected SQLite holder message: {message}")
        return message["issue_id"]

    def send(command: str):
        child.stdin.write(command + "\n")
        child.stdin.flush()

    committed = False
    try:
        issue_id = receive("pinned")
        main_before = db.read_bytes()  # A different process owns the SQLite locks.
        send("commit")
        require(receive("committed") == issue_id, "fixture issue changed")
        committed = True
        require(db.read_bytes() == main_before, "closure unexpectedly reached the main file")
        require(len(component(db, "-wal").read_bytes()) > 32, "no WAL frames were produced")
        yield issue_id
    finally:
        try:
            if child.poll() is None:
                send("abandon" if committed and leave_wal else "release")
            child.wait(timeout=10)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
        finally:
            child.stdin.close()
            observer.join(timeout=5)
            child.stdout.close()
            error = child.stderr.read()
            child.stderr.close()
        require(child.returncode == 0, f"SQLite holder failed: {child.returncode}: {error}")


def assert_refusal(result: subprocess.CompletedProcess, before: Family, after: Family) -> None:
    # Check bytes first: old br can report failure only AFTER a WAL rewrite.
    unchanged(before, after)
    require(result.returncode != 0, "unsafe recovery was reported successful")
    require(REFUSAL in result.stdout + result.stderr,
            f"expected a pre-open WAL refusal, not an unrelated error: {result.stdout}{result.stderr}")


def orphaned_closure(db: Path) -> str:
    """Commit on a disposable copy, then exit without a close-time checkpoint."""
    with pinned_closure(db, leave_wal=True) as issue_id:
        pass
    require(len(component(db, "-wal").read_bytes()) > 32, "child checkpointed away the WAL")
    return issue_id


@contextlib.contextmanager
def sqlite_main_exclusion(db: Path):
    """Probe the existing Rust RecoveryLock's Linux syscall, NOT Rust or br."""
    require(HAS_LINUX_OFD, "this syscall probe requires 64-bit Linux with OFD locks")

    class Flock(ctypes.Structure):
        _fields_ = [("l_type", ctypes.c_short), ("l_whence", ctypes.c_short),
                    ("l_start", ctypes.c_longlong), ("l_len", ctypes.c_longlong),
                    ("l_pid", ctypes.c_int)]

    fd = os.open(db, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        # Same range and ownership as src/sync/db_inode_lock.rs::RecoveryLock.
        lock = Flock(fcntl.F_WRLCK, os.SEEK_SET, 0x40000000, 512, 0)
        fcntl.fcntl(fd, fcntl.F_OFD_SETLK, bytes(lock))
        yield
    finally:
        os.close(fd)


def run_br(br: Path, cwd: Path, beads: Path, args: list[str], label: str):
    env = dict(os.environ)
    # Do not inherit a real workspace, route, or user configuration into fixtures.
    for key in tuple(env):
        if key.startswith(("BEADS_", "BD_", "BR_")):
            env.pop(key)
    home = cwd / "isolated-home"
    home.mkdir(exist_ok=True)
    env.update(BEADS_DIR=str(beads), HOME=str(home), USERPROFILE=str(home),
               XDG_CONFIG_HOME=str(home / "config"), NO_COLOR="1")
    result = subprocess.run([str(br), *args], cwd=cwd, env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60,
                            check=False)
    (cwd / f"{label}.stdout").write_text(result.stdout)
    (cwd / f"{label}.stderr").write_text(result.stderr)
    return result


def setup_seed(br: Path, root: Path) -> Path:
    root.mkdir()
    beads = root / ".beads"
    for args, label in [(["init", "--prefix", "w556"], "init"),
                        (["create", TITLE], "create")]:
        result = run_br(br, root, beads, args, label)
        require(result.returncode == 0, f"fixture {label} failed: {result.stdout}{result.stderr}")
    db = beads / "beads.db"
    require(len(component(db, "-wal").read_bytes()) == 32,
            "expected a settled br family with a header-only WAL; refusing to discard frames")
    return db


def test_pinned_wal(br: Path, seed: Path, ambient: Path, root: Path) -> None:
    db = root / "copy" / "selected.db"
    retain(family(seed), db)
    with pinned_closure(db) as issue_id:
        before = family(db)
        ambient_before = recovery_files(ambient)
        retain(before, root / "before" / db.name)
        for attempt in range(3):
            result = run_br(br, root, ambient, ["--db", str(db), "--no-auto-import",
                            "--no-auto-flush", "doctor", "migrate-schema", "recover", "--json"],
                            f"recover-{attempt}")
            after = family(db)
            # Persist the observation BEFORE any connection is closed or reopened.
            observed = root / f"after-{attempt}" / db.name
            retain(after, observed)
            assert_refusal(result, before, after)
            require(not (db.parent / ".br_recovery").exists(),
                    "WAL refusal must not allocate another recovery run")
            require(recovery_files(ambient) == ambient_before,
                    "WAL refusal must not write into the ambient workspace")
            inspection = root / f"oracle-{attempt}" / db.name
            retain(after, inspection)  # SQLite must not modify the retained observation's SHM.
            with contextlib.closing(readonly(inspection)) as recovered:
                require(issue_state(recovered, issue_id) == ("closed", STAMP, REASON),
                        "a fresh reader of the post-recovery copy lost the committed closure")


def test_orphaned_wal(br: Path, seed: Path, ambient: Path, root: Path,
                      automatic: bool = False) -> None:
    db = root / "copy" / "selected.db"
    retain(family(seed), db)
    issue_id = orphaned_closure(db)
    if automatic:
        # No process holds this disposable copy now. Give ordinary startup
        # the known legacy-index signature without changing the WAL payload.
        legacy = bytearray(32 * 1024)
        legacy[108:120] = b"\xff" * 12
        component(db, "-shm").write_bytes(legacy)
    before = family(db)
    retain(before, root / "before" / db.name)
    ambient_before = recovery_files(ambient)
    for attempt in range(3):
        command = (["list", "--all", "--json"] if automatic else
                   ["--no-auto-import", "--no-auto-flush",
                    "doctor", "migrate-schema", "recover", "--json"])
        result = run_br(br, root, ambient, ["--db", str(db), *command],
                        f"orphaned-recover-{attempt}")
        after = family(db)
        observed = root / f"after-{attempt}" / db.name
        retain(after, observed)
        assert_refusal(result, before, after)
        require(not (db.parent / ".br_recovery").exists(),
                "refusing an idle frame-bearing WAL must not allocate recovery runs")
        require(recovery_files(ambient) == ambient_before, "ambient evidence changed")
        inspection = root / f"oracle-{attempt}" / db.name
        retain(after, inspection)
        with contextlib.closing(readonly(inspection)) as current:
            require(issue_state(current, issue_id) == ("closed", STAMP, REASON),
                    "idle WAL-only closure was lost")


def poison_index_headers(db: Path) -> None:
    """Install #507's signature only in a disposable fixture's two headers.

    Do not truncate or replace a file mapped by the stock reader. Its read
    marks and lock region stay intact, and its child exits without more SQL.
    """
    before = family(db)
    require(before["-shm"] is not None and len(before["-shm"]) >= 136,
            "fixture requires both WAL-index headers and read marks")
    header = bytearray(48)
    header[:4] = (3_007_000).to_bytes(4, sys.byteorder)
    header[12] = 1
    with component(db, "-shm").open("r+b") as out:
        out.write(header + header)
    after = family(db)
    unchanged(before, after, PROTECTED)
    require(before["-shm"][96:] == after["-shm"][96:], "fixture changed lock/read-mark bytes")


def assert_closed_observation(state: Family, root: Path, issue_id: str) -> None:
    observed = root / "observation.db"
    inspection = root / "oracle" / "observation.db"
    retain(state, observed)
    retain(state, inspection)
    with contextlib.closing(readonly(inspection)) as current:
        require(issue_state(current, issue_id) == ("closed", STAMP, REASON),
                "post-refusal observation lost the committed WAL-only closure")
    unchanged(state, family(observed))


def test_pinned_automatic(br: Path, seed: Path, ambient: Path, root: Path,
                          writing: bool) -> None:
    db = root / "copy" / "selected.db"
    retain(family(seed), db)
    # The holder must not read a deliberately poisoned index or checkpoint on
    # exit; its committed bytes remain available for post-mortem inspection.
    with pinned_closure(db, leave_wal=True) as issue_id:
        poison_index_headers(db)
        before = family(db)
        ambient_before = family(ambient / "beads.db")
        ambient_artifacts = recovery_files(ambient)
        command = (["comments", "add", issue_id, "GH556 forbidden write"] if writing
                   else ["list", "--all", "--json"])
        for attempt in range(3):
            # No opt-out from normal automatic import/flush/startup behavior.
            result = run_br(br, root, ambient, ["--db", str(db), *command],
                            f"automatic-{attempt}")
            after = family(db)
            retain(after, root / f"after-{attempt}" / db.name)
            assert_refusal(result, before, after)
            assert_closed_observation(after, root / f"record-{attempt}", issue_id)
            unchanged(ambient_before, family(ambient / "beads.db"))
            require(recovery_files(ambient) == ambient_artifacts, "ambient evidence changed")
            require(not (db.parent / ".br_recovery").exists(),
                    "automatic WAL refusal allocated recovery artifacts")


@contextlib.contextmanager
def pinned_header_only_reader(db: Path):
    """A real stock reader in another process; never checkpoint on release."""
    script = r"""
import json, os, sqlite3, sys
from pathlib import Path
conn = sqlite3.connect(Path(sys.argv[1]).as_uri() + '?mode=ro', uri=True, timeout=1)
conn.execute('BEGIN')
row = conn.execute('SELECT id, status, closed_at, close_reason FROM issues WHERE title=?',
                   (sys.argv[2],)).fetchone()
assert row is not None
print(json.dumps(row), flush=True)
assert sys.stdin.readline().strip() == 'release'
os._exit(0)
"""
    child = subprocess.Popen(
        [sys.executable, "-I", "-u", "-c", script, str(db.resolve()), TITLE],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    messages = queue.Queue()
    observer = threading.Thread(target=lambda: messages.put(child.stdout.readline()), daemon=True)
    observer.start()
    try:
        try:
            line = messages.get(timeout=10)
        except queue.Empty as error:
            raise AssertionError("header-only reader failed to pin its snapshot") from error
        require(bool(line), "header-only reader exited before pinning its snapshot")
        require(len(component(db, "-wal").read_bytes()) == 32, "fixture WAL is not header-only")
        yield json.loads(line)
    finally:
        try:
            if child.poll() is None:
                child.stdin.write("release\n")
                child.stdin.flush()
            child.wait(timeout=10)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
        finally:
            child.stdin.close()
            observer.join(timeout=5)
            child.stdout.close()
            error = child.stderr.read()
            child.stderr.close()
        require(child.returncode == 0, f"header-only reader failed: {error}")


def test_header_only_contention_and_retry(br: Path, seed: Path, ambient: Path, root: Path) -> None:
    db = root / "copy" / "nondefault.db"
    retain(family(seed), db)
    ambient_before = recovery_files(ambient)
    ambient_db_before = family(ambient / "beads.db")
    command = ["--db", str(db), "--no-auto-import", "--no-auto-flush",
               "doctor", "migrate-schema", "recover", "--json"]
    with pinned_header_only_reader(db) as row:
        poison_index_headers(db)
        before = family(db)
        result = run_br(br, root, ambient, command, "held-reader")
        after = family(db)
        retain(after, root / "after-refusal" / db.name)
        unchanged(before, after)
        require(result.returncode != 0, "recovery admitted a mapped stock-SQLite index")
        require("transient contention" in result.stdout + result.stderr,
                f"expected typed contention, not another error: {result.stdout}{result.stderr}")
        assert_artifact_scope(db, ambient, ambient_before, "failed", before)
        inspection = root / "oracle-refusal" / db.name
        retain(after, inspection)
        with contextlib.closing(readonly(inspection)) as reader:
            require(issue_state(reader, row[0]) == tuple(row[1:]), "failed recovery lost a record")
    retained_failure = recovery_files(db.parent)
    result = run_br(br, root, ambient, command, "after-reader-release")
    after = family(db)
    retain(after, root / "after-success" / db.name)
    unchanged(before, after, PROTECTED)
    require(result.returncode == 0, f"legitimate recovery could not retry: {result.stdout}{result.stderr}")
    assert_artifact_scope(db, ambient, ambient_before, "complete", before)
    retained_after = recovery_files(db.parent)
    require(all(retained_after.get(name) == data for name, data in retained_failure.items()),
            "successful retry changed or removed retained failure evidence")
    unchanged(ambient_db_before, family(ambient / "beads.db"))
    inspection = root / "oracle-success" / db.name
    retain(after, inspection)
    with contextlib.closing(readonly(inspection)) as reader:
        require(issue_state(reader, row[0]) == tuple(row[1:]), "successful recovery lost a record")


def recovery_files(root: Path) -> dict[str, bytes]:
    recovery = root / ".br_recovery"
    return {str(path.relative_to(recovery)): path.read_bytes()
            for path in recovery.rglob("*") if path.is_file()}


def assert_artifact_scope(db: Path, ambient: Path, ambient_before: dict[str, bytes],
                          verdict: str, original: Family) -> None:
    require(recovery_files(ambient) == ambient_before,
            "recovery wrote artifacts beneath ambient BEADS_DIR")
    runs = db.parent / ".br_recovery" / "schema-migrations"
    receipts = list(runs.glob(f"*/recovery-{verdict}.json"))
    require(len(receipts) == 1, f"expected one {verdict} receipt beside selected --db: {receipts}")
    receipt = json.loads(receipts[0].read_text())
    require(Path(receipt["database_path"]).resolve() == db.resolve(), "receipt names the wrong DB")
    backup = Path(receipt["backup_path"])
    require(backup.resolve() == (receipts[0].parent / "recovery-before").resolve(),
            "backup path does not belong to the selected database's recovery run")
    saved = family(backup / db.name)
    scope = receipt.get("backup_scope")
    require(scope in ("complete-family", "wal-index-only"), f"unknown backup scope: {scope}")
    if scope == "wal-index-only":
        unchanged(original, saved, ("-shm",))
        require(all(saved[suffix] is None for suffix in PROTECTED),
                "an index-only backup unexpectedly contains durable payloads")
    else:
        unchanged(original, saved)


def test_artifact_location(br: Path, seed: Path, ambient: Path, root: Path,
                           relative: bool, fail: bool, unix_mode: int | None = None,
                           index_only: bool = False) -> None:
    db = root / "external-copy" / "nondefault.db"
    retain(family(seed), db)
    if index_only:
        poison_index_headers(db)
    else:
        shm = bytearray(component(db, "-shm").read_bytes())
        require(len(shm) >= 96, "fixture needs both WAL-index headers")
        shm[16] ^= 0xFF  # Existing unrecognized-index-damage regression uses this shape.
        component(db, "-shm").write_bytes(shm)
    if fail:
        # Deliberately damage only this newly allocated disposable copy.
        image = bytearray(db.read_bytes())
        image[:16] = b"GH556 bad header"
        db.write_bytes(image)
    if unix_mode is not None:
        for suffix in SUFFIXES:
            path = component(db, suffix)
            if path.exists():
                path.chmod(unix_mode)
    modes_before = family_modes(db)
    before = family(db)
    retain(before, root / "before" / db.name)
    ambient_before = recovery_files(ambient)
    ambient_db_before = family(ambient / "beads.db")
    spelling = os.path.relpath(db, root) if relative else str(db)
    result = run_br(br, root, ambient, ["--db", spelling, "--no-auto-import",
                    "--no-auto-flush", "doctor", "migrate-schema", "recover", "--json"], "recover")
    after = family(db)
    retain(after, root / "after" / db.name)
    unchanged(before, after, PROTECTED)
    unchanged(ambient_db_before, family(ambient / "beads.db"))
    require((result.returncode != 0) == fail,
            f"unexpected recovery result: {result.returncode}: {result.stdout}{result.stderr}")
    assert_artifact_scope(db, ambient, ambient_before, "failed" if fail else "complete", before)
    if unix_mode is not None:
        unchanged_modes(modes_before, family_modes(db))
    if not fail:
        # Query distinct copies; never let the oracle repair the observations.
        rows = []
        for label, observed in (("before", before), ("after", after)):
            inspection = root / f"oracle-{label}" / db.name
            retain(observed, inspection)
            with contextlib.closing(readonly(inspection)) as reader:
                found = reader.execute(
                    "SELECT id, status, closed_at, close_reason FROM issues WHERE title=?",
                    (TITLE,),
                ).fetchall()
                require(len(found) == 1, "recovery lost or duplicated the committed sentinel")
                rows.append(found)
        require(rows[0] == rows[1], "successful recovery changed committed issue state")
        unchanged(after, family(db))
        if unix_mode is not None:
            runs = db.parent / ".br_recovery" / "schema-migrations"
            receipt = json.loads(next(runs.glob("*/recovery-complete.json")).read_text())
            expected_scope = "wal-index-only" if index_only else "complete-family"
            require(receipt["backup_scope"] == expected_scope,
                    f"permission regression exercised the wrong recovery branch: {receipt}")
            backup = Path(receipt["backup_path"]) / db.name
            for mode in family_modes(backup).values():
                require(mode is None or mode == 0o600, "recovery backup is not private")


class FixtureChecks(unittest.TestCase):
    """Fixture/oracle tests, explicitly NOT evidence of a passing br binary."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-556-selftest-"))
        self.db = self.root / "copy.db"
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            conn.execute("CREATE TABLE issues (id TEXT PRIMARY KEY, title TEXT, status TEXT, "
                         "closed_at TEXT, close_reason TEXT)")
            conn.execute("INSERT INTO issues VALUES ('w556-1', ?, 'open', NULL, '')", (TITLE,))
            conn.commit()

    def test_reader_pins_old_state_and_closure_is_wal_only(self):
        with pinned_closure(self.db) as issue_id:
            state = family(self.db)
            observed = self.root / "observed" / self.db.name
            retain(state, observed)
            with contextlib.closing(readonly(observed)) as conn:
                self.assertEqual(issue_state(conn, issue_id), ("closed", STAMP, REASON))

    @unittest.skipUnless(os.name == "posix", "requires Unix permission bits")
    def test_permission_oracle_rejects_live_mode_drift_but_keeps_copy_bytes_equal(self):
        self.db.chmod(0o640)
        before = family(self.db)
        modes = family_modes(self.db)
        private = self.root / "private" / self.db.name
        retain(before, private)
        private.chmod(0o600)
        unchanged(before, family(private))
        with self.assertRaisesRegex(AssertionError, "live permissions"):
            unchanged_modes(modes, family_modes(private))
        private.chmod(0o640)
        unchanged_modes(modes, family_modes(private))
        unchanged_modes(modes, family_modes(self.db))
        unchanged(before, family(self.db))

    def test_orphaned_wal_has_a_committed_closure_without_live_openers(self):
        issue_id = orphaned_closure(self.db)
        before = family(self.db)
        observed = self.root / "observed" / self.db.name
        retain(before, observed)
        with contextlib.closing(readonly(observed)) as conn:
            self.assertEqual(issue_state(conn, issue_id), ("closed", STAMP, REASON))
        main_only = self.root / "main-only.db"
        main_only.write_bytes(before[""])
        with contextlib.closing(sqlite3.connect(
                main_only.as_uri() + "?immutable=1", uri=True)) as conn:
            self.assertEqual(issue_state(conn, issue_id)[0], "open")
        unchanged(before, family(self.db))

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_ofd_exclusion_refuses_a_pinned_stock_sqlite_reader(self):
        with pinned_closure(self.db):
            before = family(self.db)
            with self.assertRaises(OSError) as caught:
                with sqlite_main_exclusion(self.db):
                    self.fail("live stock SQLite peer was not excluded")
            self.assertIn(caught.exception.errno, (errno.EAGAIN, errno.EACCES))
            unchanged(before, family(self.db))

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_ofd_exclusion_blocks_an_arriving_writer_and_survives_unrelated_close(self):
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            self.assertEqual(conn.execute("PRAGMA journal_mode=WAL").fetchone()[0], "wal")
        before = family(self.db)
        writer = """
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1], timeout=0)
try:
    conn.execute("UPDATE issues SET title='arriving writer'")
    conn.commit()
except sqlite3.OperationalError as error:
    if error.sqlite_errorcode == sqlite3.SQLITE_BUSY:
        print('SQLITE_BUSY', flush=True)
        sys.exit(42)
    raise
finally:
    conn.close()
"""
        with sqlite_main_exclusion(self.db):
            with self.db.open("rb") as unrelated:
                unrelated.read(1)
            result = subprocess.run([sys.executable, "-I", "-c", writer, str(self.db)],
                                    capture_output=True, text=True, timeout=5, check=False)
            self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
            self.assertEqual(result.stdout.strip(), "SQLITE_BUSY")
            unchanged(before, family(self.db))
        result = subprocess.run([sys.executable, "-I", "-c", writer, str(self.db)],
                                capture_output=True, text=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        with contextlib.closing(readonly(self.db)) as conn:
            self.assertEqual(conn.execute("SELECT title FROM issues").fetchone()[0],
                             "arriving writer")

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_a_writable_private_copy_does_not_conflict_with_live_exclusion(self):
        before = family(self.db)
        with sqlite_main_exclusion(self.db):
            private = self.root / "private" / self.db.name
            retain(before, private)
            with contextlib.closing(sqlite3.connect(private, timeout=0)) as conn:
                conn.execute("UPDATE issues SET title='private copy only'")
                conn.commit()
            unchanged(before, family(self.db))

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_poison_fixture_keeps_real_reader_locks_and_committed_records(self):
        with pinned_closure(self.db, leave_wal=True) as issue_id:
            before = family(self.db)
            poison_index_headers(self.db)
            after = family(self.db)
            unchanged(before, after, PROTECTED)
            self.assertEqual(before["-shm"][96:], after["-shm"][96:])
            self.assertEqual(after["-shm"][:48], after["-shm"][48:96])
            with self.assertRaises(OSError) as caught:
                with sqlite_main_exclusion(self.db):
                    self.fail("poison setup released the stock reader's lock")
            self.assertIn(caught.exception.errno, (errno.EAGAIN, errno.EACCES))
            unchanged(after, family(self.db))
            assert_closed_observation(after, self.root / "retained", issue_id)

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_header_only_reader_fixture_really_excludes_recovery(self):
        # Save a valid header from SQLite, then settle all frames before
        # installing that empty-generation header on this disposable copy.
        with contextlib.closing(sqlite3.connect(self.db)) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("UPDATE issues SET close_reason='fixture'")
            writer.commit()
            header = component(self.db, "-wal").read_bytes()[:32]
            self.assertEqual(len(header), 32)
            self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0], 0)
        # No SQLite connection is alive here. This reproduces a settled br
        # header-only family without requiring or pretending to test br.
        component(self.db, "-wal").write_bytes(header)
        with pinned_header_only_reader(self.db) as row:
            self.assertEqual(row[1:], ["open", None, "fixture"])
            poison_index_headers(self.db)
            before = family(self.db)
            with self.assertRaises(OSError) as caught:
                with sqlite_main_exclusion(self.db):
                    self.fail("header-only reader did not exclude recovery")
            self.assertIn(caught.exception.errno, (errno.EAGAIN, errno.EACCES))
            unchanged(before, family(self.db))
        with sqlite_main_exclusion(self.db):
            unchanged(before, family(self.db))

    def test_record_oracle_rejects_a_discarded_committed_wal(self):
        issue_id = orphaned_closure(self.db)
        state = family(self.db)
        state["-wal"] = None
        state["-shm"] = None
        with self.assertRaisesRegex(AssertionError, "lost the committed WAL-only closure"):
            assert_closed_observation(state, self.root / "discarded", issue_id)

    @unittest.skipUnless(HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_torn_wal_probe_does_not_remove_a_stock_readers_exclusion(self):
        with contextlib.closing(sqlite3.connect(self.db)) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("UPDATE issues SET close_reason='settled in main'")
            writer.commit()
            header = component(self.db, "-wal").read_bytes()[:32]
            self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0], 0)
        component(self.db, "-wal").write_bytes(header)
        with pinned_header_only_reader(self.db) as row:
            self.assertEqual(row[1:], ["open", None, "settled in main"])
            # Only a disposable, fully checkpointed fixture is damaged. The
            # reader never runs SQL again and exits without a checkpoint.
            with component(self.db, "-wal").open("r+b") as fixture:
                fixture.truncate(4)
            before = family(self.db)
            self.assertEqual(len(before["-wal"]), 4)
            with self.assertRaises(OSError) as caught:
                with sqlite_main_exclusion(self.db):
                    self.fail("a torn WAL probe bypassed the stock reader's lock")
            self.assertIn(caught.exception.errno, (errno.EAGAIN, errno.EACCES))
            unchanged(before, family(self.db))
        with sqlite_main_exclusion(self.db):
            unchanged(before, family(self.db))

    def test_refusal_oracle_checks_every_family_component(self):
        state = family(self.db)
        result = subprocess.CompletedProcess([], 1, "", REFUSAL)
        for suffix in SUFFIXES:
            with self.subTest(suffix=suffix):
                changed = dict(state)
                changed[suffix] = (changed[suffix] or b"") + b"changed"
                with self.assertRaisesRegex(AssertionError, "changed file bytes"):
                    assert_refusal(result, state, changed)

    def test_nonzero_exit_does_not_hide_wal_mutation(self):
        with pinned_closure(self.db):
            before = family(self.db)
            after = dict(before)
            after["-wal"] = before["-wal"] + b"mutation"
            result = subprocess.CompletedProcess([], 1, "", REFUSAL)
            with self.assertRaisesRegex(AssertionError, "changed file bytes"):
                assert_refusal(result, before, after)

    def test_missing_wal_is_not_equal_to_empty_wal(self):
        before = family(self.db)
        after = dict(before)
        after["-wal"] = b""
        with self.assertRaisesRegex(AssertionError, "changed file bytes"):
            unchanged(before, after)

    def test_wrong_artifact_directory_fails(self):
        ambient = self.root / "ambient"
        (ambient / ".br_recovery").mkdir(parents=True)
        (ambient / ".br_recovery" / "wrong.json").write_text("{}")
        with self.assertRaisesRegex(AssertionError, "ambient BEADS_DIR"):
            assert_artifact_scope(self.db, ambient, {}, "failed", family(self.db))

    def test_unrelated_failure_or_success_is_not_safe_refusal(self):
        state = family(self.db)
        for result in [subprocess.CompletedProcess([], 1, "", "database busy"),
                       subprocess.CompletedProcess([], 0, REFUSAL, "")]:
            with self.assertRaises(AssertionError):
                assert_refusal(result, state, state)
        assert_refusal(subprocess.CompletedProcess([], 1, "", REFUSAL), state, state)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--br", type=Path, help="br executable; never an input database")
    mode.add_argument("--self-test", action="store_true", help="only test fixtures/oracles, not br")
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(FixtureChecks)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        print("Fixture/oracle checks only. No br binary was tested.")
        return 0 if result.wasSuccessful() else 1
    br = args.br.resolve(strict=True)
    require(br.is_file() and os.access(br, os.X_OK), f"not an executable: {br}")
    root = Path(tempfile.mkdtemp(prefix="br-recovery-556-"))
    print(f"Retaining all disposable fixtures and results at {root}", flush=True)
    seed = setup_seed(br, root / "seed")
    ambient_db = setup_seed(br, root / "ambient-workspace")
    cases = [("pinned-wal", lambda directory: test_pinned_wal(
        br, seed, ambient_db.parent, directory)),
             ("orphaned-wal", lambda directory: test_orphaned_wal(
        br, seed, ambient_db.parent, directory)),
             ("automatic-orphaned-wal", lambda directory: test_orphaned_wal(
        br, seed, ambient_db.parent, directory, automatic=True)),
             ("pinned-automatic-list", lambda directory: test_pinned_automatic(
        br, seed, ambient_db.parent, directory, writing=False)),
             ("pinned-automatic-comments", lambda directory: test_pinned_automatic(
        br, seed, ambient_db.parent, directory, writing=True)),
             ("header-only-contention-retry", lambda directory: test_header_only_contention_and_retry(
        br, seed, ambient_db.parent, directory))]
    for relative in (False, True):
        for fail in (False, True):
            label = f"artifacts-{'relative' if relative else 'absolute'}-{'failure' if fail else 'success'}"
            cases.append((label, lambda directory, r=relative, f=fail: test_artifact_location(
                br, seed, ambient_db.parent, directory, r, f)))
    failures = []
    if os.name == "posix":
        for relative in (False, True):
            for index_only in (False, True):
                label = (f"mode-0640-{'relative' if relative else 'absolute'}-"
                         f"{'index-only' if index_only else 'complete-family'}")
                cases.append((label, lambda directory, r=relative, i=index_only: test_artifact_location(
                    br, seed, ambient_db.parent, directory, r, False, 0o640, i)))
    for label, test in cases:
        directory = root / label
        directory.mkdir()
        try:
            test(directory)
            print(f"PASS {label}", flush=True)
        except (AssertionError, OSError, sqlite3.Error, subprocess.TimeoutExpired) as error:
            failures.append(label)
            (directory / "failure.txt").write_text(str(error))
            print(f"FAIL {label}: {error}", file=sys.stderr, flush=True)
    print(f"{len(cases) - len(failures)}/{len(cases)} br regression cases passed; evidence: {root}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
