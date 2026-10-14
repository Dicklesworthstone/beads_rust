#!/usr/bin/env python3
"""Qualify the packaged recovery launcher or explicitly test only its payload.

--payload-only executes the embedded -I/-S/-u/-c recipe, NOT a Rust binary.
--recovery-binary requires a real compiled br-wal-recover executable.
Add --br to qualify restored/copied trackers with the real br engine as well.
All databases are new disposable fixtures; all observations and logs are kept.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PAYLOAD = (ROOT / "scripts/recover_orphaned_wal.py").read_text(encoding="utf-8")
BINARY = None
BR = None
SUFFIXES = ("", "-wal", "-shm", "-journal", "-wal-cert", "-wal-cert-head",
            "-ns", "-fsqlite-ns-gate", "-fsqlite-ns-use", ".fsqlite-migration-state")
CLOSURE = ("closed", "2026-10-14T00:00:00Z", "WAL-only ' closure λ")


def sidecar(db, suffix):
    return db.with_name(db.name + suffix)


def family(db):
    result = {}
    for suffix in SUFFIXES:
        path = sidecar(db, suffix)
        try:
            info = path.stat()
            result[suffix] = (path.read_bytes(), info.st_dev, info.st_ino,
                              stat.S_IMODE(info.st_mode), info.st_uid, info.st_gid,
                              info.st_mtime_ns, info.st_ctime_ns)
        except FileNotFoundError:
            result[suffix] = None
    return result


def make_orphan(root):
    original = root / "writer.db"
    script = r'''
import os, sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
conn.execute('PRAGMA journal_mode=WAL')
conn.execute('PRAGMA wal_autocheckpoint=0')
conn.execute('CREATE TABLE issues(id TEXT PRIMARY KEY, status TEXT, closed_at TEXT, close_reason TEXT)')
conn.execute("INSERT INTO issues VALUES('w562-1', 'open', NULL, '')")
conn.execute('CREATE TABLE extra(id INTEGER PRIMARY KEY, data BLOB)')
conn.execute('INSERT INTO extra VALUES(1, ?)', (b'\x00\xff' * 1024,))
conn.commit()
assert conn.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone() == (0, 0, 0)
conn.execute('UPDATE issues SET status=?, closed_at=?, close_reason=?', sys.argv[2:])
conn.commit()
os._exit(0)
'''
    subprocess.run([sys.executable, "-I", "-S", "-c", script, str(original), *CLOSURE],
                   check=True, capture_output=True, text=True, timeout=10)
    directory = root / "source"
    directory.mkdir()
    db = directory / "-source ' λ.db"
    for suffix in ("", "-wal"):
        with sidecar(db, suffix).open("xb") as out:
            out.write(sidecar(original, suffix).read_bytes())
        sidecar(db, suffix).chmod(0o600)
    assert sidecar(db, "-wal").stat().st_size > 32
    assert not sidecar(db, "-shm").exists()
    return db


@contextlib.contextmanager
def pinned_reader(db):
    # A different process owns SQLite's POSIX locks. Observing family bytes
    # here must not release them by closing another descriptor on the inode.
    script = r'''
import os, sqlite3, sys
from pathlib import Path
conn = sqlite3.connect(Path(sys.argv[1]).as_uri() + '?mode=ro', uri=True, timeout=0)
conn.execute('BEGIN')
assert conn.execute('SELECT status FROM issues').fetchone() == ('closed',)
print('ready', flush=True)
sys.stdin.readline()
os._exit(0)
'''
    child = subprocess.Popen([sys.executable, "-I", "-S", "-u", "-c", script, str(db)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    try:
        if not select.select([child.stdout], [], [], 10)[0]:
            raise AssertionError("stock reader failed to acquire a snapshot")
        if child.stdout.readline().strip() != "ready":
            raise AssertionError("stock reader exited before its readiness marker")
        yield
    finally:
        if child.poll() is None:
            child.stdin.close()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        else:
            child.stdin.close()
        child.stdout.close()
        error = child.stderr.read()
        child.stderr.close()
        if child.returncode != 0:
            raise AssertionError(f"fixture reader failed: {error}")


class RecoveryChecks(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="br-packaged-wal-"))
        self.db = make_orphan(self.root)
        self.output = self.root / "new evidence ' λ"
        self.cwd = self.root / "without-checkout"
        self.cwd.mkdir()
        self.counter = 0
        self.program = None
        if BINARY is not None:
            self.program = self.cwd / "br-wal-recover"
            shutil.copyfile(BINARY, self.program)
            self.program.chmod(0o700)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("BR_", "BD_", "BEADS_", "PYTHON"))}
        self.env.update(HOME=str(self.cwd), XDG_CONFIG_HOME=str(self.cwd), NO_COLOR="1")

    def invoke(self, *extra, db=None, output=None, env=None, cwd=None):
        program = ([str(self.program)] if self.program is not None else
                   [sys.executable, "-I", "-S", "-u", "-c", PAYLOAD])
        arguments = [f"--db={db if db is not None else self.db}",
                     f"--output={output if output is not None else self.output}", *extra]
        result = subprocess.run([*program, *arguments], env=self.env if env is None else env,
                                cwd=self.cwd if cwd is None else cwd, text=True,
                                capture_output=True, timeout=60, check=False)
        self.counter += 1
        (self.root / f"run-{self.counter}.stdout").write_text(result.stdout)
        (self.root / f"run-{self.counter}.stderr").write_text(result.stderr)
        return result

    def completed(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        report = json.loads(result.stdout)
        self.assertIs(report["ok"], True)
        self.assertEqual(report["schema"], "br.orphaned_wal.copy.v1")
        self.assertEqual(report["source"], str(self.db))
        retained = json.loads((self.output / "complete.json").read_text())
        self.assertEqual(retained, {key: value for key, value in report.items() if key != "ok"})
        self.assertFalse((self.output / "failed.json").exists())
        self.assertEqual(hashlib.sha256((self.output / "beads.db").read_bytes()).hexdigest(),
                         report["recovered_sha256"])
        with contextlib.closing(sqlite3.connect(
                (self.output / "beads.db").as_uri() + "?immutable=1", uri=True)) as conn:
            self.assertEqual(conn.execute("SELECT status,closed_at,close_reason FROM issues").fetchone(),
                             CLOSURE)
            self.assertEqual(conn.execute("SELECT data FROM extra").fetchone()[0], b"\x00\xff" * 1024)
            self.assertEqual(conn.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
        return report

    def test_default_copy_preserves_source_and_retains_committed_data(self):
        before = family(self.db)
        report = self.completed(self.invoke())
        self.assertIs(report["source_unchanged"], True)
        self.assertIs(report["live_recovery"], False)
        self.assertEqual(report["database_path"], str(self.output / "beads.db"))
        self.assertEqual(family(self.db), before)
        for suffix, observed in before.items():
            saved = sidecar(self.output / "before" / "beads.db", suffix)
            if observed is None:
                self.assertFalse(saved.exists())
            else:
                self.assertEqual(saved.read_bytes(), observed[0])

    def test_explicit_restore_adds_only_the_missing_index(self):
        before = family(self.db)
        report = self.completed(self.invoke("--restore-shm"))
        self.assertIs(report["source_unchanged"], False)
        self.assertIs(report["existing_source_files_unchanged"], True)
        self.assertIs(report["live_recovery"], True)
        self.assertIs(report["restored_shm"]["checkpoint_performed"], False)
        self.assertEqual(report["database_path"], str(self.db))
        self.assertEqual(report["recovered_copy_path"], str(self.output / "beads.db"))
        after = family(self.db)
        self.assertEqual({k: v for k, v in after.items() if k != "-shm"},
                         {k: v for k, v in before.items() if k != "-shm"})
        self.assertEqual(after["-shm"][0], (self.output / "verified-shm").read_bytes())
        self.assertTrue((self.output / "shm-install-complete.json").is_file())
        # The copy oracle above never opens the source. Verify the installed
        # template's bytes before allowing a source SQLite connection here.
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            self.assertEqual(conn.execute("SELECT status,closed_at,close_reason FROM issues").fetchone(),
                             CLOSURE)
            conn.execute("INSERT INTO extra VALUES(2, X'0102')")
            conn.commit()
        with contextlib.closing(sqlite3.connect(self.db)) as conn:
            self.assertEqual(conn.execute("SELECT COUNT(*) FROM extra").fetchone(), (2,))
            self.assertEqual(conn.execute("SELECT status,closed_at,close_reason FROM issues").fetchone(),
                             CLOSURE)

    def test_existing_shm_is_never_replaced(self):
        sidecar(self.db, "-shm").write_bytes(b"short")
        before = family(self.db)
        result = self.invoke("--restore-shm")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHM already exists", result.stderr)
        self.assertEqual(family(self.db), before)
        self.assertFalse(self.output.exists())

    def test_existing_destination_is_never_reused(self):
        self.output.mkdir()
        sentinel = self.output / "keep"
        sentinel.write_bytes(b"preexisting evidence")
        before = family(self.db)
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("destination already exists", result.stderr)
        self.assertEqual(sentinel.read_bytes(), b"preexisting evidence")
        self.assertEqual(list(self.output.iterdir()), [sentinel])
        self.assertEqual(family(self.db), before)

    def test_corruption_and_partial_frames_refuse_without_source_mutation(self):
        original = sidecar(self.db, "-wal").read_bytes()
        for name, data in [("checksum", original[:-1] + bytes([original[-1] ^ 1])),
                           ("partial", original + b"x")]:
            with self.subTest(name=name):
                # Only the newly generated fixture is damaged.
                sidecar(self.db, "-wal").write_bytes(data)
                before = family(self.db)
                result = self.invoke("--restore-shm")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("checksum" if name == "checksum" else "partial frame", result.stderr)
                self.assertEqual(family(self.db), before)
                self.assertFalse(self.output.exists())

    def test_byte_bound_refuses_without_publishing(self):
        before = family(self.db)
        result = self.invoke("--max-bytes", "1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--max-bytes", result.stderr)
        self.assertEqual(family(self.db), before)
        self.assertFalse(self.output.exists())

    def test_missing_source_does_not_create_a_database(self):
        before = family(self.db)
        missing = self.root / "missing.db"
        result = self.invoke(db=missing)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(missing.exists())
        self.assertFalse(self.output.exists())
        self.assertEqual(family(self.db), before)

    def test_interpreter_and_workspace_environment_cannot_retarget_recovery(self):
        marker = self.cwd / "unexpected-import"
        poison = f"from pathlib import Path\nPath({str(marker)!r}).write_text('ran')\nraise RuntimeError('poison')\n"
        for name in ("sitecustomize.py", "sqlite3.py", "json.py"):
            (self.cwd / name).write_text(poison)
        ambient = self.root / "ambient.db"
        ambient.write_bytes(b"must not be opened or repaired")
        before = family(self.db)
        environment = dict(self.env, PYTHONPATH=str(self.cwd), PYTHONHOME=str(self.cwd),
                           BEADS_DB=str(ambient), BEADS_DIR=str(self.cwd))
        self.completed(self.invoke(env=environment))
        self.assertFalse(marker.exists())
        self.assertEqual(ambient.read_bytes(), b"must not be opened or repaired")
        self.assertEqual(family(self.db), before)

    def test_stock_reader_refuses_both_modes_and_retry_succeeds(self):
        with pinned_reader(self.db):
            before = family(self.db)
            for extra in ((), ("--restore-shm",)):
                result = self.invoke(*extra)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("database busy", result.stderr)
                self.assertEqual(family(self.db), before)
                self.assertFalse(self.output.exists())
        # No sidecar is deleted to retry; the copy-only path preserves it.
        before = family(self.db)
        self.completed(self.invoke())
        self.assertEqual(family(self.db), before)

    def test_relative_leading_hyphen_and_unicode_paths_remain_literal(self):
        before = family(self.db)
        relative_output = os.path.relpath(self.output, self.db.parent)
        self.completed(self.invoke(db=Path(self.db.name), output=Path(relative_output), cwd=self.db.parent))
        self.assertEqual(family(self.db), before)


class NativeBoundaryChecks(RecoveryChecks):
    # Constructed only for a supplied real binary, never for payload-only proof.
    def test_help_without_python_does_not_start_recovery(self):
        result = subprocess.run([str(self.program), "--help"],
                                env=dict(self.env, PATH=str(self.cwd)), cwd=self.cwd,
                                capture_output=True, text=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--restore-shm", result.stdout)
        self.assertFalse(self.output.exists())

    def test_missing_python_is_a_prelaunch_error_not_recovery_success(self):
        before = family(self.db)
        result = self.invoke(env=dict(self.env, PATH=str(self.cwd)))
        self.assertNotEqual(result.returncode, 0)
        error = json.loads(result.stderr)
        self.assertIs(error["ok"], False)
        self.assertIs(error["recovery_started"], False)
        self.assertEqual(family(self.db), before)
        self.assertFalse(self.output.exists())


def qualify_br(restore):
    spec = importlib.util.spec_from_file_location("fixture_556", ROOT / "tests/e2e_scripts/wal_recovery_556.py")
    base = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(base)
    root = Path(tempfile.mkdtemp(prefix="br-packaged-wal-br-"))
    seed = base.setup_seed(BR, root / "seed")
    ambient = base.setup_seed(BR, root / "ambient")
    orphan = root / "orphan" / "selected.db"
    base.retain(base.family(seed), orphan)
    issue_id = base.orphaned_closure(orphan)
    before = base.family(orphan)
    before["-shm"] = None
    source = root / "source" / "-selected ' λ.db"
    base.retain(before, source)
    ambient_before = base.family(ambient)
    artifacts_before = base.recovery_files(ambient.parent)
    runtime = root / "without-checkout"
    runtime.mkdir()
    executable = runtime / "br-wal-recover"
    shutil.copyfile(BINARY, executable)
    executable.chmod(0o700)
    output = root / "new-evidence"
    arguments = ["--db", str(source), "--output", str(output)]
    if restore:
        arguments.append("--restore-shm")
    result = base.run_br(executable, runtime, ambient.parent, arguments, "packaged-recovery")
    base.require(result.returncode == 0, f"packaged recovery failed: {result.stderr}")
    report = json.loads(result.stdout)
    base.unchanged(before, base.family(source), tuple(s for s in before if not (restore and s == "-shm")))
    db = Path(report["database_path"])
    flags = ["--db", str(db), "--no-auto-import", "--no-auto-flush"]
    # The first source engine read after SHM installation MUST be br, not an
    # oracle that could silently rebuild the installed template first.
    listed = base.run_br(BR, root, ambient.parent, [*flags, "list", "--all", "--json"], "br-first-read")
    base.require(listed.returncode == 0, f"br could not read recovered state: {listed.stderr}")
    base.require(any(row["id"] == issue_id and row["status"] == "closed"
                     for row in json.loads(listed.stdout)["issues"]), "br missed the WAL-only closure")
    created = base.run_br(BR, root, ambient.parent,
                         [*flags, "create", "GH562 packaged recovery write", "--json"], "br-write")
    base.require(created.returncode == 0, f"br could not write recovered state: {created.stderr}")
    listed = base.run_br(BR, root, ambient.parent, [*flags, "list", "--all", "--json"], "br-after-write")
    base.require(listed.returncode == 0, f"br post-write read failed: {listed.stderr}")
    base.require(any(row["title"] == "GH562 packaged recovery write"
                     for row in json.loads(listed.stdout)["issues"]), "br write did not persist")
    inspection = root / "oracle" / "beads.db"
    base.retain(base.family(db), inspection)
    with contextlib.closing(base.readonly(inspection)) as conn:
        base.require(base.issue_state(conn, issue_id) == ("closed", base.STAMP, base.REASON),
                     "br write lost exact WAL-only closure fields")
    if not restore:
        base.unchanged(before, base.family(source))
    base.unchanged(ambient_before, base.family(ambient))
    base.require(base.recovery_files(ambient.parent) == artifacts_before, "ambient evidence changed")
    print(f"Actual br and packaged recovery verified; retained fixtures: {root}", flush=True)


class BrWorkflowChecks(unittest.TestCase):
    def test_packaged_copy_followed_by_real_br_read_write_read(self):
        qualify_br(False)

    def test_packaged_restoration_consumed_first_by_real_br(self):
        qualify_br(True)


def main():
    global BINARY, BR
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--payload-only", action="store_true")
    mode.add_argument("--recovery-binary", type=Path)
    parser.add_argument("--br", type=Path)
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("the recovery runtime tests require Linux")
    if args.br and not args.recovery_binary:
        parser.error("--br requires an actual --recovery-binary; payload tests are not native qualification")
    for value in (args.recovery_binary, args.br):
        if value is not None and (not value.is_file() or not os.access(value, os.X_OK)):
            parser.error(f"not an executable: {value}")
    BINARY = args.recovery_binary.resolve() if args.recovery_binary else None
    BR = args.br.resolve() if args.br else None
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RecoveryChecks)
    if BINARY:
        # Include only the two extra native boundary methods, not the inherited
        # recovery cases again. Every reported test corresponds to unique work.
        suite.addTests(NativeBoundaryChecks(name) for name in (
            "test_help_without_python_does_not_start_recovery",
            "test_missing_python_is_a_prelaunch_error_not_recovery_success"))
    if BR:
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(BrWorkflowChecks))
    print(f"SQLite {sqlite3.sqlite_version}; mode: " +
          ("REAL packaged binary" if BINARY else "PAYLOAD ONLY; no Rust binary tested"), flush=True)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
