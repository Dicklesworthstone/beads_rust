#!/usr/bin/env python3
"""Reproduce import/status/reopen/detailed-blocker workloads for GitHub #574.

Run matched, prebuilt binaries; compilation is deliberately outside measurement:

    python3 scripts/bench_n_plus_one.py --baseline /path/to/before/br \
        --candidate /path/to/after/br --output /new/path/574-evidence

For SQL counts, also supply --baseline-trace and --candidate-trace built with
the reporter's BR_SQL_TRACE probe. Upstream release binaries do not implement
that probe. Zero trace records are an error, never a zero-statement result.
Counts follow the report's convention: direct facade calls plus prepares,
not engine VM executions or prepared-statement executions. Instrumented runs
are separate from the uninstrumented timing samples. --enforce-hot-paths checks
only detailed-blocker and localized status/reopen optimizations, not all of #574.
Reopen setup closes one blocker and warms the cache outside the measured call;
the measured command must really reopen it, not take an already-open skip path.

All fixtures, copies, raw stdout/stderr, and receipts are retained in a NEW
output directory. No user workspace is accepted, modified or deleted. Timings
are descriptive same-host samples, not a statistical confidence claim.

    python3 scripts/bench_n_plus_one.py --self-test

Self-tests validate the generator and measurement checks, not beads_rust.
"""

from __future__ import annotations

import argparse
from collections import Counter
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
import unittest
from unittest import mock


STAMP = "2026-10-01T00:00:00Z"
CASES = ("import_one_changed", "status_change", "reopen_one", "blocked_detailed")


def fixture(blocked_count: int) -> list[dict]:
    if blocked_count < 1:
        raise ValueError("blocked_count must be positive")

    def issue(issue_id: str, title: str, targets: list[str]) -> dict:
        return {
            "id": issue_id, "title": title, "description": "seeded",
            "status": "open", "priority": 2, "issue_type": "task",
            "created_at": STAMP, "created_by": "seed", "updated_at": STAMP,
            "source_repo": ".", "compaction_level": 0, "original_size": 0,
            "labels": ["seed"], "comments": [],
            "dependencies": [
                {"issue_id": issue_id, "depends_on_id": target, "type": "blocks",
                 "created_at": STAMP, "created_by": "seed", "metadata": "{}",
                 "thread_id": ""}
                for target in targets
            ],
        }

    rows = [issue(f"nb-k{i:05d}", f"blocker {i}", [])
            for i in range(2 * blocked_count)]
    rows.extend(issue(f"nb-b{i:05d}", f"blocked {i}",
                      [f"nb-k{2*i:05d}", f"nb-k{2*i+1:05d}"])
                for i in range(blocked_count))
    return sorted(rows, key=lambda row: row["id"])


def write_jsonl(path: Path, rows: list[dict]) -> None:
    path.write_text("".join(json.dumps(row, separators=(",", ":")) + "\n"
                            for row in rows), encoding="utf-8")


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()]


def semantic_rows(rows: list[dict]) -> dict:
    result = {}
    for row in rows:
        issue_id = row["id"]
        if issue_id in result:
            raise AssertionError(f"duplicate issue: {issue_id}")
        result[issue_id] = {
            "title": row["title"], "status": row["status"],
            "description": row.get("description"), "priority": row["priority"],
            "issue_type": row["issue_type"],
            "closed_at": row.get("closed_at"),
            "close_reason": row.get("close_reason"),
            "closed_by_session": row.get("closed_by_session"),
            "defer_until": row.get("defer_until"),
            "deleted_at": row.get("deleted_at"),
            "deleted_by": row.get("deleted_by"),
            "delete_reason": row.get("delete_reason"),
            "labels": sorted(row.get("labels", [])),
            "dependencies": sorted((dep["depends_on_id"], dep["type"])
                                   for dep in row.get("dependencies", [])),
            "comments": sorted((c["id"], c["author"], c["text"])
                               for c in row.get("comments", [])),
        }
    return result


def trace_counts(stderr: str) -> dict:
    statements = [" ".join(line.split("\t", 1)[1].split())
                  for line in stderr.splitlines() if line.startswith("BR_SQL\t")]
    if not statements or any(not sql for sql in statements):
        raise ValueError("missing/empty BR_SQL records; an instrumented binary is required")
    upper = [sql.upper() for sql in statements]
    # Preparing a cache rewrite must not evade the operation-specific gates.
    # Total/prepares still follow the reporter's facade-call convention.
    operations = [sql.removeprefix("PREPARE ") for sql in upper]
    return {
        "convention": "direct_facade_calls_plus_prepares_as_in_issue_574",
        "total": len(statements),
        "prepares": sum(sql.startswith("PREPARE ") for sql in upper),
        "full_cache_deletes": sum(bool(re.fullmatch(
            r"DELETE FROM BLOCKED_ISSUES_CACHE\s*;?", sql)) for sql in operations),
        "cache_inserts": sum(bool(re.match(
            r"INSERT(?: OR \w+)? INTO BLOCKED_ISSUES_CACHE\b", sql)) for sql in operations),
        "export_hash_inserts": sum(bool(re.match(
            r"INSERT(?: OR \w+)? INTO EXPORT_HASHES\b", sql)) for sql in operations),
        "frequencies": dict(Counter(statements).most_common()),
    }


def check_hot_paths(counts: dict, plain: dict, blocked_count: int) -> None:
    # get_issues_by_ids uses chunks of at most 900 IDs; two distinct blockers
    # per displayed issue. Count against the same binary's non-detailed path.
    extra = counts["blocked_detailed"]["total"] - plain["total"]
    if extra < 0 or extra > math.ceil(2 * blocked_count / 900):
        raise AssertionError(f"detailed blocker SQL is not chunk-bounded: extra={extra}")
    for case in ("status_change", "reopen_one"):
        status = counts[case]
        if status["full_cache_deletes"] != 0 or status["cache_inserts"] > 1:
            raise AssertionError(f"{case} rewrites unrelated cache rows: {status}")


def binary_info(path: Path) -> dict:
    path = path.expanduser().resolve(strict=True)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"not an executable regular file: {path}")
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"path": str(path), "sha256": digest}


class Runner:
    def __init__(self, binary: Path, root: Path, timeout: int):
        self.binary = binary
        self.root = root
        self.timeout = timeout
        self.counter = 0

    def run(self, workspace: Path, args: list[str], *, trace: bool = False,
            warn: bool = False) -> tuple[str, str, float]:
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("BR_", "BEADS_"))}
        env.pop("TOON_DEFAULT_FORMAT", None)
        env.update(NO_COLOR="1", RUST_LOG="warn" if warn else "error",
                   HOME=str(workspace / ".home"),
                   XDG_CONFIG_HOME=str(workspace / ".home" / ".config"),
                   BEADS_DIR=str(workspace / ".beads"))
        if trace:
            env["BR_SQL_TRACE"] = "1"
        command = [str(self.binary), "--no-auto-import", "--no-auto-flush",
                   "--db", str(workspace / ".beads" / "beads.db"), *args]
        self.counter += 1
        stem = self.root / f"command-{self.counter:05d}"
        start = time.perf_counter()
        try:
            completed = subprocess.run(command, cwd=workspace, env=env,
                                       capture_output=True, timeout=self.timeout, check=False)
        except subprocess.TimeoutExpired as error:
            stem.with_suffix(".stdout").write_bytes(error.stdout or b"")
            stem.with_suffix(".stderr").write_bytes(error.stderr or b"")
            raise RuntimeError(f"timed out after {self.timeout}s: {command}") from error
        elapsed_ms = (time.perf_counter() - start) * 1000
        stem.with_suffix(".stdout").write_bytes(completed.stdout)
        stem.with_suffix(".stderr").write_bytes(completed.stderr)
        stem.with_suffix(".json").write_text(json.dumps({
            "command": command, "workspace": str(workspace), "trace": trace,
            "warn": warn, "elapsed_ms": elapsed_ms, "exit_code": completed.returncode,
        }, indent=2) + "\n", encoding="utf-8")
        if completed.returncode:
            raise RuntimeError(f"command failed ({completed.returncode}); evidence: {stem}")
        return (completed.stdout.decode("utf-8"), completed.stderr.decode("utf-8"), elapsed_ms)


def prepare(runner: Runner, workspace: Path, rows: list[dict]) -> list[dict]:
    workspace.mkdir()
    (workspace / ".home").mkdir()
    runner.run(workspace, ["init", "--prefix", "nb", "--json"])
    path = workspace / ".beads" / "issues.jsonl"
    write_jsonl(path, rows)
    runner.run(workspace, ["sync", "--import-only", "--json"])
    runner.run(workspace, ["sync", "--flush-only", "--json"])
    exported = read_jsonl(path)
    if semantic_rows(exported) != semantic_rows(rows):
        raise AssertionError("fixture import/export changed semantic data")
    # Establish a fresh cache before copying the fixture, outside measurement.
    runner.run(workspace, ["blocked", "--json", "--limit", "0"])
    return exported


def scenario(runner: Runner, template: Path, workspace: Path, case: str,
             exported: list[dict]) -> tuple[list[str], list[dict]]:
    shutil.copytree(template, workspace)
    expected = copy.deepcopy(exported)
    if case == "import_one_changed":
        expected[0]["title"] = "changed upstream"
        expected[0]["updated_at"] = "2030-01-01T00:00:00Z"
        write_jsonl(workspace / ".beads" / "issues.jsonl", expected)
        return ["sync", "--import-only", "--json"], expected
    if case == "status_change":
        target = next(row for row in expected if row["id"] == "nb-k00000")
        target["status"] = "in_progress"
        return ["update", "nb-k00000", "--status", "in_progress", "--json"], expected
    if case == "reopen_one":
        # These calls are fixture setup, never the timing or SQL sample.
        runner.run(workspace, ["close", "nb-k00000", "--reason",
                               "reopen benchmark setup", "--json"])
        runner.run(workspace, ["sync", "--flush-only", "--json"])
        seeded = read_jsonl(workspace / ".beads" / "issues.jsonl")
        closed = next((row for row in seeded if row["id"] == "nb-k00000"), None)
        if closed is None or closed["status"] != "closed":
            raise AssertionError("reopen setup did not close the target")
        runner.run(workspace, ["blocked", "--json", "--limit", "0"])
        return ["reopen", "nb-k00000", "--json"], expected
    if case == "blocked_detailed":
        return ["blocked", "--detailed", "--limit", "0"], expected
    raise ValueError(case)


def verify_reopen_receipt(stdout: str) -> None:
    result = json.loads(stdout)
    if not isinstance(result, dict) or result.get("skipped"):
        raise AssertionError("reopen workload was skipped or returned an invalid receipt")
    reopened = result.get("reopened")
    if not isinstance(reopened, list) or len(reopened) != 1:
        raise AssertionError("reopen workload did not reopen exactly one issue")
    issue = reopened[0]
    if (not isinstance(issue, dict) or issue.get("id") != "nb-k00000"
            or issue.get("status") != "open" or issue.get("closed_at") is not None):
        raise AssertionError("reopen receipt has the wrong target or terminal metadata")


def verify(runner: Runner, workspace: Path, expected: list[dict], blocked_count: int) -> None:
    runner.run(workspace, ["sync", "--flush-only", "--json"])
    actual = read_jsonl(workspace / ".beads" / "issues.jsonl")
    if semantic_rows(actual) != semantic_rows(expected):
        raise AssertionError(f"post-workload issue/relation mismatch: {workspace}")
    stdout, _, _ = runner.run(workspace, ["blocked", "--json", "--limit", "0"])
    page = json.loads(stdout)
    expected_blockers = {
        f"nb-b{i:05d}": {f"nb-k{2*i:05d}", f"nb-k{2*i+1:05d}"}
        for i in range(blocked_count)
    }
    issues = page["issues"]
    if len(issues) != blocked_count or page["total"] != blocked_count or page["has_more"]:
        raise AssertionError("blocked page lost or duplicated issues")
    actual_blockers = {row["id"]: set(row["blocked_by"]) for row in issues}
    if actual_blockers != expected_blockers or any(row["blocked_by_count"] != 2 for row in issues):
        raise AssertionError("dependency semantics changed")


def run_benchmarks(args: argparse.Namespace) -> dict:
    binaries = {side: binary_info(getattr(args, side)) for side in ("baseline", "candidate")}
    traces = {side: binary_info(path) for side in binaries
              if (path := getattr(args, side + "_trace")) is not None}
    if bool(traces) and set(traces) != set(binaries):
        raise ValueError("supply both trace binaries, or neither")
    if args.enforce_hot_paths and not traces:
        raise ValueError("--enforce-hot-paths requires both instrumented binaries")
    if any(traces[side]["sha256"] == binaries[side]["sha256"] for side in traces):
        raise ValueError("timing and trace binaries must be separate builds")
    root = args.output.expanduser().resolve()
    root.mkdir(parents=True, exist_ok=False)
    report = {"schema": "br.n-plus-one.574.v1", "host": platform.platform(),
              "python": sys.version, "binaries": binaries, "trace_binaries": traces,
              "timing_statistic": "raw samples, minimum and median; descriptive only",
              "sampling_order": "baseline then candidate; not randomized or ABBA",
              "trace_source_match": "operator_supplied_not_verified" if traces else "not_supplied",
              "sql_gate": "pending" if args.enforce_hot_paths else "not_requested",
              "state": "running", "workloads": []}
    try:
        for count in args.blocked_counts:
            rows = fixture(count)
            for side, binary in binaries.items():
                side_root = root / f"{count}-{side}"
                side_root.mkdir()
                runner = Runner(Path(binary["path"]), side_root, args.timeout)
                template = side_root / "template"
                exported = prepare(runner, template, rows)
                version, _, _ = runner.run(template, ["--version"])
                binary["version_stdout"] = version.strip()
                source_digest = hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()
                counts = {}
                for case in CASES:
                    samples = []
                    for sample in range(args.repeats):
                        ws = side_root / f"{case}-{sample}"
                        command, expected = scenario(runner, template, ws, case, exported)
                        stdout, _, elapsed = runner.run(ws, command)
                        if case == "blocked_detailed":
                            if "(not found)" in stdout or any(
                                f"nb-k{i:05d}: blocker {i} [P2]" not in stdout
                                for i in range(2 * count)
                            ):
                                raise AssertionError("detailed output lost blocker titles")
                        elif case == "reopen_one":
                            verify_reopen_receipt(stdout)
                        verify(runner, ws, expected, count)
                        samples.append(elapsed)
                    receipt = {"side": side, "blocked_count": count, "issue_count": 3 * count,
                               "fixture_sha256": source_digest, "case": case,
                               "samples_ms": samples, "min_ms": min(samples),
                               "median_ms": statistics.median(samples), "sql": None}
                    if traces:
                        trace_runner = Runner(Path(traces[side]["path"]), side_root, args.timeout)
                        # Separate log namespace and fresh fixture for the profiling build.
                        trace_root = side_root / f"{case}-trace"
                        trace_root.mkdir()
                        trace_runner.root = trace_root
                        trace_template = trace_root / "template"
                        trace_exported = prepare(trace_runner, trace_template, rows)
                        version, _, _ = trace_runner.run(trace_template, ["--version"])
                        traces[side]["version_stdout"] = version.strip()
                        ws = trace_root / "measured"
                        command, expected = scenario(
                            trace_runner, trace_template, ws, case, trace_exported)
                        stdout, stderr, _ = trace_runner.run(ws, command, trace=True)
                        if case == "reopen_one":
                            verify_reopen_receipt(stdout)
                        counts[case] = trace_counts(stderr)
                        receipt["sql"] = counts[case]
                        verify(trace_runner, ws, expected, count)
                        if case == "blocked_detailed":
                            plain_ws = trace_root / "plain"
                            _, plain_expected = scenario(
                                trace_runner, trace_template, plain_ws, case, trace_exported)
                            _, stderr, _ = trace_runner.run(
                                plain_ws, ["blocked", "--limit", "0"], trace=True)
                            plain = trace_counts(stderr)
                            receipt["plain_sql"] = plain
                            verify(trace_runner, plain_ws, plain_expected, count)
                    if case == "import_one_changed":
                        ws = side_root / "import-warnings"
                        command, expected = scenario(runner, template, ws, case, exported)
                        _, stderr, _ = runner.run(ws, command, warn=True)
                        receipt["import_skip_warnings"] = stderr.count("Import skip:")
                        verify(runner, ws, expected, count)
                    report["workloads"].append(receipt)
                if args.enforce_hot_paths and side == "candidate":
                    check_hot_paths(counts, plain, count)
        report["state"] = "measurements_completed"
        if args.enforce_hot_paths:
            report["sql_gate"] = "passed"
    except Exception as error:
        report["state"] = "failed"
        report["error"] = f"{type(error).__name__}: {error}"
        if args.enforce_hot_paths:
            report["sql_gate"] = "not_established"
        raise
    finally:
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


class HarnessTests(unittest.TestCase):
    def test_fixture_matches_reported_shape(self):
        rows = fixture(500)
        self.assertEqual(len(rows), 1500)
        self.assertEqual(sum(len(r["dependencies"]) for r in rows), 1000)
        self.assertEqual(len(semantic_rows(rows)), 1500)
        self.assertEqual([r["id"] for r in rows], sorted(r["id"] for r in rows))
        self.assertTrue(all(r["labels"] == ["seed"] for r in rows))

    def test_semantic_check_detects_data_loss(self):
        original = fixture(2)
        for field, value in (("title", "wrong"), ("status", "closed"),
                             ("labels", []), ("dependencies", []),
                             ("closed_at", STAMP), ("close_reason", "stale"),
                             ("closed_by_session", "stale"), ("defer_until", STAMP),
                             ("deleted_at", STAMP), ("deleted_by", "stale"),
                             ("delete_reason", "stale")):
            changed = copy.deepcopy(original)
            changed[0][field] = value
            self.assertNotEqual(semantic_rows(original), semantic_rows(changed))
        with self.assertRaises(AssertionError):
            semantic_rows([*original, original[0]])

    def test_trace_counts_reporter_convention(self):
        result = trace_counts("noise\nBR_SQL\tDELETE FROM blocked_issues_cache\n"
                              "BR_SQL\tINSERT INTO blocked_issues_cache VALUES (?)\n"
                              "BR_SQL\tPREPARE SELECT 1\n")
        self.assertEqual(result["total"], 3)
        self.assertEqual(result["prepares"], 1)
        self.assertEqual(result["full_cache_deletes"], 1)
        self.assertEqual(result["cache_inserts"], 1)

    def test_absent_or_empty_trace_is_not_zero(self):
        for text in ("", "no trace", "BR_SQL\t\n"):
            with self.assertRaises(ValueError):
                trace_counts(text)

    def test_sql_gate_rejects_planted_n_plus_one_and_cache_rebuild(self):
        good = {"blocked_detailed": {"total": 18},
                "status_change": {"full_cache_deletes": 0, "cache_inserts": 1},
                "reopen_one": {"full_cache_deletes": 0, "cache_inserts": 1}}
        check_hot_paths(good, {"total": 16}, 500)
        for case, field, value in (("blocked_detailed", "total", 1016),
                                   ("status_change", "full_cache_deletes", 1),
                                   ("status_change", "cache_inserts", 500),
                                   ("reopen_one", "full_cache_deletes", 1),
                                   ("reopen_one", "cache_inserts", 500)):
            bad = copy.deepcopy(good)
            bad[case][field] = value
            with self.assertRaises(AssertionError):
                check_hot_paths(bad, {"total": 16}, 500)

    def test_prepared_cache_rewrites_are_classified_without_changing_total(self):
        result = trace_counts("BR_SQL\tPREPARE DELETE FROM blocked_issues_cache;\n"
                              "BR_SQL\tPREPARE INSERT INTO blocked_issues_cache VALUES (?)\n"
                              "BR_SQL\tPREPARE INSERT OR REPLACE INTO export_hashes VALUES (?)\n")
        self.assertEqual(result["total"], 3)
        self.assertEqual(result["prepares"], 3)
        self.assertEqual(result["full_cache_deletes"], 1)
        self.assertEqual(result["cache_inserts"], 1)
        self.assertEqual(result["export_hash_inserts"], 1)

    def test_reopen_setup_is_outside_the_measured_command(self):
        rows = fixture(2)
        closed = copy.deepcopy(rows)
        next(row for row in closed if row["id"] == "nb-k00000")["status"] = "closed"
        runner = mock.Mock(spec=Runner)
        workspace = Path("unwritten-measured-workspace")
        with mock.patch.object(shutil, "copytree") as copytree, \
                mock.patch(f"{__name__}.read_jsonl", return_value=closed):
            command, expected = scenario(runner, Path("template"), workspace, "reopen_one", rows)
        copytree.assert_called_once_with(Path("template"), workspace)
        self.assertEqual(command, ["reopen", "nb-k00000", "--json"])
        self.assertEqual(expected, rows)
        self.assertEqual([call.args[1][0] for call in runner.run.call_args_list],
                         ["close", "sync", "blocked"])
        self.assertTrue(all(not call.kwargs.get("trace") for call in runner.run.call_args_list))
        self.assertEqual(next(row for row in rows if row["id"] == "nb-k00000")["status"], "open")

    def test_reopen_setup_rejects_an_already_open_target(self):
        with mock.patch.object(shutil, "copytree"), \
                mock.patch(f"{__name__}.read_jsonl", return_value=fixture(2)):
            with self.assertRaisesRegex(AssertionError, "did not close"):
                scenario(mock.Mock(spec=Runner), Path("template"), Path("unused"),
                         "reopen_one", fixture(2))

    def test_reopen_receipt_rejects_skips_wrong_ids_and_stale_close_fields(self):
        good = {"reopened": [{"id": "nb-k00000", "status": "open"}]}
        verify_reopen_receipt(json.dumps(good))
        bad_receipts = [[], {}, {"reopened": []},
                        {**good, "skipped": [{"id": "nb-k00000"}]}]
        for field, value in (("id", "wrong"), ("status", "closed"), ("closed_at", STAMP)):
            bad = copy.deepcopy(good)
            bad["reopened"][0][field] = value
            bad_receipts.append(bad)
        for bad in bad_receipts:
            with self.assertRaises(AssertionError):
                verify_reopen_receipt(json.dumps(bad))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    for side in ("baseline", "candidate"):
        parser.add_argument("--" + side, type=Path)
        parser.add_argument("--" + side + "-trace", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--blocked-counts", type=int, nargs="+", default=[50, 500, 2000])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--enforce-hot-paths", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(HarnessTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    if any(getattr(args, name) is None for name in ("baseline", "candidate", "output")):
        parser.error("--baseline, --candidate and --output are required")
    if min([args.repeats, args.timeout, *args.blocked_counts]) < 1:
        parser.error("counts, repeats and timeout must be positive")
    if len(set(args.blocked_counts)) != len(args.blocked_counts):
        parser.error("blocked counts must be distinct")
    try:
        report = run_benchmarks(args)
    except (OSError, ValueError, RuntimeError, AssertionError) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1
    for receipt in report["workloads"]:
        print(f"{receipt['side']} {receipt['issue_count']} {receipt['case']}: "
              f"min={receipt['min_ms']:.3f} ms median={receipt['median_ms']:.3f} ms "
              f"sql={receipt['sql']['total'] if receipt['sql'] else 'not measured'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
