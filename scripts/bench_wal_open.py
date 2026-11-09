#!/usr/bin/env python3
"""Reproduce #598 and compare fresh br processes on the same retained WAL.

Linux, Python standard library only. Defaults reproduce the issue's 5,000 x
25,000-byte corpus and five batches of 400 updates. A shared opener lease keeps
checkpointing disabled. Optional readers/writers exercise real CLI processes;
instrumented runs are separate from the timing samples. Every command, output,
binary identity, and correctness result is retained. The script never cleans up
the fixture. Use a new --output directory for every run.

Example:
  python3 scripts/bench_wal_open.py --baseline /abs/br --output /abs/results \
      --baseline-source v0.7.6 --diagnostic --io-counter /abs/libwal_io.so

For a paired comparison add --candidate /abs/br-fixed --candidate-source SHA.
Use --readers 2 --writer-updates 4 for concurrent real writer transactions.
Large runs may set --history-min-interval-secs explicitly; this changes only the
fixture writer's history policy and is recorded. No production flags are hidden.
"""

import argparse
import concurrent.futures
import datetime
import fcntl
import hashlib
import json
import multiprocessing
import os
from pathlib import Path
import platform
import resource
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import traceback


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path, value):
    with Path(path).open("x") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def file_record(path):
    path = Path(path)
    return {"bytes": path.stat().st_size, "sha256": sha256(path)} if path.exists() else None


def wal_state(workspace):
    path = Path(workspace) / ".beads" / "beads.db-wal"
    if not path.exists():
        return {"bytes": 0, "frames": 0, "exists": False}
    with path.open("rb") as stream:
        header = stream.read(32)
        size = os.fstat(stream.fileno()).st_size
    result = {"bytes": size, "exists": True}
    if len(header) == 32:
        page_size = struct.unpack_from(">I", header, 8)[0]
        if page_size == 1:
            page_size = 65536
        result.update(page_size=page_size, salts=header[16:24].hex())
        if page_size:
            result.update(frames=max(0, size - 32) // (page_size + 24),
                          trailing_bytes=max(0, size - 32) % (page_size + 24))
    return result


def recovery_inventory(workspace):
    beads = Path(workspace) / ".beads"
    return {str(p.relative_to(beads)): file_record(p) for p in sorted(beads.rglob("*"))
            if p.is_file() and any(word in str(p.relative_to(beads)).lower()
                                   for word in ("recovery", "quarantine", "corrupt"))}


def family_record(workspace):
    return {name: file_record(Path(workspace) / ".beads" / name)
            for name in ("beads.db", "beads.db-wal", "beads.db-shm", "issues.jsonl")}


def clean_environment(workspace):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("BR_", "BEADS_", "BD_", "FSQLITE_", "TOON_"))
           and k not in ("LD_PRELOAD", "WAL_IO_OUTPUT", "RUST_LOG")}
    env.update(BEADS_DIR=str(Path(workspace) / ".beads"), NO_COLOR="1", RUST_LOG="error")
    return env


def check_issue(stdout, expected):
    rows = json.loads(stdout)
    assert isinstance(rows, list) and len(rows) == 1, "expected exactly one JSON issue"
    issue = rows[0]
    for key, value in expected.items():
        assert issue.get(key) == value, f"unexpected {key}: {issue.get(key)!r}"


def run_child(job):
    """One pool worker runs one child at a time, so rusage deltas are isolated."""
    prefix = Path(job["prefix"])
    before_wal = wal_state(job["workspace"])
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic_ns()
    child = subprocess.run(job["command"], cwd=job["workspace"], env=job["env"],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           timeout=job.get("timeout", 600), check=False)
    stop = time.monotonic_ns()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    with prefix.with_suffix(".stdout").open("xb") as stream:
        stream.write(child.stdout)
    with prefix.with_suffix(".stderr").open("xb") as stream:
        stream.write(child.stderr)
    result = {k: v for k, v in job.items() if k not in ("env", "expected")}
    result.update(exit_code=child.returncode, start_ns=start, stop_ns=stop,
                  wall_seconds=(stop-start)/1e9, user_seconds=after.ru_utime-before.ru_utime,
                  system_seconds=after.ru_stime-before.ru_stime,
                  wal_before=before_wal, wal_after=wal_state(job["workspace"]),
                  stdout_sha256=hashlib.sha256(child.stdout).hexdigest(),
                  stderr_sha256=hashlib.sha256(child.stderr).hexdigest())
    result["cpu_seconds"] = result["user_seconds"] + result["system_seconds"]
    error = None
    if child.returncode not in job.get("allowed_exit_codes", [0]):
        error = f"child exit {child.returncode}: {child.stderr[-2000:].decode(errors='replace')}"
    elif any((b"jsonl" in line and b"fallback" in line) or b"reads issues.jsonl directly" in line
             for line in child.stderr.lower().splitlines()):
        error = "a database read fell back to JSONL"
    elif "expected" in job:
        try:
            check_issue(child.stdout, job["expected"])
        except (AssertionError, ValueError) as exc:
            error = str(exc)
    result["correct"] = error is None
    result["error"] = error
    write_json(prefix.with_suffix(".json"), result)
    return result


def writer_run(template, initial_priority, minimum_commits, stop_path, started_path):
    """Keep real writer processes running through every measured reader group."""
    with Path(started_path).open("x") as stream:
        stream.write(str(time.monotonic_ns()))
    results = []
    deadline = time.monotonic() + 600
    while len(results) < minimum_commits or not Path(stop_path).exists():
        assert time.monotonic() < deadline, "writer wave exceeded ten minutes"
        index = len(results)
        priority = (initial_priority + index + 1) % 4
        job = {**template, "command": list(template["command"]), "priority_written": priority}
        job["command"][4] = str(priority)
        job["prefix"] = template["prefix"] + f"-{index:04d}"
        result = run_child(job)
        results.append(result)
        if not result["correct"]:
            break
    return results


def warm_worker():
    return os.getpid()


def summarize(rows):
    groups = {}
    for row in rows:
        if row.get("kind") != "sample" or row.get("warmup"):
            continue
        key = (row["stage"], row["binary"], row["mode"])
        groups.setdefault(key, []).append(row)
    answer = []
    for (stage, binary, mode), group in groups.items():
        item = {"stage": stage, "binary": binary, "mode": mode, "samples": len(group),
                "correct": all(r["correct"] for r in group),
                "wal_bytes_min": min(r["wal_before"]["bytes"] for r in group),
                "wal_bytes_max": max(r["wal_after"]["bytes"] for r in group),
                "readers_overlapping_writer": sum(bool(r.get("overlapping_writer_commands")) for r in group)}
        for field in ("wall_seconds", "user_seconds", "system_seconds", "cpu_seconds"):
            values = [r[field] for r in group]
            item[field] = {"min": min(values), "median": statistics.median(values), "max": max(values)}
        answer.append(item)
    return answer


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--baseline-source", required=True)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--candidate-source")
    parser.add_argument("--driver", type=Path, help="fixture writer; defaults to baseline")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reuse-workspace", type=Path,
                        help="mutate a previous successful benchmark fixture; retain and bind its original reports")
    parser.add_argument("--issues", type=int, default=5000)
    parser.add_argument("--description-bytes", type=int, default=25000)
    parser.add_argument("--batch", type=int, default=400)
    parser.add_argument("--steps", type=int, default=5)
    parser.add_argument("--growth-offset", type=int, default=0,
                        help="offset the issue's update-batch sequence when reusing an already-mutated fixture")
    parser.add_argument("--target-wal-mib", type=float,
                        help="stop growth after reaching this size; --steps is still the maximum")
    parser.add_argument("--measure-at-mib", default="",
                        help="comma-separated WAL thresholds; sample at crossings, stage zero, and the final stage")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--readers", type=int, default=1)
    parser.add_argument("--writer-updates", type=int, default=0,
                        help="minimum writer commits per measured stage; continue until all reader waves finish")
    parser.add_argument("--writer-sync", choices=("default", "no-auto"), default="default")
    parser.add_argument("--diagnostic", action="store_true", help="also time explicit no-auto-sync reads")
    parser.add_argument("--io-counter", type=Path, help="separate untimed LD_PRELOAD diagnostics")
    parser.add_argument("--replay-logs", action="store_true", help="separate untimed WAL debug diagnostics")
    parser.add_argument("--history-min-interval-secs", type=int)
    parser.add_argument("--reserve-mib", type=int, default=2048)
    parser.add_argument("--disk-temp", action="store_true",
                        help="put TMPDIR under this campaign; required when large concurrent snapshots would fill /tmp")
    parser.add_argument("--notes", default="")
    args = parser.parse_args()
    assert sys.platform == "linux", "the shared opener flock protocol is Linux-specific"
    assert args.issues >= 2 and 1 <= args.batch <= args.issues
    assert args.description_bytes >= 0 and args.samples >= 1 and args.warmups >= 0
    assert args.steps >= 0 and args.growth_offset >= 0 and args.readers >= 1 and args.writer_updates >= 0
    assert bool(args.candidate) == bool(args.candidate_source)
    assert args.reserve_mib >= 0
    assert args.target_wal_mib is None or args.target_wal_mib > 0
    thresholds = sorted({float(value) * 1024 * 1024 for value in args.measure_at_mib.split(",") if value})
    assert all(value > 0 for value in thresholds)
    target_bytes = args.target_wal_mib * 1024 * 1024 if args.target_wal_mib else None
    target_reached = False
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    commands = output / "commands"
    commands.mkdir()
    workspace = args.reuse_workspace.resolve() if args.reuse_workspace else output / "workspace"
    origin_reports = {}
    if args.reuse_workspace:
        origin = workspace.parent
        origin_metadata = json.loads((origin / "metadata.json").read_text())
        assert origin_metadata["schema"] == "br.wal-open-benchmark.v2"
        assert Path(origin_metadata["workspace"]).resolve() == workspace
        assert not (origin / "failure.json").exists(), "cannot reuse a failed campaign"
        assert json.loads((origin / "summary.json").read_text())["ok"] is True
        for key in ("issues", "description_bytes"):
            assert origin_metadata["arguments"][key] == getattr(args, key), f"reuse mismatch: {key}"
        origin_reports = {str(origin / name): file_record(origin / name)
                          for name in ("metadata.json", "summary.json", "observations.json")}
    else:
        workspace.mkdir()
    shutil.copyfile(__file__, output / "benchmark-source.py")
    binaries = {"baseline": args.baseline.resolve()}
    if args.candidate:
        binaries["candidate"] = args.candidate.resolve()
    driver = (args.driver or args.baseline).resolve()
    env = clean_environment(workspace)
    temporary_directory = output / "tmp" if args.disk_temp else Path(tempfile.gettempdir())
    if args.disk_temp:
        temporary_directory.mkdir()
        env["TMPDIR"] = str(temporary_directory)
    mount = json.loads(subprocess.check_output(
        ["findmnt", "-J", "-T", str(temporary_directory), "-o", "TARGET,SOURCE,FSTYPE,OPTIONS"],
        env=env))
    if args.disk_temp:
        assert all(item["fstype"] not in ("tmpfs", "ramfs") for item in mount["filesystems"]), mount
    driver_env = dict(env)
    if args.history_min_interval_secs is not None:
        assert args.history_min_interval_secs >= 0
        driver_env["BR_HISTORY_MIN_INTERVAL_SECS"] = str(args.history_min_interval_secs)
    io_counter = args.io_counter.resolve() if args.io_counter else None
    rows, stages = [], []
    expected_priority = {i: i % 4 for i in range(args.issues)}
    description = "x" * args.description_bytes
    if args.reuse_workspace:
        observed = set()
        with (workspace / ".beads" / "issues.jsonl").open() as stream:
            for line in stream:
                issue = json.loads(line)
                number = int(issue["id"].removeprefix("probe-"), 16)
                assert 0 <= number < args.issues and number not in observed
                assert issue["id"] == f"probe-{number:06x}" and issue["title"] == f"t{number}"
                assert issue["description"] == description and issue["status"] == "open"
                assert issue["issue_type"] == "task" and issue["priority"] in range(5)
                expected_priority[number] = issue["priority"]
                observed.add(number)
        assert len(observed) == args.issues, "reused corpus is incomplete"
    serial = 0
    lease = None
    initial_inventory = None

    def reserve(extra=0):
        free = shutil.disk_usage(output).free
        # Strict main-branch readers retain private family copies. Budget two
        # copies per reader plus two for writer/doctor control work, beyond the
        # output reserve and JSONL publication/history headroom. This is a
        # conservative capacity guard, not a claim that every copy coexists.
        family_bytes = sum(path.stat().st_size for name in ("beads.db", "beads.db-wal", "beads.db-shm")
                           if (path := workspace / ".beads" / name).is_file())
        temporary_copies = family_bytes * (2 * args.readers + 2) if args.disk_temp else 0
        required = args.reserve_mib * 1024 * 1024 + extra + temporary_copies
        assert free >= required, f"disk reserve: {free} free, {required} required; stopping without cleanup"

    def job(label, binary_path, command, **extra):
        nonlocal serial
        serial += 1
        result = {"command": [str(binary_path)] + command, "workspace": str(workspace),
                  "env": env, "prefix": str(commands / f"{serial:06d}-{label}"), **extra}
        return result

    def accept(result):
        rows.append(result)
        assert result["correct"], result["error"]
        return result

    def control(label, command, **extra):
        reserve(args.issues * args.description_bytes * 3)
        return accept(run_child(job(label, driver, command, env=driver_env, kind="control", **extra)))

    def doctor(label):
        result = control(label, ["doctor", "--json"], allowed_exit_codes=[0, 1])
        report = json.loads(Path(result["prefix"]).with_suffix(".stdout").read_text())
        checks = {item["name"]: item for item in report["checks"]}
        assert checks["sqlite.integrity_check"]["status"] == "ok", checks
        assert checks["counts.db_vs_jsonl"]["status"] == "ok", checks
        findings = {(item["name"], item["status"]) for item in report["checks"] if item["status"] != "ok"}
        allowed = {("jsonl_size", "warn"), ("db.recovery_artifacts", "warn")}
        assert findings <= allowed, f"unexpected doctor findings: {findings-allowed}"
        return findings

    def expected(i):
        return {"id": f"probe-{i:06x}", "title": f"t{i}", "description": description,
                "status": "open", "priority": expected_priority[i], "issue_type": "task"}

    def probe_job(label, binary_name, mode, stage, **extra):
        flags = ["--no-auto-import", "--no-auto-flush"] if mode == "explicit-no-sync" else []
        return job(label, binaries[binary_name], ["show", "probe-000001", "--json"] + flags,
                   expected=expected(1), binary=binary_name, mode=mode, stage=stage, **extra)

    metadata = {"schema": "br.wal-open-benchmark.v2", "time_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                "workspace": str(workspace), "platform": platform.platform(), "python": sys.version,
                "benchmark_sha256": sha256(__file__), "notes": args.notes,
                "temporary_directory": {"path": str(temporary_directory), "TMPDIR_override": args.disk_temp, "mount": mount},
                "temporary_capacity_budget": "two database+WAL+SHM copies per reader plus two for writer/doctor when --disk-temp",
                "fixture_mode": "reuse" if args.reuse_workspace else "fresh",
                "origin_reports": origin_reports,
                "reused_family_before_any_command": family_record(workspace) if args.reuse_workspace else None,
                "binaries": {k: {"path": str(v), "sha256": sha256(v)} for k, v in binaries.items()},
                "driver": {"path": str(driver), "sha256": sha256(driver)},
                "io_counter": {"path": str(io_counter), "sha256": sha256(io_counter)} if io_counter else None,
                "timing": "fresh processes; warm filesystem; uninstrumented samples; process-local child rusage deltas",
                "writer_policy": "growth uses default sync; writer continues through measured waves; sync=" + args.writer_sync,
                "disk_free_initial": shutil.disk_usage(output).free}
    write_json(output / "metadata.json", metadata)
    try:
        control("version", ["--version"])
        if args.reuse_workspace:
            for i in (0, 1):
                accept(run_child(job(f"verify-reuse-{i}", driver,
                                    ["show", f"probe-{i:06x}", "--json", "--no-auto-import", "--no-auto-flush"],
                                    kind="control", expected=expected(i))))
        else:
            control("init", ["init", "--prefix", "probe"])
            with (workspace / ".beads" / "issues.jsonl").open("w") as stream:
                for i in range(args.issues):
                    issue = expected(i)
                    issue.update(created_at="2026-11-01T00:00:00Z", updated_at="2026-11-01T00:00:00Z")
                    stream.write(json.dumps(issue) + "\n")
            control("import", ["sync", "--import-only", "--force"])
        initial_findings = doctor("doctor-initial")
        initial_inventory = recovery_inventory(workspace)
        write_json(output / "initial-family.json", family_record(workspace))
        write_json(output / "initial-recovery.json", initial_inventory)
        locks = [p for p in (workspace / ".beads").glob(".br-db-openers-*.lock") if "transition" not in p.name]
        assert len(locks) == 1, f"expected one shared opener lock, got {locks}"
        modes = ["default", "explicit-no-sync"] if args.diagnostic else ["default"]
        context = multiprocessing.get_context("spawn")
        with concurrent.futures.ProcessPoolExecutor(max_workers=args.readers+1, mp_context=context) as pool:
            # Start workers before acquiring the lease; spawn avoids inherited flock descriptors.
            futures = [pool.submit(warm_worker) for _ in range(args.readers+1)]
            for future in futures:
                future.result()
            lease = locks[0].open("rb")
            fcntl.flock(lease.fileno(), fcntl.LOCK_SH)
            for stage in list(range(args.steps+1)) + [-1]:
                if stage >= 0 and target_reached:
                    continue
                reserve(args.issues * args.description_bytes * 3)
                if stage == -1:
                    assert target_bytes is None or target_reached, "maximum growth steps did not reach target WAL size"
                    fcntl.flock(lease.fileno(), fcntl.LOCK_UN)
                    lease.close()
                    lease = None
                    expected_priority[0] = (expected_priority[0]+1) % 4
                    control("checkpoint", ["update", "probe-000000", "--priority", str(expected_priority[0])])
                    assert wal_state(workspace).get("frames", 0) == 0, "sole-opener checkpoint did not empty WAL"
                elif stage:
                    growth_step = stage + args.growth_offset
                    ids = [(growth_step * args.batch + i) % args.issues for i in range(args.batch)]
                    priority = growth_step % 4
                    control(f"grow-{stage}", ["update"] + [f"probe-{i:06x}" for i in ids] + ["--priority", str(priority)])
                    for i in ids:
                        expected_priority[i] = priority
                current_wal = wal_state(workspace)
                crossed = []
                while thresholds and current_wal["bytes"] >= thresholds[0]:
                    crossed.append(thresholds.pop(0))
                target_reached = target_bytes is not None and current_wal["bytes"] >= target_bytes
                measure = not args.measure_at_mib or bool(crossed) or stage in (0, -1, args.steps) or target_reached
                entry = {"stage": stage, "wal_before": current_wal,
                         "disk_free_before": shutil.disk_usage(output).free,
                         "measured": measure, "crossed_measurement_threshold_bytes": crossed}
                if not measure:
                    stages.append(entry)
                    write_json(output / f"stage-{stage}.json", entry)
                    print(json.dumps(entry), flush=True)
                    continue
                first_row = len(rows)
                writer_future = None
                writer_template = None
                writer_stop = output / f"writer-{stage}.stop"
                writer_started = output / f"writer-{stage}.started"
                if stage >= 0 and args.writer_updates:
                    writer_flags = ["--no-auto-import", "--no-auto-flush"] if args.writer_sync == "no-auto" else []
                    writer_template = job(f"writer-{stage}", driver,
                        ["update", "probe-000000", "--priority", str(expected_priority[0])] + writer_flags,
                        env=driver_env, kind="writer", stage=stage)
                try:
                    for repetition in range(args.warmups+args.samples):
                        reserve(args.issues * args.description_bytes * 3)
                        if writer_template and repetition == args.warmups:
                            writer_future = pool.submit(writer_run, writer_template, expected_priority[0],
                                                        args.writer_updates, str(writer_stop), str(writer_started))
                            deadline = time.monotonic() + 30
                            while not writer_started.exists():
                                assert not writer_future.done(), "writer exited before starting"
                                assert time.monotonic() < deadline, "writer did not start"
                                time.sleep(0.01)
                        for mode in modes:
                            order = list(binaries)
                            if (stage+repetition) % 2:
                                order.reverse()
                            for binary_name in order:
                                jobs = [probe_job(f"read-{stage}-{mode}-{binary_name}-{repetition}-{reader}",
                                                  binary_name, mode, stage, kind="sample", warmup=repetition<args.warmups,
                                                  repetition=repetition, reader=reader) for reader in range(args.readers)]
                                for result in pool.map(run_child, jobs):
                                    accept(result)
                finally:
                    if writer_future is not None:
                        with writer_stop.open("x") as stream:
                            stream.write(str(time.monotonic_ns()))
                writers = writer_future.result() if writer_future else []
                for result in writers:
                    accept(result)
                for result in rows[first_row:]:
                    if result.get("kind") == "sample":
                        result["overlapping_writer_commands"] = sum(
                            result["start_ns"] < writer["stop_ns"] and writer["start_ns"] < result["stop_ns"]
                            for writer in writers)
                if writers:
                    expected_priority[0] = writers[-1]["priority_written"]
                    for binary_name in binaries:
                        for mode in modes:
                            assert any(r.get("overlapping_writer_commands") and not r.get("warmup")
                                       and r.get("binary") == binary_name and r.get("mode") == mode
                                       for r in rows[first_row:]), f"no measured writer overlap: {binary_name}/{mode}"
                    verify = job(f"verify-writer-{stage}", driver,
                        ["show", "probe-000000", "--json", "--no-auto-import", "--no-auto-flush"],
                        kind="control", expected=expected(0))
                    accept(run_child(verify))
                for binary_name in binaries:
                    for mode in modes:
                        if io_counter:
                            diagnostic = probe_job(f"io-{stage}-{binary_name}-{mode}", binary_name, mode, stage, kind="io-diagnostic")
                            count_path = Path(diagnostic["prefix"]).with_suffix(".counts.json")
                            diagnostic["env"] = {**env, "LD_PRELOAD": str(io_counter), "WAL_IO_OUTPUT": str(count_path)}
                            result = accept(run_child(diagnostic))
                            counts = json.loads(count_path.read_text())
                            assert counts.get("overflow_calls", 0) == 0 and counts.get("pread_failed_calls", 0) == 0, counts
                            result["io_counts"] = counts
                        if args.replay_logs:
                            diagnostic = probe_job(f"replay-{stage}-{binary_name}-{mode}", binary_name, mode, stage, kind="replay-diagnostic")
                            diagnostic["env"] = {**env, "RUST_LOG": "fsqlite_wal::wal=debug"}
                            accept(run_child(diagnostic))
                entry.update(wal_after=wal_state(workspace), disk_free_after=shutil.disk_usage(output).free)
                reserve()
                stages.append(entry)
                write_json(output / f"stage-{stage}.json", entry)
                print(json.dumps(entry), flush=True)
        listing = control("verify-all-rows", ["list", "--all", "--limit", "0", "--fields",
                          "id,title,status,priority,issue_type", "--json", "--no-auto-import", "--no-auto-flush"])
        page = json.loads(Path(listing["prefix"]).with_suffix(".stdout").read_text())
        assert page["total"] == args.issues and page["has_more"] is False
        observed = set()
        for issue in page["issues"]:
            number = int(issue["id"].removeprefix("probe-"), 16)
            assert number not in observed and 0 <= number < args.issues
            assert issue == {key: value for key, value in expected(number).items() if key != "description"}
            observed.add(number)
        assert len(observed) == args.issues
        observed.clear()
        with (workspace / ".beads" / "issues.jsonl").open() as stream:
            for line in stream:
                issue = json.loads(line)
                number = int(issue["id"].removeprefix("probe-"), 16)
                assert number not in observed and 0 <= number < args.issues
                assert all(issue.get(key) == value for key, value in expected(number).items())
                observed.add(number)
        assert len(observed) == args.issues
        final_findings = doctor("doctor-final")
        assert final_findings == initial_findings, "doctor findings changed"
        final_inventory = recovery_inventory(workspace)
        assert final_inventory == initial_inventory, "recovery or quarantine inventory changed"
        write_json(output / "final-family.json", family_record(workspace))
        write_json(output / "final-recovery.json", final_inventory)
        assert all(file_record(path) == record for path, record in origin_reports.items()), "origin reports changed"
        write_json(output / "summary.json", {"ok": True, "notes": args.notes, "stages": stages,
                                             "fixture_mode": metadata["fixture_mode"], "origin_reports": origin_reports,
                                             "groups": summarize(rows), "all_commands_correct": all(r["correct"] for r in rows)})
    except BaseException as exc:
        write_json(output / "failure.json", {"type": type(exc).__name__, "error": str(exc),
                                              "traceback": traceback.format_exc(), "wal": wal_state(workspace),
                                              "disk_free": shutil.disk_usage(output).free})
        raise
    finally:
        if lease is not None:
            fcntl.flock(lease.fileno(), fcntl.LOCK_UN)
            lease.close()
        write_json(output / "observations.json", rows)


if __name__ == "__main__":
    main()
