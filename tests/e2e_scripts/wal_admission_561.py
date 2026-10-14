#!/usr/bin/env python3
"""GH #561 / FrankenSQLite #515: disposable admission qualification.

    python3 tests/e2e_scripts/wal_admission_561.py --self-test
    python3 tests/e2e_scripts/wal_admission_561.py --br target/debug/br

The self-tests validate fixtures and the independent byte oracle, NOT br or Rust.
The binary mode requires every case to pass; it has no expected-failure escape.
Run wal_recovery_556.py as well: its genuine pinned readers and WAL-only closures
remain mandatory. No input database is accepted. All fixtures/logs are retained.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import queue
import sqlite3
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import unittest

SUFFIXES = ("", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head")
PROTECTED = tuple(s for s in SUFFIXES if s != "-shm")
TITLE = "GH561 preserved admission sentinel"


def require(ok: bool, message: str) -> None:
    if not ok:
        raise AssertionError(message)


def component(db: Path, suffix: str) -> Path:
    return db.with_name(db.name + suffix)


def family(db: Path) -> dict[str, bytes | None]:
    result = {}
    for suffix in SUFFIXES:
        path = component(db, suffix)
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            result[suffix] = None
            continue
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1,
                f"unsafe fixture component: {path}")
        result[suffix] = path.read_bytes()
    return result


def modes(db: Path) -> dict[str, int | None]:
    return {s: stat.S_IMODE(component(db, s).stat().st_mode)
            if component(db, s).exists() else None for s in PROTECTED}


def retain(state: dict[str, bytes | None], db: Path) -> None:
    db.parent.mkdir(parents=True, exist_ok=True)
    for suffix, data in state.items():
        if data is not None:
            with component(db, suffix).open("xb") as stream:
                stream.write(data)


def unchanged(before: dict, after: dict, suffixes=SUFFIXES) -> None:
    changed = [s or "main" for s in suffixes if before[s] != after[s]]
    require(not changed, f"protected presence/bytes changed: {changed}")


def page_size(main: bytes) -> int:
    require(len(main) >= 100 and main[:16] == b"SQLite format 3\0", "invalid database header")
    size = int.from_bytes(main[16:18], "big")
    size = 65536 if size == 1 else size
    require(512 <= size <= 65536 and size & (size - 1) == 0, "invalid database page size")
    return size


def wal_header(wal: bytes, size: int) -> None:
    """Independent strict WAL proof; never truncate input to a header prefix."""
    require(len(wal) == 32, "WAL must contain exactly one header and no frame bytes")
    magic, version, pages, _, _, _, expected_a, expected_b = struct.unpack(">8I", wal)
    require(magic in (0x377F0682, 0x377F0683) and version == 3007000,
            "invalid WAL magic/version")
    require(pages == size and 512 <= pages <= 65536 and pages & (pages - 1) == 0,
            "WAL/database page-size disagreement")
    words = struct.unpack((">" if magic & 1 else "<") + "6I", wal[:24])
    a = b = 0
    for first, second in zip(words[::2], words[1::2]):
        a = (a + first + b) & 0xFFFFFFFF
        b = (b + second + a) & 0xFFFFFFFF
    require((a, b) == (expected_a, expected_b), "invalid WAL checksum")


def preserved_admission(before: dict, after: dict) -> None:
    """Narrow test oracle, not permission for the application to publish files.

    Full live preservation is preferable. An existing empty WAL may gain a
    complete valid header only when main/journal/certificate bytes are identical.
    No missing-file exception, reset, truncation, partial header or frame allowed.
    """
    unchanged(before, after, tuple(s for s in PROTECTED if s != "-wal"))
    size = page_size(before[""])
    old, new = before["-wal"], after["-wal"]
    for value in (old, new):
        if value is not None and value != b"":
            wal_header(value, size)
    if old != new:
        require(old == b"" and new is not None and len(new) == 32,
                "WAL change is not a zero-byte to header generation binding")


def sqlite_snapshot(db: Path) -> tuple[dict, bytes]:
    """Checkpoint a new fixture only; obtain a real SQLite-generated WAL header."""
    with contextlib.closing(sqlite3.connect(db)) as conn:
        require(conn.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",), "WAL unavailable")
        conn.execute("PRAGMA wal_autocheckpoint=0")
        # A harmless transactional user_version assignment forces a real header.
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        conn.execute(f"PRAGMA user_version={int(version)}")
        conn.commit()
        generated = component(db, "-wal").read_bytes()
        require(len(generated) > 32, "SQLite fixture produced no WAL frames")
        header = generated[:32]
        require(conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone() == (0, 0, 0),
                "fixture checkpoint did not complete")
        require(component(db, "-wal").read_bytes() == b"", "fixture WAL is not empty")
        require(conn.execute("PRAGMA integrity_check").fetchall() == [("ok",)], "bad fixture")
        state = family(db)
    wal_header(header, page_size(state[""]))
    # The captured main image contains every committed row. Only create NEW
    # families from these bytes; never shorten an existing live database's WAL.
    state["-wal"] = b""
    state["-shm"] = None
    return state, header


def records(state: dict, root: Path) -> list[tuple]:
    db = root / "oracle.db"
    retain(state, db)
    with contextlib.closing(sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True)) as conn:
        require(conn.execute("PRAGMA integrity_check").fetchall() == [("ok",)],
                "post-command SQLite integrity check failed")
        return conn.execute("SELECT id,title,status FROM issues ORDER BY id").fetchall()


def recovery_evidence(directory: Path) -> dict[str, str]:
    root = directory / ".br_recovery"
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file()}


def run_br(br: Path, cwd: Path, ambient: Path, args: list[str], label: str):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("BEADS_", "BD_", "BR_"))}
    home = cwd / "isolated-home"
    home.mkdir(exist_ok=True)
    env.update(BEADS_DIR=str(ambient), HOME=str(home), USERPROFILE=str(home),
               XDG_CONFIG_HOME=str(home / "config"), NO_COLOR="1")
    result = subprocess.run([str(br), *args], cwd=cwd, env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60, check=False)
    (cwd / (label + ".stdout")).write_text(result.stdout)
    (cwd / (label + ".stderr")).write_text(result.stderr)
    require(result.returncode >= 0, f"br terminated by signal: {result.returncode}")
    return result


def succeeded(result) -> None:
    require(result.returncode == 0, f"br failed ({result.returncode}): {result.stdout}{result.stderr}")


@contextlib.contextmanager
def held_main_lock(db: Path):
    """Separate-process POSIX lock conflicts with RecoveryLock's entire range.

    No SQLite opener is allowed to initialize the deliberately missing SHM before
    the test. Real SQLite reader coverage is separately supplied by GH #556.
    """
    script = """
import fcntl, os, sys
fd = os.open(sys.argv[1], os.O_RDWR)
fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 512, 0x40000000, os.SEEK_SET)
print('locked', flush=True)
assert sys.stdin.readline().strip() == 'release'
os.close(fd)
"""
    child = subprocess.Popen([sys.executable, "-I", "-u", "-c", script, str(db)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             text=True)
    ready = queue.Queue()
    reader = threading.Thread(target=lambda: ready.put(child.stdout.readline()), daemon=True)
    reader.start()
    try:
        try:
            line = ready.get(timeout=10)
        except queue.Empty as error:
            raise AssertionError("main-file lock holder timed out") from error
        require(line.strip() == "locked", "main-file lock holder failed")
        yield
    finally:
        try:
            if child.poll() is None:
                child.stdin.write("release\n")
                child.stdin.flush()
            child.wait(timeout=10)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            child.kill()
            child.wait(timeout=5)
        finally:
            child.stdin.close()
            reader.join(timeout=5)
            child.stdout.close()
            error = child.stderr.read()
            child.stderr.close()
        require(child.returncode == 0, f"lock holder failed: {error}")


def qualify(br: Path, root: Path) -> None:
    seed = root / "seed"
    seed.mkdir()
    ambient = seed / ".beads"
    succeeded(run_br(br, seed, ambient, ["init", "--prefix", "w561"], "init"))
    succeeded(run_br(br, seed, ambient, ["create", TITLE], "create"))
    state, header = sqlite_snapshot(ambient / "beads.db")
    expected = records(state, root / "seed-oracle")
    require(any(row[1] == TITLE for row in expected), "missing fixture sentinel")
    ambient_before = family(ambient / "beads.db")
    evidence_before = recovery_evidence(ambient)
    failures = []

    def case(name, wal, *, recover=False, corrupt=False, locked=False, source=None):
        directory = root / name
        directory.mkdir()
        db = directory / "external family" / "selected.db"
        initial = dict(state if source is None else source)
        initial["-wal"] = wal
        if corrupt:
            initial[""] = b"not a database!!\0" + initial[""][16:]
        retain(initial, db)
        if os.name == "posix":
            for suffix in SUFFIXES:
                if component(db, suffix).exists():
                    component(db, suffix).chmod(0o640)
        before = family(db)
        permissions = modes(db)
        retain(before, directory / "before" / db.name)
        (directory / "before-modes.json").write_text(json.dumps(permissions, indent=2))
        command = ["doctor", "migrate-schema", "recover", "--json"] if recover else ["list", "--all", "--json"]
        args = ["--db", str(db), "--no-auto-import", "--no-auto-flush", *command]
        must_fail = corrupt or locked or (wal is not None and len(wal) not in (0, 32)) or wal == bytes(32)
        old_evidence = {}
        # Exercise repeated admission, not just one successful open.
        for attempt in range(2):
            immediately_before = family(db)
            lock = held_main_lock(db) if locked else contextlib.nullcontext()
            with lock:
                result = run_br(br, directory, ambient, args, f"attempt-{attempt}")
                after = family(db)
            retain(after, directory / f"observed-{attempt}" / db.name)
            if must_fail:
                unchanged(before, after)
                require(result.returncode != 0 and bool(result.stdout + result.stderr),
                        "unsafe/contended recovery succeeded or had no diagnostic")
                if wal is not None and len(wal) > 32:
                    require("engine recovery refused before opening" in result.stdout + result.stderr,
                            "framed WAL failed for an unrelated reason")
                    require(not (db.parent / ".br_recovery").exists(),
                            "framed refusal allocated recovery evidence")
                if source is not None:
                    require(records(after, directory / f"framed-oracle-{attempt}") ==
                            records(before, directory / f"expected-framed-{attempt}"),
                            "failed recovery lost committed WAL-only records")
                if locked:
                    require(any(word in (result.stdout + result.stderr).lower()
                                for word in ("busy", "contention", "locked")),
                            "locked recovery did not report contention")
            else:
                preserved_admission(immediately_before, after)
                succeeded(result)
                if not recover:
                    decoded = json.loads(result.stdout)
                    require(isinstance(decoded, list) and any(row.get("title") == TITLE for row in decoded),
                            "ordinary read did not return the committed sentinel")
                require(records(after, directory / f"oracle-{attempt}") == expected,
                        "admission changed committed issue records")
            require(modes(db) == permissions, "protected permissions/presence changed")
            evidence = recovery_evidence(db.parent)
            require(all(evidence.get(k) == v for k, v in old_evidence.items()),
                    "retry changed or removed earlier recovery evidence")
            old_evidence = evidence
            if corrupt:
                receipts = list((db.parent / ".br_recovery" / "schema-migrations").glob(
                    "*/recovery-failed.json"))
                require(bool(receipts), "failed private recovery retained no failure receipt")
                for receipt_path in receipts:
                    receipt = json.loads(receipt_path.read_text())
                    require(Path(receipt["database_path"]).resolve() == db.resolve(),
                            "failure receipt is bound to the wrong database")
                    require(receipt["backup_scope"] == "complete-family", "missing complete backup")
                    backup = Path(receipt["backup_path"])
                    require(backup.resolve() == (receipt_path.parent / "recovery-before").resolve(),
                            "failure backup is outside its selected-database recovery run")
                    unchanged(before, family(backup / db.name))
            unchanged(ambient_before, family(ambient / "beads.db"))
            require(recovery_evidence(ambient) == evidence_before, "recovery evidence was misrouted")
        if locked:
            # Once exclusion is released, the same preserved family must recover;
            # an earlier contention failure must not poison every future attempt.
            succeeded_result = run_br(br, directory, ambient, args, "after-lock-release")
            after = family(db)
            preserved_admission(before, after)
            succeeded(succeeded_result)
            require(records(after, directory / "released-oracle") == expected,
                    "retry after contention lost records")
            evidence = recovery_evidence(db.parent)
            require(all(evidence.get(k) == v for k, v in old_evidence.items()),
                    "successful retry removed failure evidence")
        if not must_fail:
            # Actual first write after the admission transition, independently read
            # by stock SQLite from a fresh disposable copy.
            title = f"GH561 first write after {name}"
            succeeded(run_br(br, directory, ambient,
                ["--db", str(db), "--no-auto-import", "--no-auto-flush", "create", title], "first-write"))
            rows = records(family(db), directory / "write-oracle")
            require(all(row in rows for row in expected), "first write lost preexisting records")
            require(sum(row[1] == title for row in rows) == 1, "first write was lost or duplicated")

    framed_db = root / "framed-source" / "selected.db"
    retain(state, framed_db)
    with contextlib.closing(sqlite3.connect(framed_db)) as writer:
        writer.execute("PRAGMA wal_autocheckpoint=0")
        before_main = framed_db.read_bytes()
        writer.execute("UPDATE issues SET status='closed', closed_at=?, close_reason=? WHERE id=?",
                       ("2026-10-13T00:00:00Z", "GH561 committed only in WAL", expected[0][0]))
        writer.commit()
        framed = family(framed_db)
        require(framed[""] == before_main and len(framed["-wal"]) > 32,
                "committed WAL-only fixture was not produced")
        require(records(framed, root / "wal-only-oracle") != expected,
                "WAL-only fixture did not change a record")

    matrix = [
        ("empty-missing-shm-read", b"", {}),
        ("empty-missing-shm-recover", b"", {"recover": True}),
        ("header-missing-shm-read", header, {}),
        ("header-missing-shm-recover", header, {"recover": True}),
        ("failed-rehearsal-empty", b"", {"recover": True, "corrupt": True}),
        ("failed-rehearsal-header", header, {"recover": True, "corrupt": True}),
        ("malformed-header", bytes(32), {"recover": True}),
        ("partial-header", header[:31], {"recover": True}),
        ("partial-frame", header + b"x", {"recover": True}),
        ("committed-wal-frames", framed["-wal"], {"recover": True, "source": framed}),
    ]
    if os.name == "posix":
        matrix += [("empty-locked", b"", {"recover": True, "locked": True}),
                   ("header-locked", header, {"recover": True, "locked": True})]
    else:
        raise RuntimeError("native exclusion qualification requires the POSIX lane; no silent skip")
    for name, wal, options in matrix:
        try:
            case(name, wal, **options)
            print(f"PASS {name}", flush=True)
        except Exception as error:
            failures.append((name, str(error)))
            print(f"FAIL {name}: {error}", flush=True)
    (root / "results.json").write_text(json.dumps({"br": str(br), "cases": len(matrix),
        "failures": failures, "sqlite_version": sqlite3.sqlite_version}, indent=2))
    require(not failures, f"{len(failures)}/{len(matrix)} binary qualification cases failed; evidence: {root}")


class OracleTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(tempfile.mkdtemp(prefix="br-561-selftest-"))
        db = cls.root / "source.db"
        with contextlib.closing(sqlite3.connect(db)) as conn:
            conn.execute("CREATE TABLE issues(id TEXT PRIMARY KEY,title TEXT,status TEXT)")
            conn.execute("INSERT INTO issues VALUES('w561-1',?,'open')", (TITLE,))
            conn.commit()
        cls.state, cls.header = sqlite_snapshot(db)
        print(f"Fixture-only evidence: {cls.root}", flush=True)

    def test_real_sqlite_empty_to_header_preserves_records(self):
        after = dict(self.state, **{"-wal": self.header})
        preserved_admission(self.state, after)
        self.assertEqual(records(self.state, self.root / "empty-oracle"),
                         records(after, self.root / "header-oracle"))

    def test_unchanged_malformed_states_are_never_accepted(self):
        for wal in [self.header[:n] for n in range(1, 32)] + [bytes(32), self.header + b"x"]:
            state = dict(self.state, **{"-wal": wal})
            with self.assertRaises(AssertionError):
                preserved_admission(state, state)

    def test_bit_corruption_never_passes(self):
        for at in range(32):
            for bit in range(8):
                value = bytearray(self.header)
                value[at] ^= 1 << bit
                with self.assertRaises(AssertionError):
                    wal_header(bytes(value), page_size(self.state[""]))

    def test_presence_and_reset_changes_are_refused(self):
        header_state = dict(self.state, **{"-wal": self.header})
        missing = dict(self.state, **{"-wal": None})
        for before, after in [(missing, header_state), (header_state, missing),
                              (header_state, self.state), (self.state, missing)]:
            with self.assertRaises(AssertionError):
                preserved_admission(before, after)

    def test_valid_header_reset_with_same_salts_is_refused_in_both_orders(self):
        for big_endian in (False, True):
            values = list(struct.unpack(">8I", self.header))
            values[0] = 0x377F0683 if big_endian else 0x377F0682
            headers = []
            for sequence in (values[3], (values[3] + 1) & 0xFFFFFFFF):
                values[3] = sequence
                prefix = struct.pack(">6I", *values[:6])
                words = struct.unpack((">" if big_endian else "<") + "6I", prefix)
                a = b = 0
                for first, second in zip(words[::2], words[1::2]):
                    a = (a + first + b) & 0xFFFFFFFF
                    b = (b + second + a) & 0xFFFFFFFF
                header = prefix + struct.pack(">2I", a, b)
                wal_header(header, page_size(self.state[""]))
                preserved_admission(self.state, dict(self.state, **{"-wal": header}))
                headers.append(header)
            self.assertEqual(headers[0][16:24], headers[1][16:24])
            with self.assertRaises(AssertionError):
                preserved_admission(dict(self.state, **{"-wal": headers[0]}),
                                    dict(self.state, **{"-wal": headers[1]}))

    def test_database_journal_certificate_mutations_are_refused(self):
        for suffix in ("", "-journal", "-wal-cert", "-wal-cert-head"):
            after = dict(self.state, **{suffix: b"changed", "-wal": self.header})
            with self.assertRaises(AssertionError):
                preserved_admission(self.state, after)

    def test_committed_wal_frames_are_not_frame_free(self):
        db = self.root / "framed" / "selected.db"
        retain(self.state, db)
        with contextlib.closing(sqlite3.connect(db)) as writer:
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("INSERT INTO issues VALUES('w561-2','WAL only','closed')")
            writer.commit()
            before = family(db)
            require(len(before["-wal"]) > 32, "missing genuine frames")
            with self.assertRaises(AssertionError):
                preserved_admission(before, before)
            with self.assertRaises(AssertionError):
                preserved_admission(before, dict(before, **{"-wal": before["-wal"][:32]}))
            self.assertEqual(len(records(before, self.root / "framed-oracle")), 2)
            self.assertEqual(len(records(dict(before, **{"-wal": b"", "-shm": None}),
                                         self.root / "main-only-oracle")), 1)

    @unittest.skipUnless(os.name == "posix", "POSIX-only lock fixture")
    def test_lock_holder_excludes_other_process_without_sidecar_writes(self):
        db = self.root / "locked" / "selected.db"
        retain(self.state, db)
        before = family(db)
        with held_main_lock(db):
            script = """
import sqlite3, sys
try:
    conn = sqlite3.connect(sys.argv[1], timeout=0)
    conn.execute('SELECT * FROM issues').fetchall()
except sqlite3.OperationalError as error:
    assert 'locked' in str(error), str(error)
else:
    raise AssertionError('stock SQLite bypassed the lock')
"""
            subprocess.run([sys.executable, "-I", "-c", script, str(db)], check=True, timeout=10)
            unchanged(before, family(db))
        unchanged(before, family(db))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--br", type=Path)
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(OracleTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() and not result.skipped else 1
    br = args.br.resolve(strict=True)
    require(br.is_file() and os.access(br, os.X_OK), "--br must name an executable")
    root = Path(tempfile.mkdtemp(prefix="br-admission-561-"))
    print(f"Binary qualification evidence: {root}", flush=True)
    qualify(br, root)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
