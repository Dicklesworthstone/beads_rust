"""Real stock-SQLite/CLI regression for GH #532; uses only scratch databases.

Arguments: BR WORKSPACE CASE {discovery,explicit} [--expect-latch].
The optional negative control verifies the original failure on an old binary;
normal Cargo tests require recovery without any manual repair or sidecar removal.
"""
import base64
import concurrent.futures
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import selectors
import sqlite3
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
CREATE_AFTER = ("create", "after", "-t", "task", "-p", "3", "--json")


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


def listed_issues(result):
    page = json.loads(result.stdout)
    assert isinstance(page, dict) and isinstance(page.get("issues"), list), page
    return page["issues"]


def contention(result):
    message = text(result)
    assert result.returncode != 0, ("reader must exclude recovery", message)
    assert LATCH not in message, ("transient contention latched", message)
    assert any(part in message for part in (
        "database is busy", "verified sole opener", "Timed out after",
        "Database is locked", "database is locked",
    )), ("unexpected failure", result.returncode, message)


def protected_family():
    # Closing ANY descriptor for an inode in the SQLite reader's process can
    # release that process's POSIX locks, including the reader's own locks.
    # Witness main/WAL/SHM from a separate process, never with read_bytes here.
    # Retain complete bytes and modes, not just the presence of the files.
    observe = """
import base64, json, stat, sys
from pathlib import Path
result = {}
for name in sys.argv[1:]:
    path = Path(name)
    try:
        result[name] = [base64.b64encode(path.read_bytes()).decode("ascii"),
                        stat.S_IMODE(path.stat().st_mode)]
    except FileNotFoundError:
        result[name] = None
print(json.dumps(result))
"""
    paths = (
        DB, Path(str(DB) + "-wal"), Path(str(DB) + "-shm"),
        Path(str(DB) + "-journal"), BEADS / "issues.jsonl",
    )
    observed = subprocess.run(
        [sys.executable, "-I", "-S", "-c", observe, *map(str, paths)],
        env=ENV, text=True, capture_output=True, timeout=45, check=True,
    )
    return {
        name: None if value is None else (base64.b64decode(value[0], validate=True), value[1])
        for name, value in json.loads(observed.stdout).items()
    }


@contextmanager
def stock_reader():
    uri = DB.as_uri() + "?mode=ro"
    if EXPLICIT:
        # Linux corroboration: one Python process owns the connection and
        # sequences br subprocesses, then closes it before the next br open.
        reader = sqlite3.connect(uri, uri=True)
        try:
            assert reader.execute("select count(*) from issues").fetchone() == (1,)
            yield
        finally:
            reader.close()
        return
    # Reporter ordering: a separate stock-reader process stays alive until
    # all refused br commands finish. Pipes synchronize this without sleeps.
    holder = """
import sqlite3, sys
reader = sqlite3.connect(sys.argv[1], uri=True)
try:
    assert reader.execute("select count(*) from issues").fetchone() == (1,)
    print("reader open", flush=True)
    assert sys.stdin.read(1) == "x"
finally:
    reader.close()
"""
    child = subprocess.Popen(
        [sys.executable, "-I", "-S", "-c", holder, uri], env=ENV, text=True,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        with selectors.DefaultSelector() as ready:
            ready.register(child.stdout, selectors.EVENT_READ)
            assert ready.select(45), "stock reader did not announce readiness"
        assert child.stdout.readline().strip() == "reader open", "stock reader failed to open"
        yield
    finally:
        try:
            _, stderr = child.communicate(input="x", timeout=45)
            assert child.returncode == 0, ("stock reader failed", stderr)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=45)


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
    with stock_reader():
        before = protected_family()
        wal = before[str(DB) + "-wal"][0]
        assert len(wal) == 32, "the index-only recovery fast path must be exercised"
        original_index = before[str(DB) + "-shm"][0]
        old_failures = set(failures())
        busy = run("list", "--json")
        assert busy.returncode == 2, ("first busy attempt", busy.returncode, text(busy))
        assert "database is busy (recovery in progress)" in text(busy), text(busy)
        created = set(failures()) - old_failures
        assert len(created) == 1, (
            "one invocation must run recovery exactly once, not time out or retry internally",
            created,
        )
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
            if iteration == rounds - 1:
                # The mutation must stop at the same single failed startup
                # attempt. Submit this exact request again only after the
                # holder has exited, below, and require exactly one new issue.
                before_create = set(failures())
                refused_create = run(*CREATE_AFTER)
                contention(refused_create)
                assert refused_create.returncode == 2, text(refused_create)
                create_failures = set(failures()) - before_create
                assert len(create_failures) == 1, (
                    "a contended mutation must retain one recovery attempt and return",
                    create_failures,
                )
                remember_evidence(create_failures, original_index)
        assert protected_family() == before, "a busy recovery changed the original family"
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
        # Race the FIRST successful recovery after the holder exits, then
        # repeat fresh-process waves. All-busy waves are not proof of progress.
        for _ in range(3):
            results = fanout()
            assert any(result.returncode == 0 for result in results), [text(r) for r in results]
            for result in results:
                if result.returncode != 0:
                    contention(result)
                else:
                    assert any(issue["id"] == seed_id for issue in listed_issues(result)), result.stdout
    for _ in range(3):
        result = success("list", "--json")
        assert [issue["id"] for issue in listed_issues(result)] == [seed_id], (
            "a refused command must not have created an issue",
            result.stdout,
        )
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
after = json.loads(success(*CREATE_AFTER).stdout)
assert after["id"] != seed_id
listed = listed_issues(success("list", "--json"))
assert {issue["id"] for issue in listed} == {seed_id, after["id"]}
assert len(listed) == 2, "the later mutation must execute exactly once"
assert_evidence_retained()
print(json.dumps({"result": "passed", "case": CASE, "explicit_db": EXPLICIT}))
