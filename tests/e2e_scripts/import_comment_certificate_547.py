#!/usr/bin/env python3
"""Exercise #547 with a real br binary, only in newly allocated workspaces.

Usage:
    python3 tests/e2e_scripts/import_comment_certificate_547.py --self-test
    python3 tests/e2e_scripts/import_comment_certificate_547.py --br /path/to/br

No user tracker is accepted. Inputs are synthetic JSONL; every database write
is performed by br. SQLite reads only private copies, including any WAL.
Artifacts and command logs are retained on success and failure. Self-tests
qualify the fixture/oracle, not br, and never count as br-facing test passes.
"""
from __future__ import annotations

import argparse
from collections import Counter
from contextlib import closing
from copy import deepcopy
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import tempfile
import unittest

ISSUE_ID = "bd-547"
FAMILY_SUFFIXES = ("", "-wal", "-shm", "-journal", "-wal-cert")
BASE_TIME = "2026-01-01T00:00:00Z"
LOCAL_TIME = "2026-01-03T00:00:00Z"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def dump(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def timestamp(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "fixture timestamps must have explicit time zones")
    return parsed.astimezone(timezone.utc)


def precise_timestamp(value: str) -> tuple[datetime, int]:
    fractional = re.search(r"[T ]\d{2}:\d{2}:\d{2}\.(\d+)", value)
    digits = fractional.group(1) if fractional else ""
    require(len(digits) <= 9, "timestamp exceeds the model's nanosecond precision")
    nanos = int(digits.ljust(9, "0"))
    return timestamp(value).replace(microsecond=0), nanos


def comment_key(comment: dict) -> tuple:
    # Match portable identity, never database-local rowid; retain multiplicity.
    return (comment["issue_id"], precise_timestamp(comment["created_at"]),
            comment["author"], comment["text"])


def comment_counts(comments: list[dict]) -> Counter:
    return Counter(map(comment_key, comments))


def record(issue_id: str, title: str) -> dict:
    return {"id": issue_id, "title": title, "status": "open", "priority": 2,
            "issue_type": "task", "created_at": BASE_TIME, "updated_at": BASE_TIME}


def write_jsonl(path: Path, records: list[dict]) -> None:
    path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in records),
                    encoding="utf-8")


def read_jsonl(path: Path) -> dict[str, dict]:
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()]
    result = {row["id"]: row for row in rows}
    require(len(result) == len(rows), "duplicate issue IDs in exported JSONL")
    return result


def fingerprint(db: Path) -> dict[str, str | None]:
    return {suffix: (hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None)
            for suffix in FAMILY_SUFFIXES for path in [Path(str(db) + suffix)]}


def inspect_copy(db: Path, destination: Path) -> dict:
    """Do not let a stock-SQLite inspection touch br's original WAL/index."""
    before = fingerprint(db)
    destination.mkdir()
    copy = destination / db.name
    for suffix in FAMILY_SUFFIXES:
        source = Path(str(db) + suffix)
        if source.is_file():
            # Keep raw evidence separately; the inspection must rebuild its
            # own -shm instead of trusting an engine-specific cached index.
            shutil.copyfile(source, destination / ("before-" + source.name))
            if suffix in ("", "-wal", "-journal"):
                shutil.copyfile(source, Path(str(copy) + suffix))
    with closing(sqlite3.connect(copy.as_uri() + "?mode=ro", uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        state = {
            "issues": {row["id"]: dict(row) for row in conn.execute("SELECT * FROM issues")},
            "comments": [dict(row) for row in conn.execute(
                "SELECT id, issue_id, author, text, created_at FROM comments ORDER BY id")],
            "certificates": [row[0] for row in conn.execute("SELECT issue_id FROM export_hashes")],
            "dirty": [row[0] for row in conn.execute("SELECT issue_id FROM dirty_issues")],
            "needs_flush": next(iter(conn.execute(
                "SELECT value FROM metadata WHERE key = 'needs_flush' ORDER BY rowid DESC LIMIT 1")), [None])[0],
        }
    require(fingerprint(db) == before, "inspection changed the original database family")
    dump(destination / "state.json", state)
    return state


def assert_import_state(state: dict, expected: list[dict], control_ids: set[str],
                        needs_flush: bool) -> None:
    actual = [comment for comment in state["comments"] if comment["issue_id"] == ISSUE_ID]
    require(comment_counts(actual) == comment_counts(expected),
            "import lost, changed, or duplicated comment payloads")
    require(set(state["issues"]) == control_ids | {ISSUE_ID}, "import changed the issue ID set")
    require(timestamp(state["issues"][ISSUE_ID]["updated_at"]) == timestamp(LOCAL_TIME),
            "skip-only import replaced the newer local revision")
    certificates = set(state["certificates"])
    require(control_ids <= certificates, "unchanged controls lost export certification")
    require((ISSUE_ID in certificates) == (not needs_flush),
            "#547: export certificate falsely claims incomplete JSONL carries local comments")
    require((state["needs_flush"] == "true") == needs_flush,
            "#547: needs_flush does not reflect database-only comment payloads")


def make_incoming(exported: dict, kind: str) -> tuple[dict, list[dict]]:
    incoming = deepcopy(exported)
    local = exported["comments"]
    require(len(local) == 1 and local[0]["id"] > 0, "fixture must start with one positive comment ID")
    incoming["updated_at"] = "2026-01-02T12:00:00Z"
    if kind == "complete":
        incoming["comments"][0]["id"] += 1000
        return incoming, deepcopy(local)
    peer = deepcopy(local[0])
    peer["author"] = "peer-agent"
    peer["text"] = "peer-only comment: must also survive"
    peer["created_at"] = ("2026-01-01T12:00:00Z" if kind == "incoming-first"
                          else "2026-01-02T00:00:01Z")
    require(peer["id"] == local[0]["id"], "fixture must collide on the local rowid")
    require((comment_key(peer) < comment_key(local[0])) == (kind == "incoming-first"),
            "fixture did not establish the requested union order")
    incoming["comments"] = [peer]
    return incoming, deepcopy(local) + [peer]


class Runner:
    def __init__(self, binary: Path, root: Path, timeout: float):
        self.binary, self.root, self.timeout, self.sequence = binary, root, timeout, 0
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("BEADS_", "BR_", "XDG_"))}
        home = root / "home"
        home.mkdir()
        self.env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                        XDG_CACHE_HOME=str(home / "cache"), NO_COLOR="1", RUST_LOG="error")

    def run(self, workspace: Path, *arguments: str) -> dict:
        self.sequence += 1
        argv = [str(self.binary), "--no-auto-import", "--no-auto-flush", "--json", *arguments]
        log = self.root / f"command-{self.sequence:03d}.json"
        try:
            result = subprocess.run(argv, cwd=workspace, env=self.env, capture_output=True,
                                    text=True, encoding="utf-8", errors="replace", timeout=self.timeout,
                                    check=False)
        except subprocess.TimeoutExpired as error:
            dump(log, {"argv": argv, "cwd": str(workspace), "timed_out": True,
                       "stdout": str(error.stdout), "stderr": str(error.stderr)})
            raise AssertionError(f"br timed out, not a passing regression: {log}") from error
        receipt = {"argv": argv, "cwd": str(workspace), "returncode": result.returncode,
                   "stdout": result.stdout, "stderr": result.stderr}
        dump(log, receipt)
        require(result.returncode == 0, f"br command failed, not a passing regression: {log}")
        return receipt


def run_case(binary: Path, root: Path, kind: str, controls: int, timeout: float) -> None:
    workspace = root / "workspace"
    workspace.mkdir()
    runner = Runner(binary, root, timeout)
    runner.run(workspace, "init", "--prefix", "bd")
    beads = workspace / ".beads"
    jsonl = beads / "issues.jsonl"
    db = beads / "beads.db"
    local = record(ISSUE_ID, "same scalar payload on both clones")
    local["updated_at"] = LOCAL_TIME
    local["comments"] = [{"id": 73, "issue_id": ISSUE_ID, "author": "local-agent",
                          "text": "local-only comment: must reach the export",
                          "created_at": "2026-01-02T00:00:00Z"}]
    unchanged = [record(f"bd-control-{index:04d}", f"unchanged control {index}")
                 for index in range(controls)]
    control_ids = {issue["id"] for issue in unchanged}
    write_jsonl(jsonl, unchanged + [local])
    runner.run(workspace, "sync", "--import-only")
    # Force is used only to establish a canonical, fully exported clean fixture.
    # The tested import, subsequent flush, and repeat import are all unforced.
    runner.run(workspace, "sync", "--flush-only", "--force")
    exported = read_jsonl(jsonl)
    require(set(exported) == control_ids | {ISSUE_ID}, "initial export omitted fixture rows")
    baseline = inspect_copy(db, root / "baseline")
    assert_import_state(baseline, exported[ISSUE_ID]["comments"], control_ids, False)
    require(not baseline["dirty"], "initial fixture must have no dirty markers")
    incoming, expected = make_incoming(exported[ISSUE_ID], kind)
    # In the large case all 900 cached skips certify before the mutation;
    # the following target exercises reuse after a completed certificate batch.
    write_jsonl(jsonl, [exported[row["id"]] for row in unchanged] + [incoming])
    incoming_bytes = jsonl.read_bytes()
    (root / "incoming.jsonl").write_bytes(incoming_bytes)
    runner.run(workspace, "sync", "--import-only")
    require(jsonl.read_bytes() == incoming_bytes, "import unexpectedly rewrote its JSONL source")
    state = inspect_copy(db, root / "after-import")
    # Record the unforced export even when the certificate oracle detects the
    # bug, so retained evidence shows whether the local comment reached JSONL.
    failures = []
    try:
        assert_import_state(state, expected, control_ids, kind != "complete")
    except AssertionError as error:
        failures.append(str(error))
    runner.run(workspace, "sync", "--flush-only")
    flushed = read_jsonl(jsonl)
    require(set(flushed) == set(exported), "flush changed the fixture issue ID set")
    if comment_counts(flushed[ISSUE_ID].get("comments", [])) != comment_counts(expected):
        failures.append("#547: ordinary unforced flush did not export the complete comment union")
    (root / "flushed.jsonl").write_bytes(jsonl.read_bytes())
    require(not failures, "; ".join(failures))
    flushed_bytes = jsonl.read_bytes()
    runner.run(workspace, "sync", "--import-only")
    assert_import_state(inspect_copy(db, root / "after-repeat"), expected, control_ids, False)
    require(jsonl.read_bytes() == flushed_bytes, "repeat import rewrote the completed export")


class OracleTests(unittest.TestCase):
    def state(self) -> tuple[dict, list[dict]]:
        local = {"id": 73, "issue_id": ISSUE_ID, "author": "local-agent", "text": "local",
                 "created_at": "2026-01-02T00:00:00Z"}
        peer = dict(local, id=74, author="peer-agent", text="peer")
        comments = [local, peer]
        return ({"issues": {ISSUE_ID: {"updated_at": LOCAL_TIME}, "bd-control": {}},
                 "comments": deepcopy(comments), "certificates": ["bd-control"],
                 "needs_flush": "true", "dirty": []}, comments)

    def test_correct_union_and_renumbered_ids_pass(self):
        state, expected = self.state()
        state["comments"][0]["id"] = 1234
        assert_import_state(state, expected, {"bd-control"}, True)

    def test_false_certificate_is_rejected_even_when_database_has_both_comments(self):
        state, expected = self.state()
        state["certificates"].append(ISSUE_ID)
        with self.assertRaisesRegex(AssertionError, "certificate"):
            assert_import_state(state, expected, {"bd-control"}, True)

    def test_missing_flush_signal_is_rejected(self):
        state, expected = self.state()
        state["needs_flush"] = "false"
        with self.assertRaisesRegex(AssertionError, "needs_flush"):
            assert_import_state(state, expected, {"bd-control"}, True)

    def test_loss_duplication_owner_and_timestamp_changes_are_rejected(self):
        for mutation in range(4):
            with self.subTest(mutation=mutation):
                state, expected = self.state()
                if mutation == 0:
                    state["comments"].pop()
                elif mutation == 1:
                    state["comments"].append(deepcopy(state["comments"][0]))
                elif mutation == 2:
                    state["comments"][0]["issue_id"] = "bd-other"
                else:
                    state["comments"][0]["created_at"] = BASE_TIME
                with self.assertRaisesRegex(AssertionError, "payloads"):
                    assert_import_state(state, expected, {"bd-control"}, True)

    def test_fixture_orders_and_no_growth_control(self):
        state, _ = self.state()
        issue = record(ISSUE_ID, "fixture")
        issue["comments"] = state["comments"][:1]
        for kind in ("incoming-first", "incoming-last", "complete"):
            before = deepcopy(issue)
            incoming, expected = make_incoming(issue, kind)
            self.assertEqual(issue, before)
            self.assertEqual(len(expected), 1 if kind == "complete" else 2)
            self.assertLess(timestamp(incoming["updated_at"]), timestamp(LOCAL_TIME))

    def test_nanosecond_comment_identity_is_not_rounded_away(self):
        state, expected = self.state()
        state["comments"][0]["created_at"] = "2026-01-02T00:00:00.000000001Z"
        with self.assertRaisesRegex(AssertionError, "payloads"):
            assert_import_state(state, expected, {"bd-control"}, True)
        self.assertEqual(precise_timestamp("2026-01-02T01:00:00.123456789+01:00"),
                         precise_timestamp("2026-01-02T00:00:00.123456789Z"))

    def test_inspection_reads_committed_wal_without_touching_source(self):
        root = Path(tempfile.mkdtemp(prefix="br-547-oracle-")).resolve()
        db = root / "beads.db"
        with closing(sqlite3.connect(db)) as conn:
            conn.executescript("""
                PRAGMA journal_mode = WAL;
                PRAGMA wal_autocheckpoint = 0;
                CREATE TABLE issues (id TEXT, updated_at TEXT);
                CREATE TABLE comments (id INTEGER, issue_id TEXT, author TEXT,
                                       text TEXT, created_at TEXT);
                CREATE TABLE export_hashes (issue_id TEXT);
                CREATE TABLE dirty_issues (issue_id TEXT);
                CREATE TABLE metadata (key TEXT, value TEXT);
            """)
            conn.execute("INSERT INTO issues VALUES (?, ?)", (ISSUE_ID, LOCAL_TIME))
            conn.execute("INSERT INTO metadata VALUES ('needs_flush', 'false')")
            conn.execute("INSERT INTO metadata VALUES ('needs_flush', 'true')")
            conn.commit()
            self.assertEqual(conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(),
                             (0, 0, 0))
            conn.execute("INSERT INTO comments VALUES (73, ?, 'local', 'WAL-only', ?)",
                         (ISSUE_ID, BASE_TIME))
            conn.commit()
            self.assertGreater(Path(str(db) + "-wal").stat().st_size, 32)
            before = fingerprint(db)
            state = inspect_copy(db, root / "inspection")
            self.assertEqual(state["comments"][0]["text"], "WAL-only")
            self.assertEqual(state["needs_flush"], "true")
            self.assertEqual(fingerprint(db), before)

    def test_sqlite_lookup_failure_mechanism(self):
        # Native SQLite evidence for why a cached rowid is not a payload proof.
        # This does not execute br or FrankenSQLite.
        with sqlite3.connect(":memory:") as conn:
            conn.execute("CREATE TABLE comments (id INTEGER PRIMARY KEY, body TEXT)")
            conn.execute("INSERT INTO comments VALUES (73, 'local')")
            cached = conn.execute("SELECT id FROM comments").fetchone()[0]
            conn.execute("DELETE FROM comments")
            conn.executemany("INSERT INTO comments VALUES (?, ?)", [(73, "peer"), (74, "local")])
            self.assertEqual(conn.execute("SELECT body FROM comments WHERE id = ?", (cached,))
                             .fetchall(), [("peer",)])
            self.assertEqual(conn.execute("SELECT body FROM comments ORDER BY id").fetchall(),
                             [("peer",), ("local",)])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--br", type=Path, help="built br executable; never a tracker directory")
    parser.add_argument("--timeout-seconds", type=float, default=120)
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(OracleTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if not 0 < args.timeout_seconds <= 600:
        parser.error("--timeout-seconds must be in (0, 600]")
    binary = args.br.expanduser().resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--br must name an executable file")
    root = Path(tempfile.mkdtemp(prefix="br-import-547-")).resolve()
    report = {"schema": "br.import-comment-certificate-547.v1", "binary": str(binary),
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "artifact_root": str(root), "br_cases": []}
    print(f"Retaining disposable fixtures and logs at {root}", flush=True)
    for name, kind, controls in [("incoming-first", "incoming-first", 1),
                                  ("incoming-last", "incoming-last", 1),
                                  ("complete-no-growth", "complete", 1),
                                  ("after-full-skip-batch", "incoming-first", 901)]:
        case_root = root / name
        case_root.mkdir()
        try:
            run_case(binary, case_root, kind, controls, args.timeout_seconds)
            result = {"name": name, "passed": True}
        except Exception as error:
            result = {"name": name, "passed": False,
                      "error": f"{type(error).__name__}: {error}"}
        report["br_cases"].append(result)
        dump(root / "report.json", report)
        print(json.dumps(result), flush=True)
    return 0 if all(case["passed"] for case in report["br_cases"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
