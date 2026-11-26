#!/usr/bin/env python3
r"""Qualify #619 with paired real CLI label writes and retained correctness evidence.

Python standard library and a C11 compiler; Linux and macOS. Supply two absolute executable
paths and source identifiers. Each side initializes its own store through its
real CLI, so release/current schemas are never mixed. Both sides are measured
in the same invocation, alternating AB/BA order. Setup and verification are
outside the measured intervals. Every command and its output is retained.

Example (the source IDs and comparison kind are operator declarations):
  python3 scripts/bench_bounded_write.py \
    --baseline /abs/br-old-opener --baseline-source SHA-plus-opener-control \
    --candidate /abs/br --candidate-source SHA \
    --comparison-kind same-tree-opener --output /abs/new-results

Defaults match #619's 1k/4k/16k stores with 10,000-byte descriptions. Add
--writers 4,16 for same-target writer waves at the largest size. To measure a
separate WAL axis, use a NEW output directory and --retain-wal together with
--wal-growth-updates N. That holds a cooperative shared opener lease, so real
growth writes and samples retain their committed WAL. Growth is not timed.

The default serial lane requires an empty WAL before and after every sample.
Samples use --no-auto-import --no-auto-flush. Results therefore do not qualify
default synchronization, cold filesystem caches, or writer-lock hold duration.
A small C supervisor supplies each child's own wait4 CPU/RSS. It is compiled
once before setup, avoiding Python-parent RSS inheritance across fork/exec.
There is no timing threshold or automatic release-gate verdict. A release
comparison includes all intervening changes, not only the opener change.

No fixture cleanup is performed. Use a new --output directory on every run.
"""

import argparse
from collections import Counter, defaultdict
import concurrent.futures
import datetime
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import sqlite3
import statistics
import struct
import subprocess
import sys
import threading
import time
import traceback


ACTOR = "bounded-write-619"
SCHEMA = "br.bounded-write-benchmark.v1"
NO_SYNC = ["--no-auto-import", "--no-auto-flush"]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def file_record(path):
    path = Path(path)
    return {"bytes": path.stat().st_size, "sha256": sha256(path)} if path.is_file() else None


def wal_state(workspace, tolerant=False):
    path = Path(workspace) / ".beads" / "beads.db-wal"
    if not path.exists():
        return {"exists": False, "bytes": 0, "frames": 0}
    try:
        with path.open("rb") as stream:
            header = stream.read(32)
            size = os.fstat(stream.fileno()).st_size
    except OSError as error:
        if tolerant:
            return {"unstable": True, "observation_error": str(error)}
        raise
    result = {"exists": True, "bytes": size, "frames": 0}
    if size:
        if tolerant and (len(header) != 32 or size < 32):
            return {**result, "unstable": True, "observation_error": "header changed during concurrent observation"}
        require(len(header) == 32, f"truncated WAL header: {path}")
        page_size = struct.unpack_from(">I", header, 8)[0]
        if page_size == 1:
            page_size = 65536
        if tolerant and not (page_size >= 512 and page_size & (page_size - 1) == 0):
            return {**result, "unstable": True, "observation_error": "page size changed during concurrent observation"}
        require(page_size >= 512 and page_size & (page_size - 1) == 0,
                f"invalid WAL page size: {page_size}")
        frames, tail = divmod(size - 32, page_size + 24)
        result.update(page_size=page_size, frames=frames, trailing_bytes=tail,
                      salts=header[16:24].hex())
        # Concurrent sampling may observe an append in progress. Quiescent
        # boundaries check this field separately; frames are physical, not a
        # claim about the number of committed/replayed frames.
    return result


def clean_environment(workspace):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("BR_", "BEADS_", "BD_", "FSQLITE_", "TOON_"))
           and key not in ("LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "RUST_LOG")}
    env.update(BEADS_DIR=str(Path(workspace) / ".beads"), NO_COLOR="1", RUST_LOG="error")
    return env


def host_state():
    available = None
    path = Path("/proc/meminfo")
    if path.is_file():
        for line in path.read_text().splitlines():
            if line.startswith("MemAvailable:"):
                available = int(line.split()[1]) * 1024
                break
    return {"load_average": os.getloadavg(), "mem_available_bytes": available}


def json_digest(value):
    def encode(item):
        if isinstance(item, bytes):
            return {"blob_hex": item.hex()}
        raise TypeError(f"unsupported SQL value: {type(item).__name__}")
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     default=encode).encode()).hexdigest()


def row_digest(rows):
    digest, count = hashlib.sha256(), 0
    for row in rows:
        digest.update(bytes.fromhex(json_digest(dict(row))))
        count += 1
    return {"rows": count, "sha256": digest.hexdigest()}


class Campaign:
    def __init__(self, args, output):
        self.args, self.output = args, output
        self.commands = output / "commands"
        self.commands.mkdir()
        self.rows, self.waves = [], []
        self.serial = 0
        self.mutex = threading.Lock()
        self.helper = None

    def build_helper(self):
        compiler = shutil.which(self.args.cc)
        require(compiler is not None, f"C compiler not found: {self.args.cc}")
        source = Path(__file__).resolve().with_name("bench_bounded_write_usage.c")
        copied = self.output / source.name
        shutil.copyfile(source, copied)
        self.helper = self.output / "bounded-write-usage"
        command = [compiler, "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", str(copied), "-o", str(self.helper)]
        with (self.output / "helper-build.stdout").open("xb") as stdout, \
                (self.output / "helper-build.stderr").open("xb") as stderr:
            built = subprocess.run(command, stdout=stdout, stderr=stderr, check=False, timeout=60)
        require(built.returncode == 0, "usage supervisor failed to compile; see helper-build.stderr")
        version = subprocess.run([compiler, "--version"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 check=True, timeout=30).stdout.decode(errors="replace")
        (self.output / "compiler-version.txt").write_text(version)
        return {"compiler": compiler, "compiler_version": version, "command": command,
                "source": {"path": str(copied), **file_record(copied)},
                "executable": {"path": str(self.helper), **file_record(self.helper)}}

    def run(self, binary, workspace, arguments, **context):
        with self.mutex:
            self.serial += 1
            serial = self.serial
        prefix = self.commands / f"{serial:06d}"
        command = [str(binary), *arguments]
        require(self.helper is not None, "usage supervisor is not initialized")
        usage_path = prefix.with_suffix(".usage.json")
        supervised_command = [str(self.helper), str(usage_path), *command]
        concurrent_sample = context.get("kind") == "concurrent-sample"
        before_wal = wal_state(workspace, tolerant=concurrent_sample)
        before_host = host_state()
        outer_start = time.monotonic_ns()
        timed_out = False
        with prefix.with_suffix(".stdout").open("xb") as stdout, \
                prefix.with_suffix(".stderr").open("xb") as stderr:
            child = subprocess.Popen(supervised_command, cwd=workspace,
                                     env=clean_environment(workspace), stdin=subprocess.DEVNULL,
                                     stdout=stdout, stderr=stderr, start_new_session=True)
            try:
                child.wait(timeout=self.args.timeout_seconds)
            except subprocess.TimeoutExpired:
                timed_out = True
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
            except BaseException:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    child.wait()
                raise
        outer_stop = time.monotonic_ns()
        receipt_error = None
        try:
            usage = json.loads(usage_path.read_text())
            require(usage.get("schema") == "br.bounded-write-usage.v1", "unexpected usage receipt schema")
            for field in ("start_ns", "stop_ns", "wall_seconds", "cpu_seconds", "peak_rss_bytes", "exit_code"):
                require(field in usage, f"usage receipt missing {field}")
        except (OSError, ValueError, AttributeError) as error:
            usage, receipt_error = {}, str(error)
        row = {"serial": serial, "command": command, "workspace": str(workspace),
               "supervised_command": supervised_command, "prefix": str(prefix),
               "outer_wall_seconds": (outer_stop - outer_start) / 1e9,
               "supervisor_exit_code": child.returncode, "timed_out": timed_out, **usage,
               "usage_receipt_error": receipt_error,
               "host_before": before_host, "host_after": host_state(),
               "wal_before": before_wal, "wal_after": wal_state(workspace, tolerant=concurrent_sample),
               "stdout": file_record(prefix.with_suffix(".stdout")),
               "stderr": file_record(prefix.with_suffix(".stderr")), **context}
        write_json(prefix.with_suffix(".json"), row)
        with self.mutex:
            self.rows.append(row)
        require(receipt_error is None and not timed_out and child.returncode == 0 and usage.get("exit_code") == 0,
                f"command {serial} failed (supervisor exit={child.returncode}, timeout={timed_out}): "
                f"see {prefix}.stderr")
        return row

    def reserve(self, additional=0):
        free = shutil.disk_usage(self.output).free
        required = self.args.reserve_mib * 1024 * 1024 + additional
        require(free >= required, f"disk reserve: {free} free bytes, {required} required")


class Fixture:
    def __init__(self, campaign, side, binary, size):
        self.campaign, self.side, self.binary, self.size = campaign, side, binary, size
        self.root = campaign.output / f"n{size}" / side
        self.root.mkdir(parents=True)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.beads = self.workspace / ".beads"
        self.database = self.beads / "beads.db"
        self.expected_labels = defaultdict(set)
        self.expected_events = Counter()
        self.touched = set()
        self.lease = None
        self.initial = None
        self.initial_export = {}

    def run(self, arguments, **context):
        return self.campaign.run(self.binary, self.workspace, arguments,
                                 side=self.side, issues=self.size, **context)

    def flags(self):
        return [*NO_SYNC, "--actor", ACTOR, "--lock-timeout",
                str(self.campaign.args.lock_timeout_ms)]

    def expected_issue(self, number):
        return {"id": f"probe-{number:06x}", "title": f"t{number}",
                "description": "x" * self.campaign.args.description_bytes,
                "status": "closed" if number % 4 else "open",
                "priority": number % 4, "issue_type": "task"}

    def marker(self):
        path = Path(str(self.database) + ".fsqlite-migration-state")
        record = file_record(path)
        require(record is not None, f"engine migration marker missing after setup: {path}")
        marker = json.loads(path.read_text())
        require(isinstance(marker.get("last_upgrade_version"), int),
                f"invalid engine migration marker: {path}")
        return {"path": str(path), **record, "contents": marker}

    def prepare(self):
        self.run(["init", "--prefix", "probe"], kind="control", phase="init")
        corpus = self.root / "seed.jsonl"
        with corpus.open("x", encoding="utf-8") as stream:
            for number in range(self.size):
                issue = self.expected_issue(number)
                issue.update(created_at="2026-11-01T00:00:00Z", updated_at="2026-11-01T00:00:00Z")
                if issue["status"] == "closed":
                    issue["closed_at"] = "2026-11-01T00:00:00Z"
                stream.write(json.dumps(issue, separators=(",", ":")) + "\n")
        # init owns the empty JSONL file; keep that exact input before supplying
        # the reproducible import corpus. No existing user workspace is touched.
        shutil.copyfile(self.beads / "issues.jsonl", self.root / "init-issues.jsonl")
        shutil.copyfile(corpus, self.beads / "issues.jsonl")
        self.run(["sync", "--import-only", "--force", "--json"],
                 kind="control", phase="import")
        # The released importer leaves a bookkeeping frame behind even after
        # flush. Real sole-opener label writes drain it through the normal
        # checkpoint path. They also ensure the setup is a writable fixture;
        # their events are included in the preserved pre-measurement baseline.
        for operation in ("add", "remove"):
            self.write_label("probe-000000", operation, "bw619-setup",
                             kind="control", phase="settle-import-wal")
        self.flush_and_check("initial")
        self.initial = self.snapshot("initial")
        require(not self.initial["dirty_ids"], "seed must be clean after explicit export")
        require(not self.initial["labels"], "seed unexpectedly contains labels")
        self.initial_marker = self.marker()
        write_json(self.root / "fixture.json", {
            "issues": self.size, "description_bytes": self.campaign.args.description_bytes,
            "corpus": file_record(corpus), "migration_marker": self.initial_marker,
            "database": file_record(self.database), "wal": wal_state(self.workspace),
        })
        if self.campaign.args.retain_wal:
            locks = [p for p in self.beads.glob(".br-db-openers-*.lock")
                     if "transition" not in p.name]
            require(len(locks) == 1, f"expected one cooperative opener lock: {locks}")
            self.lease = locks[0].open("rb")
            fcntl.flock(self.lease.fileno(), fcntl.LOCK_SH)
        self.check_wal_boundary()

    def check_wal_boundary(self):
        state = wal_state(self.workspace)
        require(not state.get("trailing_bytes", 0), "quiescent WAL has a partial physical frame")
        if not self.campaign.args.retain_wal:
            require(state["frames"] == 0,
                    f"checkpointed lane has unexpected retained WAL: {self.workspace}: {state}")
        return state

    def account_write(self, issue_id, operation, label):
        labels = self.expected_labels[issue_id]
        if operation == "add":
            require(label not in labels, "benchmark attempted a no-op label add")
            labels.add(label)
            event = (issue_id, "label_added", None, None, f"Added label {label}", ACTOR)
        else:
            require(label in labels, "benchmark attempted a no-op label removal")
            labels.remove(label)
            event = (issue_id, "label_removed", None, None, f"Removed label {label}", ACTOR)
        self.expected_events[event] += 1
        self.touched.add(issue_id)

    def write_label(self, issue_id, operation, label, **context):
        return self.run([*self.flags(), "update", issue_id, f"--{operation}-label", label],
                        operation=operation, target=issue_id, label=label, **context)

    def check_target(self, issue_id, phase):
        result = self.run([*self.flags(), "show", issue_id, "--json"],
                          kind="control", phase=phase)
        issues = json.loads(Path(result["prefix"]).with_suffix(".stdout").read_text())
        require(isinstance(issues, list) and len(issues) == 1, "show must return exactly one issue")
        issue = issues[0]
        expected = self.expected_issue(int(issue_id.split("-")[1], 16))
        require(all(issue.get(key) == value for key, value in expected.items()),
                f"target payload changed: {issue_id}")
        require(sorted(issue.get("labels", [])) == sorted(self.expected_labels[issue_id]),
                f"wrong durable labels on {issue_id}: {issue.get('labels')}")

    def snapshot(self, name):
        """Observe a quiescent private family copy, never SQLite-open the live DB."""
        self.campaign.reserve(sum(p.stat().st_size for p in self.beads.glob("beads.db*")))
        snapshot = self.root / f"snapshot-{name}"
        snapshot.mkdir()
        for suffix in ("", "-wal", "-shm", "-journal"):
            source = Path(str(self.database) + suffix)
            if source.exists():
                shutil.copyfile(source, snapshot / ("beads.db" + suffix))
        connection = sqlite3.connect((snapshot / "beads.db").as_uri() + "?mode=ro", uri=True)
        connection.row_factory = sqlite3.Row
        try:
            connection.execute("PRAGMA query_only = ON")
            integrity = [row[0] for row in connection.execute("PRAGMA integrity_check")]
            require(integrity == ["ok"], f"independent SQLite integrity check: {integrity}")
            require(not list(connection.execute("PRAGMA foreign_key_check")), "foreign-key violation")
            issues = {}
            for row in connection.execute("SELECT * FROM issues ORDER BY id"):
                value = dict(row)
                issue_id = value["id"]
                number = int(issue_id.split("-")[1], 16)
                require(0 <= number < self.size and issue_id not in issues, "unexpected issue identity")
                require(all(value.get(key) == expected for key, expected in self.expected_issue(number).items()),
                        f"stored payload mismatch: {issue_id}")
                protected = {key: item for key, item in value.items()
                             if key not in ("updated_at", "content_hash")}
                issues[issue_id] = {"full": json_digest(value), "protected": json_digest(protected)}
            require(len(issues) == self.size, "missing or extra stored issues")
            labels = [list(row) for row in connection.execute(
                "SELECT issue_id, label FROM labels ORDER BY issue_id, label")]
            dirty = [row[0] for row in connection.execute("SELECT issue_id FROM dirty_issues ORDER BY issue_id")]
            max_event = connection.execute("SELECT COALESCE(MAX(id), 0) FROM events").fetchone()[0]
            initial_max = self.initial["max_event_id"] if self.initial else max_event
            events_before = row_digest(connection.execute("SELECT * FROM events WHERE id <= ? ORDER BY id",
                                                           (initial_max,)))
            new_events = [tuple(row) for row in connection.execute(
                "SELECT issue_id, event_type, old_value, new_value, comment, actor FROM events WHERE id > ? ORDER BY id",
                (initial_max,))]
            relations = {table: row_digest(connection.execute(f"SELECT * FROM {table} ORDER BY rowid"))
                         for table in ("comments", "dependencies")}
        finally:
            connection.close()
        result = {"issues": issues, "labels": labels, "dirty_ids": dirty, "max_event_id": max_event,
                  "prior_events": events_before, "new_events": new_events, "relations": relations,
                  "sqlite_version": sqlite3.sqlite_version, "integrity": integrity}
        write_json(self.root / f"verification-{name}.json", result)
        return result

    def verify_stored_changes(self):
        observed = self.snapshot("final-before-export")
        for issue_id, hashes in observed["issues"].items():
            required = "protected" if issue_id in self.touched else "full"
            require(hashes[required] == self.initial["issues"][issue_id][required],
                    f"unrelated issue fields changed: {issue_id}")
        expected_labels = sorted([issue_id, label] for issue_id, labels in self.expected_labels.items()
                                 for label in labels)
        require(observed["labels"] == expected_labels, "stored label set differs from successful writes")
        require(observed["dirty_ids"] == sorted(self.touched), "dirty-ID set is not the exact changed set")
        require(observed["prior_events"] == self.initial["prior_events"], "prior audit events changed")
        require(Counter(observed["new_events"]) == self.expected_events, "missing, extra, or wrong audit events")
        require(observed["relations"] == self.initial["relations"], "unrelated relationships changed")
        require(self.marker() == self.initial_marker, "engine migration marker changed during samples")
        self.flush_and_check("final")
        write_json(self.root / "correctness.json", {
            "ok": True, "issues": self.size, "touched_ids": sorted(self.touched),
            "expected_new_events": sum(self.expected_events.values()),
            "exact_dirty_ids": True, "exact_labels": True, "unrelated_rows_unchanged": True,
            "prior_events_unchanged": True, "export_payloads_correct": True,
        })

    def flush_and_check(self, name):
        self.run([*self.flags(), "sync", "--flush-only", "--json"], kind="control", phase=f"export-{name}")
        exported = self.root / f"export-{name}.jsonl"
        shutil.copyfile(self.beads / "issues.jsonl", exported)
        seen = set()
        hashes = {}
        with exported.open(encoding="utf-8") as stream:
            for line in stream:
                issue = json.loads(line)
                issue_id = issue["id"]
                number = int(issue_id.split("-")[1], 16)
                require(0 <= number < self.size and issue_id not in seen, "bad exported issue identity")
                require(all(issue.get(key) == value for key, value in self.expected_issue(number).items()),
                        f"export changed payload: {issue_id}")
                require(sorted(issue.get("labels", [])) == sorted(self.expected_labels[issue_id]),
                        f"export changed labels: {issue_id}")
                hashes[issue_id] = {
                    "full": json_digest(issue),
                    "protected": json_digest({key: value for key, value in issue.items()
                                              if key not in ("updated_at", "content_hash", "labels")}),
                }
                if name != "initial":
                    required = "protected" if issue_id in self.touched else "full"
                    require(hashes[issue_id][required] == self.initial_export[issue_id][required],
                            f"export changed unrelated fields: {issue_id}")
                seen.add(issue_id)
        require(len(seen) == self.size, "export lost issues")
        if name == "initial":
            self.initial_export = hashes
        write_json(self.root / f"export-{name}-receipt.json", {"file": file_record(exported), "issues": hashes})

    def release_lease(self):
        if self.lease is not None:
            fcntl.flock(self.lease.fileno(), fcntl.LOCK_UN)
            self.lease.close()
            self.lease = None


def statistics_for(values):
    values = sorted(values)
    return {"min": values[0], "median": statistics.median(values), "max": values[-1],
            "p95_nearest_rank": values[math.ceil(0.95 * len(values)) - 1]}


def summary(campaign, metadata):
    samples = [row for row in campaign.rows if row.get("kind") == "sample" and not row["warmup"]]
    metrics = ("wall_seconds", "cpu_seconds", "peak_rss_bytes")
    groups = defaultdict(list)
    for row in samples:
        groups[(row["side"], row["issues"], row["position"], row["operation"])].append(row)
    summaries = []
    for (side, size, position, operation), rows in sorted(groups.items()):
        summaries.append({"side": side, "issues": size, "position": position,
                          "operation": operation, "samples": len(rows),
                          **{field: statistics_for([row[field] for row in rows]) for field in metrics}})
    paired = defaultdict(dict)
    for row in samples:
        paired[(row["issues"], row["position"], row["operation"], row["repeat"])][row["side"]] = row
    ratios = []
    for key, pair in sorted(paired.items()):
        require(set(pair) == {"baseline", "candidate"}, "incomplete baseline/candidate pair")
        ratios.append({"issues": key[0], "position": key[1], "operation": key[2], "repeat": key[3],
                       **{field: pair["candidate"][field] / pair["baseline"][field]
                          if pair["baseline"][field] else None for field in metrics}})
    result = {"schema": SCHEMA, "ok": True, "comparison_kind": metadata["comparison_kind"],
              "qualification": "descriptive measurements; no performance gate or O(1) claim",
              "groups": summaries, "paired_candidate_over_baseline": ratios, "writer_waves": campaign.waves}
    write_json(campaign.output / "summary.json", result)
    lines = ["# Beads #619 bounded-write scaling", "",
             f"Comparison: **{metadata['comparison_kind']}**. WAL mode: **{metadata['wal_mode']}**.", "",
             "Fresh CLI children, warm filesystem, automatic import/export disabled. "
             "The table pools target positions and add/remove operations; exact groups and paired observations "
             "are retained in summary.json. Setup, payload verification, and export are excluded.", "",
             "| Issues | Side | Samples | Median wall ms | Median CPU ms | Median peak RSS MiB |",
             "|---:|---|---:|---:|---:|---:|"]
    for size in campaign.args.sizes:
        for side in ("baseline", "candidate"):
            rows = [r for r in samples if r["issues"] == size and r["side"] == side]
            lines.append(f"| {size} | {side} | {len(rows)} | "
                         f"{statistics.median(r['wall_seconds'] for r in rows) * 1000:.3f} | "
                         f"{statistics.median(r['cpu_seconds'] for r in rows) * 1000:.3f} | "
                         f"{statistics.median(r['peak_rss_bytes'] for r in rows) / 2**20:.3f} |")
    lines.extend(["", "Correctness passed: exact label changes and dirty IDs, expected audit events, "
                  "preserved prior events and unrelated issue/relationship rows, full exported payloads, "
                  "and independent integrity checks on private database-family copies.", "",
                  "Peak RSS comes from each child via wait4, not cumulative child rusage. "
                  "Concurrency latency includes writer queueing; it is not a direct lock-hold measurement. "
                  "Zero MemDatabase hydration is not measured here and would not prove zero scanning. "
                  "No speedup is attributed specifically to #619 when the binaries differ in other ways.", ""])
    (campaign.output / "summary.md").write_text("\n".join(lines), encoding="utf-8")


def integer_list(value):
    try:
        values = [int(part) for part in value.split(",")]
        require(values and len(values) == len(set(values)), "list must be nonempty and contain no duplicates")
        return values
    except ValueError as exc:
        raise argparse.ArgumentTypeError(str(exc)) from exc


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for side in ("baseline", "candidate"):
        parser.add_argument(f"--{side}", type=Path, required=True)
        parser.add_argument(f"--{side}-source", required=True)
        parser.add_argument(f"--{side}-lockfile", type=Path)
    parser.add_argument("--comparison-kind", choices=("release", "same-tree-opener", "calibration"), default="release")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sizes", type=integer_list, default=[1000, 4000, 16000])
    parser.add_argument("--description-bytes", type=int, default=10000)
    parser.add_argument("--targets", default="first,middle,last", help="comma-separated positions or zero-based indices")
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--writers", type=integer_list, default=[0], help="concurrent same-target writer counts; 0 disables")
    parser.add_argument("--writer-repeats", type=int, default=3)
    parser.add_argument("--retain-wal", action="store_true")
    parser.add_argument("--wal-growth-updates", type=int, default=0)
    parser.add_argument("--lock-timeout-ms", type=int, default=120000)
    parser.add_argument("--timeout-seconds", type=float, default=600)
    parser.add_argument("--reserve-mib", type=int, default=512)
    parser.add_argument("--notes", default="")
    parser.add_argument("--cc", default="cc", help="C11 compiler for the isolated wait4 supervisor")
    args = parser.parse_args()
    require(sys.platform in ("linux", "darwin") and hasattr(os, "wait4"), "requires Linux/macOS wait4 and flock")
    require(all(size >= 3 for size in args.sizes), "every size must contain at least three issues")
    require(args.description_bytes >= 0 and args.repeats > 0 and args.warmups >= 0, "invalid sample dimensions")
    require(all(0 <= count <= 32 for count in args.writers) and args.writer_repeats > 0, "writers must be 0..32")
    require(args.wal_growth_updates >= 0 and (args.retain_wal or not args.wal_growth_updates),
            "WAL growth requires --retain-wal")
    require(args.lock_timeout_ms > 0 and args.timeout_seconds > 0 and args.reserve_mib >= 0, "invalid limits")
    for side in ("baseline", "candidate"):
        path = getattr(args, side)
        require(path.is_absolute() and path.is_file() and os.access(path, os.X_OK), f"{side} must be an absolute executable")
        setattr(args, side, path.resolve())
    targets = args.targets.split(",")
    target_indices = {}
    for size in args.sizes:
        positions = {"first": 0, "middle": size // 2, "last": size - 1}
        indices = [positions[position] if position in positions else int(position) for position in targets]
        require(indices and len(set(indices)) == len(indices) and all(0 <= i < size for i in indices),
                f"invalid or duplicate targets for size {size}")
        target_indices[size] = list(zip(targets, indices))
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(__file__, output / "benchmark-source.py")
    campaign = Campaign(args, output)
    fixtures = []
    metadata = {"schema": SCHEMA, "time_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "platform": platform.platform(), "python": sys.version, "cpu_count": os.cpu_count(),
                "load_average": os.getloadavg(), "benchmark_sha256": sha256(__file__),
                "arguments": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                "comparison_kind": args.comparison_kind, "source_binding": "operator-supplied identifiers; binary hashes verified",
                "wal_mode": "retained" if args.retain_wal else "checkpointed",
                "timing": "fresh C supervisor per child; monotonic child wall; per-PID wait4 CPU/RSS; warm filesystem; untimed verification",
                "lock_scope": "end-to-end child duration includes queue wait; lock hold is not instrumented",
                "rss_units": "wait4 bytes on Darwin; wait4 KiB converted to bytes on Linux",
                "environment_policy": "remove BR_/BEADS_/BD_/FSQLITE_/TOON_ and preload overrides; pin BEADS_DIR and actor",
                "binaries": {}, "notes": args.notes}
    try:
        campaign.reserve()
        metadata["usage_supervisor"] = campaign.build_helper()
        for side in ("baseline", "candidate"):
            binary = getattr(args, side)
            row = campaign.run(binary, output, ["--version"], kind="control", phase="version", side=side)
            lockfile = getattr(args, f"{side}_lockfile")
            metadata["binaries"][side] = {"path": str(binary), "sha256": sha256(binary),
                "version": Path(row["prefix"]).with_suffix(".stdout").read_text().strip(),
                "source": getattr(args, f"{side}_source"),
                "lockfile": {"path": str(lockfile.resolve()), **file_record(lockfile)} if lockfile else None}
        same_binary = metadata["binaries"]["baseline"]["sha256"] == metadata["binaries"]["candidate"]["sha256"]
        require(not same_binary or args.comparison_kind == "calibration", "identical binaries require --comparison-kind calibration")
        write_json(output / "metadata.json", metadata)
        for size_index, size in enumerate(args.sizes):
            pair = {}
            for side in ("baseline", "candidate"):
                campaign.reserve(size * (args.description_bytes + 2000) * 4)
                fixture = Fixture(campaign, side, getattr(args, side), size)
                fixtures.append(fixture)
                fixture.prepare()
                pair[side] = fixture
            for growth in range(args.wal_growth_updates):
                operation = "add" if growth % 2 == 0 else "remove"
                for side in ("baseline", "candidate"):
                    fixture = pair[side]
                    fixture.write_label("probe-000000", operation, "bw619-wal-growth",
                                        kind="control", phase="wal-growth", growth=growth)
                    fixture.account_write("probe-000000", operation, "bw619-wal-growth")
            for fixture in pair.values():
                write_json(fixture.root / "measurement-start.json", {
                    "wal": fixture.check_wal_boundary(), "migration_marker": fixture.marker(),
                    "growth_updates": args.wal_growth_updates,
                })
            for repeat in range(args.warmups + args.repeats):
                for position_index, (position, number) in enumerate(target_indices[size]):
                    issue_id = f"probe-{number:06x}"
                    label = f"bw619-serial-{number}-{repeat}"
                    for operation_index, operation in enumerate(("add", "remove")):
                        order = ["baseline", "candidate"]
                        if (size_index + repeat + position_index + operation_index) % 2:
                            order.reverse()
                        for side in order:
                            fixture = pair[side]
                            fixture.check_wal_boundary()
                            fixture.write_label(issue_id, operation, label, kind="sample", stage="serial",
                                                position=position, repeat=repeat, warmup=repeat < args.warmups,
                                                pair_order=order)
                            fixture.account_write(issue_id, operation, label)
                            fixture.check_wal_boundary()
                            fixture.check_target(issue_id, "verify-serial-write")
            if size == max(args.sizes):
                for writers in args.writers:
                    if not writers:
                        continue
                    for repeat in range(args.writer_repeats):
                        issue_id = f"probe-{size - 1:06x}"
                        labels = [f"bw619-wave-{writers}-{repeat}-{worker}" for worker in range(writers)]
                        for operation_index, operation in enumerate(("add", "remove")):
                            order = ["baseline", "candidate"]
                            if (repeat + operation_index) % 2:
                                order.reverse()
                            for side in order:
                                fixture = pair[side]
                                before = fixture.check_wal_boundary()
                                wave_release = []
                                barrier = threading.Barrier(writers + 1,
                                    action=lambda: wave_release.append(time.monotonic_ns()))

                                def worker(worker_index):
                                    barrier.wait(timeout=args.timeout_seconds)
                                    return fixture.write_label(issue_id, operation, labels[worker_index],
                                        kind="concurrent-sample", writers=writers, worker=worker_index,
                                        repeat=repeat, pair_order=order, stage="writer-wave")

                                with concurrent.futures.ThreadPoolExecutor(max_workers=writers) as pool:
                                    futures = [pool.submit(worker, index) for index in range(writers)]
                                    barrier.wait(timeout=args.timeout_seconds)
                                    rows = [future.result() for future in futures]
                                released = wave_release[0]
                                for label in labels:
                                    fixture.account_write(issue_id, operation, label)
                                fixture.check_target(issue_id, "verify-concurrent-label-set")
                                wave = {"side": side, "issues": size, "writers": writers, "repeat": repeat,
                                        "operation": operation, "wal_before": before, "wal_after": wal_state(fixture.workspace),
                                        "aggregate_wall_seconds": (max(r["stop_ns"] for r in rows) - released) / 1e9,
                                        "completion_seconds": sorted((r["stop_ns"] - released) / 1e9 for r in rows),
                                        "individual_wall_seconds": [r["wall_seconds"] for r in rows],
                                        "total_cpu_seconds": sum(r["cpu_seconds"] for r in rows),
                                        "maximum_child_rss_bytes": max(r["peak_rss_bytes"] for r in rows),
                                        "launch_spread_seconds": (max(r["start_ns"] for r in rows) - min(r["start_ns"] for r in rows)) / 1e9,
                                        "serials": [r["serial"] for r in rows], "exact_durable_labels": True}
                                campaign.waves.append(wave)
                                write_json(fixture.root / f"wave-{writers}-{repeat}-{operation}.json", wave)
                                # A wave may leave WAL when peer teardown overlaps.
                                # A real sole-opener write, outside the sample,
                                # drains it before the next checkpointed wave.
                                if not args.retain_wal and wal_state(fixture.workspace)["frames"]:
                                    for reset_operation in ("add", "remove"):
                                        fixture.write_label(issue_id, reset_operation, "bw619-wave-checkpoint",
                                                            kind="control", phase="wave-checkpoint")
                                        fixture.account_write(issue_id, reset_operation, "bw619-wave-checkpoint")
                                    fixture.check_wal_boundary()
            for fixture in pair.values():
                fixture.verify_stored_changes()
                fixture.release_lease()
            print(json.dumps({"issues": size, "correct": True, "completed_samples":
                              sum(r.get("kind") == "sample" for r in campaign.rows)}), flush=True)
        for side, identity in metadata["binaries"].items():
            require(sha256(getattr(args, side)) == identity["sha256"], f"{side} executable changed during campaign")
        summary(campaign, metadata)
        print(str(output / "summary.md"))
    except BaseException as error:
        write_json(output / "failure.json", {"error": str(error), "type": type(error).__name__,
                                             "traceback": traceback.format_exc()})
        raise
    finally:
        for fixture in fixtures:
            fixture.release_lease()
        write_json(output / "observations.json", sorted(campaign.rows, key=lambda row: row["serial"]))


if __name__ == "__main__":
    main()
