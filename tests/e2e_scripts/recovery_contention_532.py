"""Real stock-SQLite/CLI regression for GH #532; uses only scratch databases.

Arguments: BR WORKSPACE CASE {discovery,explicit} [--expect-latch].
The optional negative control verifies the original failure on an old binary;
normal Cargo tests require recovery without any manual repair or sidecar removal.
"""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys
import threading


BR = str(Path(sys.argv[1]).resolve())
ROOT = Path(sys.argv[2]).resolve()
CASE = sys.argv[3]
EXPLICIT = sys.argv[4] == "explicit"
EXPECT_LATCH = "--expect-latch" in sys.argv[5:]
HOME = ROOT / "home"
REPO = ROOT / "repo"
BEADS = REPO / ".beads"
DB = BEADS / "beads.db"
for directory in (HOME, REPO):
    directory.mkdir(parents=True, exist_ok=True)
ENV = {
    key: value for key, value in os.environ.items()
    if not key.startswith(("BEADS_", "BD_", "BR_", "FSQLITE_"))
}
ENV.update({
    "HOME": str(HOME),
    "XDG_CONFIG_HOME": str(HOME / ".config"),
    "BEADS_DIR": str(BEADS),
    "BEADS_DB": str(DB),
    "BD_DB": str(DB),
    "RUST_LOG": "off",
    "NO_COLOR": "1",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": os.devnull,
})
PREFIX = [BR, "--db", str(DB)] if EXPLICIT else [BR]
LATCH = "automatic WAL index recovery already failed for this exact database family"
EVIDENCE = {}


def run(*args):
    return subprocess.run(
        PREFIX + list(args), cwd=REPO, env=ENV, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45, check=False,
    )


def text(result):
    return result.stdout + result.stderr


def success(*args):
    result = run(*args)
    assert result.returncode == 0, (args, result.returncode, text(result))
    return result


def contention(result):
    message = text(result)
    assert result.returncode != 0, ("reader must exclude recovery", message)
    assert LATCH not in message, ("transient contention latched", message)
    assert any(part in message for part in (
        "database is busy", "verified sole opener", "Timed out after",
        "Database is locked", "database is locked",
    )), ("unexpected failure", result.returncode, message)


def bytes_and_mode(path):
    try:
        return path.read_bytes(), stat.S_IMODE(path.stat().st_mode)
    except FileNotFoundError:
        return None


def protected_family():
    # Include the original index in the busy-attempt witness; only successful
    # recovery may replace it. JSONL cannot be used to rebuild away the problem.
    return {
        str(path): bytes_and_mode(path)
        for path in (
            DB, Path(str(DB) + "-wal"), Path(str(DB) + "-shm"),
            Path(str(DB) + "-journal"), BEADS / "issues.jsonl",
        )
    }


def failures():
    return sorted((BEADS / ".br_recovery/schema-migrations").glob("*/recovery-failed.json"))


def remember_evidence(paths, expected_index):
    for path in paths:
        receipt = json.loads(path.read_text())
        assert receipt["stage"] == "live-recovery", receipt
        assert receipt["backup_scope"] == "wal-index-only", receipt
        assert receipt["error"] == "Database error: database is busy (recovery in progress)", receipt
        if not EXPECT_LATCH:
            assert receipt["failure_kind"] == "transient-contention", receipt
        backup = Path(receipt["backup_path"])
        assert backup.resolve().is_relative_to(BEADS.resolve()), backup
        index = backup / "beads.db-shm"
        assert index.read_bytes() == expected_index, "pre-recovery index evidence changed"
        for retained in (path, index):
            current = retained.read_bytes()
            if retained in EVIDENCE:
                assert EVIDENCE[retained] == current, ("evidence overwritten", retained)
            else:
                EVIDENCE[retained] = current


def assert_evidence_retained():
    for path, original in EVIDENCE.items():
        assert path.read_bytes() == original, ("recovery evidence modified or pruned", path)


def fanout():
    # A barrier, not sleeps, starts four distinct CLI processes together.
    barrier = threading.Barrier(4)

    def attempt(_):
        barrier.wait(timeout=10)
        return run("list", "--json")

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        return list(pool.map(attempt, range(4)))


subprocess.run(["git", "init", "-q"], cwd=REPO, env=ENV, check=True, timeout=30)
success("init", "--prefix", "probe", "--json")
seed = json.loads(success("create", "seed", "-t", "task", "-p", "3", "--json").stdout)
seed_id = seed["id"]
success("list", "--json")
rounds = 3 if CASE == "concurrent" else 1
for iteration in range(rounds):
    # Exactly the reporter's sequence: a stock read-only SQLite connection,
    # a completed SELECT, one or more completed br opens, then reader.close()
    # before the next br process is even started. No timing-based release.
    reader = sqlite3.connect(DB.as_uri() + "?mode=ro", uri=True)
    try:
        assert reader.execute("select count(*) from issues").fetchone()[0] == 1
        before = protected_family()
        wal = Path(str(DB) + "-wal").read_bytes()
        assert len(wal) == 32, "the index-only recovery fast path must be exercised"
        original_index = Path(str(DB) + "-shm").read_bytes()
        old_failures = set(failures())
        busy = run("list", "--json")
        assert busy.returncode == 2, ("first busy attempt", busy.returncode, text(busy))
        assert "database is busy (recovery in progress)" in text(busy), text(busy)
        created = set(failures()) - old_failures
        assert created, "the actual recovery failure path must run, not just a lock timeout"
        remember_evidence(created, original_index)
        if not EXPECT_LATCH:
            if CASE == "concurrent":
                for result in fanout():
                    contention(result)
            else:
                # A second busy attempt over unchanged bytes is also eligible;
                # no unbounded internal retry loop or manual repair is involved.
                contention(run("list", "--json"))
            remember_evidence(set(failures()) - old_failures, original_index)
        assert protected_family() == before, "a busy recovery changed the original family"
    finally:
        reader.close()
    assert protected_family() == before, "reader exit must leave the same witnessed bytes"

    if EXPECT_LATCH:
        for _ in range(3):
            latched = run("list", "--json")
            assert latched.returncode == 6 and LATCH in text(latched), text(latched)
        assert_evidence_retained()
        print(json.dumps({"control": "original-latch-reproduced", "sqlite": sqlite3.sqlite_version}))
        sys.exit(0)

    if CASE == "legacy":
        # Model an upgrade from 0.7.4: remove only the new diagnostic field
        # from synthetic receipts. The actual retained indexes stay untouched.
        for path in failures():
            receipt = json.loads(path.read_text())
            receipt.pop("failure_kind")
            path.write_text(json.dumps(receipt) + "\n")
            EVIDENCE[path] = path.read_bytes()

    if CASE == "concurrent":
        for result in fanout():
            if result.returncode != 0:
                contention(result)
    for _ in range(3):
        result = success("list", "--json")
        assert any(issue["id"] == seed_id for issue in json.loads(result.stdout)), result.stdout
    # Successful recovery may rebuild -shm, but not replace durable issue
    # bytes from JSONL or alter a journal that it did not own.
    after_recovery = protected_family()
    for path in (DB, Path(str(DB) + "-journal"), BEADS / "issues.jsonl"):
        assert after_recovery[str(path)] == before[str(path)], (
            "recovery changed a protected component", path,
        )
    assert_evidence_retained()
    print(json.dumps({
        "round": iteration + 1, "case": CASE, "post_reader_lists": "passed",
        "retained_failures": len(failures()),
        "original_db_sha256": hashlib.sha256(before[str(DB)][0]).hexdigest(),
        "sqlite": sqlite3.sqlite_version,
    }))

# Reading again is insufficient: the workspace must admit a real new mutation.
after = json.loads(success("create", "after", "-t", "task", "-p", "3", "--json").stdout)
assert after["id"] != seed_id
listed = json.loads(success("list", "--json").stdout)
assert {issue["id"] for issue in listed} == {seed_id, after["id"]}
assert_evidence_retained()
print(json.dumps({"result": "passed", "case": CASE, "explicit_db": EXPLICIT}))
