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
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

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


def readonly(db: Path) -> sqlite3.Connection:
    return sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True, timeout=1)


def issue_state(conn: sqlite3.Connection, issue_id: str) -> tuple:
    row = conn.execute(
        "SELECT status, closed_at, close_reason FROM issues WHERE id = ?", (issue_id,)
    ).fetchone()
    require(row is not None, "the sentinel issue disappeared")
    return row


@contextlib.contextmanager
def pinned_closure(db: Path):
    """Keep both connections alive until the caller records recovery's result."""
    writer = sqlite3.connect(db.resolve().as_uri() + "?mode=rw", uri=True, timeout=1)
    reader = None
    try:
        require(writer.execute("PRAGMA journal_mode=WAL").fetchone()[0] == "wal",
                "fixture needs WAL mode")
        writer.execute("PRAGMA wal_autocheckpoint=0")
        require(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0] == 0,
                "fixture's initial checkpoint did not complete")
        found = writer.execute("SELECT id FROM issues WHERE title = ?", (TITLE,)).fetchall()
        require(len(found) == 1, "fixture must contain exactly one sentinel issue")
        issue_id = found[0][0]
        reader = readonly(db)
        reader.execute("BEGIN")
        initial = issue_state(reader, issue_id)  # BEGIN alone does not pin a snapshot.
        require(initial[0] == "open", f"fixture must start open: {initial}")
        main_before = db.read_bytes()
        writer.execute(
            "UPDATE issues SET status='closed', closed_at=?, close_reason=? WHERE id=?",
            (STAMP, REASON, issue_id),
        )
        writer.commit()
        require(issue_state(reader, issue_id) == initial, "reader snapshot is not pinned")
        require(db.read_bytes() == main_before, "closure unexpectedly reached the main file")
        require(len(component(db, "-wal").read_bytes()) > 32, "no WAL frames were produced")
        with contextlib.closing(readonly(db)) as current:
            require(issue_state(current, issue_id) == ("closed", STAMP, REASON),
                    "a fresh connection cannot see the committed closure")
        # All precondition reads finish before the caller snapshots the SHM.
        yield issue_id
    finally:
        if reader is not None:
            reader.close()
        writer.close()


def assert_refusal(result: subprocess.CompletedProcess, before: Family, after: Family) -> None:
    # Check bytes first: old br can report failure only AFTER a WAL rewrite.
    unchanged(before, after)
    require(result.returncode != 0, "unsafe recovery was reported successful")
    require(REFUSAL in result.stdout + result.stderr,
            f"expected a pre-open WAL refusal, not an unrelated error: {result.stdout}{result.stderr}")


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
            with contextlib.closing(readonly(observed)) as recovered:
                require(issue_state(recovered, issue_id) == ("closed", STAMP, REASON),
                        "a fresh reader of the post-recovery copy lost the committed closure")


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
    unchanged(original, family(backup / db.name))


def test_artifact_location(br: Path, seed: Path, ambient: Path, root: Path,
                           relative: bool, fail: bool) -> None:
    db = root / "external-copy" / "nondefault.db"
    retain(family(seed), db)
    shm = bytearray(component(db, "-shm").read_bytes())
    require(len(shm) >= 96, "fixture needs both WAL-index headers")
    shm[16] ^= 0xFF  # Existing unrecognized-index-damage regression uses this shape.
    component(db, "-shm").write_bytes(shm)
    if fail:
        # Deliberately damage only this newly allocated disposable copy.
        image = bytearray(db.read_bytes())
        image[:16] = b"GH556 bad header"
        db.write_bytes(image)
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
        br, seed, ambient_db.parent, directory))]
    for relative in (False, True):
        for fail in (False, True):
            label = f"artifacts-{'relative' if relative else 'absolute'}-{'failure' if fail else 'success'}"
            cases.append((label, lambda directory, r=relative, f=fail: test_artifact_location(
                br, seed, ambient_db.parent, directory, r, f)))
    failures = []
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
