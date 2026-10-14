#!/usr/bin/env python3
"""GH #562: missing SHM, actionable diagnosis, and verified stock-SQLite recovery.

python3 tests/e2e_scripts/wal_recovery_562.py --self-test
python3 tests/e2e_scripts/wal_recovery_562.py --br target/debug/br

No input database is accepted. Only newly allocated disposable families are
checkpointed. Original observations, failure evidence, and logs are retained.
--self-test exercises stock SQLite and the test oracles, NOT a br executable.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import shlex
import sqlite3
import subprocess
import sys
import tempfile
import unittest

import wal_recovery_556 as fixture

CHECKPOINT_SQL = "PRAGMA wal_checkpoint(TRUNCATE);"
EXPECTED_CLOSURE = ["closed", fixture.STAMP, fixture.REASON]


def without_shm(state: fixture.Family) -> fixture.Family:
    """Omit SHM only when creating a NEW copy, never unlink a live sidecar."""
    result = dict(state)
    result["-shm"] = None
    return result


def run_checkpoint(db: Path, issue_id: str, sql: str = CHECKPOINT_SQL):
    """Run stock SQLite in a child so observers cannot release its POSIX locks."""
    fixture.require(sql == CHECKPOINT_SQL, "unexpected checkpoint SQL")
    script = r"""
import contextlib, json, sqlite3, sys
from pathlib import Path
db, issue_id, sql = sys.argv[1:]
with contextlib.closing(sqlite3.connect(Path(db).as_uri() + '?mode=rw',
                                      uri=True, timeout=0)) as conn:
    conn.execute('PRAGMA wal_autocheckpoint=0')
    checkpoint = conn.execute(sql).fetchone()
    wal = Path(db + '-wal')
    report = {
        'sqlite_version': sqlite3.sqlite_version,
        'checkpoint': checkpoint,
        'wal_bytes_after_checkpoint': wal.stat().st_size if wal.exists() else None,
        'integrity': [row[0] for row in conn.execute('PRAGMA integrity_check')],
        'closure': conn.execute('SELECT status, closed_at, close_reason FROM issues WHERE id=?',
                                (issue_id,)).fetchone(),
    }
    print(json.dumps(report), flush=True)
"""
    result = subprocess.run(
        [sys.executable, "-I", "-c", script, str(db.resolve()), issue_id, sql],
        capture_output=True, text=True, timeout=15, check=False,
    )
    with (db.parent / "checkpoint.stdout").open("x") as out:
        out.write(result.stdout)
    with (db.parent / "checkpoint.stderr").open("x") as out:
        out.write(result.stderr)
    return result


def successful_checkpoint(result: subprocess.CompletedProcess) -> dict:
    fixture.require(result.returncode == 0, f"checkpoint failed: {result.stderr}")
    report = json.loads(result.stdout)
    row = report.get("checkpoint")
    fixture.require(isinstance(row, list) and len(row) == 3
                    and all(type(value) is int for value in row) and row == [0, 0, 0],
                    f"checkpoint did not complete: {row}; exit status alone is insufficient")
    fixture.require(report.get("wal_bytes_after_checkpoint") == 0, "WAL was not truncated")
    fixture.require(report.get("integrity") == ["ok"], "integrity_check failed")
    fixture.require(report.get("closure") == EXPECTED_CLOSURE, "committed closure was lost")
    return report


def main_only_observation(db: Path, destination: Path, issue_id: str) -> None:
    """Prove the closure reached main, not merely a rebuilt WAL index."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("xb") as out:
        out.write(db.read_bytes())
    with contextlib.closing(sqlite3.connect(
            destination.resolve().as_uri() + "?mode=ro&immutable=1", uri=True)) as conn:
        fixture.require(conn.execute("PRAGMA integrity_check").fetchall() == [("ok",)],
                        "checkpointed main failed integrity_check")
        fixture.require(list(fixture.issue_state(conn, issue_id)) == EXPECTED_CLOSURE,
                        "main-only observation lost the WAL-only closure")


def logical_observation(state: fixture.Family, db: Path) -> str:
    """Read a separate oracle copy, not the retained original evidence."""
    fixture.retain(state, db)
    with contextlib.closing(fixture.readonly(db)) as conn:
        return "\n".join(conn.iterdump())


class CheckpointChecks(unittest.TestCase):
    """Stock SQLite fixture/oracle evidence, never a claim that br passed."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-562-selftest-"))
        print(f"Retaining {self.id()}: {self.root}", flush=True)
        self.db = self.root / "seed.db"
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            conn.execute("CREATE TABLE issues (id TEXT PRIMARY KEY, title TEXT, status TEXT, "
                         "closed_at TEXT, close_reason TEXT)")
            conn.execute("INSERT INTO issues VALUES ('w556-1', ?, 'open', NULL, '')",
                         (fixture.TITLE,))
            conn.execute("CREATE TABLE unrelated (id INTEGER PRIMARY KEY, data BLOB, note TEXT)")
            conn.execute("INSERT INTO unrelated VALUES (1, ?, ?)", (b"\x00\xff" * 8192, "text ' λ"))
            conn.commit()

    def test_missing_shm_checkpoint_preserves_commit_and_all_logical_data(self):
        issue_id = fixture.orphaned_closure(self.db)
        original = fixture.family(self.db)
        missing = without_shm(original)
        copy = self.root / "checkpoint" / "-selected ' $HOME ; database.db"
        fixture.retain(missing, copy)
        self.assertFalse(fixture.component(copy, "-shm").exists())
        logical_before = logical_observation(missing, self.root / "oracle-before" / "copy.db")
        report = successful_checkpoint(run_checkpoint(copy, issue_id))
        self.assertEqual(report["sqlite_version"], sqlite3.sqlite_version)
        main_only_observation(copy, self.root / "main-only" / "copy.db", issue_id)
        logical_after = logical_observation(fixture.family(copy),
                                            self.root / "oracle-after" / "copy.db")
        self.assertEqual(logical_before, logical_after)
        fixture.unchanged(original, fixture.family(self.db))

    def test_busy_checkpoint_can_exit_zero_but_is_not_recovery_success(self):
        with fixture.pinned_closure(self.db, leave_wal=True) as issue_id:
            before = fixture.family(self.db)
            result = run_checkpoint(self.db, issue_id)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["checkpoint"][0], 1)
            with self.assertRaisesRegex(AssertionError, "checkpoint did not complete"):
                successful_checkpoint(result)
            # A busy checkpoint may backfill main; it must not discard WAL.
            self.assertEqual(fixture.family(self.db)["-wal"], before["-wal"])
        copy = self.root / "retry" / "copy.db"
        fixture.retain(without_shm(fixture.family(self.db)), copy)
        successful_checkpoint(run_checkpoint(copy, issue_id))
        main_only_observation(copy, self.root / "retry-main" / "copy.db", issue_id)

    @unittest.skipUnless(fixture.HAS_LINUX_OFD, "requires Linux OFD locks")
    def test_sqlite_checkpoint_cannot_run_inside_the_existing_main_file_exclusion(self):
        issue_id = fixture.orphaned_closure(self.db)
        copy = self.root / "excluded" / "copy.db"
        fixture.retain(without_shm(fixture.family(self.db)), copy)
        before = fixture.family(copy)
        with fixture.sqlite_main_exclusion(copy):
            result = run_checkpoint(copy, issue_id)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("locked", result.stderr)
            fixture.unchanged(before, fixture.family(copy))

    def test_integrity_ok_without_wal_does_not_prove_committed_data_survived(self):
        issue_id = fixture.orphaned_closure(self.db)
        before = fixture.family(self.db)
        with self.assertRaisesRegex(AssertionError, "lost the WAL-only closure"):
            main_only_observation(self.db, self.root / "discarded" / "copy.db", issue_id)
        fixture.unchanged(before, fixture.family(self.db))

    def test_non_success_and_malformed_checkpoint_rows_are_rejected(self):
        for row in [None, [], [0, 0], [1, 1, 0], [0, -1, -1], [0, 2, 2],
                    [False, 0, 0], ["0", 0, 0], [0, 0, 0, 0]]:
            with self.subTest(row=row):
                report = {"checkpoint": row, "wal_bytes_after_checkpoint": 0,
                          "integrity": ["ok"], "closure": EXPECTED_CLOSURE}
                result = subprocess.CompletedProcess([], 0, json.dumps(report), "")
                with self.assertRaisesRegex(AssertionError, "checkpoint did not complete"):
                    successful_checkpoint(result)

    def test_checkpoint_result_alone_does_not_hide_bad_records_or_integrity(self):
        good = {"checkpoint": [0, 0, 0], "wal_bytes_after_checkpoint": 0,
                "integrity": ["ok"], "closure": EXPECTED_CLOSURE}
        for key, value in [("wal_bytes_after_checkpoint", 32), ("integrity", ["corrupt"]),
                           ("closure", ["open", None, ""])]:
            with self.subTest(key=key):
                bad = dict(good, **{key: value})
                result = subprocess.CompletedProcess([], 0, json.dumps(bad), "")
                with self.assertRaises(AssertionError):
                    successful_checkpoint(result)

    def test_nonexistent_checkpoint_target_is_not_created(self):
        missing = self.root / "does-not-exist.db"
        result = run_checkpoint(missing, "w556-1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(missing.exists())


def assert_advice(output: str, db: Path, wal_bytes: int) -> dict:
    report = json.loads(output)
    advice = report["engine"]["wal_recovery"]
    fixture.require(advice["code"] == "WAL_CHECKPOINT_REQUIRED", "missing recovery code")
    fixture.require(advice["reason"] == "missing_shm", "wrong missing-index diagnosis")
    fixture.require(advice["wal_bytes"] == wal_bytes, "wrong WAL byte count")
    fixture.require(advice["automatic_recovery_supported"] is False, "unsafe recovery advertised")
    fixture.require(advice["requires_quiescence"] is True, "missing quiescence precondition")
    fixture.require(advice["checkpoint_success_row"] == "0|0|0", "busy is not success")
    fixture.require(advice["integrity_check_success_row"] == "ok", "missing integrity condition")
    argv = advice["checkpoint_argv"]
    fixture.require(argv == ["sqlite3", "-batch", "-bail", str(db.resolve()), CHECKPOINT_SQL],
                    f"checkpoint must target the selected --db: {argv}")
    fixture.require(advice["integrity_check_argv"] == ["sqlite3", "-batch", "-bail", "-readonly",
                    str(db.resolve()), "PRAGMA integrity_check;"], "wrong verification target")
    if os.name == "posix":
        fixture.require(advice["command_shell"] == "posix", "unlabelled shell dialect")
        fixture.require(shlex.split(advice["checkpoint_command"]) == argv, "unsafe shell quoting")
        fixture.require(shlex.split(advice["integrity_check_command"])
                        == advice["integrity_check_argv"], "unsafe integrity command quoting")
    return advice


def test_binary(br: Path, seed: Path, ambient: Path, root: Path, relative: bool) -> None:
    origin = root / "original" / "seed.db"
    fixture.retain(fixture.family(seed), origin)
    issue_id = fixture.orphaned_closure(origin)
    original = fixture.family(origin)
    missing = without_shm(original)
    selected = root / "selected" / "-selected ' $HOME ; database.db"
    fixture.retain(missing, selected)
    before = fixture.family(selected)
    modes = fixture.family_modes(selected)
    ambient_before = fixture.family(ambient / "beads.db")
    ambient_artifacts = fixture.recovery_files(ambient)
    spelling = os.path.relpath(selected, root) if relative else str(selected)
    flags = ["--db", spelling, "--no-auto-import", "--no-auto-flush"]
    for command in ["info", "doctor"]:
        result = fixture.run_br(br, root, ambient, [*flags, command, "--json"], command)
        fixture.unchanged(before, fixture.family(selected))
        fixture.unchanged_modes(modes, fixture.family_modes(selected))
        # doctor may report unrelated findings; its engine diagnostic must
        # still be available. An unrelated error envelope fails this check.
        if command == "info":
            fixture.require(result.returncode == 0, result.stdout + result.stderr)
        assert_advice(result.stdout, selected, len(before["-wal"]))
    human = fixture.run_br(br, root, ambient, [*flags, "info"], "info-human")
    fixture.require(human.returncode == 0, human.stderr)
    fixture.require("WAL_CHECKPOINT_REQUIRED" in human.stdout, "human diagnostic is missing")
    listed = fixture.run_br(br, root, ambient, [*flags, "list", "--all", "--json"], "list")
    fixture.require(listed.returncode == 0, listed.stdout + listed.stderr)
    page = json.loads(listed.stdout)
    rows = page["issues"] if isinstance(page, dict) else page
    fixture.require(isinstance(rows, list), "list did not return issue rows")
    sentinel = next(row for row in rows if row["id"] == issue_id)
    fixture.require([sentinel[key] for key in ("status", "closed_at", "close_reason")]
                    == EXPECTED_CLOSURE, "read-only list lost the WAL-only closure")
    for command, label in [(["create", "must be refused", "--json"], "create-refused"),
                           (["doctor", "migrate-schema", "recover", "--json"], "recover-refused")]:
        result = fixture.run_br(br, root, ambient, [*flags, *command], label)
        fixture.assert_refusal(result, before, fixture.family(selected))
        fixture.require("SYNC_CONFLICT" in result.stdout + result.stderr, "wrong refusal code")
    fixture.unchanged(before, fixture.family(selected))
    fixture.unchanged_modes(modes, fixture.family_modes(selected))
    fixture.require(not (selected.parent / ".br_recovery").exists(), "refusal created artifacts")
    # Rehearse on ANOTHER copy. The retained pre-recovery selection stays intact.
    recovered = root / "rehearsal" / selected.name
    fixture.retain(missing, recovered)
    result = fixture.run_br(br, root, ambient, ["--db", str(recovered), "info", "--json"],
                            "rehearsal-info")
    fixture.require(result.returncode == 0, result.stdout + result.stderr)
    advice = assert_advice(result.stdout, recovered, len(missing["-wal"]))
    successful_checkpoint(run_checkpoint(recovered, issue_id, advice["checkpoint_argv"][-1]))
    main_only_observation(recovered, root / "main-only" / "recovered.db", issue_id)
    result = fixture.run_br(br, root, ambient, ["--db", str(recovered), "--no-auto-import",
                            "--no-auto-flush", "create", "after verified checkpoint", "--json"],
                            "create-after-checkpoint")
    fixture.require(result.returncode == 0, result.stdout + result.stderr)
    observed = root / "after-write-oracle" / "recovered.db"
    fixture.retain(fixture.family(recovered), observed)
    with contextlib.closing(fixture.readonly(observed)) as conn:
        fixture.require(list(fixture.issue_state(conn, issue_id)) == EXPECTED_CLOSURE,
                        "br write after checkpoint lost the previous committed closure")
        fixture.require(conn.execute("SELECT count(*) FROM issues WHERE title=?",
                        ("after verified checkpoint",)).fetchone()[0] == 1, "new br write missing")
    fixture.unchanged(before, fixture.family(selected))
    fixture.unchanged(original, fixture.family(origin))
    fixture.unchanged(ambient_before, fixture.family(ambient / "beads.db"))
    fixture.require(fixture.recovery_files(ambient) == ambient_artifacts, "ambient evidence changed")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--self-test", action="store_true")
    modes.add_argument("--br", type=Path)
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(CheckpointChecks)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        print(f"SQLite {sqlite3.sqlite_version}: fixture/oracle tests only; no br binary tested.")
        return 0 if result.wasSuccessful() else 1
    br = args.br.resolve(strict=True)
    fixture.require(br.is_file() and os.access(br, os.X_OK), f"not an executable: {br}")
    fixture.require(os.name == "posix", "br's on-disk-index diagnostic is Unix-specific")
    root = Path(tempfile.mkdtemp(prefix="br-recovery-562-"))
    print(f"Retaining all disposable fixtures and results at {root}", flush=True)
    seed = fixture.setup_seed(br, root / "seed")
    ambient = fixture.setup_seed(br, root / "ambient").parent
    failures = []
    for label, relative in [("absolute", False), ("relative", True)]:
        case = root / label
        case.mkdir()
        try:
            test_binary(br, seed, ambient, case, relative)
            print(f"PASS {label}", flush=True)
        except Exception as error:
            failures.append(label)
            (case / "failure.txt").write_text(repr(error))
            print(f"FAIL {label}: {error}", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
